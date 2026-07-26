//! Diagnósticos de walking noise y patrones anclados al detector.
//!
//! No intenta "corregir" datos sin soporte. Produce métricas deterministas
//! para que el preflight pueda advertir dithering insuficiente, deriva casi
//! lineal y banding de filas/columnas incluso cuando drizzle está desactivado.

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DitherDiagnostics {
    pub frames: usize,
    pub unique_quarter_pixel_cells: usize,
    pub span_x_px: f64,
    pub span_y_px: f64,
    pub rms_radius_px: f64,
    /// 0 = trayectoria lineal; 1 = diversidad isotrópica.
    pub isotropy: f64,
    /// Correlación absoluta entre tiempo y eje principal de la trayectoria.
    pub temporal_drift_correlation: f64,
    pub walking_noise_risk: bool,
    pub reasons: Vec<String>,
}

fn correlation(x: &[f64], y: &[f64]) -> f64 {
    if x.len() != y.len() || x.len() < 2 {
        return 0.0;
    }
    let mx = x.iter().sum::<f64>() / x.len() as f64;
    let my = y.iter().sum::<f64>() / y.len() as f64;
    let mut xy = 0.0;
    let mut xx = 0.0;
    let mut yy = 0.0;
    for (&a, &b) in x.iter().zip(y) {
        let da = a - mx;
        let db = b - my;
        xy += da * db;
        xx += da * da;
        yy += db * db;
    }
    if xx <= 0.0 || yy <= 0.0 {
        0.0
    } else {
        (xy / (xx * yy).sqrt()).clamp(-1.0, 1.0)
    }
}

/// Analiza posiciones del centro del sensor ya expresadas en coordenadas de
/// referencia y en orden temporal.
pub(crate) fn analyze_dither_positions(positions: &[(f64, f64)]) -> DitherDiagnostics {
    let n = positions.len();
    if n == 0 {
        return DitherDiagnostics {
            walking_noise_risk: true,
            reasons: vec!["sin posiciones de registro".into()],
            ..DitherDiagnostics::default()
        };
    }
    let mx = positions.iter().map(|p| p.0).sum::<f64>() / n as f64;
    let my = positions.iter().map(|p| p.1).sum::<f64>() / n as f64;
    let mut cxx = 0.0;
    let mut cxy = 0.0;
    let mut cyy = 0.0;
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut cells = std::collections::BTreeSet::new();
    for &(x, y) in positions {
        let dx = x - mx;
        let dy = y - my;
        cxx += dx * dx;
        cxy += dx * dy;
        cyy += dy * dy;
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
        cells.insert(((x * 4.0).round() as i64, (y * 4.0).round() as i64));
    }
    cxx /= n as f64;
    cxy /= n as f64;
    cyy /= n as f64;
    let trace = cxx + cyy;
    let disc = ((cxx - cyy) * (cxx - cyy) + 4.0 * cxy * cxy).sqrt();
    let lambda_max = ((trace + disc) * 0.5).max(0.0);
    let lambda_min = ((trace - disc) * 0.5).max(0.0);
    let isotropy = if lambda_max > 1e-12 {
        (lambda_min / lambda_max).sqrt().clamp(0.0, 1.0)
    } else {
        0.0
    };

    // Eje principal de la matriz 2x2. Para la degeneración diagonal elegimos X.
    let angle = if cxy.abs() > 1e-15 || (cxx - cyy).abs() > 1e-15 {
        0.5 * (2.0 * cxy).atan2(cxx - cyy)
    } else {
        0.0
    };
    let (ca, sa) = (angle.cos(), angle.sin());
    let principal: Vec<f64> = positions
        .iter()
        .map(|&(x, y)| (x - mx) * ca + (y - my) * sa)
        .collect();
    let time: Vec<f64> = (0..n).map(|index| index as f64).collect();
    let drift_corr = correlation(&time, &principal).abs();

    let span_x = max_x - min_x;
    let span_y = max_y - min_y;
    let rms_radius = trace.sqrt();
    let mut reasons = Vec::new();
    if n < 6 {
        reasons.push("menos de 6 lights registrados".into());
    }
    if cells.len() < 4 {
        reasons.push("menos de 4 posiciones de dither a 0.25 px".into());
    }
    if span_x.hypot(span_y) < 1.0 {
        reasons.push("recorrido total de dither menor que 1 px".into());
    }
    if n >= 6 && isotropy < 0.2 && drift_corr > 0.9 {
        reasons.push("deriva temporal casi lineal en coordenadas del detector".into());
    }

    DitherDiagnostics {
        frames: n,
        unique_quarter_pixel_cells: cells.len(),
        span_x_px: span_x,
        span_y_px: span_y,
        rms_radius_px: rms_radius,
        isotropy,
        temporal_drift_correlation: drift_corr,
        walking_noise_risk: !reasons.is_empty(),
        reasons,
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DetectorPatternDiagnostics {
    pub row_offset_rms_adu: f64,
    pub column_offset_rms_adu: f64,
    pub row_lag1_correlation: f64,
    pub column_lag1_correlation: f64,
    pub banding_sigma: f64,
    pub banding_detected: bool,
}

fn centered_rms(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    values.iter_mut().for_each(|value| *value -= mean);
    (values.iter().map(|value| value * value).sum::<f64>() / values.len() as f64).sqrt()
}

/// Elimina la componente SUAVE de un perfil (viñeteo, gradiente de cielo) con
/// una media móvil centrada: el banding es estructura de alta frecuencia
/// fila-a-fila; sin este high-pass, un simple viñeteo dispara falsos
/// positivos (RMS enorme con correlación lag-1 ≈ 1).
fn highpass_profile(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    if n < 5 {
        return values.to_vec();
    }
    let half = (n / 16).clamp(4, 64);
    let mut residual = Vec::with_capacity(n);
    for i in 0..n {
        let lo = i.saturating_sub(half);
        let hi = (i + half + 1).min(n);
        let mean = values[lo..hi].iter().sum::<f64>() / (hi - lo) as f64;
        residual.push(values[i] - mean);
    }
    residual
}

/// Doble high-pass + recorte de bordes: una sola pasada deja el sesgo de
/// curvatura de un viñeteo parabólico (residuo ∝ f''·w²) y los extremos del
/// perfil quedan sesgados por la ventana asimétrica. El banding real (alta
/// frecuencia) atraviesa ambas pasadas casi intacto.
fn detrended_profile(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    let half = (n / 16).clamp(4, 64);
    let second = highpass_profile(&highpass_profile(values));
    if n > 4 * half {
        second[half..n - half].to_vec()
    } else {
        second
    }
}

/// Variante para tomas CRUDAS (sin calibrar): elimina la componente suave de
/// los perfiles antes de medir, para que viñeteo y gradiente de cielo no se
/// confundan con banding del detector. El diagnóstico del máster calibrado
/// usa la variante completa (el flat ya eliminó la componente óptica).
pub(crate) fn analyze_detector_pattern_raw(
    data: &[f32],
    width: usize,
    height: usize,
    noise_sigma_adu: f64,
) -> Option<DetectorPatternDiagnostics> {
    let (rows, cols) = profile_means(data, width, height)?;
    let mut rows = detrended_profile(&rows);
    let mut cols = detrended_profile(&cols);
    pattern_from_profiles(&mut rows, &mut cols, noise_sigma_adu)
}

fn profile_means(data: &[f32], width: usize, height: usize) -> Option<(Vec<f64>, Vec<f64>)> {
    if width == 0 || height == 0 || data.len() != width.checked_mul(height)? {
        return None;
    }
    let mut rows = vec![0.0f64; height];
    let mut row_n = vec![0usize; height];
    let mut cols = vec![0.0f64; width];
    let mut col_n = vec![0usize; width];
    for y in 0..height {
        for x in 0..width {
            let value = data[y * width + x] as f64;
            if value.is_finite() {
                rows[y] += value;
                row_n[y] += 1;
                cols[x] += value;
                col_n[x] += 1;
            }
        }
    }
    for (sum, &count) in rows.iter_mut().zip(&row_n) {
        *sum = if count > 0 { *sum / count as f64 } else { 0.0 };
    }
    for (sum, &count) in cols.iter_mut().zip(&col_n) {
        *sum = if count > 0 { *sum / count as f64 } else { 0.0 };
    }
    Some((rows, cols))
}

fn pattern_from_profiles(
    rows: &mut [f64],
    cols: &mut [f64],
    noise_sigma_adu: f64,
) -> Option<DetectorPatternDiagnostics> {
    let row_rms = centered_rms(rows);
    let col_rms = centered_rms(cols);
    let row_lag = if rows.len() > 1 {
        correlation(&rows[..rows.len() - 1], &rows[1..])
    } else {
        0.0
    };
    let col_lag = if cols.len() > 1 {
        correlation(&cols[..cols.len() - 1], &cols[1..])
    } else {
        0.0
    };
    let sigma = noise_sigma_adu.max(1e-12);
    let banding_sigma = row_rms.max(col_rms) / sigma;
    Some(DetectorPatternDiagnostics {
        row_offset_rms_adu: row_rms,
        column_offset_rms_adu: col_rms,
        row_lag1_correlation: row_lag,
        column_lag1_correlation: col_lag,
        banding_sigma,
        banding_detected: banding_sigma >= 0.5
            && (row_lag.abs() >= 0.5 || col_lag.abs() >= 0.5),
    })
}

/// Mide banding aditivo por filas/columnas. `noise_sigma_adu` debe ser la
/// estimación robusta del ruido de fondo del mismo plano.
pub(crate) fn analyze_detector_pattern(
    data: &[f32],
    width: usize,
    height: usize,
    noise_sigma_adu: f64,
) -> Option<DetectorPatternDiagnostics> {
    let (mut rows, mut cols) = profile_means(data, width, height)?;
    pattern_from_profiles(&mut rows, &mut cols, noise_sigma_adu)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_one_axis_drift_is_walking_noise_risk() {
        let positions: Vec<(f64, f64)> = (0..12)
            .map(|index| (index as f64 * 0.35, index as f64 * 0.01))
            .collect();
        let report = analyze_dither_positions(&positions);
        assert!(report.walking_noise_risk);
        assert!(report.temporal_drift_correlation > 0.99);
        assert!(report.isotropy < 0.1);
    }

    #[test]
    fn two_dimensional_dither_passes_diversity_gate() {
        let positions = [
            (0.0, 0.0),
            (1.2, 0.3),
            (-0.8, 1.1),
            (0.4, -1.3),
            (1.5, 1.4),
            (-1.4, -0.6),
            (-0.3, 1.8),
            (1.7, -1.1),
        ];
        let report = analyze_dither_positions(&positions);
        assert!(!report.walking_noise_risk, "{:?}", report.reasons);
        assert!(report.isotropy > 0.5);
    }

    #[test]
    fn raw_variant_ignores_vignetting_but_keeps_banding() {
        let (w, h) = (128usize, 96usize);
        // Viñeteo puro (parabólico, amplitud 400 ADU): NO es banding.
        let mut vignette = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let dx = (x as f32 / w as f32) - 0.5;
                let dy = (y as f32 / h as f32) - 0.5;
                vignette[y * w + x] = 1000.0 - 400.0 * (dx * dx + dy * dy);
            }
        }
        let clean = analyze_detector_pattern_raw(&vignette, w, h, 2.0).unwrap();
        assert!(
            !clean.banding_detected,
            "el viñeteo no debe declararse banding: {clean:?}"
        );
        // Viñeteo + banding real de filas (periodo corto): sí debe detectarse.
        let mut banded = vignette.clone();
        for y in 0..h {
            let offset = 8.0 * (y as f64 * 1.3).sin();
            for x in 0..w {
                banded[y * w + x] += offset as f32;
            }
        }
        let detected = analyze_detector_pattern_raw(&banded, w, h, 2.0).unwrap();
        assert!(detected.banding_detected, "{detected:?}");
    }

    #[test]
    fn row_banding_is_detected() {
        let (w, h) = (64usize, 32usize);
        let mut data = vec![1000.0f32; w * h];
        for y in 0..h {
            let offset = 8.0 * (y as f64 * 0.25).sin();
            for x in 0..w {
                data[y * w + x] += offset as f32;
            }
        }
        let report = analyze_detector_pattern(&data, w, h, 2.0).unwrap();
        assert!(report.banding_detected, "{report:?}");
        assert!(report.row_offset_rms_adu > report.column_offset_rms_adu * 10.0);
    }
}
