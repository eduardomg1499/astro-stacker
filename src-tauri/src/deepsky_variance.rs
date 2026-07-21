//! Contrato lineal de cielo profundo (F1): varianza por píxel, número
//! efectivo de muestras (NEFF) y máscara de calidad DQ.
//!
//! Este módulo es la base de NebulaFusion/EIDR: los másters de calibración
//! dejan de ser solo una media/mediana y pasan a llevar VAR + NEFF, y la
//! calibración propaga Var(S) = (V_L + V_B + k²·V_D)/f² + n²·V_f/f⁴.
//! Classic streaming 1x también publica VAR/NEFF/DQ. Las rutas Classic GPU,
//! tiled y drizzle sólo podrán hacerlo cuando conserven sus momentos y suma
//! de pesos al cuadrado; hasta entonces declaran el producto no disponible en
//! vez de inventar incertidumbre.

#![allow(dead_code)] // API consumida progresivamente por F2/F3 (NF-Lite)

use std::sync::atomic::AtomicBool;

/// Bits de la máscara de calidad DQ (u32 por píxel). Un píxel limpio vale 0.
/// Los bits son acumulativos: un píxel puede estar saturado Y ser hot.
pub(crate) mod dq {
    /// Por encima del límite de linealidad/full-well declarado.
    pub const SATURATED: u32 = 1 << 0;
    /// Zona de respuesta no lineal (cerca de saturación, sin llegar a clip).
    pub const NONLINEAR: u32 = 1 << 1;
    /// Hot/cold pixel detectado en los másters de calibración.
    pub const HOT_COLD: u32 = 1 << 2;
    /// Rechazado por el cross-fit (cosmic/satélite/outlier) — lo fija F3.
    pub const COSMIC: u32 = 1 << 3;
    /// NaN/Inf/BLANK en el archivo de entrada; nunca debe tratarse como cero
    /// científicamente válido.
    pub const NAN_INPUT: u32 = 1 << 4;
    /// Flat inválido en ese píxel (f <= umbral de división 0.05).
    pub const FLAT_INVALID: u32 = 1 << 5;
    /// Sin cobertura geométrica en la salida (hueco de warp/drizzle).
    pub const NO_COVERAGE: u32 = 1 << 6;
    /// Valor interpolado (no medido): p.ej. debayer o relleno de preview.
    pub const INTERPOLATED: u32 = 1 << 7;
    /// Borde del frame nativo (halo de warp / recorte de registro).
    pub const EDGE: u32 = 1 << 8;
    /// EIDR no publicó la reconstrucción de ese tile: SCI procede del piloto
    /// limitado a Nyquist nativo, no del iterado super-resuelto.
    pub const EIDR_FALLBACK_NATIVE: u32 = 1 << 9;
    /// La calibración continuó bajo AllowDegraded. Este bit impide confundir
    /// un producto exploratorio con un bundle científico elegible.
    pub const DEGRADED_CALIBRATION: u32 = 1 << 10;
    /// SCI fue sustituida o mezclada por la puerta local EIDR, pero la
    /// covarianza necesaria para propagar VAR/NEFF del producto efectivo no
    /// está disponible. VAR debe ser NaN y NEFF 0; nunca se reutilizan las
    /// capas del solve super-resuelto como si aún describieran esa señal.
    pub const EIDR_UNCERTAINTY_UNAVAILABLE: u32 = 1 << 11;
}

/// Procedencia de la varianza — se registra en la receta y en los headers
/// FITS para que cada producto declare de dónde salió su incertidumbre.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VarianceOrigin {
    /// Propagada término a término desde másters con varianza propia.
    Propagated,
    /// El término de ruido del light se estimó empíricamente y después se
    /// propagaron las incertidumbres de bias/dark/flat. Evita declarar como
    /// puramente propagada una capa que no parte de un modelo de cámara.
    HybridEmpiricalPropagated,
    /// Modelo de cámara: shot noise + read noise desde gain/read-noise reales.
    CameraModel,
    /// Estimada empíricamente del propio frame (ruido MRS por canal). Se usa
    /// cuando faltan metadatos; NUNCA se inventan electrones.
    Empirical,
}

impl VarianceOrigin {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            VarianceOrigin::Propagated => "propagated",
            VarianceOrigin::HybridEmpiricalPropagated => "hybrid_empirical_propagated",
            VarianceOrigin::CameraModel => "camera_model",
            VarianceOrigin::Empirical => "empirical",
        }
    }
}

/// Máster de calibración con incertidumbre: el plano combinado más la
/// varianza DEL VALOR COMBINADO por píxel y el número efectivo de muestras.
pub(crate) struct MasterWithVariance {
    /// Valor combinado (media/mediana) — BIT-IDÉNTICO a la ruta clásica
    /// `ds_combine_master_store` (misma aritmética y orden de reducción).
    pub data: Vec<f32>,
    /// Varianza del valor combinado en ADU². Con n<2 no hay estimador de
    /// dispersión: se marca NaN y el consumidor debe caer a `Empirical`.
    pub variance: Vec<f32>,
    /// Número efectivo de muestras: n para media; n·(2/π) para mediana
    /// (eficiencia asintótica de la mediana bajo ruido gaussiano).
    pub neff: Vec<f32>,
    pub frames: usize,
    pub use_median: bool,
}

/// Máster productivo con media robusta, incertidumbre y calidad espacial.
/// rejected_fraction permite auditar defectos sin convertir una muestra
/// inválida en un cero científicamente válido.
pub(crate) struct RobustMasterWithVariance {
    pub data: Vec<f32>,
    pub variance: Vec<f32>,
    pub neff: Vec<f32>,
    pub dq: Vec<u32>,
    pub rejected_fraction: Vec<f32>,
    pub frames: usize,
}

/// Combina masters con máscara MAD congelada y media de valores aceptados.
/// La VAR corresponde al estimador publicado y NEFF varía por píxel.
pub(crate) fn combine_master_store_robust(
    store: &crate::frame_store::AdaptiveFrameStore,
    frame_count: usize,
    pixel_count: usize,
    cancel: &std::sync::Arc<AtomicBool>,
) -> Result<RobustMasterWithVariance, String> {
    if frame_count == 0 || pixel_count == 0 {
        return Err("máster robusto con geometría o censo vacío".into());
    }
    let mut sys = sysinfo::System::new_all();
    sys.refresh_memory();
    let tile_budget = (sys.available_memory().saturating_mul(15) / 100)
        .min(512 * 1024 * 1024)
        .max(1024 * 1024) as usize;
    let bytes_per_pixel = frame_count.saturating_mul(4).saturating_add(36);
    let tile_len = (tile_budget / bytes_per_pixel.max(1))
        .clamp(1, pixel_count)
        .min(262_144);

    fn try_plane<T: Clone>(len: usize, value: T, label: &str) -> Result<Vec<T>, String> {
        let mut output = Vec::new();
        output
            .try_reserve_exact(len)
            .map_err(|error| format!("máster robusto: reserva {label}: {error}"))?;
        output.resize(len, value);
        Ok(output)
    }
    let mut data = try_plane(pixel_count, f32::NAN, "SCI")?;
    let mut variance = try_plane(pixel_count, f32::NAN, "VAR")?;
    let mut neff = try_plane(pixel_count, 0.0f32, "NEFF")?;
    let mut dq_plane = try_plane(pixel_count, 0u32, "DQ")?;
    let mut rejected_fraction = try_plane(pixel_count, 0.0f32, "rechazo")?;

    let mut start = 0usize;
    while start < pixel_count {
        crate::pipeline::cancellation_checkpoint(
            cancel.as_ref(),
            "combinación robusta de másters",
        )?;
        let end = (start + tile_len).min(pixel_count);
        let mut frame_tiles = Vec::with_capacity(frame_count);
        for frame_index in 0..frame_count {
            frame_tiles.push(store.get_range(frame_index, start..end)?);
        }
        use rayon::prelude::*;
        let tile: Vec<(f32, f32, f32, u32, f32)> = (0..end - start)
            .into_par_iter()
            .map_init(
                || {
                    (
                        Vec::<f32>::with_capacity(frame_count),
                        Vec::<f32>::with_capacity(frame_count),
                    )
                },
                |(values, deviations), pixel| {
                    values.clear();
                    values.extend(frame_tiles.iter().map(|frame| frame[pixel]));
                    match crate::deepsky_calibration_stats::robust_calibration_stats(
                        values,
                        deviations,
                    ) {
                        Some(stats) => {
                            let mut flags = 0u32;
                            if stats.nonfinite_samples > 0 {
                                flags |= dq::NAN_INPUT;
                            }
                            let rejected = stats.rejected_samples + stats.nonfinite_samples;
                            (
                                stats.mean,
                                stats.variance_of_mean,
                                stats.accepted_samples as f32,
                                flags,
                                rejected as f32 / frame_count as f32,
                            )
                        }
                        None => (
                            f32::NAN,
                            f32::NAN,
                            0.0,
                            dq::NAN_INPUT | dq::NO_COVERAGE,
                            1.0,
                        ),
                    }
                },
            )
            .collect();
        for (offset, (mean, var, effective, flags, rejected)) in tile.into_iter().enumerate() {
            let index = start + offset;
            data[index] = mean;
            variance[index] = var;
            neff[index] = effective;
            dq_plane[index] = flags;
            rejected_fraction[index] = rejected;
        }
        start = end;
    }

    Ok(RobustMasterWithVariance {
        data,
        variance,
        neff,
        dq: dq_plane,
        rejected_fraction,
        frames: frame_count,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DarkVarianceConvention {
    /// Se resta un dark crudo 1:1 y no se resta bias por separado.
    RawDarkOnly,
    /// Se resta bias y un dark térmico construido con un bias independiente.
    IndependentThermalDark,
    /// El dark térmico es D_raw-B y comparte el mismo máster bias con el light.
    SharedBiasSubtractedThermal,
}

/// Varianza del numerador calibrado sin contar dos veces el bias compartido.
pub(crate) fn calibrated_numerator_variance(
    light_variance: f32,
    bias_variance: f32,
    dark_variance: f32,
    dark_scale: f32,
    convention: DarkVarianceConvention,
) -> Result<f32, String> {
    if ![light_variance, bias_variance, dark_variance, dark_scale]
        .iter()
        .all(|value| value.is_finite())
        || light_variance < 0.0
        || bias_variance < 0.0
        || dark_variance < 0.0
        || dark_scale < 0.0
    {
        return Err("términos de varianza de calibración inválidos".into());
    }
    let k2 = dark_scale * dark_scale;
    let value = match convention {
        DarkVarianceConvention::RawDarkOnly => {
            if (dark_scale - 1.0).abs() > 1.0e-6 {
                return Err("un dark crudo sólo puede restarse 1:1".into());
            }
            light_variance + dark_variance
        }
        DarkVarianceConvention::IndependentThermalDark => {
            light_variance + bias_variance + k2 * dark_variance
        }
        DarkVarianceConvention::SharedBiasSubtractedThermal => {
            // L-B-k(D-B) = L-kD+(k-1)B.
            light_variance
                + k2 * dark_variance
                + (dark_scale - 1.0).powi(2) * bias_variance
        }
    };
    Ok(value)
}

pub(crate) struct FlatDivisionResult {
    pub science: Vec<f32>,
    pub variance: Vec<f32>,
    pub dq: Vec<u32>,
}

/// Divide un numerador ya calibrado por un flat normalizado, propagando VAR.
/// Un flat no finito/débil/no lineal invalida SCI: nunca se limita a 0.05 para
/// fabricar una amplificación de hasta 20x.
#[allow(clippy::too_many_arguments)]
pub(crate) fn divide_by_flat_scientific(
    numerator: &[f32],
    numerator_variance: &[f32],
    flat: &[f32],
    flat_variance: &[f32],
    width: usize,
    height: usize,
    channels: usize,
    flat_channels: usize,
    initial_dq: &[u32],
    minimum_response: f32,
) -> Result<FlatDivisionResult, String> {
    let pixels = width
        .checked_mul(height)
        .ok_or("flat científico: geometría fuera de rango")?;
    let samples = pixels
        .checked_mul(channels)
        .ok_or("flat científico: muestras fuera de rango")?;
    let flat_samples = pixels
        .checked_mul(flat_channels)
        .ok_or("flat científico: muestras de flat fuera de rango")?;
    if !matches!(channels, 1 | 3)
        || !matches!(flat_channels, 1 | 3)
        || numerator.len() != samples
        || numerator_variance.len() != samples
        || flat.len() != flat_samples
        || flat_variance.len() != flat_samples
        || initial_dq.len() != pixels
        || !minimum_response.is_finite()
        || minimum_response <= 0.0
    {
        return Err("flat científico: planos/layout incompatibles".into());
    }
    let mut science = vec![f32::NAN; samples];
    let mut variance = vec![f32::NAN; samples];
    let mut dq_out = initial_dq.to_vec();
    for pixel in 0..pixels {
        for channel in 0..channels {
            let index = pixel * channels + channel;
            let flat_index = pixel * flat_channels + channel.min(flat_channels - 1);
            let n = numerator[index];
            let vn = numerator_variance[index];
            let f = flat[flat_index];
            let vf = flat_variance[flat_index];
            if !n.is_finite() || !vn.is_finite() || vn < 0.0 {
                dq_out[pixel] |= dq::NAN_INPUT;
                continue;
            }
            if !f.is_finite()
                || f <= minimum_response
                || !vf.is_finite()
                || vf < 0.0
                || dq_out[pixel] & (dq::SATURATED | dq::NONLINEAR | dq::FLAT_INVALID) != 0
            {
                dq_out[pixel] |= dq::FLAT_INVALID;
                continue;
            }
            let f2 = f * f;
            science[index] = n / f;
            variance[index] = vn / f2 + n * n * vf / (f2 * f2);
        }
    }
    Ok(FlatDivisionResult {
        science,
        variance,
        dq: dq_out,
    })
}

/// Eficiencia asintótica de la mediana respecto a la media (gaussiano):
/// Var(mediana) ≈ (π/2)·σ²/n, luego NEFF = n·2/π.
pub(crate) const MEDIAN_VARIANCE_FACTOR: f64 = std::f64::consts::PI / 2.0;

/// Combinación tiled de un almacén de frames en máster + VAR + NEFF.
///
/// El VALOR replica exactamente la aritmética de `ds_combine_master_store`
/// (mediana por `select_nth_unstable_by` con `total_cmp`; media por suma f64
/// en orden de frame) para que el máster sea bit-idéntico al clásico. La
/// dispersión muestral s² se acumula con Welford en f64 en la misma pasada.
pub(crate) fn combine_master_store_with_variance(
    store: &crate::frame_store::AdaptiveFrameStore,
    frame_count: usize,
    pixel_count: usize,
    use_median: bool,
    cancel: &std::sync::Arc<AtomicBool>,
) -> Result<MasterWithVariance, String> {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_memory();
    let tile_budget = (sys.available_memory().saturating_mul(15) / 100)
        .min(512 * 1024 * 1024)
        .max(1024 * 1024) as usize;
    // Presupuesto por píxel: tiles de frames (mediana) o acumuladores f64
    // (media: sum + welford_mean + m2 = 24 bytes) + salidas f32.
    let bytes_per_pixel = if use_median {
        frame_count.saturating_mul(4).saturating_add(4 + 24)
    } else {
        12 + 24
    };
    let tile_len = (tile_budget / bytes_per_pixel.max(1))
        .clamp(1, pixel_count)
        .min(262_144);

    let mut data = vec![0.0f32; pixel_count];
    let mut variance = vec![f32::NAN; pixel_count];
    let neff_value = if use_median {
        frame_count as f64 / MEDIAN_VARIANCE_FACTOR
    } else {
        frame_count as f64
    } as f32;
    let neff = vec![neff_value; pixel_count];

    let mut start = 0usize;
    while start < pixel_count {
        crate::pipeline::cancellation_checkpoint(
            cancel.as_ref(),
            "combinación tiled de masters con varianza",
        )?;
        let end = (start + tile_len).min(pixel_count);
        let len = end - start;

        if use_median {
            let mut frame_tiles = Vec::with_capacity(frame_count);
            for frame_idx in 0..frame_count {
                crate::pipeline::cancellation_checkpoint(
                    cancel.as_ref(),
                    "lectura tiled de masters con varianza",
                )?;
                frame_tiles.push(store.get_range(frame_idx, start..end)?);
            }
            use rayon::prelude::*;
            let tile: Vec<(f32, f32)> = (0..len)
                .into_par_iter()
                .map_init(
                    || Vec::<f32>::with_capacity(frame_count),
                    |values, pixel| {
                        values.clear();
                        values.extend(frame_tiles.iter().map(|frame| frame[pixel]));
                        // Mediana IDÉNTICA a la ruta clásica.
                        let mid = values.len() / 2;
                        let (_, median, _) =
                            values.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
                        let median = *median;
                        // Dispersión muestral (Welford f64) → Var(mediana).
                        let var = if frame_count >= 2 {
                            let mut mean = 0.0f64;
                            let mut m2 = 0.0f64;
                            for (idx, &v) in values.iter().enumerate() {
                                let v = v as f64;
                                let delta = v - mean;
                                mean += delta / (idx as f64 + 1.0);
                                m2 += delta * (v - mean);
                            }
                            let s2 = m2 / (frame_count as f64 - 1.0);
                            (MEDIAN_VARIANCE_FACTOR * s2 / frame_count as f64) as f32
                        } else {
                            f32::NAN
                        };
                        (median, var)
                    },
                )
                .collect();
            for (offset, (median, var)) in tile.into_iter().enumerate() {
                data[start + offset] = median;
                variance[start + offset] = var;
            }
        } else {
            // MEDIA: suma f64 en orden de frame (bit-idéntica al clásico) +
            // Welford f64 para la dispersión, en la misma pasada por tiles.
            let mut sum = vec![0.0f64; len];
            let mut wmean = vec![0.0f64; len];
            let mut m2 = vec![0.0f64; len];
            for frame_idx in 0..frame_count {
                crate::pipeline::cancellation_checkpoint(
                    cancel.as_ref(),
                    "lectura tiled de masters con varianza",
                )?;
                let frame = store.get_range(frame_idx, start..end)?;
                let count = frame_idx as f64 + 1.0;
                for pixel in 0..len {
                    let v = frame[pixel] as f64;
                    sum[pixel] += v;
                    let delta = v - wmean[pixel];
                    wmean[pixel] += delta / count;
                    m2[pixel] += delta * (v - wmean[pixel]);
                }
            }
            for pixel in 0..len {
                data[start + pixel] = (sum[pixel] / frame_count as f64) as f32;
                variance[start + pixel] = if frame_count >= 2 {
                    let s2 = m2[pixel] / (frame_count as f64 - 1.0);
                    (s2 / frame_count as f64) as f32
                } else {
                    f32::NAN
                };
            }
        }
        start = end;
    }

    Ok(MasterWithVariance {
        data,
        variance,
        neff,
        frames: frame_count,
        use_median,
    })
}

/// Varianza del light SIN calibrar según el modelo de cámara:
/// V_adu = señal_adu/gain + (read_noise_e/gain)², con la señal recortada a
/// cero por debajo (el shot noise no puede ser negativo). `VarianceOrigin::CameraModel`.
pub(crate) fn camera_model_variance(
    signal_adu: &[f32],
    gain_e_per_adu: f32,
    read_noise_e: f32,
    out: &mut Vec<f32>,
) {
    let gain = gain_e_per_adu.max(1e-6) as f64;
    let rn_adu2 = (read_noise_e as f64 / gain).powi(2);
    out.clear();
    out.reserve(signal_adu.len());
    out.extend(
        signal_adu
            .iter()
            .map(|&v| ((v.max(0.0) as f64) / gain + rn_adu2) as f32),
    );
}

/// Varianza empírica CONSTANTE POR CANAL del propio frame: σ² del ruido MRS
/// (à trous B3, paridad PixInsight) medido sobre cada plano de canal. Es la
/// varianza de FONDO — deliberadamente independiente del brillo del objeto,
/// que es exactamente lo que exigen los pesos de NebulaFusion (§2 del plan
/// técnico: pesos jamás dependientes de la señal). `VarianceOrigin::Empirical`.
pub(crate) fn empirical_channel_variance(img: &crate::DsImage) -> Vec<f32> {
    let npx = img.w * img.h;
    // CFA crudo: el suavizado B3 del MRS anula EXACTAMENTE las señales de
    // periodo 2 (1−4+6−4+1 = 0), así que la alternancia de niveles R/G/B del
    // mosaico caería íntegra en la capa de detalle más fina y el MAD la
    // confundiría con ruido (σ inflada en órdenes de magnitud → VAR falsa).
    // Se mide cada subplano Bayer por separado (población homogénea) y se
    // toma la mediana de los cuatro (auditoría 2026-07-20).
    if img.ch == 1 && img.bayer.is_some() && img.w >= 4 && img.h >= 4 {
        let half_w = img.w / 2;
        let half_h = img.h / 2;
        let mut sigmas: Vec<f32> = (0..4usize)
            .map(|sub| {
                let (ox, oy) = (sub % 2, sub / 2);
                let plane: Vec<f32> = (0..half_h)
                    .flat_map(|y| (0..half_w).map(move |x| (2 * y + oy) * img.w + 2 * x + ox))
                    .map(|index| img.data[index])
                    .collect();
                crate::ds_mrs_noise(&plane, half_w, half_h)
            })
            .collect();
        sigmas.sort_by(|a, b| a.total_cmp(b));
        let sigma = 0.5 * (sigmas[1] + sigmas[2]);
        return vec![sigma * sigma];
    }
    (0..img.ch)
        .map(|c| {
            let plane: Vec<f32> = if img.ch == 1 {
                img.data.clone()
            } else {
                (0..npx).map(|p| img.data[p * img.ch + c]).collect()
            };
            let sigma = crate::ds_mrs_noise(&plane, img.w, img.h);
            sigma * sigma
        })
        .collect()
}

/// Entradas de la propagación de varianza de calibración. Todos los planos
/// van en el layout de SU máster (la adaptación de canales mono↔color se
/// resuelve aquí con la misma indexación que `ds_calibrate::sub`).
pub(crate) struct CalibrationVarianceInputs<'a> {
    /// Varianza del light sin calibrar: o un plano por píxel (len == light) o
    /// un valor constante por canal (len == ch, modelo empírico).
    pub v_light: &'a [f32],
    pub v_bias: Option<(&'a [f32], usize)>, // (plano, ch del máster)
    pub v_dark: Option<(&'a [f32], usize)>,
    /// Varianza del máster FLAT NORMALIZADO (post ds_flat_normalize).
    pub v_flat: Option<(&'a [f32], usize)>,
    pub dark_scale: f32,
}

/// Propaga la varianza de calibración sobre el light YA calibrado:
///
///   Var(S) = (V_L + V_B + k²·V_D)/f² + S²·V_f/f²
///
/// (la segunda forma sale de n = S·f, con lo que n²·V_f/f⁴ = S²·V_f/f²).
/// `calibrated` es el frame DESPUÉS de `ds_calibrate`; `flat_norm` el mismo
/// máster flat normalizado que se usó. Esta función conserva compatibilidad
/// con la aritmética clásica y marca el antiguo umbral de división; la ruta
/// científica nueva debe preferir `divide_by_flat_scientific`, que publica
/// NaN + FLAT_INVALID en vez de amplificar un flat débil. DQ siempre tiene layout
/// espacial (un `u32` por píxel), incluso cuando SCI/VAR están intercalados
/// RGB; los flags de cualquiera de los canales se acumulan con OR en el
/// mismo píxel.
///
/// ATENCIÓN (convención dark-neto, para F3): la fórmula trata V_B y V_D como
/// independientes. Si el dark que se pasa al motor es NETO (dark − bias) con
/// k=1, el mismo máster bias se resta al light Y va embebido en el dark neto,
/// de modo que se CANCELA algebraicamente en el numerador — en ese caso NO se
/// debe sumar V_bias dos veces (pásese V_B en un solo término, o el V_dark
/// del máster dark crudo). Con n_cal frames por máster el exceso es
/// ~V_bias/n_cal, pequeño frente al ruido del light, pero el cableado de
/// producción debe hacerlo bien.
pub(crate) fn propagate_calibration_variance(
    calibrated: &crate::DsImage,
    flat_norm: Option<&crate::DsImage>,
    inputs: &CalibrationVarianceInputs,
    dq_out: Option<&mut Vec<u32>>,
) -> Vec<f32> {
    let n = calibrated.data.len();
    let ch = calibrated.ch;
    debug_assert!(ch > 0);
    let npx = calibrated.w.saturating_mul(calibrated.h);
    debug_assert_eq!(n, npx.saturating_mul(ch));
    let v_light_per_channel = inputs.v_light.len() == ch && ch != n;
    // Misma indexación que ds_calibrate::sub — máster mono sobre light color
    // (o viceversa) usa su luma/canal más cercano.
    let master_at = |plane: Option<(&[f32], usize)>, i: usize, ch_i: usize| -> f32 {
        match plane {
            Some((data, mch)) => {
                if mch == ch {
                    data[i]
                } else {
                    data[(i / ch) * mch + ch_i.min(mch - 1)]
                }
            }
            None => 0.0,
        }
    };
    let k2 = (inputs.dark_scale as f64) * (inputs.dark_scale as f64);
    let mut dq = dq_out;
    if let Some(dq) = dq.as_deref_mut() {
        // El contrato es espacial. Completar un buffer nuevo/corto evita
        // perder flags; un buffer mayor se conserva para no borrar flags que
        // pertenezcan a etapas posteriores del caller.
        if dq.len() < npx {
            dq.resize(npx, 0);
        }
        debug_assert!(dq.len() >= npx);
    }
    let mut out = vec![0.0f32; n];
    for i in 0..n {
        let ch_i = i % ch;
        let vl = if v_light_per_channel {
            inputs.v_light[ch_i] as f64
        } else {
            inputs.v_light[i] as f64
        };
        let vb = master_at(inputs.v_bias, i, ch_i) as f64;
        let vd = master_at(inputs.v_dark, i, ch_i) as f64;
        let mut var = vl + vb + k2 * vd;
        if let Some(f) = flat_norm {
            if f.w == calibrated.w && f.h == calibrated.h {
                let fv = if f.ch == ch {
                    f.data[i]
                } else {
                    f.data[(i / ch) * f.ch + ch_i.min(f.ch - 1)]
                };
                let clamped = fv.max(0.05) as f64;
                if fv <= 0.05 {
                    if let Some(dq) = dq.as_deref_mut() {
                        // SCI/VAR son intercalados por canal, DQ es espacial.
                        // Usar `i` aquí escribía fuera del plano DQ para RGB
                        // y, aun con un buffer sobredimensionado, separaba los
                        // flags de los canales de un mismo píxel.
                        if let Some(pixel_dq) = dq.get_mut(i / ch) {
                            *pixel_dq |= dq::FLAT_INVALID;
                        }
                    }
                }
                let f2 = clamped * clamped;
                var /= f2;
                let vf = master_at(inputs.v_flat, i, ch_i) as f64;
                if vf.is_finite() && vf > 0.0 {
                    let s = calibrated.data[i] as f64;
                    var += s * s * vf / f2;
                }
            }
        }
        out[i] = var as f32;
    }
    out
}

/// Escala fotométrica: si el frame se multiplica por g, su varianza se
/// multiplica por g² (V′ = g²·V).
pub(crate) fn apply_photometric_scale_to_variance(variance: &mut [f32], g: f32) {
    let g2 = g * g;
    if (g2 - 1.0).abs() > f32::EPSILON {
        for v in variance.iter_mut() {
            *v *= g2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfa_empirical_variance_measures_subplanes_not_mosaic_alternation() {
        // Mosaico RGGB 64×64 con fondos R=300, G=600, B=200 y ruido gaussiano
        // determinista σ≈15. Medir el mosaico entero confundiría la
        // alternancia (~300 ADU) con ruido; por subplano debe recuperarse un
        // σ del orden del real.
        let (w, h) = (64usize, 64usize);
        let mut seed = 0x2458_9f31_u32;
        let mut noise = || {
            // Box-Muller con LCG determinista.
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let u1 = (seed >> 8) as f32 / (1u32 << 24) as f32;
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let u2 = (seed >> 8) as f32 / (1u32 << 24) as f32;
            (-2.0 * u1.max(1e-7).ln()).sqrt()
                * (2.0 * std::f32::consts::PI * u2).cos()
        };
        let mut data = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let base = match (y % 2, x % 2) {
                    (0, 0) => 300.0,          // R
                    (0, 1) | (1, 0) => 600.0, // G
                    _ => 200.0,               // B
                };
                data[y * w + x] = base + 15.0 * noise();
            }
        }
        let img = crate::DsImage {
            data,
            w,
            h,
            ch: 1,
            bayer: Some(0),
        };
        let variance = empirical_channel_variance(&img);
        assert_eq!(variance.len(), 1);
        let sigma = variance[0].sqrt();
        assert!(
            sigma > 5.0 && sigma < 45.0,
            "σ CFA {sigma} debe reflejar el ruido (~15 ADU), no la alternancia (~300 ADU)"
        );
    }

    #[test]
    fn dq_bits_are_unique_and_native_fallback_is_auditable() {
        let bits = [
            dq::SATURATED,
            dq::NONLINEAR,
            dq::HOT_COLD,
            dq::COSMIC,
            dq::NAN_INPUT,
            dq::FLAT_INVALID,
            dq::NO_COVERAGE,
            dq::INTERPOLATED,
            dq::EDGE,
            dq::EIDR_FALLBACK_NATIVE,
            dq::DEGRADED_CALIBRATION,
            dq::EIDR_UNCERTAINTY_UNAVAILABLE,
        ];
        let mut union = 0u32;
        for bit in bits {
            assert_eq!(bit.count_ones(), 1);
            assert_eq!(union & bit, 0, "bit DQ duplicado: {bit:#x}");
            union |= bit;
        }
        assert_ne!(union & dq::EIDR_FALLBACK_NATIVE, 0);
        assert_ne!(union & dq::EIDR_UNCERTAINTY_UNAVAILABLE, 0);
    }

    fn make_store(frames: &[Vec<f32>]) -> crate::frame_store::AdaptiveFrameStore {
        static STORE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join("zenith_var_tests");
        let tag = format!(
            "var_test_{}_{}",
            std::process::id(),
            STORE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let mut store =
            crate::frame_store::AdaptiveFrameStore::new(frames.len(), frames[0].len(), &dir, &tag)
                .expect("store");
        for (i, f) in frames.iter().enumerate() {
            store.put(i, f).expect("put");
        }
        store
    }

    fn no_cancel() -> std::sync::Arc<AtomicBool> {
        std::sync::Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn test_master_mean_matches_classic_and_variance_is_sample_over_n() {
        // 4 frames (media: n<5) con valores conocidos.
        let frames: Vec<Vec<f32>> = vec![
            vec![10.0, 100.0, -5.0],
            vec![12.0, 102.0, -3.0],
            vec![14.0, 98.0, -7.0],
            vec![16.0, 100.0, -1.0],
        ];
        let store = make_store(&frames);
        let m = combine_master_store_with_variance(&store, 4, 3, false, &no_cancel()).unwrap();
        // Media exacta.
        assert_eq!(m.data, vec![13.0, 100.0, -4.0]);
        // s² muestral del primer píxel: valores 10,12,14,16 → s²=20/3·... :
        // media 13, desvíos -3,-1,1,3 → Σd²=20, s²=20/3, Var(media)=s²/4.
        let expected = (20.0 / 3.0) / 4.0;
        assert!((m.variance[0] - expected as f32).abs() < 1e-5);
        assert_eq!(m.neff[0], 4.0);
        assert!(!m.use_median);
    }

    #[test]
    fn test_master_median_variance_uses_pi_over_2n() {
        // 5 frames (mediana: n>=5).
        let frames: Vec<Vec<f32>> =
            vec![vec![10.0], vec![11.0], vec![12.0], vec![13.0], vec![14.0]];
        let store = make_store(&frames);
        let m = combine_master_store_with_variance(&store, 5, 1, true, &no_cancel()).unwrap();
        assert_eq!(m.data[0], 12.0);
        // s² = 2.5; Var(mediana) ≈ (π/2)·2.5/5.
        let expected = MEDIAN_VARIANCE_FACTOR * 2.5 / 5.0;
        assert!((m.variance[0] as f64 - expected).abs() < 1e-6);
        let neff_expected = 5.0 / MEDIAN_VARIANCE_FACTOR;
        assert!((m.neff[0] as f64 - neff_expected).abs() < 1e-6);
    }

    #[test]
    fn test_single_frame_master_variance_is_nan() {
        let frames: Vec<Vec<f32>> = vec![vec![42.0, 7.0]];
        let store = make_store(&frames);
        let m = combine_master_store_with_variance(&store, 1, 2, false, &no_cancel()).unwrap();
        assert_eq!(m.data, vec![42.0, 7.0]);
        assert!(m.variance.iter().all(|v| v.is_nan()));
    }

    #[test]
    fn test_camera_model_variance_shot_plus_read() {
        let signal = vec![100.0f32, 0.0, -50.0];
        let mut out = Vec::new();
        camera_model_variance(&signal, 2.0, 4.0, &mut out);
        // V = s/gain + (RN/gain)² = 100/2 + 4 = 54; señal negativa → solo RN².
        assert!((out[0] - 54.0).abs() < 1e-5);
        assert!((out[1] - 4.0).abs() < 1e-5);
        assert!((out[2] - 4.0).abs() < 1e-5);
    }

    #[test]
    fn test_propagation_formula_against_manual_computation() {
        // Light mono 2×1 ya calibrado, con flat conocido.
        let calibrated = crate::DsImage {
            data: vec![200.0, 400.0],
            w: 2,
            h: 1,
            ch: 1,
            bayer: None,
        };
        let flat = crate::DsImage {
            data: vec![0.8, 1.0],
            w: 2,
            h: 1,
            ch: 1,
            bayer: None,
        };
        let v_light = vec![25.0f32, 25.0];
        let v_bias = vec![1.0f32, 1.0];
        let v_dark = vec![4.0f32, 4.0];
        let v_flat = vec![0.0001f32, 0.0001];
        let inputs = CalibrationVarianceInputs {
            v_light: &v_light,
            v_bias: Some((&v_bias, 1)),
            v_dark: Some((&v_dark, 1)),
            v_flat: Some((&v_flat, 1)),
            dark_scale: 2.0,
        };
        let var = propagate_calibration_variance(&calibrated, Some(&flat), &inputs, None);
        // Píxel 0: (25+1+4·4)/0.64 + 200²·1e-4/0.64 = 42/0.64 + 4/0.64.
        let expected0 = (25.0 + 1.0 + 4.0 * 4.0) / 0.64 + 200.0f64.powi(2) * 1e-4 / 0.64;
        assert!((var[0] as f64 - expected0).abs() / expected0 < 1e-6);
        // Píxel 1: flat=1 → (42) + 400²·1e-4 = 42 + 16.
        let expected1 = 42.0 + 16.0;
        assert!((var[1] as f64 - expected1).abs() / expected1 < 1e-6);
    }

    #[test]
    fn test_propagation_marks_invalid_flat_in_dq() {
        let calibrated = crate::DsImage {
            data: vec![10.0],
            w: 1,
            h: 1,
            ch: 1,
            bayer: None,
        };
        let flat = crate::DsImage {
            data: vec![0.01],
            w: 1,
            h: 1,
            ch: 1,
            bayer: None,
        };
        let v_light = vec![1.0f32];
        let inputs = CalibrationVarianceInputs {
            v_light: &v_light,
            v_bias: None,
            v_dark: None,
            v_flat: None,
            dark_scale: 1.0,
        };
        let mut dq_plane = vec![0u32; 1];
        let _ =
            propagate_calibration_variance(&calibrated, Some(&flat), &inputs, Some(&mut dq_plane));
        assert_eq!(dq_plane[0] & dq::FLAT_INVALID, dq::FLAT_INVALID);
    }

    #[test]
    fn test_propagation_rgb_ors_channel_flags_into_spatial_dq() {
        let calibrated = crate::DsImage {
            data: vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0],
            w: 2,
            h: 1,
            ch: 3,
            bayer: None,
        };
        // Píxel 0: G y B inválidos. Píxel 1: R inválido. Los tres
        // canales deben terminar en los dos elementos del DQ espacial.
        let flat = crate::DsImage {
            data: vec![1.0, 0.01, 0.02, 0.03, 1.0, 1.0],
            w: 2,
            h: 1,
            ch: 3,
            bayer: None,
        };
        let v_light = vec![1.0f32; 3];
        let inputs = CalibrationVarianceInputs {
            v_light: &v_light,
            v_bias: None,
            v_dark: None,
            v_flat: None,
            dark_scale: 1.0,
        };
        let mut dq_plane = vec![dq::HOT_COLD, dq::COSMIC];

        let variance =
            propagate_calibration_variance(&calibrated, Some(&flat), &inputs, Some(&mut dq_plane));

        assert_eq!(variance.len(), calibrated.data.len());
        assert_eq!(dq_plane.len(), calibrated.w * calibrated.h);
        assert_eq!(dq_plane[0], dq::HOT_COLD | dq::FLAT_INVALID);
        assert_eq!(dq_plane[1], dq::COSMIC | dq::FLAT_INVALID);
    }

    #[test]
    fn test_photometric_scale_squares_variance() {
        let mut v = vec![10.0f32];
        apply_photometric_scale_to_variance(&mut v, 3.0);
        assert!((v[0] - 90.0).abs() < 1e-5);
    }

    // ------------------------------------------------------------------
    // GATE F1: Monte Carlo con el simulador de verdad conocida.
    // ------------------------------------------------------------------

    fn flat_scene(w: usize, h: usize, background: f64) -> crate::deepsky_sim::SimScene {
        crate::deepsky_sim::SimScene {
            width: w,
            height: h,
            background_adu: background,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0, 1.0, 1.0],
            stars: Vec::new(),
        }
    }

    fn test_sensor(read_noise_e: f64) -> crate::deepsky_sim::SimSensor {
        crate::deepsky_sim::SimSensor {
            gain_e_per_adu: 1.0,
            read_noise_e,
            bias_adu: 500.0,
            dark_adu_per_s: 0.0,
            full_well_adu: 65535.0,
            hot_pixels: Vec::new(),
            bayer: None,
            vignette: None,
        }
    }

    fn render_flat_frame(
        scene: &crate::deepsky_sim::SimScene,
        sensor: &crate::deepsky_sim::SimSensor,
        seed: u64,
    ) -> Vec<f32> {
        let exp = crate::deepsky_sim::SimExposure {
            exposure_s: 60.0,
            dx: 0.0,
            dy: 0.0,
            seed,
        };
        crate::deepsky_sim::render_light(scene, sensor, &exp).0
    }

    /// Gate F1 (media): la VAR predicha del máster (s²/n por Welford) debe
    /// coincidir con la varianza empírica del máster sobre 200 realizaciones
    /// dentro de ±5%.
    #[test]
    fn gate_f1_master_mean_variance_within_5pct_of_monte_carlo() {
        let (w, h) = (16, 16);
        let npx = w * h;
        let scene = flat_scene(w, h, 200.0);
        let sensor = test_sensor(3.5);
        let n_frames = 4usize; // n<5 ⇒ media (misma regla que ds_build_master)
        let realizations = 200usize;

        let mut masters: Vec<Vec<f32>> = Vec::with_capacity(realizations);
        let mut predicted_sum = vec![0.0f64; npx];
        for r in 0..realizations {
            let frames: Vec<Vec<f32>> = (0..n_frames)
                .map(|i| render_flat_frame(&scene, &sensor, (r * n_frames + i) as u64 + 1))
                .collect();
            let store = make_store(&frames);
            let m = combine_master_store_with_variance(&store, n_frames, npx, false, &no_cancel())
                .unwrap();
            for p in 0..npx {
                predicted_sum[p] += m.variance[p] as f64;
            }
            masters.push(m.data);
        }
        // Varianza empírica del máster por píxel, promediada sobre píxeles.
        let mut ratio_num = 0.0f64;
        let mut ratio_den = 0.0f64;
        for p in 0..npx {
            let mean = masters.iter().map(|m| m[p] as f64).sum::<f64>() / realizations as f64;
            let emp = masters
                .iter()
                .map(|m| (m[p] as f64 - mean).powi(2))
                .sum::<f64>()
                / (realizations as f64 - 1.0);
            ratio_num += predicted_sum[p] / realizations as f64;
            ratio_den += emp;
        }
        let ratio = ratio_num / ratio_den;
        assert!(
            (ratio - 1.0).abs() < 0.05,
            "VAR predicha/empírica = {ratio:.4} fuera de ±5%"
        );
    }

    /// Gate F1 (mediana): con n=17 el factor asintótico π/2 sobreestima la
    /// varianza real de la mediana en ~3% (correción O(1/n)) — debe quedar
    /// dentro del ±5% del gate.
    #[test]
    fn gate_f1_master_median_variance_within_5pct_of_monte_carlo() {
        let (w, h) = (12, 12);
        let npx = w * h;
        let scene = flat_scene(w, h, 150.0);
        let sensor = test_sensor(4.0);
        let n_frames = 17usize; // n>=5 ⇒ mediana
        let realizations = 250usize;

        let mut masters: Vec<Vec<f32>> = Vec::with_capacity(realizations);
        let mut predicted_sum = vec![0.0f64; npx];
        for r in 0..realizations {
            let frames: Vec<Vec<f32>> = (0..n_frames)
                .map(|i| render_flat_frame(&scene, &sensor, 10_000 + (r * n_frames + i) as u64))
                .collect();
            let store = make_store(&frames);
            let m = combine_master_store_with_variance(&store, n_frames, npx, true, &no_cancel())
                .unwrap();
            for p in 0..npx {
                predicted_sum[p] += m.variance[p] as f64;
            }
            masters.push(m.data);
        }
        let mut ratio_num = 0.0f64;
        let mut ratio_den = 0.0f64;
        for p in 0..npx {
            let mean = masters.iter().map(|m| m[p] as f64).sum::<f64>() / realizations as f64;
            let emp = masters
                .iter()
                .map(|m| (m[p] as f64 - mean).powi(2))
                .sum::<f64>()
                / (realizations as f64 - 1.0);
            ratio_num += predicted_sum[p] / realizations as f64;
            ratio_den += emp;
        }
        let ratio = ratio_num / ratio_den;
        assert!(
            (ratio - 1.0).abs() < 0.05,
            "VAR(mediana) predicha/empírica = {ratio:.4} fuera de ±5%"
        );
    }

    /// Gate F1 (propagación completa): bias/dark/flat + light simulados por
    /// realización; Var(S) propagada vs varianza empírica del frame calibrado.
    /// El modelo de cámara se aplica sobre el light MENOS EL BIAS (el pedestal
    /// es un offset sin shot noise; los fotones de cielo+dark sí son Poisson).
    #[test]
    fn gate_f1_calibration_propagation_within_5pct_of_monte_carlo() {
        let (w, h) = (8, 8);
        let npx = w * h;
        let scene = flat_scene(w, h, 300.0);
        let mut sensor = test_sensor(3.0);
        sensor.dark_adu_per_s = 0.5; // 30 ADU en 60 s
        sensor.vignette = Some(Box::new(|x: f64, y: f64| {
            1.0 - 0.002 * ((x - 4.0).powi(2) + (y - 4.0).powi(2)).sqrt()
        }));
        let n_cal = 6usize; // másters por MEDIANA (n>=5), como en producción
        let realizations = 250usize;
        let flat_level = 30_000.0f64;

        let mut calibrated_frames: Vec<Vec<f32>> = Vec::with_capacity(realizations);
        let mut predicted_sum = vec![0.0f64; npx];
        for r in 0..realizations {
            let base = 1_000_000 + (r as u64) * 1000;
            // Másters de calibración de esta realización.
            let bias_frames: Vec<Vec<f32>> = (0..n_cal)
                .map(|i| crate::deepsky_sim::render_bias(&sensor, w, h, base + i as u64))
                .collect();
            let dark_frames: Vec<Vec<f32>> = (0..n_cal)
                .map(|i| {
                    crate::deepsky_sim::render_dark(&sensor, w, h, 60.0, base + 100 + i as u64)
                })
                .collect();
            let flat_frames: Vec<Vec<f32>> = (0..n_cal)
                .map(|i| {
                    crate::deepsky_sim::render_flat(
                        &sensor,
                        w,
                        h,
                        flat_level,
                        base + 200 + i as u64,
                    )
                })
                .collect();
            let bias_store = make_store(&bias_frames);
            let dark_store = make_store(&dark_frames);
            let flat_store = make_store(&flat_frames);
            let bias_m =
                combine_master_store_with_variance(&bias_store, n_cal, npx, true, &no_cancel())
                    .unwrap();
            let dark_m =
                combine_master_store_with_variance(&dark_store, n_cal, npx, true, &no_cancel())
                    .unwrap();
            let flat_m =
                combine_master_store_with_variance(&flat_store, n_cal, npx, true, &no_cancel())
                    .unwrap();
            // Flat normalizado a media 1 (el dark del flat ≈ bias en este set):
            // f = (flat − bias)/media(flat − bias). Var(f) escala con 1/media².
            let mut fdata: Vec<f32> = flat_m
                .data
                .iter()
                .zip(&bias_m.data)
                .map(|(f, b)| f - b)
                .collect();
            let fmean = (fdata.iter().map(|&v| v as f64).sum::<f64>() / npx as f64).max(1.0) as f32;
            for v in fdata.iter_mut() {
                *v /= fmean;
            }
            let fvar: Vec<f32> = flat_m
                .variance
                .iter()
                .zip(&bias_m.variance)
                .map(|(vf, vb)| (vf + vb) / (fmean * fmean))
                .collect();

            // Light y su calibración con la MISMA aritmética de producción.
            let light_raw = render_flat_frame(&scene, &sensor, base + 300);
            let mut light = crate::DsImage {
                data: light_raw.clone(),
                w,
                h,
                ch: 1,
                bayer: None,
            };
            let bias_img = crate::DsImage {
                data: bias_m.data.clone(),
                w,
                h,
                ch: 1,
                bayer: None,
            };
            let dark_net = crate::DsImage {
                // Dark NETO (dark − bias), k=1: mismo espacio que usa el motor
                // cuando hay bias explícito.
                data: dark_m
                    .data
                    .iter()
                    .zip(&bias_m.data)
                    .map(|(d, b)| d - b)
                    .collect(),
                w,
                h,
                ch: 1,
                bayer: None,
            };
            let flat_img = crate::DsImage {
                data: fdata,
                w,
                h,
                ch: 1,
                bayer: None,
            };
            crate::ds_calibrate(
                &mut light,
                Some(&bias_img),
                Some(&dark_net),
                Some(&flat_img),
                1.0,
            );

            // Var del light crudo: modelo de cámara sobre (raw − bias).
            let signal: Vec<f32> = light_raw
                .iter()
                .zip(&bias_m.data)
                .map(|(l, b)| l - b)
                .collect();
            let mut v_light = Vec::new();
            camera_model_variance(
                &signal,
                sensor.gain_e_per_adu as f32,
                sensor.read_noise_e as f32,
                &mut v_light,
            );
            let dark_net_var: Vec<f32> = dark_m
                .variance
                .iter()
                .zip(&bias_m.variance)
                .map(|(vd, vb)| vd + vb)
                .collect();
            let inputs = CalibrationVarianceInputs {
                v_light: &v_light,
                v_bias: Some((&bias_m.variance, 1)),
                v_dark: Some((&dark_net_var, 1)),
                v_flat: Some((&fvar, 1)),
                dark_scale: 1.0,
            };
            let var = propagate_calibration_variance(&light, Some(&flat_img), &inputs, None);
            for p in 0..npx {
                predicted_sum[p] += var[p] as f64;
            }
            calibrated_frames.push(light.data);
        }
        let mut ratio_num = 0.0f64;
        let mut ratio_den = 0.0f64;
        for p in 0..npx {
            let mean =
                calibrated_frames.iter().map(|f| f[p] as f64).sum::<f64>() / realizations as f64;
            let emp = calibrated_frames
                .iter()
                .map(|f| (f[p] as f64 - mean).powi(2))
                .sum::<f64>()
                / (realizations as f64 - 1.0);
            ratio_num += predicted_sum[p] / realizations as f64;
            ratio_den += emp;
        }
        let ratio = ratio_num / ratio_den;
        assert!(
            (ratio - 1.0).abs() < 0.05,
            "Var(S) propagada/empírica = {ratio:.4} fuera de ±5%"
        );
    }

    #[test]
    fn robust_master_tracks_neff_rejection_and_nonfinite_dq() {
        let frames = vec![
            vec![10.0, 20.0, f32::NAN],
            vec![11.0, 20.0, f32::NAN],
            vec![9.0, f32::NAN, f32::NAN],
            vec![10.0, 20.0, f32::NAN],
            vec![10.0, 20.0, f32::NAN],
            vec![50_000.0, 20.0, f32::NAN],
        ];
        let store = make_store(&frames);
        let master =
            combine_master_store_robust(&store, frames.len(), 3, &no_cancel()).unwrap();
        assert!((master.data[0] - 10.0).abs() < 0.01);
        assert_eq!(master.neff[0], 5.0);
        assert!((master.rejected_fraction[0] - 1.0 / 6.0).abs() < 1.0e-6);
        assert_eq!(master.neff[1], 5.0);
        assert_ne!(master.dq[1] & dq::NAN_INPUT, 0);
        assert!(master.data[2].is_nan());
        assert!(master.variance[2].is_nan());
        assert_eq!(master.neff[2], 0.0);
        assert_ne!(master.dq[2] & dq::NO_COVERAGE, 0);
    }

    #[test]
    fn weak_flat_is_invalid_not_twenty_x_signal() {
        let result = divide_by_flat_scientific(
            &[100.0, 100.0],
            &[4.0, 4.0],
            &[1.0, 0.01],
            &[0.0001, 0.0001],
            2,
            1,
            1,
            1,
            &[0, 0],
            0.05,
        )
        .unwrap();
        assert_eq!(result.science[0], 100.0);
        assert!(result.variance[0] > 4.0);
        assert!(result.science[1].is_nan());
        assert!(result.variance[1].is_nan());
        assert_ne!(result.dq[1] & dq::FLAT_INVALID, 0);
    }

    #[test]
    fn shared_bias_dark_variance_uses_covariance_identity() {
        // Para k=1 el bias compartido se cancela exactamente:
        // L-B-(D-B)=L-D, por lo que Vbias no debe aparecer.
        let shared = calibrated_numerator_variance(
            100.0,
            25.0,
            36.0,
            1.0,
            DarkVarianceConvention::SharedBiasSubtractedThermal,
        )
        .unwrap();
        assert_eq!(shared, 136.0);
        let independent = calibrated_numerator_variance(
            100.0,
            25.0,
            36.0,
            1.0,
            DarkVarianceConvention::IndependentThermalDark,
        )
        .unwrap();
        assert_eq!(independent, 161.0);
        assert!(calibrated_numerator_variance(
            100.0,
            25.0,
            36.0,
            1.2,
            DarkVarianceConvention::RawDarkOnly,
        )
        .is_err());
    }

    #[test]
    fn robust_mean_intervals_cover_68_and_95_percent_within_five_points() {
        let mut seed = 0x5eed_u64;
        let mut uniform = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 11) as f64 + 0.5) / ((1_u64 << 53) as f64)
        };
        let mut within_68 = 0usize;
        let mut within_95 = 0usize;
        let experiments = 4_000usize;
        let mut values = Vec::with_capacity(16);
        let mut scratch = Vec::with_capacity(16);
        for _ in 0..experiments {
            values.clear();
            while values.len() < 16 {
                let u1 = uniform().max(1.0e-12);
                let u2 = uniform();
                let radius = (-2.0 * u1.ln()).sqrt();
                let angle = 2.0 * std::f64::consts::PI * u2;
                values.push((radius * angle.cos()) as f32);
                if values.len() < 16 {
                    values.push((radius * angle.sin()) as f32);
                }
            }
            let stats =
                crate::deepsky_calibration_stats::robust_calibration_stats(&mut values, &mut scratch)
                    .unwrap();
            let standard_error = (stats.variance_of_mean as f64).sqrt();
            if standard_error.is_finite() && standard_error > 0.0 {
                let z = (stats.mean as f64 / standard_error).abs();
                within_68 += usize::from(z <= 1.0);
                within_95 += usize::from(z <= 1.96);
            }
        }
        let coverage_68 = within_68 as f64 / experiments as f64;
        let coverage_95 = within_95 as f64 / experiments as f64;
        assert!((coverage_68 - 0.68).abs() <= 0.05, "68%={coverage_68:.3}");
        assert!((coverage_95 - 0.95).abs() <= 0.05, "95%={coverage_95:.3}");
    }
}
