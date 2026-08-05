//! F6: NF-Full — coadición GLS por frecuencia con PSF objetivo.
//!
//! Coadición "propia" en el dominio de Fourier (estilo proper coaddition de
//! Zackay & Ofek) con control explícito de la PSF de salida (espíritu IMCOM):
//! por cada tile se acumulan Q(k) = Σ conj(G_i)·Y_i/S_i y D(k) = Σ |G_i|²/S_i
//! y el máster combinado es SCI(k) = Γ(k)·Q(k)/D(k), con Γ una gaussiana
//! circular elegida para acotar la amplificación de ruido por modo. Se trabaja
//! por tiles con solape 50 % y ventana sqrt-Hann de análisis y síntesis
//! (análisis×síntesis = Hann, partición de unidad EXACTA con hop T/2), con
//! overlap-add normalizado por la suma de ventanas² acumulada por píxel. La
//! rejilla de tiles se extiende medio tile más allá de la imagen leyendo por
//! reflexión especular: así TODOS los píxeles reales quedan bajo la POU
//! exacta (sin píxeles de borde cubiertos solo por la cola de una ventana,
//! donde la normalización 1/ventana² amplificaría el error de borde del
//! filtro de deconvolución y deprimiría el DC del marco).
//!
//! Decisiones de diseño v2 (documentadas):
//! - PSF Moffat espacialmente constante por frame y canal (evaluada al
//!   centro); G_i,c(k) y D_c(k) se evalúan por tile porque el Jacobiano y la
//!   fase de un warp projective son espaciales.
//! - Kernel PSF: rasterize_psf con lado impar >= 8·FWHM (mínimo 9, tope T-1),
//!   incrustado con su centro en el píxel (0,0) del tile envolviendo los
//!   negativos al final (wrap-around) para fase cero: H real-céntrica.
//! - G_i,c = W_i P_i: la PSF nativa se lleva al marco de referencia con el
//!   Jacobiano local target→reference y se compone con la transferencia
//!   COMPLEJA del interpolador. S_i,c(k) = σ′²_i,c·|L_i(k)|² usa la
//!   misma geometría y fase; por tanto el forward y su adjunto comparten el
//!   mismo operador. Similarity/affine son exactos bajo la aproximación de
//!   PSF constante por frame; projective se admite sólo cuando la variación
//!   del Jacobiano dentro del tile desplaza el soporte PSF <= 0.05 px.
//!   LocalDistortion permanece fuera del contrato y degrada a Lite.
//! - Huecos, outliers y no-finitos llegan con peso cero. Un tile incompleto
//!   usa media espacial 1/σ² por píxel; nunca se rellena con el piloto ni se
//!   cuentan imputaciones en VAR/NEFF.
//! - Γ objetivo: gaussiana circular con FWHM inicial = max(2.2 px, percentil
//!   20 de las FWHM medias de los frames), ensanchada en pasos de +0.1 px
//!   (máx 30 iteraciones) hasta que la amplificación por modo
//!   A = max_k[|Γ(k)|²/D(k)] · Σ_i(1/σ′²_i) quede <= 1.5 sobre los modos con
//!   D(k) > 1e-8·D_max. Si no converge, el canal degrada a media ponderada
//!   plana (fallback, contado en tiles_fallback).
//! - Taper suave t(k) = D/(D+εD·D_max): SCI(k) = Γ·Q/(D+εD·D_max), que es
//!   exactamente t·Γ·Q/D sin divisiones inestables.
//! - Γ(0)=1 y G_i(0)=1 (PSF/interpolador suma 1) ⇒ ganancia DC exacta.
//! - Varianza por tile: escalar de Parseval VAR = (1/T²)·Σ_k |Γ|²·t²/D.
//!   (La spec original decía 1/T⁴; con la convención de DFT no normalizada,
//!   Var(Y(k)) = T²·S(k), el factor correcto es 1/T²: así el caso degenerado
//!   de PSF delta y Γ delta reproduce exactamente la óptima 1/Σ(1/σ′²) y
//!   coincide con la varianza del fallback plano.)
//! - Todo el dominio espectral en f64 (rustfft con Complex<f64>).

#![allow(dead_code)]

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::f64::consts::PI;
use std::sync::Arc;

/// FWHM -> sigma gaussiana: 2·sqrt(2·ln 2).
const FWHM_A_SIGMA: f64 = 2.354_820_045_030_949;
/// Umbral numérico del denominador; la fuga PSF solicitada se controla de
/// forma independiente mediante el soporte rasterizado del kernel.
const EPS_D: f64 = 1e-8;
/// FWHM mínima de la Γ objetivo (muestreo sano del máster).
const FWHM_T_MIN: f64 = 2.2;
/// Paso de ensanchado de Γ y número máximo de intentos.
const GAMMA_PASO: f64 = 0.1;
const GAMMA_MAX_ITERS: usize = 30;
/// Suelo de |L(k)|² (evita dividir por ~0 en Nyquist).
const L2_SUELO: f64 = 1e-4;
/// Suelo de σ′² por frame/canal (una σ nula degeneraría los pesos).
const SIGMA2_SUELO: f64 = 1e-12;
/// Error geométrico máximo de la linealización local sobre el soporte PSF.
const MAX_LOCAL_WARP_ERROR_PX: f64 = 0.05;
/// Condicionamiento máximo del Jacobiano. Por encima, el soporte PSF y el
/// ruido remuestreado no pueden representarse de forma estable en el tile.
const MAX_JACOBIAN_CONDITION: f64 = 20.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NfFullWarpKind {
    Translation,
    Affine,
    Projective,
}

/// Geometría que el operador Full puede representar. Se construye mediante
/// `prepare_full_warps`; no debe fabricarse sin pasar sus gates de campo.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NfFullWarp {
    transform: crate::DsTransform,
    pub kind: NfFullWarpKind,
}

pub(crate) struct NfFullInputs<'a> {
    pub warped: &'a crate::frame_store::AdaptiveFrameStore,
    /// Peso binario por frame/píxel/canal, mismo layout que `warped`.
    /// 0 = hueco geométrico, outlier o dato no finito: nunca se imputa.
    pub validity: &'a crate::frame_store::AdaptiveFrameStore,
    /// Precision espacial formal (1/VAR) tras calibracion, normalizacion y
    /// warp. Cero significa muestra ausente/rechazada. Cuando existe, los
    /// sigmas escalares no gobiernan ningun peso cientifico.
    pub precision: Option<&'a crate::frame_store::AdaptiveFrameStore>,
    pub n_frames: usize,
    pub w: usize,
    pub h: usize,
    pub ch: usize,
    /// σ′ de fondo por frame y canal (varianza = σ′²), ya en la escala normalizada.
    pub sigmas: &'a [[f32; 3]],
    /// PSF Moffat independiente por frame/canal (constante espacialmente en
    /// esta versión; evaluada al centro).
    pub psfs: &'a [[crate::deepsky_psf::MoffatPsf; 3]],
    /// Transform target→reference validado para el operador W×PSF.
    pub warps: &'a [NfFullWarp],
    /// true si el warp usó Lanczos-3 (corrección |L(k)|² de la PSD); false = bilineal.
    pub lanczos: bool,
    pub tile_size: usize,
    pub max_psf_leakage: f32,
    pub max_noise_amplification: f32,
    pub cancel: &'a std::sync::atomic::AtomicBool,
}

pub(crate) struct NfFullOutput {
    /// Máster combinado, w*h*ch interleaved.
    pub sci: Vec<f32>,
    /// Varianza por píxel (escalar por tile mezclado por POU), w*h*ch.
    pub var_map: Vec<f32>,
    /// Número efectivo de observaciones reales. Las imputaciones no existen
    /// en el contrato y, por tanto, nunca aumentan este mapa.
    pub neff_map: Vec<f32>,
    /// FWHM de la Γ objetivo elegida (px). 0.0 si ningún canal convergió.
    pub target_fwhm: f32,
    pub tiles_total: usize,
    /// Tiles degradados a media ponderada plana (sin PSF utilizable / Γ no converge).
    pub tiles_fallback: usize,
    /// Tiles que sí ejecutaron el operador GLS espectral.
    pub gls_tiles: usize,
}

// ---------------------------------------------------------------------------
// FFT 2D con planes cacheados (filas + columnas vía buffer transpuesto)
// ---------------------------------------------------------------------------

struct Fft2d {
    n: usize,
    adelante: Arc<dyn Fft<f64>>,
    atras: Arc<dyn Fft<f64>>,
    scratch: Vec<Complex<f64>>,
    transpuesto: Vec<Complex<f64>>,
}

fn transponer(origen: &[Complex<f64>], destino: &mut [Complex<f64>], n: usize) {
    for y in 0..n {
        for x in 0..n {
            destino[x * n + y] = origen[y * n + x];
        }
    }
}

impl Fft2d {
    fn nueva(n: usize) -> Result<Self, String> {
        let mut planner = FftPlanner::new();
        let adelante = planner.plan_fft_forward(n);
        let atras = planner.plan_fft_inverse(n);
        let scratch_len = adelante
            .get_inplace_scratch_len()
            .max(atras.get_inplace_scratch_len());
        Ok(Fft2d {
            n,
            adelante,
            atras,
            scratch: try_zeroed(scratch_len, Complex::default(), "scratch FFT")?,
            transpuesto: try_zeroed(
                n.checked_mul(n)
                    .ok_or_else(|| "NF-Full: FFT fuera de rango".to_string())?,
                Complex::default(),
                "buffer transpuesto FFT",
            )?,
        })
    }

    fn pasada(&mut self, buf: &mut [Complex<f64>], hacia_adelante: bool) {
        let n = self.n;
        let plan = if hacia_adelante {
            self.adelante.clone()
        } else {
            self.atras.clone()
        };
        for fila in buf.chunks_exact_mut(n) {
            plan.process_with_scratch(fila, &mut self.scratch);
        }
        transponer(buf, &mut self.transpuesto, n);
        for fila in self.transpuesto.chunks_exact_mut(n) {
            plan.process_with_scratch(fila, &mut self.scratch);
        }
        transponer(&self.transpuesto, buf, n);
    }

    /// FFT 2D directa (sin normalizar, convención rustfft).
    fn fft(&mut self, buf: &mut [Complex<f64>]) {
        self.pasada(buf, true);
    }

    /// IFFT 2D normalizada por 1/T².
    fn ifft(&mut self, buf: &mut [Complex<f64>]) {
        self.pasada(buf, false);
        let escala = 1.0 / (self.n * self.n) as f64;
        for v in buf.iter_mut() {
            *v *= escala;
        }
    }
}

fn try_zeroed<T: Clone>(len: usize, value: T, label: &str) -> Result<Vec<T>, String> {
    let mut out = Vec::new();
    out.try_reserve_exact(len).map_err(|error| {
        format!("NF-Full: no se pudo reservar {label} ({len} elementos): {error}")
    })?;
    out.resize(len, value);
    Ok(out)
}

#[inline]
fn jacobian_condition(j: [[f64; 2]; 2]) -> Option<f64> {
    let a = j[0][0] * j[0][0] + j[1][0] * j[1][0];
    let b = j[0][0] * j[0][1] + j[1][0] * j[1][1];
    let d = j[0][1] * j[0][1] + j[1][1] * j[1][1];
    let disc = ((a - d) * (a - d) + 4.0 * b * b).sqrt();
    let lmax = 0.5 * (a + d + disc);
    let lmin = 0.5 * (a + d - disc);
    if !lmax.is_finite() || !lmin.is_finite() || lmin <= 1e-12 {
        None
    } else {
        Some((lmax / lmin).sqrt())
    }
}

/// Jacobiano analítico target→reference de la homografía. Similarity y affine
/// son los casos h6=h7=0; usar f64 evita que la cuantización de `forward(f32)`
/// parezca variación geométrica en sensores grandes.
fn jacobian_forward(transform: crate::DsTransform, x: f64, y: f64) -> Option<[[f64; 2]; 2]> {
    if transform.model == crate::DsRegistrationModel::LocalDistortion {
        return None;
    }
    let h = transform.h;
    let denominator = h[6] * x + h[7] * y + h[8];
    if !denominator.is_finite() || denominator.abs() < 1e-12 {
        return None;
    }
    let nx = h[0] * x + h[1] * y + h[2];
    let ny = h[3] * x + h[4] * y + h[5];
    let d2 = denominator * denominator;
    let j = [
        [
            (h[0] * denominator - nx * h[6]) / d2,
            (h[1] * denominator - nx * h[7]) / d2,
        ],
        [
            (h[3] * denominator - ny * h[6]) / d2,
            (h[4] * denominator - ny * h[7]) / d2,
        ],
    ];
    j.iter()
        .flatten()
        .all(|value| value.is_finite())
        .then_some(j)
}

fn validate_jacobian(j: [[f64; 2]; 2]) -> Result<(), String> {
    let det = j[0][0] * j[1][1] - j[0][1] * j[1][0];
    if !det.is_finite() || det <= 1e-8 {
        return Err(format!("Jacobiano no positivo o singular (det={det:.3e})"));
    }
    let condition = jacobian_condition(j).unwrap_or(f64::INFINITY);
    if condition > MAX_JACOBIAN_CONDITION {
        return Err(format!(
            "Jacobiano mal condicionado (cond={condition:.2} > {MAX_JACOBIAN_CONDITION:.0})"
        ));
    }
    Ok(())
}

/// Preflight de geometría Full. Similarity (incluyendo rotación/escala),
/// affine y projective pasan si todo el campo es invertible, orientado y bien
/// condicionado. LocalDistortion conserva el fallback seguro: su polinomio no
/// tiene todavía un contrato de soporte/adjunto por tile suficientemente
/// acotado para publicarlo como Full.
pub(crate) fn prepare_full_warps(
    registered: &[(usize, crate::DsTransform, f64)],
    w: usize,
    h: usize,
) -> Result<Vec<NfFullWarp>, String> {
    let mut out = Vec::new();
    out.try_reserve_exact(registered.len())
        .map_err(|error| format!("NF-Full: no se pudo reservar geometría: {error}"))?;
    let xs = [
        0.0f32,
        0.5 * w.saturating_sub(1) as f32,
        w.saturating_sub(1) as f32,
    ];
    let ys = [
        0.0f32,
        0.5 * h.saturating_sub(1) as f32,
        h.saturating_sub(1) as f32,
    ];
    for (k, &(_frame, transform, _weight)) in registered.iter().enumerate() {
        let kind = match transform.model {
            crate::DsRegistrationModel::LocalDistortion => {
                return Err(format!(
                    "frame registrado {k}: LocalDistortion no tiene aún operador Full con adjunto publicable; degradado a Lite"
                ));
            }
            crate::DsRegistrationModel::Projective => NfFullWarpKind::Projective,
            crate::DsRegistrationModel::Affine => NfFullWarpKind::Affine,
            crate::DsRegistrationModel::Similarity => {
                let identity_error = (transform.h[0] - 1.0)
                    .abs()
                    .max(transform.h[1].abs())
                    .max(transform.h[3].abs())
                    .max((transform.h[4] - 1.0).abs())
                    .max(transform.h[6].abs())
                    .max(transform.h[7].abs())
                    .max((transform.h[8] - 1.0).abs());
                if identity_error <= 1e-10 {
                    NfFullWarpKind::Translation
                } else {
                    NfFullWarpKind::Affine
                }
            }
        };
        for &ry in &ys {
            for &rx in &xs {
                let source = transform.inverse(rx, ry).ok_or_else(|| {
                    format!("frame registrado {k}: inversa no finita en ({rx:.1},{ry:.1})")
                })?;
                let j = jacobian_forward(transform, source.0 as f64, source.1 as f64).ok_or_else(
                    || format!("frame registrado {k}: Jacobiano no finito en ({rx:.1},{ry:.1})"),
                )?;
                validate_jacobian(j).map_err(|reason| {
                    format!("frame registrado {k}: {reason}; Full degradado a Lite")
                })?;
            }
        }
        out.push(NfFullWarp { transform, kind });
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug)]
struct LocalWarp {
    /// Jacobiano target→reference en el centro del tile.
    j: [[f64; 2]; 2],
    /// Fase fraccional de la consulta reference→target.
    phase: [f64; 2],
    /// Variación Frobenius máxima de J dentro del tile.
    jacobian_delta: f64,
}

fn local_warp(
    warp: NfFullWarp,
    ox: isize,
    oy: isize,
    t: usize,
    w: usize,
    h: usize,
) -> Option<LocalWarp> {
    let clamp_x = |x: f64| x.clamp(0.0, w.saturating_sub(1) as f64);
    let clamp_y = |y: f64| y.clamp(0.0, h.saturating_sub(1) as f64);
    // La fase debe evaluarse en un centro que sea un PÍXEL real de salida.
    // Usar (T-1)/2 en tiles pares introduciría artificialmente medio píxel
    // incluso en una traslación estacionaria.
    let cx = clamp_x(ox as f64 + (t / 2) as f64);
    let cy = clamp_y(oy as f64 + (t / 2) as f64);
    let source = warp.transform.inverse(cx as f32, cy as f32)?;
    let j = jacobian_forward(warp.transform, source.0 as f64, source.1 as f64)?;
    validate_jacobian(j).ok()?;
    let mut jacobian_delta = 0.0f64;
    let corners = [
        (clamp_x(ox as f64), clamp_y(oy as f64)),
        (clamp_x(ox as f64 + t as f64 - 1.0), clamp_y(oy as f64)),
        (clamp_x(ox as f64), clamp_y(oy as f64 + t as f64 - 1.0)),
        (
            clamp_x(ox as f64 + t as f64 - 1.0),
            clamp_y(oy as f64 + t as f64 - 1.0),
        ),
    ];
    for &(rx, ry) in &corners {
        let q = warp.transform.inverse(rx as f32, ry as f32)?;
        let jc = jacobian_forward(warp.transform, q.0 as f64, q.1 as f64)?;
        validate_jacobian(jc).ok()?;
        let delta = ((jc[0][0] - j[0][0]).powi(2)
            + (jc[0][1] - j[0][1]).powi(2)
            + (jc[1][0] - j[1][0]).powi(2)
            + (jc[1][1] - j[1][1]).powi(2))
        .sqrt();
        jacobian_delta = jacobian_delta.max(delta);
    }
    Some(LocalWarp {
        j,
        phase: [
            (source.0 as f64).rem_euclid(1.0),
            (source.1 as f64).rem_euclid(1.0),
        ],
        jacobian_delta,
    })
}

// ---------------------------------------------------------------------------
// Geometría de tiles y ventanas
// ---------------------------------------------------------------------------

/// Lado efectivo del tile. `requested` gobierna el techo; en imágenes
/// pequeñas se reduce para evitar FFTs dominadas por reflexión de borde.
fn lado_tile(w: usize, h: usize, requested: usize) -> usize {
    let menor = w.min(h);
    if menor >= requested.saturating_mul(2) {
        return requested;
    }
    let objetivo = (menor / 2).max(64);
    let mut lado = 64usize;
    while lado < objetivo {
        lado *= 2;
    }
    lado.min(requested)
}

/// Orígenes de tiles a lo largo de un eje: rejilla uniforme de paso `hop`
/// que empieza medio tile ANTES del borde (origen -hop) y termina cuando el
/// último origen entra en la imagen. Así cada píxel real queda cubierto por
/// dos tiles consecutivos con offsets que difieren exactamente T/2 y la
/// partición de unidad de la Hann es EXACTA también en los bordes (sin
/// píxeles cubiertos solo por la cola de una ventana, donde la normalización
/// 1/ventana² amplificaría el error de borde de la deconvolución). Los datos
/// fuera de la imagen se leen por reflexión especular.
fn origenes(len: usize, hop: usize) -> Vec<isize> {
    let mut v = Vec::new();
    let mut x = -(hop as isize);
    while x < len as isize {
        v.push(x);
        x += hop as isize;
    }
    v
}

/// Reflexión especular sin repetir el borde (…2,1,0 | 0,1,2… -> -1 ↦ 0).
#[inline]
fn reflejar(mut v: isize, len: usize) -> usize {
    let l = len as isize;
    loop {
        if v < 0 {
            v = -v - 1;
        } else if v >= l {
            v = 2 * l - 1 - v;
        } else {
            return v as usize;
        }
    }
}

/// Frecuencia normalizada (ciclos/píxel) del índice DFT k.
#[inline]
fn frecuencia(k: usize, t: usize) -> f64 {
    if k <= t / 2 {
        k as f64 / t as f64
    } else {
        (k as f64 - t as f64) / t as f64
    }
}

#[inline]
fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

/// Taps 1D del interpolador en coordenadas del frame nativo. La suma se
/// normaliza a uno para conservar exactamente el DC.
fn taps_interpolador(lanczos: bool, phase: f64) -> Vec<(i64, f64)> {
    let phase = phase.rem_euclid(1.0);
    let mut taps = Vec::with_capacity(if lanczos { 6 } else { 2 });
    if lanczos {
        for n in -2i64..=3 {
            let x = n as f64 - phase;
            if x.abs() < 3.0 {
                taps.push((n, sinc(x) * sinc(x / 3.0)));
            }
        }
    } else {
        taps.push((0, 1.0 - phase));
        taps.push((1, phase));
    }
    let suma: f64 = taps.iter().map(|(_, value)| *value).sum();
    if suma.abs() < 1e-12 {
        return vec![(0, 1.0)];
    }
    for (_, value) in &mut taps {
        *value /= suma;
    }
    taps
}

/// Transferencia COMPLEJA del interpolador a una frecuencia nativa arbitraria
/// (ciclos/píxel). Esto permite evaluar L(Jᵀk) para similarity/affine/
/// projective en vez de fingir que toda geometría es una traslación.
fn transferencia_interpolador_1d(lanczos: bool, phase: f64, frequency: f64) -> Complex<f64> {
    let mut out = Complex::default();
    for (n, value) in taps_interpolador(lanczos, phase) {
        let angle = -2.0 * PI * frequency * n as f64;
        out += Complex::new(angle.cos(), angle.sin()) * value;
    }
    out
}

/// |L_i(k)|² 1D conservado para los tests/paridad de traslación.
fn l2_interpolador_1d(t: usize, lanczos: bool, phase: f64) -> Vec<f64> {
    let mut salida = vec![0f64; t];
    for (k, s) in salida.iter_mut().enumerate() {
        *s = transferencia_interpolador_1d(lanczos, phase, frecuencia(k, t)).norm_sqr();
    }
    salida
}

/// Γ gaussiana 1D en frecuencia (transformada analítica, Γ(0)=1 exacto).
fn gamma_1d(t: usize, fwhm: f64) -> Vec<f64> {
    let sigma = fwhm / FWHM_A_SIGMA;
    (0..t)
        .map(|k| {
            let f = frecuencia(k, t);
            (-2.0 * PI * PI * sigma * sigma * f * f).exp()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// PSF -> H(k)
// ---------------------------------------------------------------------------

fn fwhm_media(psf: &crate::deepsky_psf::MoffatPsf) -> f64 {
    0.5 * (psf.fwhm_x as f64 + psf.fwhm_y as f64)
}

/// Lleva la elipse Moffat del sensor target al marco reference mediante J.
/// Los isocontornos Moffat son formas cuadráticas; por ello C' = J C Jᵀ es
/// exacto para similarity/affine y la linealización defendible por tile para
/// projective. `rasterize_psf` renormaliza luego a suma uno.
fn transform_psf(
    psf: &crate::deepsky_psf::MoffatPsf,
    j: [[f64; 2]; 2],
) -> Option<crate::deepsky_psf::MoffatPsf> {
    let fx = psf.fwhm_x as f64;
    let fy = psf.fwhm_y as f64;
    let theta = psf.theta as f64;
    let beta = psf.beta as f64;
    if ![fx, fy, theta, beta].iter().all(|v| v.is_finite())
        || fx <= 0.05
        || fy <= 0.05
        || beta <= 1.0
    {
        return None;
    }
    let (sin_t, cos_t) = theta.sin_cos();
    let fx2 = fx * fx;
    let fy2 = fy * fy;
    // C = R diag(fx²,fy²) Rᵀ, con el primer eje a theta.
    let c00 = cos_t * cos_t * fx2 + sin_t * sin_t * fy2;
    let c01 = cos_t * sin_t * (fx2 - fy2);
    let c11 = sin_t * sin_t * fx2 + cos_t * cos_t * fy2;
    let a00 = j[0][0] * c00 + j[0][1] * c01;
    let a01 = j[0][0] * c01 + j[0][1] * c11;
    let a10 = j[1][0] * c00 + j[1][1] * c01;
    let a11 = j[1][0] * c01 + j[1][1] * c11;
    let r00 = a00 * j[0][0] + a01 * j[0][1];
    let r01 = a00 * j[1][0] + a01 * j[1][1];
    let r11 = a10 * j[1][0] + a11 * j[1][1];
    let disc = ((r00 - r11) * (r00 - r11) + 4.0 * r01 * r01).sqrt();
    let lmax = 0.5 * (r00 + r11 + disc);
    let lmin = 0.5 * (r00 + r11 - disc);
    if !lmax.is_finite() || !lmin.is_finite() || lmin <= 0.0 {
        return None;
    }
    let angle = 0.5 * (2.0 * r01).atan2(r00 - r11);
    Some(crate::deepsky_psf::MoffatPsf {
        fwhm_x: lmax.sqrt() as f32,
        fwhm_y: lmin.sqrt() as f32,
        theta: angle as f32,
        beta: beta as f32,
    })
}

/// Lado mínimo del kernel para que la fracción de flujo Moffat fuera del
/// soporte sea <= `max_leakage`. Usa el eje de mayor alpha como cota
/// conservadora. None implica que la PSF no puede representarse en el tile.
fn psf_kernel_side(
    psf: &crate::deepsky_psf::MoffatPsf,
    t: usize,
    max_leakage: f64,
) -> Option<usize> {
    let fx = psf.fwhm_x as f64;
    let fy = psf.fwhm_y as f64;
    let beta = psf.beta as f64;
    let theta = psf.theta as f64;
    if !fx.is_finite()
        || !fy.is_finite()
        || !beta.is_finite()
        || !theta.is_finite()
        || fx <= 0.05
        || fy <= 0.05
        || beta <= 1.0
        || !(0.0..1.0).contains(&max_leakage)
    {
        return None;
    }
    let fwhm = fx.max(fy);
    let denom = 2.0 * (2.0f64.powf(1.0 / beta) - 1.0).sqrt();
    if !(denom > 0.0) {
        return None;
    }
    let alpha = fwhm / denom;
    let radial_term = max_leakage.powf(1.0 / (1.0 - beta)) - 1.0;
    if !radial_term.is_finite() || radial_term < 0.0 {
        return None;
    }
    let radius = alpha * radial_term.sqrt();
    let mut side = (2.0 * radius.ceil() + 1.0) as usize;
    side = side.max(9);
    if side % 2 == 0 {
        side += 1;
    }
    (side < t).then_some(side)
}

/// H(k) = FFT 2D del kernel PSF rasterizado, incrustado con su centro en el
/// píxel (0,0) del tile y wrap-around de los negativos (fase cero).
fn h_de_psf(
    psf: &crate::deepsky_psf::MoffatPsf,
    lado: usize,
    t: usize,
    fft: &mut Fft2d,
) -> Vec<Complex<f64>> {
    let kernel = crate::deepsky_psf::rasterize_psf(psf, lado);
    let centro = (lado - 1) / 2;
    let mut tile = vec![Complex::default(); t * t];
    for ky in 0..lado {
        let ty = (ky as isize - centro as isize).rem_euclid(t as isize) as usize;
        for kx in 0..lado {
            let tx = (kx as isize - centro as isize).rem_euclid(t as isize) as usize;
            tile[ty * t + tx] = Complex::new(kernel[ky * lado + kx] as f64, 0.0);
        }
    }
    fft.fft(&mut tile);
    tile
}

// ---------------------------------------------------------------------------
// Selección de la Γ objetivo
// ---------------------------------------------------------------------------

/// Ensancha Γ hasta acotar la amplificación de ruido por modo. Devuelve
/// (fwhm_t, plano Γ T×T) o None si no converge en GAMMA_MAX_ITERS.
fn elegir_gamma(
    d: &[f64],
    t: usize,
    ref_inv_var: f64,
    fwhm_inicial: f64,
    max_noise_amplification: f64,
) -> Option<(f64, Vec<f64>)> {
    let d_max = d.iter().cloned().fold(0.0f64, f64::max);
    if !(d_max > 0.0) || !ref_inv_var.is_finite() || ref_inv_var <= 0.0 {
        return None;
    }
    let mut fwhm = fwhm_inicial.max(FWHM_T_MIN);
    for _ in 0..GAMMA_MAX_ITERS {
        if let Some(plano) = gamma_fija_segura(d, t, ref_inv_var, fwhm, max_noise_amplification) {
            return Some((fwhm, plano));
        }
        fwhm += GAMMA_PASO;
    }
    None
}

/// Construye una Γ fija y la acepta sólo si respeta el mismo gate de
/// amplificación que `elegir_gamma`. Se usa para mantener UNA PSF objetivo
/// común entre canales/tiles: un tile incapaz de sostenerla cae a plano, no
/// publica silenciosamente una Γ distinta.
fn gamma_fija_segura(
    d: &[f64],
    t: usize,
    ref_inv_var: f64,
    fwhm: f64,
    max_noise_amplification: f64,
) -> Option<Vec<f64>> {
    let d_max = d.iter().copied().fold(0.0f64, f64::max);
    if !(d_max > 0.0) || !ref_inv_var.is_finite() || ref_inv_var <= 0.0 {
        return None;
    }
    let umbral = EPS_D * d_max;
    let g1 = gamma_1d(t, fwhm);
    let mut amp_max = 0.0f64;
    for ky in 0..t {
        let gy = g1[ky];
        for kx in 0..t {
            let dv = d[ky * t + kx];
            if dv > umbral {
                let g = gy * g1[kx];
                amp_max = amp_max.max(g * g * ref_inv_var / dv);
            }
        }
    }
    if amp_max > max_noise_amplification {
        return None;
    }
    let mut plano = try_zeroed(t.checked_mul(t)?, 0.0f64, "PSF objetivo").ok()?;
    for ky in 0..t {
        for kx in 0..t {
            plano[ky * t + kx] = g1[ky] * g1[kx];
        }
    }
    Some(plano)
}

/// Percentil por rango más próximo sobre una copia ordenada.
fn percentil(valores: &[f64], p: f64) -> f64 {
    let mut v: Vec<f64> = valores.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let idx = ((v.len() - 1) as f64 * p).floor() as usize;
    v[idx]
}

// ---------------------------------------------------------------------------
// Lectura de tiles del store
// ---------------------------------------------------------------------------

/// Lee el subrect del frame para un canal, a f64, con reflexión especular
/// fuera de la imagen (condición de contorno estándar para deconvolución).
/// Lectura por filas con get_range (RAM/mmap sin copiar el frame completo):
/// la imagen reflejada de un intervalo es siempre un rango contiguo, así que
/// cada fila se lee de una sola vez.
#[allow(clippy::too_many_arguments)]
fn leer_tile_canal(
    store: &crate::frame_store::AdaptiveFrameStore,
    frame: usize,
    w: usize,
    h: usize,
    ch: usize,
    canal: usize,
    ox: isize,
    oy: isize,
    t: usize,
) -> Result<Vec<f64>, String> {
    let tile_len = t
        .checked_mul(t)
        .ok_or_else(|| "NF-Full: tile fuera de rango".to_string())?;
    let mut salida = try_zeroed(tile_len, 0f64, "lectura de tile")?;
    // Columnas fuente (reflejadas) y su rango contiguo [lo, hi).
    let mut columnas = Vec::new();
    columnas
        .try_reserve_exact(t)
        .map_err(|error| format!("NF-Full: no se pudo reservar índice de columnas: {error}"))?;
    columnas.extend((0..t).map(|tx| reflejar(ox + tx as isize, w)));
    let (mut lo, mut hi) = (usize::MAX, 0usize);
    for &sx in &columnas {
        lo = lo.min(sx);
        hi = hi.max(sx + 1);
    }
    for ty in 0..t {
        let sy = reflejar(oy + ty as isize, h);
        let fila = store.get_range(frame, (sy * w + lo) * ch..(sy * w + hi) * ch)?;
        let destino = &mut salida[ty * t..(ty + 1) * t];
        for (tx, d) in destino.iter_mut().enumerate() {
            *d = fila[(columnas[tx] - lo) * ch + canal] as f64;
        }
    }
    Ok(salida)
}

/// El operador FFT supone ruido estacionario dentro del tile. Con VAR formal
/// espacial sólo es diagonal en Fourier cuando la precision de cada frame es
/// uniforme dentro de una tolerancia numerica estrecha. Si no se demuestra,
/// el llamador debe usar el fallback GLS espacial por pixel; promediar la VAR
/// y seguir en FFT fabricaria una covarianza que no existe.
fn precisiones_espectrales_tile(
    inputs: &NfFullInputs,
    channel: usize,
    ox: isize,
    oy: isize,
    t: usize,
) -> Result<Option<Vec<f64>>, String> {
    const MAX_RELATIVE_RANGE: f64 = 1.0e-3;
    let mut inverse_variances = Vec::new();
    inverse_variances
        .try_reserve_exact(inputs.n_frames)
        .map_err(|error| format!("NF-Full: no se pudo reservar precision del tile: {error}"))?;
    for frame in 0..inputs.n_frames {
        let validity = leer_tile_canal(
            inputs.validity,
            frame,
            inputs.w,
            inputs.h,
            inputs.ch,
            channel,
            ox,
            oy,
            t,
        )?;
        if validity
            .iter()
            .any(|value| !value.is_finite() || *value < 0.5)
        {
            return Ok(None);
        }
        if let Some(store) = inputs.precision {
            let precision = leer_tile_canal(
                store, frame, inputs.w, inputs.h, inputs.ch, channel, ox, oy, t,
            )?;
            if precision
                .iter()
                .any(|value| !value.is_finite() || *value <= 0.0)
            {
                return Ok(None);
            }
            let mean = precision.iter().sum::<f64>() / precision.len().max(1) as f64;
            let (minimum, maximum) = precision.iter().fold(
                (f64::INFINITY, f64::NEG_INFINITY),
                |(minimum, maximum), value| (minimum.min(*value), maximum.max(*value)),
            );
            if !(mean > 0.0) || !mean.is_finite() || (maximum - minimum) / mean > MAX_RELATIVE_RANGE
            {
                return Ok(None);
            }
            inverse_variances.push(mean);
        } else {
            let sigma = inputs.sigmas[frame][channel.min(2)] as f64;
            if !sigma.is_finite() || sigma <= 0.0 {
                return Ok(None);
            }
            inverse_variances.push(1.0 / (sigma * sigma).max(SIGMA2_SUELO));
        }
    }
    Ok(Some(inverse_variances))
}

// ---------------------------------------------------------------------------
// Motor principal
// ---------------------------------------------------------------------------

enum ModoCanal {
    /// GLS espectral: Γ elegida, denominador D+ε′ y varianza escalar por tile.
    Gls {
        gamma: Vec<f64>,
        denominador: Vec<f64>,
        var_tile: f64,
        neff_tile: f64,
        fwhm_t: f64,
    },
    /// Media ponderada plana con pesos 1/σ′² (equivalente Lite).
    Plano,
}

struct FrameTransfer {
    /// G_i = W_i P_i en Fourier para este tile/canal.
    g: Vec<Complex<f64>>,
    /// 1/|L_i|²; la varianza escalar σ² se aplica fuera para reutilizarlo.
    inv_l2: Vec<f64>,
    fwhm: f64,
}

fn modo_desde_denominador(
    inputs: &NfFullInputs,
    t: usize,
    inverse_variances: &[f64],
    neff_plano: f64,
    target_fwhm: Option<f64>,
    d: &[f64],
    fwhms: &[f64],
) -> Option<ModoCanal> {
    let len = t.checked_mul(t)?;
    if d.len() != len || fwhms.is_empty() {
        return None;
    }
    let ref_inv_var: f64 = inverse_variances.iter().sum();
    let fwhm_ini = percentil(fwhms, 0.2);
    let (fwhm_t, gamma) = if let Some(fixed) = target_fwhm {
        (
            fixed,
            gamma_fija_segura(
                d,
                t,
                ref_inv_var,
                fixed,
                inputs.max_noise_amplification as f64,
            )?,
        )
    } else {
        elegir_gamma(
            d,
            t,
            ref_inv_var,
            fwhm_ini,
            inputs.max_noise_amplification as f64,
        )?
    };
    let d_max = d.iter().copied().fold(0.0f64, f64::max);
    let eps = EPS_D * d_max;
    let mut denominador = try_zeroed(len, 0.0f64, "denominador regularizado").ok()?;
    let mut var_tile = 0.0f64;
    for k in 0..len {
        denominador[k] = d[k] + eps;
        var_tile += gamma[k] * gamma[k] * d[k] / (denominador[k] * denominador[k]);
    }
    var_tile /= len as f64;
    Some(ModoCanal::Gls {
        gamma,
        denominador,
        var_tile,
        neff_tile: neff_plano,
        fwhm_t,
    })
}

#[allow(clippy::too_many_arguments)]
fn frame_transfer(
    psf: &crate::deepsky_psf::MoffatPsf,
    warp: NfFullWarp,
    ox: isize,
    oy: isize,
    w: usize,
    h: usize,
    t: usize,
    lanczos: bool,
    max_psf_leakage: f64,
    fft: &mut Fft2d,
) -> Option<FrameTransfer> {
    let local = local_warp(warp, ox, oy, t, w, h)?;
    let source_side = psf_kernel_side(psf, t, max_psf_leakage)?;
    if warp.kind == NfFullWarpKind::Projective
        && local.jacobian_delta * (source_side as f64 - 1.0) * 0.5 > MAX_LOCAL_WARP_ERROR_PX
    {
        return None;
    }
    let warped_psf = transform_psf(psf, local.j)?;
    let side = psf_kernel_side(&warped_psf, t, max_psf_leakage)?;
    let h_psf = h_de_psf(&warped_psf, side, t, fft);
    let len = t.checked_mul(t)?;
    let mut g = try_zeroed(len, Complex::default(), "transferencia W×PSF").ok()?;
    let mut inv_l2 = try_zeroed(len, 0.0f64, "PSD del warp").ok()?;
    for ky in 0..t {
        let fy = frecuencia(ky, t);
        for kx in 0..t {
            let fx = frecuencia(kx, t);
            // f_target = Jᵀ f_reference.
            let ftx = local.j[0][0] * fx + local.j[1][0] * fy;
            let fty = local.j[0][1] * fx + local.j[1][1] * fy;
            let lx = transferencia_interpolador_1d(lanczos, local.phase[0], ftx);
            let ly = transferencia_interpolador_1d(lanczos, local.phase[1], fty);
            let interpolation = lx * ly;
            let k = ky * t + kx;
            g[k] = h_psf[k] * interpolation;
            inv_l2[k] = 1.0 / interpolation.norm_sqr().max(L2_SUELO);
        }
    }
    // DC exacto: H(0)=L(0)=1. Si se rompe, no existe transferencia
    // fotométrica defendible para este tile.
    if (g[0] - Complex::new(1.0, 0.0)).norm() > 2e-5 {
        return None;
    }
    Some(FrameTransfer {
        g,
        inv_l2,
        fwhm: fwhm_media(&warped_psf),
    })
}

#[allow(clippy::too_many_arguments)]
fn preparar_modo_tile(
    inputs: &NfFullInputs,
    channel: usize,
    ox: isize,
    oy: isize,
    t: usize,
    inverse_variances: &[f64],
    neff_plano: f64,
    target_fwhm: Option<f64>,
    fft: &mut Fft2d,
) -> Option<ModoCanal> {
    let idx_sigma = channel.min(2);
    let len = t.checked_mul(t)?;
    let mut d = try_zeroed(len, 0.0f64, "denominador espectral").ok()?;
    let mut fwhms = Vec::new();
    fwhms.try_reserve_exact(inputs.n_frames).ok()?;
    // Una transferencia por vez: a 512², retener G+PSD de 100 frames
    // superaría ~600 MiB por tile. G se recomputa en la pasada Q para limitar
    // el pico a O(T²), siempre mediante reservas fallibles.
    for i in 0..inputs.n_frames {
        let transfer = frame_transfer(
            &inputs.psfs[i][idx_sigma],
            inputs.warps[i],
            ox,
            oy,
            inputs.w,
            inputs.h,
            t,
            inputs.lanczos,
            inputs.max_psf_leakage as f64,
            fft,
        )?;
        fwhms.push(transfer.fwhm);
        let inv_var = inverse_variances[i];
        for (k, value) in d.iter_mut().enumerate() {
            *value += transfer.g[k].norm_sqr() * inv_var * transfer.inv_l2[k];
        }
    }
    modo_desde_denominador(
        inputs,
        t,
        inverse_variances,
        neff_plano,
        target_fwhm,
        &d,
        &fwhms,
    )
}

/// Construye D y Q en una sola pasada por frame. Conserva el pico O(T²) y
/// evita recalcular W×PSF dos veces por cada tile productivo.
#[allow(clippy::too_many_arguments)]
fn preparar_tile_con_datos(
    inputs: &NfFullInputs,
    channel: usize,
    ox: isize,
    oy: isize,
    t: usize,
    inverse_variances: &[f64],
    neff_plano: f64,
    target_fwhm: Option<f64>,
    analysis_window: &[f64],
    fft: &mut Fft2d,
) -> Result<Option<(ModoCanal, Vec<Complex<f64>>)>, String> {
    let len = t
        .checked_mul(t)
        .ok_or_else(|| "NF-Full: tile fuera de rango".to_string())?;
    let mut d = try_zeroed(len, 0.0f64, "denominador espectral")?;
    let mut q = try_zeroed(len, Complex::<f64>::default(), "numerador espectral")?;
    let mut buffer = try_zeroed(len, Complex::<f64>::default(), "buffer FFT")?;
    let mut fwhms = Vec::new();
    fwhms
        .try_reserve_exact(inputs.n_frames)
        .map_err(|error| format!("NF-Full: no se pudo reservar lista PSF: {error}"))?;
    let idx_sigma = channel.min(2);
    for i in 0..inputs.n_frames {
        let Some(transfer) = frame_transfer(
            &inputs.psfs[i][idx_sigma],
            inputs.warps[i],
            ox,
            oy,
            inputs.w,
            inputs.h,
            t,
            inputs.lanczos,
            inputs.max_psf_leakage as f64,
            fft,
        ) else {
            return Ok(None);
        };
        fwhms.push(transfer.fwhm);
        let data = leer_tile_canal(
            inputs.warped,
            i,
            inputs.w,
            inputs.h,
            inputs.ch,
            channel,
            ox,
            oy,
            t,
        )?;
        for ty in 0..t {
            let wy = analysis_window[ty];
            for tx in 0..t {
                buffer[ty * t + tx] =
                    Complex::new(data[ty * t + tx] * wy * analysis_window[tx], 0.0);
            }
        }
        fft.fft(&mut buffer);
        let inv_var = inverse_variances[i];
        for k in 0..len {
            let inv_noise = inv_var * transfer.inv_l2[k];
            d[k] += transfer.g[k].norm_sqr() * inv_noise;
            q[k] += transfer.g[k].conj() * buffer[k] * inv_noise;
        }
    }
    Ok(modo_desde_denominador(
        inputs,
        t,
        inverse_variances,
        neff_plano,
        target_fwhm,
        &d,
        &fwhms,
    )
    .map(|mode| (mode, q)))
}

pub(crate) fn combine_full(
    inputs: &NfFullInputs,
    progress: &mut dyn FnMut(&str, usize, usize),
) -> Result<NfFullOutput, String> {
    let (w, h, ch, n) = (inputs.w, inputs.h, inputs.ch, inputs.n_frames);
    if n == 0 || w == 0 || h == 0 || ch == 0 {
        return Err("NF-Full: geometría o número de frames vacíos".into());
    }
    if inputs.sigmas.len() != n || inputs.psfs.len() != n || inputs.warps.len() != n {
        return Err(format!(
            "NF-Full: sigmas ({}) / psfs ({}) / warps ({}) no cuadran con n_frames ({n})",
            inputs.sigmas.len(),
            inputs.psfs.len(),
            inputs.warps.len()
        ));
    }
    if !(64..=512).contains(&inputs.tile_size) || !inputs.tile_size.is_power_of_two() {
        return Err(format!(
            "NF-Full: tile_size={} debe ser potencia de dos entre 64 y 512",
            inputs.tile_size
        ));
    }
    if !inputs.max_psf_leakage.is_finite() || !(1e-6..=0.1).contains(&inputs.max_psf_leakage) {
        return Err("NF-Full: max_psf_leakage fuera de [1e-6, 0.1]".into());
    }
    if !inputs.max_noise_amplification.is_finite()
        || !(1.0..=10.0).contains(&inputs.max_noise_amplification)
    {
        return Err("NF-Full: max_noise_amplification fuera de [1, 10]".into());
    }
    if inputs.precision.is_none()
        && inputs
            .sigmas
            .iter()
            .take(n)
            .flat_map(|sigma| sigma.iter().take(ch.min(3)))
            .any(|sigma| !sigma.is_finite() || *sigma <= 0.0)
    {
        return Err("NF-Full: sigma no finito o no positivo".into());
    }

    let t = lado_tile(w, h, inputs.tile_size);
    let hop = t / 2;
    let orig_x = origenes(w, hop);
    let orig_y = origenes(h, hop);
    let n_tiles_xy = orig_x.len() * orig_y.len();
    let tiles_total = n_tiles_xy * ch;

    // Ventana sqrt-Hann 1D: a[i] = sin(π(i+0.5)/T); análisis×síntesis = Hann
    // desplazada media muestra, cuya suma con hop T/2 es 1 exacta.
    let a1: Vec<f64> = (0..t)
        .map(|i| (PI * (i as f64 + 0.5) / t as f64).sin())
        .collect();
    let hann1: Vec<f64> = a1.iter().map(|v| v * v).collect();

    // Rango de índices del tile que caen sobre píxeles reales de la imagen.
    let rango_valido = |o: isize, len: usize| -> (usize, usize) {
        let ini = (-o).max(0) as usize;
        let fin = ((len as isize - o).min(t as isize)).max(0) as usize;
        (ini, fin)
    };

    let mut fft = Fft2d::nueva(t)?;

    // Selecciona una única Γ global antes de integrar. Cada canal propone
    // la FWHM mínima que sostiene en al menos un tile geométricamente válido;
    // se usa la mayor para que RGB comparta PSF. Los tiles que no soporten
    // esta Γ fija degradan a plano en vez de fabricar otra resolución.
    let mut common_target_fwhm = 0.0f64;
    for c in 0..ch {
        let mut candidate = None;
        'search: for &oy in &orig_y {
            for &ox in &orig_x {
                if let Some(inverse_variances) = precisiones_espectrales_tile(inputs, c, ox, oy, t)?
                {
                    let sum = inverse_variances.iter().sum::<f64>();
                    let sum_sq = inverse_variances
                        .iter()
                        .map(|value| value * value)
                        .sum::<f64>();
                    let neff = sum * sum / sum_sq.max(SIGMA2_SUELO);
                    if let Some(ModoCanal::Gls { fwhm_t, .. }) = preparar_modo_tile(
                        inputs,
                        c,
                        ox,
                        oy,
                        t,
                        &inverse_variances,
                        neff,
                        None,
                        &mut fft,
                    ) {
                        candidate = Some(fwhm_t);
                        break 'search;
                    }
                }
            }
        }
        if let Some(value) = candidate {
            common_target_fwhm = common_target_fwhm.max(value);
        }
    }

    let output_len = w
        .checked_mul(h)
        .and_then(|value| value.checked_mul(ch))
        .ok_or_else(|| "NF-Full: geometría de salida fuera de rango".to_string())?;
    let mut sci_acc = try_zeroed(output_len, 0f64, "SCI acumulado")?;
    let mut var_acc = try_zeroed(output_len, 0f64, "VAR acumulado")?;
    let mut neff_acc = try_zeroed(output_len, 0f64, "NEFF acumulado")?;
    let mut out_wgt_acc = try_zeroed(output_len, 0f64, "peso de salida")?;
    let mut tiles_fallback = 0usize;
    let mut gls_tiles = 0usize;
    let mut target_fwhm = 0.0f32;
    let mut hechos = 0usize;

    // Canales secuenciales: σ′ (y por tanto D y Γ) dependen del canal.
    for c in 0..ch {
        for &oy in &orig_y {
            for &ox in &orig_x {
                crate::pipeline::cancellation_checkpoint(inputs.cancel, "NF-Full")?;
                let (ty0, ty1) = rango_valido(oy, h);
                let (tx0, tx1) = rango_valido(ox, w);
                // FFT sólo cuando cada frame tiene cobertura completa y una
                // precision espacial demostrablemente estacionaria. El resto
                // usa GLS espacial exacto, sin colapsar VAR a un sigma falso.
                let spectral_precisions = precisiones_espectrales_tile(inputs, c, ox, oy, t)?;
                let prepared = if let Some(inverse_variances) = spectral_precisions.as_ref() {
                    let sum = inverse_variances.iter().sum::<f64>();
                    let sum_sq = inverse_variances
                        .iter()
                        .map(|value| value * value)
                        .sum::<f64>();
                    let neff_plano = sum * sum / sum_sq.max(SIGMA2_SUELO);
                    preparar_tile_con_datos(
                        inputs,
                        c,
                        ox,
                        oy,
                        t,
                        inverse_variances,
                        neff_plano,
                        (common_target_fwhm > 0.0).then_some(common_target_fwhm),
                        &a1,
                        &mut fft,
                    )?
                } else {
                    None
                };
                if let Some((modo, mut q)) = prepared {
                    let ModoCanal::Gls {
                        gamma,
                        denominador,
                        var_tile,
                        neff_tile,
                        fwhm_t,
                    } = modo
                    else {
                        unreachable!("preparar_tile_con_datos sólo publica GLS")
                    };
                    target_fwhm = target_fwhm.max(fwhm_t as f32);
                    // Q(k) = Σ_i G_i*(k)·Y_i/S_i con G_i=W_i P_i. La
                    // misma transferencia compleja entra en D y constituye
                    // el adjunto exacto del forward local publicado.
                    let tile_len = t * t;
                    // SCI(k) = Γ·Q/(D+ε) = t(k)·Γ·Q/D; IFFT y overlap-add.
                    for k in 0..tile_len {
                        q[k] *= gamma[k] / denominador[k];
                    }
                    fft.ifft(&mut q);
                    for ty in ty0..ty1 {
                        let ay = a1[ty];
                        let hy = hann1[ty];
                        let base = (oy + ty as isize) as usize * w;
                        for tx in tx0..tx1 {
                            let x = (ox + tx as isize) as usize;
                            let p = (base + x) * ch + c;
                            let hann = hy * hann1[tx];
                            // Síntesis sqrt-Hann; el análisis ya iba en los datos.
                            sci_acc[p] += q[ty * t + tx].re * ay * a1[tx];
                            var_acc[p] += var_tile * hann;
                            neff_acc[p] += neff_tile * hann;
                            out_wgt_acc[p] += hann;
                        }
                    }
                    gls_tiles += 1;
                } else {
                    // Fallback por tile/píxel: media 1/σ² usando sólo
                    // observaciones reales. Un hueco no aporta ni a VAR ni NEFF.
                    let tile_len = t * t;
                    let mut sum_tile = try_zeroed(tile_len, 0f64, "suma fallback")?;
                    let mut weight_tile = try_zeroed(tile_len, 0f64, "peso fallback")?;
                    let mut weight_sq_tile =
                        try_zeroed(tile_len, 0f64, "peso cuadrático fallback")?;
                    for i in 0..n {
                        let datos = leer_tile_canal(inputs.warped, i, w, h, ch, c, ox, oy, t)?;
                        let valid = leer_tile_canal(inputs.validity, i, w, h, ch, c, ox, oy, t)?;
                        let precision = match inputs.precision {
                            Some(store) => Some(leer_tile_canal(store, i, w, h, ch, c, ox, oy, t)?),
                            None => None,
                        };
                        let scalar_precision = if precision.is_none() {
                            let sigma = inputs.sigmas[i][c.min(2)] as f64;
                            1.0 / (sigma * sigma).max(SIGMA2_SUELO)
                        } else {
                            0.0
                        };
                        for k in 0..t * t {
                            let peso = precision
                                .as_ref()
                                .map(|plane| plane[k])
                                .unwrap_or(scalar_precision);
                            if valid[k] >= 0.5
                                && valid[k].is_finite()
                                && datos[k].is_finite()
                                && peso.is_finite()
                                && peso > 0.0
                            {
                                sum_tile[k] += datos[k] * peso;
                                weight_tile[k] += peso;
                                weight_sq_tile[k] += peso * peso;
                            }
                        }
                    }
                    for ty in ty0..ty1 {
                        let hy = hann1[ty];
                        let base = (oy + ty as isize) as usize * w;
                        for tx in tx0..tx1 {
                            let k = ty * t + tx;
                            let denom = weight_tile[k];
                            if denom <= 0.0 {
                                continue;
                            }
                            let x = (ox + tx as isize) as usize;
                            let p = (base + x) * ch + c;
                            let hann = hy * hann1[tx];
                            sci_acc[p] += (sum_tile[k] / denom) * hann;
                            var_acc[p] += (1.0 / denom) * hann;
                            neff_acc[p] +=
                                (denom * denom / weight_sq_tile[k].max(SIGMA2_SUELO)) * hann;
                            out_wgt_acc[p] += hann;
                        }
                    }
                    tiles_fallback += 1;
                }
                hechos += 1;
                progress("tiles", hechos, tiles_total);
            }
        }
    }

    // Normalización sólo por ventanas que aportaron datos reales.
    let mut sci = try_zeroed(output_len, f32::NAN, "SCI final")?;
    let mut var_map = try_zeroed(output_len, f32::NAN, "VAR final")?;
    let mut neff_map = try_zeroed(output_len, 0.0f32, "NEFF final")?;
    for p in 0..output_len {
        let g = out_wgt_acc[p];
        if g > 0.0 {
            sci[p] = (sci_acc[p] / g) as f32;
            var_map[p] = (var_acc[p] / g) as f32;
            neff_map[p] = (neff_acc[p] / g) as f32;
        }
    }

    Ok(NfFullOutput {
        sci,
        var_map,
        neff_map,
        target_fwhm,
        tiles_total,
        tiles_fallback,
        gls_tiles,
    })
}

// ---------------------------------------------------------------------------
// Tests (verdad conocida con deepsky_sim; frames identidad = ya "warpeados")
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deepsky_sim::{render_light, SimExposure, SimScene, SimSensor, SimStar};
    use std::sync::atomic::AtomicBool;

    /// PSF Moffat circular con literales sin tipar (se adapta a f32 o f64).
    macro_rules! psf_circular {
        ($fwhm:expr) => {
            crate::deepsky_psf::MoffatPsf {
                fwhm_x: $fwhm,
                fwhm_y: $fwhm,
                theta: 0.0,
                beta: 4.5,
            }
        };
    }

    fn sensor_limpio(read_noise_e: f64) -> SimSensor {
        SimSensor {
            gain_e_per_adu: 1.0,
            read_noise_e,
            bias_adu: 500.0,
            dark_adu_per_s: 0.0,
            full_well_adu: 1.0e9,
            hot_pixels: Vec::new(),
            bayer: None,
            vignette: None,
        }
    }

    fn escena_con_estrella(
        w: usize,
        h: usize,
        fondo: f64,
        cx: f64,
        cy: f64,
        flux: f64,
        fwhm: f64,
    ) -> SimScene {
        SimScene {
            width: w,
            height: h,
            background_adu: fondo,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0, 1.0, 1.0],
            stars: vec![SimStar {
                x: cx,
                y: cy,
                flux_adu: flux,
                fwhm_px: fwhm,
                moffat_beta: Some(4.5),
            }],
        }
    }

    fn exposicion(seed: u64) -> SimExposure {
        SimExposure {
            exposure_s: 60.0,
            dx: 0.0,
            dy: 0.0,
            seed,
        }
    }

    /// Frames identidad (el render YA es el frame warpeado) a un store.
    fn a_store(
        frames: &[Vec<f32>],
        frame_len: usize,
        tag: &str,
    ) -> crate::frame_store::AdaptiveFrameStore {
        let dir =
            std::env::temp_dir().join(format!("zas-nf-full-test-{}-{}", tag, std::process::id()));
        let mut store =
            crate::frame_store::AdaptiveFrameStore::new(frames.len(), frame_len, &dir, tag)
                .expect("store de test");
        for (i, f) in frames.iter().enumerate() {
            store.put(i, f).expect("put de test");
        }
        store
    }

    fn combinar(
        store: &crate::frame_store::AdaptiveFrameStore,
        n: usize,
        w: usize,
        h: usize,
        sigmas: &[[f32; 3]],
        psfs: &[crate::deepsky_psf::MoffatPsf],
        lanczos: bool,
    ) -> NfFullOutput {
        let cancel = AtomicBool::new(false);
        let valid_frames = vec![vec![1.0f32; w * h]; n];
        let validity_tag = crate::pipeline::new_job_id("nf-full-valid-test");
        let validity = a_store(&valid_frames, w * h, &validity_tag);
        let psfs_rgb: Vec<_> = psfs.iter().map(|&psf| [psf; 3]).collect();
        let registered: Vec<_> = (0..n)
            .map(|i| {
                (
                    i,
                    crate::DsTransform::from_similarity((1.0, 0.0, 0.0, 0.0)),
                    1.0,
                )
            })
            .collect();
        let warps = prepare_full_warps(&registered, w, h).expect("warps identidad");
        let inputs = NfFullInputs {
            warped: store,
            validity: &validity,
            precision: None,
            n_frames: n,
            w,
            h,
            ch: 1,
            sigmas,
            psfs: &psfs_rgb,
            warps: &warps,
            lanczos,
            tile_size: 512,
            max_psf_leakage: 1e-3,
            max_noise_amplification: 1.5,
            cancel: &cancel,
        };
        combine_full(&inputs, &mut |_, _, _| {}).expect("combine_full")
    }

    fn combinar_con_mascaras(
        frames: &[Vec<f32>],
        validity_frames: &[Vec<f32>],
        w: usize,
        h: usize,
        ch: usize,
        sigmas: &[[f32; 3]],
        psfs: &[[crate::deepsky_psf::MoffatPsf; 3]],
    ) -> NfFullOutput {
        let data_tag = crate::pipeline::new_job_id("nf-full-data-test");
        let valid_tag = crate::pipeline::new_job_id("nf-full-mask-test");
        let store = a_store(frames, w * h * ch, &data_tag);
        let validity = a_store(validity_frames, w * h * ch, &valid_tag);
        let registered: Vec<_> = (0..frames.len())
            .map(|i| {
                (
                    i,
                    crate::DsTransform::from_similarity((1.0, 0.0, 0.0, 0.0)),
                    1.0,
                )
            })
            .collect();
        let warps = prepare_full_warps(&registered, w, h).expect("warps identidad");
        let cancel = AtomicBool::new(false);
        let inputs = NfFullInputs {
            warped: &store,
            validity: &validity,
            precision: None,
            n_frames: frames.len(),
            w,
            h,
            ch,
            sigmas,
            psfs,
            warps: &warps,
            lanczos: false,
            tile_size: 64,
            max_psf_leakage: 1e-3,
            max_noise_amplification: 1.5,
            cancel: &cancel,
        };
        combine_full(&inputs, &mut |_, _, _| {}).expect("combine_full con máscaras")
    }

    /// Media del fondo lejos de la estrella (r > r_min).
    fn fondo_lejano(img: &[f32], w: usize, h: usize, cx: f64, cy: f64, r_min: f64) -> f64 {
        let (mut suma, mut cuenta) = (0.0f64, 0usize);
        for y in 0..h {
            for x in 0..w {
                let dx = x as f64 - cx;
                let dy = y as f64 - cy;
                if dx * dx + dy * dy > r_min * r_min {
                    suma += img[y * w + x] as f64;
                    cuenta += 1;
                }
            }
        }
        suma / cuenta as f64
    }

    /// Flujo de apertura circular con fondo restado.
    fn flujo_apertura(
        img: &[f32],
        w: usize,
        h: usize,
        cx: f64,
        cy: f64,
        radio: f64,
        fondo: f64,
    ) -> f64 {
        let mut suma = 0.0f64;
        for y in 0..h {
            for x in 0..w {
                let dx = x as f64 - cx;
                let dy = y as f64 - cy;
                if dx * dx + dy * dy <= radio * radio {
                    suma += img[y * w + x] as f64 - fondo;
                }
            }
        }
        suma
    }

    /// FWHM por segundos momentos con recentrado (centroide sobre positivos).
    fn fwhm_por_momentos(
        img: &[f32],
        w: usize,
        h: usize,
        cx0: f64,
        cy0: f64,
        radio: f64,
        fondo: f64,
    ) -> f64 {
        // Paso 1: centroide con valores positivos (robusto al ruido).
        let (mut sx, mut sy, mut s) = (0.0f64, 0.0f64, 0.0f64);
        for y in 0..h {
            for x in 0..w {
                let dx = x as f64 - cx0;
                let dy = y as f64 - cy0;
                if dx * dx + dy * dy <= radio * radio {
                    let v = (img[y * w + x] as f64 - fondo).max(0.0);
                    sx += v * x as f64;
                    sy += v * y as f64;
                    s += v;
                }
            }
        }
        let (cx, cy) = (sx / s, sy / s);
        // Paso 2: momentos segundos centrales (sin recorte: ruido insesgado).
        let (mut m2, mut m0) = (0.0f64, 0.0f64);
        for y in 0..h {
            for x in 0..w {
                let dx = x as f64 - cx;
                let dy = y as f64 - cy;
                let r2 = dx * dx + dy * dy;
                if r2 <= radio * radio {
                    let v = img[y * w + x] as f64 - fondo;
                    m2 += v * r2;
                    m0 += v;
                }
            }
        }
        // σ² circular = E[r²]/2; FWHM = 2.3548·σ.
        FWHM_A_SIGMA * (m2 / m0 / 2.0).sqrt()
    }

    /// 1. Transferencia de flujo: estrella FWHM 3 px, flux 60k, fondo 300.
    ///    El flujo de apertura (r = 6·FWHM_t) de SCI debe quedar en 0.98–1.02
    ///    del verdadero (Γ conserva DC: la fotometría no se toca).
    #[test]
    fn gate_f6_flux_transfer() {
        let (w, h, n) = (256usize, 256usize, 12usize);
        let (cx, cy, flux) = (128.3f64, 127.7f64, 60_000.0f64);
        let scene = escena_con_estrella(w, h, 300.0, cx, cy, flux, 3.0);
        let sensor = sensor_limpio(3.5);
        let frames: Vec<Vec<f32>> = (0..n)
            .map(|i| render_light(&scene, &sensor, &exposicion(100 + i as u64)).0)
            .collect();
        let store = a_store(&frames, w * h, "flux");
        let sigma = (300.0f32 + 3.5 * 3.5).sqrt();
        let sigmas = vec![[sigma; 3]; n];
        let psfs: Vec<_> = (0..n).map(|_| psf_circular!(3.0)).collect();
        let salida = combinar(&store, n, w, h, &sigmas, &psfs, false);

        assert_eq!(salida.tiles_fallback, 0, "no debe haber tiles degradados");
        assert!(
            salida.target_fwhm >= 2.2,
            "target_fwhm={}",
            salida.target_fwhm
        );
        let fondo = fondo_lejano(&salida.sci, w, h, cx, cy, 60.0);
        let radio = 6.0 * salida.target_fwhm as f64;
        let medido = flujo_apertura(&salida.sci, w, h, cx, cy, radio, fondo);
        let razon = medido / flux;
        assert!(
            (0.98..=1.02).contains(&razon),
            "flujo de apertura {medido:.1} vs verdadero {flux:.1} (razón {razon:.4})"
        );
    }

    /// 2. PSF de salida ≈ Γ objetivo con dos poblaciones de seeing (2.6 y
    ///    4.5 px): la FWHM medida queda a <= 10 % de target_fwhm, y
    ///    target_fwhm < 3.4 px — la combinación NO se degrada a la peor PSF
    ///    (la media plana daría ~3.5+): esa es la promesa de NF-Full.
    #[test]
    fn gate_f6_output_psf_near_target() {
        let (w, h, n) = (256usize, 256usize, 12usize);
        let (cx, cy, flux) = (127.6f64, 128.4f64, 60_000.0f64);
        let sensor = sensor_limpio(3.0);
        let mut frames = Vec::with_capacity(n);
        let mut psfs = Vec::with_capacity(n);
        for i in 0..n {
            let fwhm = if i < 6 { 2.6 } else { 4.5 };
            let scene = escena_con_estrella(w, h, 100.0, cx, cy, flux, fwhm);
            frames.push(render_light(&scene, &sensor, &exposicion(200 + i as u64)).0);
            psfs.push(if i < 6 {
                psf_circular!(2.6)
            } else {
                psf_circular!(4.5)
            });
        }
        let store = a_store(&frames, w * h, "psf");
        let sigma = (100.0f32 + 3.0 * 3.0).sqrt();
        let sigmas = vec![[sigma; 3]; n];
        let salida = combinar(&store, n, w, h, &sigmas, &psfs, false);

        assert_eq!(salida.tiles_fallback, 0, "no debe haber tiles degradados");
        let objetivo = salida.target_fwhm as f64;
        assert!(
            objetivo < 3.4,
            "target_fwhm {objetivo:.2} >= 3.4: la Γ se degradó hacia la peor PSF"
        );
        let fondo = fondo_lejano(&salida.sci, w, h, cx, cy, 50.0);
        let medido = fwhm_por_momentos(&salida.sci, w, h, cx, cy, 3.0 * objetivo, fondo);
        let err = (medido - objetivo).abs() / objetivo;
        assert!(
            err <= 0.10,
            "FWHM medida {medido:.3} vs objetivo {objetivo:.3} (err rel {err:.3})"
        );
    }

    /// 3. Sin ringing: el mínimo en el anillo r ∈ [3σ_t, 6σ_t] alrededor de
    ///    la estrella debe quedar por encima de −1e-3·pico.
    #[test]
    fn gate_f6_no_ringing() {
        let (w, h, n) = (256usize, 256usize, 12usize);
        let (cx, cy, flux) = (128.2f64, 127.8f64, 200_000.0f64);
        let scene = escena_con_estrella(w, h, 10.0, cx, cy, flux, 3.0);
        let sensor = sensor_limpio(1.0);
        let frames: Vec<Vec<f32>> = (0..n)
            .map(|i| render_light(&scene, &sensor, &exposicion(300 + i as u64)).0)
            .collect();
        let store = a_store(&frames, w * h, "ringing");
        let sigma = (10.0f32 + 1.0).sqrt();
        let sigmas = vec![[sigma; 3]; n];
        let psfs: Vec<_> = (0..n).map(|_| psf_circular!(3.0)).collect();
        let salida = combinar(&store, n, w, h, &sigmas, &psfs, false);

        assert_eq!(salida.tiles_fallback, 0, "no debe haber tiles degradados");
        let fondo = fondo_lejano(&salida.sci, w, h, cx, cy, 60.0);
        let sigma_t = salida.target_fwhm as f64 / FWHM_A_SIGMA;
        let mut pico = f64::MIN;
        let mut minimo_anillo = f64::MAX;
        for y in 0..h {
            for x in 0..w {
                let dx = x as f64 - cx;
                let dy = y as f64 - cy;
                let r = (dx * dx + dy * dy).sqrt();
                let v = salida.sci[y * w + x] as f64 - fondo;
                if r <= 2.0 && v > pico {
                    pico = v;
                }
                if r >= 3.0 * sigma_t && r <= 6.0 * sigma_t && v < minimo_anillo {
                    minimo_anillo = v;
                }
            }
        }
        assert!(pico > 0.0, "pico no positivo: {pico}");
        assert!(
            minimo_anillo > -1e-3 * pico,
            "ringing: mínimo del anillo {minimo_anillo:.3} <= −1e-3·pico ({:.3})",
            -1e-3 * pico
        );
    }

    /// Warp bilineal de medio píxel (dither representativo): deja una escena
    /// plana intacta y da al ruido EXACTAMENTE la PSD σ²·|L(k)|² del modelo
    /// bilineal del motor — los frames del contrato llegan YA warpeados.
    fn warp_bilineal_medio_pixel(img: &[f32], w: usize, h: usize) -> Vec<f32> {
        let mut out = vec![0f32; w * h];
        for y in 0..h {
            let y1 = (y + 1).min(h - 1);
            for x in 0..w {
                let x1 = (x + 1).min(w - 1);
                out[y * w + x] =
                    0.25 * (img[y * w + x] + img[y * w + x1] + img[y1 * w + x] + img[y1 * w + x1]);
            }
        }
        out
    }

    /// 4. Fondo plano con dos poblaciones de ruido (σ 3 y 12): sin costuras
    ///    de tile (medias de bloques 64×64 dentro de 0.2·σ_out del global) y
    ///    varianza espacial <= 1.10 × la óptima 1/Σ(1/σ²).
    #[test]
    fn gate_f6_flat_background_no_seams() {
        let (w, h, n) = (256usize, 256usize, 12usize);
        let scene = SimScene {
            width: w,
            height: h,
            background_adu: 0.0,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0, 1.0, 1.0],
            stars: Vec::new(),
        };
        let mut frames = Vec::with_capacity(n);
        let mut sigmas = Vec::with_capacity(n);
        for i in 0..n {
            let rn = if i < 6 { 3.0 } else { 12.0 };
            let sensor = sensor_limpio(rn);
            let crudo = render_light(&scene, &sensor, &exposicion(400 + i as u64)).0;
            frames.push(warp_bilineal_medio_pixel(&crudo, w, h));
            sigmas.push([rn as f32; 3]);
        }
        let store = a_store(&frames, w * h, "seams");
        let psfs: Vec<_> = (0..n).map(|_| psf_circular!(2.5)).collect();
        let salida = combinar(&store, n, w, h, &sigmas, &psfs, false);

        assert_eq!(salida.tiles_fallback, 0, "no debe haber tiles degradados");
        let var_optima: f64 = 1.0 / (6.0 / 9.0 + 6.0 / 144.0);
        let sigma_salida = var_optima.sqrt();

        // Media global y de bloques 64×64 (los tiles internos van a 64 px:
        // cualquier costura de OLA aparecería como salto entre bloques).
        let media_global = salida.sci.iter().map(|&v| v as f64).sum::<f64>() / (w * h) as f64;
        let bloque = 64usize;
        for by in 0..h / bloque {
            for bx in 0..w / bloque {
                let mut suma = 0.0f64;
                for y in 0..bloque {
                    for x in 0..bloque {
                        suma += salida.sci[(by * bloque + y) * w + bx * bloque + x] as f64;
                    }
                }
                let media_bloque = suma / (bloque * bloque) as f64;
                let desvio = (media_bloque - media_global).abs();
                assert!(
                    desvio < 0.2 * sigma_salida,
                    "costura: bloque ({bx},{by}) media {media_bloque:.4} vs global \
                     {media_global:.4} (desvío {desvio:.4} > {:.4})",
                    0.2 * sigma_salida
                );
            }
        }

        // Varianza espacial frente a la óptima (la Γ solo puede mejorarla).
        let mut var_espacial = 0.0f64;
        for y in 0..h {
            for x in 0..w {
                let d = salida.sci[y * w + x] as f64 - media_global;
                var_espacial += d * d;
            }
        }
        var_espacial /= (w * h - 1) as f64;
        assert!(
            var_espacial <= 1.10 * var_optima,
            "varianza espacial {var_espacial:.4} > 1.10 × óptima ({var_optima:.4})"
        );
    }

    /// 5. Partición de unidad y normalización de bordes: señal constante 1000
    ///    con PSF delta-like (FWHM 1.2, Γ ~2.2) => SCI constante 1000 dentro
    ///    del 0.5 % en el interior (Γ ensancha pero Γ(0)=1 conserva el DC).
    #[test]
    fn test_window_partition_of_unity() {
        let (w, h, n) = (256usize, 256usize, 10usize);
        let frames: Vec<Vec<f32>> = (0..n).map(|_| vec![1000.0f32; w * h]).collect();
        let store = a_store(&frames, w * h, "pou");
        let sigmas = vec![[1.0f32; 3]; n];
        let psfs: Vec<_> = (0..n).map(|_| psf_circular!(1.2)).collect();
        let salida = combinar(&store, n, w, h, &sigmas, &psfs, false);

        assert_eq!(salida.tiles_fallback, 0, "no debe haber tiles degradados");
        assert!(
            (salida.target_fwhm as f64 - FWHM_T_MIN).abs() < 0.3,
            "Γ esperada ~2.2, obtenida {}",
            salida.target_fwhm
        );
        let margen = 16usize;
        for y in margen..h - margen {
            for x in margen..w - margen {
                let v = salida.sci[y * w + x] as f64;
                assert!(
                    (v - 1000.0).abs() <= 5.0,
                    "POU rota en ({x},{y}): {v:.4} (tolerancia 1000±5)"
                );
            }
        }
        assert!(
            salida.var_map.iter().all(|v| v.is_finite() && *v >= 0.0),
            "var_map con valores no finitos o negativos"
        );
    }

    #[test]
    fn missing_samples_have_zero_weight_and_do_not_inflate_neff() {
        let (w, h, ch, n) = (64usize, 64usize, 1usize, 3usize);
        let frame_len = w * h;
        let frames = vec![
            vec![10.0f32; frame_len],
            vec![20.0f32; frame_len],
            vec![90.0f32; frame_len],
        ];
        let mut validity = vec![vec![1.0f32; frame_len]; n];
        let partial = 32 * w + 32;
        validity[2][partial] = 0.0;
        let uncovered = 12 * w + 9;
        for mask in &mut validity {
            mask[uncovered] = 0.0;
        }
        let sigmas = vec![[1.0f32; 3]; n];
        let psfs = vec![[psf_circular!(2.5); 3]; n];
        let out = combinar_con_mascaras(&frames, &validity, w, h, ch, &sigmas, &psfs);

        assert!(
            out.tiles_fallback > 0,
            "el hueco debe activar fallback local"
        );
        assert!(
            (out.sci[partial] - 15.0).abs() < 1e-4,
            "SCI parcial {} contó una imputación o el frame inválido",
            out.sci[partial]
        );
        assert!(
            (out.var_map[partial] - 0.5).abs() < 1e-4,
            "VAR parcial {} vs 1/2",
            out.var_map[partial]
        );
        assert!(
            (out.neff_map[partial] - 2.0).abs() < 1e-4,
            "NEFF parcial {} vs 2 observaciones reales",
            out.neff_map[partial]
        );
        assert!(out.sci[uncovered].is_nan());
        assert!(out.var_map[uncovered].is_nan());
        assert_eq!(out.neff_map[uncovered], 0.0);
    }

    #[test]
    fn spatial_formal_precision_uses_exact_pixel_gls_instead_of_scalar_sigma() {
        let (w, h, ch, n) = (64usize, 64usize, 1usize, 2usize);
        let frames = [vec![10.0f32; w * h], vec![20.0f32; w * h]];
        let validity_frames = vec![vec![1.0f32; w * h]; n];
        // Franjas verticales de 8 px: con tile 64 y orígenes [-32, 0, 32],
        // los tiles de borde leen por reflexión especular y un simple corte a
        // mitad de imagen dejaría 6 tiles internamente uniformes (GLS legítimo
        // por tile). Las franjas garantizan precision no estacionaria dentro
        // de TODOS los tiles, que es lo que este test quiere provocar.
        let mut first_precision = vec![1.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                if (x / 8) % 2 == 1 {
                    first_precision[y * w + x] = 4.0;
                }
            }
        }
        let precision_frames = [first_precision, vec![0.25f32; w * h]];
        let data_tag = crate::pipeline::new_job_id("nf-full-spatial-data");
        let valid_tag = crate::pipeline::new_job_id("nf-full-spatial-valid");
        let precision_tag = crate::pipeline::new_job_id("nf-full-spatial-precision");
        let data = a_store(&frames, w * h, &data_tag);
        let validity = a_store(&validity_frames, w * h, &valid_tag);
        let precision = a_store(&precision_frames, w * h, &precision_tag);
        let registered: Vec<_> = (0..n)
            .map(|index| {
                (
                    index,
                    crate::DsTransform::from_similarity((1.0, 0.0, 0.0, 0.0)),
                    1.0,
                )
            })
            .collect();
        let warps = prepare_full_warps(&registered, w, h).expect("warps identidad");
        let sigmas = vec![[999.0f32; 3]; n];
        let psfs = vec![[psf_circular!(2.5); 3]; n];
        let cancel = AtomicBool::new(false);
        let inputs = NfFullInputs {
            warped: &data,
            validity: &validity,
            precision: Some(&precision),
            n_frames: n,
            w,
            h,
            ch,
            sigmas: &sigmas,
            psfs: &psfs,
            warps: &warps,
            lanczos: false,
            tile_size: 64,
            max_psf_leakage: 1e-3,
            max_noise_amplification: 1.5,
            cancel: &cancel,
        };
        let out = combine_full(&inputs, &mut |_, _, _| {}).expect("Full espacial");
        assert_eq!(
            out.gls_tiles, 0,
            "VAR no estacionaria no debe entrar al FFT"
        );
        assert!(out.tiles_fallback > 0);
        // x=16 cae en franja de precision 1.0; x=40 en franja de 4.0.
        let left = 32 * w + 16;
        let right = 32 * w + 40;
        assert!((out.sci[left] - 12.0).abs() < 1.0e-5);
        assert!((out.var_map[left] - 0.8).abs() < 1.0e-5);
        assert!((out.neff_map[left] - 1.470_588_2).abs() < 1.0e-5);
        assert!((out.sci[right] - (45.0 / 4.25) as f32).abs() < 1.0e-5);
        assert!((out.var_map[right] - (1.0 / 4.25) as f32).abs() < 1.0e-5);
    }

    #[test]
    fn rgb_uses_distinct_noise_and_psf_per_channel() {
        let (w, h, ch, n) = (64usize, 64usize, 3usize, 4usize);
        let mut frame = vec![0.0f32; w * h * ch];
        for p in 0..w * h {
            frame[p * ch] = 100.0;
            frame[p * ch + 1] = 200.0;
            frame[p * ch + 2] = 300.0;
        }
        let frames = vec![frame; n];
        let validity = vec![vec![1.0f32; w * h * ch]; n];
        let sigmas = vec![[1.0f32, 2.0, 4.0]; n];
        let psfs = vec![[psf_circular!(2.2), psf_circular!(3.2), psf_circular!(4.2),]; n];
        let out = combinar_con_mascaras(&frames, &validity, w, h, ch, &sigmas, &psfs);
        let center = (32 * w + 32) * ch;

        assert!(out.gls_tiles > 0, "las PSF por canal deben ser utilizables");
        assert!((out.sci[center] - 100.0).abs() < 0.5);
        assert!((out.sci[center + 1] - 200.0).abs() < 1.0);
        assert!((out.sci[center + 2] - 300.0).abs() < 1.5);
        assert!(
            out.var_map[center] < out.var_map[center + 1]
                && out.var_map[center + 1] < out.var_map[center + 2],
            "VAR RGB no respeta sigmas independientes: {:?}",
            &out.var_map[center..center + 3]
        );
        for c in 0..3 {
            assert!(
                (out.neff_map[center + c] - n as f32).abs() < 1e-3,
                "NEFF canal {c}: {}",
                out.neff_map[center + c]
            );
        }
        assert!(
            out.target_fwhm >= 4.1,
            "la PSF azul no gobernó el target conservador: {}",
            out.target_fwhm
        );
    }

    fn apply_local_transfer(
        input: &[f64],
        transfer: &[Complex<f64>],
        n: usize,
        adjoint: bool,
    ) -> Vec<f64> {
        let mut fft = Fft2d::nueva(n).expect("FFT test");
        let mut buffer: Vec<_> = input
            .iter()
            .map(|&value| Complex::new(value, 0.0))
            .collect();
        fft.fft(&mut buffer);
        for (value, &g) in buffer.iter_mut().zip(transfer) {
            *value *= if adjoint { g.conj() } else { g };
        }
        fft.ifft(&mut buffer);
        buffer.into_iter().map(|value| value.re).collect()
    }

    fn test_warp(transform: crate::DsTransform, kind: NfFullWarpKind) -> NfFullWarp {
        NfFullWarp { transform, kind }
    }

    #[test]
    fn warp_psf_forward_and_adjoint_match_for_rotated_similarity() {
        let n = 64usize;
        let angle = 2.0f32.to_radians();
        let transform = crate::DsTransform::from_similarity((
            1.003 * angle.cos(),
            1.003 * angle.sin(),
            0.31,
            -0.27,
        ));
        let warp = test_warp(transform, NfFullWarpKind::Affine);
        let mut fft = Fft2d::nueva(n).expect("FFT test");
        let transfer = frame_transfer(
            &crate::deepsky_psf::MoffatPsf {
                fwhm_x: 3.4,
                fwhm_y: 2.5,
                theta: 0.3,
                beta: 4.2,
            },
            warp,
            0,
            0,
            n,
            n,
            n,
            true,
            1e-3,
            &mut fft,
        )
        .expect("transferencia similarity rotada");
        let x: Vec<f64> = (0..n * n)
            .map(|i| ((i * 37 % 101) as f64 - 50.0) / 31.0)
            .collect();
        let y: Vec<f64> = (0..n * n)
            .map(|i| ((i * 53 % 97) as f64 - 48.0) / 29.0)
            .collect();
        let gx = apply_local_transfer(&x, &transfer.g, n, false);
        let gty = apply_local_transfer(&y, &transfer.g, n, true);
        let lhs: f64 = gx.iter().zip(&y).map(|(a, b)| a * b).sum();
        let rhs: f64 = x.iter().zip(&gty).map(|(a, b)| a * b).sum();
        let scale = lhs.abs().max(rhs.abs()).max(1.0);
        assert!(
            (lhs - rhs).abs() / scale < 2e-10,
            "adjunto inconsistente: <Gx,y>={lhs:.12e}, <x,G*y>={rhs:.12e}"
        );
    }

    #[test]
    fn identity_warp_matches_psf_only_operator() {
        let n = 64usize;
        let psf = psf_circular!(2.8);
        let warp = test_warp(
            crate::DsTransform::from_similarity((1.0, 0.0, 0.0, 0.0)),
            NfFullWarpKind::Translation,
        );
        let mut fft = Fft2d::nueva(n).expect("FFT test");
        let transfer = frame_transfer(&psf, warp, 0, 0, n, n, n, false, 1e-3, &mut fft)
            .expect("transferencia identidad");
        let side = psf_kernel_side(&psf, n, 1e-3).expect("soporte PSF");
        let expected = h_de_psf(&psf, side, n, &mut fft);
        let worst = transfer
            .g
            .iter()
            .zip(expected)
            .map(|(actual, reference)| (*actual - reference).norm())
            .fold(0.0f64, f64::max);
        assert!(worst < 2e-6, "paridad identidad rota: error={worst:.3e}");
        assert!(
            transfer
                .inv_l2
                .iter()
                .all(|value| (*value - 1.0).abs() < 1e-12),
            "la identidad fabricó correlación de interpolación"
        );
    }

    #[test]
    fn affine_and_mild_projective_preserve_dc_photometry() {
        let n = 64usize;
        let affine = crate::DsTransform {
            model: crate::DsRegistrationModel::Affine,
            h: [1.01, -0.012, 0.23, 0.008, 0.995, -0.17, 0.0, 0.0, 1.0],
            poly: [0.0; 12],
            norm: [0.0, 0.0, 1.0],
        };
        let projective = crate::DsTransform {
            model: crate::DsRegistrationModel::Projective,
            h: [1.0, -0.002, 0.1, 0.001, 1.0, -0.2, 2e-7, -1e-7, 1.0],
            poly: [0.0; 12],
            norm: [0.0, 0.0, 1.0],
        };
        for (transform, kind) in [
            (affine, NfFullWarpKind::Affine),
            (projective, NfFullWarpKind::Projective),
        ] {
            let mut fft = Fft2d::nueva(n).expect("FFT test");
            let transfer = frame_transfer(
                &psf_circular!(3.1),
                test_warp(transform, kind),
                0,
                0,
                n,
                n,
                n,
                true,
                1e-3,
                &mut fft,
            )
            .expect("warp soportado");
            let flat = vec![731.25f64; n * n];
            let out = apply_local_transfer(&flat, &transfer.g, n, false);
            let max_error = out
                .iter()
                .map(|value| (value - 731.25).abs())
                .fold(0.0f64, f64::max);
            assert!(
                max_error < 5e-6,
                "{kind:?} no conserva flujo DC: error={max_error:.3e}"
            );
            let mut impulse = vec![0.0f64; n * n];
            impulse[(n / 2) * n + n / 2] = 1.0;
            let point = apply_local_transfer(&impulse, &transfer.g, n, false);
            let point_flux: f64 = point.iter().sum();
            assert!(
                (point_flux - 1.0).abs() < 1e-8,
                "{kind:?} sesgó fotometría puntual: flujo={point_flux:.12}"
            );
        }
    }

    #[test]
    fn projective_tile_with_excessive_psf_warp_falls_back() {
        let n = 64usize;
        let strong = crate::DsTransform {
            model: crate::DsRegistrationModel::Projective,
            h: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 8e-4, 0.0, 1.0],
            poly: [0.0; 12],
            norm: [0.0, 0.0, 1.0],
        };
        let mut fft = Fft2d::nueva(n).expect("FFT test");
        assert!(
            frame_transfer(
                &psf_circular!(4.0),
                test_warp(strong, NfFullWarpKind::Projective),
                0,
                0,
                n,
                n,
                n,
                true,
                1e-3,
                &mut fft,
            )
            .is_none(),
            "una homografía que varía >0.05 px sobre el soporte no debe publicar GLS"
        );
    }

    /// Extra: geometría de tiles y transferencias con ganancia DC exacta.
    #[test]
    fn test_nf_full_helpers() {
        // Orígenes: rejilla extendida medio tile más allá de ambos bordes.
        assert_eq!(origenes(256, 64), vec![-64, 0, 64, 128, 192]);
        assert_eq!(origenes(300, 64), vec![-64, 0, 64, 128, 192, 256]);
        assert_eq!(origenes(100, 64), vec![-64, 0, 64]);
        // Reflexión especular sin repetir el borde.
        assert_eq!(reflejar(-1, 10), 0);
        assert_eq!(reflejar(-3, 10), 2);
        assert_eq!(reflejar(10, 10), 9);
        assert_eq!(reflejar(13, 10), 6);
        assert_eq!(reflejar(25, 10), 5); // doble rebote: 25 -> -6 -> 5
                                         // POU exacta de la Hann desplazada media muestra con hop T/2.
        let t = 64usize;
        let hann: Vec<f64> = (0..t)
            .map(|i| (PI * (i as f64 + 0.5) / t as f64).sin().powi(2))
            .collect();
        for i in 0..t / 2 {
            assert!((hann[i] + hann[i + t / 2] - 1.0).abs() < 1e-12);
        }
        // L(0) = 1 para ambos interpoladores (taps normalizados).
        for lanczos in [true, false] {
            let l2 = l2_interpolador_1d(64, lanczos, 0.5);
            assert!((l2[0] - 1.0).abs() < 1e-12, "L(0) != 1 (lanczos={lanczos})");
        }
        // Γ(0) = 1 siempre.
        let g = gamma_1d(64, 3.7);
        assert!((g[0] - 1.0).abs() < 1e-15);
    }
}
