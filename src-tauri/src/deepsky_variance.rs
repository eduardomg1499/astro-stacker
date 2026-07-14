//! Contrato lineal de cielo profundo (F1): varianza por píxel, número
//! efectivo de muestras (NEFF) y máscara de calidad DQ.
//!
//! Este módulo es la base de NebulaFusion/EIDR: los másters de calibración
//! dejan de ser solo una media/mediana y pasan a llevar VAR + NEFF, y la
//! calibración propaga Var(S) = (V_L + V_B + k²·V_D)/f² + n²·V_f/f⁴.
//! El motor clásico NO usa nada de aquí: con los productos científicos
//! desactivados el resultado clásico es bit-idéntico (este módulo solo se
//! invoca desde las rutas científicas nuevas).

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
    /// NaN/Inf en el archivo de entrada (sustituido por 0 en la lectura).
    pub const NAN_INPUT: u32 = 1 << 4;
    /// Flat inválido en ese píxel (f <= umbral de división 0.05).
    pub const FLAT_INVALID: u32 = 1 << 5;
    /// Sin cobertura geométrica en la salida (hueco de warp/drizzle).
    pub const NO_COVERAGE: u32 = 1 << 6;
    /// Valor interpolado (no medido): p.ej. debayer o relleno de preview.
    pub const INTERPOLATED: u32 = 1 << 7;
    /// Borde del frame nativo (halo de warp / recorte de registro).
    pub const EDGE: u32 = 1 << 8;
}

/// Procedencia de la varianza — se registra en la receta y en los headers
/// FITS para que cada producto declare de dónde salió su incertidumbre.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VarianceOrigin {
    /// Propagada término a término desde másters con varianza propia.
    Propagated,
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
/// máster flat normalizado que se usó, con el clamp de división `max(0.05)`
/// replicado exactamente. Devuelve el plano Var(S) y marca en `dq_out` los
/// píxeles con flat inválido (bit FLAT_INVALID).
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
                        dq[i] |= dq::FLAT_INVALID;
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
        let frames: Vec<Vec<f32>> = vec![
            vec![10.0],
            vec![11.0],
            vec![12.0],
            vec![13.0],
            vec![14.0],
        ];
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
        let _ = propagate_calibration_variance(&calibrated, Some(&flat), &inputs, Some(&mut dq_plane));
        assert_eq!(dq_plane[0] & dq::FLAT_INVALID, dq::FLAT_INVALID);
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
            let m =
                combine_master_store_with_variance(&store, n_frames, npx, false, &no_cancel())
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
                .map(|i| crate::deepsky_sim::render_dark(&sensor, w, h, 60.0, base + 100 + i as u64))
                .collect();
            let flat_frames: Vec<Vec<f32>> = (0..n_cal)
                .map(|i| {
                    crate::deepsky_sim::render_flat(&sensor, w, h, flat_level, base + 200 + i as u64)
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
            let fmean =
                (fdata.iter().map(|&v| v as f64).sum::<f64>() / npx as f64).max(1.0) as f32;
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
            crate::ds_calibrate(&mut light, Some(&bias_img), Some(&dark_net), Some(&flat_img), 1.0);

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
            let mean = calibrated_frames
                .iter()
                .map(|f| f[p] as f64)
                .sum::<f64>()
                / realizations as f64;
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
}
