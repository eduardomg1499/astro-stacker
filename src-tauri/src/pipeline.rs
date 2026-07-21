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

    /// Compatibilidad específica de las APIs planetarias. Históricamente la
    /// ausencia del campo significaba Hybrid; desde el esquema adaptativo el
    /// valor omitido debe ser Auto sin alterar los defaults de cielo profundo.
    pub fn from_planetary_legacy(value: Option<&str>) -> Self {
        value.map_or(Self::Auto, |value| Self::from_legacy(Some(value)))
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

/// Política de decodificación independiente del motor de cómputo. Esta
/// separación evita presentar como "GPU compute" la decodificación por
/// VideoToolbox/D3D11VA/QSV y permite que SER conserve su lector nativo.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecodePolicy {
    #[default]
    Auto,
    #[serde(alias = "cpu", alias = "cpu_only", alias = "sw")]
    Software,
    #[serde(alias = "gpu", alias = "gpu_only", alias = "hw")]
    Hardware,
}

impl DecodePolicy {
    /// Adapta los valores almacenados por las versiones que compartían un
    /// único selector CPU/GPU. Un valor desconocido cae a Auto de forma segura.
    pub fn from_legacy(value: Option<&str>) -> Self {
        match value.unwrap_or("auto").trim().to_ascii_lowercase().as_str() {
            "software" | "cpu" | "cpu_only" | "sw" => Self::Software,
            "hardware" | "gpu" | "gpu_only" | "hw" => Self::Hardware,
            _ => Self::Auto,
        }
    }

    /// Valor compatible con los selectores persistidos por la UI legada.
    pub fn legacy_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Software => "cpu",
            Self::Hardware => "gpu",
        }
    }

    pub fn allows_hardware(self) -> bool {
        !matches!(self, Self::Software)
    }

    /// Sólo Auto puede reiniciar una etapa completa con otro decodificador.
    pub fn allows_fallback(self) -> bool {
        matches!(self, Self::Auto)
    }

    pub fn requires_hardware(self) -> bool {
        matches!(self, Self::Hardware)
    }
}

/// Rigor científico solicitado para la validación local de los AP. Esta
/// política no cambia la cantidad de AP, el porcentaje seleccionado, la
/// profundidad ni el número de pasadas; sólo decide cuánto se valida una
/// coincidencia antes de admitir su vector en el campo de deformación.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QualityPolicy {
    /// Gates económicos para todos los AP y revalidación costosa únicamente
    /// para coincidencias ambiguas. Es el contrato planetario predeterminado.
    #[default]
    Adaptive,
    /// Conserva las correcciones de exactitud, pero evita revisiones
    /// bidireccionales/piramidales adicionales.
    Standard,
    /// Revalida AP ambiguos, de baja textura, limbo o seeing pobre mediante
    /// las rutas robustas disponibles.
    #[serde(alias = "maximum_quality", alias = "max")]
    Maximum,
}

impl QualityPolicy {
    pub fn from_legacy(value: Option<&str>) -> Self {
        match value
            .unwrap_or("adaptive")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "standard" | "baseline" => Self::Standard,
            "maximum" | "maximum_quality" | "max" => Self::Maximum,
            _ => Self::Adaptive,
        }
    }

    pub fn legacy_value(self) -> &'static str {
        match self {
            Self::Adaptive => "adaptive",
            Self::Standard => "standard",
            Self::Maximum => "maximum",
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

/// Registro de trabajos (F8): cancelación POR TRABAJO además del botón
/// global. Los motores largos (NF-Full hoy, EIDR mañana) registran su id al
/// arrancar y consultan SU flag en cada checkpoint; el cancel global barre
/// todos los registrados (compatibilidad con el botón Cancelar actual).
/// Clonable (Arc interno) para que un guard pueda dar de baja el trabajo en
/// Drop incluso ante errores tempranos o pánicos.
#[derive(Clone, Default)]
pub struct JobRegistry {
    jobs: std::sync::Arc<
        Mutex<std::collections::HashMap<String, std::sync::Arc<std::sync::atomic::AtomicBool>>>,
    >,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock poison-healed: el registro es la infraestructura de cancelación —
    /// si un hilo cayó en pánico con el mutex tomado, envenenarlo en cascada
    /// dejaría el botón Cancelar (y todo register/finish posterior) roto. El
    /// estado interno (HashMap de flags atómicos) es válido en cualquier
    /// punto de interrupción, así que recuperar el guard es seguro.
    fn jobs_guard(
        &self,
    ) -> std::sync::MutexGuard<
        '_,
        std::collections::HashMap<String, std::sync::Arc<std::sync::atomic::AtomicBool>>,
    > {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registra un trabajo y devuelve SU flag de cancelación (en false).
    /// Un id repetido reutiliza el flag existente (reintentos idempotentes).
    pub fn register(&self, id: &str) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        let mut jobs = self.jobs_guard();
        jobs.entry(id.to_string())
            .or_insert_with(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)))
            .clone()
    }

    /// Cancela un trabajo por id. Devuelve false si no está registrado.
    pub fn cancel(&self, id: &str) -> bool {
        let jobs = self.jobs_guard();
        match jobs.get(id) {
            Some(flag) => {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Da de baja un trabajo terminado (su flag deja de ser alcanzable).
    pub fn finish(&self, id: &str) {
        self.jobs_guard().remove(id);
    }

    /// Cancela TODOS los trabajos activos (botón Cancelar global).
    pub fn cancel_all(&self) {
        for flag in self.jobs_guard().values() {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn active(&self) -> usize {
        self.jobs_guard().len()
    }
}

/// Da de baja el trabajo al salir del scope — también en errores tempranos
/// (`?`) y pánicos, para que el registro no acumule ids muertos.
pub struct JobGuard {
    registry: JobRegistry,
    id: String,
}

impl JobGuard {
    pub fn new(registry: JobRegistry, id: String) -> Self {
        Self { registry, id }
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        self.registry.finish(&self.id);
    }
}

#[cfg(test)]
mod job_registry_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn test_job_registry_cancel_by_id_and_global() {
        let reg = JobRegistry::new();
        let a = reg.register("job-a");
        let b = reg.register("job-b");
        assert_eq!(reg.active(), 2);
        // Cancelación individual: solo afecta a su trabajo.
        assert!(reg.cancel("job-a"));
        assert!(a.load(Ordering::Relaxed));
        assert!(!b.load(Ordering::Relaxed));
        assert!(!reg.cancel("job-x"));
        // Global: barre lo que quede activo.
        reg.cancel_all();
        assert!(b.load(Ordering::Relaxed));
        // El checkpoint corta con el mensaje esperado.
        let err = cancellation_checkpoint(&b, "prueba").unwrap_err();
        assert!(err.contains("Cancelado"));
    }

    #[test]
    fn test_job_guard_deregisters_on_drop_and_reuse_is_fresh() {
        let reg = JobRegistry::new();
        {
            let flag = reg.register("job-g");
            let _guard = JobGuard::new(reg.clone(), "job-g".into());
            flag.store(true, Ordering::Relaxed);
            assert_eq!(reg.active(), 1);
        }
        assert_eq!(reg.active(), 0);
        // Re-registrar tras finish parte con flag limpio.
        let fresh = reg.register("job-g");
        assert!(!fresh.load(Ordering::Relaxed));
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
    /// Receta 100% automática de cielo profundo: se resuelve en preflight con
    /// señales MEDIDAS de los datos (nº de lights, filtro, dithering,
    /// gradiente, fondo) vía `ds_resolve_auto_recipe`. En planetario equivale
    /// a Balanced.
    Auto,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PipelineDomain {
    Planetary,
    DeepSky,
}

pub const PLANETARY_EXECUTION_PLAN_SCHEMA_VERSION: u16 = 2;
pub const PLANETARY_QUALITY_PLAN_SCHEMA_VERSION: u16 = 1;
pub const PLANETARY_SEQUENCE_PLAN_SCHEMA_VERSION: u16 = 1;

fn default_planetary_execution_plan_schema_version() -> u16 {
    PLANETARY_EXECUTION_PLAN_SCHEMA_VERSION
}

fn default_unit_drizzle() -> f32 {
    1.0
}

/// Etapas con decisión de recursos independiente. El orden de las variantes
/// es también el orden canónico de ejecución/telemetría.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "snake_case")]
pub enum PlanetaryStage {
    #[default]
    Decode,
    CanonicalizeDebayer,
    Preprocess,
    QualitySelection,
    MasterReference,
    CoarseSad,
    FineSad,
    ApAlignment,
    WarpMap,
    Enhance,
    Accumulation,
    Postprocess,
    Publish,
}

impl PlanetaryStage {
    pub const ORDERED: [Self; 13] = [
        Self::Decode,
        Self::CanonicalizeDebayer,
        Self::Preprocess,
        Self::QualitySelection,
        Self::MasterReference,
        Self::CoarseSad,
        Self::FineSad,
        Self::ApAlignment,
        Self::WarpMap,
        Self::Enhance,
        Self::Accumulation,
        Self::Postprocess,
        Self::Publish,
    ];

    pub fn is_decode(self) -> bool {
        matches!(self, Self::Decode)
    }
}

/// Motor que ejecutará realmente una etapa. El backend concreto (Metal,
/// Vulkan, D3D12, VideoToolbox...) viaja separado en `StageDecision::backend`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveEngine {
    NativeIo,
    FfmpegSoftware,
    FfmpegHardware,
    #[default]
    CpuSimd,
    GpuCompute,
    HybridPipeline,
    /// CPU obligatoria porque la etapa no tiene implementación GPU elegible.
    RequiredCpu,
}

impl EffectiveEngine {
    pub fn uses_gpu(self) -> bool {
        matches!(
            self,
            Self::FfmpegHardware | Self::GpuCompute | Self::HybridPipeline
        )
    }

    pub fn is_hardware_decode(self) -> bool {
        matches!(self, Self::FfmpegHardware)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionReason {
    RequestedStrict,
    RequiredCpu,
    CapabilityGate,
    ParityGate,
    MemoryGate,
    CalibrationWinner,
    CalibrationUnstable,
    CachedProfile,
    SourceNative,
    Fallback,
    #[default]
    SafeDefault,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParityStatus {
    #[default]
    Unknown,
    Passed,
    Failed,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanetarySourceKind {
    NativeSer,
    Ffmpeg,
    ImageSequence,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SampleLayout {
    Mono,
    Cfa,
    InterleavedColor,
    PlanarColor,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SampleByteOrder {
    LittleEndian,
    BigEndian,
    NotApplicable,
    #[default]
    Unknown,
}

/// Firma estable de la carga científica. Los campos variables que no aplican
/// (por ejemplo codec en SER) permanecen en `None`; no se inventan valores.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct WorkloadSignature {
    pub source_kind: PlanetarySourceKind,
    pub reader: Option<String>,
    pub codec: Option<String>,
    pub pixel_format: Option<String>,
    pub sample_bits: u8,
    pub sample_layout: SampleLayout,
    pub cfa_pattern: Option<String>,
    pub byte_order: SampleByteOrder,
    pub width: u32,
    pub height: u32,
    pub roi: Option<[u32; 4]>,
    pub target_type: String,
    pub is_surface: bool,
    pub selected_frames: usize,
    pub ap_count: usize,
    pub ap_size: u32,
    pub drizzle: f32,
    pub double_pass: bool,
    pub quality_policy: QualityPolicy,
    pub color_range: Option<String>,
    pub color_matrix: Option<String>,
    pub rotation_degrees: i16,
}

impl Default for WorkloadSignature {
    fn default() -> Self {
        Self {
            source_kind: PlanetarySourceKind::Unknown,
            reader: None,
            codec: None,
            pixel_format: None,
            sample_bits: 0,
            sample_layout: SampleLayout::Unknown,
            cfa_pattern: None,
            byte_order: SampleByteOrder::Unknown,
            width: 0,
            height: 0,
            roi: None,
            target_type: default_planet_target(),
            is_surface: false,
            selected_frames: 0,
            ap_count: 0,
            ap_size: 0,
            drizzle: default_unit_drizzle(),
            double_pass: false,
            quality_policy: QualityPolicy::Adaptive,
            color_range: None,
            color_matrix: None,
            rotation_degrees: 0,
        }
    }
}

impl WorkloadSignature {
    pub fn source_pixels(&self) -> Option<u64> {
        (self.width > 0 && self.height > 0).then(|| u64::from(self.width) * u64::from(self.height))
    }

    pub fn effective_pixels(&self) -> Option<u64> {
        match self.roi {
            Some([_, _, width, height]) if width > 0 && height > 0 => {
                Some(u64::from(width) * u64::from(height))
            }
            _ => self.source_pixels(),
        }
    }
}

/// Perfil científico efectivo. El seeing pobre se modela como overlay en el
/// plan para reforzar cualquiera de estos perfiles sin cambiar radiometría.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanetaryScientificProfile {
    SurfaceMono,
    LunarRgbLarge,
    CompactDisc,
    #[default]
    SurfaceGeneral,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct ApConfidenceGates {
    /// Margen normalizado entre el mejor SAD y el runner-up fuera de 3x3.
    pub min_runner_up_margin: f32,
    /// LK sólo se admite si mejora el objetivo al menos esta fracción.
    pub min_lk_objective_improvement: f32,
    /// Límite robusto del residual espacial expresado en MAD.
    pub spatial_mad_limit: f32,
    pub recheck_low_texture: bool,
    pub bidirectional_recheck: bool,
    pub full_pyramid_recheck: bool,
}

impl Default for ApConfidenceGates {
    fn default() -> Self {
        Self {
            min_runner_up_margin: 0.03,
            min_lk_objective_improvement: 0.0,
            spatial_mad_limit: 3.5,
            recheck_low_texture: true,
            bidirectional_recheck: false,
            full_pyramid_recheck: false,
        }
    }
}

/// Contrato de calidad congelado con el plan de recursos. Hace reproducible
/// qué gates cambiaron shifts/aceptación y qué trabajo fue bit-exacto.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct PlanetaryQualityPlan {
    pub schema_version: u16,
    pub algorithm_version: String,
    pub quality_policy: QualityPolicy,
    pub scientific_profile: PlanetaryScientificProfile,
    pub poor_seeing_overlay: bool,
    pub ap_count: usize,
    pub ap_size: u32,
    pub double_pass: bool,
    pub preserve_dark_filaments: bool,
    pub exact_ap_scale_quality: bool,
    pub temporal_rejection: bool,
    pub spatial_final_filter: bool,
    pub confidence: ApConfidenceGates,
    pub frozen: bool,
}

impl Default for PlanetaryQualityPlan {
    fn default() -> Self {
        Self {
            schema_version: PLANETARY_QUALITY_PLAN_SCHEMA_VERSION,
            algorithm_version: "planetary-quality-v3".into(),
            quality_policy: QualityPolicy::Adaptive,
            scientific_profile: PlanetaryScientificProfile::SurfaceGeneral,
            poor_seeing_overlay: false,
            ap_count: 0,
            ap_size: 0,
            double_pass: true,
            preserve_dark_filaments: true,
            exact_ap_scale_quality: true,
            temporal_rejection: true,
            spatial_final_filter: false,
            confidence: ApConfidenceGates::default(),
            frozen: false,
        }
    }
}

impl PlanetaryQualityPlan {
    pub fn for_workload(workload: &WorkloadSignature, policy: QualityPolicy) -> Self {
        let target = workload.target_type.to_ascii_lowercase();
        let is_lunar = target.contains("lunar") || target.contains("moon") || target.contains("luna");
        let is_mono = matches!(workload.sample_layout, SampleLayout::Mono);
        let scientific_profile = if !workload.is_surface {
            PlanetaryScientificProfile::CompactDisc
        } else if is_lunar && !is_mono {
            PlanetaryScientificProfile::LunarRgbLarge
        } else if is_mono {
            PlanetaryScientificProfile::SurfaceMono
        } else {
            PlanetaryScientificProfile::SurfaceGeneral
        };
        let mut confidence = ApConfidenceGates::default();
        match policy {
            QualityPolicy::Standard => {
                confidence.min_runner_up_margin = 0.0;
                confidence.spatial_mad_limit = 4.5;
                confidence.recheck_low_texture = false;
            }
            QualityPolicy::Adaptive => {}
            QualityPolicy::Maximum => {
                confidence.min_runner_up_margin = 0.05;
                confidence.spatial_mad_limit = 3.0;
                confidence.bidirectional_recheck = true;
                confidence.full_pyramid_recheck = true;
            }
        }
        Self {
            quality_policy: policy,
            scientific_profile,
            // Maximum es la opt-in explícita para material difícil: activa el
            // overlay reproducible de seeing pobre. Auto/Adaptive no lo
            // infieren sin evidencia calibrada de la fuente.
            poor_seeing_overlay: policy == QualityPolicy::Maximum,
            ap_count: workload.ap_count,
            ap_size: workload.ap_size,
            double_pass: workload.double_pass,
            temporal_rejection: workload.double_pass,
            spatial_final_filter: !workload.double_pass && policy == QualityPolicy::Standard,
            confidence,
            frozen: true,
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != PLANETARY_QUALITY_PLAN_SCHEMA_VERSION {
            return Err(format!(
                "Versión de PlanetaryQualityPlan no soportada: {}",
                self.schema_version
            ));
        }
        for (label, value) in [
            ("runner-up", self.confidence.min_runner_up_margin),
            ("LK", self.confidence.min_lk_objective_improvement),
            ("MAD", self.confidence.spatial_mad_limit),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(format!("El gate {label} debe ser finito y no negativo"));
            }
        }
        if self.double_pass && self.spatial_final_filter {
            return Err(
                "El filtro espacial final no puede activarse junto al rechazo temporal de doble pase"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Geometría y fotometría congeladas para una secuencia planetaria. Nunca se
/// usa la textura interior para estabilizar: así la rotación física sobrevive.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct PlanetarySequencePlan {
    pub schema_version: u16,
    pub plan_id: String,
    pub reference_source: String,
    pub normalized_ap_geometry: bool,
    pub fixed_canvas: [u32; 2],
    pub fit_limb_not_texture: bool,
    pub max_scale_delta: f32,
    pub max_roll_degrees: f32,
    pub common_rgb_luminance_scalar: bool,
    pub retain_linear_master_16bit: bool,
    pub frozen: bool,
}

impl Default for PlanetarySequencePlan {
    fn default() -> Self {
        Self {
            schema_version: PLANETARY_SEQUENCE_PLAN_SCHEMA_VERSION,
            plan_id: String::new(),
            reference_source: String::new(),
            normalized_ap_geometry: true,
            fixed_canvas: [0, 0],
            fit_limb_not_texture: true,
            max_scale_delta: 0.02,
            max_roll_degrees: 1.0,
            common_rgb_luminance_scalar: true,
            retain_linear_master_16bit: true,
            frozen: false,
        }
    }
}

impl PlanetarySequencePlan {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != PLANETARY_SEQUENCE_PLAN_SCHEMA_VERSION {
            return Err(format!(
                "Versión de PlanetarySequencePlan no soportada: {}",
                self.schema_version
            ));
        }
        if self.frozen && (self.plan_id.is_empty() || self.reference_source.is_empty()) {
            return Err("Una secuencia congelada requiere planId y referencia".into());
        }
        if !self.max_scale_delta.is_finite()
            || !(0.0..=0.25).contains(&self.max_scale_delta)
            || !self.max_roll_degrees.is_finite()
            || !(0.0..=45.0).contains(&self.max_roll_degrees)
        {
            return Err("Los límites geométricos de la secuencia no son válidos".into());
        }
        Ok(())
    }
}

/// Recursos observados antes de congelar el plan. `gpu_memory_total_mb` es
/// opcional porque wgpu no expone memoria libre de forma portable.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct ResourceSnapshot {
    pub os: String,
    pub architecture: String,
    pub cpu_name: Option<String>,
    pub physical_cpu_cores: usize,
    pub logical_cpu_threads: usize,
    /// Carga global observada justo antes de congelar el plan (0..=100).
    /// Es estado transitorio y, por tanto, no forma parte de la huella del
    /// dispositivo ni invalida los perfiles científicos persistidos.
    pub cpu_load_percent: u8,
    pub ram_total_mb: u64,
    pub ram_available_mb: u64,
    pub swap_used_mb: u64,
    pub gpu_available: bool,
    pub gpu_name: Option<String>,
    pub gpu_backend: Option<String>,
    pub gpu_memory_total_mb: Option<u64>,
    pub gpu_memory_budget_mb: u64,
    pub gpu_max_buffer_bytes: Option<u64>,
    pub ffmpeg_version: Option<String>,
    pub hardware_decode_backends: Vec<String>,
}

/// Decisión congelable de una etapa. Los contadores de concurrencia son
/// deliberadamente independientes para impedir que `threads == scratches`
/// vuelva a ser una suposición implícita del scheduler.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct StageDecision {
    pub stage: PlanetaryStage,
    pub requested_compute_policy: ComputePolicy,
    pub requested_decode_policy: DecodePolicy,
    pub effective_engine: EffectiveEngine,
    pub backend: Option<String>,
    pub reason: DecisionReason,
    pub reason_detail: Option<String>,
    pub parity_status: ParityStatus,
    pub calibration_confidence: Option<f32>,
    pub calibration_samples: usize,
    pub required_cpu: bool,
    pub compute_threads: usize,
    pub frame_concurrency: usize,
    pub scratch_slots: usize,
    pub ap_workers: usize,
    pub batch_size: usize,
    pub ring_bytes: u64,
    pub ram_budget_mb: u64,
    pub vram_budget_mb: u64,
    pub estimated_ms: Option<f64>,
}

impl Default for StageDecision {
    fn default() -> Self {
        Self {
            stage: PlanetaryStage::Decode,
            requested_compute_policy: ComputePolicy::Auto,
            requested_decode_policy: DecodePolicy::default(),
            effective_engine: EffectiveEngine::default(),
            backend: None,
            reason: DecisionReason::default(),
            reason_detail: None,
            parity_status: ParityStatus::default(),
            calibration_confidence: None,
            calibration_samples: 0,
            required_cpu: false,
            compute_threads: 0,
            frame_concurrency: 0,
            scratch_slots: 0,
            ap_workers: 0,
            batch_size: 0,
            ring_bytes: 0,
            ram_budget_mb: 0,
            vram_budget_mb: 0,
            estimated_ms: None,
        }
    }
}

impl StageDecision {
    pub fn new(
        stage: PlanetaryStage,
        requested_compute_policy: ComputePolicy,
        requested_decode_policy: DecodePolicy,
        effective_engine: EffectiveEngine,
    ) -> Self {
        Self {
            stage,
            requested_compute_policy,
            requested_decode_policy,
            effective_engine,
            ..Self::default()
        }
    }

    pub fn allows_fallback(&self) -> bool {
        if self.stage.is_decode() {
            self.requested_decode_policy.allows_fallback()
        } else {
            self.requested_compute_policy.allows_fallback()
        }
    }

    pub fn requires_strict_engine(&self) -> bool {
        if self.stage.is_decode() {
            self.requested_decode_policy.requires_hardware()
        } else {
            matches!(self.requested_compute_policy, ComputePolicy::GpuOnly) && !self.required_cpu
        }
    }

    pub fn strict_contract_satisfied(&self) -> bool {
        if self.stage.is_decode() {
            // SER y secuencias nativas no atraviesan un decoder. Auto y
            // Software aceptan su I/O directo; Hardware strict no puede
            // declararse satisfecho por una operación que nunca usó backend.
            if matches!(self.effective_engine, EffectiveEngine::NativeIo) {
                return !self.requested_decode_policy.requires_hardware();
            }
            return !self.requested_decode_policy.requires_hardware()
                || self.effective_engine.is_hardware_decode();
        }
        if !matches!(self.requested_compute_policy, ComputePolicy::GpuOnly) {
            return true;
        }
        if self.required_cpu {
            return matches!(
                self.effective_engine,
                EffectiveEngine::RequiredCpu | EffectiveEngine::CpuSimd
            );
        }
        self.effective_engine.uses_gpu() && matches!(self.parity_status, ParityStatus::Passed)
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(confidence) = self.calibration_confidence {
            if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
                return Err(format!(
                    "La confianza de calibración de {:?} debe estar entre 0 y 1",
                    self.stage
                ));
            }
        }
        if matches!(self.parity_status, ParityStatus::Failed) && self.effective_engine.uses_gpu() {
            return Err(format!(
                "La etapa {:?} no puede seleccionar GPU con paridad fallida",
                self.stage
            ));
        }
        if !self.strict_contract_satisfied() {
            return Err(format!(
                "La etapa {:?} no satisface la política strict solicitada",
                self.stage
            ));
        }
        Ok(())
    }
}

/// Plan completo e inmutable por contrato una vez que `freeze` ha pasado.
/// Los perfiles persistidos deben validar el esquema y después construir un
/// plan nuevo; nunca deben mutar un plan activo.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct PlanetaryExecutionPlan {
    pub schema_version: u16,
    pub plan_id: String,
    pub algorithm_version: String,
    pub device_fingerprint: String,
    pub workload: WorkloadSignature,
    pub resources: ResourceSnapshot,
    pub requested_compute_policy: ComputePolicy,
    pub requested_decode_policy: DecodePolicy,
    pub requested_quality_policy: QualityPolicy,
    pub quality_plan: PlanetaryQualityPlan,
    pub sequence_plan: Option<PlanetarySequencePlan>,
    pub stages: Vec<StageDecision>,
    pub ram_budget_mb: u64,
    pub vram_budget_mb: u64,
    pub calibration_key: Option<String>,
    pub frozen: bool,
}

impl Default for PlanetaryExecutionPlan {
    fn default() -> Self {
        Self {
            schema_version: default_planetary_execution_plan_schema_version(),
            plan_id: String::new(),
            algorithm_version: String::new(),
            device_fingerprint: String::new(),
            workload: WorkloadSignature::default(),
            resources: ResourceSnapshot::default(),
            requested_compute_policy: ComputePolicy::Auto,
            requested_decode_policy: DecodePolicy::default(),
            requested_quality_policy: QualityPolicy::default(),
            quality_plan: PlanetaryQualityPlan::default(),
            sequence_plan: None,
            stages: Vec::new(),
            ram_budget_mb: 0,
            vram_budget_mb: 0,
            calibration_key: None,
            frozen: false,
        }
    }
}

impl PlanetaryExecutionPlan {
    pub fn new(
        plan_id: impl Into<String>,
        workload: WorkloadSignature,
        resources: ResourceSnapshot,
        requested_compute_policy: ComputePolicy,
        requested_decode_policy: DecodePolicy,
    ) -> Self {
        let quality_plan =
            PlanetaryQualityPlan::for_workload(&workload, QualityPolicy::Adaptive);
        Self {
            plan_id: plan_id.into(),
            workload,
            resources,
            requested_compute_policy,
            requested_decode_policy,
            requested_quality_policy: QualityPolicy::Adaptive,
            quality_plan,
            ..Self::default()
        }
    }

    /// Inserta o sustituye una decisión antes del freeze y conserva el orden
    /// canónico. Esto permite que la calibración reemplace el safe default.
    pub fn set_stage(&mut self, decision: StageDecision) -> Result<(), String> {
        if self.frozen {
            return Err("El plan planetario ya está congelado".into());
        }
        if decision.requested_compute_policy != self.requested_compute_policy
            || decision.requested_decode_policy != self.requested_decode_policy
        {
            return Err("La etapa no conserva las políticas solicitadas por el plan".into());
        }
        decision.validate()?;
        if let Some(existing) = self
            .stages
            .iter_mut()
            .find(|existing| existing.stage == decision.stage)
        {
            *existing = decision;
        } else {
            self.stages.push(decision);
            self.stages.sort_by_key(|decision| decision.stage);
        }
        Ok(())
    }

    pub fn stage(&self, stage: PlanetaryStage) -> Option<&StageDecision> {
        self.stages.iter().find(|decision| decision.stage == stage)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != PLANETARY_EXECUTION_PLAN_SCHEMA_VERSION {
            return Err(format!(
                "Versión de PlanetaryExecutionPlan no soportada: {}",
                self.schema_version
            ));
        }
        self.quality_plan.validate()?;
        if self.frozen && !self.quality_plan.frozen {
            return Err("Un plan de ejecución congelado requiere un qualityPlan congelado".into());
        }
        if let Some(sequence) = self.sequence_plan.as_ref() {
            sequence.validate()?;
            if self.frozen && !sequence.frozen {
                return Err(
                    "Un plan de ejecución congelado no puede contener una secuencia mutable"
                        .into(),
                );
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for decision in &self.stages {
            if !seen.insert(decision.stage) {
                return Err(format!(
                    "La etapa {:?} aparece más de una vez en el plan",
                    decision.stage
                ));
            }
            if decision.requested_compute_policy != self.requested_compute_policy
                || decision.requested_decode_policy != self.requested_decode_policy
            {
                return Err(format!(
                    "La etapa {:?} no conserva las políticas globales",
                    decision.stage
                ));
            }
            decision.validate()?;
        }
        Ok(())
    }

    pub fn freeze(&mut self) -> Result<(), String> {
        if self.stages.is_empty() {
            return Err("No se puede congelar un plan planetario sin etapas".into());
        }
        if !self.quality_plan.frozen {
            return Err("No se puede congelar recursos con un qualityPlan mutable".into());
        }
        if self
            .sequence_plan
            .as_ref()
            .is_some_and(|sequence| !sequence.frozen)
        {
            return Err("No se puede congelar recursos con una secuencia mutable".into());
        }
        self.validate()?;
        self.frozen = true;
        Ok(())
    }
}

/// Resultado observado por etapa. Se mantiene separado de `PipelineTelemetry`
/// para que un trabajo pueda devolver su plan y su balance final completo.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct StageTelemetry {
    pub schema_version: u16,
    pub stage: PlanetaryStage,
    pub effective_engine: EffectiveEngine,
    pub backend: Option<String>,
    pub estimated_ms: Option<f64>,
    /// Tiempo de pared. `elapsed_ms` permanece como alias compatible durante
    /// una versión de esquema.
    pub wall_ms: u64,
    pub elapsed_ms: u64,
    /// Trabajo agregado de workers; puede superar wall_ms con paralelismo.
    pub worker_ms: u64,
    pub items_done: usize,
    pub items_total: usize,
    pub throughput_per_second: Option<f64>,
    pub ram_peak_mb: u64,
    pub vram_peak_mb: u64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub scratch_slots: usize,
    pub scratch_high_water: usize,
    pub scratch_retry_count: u32,
    pub fallback_reason: Option<String>,
    pub completed: bool,
}

impl Default for StageTelemetry {
    fn default() -> Self {
        Self {
            schema_version: default_planetary_execution_plan_schema_version(),
            stage: PlanetaryStage::Decode,
            effective_engine: EffectiveEngine::default(),
            backend: None,
            estimated_ms: None,
            wall_ms: 0,
            elapsed_ms: 0,
            worker_ms: 0,
            items_done: 0,
            items_total: 0,
            throughput_per_second: None,
            ram_peak_mb: 0,
            vram_peak_mb: 0,
            io_read_bytes: 0,
            io_write_bytes: 0,
            cache_hits: 0,
            cache_misses: 0,
            scratch_slots: 0,
            scratch_high_water: 0,
            scratch_retry_count: 0,
            fallback_reason: None,
            completed: false,
        }
    }
}

impl StageTelemetry {
    pub fn record_scratch_retry(&mut self) {
        self.scratch_retry_count = self.scratch_retry_count.saturating_add(1);
    }
}

/// Respuesta tipada del apilado planetario. El comando legado continúa
/// devolviendo sólo `previewSrc`, mientras `run_planetary_stack` expone el
/// plan congelado y la telemetría de cada etapa sin romper a la UI actual.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanetaryStackResponse {
    pub preview_src: String,
    pub execution_plan: PlanetaryExecutionPlan,
    pub quality_plan: PlanetaryQualityPlan,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence_plan: Option<PlanetarySequencePlan>,
    pub stage_telemetry: Vec<StageTelemetry>,
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
    #[serde(default = "default_planetary_compute_policy")]
    pub compute_policy: ComputePolicy,
    #[serde(default)]
    pub decode_policy: DecodePolicy,
    #[serde(default)]
    pub quality_policy: QualityPolicy,
    #[serde(default)]
    pub profile: PipelineProfile,
}

fn default_planet_target() -> String {
    "general".into()
}

fn default_planetary_compute_policy() -> ComputePolicy {
    ComputePolicy::Auto
}

impl PlanetaryAnalysisRequest {
    /// Los perfiles sólo fijan decisiones algorítmicas; `Custom` conserva
    /// exactamente lo solicitado por el cliente.
    pub fn resolved_profile(mut self) -> Self {
        match self.profile {
            PipelineProfile::Fast => self.warping_analysis = false,
            PipelineProfile::MaximumQuality => {
                self.warping_analysis = true;
                self.quality_policy = QualityPolicy::Maximum;
            }
            PipelineProfile::Balanced
            | PipelineProfile::Custom
            | PipelineProfile::Auto => {}
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
    #[serde(default = "default_planetary_compute_policy")]
    pub compute_policy: ComputePolicy,
    #[serde(default)]
    pub decode_policy: DecodePolicy,
    #[serde(default)]
    pub quality_policy: QualityPolicy,
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
            return Err(format!(
                "El punto AP #{index} contiene coordenadas no finitas"
            ));
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
                self.quality_policy = QualityPolicy::Maximum;
            }
            PipelineProfile::Balanced
            | PipelineProfile::Custom
            | PipelineProfile::Auto => {}
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
    #[serde(default = "crate::pipeline::default_true_flag")]
    pub warm_start: bool,
    #[serde(default = "crate::pipeline::default_true_flag")]
    pub multigrid: bool,
    /// CFA directo (sin debayer, por fotodiodo): resuelve x_R/x_G/x_B sobre
    /// las retículas Bayer (§7.6). 2x exige N≥24.
    #[serde(default)]
    pub cfa_direct: bool,
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

/// Versión del contrato público de apilado de cielo profundo. La v4 añade
/// dark-flats explícitos, modo de captura y una política de calibración que no
/// permite degradaciones silenciosas.
pub const DEEP_SKY_STACK_REQUEST_SCHEMA_VERSION: u16 = 4;
pub const CALIBRATION_SIGNATURE_SCHEMA_VERSION: u16 = 1;
pub const CALIBRATION_DECISION_SCHEMA_VERSION: u16 = 1;
pub const SCIENTIFIC_BUNDLE_SCHEMA_VERSION: &str = "zenith-deepsky-scientific-bundle-v1";
pub const DEEP_SKY_RECIPE_SCHEMA_VERSION: &str = "zenith-deepsky-recipe-v4";

fn default_deep_sky_stack_request_schema_version() -> u16 {
    DEEP_SKY_STACK_REQUEST_SCHEMA_VERSION
}

fn default_calibration_signature_schema_version() -> u16 {
    CALIBRATION_SIGNATURE_SCHEMA_VERSION
}

fn default_calibration_decision_schema_version() -> u16 {
    CALIBRATION_DECISION_SCHEMA_VERSION
}

fn default_scientific_bundle_schema_version() -> String {
    SCIENTIFIC_BUNDLE_SCHEMA_VERSION.to_string()
}

fn default_deep_sky_recipe_schema_version() -> String {
    DEEP_SKY_RECIPE_SCHEMA_VERSION.to_string()
}

/// Geometría/espectro de la captura. `Auto` sólo clasifica la adquisición;
/// no habilita automáticamente los motores experimentales NF/EIDR.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DeepSkyCaptureMode {
    #[default]
    Auto,
    #[serde(alias = "broadband_osc")]
    BroadbandOsc,
    #[serde(alias = "broadband_mono")]
    BroadbandMono,
    #[serde(alias = "dual_band_osc")]
    DualBandOsc,
    #[serde(alias = "mono_narrowband")]
    MonoNarrowband,
}

/// Política de seguridad de la calibración. `AllowDegraded` debe ser una
/// elección explícita del usuario y todo fallback queda en la receta.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DeepSkyCalibrationPolicy {
    #[default]
    Strict,
    #[serde(alias = "allow_degraded")]
    AllowDegraded,
}

/// Estado del pedestal de un máster para evitar restar el bias dos veces.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PedestalState {
    #[default]
    RawIncludesBias,
    BiasSubtracted,
}

/// Layout radiométrico persistido por el almacén de frames. La fase forma
/// parte del layout CFA porque un ROI impar cambia qué color ocupa cada píxel.
/// `Rgb` conserva el comportamiento de las cachés anteriores al contrato v4.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "layout",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum StoreLayout {
    Cfa {
        pattern: String,
        phase_x: u8,
        phase_y: u8,
    },
    Mono,
    #[default]
    Rgb,
}

/// Firma completa de compatibilidad para lights y calibraciones. Los campos
/// son opcionales porque FITS/TIFF históricos pueden carecer de metadata; la
/// política Strict decide después qué ausencia es bloqueante.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct CalibrationSignature {
    #[serde(default = "default_calibration_signature_schema_version")]
    pub schema_version: u16,
    pub camera: Option<String>,
    pub sensor: Option<String>,
    pub read_mode: Option<String>,
    pub gain: Option<f32>,
    pub iso: Option<u32>,
    pub offset: Option<f32>,
    pub temperature_c: Option<f32>,
    pub exposure_seconds: Option<f64>,
    pub binning_x: Option<u32>,
    pub binning_y: Option<u32>,
    /// [x, y, width, height] en coordenadas del sensor.
    pub roi: Option<[u32; 4]>,
    pub cfa_pattern: Option<String>,
    /// [x, y] de la fase CFA tras ROI/crop.
    pub cfa_phase: Option<[u8; 2]>,
    pub filter: Option<String>,
    pub session: Option<String>,
    pub optical_train: Option<String>,
    pub adc_bits: Option<u8>,
    pub white_level_adu: Option<f32>,
}

impl Default for CalibrationSignature {
    fn default() -> Self {
        Self {
            schema_version: CALIBRATION_SIGNATURE_SCHEMA_VERSION,
            camera: None,
            sensor: None,
            read_mode: None,
            gain: None,
            iso: None,
            offset: None,
            temperature_c: None,
            exposure_seconds: None,
            binning_x: None,
            binning_y: None,
            roi: None,
            cfa_pattern: None,
            cfa_phase: None,
            filter: None,
            session: None,
            optical_train: None,
            adc_bits: None,
            white_level_adu: None,
        }
    }
}

/// Decisión auditable de calibración por light/grupo. Sólo contiene rutas y
/// metadata; los píxeles de los másters permanecen en el almacén científico.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct PreparedCalibrationDecision {
    #[serde(default = "default_calibration_decision_schema_version")]
    pub schema_version: u16,
    pub frame_path: String,
    pub signature: CalibrationSignature,
    pub calibration_policy: DeepSkyCalibrationPolicy,
    pub bias_master_path: Option<String>,
    pub dark_master_path: Option<String>,
    pub dark_flat_master_path: Option<String>,
    pub flat_master_path: Option<String>,
    pub dark_scale: Option<f32>,
    pub pedestal_state: PedestalState,
    pub compatible: bool,
    pub degraded: bool,
    /// true cuando el usuario forzó la calibración con una asignación manual
    /// (estilo PixInsight): la elección queda registrada, nunca bloquea.
    #[serde(default)]
    pub manual: bool,
    pub fallback: Option<String>,
    pub reasons: Vec<String>,
}

/// Asignación MANUAL de calibración: para los lights listados, los ficheros
/// indicados sustituyen al emparejamiento automático por firma. `lights`
/// vacío = todos los lights del request. La responsabilidad del emparejado es
/// del usuario y queda divulgada en decisiones y receta.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct DeepSkyCalibrationOverride {
    pub lights: Vec<String>,
    pub darks: Vec<String>,
    pub flats: Vec<String>,
}

impl Default for PreparedCalibrationDecision {
    fn default() -> Self {
        Self {
            schema_version: CALIBRATION_DECISION_SCHEMA_VERSION,
            frame_path: String::new(),
            signature: CalibrationSignature::default(),
            calibration_policy: DeepSkyCalibrationPolicy::Strict,
            bias_master_path: None,
            dark_master_path: None,
            dark_flat_master_path: None,
            flat_master_path: None,
            dark_scale: None,
            pedestal_state: PedestalState::RawIncludesBias,
            compatible: false,
            degraded: false,
            manual: false,
            fallback: None,
            reasons: Vec::new(),
        }
    }
}

/// Tipos de producto publicables por un grupo científico.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum ScientificProductKind {
    #[default]
    Sci,
    Var,
    Neff,
    Dq,
    Coverage,
    Rejection,
    Psf,
    Mtf,
    Psd,
    Background,
    Struct,
    Recov,
    Residual,
}

/// Entrada liviana del manifiesto: ruta, unidades y forma, nunca el payload.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct ScientificProductMetadata {
    pub kind: ScientificProductKind,
    pub path: String,
    pub bunit: Option<String>,
    pub sample_type: Option<String>,
    pub width: u32,
    pub height: u32,
    pub channels: u8,
    pub linear: bool,
    pub derived: bool,
    pub metadata: BTreeMap<String, String>,
}

/// Manifiesto por grupo de los productos SCI/VAR/NEFF/DQ y diagnósticos.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct ScientificBundleManifest {
    #[serde(default = "default_scientific_bundle_schema_version")]
    pub schema_version: String,
    #[serde(default = "default_deep_sky_recipe_schema_version")]
    pub recipe_schema: String,
    pub group_id: String,
    pub filter_profile: Option<String>,
    pub capture_mode: DeepSkyCaptureMode,
    pub calibration_policy: DeepSkyCalibrationPolicy,
    pub width: u32,
    pub height: u32,
    pub channels: u8,
    pub products: Vec<ScientificProductMetadata>,
    pub calibration_decisions: Vec<PreparedCalibrationDecision>,
    pub fallbacks: Vec<String>,
    pub warnings: Vec<String>,
    pub metadata: BTreeMap<String, String>,
}

impl Default for ScientificBundleManifest {
    fn default() -> Self {
        Self {
            schema_version: default_scientific_bundle_schema_version(),
            recipe_schema: default_deep_sky_recipe_schema_version(),
            group_id: String::new(),
            filter_profile: None,
            capture_mode: DeepSkyCaptureMode::Auto,
            calibration_policy: DeepSkyCalibrationPolicy::Strict,
            width: 0,
            height: 0,
            channels: 0,
            products: Vec::new(),
            calibration_decisions: Vec::new(),
            fallbacks: Vec::new(),
            warnings: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkyStackRequest {
    #[serde(default = "default_deep_sky_stack_request_schema_version")]
    pub schema_version: u16,
    pub lights: Vec<String>,
    #[serde(default)]
    pub darks: Vec<String>,
    #[serde(default)]
    pub flats: Vec<String>,
    /// Darks con la misma exposición/geometría que los flats. El alias
    /// snake_case facilita clientes CLI; JSON/UI canónico usa `darkFlats`.
    #[serde(default, alias = "dark_flats")]
    pub dark_flats: Vec<String>,
    #[serde(default)]
    pub bias: Vec<String>,
    #[serde(default)]
    pub capture_mode: DeepSkyCaptureMode,
    #[serde(default)]
    pub calibration_policy: DeepSkyCalibrationPolicy,
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
    /// Exporta los productos científicos (VAR/NEFF/DQ/…) cuando la ruta
    /// efectiva conserva la evidencia necesaria. El default v4 es `true`;
    /// una ruta incapaz de producirlos debe declarar la ausencia, no fabricar
    /// mapas ni degradar silenciosamente a un master sin trazabilidad.
    #[serde(default = "default_true")]
    pub scientific_products: bool,
    /// Asignaciones manuales de calibración (opcional, estilo PixInsight).
    #[serde(default)]
    pub calibration_overrides: Vec<DeepSkyCalibrationOverride>,
}

impl Default for DeepSkyStackRequest {
    fn default() -> Self {
        Self {
            schema_version: DEEP_SKY_STACK_REQUEST_SCHEMA_VERSION,
            lights: Vec::new(),
            darks: Vec::new(),
            flats: Vec::new(),
            dark_flats: Vec::new(),
            bias: Vec::new(),
            capture_mode: DeepSkyCaptureMode::Auto,
            calibration_policy: DeepSkyCalibrationPolicy::Strict,
            compute_policy: ComputePolicy::default(),
            profile: PipelineProfile::default(),
            rejection: default_rejection(),
            kappa_low: default_kappa(),
            kappa_high: default_kappa(),
            clip_iters: None,
            normalization: default_normalization(),
            interpolation: default_interpolation(),
            drizzle: default_drizzle(),
            pixfrac: default_pixfrac(),
            cosmetic: None,
            gradient: false,
            optimize_dark: None,
            auto_crop: true,
            pedestal: None,
            local_weighting: false,
            work_dir: None,
            integration_method: None,
            scientific_products: true,
            calibration_overrides: Vec::new(),
        }
    }
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
                self.local_weighting = false;
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
                self.local_weighting = false;
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
                // Rescate de detalle (pesos locales por FWHM): el diferenciador
                // de Máxima Calidad. Fotometría intacta (media ponderada lineal).
                self.local_weighting = true;
            }
            PipelineProfile::Custom => {}
            // Auto se resuelve en deepsky::ds_resolve_auto_recipe con las
            // señales medidas del preflight; aquí no hay datos que mirar.
            PipelineProfile::Auto => {}
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
    /// Receta resuelta del perfil AUTO (rechazo, κ, normalización, drizzle,
    /// interpolación, pedestal) más las señales medidas que la justifican
    /// (dithering_rms, gradient_strength, background_over_noise, …). Vacía
    /// para cualquier otro perfil. La UI la muestra en solo-lectura: el plan
    /// que ve el usuario ES la receta que se ejecutará.
    pub resolved_recipe: BTreeMap<String, String>,
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
    /// Decisión exacta por light/grupo: masters efectivos, compatibilidad,
    /// escala, pedestal y razones. No se codifica dentro de warnings porque
    /// la UI y la receta deben poder auditarla de forma tipada.
    pub calibration_decisions: Vec<PreparedCalibrationDecision>,
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
    /// Índice autocontenido de todos los productos científicos y derivados
    /// publicados para este grupo. Evita que una sesión multibanda pierda
    /// VAR/NEFF/DQ/STRUCT/RECOV aunque el estado interactivo avance al grupo
    /// siguiente.
    pub scientific_bundle: ScientificBundleManifest,
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
    /// Productos científicos. Classic CPU streaming 1×, NebulaFusion y EIDR
    /// publican el bundle cuando sus invariantes pasan; Classic GPU/tiled y
    /// drizzle usan `None` hasta conservar momentos/VAR por depósito.
    /// VAR y NEFF comparten el layout interleaved del máster; DQ es u32 por
    /// píxel con los bits de `deepsky_variance::dq`.
    pub variance: Option<Vec<f32>>,
    pub neff: Option<Vec<f32>>,
    pub dq: Option<Vec<u32>>,
    /// STRUCT (F7): mapa de evidencia multiescala validado A/B y su residual
    /// (planos LUMA de w*h; None fuera del modo FullWithStruct).
    pub struct_map: Option<Vec<f32>>,
    pub struct_residual: Option<Vec<f32>>,
    /// Mapa de recuperabilidad EIDR (R por tile, 0..1) a resolución del
    /// máster. Solo con motor EIDR (F9).
    pub recoverability: Option<Vec<f32>>,
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

    #[test]
    fn job_registry_survives_a_poisoned_lock() {
        let registry = JobRegistry::new();
        let flag = registry.register("stack-1");
        // Envenenar el mutex: un hilo hace panic con el lock tomado.
        {
            let reg = registry.clone();
            let _ = std::thread::spawn(move || {
                let _guard = reg.jobs_guard();
                panic!("panic con el lock tomado (simulado)");
            })
            .join();
        }
        // Toda la API sigue funcionando (poison-healed): la cancelación no
        // puede romperse en cascada por un panic ajeno.
        assert!(registry.cancel("stack-1"));
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
        let _ = registry.register("stack-2");
        assert_eq!(registry.active(), 2);
        registry.cancel_all();
        registry.finish("stack-1");
        registry.finish("stack-2");
        assert_eq!(registry.active(), 0);
    }

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
            decode_policy: DecodePolicy::default(),
            quality_policy: QualityPolicy::Adaptive,
            profile: PipelineProfile::Custom,
        }
    }

    #[test]
    fn planetary_stack_validation_rejects_non_finite_and_unsupported_scalars() {
        let mut request = valid_planetary_stack_request();
        assert!(request.validate_static().is_ok());

        request.percent = f32::NAN;
        assert!(request
            .validate_static()
            .unwrap_err()
            .contains("porcentaje"));
        request.percent = 15.0;
        request.drizzle = 2.5;
        assert!(request.validate_static().unwrap_err().contains("Drizzle"));
        request.drizzle = 2.0;
        request.sharpen_intensity = f32::INFINITY;
        assert!(request
            .validate_static()
            .unwrap_err()
            .contains("sharpening"));
        request.sharpen_intensity = 0.5;
        request.ap_size = 0;
        assert!(request.validate_static().unwrap_err().contains("AP size"));
    }

    #[test]
    fn planetary_stack_validation_guards_vectors_before_indexing() {
        let mut request = valid_planetary_stack_request();
        request.anchor_override = Some(vec![1]);
        assert!(request
            .validate_static()
            .unwrap_err()
            .contains("exactamente"));

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
        assert!(request
            .validate_static()
            .unwrap_err()
            .contains("no finitas"));
        request.custom_points[0].x = 640.0;
        assert!(request
            .validate_for_source(640, 480)
            .unwrap_err()
            .contains("punto AP"));

        request.custom_points[0].x = 320.0;
        request.anchor_override = Some(vec![320, 240]);
        request.stacking_roi = Some(vec![100, 80, 400, 300]);
        assert_eq!(
            planetary_output_dimensions(640, 480, request.stacking_roi.as_deref(), 1.5).unwrap(),
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
        assert_eq!(ComputePolicy::from_legacy(None), ComputePolicy::Hybrid);
        assert_eq!(
            ComputePolicy::from_planetary_legacy(None),
            ComputePolicy::Auto
        );
        assert_eq!(
            ComputePolicy::from_planetary_legacy(Some("hybrid")),
            ComputePolicy::Hybrid
        );
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
    fn decode_policy_defaults_and_accepts_legacy_values() {
        assert_eq!(DecodePolicy::default(), DecodePolicy::Auto);
        assert_eq!(DecodePolicy::from_legacy(None), DecodePolicy::Auto);
        assert_eq!(
            DecodePolicy::from_legacy(Some("cpu")),
            DecodePolicy::Software
        );
        assert_eq!(
            DecodePolicy::from_legacy(Some("GPU_ONLY")),
            DecodePolicy::Hardware
        );
        assert_eq!(DecodePolicy::Hardware.legacy_value(), "gpu");
        assert!(DecodePolicy::Auto.allows_fallback());
        assert!(!DecodePolicy::Hardware.allows_fallback());

        let legacy: PlanetaryAnalysisRequest = serde_json::from_value(serde_json::json!({
            "path": "/tmp/legacy.mov"
        }))
        .unwrap();
        assert_eq!(legacy.decode_policy, DecodePolicy::Auto);
        assert_eq!(legacy.quality_policy, QualityPolicy::Adaptive);
        // El nuevo contrato planetario migra su valor ausente a Auto sin
        // cambiar el default global que aún consumen otras canalizaciones.
        assert_eq!(legacy.compute_policy, ComputePolicy::Auto);

        assert_eq!(
            serde_json::from_str::<DecodePolicy>("\"cpu\"").unwrap(),
            DecodePolicy::Software
        );
        assert_eq!(
            serde_json::from_str::<DecodePolicy>("\"hw\"").unwrap(),
            DecodePolicy::Hardware
        );
    }

    #[test]
    fn quality_policy_defaults_adaptive_and_freezes_scientific_profile() {
        assert_eq!(QualityPolicy::default(), QualityPolicy::Adaptive);
        assert_eq!(QualityPolicy::from_legacy(None), QualityPolicy::Adaptive);
        assert_eq!(
            QualityPolicy::from_legacy(Some("maximum_quality")),
            QualityPolicy::Maximum
        );
        let mono_surface = WorkloadSignature {
            sample_layout: SampleLayout::Mono,
            target_type: "solar_surface".into(),
            is_surface: true,
            ap_count: 128,
            ap_size: 32,
            double_pass: true,
            ..WorkloadSignature::default()
        };
        let adaptive =
            PlanetaryQualityPlan::for_workload(&mono_surface, QualityPolicy::Adaptive);
        assert_eq!(
            adaptive.scientific_profile,
            PlanetaryScientificProfile::SurfaceMono
        );
        assert!(adaptive.preserve_dark_filaments);
        assert!(adaptive.temporal_rejection);
        assert!(!adaptive.spatial_final_filter);
        assert!(adaptive.validate().is_ok());

        let standard =
            PlanetaryQualityPlan::for_workload(&mono_surface, QualityPolicy::Standard);
        assert_eq!(standard.confidence.min_runner_up_margin, 0.0);
        assert!(!standard.confidence.recheck_low_texture);
        assert!(!standard.poor_seeing_overlay);

        let maximum = PlanetaryQualityPlan::for_workload(
            &WorkloadSignature {
                sample_layout: SampleLayout::InterleavedColor,
                target_type: "lunar_surface".into(),
                is_surface: true,
                double_pass: true,
                ..WorkloadSignature::default()
            },
            QualityPolicy::Maximum,
        );
        assert_eq!(
            maximum.scientific_profile,
            PlanetaryScientificProfile::LunarRgbLarge
        );
        assert!(maximum.confidence.bidirectional_recheck);
        assert!(maximum.confidence.full_pyramid_recheck);
        assert!(maximum.poor_seeing_overlay);
        assert!(!adaptive.poor_seeing_overlay);
    }

    #[test]
    fn sequence_plan_rejects_unidentified_frozen_geometry() {
        let mut sequence = PlanetarySequencePlan::default();
        sequence.frozen = true;
        assert!(sequence.validate().is_err());
        sequence.plan_id = "sequence-1".into();
        sequence.reference_source = "/capture/jupiter-01.ser".into();
        sequence.fixed_canvas = [520, 444];
        assert!(sequence.validate().is_ok());
        assert!(sequence.fit_limb_not_texture);
        assert!(sequence.retain_linear_master_16bit);
    }

    #[test]
    fn execution_plan_freezes_ordered_unique_stage_decisions() {
        let workload = WorkloadSignature {
            source_kind: PlanetarySourceKind::NativeSer,
            sample_bits: 12,
            sample_layout: SampleLayout::Cfa,
            cfa_pattern: Some("rggb".into()),
            byte_order: SampleByteOrder::LittleEndian,
            width: 1920,
            height: 1080,
            selected_frames: 500,
            ap_count: 128,
            ap_size: 48,
            ..WorkloadSignature::default()
        };
        assert_eq!(workload.source_pixels(), Some(2_073_600));

        let mut plan = PlanetaryExecutionPlan::new(
            "plan-test",
            workload,
            ResourceSnapshot {
                logical_cpu_threads: 10,
                ram_available_mb: 12_000,
                gpu_available: true,
                gpu_backend: Some("metal".into()),
                gpu_memory_budget_mb: 4_096,
                ..ResourceSnapshot::default()
            },
            ComputePolicy::Auto,
            DecodePolicy::Auto,
        );
        let mut accumulation = StageDecision::new(
            PlanetaryStage::Accumulation,
            ComputePolicy::Auto,
            DecodePolicy::Auto,
            EffectiveEngine::GpuCompute,
        );
        accumulation.parity_status = ParityStatus::Passed;
        accumulation.reason = DecisionReason::CalibrationWinner;
        accumulation.calibration_confidence = Some(0.95);
        plan.set_stage(accumulation).unwrap();

        let mut decode = StageDecision::new(
            PlanetaryStage::Decode,
            ComputePolicy::Auto,
            DecodePolicy::Auto,
            EffectiveEngine::NativeIo,
        );
        decode.reason = DecisionReason::SourceNative;
        decode.parity_status = ParityStatus::NotApplicable;
        plan.set_stage(decode).unwrap();
        assert_eq!(plan.stages[0].stage, PlanetaryStage::Decode);
        assert_eq!(plan.stages[1].stage, PlanetaryStage::Accumulation);

        plan.freeze().unwrap();
        assert!(plan.frozen);
        assert!(plan.stage(PlanetaryStage::Accumulation).is_some());
        assert!(plan
            .set_stage(StageDecision::new(
                PlanetaryStage::Enhance,
                ComputePolicy::Auto,
                DecodePolicy::Auto,
                EffectiveEngine::CpuSimd,
            ))
            .unwrap_err()
            .contains("congelado"));

        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(
            json["schemaVersion"],
            PLANETARY_EXECUTION_PLAN_SCHEMA_VERSION
        );
        assert_eq!(json["requestedQualityPolicy"], "adaptive");
        assert_eq!(json["requestedDecodePolicy"], "auto");
        assert_eq!(json["stages"][1]["effectiveEngine"], "gpu_compute");
        let restored: PlanetaryExecutionPlan = serde_json::from_value(json).unwrap();
        assert_eq!(restored, plan);
        restored.validate().unwrap();
    }

    #[test]
    fn strict_stage_contract_rejects_silent_cpu_fallback() {
        let hardware_decode = StageDecision::new(
            PlanetaryStage::Decode,
            ComputePolicy::GpuOnly,
            DecodePolicy::Hardware,
            EffectiveEngine::FfmpegSoftware,
        );
        assert!(!hardware_decode.allows_fallback());
        assert!(hardware_decode.validate().unwrap_err().contains("strict"));

        let native_ser = StageDecision::new(
            PlanetaryStage::Decode,
            ComputePolicy::GpuOnly,
            DecodePolicy::Hardware,
            EffectiveEngine::NativeIo,
        );
        assert!(!native_ser.strict_contract_satisfied());
        assert!(native_ser.validate().unwrap_err().contains("strict"));

        let mut gpu_compute = StageDecision::new(
            PlanetaryStage::FineSad,
            ComputePolicy::GpuOnly,
            DecodePolicy::Auto,
            EffectiveEngine::GpuCompute,
        );
        assert!(!gpu_compute.strict_contract_satisfied());
        gpu_compute.parity_status = ParityStatus::Passed;
        assert!(gpu_compute.validate().is_ok());

        let mut required_cpu = StageDecision::new(
            PlanetaryStage::ApAlignment,
            ComputePolicy::GpuOnly,
            DecodePolicy::Auto,
            EffectiveEngine::RequiredCpu,
        );
        required_cpu.required_cpu = true;
        required_cpu.reason = DecisionReason::RequiredCpu;
        required_cpu.parity_status = ParityStatus::NotApplicable;
        assert!(required_cpu.validate().is_ok());
    }

    #[test]
    fn planetary_stack_response_keeps_legacy_preview_and_adds_plan_telemetry() {
        let mut plan = PlanetaryExecutionPlan::new(
            "response-plan",
            WorkloadSignature::default(),
            ResourceSnapshot::default(),
            ComputePolicy::Auto,
            DecodePolicy::Auto,
        );
        let mut decode = StageDecision::new(
            PlanetaryStage::Decode,
            ComputePolicy::Auto,
            DecodePolicy::Auto,
            EffectiveEngine::NativeIo,
        );
        decode.reason = DecisionReason::SourceNative;
        decode.parity_status = ParityStatus::NotApplicable;
        plan.set_stage(decode).unwrap();
        plan.freeze().unwrap();
        let response = PlanetaryStackResponse {
            preview_src: "data:image/png;base64,AA==".into(),
            quality_plan: plan.quality_plan.clone(),
            sequence_plan: None,
            execution_plan: plan,
            stage_telemetry: vec![StageTelemetry {
                stage: PlanetaryStage::Decode,
                effective_engine: EffectiveEngine::NativeIo,
                completed: true,
                items_done: 20,
                items_total: 20,
                ..StageTelemetry::default()
            }],
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["previewSrc"], "data:image/png;base64,AA==");
        assert_eq!(json["executionPlan"]["frozen"], true);
        assert_eq!(json["qualityPlan"]["qualityPolicy"], "adaptive");
        assert_eq!(json["stageTelemetry"][0]["effectiveEngine"], "native_io");
        assert_eq!(
            serde_json::from_value::<PlanetaryStackResponse>(json).unwrap(),
            response
        );
    }

    #[test]
    fn new_planner_contracts_deserialize_missing_fields_safely() {
        let plan: PlanetaryExecutionPlan = serde_json::from_str("{}").unwrap();
        assert_eq!(plan.schema_version, PLANETARY_EXECUTION_PLAN_SCHEMA_VERSION);
        assert_eq!(plan.requested_decode_policy, DecodePolicy::Auto);
        assert_eq!(plan.requested_quality_policy, QualityPolicy::Adaptive);
        assert!(!plan.frozen);

        let mut telemetry: StageTelemetry = serde_json::from_str("{}").unwrap();
        assert_eq!(telemetry.stage, PlanetaryStage::Decode);
        assert_eq!(telemetry.scratch_retry_count, 0);
        telemetry.record_scratch_retry();
        assert_eq!(telemetry.scratch_retry_count, 1);
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
        assert!(
            maximum.local_weighting,
            "Máxima calidad activa el rescate de detalle (pesos locales)"
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

        // Auto no toca nada en resolved_profile (paridad con Custom): la
        // resolución real ocurre en deepsky::ds_resolve_auto_recipe con las
        // señales medidas del preflight.
        let auto = DeepSkyStackRequest {
            profile: PipelineProfile::Auto,
            rejection: "median".into(),
            normalization: "none".into(),
            drizzle: 2.0,
            ..Default::default()
        }
        .resolved_profile();
        assert_eq!(auto.profile, PipelineProfile::Auto);
        assert_eq!(auto.rejection, "median");
        assert_eq!(auto.normalization, "none");
        assert_eq!(auto.drizzle, 2.0);
        assert_eq!(
            serde_json::to_value(PipelineProfile::Auto).unwrap(),
            serde_json::json!("auto"),
            "el contrato serde del perfil AUTO es 'auto'"
        );
    }

    #[test]
    fn deepsky_v4_legacy_request_defaults_are_strict_and_backward_compatible() {
        let request: DeepSkyStackRequest = serde_json::from_value(serde_json::json!({
            "lights": ["light-001.fits"]
        }))
        .unwrap();

        assert_eq!(
            request.schema_version,
            DEEP_SKY_STACK_REQUEST_SCHEMA_VERSION
        );
        assert!(request.dark_flats.is_empty());
        assert_eq!(request.capture_mode, DeepSkyCaptureMode::Auto);
        assert_eq!(request.calibration_policy, DeepSkyCalibrationPolicy::Strict);
        // Compute/profile conservan compatibilidad; v4 activa trazabilidad
        // científica por defecto y cada ruta declara qué mapas pudo producir.
        assert_eq!(request.compute_policy, ComputePolicy::Hybrid);
        assert_eq!(request.profile, PipelineProfile::Balanced);
        assert!(request.scientific_products);
    }

    #[test]
    fn deepsky_v4_serializes_canonical_camel_case_and_accepts_cli_aliases() {
        let request: DeepSkyStackRequest = serde_json::from_value(serde_json::json!({
            "lights": ["light.fits"],
            "dark_flats": ["df-001.fits"],
            "captureMode": "dual_band_osc",
            "calibrationPolicy": "allow_degraded"
        }))
        .unwrap();
        assert_eq!(request.dark_flats, vec!["df-001.fits"]);
        assert_eq!(request.capture_mode, DeepSkyCaptureMode::DualBandOsc);
        assert_eq!(
            request.calibration_policy,
            DeepSkyCalibrationPolicy::AllowDegraded
        );

        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["schemaVersion"], DEEP_SKY_STACK_REQUEST_SCHEMA_VERSION);
        assert_eq!(json["darkFlats"][0], "df-001.fits");
        assert_eq!(json["captureMode"], "dualBandOsc");
        assert_eq!(json["calibrationPolicy"], "allowDegraded");
        assert!(json.get("dark_flats").is_none());
    }

    #[test]
    fn deepsky_capture_mode_accepts_broadband_mono_alias_and_serializes_canonical_value() {
        let request: DeepSkyStackRequest = serde_json::from_value(serde_json::json!({
            "lights": ["luminance-001.fits"],
            "captureMode": "broadband_mono"
        }))
        .unwrap();

        assert_eq!(request.capture_mode, DeepSkyCaptureMode::BroadbandMono);

        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["captureMode"], "broadbandMono");
    }

    #[test]
    fn scientific_bundle_contracts_have_versioned_safe_defaults() {
        assert_eq!(StoreLayout::default(), StoreLayout::Rgb);
        let cfa_layout = serde_json::to_value(StoreLayout::Cfa {
            pattern: "RGGB".into(),
            phase_x: 1,
            phase_y: 0,
        })
        .unwrap();
        assert_eq!(cfa_layout["layout"], "cfa");
        assert_eq!(cfa_layout["pattern"], "RGGB");
        assert_eq!(cfa_layout["phaseX"], 1);
        assert_eq!(cfa_layout["phaseY"], 0);

        let signature: CalibrationSignature = serde_json::from_str("{}").unwrap();
        assert_eq!(
            signature.schema_version,
            CALIBRATION_SIGNATURE_SCHEMA_VERSION
        );

        let decision: PreparedCalibrationDecision = serde_json::from_str("{}").unwrap();
        assert_eq!(decision.schema_version, CALIBRATION_DECISION_SCHEMA_VERSION);
        assert_eq!(
            decision.calibration_policy,
            DeepSkyCalibrationPolicy::Strict
        );
        assert_eq!(decision.pedestal_state, PedestalState::RawIncludesBias);
        assert!(!decision.compatible);
        assert!(!decision.degraded);

        let mut bundle: ScientificBundleManifest = serde_json::from_str("{}").unwrap();
        assert_eq!(bundle.schema_version, SCIENTIFIC_BUNDLE_SCHEMA_VERSION);
        assert_eq!(bundle.recipe_schema, DEEP_SKY_RECIPE_SCHEMA_VERSION);
        assert_eq!(bundle.capture_mode, DeepSkyCaptureMode::Auto);
        assert_eq!(bundle.calibration_policy, DeepSkyCalibrationPolicy::Strict);
        bundle.products.push(ScientificProductMetadata {
            kind: ScientificProductKind::Var,
            path: "group-VAR.fits".into(),
            bunit: Some("ADU^2".into()),
            sample_type: Some("float32".into()),
            width: 16,
            height: 8,
            channels: 1,
            linear: true,
            ..Default::default()
        });
        let json = serde_json::to_value(bundle).unwrap();
        assert_eq!(json["products"][0]["kind"], "VAR");
        assert_eq!(json["products"][0]["bunit"], "ADU^2");
        assert_eq!(json["products"][0]["sampleType"], "float32");
    }
}
