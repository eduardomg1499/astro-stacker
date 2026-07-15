//! F6: NF-Full — coadición GLS por frecuencia con PSF objetivo.
//!
//! Coadición "propia" en el dominio de Fourier (estilo proper coaddition de
//! Zackay & Ofek) con control explícito de la PSF de salida (espíritu IMCOM):
//! por cada tile se acumulan Q(k) = Σ conj(H_i)·Y_i/S_i y D(k) = Σ |H_i|²/S_i
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
//! Decisiones de diseño v1 (documentadas):
//! - PSF Moffat espacialmente constante por frame (evaluada al centro); como
//!   las PSF no dependen del tile, H_i(k) y D(k) se calculan UNA vez por
//!   geometría/canal y se reutilizan en todos los tiles.
//! - Kernel PSF: rasterize_psf con lado impar >= 8·FWHM (mínimo 9, tope T-1),
//!   incrustado con su centro en el píxel (0,0) del tile envolviendo los
//!   negativos al final (wrap-around) para fase cero: H real-céntrica.
//! - S_i(k) = σ′²_i(canal)·|L(k)|², con |L(k)|² la transferencia del
//!   interpolador del warp: kernel Lanczos-3 (a=3) o triángulo (bilineal)
//!   muestreado a desplazamiento de medio píxel (caso representativo),
//!   separable |L(kx)|²·|L(ky)|² con suelo 1e-4 (no dividir por ~0 en
//!   Nyquist). Al ser común a todos los frames se cancela en Q/D: solo
//!   afecta a la elección de Γ y a la varianza.
//! - Γ objetivo: gaussiana circular con FWHM inicial = max(2.2 px, percentil
//!   20 de las FWHM medias de los frames), ensanchada en pasos de +0.1 px
//!   (máx 30 iteraciones) hasta que la amplificación por modo
//!   A = max_k[|Γ(k)|²/D(k)] · Σ_i(1/σ′²_i) quede <= 1.5 sobre los modos con
//!   D(k) > 1e-8·D_max. Si no converge, el canal degrada a media ponderada
//!   plana (fallback, contado en tiles_fallback).
//! - Taper suave t(k) = D/(D+εD·D_max): SCI(k) = Γ·Q/(D+εD·D_max), que es
//!   exactamente t·Γ·Q/D sin divisiones inestables.
//! - Γ(0)=1 y H_i(0)=1 (kernels suma 1) ⇒ ganancia DC exacta (fotometría).
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
/// Umbral relativo εD del taper y del filtrado de modos para el criterio de Γ.
const EPS_D: f64 = 1e-8;
/// Cota de amplificación de ruido por modo respecto a la media plana óptima.
const AMP_MAX: f64 = 1.5;
/// FWHM mínima de la Γ objetivo (muestreo sano del máster).
const FWHM_T_MIN: f64 = 2.2;
/// Paso de ensanchado de Γ y número máximo de intentos.
const GAMMA_PASO: f64 = 0.1;
const GAMMA_MAX_ITERS: usize = 30;
/// Suelo de |L(k)|² (evita dividir por ~0 en Nyquist).
const L2_SUELO: f64 = 1e-4;
/// Suelo de σ′² por frame/canal (una σ nula degeneraría los pesos).
const SIGMA2_SUELO: f64 = 1e-12;

pub(crate) struct NfFullInputs<'a> {
    pub warped: &'a crate::frame_store::AdaptiveFrameStore,
    pub n_frames: usize,
    pub w: usize,
    pub h: usize,
    pub ch: usize,
    /// σ′ de fondo por frame y canal (varianza = σ′²), ya en la escala normalizada.
    pub sigmas: &'a [[f32; 3]],
    /// PSF Moffat por frame (constante espacialmente en v1; evaluada al centro).
    pub psfs: &'a [crate::deepsky_psf::MoffatPsf],
    /// true si el warp usó Lanczos-3 (corrección |L(k)|² de la PSD); false = bilineal.
    pub lanczos: bool,
    pub cancel: &'a std::sync::atomic::AtomicBool,
}

pub(crate) struct NfFullOutput {
    /// Máster combinado, w*h*ch interleaved.
    pub sci: Vec<f32>,
    /// Varianza por píxel (escalar por tile mezclado por POU), w*h*ch.
    pub var_map: Vec<f32>,
    /// FWHM de la Γ objetivo elegida (px). 0.0 si ningún canal convergió.
    pub target_fwhm: f32,
    pub tiles_total: usize,
    /// Tiles degradados a media ponderada plana (sin PSF utilizable / Γ no converge).
    pub tiles_fallback: usize,
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
    fn nueva(n: usize) -> Self {
        let mut planner = FftPlanner::new();
        let adelante = planner.plan_fft_forward(n);
        let atras = planner.plan_fft_inverse(n);
        let scratch_len = adelante
            .get_inplace_scratch_len()
            .max(atras.get_inplace_scratch_len());
        Fft2d {
            n,
            adelante,
            atras,
            scratch: vec![Complex::default(); scratch_len],
            transpuesto: vec![Complex::default(); n * n],
        }
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

// ---------------------------------------------------------------------------
// Geometría de tiles y ventanas
// ---------------------------------------------------------------------------

/// Lado del tile: 512 para imágenes grandes; en pequeñas la menor potencia de
/// 2 >= min(w,h)/2, con mínimo 64.
fn lado_tile(w: usize, h: usize) -> usize {
    let menor = w.min(h);
    if menor >= 1024 {
        return 512;
    }
    let objetivo = (menor / 2).max(64);
    let mut lado = 64usize;
    while lado < objetivo {
        lado *= 2;
    }
    lado.min(512)
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

/// |L(k)|² 1D del interpolador del warp, muestreado a medio píxel de
/// desplazamiento (caso representativo/peor): Lanczos-3 (6 taps) o triángulo
/// bilineal (2 taps). Taps normalizados a suma 1 => L(0) = 1. DFT directa
/// O(T²): coste despreciable frente a las FFT 2D de los tiles.
fn l2_interpolador_1d(t: usize, lanczos: bool) -> Vec<f64> {
    let mut taps = vec![0f64; t];
    if lanczos {
        for n in -3i64..3 {
            let x = n as f64 + 0.5;
            taps[n.rem_euclid(t as i64) as usize] += sinc(x) * sinc(x / 3.0);
        }
    } else {
        // Bilineal: triángulo evaluado en ±0.5 -> dos taps de 0.5.
        taps[0] += 0.5;
        taps[1 % t] += 0.5;
    }
    let suma: f64 = taps.iter().sum();
    for v in taps.iter_mut() {
        *v /= suma;
    }
    let mut salida = vec![0f64; t];
    for (k, s) in salida.iter_mut().enumerate() {
        let (mut re, mut im) = (0f64, 0f64);
        for (n, &v) in taps.iter().enumerate() {
            if v == 0.0 {
                continue;
            }
            let ang = -2.0 * PI * ((k * n) % t) as f64 / t as f64;
            re += v * ang.cos();
            im += v * ang.sin();
        }
        *s = re * re + im * im;
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

fn psf_utilizable(psf: &crate::deepsky_psf::MoffatPsf, t: usize) -> bool {
    let fx = psf.fwhm_x as f64;
    let fy = psf.fwhm_y as f64;
    let beta = psf.beta as f64;
    let theta = psf.theta as f64;
    fx.is_finite()
        && fy.is_finite()
        && beta.is_finite()
        && theta.is_finite()
        && fx > 0.05
        && fy > 0.05
        && fx < t as f64 / 2.0
        && fy < t as f64 / 2.0
        && beta > 1.0
}

/// H(k) = FFT 2D del kernel PSF rasterizado, incrustado con su centro en el
/// píxel (0,0) del tile y wrap-around de los negativos (fase cero).
fn h_de_psf(psf: &crate::deepsky_psf::MoffatPsf, t: usize, fft: &mut Fft2d) -> Vec<Complex<f64>> {
    let fwhm_max: f64 = (psf.fwhm_x as f64).max(psf.fwhm_y as f64);
    let mut lado = (8.0 * fwhm_max).ceil() as usize;
    if lado % 2 == 0 {
        lado += 1;
    }
    lado = lado.max(9);
    // El kernel debe caber en el tile; T es potencia de 2 (par) => T-1 impar.
    lado = lado.min(t - 1);
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
) -> Option<(f64, Vec<f64>)> {
    let d_max = d.iter().cloned().fold(0.0f64, f64::max);
    if !(d_max > 0.0) || !ref_inv_var.is_finite() || ref_inv_var <= 0.0 {
        return None;
    }
    let umbral = EPS_D * d_max;
    let mut fwhm = fwhm_inicial.max(FWHM_T_MIN);
    for _ in 0..GAMMA_MAX_ITERS {
        let g1 = gamma_1d(t, fwhm);
        let mut amp_max = 0.0f64;
        for ky in 0..t {
            let gy = g1[ky];
            for kx in 0..t {
                let dv = d[ky * t + kx];
                if dv > umbral {
                    let g = gy * g1[kx];
                    let a = g * g * ref_inv_var / dv;
                    if a > amp_max {
                        amp_max = a;
                    }
                }
            }
        }
        if amp_max <= AMP_MAX {
            let mut plano = vec![0f64; t * t];
            for ky in 0..t {
                for kx in 0..t {
                    plano[ky * t + kx] = g1[ky] * g1[kx];
                }
            }
            return Some((fwhm, plano));
        }
        fwhm += GAMMA_PASO;
    }
    None
}

/// Percentil por rango más próximo sobre una copia ordenada.
fn percentil(valores: &[f64], p: f64) -> f64 {
    let mut v: Vec<f64> = valores.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
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
    let mut salida = vec![0f64; t * t];
    // Columnas fuente (reflejadas) y su rango contiguo [lo, hi).
    let columnas: Vec<usize> = (0..t).map(|tx| reflejar(ox + tx as isize, w)).collect();
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

// ---------------------------------------------------------------------------
// Motor principal
// ---------------------------------------------------------------------------

enum ModoCanal {
    /// GLS espectral: Γ elegida, denominador D+ε′ y varianza escalar por tile.
    Gls {
        gamma: Vec<f64>,
        denominador: Vec<f64>,
        var_tile: f64,
        fwhm_t: f64,
    },
    /// Media ponderada plana con pesos 1/σ′² (equivalente Lite).
    Plano,
}

pub(crate) fn combine_full(
    inputs: &NfFullInputs,
    progress: &mut dyn FnMut(&str, usize, usize),
) -> Result<NfFullOutput, String> {
    let (w, h, ch, n) = (inputs.w, inputs.h, inputs.ch, inputs.n_frames);
    if n == 0 || w == 0 || h == 0 || ch == 0 {
        return Err("NF-Full: geometría o número de frames vacíos".into());
    }
    if inputs.sigmas.len() != n || inputs.psfs.len() != n {
        return Err(format!(
            "NF-Full: sigmas ({}) / psfs ({}) no cuadran con n_frames ({n})",
            inputs.sigmas.len(),
            inputs.psfs.len()
        ));
    }

    let t = lado_tile(w, h);
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

    // Suma de ventanas² acumulada por píxel (igual para todos los canales).
    // Con la rejilla extendida la POU es exacta (~1.0 en todas partes); se
    // mantiene la normalización explícita por robustez numérica.
    let mut wgt_acc = vec![0f64; w * h];
    for &oy in &orig_y {
        for &ox in &orig_x {
            let (ty0, ty1) = rango_valido(oy, h);
            let (tx0, tx1) = rango_valido(ox, w);
            for ty in ty0..ty1 {
                let base = (oy + ty as isize) as usize * w;
                for tx in tx0..tx1 {
                    wgt_acc[base + (ox + tx as isize) as usize] += hann1[ty] * hann1[tx];
                }
            }
        }
    }

    let mut fft = Fft2d::nueva(t);

    // |L(k)|² separable con suelo (común a todos los frames y canales).
    let l2_1 = l2_interpolador_1d(t, inputs.lanczos);
    let mut inv_l2 = vec![0f64; t * t];
    for ky in 0..t {
        for kx in 0..t {
            inv_l2[ky * t + kx] = 1.0 / (l2_1[ky] * l2_1[kx]).max(L2_SUELO);
        }
    }

    // H_i(k): las PSF son constantes espacialmente => una FFT por frame,
    // compartida por todos los tiles y canales (~T²·16 bytes por frame).
    let psfs_ok = inputs.psfs.iter().all(|p| psf_utilizable(p, t));
    let hs: Vec<Vec<Complex<f64>>> = if psfs_ok {
        inputs
            .psfs
            .iter()
            .map(|p| h_de_psf(p, t, &mut fft))
            .collect()
    } else {
        Vec::new()
    };

    // FWHM inicial de Γ: percentil 20 de las FWHM medias de los frames.
    let fwhms: Vec<f64> = inputs.psfs.iter().map(fwhm_media).collect();
    let fwhm_ini = if psfs_ok { percentil(&fwhms, 0.2) } else { 0.0 };

    let mut sci_acc = vec![0f64; w * h * ch];
    let mut var_acc = vec![0f64; w * h * ch];
    let mut tiles_fallback = 0usize;
    let mut target_fwhm = 0.0f32;
    let mut hechos = 0usize;

    // Canales secuenciales: σ′ (y por tanto D y Γ) dependen del canal.
    for c in 0..ch {
        let idx_sigma = c.min(2);
        let sig2: Vec<f64> = (0..n)
            .map(|i| {
                let s = inputs.sigmas[i][idx_sigma] as f64;
                (s * s).max(SIGMA2_SUELO)
            })
            .collect();
        let ref_inv_var: f64 = sig2.iter().map(|v| 1.0 / v).sum();
        let var_plano = 1.0 / ref_inv_var;

        // D(k) = Σ_i |H_i(k)|²/S_i(k): independiente de los datos, se calcula
        // una única vez por canal y vale para todos los tiles.
        let modo = if psfs_ok {
            let mut d = vec![0f64; t * t];
            for i in 0..n {
                let inv_s = 1.0 / sig2[i];
                let hi = &hs[i];
                for k in 0..t * t {
                    d[k] += hi[k].norm_sqr() * inv_s * inv_l2[k];
                }
            }
            match elegir_gamma(&d, t, ref_inv_var, fwhm_ini) {
                Some((fwhm_t, gamma)) => {
                    let d_max = d.iter().cloned().fold(0.0f64, f64::max);
                    let eps = EPS_D * d_max;
                    let denominador: Vec<f64> = d.iter().map(|&v| v + eps).collect();
                    // Varianza escalar del tile (Parseval, ver doc del módulo):
                    // (1/T²)·Σ_k |Γ|²·t², con t(k)=D/(D+ε) => Γ²·D/(D+ε)².
                    let mut var_tile = 0.0f64;
                    for k in 0..t * t {
                        var_tile += gamma[k] * gamma[k] * d[k] / (denominador[k] * denominador[k]);
                    }
                    var_tile /= (t * t) as f64;
                    // La FWHM objetivo reportada es la mayor entre los
                    // canales que convergieron (conservadora si difieren).
                    if fwhm_t as f32 > target_fwhm {
                        target_fwhm = fwhm_t as f32;
                    }
                    ModoCanal::Gls {
                        gamma,
                        denominador,
                        var_tile,
                        fwhm_t,
                    }
                }
                None => ModoCanal::Plano,
            }
        } else {
            ModoCanal::Plano
        };

        for &oy in &orig_y {
            for &ox in &orig_x {
                crate::pipeline::cancellation_checkpoint(inputs.cancel, "NF-Full")?;
                let (ty0, ty1) = rango_valido(oy, h);
                let (tx0, tx1) = rango_valido(ox, w);
                match &modo {
                    ModoCanal::Gls {
                        gamma,
                        denominador,
                        var_tile,
                        ..
                    } => {
                        // Q(k) = Σ_i conj(H_i)·Y_i/S_i sobre el tile ventaneado.
                        let mut q = vec![Complex::<f64>::default(); t * t];
                        let mut buf = vec![Complex::<f64>::default(); t * t];
                        for i in 0..n {
                            let datos = leer_tile_canal(
                                inputs.warped,
                                i,
                                w,
                                h,
                                ch,
                                c,
                                ox,
                                oy,
                                t,
                            )?;
                            for ty in 0..t {
                                let ay = a1[ty];
                                for tx in 0..t {
                                    buf[ty * t + tx] =
                                        Complex::new(datos[ty * t + tx] * ay * a1[tx], 0.0);
                                }
                            }
                            fft.fft(&mut buf);
                            let inv_s = 1.0 / sig2[i];
                            let hi = &hs[i];
                            for k in 0..t * t {
                                q[k] += hi[k].conj() * buf[k] * (inv_s * inv_l2[k]);
                            }
                        }
                        // SCI(k) = Γ·Q/(D+ε) = t(k)·Γ·Q/D; IFFT y overlap-add.
                        for k in 0..t * t {
                            buf[k] = q[k] * (gamma[k] / denominador[k]);
                        }
                        fft.ifft(&mut buf);
                        for ty in ty0..ty1 {
                            let ay = a1[ty];
                            let hy = hann1[ty];
                            let base = (oy + ty as isize) as usize * w;
                            for tx in tx0..tx1 {
                                let x = (ox + tx as isize) as usize;
                                let p = (base + x) * ch + c;
                                // Síntesis sqrt-Hann; el análisis ya iba en los datos.
                                sci_acc[p] += buf[ty * t + tx].re * ay * a1[tx];
                                var_acc[p] += var_tile * hy * hann1[tx];
                            }
                        }
                    }
                    ModoCanal::Plano => {
                        // Fallback: media ponderada plana 1/σ′² (equivalente
                        // Lite), misma mezcla POU con la ventana Hann completa.
                        let mut acumulado = vec![0f64; t * t];
                        for i in 0..n {
                            let datos = leer_tile_canal(
                                inputs.warped,
                                i,
                                w,
                                h,
                                ch,
                                c,
                                ox,
                                oy,
                                t,
                            )?;
                            let peso = 1.0 / sig2[i];
                            for (a, d) in acumulado.iter_mut().zip(datos.iter()) {
                                *a += d * peso;
                            }
                        }
                        for ty in ty0..ty1 {
                            let hy = hann1[ty];
                            let base = (oy + ty as isize) as usize * w;
                            for tx in tx0..tx1 {
                                let x = (ox + tx as isize) as usize;
                                let p = (base + x) * ch + c;
                                let hann = hy * hann1[tx];
                                sci_acc[p] += acumulado[ty * t + tx] * var_plano * hann;
                                var_acc[p] += var_plano * hann;
                            }
                        }
                        tiles_fallback += 1;
                    }
                }
                hechos += 1;
                progress("tiles", hechos, tiles_total);
            }
        }
    }

    // Normalización final por la suma de ventanas² acumulada.
    let mut sci = vec![0f32; w * h * ch];
    let mut var_map = vec![0f32; w * h * ch];
    for p in 0..w * h {
        let g = wgt_acc[p].max(1e-12);
        for c in 0..ch {
            sci[p * ch + c] = (sci_acc[p * ch + c] / g) as f32;
            var_map[p * ch + c] = (var_acc[p * ch + c] / g) as f32;
        }
    }

    Ok(NfFullOutput {
        sci,
        var_map,
        target_fwhm,
        tiles_total,
        tiles_fallback,
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
        let dir = std::env::temp_dir().join(format!(
            "zas-nf-full-test-{}-{}",
            tag,
            std::process::id()
        ));
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
        let inputs = NfFullInputs {
            warped: store,
            n_frames: n,
            w,
            h,
            ch: 1,
            sigmas,
            psfs,
            lanczos,
            cancel: &cancel,
        };
        combine_full(&inputs, &mut |_, _, _| {}).expect("combine_full")
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
        assert!(salida.target_fwhm >= 2.2, "target_fwhm={}", salida.target_fwhm);
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
                out[y * w + x] = 0.25
                    * (img[y * w + x] + img[y * w + x1] + img[y1 * w + x] + img[y1 * w + x1]);
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
        let media_global =
            salida.sci.iter().map(|&v| v as f64).sum::<f64>() / (w * h) as f64;
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
            let l2 = l2_interpolador_1d(64, lanczos);
            assert!((l2[0] - 1.0).abs() < 1e-12, "L(0) != 1 (lanczos={lanczos})");
        }
        // Γ(0) = 1 siempre.
        let g = gamma_1d(64, 3.7);
        assert!((g[0] - 1.0).abs() < 1e-15);
    }
}
