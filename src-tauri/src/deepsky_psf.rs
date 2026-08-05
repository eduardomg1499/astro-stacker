//! F5: PSF Moffat elíptica por frame + campo espacial con holdout.
//!
//! Ajusta la PSF de un frame de cielo profundo como Moffat elíptica
//! I(x,y) = fondo + A·(1+u)^(-beta), con u = (x'/ax)^2 + (y'/ay)^2 y (x',y')
//! la rotación por theta del offset al centro. El resultado alimenta la FFT
//! numérica de NF-Full (F6) vía `rasterize_psf`.
//!
//! Decisiones de diseño documentadas:
//! - Ajuste por estrella: Levenberg-Marquardt con jacobiano ANALÍTICO sobre un
//!   recorte 17x17, resolviendo las ecuaciones normales con ds_solve_linear_n.
//!   El modelo se integra por píxel con supersampling 4x4 (regla del punto
//!   medio): evaluar solo en el centro del píxel sesga la FWHM ~+2 % (la caja
//!   de 1 px añade ~1/12 px^2 al segundo momento), inaceptable frente al gate
//!   del 3 %. Con 4x4 el sesgo residual queda < 0.15 %.
//! - beta en 2 etapas (elección documentada, sugerida por el plan §F5): el
//!   ajuste con beta libre por estrella es inestable en estrellas débiles por
//!   la degeneración beta<->alpha<->fondo con alas truncadas a r=8. Etapa A:
//!   beta libre solo en las <=12 más brillantes; beta del frame = mediana de
//!   los ajustes válidos (2.5 de reserva si ninguno converge). Etapa B: TODAS
//!   las estrellas con beta fijo. Consecuencia: beta no varía espacialmente
//!   dentro del frame; sus coeficientes espaciales degeneran a constante.
//! - Representación canónica del par (fwhm_x, fwhm_y, theta): theta se
//!   envuelve a [-pi/4, pi/4) intercambiando ejes si hace falta (el modelo es
//!   invariante bajo (ax,ay,th) -> (ay,ax,th+-pi/2)). NO se fuerza
//!   fwhm_x >= fwhm_y: ese convenio sesga sistemáticamente la elipticidad al
//!   alza en PSFs casi circulares (el ruido siempre caería en el "eje mayor").
//!   Caso borde: campos elongados a ~45 grados exactos pueden alternar de
//!   representación entre estrellas; documentado, no afecta a fwhm_mean.
//! - Filtros de censo: dentro de márgenes (recorte completo en el frame),
//!   aisladas (sin vecina a < 10 px en la lista de entrada), no saturadas
//!   (pico 3x3 < 60000) y con señal (pico - fondo local > 10·sigma_MRS).
//! - Escalera de censo (plan §6.4) sobre las estrellas AJUSTADAS con éxito
//!   (no las detectadas): son las que realmente sostienen el modelo espacial.
//! - Holdout determinista: con las estrellas ordenadas por flujo descendente,
//!   el índice i va al holdout si i % k == 0, k = round(1/holdout_fraction)
//!   (>= 2). Sesgo = mediana((fwhm_modelo_at - fwhm_medida)/fwhm_medida).
//! - Campo espacial: mínimos cuadrados por parámetro contra la base
//!   [1, xn, yn] (Linear) o [1, xn, yn, xn^2, xn*yn, yn^2] (Quadratic) con
//!   UNA pasada de rechazo a 3·sigma (sigma = 1.4826·MAD de los residuos).
//!   Con 80+ estrellas se ajustan ambos órdenes y se elige Quadratic solo si
//!   REDUCE la mediana de |error relativo de FWHM| en el holdout (si no hay
//!   holdout se prefiere Linear, más seguro frente al sobreajuste).
//! - Coordenadas normalizadas: xn = x/w, yn = y/h (0..1); `base` es el modelo
//!   evaluado en el centro del campo (0.5, 0.5).
//! - Degradación con motivo: si el LS Linear/Quadratic queda singular se cae
//!   al orden inferior (Quadratic -> Linear -> Constant) sin fallar.

#![allow(dead_code)]

use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};

// ---------------------------------------------------------------------------
// Tipos públicos (contrato consumido por F6 / nebula_fusion_full)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub(crate) struct MoffatPsf {
    pub fwhm_x: f32,
    pub fwhm_y: f32,
    /// Rotación del eje x' del modelo respecto al eje x de la imagen, en
    /// radianes, canónica en [-pi/4, pi/4) (ver doc del módulo).
    pub theta: f32,
    pub beta: f32,
}

impl MoffatPsf {
    /// FWHM media (aritmética) de los dos ejes.
    pub(crate) fn fwhm_mean(&self) -> f32 {
        0.5 * (self.fwhm_x + self.fwhm_y)
    }

    /// Elipticidad 1 - min/max (0 = circular).
    pub(crate) fn ellipticity(&self) -> f32 {
        let mx = self.fwhm_x.max(self.fwhm_y);
        let mn = self.fwhm_x.min(self.fwhm_y);
        if mx <= 0.0 {
            0.0
        } else {
            1.0 - mn / mx
        }
    }
}

/// Modelo espacial de la PSF sobre el campo. Coeficientes por parámetro en el
/// orden [fwhm_x, fwhm_y, theta, beta], evaluados en coordenadas normalizadas
/// (xn, yn) en 0..1.
pub(crate) enum PsfSpatial {
    Constant,
    /// p = c0 + c1·xn + c2·yn.
    Linear([[f32; 3]; 4]),
    /// p = c0 + c1·xn + c2·yn + c3·xn^2 + c4·xn·yn + c5·yn^2
    /// (base polinómica 2D completa de grado 2; solo con 80+ estrellas y si
    /// mejora el holdout frente a Linear — ver doc del módulo).
    Quadratic([[f32; 6]; 4]),
}

pub(crate) struct FramePsf {
    /// PSF en el centro del campo (xn = yn = 0.5).
    pub base: MoffatPsf,
    pub spatial: PsfSpatial,
}

impl FramePsf {
    /// PSF del modelo en (xn, yn) normalizadas 0..1. Los parámetros evaluados
    /// se sanean (FWHM > 0, beta >= 1.05) por si el polinomio extrapola.
    pub(crate) fn at(&self, xn: f32, yn: f32) -> MoffatPsf {
        match &self.spatial {
            PsfSpatial::Constant => self.base,
            PsfSpatial::Linear(c) => {
                let b = [1.0f32, xn, yn];
                sanea_psf(
                    dot(&c[0], &b),
                    dot(&c[1], &b),
                    dot(&c[2], &b),
                    dot(&c[3], &b),
                )
            }
            PsfSpatial::Quadratic(c) => {
                let b = [1.0f32, xn, yn, xn * xn, xn * yn, yn * yn];
                sanea_psf(
                    dot(&c[0], &b),
                    dot(&c[1], &b),
                    dot(&c[2], &b),
                    dot(&c[3], &b),
                )
            }
        }
    }
}

pub(crate) struct PsfFitReport {
    /// Estrellas empleadas en el ajuste espacial (tras separar el holdout).
    pub stars_used: usize,
    pub stars_holdout: usize,
    /// Sesgo relativo de FWHM en el holdout:
    /// mediana((fwhm_modelo - fwhm_medida)/fwhm_medida). 0 si no hay holdout.
    pub holdout_fwhm_bias: f32,
    /// 0 = Constant, 1 = Linear, 2 = Quadratic.
    pub spatial_order: u8,
}

// ---------------------------------------------------------------------------
// Utilidades
// ---------------------------------------------------------------------------

#[inline]
fn dot<const N: usize>(c: &[f32; N], b: &[f32; N]) -> f32 {
    c.iter().zip(b.iter()).map(|(a, v)| a * v).sum()
}

#[inline]
fn sanea_psf(fx: f32, fy: f32, th: f32, be: f32) -> MoffatPsf {
    MoffatPsf {
        fwhm_x: fx.clamp(0.2, 60.0),
        fwhm_y: fy.clamp(0.2, 60.0),
        theta: th,
        beta: be.clamp(1.05, 12.0),
    }
}

/// alpha del perfil Moffat a partir de la FWHM: alpha = FWHM/(2*sqrt(2^(1/beta)-1)).
#[inline]
fn alfa_de_fwhm(fwhm: f64, beta: f64) -> f64 {
    fwhm / (2.0 * (2f64.powf(1.0 / beta) - 1.0).sqrt())
}

#[inline]
fn fwhm_de_alfa(alfa: f64, beta: f64) -> f64 {
    alfa * 2.0 * (2f64.powf(1.0 / beta) - 1.0).sqrt()
}

/// Mediana in situ (ordena el slice). 0 si está vacío.
fn mediana(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

// ---------------------------------------------------------------------------
// Ajuste Moffat elíptico por estrella (Levenberg-Marquardt)
// ---------------------------------------------------------------------------

/// Radio del recorte de ajuste (ventana 17x17).
const RADIO: i64 = 8;
/// Supersampling del modelo dentro del ajuste (punto medio 4x4).
const SS_AJUSTE: usize = 4;
/// Pico (3x3) a partir del cual la estrella se considera saturada.
const PICO_SATURACION: f32 = 60_000.0;

/// Resultado del ajuste de una estrella individual (parámetros canónicos).
#[derive(Clone, Copy, Debug)]
struct EstrellaAjustada {
    x: f64,
    y: f64,
    fwhm_x: f64,
    fwhm_y: f64,
    theta: f64,
    beta: f64,
}

impl EstrellaAjustada {
    fn fwhm_media(&self) -> f64 {
        0.5 * (self.fwhm_x + self.fwhm_y)
    }
}

/// Modelo Moffat elíptico integrado en el píxel (px, py) con supersampling
/// SS_AJUSTE x SS_AJUSTE, junto con su jacobiano analítico respecto a
/// p = [fondo, A, x0, y0, ax, ay, theta, beta].
fn modelo_pixel(p: &[f64; 8], px: f64, py: f64) -> (f64, [f64; 8]) {
    let [fondo, a, x0, y0, ax, ay, th, beta] = *p;
    let (s, c) = th.sin_cos();
    let inv_ax2 = 1.0 / (ax * ax);
    let inv_ay2 = 1.0 / (ay * ay);
    let paso = 1.0 / SS_AJUSTE as f64;
    let mut m = 0.0f64;
    let mut j = [0.0f64; 8];
    for sy in 0..SS_AJUSTE {
        let dy = py - 0.5 + (sy as f64 + 0.5) * paso - y0;
        for sx in 0..SS_AJUSTE {
            let dx = px - 0.5 + (sx as f64 + 0.5) * paso - x0;
            let xp = c * dx + s * dy;
            let yp = -s * dx + c * dy;
            let u = xp * xp * inv_ax2 + yp * yp * inv_ay2;
            let unop = 1.0 + u;
            let base = unop.powf(-beta);
            // dI/du = -A*beta*(1+u)^(-beta-1)
            let dmu = -a * beta * base / unop;
            m += fondo + a * base;
            j[0] += 1.0;
            j[1] += base;
            // du/dx0 = -2*(xp*c/ax^2 - yp*s/ay^2); du/dy0 = -2*(xp*s/ax^2 + yp*c/ay^2)
            j[2] += dmu * (-2.0) * (xp * c * inv_ax2 - yp * s * inv_ay2);
            j[3] += dmu * (-2.0) * (xp * s * inv_ax2 + yp * c * inv_ay2);
            // du/dax = -2*xp^2/ax^3; du/day = -2*yp^2/ay^3
            j[4] += dmu * (-2.0 * xp * xp * inv_ax2 / ax);
            j[5] += dmu * (-2.0 * yp * yp * inv_ay2 / ay);
            // du/dth = 2*xp*yp*(1/ax^2 - 1/ay^2)
            j[6] += dmu * 2.0 * xp * yp * (inv_ax2 - inv_ay2);
            // dI/dbeta = -A*(1+u)^(-beta)*ln(1+u)
            j[7] += -a * base * unop.ln();
        }
    }
    let inv = 1.0 / (SS_AJUSTE * SS_AJUSTE) as f64;
    for v in j.iter_mut() {
        *v *= inv;
    }
    (m * inv, j)
}

/// Suma de cuadrados de residuos del modelo sobre el recorte (sin jacobiano).
fn sse_recorte(luma: &[f32], w: usize, cx: i64, cy: i64, p: &[f64; 8]) -> f64 {
    let mut sse = 0.0f64;
    for dy in -RADIO..=RADIO {
        for dx in -RADIO..=RADIO {
            let px = (cx + dx) as f64;
            let py = (cy + dy) as f64;
            let (m, _) = modelo_pixel(p, px, py);
            let r = luma[(cy + dy) as usize * w + (cx + dx) as usize] as f64 - m;
            sse += r * r;
        }
    }
    sse
}

/// Mediana del anillo exterior del recorte 17x17: fondo local robusto.
fn fondo_local(luma: &[f32], w: usize, cx: i64, cy: i64) -> f64 {
    let mut borde: Vec<f64> = Vec::with_capacity(64);
    for dy in -RADIO..=RADIO {
        for dx in -RADIO..=RADIO {
            if dx.abs() < RADIO && dy.abs() < RADIO {
                continue;
            }
            borde.push(luma[(cy + dy) as usize * w + (cx + dx) as usize] as f64);
        }
    }
    mediana(&mut borde)
}

/// Aplica un paso LM con recortes de seguridad (evita saltos que descarrilan
/// el ajuste en estrellas débiles). `d` tiene 7 u 8 componentes según beta.
fn aplica_paso(p: &[f64; 8], d: &[f64], x_ini: f64, y_ini: f64, beta_libre: bool) -> [f64; 8] {
    let mut q = *p;
    q[0] += d[0];
    q[1] = (q[1] + d[1].clamp(-0.7 * p[1].abs(), 0.7 * p[1].abs())).max(1e-3);
    q[2] = (q[2] + d[2].clamp(-1.0, 1.0)).clamp(x_ini - 2.5, x_ini + 2.5);
    q[3] = (q[3] + d[3].clamp(-1.0, 1.0)).clamp(y_ini - 2.5, y_ini + 2.5);
    q[4] = (q[4] + d[4].clamp(-0.5 * p[4], 0.5 * p[4])).clamp(0.4, 25.0);
    q[5] = (q[5] + d[5].clamp(-0.5 * p[5], 0.5 * p[5])).clamp(0.4, 25.0);
    q[6] += d[6].clamp(-0.35, 0.35);
    if beta_libre {
        q[7] = (q[7] + d[7].clamp(-0.6, 0.6)).clamp(1.1, 10.0);
    }
    q
}

/// Canonicaliza (fwhm_x, fwhm_y, theta): theta en [-pi/4, pi/4) intercambiando
/// ejes si hace falta (el modelo es invariante bajo el intercambio + giro de
/// pi/2). No se impone fwhm_x >= fwhm_y (ver doc del módulo: sesgaría).
fn canonicaliza(fx: f64, fy: f64, th: f64) -> (f64, f64, f64) {
    let mut th = th % PI;
    if th >= FRAC_PI_2 {
        th -= PI;
    } else if th < -FRAC_PI_2 {
        th += PI;
    }
    let (mut fx, mut fy) = (fx, fy);
    if th >= FRAC_PI_4 {
        std::mem::swap(&mut fx, &mut fy);
        th -= FRAC_PI_2;
    } else if th < -FRAC_PI_4 {
        std::mem::swap(&mut fx, &mut fy);
        th += FRAC_PI_2;
    }
    (fx, fy, th)
}

/// Ajuste Moffat elíptico de UNA estrella por Levenberg-Marquardt sobre el
/// recorte 17x17 centrado en la posición redondeada. `beta_fijo = Some(b)`
/// ajusta 7 parámetros (etapa B); `None` ajusta los 8 (etapa A).
fn ajusta_estrella(
    luma: &[f32],
    w: usize,
    h: usize,
    x_ini: f64,
    y_ini: f64,
    beta_fijo: Option<f64>,
) -> Option<EstrellaAjustada> {
    let cx = x_ini.round() as i64;
    let cy = y_ini.round() as i64;
    if cx < RADIO || cy < RADIO || cx + RADIO >= w as i64 || cy + RADIO >= h as i64 {
        return None;
    }

    // Inicialización: fondo del anillo, pico 3x3 y sigma por momentos.
    let fondo0 = fondo_local(luma, w, cx, cy);
    let mut pico = f32::MIN;
    for dy in -1i64..=1 {
        for dx in -1i64..=1 {
            pico = pico.max(luma[(cy + dy) as usize * w + (cx + dx) as usize]);
        }
    }
    let a0 = (pico as f64 - fondo0).max(1.0);
    let (mut suma, mut sx, mut sy, mut sr2) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for dy in -RADIO..=RADIO {
        for dx in -RADIO..=RADIO {
            let v = (luma[(cy + dy) as usize * w + (cx + dx) as usize] as f64 - fondo0).max(0.0);
            suma += v;
            sx += v * (cx + dx) as f64;
            sy += v * (cy + dy) as f64;
            sr2 += v * (dx * dx + dy * dy) as f64;
        }
    }
    if suma <= 1e-6 {
        return None;
    }
    let beta0 = beta_fijo.unwrap_or(2.5);
    let sigma0 = (sr2 / suma / 2.0).sqrt().clamp(0.6, 5.0);
    let alfa0 = alfa_de_fwhm((2.354_820 * sigma0).clamp(1.0, 12.0), beta0).clamp(0.5, 10.0);
    let mut p: [f64; 8] = [
        fondo0,
        a0,
        (sx / suma).clamp(x_ini - 2.5, x_ini + 2.5),
        (sy / suma).clamp(y_ini - 2.5, y_ini + 2.5),
        alfa0,
        alfa0,
        0.0,
        beta0,
    ];
    let beta_libre = beta_fijo.is_none();
    let n = if beta_libre { 8 } else { 7 };

    // Bucle Levenberg-Marquardt con aceptación por SSE.
    let mut lambda = 1e-3f64;
    let mut sse = sse_recorte(luma, w, cx, cy, &p);
    for _ in 0..40 {
        // Ecuaciones normales JtJ / Jtr del recorte.
        let mut jtj = [[0.0f64; 8]; 8];
        let mut jtr = [0.0f64; 8];
        for dy in -RADIO..=RADIO {
            for dx in -RADIO..=RADIO {
                let (m, jac) = modelo_pixel(&p, (cx + dx) as f64, (cy + dy) as f64);
                let r = luma[(cy + dy) as usize * w + (cx + dx) as usize] as f64 - m;
                for a in 0..n {
                    jtr[a] += jac[a] * r;
                    for b in a..n {
                        jtj[a][b] += jac[a] * jac[b];
                    }
                }
            }
        }
        for a in 0..n {
            for b in 0..a {
                jtj[a][b] = jtj[b][a];
            }
        }
        let mut diag_max = 0.0f64;
        for (a, fila) in jtj.iter().enumerate().take(n) {
            diag_max = diag_max.max(fila[a].abs());
        }

        // Intentos con amortiguación creciente hasta reducir el SSE.
        let mut mejorado = false;
        let mut convergido = false;
        for _ in 0..8 {
            let mut m = vec![vec![0.0f64; n + 1]; n];
            for a in 0..n {
                for b in 0..n {
                    m[a][b] = jtj[a][b];
                }
                // Amortiguación relativa + término absoluto escalado: mantiene
                // el sistema no singular aunque una columna sea nula (p.ej.
                // theta con PSF exactamente circular).
                m[a][a] = jtj[a][a] * (1.0 + lambda) + 1e-9 * (diag_max + 1.0);
                m[a][n] = jtr[a];
            }
            if let Some(d) = crate::ds_solve_linear_n(&mut m, n) {
                let q = aplica_paso(&p, &d, x_ini, y_ini, beta_libre);
                let sse_q = sse_recorte(luma, w, cx, cy, &q);
                if sse_q.is_finite() && sse_q < sse {
                    convergido = d[2].abs() < 1e-3
                        && d[3].abs() < 1e-3
                        && d[4].abs() < 1e-3
                        && d[5].abs() < 1e-3
                        && (!beta_libre || d[7].abs() < 1e-3);
                    p = q;
                    sse = sse_q;
                    lambda = (lambda / 3.0).max(1e-9);
                    mejorado = true;
                    break;
                }
            }
            lambda *= 10.0;
            if lambda > 1e8 {
                break;
            }
        }
        if !mejorado || convergido {
            break;
        }
    }

    // Validación: parámetros finitos, no pegados a los topes ni desplazados.
    if !p.iter().all(|v| v.is_finite()) {
        return None;
    }
    let (ax, ay) = (p[4], p[5]);
    if !(0.45..24.0).contains(&ax) || !(0.45..24.0).contains(&ay) {
        return None;
    }
    if (p[2] - x_ini).abs() > 2.4 || (p[3] - y_ini).abs() > 2.4 {
        return None;
    }
    let beta = p[7];
    if beta_libre && !(1.15..9.5).contains(&beta) {
        return None;
    }
    let (fx, fy, th) = canonicaliza(fwhm_de_alfa(ax, beta), fwhm_de_alfa(ay, beta), p[6]);
    Some(EstrellaAjustada {
        x: p[2],
        y: p[3],
        fwhm_x: fx,
        fwhm_y: fy,
        theta: th,
        beta,
    })
}

// ---------------------------------------------------------------------------
// Ajuste espacial por parámetro (LS con 1 pasada de rechazo a 3 sigma)
// ---------------------------------------------------------------------------

/// Base polinómica del modelo espacial: orden 1 -> [1, xn, yn];
/// orden 2 -> [1, xn, yn, xn^2, xn*yn, yn^2].
fn base_espacial(xn: f64, yn: f64, orden: usize) -> Vec<f64> {
    if orden == 1 {
        vec![1.0, xn, yn]
    } else {
        vec![1.0, xn, yn, xn * xn, xn * yn, yn * yn]
    }
}

#[inline]
fn eval_espacial(coef: &[f64], xn: f64, yn: f64, orden: usize) -> f64 {
    base_espacial(xn, yn, orden)
        .iter()
        .zip(coef.iter())
        .map(|(b, c)| b * c)
        .sum()
}

/// Mínimos cuadrados de un parámetro escalar contra (xn, yn) con UNA pasada
/// de rechazo a 3·sigma (sigma = 1.4826·MAD de los residuos). `pts` son
/// (xn, yn, valor). None si el sistema queda singular o hay pocos puntos.
fn ls_param(pts: &[(f64, f64, f64)], orden: usize) -> Option<Vec<f64>> {
    let nt = if orden == 1 { 3 } else { 6 };
    if pts.len() < nt + 2 {
        return None;
    }
    let ajusta = |mask: &[bool]| -> Option<Vec<f64>> {
        let mut m = vec![vec![0.0f64; nt + 1]; nt];
        let mut cnt = 0usize;
        for (i, &(xn, yn, v)) in pts.iter().enumerate() {
            if !mask[i] {
                continue;
            }
            cnt += 1;
            let b = base_espacial(xn, yn, orden);
            for r in 0..nt {
                for c in 0..nt {
                    m[r][c] += b[r] * b[c];
                }
                m[r][nt] += b[r] * v;
            }
        }
        if cnt < nt + 2 {
            return None;
        }
        crate::ds_solve_linear_n(&mut m, nt)
    };
    let coef = ajusta(&vec![true; pts.len()])?;
    let res: Vec<f64> = pts
        .iter()
        .map(|&(xn, yn, v)| v - eval_espacial(&coef, xn, yn, orden))
        .collect();
    let mut abs: Vec<f64> = res.iter().map(|r| r.abs()).collect();
    let sigma = (1.4826 * mediana(&mut abs)).max(1e-9);
    let mask: Vec<bool> = res.iter().map(|r| r.abs() <= 3.0 * sigma).collect();
    if mask.iter().filter(|&&b| b).count() < nt + 2 {
        return Some(coef); // el rechazo dejaría el sistema corto: conserva el primer ajuste
    }
    Some(ajusta(&mask).unwrap_or(coef))
}

/// Ajusta los 4 parámetros [fwhm_x, fwhm_y, theta, beta] contra (xn, yn) al
/// orden pedido. None si algún parámetro queda singular (el caller degrada).
fn ajusta_campo(
    estrellas: &[EstrellaAjustada],
    w: usize,
    h: usize,
    orden: usize,
) -> Option<[Vec<f64>; 4]> {
    let puntos = |sel: fn(&EstrellaAjustada) -> f64| -> Vec<(f64, f64, f64)> {
        estrellas
            .iter()
            .map(|e| (e.x / w as f64, e.y / h as f64, sel(e)))
            .collect()
    };
    Some([
        ls_param(&puntos(|e| e.fwhm_x), orden)?,
        ls_param(&puntos(|e| e.fwhm_y), orden)?,
        ls_param(&puntos(|e| e.theta), orden)?,
        ls_param(&puntos(|e| e.beta), orden)?,
    ])
}

/// Convierte los coeficientes f64 del LS al almacenamiento f32 del contrato.
fn coefs_a_f32<const N: usize>(coefs: &[Vec<f64>; 4]) -> [[f32; N]; 4] {
    let mut out = [[0.0f32; N]; 4];
    for (fila, c) in out.iter_mut().zip(coefs.iter()) {
        for (dst, src) in fila.iter_mut().zip(c.iter()) {
            *dst = *src as f32;
        }
    }
    out
}

/// Mediana de |error relativo de FWHM| del modelo sobre un conjunto de
/// estrellas (métrica de selección Linear vs Quadratic en el holdout).
fn error_fwhm_holdout(fp: &FramePsf, holdout: &[EstrellaAjustada], w: usize, h: usize) -> f64 {
    let mut errs: Vec<f64> = holdout
        .iter()
        .filter(|e| e.fwhm_media() > 1e-6)
        .map(|e| {
            let m = fp.at((e.x / w as f64) as f32, (e.y / h as f64) as f32);
            ((m.fwhm_mean() as f64 - e.fwhm_media()) / e.fwhm_media()).abs()
        })
        .collect();
    mediana(&mut errs)
}

/// Sesgo relativo (con signo) de FWHM del modelo en el holdout.
fn sesgo_fwhm_holdout(fp: &FramePsf, holdout: &[EstrellaAjustada], w: usize, h: usize) -> f64 {
    let mut rels: Vec<f64> = holdout
        .iter()
        .filter(|e| e.fwhm_media() > 1e-6)
        .map(|e| {
            let m = fp.at((e.x / w as f64) as f32, (e.y / h as f64) as f32);
            (m.fwhm_mean() as f64 - e.fwhm_media()) / e.fwhm_media()
        })
        .collect();
    mediana(&mut rels)
}

/// Modelo Constant: mediana robusta de cada parámetro.
fn campo_constante(estrellas: &[EstrellaAjustada]) -> FramePsf {
    let med = |sel: fn(&EstrellaAjustada) -> f64| -> f64 {
        let mut v: Vec<f64> = estrellas.iter().map(sel).collect();
        mediana(&mut v)
    };
    FramePsf {
        base: sanea_psf(
            med(|e| e.fwhm_x) as f32,
            med(|e| e.fwhm_y) as f32,
            med(|e| e.theta) as f32,
            med(|e| e.beta) as f32,
        ),
        spatial: PsfSpatial::Constant,
    }
}

/// Construye el FramePsf de un orden dado (base = modelo en el centro).
fn campo_de_orden(coefs: &[Vec<f64>; 4], orden: usize) -> FramePsf {
    let spatial = if orden == 1 {
        PsfSpatial::Linear(coefs_a_f32::<3>(coefs))
    } else {
        PsfSpatial::Quadratic(coefs_a_f32::<6>(coefs))
    };
    let mut fp = FramePsf {
        base: sanea_psf(0.0, 0.0, 0.0, 2.5),
        spatial,
    };
    fp.base = fp.at(0.5, 0.5);
    fp
}

// ---------------------------------------------------------------------------
// API principal
// ---------------------------------------------------------------------------

/// Ajusta la PSF del frame a partir de la lista de estrellas detectadas
/// (x, y, flujo — típicamente de ds_detect_stars). Escalera de censo
/// (plan §6.4): < 8 estrellas ajustadas -> None (el caller cae a GLS sin
/// PSF); 8-29 -> Constant; 30-79 -> Linear; 80+ -> Quadratic (o Linear si el
/// cuadrático no mejora el holdout). Reserva ~`holdout_fraction` de las
/// estrellas para validar; el reporte lleva el sesgo de FWHM en el holdout.
pub(crate) fn fit_frame_psf(
    luma: &[f32],
    w: usize,
    h: usize,
    stars: &[(f32, f32, f32)],
    holdout_fraction: f32,
) -> Option<(FramePsf, PsfFitReport)> {
    if w < 2 * RADIO as usize + 1 || h < 2 * RADIO as usize + 1 || luma.len() < w * h {
        return None;
    }

    // 1. Filtros de censo: márgenes, aislamiento, saturación y señal.
    let ruido = crate::ds_mrs_noise(luma, w, h).max(1e-6) as f64;
    let mut candidatas: Vec<(f64, f64, f32)> = Vec::new();
    for (i, &(x, y, flujo)) in stars.iter().enumerate() {
        if !x.is_finite() || !y.is_finite() || !(flujo > 0.0) {
            continue;
        }
        let cx = x.round() as i64;
        let cy = y.round() as i64;
        if cx < RADIO || cy < RADIO || cx + RADIO >= w as i64 || cy + RADIO >= h as i64 {
            continue;
        }
        // Aislamiento: sin vecina a < 10 px en TODA la lista de entrada.
        let aislada = stars
            .iter()
            .enumerate()
            .all(|(k, &(x2, y2, _))| k == i || (x - x2) * (x - x2) + (y - y2) * (y - y2) >= 100.0);
        if !aislada {
            continue;
        }
        // Saturación (pico 3x3) y señal mínima sobre el fondo local.
        let mut pico = f32::MIN;
        for dy in -1i64..=1 {
            for dx in -1i64..=1 {
                pico = pico.max(luma[(cy + dy) as usize * w + (cx + dx) as usize]);
            }
        }
        if pico >= PICO_SATURACION {
            continue;
        }
        if (pico as f64 - fondo_local(luma, w, cx, cy)) < 10.0 * ruido {
            continue;
        }
        candidatas.push((x as f64, y as f64, flujo));
    }
    // Orden determinista por flujo descendente (define etapa A y holdout).
    candidatas.sort_by(|a, b| b.2.total_cmp(&a.2));

    // 2. Etapa A: beta global — mediana de las <=12 más brillantes con beta libre.
    let mut betas: Vec<f64> = candidatas
        .iter()
        .take(12)
        .filter_map(|&(x, y, _)| ajusta_estrella(luma, w, h, x, y, None).map(|e| e.beta))
        .collect();
    // 2.5 de reserva si ninguna converge con beta libre (valor típico de seeing).
    let beta_frame = if betas.is_empty() {
        2.5
    } else {
        mediana(&mut betas)
    };

    // 3. Etapa B: todas las candidatas con beta fijo.
    let ajustadas: Vec<EstrellaAjustada> = candidatas
        .iter()
        .filter_map(|&(x, y, _)| ajusta_estrella(luma, w, h, x, y, Some(beta_frame)))
        .collect();
    let censo = ajustadas.len();
    if censo < 8 {
        return None;
    }

    // 4. Holdout determinista: índice % k == 0 (orden de flujo descendente).
    let f = holdout_fraction.clamp(0.0, 0.5);
    let k = if f < 1e-6 {
        usize::MAX
    } else {
        (1.0 / f as f64).round().max(2.0) as usize
    };
    let (mut usadas, mut holdout) = (Vec::new(), Vec::new());
    for (i, e) in ajustadas.iter().enumerate() {
        if k != usize::MAX && i % k == 0 {
            holdout.push(*e);
        } else {
            usadas.push(*e);
        }
    }

    // 5. Escalera de censo y ajuste espacial (con degradación documentada).
    let (fp, orden) = if censo < 30 {
        (campo_constante(&usadas), 0u8)
    } else if censo < 80 {
        match ajusta_campo(&usadas, w, h, 1) {
            Some(c) => (campo_de_orden(&c, 1), 1),
            None => (campo_constante(&usadas), 0),
        }
    } else {
        let lineal = ajusta_campo(&usadas, w, h, 1).map(|c| campo_de_orden(&c, 1));
        let cuad = ajusta_campo(&usadas, w, h, 2).map(|c| campo_de_orden(&c, 2));
        match (lineal, cuad) {
            (Some(l), Some(q)) => {
                // Quadratic solo si REDUCE el error de FWHM en el holdout.
                if !holdout.is_empty()
                    && error_fwhm_holdout(&q, &holdout, w, h)
                        < error_fwhm_holdout(&l, &holdout, w, h)
                {
                    (q, 2)
                } else {
                    (l, 1)
                }
            }
            (Some(l), None) => (l, 1),
            (None, Some(q)) => (q, 2),
            (None, None) => (campo_constante(&usadas), 0),
        }
    };

    let reporte = PsfFitReport {
        stars_used: usadas.len(),
        stars_holdout: holdout.len(),
        holdout_fwhm_bias: sesgo_fwhm_holdout(&fp, &holdout, w, h) as f32,
        spatial_order: orden,
    };
    Some((fp, reporte))
}

/// Rasteriza la PSF Moffat elíptica en una rejilla size x size (size IMPAR),
/// centrada en el píxel central, normalizada a SUMA 1, con supersampling 4x4
/// por píxel (regla del punto medio) — para la FFT numérica de NF-Full.
/// beta <= 1.05 se satura a 1.05 (mantiene la integral finita y evita alas
/// patológicas); FWHM <= 0 se satura a 0.1 px.
pub(crate) fn rasterize_psf(psf: &MoffatPsf, size: usize) -> Vec<f32> {
    assert!(
        size % 2 == 1 && size > 0,
        "rasterize_psf: size debe ser impar"
    );
    const SS: usize = 4;
    let beta = (psf.beta as f64).max(1.05);
    let ax = alfa_de_fwhm((psf.fwhm_x as f64).max(0.1), beta);
    let ay = alfa_de_fwhm((psf.fwhm_y as f64).max(0.1), beta);
    let (s, c) = (psf.theta as f64).sin_cos();
    let inv_ax2 = 1.0 / (ax * ax);
    let inv_ay2 = 1.0 / (ay * ay);
    let centro = (size / 2) as f64;
    let paso = 1.0 / SS as f64;
    let mut grid = vec![0.0f64; size * size];
    let mut suma = 0.0f64;
    for py in 0..size {
        for px in 0..size {
            let mut acc = 0.0f64;
            for sy in 0..SS {
                let dy = py as f64 - 0.5 + (sy as f64 + 0.5) * paso - centro;
                for sx in 0..SS {
                    let dx = px as f64 - 0.5 + (sx as f64 + 0.5) * paso - centro;
                    let xp = c * dx + s * dy;
                    let yp = -s * dx + c * dy;
                    acc += (1.0 + xp * xp * inv_ax2 + yp * yp * inv_ay2).powf(-beta);
                }
            }
            let v = acc / (SS * SS) as f64;
            grid[py * size + px] = v;
            suma += v;
        }
    }
    let inv = if suma > 0.0 { 1.0 / suma } else { 0.0 };
    grid.into_iter().map(|v| (v * inv) as f32).collect()
}

// ---------------------------------------------------------------------------
// Tests (gates F5) — flujo real: simular -> ds_detect_stars -> fit_frame_psf
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deepsky_sim::{render_light, SimExposure, SimScene, SimSensor, SimStar};

    /// LCG determinista propio (no depende de la estabilidad de `rand`).
    fn lcg(estado: &mut u64) -> f64 {
        *estado = estado
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((*estado >> 11) as f64) / ((1u64 << 53) as f64)
    }

    /// Posiciones en rejilla con jitter +-4 px: separación mínima >= paso-8
    /// (> 10 px para los censos usados) y margen >= 18 px de los bordes.
    fn posiciones(w: usize, h: usize, n: usize, semilla: u64) -> Vec<(f64, f64)> {
        let margen = 22.0;
        let g = (n as f64).sqrt().ceil() as usize;
        let paso_x = (w as f64 - 2.0 * margen) / g as f64;
        let paso_y = (h as f64 - 2.0 * margen) / g as f64;
        assert!(
            paso_x.min(paso_y) >= 22.0,
            "rejilla demasiado densa para el aislamiento"
        );
        let mut estado = semilla;
        let mut out = Vec::with_capacity(n);
        'rejilla: for gy in 0..g {
            for gx in 0..g {
                if out.len() >= n {
                    break 'rejilla;
                }
                let jx = (lcg(&mut estado) - 0.5) * 8.0;
                let jy = (lcg(&mut estado) - 0.5) * 8.0;
                out.push((
                    margen + (gx as f64 + 0.5) * paso_x + jx,
                    margen + (gy as f64 + 0.5) * paso_y + jy,
                ));
            }
        }
        out
    }

    fn sensor_base() -> SimSensor {
        SimSensor {
            gain_e_per_adu: 1.0,
            read_noise_e: 3.0,
            bias_adu: 500.0,
            dark_adu_per_s: 0.0,
            full_well_adu: 65535.0,
            hot_pixels: Vec::new(),
            bayer: None,
            vignette: None,
        }
    }

    fn renderiza(w: usize, h: usize, stars: Vec<SimStar>, seed: u64) -> Vec<f32> {
        let scene = SimScene {
            width: w,
            height: h,
            background_adu: 200.0,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0, 1.0, 1.0],
            stars,
        };
        let exp = SimExposure {
            exposure_s: 60.0,
            dx: 0.0,
            dy: 0.0,
            seed,
        };
        render_light(&scene, &sensor_base(), &exp).0
    }

    /// Escena de FWHM constante: n estrellas Moffat beta=2.5, flujos en
    /// [flujo_min, flujo_max] (deterministas).
    fn escena_constante(
        w: usize,
        h: usize,
        n: usize,
        fwhm: f64,
        flujo_min: f64,
        flujo_max: f64,
        semilla: u64,
    ) -> Vec<f32> {
        let mut estado = semilla ^ 0xA5A5_5A5A;
        let stars = posiciones(w, h, n, semilla)
            .into_iter()
            .map(|(x, y)| SimStar {
                x,
                y,
                flux_adu: flujo_min + (flujo_max - flujo_min) * lcg(&mut estado),
                fwhm_px: fwhm,
                moffat_beta: Some(2.5),
            })
            .collect();
        renderiza(w, h, stars, semilla.wrapping_add(99))
    }

    /// 1. Recuperación Moffat: ~40 estrellas beta=2.5, FWHM=3.2 constante,
    ///    flujos 6k-40k, fondo 200, RN 3. fwhm_mean a < 3 % y elipticidad
    ///    < 0.05 (la verdad es circular). Flujo real: detectar -> ajustar.
    #[test]
    fn gate_f5_moffat_recovery() {
        let (w, h) = (256usize, 256usize);
        let luma = escena_constante(w, h, 40, 3.2, 6000.0, 40000.0, 7);
        let detectadas = crate::ds_detect_stars(&luma, w, h, 120);
        assert!(
            detectadas.len() >= 30,
            "detector: solo {} estrellas de 40",
            detectadas.len()
        );
        let (fp, rep) = fit_frame_psf(&luma, w, h, &detectadas, 0.2).expect("ajuste PSF del frame");
        let fwhm = fp.base.fwhm_mean();
        let err = ((fwhm - 3.2) / 3.2).abs();
        assert!(
            err < 0.03,
            "fwhm_mean {fwhm:.4} vs 3.2 (err rel {err:.4}, beta {})",
            fp.base.beta
        );
        assert!(
            fp.base.ellipticity() < 0.05,
            "elipticidad {:.4} en verdad circular (fx {:.3}, fy {:.3})",
            fp.base.ellipticity(),
            fp.base.fwhm_x,
            fp.base.fwhm_y
        );
        assert!(
            rep.stars_used >= 20,
            "censo usado {} demasiado bajo",
            rep.stars_used
        );
    }

    /// 2. Campo espacial: FWHM crece linealmente 2.6 -> 3.8 px de izquierda a
    ///    derecha (fwhm(x) = 2.6 + 1.2*x/w). Con ~50 estrellas el campo Linear
    ///    recupera la FWHM en el centro y en los bordes a < 3 %.
    #[test]
    fn gate_f5_spatial_field() {
        let (w, h) = (256usize, 256usize);
        let mut estado = 0xBEEFu64;
        let stars: Vec<SimStar> = posiciones(w, h, 50, 11)
            .into_iter()
            .map(|(x, y)| SimStar {
                x,
                y,
                flux_adu: 8000.0 + 32000.0 * lcg(&mut estado),
                fwhm_px: 2.6 + 1.2 * x / w as f64,
                moffat_beta: Some(2.5),
            })
            .collect();
        let luma = renderiza(w, h, stars, 1234);
        let detectadas = crate::ds_detect_stars(&luma, w, h, 120);
        assert!(
            detectadas.len() >= 35,
            "detector: solo {} estrellas de 50",
            detectadas.len()
        );
        let (fp, rep) = fit_frame_psf(&luma, w, h, &detectadas, 0.2).expect("ajuste PSF del frame");
        assert!(
            rep.spatial_order >= 1,
            "con ~50 estrellas el campo debe ser al menos Linear (orden {})",
            rep.spatial_order
        );
        for (xn, esperado) in [(0.1f32, 2.72f32), (0.5, 3.2), (0.9, 3.68)] {
            let m = fp.at(xn, 0.5).fwhm_mean();
            let err = ((m - esperado) / esperado).abs();
            assert!(
                err < 0.03,
                "fwhm en xn={xn}: {m:.4} vs {esperado} (err rel {err:.4})"
            );
        }
    }

    /// 3. Holdout sin sesgo: |holdout_fwhm_bias| < 0.03 en el escenario 1.
    #[test]
    fn gate_f5_holdout_unbiased() {
        let (w, h) = (256usize, 256usize);
        let luma = escena_constante(w, h, 40, 3.2, 6000.0, 40000.0, 7);
        let detectadas = crate::ds_detect_stars(&luma, w, h, 120);
        let (_fp, rep) =
            fit_frame_psf(&luma, w, h, &detectadas, 0.2).expect("ajuste PSF del frame");
        assert!(
            rep.stars_holdout >= 4,
            "holdout de {} estrellas",
            rep.stars_holdout
        );
        assert!(
            rep.holdout_fwhm_bias.abs() < 0.03,
            "sesgo de FWHM en holdout {:.4}",
            rep.holdout_fwhm_bias
        );
    }

    /// 4. Escalera de censo: 5 estrellas -> None; 12 -> Constant; 50 -> Linear.
    #[test]
    fn test_census_ladder() {
        let (w, h) = (256usize, 256usize);
        for (n, esperado) in [(5usize, None), (12, Some(0u8)), (50, Some(1u8))] {
            let luma = escena_constante(w, h, n, 3.0, 20000.0, 30000.0, 21 + n as u64);
            let detectadas = crate::ds_detect_stars(&luma, w, h, 120);
            let resultado = fit_frame_psf(&luma, w, h, &detectadas, 0.2);
            match esperado {
                None => assert!(
                    resultado.is_none(),
                    "con {n} estrellas el ajuste debe devolver None"
                ),
                Some(orden) => {
                    let (_fp, rep) = resultado
                        .unwrap_or_else(|| panic!("con {n} estrellas el ajuste debe existir"));
                    assert_eq!(
                        rep.spatial_order, orden,
                        "con {n} estrellas el orden debe ser {orden}"
                    );
                }
            }
        }
    }

    /// 5. Rasterizado: suma ~ 1 (+-1e-3), pico en el centro y simetría en
    ///    ambos ejes para theta = 0 (también con PSF elíptica).
    #[test]
    fn test_rasterize_psf_normalized() {
        for psf in [
            MoffatPsf {
                fwhm_x: 3.0,
                fwhm_y: 3.0,
                theta: 0.0,
                beta: 2.5,
            },
            MoffatPsf {
                fwhm_x: 4.0,
                fwhm_y: 2.2,
                theta: 0.0,
                beta: 3.0,
            },
        ] {
            let size = 21usize;
            let g = rasterize_psf(&psf, size);
            assert_eq!(g.len(), size * size);
            let suma: f64 = g.iter().map(|&v| v as f64).sum();
            assert!((suma - 1.0).abs() < 1e-3, "suma {suma}");
            let centro = size / 2;
            let (imax, _) = g
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap();
            assert_eq!(
                imax,
                centro * size + centro,
                "el pico debe estar en el centro"
            );
            // Simetría especular en x y en y (theta = 0).
            for py in 0..size {
                for px in 0..size {
                    let v = g[py * size + px];
                    let vx = g[py * size + (size - 1 - px)];
                    let vy = g[(size - 1 - py) * size + px];
                    assert!((v - vx).abs() < 1e-6, "asimetria en x en ({px},{py})");
                    assert!((v - vy).abs() < 1e-6, "asimetria en y en ({px},{py})");
                }
            }
        }
    }

    /// Extra: ida y vuelta FWHM <-> alpha y saturación de beta en el raster.
    #[test]
    fn test_fwhm_alfa_roundtrip_y_beta_saturado() {
        for beta in [1.5f64, 2.5, 4.0] {
            let a = alfa_de_fwhm(3.2, beta);
            assert!((fwhm_de_alfa(a, beta) - 3.2).abs() < 1e-12);
        }
        // beta por debajo del tope: no debe producir NaN y sigue normalizado.
        let psf = MoffatPsf {
            fwhm_x: 3.0,
            fwhm_y: 3.0,
            theta: 0.0,
            beta: 0.8,
        };
        let g = rasterize_psf(&psf, 15);
        assert!(g.iter().all(|v| v.is_finite()));
        let suma: f64 = g.iter().map(|&v| v as f64).sum();
        assert!((suma - 1.0).abs() < 1e-3);
    }
}
