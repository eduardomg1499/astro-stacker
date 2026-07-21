//! Contenedor radiométrico validado para las etapas científicas de cielo profundo.
//!
//! Mantener SCI, VAR y DQ en vectores independientes permitió publicar mapas con
//! geometría o semántica distintas. `LinearFrame` convierte esas invariantes en
//! un contrato comprobable antes de pasar un resultado a Classic, NebulaFusion,
//! EIDR o al exportador.

#![allow(dead_code)]

use crate::deepsky_psf::MoffatPsf;
use crate::deepsky_variance::{dq, VarianceOrigin};
use crate::pipeline::{CalibrationSignature, PedestalState, StoreLayout};

#[derive(Clone, Debug)]
pub(crate) struct LinearFrameMetadata {
    pub layout: StoreLayout,
    pub pedestal_state: PedestalState,
    pub calibration_signature: CalibrationSignature,
    pub variance_origin: Option<VarianceOrigin>,
    /// Unidades de SCI. VAR deriva siempre de este valor al cuadrado.
    pub science_unit: &'static str,
}

impl Default for LinearFrameMetadata {
    fn default() -> Self {
        Self {
            layout: StoreLayout::Rgb,
            pedestal_state: PedestalState::BiasSubtracted,
            calibration_signature: CalibrationSignature::default(),
            variance_origin: None,
            science_unit: "ADU",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LinearFrame {
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    /// SCI lineal e intercalado por canal. Los huecos permanecen como NaN.
    pub sci: Vec<f32>,
    /// VAR en `science_unit^2`, con el mismo layout intercalado que SCI.
    pub var: Vec<f32>,
    /// DQ espacial: exactamente un u32 por píxel, con OR entre canales.
    pub dq: Vec<u32>,
    pub psf: Option<MoffatPsf>,
    pub metadata: LinearFrameMetadata,
}

impl LinearFrame {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        width: usize,
        height: usize,
        channels: usize,
        sci: Vec<f32>,
        var: Vec<f32>,
        dq: Vec<u32>,
        psf: Option<MoffatPsf>,
        metadata: LinearFrameMetadata,
    ) -> Result<Self, String> {
        let frame = Self {
            width,
            height,
            channels,
            sci,
            var,
            dq,
            psf,
            metadata,
        };
        frame.validate()?;
        Ok(frame)
    }

    pub(crate) fn pixel_count(&self) -> Result<usize, String> {
        self.width
            .checked_mul(self.height)
            .ok_or_else(|| "LinearFrame: geometría desborda usize".to_string())
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.width == 0 || self.height == 0 || !matches!(self.channels, 1 | 3) {
            return Err(format!(
                "LinearFrame: geometría/layout inválido {}x{}x{}",
                self.width, self.height, self.channels
            ));
        }
        match (&self.metadata.layout, self.channels) {
            (StoreLayout::Rgb, 3)
            | (StoreLayout::Mono, 1)
            | (StoreLayout::Cfa { .. }, 1) => {}
            (layout, channels) => {
                return Err(format!(
                    "LinearFrame: layout {layout:?} incompatible con {channels} canal(es)"
                ));
            }
        }
        if self.metadata.science_unit.trim().is_empty() {
            return Err("LinearFrame: science_unit vacío".into());
        }
        let pixels = self.pixel_count()?;
        let samples = pixels
            .checked_mul(self.channels)
            .ok_or_else(|| "LinearFrame: número de muestras desborda usize".to_string())?;
        if self.sci.len() != samples || self.var.len() != samples || self.dq.len() != pixels {
            return Err(format!(
                "LinearFrame: longitudes incompatibles SCI={} VAR={} DQ={}; esperadas {} {} {}",
                self.sci.len(),
                self.var.len(),
                self.dq.len(),
                samples,
                samples,
                pixels
            ));
        }

        for pixel in 0..pixels {
            let flags = self.dq[pixel];
            let no_coverage = flags & dq::NO_COVERAGE != 0;
            // La puerta local EIDR puede publicar SCI nativa/mezclada sin una
            // covarianza defendible. En ese único caso VAR=NaN es deliberado
            // y queda auditado por DQ; no se transforma en una cifra falsa.
            let uncertainty_unavailable = flags & dq::EIDR_UNCERTAINTY_UNAVAILABLE != 0;
            // NAN_INPUT y FLAT_INVALID son ausencias cientificas explicitas:
            // su representacion correcta es NaN, no un cero que parezca dato.
            // NO_COVERAGE tiene una comprobacion mas estricta justo arriba.
            let invalid_sample = flags & (dq::NAN_INPUT | dq::FLAT_INVALID) != 0;
            for channel in 0..self.channels {
                let index = pixel * self.channels + channel;
                let science = self.sci[index];
                let variance = self.var[index];
                if no_coverage {
                    if science.is_finite() || variance.is_finite() {
                        return Err(format!(
                            "LinearFrame: píxel {pixel} NO_COVERAGE contiene SCI/VAR finitos"
                        ));
                    }
                    continue;
                }
                if invalid_sample {
                    if science.is_finite() || variance.is_finite() {
                        return Err(format!(
                            "LinearFrame: píxel {pixel} marcado NAN_INPUT/FLAT_INVALID contiene SCI/VAR finitos"
                        ));
                    }
                    continue;
                }
                if !science.is_finite() {
                    return Err(format!(
                        "LinearFrame: SCI no finito en píxel {pixel} sin una máscara DQ de invalidez"
                    ));
                }
                if variance.is_finite() && variance < 0.0 {
                    return Err(format!(
                        "LinearFrame: VAR negativa en píxel {pixel}, canal {channel}"
                    ));
                }
                if !variance.is_finite() && !uncertainty_unavailable {
                    return Err(format!(
                        "LinearFrame: VAR no finita en píxel {pixel} sin máscara DQ"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Marca un hueco científico. El preview puede rellenarlo en una copia,
    /// pero el contenedor publicable siempre conserva NaN + NO_COVERAGE.
    pub(crate) fn mark_no_coverage(&mut self, pixel: usize) -> Result<(), String> {
        let pixels = self.pixel_count()?;
        if pixel >= pixels {
            return Err(format!(
                "LinearFrame: píxel {pixel} fuera de rango 0..{pixels}"
            ));
        }
        self.dq[pixel] |= dq::NO_COVERAGE;
        for channel in 0..self.channels {
            let index = pixel * self.channels + channel;
            self.sci[index] = f32::NAN;
            self.var[index] = f32::NAN;
        }
        Ok(())
    }

    /// Aplica una normalización fotométrica por canal sin perder el contrato:
    /// SCI'=g*SCI+a y VAR'=g²*VAR. DQ y huecos no cambian.
    pub(crate) fn apply_photometric_affine(
        &mut self,
        gain: [f32; 3],
        offset: [f32; 3],
    ) -> Result<(), String> {
        for channel in 0..self.channels {
            if !gain[channel].is_finite()
                || gain[channel] <= 0.0
                || !offset[channel].is_finite()
            {
                return Err(format!(
                    "LinearFrame: normalización inválida en canal {channel}"
                ));
            }
        }
        let pixels = self.pixel_count()?;
        for pixel in 0..pixels {
            if self.dq[pixel] & (dq::NO_COVERAGE | dq::NAN_INPUT | dq::FLAT_INVALID) != 0 {
                continue;
            }
            for channel in 0..self.channels {
                let index = pixel * self.channels + channel;
                let g = gain[channel];
                if self.sci[index].is_finite() {
                    self.sci[index] = self.sci[index] * g + offset[channel];
                }
                if self.var[index].is_finite() {
                    self.var[index] *= g * g;
                }
            }
        }
        self.validate()
    }

    pub(crate) fn into_components(
        self,
    ) -> (
        Vec<f32>,
        Vec<f32>,
        Vec<u32>,
        Option<MoffatPsf>,
        LinearFrameMetadata,
    ) {
        (self.sci, self.var, self.dq, self.psf, self.metadata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono_metadata() -> LinearFrameMetadata {
        LinearFrameMetadata {
            layout: StoreLayout::Mono,
            ..LinearFrameMetadata::default()
        }
    }

    #[test]
    fn rejects_mismatched_scientific_planes() {
        let error = LinearFrame::new(
            2,
            2,
            1,
            vec![1.0; 4],
            vec![1.0; 3],
            vec![0; 4],
            None,
            mono_metadata(),
        )
        .unwrap_err();
        assert!(error.contains("longitudes incompatibles"));
    }

    #[test]
    fn no_coverage_is_nan_and_flagged() {
        let mut frame = LinearFrame::new(
            2,
            1,
            1,
            vec![10.0, 20.0],
            vec![4.0, 9.0],
            vec![0, 0],
            None,
            mono_metadata(),
        )
        .unwrap();
        frame.mark_no_coverage(1).unwrap();
        assert!(frame.sci[1].is_nan());
        assert!(frame.var[1].is_nan());
        assert_ne!(frame.dq[1] & dq::NO_COVERAGE, 0);
        frame.validate().unwrap();
    }

    #[test]
    fn affine_normalization_propagates_variance_squared() {
        let mut frame = LinearFrame::new(
            1,
            1,
            3,
            vec![10.0, 20.0, 30.0],
            vec![4.0, 9.0, 16.0],
            vec![0],
            None,
            LinearFrameMetadata::default(),
        )
        .unwrap();
        frame
            .apply_photometric_affine([2.0, 0.5, 1.5], [-1.0, 2.0, 0.0])
            .unwrap();
        assert_eq!(frame.sci, vec![19.0, 12.0, 45.0]);
        assert_eq!(frame.var, vec![16.0, 2.25, 36.0]);
    }

    #[test]
    fn rejects_finite_signal_in_no_coverage_pixel() {
        let error = LinearFrame::new(
            1,
            1,
            1,
            vec![0.0],
            vec![0.0],
            vec![dq::NO_COVERAGE],
            None,
            mono_metadata(),
        )
        .unwrap_err();
        assert!(error.contains("NO_COVERAGE"));
    }

    #[test]
    fn rejects_rgb_layout_for_mono_plane() {
        let error = LinearFrame::new(
            1,
            1,
            1,
            vec![0.0],
            vec![1.0],
            vec![0],
            None,
            LinearFrameMetadata::default(),
        )
        .unwrap_err();
        assert!(error.contains("incompatible"));
    }

    #[test]
    fn accepts_nan_science_when_weak_flat_is_explicitly_masked() {
        let frame = LinearFrame::new(
            1,
            1,
            1,
            vec![f32::NAN],
            vec![f32::NAN],
            vec![dq::FLAT_INVALID],
            None,
            mono_metadata(),
        )
        .unwrap();
        assert_eq!(frame.dq[0] & dq::FLAT_INVALID, dq::FLAT_INVALID);
    }

    #[test]
    fn rejects_finite_science_when_weak_flat_is_masked() {
        let error = LinearFrame::new(
            1,
            1,
            1,
            vec![0.0],
            vec![1.0],
            vec![dq::FLAT_INVALID],
            None,
            mono_metadata(),
        )
        .unwrap_err();
        assert!(error.contains("FLAT_INVALID"));
    }

    #[test]
    fn accepts_eidr_signal_with_explicitly_unavailable_uncertainty() {
        let frame = LinearFrame::new(
            1,
            1,
            1,
            vec![42.0],
            vec![f32::NAN],
            vec![dq::EIDR_UNCERTAINTY_UNAVAILABLE],
            None,
            mono_metadata(),
        )
        .unwrap();
        assert!(frame.sci[0].is_finite());
        assert!(frame.var[0].is_nan());
    }

    #[test]
    fn eidr_uncertainty_exception_does_not_hide_unmarked_or_invalid_science() {
        let unmarked = LinearFrame::new(
            1,
            1,
            1,
            vec![42.0],
            vec![f32::NAN],
            vec![0],
            None,
            mono_metadata(),
        )
        .unwrap_err();
        assert!(unmarked.contains("VAR no finita"));

        let invalid_science = LinearFrame::new(
            1,
            1,
            1,
            vec![f32::NAN],
            vec![f32::NAN],
            vec![dq::EIDR_UNCERTAINTY_UNAVAILABLE],
            None,
            mono_metadata(),
        )
        .unwrap_err();
        assert!(invalid_science.contains("SCI no finito"));
    }
}
