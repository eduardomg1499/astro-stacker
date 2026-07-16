use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

/// Política pública de cómputo. `Auto` y `Hybrid` permiten fallback seguro;
/// `GpuOnly` nunca debe ocultar un fallo de paridad/VRAM/device.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComputePolicy {
    Auto,
    #[default]
    Hybrid,
    CpuOnly,
    GpuOnly,
}

impl ComputePolicy {
    pub fn allows_gpu(self) -> bool {
        !matches!(self, Self::CpuOnly)
    }

    pub fn allows_fallback(self) -> bool {
        matches!(self, Self::Auto | Self::Hybrid)
    }

    pub fn from_legacy(value: Option<&str>) -> Self {
        match value.unwrap_or("hybrid").to_ascii_lowercase().as_str() {
            "cpu" | "cpu_only" => Self::CpuOnly,
            "gpu" | "gpu_only" => Self::GpuOnly,
            "hybrid" => Self::Hybrid,
            _ => Self::Auto,
        }
    }

    pub fn legacy_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Hybrid => "hybrid",
            Self::CpuOnly => "cpu",
            Self::GpuOnly => "gpu",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComputeCapability {
    pub gpu_available: bool,
    pub parity_ok: bool,
    pub required_vram_mb: u64,
    pub vram_budget_mb: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComputeResolution {
    pub use_gpu: bool,
    pub fallback_reason: Option<String>,
}

/// Resuelve la semántica común de Auto/Hybrid/CPU/GPU-only sin depender de
/// un adapter físico. Los planificadores usan este contrato y los tests pueden
/// reproducir GPU ausente, paridad fallida y VRAM insuficiente.
pub fn resolve_compute_policy(
    policy: ComputePolicy,
    capability: &ComputeCapability,
) -> Result<ComputeResolution, String> {
    if matches!(policy, ComputePolicy::CpuOnly) {
        return Ok(ComputeResolution {
            use_gpu: false,
            fallback_reason: None,
        });
    }
    let reason = if !capability.gpu_available {
        Some("GPU compute ausente".to_string())
    } else if !capability.parity_ok {
        Some("self-test de paridad GPU fallido".to_string())
    } else if capability.required_vram_mb > capability.vram_budget_mb {
        Some(format!(
            "VRAM insuficiente: requiere {} MB, presupuesto {} MB",
            capability.required_vram_mb, capability.vram_budget_mb
        ))
    } else {
        None
    };
    if let Some(reason) = reason {
        if matches!(policy, ComputePolicy::GpuOnly) {
            Err(format!("GPU only: {reason}"))
        } else {
            Ok(ComputeResolution {
                use_gpu: false,
                fallback_reason: Some(reason),
            })
        }
    } else {
        Ok(ComputeResolution {
            use_gpu: true,
            fallback_reason: None,
        })
    }
}

pub fn cancellation_checkpoint(
    cancelled: &std::sync::atomic::AtomicBool,
    phase: &str,
) -> Result<(), String> {
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        Err(format!("Cancelado por el usuario durante {phase}"))
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PipelineProfile {
    Fast,
    #[default]
    Balanced,
    MaximumQuality,
    Custom,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PipelineDomain {
    Planetary,
    DeepSky,
}

/// Petición tipada para el primer paso planetario. Los comandos legados se
/// mantienen una versión, pero la UI nueva ya no necesita una lista posicional
/// de argumentos sin contrato.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanetaryAnalysisRequest {
    pub path: String,
    #[serde(default)]
    pub is_surface: bool,
    #[serde(default = "default_planet_target")]
    pub target_type: String,
    #[serde(default = "default_true")]
    pub warping_analysis: bool,
    #[serde(default)]
    pub bayer_override: Option<i32>,
    #[serde(default)]
    pub anchor_override: Option<Vec<i32>>,
    #[serde(default)]
    pub progress_prefix: Option<String>,
    #[serde(default)]
    pub compute_policy: ComputePolicy,
    #[serde(default)]
    pub profile: PipelineProfile,
}

fn default_planet_target() -> String {
    "general".into()
}

impl PlanetaryAnalysisRequest {
    /// Los perfiles sólo fijan decisiones algorítmicas; `Custom` conserva
    /// exactamente lo solicitado por el cliente.
    pub fn resolved_profile(mut self) -> Self {
        match self.profile {
            PipelineProfile::Fast => self.warping_analysis = false,
            PipelineProfile::MaximumQuality => self.warping_analysis = true,
            PipelineProfile::Balanced | PipelineProfile::Custom => {}
        }
        self
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanetaryStackRequest {
    pub path: String,
    #[serde(default = "default_planet_percent")]
    pub percent: f32,
    #[serde(default)]
    pub custom_points: Vec<crate::smart_grid::ApPoint>,
    #[serde(default = "default_drizzle")]
    pub drizzle: f32,
    #[serde(default)]
    pub is_surface: bool,
    #[serde(default)]
    pub bayer_override: Option<i32>,
    #[serde(default = "default_ap_size_u32")]
    pub ap_size: u32,
    #[serde(default)]
    pub sharpened: bool,
    #[serde(default = "default_sharpen_intensity")]
    pub sharpen_intensity: f32,
    // PR-1.3: doble pasada POR DEFECTO — activa la rejection kappa-sigma y
    // la re-alineación contra el stack de la pasada 1 (estilo AS!4). El
    // perfil Fast la sigue desactivando explícitamente.
    #[serde(default = "default_true_flag")]
    pub double_pass: bool,
    #[serde(default = "default_true")]
    pub warping_analysis: bool,
    #[serde(default)]
    pub anchor_override: Option<Vec<i32>>,
    #[serde(default)]
    pub stacking_roi: Option<Vec<u32>>,
    #[serde(default = "default_true")]
    pub normalize_colors: bool,
    #[serde(default = "default_true")]
    pub is_v3: bool,
    #[serde(default = "default_planet_target")]
    pub target_type: String,
    #[serde(default)]
    pub keep_full_frame: Option<bool>,
    #[serde(default)]
    pub align_rgb: Option<bool>,
    #[serde(default)]
    pub compute_policy: ComputePolicy,
    #[serde(default)]
    pub profile: PipelineProfile,
}

fn default_planet_percent() -> f32 {
    15.0
}
fn default_ap_size_u32() -> u32 {
    48
}
fn default_sharpen_intensity() -> f32 {
    0.5
}

/// Límites del contrato planetario expuesto por la UI. Mantenerlos aquí evita
/// que los comandos tipados, los wrappers legados y el modo batch acepten
/// combinaciones distintas (o valores JSON no finitos).
pub(crate) const PLANETARY_MIN_AP_SIZE: usize = 8;
pub(crate) const PLANETARY_MAX_AP_SIZE: usize = 104;
pub(crate) const PLANETARY_MAX_CUSTOM_AP_POINTS: usize = 20_000;
const PLANETARY_DRIZZLE_FACTORS: [f32; 5] = [1.0, 1.5, 2.0, 3.0, 4.0];

#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_planetary_stack_parameters(
    path: &str,
    percent: f32,
    custom_points: &[crate::smart_grid::ApPoint],
    drizzle: f32,
    ap_size: u32,
    sharpen_intensity: f32,
    anchor_override: Option<&[i32]>,
    stacking_roi: Option<&[u32]>,
) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("La ruta del origen planetario no puede estar vacía".into());
    }
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) || percent == 0.0 {
        return Err(format!(
            "El porcentaje a apilar debe ser finito y estar en (0, 100]; recibido {percent}"
        ));
    }
    if !drizzle.is_finite()
        || !PLANETARY_DRIZZLE_FACTORS
            .iter()
            .any(|&supported| (drizzle - supported).abs() <= 1.0e-6)
    {
        return Err(format!(
            "Drizzle {drizzle} no soportado; usa únicamente 1x, 1.5x, 2x, 3x o 4x"
        ));
    }
    if !(PLANETARY_MIN_AP_SIZE..=PLANETARY_MAX_AP_SIZE).contains(&(ap_size as usize)) {
        return Err(format!(
            "AP size debe estar entre {PLANETARY_MIN_AP_SIZE} y {PLANETARY_MAX_AP_SIZE} píxeles; recibido {ap_size}"
        ));
    }
    if !sharpen_intensity.is_finite() || !(0.0..=1.0).contains(&sharpen_intensity) {
        return Err(format!(
            "La intensidad de sharpening debe ser finita y estar entre 0 y 1; recibido {sharpen_intensity}"
        ));
    }
    if custom_points.len() > PLANETARY_MAX_CUSTOM_AP_POINTS {
        return Err(format!(
            "La malla contiene {} puntos AP; el máximo seguro es {PLANETARY_MAX_CUSTOM_AP_POINTS}",
            custom_points.len()
        ));
    }
    for (index, point) in custom_points.iter().enumerate() {
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err(format!("El punto AP #{index} contiene coordenadas no finitas"));
        }
        if !(PLANETARY_MIN_AP_SIZE..=PLANETARY_MAX_AP_SIZE).contains(&point.size) {
            return Err(format!(
                "El punto AP #{index} tiene tamaño {}; debe estar entre {PLANETARY_MIN_AP_SIZE} y {PLANETARY_MAX_AP_SIZE}",
                point.size
            ));
        }
    }
    if let Some(anchor) = anchor_override {
        if anchor.len() != 2 {
            return Err(format!(
                "El ancla manual debe contener exactamente [x, y]; recibió {} valores",
                anchor.len()
            ));
        }
    }
    if let Some(roi) = stacking_roi {
        if roi.len() != 4 {
            return Err(format!(
                "La ROI de apilado debe contener exactamente [x, y, ancho, alto]; recibió {} valores",
                roi.len()
            ));
        }
        if roi[2] == 0 || roi[3] == 0 {
            return Err("La ROI de apilado debe tener ancho y alto mayores que cero".into());
        }
    }
    Ok(())
}

pub(crate) fn validate_planetary_stack_geometry(
    width: usize,
    height: usize,
    custom_points: &[crate::smart_grid::ApPoint],
    anchor_override: Option<&[i32]>,
    stacking_roi: Option<&[u32]>,
) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err(format!(
            "El origen planetario declaró dimensiones inválidas: {width}x{height}"
        ));
    }
    if let Some(anchor) = anchor_override {
        // La longitud ya se valida en el contrato estático, pero se repite el
        // guard antes de indexar para que esta función también sea segura sola.
        if anchor.len() != 2 {
            return Err("El ancla manual debe contener exactamente [x, y]".into());
        }
        if anchor[0] < 0
            || anchor[1] < 0
            || anchor[0] as usize >= width
            || anchor[1] as usize >= height
        {
            return Err(format!(
                "El ancla manual ({}, {}) queda fuera del origen {width}x{height}",
                anchor[0], anchor[1]
            ));
        }
    }
    if let Some(roi) = stacking_roi {
        if roi.len() != 4 {
            return Err("La ROI de apilado debe contener exactamente [x, y, ancho, alto]".into());
        }
        let (x, y, roi_w, roi_h) = (roi[0] as u64, roi[1] as u64, roi[2] as u64, roi[3] as u64);
        if roi_w == 0 || roi_h == 0 {
            return Err("La ROI de apilado debe tener ancho y alto mayores que cero".into());
        }
        let right = x
            .checked_add(roi_w)
            .ok_or("La coordenada horizontal de la ROI se desbordó")?;
        let bottom = y
            .checked_add(roi_h)
            .ok_or("La coordenada vertical de la ROI se desbordó")?;
        if right > width as u64 || bottom > height as u64 {
            return Err(format!(
                "La ROI [{x}, {y}, {roi_w}, {roi_h}] queda fuera del origen {width}x{height}"
            ));
        }
    }
    for (index, point) in custom_points.iter().enumerate() {
        if !point.x.is_finite()
            || !point.y.is_finite()
            || point.x < 0.0
            || point.y < 0.0
            || point.x >= width as f32
            || point.y >= height as f32
        {
            return Err(format!(
                "El punto AP #{index} ({}, {}) queda fuera del origen {width}x{height}",
                point.x, point.y
            ));
        }
    }
    Ok(())
}

pub(crate) fn planetary_output_dimensions(
    width: usize,
    height: usize,
    stacking_roi: Option<&[u32]>,
    drizzle: f32,
) -> Result<(usize, usize), String> {
    if !drizzle.is_finite()
        || !PLANETARY_DRIZZLE_FACTORS
            .iter()
            .any(|&supported| (drizzle - supported).abs() <= 1.0e-6)
    {
        return Err("No se pueden calcular dimensiones con un drizzle no soportado".into());
    }
    let (base_w, base_h) = if let Some(roi) = stacking_roi {
        if roi.len() != 4 || roi[2] == 0 || roi[3] == 0 {
            return Err("La ROI de apilado no tiene el contrato [x, y, ancho, alto] válido".into());
        }
        (roi[2] as usize, roi[3] as usize)
    } else {
        (width, height)
    };
    let output_w = (base_w as f64 * drizzle as f64).floor();
    let output_h = (base_h as f64 * drizzle as f64).floor();
    if output_w < 1.0
        || output_h < 1.0
        || output_w > usize::MAX as f64
        || output_h > usize::MAX as f64
    {
        return Err("Las dimensiones de salida planetaria se desbordaron".into());
    }
    let output = (output_w as usize, output_h as usize);
    output
        .0
        .checked_mul(output.1)
        .ok_or("La cantidad de píxeles de salida planetaria se desbordó")?;
    Ok(output)
}

impl PlanetaryStackRequest {
    pub fn validate_static(&self) -> Result<(), String> {
        validate_planetary_stack_parameters(
            &self.path,
            self.percent,
            &self.custom_points,
            self.drizzle,
            self.ap_size,
            self.sharpen_intensity,
            self.anchor_override.as_deref(),
            self.stacking_roi.as_deref(),
        )
    }

    pub fn validate_for_source(&self, width: usize, height: usize) -> Result<(), String> {
        self.validate_static()?;
        validate_planetary_stack_geometry(
            width,
            height,
            &self.custom_points,
            self.anchor_override.as_deref(),
            self.stacking_roi.as_deref(),
        )
    }

    pub fn resolved_profile(mut self) -> Self {
        match self.profile {
            PipelineProfile::Fast => {
                self.double_pass = false;
                self.warping_analysis = false;
                self.drizzle = 1.0;
            }
            PipelineProfile::MaximumQuality => {
                self.double_pass = true;
                self.warping_analysis = true;
                self.normalize_colors = true;
            }
            PipelineProfile::Balanced | PipelineProfile::Custom => {}
        }
        self
    }
}

/// Evento común de rendimiento. Los campos opcionales evitan inventar datos
/// cuando una fase no utiliza GPU, caché o un contador de frames.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PipelineTelemetry {
    pub job_id: String,
    pub domain: PipelineDomain,
    pub phase: String,
    pub engine: String,
    pub progress: f32,
    pub eta_seconds: Option<f32>,
    pub items_done: usize,
    pub items_total: usize,
    pub throughput: Option<f32>,
    pub cpu_percent: Option<f32>,
    pub gpu_percent: Option<f32>,
    pub ram_mb: u64,
    pub vram_mb: u64,
    pub io_read_mb: f64,
    pub io_write_mb: f64,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub fallback_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedPipelineTelemetry {
    pub monotonic_ms: u128,
    pub captured_at_utc: String,
    #[serde(flatten)]
    pub event: PipelineTelemetry,
}

static TELEMETRY_START: OnceLock<std::time::Instant> = OnceLock::new();
static TELEMETRY_LOG: OnceLock<Mutex<Vec<RecordedPipelineTelemetry>>> = OnceLock::new();

pub fn record_pipeline_telemetry(event: &PipelineTelemetry) {
    let start = TELEMETRY_START.get_or_init(std::time::Instant::now);
    let log = TELEMETRY_LOG.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut events) = log.lock() {
        // Límite defensivo: suficiente para miles de jobs sin crecimiento
        // indefinido en sesiones largas.
        if events.len() >= 100_000 {
            events.drain(..10_000);
        }
        events.push(RecordedPipelineTelemetry {
            monotonic_ms: start.elapsed().as_millis(),
            captured_at_utc: chrono::Utc::now().to_rfc3339(),
            event: event.clone(),
        });
    }
}

pub fn telemetry_snapshot(job_id: Option<&str>) -> Vec<RecordedPipelineTelemetry> {
    TELEMETRY_LOG
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .map(|events| {
            events
                .iter()
                .filter(|e| job_id.is_none_or(|id| e.event.job_id == id))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

pub fn clear_telemetry(job_id: Option<&str>) -> usize {
    TELEMETRY_LOG
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .map(|mut events| {
            let before = events.len();
            if let Some(id) = job_id {
                events.retain(|e| e.event.job_id != id);
            } else {
                events.clear();
            }
            before - events.len()
        })
        .unwrap_or(0)
}

/// Método de integración versionado (F1 del plan NebulaFusion/EIDR).
///
/// `None` en el request ⇒ `Classic` derivado de los campos planos actuales
/// (compatibilidad total con la UI/recetas existentes). Los motores nuevos
/// se seleccionan EXPLÍCITAMENTE; jamás hay reinterpretación silenciosa.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum DeepSkyIntegrationMethod {
    /// Motor actual (streaming/tiled/GPU + drizzle clásico). La config espeja
    /// los campos planos del request; `legacy_local_fwhm` migra el antiguo
    /// `localWeighting` con aviso.
    Classic(ClassicIntegrationConfig),
    /// NebulaFusion: coadición con varianza propagada (Lite) y PSF objetivo
    /// por frecuencia (Full). Disponible a partir de F3.
    NebulaFusion(NebulaFusionConfig),
    /// EIDR: reconstrucción forward-model sucesora de Drizzle. Disponible a
    /// partir de F9.
    Eidr(EidrConfig),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClassicIntegrationConfig {
    #[serde(default)]
    pub version: u16,
    /// Migración del antiguo `localWeighting` (rejilla FWHM 8×8 experimental).
    /// Se conserva con nombre explícito para no reinterpretarlo como
    /// NebulaFusion; la UI muestra aviso de característica legada.
    #[serde(default)]
    pub legacy_local_fwhm: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum NebulaFusionMode {
    Lite,
    Full,
    FullWithStruct,
}

impl Default for NebulaFusionMode {
    fn default() -> Self {
        NebulaFusionMode::Lite
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum OutputBinning {
    Native,
    Bin0_75,
    Bin0_5,
}

impl Default for OutputBinning {
    fn default() -> Self {
        OutputBinning::Native
    }
}

fn default_nf_tile_size() -> u16 {
    512
}
fn default_nf_psf_leakage() -> f32 {
    1e-3
}
fn default_nf_noise_amplification() -> f32 {
    1.5
}
fn default_nf_crossfit_folds() -> u8 {
    4
}
fn default_nf_fdr_q() -> f32 {
    0.01
}
fn default_nf_min_split_sigma() -> f32 {
    2.5
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NebulaFusionConfig {
    #[serde(default)]
    pub version: u16,
    #[serde(default)]
    pub mode: NebulaFusionMode,
    /// CFA directo (sin debayer, por fotodiodo) — F4. `false` = ruta
    /// demosaiced float32 actual, rotulada `demosaiced_input=true`.
    #[serde(default)]
    pub cfa_direct: bool,
    #[serde(default = "default_nf_tile_size")]
    pub tile_size: u16,
    #[serde(default = "default_nf_psf_leakage")]
    pub max_psf_leakage: f32,
    #[serde(default = "default_nf_noise_amplification")]
    pub max_noise_amplification: f32,
    #[serde(default)]
    pub empirical_psd: bool,
    #[serde(default = "default_nf_crossfit_folds")]
    pub crossfit_folds: u8,
    #[serde(default = "default_nf_fdr_q")]
    pub fdr_q: f32,
    #[serde(default = "default_nf_min_split_sigma")]
    pub min_split_sigma: f32,
    #[serde(default)]
    pub output_bin: OutputBinning,
}

impl Default for NebulaFusionConfig {
    fn default() -> Self {
        Self {
            version: 1,
            mode: NebulaFusionMode::default(),
            cfa_direct: false,
            tile_size: default_nf_tile_size(),
            max_psf_leakage: default_nf_psf_leakage(),
            max_noise_amplification: default_nf_noise_amplification(),
            empirical_psd: false,
            crossfit_folds: default_nf_crossfit_folds(),
            fdr_q: default_nf_fdr_q(),
            min_split_sigma: default_nf_min_split_sigma(),
            output_bin: OutputBinning::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum EidrScalePolicy {
    Auto,
    X1,
    X1_5,
    X2,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum EidrSolveMode {
    /// Máscaras/PSF/registro congelados, pérdida L2 y regularización
    /// cuadrática: para parámetros fijos la solución es un operador lineal.
    ScientificQuadratic,
    /// Huber + TGV. Experimental hasta validar falsos positivos.
    ExperimentalDetail,
}

fn default_eidr_max_iterations() -> u16 {
    60
}
fn default_eidr_huber_delta() -> f32 {
    2.5
}
fn default_eidr_holdout() -> f32 {
    0.15
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EidrConfig {
    #[serde(default)]
    pub version: u16,
    pub scale: EidrScalePolicy,
    pub solve_mode: EidrSolveMode,
    #[serde(default = "default_eidr_max_iterations")]
    pub max_iterations: u16,
    #[serde(default = "default_eidr_huber_delta")]
    pub huber_delta: f32,
    #[serde(default = "default_eidr_holdout")]
    pub holdout_fraction: f32,
    #[serde(default)]
    pub refine_registration: bool,
    #[serde(default)]
    pub refine_psf: bool,
    #[serde(default = "crate::pipeline::default_true_flag")]
    pub warm_start: bool,
    #[serde(default = "crate::pipeline::default_true_flag")]
    pub multigrid: bool,
}

pub fn default_true_flag() -> bool {
    true
}

impl DeepSkyIntegrationMethod {
    /// Etiqueta corta para receta/telemetría/UI.
    pub fn label(&self) -> &'static str {
        match self {
            DeepSkyIntegrationMethod::Classic(_) => "classic",
            DeepSkyIntegrationMethod::NebulaFusion(_) => "nebula_fusion",
            DeepSkyIntegrationMethod::Eidr(_) => "eidr",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkyStackRequest {
    pub lights: Vec<String>,
    #[serde(default)]
    pub darks: Vec<String>,
    #[serde(default)]
    pub flats: Vec<String>,
    #[serde(default)]
    pub bias: Vec<String>,
    #[serde(default)]
    pub compute_policy: ComputePolicy,
    #[serde(default)]
    pub profile: PipelineProfile,
    #[serde(default = "default_rejection")]
    pub rejection: String,
    #[serde(default = "default_kappa")]
    pub kappa_low: f32,
    #[serde(default = "default_kappa")]
    pub kappa_high: f32,
    #[serde(default)]
    pub clip_iters: Option<u32>,
    #[serde(default = "default_normalization")]
    pub normalization: String,
    #[serde(default = "default_interpolation")]
    pub interpolation: String,
    #[serde(default = "default_drizzle")]
    pub drizzle: f32,
    #[serde(default = "default_pixfrac")]
    pub pixfrac: f32,
    #[serde(default)]
    pub cosmetic: Option<bool>,
    #[serde(default)]
    pub gradient: bool,
    #[serde(default)]
    pub optimize_dark: Option<bool>,
    #[serde(default = "default_true")]
    pub auto_crop: bool,
    #[serde(default)]
    pub pedestal: Option<f32>,
    /// RESCATE DE DETALLE (experimental, lucky-DSO): pondera cada toma POR
    /// REGIÓN según su FWHM local — el máster toma más señal de las tomas y
    /// zonas con mejor seeing. Media ponderada lineal: fotometría intacta.
    #[serde(default)]
    pub local_weighting: bool,
    /// Carpeta de TRABAJO Y SALIDA (estilo PixInsight): cachés de frames
    /// calibrados (varios GB), masters y resultados. None = junto al sistema
    /// (temp) y a los lights, como siempre.
    #[serde(default)]
    pub work_dir: Option<String>,
    /// Método de integración versionado. `None` ⇒ Classic derivado de los
    /// campos planos de arriba (compatibilidad con UI/recetas existentes).
    #[serde(default)]
    pub integration_method: Option<DeepSkyIntegrationMethod>,
    /// Exporta también los productos científicos (VAR/NEFF/DQ/…) cuando el
    /// motor los produce. `false` = comportamiento clásico exacto.
    #[serde(default)]
    pub scientific_products: bool,
}

impl DeepSkyStackRequest {
    /// Método efectivo: el solicitado, o Classic espejando los campos planos
    /// (incluida la migración de `localWeighting` → `legacy_local_fwhm`).
    pub fn resolved_integration_method(&self) -> DeepSkyIntegrationMethod {
        self.integration_method.clone().unwrap_or_else(|| {
            DeepSkyIntegrationMethod::Classic(ClassicIntegrationConfig {
                version: 1,
                legacy_local_fwhm: self.local_weighting,
            })
        })
    }
}

fn default_rejection() -> String {
    // Winsorized sigma clipping (PixInsight default): median-centered and
    // per-channel via the tiled engine — robust to satellites/cosmic rays without
    // the mean-centering bias of plain sigma. Fast profile still opts into the
    // lighter streaming sigma for speed.
    "winsorized".into()
}
fn default_normalization() -> String {
    "scaling".into()
}
fn default_interpolation() -> String {
    "lanczos3".into()
}
fn default_kappa() -> f32 {
    3.0
}
fn default_drizzle() -> f32 {
    1.0
}
fn default_pixfrac() -> f32 {
    0.8
}
fn default_true() -> bool {
    true
}

impl DeepSkyStackRequest {
    /// Resuelve las tres recetas simples en backend para que CLI/API y UI
    /// ejecuten el mismo plan. `Custom` es la única variante que respeta cada
    /// control avanzado sin sobrescribirlo.
    pub fn resolved_profile(mut self) -> Self {
        match self.profile {
            PipelineProfile::Fast => {
                self.rejection = "sigma".into();
                self.kappa_low = 3.0;
                self.kappa_high = 3.0;
                self.clip_iters = Some(1);
                self.normalization = "additive".into();
                self.interpolation = "bilinear".into();
                // Drizzle NO se resetea: es una decisión separada (depende del
                // dithering del usuario, no del perfil) — antes cualquier
                // petición con perfil no-Custom perdía el drizzle en silencio.
                self.cosmetic = Some(true);
                self.gradient = false;
                self.optimize_dark = Some(true);
                self.auto_crop = true;
            }
            PipelineProfile::Balanced => {
                // Winsorized (tiled, per-channel, median-centered) — WBPP-grade
                // rejection as the default balanced path, not mean-centered sigma.
                self.rejection = "winsorized".into();
                self.kappa_low = 3.0;
                self.kappa_high = 3.0;
                self.clip_iters = None;
                self.normalization = "scaling".into();
                self.interpolation = "lanczos3".into();
                self.cosmetic = Some(true);
                self.gradient = false;
                self.optimize_dark = Some(true);
                self.auto_crop = true;
            }
            PipelineProfile::MaximumQuality => {
                self.rejection = "winsorized".into();
                self.kappa_low = 2.5;
                self.kappa_high = 3.0;
                self.clip_iters = Some(3);
                self.normalization = "local".into();
                self.interpolation = "lanczos3".into();
                self.cosmetic = Some(true);
                self.gradient = false;
                self.optimize_dark = Some(true);
                self.auto_crop = true;
            }
            PipelineProfile::Custom => {}
        }
        self
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedStackGroup {
    pub key: String,
    pub frame_count: usize,
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub bayer_pattern: Option<String>,
    pub filter: Option<String>,
    pub exposure_seconds: Option<f32>,
    pub gain: Option<f32>,
    pub binning: Option<i32>,
    pub temperature_c: Option<f32>,
}

/// Relación lights↔calibración POR SESIÓN (estilo PixInsight): qué noche de
/// flats calibra cada noche de lights, con exposición total y distancia en
/// días (una distancia grande delata flats de otra época o un DATE-OBS roto).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMapEntry {
    pub night: String,
    pub lights: usize,
    pub exposure_seconds: f32,
    pub flat_night: Option<String>,
    pub flat_count: usize,
    pub flat_distance_days: i64,
    pub darks: String,
}

/// Asesor de muestreo (F2): FWHM mediana medida en un light representativo y
/// recomendación de escala de salida. Cubre submuestreo (candidato EIDR) y
/// SOBREMUESTREO (super-binning), hueco del documento técnico original.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SamplingAdvisorReport {
    pub fwhm_median_px: f32,
    pub stars_measured: usize,
    pub sampled_frame: String,
    /// "undersampled" | "well_sampled" | "oversampled"
    pub classification: String,
    /// "0.5x" | "0.75x" | "1x" | "1.5x" | "2x"
    pub recommended_scale: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedStackPlan {
    pub plan_id: String,
    pub valid: bool,
    pub groups: Vec<PreparedStackGroup>,
    pub recommended_profile: PipelineProfile,
    pub recommendation_reasons: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub compute_policy: ComputePolicy,
    pub effective_engine: String,
    pub requested_rejection: String,
    pub effective_rejection: String,
    pub gpu_name: Option<String>,
    pub estimated_ram_mb: u64,
    pub estimated_vram_mb: u64,
    pub estimated_disk_mb: u64,
    pub estimated_seconds: f32,
    pub stages: BTreeMap<String, String>,
    pub normalization_model: BTreeMap<String, String>,
    /// Matriz de calibración por sesión (vacía cuando no hay metadatos de
    /// fecha o el plan no es válido).
    pub session_map: Vec<SessionMapEntry>,
    /// `false` cuando algún light carece de linealidad demostrable (PNG/JPEG
    /// con gamma/cuantización de display). El motor clásico los sigue
    /// aceptando; los motores científicos (NebulaFusion/EIDR) los bloquearán.
    pub scientific_eligible: bool,
    /// Diagnóstico de muestreo (None si no se pudieron medir estrellas).
    pub sampling_advisor: Option<SamplingAdvisorReport>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkyResultHandle {
    pub result_id: String,
    pub preview_path: String,
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub linear: bool,
    pub engine: String,
    pub frames_used: usize,
    pub frames_rejected: usize,
    pub elapsed_seconds: f32,
    pub recipe_path: Option<String>,
}

/// Una integración científica dentro de una sesión multibanda. Cada grupo se
/// calibra, registra e integra de forma independiente, pero comparte receta,
/// telemetría y carpeta de resultados con el resto de la sesión.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkySessionGroupRequest {
    pub id: String,
    pub label: String,
    pub filter_profile: String,
    pub request: DeepSkyStackRequest,
}

fn default_oiii_green_weight() -> f32 {
    0.65
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DualBandExtractionOptions {
    #[serde(default = "default_oiii_green_weight")]
    pub oiii_green_weight: f32,
    #[serde(default)]
    pub crosstalk_suppression: f32,
}

impl Default for DualBandExtractionOptions {
    fn default() -> Self {
        Self {
            oiii_green_weight: default_oiii_green_weight(),
            crosstalk_suppression: 0.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkySessionStackRequest {
    pub groups: Vec<DeepSkySessionGroupRequest>,
    pub base_path: String,
    #[serde(default)]
    pub extraction: DualBandExtractionOptions,
    #[serde(default)]
    pub palette: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedDeepSkySessionGroup {
    pub id: String,
    pub label: String,
    pub filter_profile: String,
    pub component_filters: Vec<String>,
    pub plan: PreparedStackPlan,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedDeepSkySessionPlan {
    pub session_id: String,
    pub valid: bool,
    pub groups: Vec<PreparedDeepSkySessionGroup>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub estimated_ram_mb: u64,
    pub estimated_vram_mb: u64,
    pub estimated_disk_mb: u64,
    pub estimated_seconds: f32,
    pub total_frames: usize,
    pub component_filters: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkyQualitySummary {
    pub grade: String,
    pub coverage_percent: f32,
    pub rejection_percent: f32,
    pub background_noise: f32,
    pub recommendations: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkySessionGroupResult {
    pub id: String,
    pub label: String,
    pub filter_profile: String,
    pub master_fits: String,
    pub preview_path: String,
    pub component_paths: BTreeMap<String, String>,
    pub diagnostic_paths: BTreeMap<String, String>,
    pub frames_used: usize,
    pub frames_rejected: usize,
    pub elapsed_seconds: f32,
    pub quality: DeepSkyQualitySummary,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkySessionResultHandle {
    pub session_id: String,
    pub output_dir: String,
    pub preview_path: String,
    pub groups: Vec<DeepSkySessionGroupResult>,
    pub component_paths: BTreeMap<String, Vec<String>>,
    pub frames_used: usize,
    pub frames_rejected: usize,
    pub elapsed_seconds: f32,
    pub warnings: Vec<String>,
    pub recipe_path: String,
}

/// Resultado científico separado del StackResult planetario u16. Los mapas se
/// mantienen en la misma geometría del máster y se exportan bajo demanda.
#[derive(Clone, Debug)]
pub struct DeepSkyResult {
    pub id: String,
    pub data: Vec<f32>,
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub coverage: Vec<f32>,
    pub weight: Vec<f32>,
    pub rejection_low: Vec<f32>,
    pub rejection_high: Vec<f32>,
    pub registration_residuals: Vec<f32>,
    pub engine: String,
    pub method: String,
    pub frames_used: usize,
    pub frames_rejected: usize,
    pub elapsed_seconds: f32,
    pub recipe: serde_json::Value,
    /// Productos científicos (motores NebulaFusion/EIDR; None en clásico).
    /// VAR y NEFF comparten el layout interleaved del máster; DQ es u32 por
    /// píxel con los bits de `deepsky_variance::dq`.
    pub variance: Option<Vec<f32>>,
    pub neff: Option<Vec<f32>>,
    pub dq: Option<Vec<u32>>,
    /// STRUCT (F7): mapa de evidencia multiescala validado A/B y su residual
    /// (planos LUMA de w*h; None fuera del modo FullWithStruct).
    pub struct_map: Option<Vec<f32>>,
    pub struct_residual: Option<Vec<f32>>,
}

/// Alias transitorio para los módulos internos previos a la API tipada v2.
/// La interfaz pública nueva se denomina `DeepSkyResult` como especifica el
/// contrato; se retirará el alias al eliminar los wrappers legados.
pub type DeepSkyLinearResult = DeepSkyResult;

pub fn new_job_id(prefix: &str) -> String {
    static JOB_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let sequence = JOB_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{prefix}-{millis}-{}-{sequence}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_planetary_stack_request() -> PlanetaryStackRequest {
        PlanetaryStackRequest {
            path: "/tmp/capture.ser".into(),
            percent: 15.0,
            custom_points: vec![crate::smart_grid::ApPoint {
                x: 320.0,
                y: 240.0,
                size: 32,
            }],
            drizzle: 1.5,
            ap_size: 32,
            sharpen_intensity: 0.5,
            anchor_override: Some(vec![320, 240]),
            stacking_roi: Some(vec![100, 80, 400, 300]),
            is_surface: false,
            bayer_override: None,
            sharpened: false,
            double_pass: true,
            warping_analysis: true,
            normalize_colors: true,
            is_v3: true,
            target_type: "planet_small".into(),
            keep_full_frame: None,
            align_rgb: None,
            compute_policy: ComputePolicy::default(),
            profile: PipelineProfile::Custom,
        }
    }

    #[test]
    fn planetary_stack_validation_rejects_non_finite_and_unsupported_scalars() {
        let mut request = valid_planetary_stack_request();
        assert!(request.validate_static().is_ok());

        request.percent = f32::NAN;
        assert!(request.validate_static().unwrap_err().contains("porcentaje"));
        request.percent = 15.0;
        request.drizzle = 2.5;
        assert!(request.validate_static().unwrap_err().contains("Drizzle"));
        request.drizzle = 2.0;
        request.sharpen_intensity = f32::INFINITY;
        assert!(request.validate_static().unwrap_err().contains("sharpening"));
        request.sharpen_intensity = 0.5;
        request.ap_size = 0;
        assert!(request.validate_static().unwrap_err().contains("AP size"));
    }

    #[test]
    fn planetary_stack_validation_guards_vectors_before_indexing() {
        let mut request = valid_planetary_stack_request();
        request.anchor_override = Some(vec![1]);
        assert!(request.validate_static().unwrap_err().contains("exactamente"));

        request.anchor_override = Some(vec![640, 10]);
        request.stacking_roi = Some(vec![0, 0, 640]);
        assert!(request.validate_static().unwrap_err().contains("ROI"));

        request.stacking_roi = Some(vec![600, 470, 80, 20]);
        assert!(request
            .validate_for_source(640, 480)
            .unwrap_err()
            .contains("fuera"));

        request.stacking_roi = Some(vec![0, 0, 640, 480]);
        assert!(request
            .validate_for_source(640, 480)
            .unwrap_err()
            .contains("ancla"));
    }

    #[test]
    fn planetary_stack_validation_bounds_custom_points_and_output_geometry() {
        let mut request = valid_planetary_stack_request();
        request.custom_points[0].x = f32::NAN;
        assert!(request.validate_static().unwrap_err().contains("no finitas"));
        request.custom_points[0].x = 640.0;
        assert!(request
            .validate_for_source(640, 480)
            .unwrap_err()
            .contains("punto AP"));

        request.custom_points[0].x = 320.0;
        request.anchor_override = Some(vec![320, 240]);
        request.stacking_roi = Some(vec![100, 80, 400, 300]);
        assert_eq!(
            planetary_output_dimensions(640, 480, request.stacking_roi.as_deref(), 1.5)
                .unwrap(),
            (600, 450)
        );
        assert!(request.validate_for_source(640, 480).is_ok());
    }

    #[test]
    fn job_ids_remain_unique_within_the_same_clock_tick() {
        let ids: std::collections::HashSet<String> =
            (0..1_000).map(|_| new_job_id("parallel-job")).collect();
        assert_eq!(ids.len(), 1_000);
    }

    #[test]
    fn telemetry_can_be_filtered_and_cleared_by_job() {
        let job = new_job_id("telemetry-test");
        let event = PipelineTelemetry {
            job_id: job.clone(),
            domain: PipelineDomain::Planetary,
            phase: "analysis".into(),
            engine: "CPU test".into(),
            progress: 50.0,
            eta_seconds: Some(1.0),
            items_done: 1,
            items_total: 2,
            throughput: Some(10.0),
            cpu_percent: None,
            gpu_percent: None,
            ram_mb: 10,
            vram_mb: 0,
            io_read_mb: 1.0,
            io_write_mb: 0.0,
            cache_hits: 0,
            cache_misses: 1,
            fallback_reason: None,
        };
        record_pipeline_telemetry(&event);
        assert_eq!(telemetry_snapshot(Some(&job)).len(), 1);
        assert_eq!(clear_telemetry(Some(&job)), 1);
        assert!(telemetry_snapshot(Some(&job)).is_empty());
    }

    #[test]
    fn compute_policy_covers_absent_vram_parity_and_gpu_only() {
        let mut cap = ComputeCapability {
            gpu_available: false,
            parity_ok: true,
            required_vram_mb: 128,
            vram_budget_mb: 4096,
        };
        let auto = resolve_compute_policy(ComputePolicy::Hybrid, &cap).unwrap();
        assert!(!auto.use_gpu);
        assert!(auto.fallback_reason.unwrap().contains("ausente"));
        assert!(resolve_compute_policy(ComputePolicy::GpuOnly, &cap)
            .unwrap_err()
            .contains("GPU only"));

        cap.gpu_available = true;
        cap.required_vram_mb = 8192;
        let low_vram = resolve_compute_policy(ComputePolicy::Auto, &cap).unwrap();
        assert!(!low_vram.use_gpu);
        assert!(low_vram
            .fallback_reason
            .unwrap()
            .contains("VRAM insuficiente"));

        cap.required_vram_mb = 128;
        cap.parity_ok = false;
        assert!(resolve_compute_policy(ComputePolicy::GpuOnly, &cap)
            .unwrap_err()
            .contains("paridad"));

        cap.parity_ok = true;
        assert!(
            resolve_compute_policy(ComputePolicy::Hybrid, &cap)
                .unwrap()
                .use_gpu
        );
        assert!(
            !resolve_compute_policy(ComputePolicy::CpuOnly, &cap)
                .unwrap()
                .use_gpu
        );
    }

    #[test]
    fn cancellation_is_observable_at_every_named_pipeline_phase() {
        use std::sync::atomic::AtomicBool;
        let cancelled = AtomicBool::new(true);
        for phase in [
            "lectura",
            "análisis",
            "calibración",
            "registro",
            "normalización",
            "integración",
            "drizzle",
            "exportación",
        ] {
            let err = cancellation_checkpoint(&cancelled, phase).unwrap_err();
            assert!(err.contains(phase));
        }
        cancelled.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(cancellation_checkpoint(&cancelled, "integración").is_ok());
    }

    #[test]
    fn deepsky_profiles_resolve_in_backend_and_custom_is_lossless() {
        let maximum = DeepSkyStackRequest {
            profile: PipelineProfile::MaximumQuality,
            drizzle: 2.0,
            pixfrac: 0.7,
            ..Default::default()
        }
        .resolved_profile();
        assert_eq!(maximum.rejection, "winsorized");
        assert_eq!(
            maximum.drizzle, 2.0,
            "drizzle es una decisión separada: el perfil no debe resetearlo"
        );
        assert_eq!(maximum.pixfrac, 0.7);
        assert_eq!(maximum.normalization, "local");
        assert_eq!(maximum.interpolation, "lanczos3");
        assert_eq!(maximum.clip_iters, Some(3));
        assert!(
            !maximum.gradient,
            "ABE/SCNR no pertenece al preset científico"
        );

        let custom = DeepSkyStackRequest {
            profile: PipelineProfile::Custom,
            rejection: "median".into(),
            normalization: "none".into(),
            drizzle: 2.0,
            gradient: true,
            ..Default::default()
        }
        .resolved_profile();
        assert_eq!(custom.rejection, "median");
        assert_eq!(custom.normalization, "none");
        assert_eq!(custom.drizzle, 2.0);
        assert!(custom.gradient);
    }
}
