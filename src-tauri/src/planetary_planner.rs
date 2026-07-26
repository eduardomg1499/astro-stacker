//! Planificador planetario por etapa.
//!
//! Este modulo no cambia ningun parametro cientifico. Convierte una firma de
//! fuente + recursos en un plan de ejecucion congelado, y conserva perfiles de
//! calibracion locales. Las rutas de produccion siguen validando capacidad y
//! paridad antes de obedecer una recomendacion persistida.

use crate::frame_source::FrameDescriptor;
use crate::pipeline::{
    ComputePolicy, DecisionReason, DecodePolicy, EffectiveEngine, ParityStatus,
    PlanetaryExecutionPlan, PlanetaryQualityPlan, PlanetarySourceKind, PlanetaryStage,
    QualityPolicy, ResourceSnapshot, SampleByteOrder, SampleLayout, StageDecision,
    WorkloadSignature,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const PLANETARY_PLANNER_ALGORITHM_VERSION: &str = "planetary-planner-v2";
const CALIBRATION_SCHEMA: &str = "zas-planetary-calibration-v2";
const MIN_CALIBRATION_SAMPLES: usize = 3;
const MAX_RELATIVE_VARIATION: f64 = 0.10;
const MIN_GPU_COMPUTE_SPEEDUP: f64 = 1.15;
const MIN_HARDWARE_DECODE_SPEEDUP: f64 = 1.10;
const MAX_PERSISTED_CALIBRATIONS: usize = 512;
const MIN_RAM_RESERVE_MB: u64 = 512;
const MAX_RAM_RESERVE_MB: u64 = 4_096;
const RING_RAM_FRACTION_NUMERATOR: u64 = 1;
const RING_RAM_FRACTION_DENOMINATOR: u64 = 4;
const GPU_WORKING_SET_FRAMES: u64 = 3;
/// Sólo se aparta del perfil local cuando la presión es inequívoca. Con una
/// carga menor, la observación calibrada sigue siendo una señal más estable
/// que una muestra instantánea del SO.
const CPU_PRESSURE_GPU_OVERRIDE_PERCENT: u8 = 70;
const MEMORY_PRESSURE_SWAP_MB: u64 = 512;

static CALIBRATION_STORE_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static CALIBRATION_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeResourcePressure {
    pub cpu_load_percent: u8,
    pub memory_pressure: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlannerCapabilities {
    pub gpu_available: bool,
    pub preprocess_parity: bool,
    pub coarse_sad_parity: bool,
    pub enhance_parity: bool,
    pub accumulation_parity: bool,
}

#[derive(Clone, Debug)]
pub struct PlanetaryWorkloadContext<'a> {
    pub descriptor: &'a FrameDescriptor,
    pub codec: Option<&'a str>,
    pub target_type: &'a str,
    pub is_surface: bool,
    pub selected_frames: usize,
    pub ap_count: usize,
    pub ap_size: u32,
    pub drizzle: f32,
    pub double_pass: bool,
    pub roi: Option<[u32; 4]>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationObservation {
    pub stage: PlanetaryStage,
    pub cpu_ms: Vec<f64>,
    pub accelerated_ms: Vec<f64>,
    pub parity_passed: bool,
    pub backend: Option<String>,
    pub captured_at_utc: String,
}

impl CalibrationObservation {
    pub fn new(
        stage: PlanetaryStage,
        cpu_ms: Vec<f64>,
        accelerated_ms: Vec<f64>,
        parity_passed: bool,
        backend: Option<String>,
    ) -> Self {
        Self {
            stage,
            cpu_ms,
            accelerated_ms,
            parity_passed,
            backend,
            captured_at_utc: chrono::Utc::now().to_rfc3339(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CalibrationEvaluation {
    pub accelerated_wins: bool,
    pub stable: bool,
    pub speedup: f64,
    pub confidence: f32,
    pub cpu_median_ms: f64,
    pub accelerated_median_ms: f64,
    pub samples: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CalibrationStore {
    schema: String,
    algorithm_version: String,
    entries: BTreeMap<String, CalibrationObservation>,
}

impl Default for CalibrationStore {
    fn default() -> Self {
        Self {
            schema: CALIBRATION_SCHEMA.into(),
            algorithm_version: PLANETARY_PLANNER_ALGORITHM_VERSION.into(),
            entries: BTreeMap::new(),
        }
    }
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() || values.iter().any(|v| !v.is_finite() || *v <= 0.0) {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    Some(if sorted.len() % 2 == 0 {
        (sorted[middle - 1] + sorted[middle]) * 0.5
    } else {
        sorted[middle]
    })
}

fn coefficient_of_variation(values: &[f64]) -> Option<f64> {
    if values.len() < 2 || values.iter().any(|v| !v.is_finite() || *v <= 0.0) {
        return None;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    if mean <= f64::EPSILON {
        return None;
    }
    let variance = values
        .iter()
        .map(|v| {
            let d = *v - mean;
            d * d
        })
        .sum::<f64>()
        / values.len() as f64;
    Some(variance.sqrt() / mean)
}

pub fn evaluate_calibration(observation: &CalibrationObservation) -> CalibrationEvaluation {
    let samples = observation
        .cpu_ms
        .len()
        .min(observation.accelerated_ms.len());
    // Cada lote CPU se compara contra el lote acelerado correspondiente. Las
    // muestras sobrantes de una ruta no elevan artificialmente la confianza.
    let cpu_samples = &observation.cpu_ms[..samples];
    let accelerated_samples = &observation.accelerated_ms[..samples];
    let cpu_median_ms = median(cpu_samples).unwrap_or(f64::INFINITY);
    let accelerated_median_ms = median(accelerated_samples).unwrap_or(f64::INFINITY);
    let cpu_cv = coefficient_of_variation(cpu_samples).unwrap_or(f64::INFINITY);
    let accelerated_cv = coefficient_of_variation(accelerated_samples).unwrap_or(f64::INFINITY);
    let worst_cv = cpu_cv.max(accelerated_cv);
    let stable = samples >= MIN_CALIBRATION_SAMPLES && worst_cv <= MAX_RELATIVE_VARIATION;
    let speedup = if accelerated_median_ms.is_finite() && accelerated_median_ms > 0.0 {
        cpu_median_ms / accelerated_median_ms
    } else {
        0.0
    };
    let required_speedup = if observation.stage == PlanetaryStage::Decode {
        MIN_HARDWARE_DECODE_SPEEDUP
    } else {
        MIN_GPU_COMPUTE_SPEEDUP
    };
    let stability_confidence = if stable {
        (1.0 - (worst_cv / MAX_RELATIVE_VARIATION)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let sample_confidence = (samples as f64 / 5.0).clamp(0.0, 1.0);
    CalibrationEvaluation {
        accelerated_wins: stable && observation.parity_passed && speedup >= required_speedup,
        stable,
        speedup,
        confidence: (stability_confidence * sample_confidence) as f32,
        cpu_median_ms,
        accelerated_median_ms,
        samples,
    }
}

fn stable_hash(value: impl AsRef<[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_ref());
    hex::encode(hasher.finalize())
}

fn device_fingerprint_for_app(resources: &ResourceSnapshot, application_version: &str) -> String {
    // Excluye RAM disponible/swap: son estado dinamico, no identidad del equipo.
    let mut decode_backends = resources.hardware_decode_backends.clone();
    decode_backends.sort();
    decode_backends.dedup();
    let stable = serde_json::json!({
        "algorithm": PLANETARY_PLANNER_ALGORITHM_VERSION,
        "applicationVersion": application_version,
        "os": resources.os,
        "architecture": resources.architecture,
        "cpu": resources.cpu_name,
        "physicalCpuCores": resources.physical_cpu_cores,
        "logicalCpuThreads": resources.logical_cpu_threads,
        "ramTotalMb": resources.ram_total_mb,
        "gpuName": resources.gpu_name,
        "gpuBackend": resources.gpu_backend,
        "gpuBudgetMb": resources.gpu_memory_budget_mb,
        "ffmpegVersion": resources.ffmpeg_version,
        "hardwareDecodeBackends": decode_backends,
    });
    stable_hash(serde_json::to_vec(&stable).unwrap_or_default())
}

pub fn device_fingerprint(resources: &ResourceSnapshot) -> String {
    device_fingerprint_for_app(resources, env!("CARGO_PKG_VERSION"))
}

pub fn calibration_key(
    workload: &WorkloadSignature,
    resources: &ResourceSnapshot,
    stage: PlanetaryStage,
) -> String {
    let payload = serde_json::json!({
        "algorithm": PLANETARY_PLANNER_ALGORITHM_VERSION,
        "device": device_fingerprint(resources),
        "stage": stage,
        "workload": workload,
    });
    stable_hash(serde_json::to_vec(&payload).unwrap_or_default())
}

pub fn calibration_profile_path(config_dir: &Path) -> PathBuf {
    config_dir.join("planetary-device-profiles-v2.json")
}

fn read_store(path: &Path) -> CalibrationStore {
    let Ok(raw) = std::fs::read(path) else {
        return CalibrationStore::default();
    };
    let Ok(store) = serde_json::from_slice::<CalibrationStore>(&raw) else {
        return CalibrationStore::default();
    };
    if store.schema != CALIBRATION_SCHEMA
        || store.algorithm_version != PLANETARY_PLANNER_ALGORITHM_VERSION
    {
        CalibrationStore::default()
    } else {
        store
    }
}

#[cfg(target_os = "windows")]
fn replace_existing_file_atomically(path: &Path, replacement: &Path) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "Kernel32")]
    extern "system" {
        fn ReplaceFileW(
            replaced_file_name: *const u16,
            replacement_file_name: *const u16,
            backup_file_name: *const u16,
            replace_flags: u32,
            exclude: *mut c_void,
            reserved: *mut c_void,
        ) -> i32;
    }

    let replaced = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replacement = replacement
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        ReplaceFileW(
            replaced.as_ptr(),
            replacement.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn publish_store_atomically(path: &Path, temporary: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        if path.exists() {
            replace_existing_file_atomically(path, temporary)
        } else {
            match std::fs::rename(temporary, path) {
                Ok(()) => Ok(()),
                // Otro escritor puede haber publicado entre `exists` y
                // `rename`; ReplaceFileW conserva entonces el reemplazo sin
                // una ventana en la que falte el perfil anterior.
                Err(error) if path.exists() => {
                    replace_existing_file_atomically(path, temporary).map_err(|_| error)
                }
                Err(error) => Err(error),
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::fs::rename(temporary, path)
    }
}

fn write_store_atomically(path: &Path, store: &CalibrationStore) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "La ruta de perfiles no tiene directorio padre".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("No se pudo crear el directorio de perfiles: {e}"))?;
    let sequence = CALIBRATION_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".planetary-device-profiles-{}-{}-{}.tmp",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        sequence,
    ));
    let payload = serde_json::to_vec_pretty(store)
        .map_err(|e| format!("No se pudo serializar la calibracion: {e}"))?;
    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&payload)?;
        file.sync_all()?;
        publish_store_atomically(path, &temporary)?;
        #[cfg(not(target_os = "windows"))]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    write_result.map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        format!("No se pudo publicar la calibracion atomica: {e}")
    })
}

pub fn load_calibration(
    path: &Path,
    key: &str,
    stage: PlanetaryStage,
) -> Option<CalibrationObservation> {
    read_store(path)
        .entries
        .get(&format!("{key}:{stage:?}"))
        .filter(|entry| entry.stage == stage)
        .cloned()
}

pub fn save_calibration(
    path: &Path,
    key: &str,
    observation: CalibrationObservation,
) -> Result<(), String> {
    let _write_guard = CALIBRATION_STORE_WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut store = read_store(path);
    store
        .entries
        .insert(format!("{key}:{:?}", observation.stage), observation);
    while store.entries.len() > MAX_PERSISTED_CALIBRATIONS {
        let Some(oldest_key) = store
            .entries
            .iter()
            .min_by_key(|(_, value)| &value.captured_at_utc)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        store.entries.remove(&oldest_key);
    }
    write_store_atomically(path, &store)
}

fn sample_system_with_cpu_load() -> (sysinfo::System, u8) {
    let mut system = sysinfo::System::new_all();
    // sysinfo necesita dos muestras separadas para que cpu_usage no sea el
    // promedio desde el arranque. El coste (~200 ms) ocurre una sola vez antes
    // de congelar el trabajo y evita planear 8+8 hilos sobre una máquina ya
    // ocupada por otros procesos.
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL + std::time::Duration::from_millis(5));
    system.refresh_cpu_usage();
    system.refresh_memory();
    let cpu_load_percent = system
        .global_cpu_info()
        .cpu_usage()
        .round()
        .clamp(0.0, 100.0) as u8;
    (system, cpu_load_percent)
}

fn resolved_available_ram_mb(system: &sysinfo::System) -> (u64, u64) {
    let ram_total_mb = system.total_memory() / 1_048_576;
    let reported_available_mb = system.available_memory() / 1_048_576;
    let derived_available_mb =
        system.total_memory().saturating_sub(system.used_memory()) / 1_048_576;
    let ram_available_mb = if ram_total_mb == 0 {
        reported_available_mb.max(derived_available_mb)
    } else {
        reported_available_mb
            .max(derived_available_mb)
            .min(ram_total_mb)
    };
    (ram_total_mb, ram_available_mb)
}

fn memory_pressure_values(ram_total_mb: u64, ram_available_mb: u64, swap_used_mb: u64) -> bool {
    swap_used_mb >= MEMORY_PRESSURE_SWAP_MB
        || (ram_total_mb > 0 && ram_available_mb.saturating_mul(8) < ram_total_mb)
}

pub fn resource_memory_pressure(resources: &ResourceSnapshot) -> bool {
    memory_pressure_values(
        resources.ram_total_mb,
        resources.ram_available_mb,
        resources.swap_used_mb,
    )
}

/// Una ruta GPU ya validada puede aliviar la CPU ocupada, pero nunca se fuerza
/// bajo presión de memoria (en Apple Silicon comparte la RAM del sistema).
pub fn cpu_pressure_prefers_gpu(resources: &ResourceSnapshot) -> bool {
    resources.cpu_load_percent >= CPU_PRESSURE_GPU_OVERRIDE_PERCENT
        && !resource_memory_pressure(resources)
}

pub fn capture_runtime_resource_pressure() -> RuntimeResourcePressure {
    let (system, cpu_load_percent) = sample_system_with_cpu_load();
    let (ram_total_mb, ram_available_mb) = resolved_available_ram_mb(&system);
    RuntimeResourcePressure {
        cpu_load_percent,
        memory_pressure: memory_pressure_values(
            ram_total_mb,
            ram_available_mb,
            system.used_swap() / 1_048_576,
        ),
    }
}

pub fn should_defer_calibration(pressure: RuntimeResourcePressure) -> bool {
    pressure.cpu_load_percent >= CPU_PRESSURE_GPU_OVERRIDE_PERCENT || pressure.memory_pressure
}

fn cpu_thread_capacity(resources: &ResourceSnapshot) -> usize {
    let logical = resources.logical_cpu_threads.max(1);
    let base = resources
        .physical_cpu_cores
        .max(1)
        .min(logical.saturating_sub(1).max(1));
    let by_load = match resources.cpu_load_percent {
        90..=u8::MAX => base.div_ceil(3),
        75..=89 => base.div_ceil(2),
        60..=74 => base.saturating_mul(3).div_ceil(4),
        _ => base,
    }
    .max(1);
    if resource_memory_pressure(resources) {
        by_load.min(base.div_ceil(2)).max(1)
    } else {
        by_load
    }
}

pub fn capture_resource_snapshot(
    ffmpeg_version: Option<String>,
    hardware_decode_backends: Vec<String>,
    probe_gpu: bool,
) -> ResourceSnapshot {
    let (system, cpu_load_percent) = sample_system_with_cpu_load();
    // CPU only no debe inicializar wgpu ni tocar un driver inestable. El
    // snapshot conserva CPU/RAM/FFmpeg y declara GPU no sondeada.
    let gpu = probe_gpu.then(crate::gpu_stack::gpu_info);
    let runtime = probe_gpu
        .then(crate::gpu_stack::gpu_runtime)
        .flatten();
    let (ram_total_mb, ram_available_mb) = resolved_available_ram_mb(&system);
    ResourceSnapshot {
        os: sysinfo::System::long_os_version().unwrap_or_else(|| std::env::consts::OS.into()),
        architecture: std::env::consts::ARCH.into(),
        cpu_name: system
            .cpus()
            .first()
            .map(|cpu| cpu.brand().trim().to_string())
            .filter(|name| !name.is_empty()),
        physical_cpu_cores: system
            .physical_core_count()
            .unwrap_or_else(num_cpus::get_physical),
        logical_cpu_threads: num_cpus::get(),
        cpu_load_percent,
        ram_total_mb,
        ram_available_mb,
        swap_used_mb: system.used_swap() / 1_048_576,
        gpu_available: gpu.as_ref().is_some_and(|gpu| gpu.available),
        gpu_name: gpu
            .as_ref()
            .filter(|gpu| gpu.available)
            .map(|gpu| gpu.name.clone()),
        gpu_backend: gpu
            .as_ref()
            .filter(|gpu| gpu.available)
            .map(|gpu| gpu.backend.clone()),
        gpu_memory_total_mb: None,
        gpu_memory_budget_mb: gpu.as_ref().map_or(0, |gpu| gpu.vram_budget_mb),
        gpu_max_buffer_bytes: runtime.map(|rt| rt.max_buffer_size),
        ffmpeg_version,
        hardware_decode_backends,
    }
}

pub fn build_workload_signature(context: PlanetaryWorkloadContext<'_>) -> WorkloadSignature {
    let descriptor = context.descriptor;
    let source_kind = match descriptor.source_kind.to_ascii_lowercase().as_str() {
        "ser" => PlanetarySourceKind::NativeSer,
        "ffmpeg" => PlanetarySourceKind::Ffmpeg,
        "avi" | "fits sequence" | "fits" => PlanetarySourceKind::ImageSequence,
        _ => PlanetarySourceKind::Unknown,
    };
    let sample_layout = if descriptor.bayer.is_some() {
        SampleLayout::Cfa
    } else if descriptor.bytes_per_pixel <= 2 {
        SampleLayout::Mono
    } else {
        SampleLayout::InterleavedColor
    };
    WorkloadSignature {
        source_kind,
        reader: Some(descriptor.source_kind.clone()),
        codec: context.codec.map(str::to_string),
        pixel_format: descriptor.pixel_format.clone(),
        sample_bits: descriptor.sample_bits.min(u8::MAX as usize) as u8,
        sample_layout,
        cfa_pattern: descriptor.bayer.clone(),
        byte_order: if descriptor.bytes_per_pixel < 2 {
            SampleByteOrder::NotApplicable
        } else if descriptor.little_endian {
            SampleByteOrder::LittleEndian
        } else {
            SampleByteOrder::BigEndian
        },
        width: descriptor.width.min(u32::MAX as usize) as u32,
        height: descriptor.height.min(u32::MAX as usize) as u32,
        roi: context.roi,
        target_type: context.target_type.into(),
        is_surface: context.is_surface,
        selected_frames: context.selected_frames,
        ap_count: context.ap_count,
        ap_size: context.ap_size,
        drizzle: context.drizzle,
        double_pass: context.double_pass,
        quality_policy: QualityPolicy::Adaptive,
        color_range: descriptor.color_range.clone(),
        color_matrix: descriptor.color_matrix.clone(),
        rotation_degrees: descriptor
            .rotation_degrees
            .clamp(i16::MIN as i32, i16::MAX as i32) as i16,
    }
}

fn stage_is_surface_analysis(stage: PlanetaryStage) -> bool {
    matches!(
        stage,
        PlanetaryStage::Preprocess
            | PlanetaryStage::QualitySelection
            | PlanetaryStage::MasterReference
            | PlanetaryStage::CoarseSad
            | PlanetaryStage::FineSad
            | PlanetaryStage::ApAlignment
            | PlanetaryStage::WarpMap
            | PlanetaryStage::Enhance
    )
}

fn stage_gpu_eligible(stage: PlanetaryStage) -> bool {
    matches!(
        stage,
        PlanetaryStage::Preprocess
            | PlanetaryStage::CoarseSad
            | PlanetaryStage::Enhance
            | PlanetaryStage::Accumulation
    )
}

fn stage_parity(stage: PlanetaryStage, capabilities: PlannerCapabilities) -> bool {
    match stage {
        PlanetaryStage::Preprocess => capabilities.preprocess_parity,
        PlanetaryStage::CoarseSad => capabilities.coarse_sad_parity,
        PlanetaryStage::Enhance => capabilities.enhance_parity,
        PlanetaryStage::Accumulation => capabilities.accumulation_parity,
        _ => false,
    }
}

fn available_ram_mb(resources: &ResourceSnapshot) -> u64 {
    match (resources.ram_total_mb, resources.ram_available_mb) {
        (0, 0) => 512,
        (0, available) => available,
        (total, 0) => (total / 2).max(1),
        (total, available) => available.min(total),
    }
}

fn planner_ram_budget_mb(resources: &ResourceSnapshot) -> u64 {
    let available = available_ram_mb(resources);
    let reserve_target =
        (resources.ram_total_mb / 10).clamp(MIN_RAM_RESERVE_MB, MAX_RAM_RESERVE_MB);
    // En equipos de RAM baja se conserva al menos la mitad de la memoria
    // observada para el trabajo, sin inventar memoria por encima de available.
    let reserve = reserve_target.min(available / 2);
    available.saturating_sub(reserve).max(1)
}

fn canonical_frame_bytes(workload: &WorkloadSignature) -> Result<u64, String> {
    let pixels = workload
        .effective_pixels()
        .filter(|pixels| *pixels > 0)
        .ok_or_else(|| "La firma planetaria no contiene una geometria valida".to_string())?;
    let channels = if matches!(
        workload.sample_layout,
        SampleLayout::Cfa | SampleLayout::InterleavedColor | SampleLayout::PlanarColor
    ) {
        3u64
    } else {
        1u64
    };
    // El ring canónico conserva 16 bits por muestra (`gray16`/`rgb48`),
    // también cuando la fuente original tiene 8, 10, 12 o 14 bits.
    pixels
        .checked_mul(channels)
        .and_then(|samples| samples.checked_mul(2))
        .ok_or_else(|| "La geometria planetaria excede el espacio direccionable".to_string())
}

fn gpu_memory_gate(
    workload: &WorkloadSignature,
    resources: &ResourceSnapshot,
) -> Result<Option<String>, String> {
    let frame_bytes = canonical_frame_bytes(workload)?;
    if let Some(max_buffer_bytes) = resources.gpu_max_buffer_bytes {
        if max_buffer_bytes > 0 && frame_bytes > max_buffer_bytes {
            return Ok(Some(format!(
                "un frame canonico requiere {frame_bytes} bytes y el buffer GPU admite {max_buffer_bytes}"
            )));
        }
    }
    if resources.gpu_memory_budget_mb > 0 {
        let vram_bytes = resources.gpu_memory_budget_mb.saturating_mul(1_048_576);
        let required = frame_bytes.saturating_mul(GPU_WORKING_SET_FRAMES);
        if required > vram_bytes {
            return Ok(Some(format!(
                "el working set GPU requiere {required} bytes y el presupuesto VRAM es {vram_bytes}"
            )));
        }
    }
    Ok(None)
}

fn populate_resource_controls(
    decision: &mut StageDecision,
    workload: &WorkloadSignature,
    resources: &ResourceSnapshot,
) -> Result<(), String> {
    let capacity = cpu_thread_capacity(resources);
    // Decode y scoring se solapan. Bajo presión externa, entregar la misma
    // capacidad completa a FFmpeg y Rayon recrearía la sobresuscripción que
    // este snapshot intenta evitar. Sin presión se conserva el presupuesto
    // calibrado histórico (la alimentación del pipe necesita esos hilos).
    let threads = if decision.stage == PlanetaryStage::Decode
        && workload.source_kind == PlanetarySourceKind::Ffmpeg
        && resources.cpu_load_percent >= CPU_PRESSURE_GPU_OVERRIDE_PERCENT
    {
        capacity.div_ceil(2).max(1)
    } else {
        capacity
    };
    let pixels = workload.effective_pixels().unwrap_or(1).max(1);
    let bytes_per_frame = canonical_frame_bytes(workload)?;
    let ram_budget_mb = planner_ram_budget_mb(resources);
    let working_bytes = ram_budget_mb.saturating_mul(1_048_576);
    let ring_budget_bytes =
        working_bytes.saturating_mul(RING_RAM_FRACTION_NUMERATOR) / RING_RAM_FRACTION_DENOMINATOR;
    if ring_budget_bytes < bytes_per_frame {
        return Err(format!(
            "RAM insuficiente para un frame canonico: frame={bytes_per_frame} bytes, ring={ring_budget_bytes} bytes"
        ));
    }
    let target_batch_size = if pixels >= 12_000_000 {
        2
    } else if pixels >= 4_000_000 {
        4
    } else {
        6
    };
    let selected_frames = workload.selected_frames.max(1);
    let ring_slots = (ring_budget_bytes / bytes_per_frame)
        .max(1)
        .min(target_batch_size as u64)
        .min(selected_frames as u64);
    let mut batch_size = ring_slots as usize;
    let ring_bytes = bytes_per_frame.saturating_mul(ring_slots);

    // Un lane incluye el frame preparado inmutable, scratch grande y salida.
    // Se calcula después de reservar el ring para que ambos consumidores no
    // presupuesten dos veces los mismos bytes.
    let per_lane_bytes = bytes_per_frame.saturating_mul(3).max(1);
    let lane_budget_bytes = working_bytes.saturating_sub(ring_bytes);
    let lanes_by_ram = (lane_budget_bytes / per_lane_bytes).max(1) as usize;
    let mut frame_concurrency = lanes_by_ram.min(threads).min(selected_frames).max(1);

    if matches!(
        decision.effective_engine,
        EffectiveEngine::GpuCompute | EffectiveEngine::HybridPipeline
    ) && resources.gpu_memory_budget_mb > 0
    {
        let vram_bytes = resources.gpu_memory_budget_mb.saturating_mul(1_048_576);
        let gpu_lanes = (vram_bytes
            / bytes_per_frame
                .saturating_mul(GPU_WORKING_SET_FRAMES)
                .max(1))
        .max(1) as usize;
        frame_concurrency = frame_concurrency.min(gpu_lanes).max(1);
        batch_size = batch_size.min(gpu_lanes).max(1);
    }
    decision.compute_threads = threads;
    decision.frame_concurrency = frame_concurrency;
    decision.scratch_slots = frame_concurrency;
    decision.ap_workers = threads.min(workload.ap_count.max(1));
    decision.batch_size = batch_size;
    decision.ring_bytes = bytes_per_frame.saturating_mul(batch_size as u64);
    decision.ram_budget_mb = ram_budget_mb;
    decision.vram_budget_mb = resources.gpu_memory_budget_mb;
    Ok(())
}

/// Construye un plan mutable para que el coordinador pueda aplicar la
/// calibracion local antes de comenzar el trabajo. El draft nunca debe
/// publicarse como el plan efectivo: `build_safe_plan` o `freeze` cierran el
/// contrato para toda la ejecucion.
pub fn build_safe_plan_draft(
    plan_id: impl Into<String>,
    workload: WorkloadSignature,
    resources: ResourceSnapshot,
    compute_policy: ComputePolicy,
    decode_policy: DecodePolicy,
    capabilities: PlannerCapabilities,
) -> Result<PlanetaryExecutionPlan, String> {
    let device_id = device_fingerprint(&resources);
    let mut plan = PlanetaryExecutionPlan::new(
        plan_id,
        workload.clone(),
        resources.clone(),
        compute_policy,
        decode_policy,
    );
    plan.algorithm_version = PLANETARY_PLANNER_ALGORITHM_VERSION.into();
    plan.requested_quality_policy = QualityPolicy::Adaptive;
    plan.quality_plan = PlanetaryQualityPlan::for_workload(&workload, QualityPolicy::Adaptive);
    plan.device_fingerprint = device_id;
    plan.ram_budget_mb = planner_ram_budget_mb(&resources);
    plan.vram_budget_mb = resources.gpu_memory_budget_mb;
    let gpu_memory_issue = gpu_memory_gate(&workload, &resources)?;

    for stage in PlanetaryStage::ORDERED {
        let mut decision = StageDecision::new(
            stage,
            compute_policy,
            decode_policy,
            EffectiveEngine::CpuSimd,
        );

        if stage == PlanetaryStage::Decode {
            match workload.source_kind {
                PlanetarySourceKind::Ffmpeg => match decode_policy {
                    DecodePolicy::Auto | DecodePolicy::Software => {
                        decision.effective_engine = EffectiveEngine::FfmpegSoftware;
                        decision.reason = if decode_policy == DecodePolicy::Software {
                            DecisionReason::RequestedStrict
                        } else {
                            DecisionReason::SafeDefault
                        };
                        decision.parity_status = ParityStatus::Passed;
                    }
                    DecodePolicy::Hardware => {
                        let backend = resources
                            .hardware_decode_backends
                            .first()
                            .cloned()
                            .ok_or_else(|| {
                                "Decode hardware strict no tiene un backend confirmado en preflight"
                                    .to_string()
                            })?;
                        decision.effective_engine = EffectiveEngine::FfmpegHardware;
                        decision.backend = Some(backend);
                        decision.reason = DecisionReason::RequestedStrict;
                        decision.parity_status = ParityStatus::Unknown;
                    }
                },
                _ => {
                    if decode_policy == DecodePolicy::Hardware {
                        return Err(
                            "Decode Hardware no aplica a SER/AVI/FITS nativos; seleccione Auto o Software para conservar la lectura mmap/directa"
                                .into(),
                        );
                    }
                    decision.effective_engine = EffectiveEngine::NativeIo;
                    decision.reason = DecisionReason::SourceNative;
                    decision.reason_detail =
                        Some("DecodePolicy no aplica al lector nativo; SER conserva mmap".into());
                    decision.parity_status = ParityStatus::NotApplicable;
                }
            }
            populate_resource_controls(&mut decision, &workload, &resources)?;
            plan.set_stage(decision)?;
            continue;
        }

        if matches!(
            stage,
            PlanetaryStage::CanonicalizeDebayer
                | PlanetaryStage::QualitySelection
                | PlanetaryStage::MasterReference
                | PlanetaryStage::FineSad
                | PlanetaryStage::ApAlignment
                | PlanetaryStage::WarpMap
                | PlanetaryStage::Postprocess
                | PlanetaryStage::Publish
        ) {
            decision.effective_engine = EffectiveEngine::RequiredCpu;
            decision.required_cpu = true;
            decision.reason = DecisionReason::RequiredCpu;
            decision.parity_status = ParityStatus::NotApplicable;
            populate_resource_controls(&mut decision, &workload, &resources)?;
            plan.set_stage(decision)?;
            continue;
        }

        let parity_ok = stage_parity(stage, capabilities);
        let gpu_present = capabilities.gpu_available && resources.gpu_available;
        let gpu_ready =
            gpu_present && parity_ok && stage_gpu_eligible(stage) && gpu_memory_issue.is_none();
        let surface_safe_cpu = workload.is_surface && stage_is_surface_analysis(stage);
        let auto_requires_calibration = surface_safe_cpu || stage == PlanetaryStage::Enhance;
        match compute_policy {
            ComputePolicy::CpuOnly => {
                decision.effective_engine = EffectiveEngine::CpuSimd;
                decision.reason = DecisionReason::RequestedStrict;
                decision.parity_status = ParityStatus::NotApplicable;
            }
            ComputePolicy::Auto if auto_requires_calibration => {
                decision.effective_engine = EffectiveEngine::CpuSimd;
                decision.reason = DecisionReason::SafeDefault;
                decision.reason_detail = Some(if stage == PlanetaryStage::Enhance {
                    "enhance GPU requiere calibracion completa de etapa".into()
                } else {
                    "perfil temporal seguro para superficie/disco grande".into()
                });
                decision.parity_status = ParityStatus::NotApplicable;
            }
            ComputePolicy::Auto => {
                if gpu_ready {
                    decision.effective_engine = EffectiveEngine::GpuCompute;
                    decision.reason = DecisionReason::CapabilityGate;
                    decision.parity_status = ParityStatus::Passed;
                    decision.backend = resources.gpu_backend.clone();
                } else {
                    decision.effective_engine = EffectiveEngine::CpuSimd;
                    decision.reason = if gpu_memory_issue.is_some() && gpu_present {
                        DecisionReason::MemoryGate
                    } else if gpu_present && !parity_ok {
                        DecisionReason::ParityGate
                    } else {
                        DecisionReason::CapabilityGate
                    };
                    decision.reason_detail = gpu_memory_issue.clone();
                    decision.parity_status = if gpu_present && !parity_ok {
                        ParityStatus::Failed
                    } else {
                        ParityStatus::NotApplicable
                    };
                }
            }
            ComputePolicy::Hybrid => {
                if gpu_ready {
                    decision.effective_engine = EffectiveEngine::HybridPipeline;
                    decision.reason = DecisionReason::RequestedStrict;
                    decision.parity_status = ParityStatus::Passed;
                    decision.backend = resources.gpu_backend.clone();
                } else {
                    decision.effective_engine = EffectiveEngine::CpuSimd;
                    decision.reason = if gpu_memory_issue.is_some() && gpu_present {
                        DecisionReason::MemoryGate
                    } else {
                        DecisionReason::Fallback
                    };
                    decision.reason_detail = gpu_memory_issue.clone();
                    decision.parity_status = if gpu_present && !parity_ok {
                        ParityStatus::Failed
                    } else {
                        ParityStatus::NotApplicable
                    };
                }
            }
            ComputePolicy::GpuOnly => {
                if !gpu_ready {
                    let detail = gpu_memory_issue
                        .clone()
                        .unwrap_or_else(|| "capacidad o paridad no satisfecha".into());
                    return Err(format!("GPU only: la etapa {stage:?} no es apta: {detail}"));
                }
                decision.effective_engine = EffectiveEngine::GpuCompute;
                decision.reason = DecisionReason::RequestedStrict;
                decision.parity_status = ParityStatus::Passed;
                decision.backend = resources.gpu_backend.clone();
            }
        }
        populate_resource_controls(&mut decision, &workload, &resources)?;
        plan.set_stage(decision)?;
    }

    plan.calibration_key = Some(calibration_key(
        &workload,
        &resources,
        PlanetaryStage::Preprocess,
    ));
    Ok(plan)
}

/// Construye y congela el perfil seguro cuando no hay calibracion por aplicar.
pub fn build_safe_plan(
    plan_id: impl Into<String>,
    workload: WorkloadSignature,
    resources: ResourceSnapshot,
    compute_policy: ComputePolicy,
    decode_policy: DecodePolicy,
    capabilities: PlannerCapabilities,
) -> Result<PlanetaryExecutionPlan, String> {
    let mut plan = build_safe_plan_draft(
        plan_id,
        workload,
        resources,
        compute_policy,
        decode_policy,
        capabilities,
    )?;
    plan.freeze()?;
    Ok(plan)
}

/// Aplica una observacion a un plan aun no congelado. Se expone separada para
/// que el coordinador pueda medir tres lotes sin permitir cambios a mitad de
/// un trabajo ya iniciado.
pub fn apply_calibration(
    plan: &mut PlanetaryExecutionPlan,
    observation: &CalibrationObservation,
) -> Result<CalibrationEvaluation, String> {
    if plan.frozen {
        return Err("No se puede recalibrar un plan congelado".into());
    }
    let evaluation = evaluate_calibration(observation);
    let mut decision = plan
        .stage(observation.stage)
        .cloned()
        .ok_or_else(|| format!("La etapa {:?} no existe en el plan", observation.stage))?;
    decision.calibration_samples = evaluation.samples;
    decision.calibration_confidence = Some(evaluation.confidence);
    decision.reason_detail = Some(format!(
        "calibracion: n={}, speedup={:.4}, cv_estable={}, paridad={}",
        evaluation.samples, evaluation.speedup, evaluation.stable, observation.parity_passed
    ));

    if observation.stage == PlanetaryStage::Decode {
        // SER y secuencias nativas no tienen una alternativa FFmpeg que
        // calibrar. Hardware es no aplicable, no un motivo para abandonar mmap.
        if matches!(decision.effective_engine, EffectiveEngine::NativeIo) {
            decision.reason = DecisionReason::SourceNative;
            decision.parity_status = ParityStatus::NotApplicable;
            decision.estimated_ms = evaluation
                .cpu_median_ms
                .is_finite()
                .then_some(evaluation.cpu_median_ms);
            populate_resource_controls(&mut decision, &plan.workload, &plan.resources)?;
            plan.set_stage(decision)?;
            return Ok(evaluation);
        }

        let confirmed_decode_backend = observation
            .backend
            .clone()
            .or_else(|| plan.resources.hardware_decode_backends.first().cloned());
        match plan.requested_decode_policy {
            DecodePolicy::Software => {
                decision.effective_engine = EffectiveEngine::FfmpegSoftware;
                decision.backend = None;
                decision.reason = DecisionReason::RequestedStrict;
                decision.parity_status = ParityStatus::NotApplicable;
                decision.estimated_ms = evaluation
                    .cpu_median_ms
                    .is_finite()
                    .then_some(evaluation.cpu_median_ms);
            }
            DecodePolicy::Hardware => {
                if !observation.parity_passed {
                    return Err(
                        "Decode hardware strict no supera el gate de paridad frente a software"
                            .into(),
                    );
                }
                let backend = confirmed_decode_backend
                    .clone()
                    .or_else(|| decision.backend.clone())
                    .ok_or_else(|| {
                        "Decode hardware strict no reporto un backend confirmado".to_string()
                    })?;
                decision.effective_engine = EffectiveEngine::FfmpegHardware;
                decision.backend = Some(backend);
                decision.reason = DecisionReason::RequestedStrict;
                decision.parity_status = ParityStatus::Passed;
                decision.estimated_ms = evaluation
                    .accelerated_median_ms
                    .is_finite()
                    .then_some(evaluation.accelerated_median_ms);
            }
            DecodePolicy::Auto => {
                if evaluation.accelerated_wins && confirmed_decode_backend.is_some() {
                    decision.effective_engine = EffectiveEngine::FfmpegHardware;
                    decision.backend = confirmed_decode_backend;
                    decision.reason = DecisionReason::CalibrationWinner;
                    decision.parity_status = ParityStatus::Passed;
                    decision.estimated_ms = Some(evaluation.accelerated_median_ms);
                } else {
                    decision.effective_engine = EffectiveEngine::FfmpegSoftware;
                    decision.backend = None;
                    decision.reason = if evaluation.accelerated_wins {
                        DecisionReason::CapabilityGate
                    } else if !observation.parity_passed {
                        DecisionReason::ParityGate
                    } else if evaluation.stable {
                        DecisionReason::CalibrationWinner
                    } else {
                        DecisionReason::CalibrationUnstable
                    };
                    decision.parity_status = if observation.parity_passed {
                        ParityStatus::Passed
                    } else {
                        ParityStatus::Failed
                    };
                    decision.estimated_ms = evaluation
                        .cpu_median_ms
                        .is_finite()
                        .then_some(evaluation.cpu_median_ms);
                }
            }
        }
        populate_resource_controls(&mut decision, &plan.workload, &plan.resources)?;
        plan.set_stage(decision)?;
        return Ok(evaluation);
    }

    // Estas etapas permanecen en la referencia CPU aun si se entrega por
    // error una observacion acelerada (en particular fine SAD).
    if !stage_gpu_eligible(observation.stage) {
        decision.effective_engine = if decision.required_cpu {
            EffectiveEngine::RequiredCpu
        } else {
            EffectiveEngine::CpuSimd
        };
        decision.backend = None;
        decision.reason = if decision.required_cpu {
            DecisionReason::RequiredCpu
        } else {
            DecisionReason::SafeDefault
        };
        decision.parity_status = ParityStatus::NotApplicable;
        decision.estimated_ms = evaluation
            .cpu_median_ms
            .is_finite()
            .then_some(evaluation.cpu_median_ms);
        populate_resource_controls(&mut decision, &plan.workload, &plan.resources)?;
        plan.set_stage(decision)?;
        return Ok(evaluation);
    }

    let gpu_memory_issue = gpu_memory_gate(&plan.workload, &plan.resources)?;
    let gpu_available = plan.resources.gpu_available && gpu_memory_issue.is_none();
    match plan.requested_compute_policy {
        ComputePolicy::CpuOnly => {
            decision.effective_engine = EffectiveEngine::CpuSimd;
            decision.backend = None;
            decision.reason = DecisionReason::RequestedStrict;
            decision.parity_status = ParityStatus::NotApplicable;
            decision.estimated_ms = evaluation
                .cpu_median_ms
                .is_finite()
                .then_some(evaluation.cpu_median_ms);
        }
        ComputePolicy::GpuOnly => {
            if !gpu_available || !observation.parity_passed {
                return Err(format!(
                    "GPU only: la calibracion de {:?} no supera capacidad/memoria/paridad",
                    observation.stage,
                ));
            }
            decision.effective_engine = EffectiveEngine::GpuCompute;
            decision.backend = observation
                .backend
                .clone()
                .or_else(|| decision.backend.clone())
                .or_else(|| plan.resources.gpu_backend.clone());
            decision.reason = DecisionReason::RequestedStrict;
            decision.parity_status = ParityStatus::Passed;
            decision.estimated_ms = evaluation
                .accelerated_median_ms
                .is_finite()
                .then_some(evaluation.accelerated_median_ms);
        }
        ComputePolicy::Hybrid => {
            if gpu_available && observation.parity_passed {
                decision.effective_engine = EffectiveEngine::HybridPipeline;
                decision.backend = observation
                    .backend
                    .clone()
                    .or_else(|| decision.backend.clone())
                    .or_else(|| plan.resources.gpu_backend.clone());
                decision.reason = DecisionReason::RequestedStrict;
                decision.parity_status = ParityStatus::Passed;
                decision.estimated_ms = evaluation
                    .accelerated_median_ms
                    .is_finite()
                    .then_some(evaluation.accelerated_median_ms);
            } else {
                decision.effective_engine = EffectiveEngine::CpuSimd;
                decision.backend = None;
                decision.reason = if gpu_memory_issue.is_some() {
                    DecisionReason::MemoryGate
                } else if !plan.resources.gpu_available {
                    DecisionReason::CapabilityGate
                } else {
                    DecisionReason::ParityGate
                };
                decision.parity_status = if observation.parity_passed {
                    ParityStatus::NotApplicable
                } else {
                    ParityStatus::Failed
                };
                decision.estimated_ms = evaluation
                    .cpu_median_ms
                    .is_finite()
                    .then_some(evaluation.cpu_median_ms);
            }
        }
        ComputePolicy::Auto => {
            if evaluation.accelerated_wins && gpu_available {
                decision.effective_engine = EffectiveEngine::GpuCompute;
                decision.backend = observation
                    .backend
                    .clone()
                    .or_else(|| plan.resources.gpu_backend.clone());
                decision.reason = DecisionReason::CalibrationWinner;
                decision.parity_status = ParityStatus::Passed;
                decision.estimated_ms = Some(evaluation.accelerated_median_ms);
            } else {
                decision.effective_engine = EffectiveEngine::CpuSimd;
                decision.backend = None;
                decision.reason = if evaluation.accelerated_wins && gpu_memory_issue.is_some() {
                    DecisionReason::MemoryGate
                } else if evaluation.accelerated_wins && !plan.resources.gpu_available {
                    DecisionReason::CapabilityGate
                } else if !observation.parity_passed {
                    DecisionReason::ParityGate
                } else if evaluation.stable {
                    DecisionReason::CalibrationWinner
                } else {
                    DecisionReason::CalibrationUnstable
                };
                decision.parity_status = if observation.parity_passed {
                    ParityStatus::Passed
                } else {
                    ParityStatus::Failed
                };
                decision.estimated_ms = evaluation
                    .cpu_median_ms
                    .is_finite()
                    .then_some(evaluation.cpu_median_ms);
            }
        }
    }
    populate_resource_controls(&mut decision, &plan.workload, &plan.resources)?;
    plan.set_stage(decision)?;
    Ok(evaluation)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(source: &str, pixels: (usize, usize)) -> FrameDescriptor {
        FrameDescriptor {
            source_kind: source.into(),
            width: pixels.0,
            height: pixels.1,
            frame_count: 1_000,
            bytes_per_pixel: 2,
            sample_bits: 12,
            color_id: 8,
            bayer: Some("RGGB".into()),
            little_endian: true,
            rotation_degrees: 0,
            pixel_format: None,
            color_range: None,
            color_matrix: None,
        }
    }

    fn resources() -> ResourceSnapshot {
        ResourceSnapshot {
            os: "macos".into(),
            architecture: "aarch64".into(),
            cpu_name: Some("test-cpu".into()),
            physical_cpu_cores: 8,
            logical_cpu_threads: 10,
            ram_total_mb: 24_000,
            ram_available_mb: 16_000,
            gpu_available: true,
            gpu_name: Some("test-gpu".into()),
            gpu_backend: Some("Metal".into()),
            gpu_memory_budget_mb: 3_000,
            gpu_max_buffer_bytes: Some(1_073_741_824),
            ffmpeg_version: Some("ffmpeg-test-1".into()),
            hardware_decode_backends: vec!["videotoolbox".into()],
            ..ResourceSnapshot::default()
        }
    }

    fn capabilities() -> PlannerCapabilities {
        PlannerCapabilities {
            gpu_available: true,
            preprocess_parity: true,
            coarse_sad_parity: true,
            enhance_parity: true,
            accumulation_parity: true,
        }
    }

    fn workload(source: &str, surface: bool) -> WorkloadSignature {
        let desc = descriptor(source, (1_920, 1_080));
        build_workload_signature(PlanetaryWorkloadContext {
            descriptor: &desc,
            codec: (source.eq_ignore_ascii_case("ffmpeg")).then_some("hevc"),
            target_type: if surface { "surface" } else { "planet" },
            is_surface: surface,
            selected_frames: 500,
            ap_count: 128,
            ap_size: 48,
            drizzle: 1.0,
            double_pass: true,
            roi: None,
        })
    }

    fn observation(
        stage: PlanetaryStage,
        cpu: f64,
        accelerated: f64,
        parity: bool,
    ) -> CalibrationObservation {
        CalibrationObservation::new(
            stage,
            vec![cpu, cpu, cpu],
            vec![accelerated, accelerated, accelerated],
            parity,
            Some("Metal".into()),
        )
    }

    #[test]
    fn calibration_enforces_three_samples_compute_15_percent_decode_10_percent_and_parity() {
        let compute_boundary = CalibrationObservation::new(
            PlanetaryStage::Preprocess,
            vec![115.0, 115.0, 115.0],
            vec![100.0, 100.0, 100.0],
            true,
            Some("Metal".into()),
        );
        assert!(evaluate_calibration(&compute_boundary).accelerated_wins);
        assert!(
            !evaluate_calibration(&observation(
                PlanetaryStage::Preprocess,
                114.99,
                100.0,
                true,
            ))
            .accelerated_wins
        );

        let decode_boundary = observation(PlanetaryStage::Decode, 110.0, 100.0, true);
        assert!(evaluate_calibration(&decode_boundary).accelerated_wins);
        assert!(
            !evaluate_calibration(&observation(PlanetaryStage::Decode, 109.99, 100.0, true,))
                .accelerated_wins
        );

        let too_few = CalibrationObservation::new(
            PlanetaryStage::Decode,
            vec![120.0, 120.0],
            vec![100.0, 100.0],
            true,
            None,
        );
        assert!(!evaluate_calibration(&too_few).stable);
        assert!(
            !evaluate_calibration(&observation(PlanetaryStage::Decode, 120.0, 100.0, false,))
                .accelerated_wins
        );
    }

    #[test]
    fn calibration_rejects_cv_over_ten_percent_and_ignores_unpaired_tail() {
        let stable = CalibrationObservation::new(
            PlanetaryStage::Preprocess,
            vec![88.0, 100.0, 112.0],
            vec![70.4, 80.0, 89.6],
            true,
            None,
        );
        assert!(evaluate_calibration(&stable).stable);

        let noisy = CalibrationObservation::new(
            PlanetaryStage::Preprocess,
            vec![87.0, 100.0, 113.0],
            vec![69.6, 80.0, 90.4],
            true,
            None,
        );
        assert!(!evaluate_calibration(&noisy).stable);

        let unpaired_tail = CalibrationObservation::new(
            PlanetaryStage::Preprocess,
            vec![115.0, 115.0, 115.0, 10_000.0],
            vec![100.0, 100.0, 100.0],
            true,
            None,
        );
        let evaluation = evaluate_calibration(&unpaired_tail);
        assert_eq!(evaluation.samples, 3);
        assert!(evaluation.stable);
        assert!(evaluation.accelerated_wins);
    }

    #[test]
    fn safe_surface_auto_uses_cpu_analysis_and_gpu_accumulation() {
        let desc = descriptor("FFmpeg", (3_312, 5_888));
        let workload = build_workload_signature(PlanetaryWorkloadContext {
            descriptor: &desc,
            codec: Some("hevc"),
            target_type: "surface",
            is_surface: true,
            selected_frames: 800,
            ap_count: 1_758,
            ap_size: 48,
            drizzle: 1.0,
            double_pass: true,
            roi: None,
        });
        let plan = build_safe_plan(
            "surface-safe",
            workload,
            resources(),
            ComputePolicy::Auto,
            DecodePolicy::Auto,
            capabilities(),
        )
        .unwrap();
        assert_eq!(
            plan.stage(PlanetaryStage::Preprocess)
                .unwrap()
                .effective_engine,
            EffectiveEngine::CpuSimd
        );
        assert_eq!(
            plan.stage(PlanetaryStage::FineSad)
                .unwrap()
                .effective_engine,
            EffectiveEngine::RequiredCpu
        );
        assert_eq!(
            plan.stage(PlanetaryStage::Accumulation)
                .unwrap()
                .effective_engine,
            EffectiveEngine::GpuCompute
        );
        assert_eq!(plan.stages.len(), PlanetaryStage::ORDERED.len());
        assert_eq!(
            plan.stage(PlanetaryStage::QualitySelection)
                .unwrap()
                .effective_engine,
            EffectiveEngine::RequiredCpu
        );
        assert_eq!(
            plan.stage(PlanetaryStage::WarpMap)
                .unwrap()
                .effective_engine,
            EffectiveEngine::RequiredCpu
        );
        assert_eq!(plan.requested_quality_policy, QualityPolicy::Adaptive);
        assert!(plan.quality_plan.frozen);
        let accumulation = plan.stage(PlanetaryStage::Accumulation).unwrap();
        assert_eq!(accumulation.frame_concurrency, accumulation.scratch_slots);
        assert!(plan.frozen);
    }

    #[test]
    fn cpu_gpu_hybrid_and_auto_contracts_remain_distinct() {
        let cpu = build_safe_plan(
            "cpu",
            workload("FFmpeg", false),
            resources(),
            ComputePolicy::CpuOnly,
            DecodePolicy::Software,
            capabilities(),
        )
        .unwrap();
        for stage in [
            PlanetaryStage::Preprocess,
            PlanetaryStage::CoarseSad,
            PlanetaryStage::Enhance,
            PlanetaryStage::Accumulation,
        ] {
            assert_eq!(
                cpu.stage(stage).unwrap().effective_engine,
                EffectiveEngine::CpuSimd
            );
        }

        let gpu = build_safe_plan(
            "gpu",
            workload("FFmpeg", false),
            resources(),
            ComputePolicy::GpuOnly,
            DecodePolicy::Auto,
            capabilities(),
        )
        .unwrap();
        assert_eq!(
            gpu.stage(PlanetaryStage::CoarseSad)
                .unwrap()
                .effective_engine,
            EffectiveEngine::GpuCompute
        );
        assert_eq!(
            gpu.stage(PlanetaryStage::FineSad).unwrap().effective_engine,
            EffectiveEngine::RequiredCpu
        );

        let hybrid = build_safe_plan(
            "hybrid",
            workload("FFmpeg", false),
            resources(),
            ComputePolicy::Hybrid,
            DecodePolicy::Auto,
            capabilities(),
        )
        .unwrap();
        assert_eq!(
            hybrid
                .stage(PlanetaryStage::Accumulation)
                .unwrap()
                .effective_engine,
            EffectiveEngine::HybridPipeline
        );

        let auto_gpu = build_safe_plan(
            "auto-gpu",
            workload("FFmpeg", false),
            resources(),
            ComputePolicy::Auto,
            DecodePolicy::Auto,
            capabilities(),
        )
        .unwrap();
        assert_eq!(
            auto_gpu
                .stage(PlanetaryStage::Enhance)
                .unwrap()
                .effective_engine,
            EffectiveEngine::CpuSimd
        );
        assert_eq!(
            auto_gpu
                .stage(PlanetaryStage::Accumulation)
                .unwrap()
                .effective_engine,
            EffectiveEngine::GpuCompute
        );

        let mut no_gpu = resources();
        no_gpu.gpu_available = false;
        let auto = build_safe_plan(
            "auto-no-gpu",
            workload("FFmpeg", false),
            no_gpu,
            ComputePolicy::Auto,
            DecodePolicy::Auto,
            capabilities(),
        )
        .unwrap();
        assert_eq!(
            auto.stage(PlanetaryStage::Accumulation)
                .unwrap()
                .effective_engine,
            EffectiveEngine::CpuSimd
        );
    }

    #[test]
    fn gpu_only_fails_preflight_when_parity_or_memory_gate_fails() {
        let mut no_parity = capabilities();
        no_parity.accumulation_parity = false;
        let parity_error = build_safe_plan(
            "gpu-parity",
            workload("FFmpeg", false),
            resources(),
            ComputePolicy::GpuOnly,
            DecodePolicy::Auto,
            no_parity,
        )
        .unwrap_err();
        assert!(parity_error.contains("GPU only"));

        let mut constrained = resources();
        constrained.gpu_max_buffer_bytes = Some(1_024);
        let memory_error = build_safe_plan(
            "gpu-memory",
            workload("FFmpeg", false),
            constrained,
            ComputePolicy::GpuOnly,
            DecodePolicy::Auto,
            capabilities(),
        )
        .unwrap_err();
        assert!(memory_error.contains("buffer GPU"));
    }

    #[test]
    fn native_ser_keeps_mmap_in_auto_and_rejects_hardware_strict() {
        let plan = build_safe_plan(
            "ser-native",
            workload("SER", false),
            resources(),
            ComputePolicy::Auto,
            DecodePolicy::Auto,
            capabilities(),
        )
        .unwrap();
        let decode = plan.stage(PlanetaryStage::Decode).unwrap();
        assert_eq!(decode.effective_engine, EffectiveEngine::NativeIo);
        assert_eq!(decode.parity_status, ParityStatus::NotApplicable);
        assert!(decode.strict_contract_satisfied());

        let error = build_safe_plan_draft(
            "ser-hardware-strict",
            workload("SER", false),
            resources(),
            ComputePolicy::Auto,
            DecodePolicy::Hardware,
            capabilities(),
        )
        .unwrap_err();
        assert!(error.contains("no aplica a SER/AVI/FITS"), "{error}");
    }

    #[test]
    fn ffmpeg_hardware_strict_requires_confirmed_backend() {
        let mut missing = resources();
        missing.hardware_decode_backends.clear();
        let error = build_safe_plan(
            "hardware-preflight",
            workload("FFmpeg", false),
            missing,
            ComputePolicy::Auto,
            DecodePolicy::Hardware,
            capabilities(),
        )
        .unwrap_err();
        assert!(error.contains("backend confirmado"));
    }

    #[test]
    fn draft_accepts_calibration_then_freezes_for_entire_job() {
        let mut draft = build_safe_plan_draft(
            "draft",
            workload("FFmpeg", true),
            resources(),
            ComputePolicy::Auto,
            DecodePolicy::Auto,
            capabilities(),
        )
        .unwrap();
        assert!(!draft.frozen);
        assert_eq!(
            draft
                .stage(PlanetaryStage::Preprocess)
                .unwrap()
                .effective_engine,
            EffectiveEngine::CpuSimd
        );
        let evaluation = apply_calibration(
            &mut draft,
            &observation(PlanetaryStage::Preprocess, 120.0, 100.0, true),
        )
        .unwrap();
        assert!(evaluation.accelerated_wins);
        assert_eq!(
            draft
                .stage(PlanetaryStage::Preprocess)
                .unwrap()
                .effective_engine,
            EffectiveEngine::GpuCompute
        );
        draft.freeze().unwrap();
        assert!(apply_calibration(
            &mut draft,
            &observation(PlanetaryStage::Preprocess, 120.0, 100.0, true)
        )
        .is_err());
    }

    #[test]
    fn calibration_cannot_override_strict_compute_modes_or_required_cpu() {
        let slow_gpu = observation(PlanetaryStage::Preprocess, 100.0, 120.0, true);

        let mut cpu = build_safe_plan_draft(
            "cpu-calibration",
            workload("FFmpeg", false),
            resources(),
            ComputePolicy::CpuOnly,
            DecodePolicy::Auto,
            capabilities(),
        )
        .unwrap();
        apply_calibration(&mut cpu, &slow_gpu).unwrap();
        assert_eq!(
            cpu.stage(PlanetaryStage::Preprocess)
                .unwrap()
                .effective_engine,
            EffectiveEngine::CpuSimd
        );

        let mut hybrid = build_safe_plan_draft(
            "hybrid-calibration",
            workload("FFmpeg", false),
            resources(),
            ComputePolicy::Hybrid,
            DecodePolicy::Auto,
            capabilities(),
        )
        .unwrap();
        apply_calibration(&mut hybrid, &slow_gpu).unwrap();
        assert_eq!(
            hybrid
                .stage(PlanetaryStage::Preprocess)
                .unwrap()
                .effective_engine,
            EffectiveEngine::HybridPipeline
        );

        let mut gpu = build_safe_plan_draft(
            "gpu-calibration",
            workload("FFmpeg", false),
            resources(),
            ComputePolicy::GpuOnly,
            DecodePolicy::Auto,
            capabilities(),
        )
        .unwrap();
        let parity_failure = observation(PlanetaryStage::Preprocess, 120.0, 80.0, false);
        assert!(apply_calibration(&mut gpu, &parity_failure).is_err());

        let fine = observation(PlanetaryStage::FineSad, 120.0, 80.0, true);
        apply_calibration(&mut cpu, &fine).unwrap();
        assert_eq!(
            cpu.stage(PlanetaryStage::FineSad).unwrap().effective_engine,
            EffectiveEngine::RequiredCpu
        );
    }

    #[test]
    fn ring_and_lanes_are_budgeted_by_bytes_without_oversubscription() {
        let plan = build_safe_plan(
            "resource-budget",
            workload("FFmpeg", false),
            resources(),
            ComputePolicy::Auto,
            DecodePolicy::Auto,
            capabilities(),
        )
        .unwrap();
        let stage = plan.stage(PlanetaryStage::Accumulation).unwrap();
        let frame_bytes = canonical_frame_bytes(&plan.workload).unwrap();
        assert_eq!(stage.ring_bytes, frame_bytes * stage.batch_size as u64);
        assert!(stage.ring_bytes <= stage.ram_budget_mb * 1_048_576 / 4);
        assert_eq!(stage.frame_concurrency, stage.scratch_slots);
        assert!(stage.compute_threads <= plan.resources.physical_cpu_cores);
        assert!(stage.ap_workers <= stage.compute_threads);
        assert!(stage.frame_concurrency <= stage.compute_threads);

        let mut low_ram = resources();
        low_ram.ram_total_mb = 512;
        low_ram.ram_available_mb = 256;
        let huge = descriptor("FFmpeg", (8_000, 8_000));
        let huge_workload = build_workload_signature(PlanetaryWorkloadContext {
            descriptor: &huge,
            codec: Some("hevc"),
            target_type: "surface",
            is_surface: true,
            selected_frames: 10,
            ap_count: 128,
            ap_size: 48,
            drizzle: 1.0,
            double_pass: true,
            roi: None,
        });
        assert!(build_safe_plan(
            "low-ram",
            huge_workload,
            low_ram,
            ComputePolicy::Auto,
            DecodePolicy::Auto,
            capabilities(),
        )
        .unwrap_err()
        .contains("RAM insuficiente"));
    }

    fn temporary_profile_path(label: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "zas-planner-profile-{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let path = calibration_profile_path(&root);
        (root, path)
    }

    #[test]
    fn calibration_store_roundtrip_is_atomic_and_preserves_concurrent_entries() {
        let (root, path) = temporary_profile_path("atomic");
        let decode_observation = CalibrationObservation::new(
            PlanetaryStage::Decode,
            vec![100.0, 101.0, 99.0],
            vec![80.0, 81.0, 79.0],
            true,
            Some("videotoolbox".into()),
        );
        save_calibration(&path, "key", decode_observation.clone()).unwrap();
        assert_eq!(
            load_calibration(&path, "key", PlanetaryStage::Decode),
            Some(decode_observation)
        );

        let shared_path = std::sync::Arc::new(path.clone());
        let workers = (0..8)
            .map(|index| {
                let path = std::sync::Arc::clone(&shared_path);
                std::thread::spawn(move || {
                    save_calibration(
                        &path,
                        &format!("parallel-{index}"),
                        observation(
                            PlanetaryStage::Preprocess,
                            120.0 + index as f64,
                            100.0,
                            true,
                        ),
                    )
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        for index in 0..8 {
            assert!(load_calibration(
                &path,
                &format!("parallel-{index}"),
                PlanetaryStage::Preprocess,
            )
            .is_some());
        }
        let leftovers = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn calibration_store_invalidates_schema_algorithm_and_malformed_data() {
        let (root, path) = temporary_profile_path("invalidation");
        save_calibration(
            &path,
            "key",
            observation(PlanetaryStage::Decode, 120.0, 100.0, true),
        )
        .unwrap();
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        raw["algorithmVersion"] = serde_json::json!("obsolete");
        std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        assert!(load_calibration(&path, "key", PlanetaryStage::Decode).is_none());

        save_calibration(
            &path,
            "key",
            observation(PlanetaryStage::Decode, 120.0, 100.0, true),
        )
        .unwrap();
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        raw["schema"] = serde_json::json!("obsolete");
        std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        assert!(load_calibration(&path, "key", PlanetaryStage::Decode).is_none());

        std::fs::write(&path, b"not-json").unwrap();
        assert!(load_calibration(&path, "key", PlanetaryStage::Decode).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn device_fingerprint_invalidates_ffmpeg_adapter_and_decode_backend_changes() {
        let base = resources();
        let base_id = device_fingerprint(&base);
        assert_ne!(
            device_fingerprint_for_app(&base, "old-app"),
            device_fingerprint_for_app(&base, "new-app")
        );

        let mut changed_ffmpeg = base.clone();
        changed_ffmpeg.ffmpeg_version = Some("ffmpeg-test-2".into());
        assert_ne!(base_id, device_fingerprint(&changed_ffmpeg));

        let mut changed_adapter = base.clone();
        changed_adapter.gpu_name = Some("another-gpu".into());
        assert_ne!(base_id, device_fingerprint(&changed_adapter));

        let mut changed_os = base.clone();
        changed_os.os = "new-os-version".into();
        assert_ne!(base_id, device_fingerprint(&changed_os));

        let mut changed_gpu_backend = base.clone();
        changed_gpu_backend.gpu_backend = Some("DX12".into());
        assert_ne!(base_id, device_fingerprint(&changed_gpu_backend));

        let mut changed_decode = base;
        changed_decode.hardware_decode_backends = vec!["d3d11va".into()];
        assert_ne!(base_id, device_fingerprint(&changed_decode));

        // La carga es parte del plan congelado, no de la identidad estable:
        // una aplicación abierta no invalida la calibración científica.
        let mut changed_load = resources();
        changed_load.cpu_load_percent = 93;
        assert_eq!(
            device_fingerprint(&resources()),
            device_fingerprint(&changed_load)
        );
    }

    #[test]
    fn external_pressure_caps_cpu_and_splits_ffmpeg_decode_from_compute() {
        assert!(!should_defer_calibration(RuntimeResourcePressure {
            cpu_load_percent: 69,
            memory_pressure: false,
        }));
        assert!(should_defer_calibration(RuntimeResourcePressure {
            cpu_load_percent: 70,
            memory_pressure: false,
        }));
        assert!(should_defer_calibration(RuntimeResourcePressure {
            cpu_load_percent: 10,
            memory_pressure: true,
        }));

        let workload = workload("FFmpeg", true);
        let mut clean = resources();
        clean.cpu_load_percent = 15;
        clean.swap_used_mb = 0;
        let mut clean_preprocess = StageDecision::new(
            PlanetaryStage::Preprocess,
            ComputePolicy::Auto,
            DecodePolicy::Auto,
            EffectiveEngine::CpuSimd,
        );
        populate_resource_controls(&mut clean_preprocess, &workload, &clean).unwrap();
        assert_eq!(clean_preprocess.compute_threads, 8);

        let mut pressured = clean.clone();
        pressured.cpu_load_percent = 80;
        let mut pressured_preprocess = clean_preprocess.clone();
        populate_resource_controls(&mut pressured_preprocess, &workload, &pressured).unwrap();
        assert_eq!(pressured_preprocess.compute_threads, 4);
        assert!(cpu_pressure_prefers_gpu(&pressured));

        let mut pressured_decode = StageDecision::new(
            PlanetaryStage::Decode,
            ComputePolicy::Auto,
            DecodePolicy::Auto,
            EffectiveEngine::FfmpegSoftware,
        );
        populate_resource_controls(&mut pressured_decode, &workload, &pressured).unwrap();
        assert_eq!(pressured_decode.compute_threads, 2);

        pressured.swap_used_mb = MEMORY_PRESSURE_SWAP_MB;
        assert!(!cpu_pressure_prefers_gpu(&pressured));
        populate_resource_controls(&mut pressured_preprocess, &workload, &pressured).unwrap();
        assert_eq!(pressured_preprocess.compute_threads, 4);
    }

    #[test]
    fn workload_signature_preserves_scientific_metadata_and_invalidates_calibration_key() {
        let mut desc = descriptor("FFmpeg", (1_920, 1_080));
        desc.pixel_format = Some("yuv420p10le".into());
        desc.color_range = Some("limited".into());
        desc.color_matrix = Some("bt2020nc".into());
        let base = build_workload_signature(PlanetaryWorkloadContext {
            descriptor: &desc,
            codec: Some("hevc-main10"),
            target_type: "planet",
            is_surface: false,
            selected_frames: 500,
            ap_count: 256,
            ap_size: 48,
            drizzle: 1.5,
            double_pass: true,
            roi: Some([12, 14, 1_600, 900]),
        });
        assert_eq!(base.pixel_format.as_deref(), Some("yuv420p10le"));
        assert_eq!(base.color_range.as_deref(), Some("limited"));
        assert_eq!(base.color_matrix.as_deref(), Some("bt2020nc"));

        let base_key = calibration_key(&base, &resources(), PlanetaryStage::Decode);
        let mutations: Vec<(&str, WorkloadSignature)> = vec![
            ("reader", { let mut v = base.clone(); v.reader = Some("other".into()); v }),
            ("codec", { let mut v = base.clone(); v.codec = Some("prores".into()); v }),
            ("pixelFormat", { let mut v = base.clone(); v.pixel_format = Some("p010le".into()); v }),
            ("sampleBits", { let mut v = base.clone(); v.sample_bits = 14; v }),
            ("CFA", { let mut v = base.clone(); v.cfa_pattern = Some("BGGR".into()); v }),
            ("endian", { let mut v = base.clone(); v.byte_order = SampleByteOrder::BigEndian; v }),
            ("resolution", { let mut v = base.clone(); v.width += 1; v }),
            ("ROI", { let mut v = base.clone(); v.roi = Some([13, 14, 1_600, 900]); v }),
            ("category", { let mut v = base.clone(); v.target_type = "surface".into(); v }),
            ("frames", { let mut v = base.clone(); v.selected_frames += 1; v }),
            ("AP", { let mut v = base.clone(); v.ap_count += 1; v }),
            ("drizzle", { let mut v = base.clone(); v.drizzle = 3.0; v }),
            ("doublePass", { let mut v = base.clone(); v.double_pass = false; v }),
            ("colorRange", { let mut v = base.clone(); v.color_range = Some("full".into()); v }),
            ("colorMatrix", { let mut v = base.clone(); v.color_matrix = Some("bt709".into()); v }),
        ];
        for (label, changed) in mutations {
            assert_ne!(
                base_key,
                calibration_key(&changed, &resources(), PlanetaryStage::Decode),
                "{label} debe invalidar el perfil local"
            );
        }
    }
}
