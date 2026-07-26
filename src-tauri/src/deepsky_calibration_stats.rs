//! Estadística robusta para masters de calibración.
//!
//! La mediana elimina outliers, pero paga una penalización de varianza frente
//! a la media cuando el ruido restante es aproximadamente gaussiano. Para
//! bias/dark/flat usamos la mediana y MAD sólo para congelar una máscara de
//! rechazo; el estimador publicado es la media de las muestras aceptadas.

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RobustCalibrationStats {
    pub mean: f32,
    /// Varianza estimada de la media aceptada, no de una toma individual.
    pub variance_of_mean: f32,
    pub finite_samples: usize,
    pub accepted_samples: usize,
    pub rejected_samples: usize,
    pub nonfinite_samples: usize,
}

/// Media robusta determinista con rechazo bilateral a 5 sigma-MAD.
///
/// `deviations` es scratch reutilizable por el caller para evitar una reserva
/// por píxel. NaN/Inf no participan. Con menos de cinco muestras se usa la
/// media finita porque no hay soporte suficiente para estimar MAD.
pub(crate) fn robust_calibration_mean(
    values: &mut Vec<f32>,
    deviations: &mut Vec<f32>,
) -> Option<f32> {
    robust_calibration_stats(values, deviations).map(|stats| stats.mean)
}

pub(crate) fn robust_calibration_stats(
    values: &mut Vec<f32>,
    deviations: &mut Vec<f32>,
) -> Option<RobustCalibrationStats> {
    let input_samples = values.len();
    values.retain(|value| value.is_finite());
    if values.is_empty() {
        return None;
    }
    let finite_samples = values.len();
    let nonfinite_samples = input_samples - finite_samples;
    if values.len() < 5 {
        let mean = values.iter().map(|&v| v as f64).sum::<f64>() / values.len() as f64;
        let variance_of_mean = if values.len() >= 2 {
            let sample_variance = values
                .iter()
                .map(|&value| (value as f64 - mean).powi(2))
                .sum::<f64>()
                / (values.len() - 1) as f64;
            (sample_variance / values.len() as f64) as f32
        } else {
            f32::NAN
        };
        return Some(RobustCalibrationStats {
            mean: mean as f32,
            variance_of_mean,
            finite_samples,
            accepted_samples: finite_samples,
            rejected_samples: 0,
            nonfinite_samples,
        });
    }

    let mid = values.len() / 2;
    let (_, median, _) = values.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
    let median = *median;

    deviations.clear();
    deviations.extend(values.iter().map(|value| (value - median).abs()));
    let dmid = deviations.len() / 2;
    let (_, mad, _) = deviations.select_nth_unstable_by(dmid, |a, b| a.total_cmp(b));
    let sigma = 1.482_602_2f32 * *mad;

    // Cuando MAD=0 (datos cuantizados o masters idénticos), conservar el
    // núcleo exacto y admitir sólo diferencias compatibles con redondeo f32.
    let tolerance = (5.0 * sigma).max(1.0e-6 * median.abs().max(1.0));
    let mut sum = 0.0f64;
    let mut sum_sq = 0.0f64;
    let mut accepted = 0usize;
    for &value in values.iter() {
        if (value - median).abs() <= tolerance {
            let value = value as f64;
            sum += value;
            sum_sq += value * value;
            accepted += 1;
        }
    }
    if accepted < 3 {
        Some(RobustCalibrationStats {
            mean: median,
            variance_of_mean: f32::NAN,
            finite_samples,
            accepted_samples: 1,
            rejected_samples: finite_samples.saturating_sub(1),
            nonfinite_samples,
        })
    } else {
        let mean = sum / accepted as f64;
        let sample_variance =
            ((sum_sq - accepted as f64 * mean * mean) / (accepted - 1) as f64).max(0.0);
        Some(RobustCalibrationStats {
            mean: mean as f32,
            variance_of_mean: (sample_variance / accepted as f64) as f32,
            finite_samples,
            accepted_samples: accepted,
            rejected_samples: finite_samples - accepted,
            nonfinite_samples,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robust_mean_rejects_cosmic_without_median_noise_penalty() {
        let mut values = vec![99.0, 100.0, 101.0, 100.5, 99.5, 50_000.0];
        let mut scratch = Vec::new();
        let result = robust_calibration_mean(&mut values, &mut scratch).unwrap();
        assert!((result - 100.0).abs() < 0.01, "resultado={result}");
    }

    #[test]
    fn robust_mean_excludes_nonfinite_samples() {
        let mut values = vec![10.0, 12.0, f32::NAN, f32::INFINITY];
        let mut scratch = Vec::new();
        assert_eq!(robust_calibration_mean(&mut values, &mut scratch), Some(11.0));
    }

    #[test]
    fn robust_mean_uses_mean_for_small_clean_stack() {
        let mut values = vec![8.0, 10.0, 12.0, 14.0];
        let mut scratch = Vec::new();
        assert_eq!(robust_calibration_mean(&mut values, &mut scratch), Some(11.0));
    }

    #[test]
    fn robust_mean_preserves_quantized_consensus() {
        let mut values = vec![42.0, 42.0, 42.0, 42.0, 42.0, 43.0];
        let mut scratch = Vec::new();
        assert_eq!(robust_calibration_mean(&mut values, &mut scratch), Some(42.0));
    }

    #[test]
    fn robust_stats_publish_variance_neff_and_invalid_count() {
        let mut values = vec![9.0, 10.0, 11.0, 10.0, 10.0, 50_000.0, f32::NAN];
        let mut scratch = Vec::new();
        let stats = robust_calibration_stats(&mut values, &mut scratch).unwrap();
        assert!((stats.mean - 10.0).abs() < 1.0e-6);
        assert_eq!(stats.accepted_samples, 5);
        assert_eq!(stats.rejected_samples, 1);
        assert_eq!(stats.nonfinite_samples, 1);
        assert!((stats.variance_of_mean - 0.1).abs() < 1.0e-6);
    }
}
