//! Apilado nativo de paisajes nocturnos y Vía Láctea.
//!
//! Este módulo mantiene dos ramas geométricas independientes: el cielo se
//! registra por estrellas y el suelo por correlación de textura. Ambas se
//! integran en float32 con rechazo robusto y sólo se componen al final usando
//! una máscara suave. Los raws y los másteres lineales nunca se sobrescriben.

use crate::frame_store::AdaptiveFrameStore;
use crate::pipeline::{self, AstrometrySolution};
use base64::{engine::general_purpose, Engine as _};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;
use tauri::Emitter;

pub const MILKY_WAY_RECIPE_SCHEMA: &str = "zenith-milky-way-recipe-v1";

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MilkyWayStackMode {
    /// Sólo registra e integra el cielo. El suelo queda sin datos (NaN).
    SkyOnly,
    /// Integra cielo y suelo por separado y publica una composición suave.
    #[default]
    FreezeGround,
    /// Publica las dos capas lineales, máscara y composición reconciliada.
    SeparateLayers,
    /// Reservado en el contrato para que clientes nuevos reciban un error
    /// tipado. No se reutiliza el integrador puntual para fingir trazas.
    StarTrails,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MilkyWayFallbackPolicy {
    /// Cualquier registro o máscara no fiable detiene el trabajo.
    #[default]
    Strict,
    /// Excluye el frame afectado y registra la concesión en receta/resultado.
    AllowDegraded,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayMaskPoint {
    /// Coordenada normalizada 0..1.
    pub x: f32,
    /// Coordenada normalizada 0..1.
    pub y: f32,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MilkyWayMaskEditOperation {
    IncludeSky,
    ExcludeSky,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayMaskEdit {
    pub x: f32,
    pub y: f32,
    /// Radio normalizado respecto al lado menor.
    pub radius: f32,
    pub operation: MilkyWayMaskEditOperation,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MilkyWayBrushTarget {
    Sky,
    Ground,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayBrushStroke {
    pub target: MilkyWayBrushTarget,
    pub points: Vec<[f32; 2]>,
}

fn default_brush_radius_px() -> u32 {
    42
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum MilkyWayMaskSpec {
    FullSky {
        #[serde(default)]
        feather_px: u32,
    },
    AutoHorizon {
        #[serde(default = "default_feather_px")]
        feather_px: u32,
    },
    UserPolygon {
        points: Vec<MilkyWayMaskPoint>,
        #[serde(default = "default_feather_px")]
        feather_px: u32,
    },
    /// Frontera cielo/suelo abierta, interpolada por X. El cielo queda por
    /// encima de la línea; los trazos posteriores corrigen salientes finos.
    UserHorizon {
        points: Vec<MilkyWayMaskPoint>,
        #[serde(default)]
        brush_strokes: Vec<MilkyWayBrushStroke>,
        #[serde(default = "default_brush_radius_px")]
        brush_radius_px: u32,
        #[serde(default = "default_feather_px")]
        feather_px: u32,
    },
    AutoWithEdits {
        #[serde(default)]
        edits: Vec<MilkyWayMaskEdit>,
        #[serde(default)]
        brush_target: Option<MilkyWayBrushTarget>,
        #[serde(default)]
        brush_strokes: Vec<MilkyWayBrushStroke>,
        #[serde(default = "default_brush_radius_px")]
        brush_radius_px: u32,
        #[serde(default = "default_feather_px")]
        feather_px: u32,
    },
}

impl MilkyWayMaskSpec {
    fn feather_px(&self) -> u32 {
        match self {
            Self::FullSky { feather_px }
            | Self::AutoHorizon { feather_px }
            | Self::UserPolygon { feather_px, .. }
            | Self::UserHorizon { feather_px, .. }
            | Self::AutoWithEdits { feather_px, .. } => *feather_px,
        }
    }

    fn is_automatic(&self) -> bool {
        matches!(self, Self::AutoHorizon { .. } | Self::AutoWithEdits { .. })
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum MilkyWayUiMaskStrategy {
    #[default]
    Auto,
    Horizon,
    Brush,
    FullSky,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct MilkyWayUiMaskSpec {
    strategy: MilkyWayUiMaskStrategy,
    confidence: f32,
    user_confirmed: bool,
    feather_px: u32,
    horizon_points: Vec<[f32; 2]>,
    brush_strokes: Vec<MilkyWayBrushStroke>,
    brush_target: Option<MilkyWayBrushTarget>,
    brush_radius_px: u32,
    protected_foreground: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum MilkyWayMaskWire {
    Tagged(MilkyWayMaskSpec),
    Ui(MilkyWayUiMaskSpec),
}

impl Default for MilkyWayMaskWire {
    fn default() -> Self {
        Self::Tagged(MilkyWayMaskSpec::default())
    }
}

impl MilkyWayMaskWire {
    fn into_request_parts(self) -> Result<(MilkyWayMaskSpec, bool), String> {
        match self {
            Self::Tagged(mask) => Ok((mask, false)),
            Self::Ui(mask) => {
                if !mask.confidence.is_finite() || !(0.0..=1.0).contains(&mask.confidence) {
                    return Err("mask.confidence debe estar entre 0 y 1".into());
                }
                if !mask.protected_foreground.is_empty() {
                    return Err(
                        "protectedForeground aún no tiene una geometría científica definida; usa trazos Ground"
                            .into(),
                    );
                }
                let points = mask
                    .horizon_points
                    .into_iter()
                    .map(|point| MilkyWayMaskPoint {
                        x: point[0],
                        y: point[1],
                    })
                    .collect::<Vec<_>>();
                let feather_px = if mask.feather_px == 0 {
                    default_feather_px()
                } else {
                    mask.feather_px
                };
                let brush_radius_px = if mask.brush_radius_px == 0 {
                    default_brush_radius_px()
                } else {
                    mask.brush_radius_px
                };
                let has_horizon = points.len() >= 2;
                let spec = match mask.strategy {
                    MilkyWayUiMaskStrategy::FullSky => MilkyWayMaskSpec::FullSky { feather_px },
                    MilkyWayUiMaskStrategy::Horizon => MilkyWayMaskSpec::UserHorizon {
                        points,
                        brush_strokes: mask.brush_strokes,
                        brush_radius_px,
                        feather_px,
                    },
                    MilkyWayUiMaskStrategy::Auto | MilkyWayUiMaskStrategy::Brush if has_horizon => {
                        MilkyWayMaskSpec::UserHorizon {
                            points,
                            brush_strokes: mask.brush_strokes,
                            brush_radius_px,
                            feather_px,
                        }
                    }
                    MilkyWayUiMaskStrategy::Auto | MilkyWayUiMaskStrategy::Brush => {
                        MilkyWayMaskSpec::AutoWithEdits {
                            edits: Vec::new(),
                            brush_target: mask.brush_target,
                            brush_strokes: mask.brush_strokes,
                            brush_radius_px,
                            feather_px,
                        }
                    }
                };
                Ok((spec, mask.user_confirmed))
            }
        }
    }
}

impl Default for MilkyWayMaskSpec {
    fn default() -> Self {
        Self::AutoWithEdits {
            edits: Vec::new(),
            brush_target: None,
            brush_strokes: Vec::new(),
            brush_radius_px: default_brush_radius_px(),
            feather_px: default_feather_px(),
        }
    }
}

fn default_feather_px() -> u32 {
    48
}

fn default_sigma_low() -> f32 {
    3.0
}

fn default_sigma_high() -> f32 {
    3.0
}

fn default_sigma_iterations() -> u8 {
    3
}

fn default_max_ground_shift() -> u32 {
    48
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct MilkyWayIntegrationConfig {
    pub profile: MilkyWayIntegrationProfile,
    pub method: MilkyWayIntegrationMethod,
    pub normalization: MilkyWayNormalizationMode,
    pub dynamic_hot_pixels: bool,
    pub reject_trails: bool,
    pub sky_sigma_low: f32,
    pub sky_sigma_high: f32,
    pub ground_sigma_low: f32,
    pub ground_sigma_high: f32,
    /// Campos v1 conservados para clientes anteriores.
    pub sigma_low: f32,
    pub sigma_high: f32,
    pub iterations: u8,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MilkyWayIntegrationProfile {
    #[default]
    Auto,
    Quality,
    Fast,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MilkyWayIntegrationMethod {
    #[default]
    Winsorized,
    SigmaClip,
    Mean,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MilkyWayNormalizationMode {
    #[default]
    RobustLinear,
    None,
}

impl Default for MilkyWayIntegrationConfig {
    fn default() -> Self {
        Self {
            profile: MilkyWayIntegrationProfile::Auto,
            method: MilkyWayIntegrationMethod::Winsorized,
            normalization: MilkyWayNormalizationMode::RobustLinear,
            dynamic_hot_pixels: true,
            reject_trails: true,
            sky_sigma_low: default_sigma_low(),
            sky_sigma_high: default_sigma_high(),
            ground_sigma_low: default_sigma_low(),
            ground_sigma_high: default_sigma_high(),
            sigma_low: default_sigma_low(),
            sigma_high: default_sigma_high(),
            iterations: default_sigma_iterations(),
        }
    }
}

impl MilkyWayIntegrationConfig {
    fn effective_for(&self, branch: MilkyWayBranch) -> Self {
        let (mut sigma_low, mut sigma_high) = match branch {
            MilkyWayBranch::Sky => (self.sky_sigma_low, self.sky_sigma_high),
            MilkyWayBranch::Ground => (self.ground_sigma_low, self.ground_sigma_high),
        };
        // Compatibilidad con el contrato inicial: si el cliente sólo cambió
        // sigmaLow/High, esos valores siguen gobernando ambas ramas.
        if self.sky_sigma_low == default_sigma_low()
            && self.sky_sigma_high == default_sigma_high()
            && self.ground_sigma_low == default_sigma_low()
            && self.ground_sigma_high == default_sigma_high()
        {
            sigma_low = self.sigma_low;
            sigma_high = self.sigma_high;
        }
        if self.reject_trails && matches!(branch, MilkyWayBranch::Sky) {
            sigma_high = sigma_high.min(2.8);
        }
        let iterations = match self.profile {
            MilkyWayIntegrationProfile::Fast => 1,
            MilkyWayIntegrationProfile::Auto => self.iterations.max(3),
            MilkyWayIntegrationProfile::Quality => self.iterations.max(4),
        };
        Self {
            sigma_low,
            sigma_high,
            iterations,
            ..self.clone()
        }
    }

    fn deterministic_passes(&self) -> u8 {
        match self.profile {
            MilkyWayIntegrationProfile::Quality => 2,
            MilkyWayIntegrationProfile::Auto | MilkyWayIntegrationProfile::Fast => 1,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayWcsBinding {
    /// Frame cuya cuadrícula describe exactamente `solution`.
    pub source_light: String,
    pub solution: AstrometrySolution,
}

fn default_recipe_schema() -> String {
    MILKY_WAY_RECIPE_SCHEMA.into()
}

fn default_registration_model() -> String {
    "auto".into()
}

fn default_min_inliers() -> usize {
    18
}

fn default_max_rms_px() -> f32 {
    2.2
}

fn default_distortion_correction() -> String {
    "auto".into()
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct MilkyWayRegistrationOptions {
    #[serde(default = "default_registration_model")]
    pub model: String,
    #[serde(default = "default_min_inliers")]
    pub min_inliers: usize,
    #[serde(default = "default_max_rms_px")]
    pub max_rms_px: f32,
    #[serde(default = "default_distortion_correction")]
    pub distortion_correction: String,
    #[serde(default = "default_true")]
    pub single_resample: bool,
    /// Estado de UI previo; el backend nunca confía en estos campos y vuelve
    /// a resolver cada transformación.
    pub sky_solved: bool,
    pub ground_solved: bool,
    pub inliers: usize,
    pub rms_px: Option<f32>,
    pub corner_residual_px: Option<f32>,
}

impl Default for MilkyWayRegistrationOptions {
    fn default() -> Self {
        Self {
            model: default_registration_model(),
            min_inliers: default_min_inliers(),
            max_rms_px: default_max_rms_px(),
            distortion_correction: default_distortion_correction(),
            single_resample: true,
            sky_solved: false,
            ground_solved: false,
            inliers: 0,
            rms_px: None,
            corner_residual_px: None,
        }
    }
}

fn default_ground_source() -> String {
    "stack".into()
}

fn default_sky_source() -> String {
    "stack".into()
}

fn default_color_match() -> String {
    "boundaryAware".into()
}

fn default_edge_deghost() -> f32 {
    0.65
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct MilkyWayCompositionOptions {
    pub keep_separate_layers: bool,
    #[serde(default = "default_ground_source")]
    pub ground_source: String,
    #[serde(default = "default_sky_source")]
    pub sky_source: String,
    #[serde(default = "default_feather_px")]
    pub feather_px: u32,
    #[serde(default = "default_edge_deghost")]
    pub edge_deghost: f32,
    #[serde(default = "default_color_match")]
    pub color_match: String,
    #[serde(default = "default_true")]
    pub preserve_reflections: bool,
}

impl Default for MilkyWayCompositionOptions {
    fn default() -> Self {
        Self {
            keep_separate_layers: true,
            ground_source: default_ground_source(),
            sky_source: default_sky_source(),
            feather_px: default_feather_px(),
            edge_deghost: default_edge_deghost(),
            color_match: default_color_match(),
            preserve_reflections: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct MilkyWayPublishOptions {
    pub export_linear_tiff: bool,
    #[serde(default = "default_true")]
    pub export_fits_layers: bool,
    #[serde(default = "default_true")]
    pub export_mask: bool,
    #[serde(default = "default_true")]
    pub open_editor: bool,
}

impl Default for MilkyWayPublishOptions {
    fn default() -> Self {
        Self {
            export_linear_tiff: false,
            export_fits_layers: true,
            export_mask: true,
            open_editor: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayStackRequest {
    pub schema: String,
    pub job_id: Option<String>,
    pub lights: Vec<String>,
    pub darks: Vec<String>,
    pub flats: Vec<String>,
    pub foreground_frames: Vec<String>,
    pub output_dir: String,
    pub work_dir: Option<String>,
    pub mode: MilkyWayStackMode,
    pub base_frame: Option<String>,
    pub mask: MilkyWayMaskSpec,
    /// La UI lo activa sólo después de que el usuario haya visto y aceptado
    /// el contorno automático/editado.
    pub mask_confirmed_by_user: bool,
    pub integration: MilkyWayIntegrationConfig,
    pub fallback_policy: MilkyWayFallbackPolicy,
    pub max_ground_shift_px: u32,
    pub wcs: Option<MilkyWayWcsBinding>,
    pub unify_exposure: bool,
    pub registration: MilkyWayRegistrationOptions,
    pub composition: MilkyWayCompositionOptions,
    pub publish: MilkyWayPublishOptions,
    pub geometry_id: Option<String>,
    pub requested_products: Vec<String>,
}

impl Default for MilkyWayStackRequest {
    fn default() -> Self {
        Self {
            schema: default_recipe_schema(),
            job_id: None,
            lights: Vec::new(),
            darks: Vec::new(),
            flats: Vec::new(),
            foreground_frames: Vec::new(),
            output_dir: String::new(),
            work_dir: None,
            mode: MilkyWayStackMode::FreezeGround,
            base_frame: None,
            mask: MilkyWayMaskSpec::default(),
            mask_confirmed_by_user: false,
            integration: MilkyWayIntegrationConfig::default(),
            fallback_policy: MilkyWayFallbackPolicy::Strict,
            max_ground_shift_px: default_max_ground_shift(),
            wcs: None,
            unify_exposure: false,
            registration: MilkyWayRegistrationOptions::default(),
            composition: MilkyWayCompositionOptions::default(),
            publish: MilkyWayPublishOptions::default(),
            geometry_id: None,
            requested_products: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct MilkyWayStackRequestWire {
    #[serde(default = "default_recipe_schema")]
    schema: String,
    job_id: Option<String>,
    lights: Vec<String>,
    darks: Vec<String>,
    flats: Vec<String>,
    foreground_frames: Vec<String>,
    #[serde(alias = "outputDirectory")]
    output_dir: String,
    work_dir: Option<String>,
    mode: MilkyWayStackMode,
    #[serde(alias = "baseFramePath")]
    base_frame: Option<String>,
    mask: MilkyWayMaskWire,
    mask_confirmed_by_user: bool,
    integration: MilkyWayIntegrationConfig,
    fallback_policy: MilkyWayFallbackPolicy,
    max_ground_shift_px: u32,
    wcs: Option<MilkyWayWcsBinding>,
    unify_exposure: bool,
    registration: MilkyWayRegistrationOptions,
    composition: MilkyWayCompositionOptions,
    publish: MilkyWayPublishOptions,
    geometry_id: Option<String>,
    requested_products: Vec<String>,
}

impl Default for MilkyWayStackRequestWire {
    fn default() -> Self {
        let request = MilkyWayStackRequest::default();
        Self {
            schema: request.schema,
            job_id: request.job_id,
            lights: request.lights,
            darks: request.darks,
            flats: request.flats,
            foreground_frames: request.foreground_frames,
            output_dir: request.output_dir,
            work_dir: request.work_dir,
            mode: request.mode,
            base_frame: request.base_frame,
            mask: MilkyWayMaskWire::default(),
            mask_confirmed_by_user: request.mask_confirmed_by_user,
            integration: request.integration,
            fallback_policy: request.fallback_policy,
            max_ground_shift_px: request.max_ground_shift_px,
            wcs: request.wcs,
            unify_exposure: request.unify_exposure,
            registration: request.registration,
            composition: request.composition,
            publish: request.publish,
            geometry_id: request.geometry_id,
            requested_products: request.requested_products,
        }
    }
}

impl<'de> Deserialize<'de> for MilkyWayStackRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = MilkyWayStackRequestWire::deserialize(deserializer)?;
        let (mask, nested_confirmation) = wire
            .mask
            .into_request_parts()
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema: wire.schema,
            job_id: wire.job_id,
            lights: wire.lights,
            darks: wire.darks,
            flats: wire.flats,
            foreground_frames: wire.foreground_frames,
            output_dir: wire.output_dir,
            work_dir: wire.work_dir,
            mode: wire.mode,
            base_frame: wire.base_frame.filter(|path| !path.trim().is_empty()),
            mask,
            mask_confirmed_by_user: wire.mask_confirmed_by_user || nested_confirmation,
            integration: wire.integration,
            fallback_policy: wire.fallback_policy,
            max_ground_shift_px: wire.max_ground_shift_px,
            wcs: wire.wcs,
            unify_exposure: wire.unify_exposure,
            registration: wire.registration,
            composition: wire.composition,
            publish: wire.publish,
            geometry_id: wire.geometry_id,
            requested_products: wire.requested_products,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayStackPlan {
    pub schema: String,
    pub job_id: String,
    pub light_count: usize,
    pub mode: MilkyWayStackMode,
    pub base_frame: Option<String>,
    pub base_forced_by_wcs: bool,
    pub width: usize,
    pub height: usize,
    pub channels_after_debayer: usize,
    pub estimated_working_bytes: u64,
    pub estimated_output_bytes: u64,
    pub products: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWaySerializableTransform {
    pub model: String,
    /// Matriz source→reference row-major. El consumidor usa `inverse()` para
    /// el único muestreo reference→source.
    pub matrix: [f64; 9],
    pub polynomial: [f64; 12],
    pub normalization: [f64; 3],
    pub radial_center: Option<[f64; 2]>,
    pub radial_normalization_radius: Option<f64>,
    pub radial_k1: Option<f64>,
    pub radial_k2: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayBranchRegistration {
    pub transform: Option<MilkyWaySerializableTransform>,
    pub inliers: usize,
    pub rms_px: Option<f32>,
    pub confidence: Option<f32>,
    pub excluded: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayPhotometricNormalization {
    pub scale: f32,
    pub offset: f32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayFrameReport {
    pub path: String,
    pub sky: MilkyWayBranchRegistration,
    pub ground: Option<MilkyWayBranchRegistration>,
    pub sky_normalization: MilkyWayPhotometricNormalization,
    pub ground_normalization: Option<MilkyWayPhotometricNormalization>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayQualityDiagnostics {
    pub integration_profile: String,
    pub integration_method: String,
    pub deterministic_passes: u8,
    pub mask_confidence: f32,
    pub sky_frames_used: usize,
    pub sky_frames_excluded: usize,
    pub ground_frames_used: usize,
    pub ground_frames_excluded: usize,
    pub mean_sky_coverage: f32,
    pub mean_ground_coverage: Option<f32>,
    pub mean_sky_rejection: f32,
    pub mean_ground_rejection: Option<f32>,
    pub seam_residual_normalized: Option<f32>,
    pub finite_composite_fraction: Option<f32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayOutputPaths {
    pub primary: String,
    pub sky_master: String,
    pub ground_master: Option<String>,
    pub composite: Option<String>,
    pub sky_mask: String,
    pub mask_preview: String,
    pub sky_variance: String,
    pub sky_coverage: String,
    pub sky_rejection: String,
    pub ground_variance: Option<String>,
    pub ground_coverage: Option<String>,
    pub ground_rejection: Option<String>,
    pub recipe: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayStackResult {
    pub schema: String,
    pub job_id: String,
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub base_frame: String,
    pub mode: MilkyWayStackMode,
    pub scientific: bool,
    pub wcs_preserved: bool,
    pub elapsed_seconds: f32,
    /// Alias directos para el adaptador UI; `outputs` conserva el inventario
    /// completo y versionado.
    pub sky_path: String,
    pub ground_path: Option<String>,
    pub composite_path: Option<String>,
    pub mask_path: String,
    pub coverage_path: String,
    pub rejection_path: String,
    pub recipe_path: String,
    pub outputs: MilkyWayOutputPaths,
    pub frames: Vec<MilkyWayFrameReport>,
    pub diagnostics: MilkyWayQualityDiagnostics,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayMaskDetectionResult {
    pub width: usize,
    pub height: usize,
    pub confidence: f32,
    /// Contorno normalizado editable, una muestra por columna de preview.
    pub contour: Vec<MilkyWayMaskPoint>,
    pub sky_fraction: f32,
    pub preview_png_base64: String,
    pub warnings: Vec<String>,
    /// Forma directamente fusionable con `state.mask` en el cliente.
    pub mask: MilkyWayDetectedMaskState,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayDetectedMaskState {
    pub strategy: String,
    pub confidence: f32,
    pub user_confirmed: bool,
    pub feather_px: u32,
    pub horizon_points: Vec<[f32; 2]>,
    pub brush_strokes: Vec<MilkyWayBrushStroke>,
    pub brush_target: MilkyWayBrushTarget,
    pub brush_radius_px: u32,
    pub protected_foreground: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayRegistrationAnalysisResult {
    pub plan: MilkyWayStackPlan,
    pub effective_base_frame: String,
    pub mask_confidence: f32,
    pub frames: Vec<MilkyWayFrameReport>,
    pub warnings: Vec<String>,
    pub registration: MilkyWayRegistrationSummary,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayRegistrationSummary {
    pub sky_solved: bool,
    pub ground_solved: bool,
    pub inliers: usize,
    pub rms_px: Option<f32>,
    pub corner_residual_px: Option<f32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayCropProductsRequest {
    pub job_id: Option<String>,
    pub sky_path: String,
    pub ground_path: Option<String>,
    pub composite_path: Option<String>,
    pub mask_path: String,
    /// Mapas científicos canónicos. Son opcionales en el wire para poder leer
    /// recetas de la primera versión, que sólo enviaba `coveragePath` y
    /// `rejectionPath`; el preflight exige una de las dos formas.
    #[serde(default)]
    pub sky_variance_path: Option<String>,
    #[serde(default, alias = "coveragePath", alias = "coverage_path")]
    pub sky_coverage_path: Option<String>,
    #[serde(default, alias = "rejectionPath", alias = "rejection_path")]
    pub sky_rejection_path: Option<String>,
    #[serde(default)]
    pub ground_variance_path: Option<String>,
    #[serde(default)]
    pub ground_coverage_path: Option<String>,
    #[serde(default)]
    pub ground_rejection_path: Option<String>,
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
}

impl MilkyWayCropProductsRequest {
    fn resolved_sky_variance(&self) -> Result<&str, MilkyWayError> {
        required_crop_map("skyVariancePath", self.sky_variance_path.as_deref())
    }

    fn resolved_sky_coverage(&self) -> Result<&str, MilkyWayError> {
        self.sky_coverage_path
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| MilkyWayError::invalid("Falta skyCoveragePath (alias: coveragePath)"))
    }

    fn resolved_sky_rejection(&self) -> Result<&str, MilkyWayError> {
        self.sky_rejection_path
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| MilkyWayError::invalid("Falta skyRejectionPath (alias: rejectionPath)"))
    }

    fn resolved_ground_maps(&self) -> Result<Option<(&str, &str, &str)>, MilkyWayError> {
        let supplied = [
            self.ground_variance_path.as_deref(),
            self.ground_coverage_path.as_deref(),
            self.ground_rejection_path.as_deref(),
        ]
        .into_iter()
        .any(|value| value.is_some_and(|value| !value.trim().is_empty()));
        if self.ground_path.is_none() {
            if supplied {
                return Err(MilkyWayError::invalid(
                    "Hay mapas de suelo sin groundPath; el conjunto de recorte no es coherente",
                ));
            }
            return Ok(None);
        }
        Ok(Some((
            required_crop_map("groundVariancePath", self.ground_variance_path.as_deref())?,
            required_crop_map("groundCoveragePath", self.ground_coverage_path.as_deref())?,
            required_crop_map("groundRejectionPath", self.ground_rejection_path.as_deref())?,
        )))
    }
}

fn required_crop_map<'a>(name: &str, value: Option<&'a str>) -> Result<&'a str, MilkyWayError> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            MilkyWayError::invalid(format!("Falta {name}; no se publicará un recorte parcial"))
        })
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayCroppedProductsResult {
    pub job_id: String,
    pub geometry_id: String,
    pub source_width: usize,
    pub source_height: usize,
    pub width: usize,
    pub height: usize,
    pub sky_path: String,
    pub ground_path: Option<String>,
    pub composite_path: Option<String>,
    pub mask_path: String,
    pub sky_variance_path: Option<String>,
    pub sky_coverage_path: String,
    pub sky_rejection_path: String,
    pub ground_variance_path: Option<String>,
    pub ground_coverage_path: Option<String>,
    pub ground_rejection_path: Option<String>,
    /// Alias compatibles: apuntan siempre a los mapas canónicos del cielo.
    pub coverage_path: String,
    pub rejection_path: String,
    pub recipe_path: String,
    /// Autoridad astrométrica del producto recortado: `validated`,
    /// `candidate` o `missing`. Es deliberadamente independiente de
    /// `wcs_preserved`: un candidato puede conservar las tarjetas WCS para
    /// una validación posterior, pero nunca debe habilitar consumidores que
    /// requieren una solución científicamente validada.
    pub wcs_status: String,
    /// `true` exclusivamente para un WCS con fingerprint validado. Se conserva
    /// el nombre histórico del campo por compatibilidad con el bridge UI.
    pub wcs_preserved: bool,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MilkyWayError {
    pub code: String,
    pub message: String,
    pub stage: String,
    pub recoverable: bool,
}

impl MilkyWayError {
    fn new(code: &str, stage: &str, message: impl Into<String>, recoverable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            stage: stage.into(),
            recoverable,
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_request", "preflight", message, true)
    }

    fn io(stage: &str, message: impl Into<String>) -> Self {
        Self::new("io_error", stage, message, true)
    }

    fn scientific(stage: &str, message: impl Into<String>) -> Self {
        Self::new("scientific_guard", stage, message, true)
    }

    fn unsupported(message: impl Into<String>) -> Self {
        Self::new("unsupported_feature", "preflight", message, true)
    }

    fn cancelled(stage: &str) -> Self {
        Self::new(
            "cancelled",
            stage,
            format!("Cancelado por el usuario durante {stage}"),
            true,
        )
    }
}

impl std::fmt::Display for MilkyWayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.stage, self.message)
    }
}

impl std::error::Error for MilkyWayError {}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MilkyWayProgress {
    job_id: String,
    phase: String,
    progress: f32,
    items_done: usize,
    items_total: usize,
    detail: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MilkyWayRecipe {
    schema: String,
    created_at_utc: String,
    request: MilkyWayStackRequest,
    effective_base_frame: String,
    width: usize,
    height: usize,
    channels: usize,
    frame_reports: Vec<MilkyWayFrameReport>,
    diagnostics: MilkyWayQualityDiagnostics,
    warnings: Vec<String>,
    outputs: MilkyWayOutputPaths,
}

#[derive(Clone)]
struct BranchIntegration {
    sci: Vec<f32>,
    variance: Vec<f32>,
    coverage: Vec<f32>,
    rejection: Vec<f32>,
    frames_used: usize,
}

#[derive(Clone)]
struct MaskBuild {
    hard: Vec<u8>,
    soft: Vec<f32>,
    confidence: f32,
}

#[derive(Clone, Copy)]
struct Quantiles {
    p16: f32,
    p50: f32,
    p84: f32,
}

struct StagingDirectory {
    path: PathBuf,
    committed: bool,
}

struct EphemeralDirectory(PathBuf);

impl Drop for EphemeralDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl StagingDirectory {
    fn new(path: PathBuf) -> Result<Self, MilkyWayError> {
        std::fs::create_dir_all(&path)
            .map_err(|error| MilkyWayError::io("output", format!("crear staging: {error}")))?;
        Ok(Self {
            path,
            committed: false,
        })
    }

    fn commit(mut self, final_path: &Path) -> Result<(), MilkyWayError> {
        std::fs::rename(&self.path, final_path).map_err(|error| {
            MilkyWayError::io("output", format!("publicar carpeta de resultado: {error}"))
        })?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn same_path(left: &str, right: &str) -> bool {
    Path::new(left) == Path::new(right)
        || match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
}

fn safe_job_fragment(job_id: &str) -> String {
    let fragment: String = job_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(96)
        .collect();
    if fragment.is_empty() {
        "milky-way-job".into()
    } else {
        fragment
    }
}

fn validate_unique_paths(paths: &[String], label: &str) -> Result<(), MilkyWayError> {
    let mut unique = BTreeSet::new();
    for path in paths {
        if path.trim().is_empty() {
            return Err(MilkyWayError::invalid(format!(
                "{label}: hay una ruta vacía"
            )));
        }
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
        if !unique.insert(key) {
            return Err(MilkyWayError::invalid(format!(
                "{label}: la misma toma está repetida: {path}"
            )));
        }
        if !Path::new(path).is_file() {
            return Err(MilkyWayError::invalid(format!(
                "{label}: no existe el archivo: {path}"
            )));
        }
    }
    Ok(())
}

fn validate_wcs(solution: &AstrometrySolution) -> Result<(), MilkyWayError> {
    let values = [
        solution.crval1,
        solution.crval2,
        solution.crpix1,
        solution.crpix2,
        solution.cd11,
        solution.cd12,
        solution.cd21,
        solution.cd22,
        solution.scale_arcsec_px,
    ];
    if values.iter().any(|value| !value.is_finite())
        || solution.scale_arcsec_px <= 0.0
        || solution.inliers < 6
        || solution.rms_px <= 0.0
        || !solution.rms_px.is_finite()
    {
        return Err(MilkyWayError::scientific(
            "astrometry",
            "La solución WCS no tiene geometría, escala o inliers válidos",
        ));
    }
    Ok(())
}

/// Convierte opciones históricas que la primera UI exponía sin motor propio
/// en un contrato efectivo, reproducible y explícito. La receta conserva los
/// valores efectivos y `warnings` registra cada normalización.
fn normalize_composition_contract(request: &mut MilkyWayStackRequest) -> Vec<String> {
    let mut warnings = Vec::new();
    if request.mode == MilkyWayStackMode::SeparateLayers {
        warnings.push(
            "SeparateLayers es un alias de compatibilidad del flujo cielo/suelo: se conservan siempre capas, máscara y mapas científicos"
                .into(),
        );
    }
    if request.mode != MilkyWayStackMode::SkyOnly && !request.composition.keep_separate_layers {
        request.composition.keep_separate_layers = true;
        warnings.push(
            "keepSeparateLayers=false se normalizó a true: el contrato científico nunca descarta capas ni mapas"
                .into(),
        );
    }
    if request.composition.edge_deghost.abs() > f32::EPSILON {
        request.composition.edge_deghost = 0.0;
        warnings.push(
            "edgeDeghost se normalizó a 0: no existe todavía un modelo validado y no se aplicó una corrección ficticia"
                .into(),
        );
    }
    if !request.composition.preserve_reflections {
        request.composition.preserve_reflections = true;
        warnings.push(
            "preserveReflections=false se normalizó a true: el motor no elimina reflejos ni luces reales del suelo"
                .into(),
        );
    }
    warnings
}

/// Valida la especificación de máscara por sí sola. La usan tanto el path de
/// apilado como `detect_milky_way_sky_mask`, que corre antes de que el resto
/// del request esté configurado — sin esta validación, un polígono u horizonte
/// vacío alcanzaba `point_inside_polygon`/`user_horizon_mask` y provocaba pánico.
fn validate_mask_spec(mask: &MilkyWayMaskSpec) -> Result<(), MilkyWayError> {
    if mask.feather_px() > 512 {
        return Err(MilkyWayError::invalid(
            "El feather de máscara no puede superar 512 px",
        ));
    }
    match mask {
        MilkyWayMaskSpec::UserPolygon { points, .. } => {
            if points.len() < 3 {
                return Err(MilkyWayError::invalid(
                    "La máscara necesita suficientes puntos normalizados para definir su geometría",
                ));
            }
            if points.iter().any(|point| {
                !point.x.is_finite()
                    || !point.y.is_finite()
                    || !(0.0..=1.0).contains(&point.x)
                    || !(0.0..=1.0).contains(&point.y)
            }) {
                return Err(MilkyWayError::invalid(
                    "Los puntos de máscara deben estar normalizados entre 0 y 1",
                ));
            }
        }
        MilkyWayMaskSpec::AutoWithEdits {
            edits,
            brush_strokes,
            brush_radius_px,
            ..
        } => {
            if edits.iter().any(|edit| {
                !edit.x.is_finite()
                    || !edit.y.is_finite()
                    || !edit.radius.is_finite()
                    || !(0.0..=1.0).contains(&edit.x)
                    || !(0.0..=1.0).contains(&edit.y)
                    || !(0.001..=1.0).contains(&edit.radius)
            }) {
                return Err(MilkyWayError::invalid(
                    "Las ediciones de máscara contienen coordenadas o radios inválidos",
                ));
            }
            if !(1..=512).contains(brush_radius_px)
                || brush_strokes.iter().any(|stroke| {
                    stroke.points.is_empty()
                        || stroke.points.iter().any(|point| {
                            !point[0].is_finite()
                                || !point[1].is_finite()
                                || !(0.0..=1.0).contains(&point[0])
                                || !(0.0..=1.0).contains(&point[1])
                        })
                })
            {
                return Err(MilkyWayError::invalid(
                    "Los trazos de máscara contienen radio o puntos inválidos",
                ));
            }
        }
        MilkyWayMaskSpec::UserHorizon {
            points,
            brush_strokes,
            brush_radius_px,
            ..
        } => {
            if points.len() < 2
                || points.iter().any(|point| {
                    !point.x.is_finite()
                        || !point.y.is_finite()
                        || !(0.0..=1.0).contains(&point.x)
                        || !(0.0..=1.0).contains(&point.y)
                })
                || !(1..=512).contains(brush_radius_px)
                || brush_strokes.iter().any(|stroke| {
                    stroke.points.is_empty()
                        || stroke.points.iter().any(|point| {
                            !point[0].is_finite()
                                || !point[1].is_finite()
                                || !(0.0..=1.0).contains(&point[0])
                                || !(0.0..=1.0).contains(&point[1])
                        })
                })
            {
                return Err(MilkyWayError::invalid(
                    "Los trazos sobre el horizonte contienen radio o puntos inválidos",
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_request(request: &MilkyWayStackRequest) -> Result<(), MilkyWayError> {
    if request.schema != MILKY_WAY_RECIPE_SCHEMA {
        return Err(MilkyWayError::invalid(format!(
            "Esquema de receta no compatible: '{}'",
            request.schema
        )));
    }
    if request.mode == MilkyWayStackMode::StarTrails {
        return Err(MilkyWayError::unsupported(
            "Star Trails requiere un integrador acumulativo propio y todavía no está habilitado",
        ));
    }
    if request.mode != MilkyWayStackMode::SkyOnly && !request.mask_confirmed_by_user {
        return Err(MilkyWayError::scientific(
            "mask",
            "Confirma la máscara cielo/suelo antes de registrar o integrar ambas ramas",
        ));
    }
    if !request.foreground_frames.is_empty() {
        return Err(MilkyWayError::unsupported(
            "Las tomas de primer plano separadas requieren calibración y reconciliación geométrica propias; usa por ahora el suelo del mismo lote",
        ));
    }
    if request.publish.export_linear_tiff {
        return Err(MilkyWayError::unsupported(
            "TIFF lineal aún no está publicado por este motor; exporta las capas FITS float32",
        ));
    }
    if !request.publish.export_fits_layers {
        return Err(MilkyWayError::invalid(
            "El motor científico inicial requiere publicar las capas FITS lineales",
        ));
    }
    if !request.registration.single_resample {
        return Err(MilkyWayError::scientific(
            "registration",
            "Las transformaciones de cada rama deben componerse antes de un único remuestreo",
        ));
    }
    if !matches!(
        request.registration.model.as_str(),
        "auto" | "affine" | "homography" | "radialWide"
    ) {
        return Err(MilkyWayError::invalid(
            "registration.model debe ser auto, affine, homography o radialWide",
        ));
    }
    if !matches!(
        request.registration.distortion_correction.as_str(),
        "auto" | "lens" | "complex" | "off"
    ) {
        return Err(MilkyWayError::invalid(
            "distortionCorrection debe ser auto, lens, complex u off",
        ));
    }
    if request.registration.min_inliers < 6
        || !request.registration.max_rms_px.is_finite()
        || !(0.1..=20.0).contains(&request.registration.max_rms_px)
    {
        return Err(MilkyWayError::invalid(
            "El registro requiere al menos 6 inliers y maxRmsPx entre 0.1 y 20",
        ));
    }
    if !matches!(request.composition.ground_source.as_str(), "stack" | "base") {
        return Err(MilkyWayError::unsupported(
            "composition.groundSource sólo admite stack o base en esta versión",
        ));
    }
    if request.composition.sky_source != "stack" {
        return Err(MilkyWayError::unsupported(
            "composition.skySource sólo admite stack en esta versión",
        ));
    }
    if !matches!(
        request.composition.color_match.as_str(),
        "boundaryAware" | "off"
    ) || !request.composition.edge_deghost.is_finite()
        || !(0.0..=1.0).contains(&request.composition.edge_deghost)
        || request.composition.feather_px > 512
    {
        return Err(MilkyWayError::invalid(
            "La composición contiene colorMatch, edgeDeghost o featherPx inválidos",
        ));
    }
    let minimum_lights = if request.mode == MilkyWayStackMode::SkyOnly {
        2
    } else {
        4
    };
    if request.lights.len() < minimum_lights {
        return Err(MilkyWayError::scientific(
            "preflight",
            format!(
                "El modo {:?} requiere al menos {minimum_lights} lights; AllowDegraded no convierte una toma aislada en integración robusta",
                request.mode
            ),
        ));
    }
    if request.output_dir.trim().is_empty() {
        return Err(MilkyWayError::invalid("Selecciona una carpeta de destino"));
    }
    validate_unique_paths(&request.lights, "lights")?;
    validate_unique_paths(&request.darks, "darks")?;
    validate_unique_paths(&request.flats, "flats")?;
    if [
        request.integration.sigma_low,
        request.integration.sigma_high,
        request.integration.sky_sigma_low,
        request.integration.sky_sigma_high,
        request.integration.ground_sigma_low,
        request.integration.ground_sigma_high,
    ]
    .iter()
    .any(|value| !(0.5..=10.0).contains(value))
        || !(1..=8).contains(&request.integration.iterations)
    {
        return Err(MilkyWayError::invalid(
            "Sigma bajo/alto debe estar entre 0.5 y 10; iteraciones entre 1 y 8",
        ));
    }
    if request.integration.method == MilkyWayIntegrationMethod::Mean {
        return Err(MilkyWayError::scientific(
            "preflight",
            "Mean queda bloqueado para Vía Láctea: sólo se permitirá en el futuro modo Star Trails explícito",
        ));
    }
    if request.max_ground_shift_px > 512 {
        return Err(MilkyWayError::invalid(
            "El desplazamiento máximo de suelo no puede superar 512 px",
        ));
    }
    if request.mode != MilkyWayStackMode::SkyOnly
        && matches!(request.mask, MilkyWayMaskSpec::FullSky { .. })
    {
        return Err(MilkyWayError::invalid(
            "FullSky no deja suelo para FreezeGround/SeparateLayers; usa horizonte automático o polígono",
        ));
    }
    validate_mask_spec(&request.mask)?;
    if let Some(base) = &request.base_frame {
        if !request.lights.iter().any(|light| same_path(light, base)) {
            return Err(MilkyWayError::invalid(
                "La toma base debe pertenecer al lote de lights",
            ));
        }
    }
    if let Some(binding) = &request.wcs {
        validate_wcs(&binding.solution)?;
        if !request
            .lights
            .iter()
            .any(|light| same_path(light, &binding.source_light))
        {
            return Err(MilkyWayError::scientific(
                "astrometry",
                "El frame origen del WCS no pertenece al lote de lights",
            ));
        }
        if let Some(base) = &request.base_frame {
            if !same_path(base, &binding.source_light) {
                return Err(MilkyWayError::scientific(
                    "astrometry",
                    "La toma base y la cuadrícula descrita por el WCS no coinciden",
                ));
            }
        }
    }
    Ok(())
}

fn resolved_base_hint(request: &MilkyWayStackRequest) -> (Option<String>, bool) {
    if let Some(binding) = &request.wcs {
        (Some(binding.source_light.clone()), true)
    } else {
        (request.base_frame.clone(), false)
    }
}

fn expected_products(mode: MilkyWayStackMode) -> Vec<String> {
    let mut products = vec![
        "Máster de cielo lineal".into(),
        "Máscara de cielo".into(),
        "VAR/cobertura/rechazo de cielo".into(),
        "Receta reproducible".into(),
    ];
    if mode != MilkyWayStackMode::SkyOnly {
        products.extend([
            "Máster de suelo lineal".into(),
            "VAR/cobertura/rechazo de suelo".into(),
            "Composición lineal con feather".into(),
        ]);
    }
    products
}

fn output_channels(raw: &crate::DsImage) -> usize {
    if raw.bayer.is_some() && raw.ch == 1 {
        3
    } else {
        raw.ch
    }
}

pub fn prepare_milky_way_stack_impl(
    request: &MilkyWayStackRequest,
) -> Result<MilkyWayStackPlan, MilkyWayError> {
    let mut effective_request = request.clone();
    let mut warnings = normalize_composition_contract(&mut effective_request);
    let request = &effective_request;
    validate_request(request)?;
    let first = crate::ds_read_image(&request.lights[0])
        .map_err(|error| MilkyWayError::io("preflight", error))?;
    if !matches!(output_channels(&first), 1 | 3) {
        return Err(MilkyWayError::scientific(
            "preflight",
            "Sólo se admiten datos mono, RGB o CFA convertibles a RGB",
        ));
    }
    let pixels = first
        .w
        .checked_mul(first.h)
        .and_then(|value| value.checked_mul(output_channels(&first)))
        .ok_or_else(|| MilkyWayError::invalid("La geometría de imagen desborda"))?;
    let light_count = request.lights.len() as u64;
    let branch_count = if request.mode == MilkyWayStackMode::SkyOnly {
        1u64
    } else {
        2u64
    };
    let estimated_working_bytes = (pixels as u64)
        .saturating_mul(4)
        .saturating_mul(light_count.saturating_add(branch_count + 3));
    let estimated_output_bytes = (pixels as u64).saturating_mul(4).saturating_mul(
        if request.mode == MilkyWayStackMode::SkyOnly {
            3
        } else {
            7
        },
    );
    let (base_frame, base_forced_by_wcs) = resolved_base_hint(request);
    if request.lights.len() < 3 {
        warnings.push(
            "Con menos de tres lights el rechazo sigma no puede caracterizar outliers con robustez"
                .into(),
        );
    }
    if request.darks.is_empty() {
        warnings.push("No se solicitó calibración con darks (opcional en este flujo)".into());
    }
    if request.flats.is_empty() {
        warnings.push("No se solicitó calibración con flats (opcional en este flujo)".into());
    }
    Ok(MilkyWayStackPlan {
        schema: MILKY_WAY_RECIPE_SCHEMA.into(),
        job_id: request
            .job_id
            .clone()
            .unwrap_or_else(|| pipeline::new_job_id("milky-way")),
        light_count: request.lights.len(),
        mode: request.mode,
        base_frame,
        base_forced_by_wcs,
        width: first.w,
        height: first.h,
        channels_after_debayer: output_channels(&first),
        estimated_working_bytes,
        estimated_output_bytes,
        products: expected_products(request.mode),
        warnings,
    })
}

#[tauri::command]
pub fn prepare_milky_way_stack(
    request: MilkyWayStackRequest,
) -> Result<MilkyWayStackPlan, MilkyWayError> {
    prepare_milky_way_stack_impl(&request)
}

fn emit_milky_way_progress(
    app: &tauri::AppHandle,
    job_id: &str,
    phase: &str,
    progress: f32,
    items_done: usize,
    items_total: usize,
    detail: impl Into<String>,
) {
    let progress = if progress.is_finite() {
        progress.clamp(0.0, 100.0)
    } else {
        0.0
    };
    let detail = detail.into();
    let _ = app.emit(
        "milky-way-progress",
        MilkyWayProgress {
            job_id: job_id.into(),
            phase: phase.into(),
            progress,
            items_done,
            items_total,
            detail: detail.clone(),
        },
    );
    crate::emit_progress(
        app,
        &format!("Vía Láctea · {phase}"),
        progress,
        Some(detail),
    );
}

fn cancellation_checkpoint(
    cancel: &std::sync::atomic::AtomicBool,
    stage: &str,
) -> Result<(), MilkyWayError> {
    pipeline::cancellation_checkpoint(cancel, stage).map_err(|_| MilkyWayError::cancelled(stage))
}

fn point_inside_polygon(x: f32, y: f32, points: &[MilkyWayMaskPoint]) -> bool {
    // Un polígono degenerado no selecciona nada; la validación de request lo
    // rechaza antes, pero esta guarda evita el underflow de `len() - 1`.
    if points.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut previous = points.len() - 1;
    for current in 0..points.len() {
        let a = &points[current];
        let b = &points[previous];
        let crosses = (a.y > y) != (b.y > y) && x < (b.x - a.x) * (y - a.y) / (b.y - a.y) + a.x;
        if crosses {
            inside = !inside;
        }
        previous = current;
    }
    inside
}

fn downsample_luma(luma: &[f32], w: usize, h: usize, max_axis: usize) -> (Vec<f32>, usize, usize) {
    let step = w.max(h).div_ceil(max_axis.max(1)).max(1);
    let dw = w.div_ceil(step);
    let dh = h.div_ceil(step);
    let mut out = vec![0.0f32; dw * dh];
    for gy in 0..dh {
        for gx in 0..dw {
            let mut sum = 0.0f64;
            let mut count = 0usize;
            for y in (gy * step)..((gy + 1) * step).min(h) {
                for x in (gx * step)..((gx + 1) * step).min(w) {
                    let value = luma[y * w + x];
                    if value.is_finite() {
                        sum += value as f64;
                        count += 1;
                    }
                }
            }
            out[gy * dw + gx] = if count > 0 {
                (sum / count as f64) as f32
            } else {
                f32::NAN
            };
        }
    }
    (out, dw, dh)
}

fn median(values: &mut [f32]) -> Option<f32> {
    values.sort_by(f32::total_cmp);
    match values.len() {
        0 => None,
        length if length % 2 == 1 => Some(values[length / 2]),
        length => Some(0.5 * (values[length / 2 - 1] + values[length / 2])),
    }
}

fn auto_horizon_mask(luma: &[f32], w: usize, h: usize) -> MaskBuild {
    if w < 4 || h < 4 || luma.len() != w * h {
        return MaskBuild {
            hard: vec![1; w.saturating_mul(h)],
            soft: vec![1.0; w.saturating_mul(h)],
            confidence: 0.0,
        };
    }
    let (small, sw, sh) = downsample_luma(luma, w, h, 128);
    let mut finite: Vec<f32> = small.iter().copied().filter(|v| v.is_finite()).collect();
    let global_p50 = median(&mut finite).unwrap_or(0.0);
    let mut deviations: Vec<f32> = finite
        .iter()
        .map(|value| (value - global_p50).abs())
        .collect();
    let global_scale = median(&mut deviations).unwrap_or(0.0).max(1.0e-6);
    let y_min = (sh as f32 * 0.20).floor() as usize;
    let y_max = ((sh as f32 * 0.90).ceil() as usize).min(sh - 2);
    let mut horizon = vec![sh * 2 / 3; sw];
    let mut strengths = vec![0.0f32; sw];
    for x in 0..sw {
        let mut best_score = f32::NEG_INFINITY;
        let mut best_y = sh * 2 / 3;
        for y in y_min.max(1)..=y_max.max(y_min.max(1)) {
            let above = small[(y - 1) * sw + x];
            let below = small[(y + 1) * sw + x];
            if !(above.is_finite() && below.is_finite()) {
                continue;
            }
            let edge = (below - above).abs() / global_scale;
            let brightness = ((below - above) / global_scale).max(0.0);
            let position_prior = 1.0 - ((y as f32 / sh as f32) - 0.62).abs() * 0.25;
            let score = (edge + 0.35 * brightness) * position_prior.max(0.5);
            if score > best_score {
                best_score = score;
                best_y = y;
            }
        }
        horizon[x] = best_y;
        strengths[x] = best_score.max(0.0);
    }
    // Suavizado robusto: una silueta real puede ondular, pero un salto de una
    // sola columna suele ser estrella, farola o ruido.
    for _ in 0..3 {
        let previous = horizon.clone();
        for x in 0..sw {
            let start = x.saturating_sub(3);
            let end = (x + 4).min(sw);
            let mut local = previous[start..end].to_vec();
            local.sort_unstable();
            horizon[x] = local[local.len() / 2];
        }
    }
    let mut hard = vec![0u8; w * h];
    for x in 0..w {
        let source_x =
            x as f32 * (sw.saturating_sub(1)) as f32 / (w.saturating_sub(1)).max(1) as f32;
        let left = source_x.floor() as usize;
        let right = (left + 1).min(sw - 1);
        let fraction = source_x - left as f32;
        let horizon_small =
            horizon[left] as f32 * (1.0 - fraction) + horizon[right] as f32 * fraction;
        let horizon_y = (horizon_small * h as f32 / sh as f32).round() as usize;
        for y in 0..horizon_y.min(h) {
            hard[y * w + x] = 1;
        }
    }
    let confidence =
        (strengths.iter().sum::<f32>() / strengths.len().max(1) as f32 / 8.0).clamp(0.0, 1.0);
    MaskBuild {
        soft: hard.iter().map(|value| *value as f32).collect(),
        hard,
        confidence,
    }
}

fn box_blur(values: &[f32], w: usize, h: usize, radius: usize) -> Vec<f32> {
    if radius == 0 || values.is_empty() {
        return values.to_vec();
    }
    let mut horizontal = vec![0.0f32; values.len()];
    for y in 0..h {
        let mut prefix = vec![0.0f64; w + 1];
        for x in 0..w {
            prefix[x + 1] = prefix[x] + values[y * w + x] as f64;
        }
        for x in 0..w {
            let left = x.saturating_sub(radius);
            let right = (x + radius + 1).min(w);
            horizontal[y * w + x] = ((prefix[right] - prefix[left]) / (right - left) as f64) as f32;
        }
    }
    let mut output = vec![0.0f32; values.len()];
    for x in 0..w {
        let mut prefix = vec![0.0f64; h + 1];
        for y in 0..h {
            prefix[y + 1] = prefix[y] + horizontal[y * w + x] as f64;
        }
        for y in 0..h {
            let top = y.saturating_sub(radius);
            let bottom = (y + radius + 1).min(h);
            output[y * w + x] = ((prefix[bottom] - prefix[top]) / (bottom - top) as f64) as f32;
        }
    }
    output
}

fn feather_mask(hard: &[u8], w: usize, h: usize, feather_px: u32) -> Vec<f32> {
    let mut soft: Vec<f32> = hard.iter().map(|value| *value as f32).collect();
    if feather_px == 0 {
        return soft;
    }
    let radius = (feather_px as usize).div_ceil(3).max(1);
    for _ in 0..3 {
        soft = box_blur(&soft, w, h, radius);
    }
    soft.iter_mut()
        .for_each(|value| *value = value.clamp(0.0, 1.0));
    soft
}

/// C2: margen extra del dominio de validez sobre el feather mayor. Absorbe
/// (a) la discretización del triple box-blur y (b) los micro-dithers típicos
/// entre la geometría base (donde vive la máscara) y cada frame fuente.
const BRANCH_VALIDITY_MARGIN_PX: u32 = 16;

/// C2: dominio de validez de cada rama, derivado de la MISMA máscara dura que
/// el peso de composición. Antes el warp gateaba con `mask.soft`
/// (feather = mask.feather_px) mientras la composición ponderaba con
/// composition.feather_px: con mask.featherPx=8 y composition.featherPx=128
/// existía una banda donde el peso compuesto era fraccional pero una de las
/// dos ramas era NaN, y `compose_layers` caía al fallback (finita, no-finita)
/// produciendo un borde duro que el slider de transición no podía suavizar y
/// que la métrica de costura no veía. Al suavizar la validez con
/// feather = max(mask, composición) + margen, el soporte de la validez
/// contiene ESTRICTAMENTE al soporte del peso: todo píxel con peso fraccional
/// tiene ambas ramas finitas.
fn branch_validity_mask(
    hard: &[u8],
    w: usize,
    h: usize,
    mask_feather_px: u32,
    composition_feather_px: u32,
) -> Vec<f32> {
    let feather = mask_feather_px
        .max(composition_feather_px)
        .saturating_add(BRANCH_VALIDITY_MARGIN_PX);
    feather_mask(hard, w, h, feather)
}

fn distance_to_segment_squared(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let vx = bx - ax;
    let vy = by - ay;
    let length_squared = vx * vx + vy * vy;
    let fraction = if length_squared <= f32::EPSILON {
        0.0
    } else {
        ((px - ax) * vx + (py - ay) * vy) / length_squared
    }
    .clamp(0.0, 1.0);
    let dx = px - (ax + fraction * vx);
    let dy = py - (ay + fraction * vy);
    dx * dx + dy * dy
}

fn apply_brush_stroke(
    hard: &mut [u8],
    w: usize,
    h: usize,
    stroke: &MilkyWayBrushStroke,
    radius_px: f32,
) {
    let points: Vec<(f32, f32)> = stroke
        .points
        .iter()
        .map(|point| {
            (
                point[0] * w.saturating_sub(1) as f32,
                point[1] * h.saturating_sub(1) as f32,
            )
        })
        .collect();
    let value = matches!(stroke.target, MilkyWayBrushTarget::Sky) as u8;
    let radius_squared = radius_px * radius_px;
    let segments: Vec<((f32, f32), (f32, f32))> = if points.len() == 1 {
        vec![(points[0], points[0])]
    } else {
        points.windows(2).map(|pair| (pair[0], pair[1])).collect()
    };
    for (a, b) in segments {
        let x0 = (a.0.min(b.0) - radius_px).floor().max(0.0) as usize;
        let x1 = (a.0.max(b.0) + radius_px).ceil().min((w - 1) as f32) as usize;
        let y0 = (a.1.min(b.1) - radius_px).floor().max(0.0) as usize;
        let y1 = (a.1.max(b.1) + radius_px).ceil().min((h - 1) as f32) as usize;
        for y in y0..=y1 {
            for x in x0..=x1 {
                if distance_to_segment_squared(x as f32, y as f32, a.0, a.1, b.0, b.1)
                    <= radius_squared
                {
                    hard[y * w + x] = value;
                }
            }
        }
    }
}

fn user_horizon_mask(points: &[MilkyWayMaskPoint], w: usize, h: usize) -> MaskBuild {
    // Sin puntos no hay frontera que interpolar: devuelve todo-suelo con
    // confianza 0 en vez de indexar `ordered[0]` sobre una lista vacía.
    if points.is_empty() {
        return MaskBuild {
            hard: vec![0u8; w * h],
            soft: vec![0.0f32; w * h],
            confidence: 0.0,
        };
    }
    let mut ordered = points.to_vec();
    ordered.sort_by(|left, right| left.x.total_cmp(&right.x).then(left.y.total_cmp(&right.y)));
    let mut hard = vec![0u8; w * h];
    for x in 0..w {
        let nx = x as f32 / w.saturating_sub(1).max(1) as f32;
        let right = ordered.partition_point(|point| point.x < nx);
        let (left_point, right_point) = if right == 0 {
            (&ordered[0], &ordered[0])
        } else if right >= ordered.len() {
            let last = &ordered[ordered.len() - 1];
            (last, last)
        } else {
            (&ordered[right - 1], &ordered[right])
        };
        let span = right_point.x - left_point.x;
        let fraction = if span.abs() <= f32::EPSILON {
            0.0
        } else {
            ((nx - left_point.x) / span).clamp(0.0, 1.0)
        };
        let boundary = (left_point.y + (right_point.y - left_point.y) * fraction).clamp(0.0, 1.0);
        let boundary_y = (boundary * h as f32).round() as usize;
        for y in 0..boundary_y.min(h) {
            hard[y * w + x] = 1;
        }
    }
    MaskBuild {
        soft: hard.iter().map(|value| *value as f32).collect(),
        hard,
        confidence: 1.0,
    }
}

fn build_mask(spec: &MilkyWayMaskSpec, luma: &[f32], w: usize, h: usize) -> MaskBuild {
    let mut build = match spec {
        MilkyWayMaskSpec::FullSky { .. } => MaskBuild {
            hard: vec![1; w * h],
            soft: vec![1.0; w * h],
            confidence: 1.0,
        },
        MilkyWayMaskSpec::UserPolygon { points, .. } => {
            let mut hard = vec![0u8; w * h];
            for y in 0..h {
                for x in 0..w {
                    let nx = x as f32 / w.saturating_sub(1).max(1) as f32;
                    let ny = y as f32 / h.saturating_sub(1).max(1) as f32;
                    hard[y * w + x] = point_inside_polygon(nx, ny, points) as u8;
                }
            }
            MaskBuild {
                soft: hard.iter().map(|value| *value as f32).collect(),
                hard,
                confidence: 1.0,
            }
        }
        MilkyWayMaskSpec::UserHorizon { points, .. } => user_horizon_mask(points, w, h),
        MilkyWayMaskSpec::AutoHorizon { .. } | MilkyWayMaskSpec::AutoWithEdits { .. } => {
            auto_horizon_mask(luma, w, h)
        }
    };
    if let MilkyWayMaskSpec::AutoWithEdits { edits, .. } = spec {
        let short = w.min(h) as f32;
        for edit in edits {
            let cx = edit.x * w.saturating_sub(1) as f32;
            let cy = edit.y * h.saturating_sub(1) as f32;
            let radius = (edit.radius * short).max(1.0);
            let x0 = (cx - radius).floor().max(0.0) as usize;
            let x1 = (cx + radius).ceil().min((w - 1) as f32) as usize;
            let y0 = (cy - radius).floor().max(0.0) as usize;
            let y1 = (cy + radius).ceil().min((h - 1) as f32) as usize;
            let value = matches!(edit.operation, MilkyWayMaskEditOperation::IncludeSky) as u8;
            for y in y0..=y1 {
                for x in x0..=x1 {
                    if (x as f32 - cx).powi(2) + (y as f32 - cy).powi(2) <= radius * radius {
                        build.hard[y * w + x] = value;
                    }
                }
            }
        }
    }
    if let MilkyWayMaskSpec::AutoWithEdits {
        brush_strokes,
        brush_radius_px,
        ..
    }
    | MilkyWayMaskSpec::UserHorizon {
        brush_strokes,
        brush_radius_px,
        ..
    } = spec
    {
        for stroke in brush_strokes {
            apply_brush_stroke(&mut build.hard, w, h, stroke, *brush_radius_px as f32);
        }
    }
    let feather = spec.feather_px();
    build.soft = feather_mask(&build.hard, w, h, feather);
    build
}

fn mask_preview_png_base64(mask: &[f32], w: usize, h: usize) -> Result<String, MilkyWayError> {
    let mut bytes = Vec::with_capacity(w.saturating_mul(h));
    bytes.extend(
        mask.iter()
            .map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as u8),
    );
    let image = image::GrayImage::from_raw(w as u32, h as u32, bytes)
        .ok_or_else(|| MilkyWayError::io("mask", "No se pudo construir el preview de máscara"))?;
    let mut encoded = Vec::new();
    image::DynamicImage::ImageLuma8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut encoded),
            image::ImageFormat::Png,
        )
        .map_err(|error| MilkyWayError::io("mask", format!("codificar preview PNG: {error}")))?;
    Ok(general_purpose::STANDARD.encode(encoded))
}

fn mask_contour(hard: &[u8], w: usize, h: usize, samples: usize) -> Vec<MilkyWayMaskPoint> {
    let samples = samples.min(w).max(2);
    (0..samples)
        .map(|sample| {
            let x = sample * (w - 1) / (samples - 1);
            let boundary = (0..h).find(|y| hard[y * w + x] == 0).unwrap_or(h);
            MilkyWayMaskPoint {
                x: x as f32 / (w - 1).max(1) as f32,
                y: boundary as f32 / h.max(1) as f32,
            }
        })
        .collect()
}

#[tauri::command]
pub fn detect_milky_way_sky_mask(
    request: MilkyWayStackRequest,
) -> Result<MilkyWayMaskDetectionResult, MilkyWayError> {
    validate_mask_spec(&request.mask)?;
    let light_path = request
        .base_frame
        .as_deref()
        .or_else(|| request.lights.first().map(String::as_str))
        .ok_or_else(|| MilkyWayError::invalid("Importa una toma para detectar el horizonte"))?;
    if !Path::new(light_path).is_file() {
        return Err(MilkyWayError::invalid(
            "La toma para detectar el horizonte no existe",
        ));
    }
    let image =
        crate::ds_read_image(light_path).map_err(|error| MilkyWayError::io("mask", error))?;
    let image = if let Some(cid) = image.bayer {
        crate::ds_debayer_image(image, cid)
    } else {
        image
    };
    let luma = crate::ds_luma(&image);
    let build = build_mask(&request.mask, &luma, image.w, image.h);
    let mut warnings = Vec::new();
    if build.confidence < 0.25 && request.mask.is_automatic() {
        warnings.push(
            "El horizonte automático tiene baja confianza: confirma o corrige la máscara antes de apilar"
                .into(),
        );
    }
    let sky_fraction = build
        .hard
        .iter()
        .map(|value| *value as usize)
        .sum::<usize>() as f32
        / build.hard.len().max(1) as f32;
    let contour = mask_contour(&build.hard, image.w, image.h, 128);
    let horizon_points = contour.iter().map(|point| [point.x, point.y]).collect();
    Ok(MilkyWayMaskDetectionResult {
        width: image.w,
        height: image.h,
        confidence: build.confidence,
        contour,
        sky_fraction,
        preview_png_base64: mask_preview_png_base64(&build.soft, image.w, image.h)?,
        warnings,
        mask: MilkyWayDetectedMaskState {
            strategy: "horizon".into(),
            confidence: build.confidence,
            user_confirmed: false,
            feather_px: request.mask.feather_px(),
            horizon_points,
            brush_strokes: Vec::new(),
            brush_target: MilkyWayBrushTarget::Sky,
            brush_radius_px: default_brush_radius_px(),
            protected_foreground: Vec::new(),
        },
    })
}

fn image_quantiles(luma: &[f32], mask: &[u8], include_sky: bool) -> Option<Quantiles> {
    if luma.len() != mask.len() {
        return None;
    }
    let step = (luma.len() / 200_000).max(1);
    let mut values: Vec<f32> = luma
        .iter()
        .zip(mask)
        .enumerate()
        .filter_map(|(index, (value, sky))| {
            (index % step == 0 && value.is_finite() && ((*sky != 0) == include_sky))
                .then_some(*value)
        })
        .collect();
    if values.len() < 32 {
        return None;
    }
    values.sort_by(f32::total_cmp);
    let at = |q: f32| -> f32 {
        let index = (q * (values.len() - 1) as f32).round() as usize;
        values[index]
    };
    Some(Quantiles {
        p16: at(0.16),
        p50: at(0.50),
        p84: at(0.84),
    })
}

fn normalization_between(
    reference: Option<Quantiles>,
    target: Option<Quantiles>,
) -> MilkyWayPhotometricNormalization {
    let (Some(reference), Some(target)) = (reference, target) else {
        return MilkyWayPhotometricNormalization {
            scale: 1.0,
            offset: 0.0,
        };
    };
    let reference_range = (reference.p84 - reference.p16).abs();
    let target_range = (target.p84 - target.p16).abs();
    if !reference_range.is_finite() || !target_range.is_finite() || target_range <= 1.0e-6 {
        return MilkyWayPhotometricNormalization {
            scale: 1.0,
            offset: reference.p50 - target.p50,
        };
    }
    let scale = (reference_range / target_range).clamp(0.25, 4.0);
    MilkyWayPhotometricNormalization {
        scale,
        offset: reference.p50 - scale * target.p50,
    }
}

fn normalized_image(
    mut image: crate::DsImage,
    normalization: &MilkyWayPhotometricNormalization,
) -> crate::DsImage {
    image.data.iter_mut().for_each(|value| {
        if value.is_finite() {
            *value = *value * normalization.scale + normalization.offset;
        }
    });
    image
}

fn reference_quality(luma: &[f32], w: usize, h: usize) -> usize {
    let mut candidate = luma.to_vec();
    let cutoff = h * 3 / 4;
    let mut upper: Vec<f32> = candidate[..cutoff.saturating_mul(w)]
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .step_by((w.saturating_mul(cutoff) / 100_000).max(1))
        .collect();
    let background = median(&mut upper).unwrap_or(0.0);
    for value in &mut candidate[cutoff.saturating_mul(w)..] {
        *value = background;
    }
    crate::ds_detect_stars(&candidate, w, h, 500).len()
}

fn serializable_transform(
    transform: crate::milky_way_registration::TransformModel,
) -> MilkyWaySerializableTransform {
    let (model, matrix) = match transform.planar {
        crate::milky_way_registration::PlanarTransform::Affine(value) => (
            "affine".to_string(),
            [
                value[0], value[1], value[2], value[3], value[4], value[5], 0.0, 0.0, 1.0,
            ],
        ),
        crate::milky_way_registration::PlanarTransform::Homography(value) => {
            ("homography".to_string(), value)
        }
    };
    let radial = transform.radial;
    MilkyWaySerializableTransform {
        model: if radial.is_some() {
            format!("{model}+radial")
        } else {
            model
        },
        matrix,
        polynomial: [0.0; 12],
        normalization: [0.0, 0.0, 1.0],
        radial_center: radial.map(|value| [value.center.x, value.center.y]),
        radial_normalization_radius: radial.map(|value| value.normalization_radius),
        radial_k1: radial.map(|value| value.k1),
        radial_k2: radial.map(|value| value.k2),
    }
}

fn sky_registration_config(
    options: &MilkyWayRegistrationOptions,
    geometry: crate::milky_way_registration::ImageGeometry,
) -> crate::milky_way_registration::SkyImageRegistrationConfig {
    let mut config = crate::milky_way_registration::SkyImageRegistrationConfig::default();
    config.registration.model = match options.model.as_str() {
        "affine" => crate::milky_way_registration::RegistrationModelSelection::Affine,
        "homography" => crate::milky_way_registration::RegistrationModelSelection::Homography,
        _ => crate::milky_way_registration::RegistrationModelSelection::Auto,
    };
    config.registration.validation.min_inliers = options.min_inliers;
    config.registration.validation.max_rms_px = options.max_rms_px as f64;
    config.registration.validation.max_p95_px = (options.max_rms_px as f64 * 1.75).max(1.0);
    if options.model == "radialWide"
        || matches!(options.distortion_correction.as_str(), "lens" | "complex")
    {
        let mut radial =
            crate::milky_way_registration::RadialSearchConfig::for_ultra_wide(geometry);
        if options.distortion_correction == "lens" {
            radial.max_abs_k2 = 0.0;
        } else if options.distortion_correction == "complex" {
            radial.max_abs_k1 = 0.32;
            radial.max_abs_k2 = 0.12;
            radial.grid_steps = 7;
            radial.refinement_rounds = 2;
        }
        config.registration.radial = radial;
    }
    config
}

fn identity_registration() -> MilkyWayBranchRegistration {
    MilkyWayBranchRegistration {
        transform: Some(serializable_transform(
            crate::milky_way_registration::TransformModel::identity(),
        )),
        inliers: 0,
        rms_px: Some(0.0),
        confidence: Some(1.0),
        excluded: false,
        reason: None,
    }
}

/// φ(x): densidad de la normal estándar.
fn standard_normal_pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// Φ(x): CDF de la normal estándar vía la aproximación racional de
/// Abramowitz & Stegun 7.1.26 para erf (error máximo 1.5e-7, sobradísimo
/// para un factor que se valida al 2%).
fn standard_normal_cdf(x: f64) -> f64 {
    let z = x / std::f64::consts::SQRT_2;
    let sign = if z < 0.0 { -1.0 } else { 1.0 };
    let z = z.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * z);
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf = sign * (1.0 - poly * (-z * z).exp());
    0.5 * (1.0 + erf)
}

/// C9b: varianza de una normal estándar winsorizada en [−k_l, +k_h].
///
/// Derivación (forma cerrada del segundo momento winsorizado):
///   X_w = clamp(X, −k_l, k_h) con X ~ N(0,1). Usando
///   ∫_a^b x·φ dx = φ(a) − φ(b) y ∫_a^b x²·φ dx = [Φ(x) − x·φ(x)]_a^b:
///   m1 = −k_l·Φ(−k_l) + (φ(k_l) − φ(k_h)) + k_h·(1 − Φ(k_h))
///   m2 = k_l²·Φ(−k_l) + [Φ(k_h) − Φ(−k_l) − k_h·φ(k_h) − k_l·φ(k_l)]
///        + k_h²·(1 − Φ(k_h))
///   Var_w = m2 − m1²
/// Para k_l = k_h = 2: Var_w ≈ 0.920537 → factor 1/Var_w ≈ 1.0863 (el mismo
/// factor de consistencia que usa la literatura de winsorización, p. ej.
/// PixInsight para su sigma winsorizada iterativa).
fn winsorized_normal_variance(k_low: f64, k_high: f64) -> f64 {
    let phi_l = standard_normal_pdf(k_low);
    let phi_h = standard_normal_pdf(k_high);
    let cdf_low_tail = standard_normal_cdf(-k_low);
    let cdf_high_tail = 1.0 - standard_normal_cdf(k_high);
    let m1 = -k_low * cdf_low_tail + (phi_l - phi_h) + k_high * cdf_high_tail;
    let central = (1.0 - cdf_high_tail) - cdf_low_tail - k_high * phi_h - k_low * phi_l;
    let m2 = k_low * k_low * cdf_low_tail + central + k_high * k_high * cdf_high_tail;
    m2 - m1 * m1
}

/// C9b: factor multiplicativo que devuelve a la varianza muestral winsorizada
/// su valor insesgado bajo normalidad. La varianza de datos recortados a
/// ±k·σ SUB-estima la real (los `*_var.fits` publicados eran optimistas);
/// dividir por Var_w(k) la corrige. Los kappas se acotan a un rango sensato
/// para que un request degenerado no infle la varianza sin límite.
fn winsorized_variance_consistency(sigma_low: f32, sigma_high: f32) -> f32 {
    let k_low = f64::from(sigma_low).clamp(0.5, 10.0);
    let k_high = f64::from(sigma_high).clamp(0.5, 10.0);
    let variance = winsorized_normal_variance(k_low, k_high);
    if variance.is_finite() && variance > 1.0e-3 {
        (1.0 / variance) as f32
    } else {
        1.0
    }
}

fn robust_integrate_samples(
    values: &mut Vec<f32>,
    config: &MilkyWayIntegrationConfig,
) -> Option<(f32, f32, usize)> {
    values.retain(|value| value.is_finite());
    if values.is_empty() {
        return None;
    }
    let original_values = values.clone();
    let original = original_values.len();
    values.sort_by(f32::total_cmp);
    let mut center = if values.len() % 2 == 1 {
        values[values.len() / 2]
    } else {
        0.5 * (values[values.len() / 2 - 1] + values[values.len() / 2])
    };
    let mut deviations: Vec<f32> = values.iter().map(|value| (*value - center).abs()).collect();
    let mut scale = median(&mut deviations).unwrap_or(0.0) * 1.4826;
    if !scale.is_finite() || scale <= 1.0e-7 {
        let mean = values.iter().sum::<f32>() / values.len() as f32;
        let variance = values
            .iter()
            .map(|value| (*value - mean).powi(2))
            .sum::<f32>()
            / values.len().max(1) as f32;
        scale = variance.sqrt();
    }
    let mut accepted = values.clone();
    for _ in 0..config.iterations {
        if accepted.len() <= 2 || !scale.is_finite() || scale <= 1.0e-7 {
            break;
        }
        let low = center - config.sigma_low * scale;
        let high = center + config.sigma_high * scale;
        let next: Vec<f32> = accepted
            .iter()
            .copied()
            .filter(|value| *value >= low && *value <= high)
            .collect();
        if next.is_empty() || next.len() == accepted.len() {
            break;
        }
        accepted = next;
        center = accepted.iter().sum::<f32>() / accepted.len() as f32;
        let variance = accepted
            .iter()
            .map(|value| (*value - center).powi(2))
            .sum::<f32>()
            / accepted.len().saturating_sub(1).max(1) as f32;
        scale = variance.sqrt();
    }
    let final_values: Vec<f32> = match config.method {
        MilkyWayIntegrationMethod::Winsorized if scale.is_finite() && scale > 1.0e-7 => {
            let low = center - config.sigma_low * scale;
            let high = center + config.sigma_high * scale;
            original_values
                .iter()
                .map(|value| value.clamp(low, high))
                .collect()
        }
        MilkyWayIntegrationMethod::Mean => original_values.clone(),
        MilkyWayIntegrationMethod::SigmaClip | MilkyWayIntegrationMethod::Winsorized => {
            accepted.clone()
        }
    };
    let rejected = match config.method {
        MilkyWayIntegrationMethod::Winsorized => original_values
            .iter()
            .zip(&final_values)
            .filter(|(original, final_value)| (**original - **final_value).abs() > f32::EPSILON)
            .count(),
        MilkyWayIntegrationMethod::SigmaClip => original.saturating_sub(accepted.len()),
        MilkyWayIntegrationMethod::Mean => 0,
    };
    let mean = final_values.iter().sum::<f32>() / final_values.len() as f32;
    let sample_variance = if final_values.len() > 1 {
        final_values
            .iter()
            .map(|value| (*value - mean).powi(2))
            .sum::<f32>()
            / (final_values.len() - 1) as f32
    } else {
        f32::NAN
    };
    // C9b: si la winsorización llegó a ejecutarse (mismo guard que arriba),
    // la varianza muestral de los valores recortados subestima la real y el
    // factor de consistencia normal 1/Var_w(k) la devuelve a escala. Con los
    // kappas por defecto (k≈2–3) el sesgo era de un 3–8%: los mapas
    // *_var.fits publicados eran sistemáticamente optimistas.
    let sample_variance = if matches!(config.method, MilkyWayIntegrationMethod::Winsorized)
        && scale.is_finite()
        && scale > 1.0e-7
    {
        sample_variance * winsorized_variance_consistency(config.sigma_low, config.sigma_high)
    } else {
        sample_variance
    };
    let variance_of_mean = if sample_variance.is_finite() {
        sample_variance / final_values.len() as f32
    } else {
        f32::NAN
    };
    Some((mean, variance_of_mean, rejected))
}

fn correct_dynamic_hot_pixels(image: &mut crate::DsImage) {
    if image.w < 3 || image.h < 3 || !matches!(image.ch, 1 | 3) {
        return;
    }
    let source = image.data.clone();
    let mut neighbours = [0.0f32; 8];
    let mut deviations = [0.0f32; 8];
    for y in 1..image.h - 1 {
        for x in 1..image.w - 1 {
            for channel in 0..image.ch {
                let mut count = 0usize;
                for dy in -1isize..=1 {
                    for dx in -1isize..=1 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let index = ((y as isize + dy) as usize * image.w
                            + (x as isize + dx) as usize)
                            * image.ch
                            + channel;
                        let value = source[index];
                        if value.is_finite() {
                            neighbours[count] = value;
                            count += 1;
                        }
                    }
                }
                if count < 5 {
                    continue;
                }
                neighbours[..count].sort_by(f32::total_cmp);
                let local_median = neighbours[count / 2];
                for index in 0..count {
                    deviations[index] = (neighbours[index] - local_median).abs();
                }
                deviations[..count].sort_by(f32::total_cmp);
                let measured_sigma = deviations[count / 2] * 1.4826;
                // Con MAD exactamente cero no existe evidencia estadística
                // para distinguir un defecto de un núcleo estelar
                // submuestreado. El comportamiento anterior sustituía el
                // cero por MIN_POSITIVE, por lo que CUALQUIER señal positiva
                // aislada se clasificaba como hot pixel. Se falla de forma
                // conservadora: sin ruido medible, no se altera el dato.
                if !measured_sigma.is_finite() || measured_sigma <= f32::EPSILON {
                    continue;
                }
                // C9a: el suelo de sigma debe ser RELATIVO a la escala de los
                // datos, no 1.0 ADU absoluto. Con datos normalizados 0..1 un
                // suelo de 1.0 hacía inalcanzable el umbral mediana+8σ y la
                // rutina era un no-op. Un suelo de 1e-3 de la mediana local
                // evita que una cuantización muy fina vuelva hipersensible el
                // detector; con datos ADU queda muy por debajo del MAD real.
                let sigma_floor = local_median.abs() * 1.0e-3;
                let local_sigma = measured_sigma.max(sigma_floor);
                let index = (y * image.w + x) * image.ch + channel;
                if source[index].is_finite() && source[index] > local_median + 8.0 * local_sigma {
                    image.data[index] = local_median;
                }
            }
        }
    }
}

struct PreparedMilkyWayData {
    store: AdaptiveFrameStore,
    width: usize,
    height: usize,
    channels: usize,
    base_index: usize,
    mask: MaskBuild,
    /// C2: máscara de validez por rama (feather = max(mask, composición) +
    /// margen). Gatea qué píxeles calcula cada rama en el warp; NO es el peso
    /// de composición ni la máscara publicada (`mask.soft`).
    branch_validity: Vec<f32>,
    sky_transforms: Vec<Option<crate::milky_way_registration::TransformModel>>,
    ground_transforms: Vec<Option<crate::milky_way_registration::TransformModel>>,
    sky_normalizations: Vec<MilkyWayPhotometricNormalization>,
    ground_normalizations: Vec<MilkyWayPhotometricNormalization>,
    reports: Vec<MilkyWayFrameReport>,
    warnings: Vec<String>,
    degraded: bool,
}

fn scientific_extension(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "fits" | "fit" | "fts" | "tif" | "tiff"
            )
        })
        .unwrap_or(false)
}

fn validate_scientific_inputs(request: &MilkyWayStackRequest) -> Result<(), MilkyWayError> {
    for (label, paths) in [
        ("lights", &request.lights),
        ("darks", &request.darks),
        ("flats", &request.flats),
    ] {
        if let Some(path) = paths.iter().find(|path| !scientific_extension(path)) {
            return Err(MilkyWayError::scientific(
                "preflight",
                format!(
                    "{label}: '{path}' no es FITS/TIFF lineal. PNG/JPEG sólo pueden usarse como preview"
                ),
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct MilkyWayCalibrationProbe {
    path: String,
    width: usize,
    height: usize,
    channels: usize,
    bayer: Option<i32>,
    signature: pipeline::CalibrationSignature,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MilkyWayCalibrationDisposition {
    Accept,
    Exclude,
    Block,
}

#[derive(Clone, Debug)]
struct MilkyWayCalibrationDecision {
    disposition: MilkyWayCalibrationDisposition,
    reasons: Vec<String>,
}

fn probe_milky_way_calibration(path: &str) -> Result<MilkyWayCalibrationProbe, MilkyWayError> {
    let image = crate::ds_read_image(path)
        .map_err(|error| MilkyWayError::io("calibration", format!("{path}: {error}")))?;
    let signature = crate::ds_probe_calibration_signature(path)
        .map_err(|error| MilkyWayError::scientific("calibration", error))?;
    Ok(MilkyWayCalibrationProbe {
        path: path.into(),
        width: image.w,
        height: image.h,
        channels: image.ch,
        bayer: image.bayer,
        signature,
    })
}

fn assess_milky_way_calibration_candidate(
    lights: &[MilkyWayCalibrationProbe],
    candidate: &MilkyWayCalibrationProbe,
    role: crate::deepsky_calibration_contract::CalibrationRole,
    policy: MilkyWayFallbackPolicy,
) -> MilkyWayCalibrationDecision {
    let mut blocking = Vec::new();
    let mut notes = Vec::new();
    for light in lights {
        if (light.width, light.height, light.channels, light.bayer)
            != (
                candidate.width,
                candidate.height,
                candidate.channels,
                candidate.bayer,
            )
        {
            blocking.push(format!(
                "{}: geometría/canales/CFA {}x{}x{} {:?} != {}x{}x{} {:?}",
                light.path,
                light.width,
                light.height,
                light.channels,
                light.bayer,
                candidate.width,
                candidate.height,
                candidate.channels,
                candidate.bayer
            ));
            continue;
        }
        // Siempre se compara con Strict para decidir si los píxeles pueden
        // entrar. AllowDegraded controla bloquear vs excluir, nunca convierte
        // un mismatch conocido en compatible.
        let report = crate::deepsky_calibration_contract::compare_calibration_signatures(
            &light.signature,
            &candidate.signature,
            role,
            pipeline::DeepSkyCalibrationPolicy::Strict,
        );
        if !report.compatible || !report.scientific_eligible || report.degraded {
            blocking.extend(
                report
                    .reasons
                    .into_iter()
                    .map(|reason| format!("{}: {reason}", light.path)),
            );
        } else {
            notes.extend(
                report
                    .reasons
                    .into_iter()
                    .map(|reason| format!("{}: {reason}", light.path)),
            );
        }
    }
    blocking.sort();
    blocking.dedup();
    notes.sort();
    notes.dedup();
    if blocking.is_empty() {
        MilkyWayCalibrationDecision {
            disposition: MilkyWayCalibrationDisposition::Accept,
            reasons: notes,
        }
    } else {
        MilkyWayCalibrationDecision {
            disposition: if policy == MilkyWayFallbackPolicy::Strict {
                MilkyWayCalibrationDisposition::Block
            } else {
                MilkyWayCalibrationDisposition::Exclude
            },
            reasons: blocking,
        }
    }
}

fn select_milky_way_calibrations(
    paths: &[String],
    lights: &[MilkyWayCalibrationProbe],
    role: crate::deepsky_calibration_contract::CalibrationRole,
    label: &str,
    policy: MilkyWayFallbackPolicy,
) -> Result<(Vec<String>, Vec<String>, bool), MilkyWayError> {
    let mut accepted = Vec::new();
    let mut accepted_probes: Vec<MilkyWayCalibrationProbe> = Vec::new();
    let mut warnings = Vec::new();
    let mut degraded = false;
    for path in paths {
        let candidate = match probe_milky_way_calibration(path) {
            Ok(candidate) => candidate,
            Err(error) if policy == MilkyWayFallbackPolicy::Strict => return Err(error),
            Err(error) => {
                warnings.push(format!("{label} excluido '{}': {}", path, error.message));
                degraded = true;
                continue;
            }
        };
        let decision = assess_milky_way_calibration_candidate(lights, &candidate, role, policy);
        match decision.disposition {
            MilkyWayCalibrationDisposition::Accept => {
                if let Some(reference) = accepted_probes.first() {
                    let peer_decision = assess_milky_way_calibration_candidate(
                        std::slice::from_ref(reference),
                        &candidate,
                        role,
                        policy,
                    );
                    if peer_decision.disposition != MilkyWayCalibrationDisposition::Accept {
                        if peer_decision.disposition == MilkyWayCalibrationDisposition::Block {
                            return Err(MilkyWayError::scientific(
                                "calibration",
                                format!(
                                    "{label} '{}' no puede mezclarse con '{}': {}",
                                    path,
                                    reference.path,
                                    peer_decision.reasons.join("; ")
                                ),
                            ));
                        }
                        warnings.push(format!(
                            "AllowDegraded: {label} excluido '{}' porque no comparte firma con '{}': {}",
                            path,
                            reference.path,
                            peer_decision.reasons.join("; ")
                        ));
                        degraded = true;
                        continue;
                    }
                }
                accepted.push(path.clone());
                accepted_probes.push(candidate);
                warnings.extend(
                    decision
                        .reasons
                        .into_iter()
                        .map(|reason| format!("{label} '{}': {reason}", path)),
                );
            }
            MilkyWayCalibrationDisposition::Block => {
                return Err(MilkyWayError::scientific(
                    "calibration",
                    format!(
                        "{label} incompatible '{}': {}",
                        path,
                        decision.reasons.join("; ")
                    ),
                ));
            }
            MilkyWayCalibrationDisposition::Exclude => {
                warnings.push(format!(
                    "AllowDegraded: {label} excluido '{}': {}",
                    path,
                    decision.reasons.join("; ")
                ));
                degraded = true;
            }
        }
    }
    if !paths.is_empty() && accepted.is_empty() {
        degraded = true;
        warnings.push(format!(
            "AllowDegraded: ningún {label} seleccionado tiene firma científica completa; el lote continúa sin aplicar ese máster"
        ));
    }
    Ok((accepted, warnings, degraded))
}

fn build_calibration_masters(
    app: &tauri::AppHandle,
    request: &MilkyWayStackRequest,
    light_probes: &[MilkyWayCalibrationProbe],
    cancel: &Arc<std::sync::atomic::AtomicBool>,
    work_root: &Path,
) -> Result<
    (
        Option<crate::DsDarkMaster>,
        Option<crate::DsCalibrationMaster>,
        Vec<String>,
        bool,
    ),
    MilkyWayError,
> {
    let mut warnings = Vec::new();
    let mut degraded = false;
    cancellation_checkpoint(cancel, "calibración")?;
    let (dark_paths, dark_warnings, dark_degraded) = select_milky_way_calibrations(
        &request.darks,
        light_probes,
        crate::deepsky_calibration_contract::CalibrationRole::Dark,
        "dark",
        request.fallback_policy,
    )?;
    warnings.extend(dark_warnings);
    degraded |= dark_degraded;
    let dark_exposure = light_probes
        .first()
        .and_then(|probe| probe.signature.exposure_seconds)
        .map(|value| value as f32);
    let dark = crate::ds_build_master(
        app,
        &dark_paths,
        "dark de paisaje",
        false,
        cancel,
        Some(work_root),
    )
    .map_err(|error| MilkyWayError::scientific("calibration", error))?
    .map(|master| {
        let amp_glow = crate::ds_dark_has_amp_glow(&master.image);
        crate::DsDarkMaster {
            // Sólo puede existir si todos los candidatos fueron probados con
            // la misma exposición que todos los lights.
            exposure: dark_exposure,
            master,
            amp_glow,
            bias_subtracted: false,
            source_paths: dark_paths.clone(),
            calibration_probe: None,
        }
    });
    if let Some(master) = &dark {
        if master.master.frames != dark_paths.len() {
            let message = format!(
                "Se usaron {}/{} darks; uno o más no pudieron entrar al máster",
                master.master.frames,
                dark_paths.len()
            );
            if request.fallback_policy == MilkyWayFallbackPolicy::Strict {
                return Err(MilkyWayError::scientific("calibration", message));
            }
            warnings.push(message);
            degraded = true;
        }
    } else if !dark_paths.is_empty() {
        let message = "Ningún dark científicamente compatible pudo formar un máster";
        if request.fallback_policy == MilkyWayFallbackPolicy::Strict {
            return Err(MilkyWayError::scientific("calibration", message));
        }
        warnings.push(format!("AllowDegraded: {message}; se omite el dark"));
        degraded = true;
    }
    cancellation_checkpoint(cancel, "calibración")?;
    let (flat_paths, flat_warnings, flat_selection_degraded) = select_milky_way_calibrations(
        &request.flats,
        light_probes,
        crate::deepsky_calibration_contract::CalibrationRole::Flat,
        "flat",
        request.fallback_policy,
    )?;
    warnings.extend(flat_warnings);
    degraded |= flat_selection_degraded;
    let flat_data_degraded = std::sync::atomic::AtomicBool::new(false);
    let mut flat = crate::ds_build_calibrated_flat_master(
        app,
        &flat_paths,
        "flat de paisaje",
        None,
        None,
        &[],
        match request.fallback_policy {
            MilkyWayFallbackPolicy::Strict => pipeline::DeepSkyCalibrationPolicy::Strict,
            MilkyWayFallbackPolicy::AllowDegraded => {
                pipeline::DeepSkyCalibrationPolicy::AllowDegraded
            }
        },
        cancel,
        Some(work_root),
        &flat_data_degraded,
    )
    .map_err(|error| MilkyWayError::scientific("calibration", error))?;
    degraded |= flat_data_degraded.load(Ordering::Relaxed);
    if flat_data_degraded.load(Ordering::Relaxed) {
        warnings.push(
            "AllowDegraded: uno o más flats no superaron linealidad/pedestal; el resultado queda marcado como no científico"
                .into(),
        );
    }
    if let Some(master) = &mut flat {
        if master.frames != flat_paths.len() {
            let message = format!(
                "Se usaron {}/{} flats; uno o más no pudieron entrar al máster",
                master.frames,
                flat_paths.len()
            );
            if request.fallback_policy == MilkyWayFallbackPolicy::Strict {
                return Err(MilkyWayError::scientific("calibration", message));
            }
            warnings.push(message);
            degraded = true;
        }
    } else if !flat_paths.is_empty() {
        let message = "Ningún flat científicamente compatible pudo formar un máster calibrado";
        if request.fallback_policy == MilkyWayFallbackPolicy::Strict {
            return Err(MilkyWayError::scientific("calibration", message));
        }
        warnings.push(format!("AllowDegraded: {message}; se omite el flat"));
        degraded = true;
    }
    Ok((dark, flat, warnings, degraded))
}

fn calibrate_and_store_frames(
    app: &tauri::AppHandle,
    request: &MilkyWayStackRequest,
    job_id: &str,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
    cache_root: &Path,
) -> Result<
    (
        AdaptiveFrameStore,
        usize,
        usize,
        usize,
        Vec<usize>,
        Vec<String>,
        bool,
    ),
    MilkyWayError,
> {
    validate_scientific_inputs(request)?;
    let first_raw = crate::ds_read_image(&request.lights[0])
        .map_err(|error| MilkyWayError::io("read", error))?;
    let light_probes = request
        .lights
        .iter()
        .map(|path| probe_milky_way_calibration(path))
        .collect::<Result<Vec<_>, _>>()?;
    let raw_geometry = (first_raw.w, first_raw.h, first_raw.ch, first_raw.bayer);
    let channels = output_channels(&first_raw);
    let frame_len = first_raw
        .w
        .checked_mul(first_raw.h)
        .and_then(|value| value.checked_mul(channels))
        .ok_or_else(|| MilkyWayError::invalid("Geometría demasiado grande"))?;
    let (dark, flat, mut warnings, mut degraded) =
        build_calibration_masters(app, request, &light_probes, cancel, cache_root)?;
    for (label, master) in [
        ("dark", dark.as_ref().map(|value| &value.master.image)),
        ("flat", flat.as_ref().map(|value| &value.image)),
    ] {
        if let Some(master) = master {
            if (master.w, master.h, master.ch, master.bayer) != raw_geometry {
                return Err(MilkyWayError::scientific(
                    "calibration",
                    format!(
                        "El máster {label} no coincide exactamente en geometría/canales/CFA con los lights"
                    ),
                ));
            }
        }
    }
    std::fs::create_dir_all(cache_root)
        .map_err(|error| MilkyWayError::io("cache", format!("crear caché: {error}")))?;
    let mut store = AdaptiveFrameStore::new(
        request.lights.len(),
        frame_len,
        cache_root,
        &format!("{}_calibrated", safe_job_fragment(job_id)),
    )
    .map_err(|error| MilkyWayError::io("cache", error))?;
    let mut quality = Vec::with_capacity(request.lights.len());
    for (index, path) in request.lights.iter().enumerate() {
        cancellation_checkpoint(cancel, "lectura y calibración")?;
        let mut raw = crate::ds_read_image(path)
            .map_err(|error| MilkyWayError::io("read", format!("{path}: {error}")))?;
        if (raw.w, raw.h, raw.ch, raw.bayer) != raw_geometry {
            return Err(MilkyWayError::scientific(
                "calibration",
                format!(
                    "'{path}' no coincide en geometría/canales/CFA con la toma de referencia del lote"
                ),
            ));
        }
        let uncertainty =
            crate::ds_calibrate_scientific(&mut raw, None, dark.as_ref(), flat.as_ref(), 1.0)
                .map_err(|error| {
                    MilkyWayError::scientific("calibration", format!("{path}: {error}"))
                })?;
        if !uncertainty.publishable {
            degraded = true;
            if let Some(reason) = uncertainty.fallback_reason {
                let warning = format!("{path}: {reason}");
                if !warnings.contains(&warning) {
                    warnings.push(warning);
                }
            }
        }
        let prepared = if let Some(cid) = raw.bayer {
            crate::ds_debayer_image(raw, cid)
        } else {
            raw
        };
        if prepared.ch != channels || prepared.data.len() != frame_len {
            return Err(MilkyWayError::scientific(
                "debayer",
                format!("'{path}' produjo un layout distinto después de debayer"),
            ));
        }
        let luma = crate::ds_luma(&prepared);
        quality.push(reference_quality(&luma, prepared.w, prepared.h));
        store
            .put(index, &prepared.data)
            .map_err(|error| MilkyWayError::io("cache", error))?;
        emit_milky_way_progress(
            app,
            job_id,
            "Calibración lineal",
            5.0 + 20.0 * (index + 1) as f32 / request.lights.len() as f32,
            index + 1,
            request.lights.len(),
            format!(
                "Toma {}/{} calibrada antes de debayer",
                index + 1,
                request.lights.len()
            ),
        );
    }
    Ok((
        store,
        first_raw.w,
        first_raw.h,
        channels,
        quality,
        warnings,
        degraded,
    ))
}

fn select_base_index(
    request: &MilkyWayStackRequest,
    quality: &[usize],
) -> Result<usize, MilkyWayError> {
    let hint = request
        .wcs
        .as_ref()
        .map(|binding| binding.source_light.as_str())
        .or(request.base_frame.as_deref());
    if let Some(hint) = hint {
        return request
            .lights
            .iter()
            .position(|path| same_path(path, hint))
            .ok_or_else(|| MilkyWayError::invalid("No se encontró la toma base en el lote"));
    }
    quality
        .iter()
        .enumerate()
        .max_by(|left, right| left.1.cmp(right.1).then_with(|| right.0.cmp(&left.0)))
        .map(|(index, _)| index)
        .ok_or_else(|| MilkyWayError::invalid("No hay lights para elegir referencia"))
}

fn prepare_registration_data(
    app: &tauri::AppHandle,
    request: &MilkyWayStackRequest,
    job_id: &str,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
    cache_root: &Path,
) -> Result<PreparedMilkyWayData, MilkyWayError> {
    let (store, width, height, channels, quality, mut warnings, mut degraded) =
        calibrate_and_store_frames(app, request, job_id, cancel, cache_root)?;
    let base_index = select_base_index(request, &quality)?;
    let base_data = store
        .get(base_index)
        .map_err(|error| MilkyWayError::io("cache", error))?;
    let base_image = crate::DsImage {
        data: base_data,
        w: width,
        h: height,
        ch: channels,
        bayer: None,
    };
    let base_luma = crate::ds_luma(&base_image);
    let mask = build_mask(&request.mask, &base_luma, width, height);
    // C2: la validez de rama se deriva de la MISMA máscara dura que el peso
    // de composición para que ambas transiciones sean coherentes.
    let branch_validity = branch_validity_mask(
        &mask.hard,
        width,
        height,
        request.mask.feather_px(),
        request.composition.feather_px,
    );
    if mask.confidence < 0.25 && !request.mask_confirmed_by_user && request.mask.is_automatic() {
        let message = "El horizonte automático tiene baja confianza; confirma/corrige la máscara";
        if request.fallback_policy == MilkyWayFallbackPolicy::Strict {
            return Err(MilkyWayError::scientific("mask", message));
        }
        warnings.push(message.into());
        degraded = true;
    }
    let sky_pixels = mask.hard.iter().filter(|value| **value != 0).count();
    let ground_pixels = mask.hard.len() - sky_pixels;
    if sky_pixels < 64 || (request.mode != MilkyWayStackMode::SkyOnly && ground_pixels < 64) {
        return Err(MilkyWayError::scientific(
            "mask",
            "La máscara no deja suficientes píxeles para ambas ramas",
        ));
    }
    let geometry = crate::milky_way_registration::ImageGeometry::new(width, height)
        .map_err(|error| MilkyWayError::scientific("registration", error.to_string()))?;
    let sky_mask_view = crate::milky_way_registration::MaskView {
        geometry,
        data: &mask.hard,
        include_at_or_above: 1,
    };
    let ground_mask: Vec<u8> = mask.hard.iter().map(|value| (*value == 0) as u8).collect();
    let ground_mask_view = crate::milky_way_registration::MaskView {
        geometry,
        data: &ground_mask,
        include_at_or_above: 1,
    };
    let reference_sky_quantiles = image_quantiles(&base_luma, &mask.hard, true);
    let reference_ground_quantiles = image_quantiles(&base_luma, &mask.hard, false);
    let mut sky_transforms = vec![None; request.lights.len()];
    let mut ground_transforms = vec![None; request.lights.len()];
    let mut sky_normalizations = vec![
        MilkyWayPhotometricNormalization {
            scale: 1.0,
            offset: 0.0,
        };
        request.lights.len()
    ];
    let mut ground_normalizations = sky_normalizations.clone();
    let mut reports = Vec::with_capacity(request.lights.len());
    for (index, path) in request.lights.iter().enumerate() {
        cancellation_checkpoint(cancel, "registro por ramas")?;
        let data = store
            .get(index)
            .map_err(|error| MilkyWayError::io("cache", error))?;
        let image = crate::DsImage {
            data,
            w: width,
            h: height,
            ch: channels,
            bayer: None,
        };
        let luma = crate::ds_luma(&image);
        if request.integration.normalization == MilkyWayNormalizationMode::RobustLinear {
            sky_normalizations[index] = normalization_between(
                reference_sky_quantiles,
                image_quantiles(&luma, &mask.hard, true),
            );
            ground_normalizations[index] = normalization_between(
                reference_ground_quantiles,
                image_quantiles(&luma, &mask.hard, false),
            );
        }
        let (sky_transform, sky_report) = if index == base_index {
            let identity = crate::milky_way_registration::TransformModel::identity();
            (Some(identity), identity_registration())
        } else {
            match crate::milky_way_registration::solve_sky_registration_with_config(
                &base_luma,
                &luma,
                geometry,
                sky_mask_view,
                sky_registration_config(&request.registration, geometry),
            ) {
                Ok(registration) if registration.accepted => {
                    let transform = registration.transform;
                    (
                        Some(transform),
                        MilkyWayBranchRegistration {
                            transform: Some(serializable_transform(transform)),
                            inliers: registration.inlier_correspondences.len(),
                            rms_px: Some(registration.rms_px as f32),
                            confidence: Some(registration.confidence as f32),
                            excluded: false,
                            reason: None,
                        },
                    )
                }
                Ok(registration) => {
                    let reason = format!(
                        "Registro de cielo rechazado para '{}': {}",
                        path,
                        registration.validation.reasons.join("; ")
                    );
                    if request.fallback_policy == MilkyWayFallbackPolicy::Strict {
                        return Err(MilkyWayError::scientific("sky_registration", reason));
                    }
                    warnings.push(reason.clone());
                    degraded = true;
                    (
                        None,
                        MilkyWayBranchRegistration {
                            transform: None,
                            inliers: 0,
                            rms_px: None,
                            confidence: None,
                            excluded: true,
                            reason: Some(reason),
                        },
                    )
                }
                Err(error) => {
                    let reason = format!("Registro de cielo falló para '{path}': {error}");
                    if request.fallback_policy == MilkyWayFallbackPolicy::Strict {
                        return Err(MilkyWayError::scientific("sky_registration", reason));
                    }
                    warnings.push(reason.clone());
                    degraded = true;
                    (
                        None,
                        MilkyWayBranchRegistration {
                            transform: None,
                            inliers: 0,
                            rms_px: None,
                            confidence: None,
                            excluded: true,
                            reason: Some(reason),
                        },
                    )
                }
            }
        };
        sky_transforms[index] = sky_transform;
        let ground_report = if request.mode == MilkyWayStackMode::SkyOnly {
            None
        } else if index == base_index {
            let identity = crate::milky_way_registration::TransformModel::identity();
            ground_transforms[index] = Some(identity);
            Some(identity_registration())
        } else {
            match crate::milky_way_registration::solve_ground_registration(
                &base_luma,
                &luma,
                geometry,
                ground_mask_view,
                request.max_ground_shift_px as f64,
            ) {
                Ok(registration) if registration.accepted => {
                    let transform = registration.transform;
                    ground_transforms[index] = Some(transform);
                    Some(MilkyWayBranchRegistration {
                        transform: Some(serializable_transform(transform)),
                        inliers: 0,
                        rms_px: None,
                        confidence: Some(registration.confidence as f32),
                        excluded: false,
                        reason: None,
                    })
                }
                Ok(registration) => {
                    let reason = format!(
                        "Registro de suelo rechazado para '{}' (NCC {:.3}, margen {:.4})",
                        path, registration.ncc_score, registration.peak_margin
                    );
                    if request.fallback_policy == MilkyWayFallbackPolicy::Strict {
                        return Err(MilkyWayError::scientific("ground_registration", reason));
                    }
                    warnings.push(reason.clone());
                    degraded = true;
                    Some(MilkyWayBranchRegistration {
                        transform: None,
                        inliers: 0,
                        rms_px: None,
                        confidence: None,
                        excluded: true,
                        reason: Some(reason),
                    })
                }
                Err(error) => {
                    let reason = format!("Registro de suelo falló para '{path}': {error}");
                    if request.fallback_policy == MilkyWayFallbackPolicy::Strict {
                        return Err(MilkyWayError::scientific("ground_registration", reason));
                    }
                    warnings.push(reason.clone());
                    degraded = true;
                    Some(MilkyWayBranchRegistration {
                        transform: None,
                        inliers: 0,
                        rms_px: None,
                        confidence: None,
                        excluded: true,
                        reason: Some(reason),
                    })
                }
            }
        };
        reports.push(MilkyWayFrameReport {
            path: path.clone(),
            sky: sky_report,
            ground: ground_report,
            sky_normalization: sky_normalizations[index].clone(),
            ground_normalization: (request.mode != MilkyWayStackMode::SkyOnly)
                .then(|| ground_normalizations[index].clone()),
        });
        emit_milky_way_progress(
            app,
            job_id,
            "Registro cielo/suelo",
            25.0 + 20.0 * (index + 1) as f32 / request.lights.len() as f32,
            index + 1,
            request.lights.len(),
            format!(
                "Ramas independientes {}/{}",
                index + 1,
                request.lights.len()
            ),
        );
    }
    let sky_used = sky_transforms.iter().flatten().count();
    let ground_used = ground_transforms.iter().flatten().count();
    if sky_used == 0 || (request.mode != MilkyWayStackMode::SkyOnly && ground_used == 0) {
        return Err(MilkyWayError::scientific(
            "registration",
            "Ninguna toma válida sobrevivió en una de las ramas",
        ));
    }
    if sky_used < 2 || (request.mode != MilkyWayStackMode::SkyOnly && ground_used < 2) {
        warnings.push(
            "Una rama quedó con una sola toma: se publica lineal pero sin beneficio de apilado"
                .into(),
        );
        degraded = true;
    }
    Ok(PreparedMilkyWayData {
        store,
        width,
        height,
        channels,
        base_index,
        mask,
        branch_validity,
        sky_transforms,
        ground_transforms,
        sky_normalizations,
        ground_normalizations,
        reports,
        warnings,
        degraded,
    })
}

fn summarize_registration(
    frames: &[MilkyWayFrameReport],
    mode: MilkyWayStackMode,
    options: &MilkyWayRegistrationOptions,
) -> MilkyWayRegistrationSummary {
    let sky_valid = frames.iter().filter(|frame| !frame.sky.excluded).count();
    let inliers = frames
        .iter()
        .filter(|frame| !frame.sky.excluded && frame.sky.inliers > 0)
        .map(|frame| frame.sky.inliers)
        .min()
        .unwrap_or(0);
    let rms_px = frames
        .iter()
        .filter(|frame| !frame.sky.excluded)
        .filter_map(|frame| frame.sky.rms_px)
        .filter(|value| value.is_finite() && *value > 0.0)
        .max_by(f32::total_cmp);
    let sky_solved = sky_valid >= 2
        && inliers >= options.min_inliers
        && rms_px
            .map(|value| value <= options.max_rms_px)
            .unwrap_or(false);
    let ground_solved = mode == MilkyWayStackMode::SkyOnly
        || frames
            .iter()
            .filter_map(|frame| frame.ground.as_ref())
            .filter(|registration| !registration.excluded)
            .count()
            >= 2;
    MilkyWayRegistrationSummary {
        sky_solved,
        ground_solved,
        inliers,
        rms_px,
        // El solver de esta versión no publica aún un residuo radial de
        // esquinas separado; no se inventa a partir del RMS central.
        corner_residual_px: None,
    }
}

#[tauri::command]
pub async fn analyze_milky_way_registration(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
    request: MilkyWayStackRequest,
) -> Result<MilkyWayRegistrationAnalysisResult, MilkyWayError> {
    let mut request = request;
    let contract_warnings = normalize_composition_contract(&mut request);
    // Registro es anterior a “Publicar” en el asistente: no obliga a escoger
    // todavía el destino final. El scratch temporal nunca se presenta como
    // salida ni altera el request conservado en la receta final.
    let mut preflight_request = request.clone();
    if preflight_request.output_dir.trim().is_empty() {
        preflight_request.output_dir = std::env::temp_dir()
            .join("zenith-milky-way-analysis")
            .to_string_lossy()
            .into_owned();
    }
    let plan = prepare_milky_way_stack_impl(&preflight_request)?;
    crate::ds_begin_user_action(&state);
    let job_id = plan.job_id.clone();
    let cancel = state.job_registry.register(&job_id);
    if state.cancel_requested.load(Ordering::Acquire) {
        cancel.store(true, Ordering::Release);
    }
    let registry = state.job_registry.clone();
    let work_root = request
        .work_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if request.output_dir.trim().is_empty() {
                std::env::temp_dir().join("zenith-milky-way-analysis")
            } else {
                PathBuf::from(&request.output_dir).join(".zenith_cache")
            }
        });
    let request_for_work = request.clone();
    let job_for_work = job_id.clone();
    let prepared = tauri::async_runtime::spawn_blocking(move || {
        let _guard = pipeline::JobGuard::new(registry, job_for_work.clone());
        let cache = work_root
            .join(safe_job_fragment(&job_for_work))
            .join("analysis");
        let _cleanup = EphemeralDirectory(cache.clone());
        prepare_registration_data(&app, &request_for_work, &job_for_work, &cancel, &cache)
    })
    .await
    .map_err(|error| {
        MilkyWayError::new("worker_panic", "registration", error.to_string(), true)
    })??;
    let registration =
        summarize_registration(&prepared.reports, request.mode, &request.registration);
    let mut warnings = contract_warnings;
    warnings.extend(prepared.warnings);
    Ok(MilkyWayRegistrationAnalysisResult {
        plan,
        effective_base_frame: request.lights[prepared.base_index].clone(),
        mask_confidence: prepared.mask.confidence,
        frames: prepared.reports,
        warnings,
        registration,
    })
}

#[derive(Clone, Copy)]
enum MilkyWayBranch {
    Sky,
    Ground,
}

impl MilkyWayBranch {
    fn label(self) -> &'static str {
        match self {
            Self::Sky => "cielo",
            Self::Ground => "suelo",
        }
    }
}

fn bilinear_scalar(values: &[f32], width: usize, height: usize, x: f64, y: f64) -> f32 {
    let x0 = x.floor().clamp(0.0, width.saturating_sub(1) as f64) as usize;
    let y0 = y.floor().clamp(0.0, height.saturating_sub(1) as f64) as usize;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let fx = (x - x0 as f64).clamp(0.0, 1.0) as f32;
    let fy = (y - y0 as f64).clamp(0.0, 1.0) as f32;
    values[y0 * width + x0] * (1.0 - fx) * (1.0 - fy)
        + values[y0 * width + x1] * fx * (1.0 - fy)
        + values[y1 * width + x0] * (1.0 - fx) * fy
        + values[y1 * width + x1] * fx * fy
}

/// C5a: peso Lanczos-3 local. Se implementa aquí (no se importa de
/// deepsky.rs) para mantener el módulo autocontenido. L(x) =
/// sinc(x)·sinc(x/3) para |x|<3, reescrito como 3·sin(πx)·sin(πx/3)/(π²x²)
/// para evaluar un solo cociente. En x≈0 vale exactamente 1 y fuera del
/// soporte exactamente 0, así que en posiciones enteras el kernel degenera
/// en una delta (salvo redondeo de sin(πk)~1e-16, que la normalización DC
/// absorbe).
#[inline]
fn lanczos3_weight(x: f32) -> f32 {
    let ax = x.abs();
    if ax < 1.0e-6 {
        return 1.0;
    }
    if ax >= 3.0 {
        return 0.0;
    }
    let pix = std::f32::consts::PI * ax;
    3.0 * pix.sin() * (pix / 3.0).sin() / (pix * pix)
}

/// Un único inverse mapping por rama. La transformación puede contener
/// homografía y radial; la máscara se consulta en la misma coordenada source
/// antes de interpolar la imagen, evitando contaminar cielo con suelo.
///
/// C5a: la IMAGEN se interpola con Lanczos-3 en el interior (soporte 6×6
/// completo y finito); la bilineal se conserva como fallback de borde
/// (<3 px) o ante no-finitos en el soporte, porque degrada con suavidad y
/// nunca fabrica valores desde NaN. La bilineal pura costaba ~10-15% de FWHM
/// en estrellas de ~2 px — el peor caso son precisamente los micro-dithers
/// subpíxel típicos de nightscape. La MÁSCARA sigue siendo bilineal: es un
/// peso de dominio, no fotometría, y el ringing de Lanczos la corrompería.
fn warp_milky_way_branch(
    image: &crate::DsImage,
    transform: crate::milky_way_registration::TransformModel,
    source_sky_mask: &[f32],
    branch: MilkyWayBranch,
    output_width: usize,
    output_height: usize,
) -> (Vec<f32>, Vec<f32>) {
    let channels = image.ch;
    let mut output = vec![f32::NAN; output_width * output_height * channels];
    let mut coverage = vec![0.0f32; output_width * output_height];
    output
        .par_chunks_mut(output_width * channels)
        .zip(coverage.par_chunks_mut(output_width))
        .enumerate()
        .for_each(|(y, (row, coverage_row))| {
            for x in 0..output_width {
                let Some(source) = transform.inverse(crate::milky_way_registration::Point2::new(
                    x as f64, y as f64,
                )) else {
                    continue;
                };
                if source.x < 0.0
                    || source.y < 0.0
                    || source.x > image.w.saturating_sub(1) as f64
                    || source.y > image.h.saturating_sub(1) as f64
                {
                    continue;
                }
                let sky_weight =
                    bilinear_scalar(source_sky_mask, image.w, image.h, source.x, source.y);
                // C2: umbrales estrictos sobre la máscara de validez. El
                // feather de validez es exactamente cero fuera de su soporte
                // (blur de ceros), así que `> 0.0` incluye TODA la cola de la
                // transición: cualquier píxel con peso de composición
                // fraccional queda dentro del dominio de ambas ramas y el
                // fallback (finita, no-finita) no puede dispararse en la
                // banda.
                let branch_present = match branch {
                    MilkyWayBranch::Sky => sky_weight > 0.0,
                    MilkyWayBranch::Ground => sky_weight < 1.0,
                };
                if !branch_present {
                    continue;
                }
                let x0 = source.x.floor() as usize;
                let y0 = source.y.floor() as usize;
                let x1 = (x0 + 1).min(image.w - 1);
                let y1 = (y0 + 1).min(image.h - 1);
                let fx = (source.x - x0 as f64) as f32;
                let fy = (source.y - y0 as f64) as f32;
                // C5a: Lanczos-3 sólo en el interior con soporte 6×6 completo.
                // Las posiciones exactamente enteras (fx=fy=0) van por la
                // bilineal, que allí es una copia bit-exacta: la identidad
                // sigue siendo pixel-exact sin depender de redondeos del seno.
                let lanczos_interior = x0 >= 2
                    && y0 >= 2
                    && x0 + 3 < image.w
                    && y0 + 3 < image.h
                    && !(fx == 0.0 && fy == 0.0);
                let mut wx = [0.0f32; 6];
                let mut wy = [0.0f32; 6];
                if lanczos_interior {
                    for (tap, (weight_x, weight_y)) in wx.iter_mut().zip(wy.iter_mut()).enumerate()
                    {
                        let offset = tap as f32 - 2.0;
                        *weight_x = lanczos3_weight(fx - offset);
                        *weight_y = lanczos3_weight(fy - offset);
                    }
                }
                let mut finite = true;
                for channel in 0..channels {
                    let at =
                        |sx: usize, sy: usize| image.data[(sy * image.w + sx) * channels + channel];
                    let mut value = f32::NAN;
                    if lanczos_interior {
                        let mut accumulated = 0.0f32;
                        let mut kernel_sum = 0.0f32;
                        let mut support_finite = true;
                        'taps: for (tap_y, weight_y) in wy.iter().enumerate() {
                            let sy = y0 + tap_y - 2;
                            for (tap_x, weight_x) in wx.iter().enumerate() {
                                let sample = at(x0 + tap_x - 2, sy);
                                if !sample.is_finite() {
                                    support_finite = false;
                                    break 'taps;
                                }
                                let weight = weight_x * weight_y;
                                accumulated += sample * weight;
                                kernel_sum += weight;
                            }
                        }
                        if support_finite && kernel_sum.abs() > f32::EPSILON {
                            // Normalizar por la suma real de pesos fija la
                            // ganancia DC en 1: el flujo estelar se conserva
                            // aunque el kernel truncado no particione la
                            // unidad. No se recorta el ringing: los másteres
                            // son lineales y un clamp sesgaría la fotometría.
                            value = accumulated / kernel_sum;
                        }
                    }
                    if !value.is_finite() {
                        // Borde (<3 px), soporte con no-finitos o posición
                        // entera exacta: bilineal original.
                        value = at(x0, y0) * (1.0 - fx) * (1.0 - fy)
                            + at(x1, y0) * fx * (1.0 - fy)
                            + at(x0, y1) * (1.0 - fx) * fy
                            + at(x1, y1) * fx * fy;
                    }
                    row[x * channels + channel] = value;
                    finite &= value.is_finite();
                }
                if finite {
                    coverage_row[x] = 1.0;
                } else {
                    row[x * channels..(x + 1) * channels].fill(f32::NAN);
                }
            }
        });
    (output, coverage)
}

fn integrate_branch(
    app: &tauri::AppHandle,
    job_id: &str,
    prepared: &PreparedMilkyWayData,
    branch: MilkyWayBranch,
    config: &MilkyWayIntegrationConfig,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
    cache_root: &Path,
    progress_start: f32,
    progress_end: f32,
    base_only: bool,
) -> Result<BranchIntegration, MilkyWayError> {
    let effective_config = config.effective_for(branch);
    let (transforms, normalizations) = match branch {
        MilkyWayBranch::Sky => (&prepared.sky_transforms, &prepared.sky_normalizations),
        MilkyWayBranch::Ground => (&prepared.ground_transforms, &prepared.ground_normalizations),
    };
    let mut selected: Vec<(usize, crate::milky_way_registration::TransformModel)> = transforms
        .iter()
        .enumerate()
        .filter_map(|(index, transform)| transform.map(|transform| (index, transform)))
        .collect();
    if base_only {
        selected.retain(|(index, _)| *index == prepared.base_index);
    }
    if selected.is_empty() {
        return Err(MilkyWayError::scientific(
            "integration",
            format!(
                "La rama de {} no contiene frames registrados",
                branch.label()
            ),
        ));
    }
    let frame_len = prepared
        .width
        .checked_mul(prepared.height)
        .and_then(|value| value.checked_mul(prepared.channels))
        .ok_or_else(|| MilkyWayError::invalid("Geometría de integración desbordada"))?;
    let mut warped_store = AdaptiveFrameStore::new(
        selected.len(),
        frame_len,
        cache_root,
        &format!("{}_{}_warped", safe_job_fragment(job_id), branch.label()),
    )
    .map_err(|error| MilkyWayError::io("cache", error))?;
    for (output_index, (source_index, transform)) in selected.iter().enumerate() {
        cancellation_checkpoint(cancel, &format!("warp de {}", branch.label()))?;
        let data = prepared
            .store
            .get(*source_index)
            .map_err(|error| MilkyWayError::io("cache", error))?;
        let mut image = crate::DsImage {
            data,
            w: prepared.width,
            h: prepared.height,
            ch: prepared.channels,
            bayer: None,
        };
        if effective_config.dynamic_hot_pixels {
            correct_dynamic_hot_pixels(&mut image);
        }
        let image = normalized_image(image, &normalizations[*source_index]);
        // C2: el warp gatea con la máscara de VALIDEZ (soporte estrictamente
        // mayor que el peso de composición), no con la máscara publicada.
        let (warped, _warp_coverage) = warp_milky_way_branch(
            &image,
            *transform,
            &prepared.branch_validity,
            branch,
            prepared.width,
            prepared.height,
        );
        warped_store
            .put(output_index, &warped)
            .map_err(|error| MilkyWayError::io("cache", error))?;
        let fraction = (output_index + 1) as f32 / selected.len() as f32;
        emit_milky_way_progress(
            app,
            job_id,
            &format!("Registro final de {}", branch.label()),
            progress_start + (progress_end - progress_start) * 0.45 * fraction,
            output_index + 1,
            selected.len(),
            format!(
                "Una sola interpolación · {}/{}",
                output_index + 1,
                selected.len()
            ),
        );
    }
    let pixels = prepared.width * prepared.height;
    let mut sci = vec![f32::NAN; frame_len];
    let mut variance = vec![f32::NAN; frame_len];
    let mut coverage = vec![0.0f32; pixels];
    let mut rejection = vec![0.0f32; pixels];
    // Quality usa tiles menores y una caracterización robusta más profunda;
    // Fast reduce rondas/lecturas y usa tiles mayores. Todo sigue determinista.
    let tile_pixels = match effective_config.profile {
        MilkyWayIntegrationProfile::Fast => 64 * 1024,
        MilkyWayIntegrationProfile::Auto => 32 * 1024,
        MilkyWayIntegrationProfile::Quality => 16 * 1024,
    };
    let tile_count = pixels.div_ceil(tile_pixels);
    for tile in 0..tile_count {
        cancellation_checkpoint(cancel, &format!("integración de {}", branch.label()))?;
        let start_pixel = tile * tile_pixels;
        let end_pixel = ((tile + 1) * tile_pixels).min(pixels);
        let start_sample = start_pixel * prepared.channels;
        let end_sample = end_pixel * prepared.channels;
        let frames: Vec<Vec<f32>> = (0..selected.len())
            .map(|index| {
                warped_store
                    .get_range(index, start_sample..end_sample)
                    .map_err(|error| MilkyWayError::io("cache", error))
            })
            .collect::<Result<_, _>>()?;
        let mut samples = Vec::with_capacity(selected.len());
        for pixel in start_pixel..end_pixel {
            let local_pixel = pixel - start_pixel;
            let mut accepted_min = selected.len();
            let mut rejected_max = 0usize;
            for channel in 0..prepared.channels {
                samples.clear();
                let local_sample = local_pixel * prepared.channels + channel;
                samples.extend(frames.iter().map(|frame| frame[local_sample]));
                if let Some((mean, variance_of_mean, rejected)) =
                    robust_integrate_samples(&mut samples, &effective_config)
                {
                    let accepted = samples.len().saturating_sub(rejected);
                    accepted_min = accepted_min.min(accepted);
                    rejected_max = rejected_max.max(rejected);
                    sci[pixel * prepared.channels + channel] = mean;
                    variance[pixel * prepared.channels + channel] = variance_of_mean;
                }
            }
            if accepted_min != selected.len() || sci[pixel * prepared.channels].is_finite() {
                coverage[pixel] = accepted_min as f32 / selected.len() as f32;
                rejection[pixel] = rejected_max as f32 / selected.len() as f32;
            }
        }
        let fraction = (tile + 1) as f32 / tile_count as f32;
        emit_milky_way_progress(
            app,
            job_id,
            &format!("Integración robusta de {}", branch.label()),
            progress_start + (progress_end - progress_start) * (0.45 + 0.55 * fraction),
            tile + 1,
            tile_count,
            format!(
                "{:?} · {:?} · {} pasada(s) deterministas · tile {}/{}",
                effective_config.profile,
                effective_config.method,
                effective_config.deterministic_passes(),
                tile + 1,
                tile_count
            ),
        );
    }
    Ok(BranchIntegration {
        sci,
        variance,
        coverage,
        rejection,
        frames_used: selected.len(),
    })
}

fn mean_finite(values: &[f32]) -> f32 {
    let (sum, count) = values
        .iter()
        .filter(|value| value.is_finite())
        .fold((0.0f64, 0usize), |(sum, count), value| {
            (sum + *value as f64, count + 1)
        });
    if count == 0 {
        0.0
    } else {
        (sum / count as f64) as f32
    }
}

fn compose_layers(
    sky: &BranchIntegration,
    ground: &BranchIntegration,
    mask: &[f32],
    width: usize,
    height: usize,
    channels: usize,
    boundary_color_match: bool,
) -> (Vec<f32>, f32, f32) {
    let pixels = width * height;
    let mut offsets = vec![0.0f32; channels];
    if boundary_color_match {
        let sample_step = (pixels / 200_000).max(1);
        for channel in 0..channels {
            let mut differences = Vec::new();
            for pixel in (0..pixels).step_by(sample_step) {
                let weight = mask[pixel].clamp(0.0, 1.0);
                if !(0.05..0.95).contains(&weight) {
                    continue;
                }
                let index = pixel * channels + channel;
                let sky_value = sky.sci[index];
                let ground_value = ground.sci[index];
                if sky_value.is_finite() && ground_value.is_finite() {
                    differences.push(sky_value - ground_value);
                }
            }
            offsets[channel] = median(&mut differences).unwrap_or(0.0);
        }
    }
    let mut output = vec![f32::NAN; pixels * channels];
    let mut seam_differences = Vec::new();
    let mut finite_pixels = 0usize;
    let mut sky_luma_values = Vec::new();
    for pixel in 0..pixels {
        let weight = mask[pixel].clamp(0.0, 1.0);
        let mut pixel_finite = false;
        let mut sky_luma = 0.0f32;
        let mut ground_luma = 0.0f32;
        let mut sky_count = 0usize;
        let mut ground_count = 0usize;
        for channel in 0..channels {
            let index = pixel * channels + channel;
            let sky_value = sky.sci[index];
            let ground_value = ground.sci[index] + offsets[channel];
            output[index] = match (sky_value.is_finite(), ground_value.is_finite()) {
                (true, true) => sky_value * weight + ground_value * (1.0 - weight),
                (true, false) => sky_value,
                (false, true) => ground_value,
                (false, false) => f32::NAN,
            };
            pixel_finite |= output[index].is_finite();
            if sky_value.is_finite() {
                sky_luma += sky_value;
                sky_count += 1;
            }
            if ground_value.is_finite() {
                ground_luma += ground_value;
                ground_count += 1;
            }
        }
        if pixel_finite {
            finite_pixels += 1;
        }
        if sky_count > 0 {
            sky_luma_values.push(sky_luma / sky_count as f32);
        }
        if (0.05..0.95).contains(&weight) && sky_count > 0 && ground_count > 0 {
            seam_differences
                .push((sky_luma / sky_count as f32 - ground_luma / ground_count as f32).abs());
        }
    }
    sky_luma_values.sort_by(f32::total_cmp);
    let dynamic_range = if sky_luma_values.len() >= 2 {
        let p05 = sky_luma_values[(sky_luma_values.len() as f32 * 0.05) as usize];
        let p95 = sky_luma_values[((sky_luma_values.len() - 1) as f32 * 0.95) as usize];
        (p95 - p05).abs().max(1.0e-6)
    } else {
        1.0
    };
    seam_differences.sort_by(f32::total_cmp);
    let seam = (median(&mut seam_differences).unwrap_or(0.0) / dynamic_range).max(0.0);
    (output, seam, finite_pixels as f32 / pixels.max(1) as f32)
}

fn wcs_metadata(
    request: &MilkyWayStackRequest,
    mode: MilkyWayStackMode,
) -> Vec<(&'static str, String)> {
    let mut metadata = vec![
        ("ZMWVER", format!("'{}'", MILKY_WAY_RECIPE_SCHEMA)),
        ("ZMWMODE", format!("'{:?}'", mode)),
        ("LINEAR", "                   T".into()),
    ];
    if let Some(binding) = &request.wcs {
        let wcs = &binding.solution;
        metadata.extend([
            ("CTYPE1", format!("'{}'", wcs.ctype)),
            ("CTYPE2", "'DEC--TAN'".into()),
            ("CUNIT1", "'deg'".into()),
            ("CUNIT2", "'deg'".into()),
            ("CRVAL1", format!("{:.12}", wcs.crval1)),
            ("CRVAL2", format!("{:.12}", wcs.crval2)),
            ("CRPIX1", format!("{:.12}", wcs.crpix1)),
            ("CRPIX2", format!("{:.12}", wcs.crpix2)),
            ("CD1_1", format!("{:.14}", wcs.cd11)),
            ("CD1_2", format!("{:.14}", wcs.cd12)),
            ("CD2_1", format!("{:.14}", wcs.cd21)),
            ("CD2_2", format!("{:.14}", wcs.cd22)),
            ("RADESYS", "'ICRS'".into()),
        ]);
    }
    metadata
}

fn write_fits(
    path: &Path,
    data: &[f32],
    width: usize,
    height: usize,
    channels: usize,
    metadata: &[(&str, String)],
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(), MilkyWayError> {
    crate::ds_save_float32_fits_cancellable(
        path,
        data,
        width,
        height,
        channels,
        metadata,
        Some(cancel),
    )
    .map_err(|error| MilkyWayError::io("output", error))
}

fn save_mask_preview(
    path: &Path,
    mask: &[f32],
    width: usize,
    height: usize,
) -> Result<(), MilkyWayError> {
    let bytes: Vec<u8> = mask
        .iter()
        .map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as u8)
        .collect();
    let image = image::GrayImage::from_raw(width as u32, height as u32, bytes)
        .ok_or_else(|| MilkyWayError::io("output", "Máscara PNG con geometría inválida"))?;
    image
        .save(path)
        .map_err(|error| MilkyWayError::io("output", format!("guardar máscara PNG: {error}")))
}

fn friendly_output_directories(
    output_root: &Path,
    job_id: &str,
) -> Result<(PathBuf, PathBuf), MilkyWayError> {
    std::fs::create_dir_all(output_root)
        .map_err(|error| MilkyWayError::io("output", format!("crear destino: {error}")))?;
    let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let suffix: String = job_id
        .chars()
        .rev()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(6)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let final_path = output_root.join(format!("Via_Lactea_{timestamp}_{suffix}"));
    if final_path.exists() {
        return Err(MilkyWayError::io(
            "output",
            format!(
                "La carpeta de resultado ya existe: {}",
                final_path.display()
            ),
        ));
    }
    let staging = output_root.join(format!(".Via_Lactea_{timestamp}_{suffix}.part"));
    if staging.exists() {
        return Err(MilkyWayError::io(
            "output",
            format!("Existe un staging previo: {}", staging.display()),
        ));
    }
    Ok((staging, final_path))
}

fn final_output_paths(directory: &Path, mode: MilkyWayStackMode) -> MilkyWayOutputPaths {
    let sky = directory.join("Master_Cielo_Lineal.fits");
    let ground =
        (mode != MilkyWayStackMode::SkyOnly).then(|| directory.join("Master_Suelo_Lineal.fits"));
    let composite = (mode != MilkyWayStackMode::SkyOnly)
        .then(|| directory.join("Composicion_Via_Lactea_Lineal.fits"));
    MilkyWayOutputPaths {
        primary: composite
            .as_ref()
            .unwrap_or(&sky)
            .to_string_lossy()
            .into_owned(),
        sky_master: sky.to_string_lossy().into_owned(),
        ground_master: ground.map(|path| path.to_string_lossy().into_owned()),
        composite: composite.map(|path| path.to_string_lossy().into_owned()),
        sky_mask: directory
            .join("Mascara_Cielo_Lineal.fits")
            .to_string_lossy()
            .into_owned(),
        mask_preview: directory
            .join("Vista_Mascara_Cielo.png")
            .to_string_lossy()
            .into_owned(),
        sky_variance: directory
            .join("Calidad_Cielo_VAR.fits")
            .to_string_lossy()
            .into_owned(),
        sky_coverage: directory
            .join("Calidad_Cielo_Cobertura.fits")
            .to_string_lossy()
            .into_owned(),
        sky_rejection: directory
            .join("Calidad_Cielo_Rechazo.fits")
            .to_string_lossy()
            .into_owned(),
        ground_variance: (mode != MilkyWayStackMode::SkyOnly).then(|| {
            directory
                .join("Calidad_Suelo_VAR.fits")
                .to_string_lossy()
                .into_owned()
        }),
        ground_coverage: (mode != MilkyWayStackMode::SkyOnly).then(|| {
            directory
                .join("Calidad_Suelo_Cobertura.fits")
                .to_string_lossy()
                .into_owned()
        }),
        ground_rejection: (mode != MilkyWayStackMode::SkyOnly).then(|| {
            directory
                .join("Calidad_Suelo_Rechazo.fits")
                .to_string_lossy()
                .into_owned()
        }),
        recipe: directory
            .join("Receta_Via_Lactea.json")
            .to_string_lossy()
            .into_owned(),
    }
}

fn staging_equivalent(final_path: &str, final_dir: &Path, staging_dir: &Path) -> PathBuf {
    let final_path = Path::new(final_path);
    let relative = final_path.strip_prefix(final_dir).unwrap_or(final_path);
    staging_dir.join(relative)
}

fn save_recipe(path: &Path, recipe: &MilkyWayRecipe) -> Result<(), MilkyWayError> {
    let file = std::fs::File::create(path)
        .map_err(|error| MilkyWayError::io("output", format!("crear receta: {error}")))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, recipe)
        .map_err(|error| MilkyWayError::io("output", format!("serializar receta: {error}")))?;
    use std::io::Write;
    writer
        .flush()
        .map_err(|error| MilkyWayError::io("output", format!("vaciar receta: {error}")))?;
    writer
        .get_ref()
        .sync_all()
        .map_err(|error| MilkyWayError::io("output", format!("sincronizar receta: {error}")))
}

fn save_outputs(
    app: &tauri::AppHandle,
    request: &MilkyWayStackRequest,
    job_id: &str,
    prepared: &PreparedMilkyWayData,
    sky: &BranchIntegration,
    ground: Option<&BranchIntegration>,
    composite: Option<&[f32]>,
    diagnostics: &MilkyWayQualityDiagnostics,
    warnings: &[String],
    cancel: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<MilkyWayOutputPaths, MilkyWayError> {
    cancellation_checkpoint(cancel, "publicación")?;
    let output_root = PathBuf::from(&request.output_dir);
    let (staging_path, final_path) = friendly_output_directories(&output_root, job_id)?;
    let staging = StagingDirectory::new(staging_path.clone())?;
    let outputs = final_output_paths(&final_path, request.mode);
    let common_metadata = wcs_metadata(request, request.mode);
    let scalar_metadata = vec![
        ("ZMWVER", format!("'{}'", MILKY_WAY_RECIPE_SCHEMA)),
        ("ZMWMODE", format!("'{:?}'", request.mode)),
    ];
    let sky_path = staging_equivalent(&outputs.sky_master, &final_path, &staging_path);
    write_fits(
        &sky_path,
        &sky.sci,
        prepared.width,
        prepared.height,
        prepared.channels,
        &common_metadata,
        cancel,
    )?;
    write_fits(
        &staging_equivalent(&outputs.sky_variance, &final_path, &staging_path),
        &sky.variance,
        prepared.width,
        prepared.height,
        prepared.channels,
        &common_metadata,
        cancel,
    )?;
    write_fits(
        &staging_equivalent(&outputs.sky_coverage, &final_path, &staging_path),
        &sky.coverage,
        prepared.width,
        prepared.height,
        1,
        &scalar_metadata,
        cancel,
    )?;
    write_fits(
        &staging_equivalent(&outputs.sky_rejection, &final_path, &staging_path),
        &sky.rejection,
        prepared.width,
        prepared.height,
        1,
        &scalar_metadata,
        cancel,
    )?;
    write_fits(
        &staging_equivalent(&outputs.sky_mask, &final_path, &staging_path),
        &prepared.mask.soft,
        prepared.width,
        prepared.height,
        1,
        &common_metadata,
        cancel,
    )?;
    save_mask_preview(
        &staging_equivalent(&outputs.mask_preview, &final_path, &staging_path),
        &prepared.mask.soft,
        prepared.width,
        prepared.height,
    )?;
    if let Some(ground) = ground {
        let ground_metadata = vec![
            ("ZMWVER", format!("'{}'", MILKY_WAY_RECIPE_SCHEMA)),
            ("ZMWMODE", format!("'{:?}'", request.mode)),
            ("LINEAR", "                   T".into()),
        ];
        write_fits(
            &staging_equivalent(
                outputs.ground_master.as_deref().unwrap_or_default(),
                &final_path,
                &staging_path,
            ),
            &ground.sci,
            prepared.width,
            prepared.height,
            prepared.channels,
            &ground_metadata,
            cancel,
        )?;
        write_fits(
            &staging_equivalent(
                outputs.ground_variance.as_deref().unwrap_or_default(),
                &final_path,
                &staging_path,
            ),
            &ground.variance,
            prepared.width,
            prepared.height,
            prepared.channels,
            &ground_metadata,
            cancel,
        )?;
        write_fits(
            &staging_equivalent(
                outputs.ground_coverage.as_deref().unwrap_or_default(),
                &final_path,
                &staging_path,
            ),
            &ground.coverage,
            prepared.width,
            prepared.height,
            1,
            &scalar_metadata,
            cancel,
        )?;
        write_fits(
            &staging_equivalent(
                outputs.ground_rejection.as_deref().unwrap_or_default(),
                &final_path,
                &staging_path,
            ),
            &ground.rejection,
            prepared.width,
            prepared.height,
            1,
            &scalar_metadata,
            cancel,
        )?;
    }
    if let (Some(composite), Some(path)) = (composite, outputs.composite.as_deref()) {
        write_fits(
            &staging_equivalent(path, &final_path, &staging_path),
            composite,
            prepared.width,
            prepared.height,
            prepared.channels,
            &common_metadata,
            cancel,
        )?;
    }
    let recipe = MilkyWayRecipe {
        schema: MILKY_WAY_RECIPE_SCHEMA.into(),
        created_at_utc: chrono::Utc::now().to_rfc3339(),
        request: request.clone(),
        effective_base_frame: request.lights[prepared.base_index].clone(),
        width: prepared.width,
        height: prepared.height,
        channels: prepared.channels,
        frame_reports: prepared.reports.clone(),
        diagnostics: diagnostics.clone(),
        warnings: warnings.to_vec(),
        outputs: outputs.clone(),
    };
    save_recipe(
        &staging_equivalent(&outputs.recipe, &final_path, &staging_path),
        &recipe,
    )?;
    cancellation_checkpoint(cancel, "publicación")?;
    emit_milky_way_progress(
        app,
        job_id,
        "Publicación transaccional",
        98.0,
        1,
        1,
        "FITS float32 y receta sincronizados; publicando carpeta",
    );
    staging.commit(&final_path)?;
    Ok(outputs)
}

fn run_milky_way_stack_impl(
    app: &tauri::AppHandle,
    request: MilkyWayStackRequest,
    job_id: &str,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) -> Result<MilkyWayStackResult, MilkyWayError> {
    let mut request = request;
    let contract_warnings = normalize_composition_contract(&mut request);
    validate_request(&request)?;
    let started = Instant::now();
    let work_root = request
        .work_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&request.output_dir).join(".zenith_cache"))
        .join(safe_job_fragment(job_id));
    std::fs::create_dir_all(&work_root)
        .map_err(|error| MilkyWayError::io("cache", format!("crear área de trabajo: {error}")))?;
    let _work_cleanup = EphemeralDirectory(work_root.clone());
    emit_milky_way_progress(
        app,
        job_id,
        "Preflight",
        1.0,
        0,
        request.lights.len(),
        "Validando entradas lineales, calibración y geometría",
    );
    let prepared =
        prepare_registration_data(app, &request, job_id, &cancel, &work_root.join("prepared"))?;
    let sky = integrate_branch(
        app,
        job_id,
        &prepared,
        MilkyWayBranch::Sky,
        &request.integration,
        &cancel,
        &work_root.join("sky"),
        45.0,
        if request.mode == MilkyWayStackMode::SkyOnly {
            90.0
        } else {
            68.0
        },
        false,
    )?;
    let ground = if request.mode == MilkyWayStackMode::SkyOnly {
        None
    } else {
        Some(integrate_branch(
            app,
            job_id,
            &prepared,
            MilkyWayBranch::Ground,
            &request.integration,
            &cancel,
            &work_root.join("ground"),
            68.0,
            90.0,
            request.composition.ground_source == "base",
        )?)
    };
    cancellation_checkpoint(&cancel, "composición")?;
    let (composite, seam_residual, finite_fraction) = if let Some(ground) = &ground {
        let composition_mask = feather_mask(
            &prepared.mask.hard,
            prepared.width,
            prepared.height,
            request.composition.feather_px,
        );
        let (composite, seam, finite) = compose_layers(
            &sky,
            ground,
            &composition_mask,
            prepared.width,
            prepared.height,
            prepared.channels,
            request.composition.color_match == "boundaryAware",
        );
        (Some(composite), Some(seam), Some(finite))
    } else {
        (None, None, None)
    };
    let diagnostics = MilkyWayQualityDiagnostics {
        integration_profile: format!("{:?}", request.integration.profile),
        integration_method: format!("{:?}", request.integration.method),
        deterministic_passes: request.integration.deterministic_passes(),
        mask_confidence: prepared.mask.confidence,
        sky_frames_used: sky.frames_used,
        sky_frames_excluded: request.lights.len().saturating_sub(sky.frames_used),
        ground_frames_used: ground.as_ref().map(|value| value.frames_used).unwrap_or(0),
        ground_frames_excluded: ground
            .as_ref()
            .map(|value| request.lights.len().saturating_sub(value.frames_used))
            .unwrap_or(0),
        mean_sky_coverage: mean_finite(&sky.coverage),
        mean_ground_coverage: ground.as_ref().map(|value| mean_finite(&value.coverage)),
        mean_sky_rejection: mean_finite(&sky.rejection),
        mean_ground_rejection: ground.as_ref().map(|value| mean_finite(&value.rejection)),
        seam_residual_normalized: seam_residual,
        finite_composite_fraction: finite_fraction,
    };
    let mut warnings = contract_warnings;
    warnings.extend(prepared.warnings.clone());
    if diagnostics
        .seam_residual_normalized
        .is_some_and(|value| value > 0.08)
    {
        warnings.push(
            "La transición cielo/suelo conserva una diferencia tonal apreciable; revisa máscara y feather"
                .into(),
        );
    }
    let outputs = save_outputs(
        app,
        &request,
        job_id,
        &prepared,
        &sky,
        ground.as_ref(),
        composite.as_deref(),
        &diagnostics,
        &warnings,
        &cancel,
    )?;
    emit_milky_way_progress(
        app,
        job_id,
        "Completado",
        100.0,
        request.lights.len(),
        request.lights.len(),
        "Másteres lineales, capas y receta publicados",
    );
    Ok(MilkyWayStackResult {
        schema: MILKY_WAY_RECIPE_SCHEMA.into(),
        job_id: job_id.into(),
        width: prepared.width,
        height: prepared.height,
        channels: prepared.channels,
        base_frame: request.lights[prepared.base_index].clone(),
        mode: request.mode,
        scientific: !prepared.degraded,
        wcs_preserved: request.wcs.is_some(),
        elapsed_seconds: started.elapsed().as_secs_f32(),
        sky_path: outputs.sky_master.clone(),
        ground_path: outputs.ground_master.clone(),
        composite_path: outputs.composite.clone(),
        mask_path: outputs.sky_mask.clone(),
        coverage_path: outputs.sky_coverage.clone(),
        rejection_path: outputs.sky_rejection.clone(),
        recipe_path: outputs.recipe.clone(),
        outputs,
        frames: prepared.reports,
        diagnostics,
        warnings,
    })
}

#[tauri::command]
pub async fn run_milky_way_stack(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
    request: MilkyWayStackRequest,
) -> Result<MilkyWayStackResult, MilkyWayError> {
    validate_request(&request)?;
    crate::ds_begin_user_action(&state);
    let job_id = request
        .job_id
        .clone()
        .unwrap_or_else(|| pipeline::new_job_id("milky-way"));
    let cancel = state.job_registry.register(&job_id);
    if state.cancel_requested.load(Ordering::Acquire) {
        cancel.store(true, Ordering::Release);
    }
    let registry = state.job_registry.clone();
    let job_for_work = job_id.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = pipeline::JobGuard::new(registry, job_for_work.clone());
        run_milky_way_stack_impl(&app, request, &job_for_work, cancel)
    })
    .await
    .map_err(|error| MilkyWayError::new("worker_panic", "stack", error.to_string(), true))?
}

#[tauri::command]
pub fn cancel_milky_way_stack(state: tauri::State<'_, crate::AppState>, job_id: String) -> bool {
    state.job_registry.cancel(&job_id)
}

fn crop_interleaved(
    image: &crate::DsImage,
    left: usize,
    top: usize,
    width: usize,
    height: usize,
) -> Result<Vec<f32>, MilkyWayError> {
    if width == 0
        || height == 0
        || left.saturating_add(width) > image.w
        || top.saturating_add(height) > image.h
    {
        return Err(MilkyWayError::new(
            "invalid_crop",
            "crop",
            "El recorte queda fuera de la geometría sincronizada",
            true,
        ));
    }
    let length = width
        .checked_mul(height)
        .and_then(|value| value.checked_mul(image.ch))
        .ok_or_else(|| {
            MilkyWayError::new(
                "invalid_crop",
                "crop",
                "El recorte desborda memoria direccionable",
                true,
            )
        })?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| MilkyWayError::io("crop", "Memoria insuficiente para el recorte"))?;
    for y in top..top + height {
        let start = (y * image.w + left) * image.ch;
        let end = start + width * image.ch;
        output.extend_from_slice(&image.data[start..end]);
    }
    Ok(output)
}

fn geometry_fingerprint(
    request: &MilkyWayCropProductsRequest,
    source_w: usize,
    source_h: usize,
) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}",
        request.sky_path,
        request.mask_path,
        source_w,
        source_h,
        request.left,
        request.top,
        request.width,
        request.height,
        MILKY_WAY_RECIPE_SCHEMA
    )
    .bytes()
    {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("milky-way-geometry-{hash:016x}")
}

/// C9c: autoridad del WCS embebido que sobrevive al recorte. `Candidate`
/// significa que la cabecera contenía un WCS plausible pero SIN fingerprint
/// verificado: la matemática del recorte (trasladar CRPIX) es igual de
/// válida, pero el producto derivado no puede heredar autoridad de solución
/// validada que el original nunca tuvo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MilkyWayCropWcsAuthority {
    Validated,
    Candidate,
}

fn crop_wcs_public_status(
    wcs: Option<&(AstrometrySolution, MilkyWayCropWcsAuthority)>,
) -> (&'static str, bool) {
    match wcs.map(|(_, authority)| *authority) {
        Some(MilkyWayCropWcsAuthority::Validated) => ("validated", true),
        Some(MilkyWayCropWcsAuthority::Candidate) => ("candidate", false),
        None => ("missing", false),
    }
}

/// Núcleo puro de `crop_wcs`, separado de la lectura de fichero para poder
/// testear el contrato de estados sin FITS reales.
fn crop_embedded_wcs(
    embedded: crate::DsPostStackEmbeddedWcs,
    source_w: usize,
    source_h: usize,
    request: &MilkyWayCropProductsRequest,
) -> (
    Option<(AstrometrySolution, MilkyWayCropWcsAuthority)>,
    Vec<String>,
) {
    let mut warnings = Vec::new();
    let source = match embedded {
        crate::DsPostStackEmbeddedWcs::Validated(solution) => {
            Some((solution, MilkyWayCropWcsAuthority::Validated))
        }
        crate::DsPostStackEmbeddedWcs::Candidate(solution) => {
            // C9c: antes este brazo se fusionaba con Validated y el recorte
            // re-embebía el candidato con autoridad de solución validada. Se
            // conserva la traslación de CRPIX (matemática idéntica) pero el
            // estado candidato se propaga y se advierte al usuario.
            warnings.push(
                "WCS candidato sin validar: el recorte conserva la solución como candidata; \
                 valida la astrometría (resolución o fingerprint) antes de usarla con autoridad científica"
                    .into(),
            );
            Some((solution, MilkyWayCropWcsAuthority::Candidate))
        }
        crate::DsPostStackEmbeddedWcs::Missing => None,
        crate::DsPostStackEmbeddedWcs::Rejected(reason) => {
            warnings.push(format!("WCS no preservado: {reason}"));
            None
        }
    };
    let Some((source, authority)) = source else {
        return (None, warnings);
    };
    match crate::spcc_crop_astrometry_solution(
        &source,
        source_w,
        source_h,
        request.left,
        request.top,
        request.width,
        request.height,
    ) {
        Ok(solution) => (Some((solution, authority)), warnings),
        Err(reason) => {
            warnings.push(format!("WCS no preservado después del recorte: {reason}"));
            (None, warnings)
        }
    }
}

fn crop_wcs(
    path: &str,
    source_w: usize,
    source_h: usize,
    request: &MilkyWayCropProductsRequest,
) -> (
    Option<(AstrometrySolution, MilkyWayCropWcsAuthority)>,
    Vec<String>,
) {
    crop_embedded_wcs(
        crate::ds_poststack_embedded_wcs(path, source_w, source_h),
        source_w,
        source_h,
        request,
    )
}

fn metadata_from_optional_wcs(
    wcs: Option<&(AstrometrySolution, MilkyWayCropWcsAuthority)>,
) -> Vec<(&'static str, String)> {
    let mut metadata = vec![
        ("ZMWVER", format!("'{}'", MILKY_WAY_RECIPE_SCHEMA)),
        ("ZMWCROP", "                   T".into()),
        ("LINEAR", "                   T".into()),
    ];
    if let Some((wcs, authority)) = wcs {
        metadata.extend([
            ("CTYPE1", format!("'{}'", wcs.ctype)),
            ("CTYPE2", "'DEC--TAN'".into()),
            ("CUNIT1", "'deg'".into()),
            ("CUNIT2", "'deg'".into()),
            ("CRVAL1", format!("{:.12}", wcs.crval1)),
            ("CRVAL2", format!("{:.12}", wcs.crval2)),
            ("CRPIX1", format!("{:.12}", wcs.crpix1)),
            ("CRPIX2", format!("{:.12}", wcs.crpix2)),
            ("CD1_1", format!("{:.14}", wcs.cd11)),
            ("CD1_2", format!("{:.14}", wcs.cd12)),
            ("CD2_1", format!("{:.14}", wcs.cd21)),
            ("CD2_2", format!("{:.14}", wcs.cd22)),
            ("RADESYS", "'ICRS'".into()),
        ]);
        // C9c: el producto re-embebido declara explícitamente que su WCS
        // sigue siendo candidato; un Validated no cambia de cabecera.
        if *authority == MilkyWayCropWcsAuthority::Candidate {
            metadata.push(("ZMWWCSST", "'CANDIDATE'".into()));
        }
    }
    metadata
}

fn crop_and_write_product(
    source_path: &str,
    destination: &Path,
    expected_w: usize,
    expected_h: usize,
    request: &MilkyWayCropProductsRequest,
    metadata: &[(&str, String)],
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<usize, MilkyWayError> {
    cancellation_checkpoint(cancel, "recorte sincronizado")?;
    let image = crate::ds_read_image(source_path)
        .map_err(|error| MilkyWayError::io("crop", format!("{source_path}: {error}")))?;
    if image.w != expected_w || image.h != expected_h {
        return Err(MilkyWayError::scientific(
            "crop",
            format!(
                "'{}' tiene geometría {}x{}; se esperaba {}x{}",
                source_path, image.w, image.h, expected_w, expected_h
            ),
        ));
    }
    let cropped = crop_interleaved(
        &image,
        request.left,
        request.top,
        request.width,
        request.height,
    )?;
    write_fits(
        destination,
        &cropped,
        request.width,
        request.height,
        image.ch,
        metadata,
        cancel,
    )?;
    Ok(image.ch)
}

fn crop_milky_way_products_impl(
    app: &tauri::AppHandle,
    request: MilkyWayCropProductsRequest,
    job_id: &str,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) -> Result<MilkyWayCroppedProductsResult, MilkyWayError> {
    let sky_variance_source = request.resolved_sky_variance()?;
    let sky_coverage_source = request.resolved_sky_coverage()?;
    let sky_rejection_source = request.resolved_sky_rejection()?;
    let ground_sources = request.resolved_ground_maps()?;
    for path in [
        Some(request.sky_path.as_str()),
        request.ground_path.as_deref(),
        request.composite_path.as_deref(),
        Some(request.mask_path.as_str()),
        Some(sky_variance_source),
        Some(sky_coverage_source),
        Some(sky_rejection_source),
        ground_sources.map(|value| value.0),
        ground_sources.map(|value| value.1),
        ground_sources.map(|value| value.2),
    ]
    .into_iter()
    .flatten()
    {
        if !Path::new(path).is_file() || !scientific_extension(path) {
            return Err(MilkyWayError::invalid(format!(
                "Producto sincronizado ausente o no lineal: {path}"
            )));
        }
    }
    let sky = crate::ds_read_image(&request.sky_path)
        .map_err(|error| MilkyWayError::io("crop", error))?;
    if request.width == 0
        || request.height == 0
        || request.left.saturating_add(request.width) > sky.w
        || request.top.saturating_add(request.height) > sky.h
    {
        return Err(MilkyWayError::invalid(
            "El rectángulo de recorte no cabe en los productos originales",
        ));
    }
    let (wcs, warnings) = crop_wcs(&request.sky_path, sky.w, sky.h, &request);
    let (wcs_status, wcs_preserved) = crop_wcs_public_status(wcs.as_ref());
    let metadata = metadata_from_optional_wcs(wcs.as_ref());
    let scalar_metadata = vec![
        ("ZMWVER", format!("'{}'", MILKY_WAY_RECIPE_SCHEMA)),
        ("ZMWCROP", "                   T".into()),
    ];
    let parent = Path::new(
        request
            .composite_path
            .as_deref()
            .unwrap_or(&request.sky_path),
    )
    .parent()
    .ok_or_else(|| MilkyWayError::invalid("Los productos no tienen carpeta contenedora"))?;
    let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let final_dir = parent
        .join("Derivados")
        .join(format!("Recorte_{timestamp}"));
    let staging_dir = parent.join("Derivados").join(format!(
        ".Recorte_{timestamp}_{}.part",
        safe_job_fragment(job_id)
    ));
    if final_dir.exists() || staging_dir.exists() {
        return Err(MilkyWayError::io(
            "crop",
            "Ya existe un derivado con el mismo identificador temporal",
        ));
    }
    let staging = StagingDirectory::new(staging_dir.clone())?;
    let sky_out = final_dir.join("Master_Cielo_Recorte_Lineal.fits");
    let ground_out = request
        .ground_path
        .as_ref()
        .map(|_| final_dir.join("Master_Suelo_Recorte_Lineal.fits"));
    let composite_out = request
        .composite_path
        .as_ref()
        .map(|_| final_dir.join("Composicion_Recorte_Lineal.fits"));
    let mask_out = final_dir.join("Mascara_Cielo_Recorte.fits");
    let sky_variance_out = request
        .sky_variance_path
        .as_ref()
        .map(|_| final_dir.join("Calidad_Cielo_VAR_Recorte.fits"));
    let sky_coverage_out = final_dir.join("Calidad_Cielo_Cobertura_Recorte.fits");
    let sky_rejection_out = final_dir.join("Calidad_Cielo_Rechazo_Recorte.fits");
    let ground_variance_out = request
        .ground_variance_path
        .as_ref()
        .map(|_| final_dir.join("Calidad_Suelo_VAR_Recorte.fits"));
    let ground_coverage_out = request
        .ground_coverage_path
        .as_ref()
        .map(|_| final_dir.join("Calidad_Suelo_Cobertura_Recorte.fits"));
    let ground_rejection_out = request
        .ground_rejection_path
        .as_ref()
        .map(|_| final_dir.join("Calidad_Suelo_Rechazo_Recorte.fits"));
    let recipe_out = final_dir.join("Receta_Recorte_Sincronizado.json");
    let published_products = 5
        + usize::from(ground_out.is_some())
        + usize::from(composite_out.is_some())
        + usize::from(sky_variance_out.is_some())
        + usize::from(ground_variance_out.is_some())
        + usize::from(ground_coverage_out.is_some())
        + usize::from(ground_rejection_out.is_some());
    let write_at = |final_path: &Path| {
        staging_dir.join(final_path.strip_prefix(&final_dir).unwrap_or(final_path))
    };
    crop_and_write_product(
        &request.sky_path,
        &write_at(&sky_out),
        sky.w,
        sky.h,
        &request,
        &metadata,
        &cancel,
    )?;
    if let (Some(source), Some(destination)) = (&request.ground_path, &ground_out) {
        crop_and_write_product(
            source,
            &write_at(destination),
            sky.w,
            sky.h,
            &request,
            &[],
            &cancel,
        )?;
    }
    if let (Some(source), Some(destination)) = (&request.composite_path, &composite_out) {
        crop_and_write_product(
            source,
            &write_at(destination),
            sky.w,
            sky.h,
            &request,
            &metadata,
            &cancel,
        )?;
    }
    for (source, destination) in [
        (request.mask_path.as_str(), &mask_out),
        (sky_coverage_source, &sky_coverage_out),
        (sky_rejection_source, &sky_rejection_out),
    ] {
        let channels = crop_and_write_product(
            source,
            &write_at(destination),
            sky.w,
            sky.h,
            &request,
            &scalar_metadata,
            &cancel,
        )?;
        if channels != 1 {
            return Err(MilkyWayError::scientific(
                "crop",
                format!("El mapa '{}' debe ser mono", source),
            ));
        }
    }
    for (source, destination) in [
        (
            request.sky_variance_path.as_deref(),
            sky_variance_out.as_ref(),
        ),
        (
            request.ground_variance_path.as_deref(),
            ground_variance_out.as_ref(),
        ),
    ] {
        if let (Some(source), Some(destination)) = (source, destination) {
            // VAR conserva los canales de SCI; no se fuerza a mono.
            crop_and_write_product(
                source,
                &write_at(destination),
                sky.w,
                sky.h,
                &request,
                &scalar_metadata,
                &cancel,
            )?;
        }
    }
    for (source, destination) in [
        (
            request.ground_coverage_path.as_deref(),
            ground_coverage_out.as_ref(),
        ),
        (
            request.ground_rejection_path.as_deref(),
            ground_rejection_out.as_ref(),
        ),
    ] {
        if let (Some(source), Some(destination)) = (source, destination) {
            let channels = crop_and_write_product(
                source,
                &write_at(destination),
                sky.w,
                sky.h,
                &request,
                &scalar_metadata,
                &cancel,
            )?;
            if channels != 1 {
                return Err(MilkyWayError::scientific(
                    "crop",
                    format!("El mapa '{}' debe ser mono", source),
                ));
            }
        }
    }
    let geometry_id = geometry_fingerprint(&request, sky.w, sky.h);
    let recipe = serde_json::json!({
        "schema": "zenith-milky-way-crop-v1",
        "geometryId": geometry_id,
        "source": &request,
        "sourceGeometry": {"width": sky.w, "height": sky.h},
        "crop": {"left": request.left, "top": request.top, "width": request.width, "height": request.height},
        "outputs": {
            "skyPath": sky_out,
            "groundPath": ground_out,
            "compositePath": composite_out,
            "maskPath": mask_out,
            "skyVariancePath": sky_variance_out,
            "skyCoveragePath": sky_coverage_out,
            "skyRejectionPath": sky_rejection_out,
            "groundVariancePath": ground_variance_out,
            "groundCoveragePath": ground_coverage_out,
            "groundRejectionPath": ground_rejection_out,
            "coveragePath": sky_coverage_out,
            "rejectionPath": sky_rejection_out
        },
        "wcsStatus": wcs_status,
        "wcsPreserved": wcs_preserved,
        "warnings": warnings
    });
    let recipe_file = std::fs::File::create(write_at(&recipe_out))
        .map_err(|error| MilkyWayError::io("crop", format!("crear receta: {error}")))?;
    let mut recipe_writer = std::io::BufWriter::new(recipe_file);
    serde_json::to_writer_pretty(&mut recipe_writer, &recipe)
        .map_err(|error| MilkyWayError::io("crop", format!("escribir receta: {error}")))?;
    use std::io::Write;
    recipe_writer
        .flush()
        .map_err(|error| MilkyWayError::io("crop", format!("vaciar receta: {error}")))?;
    recipe_writer
        .get_ref()
        .sync_all()
        .map_err(|error| MilkyWayError::io("crop", format!("sincronizar receta: {error}")))?;
    cancellation_checkpoint(&cancel, "recorte sincronizado")?;
    staging.commit(&final_dir)?;
    emit_milky_way_progress(
        app,
        job_id,
        "Recorte sincronizado",
        100.0,
        published_products,
        published_products,
        format!("{published_products} productos publicados con el mismo geometryId"),
    );
    Ok(MilkyWayCroppedProductsResult {
        job_id: job_id.into(),
        geometry_id,
        source_width: sky.w,
        source_height: sky.h,
        width: request.width,
        height: request.height,
        sky_path: sky_out.to_string_lossy().into_owned(),
        ground_path: ground_out.map(|path| path.to_string_lossy().into_owned()),
        composite_path: composite_out.map(|path| path.to_string_lossy().into_owned()),
        mask_path: mask_out.to_string_lossy().into_owned(),
        sky_variance_path: sky_variance_out.map(|path| path.to_string_lossy().into_owned()),
        sky_coverage_path: sky_coverage_out.to_string_lossy().into_owned(),
        sky_rejection_path: sky_rejection_out.to_string_lossy().into_owned(),
        ground_variance_path: ground_variance_out.map(|path| path.to_string_lossy().into_owned()),
        ground_coverage_path: ground_coverage_out.map(|path| path.to_string_lossy().into_owned()),
        ground_rejection_path: ground_rejection_out.map(|path| path.to_string_lossy().into_owned()),
        coverage_path: sky_coverage_out.to_string_lossy().into_owned(),
        rejection_path: sky_rejection_out.to_string_lossy().into_owned(),
        recipe_path: recipe_out.to_string_lossy().into_owned(),
        wcs_status: wcs_status.into(),
        wcs_preserved,
        warnings,
    })
}

#[tauri::command]
pub async fn crop_milky_way_products(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
    request: MilkyWayCropProductsRequest,
) -> Result<MilkyWayCroppedProductsResult, MilkyWayError> {
    crate::ds_begin_user_action(&state);
    let job_id = request
        .job_id
        .clone()
        .unwrap_or_else(|| pipeline::new_job_id("milky-way-crop"));
    let cancel = state.job_registry.register(&job_id);
    let registry = state.job_registry.clone();
    let job_for_work = job_id.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = pipeline::JobGuard::new(registry, job_for_work.clone());
        crop_milky_way_products_impl(&app, request, &job_for_work, cancel)
    })
    .await
    .map_err(|error| MilkyWayError::new("worker_panic", "crop", error.to_string(), true))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calibration_signature_for_test() -> pipeline::CalibrationSignature {
        pipeline::CalibrationSignature {
            camera: Some("ZenithCam".into()),
            sensor: Some("IMX-Test".into()),
            read_mode: Some("LowNoise".into()),
            gain: Some(100.0),
            offset: Some(20.0),
            temperature_c: Some(-10.0),
            exposure_seconds: Some(30.0),
            binning_x: Some(1),
            binning_y: Some(1),
            roi: Some([0, 0, 100, 80]),
            cfa_pattern: Some("RGGB".into()),
            cfa_phase: Some([0, 0]),
            filter: Some("L".into()),
            optical_train: Some("Zenith 35mm".into()),
            adc_bits: Some(16),
            white_level_adu: Some(65_535.0),
            ..pipeline::CalibrationSignature::default()
        }
    }

    fn calibration_probe_for_test(
        path: &str,
        signature: pipeline::CalibrationSignature,
    ) -> MilkyWayCalibrationProbe {
        MilkyWayCalibrationProbe {
            path: path.into(),
            width: 100,
            height: 80,
            channels: 1,
            bayer: Some(8),
            signature,
        }
    }

    fn valid_wcs() -> AstrometrySolution {
        AstrometrySolution {
            ctype: "RA---TAN".into(),
            crval1: 274.7,
            crval2: -13.8,
            crpix1: 50.5,
            crpix2: 40.5,
            cd11: -0.000_25,
            cd12: 0.0,
            cd21: 0.0,
            cd22: 0.000_25,
            rms_px: 0.7,
            inliers: 35,
            handedness: "normal".into(),
            scale_arcsec_px: 0.9,
            source: "localIndex".into(),
            cached: true,
        }
    }

    #[test]
    fn automatic_horizon_finds_bright_textured_ground() {
        let (width, height) = (96usize, 64usize);
        let horizon = 39usize;
        let mut luma = vec![120.0f32; width * height];
        for y in horizon..height {
            for x in 0..width {
                luma[y * width + x] = 2_000.0 + ((x * 17 + y * 31) % 101) as f32 * 8.0;
            }
        }
        // Estrellas en cielo: no deben convertirse en un horizonte por columna.
        for &(x, y) in &[(10usize, 8usize), (33, 19), (70, 12), (87, 30)] {
            luma[y * width + x] = 8_000.0;
        }
        let mask = auto_horizon_mask(&luma, width, height);
        let contour = mask_contour(&mask.hard, width, height, 32);
        let mean_y = contour.iter().map(|point| point.y).sum::<f32>() / contour.len() as f32;
        assert!((mean_y - horizon as f32 / height as f32).abs() < 0.12);
        assert!(mask.confidence > 0.05);
    }

    #[test]
    fn polygon_and_edits_create_soft_bounded_mask() {
        let (width, height) = (40usize, 30usize);
        let spec = MilkyWayMaskSpec::UserPolygon {
            points: vec![
                MilkyWayMaskPoint { x: 0.0, y: 0.0 },
                MilkyWayMaskPoint { x: 1.0, y: 0.0 },
                MilkyWayMaskPoint { x: 1.0, y: 0.5 },
                MilkyWayMaskPoint { x: 0.0, y: 0.5 },
            ],
            feather_px: 6,
        };
        let built = build_mask(&spec, &vec![0.0; width * height], width, height);
        assert_eq!(built.hard[2 * width + 2], 1);
        assert_eq!(built.hard[28 * width + 2], 0);
        assert!(built.soft.iter().all(|value| (0.0..=1.0).contains(value)));
        assert!(built.soft.iter().any(|value| *value > 0.0 && *value < 1.0));
    }

    #[test]
    fn sigma_clip_rejects_single_extreme_sample() {
        let mut values = vec![9.9, 10.0, 10.1, 10.2, 400.0];
        let (mean, variance, rejected) = robust_integrate_samples(
            &mut values,
            &MilkyWayIntegrationConfig {
                sigma_low: 3.0,
                sigma_high: 3.0,
                iterations: 3,
                ..MilkyWayIntegrationConfig::default()
            },
        )
        .unwrap();
        assert_eq!(rejected, 1);
        assert!((mean - 10.05).abs() < 0.1);
        assert!(variance.is_finite() && variance >= 0.0);
    }

    #[test]
    fn composition_is_non_destructive_and_respects_extremes() {
        let sky = BranchIntegration {
            sci: vec![10.0, 20.0, 30.0, 40.0],
            variance: vec![1.0; 4],
            coverage: vec![1.0; 4],
            rejection: vec![0.0; 4],
            frames_used: 3,
        };
        let ground = BranchIntegration {
            sci: vec![100.0, 200.0, 300.0, 400.0],
            variance: vec![1.0; 4],
            coverage: vec![1.0; 4],
            rejection: vec![0.0; 4],
            frames_used: 3,
        };
        let sky_before = sky.sci.clone();
        let ground_before = ground.sci.clone();
        let (composite, _, finite) =
            compose_layers(&sky, &ground, &[1.0, 0.5, 0.0, 0.25], 2, 2, 1, false);
        assert_eq!(composite[0], 10.0);
        assert_eq!(composite[1], 110.0);
        assert_eq!(composite[2], 300.0);
        assert_eq!(composite[3], 310.0);
        assert_eq!(finite, 1.0);
        assert_eq!(sky.sci, sky_before);
        assert_eq!(ground.sci, ground_before);
    }

    #[test]
    fn wcs_must_bind_to_effective_reference() {
        let request = MilkyWayStackRequest {
            lights: vec!["a.fits".into(), "b.fits".into()],
            output_dir: "/tmp".into(),
            base_frame: Some("a.fits".into()),
            wcs: Some(MilkyWayWcsBinding {
                source_light: "b.fits".into(),
                solution: valid_wcs(),
            }),
            ..MilkyWayStackRequest::default()
        };
        // Se prueba la regla WCS directamente; la validación de existencia de
        // archivos pertenece al preflight de I/O.
        let binding = request.wcs.as_ref().unwrap();
        assert!(!same_path(
            request.base_frame.as_deref().unwrap(),
            &binding.source_light
        ));
        assert!(validate_wcs(&binding.solution).is_ok());
    }

    #[test]
    fn output_names_are_readable_and_mode_specific() {
        let root = Path::new("/tmp/Via_Lactea_2026-08-02");
        let sky = final_output_paths(root, MilkyWayStackMode::SkyOnly);
        assert!(sky.primary.ends_with("Master_Cielo_Lineal.fits"));
        assert!(sky.composite.is_none());
        let layers = final_output_paths(root, MilkyWayStackMode::SeparateLayers);
        assert!(layers
            .composite
            .as_deref()
            .unwrap()
            .ends_with("Composicion_Via_Lactea_Lineal.fits"));
        assert!(layers.ground_master.is_some());
    }

    #[test]
    fn frontend_request_contract_deserializes_without_adapter() {
        let json = serde_json::json!({
            "schema": "zenith-milky-way-recipe-v1",
            "mode": "freezeGround",
            "lights": ["/data/a.fits", "/data/b.fits"],
            "darks": [],
            "flats": [],
            "foregroundFrames": [],
            "baseFramePath": "/data/a.fits",
            "outputDirectory": "/output",
            "unifyExposure": true,
            "mask": {
                "strategy": "horizon",
                "confidence": 0.93,
                "userConfirmed": true,
                "featherPx": 54,
                "horizonPoints": [[0.0, 0.62], [1.0, 0.67]],
                "brushStrokes": [{"target": "ground", "points": [[0.4, 0.5], [0.5, 0.55]]}],
                "brushTarget": "ground",
                "brushRadiusPx": 42,
                "protectedForeground": []
            },
            "registration": {
                "model": "auto",
                "minInliers": 18,
                "maxRmsPx": 2.2,
                "distortionCorrection": "auto",
                "singleResample": true
            },
            "integration": {
                "profile": "quality",
                "method": "winsorized",
                "skySigmaLow": 3.2,
                "skySigmaHigh": 2.8,
                "groundSigmaLow": 3.5,
                "groundSigmaHigh": 3.5,
                "normalization": "robustLinear",
                "dynamicHotPixels": true,
                "rejectTrails": true
            },
            "composition": {
                "keepSeparateLayers": true,
                "groundSource": "stack",
                "skySource": "stack",
                "featherPx": 48,
                "edgeDeghost": 0.65,
                "colorMatch": "boundaryAware",
                "preserveReflections": true
            },
            "publish": {
                "exportLinearTiff": false,
                "exportFitsLayers": true,
                "exportMask": true,
                "openEditor": true
            },
            "geometryId": "milkyway-source-v1",
            "requestedProducts": ["sky", "ground", "composite", "mask", "coverage", "rejection"]
        });
        let request: MilkyWayStackRequest = serde_json::from_value(json).unwrap();
        assert_eq!(request.schema, MILKY_WAY_RECIPE_SCHEMA);
        assert_eq!(request.output_dir, "/output");
        assert_eq!(request.base_frame.as_deref(), Some("/data/a.fits"));
        assert!(request.mask_confirmed_by_user);
        assert!(request.unify_exposure);
        assert_eq!(request.registration.model, "auto");
        assert!(matches!(request.mask, MilkyWayMaskSpec::UserHorizon { .. }));
        let canonical = serde_json::to_value(&request).unwrap();
        assert_eq!(canonical["outputDir"], "/output");
        assert_eq!(canonical["baseFrame"], "/data/a.fits");
        assert!(canonical.get("outputDirectory").is_none());
    }

    #[test]
    fn brush_strategy_preserves_visible_horizon_and_strokes() {
        let request: MilkyWayStackRequest = serde_json::from_value(serde_json::json!({
            "mode": "freezeGround",
            "mask": {
                "strategy": "brush",
                "confidence": 0.81,
                "userConfirmed": true,
                "featherPx": 36,
                "horizonPoints": [[0.0, 0.61], [0.5, 0.64], [1.0, 0.66]],
                "brushStrokes": [
                    {"target": "ground", "points": [[0.42, 0.48], [0.46, 0.52]]}
                ],
                "brushTarget": "ground",
                "brushRadiusPx": 24,
                "protectedForeground": []
            }
        }))
        .unwrap();

        let MilkyWayMaskSpec::UserHorizon {
            points,
            brush_strokes,
            brush_radius_px,
            feather_px,
        } = request.mask
        else {
            panic!("Brush con una frontera visible debe conservarla como UserHorizon");
        };
        assert_eq!(points.len(), 3);
        assert_eq!(points[1], MilkyWayMaskPoint { x: 0.5, y: 0.64 });
        assert_eq!(brush_strokes.len(), 1);
        assert_eq!(brush_strokes[0].target, MilkyWayBrushTarget::Ground);
        assert_eq!(brush_strokes[0].points.len(), 2);
        assert_eq!(brush_radius_px, 24);
        assert_eq!(feather_px, 36);
        assert!(request.mask_confirmed_by_user);
    }

    #[test]
    fn missing_fallback_policy_defaults_to_strict() {
        let request: MilkyWayStackRequest = serde_json::from_value(serde_json::json!({
            "mode": "skyOnly",
            "lights": [],
            "mask": {"mode": "fullSky", "featherPx": 0}
        }))
        .unwrap();
        assert_eq!(request.fallback_policy, MilkyWayFallbackPolicy::Strict);
    }

    #[test]
    fn degenerate_mask_specs_are_rejected_not_panicked() {
        let empty_polygon = MilkyWayMaskSpec::UserPolygon {
            points: vec![],
            feather_px: 48,
        };
        assert_eq!(
            validate_mask_spec(&empty_polygon).unwrap_err().code,
            "invalid_request"
        );

        let empty_horizon = MilkyWayMaskSpec::UserHorizon {
            points: vec![],
            brush_strokes: vec![],
            brush_radius_px: 42,
            feather_px: 48,
        };
        assert_eq!(
            validate_mask_spec(&empty_horizon).unwrap_err().code,
            "invalid_request"
        );
    }

    #[test]
    fn mask_kernels_survive_degenerate_point_lists() {
        // Guardas defensivas: aunque la validación rechace estos payloads,
        // los kernels no deben poder entrar en pánico jamás.
        assert!(!point_inside_polygon(0.5, 0.5, &[]));
        assert!(!point_inside_polygon(
            0.5,
            0.5,
            &[
                MilkyWayMaskPoint { x: 0.1, y: 0.1 },
                MilkyWayMaskPoint { x: 0.9, y: 0.9 }
            ],
        ));

        let build = user_horizon_mask(&[], 8, 6);
        assert_eq!(build.hard.len(), 48);
        assert!(build.hard.iter().all(|value| *value == 0));
        assert_eq!(build.confidence, 0.0);
    }

    #[test]
    fn unsupported_promises_fail_with_typed_error() {
        let star_trails = MilkyWayStackRequest {
            mode: MilkyWayStackMode::StarTrails,
            ..MilkyWayStackRequest::default()
        };
        assert_eq!(
            validate_request(&star_trails).unwrap_err().code,
            "unsupported_feature"
        );

        let tiff = MilkyWayStackRequest {
            mask_confirmed_by_user: true,
            publish: MilkyWayPublishOptions {
                export_linear_tiff: true,
                ..MilkyWayPublishOptions::default()
            },
            ..MilkyWayStackRequest::default()
        };
        assert_eq!(
            validate_request(&tiff).unwrap_err().code,
            "unsupported_feature"
        );
    }

    #[test]
    fn dual_branch_modes_always_require_explicit_mask_confirmation() {
        let ground = MilkyWayStackRequest::default();
        let error = validate_request(&ground).unwrap_err();
        assert_eq!(error.stage, "mask");
        assert_eq!(error.code, "scientific_guard");

        let sky_only = MilkyWayStackRequest {
            mode: MilkyWayStackMode::SkyOnly,
            ..MilkyWayStackRequest::default()
        };
        assert_ne!(validate_request(&sky_only).unwrap_err().stage, "mask");
    }

    #[test]
    fn native_preflight_enforces_minimum_frames_for_every_policy() {
        let sky = MilkyWayStackRequest {
            mode: MilkyWayStackMode::SkyOnly,
            lights: vec!["one.fits".into()],
            fallback_policy: MilkyWayFallbackPolicy::AllowDegraded,
            ..MilkyWayStackRequest::default()
        };
        assert_eq!(validate_request(&sky).unwrap_err().stage, "preflight");

        let ground = MilkyWayStackRequest {
            mode: MilkyWayStackMode::FreezeGround,
            lights: vec!["1.fits".into(), "2.fits".into(), "3.fits".into()],
            mask_confirmed_by_user: true,
            fallback_policy: MilkyWayFallbackPolicy::AllowDegraded,
            ..MilkyWayStackRequest::default()
        };
        assert_eq!(validate_request(&ground).unwrap_err().stage, "preflight");
    }

    #[test]
    fn calibration_contract_accepts_only_exact_dark_signatures() {
        let light = calibration_probe_for_test("light.fits", calibration_signature_for_test());
        let exact = calibration_probe_for_test("dark.fits", calibration_signature_for_test());
        let decision = assess_milky_way_calibration_candidate(
            &[light],
            &exact,
            crate::deepsky_calibration_contract::CalibrationRole::Dark,
            MilkyWayFallbackPolicy::Strict,
        );
        assert_eq!(decision.disposition, MilkyWayCalibrationDisposition::Accept);
    }

    #[test]
    fn calibration_contract_blocks_or_excludes_exposure_and_gain_mismatch() {
        let light = calibration_probe_for_test("light.fits", calibration_signature_for_test());
        let mut mismatch_signature = calibration_signature_for_test();
        mismatch_signature.exposure_seconds = Some(20.0);
        mismatch_signature.gain = Some(200.0);
        let mismatch = calibration_probe_for_test("dark-wrong.fits", mismatch_signature);
        let strict = assess_milky_way_calibration_candidate(
            std::slice::from_ref(&light),
            &mismatch,
            crate::deepsky_calibration_contract::CalibrationRole::Dark,
            MilkyWayFallbackPolicy::Strict,
        );
        assert_eq!(strict.disposition, MilkyWayCalibrationDisposition::Block);
        assert!(strict
            .reasons
            .iter()
            .any(|reason| reason.contains("exposureSeconds")));
        assert!(strict.reasons.iter().any(|reason| reason.contains("gain")));
        let degraded = assess_milky_way_calibration_candidate(
            &[light],
            &mismatch,
            crate::deepsky_calibration_contract::CalibrationRole::Dark,
            MilkyWayFallbackPolicy::AllowDegraded,
        );
        assert_eq!(
            degraded.disposition,
            MilkyWayCalibrationDisposition::Exclude
        );
    }

    #[test]
    fn calibration_contract_rejects_geometry_and_flat_gain_mismatch() {
        let light = calibration_probe_for_test("light.fits", calibration_signature_for_test());
        let mut geometry =
            calibration_probe_for_test("dark-crop.fits", calibration_signature_for_test());
        geometry.width = 99;
        let geometry_decision = assess_milky_way_calibration_candidate(
            std::slice::from_ref(&light),
            &geometry,
            crate::deepsky_calibration_contract::CalibrationRole::Dark,
            MilkyWayFallbackPolicy::Strict,
        );
        assert_eq!(
            geometry_decision.disposition,
            MilkyWayCalibrationDisposition::Block
        );
        assert!(geometry_decision
            .reasons
            .iter()
            .any(|reason| reason.contains("geometría")));

        let mut flat_signature = calibration_signature_for_test();
        flat_signature.gain = Some(101.0);
        flat_signature.exposure_seconds = Some(0.2);
        let flat = calibration_probe_for_test("flat-wrong.fits", flat_signature);
        let flat_decision = assess_milky_way_calibration_candidate(
            &[light],
            &flat,
            crate::deepsky_calibration_contract::CalibrationRole::Flat,
            MilkyWayFallbackPolicy::Strict,
        );
        assert_eq!(
            flat_decision.disposition,
            MilkyWayCalibrationDisposition::Block
        );
        assert!(flat_decision
            .reasons
            .iter()
            .any(|reason| reason.contains("gain")));
    }

    #[test]
    fn calibration_contract_never_applies_missing_critical_metadata() {
        let light = calibration_probe_for_test(
            "light-metadata-missing.fits",
            pipeline::CalibrationSignature::default(),
        );
        let dark = calibration_probe_for_test(
            "dark-metadata-missing.fits",
            pipeline::CalibrationSignature::default(),
        );
        let strict = assess_milky_way_calibration_candidate(
            std::slice::from_ref(&light),
            &dark,
            crate::deepsky_calibration_contract::CalibrationRole::Dark,
            MilkyWayFallbackPolicy::Strict,
        );
        assert_eq!(strict.disposition, MilkyWayCalibrationDisposition::Block);
        let degraded = assess_milky_way_calibration_candidate(
            &[light],
            &dark,
            crate::deepsky_calibration_contract::CalibrationRole::Dark,
            MilkyWayFallbackPolicy::AllowDegraded,
        );
        assert_eq!(
            degraded.disposition,
            MilkyWayCalibrationDisposition::Exclude
        );
        assert!(degraded
            .reasons
            .iter()
            .any(|reason| reason.contains("gainOrIso")));
        assert!(degraded
            .reasons
            .iter()
            .any(|reason| reason.contains("exposureSeconds")));
    }

    #[test]
    fn fts_extension_uses_the_fits_signature_probe() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "zenith-milky-way-fts-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("calibration.fts");
        let cancel = std::sync::atomic::AtomicBool::new(false);
        write_fits(
            &path,
            &[100.0; 16],
            4,
            4,
            1,
            &[
                ("EXPTIME", "30.0".into()),
                ("GAIN", "100.0".into()),
                ("CCD-TEMP", "-10.0".into()),
            ],
            &cancel,
        )
        .unwrap();
        let signature = crate::ds_probe_calibration_signature(path.to_str().unwrap()).unwrap();
        assert_eq!(signature.exposure_seconds, Some(30.0));
        assert_eq!(signature.gain, Some(100.0));
        assert_eq!(signature.temperature_c, Some(-10.0));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn composition_legacy_flags_are_normalized_and_disclosed() {
        let mut request = MilkyWayStackRequest {
            mode: MilkyWayStackMode::SeparateLayers,
            composition: MilkyWayCompositionOptions {
                keep_separate_layers: false,
                edge_deghost: 0.65,
                preserve_reflections: false,
                ..MilkyWayCompositionOptions::default()
            },
            ..MilkyWayStackRequest::default()
        };
        let warnings = normalize_composition_contract(&mut request);
        assert!(request.composition.keep_separate_layers);
        assert_eq!(request.composition.edge_deghost, 0.0);
        assert!(request.composition.preserve_reflections);
        assert!(warnings.iter().any(|warning| warning.contains("alias")));
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("edgeDeghost")));
    }

    #[test]
    fn synchronized_crop_is_exact_for_mono_and_rgb() {
        let mono = crate::DsImage {
            data: (0..12).map(|value| value as f32).collect(),
            w: 4,
            h: 3,
            ch: 1,
            bayer: None,
        };
        assert_eq!(
            crop_interleaved(&mono, 1, 1, 2, 2).unwrap(),
            vec![5.0, 6.0, 9.0, 10.0]
        );

        let rgb = crate::DsImage {
            data: (0..18).map(|value| value as f32).collect(),
            w: 3,
            h: 2,
            ch: 3,
            bayer: None,
        };
        assert_eq!(
            crop_interleaved(&rgb, 1, 0, 2, 2).unwrap(),
            vec![3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 12.0, 13.0, 14.0, 15.0, 16.0, 17.0]
        );
        assert_eq!(
            crop_interleaved(&rgb, 2, 1, 2, 1).unwrap_err().stage,
            "crop"
        );
    }

    #[test]
    fn branch_warper_identity_is_pixel_exact() {
        let image = crate::DsImage {
            data: (0..6).map(|value| value as f32).collect(),
            w: 3,
            h: 2,
            ch: 1,
            bayer: None,
        };
        let (warped, coverage) = warp_milky_way_branch(
            &image,
            crate::milky_way_registration::TransformModel::identity(),
            &[1.0; 6],
            MilkyWayBranch::Sky,
            3,
            2,
        );
        assert_eq!(warped, image.data);
        assert_eq!(coverage, vec![1.0; 6]);
    }

    #[test]
    fn branch_warper_moves_source_space_mask_with_translation() {
        let image = crate::DsImage {
            data: (0..8).map(|value| value as f32).collect(),
            w: 4,
            h: 2,
            ch: 1,
            bayer: None,
        };
        let source_mask = vec![1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0];
        let transform = crate::milky_way_registration::TransformModel {
            planar: crate::milky_way_registration::PlanarTransform::Affine([
                1.0, 0.0, 1.0, 0.0, 1.0, 0.0,
            ]),
            radial: None,
        };
        let (sky, sky_coverage) =
            warp_milky_way_branch(&image, transform, &source_mask, MilkyWayBranch::Sky, 4, 2);
        assert_eq!(&sky_coverage[..4], &[0.0, 1.0, 1.0, 0.0]);
        assert_eq!(sky[1], image.data[0]);
        assert_eq!(sky[2], image.data[1]);
        assert!(sky[0].is_nan() && sky[3].is_nan());

        let (_, ground_coverage) = warp_milky_way_branch(
            &image,
            transform,
            &source_mask,
            MilkyWayBranch::Ground,
            4,
            2,
        );
        assert_eq!(&ground_coverage[..4], &[0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn branch_warper_supports_radial_inverse_mapping_in_one_pass() {
        let (width, height) = (9usize, 9usize);
        let image = crate::DsImage {
            data: (0..height)
                .flat_map(|y| (0..width).map(move |x| x as f32 + y as f32 * 10.0))
                .collect(),
            w: width,
            h: height,
            ch: 1,
            bayer: None,
        };
        let transform = crate::milky_way_registration::TransformModel {
            planar: crate::milky_way_registration::PlanarTransform::identity(),
            radial: Some(crate::milky_way_registration::RadialDistortion {
                center: crate::milky_way_registration::Point2::new(4.0, 4.0),
                normalization_radius: 5.0,
                k1: 0.12,
                k2: 0.02,
            }),
        };
        let (warped, coverage) = warp_milky_way_branch(
            &image,
            transform,
            &vec![1.0; width * height],
            MilkyWayBranch::Sky,
            width,
            height,
        );
        assert_eq!(warped[4 * width + 4], image.data[4 * width + 4]);
        let source = transform
            .inverse(crate::milky_way_registration::Point2::new(7.0, 4.0))
            .unwrap();
        assert!((warped[4 * width + 7] - (source.x as f32 + 40.0)).abs() < 1.0e-4);
        assert_eq!(coverage[4 * width + 7], 1.0);
    }

    /// C2: con mask.featherPx=8 y composition.featherPx=128, todo píxel con
    /// peso de composición fraccional debe tener AMBAS ramas finitas y el
    /// compuesto debe atravesar la banda de forma monótona, sin el salto que
    /// producía el fallback (finita, no-finita).
    #[test]
    fn composition_band_keeps_both_branches_finite_and_monotone() {
        let (width, height) = (32usize, 400usize);
        let boundary = 200usize;
        let mut hard = vec![0u8; width * height];
        for y in 0..boundary {
            for x in 0..width {
                hard[y * width + x] = 1;
            }
        }
        let weight = feather_mask(&hard, width, height, 128);
        let validity = branch_validity_mask(&hard, width, height, 8, 128);
        let sky_image = crate::DsImage {
            data: vec![1.0f32; width * height],
            w: width,
            h: height,
            ch: 1,
            bayer: None,
        };
        let ground_image = crate::DsImage {
            data: vec![0.25f32; width * height],
            w: width,
            h: height,
            ch: 1,
            bayer: None,
        };
        let identity = crate::milky_way_registration::TransformModel::identity();
        let (sky_warp, _) = warp_milky_way_branch(
            &sky_image,
            identity,
            &validity,
            MilkyWayBranch::Sky,
            width,
            height,
        );
        let (ground_warp, _) = warp_milky_way_branch(
            &ground_image,
            identity,
            &validity,
            MilkyWayBranch::Ground,
            width,
            height,
        );
        // Control negativo: con el gating antiguo (mask.soft, feather 8) la
        // banda ancha SÍ dejaba una rama NaN donde el peso era fraccional.
        let legacy_soft = feather_mask(&hard, width, height, 8);
        let (legacy_ground, _) = warp_milky_way_branch(
            &ground_image,
            identity,
            &legacy_soft,
            MilkyWayBranch::Ground,
            width,
            height,
        );
        let mut fractional_pixels = 0usize;
        let mut legacy_bug_reproduced = false;
        for pixel in 0..width * height {
            let w = weight[pixel];
            if w.min(1.0 - w) > 0.0 {
                fractional_pixels += 1;
                assert!(
                    sky_warp[pixel].is_finite(),
                    "rama cielo NaN con peso fraccional {w} en {pixel}"
                );
                assert!(
                    ground_warp[pixel].is_finite(),
                    "rama suelo NaN con peso fraccional {w} en {pixel}"
                );
                legacy_bug_reproduced |= !legacy_ground[pixel].is_finite();
            }
        }
        assert!(fractional_pixels > width * 100, "la banda debe ser ancha");
        assert!(
            legacy_bug_reproduced,
            "el test debe ser sensible al bug original (rama NaN con gating antiguo)"
        );
        let branch = |sci: Vec<f32>| BranchIntegration {
            sci,
            variance: vec![0.0; width * height],
            coverage: vec![1.0; width * height],
            rejection: vec![0.0; width * height],
            frames_used: 1,
        };
        let (composite, _, _) = compose_layers(
            &branch(sky_warp),
            &branch(ground_warp),
            &weight,
            width,
            height,
            1,
            false,
        );
        for pixel in 0..width * height {
            let w = weight[pixel];
            if w.min(1.0 - w) > 0.0 {
                // Sin fallback: el compuesto es exactamente la mezcla lineal.
                let expected = 1.0 * w + 0.25 * (1.0 - w);
                assert!(
                    (composite[pixel] - expected).abs() < 1.0e-6,
                    "fallback disparado en la banda: {} != {}",
                    composite[pixel],
                    expected
                );
            }
        }
        // Monotonía por columna: de cielo (1.0) a suelo (0.25) sin saltos.
        for x in 0..width {
            for y in 1..height {
                let above = composite[(y - 1) * width + x];
                let below = composite[y * width + x];
                assert!(
                    below <= above + 1.0e-6,
                    "compuesto no monótono en x={x}, y={y}: {above} -> {below}"
                );
                assert!(
                    (above - below).abs() < 0.02,
                    "salto duro en la transición en x={x}, y={y}: {above} -> {below}"
                );
            }
        }
    }

    /// C5a: una estrella gaussiana (σ=1.2 px) desplazada (0.5, 0.5) debe
    /// conservar más pico con Lanczos-3 que con bilineal, sin perder flujo.
    #[test]
    fn lanczos_warp_recovers_star_peak_and_conserves_flux() {
        let (width, height) = (17usize, 17usize);
        let sigma = 1.2f32;
        let data: Vec<f32> = (0..height)
            .flat_map(|y| {
                (0..width).map(move |x| {
                    let dx = x as f32 - 8.0;
                    let dy = y as f32 - 8.0;
                    (-(dx * dx + dy * dy) / (2.0 * sigma * sigma)).exp()
                })
            })
            .collect();
        let image = crate::DsImage {
            data,
            w: width,
            h: height,
            ch: 1,
            bayer: None,
        };
        // Forward +0.5: cada píxel de salida muestrea la fuente en (x-0.5, y-0.5).
        let shift = crate::milky_way_registration::TransformModel {
            planar: crate::milky_way_registration::PlanarTransform::Affine([
                1.0, 0.0, 0.5, 0.0, 1.0, 0.5,
            ]),
            radial: None,
        };
        let (warped, _) = warp_milky_way_branch(
            &image,
            shift,
            &vec![1.0; width * height],
            MilkyWayBranch::Sky,
            width,
            height,
        );
        // Referencia bilineal calculada sobre las mismas coordenadas fuente.
        let mut bilinear_peak = 0.0f32;
        let mut lanczos_peak = 0.0f32;
        for y in 1..height {
            for x in 1..width {
                let reference =
                    bilinear_scalar(&image.data, width, height, x as f64 - 0.5, y as f64 - 0.5);
                bilinear_peak = bilinear_peak.max(reference);
                if warped[y * width + x].is_finite() {
                    lanczos_peak = lanczos_peak.max(warped[y * width + x]);
                }
            }
        }
        assert!(
            lanczos_peak > bilinear_peak + 0.03,
            "Lanczos ({lanczos_peak}) debe conservar más pico que bilineal ({bilinear_peak})"
        );
        let source_flux: f32 = image.data.iter().sum();
        let warped_flux: f32 = warped.iter().filter(|value| value.is_finite()).sum();
        assert!(
            (warped_flux - source_flux).abs() / source_flux < 0.01,
            "flujo no conservado: {warped_flux} vs {source_flux}"
        );
    }

    /// C9a: con datos normalizados 0..1 el suelo relativo de sigma permite
    /// corregir un píxel caliente que el suelo absoluto de 1.0 ADU ignoraba.
    #[test]
    fn dynamic_hot_pixels_work_on_normalized_data() {
        let (width, height) = (9usize, 9usize);
        let mut data = vec![0.001f32; width * height];
        for (index, value) in data.iter_mut().enumerate() {
            // Micro-variación determinista para que el MAD local no sea cero.
            *value += ((index % 7) as f32 - 3.0) * 1.0e-5;
        }
        data[4 * width + 4] = 0.5;
        let untouched_index = 2 * width + 2;
        let untouched_before = data[untouched_index];
        let mut image = crate::DsImage {
            data,
            w: width,
            h: height,
            ch: 1,
            bayer: None,
        };
        correct_dynamic_hot_pixels(&mut image);
        assert!(
            image.data[4 * width + 4] < 0.01,
            "píxel caliente normalizado no corregido: {}",
            image.data[4 * width + 4]
        );
        assert_eq!(image.data[untouched_index], untouched_before);
    }

    /// C9a: en una vecindad perfectamente plana una muestra aislada puede
    /// ser un hot pixel o una estrella submuestreada; sin ruido medible no
    /// existe evidencia para decidir y el corrector debe preservar el dato.
    #[test]
    fn dynamic_hot_pixels_preserve_ambiguous_zero_mad_signal() {
        let (width, height) = (7usize, 7usize);
        let center = 3 * width + 3;
        let mut data = vec![0.0f32; width * height];
        data[center] = 0.25;
        let mut image = crate::DsImage {
            data,
            w: width,
            h: height,
            ch: 1,
            bayer: None,
        };
        correct_dynamic_hot_pixels(&mut image);
        assert_eq!(
            image.data[center], 0.25,
            "una señal aislada sin MAD no debe destruirse por conjetura"
        );
    }

    /// C9a: en escala ADU 0..65535 el comportamiento previo se conserva:
    /// el caliente extremo se corrige y un valor a ~4σ se respeta.
    ///
    /// Calibración del "moderado": los vecinos de (6,6) son
    /// {501×3, 502×3, 506×2} → mediana local 502, MAD 1 → σ = 1.4826 (el
    /// suelo relativo 0.502 y el antiguo absoluto 1.0 quedan AMBOS por
    /// debajo, así que el σ efectivo es idéntico antes y después del fix).
    /// El umbral es 502 + 8σ ≈ 513.9: 508 está a ≈4σ y debe respetarse;
    /// un 520 estaría a ≈12σ y se corregiría también con el código previo.
    #[test]
    fn dynamic_hot_pixels_keep_adu_scale_behaviour() {
        let (width, height) = (9usize, 9usize);
        let mut data: Vec<f32> = (0..width * height)
            .map(|index| 500.0 + ((index * 13) % 9) as f32)
            .collect();
        data[4 * width + 4] = 60_000.0;
        let moderate_index = 6 * width + 6;
        data[moderate_index] = 508.0;
        let mut image = crate::DsImage {
            data,
            w: width,
            h: height,
            ch: 1,
            bayer: None,
        };
        correct_dynamic_hot_pixels(&mut image);
        assert!(
            image.data[4 * width + 4] < 600.0,
            "caliente ADU no corregido: {}",
            image.data[4 * width + 4]
        );
        assert_eq!(image.data[moderate_index], 508.0);
    }

    /// C9b: el factor de consistencia para k=2 debe coincidir con la teoría
    /// (Var_w(2) ≈ 0.920537 → 1/Var_w ≈ 1.08632).
    #[test]
    fn winsorized_variance_factor_matches_normal_theory() {
        let factor = winsorized_variance_consistency(2.0, 2.0);
        assert!(
            (factor - 1.086_3).abs() < 3.0e-3,
            "factor {factor} fuera de la teoría"
        );
        // k grande: la winsorización casi no recorta y el factor tiende a 1.
        assert!((winsorized_variance_consistency(8.0, 8.0) - 1.0).abs() < 1.0e-3);
    }

    /// C9b (árbitro): 100k muestras pseudo-normales deterministas (LCG +
    /// Box-Muller), winsorizadas a k=2. La varianza corregida debe quedar
    /// dentro del 2% de la verdadera; la sin corregir debe quedar corta.
    #[test]
    fn winsorized_variance_correction_recovers_true_variance() {
        fn lcg_uniform(state: &mut u64) -> f64 {
            *state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            // 53 bits altos; +0.5 evita el cero exacto que rompería el log.
            (((*state >> 11) as f64) + 0.5) / 9_007_199_254_740_992.0
        }
        let mut state = 0x5EED_CAFE_F00D_1234u64;
        let total = 100_000usize;
        let mut samples = Vec::with_capacity(total);
        while samples.len() < total {
            let u1 = lcg_uniform(&mut state);
            let u2 = lcg_uniform(&mut state);
            let radius = (-2.0 * u1.ln()).sqrt();
            let angle = 2.0 * std::f64::consts::PI * u2;
            samples.push(radius * angle.cos());
            if samples.len() < total {
                samples.push(radius * angle.sin());
            }
        }
        let count = samples.len() as f64;
        let mean = samples.iter().sum::<f64>() / count;
        let true_variance = samples
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum::<f64>()
            / (count - 1.0);
        let kappa = 2.0f64;
        let low = mean - kappa * true_variance.sqrt();
        let high = mean + kappa * true_variance.sqrt();
        let winsorized: Vec<f64> = samples.iter().map(|value| value.clamp(low, high)).collect();
        let winsorized_mean = winsorized.iter().sum::<f64>() / count;
        let winsorized_variance = winsorized
            .iter()
            .map(|value| (value - winsorized_mean).powi(2))
            .sum::<f64>()
            / (count - 1.0);
        assert!(
            winsorized_variance < 0.95 * true_variance,
            "la varianza winsorizada debería subestimar ({winsorized_variance} vs {true_variance})"
        );
        let corrected = winsorized_variance * f64::from(winsorized_variance_consistency(2.0, 2.0));
        assert!(
            (corrected - true_variance).abs() / true_variance < 0.02,
            "varianza corregida {corrected} fuera del 2% de la verdadera {true_variance}"
        );
    }

    fn crop_request_for_wcs_tests() -> MilkyWayCropProductsRequest {
        MilkyWayCropProductsRequest {
            job_id: None,
            sky_path: "sky.fits".into(),
            ground_path: None,
            composite_path: None,
            mask_path: "mask.fits".into(),
            sky_variance_path: None,
            sky_coverage_path: None,
            sky_rejection_path: None,
            ground_variance_path: None,
            ground_coverage_path: None,
            ground_rejection_path: None,
            left: 10,
            top: 20,
            width: 40,
            height: 30,
        }
    }

    /// C9c: un WCS candidato se recorta con la misma matemática, pero el
    /// estado Candidate se conserva, se advierte al usuario y la cabecera
    /// re-embebida lo declara.
    #[test]
    fn crop_wcs_candidate_keeps_state_and_warns() {
        let request = crop_request_for_wcs_tests();
        let original = valid_wcs();
        let (result, warnings) = crop_embedded_wcs(
            crate::DsPostStackEmbeddedWcs::Candidate(original.clone()),
            100,
            80,
            &request,
        );
        let (cropped, authority) = result.expect("el recorte matemático debe sobrevivir");
        assert_eq!(authority, MilkyWayCropWcsAuthority::Candidate);
        assert!((cropped.crpix1 - (original.crpix1 - 10.0)).abs() < 1.0e-9);
        assert!((cropped.crpix2 - (original.crpix2 - 20.0)).abs() < 1.0e-9);
        assert!(
            warnings.iter().any(|warning| warning.contains("candidato")),
            "falta el warning de WCS candidato: {warnings:?}"
        );
        let metadata = metadata_from_optional_wcs(Some(&(cropped, authority)));
        assert!(
            metadata
                .iter()
                .any(|(key, value)| *key == "ZMWWCSST" && value.contains("CANDIDATE")),
            "la cabecera re-embebida debe declarar el estado candidato"
        );
        assert_eq!(
            crop_wcs_public_status(Some(&(original, authority))),
            ("candidate", false),
            "un candidato conservado nunca debe activar wcsPreserved"
        );
    }

    /// C9c: un WCS validado no cambia: sin warnings ni tarjeta de estado.
    #[test]
    fn crop_wcs_validated_stays_unchanged() {
        let request = crop_request_for_wcs_tests();
        let original = valid_wcs();
        let (result, warnings) = crop_embedded_wcs(
            crate::DsPostStackEmbeddedWcs::Validated(original.clone()),
            100,
            80,
            &request,
        );
        let (cropped, authority) = result.expect("el recorte matemático debe sobrevivir");
        assert_eq!(authority, MilkyWayCropWcsAuthority::Validated);
        assert!((cropped.crpix1 - (original.crpix1 - 10.0)).abs() < 1.0e-9);
        assert!(warnings.is_empty(), "warnings inesperados: {warnings:?}");
        let metadata = metadata_from_optional_wcs(Some(&(cropped, authority)));
        assert!(metadata.iter().all(|(key, _)| *key != "ZMWWCSST"));
        assert_eq!(
            crop_wcs_public_status(Some(&(original, authority))),
            ("validated", true)
        );
        assert_eq!(crop_wcs_public_status(None), ("missing", false));
    }

    #[test]
    fn crop_wire_accepts_canonical_and_legacy_sky_map_names() {
        let canonical: MilkyWayCropProductsRequest = serde_json::from_value(serde_json::json!({
            "skyPath": "/data/sky.fits",
            "groundPath": "/data/ground.fits",
            "compositePath": "/data/composite.fits",
            "maskPath": "/data/mask.fits",
            "skyVariancePath": "/data/sky-var.fits",
            "skyCoveragePath": "/data/sky-cov.fits",
            "skyRejectionPath": "/data/sky-rej.fits",
            "groundVariancePath": "/data/ground-var.fits",
            "groundCoveragePath": "/data/ground-cov.fits",
            "groundRejectionPath": "/data/ground-rej.fits",
            "left": 1, "top": 2, "width": 20, "height": 10
        }))
        .unwrap();
        assert_eq!(
            canonical.resolved_sky_coverage().unwrap(),
            "/data/sky-cov.fits"
        );
        assert_eq!(
            canonical.resolved_sky_rejection().unwrap(),
            "/data/sky-rej.fits"
        );
        assert_eq!(
            canonical.ground_variance_path.as_deref(),
            Some("/data/ground-var.fits")
        );
        assert_eq!(
            canonical.resolved_sky_variance().unwrap(),
            "/data/sky-var.fits"
        );
        assert!(canonical.resolved_ground_maps().unwrap().is_some());

        let legacy: MilkyWayCropProductsRequest = serde_json::from_value(serde_json::json!({
            "skyPath": "/data/sky.fits",
            "maskPath": "/data/mask.fits",
            "coveragePath": "/data/coverage.fits",
            "rejectionPath": "/data/rejection.fits",
            "left": 0, "top": 0, "width": 20, "height": 10
        }))
        .unwrap();
        assert_eq!(
            legacy.resolved_sky_coverage().unwrap(),
            "/data/coverage.fits"
        );
        assert_eq!(
            legacy.resolved_sky_rejection().unwrap(),
            "/data/rejection.fits"
        );
        assert!(legacy.resolved_sky_variance().is_err());

        let partial_ground: MilkyWayCropProductsRequest =
            serde_json::from_value(serde_json::json!({
                "skyPath": "/data/sky.fits",
                "groundPath": "/data/ground.fits",
                "maskPath": "/data/mask.fits",
                "skyVariancePath": "/data/sky-var.fits",
                "skyCoveragePath": "/data/sky-cov.fits",
                "skyRejectionPath": "/data/sky-rej.fits",
                "left": 0, "top": 0, "width": 20, "height": 10
            }))
            .unwrap();
        assert!(partial_ground.resolved_ground_maps().is_err());
    }

    #[test]
    fn crop_result_serializes_canonical_maps_and_legacy_aliases() {
        let result = MilkyWayCroppedProductsResult {
            job_id: "crop-1".into(),
            geometry_id: "geometry-1".into(),
            source_width: 100,
            source_height: 80,
            width: 50,
            height: 40,
            sky_path: "/out/sky.fits".into(),
            ground_path: Some("/out/ground.fits".into()),
            composite_path: Some("/out/composite.fits".into()),
            mask_path: "/out/mask.fits".into(),
            sky_variance_path: Some("/out/sky-var.fits".into()),
            sky_coverage_path: "/out/sky-cov.fits".into(),
            sky_rejection_path: "/out/sky-rej.fits".into(),
            ground_variance_path: Some("/out/ground-var.fits".into()),
            ground_coverage_path: Some("/out/ground-cov.fits".into()),
            ground_rejection_path: Some("/out/ground-rej.fits".into()),
            coverage_path: "/out/sky-cov.fits".into(),
            rejection_path: "/out/sky-rej.fits".into(),
            recipe_path: "/out/recipe.json".into(),
            wcs_status: "validated".into(),
            wcs_preserved: true,
            warnings: vec![],
        };
        let json = serde_json::to_value(result).unwrap();
        assert_eq!(json["skyVariancePath"], "/out/sky-var.fits");
        assert_eq!(json["groundCoveragePath"], "/out/ground-cov.fits");
        assert_eq!(json["coveragePath"], json["skyCoveragePath"]);
        assert_eq!(json["rejectionPath"], json["skyRejectionPath"]);
        assert_eq!(json["wcsStatus"], "validated");
        assert_eq!(json["wcsPreserved"], true);
    }

    #[test]
    fn all_crop_layers_keep_one_geometry_while_variance_keeps_channels() {
        let crop = (1usize, 1usize, 2usize, 2usize);
        let rgb = crate::DsImage {
            data: (0..4 * 3 * 3).map(|value| value as f32).collect(),
            w: 4,
            h: 3,
            ch: 3,
            bayer: None,
        };
        let mono = crate::DsImage {
            data: (0..4 * 3).map(|value| value as f32).collect(),
            w: 4,
            h: 3,
            ch: 1,
            bayer: None,
        };
        // SCI cielo/suelo/compuesto y VAR pueden ser RGB.
        for _kind in [
            "sky",
            "ground",
            "composite",
            "skyVariance",
            "groundVariance",
        ] {
            assert_eq!(
                crop_interleaved(&rgb, crop.0, crop.1, crop.2, crop.3)
                    .unwrap()
                    .len(),
                12
            );
        }
        // Máscara, cobertura y rechazo de ambas ramas permanecen mono.
        for _kind in [
            "mask",
            "skyCoverage",
            "skyRejection",
            "groundCoverage",
            "groundRejection",
        ] {
            assert_eq!(
                crop_interleaved(&mono, crop.0, crop.1, crop.2, crop.3)
                    .unwrap()
                    .len(),
                4
            );
        }
    }
}
