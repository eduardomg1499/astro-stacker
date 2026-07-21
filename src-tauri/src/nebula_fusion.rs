//! NebulaFusion Lite (F3): primer motor científico de integración.
//!
//! Coadición por media ponderada con PESOS INVERSO-VARIANZA independientes
//! del brillo del objeto (§2 del plan técnico: pesar por señal cambiaría la
//! PSF del máster) y máscaras de outlier CONGELADAS por cross-fit (módulo
//! `deepsky_masks`) en lugar del κσ online. Produce, además del máster SCI:
//!
//! - `VAR` — varianza por píxel-canal: con pesos w=1/σ² exactos,
//!   Var(media ponderada) = 1/Σw.
//! - `NEFF` — número efectivo de muestras: (Σw)²/Σw².
//! - `DQ` — máscara de calidad de la salida (bit NO_COVERAGE en huecos).
//!
//! Decisiones documentadas de la fase Lite:
//! - σ por celda (~64 px nativos) gobierna los pesos espaciales Lite. Full
//!   mide además σ′ independiente por canal normalizado; CFA usa cada
//!   subplano Bayer y RGB nunca replica un único sigma de luma.
//! - La correlación introducida por el remuestreo Lanczos se ignora en Lite
//!   (subestima levemente VAR); NF-Full la corrige vía PSD (F6).
//! - Los pesos por frame de calidad WBPP (FWHM/PSF/redondez) NO se usan:
//!   dependen de la señal. Solo ruido de fondo manda.
//! - Huecos sin cobertura: el máster lleva 0.0 (como el clásico, para no
//!   romper preview/stretch) pero VAR=NaN, NEFF=0 y DQ|=NO_COVERAGE los
//!   declaran; el relleno visual de drizzle NUNCA se aplica aquí.
//! - CPU siempre (la paridad GPU llega en F8). Drizzle/CFA-directo excluidos
//!   por el preflight en esta fase.

#![allow(dead_code)]

use std::sync::atomic::AtomicBool;
use rayon::prelude::*;

/// Lado de la celda nativa para la medición de σ (px).
const SIGMA_CELL_PX: usize = 64;
/// Rejilla de pesos en espacio de referencia (RG×RG, muestreada bilineal).
const WEIGHT_GRID: usize = 16;
/// Tope de peso relativo por celda (× mediana del frame): una celda muerta
/// (σ≈0) no puede dominar el máster.
const MAX_RELATIVE_WEIGHT: f32 = 25.0;

pub(crate) struct NfLiteProducts {
    /// Varianza por píxel-canal (layout del máster). NaN sin cobertura.
    pub variance: Vec<f32>,
    /// Número efectivo de muestras por píxel-canal. 0 sin cobertura.
    pub neff: Vec<f32>,
    /// DQ por píxel (u32, bits de `deepsky_variance::dq`).
    pub dq: Vec<u32>,
    /// Píxeles×frame enmascarados por el cross-fit (telemetría/receta).
    pub masked_samples: usize,
    /// Origen de la varianza para la receta.
    pub variance_origin: crate::deepsky_variance::VarianceOrigin,
    /// Máximo |mediana(G1)−mediana(G2)| entre frames (solo CFA directo):
    /// auditoría del mismo canal físico (§6.8 del plan técnico).
    pub g1g2_offset_max: Option<f32>,
}

pub(crate) struct NfLiteOutput {
    pub final_data: Vec<f32>,
    /// Cobertura de la pasada de totales (pre-máscaras) — la usa el auto-crop.
    pub wgt1: Vec<f64>,
    /// Cobertura/peso final (post-máscaras), media entre canales.
    pub weight_map: Vec<f64>,
    pub rejection_low: Vec<f64>,
    pub rejection_high: Vec<f64>,
    pub rej_pct: f64,
    /// Frames efectivos por píxel (aprox.: N·Σw_final/Σw_total).
    pub mean_cov: f64,
    pub products: NfLiteProducts,
    /// Modo Full: (FWHM de la Γ objetivo, tiles con fallback, tiles totales).
    pub full_report: Option<(f32, usize, usize)>,
    /// Modo Full solicitado pero degradado a Lite: la razón (para receta/log).
    pub full_fallback: Option<String>,
    /// STRUCT (F7): mapa de evidencia multiescala validado A/B (luma, npx).
    pub struct_map: Option<Vec<f32>>,
    /// SCI_luma − STRUCT: detalles rechazados.
    pub struct_residual: Option<Vec<f32>>,
    /// (aceptados, total) por nivel starlet — para receta/QA.
    pub struct_accepted: Option<Vec<(usize, usize)>>,
    /// FullWithStruct solicitado pero degradado conservando SCI Full/Lite.
    /// Nunca se mezcla con `full_fallback`, que significa Full → Lite.
    pub struct_fallback: Option<String>,
    /// Parámetros solicitados que no pudieron aplicarse literalmente y su
    /// comportamiento efectivo. Debe persistirse en receta/telemetría.
    pub parameter_fallbacks: Vec<String>,
    /// Valores que realmente gobernaron el motor. La receta conserva por
    /// separado la configuración solicitada; este objeto impide presentarla
    /// como efectiva cuando Full, STRUCT o cross-fit cayeron por un gate.
    pub effective_config: NfEffectiveConfig,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NfEffectiveConfig {
    pub variance_weighting: &'static str,
    pub crossfit_mode: &'static str,
    pub crossfit_reference_frames: usize,
    pub full_active: bool,
    pub tile_size: Option<usize>,
    pub max_psf_leakage: Option<f32>,
    pub max_noise_amplification: Option<f32>,
    pub empirical_psd: bool,
    pub struct_active: bool,
    pub fdr_q: Option<f32>,
    pub min_split_sigma: Option<f32>,
}

/// Incertidumbre espacial de un light ya calibrado, en la misma geometria y
/// layout que SCI. El loader productivo la construye desde los stores VAR/DQ;
/// mantenerla como contrato separado evita que NF confunda un sigma MRS
/// empirico con varianza formal de calibracion.
#[derive(Clone)]
pub(crate) struct NfCalibrationUncertainty {
    pub variance: Vec<f32>,
    pub dq: Vec<u32>,
    pub w: usize,
    pub h: usize,
    pub ch: usize,
}

impl NfCalibrationUncertainty {
    fn validate_for(&self, image: &crate::DsImage) -> Result<(), String> {
        let pixels = image
            .w
            .checked_mul(image.h)
            .ok_or("NebulaFusion: geometria VAR/DQ fuera de rango")?;
        let samples = pixels
            .checked_mul(image.ch)
            .ok_or("NebulaFusion: layout VAR fuera de rango")?;
        if self.w != image.w
            || self.h != image.h
            || self.ch != image.ch
            || self.variance.len() != samples
            || self.dq.len() != pixels
        {
            return Err(format!(
                "NebulaFusion: VAR/DQ de calibracion incompatible con SCI (SCI={}x{}x{}, VAR={}x{}x{}, muestras={}, dq={})",
                image.w,
                image.h,
                image.ch,
                self.w,
                self.h,
                self.ch,
                self.variance.len(),
                self.dq.len()
            ));
        }
        let fatal = nf_fatal_input_dq();
        let mut valid = 0usize;
        for pixel in 0..pixels {
            if self.dq[pixel] & fatal != 0 {
                continue;
            }
            for channel in 0..image.ch {
                let variance = self.variance[pixel * image.ch + channel];
                if !variance.is_finite() || variance <= 0.0 {
                    return Err(format!(
                        "NebulaFusion: VAR no positiva/no finita sin DQ fatal en pixel {pixel}, canal {channel}"
                    ));
                }
                valid += 1;
            }
        }
        if valid == 0 {
            return Err("NebulaFusion: VAR/DQ no contiene ninguna muestra cientifica valida".into());
        }
        Ok(())
    }
}

#[inline]
fn nf_fatal_input_dq() -> u32 {
    crate::deepsky_variance::dq::SATURATED
        | crate::deepsky_variance::dq::NONLINEAR
        | crate::deepsky_variance::dq::HOT_COLD
        | crate::deepsky_variance::dq::COSMIC
        | crate::deepsky_variance::dq::NAN_INPUT
        | crate::deepsky_variance::dq::FLAT_INVALID
        | crate::deepsky_variance::dq::NO_COVERAGE
        | crate::deepsky_variance::dq::DEGRADED_CALIBRATION
}

/// Normalize the final SCI/VAR/NEFF/DQ contract after all Lite/Full fallbacks.
/// DQ is spatial, so a pixel with an unavailable RGB channel cannot honestly
/// publish zeros for that channel. A transient invalid input may still yield a
/// complete estimate from the remaining observations; in that case the input
/// flag is consumed by the zero-weight mask and the aggregate remains valid.
fn nf_finalize_scientific_pixels(
    science: &mut [f32],
    variance: &mut [f32],
    neff: &mut [f32],
    dq: &mut [u32],
    pixels: usize,
    channels: usize,
) -> Result<(), String> {
    let samples = pixels
        .checked_mul(channels)
        .ok_or("NebulaFusion: geometría final fuera de rango")?;
    if !matches!(channels, 1 | 3)
        || science.len() != samples
        || variance.len() != samples
        || neff.len() != samples
        || dq.len() != pixels
    {
        return Err("NebulaFusion: planos SCI/VAR/NEFF/DQ finales incompatibles".into());
    }
    for pixel in 0..pixels {
        let complete = (0..channels).all(|channel| {
            let index = pixel * channels + channel;
            science[index].is_finite()
                && variance[index].is_finite()
                && variance[index] >= 0.0
                && neff[index].is_finite()
                && neff[index] > 0.0
        });
        if complete {
            dq[pixel] &= !(crate::deepsky_variance::dq::NO_COVERAGE
                | crate::deepsky_variance::dq::NAN_INPUT);
        } else {
            dq[pixel] |= crate::deepsky_variance::dq::NO_COVERAGE;
            for channel in 0..channels {
                let index = pixel * channels + channel;
                science[index] = f32::NAN;
                variance[index] = f32::NAN;
                neff[index] = 0.0;
            }
        }
    }
    Ok(())
}

/// Configuración efectiva del motor. Vive junto al contexto para que ninguna
/// opción pública quede sólo serializada en la receta sin gobernar el cálculo.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NfRuntimeConfig {
    pub tile_size: usize,
    pub max_psf_leakage: f32,
    pub max_noise_amplification: f32,
    pub empirical_psd: bool,
    pub crossfit_folds: usize,
    pub fdr_q: f32,
    pub min_split_sigma: f32,
}

impl Default for NfRuntimeConfig {
    fn default() -> Self {
        Self {
            tile_size: 512,
            max_psf_leakage: 1e-3,
            max_noise_amplification: 1.5,
            empirical_psd: false,
            crossfit_folds: 4,
            fdr_q: 0.01,
            min_split_sigma: 2.5,
        }
    }
}

impl From<&crate::pipeline::NebulaFusionConfig> for NfRuntimeConfig {
    fn from(value: &crate::pipeline::NebulaFusionConfig) -> Self {
        Self {
            tile_size: value.tile_size as usize,
            max_psf_leakage: value.max_psf_leakage,
            max_noise_amplification: value.max_noise_amplification,
            empirical_psd: value.empirical_psd,
            crossfit_folds: value.crossfit_folds as usize,
            fdr_q: value.fdr_q,
            min_split_sigma: value.min_split_sigma,
        }
    }
}

pub(crate) struct NfLiteContext<'a> {
    pub registered: &'a [(usize, crate::DsTransform, f64)],
    pub norms: &'a [([f32; 3], [f32; 3])],
    pub loc_fields: &'a [Option<Vec<f32>>],
    pub loc_grid: usize,
    pub w_out: usize,
    pub h_out: usize,
    pub ch: usize,
    pub use_lanczos: bool,
    pub cancel: &'a AtomicBool,
    /// Some(cid) = modo CFA DIRECTO (F4): los frames llegan como plano CFA
    /// mono calibrado SIN debayer y cada fotosito deposita solo en su canal
    /// del máster RGB (`ch` debe ser 3). None = ruta mono/RGB demosaiced.
    pub cfa: Option<i32>,
    /// Modo Full (F6): tras las máscaras y el pase C, recombina el máster por
    /// frecuencia con PSF objetivo (nebula_fusion_full). Solo demosaiced.
    pub full: bool,
    /// Modo STRUCT (F7): acumula además dos MITADES independientes (paridad
    /// de índice) en el pase C y valida las estructuras multiescala que
    /// reaparecen en ambas (starlet B3 + BH-FDR). Requiere N≥16.
    pub struct_mode: bool,
    /// Catálogos estelares por índice ORIGINAL de frame (frames[i].1 del
    /// pipeline) — los usa el ajuste PSF Moffat del modo Full.
    pub stars: &'a [Vec<(f32, f32, f32)>],
    pub config: NfRuntimeConfig,
}

/// Pesos de un frame: rejilla espacial 1/σ² (mono/RGB) o escalar por canal
/// (CFA directo: la varianza se mide por sub-plano Bayer; la variación
/// espacial por canal llega con NF-Full).
enum FrameWeights {
    Grid(Vec<f32>),
    Rgb([f64; 3]),
}

/// σ MRS por canal desde los SUB-PLANOS Bayer del frame CFA calibrado (cada
/// sub-plano es una imagen coherente a media resolución — el mosaico entero
/// inflaría la capa fina de la wavelet con el patrón 2×2). Devuelve también
/// el offset mediano G1−G2 (mismo canal físico, auditable: un offset grande
/// delata un problema de calibración por fila/columna).
pub(crate) fn cfa_channel_sigmas(img: &crate::DsImage, cid: i32) -> ([f32; 3], f32) {
    let hw = (img.w / 2).max(1);
    let hh = (img.h / 2).max(1);
    let mut planes: [Vec<f32>; 4] = [
        Vec::with_capacity(hw * hh),
        Vec::with_capacity(hw * hh),
        Vec::with_capacity(hw * hh),
        Vec::with_capacity(hw * hh),
    ];
    for y in 0..hh * 2 {
        for x in 0..hw * 2 {
            planes[(y & 1) * 2 + (x & 1)].push(img.data[y * img.w + x]);
        }
    }
    let mut sigma_ch = [0.0f32; 3];
    let mut g_sigmas: Vec<f32> = Vec::new();
    let mut g_medians: Vec<f32> = Vec::new();
    for (pos, plane) in planes.iter().enumerate() {
        let (py, px) = (pos / 2, pos % 2);
        let c = crate::ds_cfa_channel(cid, px, py);
        let sigma = crate::ds_mrs_noise(plane, hw, hh).max(1e-3);
        if c == 1 {
            g_sigmas.push(sigma);
            let step = (plane.len() / 100_000).max(1);
            let mut s: Vec<f32> = plane.iter().step_by(step).copied().collect();
            s.sort_by(|a, b| a.total_cmp(b));
            g_medians.push(s[s.len() / 2]);
        } else {
            sigma_ch[c] = sigma;
        }
    }
    sigma_ch[1] = g_sigmas.iter().sum::<f32>() / g_sigmas.len().max(1) as f32;
    let g_offset = if g_medians.len() == 2 {
        g_medians[0] - g_medians[1]
    } else {
        0.0
    };
    (sigma_ch, g_offset)
}

/// Sigma MRS independiente por canal. En RGB no se usa una luma compartida:
/// la demosaización y la respuesta del sensor correlacionan y escalan cada
/// canal de forma distinta, por lo que replicar un único sigma falsearía el
/// denominador espectral y la VAR de NF-Full.
fn image_channel_sigmas(img: &crate::DsImage) -> [f32; 3] {
    if img.ch <= 1 {
        let sigma = crate::ds_mrs_noise(&img.data, img.w, img.h).max(1e-3);
        return [sigma; 3];
    }
    let npx = img.w.saturating_mul(img.h);
    let mut out = [0.0f32; 3];
    for (c, sigma) in out.iter_mut().enumerate().take(img.ch.min(3)) {
        let mut plane = Vec::with_capacity(npx);
        plane.extend((0..npx).map(|p| img.data[p * img.ch + c]));
        *sigma = crate::ds_mrs_noise(&plane, img.w, img.h).max(1e-3);
    }
    if img.ch == 2 {
        out[2] = out[1];
    }
    out
}

/// Muestra SCI y propaga VAR con exactamente la misma huella de
/// interpolacion. Cualquier sensel fatal de peso no nulo invalida la muestra
/// completa: una correccion cosmetica, flat debil o cosmic nunca se convierte
/// en una observacion por el mero hecho de haber sido interpolado.
#[inline]
fn nf_sample_science_variance(
    image: &crate::DsImage,
    uncertainty: &NfCalibrationUncertainty,
    sxf: f32,
    syf: f32,
    lanczos: bool,
    science: &mut [f32; 3],
    variance: &mut [f32; 3],
) -> bool {
    let channels = image.ch;
    let x0 = sxf.floor() as usize;
    let y0 = syf.floor() as usize;
    let fatal = nf_fatal_input_dq();
    if lanczos
        && sxf >= 3.0
        && syf >= 3.0
        && sxf < (image.w - 4) as f32
        && syf < (image.h - 4) as f32
    {
        let lut = crate::ds_l3_lut();
        let fx = sxf - x0 as f32;
        let fy = syf - y0 as f32;
        let mut wx = [0.0f32; 6];
        let mut wy = [0.0f32; 6];
        let (mut swx, mut swy) = (0.0f32, 0.0f32);
        for tap in 0..6 {
            let offset = tap as f32 - 2.0;
            let ix = ((offset - fx).abs() * crate::DS_L3_RES as f32) as usize;
            let iy = ((offset - fy).abs() * crate::DS_L3_RES as f32) as usize;
            wx[tap] = lut.get(ix).copied().unwrap_or(0.0);
            wy[tap] = lut.get(iy).copied().unwrap_or(0.0);
            swx += wx[tap];
            swy += wy[tap];
        }
        let normalization = 1.0 / (swx * swy).max(1.0e-6);
        for channel in 0..channels {
            let mut sci = 0.0f64;
            let mut var = 0.0f64;
            for yy in 0..6 {
                for xx in 0..6 {
                    let coefficient = wx[xx] * wy[yy] * normalization;
                    if coefficient.abs() <= f32::EPSILON {
                        continue;
                    }
                    let source_pixel = (y0 + yy - 2) * image.w + (x0 + xx - 2);
                    if uncertainty.dq[source_pixel] & fatal != 0 {
                        return false;
                    }
                    let source = source_pixel * channels + channel;
                    let value = image.data[source];
                    let input_variance = uncertainty.variance[source];
                    if !value.is_finite() || !input_variance.is_finite() || input_variance <= 0.0 {
                        return false;
                    }
                    sci += coefficient as f64 * value as f64;
                    var += coefficient as f64 * coefficient as f64 * input_variance as f64;
                }
            }
            science[channel] = sci as f32;
            variance[channel] = var as f32;
        }
        return science[..channels].iter().all(|value| value.is_finite())
            && variance[..channels]
                .iter()
                .all(|value| value.is_finite() && *value > 0.0);
    }

    let fx = sxf - x0 as f32;
    let fy = syf - y0 as f32;
    let coefficients = [
        (1.0 - fx) * (1.0 - fy),
        fx * (1.0 - fy),
        (1.0 - fx) * fy,
        fx * fy,
    ];
    let source_pixels = [
        y0 * image.w + x0,
        y0 * image.w + x0 + 1,
        (y0 + 1) * image.w + x0,
        (y0 + 1) * image.w + x0 + 1,
    ];
    for channel in 0..channels {
        let mut sci = 0.0f64;
        let mut var = 0.0f64;
        for tap in 0..4 {
            let coefficient = coefficients[tap];
            if coefficient.abs() <= f32::EPSILON {
                continue;
            }
            let pixel = source_pixels[tap];
            if uncertainty.dq[pixel] & fatal != 0 {
                return false;
            }
            let source = pixel * channels + channel;
            let value = image.data[source];
            let input_variance = uncertainty.variance[source];
            if !value.is_finite() || !input_variance.is_finite() || input_variance <= 0.0 {
                return false;
            }
            sci += coefficient as f64 * value as f64;
            var += coefficient as f64 * coefficient as f64 * input_variance as f64;
        }
        science[channel] = sci as f32;
        variance[channel] = var as f32;
    }
    science[..channels].iter().all(|value| value.is_finite())
        && variance[..channels]
            .iter()
            .all(|value| value.is_finite() && *value > 0.0)
}

#[allow(clippy::too_many_arguments)]
fn nf_accumulate_formal_warp(
    ctx: &NfLiteContext,
    k: usize,
    transform: crate::DsTransform,
    image: &crate::DsImage,
    uncertainty: &NfCalibrationUncertainty,
    sum: &mut [f64],
    weight: &mut [f64],
    skip_mask: Option<&[u64]>,
    weight_sq: Option<&mut Vec<f64>>,
    variance_numerator: Option<&mut Vec<f64>>,
) {
    let samples_per_row = ctx.w_out * ctx.ch;
    let sum_ptr = sum.as_mut_ptr() as usize;
    let weight_ptr = weight.as_mut_ptr() as usize;
    let weight_sq_ptr = weight_sq.map(|plane| plane.as_mut_ptr() as usize);
    let variance_ptr = variance_numerator.map(|plane| plane.as_mut_ptr() as usize);
    let loc = ctx.loc_fields[k]
        .as_ref()
        .map(|field| (field.as_slice(), ctx.loc_grid, ctx.loc_grid));
    (0..ctx.h_out).into_par_iter().for_each(|y| {
        let sum_row = unsafe {
            std::slice::from_raw_parts_mut(
                (sum_ptr as *mut f64).add(y * samples_per_row),
                samples_per_row,
            )
        };
        let weight_row = unsafe {
            std::slice::from_raw_parts_mut(
                (weight_ptr as *mut f64).add(y * samples_per_row),
                samples_per_row,
            )
        };
        let mut weight_sq_row = weight_sq_ptr.map(|ptr| unsafe {
            std::slice::from_raw_parts_mut(
                (ptr as *mut f64).add(y * samples_per_row),
                samples_per_row,
            )
        });
        let mut variance_row = variance_ptr.map(|ptr| unsafe {
            std::slice::from_raw_parts_mut(
                (ptr as *mut f64).add(y * samples_per_row),
                samples_per_row,
            )
        });
        for x in 0..ctx.w_out {
            let pixel = y * ctx.w_out + x;
            if skip_mask.is_some_and(|mask| (mask[pixel >> 6] >> (pixel & 63)) & 1 == 1) {
                continue;
            }
            let Some((sx, sy)) = transform.inverse(x as f32, y as f32) else {
                continue;
            };
            if sx < 0.0
                || sy < 0.0
                || sx >= (image.w - 1) as f32
                || sy >= (image.h - 1) as f32
            {
                continue;
            }
            let mut sampled_science = [f32::NAN; 3];
            let mut sampled_variance = [f32::NAN; 3];
            if !nf_sample_science_variance(
                image,
                uncertainty,
                sx,
                sy,
                ctx.use_lanczos,
                &mut sampled_science,
                &mut sampled_variance,
            ) {
                continue;
            }
            for channel in 0..ctx.ch {
                let multiply = ctx.norms[k].0[channel];
                let normalized_variance = sampled_variance[channel] * multiply * multiply;
                let local_offset = loc
                    .map(|(grid, gw, gh)| {
                        crate::ds_sample_local_field(
                            grid,
                            gw,
                            gh,
                            ctx.ch,
                            channel,
                            x as f32 / ctx.w_out as f32,
                            y as f32 / ctx.h_out as f32,
                        )
                    })
                    .unwrap_or(0.0);
                let value = sampled_science[channel] * multiply
                    + ctx.norms[k].1[channel]
                    + local_offset;
                if !value.is_finite()
                    || !normalized_variance.is_finite()
                    || normalized_variance <= 0.0
                {
                    continue;
                }
                let precision = 1.0 / normalized_variance as f64;
                let index = x * ctx.ch + channel;
                sum_row[index] += value as f64 * precision;
                weight_row[index] += precision;
                if let Some(plane) = weight_sq_row.as_deref_mut() {
                    plane[index] += precision * precision;
                }
                if let Some(plane) = variance_row.as_deref_mut() {
                    plane[index] += precision * precision * normalized_variance as f64;
                }
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn nf_accumulate_formal_cfa(
    ctx: &NfLiteContext,
    k: usize,
    cid: i32,
    transform: crate::DsTransform,
    image: &crate::DsImage,
    uncertainty: &NfCalibrationUncertainty,
    sum: &mut [f64],
    weight: &mut [f64],
    skip_mask: Option<&[u64]>,
    weight_sq: Option<&mut Vec<f64>>,
    variance_numerator: Option<&mut Vec<f64>>,
) {
    if image.ch != 1 || uncertainty.ch != 1 || !matches!(cid, 8..=11) {
        return;
    }
    let half = 0.5f32;
    let sum_ptr = sum.as_mut_ptr() as usize;
    let weight_ptr = weight.as_mut_ptr() as usize;
    let weight_sq_ptr = weight_sq.map(|plane| plane.as_mut_ptr() as usize);
    let variance_ptr = variance_numerator.map(|plane| plane.as_mut_ptr() as usize);
    let fatal = nf_fatal_input_dq();
    let loc = ctx.loc_fields[k]
        .as_ref()
        .map(|field| (field.as_slice(), ctx.loc_grid, ctx.loc_grid));
    let bands = rayon::current_num_threads().max(1);
    let band_h = ctx.h_out.div_ceil(bands);
    (0..bands).into_par_iter().for_each(|band| {
        let oy0 = band * band_h;
        let oy1 = ((band + 1) * band_h).min(ctx.h_out);
        if oy0 >= oy1 {
            return;
        }
        let rows = oy1 - oy0;
        let band_samples = rows * ctx.w_out * 3;
        let sum_band = unsafe {
            std::slice::from_raw_parts_mut(
                (sum_ptr as *mut f64).add(oy0 * ctx.w_out * 3),
                band_samples,
            )
        };
        let weight_band = unsafe {
            std::slice::from_raw_parts_mut(
                (weight_ptr as *mut f64).add(oy0 * ctx.w_out * 3),
                band_samples,
            )
        };
        let mut weight_sq_band = weight_sq_ptr.map(|ptr| unsafe {
            std::slice::from_raw_parts_mut(
                (ptr as *mut f64).add(oy0 * ctx.w_out * 3),
                band_samples,
            )
        });
        let mut variance_band = variance_ptr.map(|ptr| unsafe {
            std::slice::from_raw_parts_mut(
                (ptr as *mut f64).add(oy0 * ctx.w_out * 3),
                band_samples,
            )
        });
        for iy in 0..image.h {
            for ix in 0..image.w {
                let source_pixel = iy * image.w + ix;
                if uncertainty.dq[source_pixel] & fatal != 0 {
                    continue;
                }
                let raw_variance = uncertainty.variance[source_pixel];
                let raw_value = image.data[source_pixel];
                if !raw_value.is_finite() || !raw_variance.is_finite() || raw_variance <= 0.0 {
                    continue;
                }
                let channel = crate::ds_cfa_channel(cid, ix, iy);
                let (rx, ry) = transform.forward(ix as f32, iy as f32);
                if !rx.is_finite() || !ry.is_finite() {
                    continue;
                }
                let (ox, oy) = (rx + 0.5, ry + 0.5);
                let (dx0, dx1, dy0, dy1) = (ox - half, ox + half, oy - half, oy + half);
                let px0 = dx0.floor() as i32;
                let px1 = (dx1.ceil() as i32 - 1).max(px0);
                let py0 = dy0.floor() as i32;
                let py1 = (dy1.ceil() as i32 - 1).max(py0);
                let normalized_variance =
                    raw_variance * ctx.norms[k].0[channel] * ctx.norms[k].0[channel];
                if !normalized_variance.is_finite() || normalized_variance <= 0.0 {
                    continue;
                }
                for opy in py0.max(oy0 as i32)..=py1.min(oy1 as i32 - 1) {
                    let ay = (dy1.min(opy as f32 + 1.0) - dy0.max(opy as f32)).max(0.0);
                    if ay <= 0.0 {
                        continue;
                    }
                    for opx in px0.max(0)..=px1.min(ctx.w_out as i32 - 1) {
                        let ax = (dx1.min(opx as f32 + 1.0) - dx0.max(opx as f32)).max(0.0);
                        let area = (ax * ay) as f64;
                        if area <= 0.0 {
                            continue;
                        }
                        let output_pixel = opy as usize * ctx.w_out + opx as usize;
                        if skip_mask.is_some_and(|mask| {
                            (mask[output_pixel >> 6] >> (output_pixel & 63)) & 1 == 1
                        }) {
                            continue;
                        }
                        let local_offset = loc
                            .map(|(grid, gw, gh)| {
                                crate::ds_sample_local_field(
                                    grid,
                                    gw,
                                    gh,
                                    3,
                                    channel,
                                    opx as f32 / ctx.w_out as f32,
                                    opy as f32 / ctx.h_out as f32,
                                )
                            })
                            .unwrap_or(0.0);
                        let value = raw_value * ctx.norms[k].0[channel]
                            + ctx.norms[k].1[channel]
                            + local_offset;
                        if !value.is_finite() {
                            continue;
                        }
                        let precision = area / normalized_variance as f64;
                        let local_pixel = (opy as usize - oy0) * ctx.w_out + opx as usize;
                        let index = local_pixel * 3 + channel;
                        sum_band[index] += value as f64 * precision;
                        weight_band[index] += precision;
                        if let Some(plane) = weight_sq_band.as_deref_mut() {
                            plane[index] += precision * precision;
                        }
                        if let Some(plane) = variance_band.as_deref_mut() {
                            plane[index] +=
                                precision * precision * normalized_variance as f64;
                        }
                    }
                }
            }
        }
    });
}

/// Una acumulación NF (despacho mono/RGB vs CFA directo) con los parámetros
/// comunes del contexto. Mantiene la geometría idéntica entre pasadas.
#[allow(clippy::too_many_arguments)]
fn nf_accumulate(
    ctx: &NfLiteContext,
    k: usize,
    t: crate::DsTransform,
    img: &crate::DsImage,
    weights: &FrameWeights,
    uncertainty: Option<&NfCalibrationUncertainty>,
    sum: &mut [f64],
    wgt: &mut [f64],
    mask: Option<&[u64]>,
    wsq: Option<&mut Vec<f64>>,
    variance_numerator: Option<&mut Vec<f64>>,
) {
    if let Some(uncertainty) = uncertainty {
        if let Some(cid) = ctx.cfa {
            nf_accumulate_formal_cfa(
                ctx,
                k,
                cid,
                t,
                img,
                uncertainty,
                sum,
                wgt,
                mask,
                wsq,
                variance_numerator,
            );
        } else {
            nf_accumulate_formal_warp(
                ctx,
                k,
                t,
                img,
                uncertainty,
                sum,
                wgt,
                mask,
                wsq,
                variance_numerator,
            );
        }
        return;
    }
    let loc_ref = ctx.loc_fields[k]
        .as_ref()
        .map(|f| (f.as_slice(), ctx.loc_grid, ctx.loc_grid));
    match (ctx.cfa, weights) {
        (Some(cid), FrameWeights::Rgb(fw_rgb)) => {
            crate::ds_drizzle_cfa_accumulate(
                img,
                cid,
                t,
                sum,
                None,
                wgt,
                None,
                None,
                ctx.w_out,
                ctx.h_out,
                1.0,
                1.0,
                1.0,
                ctx.norms[k],
                loc_ref,
                None,
                Some(*fw_rgb),
                mask,
                wsq,
            );
        }
        (_, FrameWeights::Grid(grid)) => {
            crate::ds_warp_accumulate(
                img,
                t,
                sum,
                None,
                wgt,
                None,
                None,
                ctx.w_out,
                ctx.h_out,
                ctx.ch,
                1.0,
                1.0,
                ctx.norms[k],
                loc_ref,
                Some((grid.as_slice(), WEIGHT_GRID, WEIGHT_GRID)),
                ctx.use_lanczos,
                mask,
                wsq,
            );
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Super-binning (F4): salida 0.75x/0.5x para datos sobremuestreados.
// ---------------------------------------------------------------------------

/// Remuestreo por ÁREA de un plano interleaved f32 al factor s = num/den < 1.
/// SCI: media ponderada por área (fotometría de superficie conservada).
/// Con `as_variance`, propaga varianza: VAR' = Σa²·VAR/(Σa)² (independencia
/// aproximada — la correlación del Lanczos se ignora en Lite, documentado).
/// Los NaN (huecos) se excluyen del numerador y del denominador.
pub(crate) fn bin_area_f32(
    data: &[f32],
    w: usize,
    h: usize,
    ch: usize,
    num: usize,
    den: usize,
    as_variance: bool,
) -> (Vec<f32>, usize, usize) {
    let nw = (w * num / den).max(1);
    let nh = (h * num / den).max(1);
    let inv = den as f64 / num as f64; // lado del bloque de entrada por píxel de salida
    let mut out = vec![f32::NAN; nw * nh * ch];
    for oy in 0..nh {
        let y0 = oy as f64 * inv;
        let y1 = ((oy + 1) as f64 * inv).min(h as f64);
        for ox in 0..nw {
            let x0 = ox as f64 * inv;
            let x1 = ((ox + 1) as f64 * inv).min(w as f64);
            for c in 0..ch {
                let mut acc = 0.0f64;
                let mut acc_sq = 0.0f64;
                let mut area_sum = 0.0f64;
                let mut iy = y0.floor() as usize;
                while (iy as f64) < y1 && iy < h {
                    let ay = (y1.min(iy as f64 + 1.0) - y0.max(iy as f64)).max(0.0);
                    let mut ix = x0.floor() as usize;
                    while (ix as f64) < x1 && ix < w {
                        let ax = (x1.min(ix as f64 + 1.0) - x0.max(ix as f64)).max(0.0);
                        let a = ax * ay;
                        let v = data[(iy * w + ix) * ch + c];
                        if a > 0.0 && v.is_finite() {
                            area_sum += a;
                            if as_variance {
                                acc_sq += a * a * v as f64;
                            } else {
                                acc += a * v as f64;
                            }
                        }
                        ix += 1;
                    }
                    iy += 1;
                }
                if area_sum > 0.0 {
                    out[(oy * nw + ox) * ch + c] = if as_variance {
                        (acc_sq / (area_sum * area_sum)) as f32
                    } else {
                        (acc / area_sum) as f32
                    };
                }
            }
        }
    }
    (out, nw, nh)
}

/// Binning del plano DQ: NO_COVERAGE solo si TODOS los píxeles del bloque lo
/// llevan; el resto de bits se acumulan con OR (defecto en cualquier parte
/// del bloque ⇒ declarado).
pub(crate) fn bin_dq(dq: &[u32], w: usize, h: usize, num: usize, den: usize) -> Vec<u32> {
    let nw = (w * num / den).max(1);
    let nh = (h * num / den).max(1);
    let inv = den as f64 / num as f64;
    let mut out = vec![0u32; nw * nh];
    for oy in 0..nh {
        for ox in 0..nw {
            let y0 = (oy as f64 * inv).floor() as usize;
            let y1 = (((oy + 1) as f64 * inv).ceil() as usize).min(h);
            let x0 = (ox as f64 * inv).floor() as usize;
            let x1 = (((ox + 1) as f64 * inv).ceil() as usize).min(w);
            let mut bits = 0u32;
            let mut all_uncovered = true;
            for iy in y0..y1.max(y0 + 1) {
                for ix in x0..x1.max(x0 + 1) {
                    let b = dq[iy.min(h - 1) * w + ix.min(w - 1)];
                    bits |= b & !crate::deepsky_variance::dq::NO_COVERAGE;
                    if b & crate::deepsky_variance::dq::NO_COVERAGE == 0 {
                        all_uncovered = false;
                    }
                }
            }
            if all_uncovered {
                bits |= crate::deepsky_variance::dq::NO_COVERAGE;
            }
            out[oy * nw + ox] = bits;
        }
    }
    out
}

/// σ MRS por celda nativa (~64 px) sobre la luma del frame calibrado.
pub(crate) fn native_sigma_grid(img: &crate::DsImage) -> (Vec<f32>, usize, usize) {
    let gw = (img.w / SIGMA_CELL_PX).max(1);
    let gh = (img.h / SIGMA_CELL_PX).max(1);
    let npx = img.w * img.h;
    let luma: Vec<f32> = if img.ch == 1 {
        img.data.clone()
    } else {
        (0..npx)
            .map(|p| {
                (img.data[p * img.ch] + img.data[p * img.ch + 1] + img.data[p * img.ch + 2]) / 3.0
            })
            .collect()
    };
    let mut grid = vec![0.0f32; gw * gh];
    for gy in 0..gh {
        for gx in 0..gw {
            let x0 = gx * img.w / gw;
            let x1 = ((gx + 1) * img.w / gw).min(img.w);
            let y0 = gy * img.h / gh;
            let y1 = ((gy + 1) * img.h / gh).min(img.h);
            let (cw, chh) = (x1 - x0, y1 - y0);
            let mut cell = Vec::with_capacity(cw * chh);
            for y in y0..y1 {
                cell.extend_from_slice(&luma[y * img.w + x0..y * img.w + x1]);
            }
            grid[gy * gw + gx] = crate::ds_mrs_noise(&cell, cw, chh).max(1e-3);
        }
    }
    (grid, gw, gh)
}

/// Rejilla de pesos 1/σ′² en ESPACIO DE REFERENCIA: para cada celda de la
/// rejilla de salida, el centro se lleva al frame nativo con la inversa del
/// transform y se toma la σ de la celda nativa correspondiente, escalada por
/// la normalización multiplicativa (σ′ = mul·σ). Celdas fuera del frame
/// heredan el peso del borde más cercano (no aportan de todos modos: el
/// acumulador descarta muestras fuera de rango).
pub(crate) fn reference_weight_grid(
    sigma: &(Vec<f32>, usize, usize),
    t: &crate::DsTransform,
    mul_mean: f32,
    native_w: usize,
    native_h: usize,
    w_out: usize,
    h_out: usize,
) -> Vec<f32> {
    let (sg, sgw, sgh) = (&sigma.0, sigma.1, sigma.2);
    let mut grid = vec![0.0f32; WEIGHT_GRID * WEIGHT_GRID];
    for gy in 0..WEIGHT_GRID {
        for gx in 0..WEIGHT_GRID {
            let ox = (gx as f32 + 0.5) / WEIGHT_GRID as f32 * w_out as f32;
            let oy = (gy as f32 + 0.5) / WEIGHT_GRID as f32 * h_out as f32;
            let (nx, ny) = t
                .inverse(ox, oy)
                .unwrap_or((native_w as f32 / 2.0, native_h as f32 / 2.0));
            let cx = ((nx as usize).min(native_w.saturating_sub(1)) * sgw / native_w.max(1))
                .min(sgw - 1);
            let cy = ((ny as usize).min(native_h.saturating_sub(1)) * sgh / native_h.max(1))
                .min(sgh - 1);
            let s = (sg[cy * sgw + cx] * mul_mean.max(1e-6)).max(1e-3);
            grid[gy * WEIGHT_GRID + gx] = 1.0 / (s * s);
        }
    }
    // Tope relativo: mediana × MAX_RELATIVE_WEIGHT.
    let mut sorted = grid.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = sorted[sorted.len() / 2].max(1e-12);
    for v in grid.iter_mut() {
        *v = v.min(median * MAX_RELATIVE_WEIGHT);
    }
    grid
}

/// Curva ruido-vs-nivel por canal (transferencia fotónica EMPÍRICA): σ del
/// residual LOO |y−piloto| en bins del nivel del piloto. Incluye por
/// construcción el shot noise del objeto y el ruido del piloto — es el
/// denominador correcto del residual sin necesitar el gain de la cámara.
/// Los núcleos estelares (nivel alto ⇒ σ alta) dejan de confundirse con
/// outliers, exactamente el fallo del κσ clásico que el plan exige evitar.
pub(crate) struct NoiseCurve {
    lo: f32,
    step: f32,
    sigma: Vec<f32>,
}

const NOISE_BINS: usize = 64;
/// Muestras mínimas por bin para confiar en su σ.
const NOISE_BIN_MIN: f64 = 100.0;
/// E|N(0,σ)| = σ·√(2/π) ⇒ σ = 1.2533·mean|d|.
const MAD_TO_SIGMA: f64 = 1.253_314;

impl NoiseCurve {
    fn bin_of(&self, level: f32) -> usize {
        if !level.is_finite() {
            return 0;
        }
        (((level - self.lo) / self.step) as isize).clamp(0, NOISE_BINS as isize - 1) as usize
    }

    pub(crate) fn eval(&self, level: f32) -> f32 {
        self.sigma[self.bin_of(level)]
    }

    /// Construye la curva desde los acumuladores por bin (Σ|d| y conteo).
    fn from_bins(lo: f32, step: f32, abs_sum: &[f64], count: &[f64]) -> NoiseCurve {
        let mut sigma = vec![0.0f32; NOISE_BINS];
        // 1. σ por bin con muestras suficientes.
        for b in 0..NOISE_BINS {
            if count[b] >= NOISE_BIN_MIN {
                sigma[b] = (MAD_TO_SIGMA * abs_sum[b] / count[b]) as f32;
            }
        }
        // 2. Bins vacíos heredan el vecino inferior (y el primero, el primer
        //    bin poblado por encima).
        let mut last = None;
        for b in 0..NOISE_BINS {
            if sigma[b] > 0.0 {
                last = Some(sigma[b]);
            } else if let Some(v) = last {
                sigma[b] = v;
            }
        }
        let mut next = None;
        for b in (0..NOISE_BINS).rev() {
            if sigma[b] > 0.0 {
                next = Some(sigma[b]);
            } else if let Some(v) = next {
                sigma[b] = v;
            }
        }
        // 3. Monótona no decreciente (el ruido crece con el nivel) + suelo.
        let mut running = 1e-3f32;
        for b in 0..NOISE_BINS {
            running = running.max(sigma[b].max(1e-3));
            sigma[b] = running;
        }
        NoiseCurve { lo, step, sigma }
    }
}

/// Rango robusto (p0.5–p99.9) del nivel por canal, muestreado de los totales.
fn pilot_level_range(
    total_sum: &[f64],
    total_wgt: &[f64],
    npx: usize,
    ch: usize,
) -> Vec<(f32, f32)> {
    let mut out = Vec::with_capacity(ch);
    let step = (npx / 200_000).max(1);
    for c in 0..ch {
        let mut vals: Vec<f32> = (0..npx)
            .step_by(step)
            .filter_map(|p| {
                let i = p * ch + c;
                if total_wgt[i] > 0.0 {
                    Some((total_sum[i] / total_wgt[i]) as f32)
                } else {
                    None
                }
            })
            .collect();
        if vals.is_empty() {
            out.push((0.0, 1.0));
            continue;
        }
        vals.sort_by(|a, b| a.total_cmp(b));
        let lo = vals[vals.len() / 200];
        let hi = vals[(vals.len() as f64 * 0.999) as usize % vals.len()].max(lo + 1.0);
        out.push((lo, hi));
    }
    out
}

fn cov_reduce(wgt: &[f64], npx: usize, ch: usize) -> Vec<f64> {
    (0..npx)
        .map(|p| {
            let mut s = 0.0f64;
            for c in 0..ch {
                s += wgt[p * ch + c];
            }
            s / ch as f64
        })
        .collect()
}

fn struct_allocation_error(label: &str, error: std::collections::TryReserveError) -> String {
    format!("STRUCT omitido: no se pudo reservar {label}: {error}")
}

fn try_zeroed_f64(len: usize, label: &str) -> Result<Vec<f64>, String> {
    let mut out = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|error| struct_allocation_error(label, error))?;
    out.resize(len, 0.0);
    Ok(out)
}

fn try_struct_halves(len: usize) -> Result<(Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>), String> {
    Ok((
        try_zeroed_f64(len, "suma split-half A")?,
        try_zeroed_f64(len, "peso split-half A")?,
        try_zeroed_f64(len, "suma split-half B")?,
        try_zeroed_f64(len, "peso split-half B")?,
    ))
}

fn try_half_luma(s: &[f64], weights: &[f64], npx: usize, ch: usize) -> Result<Vec<f32>, String> {
    let expected = npx
        .checked_mul(ch)
        .ok_or_else(|| "STRUCT omitido: geometría split-half fuera de rango".to_string())?;
    if s.len() != expected || weights.len() != expected {
        return Err("STRUCT omitido: buffers split-half con geometría inconsistente".into());
    }
    let mut out = Vec::new();
    out.try_reserve_exact(npx)
        .map_err(|error| struct_allocation_error("luma split-half", error))?;
    for p in 0..npx {
        let mut vs = 0.0f64;
        let mut vw = 0.0f64;
        for c in 0..ch {
            vs += s[p * ch + c];
            vw += weights[p * ch + c];
        }
        out.push(if vw > 0.0 { (vs / vw) as f32 } else { 0.0 });
    }
    Ok(out)
}

fn try_full_luma(data: &[f32], npx: usize, ch: usize) -> Result<Vec<f32>, String> {
    let expected = npx
        .checked_mul(ch)
        .ok_or_else(|| "STRUCT omitido: geometría SCI fuera de rango".to_string())?;
    if data.len() != expected {
        return Err("STRUCT omitido: SCI con geometría inconsistente".into());
    }
    let mut out = Vec::new();
    out.try_reserve_exact(npx)
        .map_err(|error| struct_allocation_error("luma SCI", error))?;
    if ch == 1 {
        out.extend_from_slice(data);
    } else {
        for p in 0..npx {
            let mut sum = 0.0f32;
            for c in 0..ch {
                sum += data[p * ch + c];
            }
            out.push(sum / ch as f32);
        }
    }
    Ok(out)
}

fn validate_struct_config(config: NfRuntimeConfig) -> Result<(), String> {
    if !config.fdr_q.is_finite() || !(0.0..=0.25).contains(&config.fdr_q) || config.fdr_q == 0.0 {
        return Err(format!(
            "STRUCT omitido: fdrQ={} debe estar en (0, 0.25]",
            config.fdr_q
        ));
    }
    if !config.min_split_sigma.is_finite() || !(1.0..=10.0).contains(&config.min_split_sigma) {
        return Err(format!(
            "STRUCT omitido: minSplitSigma={} debe estar en [1, 10]",
            config.min_split_sigma
        ));
    }
    Ok(())
}

fn validate_full_config(config: NfRuntimeConfig) -> Result<(), String> {
    if !(64..=512).contains(&config.tile_size) || !config.tile_size.is_power_of_two() {
        return Err(format!(
            "tileSize={} no es una potencia de dos entre 64 y 512",
            config.tile_size
        ));
    }
    if !config.max_psf_leakage.is_finite() || !(1e-6..=0.1).contains(&config.max_psf_leakage) {
        return Err(format!(
            "maxPsfLeakage={} debe estar en [1e-6, 0.1]",
            config.max_psf_leakage
        ));
    }
    if !config.max_noise_amplification.is_finite()
        || !(1.0..=10.0).contains(&config.max_noise_amplification)
    {
        return Err(format!(
            "maxNoiseAmplification={} debe estar en [1, 10]",
            config.max_noise_amplification
        ));
    }
    Ok(())
}

/// Prepara el operador geométrico W×PSF. Similarity/affine se representan
/// mediante su Jacobiano exacto y projective mediante una linealización local
/// verificada por tile. LocalDistortion conserva el fallback seguro hasta que
/// exista un contrato de soporte y adjunto publicable para el polinomio.
fn full_supported_warps(
    registered: &[(usize, crate::DsTransform, f64)],
    w: usize,
    h: usize,
) -> Result<Vec<crate::nebula_fusion_full::NfFullWarp>, String> {
    crate::nebula_fusion_full::prepare_full_warps(registered, w, h)
}

fn fit_channel_psfs(
    img: &crate::DsImage,
    stars: &[(f32, f32, f32)],
) -> Option<[crate::deepsky_psf::MoffatPsf; 3]> {
    if img.ch <= 1 {
        let (fit, _report) =
            crate::deepsky_psf::fit_frame_psf(&img.data, img.w, img.h, stars, 0.2)?;
        let psf = fit.at(0.5, 0.5);
        return Some([psf; 3]);
    }
    let npx = img.w.checked_mul(img.h)?;
    let mut out: [Option<crate::deepsky_psf::MoffatPsf>; 3] = [None, None, None];
    for (c, slot) in out.iter_mut().enumerate().take(img.ch.min(3)) {
        let mut plane = Vec::with_capacity(npx);
        plane.extend((0..npx).map(|p| img.data[p * img.ch + c]));
        let (fit, _report) = crate::deepsky_psf::fit_frame_psf(&plane, img.w, img.h, stars, 0.2)?;
        *slot = Some(fit.at(0.5, 0.5));
    }
    if img.ch == 2 {
        out[2] = out[1];
    }
    Some([out[0]?, out[1]?, out[2]?])
}

/// Motor NF-Lite completo: tres pasadas de streaming sobre los frames ya
/// calibrados/registrados/normalizados (escala nativa, drizzle excluido).
pub(crate) fn run_lite(
    ctx: &NfLiteContext,
    load: &dyn Fn(usize) -> Result<crate::DsImage, String>,
    load_uncertainty: Option<
        &dyn Fn(usize) -> Result<NfCalibrationUncertainty, String>,
    >,
    progress: &mut dyn FnMut(&str, usize, usize),
) -> Result<NfLiteOutput, String> {
    let (w_out, h_out, ch) = (ctx.w_out, ctx.h_out, ctx.ch);
    let npx = w_out * h_out;
    let n = ctx.registered.len();
    if n == 0 {
        return Err("NebulaFusion: sin frames registrados".into());
    }
    let mut parameter_fallbacks = Vec::new();
    let formal_uncertainty = load_uncertainty.is_some();
    if !formal_uncertainty {
        parameter_fallbacks.push(
            "VAR/DQ formal no suministrada: NF usa pesos empiricos MRS y declara varianceOrigin=empirical"
                .to_string(),
        );
    }
    if ctx.config.empirical_psd && !ctx.full {
        parameter_fallbacks.push(
            "empiricalPsd=true no aplica a NF-Lite: la VAR efectiva conserva el modelo analítico sin corrección espectral; active Full cuando exista un estimador PSD publicable"
                .to_string(),
        );
    }
    let crossfit_min_frames = if (2..=64).contains(&ctx.config.crossfit_folds) {
        ctx.config.crossfit_folds.saturating_add(1).max(5)
    } else {
        parameter_fallbacks.push(format!(
            "crossfitFolds={} inválido: rechazo cross-fit deshabilitado (se requieren 2..=64)",
            ctx.config.crossfit_folds
        ));
        usize::MAX
    };
    if crossfit_min_frames != usize::MAX && n >= crossfit_min_frames {
        // El motor calcula LOO exacto, estadísticamente más independiente
        // que K-fold para el mismo coste de almacenamiento. La cardinalidad
        // solicitada sí gobierna el gate de activación y el efectivo queda
        // declarado, no fingido en la receta.
        parameter_fallbacks.push(format!(
            "crossfitFolds={} solicitado; piloto efectivo leave-one-out ({} referencias por frame), gate N>={} ",
            ctx.config.crossfit_folds,
            n.saturating_sub(1),
            crossfit_min_frames
        ));
    } else if crossfit_min_frames != usize::MAX {
        parameter_fallbacks.push(format!(
            "crossfitFolds={}: N={} < {}; rechazo cross-fit deshabilitado",
            ctx.config.crossfit_folds, n, crossfit_min_frames
        ));
    }
    // STRUCT es un producto derivado: si su working set no cabe, se omite
    // antes de reservar las cuatro mitades f64. SCI Full/Lite continúa igual.
    let mut struct_fallback = None;
    let struct_enabled = if ctx.struct_mode {
        match validate_struct_config(ctx.config).and_then(|_| {
            crate::deepsky_struct::validate_pipeline_memory_budget(w_out, h_out, ch).map(|_| ())
        }) {
            Ok(()) => true,
            Err(reason) => {
                struct_fallback = Some(reason);
                false
            }
        }
    } else {
        false
    };

    let mut total_sum = vec![0.0f64; npx * ch];
    let mut total_wgt = vec![0.0f64; npx * ch];
    let mut frame_weights: Vec<FrameWeights> = Vec::with_capacity(n);
    let mut frame_sigmas: Vec<[f32; 3]> = Vec::with_capacity(n);
    let mut g1g2_offset_max = 0.0f32;

    // --- Pasada A: pesos por frame + totales S=Σw·y, W=Σw ---
    for (k, &(i, t, _fw)) in ctx.registered.iter().enumerate() {
        crate::pipeline::cancellation_checkpoint(ctx.cancel, "NebulaFusion: totales")?;
        progress("pesos y totales", k + 1, n);
        let img = load(i)?;
        let uncertainty = match load_uncertainty {
            Some(loader) => {
                let uncertainty = loader(i).map_err(|reason| {
                    format!("NebulaFusion: no se pudo cargar VAR/DQ formal del frame {i}: {reason}")
                })?;
                uncertainty.validate_for(&img)?;
                Some(uncertainty)
            }
            None => None,
        };
        let (weights, normalized_sigmas) = if let Some(cid) = ctx.cfa {
            // CFA directo: 1/σ′² POR CANAL desde los sub-planos Bayer.
            let (sigmas, g_off) = cfa_channel_sigmas(&img, cid);
            g1g2_offset_max = g1g2_offset_max.max(g_off.abs());
            let mut fw = [0.0f64; 3];
            let mut normalized = [0.0f32; 3];
            for c in 0..3 {
                let s = (sigmas[c] * ctx.norms[k].0[c].max(1e-6)).max(1e-3) as f64;
                fw[c] = 1.0 / (s * s);
                normalized[c] = s as f32;
            }
            (FrameWeights::Rgb(fw), normalized)
        } else {
            let sigma = native_sigma_grid(&img);
            let mul_mean = {
                let m = ctx.norms[k].0;
                if ch == 1 {
                    m[0]
                } else {
                    (m[0] + m[1] + m[2]) / 3.0
                }
            };
            let mut normalized = image_channel_sigmas(&img);
            for (c, value) in normalized.iter_mut().enumerate() {
                *value = (*value * ctx.norms[k].0[c].max(1e-6)).max(1e-3);
            }
            (
                FrameWeights::Grid(reference_weight_grid(
                    &sigma, &t, mul_mean, img.w, img.h, w_out, h_out,
                )),
                normalized,
            )
        };
        nf_accumulate(
            ctx,
            k,
            t,
            &img,
            &weights,
            uncertainty.as_ref(),
            &mut total_sum,
            &mut total_wgt,
            None,
            None,
            None,
        );
        frame_weights.push(weights);
        frame_sigmas.push(normalized_sigmas);
    }
    let wgt1 = cov_reduce(&total_wgt, npx, ch);
    let total_cov: f64 = wgt1.iter().sum();

    let cfg = crate::deepsky_masks::CrossFitConfig {
        dilate_px: if ctx.use_lanczos { 2 } else { 1 },
        min_frames: crossfit_min_frames,
        ..crate::deepsky_masks::CrossFitConfig::default()
    };
    let crossfit_enabled = n >= cfg.min_frames;
    let mut frame_sum = vec![0.0f64; npx * ch];
    let mut frame_wgt = vec![0.0f64; npx * ch];
    // Warp de la aportación de UN frame (reutiliza los buffers de arriba).
    let frame_weights_ref = &frame_weights;
    let warp_frame = |k: usize,
                      i: usize,
                      t: crate::DsTransform,
                      fs: &mut Vec<f64>,
                      fw: &mut Vec<f64>|
     -> Result<(crate::DsImage, Option<NfCalibrationUncertainty>), String> {
        let img = load(i)?;
        let uncertainty = match load_uncertainty {
            Some(loader) => {
                let uncertainty = loader(i).map_err(|reason| {
                    format!("NebulaFusion: no se pudo cargar VAR/DQ formal del frame {i}: {reason}")
                })?;
                uncertainty.validate_for(&img)?;
                Some(uncertainty)
            }
            None => None,
        };
        fs.iter_mut().for_each(|v| *v = 0.0);
        fw.iter_mut().for_each(|v| *v = 0.0);
        nf_accumulate(
            ctx,
            k,
            t,
            &img,
            &frame_weights_ref[k],
            uncertainty.as_ref(),
            fs,
            fw,
            None,
            None,
            None,
        );
        Ok((img, uncertainty))
    };

    // --- Pasada B1: curva ruido-vs-nivel por canal (residuales LOO) ---
    let ranges = pilot_level_range(&total_sum, &total_wgt, npx, ch);
    let mut abs_bins = vec![vec![0.0f64; NOISE_BINS]; ch];
    let mut cnt_bins = vec![vec![0.0f64; NOISE_BINS]; ch];
    if crossfit_enabled {
        for (k, &(i, t, _fw)) in ctx.registered.iter().enumerate() {
            crate::pipeline::cancellation_checkpoint(ctx.cancel, "NebulaFusion: curva de ruido")?;
            progress("curva de ruido", k + 1, n);
            let _ = warp_frame(k, i, t, &mut frame_sum, &mut frame_wgt)?;
            for c in 0..ch {
                let (lo, hi) = ranges[c];
                let step = ((hi - lo) / NOISE_BINS as f32).max(1e-6);
                for p in 0..npx {
                    let idx = p * ch + c;
                    let wi = frame_wgt[idx];
                    let wrest = total_wgt[idx] - wi;
                    if wi <= 0.0 || wrest <= 0.0 {
                        continue;
                    }
                    let y = frame_sum[idx] / wi;
                    let pilot = (total_sum[idx] - frame_sum[idx]) / wrest;
                    let b = (((pilot as f32 - lo) / step) as isize)
                        .clamp(0, NOISE_BINS as isize - 1) as usize;
                    abs_bins[c][b] += (y - pilot).abs();
                    cnt_bins[c][b] += 1.0;
                }
            }
        }
    }
    let curves: Vec<NoiseCurve> = (0..ch)
        .map(|c| {
            let (lo, hi) = ranges[c];
            let step = ((hi - lo) / NOISE_BINS as f32).max(1e-6);
            NoiseCurve::from_bins(lo, step, &abs_bins[c], &cnt_bins[c])
        })
        .collect();

    // Residual normalizado del frame contra un piloto (peor canal, signo).
    let residual_plane = |fs: &[f64],
                          fw: &[f64],
                          pilot_sum: &[f64],
                          pilot_wgt: &[f64],
                          exclude_self: bool,
                          out: &mut Vec<f32>| {
        out.iter_mut().for_each(|v| *v = 0.0);
        for p in 0..npx {
            let mut worst = 0.0f64;
            for c in 0..ch {
                let idx = p * ch + c;
                let wi = fw[idx];
                if wi <= 0.0 {
                    continue;
                }
                let (ps, pw) = if exclude_self {
                    (pilot_sum[idx] - fs[idx], pilot_wgt[idx] - wi)
                } else {
                    (pilot_sum[idx], pilot_wgt[idx])
                };
                if pw <= 0.0 {
                    continue;
                }
                let y = fs[idx] / wi;
                let pilot = ps / pw;
                let sigma = curves[c].eval(pilot as f32) as f64;
                let r = (y - pilot) / sigma.max(1e-6);
                if r.abs() > worst.abs() {
                    worst = r;
                }
            }
            out[p] = worst as f32;
        }
    };

    // --- Pasada B2: máscaras ronda 1 (piloto contaminable) + totales limpios ---
    // El piloto LOO medio se contamina con el propio outlier visto desde los
    // DEMÁS frames (un cósmico desplaza el piloto y todos parecen desviados).
    // Por eso la ronda 1 solo separa candidatos y construye unos totales
    // LIMPIOS; la ronda 2 revalida cada máscara contra el piloto limpio.
    let mut masks: Vec<crate::deepsky_masks::FrozenFrameMask> = Vec::with_capacity(n);
    let mut clean_sum = vec![0.0f64; npx * ch];
    let mut clean_wgt = vec![0.0f64; npx * ch];
    let mut residual = vec![0.0f32; npx];
    for (k, &(i, t, _fw)) in ctx.registered.iter().enumerate() {
        crate::pipeline::cancellation_checkpoint(ctx.cancel, "NebulaFusion: máscaras r1")?;
        progress("máscaras (ronda 1)", k + 1, n);
        if !crossfit_enabled {
            masks.push(crate::deepsky_masks::FrozenFrameMask {
                positive: Vec::new(),
                negative: Vec::new(),
            });
            continue;
        }
        let (img, uncertainty) = warp_frame(k, i, t, &mut frame_sum, &mut frame_wgt)?;
        residual_plane(
            &frame_sum,
            &frame_wgt,
            &total_sum,
            &total_wgt,
            true,
            &mut residual,
        );
        let mask = crate::deepsky_masks::build_frozen_mask(&residual, w_out, h_out, n, &cfg, false);
        // Acumular este frame en los totales limpios saltando su máscara r1.
        let bits;
        let mask_ref = if mask.is_empty() {
            None
        } else {
            bits = mask.to_bitset(npx);
            Some(bits.as_slice())
        };
        nf_accumulate(
            ctx,
            k,
            t,
            &img,
            &frame_weights_ref[k],
            uncertainty.as_ref(),
            &mut clean_sum,
            &mut clean_wgt,
            mask_ref,
            None,
            None,
        );
        masks.push(mask);
    }
    if !crossfit_enabled {
        clean_sum.copy_from_slice(&total_sum);
        clean_wgt.copy_from_slice(&total_wgt);
    }

    // --- Pasada B3: revalidación contra el piloto limpio + dilatación ---
    let mut rejection_low = vec![0.0f64; npx];
    let mut rejection_high = vec![0.0f64; npx];
    let mut masked_samples = 0usize;
    if crossfit_enabled {
        for (k, &(i, t, _fw)) in ctx.registered.iter().enumerate() {
            crate::pipeline::cancellation_checkpoint(ctx.cancel, "NebulaFusion: máscaras r2")?;
            progress("máscaras (ronda 2)", k + 1, n);
            let _ = warp_frame(k, i, t, &mut frame_sum, &mut frame_wgt)?;
            // Piloto limpio EXCLUYENDO la aportación limpia de este frame:
            // donde el frame estaba enmascarado en r1, su aporte a los
            // totales limpios ya es cero.
            let bits_r1 = masks[k].to_bitset(npx);
            for p in 0..npx {
                residual[p] = 0.0;
                let self_in_clean = (bits_r1[p >> 6] >> (p & 63)) & 1 == 0;
                for c in 0..ch {
                    let idx = p * ch + c;
                    let wi = frame_wgt[idx];
                    if wi <= 0.0 {
                        continue;
                    }
                    let (ps, pw) = if self_in_clean {
                        (clean_sum[idx] - frame_sum[idx], clean_wgt[idx] - wi)
                    } else {
                        (clean_sum[idx], clean_wgt[idx])
                    };
                    if pw <= 0.0 {
                        continue;
                    }
                    let y = frame_sum[idx] / wi;
                    let pilot = ps / pw;
                    let sigma = curves[c].eval(pilot as f32) as f64;
                    let r = ((y - pilot) / sigma.max(1e-6)) as f32;
                    if r.abs() > residual[p].abs() {
                        residual[p] = r;
                    }
                }
            }
            let mask =
                crate::deepsky_masks::build_frozen_mask(&residual, w_out, h_out, n, &cfg, true);
            for &p in &mask.positive {
                let p = p as usize;
                let mut wsum = 0.0f64;
                for c in 0..ch {
                    wsum += frame_wgt[p * ch + c];
                }
                rejection_high[p] += wsum / ch as f64;
            }
            for &p in &mask.negative {
                let p = p as usize;
                let mut wsum = 0.0f64;
                for c in 0..ch {
                    wsum += frame_wgt[p * ch + c];
                }
                rejection_low[p] += wsum / ch as f64;
            }
            masked_samples += mask.len();
            masks[k] = mask;
        }
    }

    // --- Pasada C: integración final con máscaras congeladas + Σw² ---
    total_sum.iter_mut().for_each(|v| *v = 0.0);
    total_wgt.iter_mut().for_each(|v| *v = 0.0);
    let mut weight_sq = vec![0.0f64; npx * ch];
    // Σ(w²·VAR) permite propagar de forma exacta tambien el drop CFA, donde
    // el factor de area hace que VAR no sea simplemente 1/Σw.
    let mut variance_numerator = formal_uncertainty.then(|| vec![0.0f64; npx * ch]);
    // STRUCT (F7): mitades independientes por paridad de índice temporal.
    let mut halves: Option<(Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>)> = if struct_enabled {
        let allocation = npx
            .checked_mul(ch)
            .ok_or_else(|| "STRUCT omitido: geometría split-half fuera de rango".to_string())
            .and_then(try_struct_halves);
        match allocation {
            Ok(halves) => Some(halves),
            Err(reason) => {
                struct_fallback = Some(reason);
                None
            }
        }
    } else {
        None
    };
    for (k, &(i, t, _fw)) in ctx.registered.iter().enumerate() {
        crate::pipeline::cancellation_checkpoint(ctx.cancel, "NebulaFusion: integración")?;
        progress("integración final", k + 1, n);
        let img = load(i)?;
        let uncertainty = match load_uncertainty {
            Some(loader) => {
                let uncertainty = loader(i).map_err(|reason| {
                    format!("NebulaFusion: no se pudo cargar VAR/DQ formal del frame {i}: {reason}")
                })?;
                uncertainty.validate_for(&img)?;
                Some(uncertainty)
            }
            None => None,
        };
        let bits;
        let mask_ref = if masks[k].is_empty() {
            None
        } else {
            bits = masks[k].to_bitset(npx);
            Some(bits.as_slice())
        };
        nf_accumulate(
            ctx,
            k,
            t,
            &img,
            &frame_weights_ref[k],
            uncertainty.as_ref(),
            &mut total_sum,
            &mut total_wgt,
            mask_ref,
            Some(&mut weight_sq),
            variance_numerator.as_mut(),
        );
        if let Some((sa, wa, sb, wb)) = halves.as_mut() {
            let (hs, hw) = if k % 2 == 0 { (sa, wa) } else { (sb, wb) };
            nf_accumulate(
                ctx,
                k,
                t,
                &img,
                &frame_weights_ref[k],
                uncertainty.as_ref(),
                hs,
                hw,
                mask_ref,
                None,
                None,
            );
        }
    }

    // --- Productos ---
    let mut final_data = vec![0.0f32; npx * ch];
    let mut variance = vec![f32::NAN; npx * ch];
    let mut neff = vec![0.0f32; npx * ch];
    let mut dq = vec![0u32; npx];
    for p in 0..npx {
        let mut covered = false;
        for c in 0..ch {
            let i = p * ch + c;
            let wv = total_wgt[i];
            if wv > 0.0 {
                covered = true;
                final_data[i] = (total_sum[i] / wv) as f32;
                variance[i] = variance_numerator
                    .as_ref()
                    .map(|numerator| (numerator[i] / (wv * wv)) as f32)
                    .unwrap_or_else(|| (1.0 / wv) as f32);
                if weight_sq[i] > 0.0 {
                    neff[i] = ((wv * wv) / weight_sq[i]) as f32;
                }
            }
        }
        if !covered {
            dq[p] |= crate::deepsky_variance::dq::NO_COVERAGE;
        }
    }
    // STRUCT (F7): lumas y σ de las mitades (la validación corre tras el
    // modo Full, sobre el SCI definitivo).
    let struct_halves: Option<(Vec<f32>, Vec<f32>, f32, f32)> =
        if let Some((sa, wa, sb, wb)) = halves.take() {
            match (
                try_half_luma(&sa, &wa, npx, ch),
                try_half_luma(&sb, &wb, npx, ch),
            ) {
                (Ok(la), Ok(lb)) => {
                    let s_a = crate::ds_mrs_noise(&la, w_out, h_out);
                    let s_b = crate::ds_mrs_noise(&lb, w_out, h_out);
                    Some((la, lb, s_a, s_b))
                }
                (Err(reason), _) | (_, Err(reason)) => {
                    struct_fallback = Some(reason);
                    None
                }
            }
        } else {
            None
        };

    // --- Modo Full (F6): recombinación espectral con PSF objetivo ---
    // Mantiene NEFF/cobertura/rechazos del pase C y REEMPLAZA SCI y VAR por
    // la combinación GLS por frecuencia. Si la PSF no es utilizable, degrada
    // a Lite con la razón REGISTRADA (jamás en silencio).
    let mut full_report: Option<(f32, usize, usize)> = None;
    let mut full_fallback: Option<String> = None;
    if ctx.full {
        if ctx.cfa.is_some() {
            full_fallback =
                Some("el modo Full requiere la ruta demosaiced (CFA directo llega después)".into());
        } else if let Err(reason) = validate_full_config(ctx.config) {
            full_fallback = Some(format!("configuración Full inválida: {reason}"));
        } else if ctx.config.empirical_psd {
            let reason = "empiricalPsd=true todavía no dispone de un estimador PSD cross-fit por frame/canal; Full se degrada a Lite para no etiquetar una PSD analítica como empírica".to_string();
            parameter_fallbacks.push(reason.clone());
            full_fallback = Some(reason);
        } else {
            let warps = full_supported_warps(ctx.registered, w_out, h_out);
            if let Err(reason) = warps {
                full_fallback = Some(reason);
            } else {
                let warps = warps.expect("resultado comprobado");
                // 1. PSF Moffat independiente por frame/canal. Si un canal
                // RGB no sostiene una medida, no se replica la PSF de luma.
                let mut psfs: Vec<[crate::deepsky_psf::MoffatPsf; 3]> = Vec::with_capacity(n);
                let mut psf_fail: Option<String> = None;
                for (k, &(i, _t, _fw)) in ctx.registered.iter().enumerate() {
                    crate::pipeline::cancellation_checkpoint(ctx.cancel, "NebulaFusion Full: PSF")?;
                    progress("ajuste PSF Moffat por canal", k + 1, n);
                    let img = load(i)?;
                    let stars = ctx.stars.get(i).map(|v| v.as_slice()).unwrap_or(&[]);
                    match fit_channel_psfs(&img, stars) {
                        Some(per_channel) => psfs.push(per_channel),
                        None => {
                            psf_fail = Some(format!(
                                "frame {i}: al menos un canal no tiene censo suficiente para medir su PSF; Full degradado a Lite"
                            ));
                            break;
                        }
                    }
                }
                if let Some(reason) = psf_fail {
                    full_fallback = Some(reason);
                } else {
                    // 2. Pase W: los datos ausentes y outliers quedan con
                    // peso CERO en un store explícito. No se imputa el piloto.
                    let dir = std::env::temp_dir().join("zenith_nf_full");
                    let job = crate::pipeline::new_job_id("w");
                    let tag = format!("nf_full_{}_{}", std::process::id(), job);
                    let vtag = format!("nf_full_valid_{}_{}", std::process::id(), job);
                    let ptag = format!("nf_full_precision_{}_{}", std::process::id(), job);
                    let mut wstore =
                        crate::frame_store::AdaptiveFrameStore::new(n, npx * ch, &dir, &tag)?;
                    let mut vstore =
                        crate::frame_store::AdaptiveFrameStore::new(n, npx * ch, &dir, &vtag)?;
                    let mut pstore = crate::frame_store::AdaptiveFrameStore::new(
                        n,
                        npx * ch,
                        &dir,
                        &ptag,
                    )?;
                    for (k, &(i, t, _fw)) in ctx.registered.iter().enumerate() {
                        crate::pipeline::cancellation_checkpoint(
                            ctx.cancel,
                            "NebulaFusion Full: warp",
                        )?;
                        progress("warp y máscara para recombinación", k + 1, n);
                        let _ = warp_frame(k, i, t, &mut frame_sum, &mut frame_wgt)?;
                        let rejected = masks[k].to_bitset(npx);
                        let mut y = vec![0.0f32; npx * ch];
                        let mut valid = vec![0.0f32; npx * ch];
                        let mut precision = vec![0.0f32; npx * ch];
                        for p in 0..npx {
                            let masked = (rejected[p >> 6] >> (p & 63)) & 1 == 1;
                            for c in 0..ch {
                                let idx = p * ch + c;
                                if masked || frame_wgt[idx] <= 0.0 {
                                    if !masked {
                                        dq[p] |= crate::deepsky_variance::dq::EDGE;
                                    }
                                    continue;
                                }
                                let value = (frame_sum[idx] / frame_wgt[idx]) as f32;
                                if value.is_finite() {
                                    y[idx] = value;
                                    valid[idx] = 1.0;
                                    precision[idx] = frame_wgt[idx] as f32;
                                } else {
                                    dq[p] |= crate::deepsky_variance::dq::NAN_INPUT;
                                }
                            }
                        }
                        wstore.put(k, &y)?;
                        vstore.put(k, &valid)?;
                        pstore.put(k, &precision)?;
                    }
                    // 3. Recombinación GLS; los tiles con alguna muestra de
                    // peso cero usan media espacial local, sin contarla en NEFF.
                    let inputs = crate::nebula_fusion_full::NfFullInputs {
                        warped: &wstore,
                        validity: &vstore,
                        precision: formal_uncertainty.then_some(&pstore),
                        n_frames: n,
                        w: w_out,
                        h: h_out,
                        ch,
                        sigmas: &frame_sigmas,
                        psfs: &psfs,
                        warps: &warps,
                        lanczos: ctx.use_lanczos,
                        tile_size: ctx.config.tile_size,
                        max_psf_leakage: ctx.config.max_psf_leakage,
                        max_noise_amplification: ctx.config.max_noise_amplification,
                        cancel: ctx.cancel,
                    };
                    match crate::nebula_fusion_full::combine_full(&inputs, progress) {
                        Ok(fout) if fout.gls_tiles > 0 => {
                            final_data = fout.sci;
                            variance = fout.var_map;
                            neff = fout.neff_map;
                            full_report =
                                Some((fout.target_fwhm, fout.tiles_fallback, fout.tiles_total));
                        }
                        Ok(fout) => {
                            full_fallback = Some(format!(
                                "ningún tile sostuvo el operador GLS ({} tiles con datos ausentes/PSF no recuperable); SCI Lite conservado",
                                fout.tiles_fallback
                            ));
                        }
                        Err(reason) => {
                            full_fallback = Some(format!(
                                "solver Full rechazado; SCI Lite conservado: {reason}"
                            ));
                        }
                    }
                }
            }
        }
    }

    nf_finalize_scientific_pixels(
        &mut final_data,
        &mut variance,
        &mut neff,
        &mut dq,
        npx,
        ch,
    )?;

    // --- STRUCT (F7): validación split-half sobre el SCI definitivo ---
    let mut struct_map: Option<Vec<f32>> = None;
    let mut struct_residual: Option<Vec<f32>> = None;
    let mut struct_accepted: Option<Vec<(usize, usize)>> = None;
    if let Some((la, lb, s_a, s_b)) = struct_halves {
        progress("STRUCT: validación por mitades", 1, 1);
        match try_full_luma(&final_data, npx, ch).and_then(|full_luma| {
            crate::deepsky_struct::build_struct(
                &full_luma,
                &la,
                &lb,
                w_out,
                h_out,
                s_a,
                s_b,
                ctx.config.fdr_q,
                ctx.config.min_split_sigma,
            )
        }) {
            Ok(out_s) => {
                struct_accepted = Some(out_s.accepted_per_level);
                struct_map = Some(out_s.struct_map);
                struct_residual = Some(out_s.residual);
            }
            Err(reason) => struct_fallback = Some(reason),
        }
    }

    if let Some(reason) = struct_fallback.take() {
        let effective = if ctx.full && full_report.is_some() {
            format!("FullWithStruct degradado a Full; SCI Full conservado: {reason}")
        } else if ctx.full {
            format!("FullWithStruct degradado a Lite sin STRUCT: {reason}")
        } else {
            format!("STRUCT omitido; SCI Lite conservado: {reason}")
        };
        progress(&effective, 1, 1);
        struct_fallback = Some(effective);
    }

    let weight_map = cov_reduce(&total_wgt, npx, ch);
    let final_cov: f64 = weight_map.iter().sum();
    let rej_pct = if total_cov > 0.0 {
        100.0 * (1.0 - final_cov / total_cov)
    } else {
        0.0
    };
    let mean_cov = if total_cov > 0.0 {
        n as f64 * final_cov / total_cov
    } else {
        0.0
    };

    let effective_config = NfEffectiveConfig {
        variance_weighting: if formal_uncertainty {
            "calibration_var_dq_spatial"
        } else {
            "empirical_mrs"
        },
        crossfit_mode: if crossfit_enabled {
            "leave_one_out"
        } else {
            "disabled"
        },
        crossfit_reference_frames: if crossfit_enabled {
            n.saturating_sub(1)
        } else {
            0
        },
        full_active: full_report.is_some(),
        tile_size: full_report.map(|_| ctx.config.tile_size),
        max_psf_leakage: full_report.map(|_| ctx.config.max_psf_leakage),
        max_noise_amplification: full_report
            .map(|_| ctx.config.max_noise_amplification),
        // empiricalPsd=true currently fails its explicit gate, so no
        // successful effective configuration can claim it.
        empirical_psd: false,
        struct_active: struct_accepted.is_some(),
        fdr_q: struct_accepted.as_ref().map(|_| ctx.config.fdr_q),
        min_split_sigma: struct_accepted
            .as_ref()
            .map(|_| ctx.config.min_split_sigma),
    };

    Ok(NfLiteOutput {
        final_data,
        wgt1,
        weight_map,
        rejection_low,
        rejection_high,
        rej_pct,
        mean_cov,
        products: NfLiteProducts {
            variance,
            neff,
            dq,
            masked_samples,
            variance_origin: if formal_uncertainty {
                crate::deepsky_variance::VarianceOrigin::HybridEmpiricalPropagated
            } else {
                crate::deepsky_variance::VarianceOrigin::Empirical
            },
            g1g2_offset_max: ctx.cfa.map(|_| g1g2_offset_max),
        },
        full_report,
        full_fallback,
        struct_map,
        struct_residual,
        struct_accepted,
        struct_fallback,
        parameter_fallbacks,
        effective_config,
    })
}

/// Recorte de un plano interleaved f32 (layout del máster) al rectángulo del
/// auto-crop — para VAR/NEFF, que comparten geometría con el máster.
pub(crate) fn crop_interleaved_f32(
    data: &[f32],
    w: usize,
    _h: usize,
    ch: usize,
    x0: usize,
    y0: usize,
    nw: usize,
    nh: usize,
) -> Vec<f32> {
    let mut out = Vec::with_capacity(nw * nh * ch);
    for y in 0..nh {
        let src = ((y0 + y) * w + x0) * ch;
        out.extend_from_slice(&data[src..src + nw * ch]);
    }
    out
}

/// Recorte del plano DQ (u32 por píxel).
pub(crate) fn crop_plane_u32(
    data: &[u32],
    w: usize,
    x0: usize,
    y0: usize,
    nw: usize,
    nh: usize,
) -> Vec<u32> {
    let mut out = Vec::with_capacity(nw * nh);
    for y in 0..nh {
        let src = (y0 + y) * w + x0;
        out.extend_from_slice(&data[src..src + nw]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_registered(n: usize) -> Vec<(usize, crate::DsTransform, f64)> {
        (0..n)
            .map(|i| {
                (
                    i,
                    crate::DsTransform::from_similarity((1.0, 0.0, 0.0, 0.0)),
                    1.0,
                )
            })
            .collect()
    }

    fn neutral_norms(n: usize) -> Vec<([f32; 3], [f32; 3])> {
        vec![([1.0, 1.0, 1.0], [0.0, 0.0, 0.0]); n]
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn flat_sensor(read_noise_e: f64) -> crate::deepsky_sim::SimSensor {
        crate::deepsky_sim::SimSensor {
            gain_e_per_adu: 1.0,
            read_noise_e,
            bias_adu: 0.0,
            dark_adu_per_s: 0.0,
            full_well_adu: 65535.0,
            hot_pixels: Vec::new(),
            bayer: None,
            vignette: None,
        }
    }

    fn run_on_frames(frames: Vec<Vec<f32>>, w: usize, h: usize) -> NfLiteOutput {
        let n = frames.len();
        let registered = identity_registered(n);
        let norms = neutral_norms(n);
        let loc: Vec<Option<Vec<f32>>> = vec![None; n];
        let cancel = no_cancel();
        let ctx = NfLiteContext {
            registered: &registered,
            norms: &norms,
            loc_fields: &loc,
            loc_grid: 24,
            w_out: w,
            h_out: h,
            ch: 1,
            use_lanczos: false,
            cancel: &cancel,
            cfa: None,
            full: false,
            stars: &[],
            struct_mode: false,
            config: NfRuntimeConfig::default(),
        };
        let load = |i: usize| -> Result<crate::DsImage, String> {
            Ok(crate::DsImage {
                data: frames[i].clone(),
                w,
                h,
                ch: 1,
                bayer: None,
            })
        };
        run_lite(&ctx, &load, None, &mut |_, _, _| {}).expect("run_lite")
    }

    /// Gate F3: SNR de fondo ≥98% del óptimo 1/σ². Dos poblaciones de ruido
    /// (σ=3 y σ=12): la media óptima tiene Var=1/Σ(1/σ²); la salida NF-Lite
    /// no puede superar Var_opt/0.98² (y debe batir a la media uniforme).
    #[test]
    fn gate_f3_background_snr_at_least_98pct_of_optimal() {
        let (w, h) = (160, 160);
        let scene = crate::deepsky_sim::SimScene {
            width: w,
            height: h,
            background_adu: 400.0,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0, 1.0, 1.0],
            stars: Vec::new(),
        };
        let mut frames = Vec::new();
        let mut inv_var_sum = 0.0f64;
        let mut var_sum = 0.0f64;
        for k in 0..12 {
            // Frames 0-5: σ_lectura=3 (σ²≈409); 6-11: σ_lectura=25 (σ²≈1025).
            let rn = if k < 6 { 3.0 } else { 25.0 };
            let sensor = flat_sensor(rn);
            let exp = crate::deepsky_sim::SimExposure {
                exposure_s: 60.0,
                dx: 0.0,
                dy: 0.0,
                seed: 7_000 + k as u64,
            };
            let (frame, tv) = crate::deepsky_sim::render_light(&scene, &sensor, &exp);
            let mean_var = tv.iter().sum::<f64>() / tv.len() as f64;
            inv_var_sum += 1.0 / mean_var;
            var_sum += mean_var;
            frames.push(frame);
        }
        let optimal_var = 1.0 / inv_var_sum;
        let uniform_var = var_sum / (12.0f64 * 12.0);
        let out = run_on_frames(frames, w, h);
        // Varianza espacial del fondo de salida (escena plana ⇒ toda la
        // dispersión es ruido; excluye 4 px de borde).
        let mut vals = Vec::new();
        for y in 4..h - 4 {
            for x in 4..w - 4 {
                vals.push(out.final_data[y * w + x] as f64);
            }
        }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (vals.len() as f64 - 1.0);
        assert!(
            var < optimal_var / (0.98 * 0.98),
            "Var salida {var:.2} > óptimo/0.98² ({:.2})",
            optimal_var / (0.98 * 0.98)
        );
        // Con σ² de 409 vs 1025 el óptimo teórico es ~0.81× la media uniforme:
        // exigir mejora real (<0.9×), no un margen imposible para el dataset.
        assert!(
            var < uniform_var * 0.9,
            "NF-Lite ({var:.2}) no mejora la media uniforme ({uniform_var:.2})"
        );
        // VAR reportada coherente con la varianza real (±15%).
        let center = (h / 2) * w + w / 2;
        let reported = out.products.variance[center] as f64;
        assert!(
            (reported / var - 1.0).abs() < 0.15,
            "VAR reportada {reported:.2} vs medida {var:.2}"
        );
    }

    /// Gate F3: cósmicos y traza de satélite inyectados — recall >95%,
    /// falso rechazo de píxeles limpios <1e-4, precisión >99% contando la
    /// dilatación por interpolación como acierto (radio 2 del inyectado).
    #[test]
    fn gate_f3_crossfit_rejection_precision_and_recall() {
        let (w, h) = (128, 128);
        let scene = crate::deepsky_sim::SimScene {
            width: w,
            height: h,
            background_adu: 300.0,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0, 1.0, 1.0],
            stars: Vec::new(),
        };
        let sensor = flat_sensor(3.0);
        let mut frames = Vec::new();
        let mut injected: Vec<(usize, usize)> = Vec::new(); // (frame, px)
        for k in 0..10 {
            let exp = crate::deepsky_sim::SimExposure {
                exposure_s: 60.0,
                dx: 0.0,
                dy: 0.0,
                seed: 9_500 + k as u64,
            };
            let (mut frame, _) = crate::deepsky_sim::render_light(&scene, &sensor, &exp);
            if k == 2 {
                // 12 cósmicos sueltos de +2000 ADU (~100σ).
                for j in 0..12 {
                    let p = (10 + j * 9) * w + (15 + j * 8);
                    frame[p] += 2000.0;
                    injected.push((k, p));
                }
            }
            if k == 6 {
                // Traza de satélite: diagonal de +400 ADU (~20σ).
                for s in 20..100 {
                    let p = s * w + s;
                    frame[p] += 400.0;
                    injected.push((k, p));
                }
            }
            frames.push(frame);
        }
        let n = frames.len();
        let registered = identity_registered(n);
        let norms = neutral_norms(n);
        let loc: Vec<Option<Vec<f32>>> = vec![None; n];
        let cancel = no_cancel();
        let ctx = NfLiteContext {
            registered: &registered,
            norms: &norms,
            loc_fields: &loc,
            loc_grid: 24,
            w_out: w,
            h_out: h,
            ch: 1,
            use_lanczos: false,
            cancel: &cancel,
            cfa: None,
            full: false,
            stars: &[],
            struct_mode: false,
            config: NfRuntimeConfig::default(),
        };
        let frames_ref = &frames;
        let load = |i: usize| -> Result<crate::DsImage, String> {
            Ok(crate::DsImage {
                data: frames_ref[i].clone(),
                w,
                h,
                ch: 1,
                bayer: None,
            })
        };
        let out = run_lite(&ctx, &load, None, &mut |_, _, _| {}).expect("run_lite");
        // Recall: cada píxel inyectado debe estar rechazado (mapa alto > 0).
        let hit = injected
            .iter()
            .filter(|&&(_, p)| out.rejection_high[p] > 0.0)
            .count();
        let recall = hit as f64 / injected.len() as f64;
        assert!(recall > 0.95, "recall {recall:.3} <= 0.95");
        // Falso rechazo: rechazos lejos (>3 px) de cualquier inyección.
        let mut injected_map = vec![false; w * h];
        for &(_, p) in &injected {
            let (px, py) = (p % w, p / w);
            for dy in -3i64..=3 {
                for dx in -3i64..=3 {
                    let nx = px as i64 + dx;
                    let ny = py as i64 + dy;
                    if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                        injected_map[ny as usize * w + nx as usize] = true;
                    }
                }
            }
        }
        let mut false_rejected = 0usize;
        let mut rejected_total = 0usize;
        for p in 0..w * h {
            if out.rejection_high[p] > 0.0 || out.rejection_low[p] > 0.0 {
                rejected_total += 1;
                if !injected_map[p] {
                    false_rejected += 1;
                }
            }
        }
        let false_rate = false_rejected as f64 / (w * h) as f64;
        assert!(false_rate < 1e-4, "falso rechazo {false_rate:.2e}");
        let precision = 1.0 - false_rejected as f64 / rejected_total.max(1) as f64;
        assert!(precision > 0.99, "precisión {precision:.4}");
        // El máster no conserva el cósmico: el valor queda cerca del fondo.
        let sample = injected[0].1;
        assert!(
            (out.final_data[sample] - 300.0).abs() < 30.0,
            "el cósmico sobrevivió: {}",
            out.final_data[sample]
        );
    }

    /// Gate F3: fotometría — el flujo de una estrella se conserva 1±0.005.
    #[test]
    fn gate_f3_photometry_slope_within_half_percent() {
        let (w, h) = (96, 96);
        let star = crate::deepsky_sim::SimStar {
            x: 48.0,
            y: 48.0,
            flux_adu: 50_000.0,
            fwhm_px: 3.0,
            moffat_beta: None,
        };
        let scene = crate::deepsky_sim::SimScene {
            width: w,
            height: h,
            background_adu: 200.0,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0, 1.0, 1.0],
            stars: vec![star],
        };
        let sensor = flat_sensor(3.0);
        let mut frames = Vec::new();
        for k in 0..10 {
            let exp = crate::deepsky_sim::SimExposure {
                exposure_s: 60.0,
                dx: 0.0,
                dy: 0.0,
                seed: 21_000 + k as u64,
            };
            frames.push(crate::deepsky_sim::render_light(&scene, &sensor, &exp).0);
        }
        let out = run_on_frames(frames, w, h);
        // Apertura radio 12 px (4·FWHM): flujo = Σ(v − fondo).
        let mut flux = 0.0f64;
        for y in 0..h {
            for x in 0..w {
                let dx = x as f64 - 48.0;
                let dy = y as f64 - 48.0;
                if dx * dx + dy * dy <= 144.0 {
                    flux += out.final_data[y * w + x] as f64 - 200.0;
                }
            }
        }
        let ratio = flux / 50_000.0;
        assert!(
            (ratio - 1.0).abs() < 0.005,
            "pendiente de flujo {ratio:.4} fuera de 1±0.005"
        );
    }

    /// Gate F4: CFA directo — cociente de canales sin sesgo (<1%) y SIN
    /// patrón 2×2 residual en la salida (las medias por fase de paridad de
    /// cada canal coinciden). 8 frames RGGB con dithers que cubren las 4
    /// paridades para que cada canal tenga cobertura completa.
    #[test]
    fn gate_f4_cfa_channel_ratio_and_no_mosaic_pattern() {
        let (w, h) = (96, 96);
        let color = [0.8f64, 1.0, 0.6];
        let scene = crate::deepsky_sim::SimScene {
            width: w,
            height: h,
            background_adu: 400.0,
            gradient_adu_per_px: (0.0, 0.0),
            color,
            stars: Vec::new(),
        };
        let sensor = crate::deepsky_sim::SimSensor {
            gain_e_per_adu: 1.0,
            read_noise_e: 3.0,
            bias_adu: 0.0,
            dark_adu_per_s: 0.0,
            full_well_adu: 65535.0,
            hot_pixels: Vec::new(),
            bayer: Some(8), // RGGB
            vignette: None,
        };
        let dithers = [(0.0f32, 0.0f32), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)];
        let mut frames = Vec::new();
        let mut registered = Vec::new();
        for k in 0..8usize {
            let exp = crate::deepsky_sim::SimExposure {
                exposure_s: 60.0,
                dx: 0.0,
                dy: 0.0,
                seed: 51_000 + k as u64,
            };
            frames.push(crate::deepsky_sim::render_light(&scene, &sensor, &exp).0);
            let (dx, dy) = dithers[k % 4];
            registered.push((
                k,
                crate::DsTransform::from_similarity((1.0, 0.0, dx, dy)),
                1.0f64,
            ));
        }
        let norms = neutral_norms(8);
        let loc: Vec<Option<Vec<f32>>> = vec![None; 8];
        let cancel = no_cancel();
        let ctx = NfLiteContext {
            registered: &registered,
            norms: &norms,
            loc_fields: &loc,
            loc_grid: 24,
            w_out: w,
            h_out: h,
            ch: 3,
            use_lanczos: false,
            cancel: &cancel,
            cfa: Some(8),
            full: false,
            stars: &[],
            struct_mode: false,
            config: NfRuntimeConfig::default(),
        };
        let frames_ref = &frames;
        let load = |i: usize| -> Result<crate::DsImage, String> {
            Ok(crate::DsImage {
                data: frames_ref[i].clone(),
                w,
                h,
                ch: 1,
                bayer: Some(8),
            })
        };
        let out = run_lite(&ctx, &load, None, &mut |_, _, _| {}).expect("run_lite CFA");
        // Cociente de canales: media interior por canal vs verdad 400·color.
        let mut ch_mean = [0.0f64; 3];
        let mut ch_cnt = [0.0f64; 3];
        // Medias por fase de paridad 2×2 (patrón mosaico residual).
        let mut phase_mean = [[0.0f64; 4]; 3];
        let mut phase_cnt = [[0.0f64; 4]; 3];
        for y in 4..h - 4 {
            for x in 4..w - 4 {
                let p = y * w + x;
                for c in 0..3 {
                    let v = out.final_data[p * 3 + c] as f64;
                    if !v.is_finite() || v == 0.0 {
                        continue;
                    }
                    ch_mean[c] += v;
                    ch_cnt[c] += 1.0;
                    let phase = (y & 1) * 2 + (x & 1);
                    phase_mean[c][phase] += v;
                    phase_cnt[c][phase] += 1.0;
                }
            }
        }
        for c in 0..3 {
            let mean = ch_mean[c] / ch_cnt[c].max(1.0);
            let truth = 400.0 * color[c];
            assert!(
                (mean / truth - 1.0).abs() < 0.01,
                "canal {c}: media {mean:.2} vs verdad {truth:.2} (sesgo >1%)"
            );
            let phases: Vec<f64> = (0..4)
                .map(|ph| phase_mean[c][ph] / phase_cnt[c][ph].max(1.0))
                .collect();
            let pmax = phases.iter().cloned().fold(f64::MIN, f64::max);
            let pmin = phases.iter().cloned().fold(f64::MAX, f64::min);
            assert!(
                (pmax - pmin) / mean < 0.015,
                "canal {c}: patrón CFA residual {:.3}% entre fases 2×2",
                100.0 * (pmax - pmin) / mean
            );
        }
        assert_eq!(out.products.dq.len(), w * h);
        assert!(out.products.g1g2_offset_max.is_some());
    }

    /// Gate F4: super-binning — brillo de superficie conservado, varianza
    /// propagada VAR/4 en 0.5x, flujo de apertura escala con el área del
    /// píxel (unidad declarada en receta), dimensiones correctas en 0.75x.
    #[test]
    fn gate_f4_superbinning_flux_and_variance() {
        let (w, h) = (64, 64);
        let mut sci = vec![100.0f32; w * h];
        // "Estrella": bloque 3×3 de +100 ADU (flujo 900 sobre fondo).
        for dy in 0..3 {
            for dx in 0..3 {
                sci[(30 + dy) * w + 30 + dx] += 100.0;
            }
        }
        let var = vec![4.0f32; w * h];
        let (b_sci, nw, nh) = bin_area_f32(&sci, w, h, 1, 1, 2, false);
        assert_eq!((nw, nh), (32, 32));
        let (b_var, _, _) = bin_area_f32(&var, w, h, 1, 1, 2, true);
        // Fondo conservado (brillo de superficie) y VAR = 4·(1²·4)/4² = 1.
        assert!((b_sci[0] - 100.0).abs() < 1e-4);
        assert!((b_var[0] - 1.0).abs() < 1e-4);
        // Flujo de apertura: Σ(v−fondo) escala por el área del píxel (1/4).
        let flux_in: f64 = sci.iter().map(|&v| (v - 100.0) as f64).sum();
        let flux_out: f64 = b_sci.iter().map(|&v| (v - 100.0) as f64).sum();
        assert!(
            (flux_out / (flux_in * 0.25) - 1.0).abs() < 0.01,
            "flujo binned {flux_out:.1} vs esperado {:.1}",
            flux_in * 0.25
        );
        // 0.75x: dimensiones 3/4.
        let (_, nw75, nh75) = bin_area_f32(&sci, w, h, 1, 3, 4, false);
        assert_eq!((nw75, nh75), (48, 48));
        // DQ: NO_COVERAGE solo si todo el bloque lo lleva.
        let mut dq = vec![0u32; w * h];
        dq[0] = crate::deepsky_variance::dq::NO_COVERAGE;
        dq[1] = crate::deepsky_variance::dq::NO_COVERAGE;
        dq[w] = crate::deepsky_variance::dq::NO_COVERAGE;
        dq[w + 1] = crate::deepsky_variance::dq::NO_COVERAGE;
        dq[2] = crate::deepsky_variance::dq::HOT_COLD;
        let b_dq = bin_dq(&dq, w, h, 1, 2);
        assert_eq!(b_dq[0], crate::deepsky_variance::dq::NO_COVERAGE);
        assert_eq!(
            b_dq[1] & crate::deepsky_variance::dq::HOT_COLD,
            crate::deepsky_variance::dq::HOT_COLD
        );
        assert_eq!(b_dq[1] & crate::deepsky_variance::dq::NO_COVERAGE, 0);
    }

    /// Auditoría G1/G2: un offset inyectado entre los dos sub-planos verdes
    /// se mide con el signo/magnitud correctos.
    #[test]
    fn test_cfa_channel_sigmas_measures_g1g2_offset() {
        let (w, h) = (64, 64);
        let mut data = vec![0.0f32; w * h];
        // RGGB (cid 8): R=(par,par), G1=(impar,par), G2=(par,impar), B=(impar,impar).
        let mut lcg = 12345u64;
        let mut noise = || {
            lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((lcg >> 33) as f32 / (1u64 << 32) as f32) - 0.5
        };
        for y in 0..h {
            for x in 0..w {
                let base = match (x & 1, y & 1) {
                    (0, 0) => 100.0, // R
                    (1, 0) => 200.0, // G1
                    (0, 1) => 210.0, // G2 (offset +10)
                    _ => 50.0,       // B
                };
                data[y * w + x] = base + noise();
            }
        }
        let img = crate::DsImage {
            data,
            w,
            h,
            ch: 1,
            bayer: Some(8),
        };
        let (sigmas, g_off) = cfa_channel_sigmas(&img, 8);
        assert!(
            g_off.abs() > 9.0 && g_off.abs() < 11.0,
            "offset G1G2 {g_off:.2} fuera de ±[9,11]"
        );
        assert!(sigmas.iter().all(|&s| s > 0.0 && s < 5.0));
    }

    /// Gate F5+F6 end-to-end: run_lite en modo Full con seeing mixto (2.6 y
    /// 4.5 px). El flujo completo: detección estelar real → PSF Moffat por
    /// frame → pase W → recombinación espectral. La FWHM del máster queda
    /// cerca de la Γ objetivo (no degradada a la peor PSF) y la fotometría
    /// se conserva.
    #[test]
    fn gate_f56_full_mode_end_to_end() {
        let (w, h) = (256, 256);
        let n = 12usize;
        let sensor = flat_sensor(3.0);
        let mut lcg = 777u64;
        let mut jit = || {
            lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((lcg >> 33) as f64 / (1u64 << 32) as f64) - 0.5
        };
        // Rejilla 5×5 de estrellas con jitter (mismas posiciones en todos los
        // frames; la FWHM cambia por frame = seeing).
        let positions: Vec<(f64, f64, f64)> = (0..25)
            .map(|s| {
                let (gx, gy) = (s % 5, s / 5);
                (
                    30.0 + gx as f64 * 48.0 + jit() * 4.0,
                    30.0 + gy as f64 * 48.0 + jit() * 4.0,
                    18_000.0 + s as f64 * 900.0,
                )
            })
            .collect();
        let mut frames = Vec::new();
        let mut catalogs: Vec<Vec<(f32, f32, f32)>> = Vec::new();
        for k in 0..n {
            let fwhm = if k < 6 { 2.6 } else { 4.5 };
            let scene = crate::deepsky_sim::SimScene {
                width: w,
                height: h,
                background_adu: 200.0,
                gradient_adu_per_px: (0.0, 0.0),
                color: [1.0, 1.0, 1.0],
                stars: positions
                    .iter()
                    .map(|&(x, y, flux)| crate::deepsky_sim::SimStar {
                        x,
                        y,
                        flux_adu: flux,
                        fwhm_px: fwhm,
                        moffat_beta: Some(2.5),
                    })
                    .collect(),
            };
            let exp = crate::deepsky_sim::SimExposure {
                exposure_s: 60.0,
                dx: 0.0,
                dy: 0.0,
                seed: 61_000 + k as u64,
            };
            let (frame, _) = crate::deepsky_sim::render_light(&scene, &sensor, &exp);
            catalogs.push(crate::ds_detect_stars(&frame, w, h, 80));
            frames.push(frame);
        }
        let registered = identity_registered(n);
        let norms = neutral_norms(n);
        let loc: Vec<Option<Vec<f32>>> = vec![None; n];
        let cancel = no_cancel();
        let ctx = NfLiteContext {
            registered: &registered,
            norms: &norms,
            loc_fields: &loc,
            loc_grid: 24,
            w_out: w,
            h_out: h,
            ch: 1,
            use_lanczos: false,
            cancel: &cancel,
            cfa: None,
            full: true,
            stars: &catalogs,
            struct_mode: true,
            config: NfRuntimeConfig::default(),
        };
        let frames_ref = &frames;
        let load = |i: usize| -> Result<crate::DsImage, String> {
            Ok(crate::DsImage {
                data: frames_ref[i].clone(),
                w,
                h,
                ch: 1,
                bayer: None,
            })
        };
        let _struct_budget = crate::deepsky_struct::override_test_memory_budget(1);
        let out = run_lite(&ctx, &load, None, &mut |_, _, _| {}).expect("run_lite full");
        assert!(
            out.full_fallback.is_none(),
            "Full degradado: {:?}",
            out.full_fallback
        );
        assert!(out.struct_map.is_none());
        assert!(out.struct_residual.is_none());
        assert!(
            out.struct_fallback
                .as_deref()
                .is_some_and(|reason| reason.contains("FullWithStruct degradado a Full")),
            "fallback STRUCT no registrado: {:?}",
            out.struct_fallback
        );
        let (target_fwhm, _fb, total) = out.full_report.expect("full_report");
        assert!(total > 0);
        // Con β=2.5 las alas Moffat hunden la MTF a media frecuencia y la
        // restricción de amplificación ≤1.5 exige una Γ más ancha que con
        // PSFs gaussianas (el gate espectral puro converge en 3.2 px con
        // β≈gaussiano). Lo exigible aquí: NO degradarse a la peor población.
        assert!(
            target_fwhm < 4.4,
            "Γ objetivo {target_fwhm:.2} px degradada hacia la peor PSF (4.5)"
        );
        // FWHM medida en el máster sobre la estrella más brillante (lejos de
        // bordes): no degradada a la peor población (4.5 px).
        let bright = positions[24];
        let fit = crate::ds_fit_star_psf(
            &out.final_data,
            w,
            h,
            bright.0.round() as usize,
            bright.1.round() as usize,
            200.0,
        )
        .expect("fit máster");
        let master_fwhm = fit.sigma * 2.3548;
        assert!(
            master_fwhm < 4.7,
            "FWHM del máster {master_fwhm:.2} px — la combinación se degradó a la peor PSF"
        );
        // Fotometría: apertura r=12 sobre una estrella aislada del centro.
        let star = positions[12];
        let mut flux = 0.0f64;
        for y in 0..h {
            for x in 0..w {
                let dx = x as f64 - star.0;
                let dy = y as f64 - star.1;
                if dx * dx + dy * dy <= 144.0 {
                    flux += out.final_data[y * w + x] as f64 - 200.0;
                }
            }
        }
        let ratio = flux / star.2;
        assert!(
            (ratio - 1.0).abs() < 0.05,
            "flujo end-to-end {ratio:.3} fuera de 1±0.05"
        );
    }

    /// Gate F7 end-to-end: run_lite con struct_mode — una "nebulosa" (blob
    /// gaussiano) presente en TODOS los frames aparece en STRUCT; los mapas
    /// existen y el residual sobre el blob es pequeño frente a su señal.
    #[test]
    fn gate_f7_struct_mode_end_to_end() {
        let (w, h) = (96, 96);
        let n = 16usize; // mínimo operativo del preflight (división 8/8)
        let scene = crate::deepsky_sim::SimScene {
            width: w,
            height: h,
            background_adu: 250.0,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0, 1.0, 1.0],
            stars: vec![crate::deepsky_sim::SimStar {
                x: 48.0,
                y: 48.0,
                flux_adu: 30_000.0,
                fwhm_px: 8.0,
                moffat_beta: None,
            }],
        };
        let sensor = flat_sensor(3.0);
        let mut frames = Vec::new();
        for k in 0..n {
            let exp = crate::deepsky_sim::SimExposure {
                exposure_s: 60.0,
                dx: 0.0,
                dy: 0.0,
                seed: 71_000 + k as u64,
            };
            frames.push(crate::deepsky_sim::render_light(&scene, &sensor, &exp).0);
        }
        let registered = identity_registered(n);
        let norms = neutral_norms(n);
        let loc: Vec<Option<Vec<f32>>> = vec![None; n];
        let cancel = no_cancel();
        let ctx = NfLiteContext {
            registered: &registered,
            norms: &norms,
            loc_fields: &loc,
            loc_grid: 24,
            w_out: w,
            h_out: h,
            ch: 1,
            use_lanczos: false,
            cancel: &cancel,
            cfa: None,
            full: false,
            stars: &[],
            struct_mode: true,
            config: NfRuntimeConfig::default(),
        };
        let frames_ref = &frames;
        let load = |i: usize| -> Result<crate::DsImage, String> {
            Ok(crate::DsImage {
                data: frames_ref[i].clone(),
                w,
                h,
                ch: 1,
                bayer: None,
            })
        };
        let out = run_lite(&ctx, &load, None, &mut |_, _, _| {}).expect("run_lite struct");
        assert!(out.struct_fallback.is_none());
        let sm = out.struct_map.as_ref().expect("struct_map");
        let sr = out.struct_residual.as_ref().expect("struct_residual");
        let acc = out.struct_accepted.as_ref().expect("accepted");
        let total_accepted: usize = acc.iter().map(|&(a, _)| a).sum();
        assert!(total_accepted > 0, "STRUCT no aceptó ningún coeficiente");
        // El blob aparece en STRUCT: su señal sobre el coarse en el centro es
        // una fracción sustancial de la señal real (~413 ADU de pico).
        let center = 48 * w + 48;
        let signal = out.final_data[center] - 250.0;
        assert!(
            sm[center] - 250.0 > 0.5 * signal,
            "STRUCT no retiene el blob: {} vs señal {}",
            sm[center] - 250.0,
            signal
        );
        // Residual pequeño sobre el blob (la estructura fue aceptada).
        assert!(
            sr[center].abs() < 0.5 * signal,
            "residual retiene el blob: {}",
            sr[center]
        );
    }

    /// VAR y NEFF con frames idénticos en ruido: VAR≈σ²/N (±10%), NEFF≈N.
    #[test]
    fn test_var_and_neff_for_equal_frames() {
        let (w, h) = (96, 96);
        let scene = crate::deepsky_sim::SimScene {
            width: w,
            height: h,
            background_adu: 250.0,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0, 1.0, 1.0],
            stars: Vec::new(),
        };
        let sensor = flat_sensor(4.0);
        let n = 8usize;
        let mut frames = Vec::new();
        let mut true_var = 0.0f64;
        for k in 0..n {
            let exp = crate::deepsky_sim::SimExposure {
                exposure_s: 60.0,
                dx: 0.0,
                dy: 0.0,
                seed: 31_000 + k as u64,
            };
            let (f, tv) = crate::deepsky_sim::render_light(&scene, &sensor, &exp);
            true_var += tv.iter().sum::<f64>() / tv.len() as f64;
            frames.push(f);
        }
        true_var /= n as f64;
        let out = run_on_frames(frames, w, h);
        let center = (h / 2) * w + w / 2;
        let var = out.products.variance[center] as f64;
        let expected = true_var / n as f64;
        assert!(
            (var / expected - 1.0).abs() < 0.10,
            "VAR {var:.2} vs esperado {expected:.2}"
        );
        let neff = out.products.neff[center] as f64;
        assert!((neff - n as f64).abs() < 0.5, "NEFF {neff:.2} vs {n}");
        assert_eq!(out.products.dq[center], 0);
    }

    #[test]
    fn formal_calibration_variance_and_dq_govern_lite_weights() {
        let (w, h) = (8usize, 8usize);
        let frames = [vec![10.0f32; w * h], vec![20.0f32; w * h]];
        let mut uncertainties = [
            NfCalibrationUncertainty {
                variance: vec![1.0; w * h],
                dq: vec![0; w * h],
                w,
                h,
                ch: 1,
            },
            NfCalibrationUncertainty {
                variance: vec![4.0; w * h],
                dq: vec![0; w * h],
                w,
                h,
                ch: 1,
            },
        ];
        let excluded = 3 * w + 3;
        uncertainties[0].dq[excluded] = crate::deepsky_variance::dq::HOT_COLD
            | crate::deepsky_variance::dq::INTERPOLATED;
        uncertainties[0].variance[excluded] = f32::NAN;
        let registered = identity_registered(2);
        let norms = neutral_norms(2);
        let loc = vec![None, None];
        let cancel = no_cancel();
        let ctx = NfLiteContext {
            registered: &registered,
            norms: &norms,
            loc_fields: &loc,
            loc_grid: 24,
            w_out: w,
            h_out: h,
            ch: 1,
            use_lanczos: false,
            cancel: &cancel,
            cfa: None,
            full: false,
            stars: &[],
            struct_mode: false,
            config: NfRuntimeConfig::default(),
        };
        let load = |index: usize| {
            Ok(crate::DsImage {
                data: frames[index].clone(),
                w,
                h,
                ch: 1,
                bayer: None,
            })
        };
        let load_uncertainty = |index: usize| Ok(uncertainties[index].clone());
        let out = run_lite(
            &ctx,
            &load,
            Some(&load_uncertainty),
            &mut |_, _, _| {},
        )
        .expect("NF formal");

        let clean = 2 * w + 2;
        assert!((out.final_data[clean] - 12.0).abs() < 1.0e-6);
        assert!((out.products.variance[clean] - 0.8).abs() < 1.0e-6);
        assert!((out.products.neff[clean] - 1.470_588_2).abs() < 1.0e-5);
        assert_eq!(out.final_data[excluded], 20.0);
        assert_eq!(out.products.variance[excluded], 4.0);
        assert_eq!(out.products.neff[excluded], 1.0);
        assert_eq!(out.products.dq[excluded], 0);
        assert_eq!(
            out.products.variance_origin,
            crate::deepsky_variance::VarianceOrigin::HybridEmpiricalPropagated
        );
    }

    #[test]
    fn formal_variance_missing_without_fatal_dq_fails_closed() {
        let image = crate::DsImage {
            data: vec![1.0; 16],
            w: 4,
            h: 4,
            ch: 1,
            bayer: None,
        };
        let uncertainty = NfCalibrationUncertainty {
            variance: vec![f32::NAN; 16],
            dq: vec![0; 16],
            w: 4,
            h: 4,
            ch: 1,
        };
        let error = uncertainty
            .validate_for(&image)
            .expect_err("VAR ausente no puede degradarse silenciosamente");
        assert!(error.contains("VAR no positiva/no finita"));
    }

    #[test]
    fn incomplete_rgb_pixel_invalidates_all_science_channels() {
        let mut science = vec![1.0, 2.0, f32::NAN];
        let mut variance = vec![1.0, 1.0, f32::NAN];
        let mut neff = vec![2.0, 2.0, 0.0];
        let mut dq = vec![0u32];
        nf_finalize_scientific_pixels(
            &mut science,
            &mut variance,
            &mut neff,
            &mut dq,
            1,
            3,
        )
        .expect("contrato RGB");
        assert!(science.iter().all(|value| value.is_nan()));
        assert!(variance.iter().all(|value| value.is_nan()));
        assert!(neff.iter().all(|value| *value == 0.0));
        assert_ne!(dq[0] & crate::deepsky_variance::dq::NO_COVERAGE, 0);
    }

    #[test]
    fn full_geometry_gate_accepts_supported_warps_and_rejects_local_distortion() {
        let angle = 0.5f32.to_radians();
        let rotated = crate::DsTransform::from_similarity((angle.cos(), angle.sin(), 0.25, -0.4));
        let registered = vec![(0usize, rotated, 1.0f64)];
        let warps = full_supported_warps(&registered, 4096, 3072)
            .expect("una rotación similarity tiene operador W×PSF");
        assert_eq!(
            warps[0].kind,
            crate::nebula_fusion_full::NfFullWarpKind::Affine
        );

        let translated = vec![(
            0usize,
            crate::DsTransform::from_similarity((1.0, 0.0, 0.25, -0.4)),
            1.0f64,
        )];
        let warps = full_supported_warps(&translated, 4096, 3072)
            .expect("la traslación tiene operador por frame");
        assert_eq!(
            warps[0].kind,
            crate::nebula_fusion_full::NfFullWarpKind::Translation
        );

        let affine = crate::DsTransform {
            model: crate::DsRegistrationModel::Affine,
            h: [1.002, -0.006, 0.3, 0.004, 0.998, -0.2, 0.0, 0.0, 1.0],
            poly: [0.0; 12],
            norm: [0.0, 0.0, 1.0],
        };
        let projective = crate::DsTransform {
            model: crate::DsRegistrationModel::Projective,
            h: [1.0, -0.002, 0.2, 0.001, 1.0, -0.1, 2e-7, -1e-7, 1.0],
            poly: [0.0; 12],
            norm: [0.0, 0.0, 1.0],
        };
        let mixed = vec![(0, affine, 1.0), (1, projective, 1.0)];
        let warps = full_supported_warps(&mixed, 4096, 3072)
            .expect("affine y projective suaves deben pasar el preflight");
        assert_eq!(
            warps[0].kind,
            crate::nebula_fusion_full::NfFullWarpKind::Affine
        );
        assert_eq!(
            warps[1].kind,
            crate::nebula_fusion_full::NfFullWarpKind::Projective
        );

        let local = crate::DsTransform {
            model: crate::DsRegistrationModel::LocalDistortion,
            h: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            poly: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0],
            norm: [0.0, 0.0, 1.0],
        };
        let error = full_supported_warps(&[(0, local, 1.0)], 4096, 3072)
            .expect_err("LocalDistortion debe conservar fallback seguro");
        assert!(
            error.contains("LocalDistortion"),
            "fallback no auditable: {error}"
        );
    }
}
