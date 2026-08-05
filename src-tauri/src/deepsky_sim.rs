//! Simulador de verdad conocida para cielo profundo (F1).
//!
//! Genera escenas float64 con supersampling e integración de área PROPIA
//! (independiente del Lanczos/warp de producción, para evitar el *inverse
//! crime*): estrellas subpíxel, fondo con gradiente, ruido Poisson + lectura,
//! hot pixels y mosaico CFA. Solo se compila para tests o con la feature
//! `deepsky-sim`; nunca forma parte del binario de producción.
//!
//! Decisiones de diseño documentadas:
//! - `background_adu`, `gradient_adu_per_px` y `flux_adu` son totales POR
//!   EXPOSICIÓN (no por segundo); `exposure_s` solo escala corriente oscura y
//!   hot pixels. Así la verdad conocida queda expresada directamente en ADU.
//! - El dither `(dx, dy)` desplaza los OBJETOS de la escena (las estrellas);
//!   el fondo y su gradiente quedan anclados al detector, como la
//!   contaminación lumínica real, que no se mueve con pequeños dithers.
//! - La PSF se integra por supersampling 8x8 (media de 64 subevaluaciones en
//!   el área del píxel, regla del punto medio). Nada de interpolación de
//!   producción.
//! - Truncado de PSF: cada estrella se evalúa solo dentro de una caja
//!   cuadrada de +-r_max alrededor de su centro. Gaussiana: r_max = 5*sigma
//!   (pérdida de flujo ~3.7e-6). Moffat: r_max encierra >= 99.9 % del flujo
//!   analítico (pérdida <= 1e-3), con tope de seguridad para betas cercanas
//!   a 1. Estrellas a > r_max del borde no aportan nada (truncado documentado
//!   en la spec).
//! - Moffat requiere beta > 1 para que la integral sea finita; valores <= 1.1
//!   se saturan a 1.1 (documentado, evita NaN y cajas de truncado absurdas).
//! - Salida SIEMPRE mono (ch = 1): con `bayer = Some(cid)` cada fotosito
//!   lleva el canal que le toca segun el patron CFA; con `bayer = None` se
//!   usa el canal G (`color[1]`). La salida RGB interleaved simulada llegara
//!   en fases posteriores.
//! - El viñeteo multiplica SOLO la luz de escena (fondo + estrellas) ANTES
//!   del ruido, como en un sensor real; la corriente oscura, los hot pixels
//!   y el bias no se viñetean. El flat se renderiza con el mismo viñeteo
//!   para que la calibración lo corrija.
//! - Varianza verdadera por píxel en ADU^2: (lambda_e + read_noise_e^2) /
//!   gain^2, donde lambda_e incluye TODO lo poissoniano que llega al
//!   fotosito (escena viñeteada + dark + hot). OJO: en píxeles recortados
//!   por full-well la varianza reportada sigue siendo la PRE-clip (la
//!   verdad estadística del proceso, no la del valor saturado); los tests de
//!   gates deben excluir píxeles saturados por su cuenta si lo necesitan.
//! - Poisson determinista sin dependencias nuevas (rand 0.8, sin rand_distr):
//!   algoritmo de Knuth para lambda < 50 y aproximación normal redondeada
//!   para lambda >= 50 (error relativo de momentos < 1 % en el corte; el
//!   redondeo añade varianza ~1/12, despreciable frente a lambda >= 50).
//!   Normal(0,1) por Box-Muller. RNG: StdRng::seed_from_u64 — determinista
//!   para una versión fija de rand en todas las plataformas.
//! - `render_ideal` se añade a la API (no estaba en la spec original) porque
//!   los tests de fotometría y centroide necesitan la escena esperada SIN
//!   ruido ni bias; también servirá a las fases de fotometría/PSF variable.

#![allow(dead_code)]

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// Estrella sintética con posición subpíxel.
/// Convención de coordenadas: el centro del píxel (0,0) está en (0.0, 0.0).
pub(crate) struct SimStar {
    pub x: f64,
    pub y: f64,
    /// Flujo TOTAL integrado (ADU, antes del tinte por canal).
    pub flux_adu: f64,
    pub fwhm_px: f64,
    /// None = PSF gaussiana; Some(beta) = Moffat con ese beta (p.ej. 2.5).
    pub moffat_beta: Option<f64>,
}

/// Escena sintética de verdad conocida (todo en ADU por exposición).
pub(crate) struct SimScene {
    pub width: usize,
    pub height: usize,
    /// Fondo de cielo en ADU (nivel base, sin bias).
    pub background_adu: f64,
    /// Gradiente lineal de fondo: ADU por píxel en x e y (contaminación
    /// lumínica). Anclado al detector: NO se desplaza con el dither.
    pub gradient_adu_per_px: (f64, f64),
    /// Tinte global del cielo+objetos por canal RGB (1.0 = neutro).
    /// Para mono (bayer = None) se usa el canal G.
    pub color: [f64; 3],
    pub stars: Vec<SimStar>,
}

/// Modelo de sensor con la disciplina de ruido de una cámara real.
pub(crate) struct SimSensor {
    pub gain_e_per_adu: f64,
    pub read_noise_e: f64,
    /// Pedestal en ADU (p.ej. 500.0).
    pub bias_adu: f64,
    /// Corriente oscura media (ADU por segundo).
    pub dark_adu_per_s: f64,
    /// Saturación: clip superior en ADU (p.ej. 65535.0).
    pub full_well_adu: f64,
    /// (x, y, adu extra por segundo) — se suman a la corriente oscura del
    /// fotosito, con su propio shot noise.
    pub hot_pixels: Vec<(usize, usize, f64)>,
    /// None = mono. Some(cid) = CFA con cid 8=RGGB, 9=GRBG, 10=GBRG, 11=BGGR
    /// (MISMA convención que ds_cfa_channel en deepsky.rs).
    pub bayer: Option<i32>,
    /// Viñeteo del flat: factor multiplicativo v(x, y) en (0, 1], evaluado en
    /// el centro del píxel (coordenadas en píxeles). None = plano.
    pub vignette: Option<Box<dyn Fn(f64, f64) -> f64 + Send + Sync>>,
}

/// Parámetros de una exposición individual.
pub(crate) struct SimExposure {
    pub exposure_s: f64,
    /// Dither en píxeles: desplazamiento de los objetos de la escena.
    pub dx: f64,
    pub dy: f64,
    /// Determinista: misma semilla => mismo frame, bit a bit.
    pub seed: u64,
}

// ---------------------------------------------------------------------------
// PSF
// ---------------------------------------------------------------------------

/// FWHM -> sigma gaussiana.
const FWHM_A_SIGMA: f64 = 2.354_820_045_030_949; // 2*sqrt(2*ln 2)

/// Subdivisiones del supersampling por eje (8x8 = 64 subevaluaciones).
const SS: usize = 8;

/// PSF precomputada para evaluación rápida en el bucle de supersampling.
enum Psf {
    /// amp = flux / (2*pi*sigma^2); valor = amp * exp(-r^2 * inv2s2).
    Gauss { amp: f64, inv2s2: f64 },
    /// amp = flux * (beta-1) / (pi*alpha^2); valor = amp * (1 + r^2/a^2)^-beta.
    Moffat { amp: f64, inv_a2: f64, beta: f64 },
}

impl Psf {
    /// Construye la PSF normalizada para que la SUMA sobre píxeles ~ flux_adu,
    /// y devuelve el radio de truncado r_max (ver doc del módulo).
    fn nueva(star: &SimStar) -> (Psf, f64) {
        let fwhm = star.fwhm_px.max(1e-6);
        if let Some(beta_raw) = star.moffat_beta {
            // Moffat: beta > 1 obligatorio para integral finita; saturamos.
            let beta = beta_raw.max(1.1);
            let alpha = fwhm / (2.0 * (2f64.powf(1.0 / beta) - 1.0).sqrt());
            let amp = star.flux_adu * (beta - 1.0) / (std::f64::consts::PI * alpha * alpha);
            // Radio que encierra >= 99.9 % del flujo:
            // fracción fuera de r = (1 + (r/alpha)^2)^(1-beta) = 1e-3.
            let r_max = alpha * ((1e-3f64).powf(-1.0 / (beta - 1.0)) - 1.0).sqrt();
            (
                Psf::Moffat {
                    amp,
                    inv_a2: 1.0 / (alpha * alpha),
                    beta,
                },
                r_max,
            )
        } else {
            let sigma = fwhm / FWHM_A_SIGMA;
            let amp = star.flux_adu / (2.0 * std::f64::consts::PI * sigma * sigma);
            (
                Psf::Gauss {
                    amp,
                    inv2s2: 1.0 / (2.0 * sigma * sigma),
                },
                5.0 * sigma,
            )
        }
    }

    #[inline]
    fn eval_r2(&self, r2: f64) -> f64 {
        match *self {
            Psf::Gauss { amp, inv2s2 } => amp * (-r2 * inv2s2).exp(),
            Psf::Moffat { amp, inv_a2, beta } => amp * (1.0 + r2 * inv_a2).powf(-beta),
        }
    }
}

/// Réplica exacta del mapeo de `ds_cfa_channel` (deepsky.rs): devuelve
/// 0=R, 1=G, 2=B para el fotosito (x, y) según el cid (8=RGGB, 9=GRBG,
/// 10=GBRG, 11=BGGR). Se duplica aquí a propósito: el simulador no debe
/// depender de código de producción.
#[inline]
fn cfa_canal(cid: i32, x: usize, y: usize) -> usize {
    let (rx, ry) = match cid {
        8 => (0usize, 0usize),
        9 => (1, 0),
        10 => (0, 1),
        11 => (1, 1),
        _ => return 1,
    };
    if (x & 1) == rx && (y & 1) == ry {
        0
    } else if (x & 1) != rx && (y & 1) != ry {
        2
    } else {
        1
    }
}

// ---------------------------------------------------------------------------
// Muestreo determinista (Box-Muller + Poisson propio, sin rand_distr)
// ---------------------------------------------------------------------------

/// Normal(0, 1) por Box-Muller. Descarta la segunda variante para mantener el
/// consumo de RNG constante por llamada (determinismo simple de razonar).
#[inline]
fn muestra_normal(rng: &mut StdRng) -> f64 {
    // u1 en (0, 1] para evitar ln(0).
    let u1: f64 = 1.0 - rng.gen::<f64>();
    let u2: f64 = rng.gen::<f64>();
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

/// Corte entre Knuth y aproximación normal para el muestreo Poisson.
const POISSON_CORTE_NORMAL: f64 = 50.0;

/// Muestreo Poisson(lambda) determinista:
/// - lambda < 50: algoritmo de Knuth (producto de uniformes), exacto.
/// - lambda >= 50: Normal(lambda, sqrt(lambda)) redondeada y recortada a 0
///   (error relativo de momentos < 1 % en el corte; el redondeo añade
///   varianza ~1/12, despreciable).
#[inline]
fn muestra_poisson(rng: &mut StdRng, lambda: f64) -> f64 {
    if lambda <= 0.0 {
        return 0.0;
    }
    if lambda < POISSON_CORTE_NORMAL {
        let umbral = (-lambda).exp();
        let mut k = 0u64;
        let mut p = 1.0f64;
        loop {
            p *= rng.gen::<f64>();
            if p <= umbral {
                return k as f64;
            }
            k += 1;
        }
    }
    (lambda + muestra_normal(rng) * lambda.sqrt())
        .round()
        .max(0.0)
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/// Escena ideal SIN ruido, SIN bias y SIN dark: señal esperada de escena en
/// ADU por fotosito (fondo + gradiente + estrellas), con tinte de color y
/// viñeteo ya aplicados. f64 para tests de fotometría/centroide y para las
/// fases futuras (PSF variable, fotometría). Solo usa `exp.dx`/`exp.dy`.
pub(crate) fn render_ideal(scene: &SimScene, sensor: &SimSensor, exp: &SimExposure) -> Vec<f64> {
    let (w, h) = (scene.width, scene.height);
    let mut img = vec![0.0f64; w * h];

    // Fondo + gradiente, evaluados en el centro del píxel y anclados al
    // detector (no se desplazan con el dither). Recorte a >= 0: no existen
    // fotones negativos aunque el gradiente baje del cero.
    let (gx, gy) = scene.gradient_adu_per_px;
    for py in 0..h {
        for px in 0..w {
            let base = scene.background_adu + gx * px as f64 + gy * py as f64;
            img[py * w + px] = base.max(0.0);
        }
    }

    // Estrellas: integración de área por supersampling 8x8 (regla del punto
    // medio con 64 subevaluaciones por píxel), truncada a una caja de
    // +-r_max alrededor del centro desplazado por el dither.
    let paso = 1.0 / SS as f64;
    for star in &scene.stars {
        let (psf, r_max) = Psf::nueva(star);
        // Tope de seguridad para betas patológicas: nunca más que el frame.
        let r_max = r_max.min(2.0 * (w + h) as f64);
        let cx = star.x + exp.dx;
        let cy = star.y + exp.dy;
        let x0 = (cx - r_max).floor().max(0.0) as usize;
        let x1 = (cx + r_max).ceil().min((w - 1) as f64) as usize;
        let y0 = (cy - r_max).floor().max(0.0) as usize;
        let y1 = (cy + r_max).ceil().min((h - 1) as f64) as usize;
        if (cx + r_max) < 0.0 || (cy + r_max) < 0.0 || x0 > x1 || y0 > y1 {
            continue; // completamente fuera del frame
        }
        for py in y0..=y1 {
            for px in x0..=x1 {
                // Subcentros: px - 0.5 + (i + 0.5) / SS, i = 0..SS-1.
                let mut acc = 0.0f64;
                for sy in 0..SS {
                    let yy = py as f64 - 0.5 + (sy as f64 + 0.5) * paso - cy;
                    let yy2 = yy * yy;
                    for sx in 0..SS {
                        let xx = px as f64 - 0.5 + (sx as f64 + 0.5) * paso - cx;
                        acc += psf.eval_r2(xx * xx + yy2);
                    }
                }
                img[py * w + px] += acc / (SS * SS) as f64;
            }
        }
    }

    // Tinte por canal (según el fotosito CFA o G para mono) y viñeteo.
    for py in 0..h {
        for px in 0..w {
            let c = match sensor.bayer {
                Some(cid) => cfa_canal(cid, px, py),
                None => 1,
            };
            let i = py * w + px;
            let mut v = img[i] * scene.color[c];
            if let Some(vg) = &sensor.vignette {
                v *= vg(px as f64, py as f64);
            }
            img[i] = v;
        }
    }
    img
}

/// Frame de luz simulado. Devuelve:
/// - datos en ADU f32, MONO (w*h): con `bayer = Some(cid)` cada fotosito lleva
///   su canal CFA; con `bayer = None` se usa el canal G (RGB interleaved
///   llegará en fases posteriores);
/// - la VARIANZA VERDADERA por píxel en ADU^2: shot noise de TODO lo que
///   llega al fotosito (escena viñeteada + dark + hot) + read noise. Para
///   píxeles recortados por full-well la varianza sigue siendo la PRE-clip.
pub(crate) fn render_light(
    scene: &SimScene,
    sensor: &SimSensor,
    exp: &SimExposure,
) -> (Vec<f32>, Vec<f64>) {
    let (w, h) = (scene.width, scene.height);
    let ideal = render_ideal(scene, sensor, exp);

    // Mapa de ADU extra por segundo de los hot pixels.
    let mut hot = vec![0.0f64; w * h];
    for &(x, y, adu_s) in &sensor.hot_pixels {
        if x < w && y < h {
            hot[y * w + x] += adu_s;
        }
    }

    let gain = sensor.gain_e_per_adu.max(1e-12);
    let rn2 = sensor.read_noise_e * sensor.read_noise_e;
    let mut rng = StdRng::seed_from_u64(exp.seed);
    let mut out = vec![0.0f32; w * h];
    let mut var = vec![0.0f64; w * h];

    for i in 0..w * h {
        // Todo lo poissoniano que llega al fotosito, en electrones.
        let dark_adu = (sensor.dark_adu_per_s + hot[i]) * exp.exposure_s;
        let lambda_e = (ideal[i] + dark_adu).max(0.0) * gain;
        let shot_e = muestra_poisson(&mut rng, lambda_e);
        let lectura_e = muestra_normal(&mut rng) * sensor.read_noise_e;
        let adu = (shot_e + lectura_e) / gain + sensor.bias_adu;
        // Clip inferior a 0 (el ADC no baja de cero) y superior a full-well.
        out[i] = adu.clamp(0.0, sensor.full_well_adu) as f32;
        // Varianza verdadera PRE-clip (ver doc del módulo).
        var[i] = (lambda_e + rn2) / (gain * gain);
    }
    (out, var)
}

/// Bias: solo pedestal + ruido de lectura (sin señal poissoniana).
pub(crate) fn render_bias(sensor: &SimSensor, w: usize, h: usize, seed: u64) -> Vec<f32> {
    let gain = sensor.gain_e_per_adu.max(1e-12);
    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = vec![0.0f32; w * h];
    for v in out.iter_mut() {
        let adu = muestra_normal(&mut rng) * sensor.read_noise_e / gain + sensor.bias_adu;
        *v = adu.clamp(0.0, sensor.full_well_adu) as f32;
    }
    out
}

/// Dark: corriente oscura media + hot pixels (con su shot noise) + lectura
/// + bias. Sin viñeteo: la corriente oscura no pasa por la óptica.
pub(crate) fn render_dark(
    sensor: &SimSensor,
    w: usize,
    h: usize,
    exposure_s: f64,
    seed: u64,
) -> Vec<f32> {
    render_dark_with_pattern(sensor, w, h, exposure_s, None, seed)
}

/// Dark con patrón térmico espacial adicional (p. ej. amp glow o banding),
/// expresado en ADU/s. El patrón pertenece al detector y no se viñetea.
/// Esta ruta permite construir corpus de calibración con verdad conocida sin
/// contaminar el modelo óptico de la escena.
pub(crate) fn render_dark_with_pattern(
    sensor: &SimSensor,
    w: usize,
    h: usize,
    exposure_s: f64,
    spatial_dark_adu_per_s: Option<&(dyn Fn(f64, f64) -> f64 + Send + Sync)>,
    seed: u64,
) -> Vec<f32> {
    let mut hot = vec![0.0f64; w * h];
    for &(x, y, adu_s) in &sensor.hot_pixels {
        if x < w && y < h {
            hot[y * w + x] += adu_s;
        }
    }
    let gain = sensor.gain_e_per_adu.max(1e-12);
    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = vec![0.0f32; w * h];
    for i in 0..w * h {
        let x = (i % w) as f64;
        let y = (i / w) as f64;
        let spatial = spatial_dark_adu_per_s.map_or(0.0, |pattern| pattern(x, y));
        let lambda_e = (sensor.dark_adu_per_s + hot[i] + spatial).max(0.0) * exposure_s * gain;
        let shot_e = muestra_poisson(&mut rng, lambda_e);
        let lectura_e = muestra_normal(&mut rng) * sensor.read_noise_e;
        out[i] =
            ((shot_e + lectura_e) / gain + sensor.bias_adu).clamp(0.0, sensor.full_well_adu) as f32;
    }
    out
}

/// Flat: iluminación uniforme de nivel medio `flat_level_adu` multiplicada
/// por el MISMO viñeteo del sensor + shot noise + lectura + bias.
/// Acromático (luz blanca de panel: no aplica el tinte de escena) y sin
/// corriente oscura (exposición de flat idealizada como instantánea).
pub(crate) fn render_flat(
    sensor: &SimSensor,
    w: usize,
    h: usize,
    flat_level_adu: f64,
    seed: u64,
) -> Vec<f32> {
    render_flat_exposure(sensor, w, h, flat_level_adu, 0.0, None, seed)
}

/// Flat de exposición finita. Además de la iluminación óptica, incluye dark
/// current, hot pixels y un patrón térmico espacial opcional. Un dark-flat
/// compatible se obtiene con `render_dark_with_pattern` usando exactamente la
/// misma exposición y patrón.
pub(crate) fn render_flat_exposure(
    sensor: &SimSensor,
    w: usize,
    h: usize,
    flat_level_adu: f64,
    exposure_s: f64,
    spatial_dark_adu_per_s: Option<&(dyn Fn(f64, f64) -> f64 + Send + Sync)>,
    seed: u64,
) -> Vec<f32> {
    let gain = sensor.gain_e_per_adu.max(1e-12);
    let mut hot = vec![0.0f64; w * h];
    for &(x, y, adu_s) in &sensor.hot_pixels {
        if x < w && y < h {
            hot[y * w + x] += adu_s;
        }
    }
    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = vec![0.0f32; w * h];
    for py in 0..h {
        for px in 0..w {
            let mut nivel = flat_level_adu;
            if let Some(vg) = &sensor.vignette {
                nivel *= vg(px as f64, py as f64);
            }
            let i = py * w + px;
            let spatial =
                spatial_dark_adu_per_s.map_or(0.0, |pattern| pattern(px as f64, py as f64));
            let thermal_adu = (sensor.dark_adu_per_s + hot[i] + spatial).max(0.0) * exposure_s;
            let lambda_e = (nivel.max(0.0) + thermal_adu) * gain;
            let shot_e = muestra_poisson(&mut rng, lambda_e);
            let lectura_e = muestra_normal(&mut rng) * sensor.read_noise_e;
            out[i] = ((shot_e + lectura_e) / gain + sensor.bias_adu)
                .clamp(0.0, sensor.full_well_adu) as f32;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Sensor mono "limpio" de referencia para los tests.
    fn sensor_base() -> SimSensor {
        SimSensor {
            gain_e_per_adu: 1.0,
            read_noise_e: 3.5,
            bias_adu: 500.0,
            dark_adu_per_s: 0.0,
            full_well_adu: 65535.0,
            hot_pixels: Vec::new(),
            bayer: None,
            vignette: None,
        }
    }

    fn escena_plana(w: usize, h: usize, fondo: f64) -> SimScene {
        SimScene {
            width: w,
            height: h,
            background_adu: fondo,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0, 1.0, 1.0],
            stars: Vec::new(),
        }
    }

    fn exposicion(seed: u64, dx: f64, dy: f64) -> SimExposure {
        SimExposure {
            exposure_s: 60.0,
            dx,
            dy,
            seed,
        }
    }

    /// 1. Fotometría de apertura sobre el render ideal (sin ruido): el flujo
    ///    dentro de radio 4*fwhm debe recuperar flux_adu con error < 0.5 %.
    #[test]
    fn sim_photometry_recovers_star_flux() {
        let flux = 50_000.0;
        let fwhm = 3.0;
        let (cx, cy) = (31.7, 32.3);
        let mut scene = escena_plana(64, 64, 0.0);
        scene.stars.push(SimStar {
            x: cx,
            y: cy,
            flux_adu: flux,
            fwhm_px: fwhm,
            moffat_beta: None,
        });
        let sensor = sensor_base();
        let ideal = render_ideal(&scene, &sensor, &exposicion(0, 0.0, 0.0));

        let radio = 4.0 * fwhm;
        let mut suma = 0.0f64;
        for py in 0..scene.height {
            for px in 0..scene.width {
                let dx = px as f64 - cx;
                let dy = py as f64 - cy;
                if dx * dx + dy * dy <= radio * radio {
                    suma += ideal[py * scene.width + px];
                }
            }
        }
        let err = (suma - flux).abs() / flux;
        assert!(
            err < 0.005,
            "flujo de apertura {suma:.2} vs esperado {flux:.2} (err rel {err:.5})"
        );
    }

    /// 2. Varianza verdadera vs varianza empírica de 400 realizaciones de una
    ///    escena plana (fondo 200 ADU, bias 500, RN 3.5 e-, gain 1): el error
    ///    relativo de la media por píxel debe ser < 5 %.
    #[test]
    fn sim_true_variance_matches_empirical() {
        let (w, h) = (16usize, 16usize);
        let n = 400usize;
        let scene = escena_plana(w, h, 200.0);
        let sensor = sensor_base();

        let mut suma = vec![0.0f64; w * h];
        let mut suma2 = vec![0.0f64; w * h];
        let mut var_verdadera = vec![0.0f64; w * h];
        for s in 0..n {
            let (frame, var) =
                render_light(&scene, &sensor, &exposicion(1000 + s as u64, 0.0, 0.0));
            if s == 0 {
                var_verdadera = var;
            }
            for i in 0..w * h {
                let v = frame[i] as f64;
                suma[i] += v;
                suma2[i] += v * v;
            }
        }
        let mut media_emp = 0.0f64;
        let mut media_verd = 0.0f64;
        for i in 0..w * h {
            let mu = suma[i] / n as f64;
            let var_emp = (suma2[i] - n as f64 * mu * mu) / (n as f64 - 1.0);
            media_emp += var_emp;
            media_verd += var_verdadera[i];
        }
        media_emp /= (w * h) as f64;
        media_verd /= (w * h) as f64;
        // Comprobación de cordura: var verdadera = 200 + 3.5^2 = 212.25 ADU^2.
        assert!((media_verd - 212.25).abs() < 1e-9);
        let err = (media_emp - media_verd).abs() / media_verd;
        assert!(
            err < 0.05,
            "var empirica {media_emp:.3} vs verdadera {media_verd:.3} (err rel {err:.4})"
        );
    }

    /// 3. Patrón CFA: con color=[1,0,0] SOLO los fotositos R (según el mapeo
    ///    de ds_cfa_channel, hardcodeado aquí de forma independiente) llevan
    ///    señal de escena; el resto queda exactamente en bias (RN=0, dark=0).
    #[test]
    fn sim_cfa_pattern_matches_ds_cfa_channel() {
        // Mapeo esperado, copiado (no llamado) de ds_cfa_channel:
        // cid -> (rx, ry) de la celda R en la matriz 2x2.
        let esperado: [(i32, usize, usize); 4] = [(8, 0, 0), (9, 1, 0), (10, 0, 1), (11, 1, 1)];
        let (w, h) = (8usize, 8usize);
        for &(cid, rx, ry) in &esperado {
            let mut scene = escena_plana(w, h, 100.0);
            scene.color = [1.0, 0.0, 0.0]; // solo canal rojo
            let mut sensor = sensor_base();
            sensor.read_noise_e = 0.0;
            sensor.bayer = Some(cid);
            let (frame, _) = render_light(&scene, &sensor, &exposicion(7, 0.0, 0.0));

            let mut suma_extra_r = 0.0f64;
            let mut n_r = 0usize;
            for py in 0..h {
                for px in 0..w {
                    let es_r = (px & 1) == rx && (py & 1) == ry;
                    let v = frame[py * w + px] as f64;
                    if es_r {
                        suma_extra_r += v - sensor.bias_adu;
                        n_r += 1;
                    } else {
                        // Sin señal de escena: Poisson(0)=0 y RN=0 => bias exacto.
                        assert!(
                            (v - sensor.bias_adu).abs() < 1e-6,
                            "cid {cid}: fotosito no-R ({px},{py}) = {v} != bias"
                        );
                    }
                }
            }
            assert_eq!(n_r, w * h / 4, "cid {cid}: numero de fotositos R");
            // Señal esperada 100 ADU por fotosito R; con shot noise la suma
            // difícilmente baja de la mitad (std de la suma = sqrt(16*100) = 40).
            assert!(
                suma_extra_r > 0.5 * 100.0 * n_r as f64,
                "cid {cid}: los fotositos R no llevan la señal de escena (suma {suma_extra_r})"
            );
        }
    }

    /// 4. Determinismo: misma semilla => bytes idénticos; semillas distintas
    ///    => frames distintos.
    #[test]
    fn sim_is_deterministic() {
        let mut scene = escena_plana(24, 24, 150.0);
        scene.gradient_adu_per_px = (0.5, -0.2);
        scene.stars.push(SimStar {
            x: 11.4,
            y: 12.6,
            flux_adu: 20_000.0,
            fwhm_px: 2.8,
            moffat_beta: Some(2.5),
        });
        let mut sensor = sensor_base();
        sensor.dark_adu_per_s = 0.02;
        sensor.hot_pixels = vec![(3, 5, 40.0)];

        let (a, va) = render_light(&scene, &sensor, &exposicion(42, 0.0, 0.0));
        let (b, vb) = render_light(&scene, &sensor, &exposicion(42, 0.0, 0.0));
        assert_eq!(a, b, "misma semilla debe dar frames identicos");
        assert_eq!(va, vb, "misma semilla debe dar varianzas identicas");

        let (c, _) = render_light(&scene, &sensor, &exposicion(43, 0.0, 0.0));
        assert!(a != c, "semillas distintas deben dar frames distintos");

        // Tambien los frames de calibracion.
        assert_eq!(
            render_bias(&sensor, 24, 24, 5),
            render_bias(&sensor, 24, 24, 5)
        );
        assert_eq!(
            render_dark(&sensor, 24, 24, 60.0, 6),
            render_dark(&sensor, 24, 24, 60.0, 6)
        );
        assert_eq!(
            render_flat(&sensor, 24, 24, 30_000.0, 8),
            render_flat(&sensor, 24, 24, 30_000.0, 8)
        );
    }

    /// 5. El dither (dx=1.5, dy=-0.5) mueve el centroide de la estrella
    ///    exactamente eso (+-0.05 px), medido por momentos en el render ideal.
    #[test]
    fn sim_dither_shifts_star_centroid() {
        let mut scene = escena_plana(32, 32, 0.0);
        scene.stars.push(SimStar {
            x: 15.2,
            y: 16.1,
            flux_adu: 10_000.0,
            fwhm_px: 2.5,
            moffat_beta: None,
        });
        let sensor = sensor_base();

        let centroide = |img: &[f64]| -> (f64, f64) {
            let (mut sx, mut sy, mut s) = (0.0f64, 0.0f64, 0.0f64);
            for py in 0..scene.height {
                for px in 0..scene.width {
                    let v = img[py * scene.width + px];
                    sx += v * px as f64;
                    sy += v * py as f64;
                    s += v;
                }
            }
            (sx / s, sy / s)
        };

        let base = render_ideal(&scene, &sensor, &exposicion(0, 0.0, 0.0));
        let movido = render_ideal(&scene, &sensor, &exposicion(0, 1.5, -0.5));
        let (x0, y0) = centroide(&base);
        let (x1, y1) = centroide(&movido);
        let (ddx, ddy) = (x1 - x0, y1 - y0);
        assert!(
            (ddx - 1.5).abs() < 0.05 && (ddy + 0.5).abs() < 0.05,
            "desplazamiento de centroide ({ddx:.4}, {ddy:.4}) != (1.5, -0.5)"
        );
    }

    /// Extra: la varianza reportada en un pixel saturado por full-well sigue
    /// siendo la PRE-clip, y el valor queda recortado.
    #[test]
    fn sim_saturated_pixel_keeps_preclip_variance() {
        let (w, h) = (4usize, 4usize);
        let scene = escena_plana(w, h, 100.0);
        let mut sensor = sensor_base();
        sensor.full_well_adu = 1000.0;
        sensor.hot_pixels = vec![(1, 1, 100_000.0)]; // satura de sobra en 60 s
        let (frame, var) = render_light(&scene, &sensor, &exposicion(9, 0.0, 0.0));
        let i = w + 1; // (1, 1)
        assert_eq!(frame[i], 1000.0, "el pixel caliente debe quedar recortado");
        let lambda = 100.0 + 100_000.0 * 60.0;
        let esperada = lambda + 3.5 * 3.5;
        assert!(
            (var[i] - esperada).abs() / esperada < 1e-12,
            "varianza pre-clip {} vs esperada {}",
            var[i],
            esperada
        );
    }

    /// Extra: el flat reproduce el viñeteo del sensor (esquina mas oscura que
    /// el centro en la proporcion v(x,y)).
    #[test]
    fn sim_flat_follows_vignette() {
        let (w, h) = (32usize, 32usize);
        let mut sensor = sensor_base();
        let (cx, cy) = ((w as f64 - 1.0) / 2.0, (h as f64 - 1.0) / 2.0);
        let r2max = cx * cx + cy * cy;
        sensor.vignette = Some(Box::new(move |x, y| {
            let r2 = (x - cx) * (x - cx) + (y - cy) * (y - cy);
            1.0 - 0.4 * (r2 / r2max)
        }));
        // Promedio de varios flats para batir el shot noise.
        let nivel = 30_000.0;
        let n = 16usize;
        let mut media = vec![0.0f64; w * h];
        for s in 0..n {
            let f = render_flat(&sensor, w, h, nivel, 100 + s as u64);
            for i in 0..w * h {
                media[i] += f[i] as f64 / n as f64;
            }
        }
        let vg = sensor.vignette.as_ref().unwrap();
        // Centro y esquina: la razon (media - bias) debe seguir v(x,y).
        let centro = media[(h / 2) * w + w / 2] - sensor.bias_adu;
        let esquina = media[0] - sensor.bias_adu;
        let esperado = vg(0.0, 0.0) / vg((w / 2) as f64, (h / 2) as f64);
        let obtenido = esquina / centro;
        assert!(
            (obtenido - esperado).abs() / esperado < 0.02,
            "razon esquina/centro {obtenido:.4} vs viñeteo esperado {esperado:.4}"
        );
    }

    /// Un flat largo con amp glow conserva un gradiente térmico si sólo se
    /// resta bias; un dark-flat de exposición idéntica debe retirarlo sin
    /// escalar. Éste es el fixture sintético mínimo del gate dark-flat/CMOS.
    #[test]
    fn sim_matching_dark_flat_removes_amp_glow_pattern() {
        let (w, h) = (32usize, 32usize);
        let mut sensor = sensor_base();
        sensor.read_noise_e = 1.0;
        sensor.dark_adu_per_s = 0.5;
        let exposure_s = 8.0;
        let amp_glow = |x: f64, _y: f64| 40.0 * x / (w - 1) as f64;
        let n = 32usize;
        let mut flat_mean = vec![0.0f64; w * h];
        let mut dark_flat_mean = vec![0.0f64; w * h];
        for k in 0..n {
            let flat = render_flat_exposure(
                &sensor,
                w,
                h,
                20_000.0,
                exposure_s,
                Some(&amp_glow),
                10_000 + k as u64,
            );
            let dark_flat = render_dark_with_pattern(
                &sensor,
                w,
                h,
                exposure_s,
                Some(&amp_glow),
                20_000 + k as u64,
            );
            for i in 0..w * h {
                flat_mean[i] += flat[i] as f64 / n as f64;
                dark_flat_mean[i] += dark_flat[i] as f64 / n as f64;
            }
        }

        let column_mean = |image: &[f64], x: usize| -> f64 {
            (0..h).map(|y| image[y * w + x]).sum::<f64>() / h as f64
        };
        let bias_only_left = column_mean(&flat_mean, 0) - sensor.bias_adu;
        let bias_only_right = column_mean(&flat_mean, w - 1) - sensor.bias_adu;
        let bias_only_delta = bias_only_right - bias_only_left;

        let calibrated: Vec<f64> = flat_mean
            .iter()
            .zip(&dark_flat_mean)
            .map(|(flat, dark_flat)| flat - dark_flat)
            .collect();
        let calibrated_delta = column_mean(&calibrated, w - 1) - column_mean(&calibrated, 0);

        assert!(
            bias_only_delta > 250.0,
            "el fixture debe contener amp glow visible: delta={bias_only_delta:.2} ADU"
        );
        assert!(
            calibrated_delta.abs() < 20.0,
            "el dark-flat exacto no retiro el patron: delta={calibrated_delta:.2} ADU"
        );
    }

    #[test]
    fn sim_long_flat_and_patterned_dark_are_deterministic() {
        let sensor = sensor_base();
        let glow = |x: f64, y: f64| 0.2 * x + 0.1 * y;
        assert_eq!(
            render_flat_exposure(&sensor, 12, 10, 10_000.0, 3.0, Some(&glow), 91),
            render_flat_exposure(&sensor, 12, 10, 10_000.0, 3.0, Some(&glow), 91)
        );
        assert_eq!(
            render_dark_with_pattern(&sensor, 12, 10, 3.0, Some(&glow), 92),
            render_dark_with_pattern(&sensor, 12, 10, 3.0, Some(&glow), 92)
        );
    }
}
