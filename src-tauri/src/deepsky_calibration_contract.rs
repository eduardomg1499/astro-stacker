//! Reglas de compatibilidad radiométrica para másters de cielo profundo.
//!
//! Este módulo no selecciona el máster "más cercano": produce una decisión
//! determinista y auditable. Las ausencias y discrepancias sólo pueden seguir
//! con AllowDegraded, y en ese caso el resultado deja de ser elegible para
//! los motores científicos.

#![allow(dead_code)]

use crate::pipeline::{CalibrationSignature, DeepSkyCalibrationPolicy, PedestalState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CalibrationRole {
    Bias,
    Dark,
    Flat,
    DarkFlat,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CalibrationCompatibility {
    pub compatible: bool,
    pub degraded: bool,
    pub scientific_eligible: bool,
    pub reasons: Vec<String>,
}

// La AUSENCIA de un campo en la cabecera NO es una incompatibilidad física:
// es un desconocido. Un mismatch de dos valores PRESENTES sí lo es. Los
// desconocidos se registran en `unverified` y el veredicto decide su peso:
// campos físicos (gain/offset/binning/exposición/temperatura/CFA/filtro) sin
// verificar degradan la elegibilidad científica; la identidad extendida
// (sensor, readMode, roi, adcBits, whiteLevelAdu…) ausente es lo habitual en
// FITS de captura y sólo se anota.
fn required_equal<T: PartialEq + std::fmt::Debug>(
    label: &str,
    reference: &Option<T>,
    candidate: &Option<T>,
    unverified: &mut Vec<String>,
    mismatched: &mut Vec<String>,
) {
    match (reference, candidate) {
        (Some(a), Some(b)) if a == b => {}
        (Some(a), Some(b)) => mismatched.push(format!("{label}: {a:?} != {b:?}")),
        _ => unverified.push(label.to_string()),
    }
}

fn required_float_equal(
    label: &str,
    reference: Option<f32>,
    candidate: Option<f32>,
    abs_tolerance: f32,
    unverified: &mut Vec<String>,
    mismatched: &mut Vec<String>,
) {
    match (reference, candidate) {
        (Some(a), Some(b)) if a.is_finite() && b.is_finite() => {
            if (a - b).abs() > abs_tolerance {
                mismatched.push(format!("{label}: {a:.4} != {b:.4}"));
            }
        }
        _ => unverified.push(label.to_string()),
    }
}

fn required_exposure_equal(
    reference: Option<f64>,
    candidate: Option<f64>,
    unverified: &mut Vec<String>,
    mismatched: &mut Vec<String>,
) {
    match (reference, candidate) {
        (Some(a), Some(b)) if a.is_finite() && b.is_finite() && a >= 0.0 && b >= 0.0 => {
            // Tolerancia de redondeo de cabecera (ms), no el antiguo 10 %.
            let tolerance = 0.001_f64.max(a.abs().max(b.abs()) * 1.0e-6);
            if (a - b).abs() > tolerance {
                mismatched.push(format!("exposureSeconds: {a:.6} != {b:.6}"));
            }
        }
        _ => unverified.push("exposureSeconds".to_string()),
    }
}

/// Compara la firma del light/flat de referencia con un candidato de
/// calibración. Una discrepancia física nunca se oculta; AllowDegraded
/// únicamente permite continuar marcando el resultado como no científico.
pub(crate) fn compare_calibration_signatures(
    reference: &CalibrationSignature,
    candidate: &CalibrationSignature,
    role: CalibrationRole,
    policy: DeepSkyCalibrationPolicy,
) -> CalibrationCompatibility {
    // Física sin verificar (degrada) vs identidad extendida sin verificar
    // (sólo nota) vs mismatch real (incompatible/degradado según política).
    let mut unverified_critical = Vec::new();
    let mut unverified_extended = Vec::new();
    let mut mismatched = Vec::new();

    required_equal(
        "camera",
        &reference.camera,
        &candidate.camera,
        &mut unverified_extended,
        &mut mismatched,
    );
    required_equal(
        "sensor",
        &reference.sensor,
        &candidate.sensor,
        &mut unverified_extended,
        &mut mismatched,
    );
    required_equal(
        "readMode",
        &reference.read_mode,
        &candidate.read_mode,
        &mut unverified_extended,
        &mut mismatched,
    );
    if reference.gain.is_some() || candidate.gain.is_some() {
        required_float_equal(
            "gain",
            reference.gain,
            candidate.gain,
            1.0e-3,
            &mut unverified_critical,
            &mut mismatched,
        );
    } else if reference.iso.is_some() || candidate.iso.is_some() {
        required_equal(
            "iso",
            &reference.iso,
            &candidate.iso,
            &mut unverified_critical,
            &mut mismatched,
        );
    } else {
        unverified_critical.push("gainOrIso".into());
    }
    required_float_equal(
        "offset",
        reference.offset,
        candidate.offset,
        1.0e-3,
        &mut unverified_critical,
        &mut mismatched,
    );
    required_equal(
        "binningX",
        &reference.binning_x,
        &candidate.binning_x,
        &mut unverified_critical,
        &mut mismatched,
    );
    required_equal(
        "binningY",
        &reference.binning_y,
        &candidate.binning_y,
        &mut unverified_critical,
        &mut mismatched,
    );
    required_equal(
        "roi",
        &reference.roi,
        &candidate.roi,
        &mut unverified_extended,
        &mut mismatched,
    );
    required_equal(
        "adcBits",
        &reference.adc_bits,
        &candidate.adc_bits,
        &mut unverified_extended,
        &mut mismatched,
    );
    required_float_equal(
        "whiteLevelAdu",
        reference.white_level_adu,
        candidate.white_level_adu,
        1.0,
        &mut unverified_extended,
        &mut mismatched,
    );

    if reference.cfa_pattern.is_some() || candidate.cfa_pattern.is_some() {
        required_equal(
            "cfaPattern",
            &reference.cfa_pattern,
            &candidate.cfa_pattern,
            &mut unverified_critical,
            &mut mismatched,
        );
        required_equal(
            "cfaPhase",
            &reference.cfa_phase,
            &candidate.cfa_phase,
            &mut unverified_critical,
            &mut mismatched,
        );
    }

    match role {
        CalibrationRole::Bias => {}
        CalibrationRole::Dark => {
            required_exposure_equal(
                reference.exposure_seconds,
                candidate.exposure_seconds,
                &mut unverified_critical,
                &mut mismatched,
            );
            match (reference.temperature_c, candidate.temperature_c) {
                (Some(a), Some(b)) if a.is_finite() && b.is_finite() => {
                    if (a - b).abs() > 1.0 {
                        mismatched.push(format!("temperatureC: {a:.2} vs {b:.2} (>1 C)"));
                    }
                }
                _ => unverified_critical.push("temperatureC".into()),
            }
        }
        CalibrationRole::DarkFlat => {
            required_exposure_equal(
                reference.exposure_seconds,
                candidate.exposure_seconds,
                &mut unverified_critical,
                &mut mismatched,
            );
            required_float_equal(
                "temperatureC",
                reference.temperature_c,
                candidate.temperature_c,
                0.1,
                &mut unverified_critical,
                &mut mismatched,
            );
        }
        CalibrationRole::Flat => {
            required_equal(
                "filter",
                &reference.filter,
                &candidate.filter,
                &mut unverified_critical,
                &mut mismatched,
            );
            required_equal(
                "opticalTrain",
                &reference.optical_train,
                &candidate.optical_train,
                &mut unverified_extended,
                &mut mismatched,
            );
        }
    }

    // 1) Mismatch físico real: se conserva el comportamiento fail-closed.
    if !mismatched.is_empty() {
        let mut reasons = mismatched;
        for list in [&mut unverified_critical, &mut unverified_extended] {
            if !list.is_empty() {
                list.sort();
                list.dedup();
                reasons.push(format!(
                    "sin verificar (cabecera ausente): {}",
                    list.join(", ")
                ));
            }
        }
        return match policy {
            DeepSkyCalibrationPolicy::Strict => CalibrationCompatibility {
                compatible: false,
                degraded: false,
                scientific_eligible: false,
                reasons,
            },
            DeepSkyCalibrationPolicy::AllowDegraded => CalibrationCompatibility {
                compatible: true,
                degraded: true,
                scientific_eligible: false,
                reasons,
            },
        };
    }

    // 2) Física sin verificar: el máster se acepta con divulgación explícita,
    //    pero el resultado deja de ser elegible para los motores científicos.
    if !unverified_critical.is_empty() {
        unverified_critical.sort();
        unverified_critical.dedup();
        let mut reasons = vec![format!(
            "sin verificar (cabecera ausente): {} — el emparejamiento usa los campos disponibles; el resultado no será científico-elegible",
            unverified_critical.join(", ")
        )];
        if !unverified_extended.is_empty() {
            unverified_extended.sort();
            unverified_extended.dedup();
            reasons.push(format!(
                "metadata extendida ausente: {}",
                unverified_extended.join(", ")
            ));
        }
        return CalibrationCompatibility {
            compatible: true,
            degraded: true,
            scientific_eligible: false,
            reasons,
        };
    }

    // 3) Sólo identidad extendida ausente: lo habitual en FITS de captura.
    //    Compatible y científico; queda anotado en la decisión y la receta.
    if !unverified_extended.is_empty() {
        unverified_extended.sort();
        unverified_extended.dedup();
        return CalibrationCompatibility {
            compatible: true,
            degraded: false,
            scientific_eligible: true,
            reasons: vec![format!(
                "metadata extendida ausente (habitual): {}",
                unverified_extended.join(", ")
            )],
        };
    }

    CalibrationCompatibility {
        compatible: true,
        degraded: false,
        scientific_eligible: true,
        reasons: Vec::new(),
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DarkScalingEvidence {
    pub pedestal_state: PedestalState,
    pub amp_glow_detected: bool,
    pub exposure_ratio: f32,
    pub correlation: f32,
    pub linearity_r2: f32,
    pub residual_fraction: f32,
}

/// Dark scaling is opt-in and evidence based. A warning is insufficient for
/// amp glow or an un-subtracted pedestal because both create structured signal.
pub(crate) fn validate_dark_scaling(evidence: DarkScalingEvidence) -> Result<f32, String> {
    if !matches!(evidence.pedestal_state, PedestalState::BiasSubtracted) {
        return Err("dark scaling bloqueado: el máster aún incluye bias".into());
    }
    if evidence.amp_glow_detected {
        return Err("dark scaling bloqueado: se detectó amp glow".into());
    }
    if !evidence.exposure_ratio.is_finite() || !(0.25..=4.0).contains(&evidence.exposure_ratio) {
        return Err("dark scaling bloqueado: razón de exposición inválida".into());
    }
    if !evidence.correlation.is_finite() || evidence.correlation < 0.995 {
        return Err(format!(
            "dark scaling bloqueado: correlación {:.5} < 0.995",
            evidence.correlation
        ));
    }
    if !evidence.linearity_r2.is_finite() || evidence.linearity_r2 < 0.995 {
        return Err(format!(
            "dark scaling bloqueado: linealidad R2 {:.5} < 0.995",
            evidence.linearity_r2
        ));
    }
    if !evidence.residual_fraction.is_finite() || evidence.residual_fraction > 0.01 {
        return Err(format!(
            "dark scaling bloqueado: residuo {:.3}% > 1%",
            evidence.residual_fraction * 100.0
        ));
    }
    Ok(evidence.exposure_ratio)
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FlatPedestalInputs {
    pub raw_dark_flat: bool,
    pub bias: bool,
    pub dark_flat_thermal: bool,
    /// Contribución térmica respecto al nivel del flat (fracción 0..1).
    pub validated_thermal_fraction: Option<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlatCalibrationConvention {
    RawDarkFlat,
    BiasPlusThermalDarkFlat,
    BiasOnlyValidated,
}

/// Elige una sola convención de pedestal; nunca permite restar bias y un
/// dark-flat crudo simultáneamente.
pub(crate) fn validate_flat_pedestal(
    inputs: FlatPedestalInputs,
) -> Result<FlatCalibrationConvention, String> {
    match (inputs.raw_dark_flat, inputs.bias, inputs.dark_flat_thermal) {
        (true, false, false) => Ok(FlatCalibrationConvention::RawDarkFlat),
        (false, true, true) => Ok(FlatCalibrationConvention::BiasPlusThermalDarkFlat),
        (false, true, false) => match inputs.validated_thermal_fraction {
            Some(value) if value.is_finite() && value <= 0.001 => {
                Ok(FlatCalibrationConvention::BiasOnlyValidated)
            }
            Some(value) if value.is_finite() => Err(format!(
                "bias-only bloqueado: térmico {:.4}% > 0.1% del flat",
                value * 100.0
            )),
            _ => {
                Err("bias-only bloqueado: falta validar que el térmico sea <=0.1% del flat".into())
            }
        },
        (true, true, _) => Err(
            "convención de flat inválida: dark-flat crudo y bias restarían el pedestal dos veces"
                .into(),
        ),
        (true, false, true) => Err(
            "convención de flat inválida: no mezcles dark-flat crudo y componente térmica".into(),
        ),
        _ => Err("flat sin calibración de pedestal válida".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signature() -> CalibrationSignature {
        CalibrationSignature {
            camera: Some("ASI2600MM".into()),
            sensor: Some("IMX571".into()),
            read_mode: Some("HighGain".into()),
            gain: Some(100.0),
            offset: Some(50.0),
            temperature_c: Some(-10.0),
            exposure_seconds: Some(300.0),
            binning_x: Some(1),
            binning_y: Some(1),
            roi: Some([0, 0, 6248, 4176]),
            filter: Some("Ha".into()),
            optical_train: Some("train-a".into()),
            adc_bits: Some(16),
            white_level_adu: Some(65535.0),
            ..CalibrationSignature::default()
        }
    }

    #[test]
    fn strict_dark_requires_exact_signature_and_exposure() {
        let reference = signature();
        let mut dark = reference.clone();
        dark.exposure_seconds = Some(270.0);
        let report = compare_calibration_signatures(
            &reference,
            &dark,
            CalibrationRole::Dark,
            DeepSkyCalibrationPolicy::Strict,
        );
        assert!(!report.compatible);
        assert!(!report.scientific_eligible);
        assert!(report
            .reasons
            .iter()
            .any(|reason| reason.contains("exposure")));
    }

    #[test]
    fn allow_degraded_is_visible_and_not_scientific() {
        let reference = signature();
        let mut bias = reference.clone();
        bias.offset = None;
        let report = compare_calibration_signatures(
            &reference,
            &bias,
            CalibrationRole::Bias,
            DeepSkyCalibrationPolicy::AllowDegraded,
        );
        assert!(report.compatible);
        assert!(report.degraded);
        assert!(!report.scientific_eligible);
        assert!(report.reasons[0].contains("offset"));
    }

    #[test]
    fn iso_is_the_exact_gain_identity_for_dslr_signatures() {
        let mut reference = signature();
        reference.gain = None;
        reference.iso = Some(800);
        let mut bias = reference.clone();
        bias.iso = Some(1600);
        let report = compare_calibration_signatures(
            &reference,
            &bias,
            CalibrationRole::Bias,
            DeepSkyCalibrationPolicy::Strict,
        );
        assert!(!report.compatible);
        assert!(report.reasons.iter().any(|reason| reason.contains("iso")));
    }

    #[test]
    fn dark_temperature_tolerance_is_one_degree() {
        let reference = signature();
        let mut dark = reference.clone();
        dark.temperature_c = Some(-9.0);
        assert!(
            compare_calibration_signatures(
                &reference,
                &dark,
                CalibrationRole::Dark,
                DeepSkyCalibrationPolicy::Strict,
            )
            .compatible
        );
        dark.temperature_c = Some(-8.9);
        assert!(
            !compare_calibration_signatures(
                &reference,
                &dark,
                CalibrationRole::Dark,
                DeepSkyCalibrationPolicy::Strict,
            )
            .compatible
        );
    }

    #[test]
    fn dark_scaling_rejects_glow_and_unsubtracted_pedestal() {
        let valid = DarkScalingEvidence {
            pedestal_state: PedestalState::BiasSubtracted,
            amp_glow_detected: false,
            exposure_ratio: 2.0,
            correlation: 0.999,
            linearity_r2: 0.999,
            residual_fraction: 0.004,
        };
        assert_eq!(validate_dark_scaling(valid).unwrap(), 2.0);
        assert!(validate_dark_scaling(DarkScalingEvidence {
            amp_glow_detected: true,
            ..valid
        })
        .unwrap_err()
        .contains("amp glow"));
        assert!(validate_dark_scaling(DarkScalingEvidence {
            pedestal_state: PedestalState::RawIncludesBias,
            ..valid
        })
        .unwrap_err()
        .contains("bias"));
    }

    #[test]
    fn flat_conventions_prevent_double_bias_and_gate_bias_only() {
        assert_eq!(
            validate_flat_pedestal(FlatPedestalInputs {
                raw_dark_flat: true,
                ..FlatPedestalInputs::default()
            })
            .unwrap(),
            FlatCalibrationConvention::RawDarkFlat
        );
        assert!(validate_flat_pedestal(FlatPedestalInputs {
            raw_dark_flat: true,
            bias: true,
            ..FlatPedestalInputs::default()
        })
        .is_err());
        assert_eq!(
            validate_flat_pedestal(FlatPedestalInputs {
                bias: true,
                validated_thermal_fraction: Some(0.0008),
                ..FlatPedestalInputs::default()
            })
            .unwrap(),
            FlatCalibrationConvention::BiasOnlyValidated
        );
        assert!(validate_flat_pedestal(FlatPedestalInputs {
            bias: true,
            validated_thermal_fraction: Some(0.002),
            ..FlatPedestalInputs::default()
        })
        .is_err());
    }
}
