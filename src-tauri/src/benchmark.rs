use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkEnvironment {
    pub app_version: String,
    pub os: String,
    pub arch: String,
    pub cpu_threads: usize,
    pub memory_mb: u64,
    pub gpu_available: bool,
    pub gpu_name: String,
    pub gpu_backend: String,
    pub gpu_budget_mb: u64,
    pub generated_at_utc: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkManifest {
    pub suite_version: String,
    #[serde(default)]
    pub environment: Option<BenchmarkEnvironment>,
    pub datasets: Vec<BenchmarkDataset>,
}

/// Matriz de escenarios que comparte la aplicación, la documentación y los
/// validadores. Se incrusta desde `benchmarks/dataset-matrix.json` para evitar
/// que una lista Rust paralela se desincronice silenciosamente.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkDatasetMatrix {
    pub schema_version: String,
    pub required_scenarios: Vec<BenchmarkScenarioSpec>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkScenarioSpec {
    pub id: String,
    pub domain: String,
    pub required_tags: Vec<String>,
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub required_evidence: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkDataset {
    pub id: String,
    pub domain: String,
    pub sources: Vec<String>,
    #[serde(default)]
    pub zenith_output: Option<String>,
    #[serde(default)]
    pub zenith_runs: Vec<ZenithBenchmarkRun>,
    #[serde(default)]
    pub baseline_cpu_seconds: Option<f64>,
    #[serde(default)]
    pub baseline_cpu_output: Option<String>,
    #[serde(default)]
    pub baseline_cpu_processing: Option<BenchmarkOutputContract>,
    #[serde(default)]
    pub baseline_cpu_parameters: Option<serde_json::Value>,
    #[serde(default)]
    pub baseline_cpu_log: Option<String>,
    /// Ground truth lineal sin la traza y máscara registrada del artefacto.
    /// Son obligatorios para `deep-sky-satellite-trails`.
    #[serde(default)]
    pub artifact_free_reference: Option<String>,
    #[serde(default)]
    pub artifact_mask: Option<String>,
    #[serde(default)]
    pub comparators: Vec<ComparatorRun>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkCrop {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

/// Contrato de salida que evita comparar un máster lineal con otra escala,
/// recorte, drizzle, formato numérico o acabado cosmético.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkOutputContract {
    pub linear: bool,
    pub stretched: bool,
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub sample_format: String,
    pub drizzle_scale: f64,
    pub crop: BenchmarkCrop,
    #[serde(default)]
    pub post_processing: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrManyStrings {
    One(String),
    Many(Vec<String>),
}

fn deserialize_one_or_many_strings<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(
        match Option::<OneOrManyStrings>::deserialize(deserializer)? {
            None => Vec::new(),
            Some(OneOrManyStrings::One(value)) => vec![value],
            Some(OneOrManyStrings::Many(values)) => values,
        },
    )
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZenithBenchmarkRun {
    pub mode: String,
    pub cold_cache: bool,
    pub elapsed_seconds: f64,
    pub output: String,
    #[serde(default, deserialize_with = "deserialize_one_or_many_strings")]
    pub telemetry: Vec<String>,
    #[serde(default)]
    pub cache_preparation: Option<String>,
    #[serde(default)]
    pub processing: Option<BenchmarkOutputContract>,
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
    #[serde(default)]
    pub recipe: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComparatorRun {
    pub engine: String,
    pub version: String,
    pub output: String,
    pub elapsed_seconds: f64,
    #[serde(default)]
    pub log: Option<String>,
    #[serde(default)]
    pub processing: Option<BenchmarkOutputContract>,
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkManifestValidation {
    pub valid: bool,
    pub dataset_count: usize,
    pub comparator_count: usize,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinearImageComparison {
    pub compatible: bool,
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub rmse_adu: f64,
    pub normalized_rmse: f64,
    pub mean_absolute_error_adu: f64,
    pub max_absolute_error_adu: f64,
    pub flux_error_percent: f64,
    pub reference_noise_adu: f64,
    pub candidate_noise_adu: f64,
    pub noise_regression_percent: f64,
    pub reference_fwhm_px: f64,
    pub candidate_fwhm_px: f64,
    pub fwhm_regression_percent: f64,
    pub registration_error_px: f64,
    pub registration_correction_px: f64,
    pub correlation: f64,
    pub fitted_scale: f64,
    pub fitted_offset: f64,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseTelemetrySummary {
    pub samples: usize,
    pub first_ms: u128,
    pub last_ms: u128,
    pub observed_duration_ms: u128,
    pub final_engine: String,
    pub peak_ram_mb: u64,
    pub peak_vram_mb: u64,
    pub mean_throughput: Option<f32>,
    pub mean_cpu_percent: Option<f32>,
    pub mean_gpu_percent: Option<f32>,
    pub io_read_mb: f64,
    pub io_write_mb: f64,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub items_done: usize,
    pub items_total: usize,
    pub effective_engines: Vec<String>,
    pub fallback_reasons: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineTelemetryExport {
    pub schema_version: String,
    pub environment: BenchmarkEnvironment,
    pub job_id: Option<String>,
    pub phases: BTreeMap<String, PhaseTelemetrySummary>,
    pub events: Vec<crate::pipeline::RecordedPipelineTelemetry>,
}

/// Inicia el intervalo medido justo antes de abrir la entrada. La preparación
/// de caché ocurre antes de invocar este comando y se conserva como evidencia,
/// porque Zenith no puede prometer una purga de caché del sistema operativo.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BeginBenchmarkRunRequest {
    pub dataset_id: String,
    pub domain: String,
    pub mode: String,
    pub cold_cache: bool,
    pub cache_preparation: String,
    pub output_directory: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkRunSessionHandle {
    pub session_id: String,
    pub dataset_id: String,
    pub domain: String,
    pub mode: String,
    pub cold_cache: bool,
    pub cache_preparation: String,
    pub started_at_utc: String,
    pub directory: String,
    pub environment_path: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishBenchmarkRunRequest {
    pub session_id: String,
    pub output: String,
    pub job_ids: Vec<String>,
    pub processing: BenchmarkOutputContract,
    pub parameters: serde_json::Value,
    #[serde(default)]
    pub recipe: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkRunArtifact {
    pub schema_version: String,
    pub session: BenchmarkRunSessionHandle,
    pub environment: BenchmarkEnvironment,
    /// Objeto listo para copiar a `datasets[].zenithRuns[]`.
    pub zenith_run: ZenithBenchmarkRun,
    pub telemetry_path: String,
    pub record_path: String,
    pub finished_at_utc: String,
    pub job_ids: Vec<String>,
    pub phases: BTreeMap<String, PhaseTelemetrySummary>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AbortBenchmarkRunRequest {
    pub session_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AbortedBenchmarkRun {
    pub schema_version: String,
    pub session: BenchmarkRunSessionHandle,
    pub aborted_at_utc: String,
    pub elapsed_seconds: f64,
    pub reason: String,
    pub record_path: String,
}

#[derive(Clone, Debug)]
struct ActiveBenchmarkRun {
    handle: BenchmarkRunSessionHandle,
    environment: BenchmarkEnvironment,
    started: Instant,
}

static ACTIVE_BENCHMARK_RUN: OnceLock<Mutex<Option<ActiveBenchmarkRun>>> = OnceLock::new();
static BENCHMARK_RUN_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkComparisonResult {
    pub zenith_mode: String,
    pub cold_cache: bool,
    pub competitor: String,
    pub competitor_version: String,
    pub speed_ratio: f64,
    pub quality_pass: bool,
    pub metrics: LinearImageComparison,
    pub rejection_quality: Option<ArtifactRejectionComparison>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkDatasetReport {
    pub id: String,
    pub domain: String,
    pub runs: Vec<BenchmarkRunEvidence>,
    pub baseline_speedups: Vec<f64>,
    pub comparisons: Vec<BenchmarkComparisonResult>,
    pub baseline_quality: Vec<BaselineQualityResult>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkRunEvidence {
    pub zenith_mode: String,
    pub cold_cache: bool,
    pub cache_preparation: String,
    pub elapsed_seconds: f64,
    pub telemetry_files: Vec<String>,
    pub job_ids: Vec<String>,
    pub phases: BTreeMap<String, PhaseTelemetrySummary>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaselineQualityResult {
    pub zenith_mode: String,
    pub cold_cache: bool,
    pub quality_pass: bool,
    pub metrics: LinearImageComparison,
    pub rejection_quality: Option<ArtifactRejectionComparison>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactResidualMeasurement {
    pub compatible: bool,
    pub masked_pixels: usize,
    pub unmasked_pixels: usize,
    pub masked_rmse_adu: f64,
    pub unmasked_rmse_adu: f64,
    pub fitted_scale: f64,
    pub fitted_offset: f64,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRejectionComparison {
    pub reference: ArtifactResidualMeasurement,
    pub candidate: ArtifactResidualMeasurement,
    pub regression_percent: f64,
    pub pass: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkAcceptance {
    pub median_vs_zenith_cpu: Option<f64>,
    pub median_vs_fastest_competitor: Option<f64>,
    pub quality_pass_rate_percent: f64,
    pub regression_guard_pass: bool,
    pub target_2x_pass: bool,
    pub target_1_5x_pass: bool,
    pub target_quality_80pct_pass: bool,
    /// Ninguna comparación contra los rivales declarados puede esconder una
    /// regresión científica mayor que los límites del contrato.
    pub competitive_regression_guard_pass: bool,
    pub matrix_complete: bool,
    pub evidence_complete: bool,
    pub publishable_claim: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkSuiteReport {
    pub schema_version: String,
    pub suite_version: String,
    pub generated_at_utc: String,
    pub environment: BenchmarkEnvironment,
    pub validation: BenchmarkManifestValidation,
    pub datasets: Vec<BenchmarkDatasetReport>,
    pub acceptance: BenchmarkAcceptance,
}

const DATASET_MATRIX_JSON: &str = include_str!("../../benchmarks/dataset-matrix.json");

fn parse_dataset_matrix() -> Result<BenchmarkDatasetMatrix, String> {
    let matrix: BenchmarkDatasetMatrix = serde_json::from_str(DATASET_MATRIX_JSON)
        .map_err(|error| format!("dataset-matrix.json inválido: {error}"))?;
    if matrix.schema_version != "zenith-dataset-matrix-v1" {
        return Err(format!(
            "schemaVersion de dataset-matrix.json no soportado: {}",
            matrix.schema_version
        ));
    }
    if matrix.required_scenarios.is_empty() {
        return Err("dataset-matrix.json no contiene escenarios".into());
    }
    let mut ids = std::collections::HashSet::new();
    for scenario in &matrix.required_scenarios {
        if scenario.id.trim().is_empty() || !ids.insert(scenario.id.as_str()) {
            return Err(format!(
                "dataset-matrix.json contiene un id vacío o duplicado: '{}'",
                scenario.id
            ));
        }
        if !matches!(scenario.domain.as_str(), "planetary" | "deep_sky") {
            return Err(format!(
                "{}: domain de la matriz debe ser planetary o deep_sky",
                scenario.id
            ));
        }
        if scenario.required_tags.is_empty() || scenario.acceptance.is_empty() {
            return Err(format!(
                "{}: requiredTags y acceptance no pueden estar vacíos",
                scenario.id
            ));
        }
    }
    Ok(matrix)
}

#[tauri::command]
pub fn get_benchmark_dataset_matrix() -> Result<BenchmarkDatasetMatrix, String> {
    parse_dataset_matrix()
}

fn benchmark_matrix_status(manifest: &BenchmarkManifest) -> (bool, Vec<String>) {
    let ids: std::collections::HashSet<&str> =
        manifest.datasets.iter().map(|d| d.id.as_str()).collect();
    let matrix = match parse_dataset_matrix() {
        Ok(matrix) => matrix,
        Err(error) => return (false, vec![error]),
    };
    let missing: Vec<String> = matrix
        .required_scenarios
        .iter()
        .filter(|scenario| !ids.contains(scenario.id.as_str()))
        .map(|scenario| scenario.id.clone())
        .collect();
    (missing.is_empty(), missing)
}

/// Cada corrida cold/warm representa un caso competitivo. Un caso sólo pasa
/// calidad si Zenith cumple contra TODOS los rivales declarados; así un rival
/// débil no puede inflar artificialmente la tasa del 80%. El cuarto valor
/// conserva el guard absoluto: ninguna comparación individual puede fallar.
fn competitive_quality_summary(cases: &[Vec<bool>]) -> (usize, usize, f64, bool) {
    let total = cases.iter().filter(|case| !case.is_empty()).count();
    let passed = cases
        .iter()
        .filter(|case| !case.is_empty() && case.iter().all(|pass| *pass))
        .count();
    let rate = if total > 0 {
        100.0 * passed as f64 / total as f64
    } else {
        0.0
    };
    let no_regression = total > 0 && cases.iter().flatten().all(|pass| *pass);
    (total, passed, rate, no_regression)
}

fn mean_f32(values: impl Iterator<Item = f32>) -> Option<f32> {
    let values: Vec<f32> = values.filter(|v| v.is_finite()).collect();
    (!values.is_empty()).then(|| values.iter().sum::<f32>() / values.len() as f32)
}

fn summarize_pipeline_telemetry(
    events: &[crate::pipeline::RecordedPipelineTelemetry],
) -> BTreeMap<String, PhaseTelemetrySummary> {
    let mut grouped: BTreeMap<String, Vec<&crate::pipeline::RecordedPipelineTelemetry>> =
        BTreeMap::new();
    for event in events {
        grouped
            .entry(event.event.phase.clone())
            .or_default()
            .push(event);
    }
    grouped
        .into_iter()
        .map(|(phase, values)| {
            let first = values.first().map(|e| e.monotonic_ms).unwrap_or(0);
            let last = values.last().map(|e| e.monotonic_ms).unwrap_or(first);
            let read_min = values
                .iter()
                .map(|e| e.event.io_read_mb)
                .filter(|v| v.is_finite())
                .reduce(f64::min)
                .unwrap_or(0.0);
            let read_max = values
                .iter()
                .map(|e| e.event.io_read_mb)
                .filter(|v| v.is_finite())
                .reduce(f64::max)
                .unwrap_or(read_min);
            let write_min = values
                .iter()
                .map(|e| e.event.io_write_mb)
                .filter(|v| v.is_finite())
                .reduce(f64::min)
                .unwrap_or(0.0);
            let write_max = values
                .iter()
                .map(|e| e.event.io_write_mb)
                .filter(|v| v.is_finite())
                .reduce(f64::max)
                .unwrap_or(write_min);
            let mut effective_engines: Vec<String> = values
                .iter()
                .map(|e| e.event.engine.clone())
                .filter(|v| !v.trim().is_empty())
                .collect();
            effective_engines.sort();
            effective_engines.dedup();
            let mut fallback_reasons: Vec<String> = values
                .iter()
                .filter_map(|e| e.event.fallback_reason.clone())
                .filter(|v| !v.trim().is_empty())
                .collect();
            fallback_reasons.sort();
            fallback_reasons.dedup();
            let summary = PhaseTelemetrySummary {
                samples: values.len(),
                first_ms: first,
                last_ms: last,
                observed_duration_ms: last.saturating_sub(first),
                final_engine: values
                    .last()
                    .map(|e| e.event.engine.clone())
                    .unwrap_or_default(),
                peak_ram_mb: values.iter().map(|e| e.event.ram_mb).max().unwrap_or(0),
                peak_vram_mb: values.iter().map(|e| e.event.vram_mb).max().unwrap_or(0),
                mean_throughput: mean_f32(values.iter().filter_map(|e| e.event.throughput)),
                mean_cpu_percent: mean_f32(values.iter().filter_map(|e| e.event.cpu_percent)),
                mean_gpu_percent: mean_f32(values.iter().filter_map(|e| e.event.gpu_percent)),
                io_read_mb: (read_max - read_min).max(0.0),
                io_write_mb: (write_max - write_min).max(0.0),
                cache_hits: values.iter().map(|e| e.event.cache_hits).max().unwrap_or(0),
                cache_misses: values
                    .iter()
                    .map(|e| e.event.cache_misses)
                    .max()
                    .unwrap_or(0),
                items_done: values.iter().map(|e| e.event.items_done).max().unwrap_or(0),
                items_total: values
                    .iter()
                    .map(|e| e.event.items_total)
                    .max()
                    .unwrap_or(0),
                effective_engines,
                fallback_reasons,
            };
            (phase, summary)
        })
        .collect()
}

fn read_telemetry_export(path: &str) -> Result<PipelineTelemetryExport, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("no se pudo leer telemetría '{path}': {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("telemetría JSON inválida '{path}': {e}"))
}

fn same_environment(a: &BenchmarkEnvironment, b: &BenchmarkEnvironment) -> bool {
    a.app_version == b.app_version
        && a.os == b.os
        && a.arch == b.arch
        && a.cpu_threads == b.cpu_threads
        && a.memory_mb == b.memory_mb
        && a.gpu_available == b.gpu_available
        && a.gpu_name == b.gpu_name
        && a.gpu_backend == b.gpu_backend
        && a.gpu_budget_mb == b.gpu_budget_mb
}

fn parameters_are_reproducible(value: &Option<serde_json::Value>) -> bool {
    value
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .is_some_and(|object| !object.is_empty())
}

fn contracts_match(a: &BenchmarkOutputContract, b: &BenchmarkOutputContract) -> bool {
    a.linear == b.linear
        && a.stretched == b.stretched
        && a.width == b.width
        && a.height == b.height
        && a.channels == b.channels
        && a.sample_format.eq_ignore_ascii_case(&b.sample_format)
        && (a.drizzle_scale - b.drizzle_scale).abs() <= 1e-6
        && a.crop == b.crop
        && a.post_processing == b.post_processing
}

fn validate_output_contract(
    label: &str,
    domain: &str,
    contract: &BenchmarkOutputContract,
    errors: &mut Vec<String>,
) {
    if !contract.linear || contract.stretched {
        errors.push(format!(
            "{label}: la salida debe declararse lineal y sin estirado"
        ));
    }
    if contract.width == 0
        || contract.height == 0
        || !matches!(contract.channels, 1 | 3)
        || contract.crop.width == 0
        || contract.crop.height == 0
    {
        errors.push(format!("{label}: geometría/crop inválidos"));
    }
    if !contract.drizzle_scale.is_finite() || !(1.0..=3.0).contains(&contract.drizzle_scale) {
        errors.push(format!("{label}: drizzleScale debe estar entre 1 y 3"));
    }
    let expected = if domain == "deep_sky" {
        "float32"
    } else {
        "uint16"
    };
    if !contract.sample_format.eq_ignore_ascii_case(expected) {
        errors.push(format!("{label}: sampleFormat debe ser {expected}"));
    }
    if !contract.post_processing.is_empty() {
        errors.push(format!(
            "{label}: postProcessing debe quedar vacío para el benchmark lineal (sin ABE/SCNR/sharpen/estirado)"
        ));
    }
}

fn fits_sample_format(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let header = &bytes[..bytes.len().min(2880 * 4)];
    let mut bitpix = None;
    let mut bzero = None;
    for card in header.chunks_exact(80) {
        let text = std::str::from_utf8(card).ok()?;
        let key = text.get(..8)?.trim();
        let value = text
            .split_once('=')
            .map(|(_, value)| value.split('/').next().unwrap_or(value).trim());
        match key {
            "BITPIX" => bitpix = value.and_then(|value| value.parse::<i32>().ok()),
            "BZERO" => bzero = value.and_then(|value| value.parse::<f64>().ok()),
            "END" => break,
            _ => {}
        }
    }
    match bitpix? {
        -32 => Some("float32".into()),
        -64 => Some("float64".into()),
        16 if bzero.is_some_and(|value| (value - 32768.0).abs() < 0.5) => Some("uint16".into()),
        16 => Some("int16".into()),
        8 => Some("uint8".into()),
        32 => Some("int32".into()),
        _ => None,
    }
}

fn raster_sample_format(path: &Path) -> Option<String> {
    use image::ColorType;
    let image = image::open(path).ok()?;
    match image.color() {
        ColorType::L16 | ColorType::La16 | ColorType::Rgb16 | ColorType::Rgba16 => {
            Some("uint16".into())
        }
        ColorType::L8 | ColorType::La8 | ColorType::Rgb8 | ColorType::Rgba8 => Some("uint8".into()),
        _ => None,
    }
}

fn validate_actual_output_contract(
    label: &str,
    path: &str,
    contract: &BenchmarkOutputContract,
    errors: &mut Vec<String>,
) {
    let image = match crate::ds_read_image(path) {
        Ok(image) => image,
        Err(error) => {
            errors.push(format!(
                "{label}: no se pudo inspeccionar el máster real: {error}"
            ));
            return;
        }
    };
    if (image.w, image.h, image.ch) != (contract.width, contract.height, contract.channels) {
        errors.push(format!(
            "{label}: el archivo real es {}×{}×{}, pero processing declara {}×{}×{}",
            image.w, image.h, image.ch, contract.width, contract.height, contract.channels
        ));
    }
    let source = Path::new(path);
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let detected = if matches!(extension.as_str(), "fits" | "fit" | "fts") {
        fits_sample_format(source)
    } else {
        raster_sample_format(source)
    };
    match detected {
        Some(format) if format.eq_ignore_ascii_case(&contract.sample_format) => {}
        Some(format) => errors.push(format!(
            "{label}: el archivo real usa {format}, pero processing declara {}",
            contract.sample_format
        )),
        None => errors.push(format!(
            "{label}: no se pudo acreditar el sampleFormat real del archivo"
        )),
    }
}

fn path_is_absolute_existing(path: &str) -> bool {
    let path = Path::new(path);
    path.is_absolute() && path.exists()
}

fn validate_required_path(label: &str, path: &str, errors: &mut Vec<String>) -> bool {
    let value = Path::new(path);
    if !value.is_absolute() {
        errors.push(format!("{label}: la ruta debe ser absoluta: {path}"));
        false
    } else if !value.exists() {
        errors.push(format!("{label}: archivo inexistente: {path}"));
        false
    } else {
        true
    }
}

fn validate_telemetry_content(
    label: &str,
    export: &PipelineTelemetryExport,
    domain: &str,
    expected_environment: Option<&BenchmarkEnvironment>,
    errors: &mut Vec<String>,
) -> (
    std::collections::HashSet<String>,
    std::collections::HashSet<String>,
) {
    if export.schema_version != "zenith-pipeline-telemetry-v2" {
        errors.push(format!(
            "{label}: schemaVersion '{}' no es zenith-pipeline-telemetry-v2",
            export.schema_version
        ));
    }
    if let Some(expected) = expected_environment {
        if !same_environment(expected, &export.environment) {
            errors.push(format!(
                "{label}: el hardware no coincide con environment del manifest"
            ));
        }
    }
    if chrono::DateTime::parse_from_rfc3339(&export.environment.generated_at_utc).is_err() {
        errors.push(format!("{label}: environment.generatedAtUtc no es RFC3339"));
    }
    if export.events.is_empty() {
        errors.push(format!("{label}: no contiene eventos"));
        return (Default::default(), Default::default());
    }
    if export
        .events
        .windows(2)
        .any(|pair| pair[1].monotonic_ms < pair[0].monotonic_ms)
    {
        errors.push(format!("{label}: monotonicMs retrocede"));
    }
    let expected_domain = if domain == "deep_sky" {
        crate::pipeline::PipelineDomain::DeepSky
    } else {
        crate::pipeline::PipelineDomain::Planetary
    };
    let mut jobs = std::collections::HashSet::new();
    let mut completed_jobs = std::collections::HashSet::new();
    let mut last_events = std::collections::HashMap::<String, (&str, f32)>::new();
    let mut phase_names = std::collections::HashSet::new();
    for event in &export.events {
        let telemetry = &event.event;
        jobs.insert(telemetry.job_id.clone());
        last_events.insert(
            telemetry.job_id.clone(),
            (telemetry.phase.as_str(), telemetry.progress),
        );
        phase_names.insert(telemetry.phase.clone());
        if telemetry.domain != expected_domain {
            errors.push(format!("{label}: mezcla eventos de otro dominio"));
        }
        if telemetry.job_id.trim().is_empty()
            || telemetry.phase.trim().is_empty()
            || telemetry.engine.trim().is_empty()
        {
            errors.push(format!("{label}: jobId/fase/motor vacío"));
        }
        if !telemetry.progress.is_finite() || !(0.0..=100.001).contains(&telemetry.progress) {
            errors.push(format!("{label}: progreso inválido"));
        }
        if !telemetry.io_read_mb.is_finite() || !telemetry.io_write_mb.is_finite() {
            errors.push(format!("{label}: E/S no finita"));
        }
        if chrono::DateTime::parse_from_rfc3339(&event.captured_at_utc).is_err() {
            errors.push(format!("{label}: capturedAtUtc no es RFC3339"));
        }
        if telemetry.phase == "complete" && telemetry.progress >= 99.999 {
            completed_jobs.insert(telemetry.job_id.clone());
        }
    }
    if let Some(job_id) = export.job_id.as_deref() {
        if job_id.trim().is_empty() || jobs.iter().any(|job| job != job_id) {
            errors.push(format!(
                "{label}: jobId de cabecera no coincide con sus eventos"
            ));
        }
    }
    for job in &jobs {
        if !completed_jobs.contains(job) {
            errors.push(format!(
                "{label}: job '{job}' no tiene evento terminal complete/100%"
            ));
        } else if !last_events
            .get(job)
            .is_some_and(|(phase, progress)| *phase == "complete" && *progress >= 99.999)
        {
            errors.push(format!(
                "{label}: job '{job}' contiene trabajo después de complete; el último evento debe ser complete/100%"
            ));
        }
    }
    let recomputed = summarize_pipeline_telemetry(&export.events);
    if recomputed.keys().ne(export.phases.keys())
        || recomputed.iter().any(|(phase, summary)| {
            export
                .phases
                .get(phase)
                .is_none_or(|saved| saved.samples != summary.samples)
        })
    {
        errors.push(format!(
            "{label}: el resumen por fases no corresponde a los eventos"
        ));
    }
    (phase_names, jobs)
}

fn validate_deepsky_recipe(
    label: &str,
    path: &str,
    run: &ZenithBenchmarkRun,
    telemetry_jobs: &std::collections::HashSet<String>,
    errors: &mut Vec<String>,
) {
    if !validate_required_path(label, path, errors) {
        return;
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            errors.push(format!("{label}: no se pudo leer: {error}"));
            return;
        }
    };
    let recipe: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(recipe) => recipe,
        Err(error) => {
            errors.push(format!("{label}: JSON inválido: {error}"));
            return;
        }
    };
    if recipe
        .get("schemaVersion")
        .and_then(|v| v.as_u64())
        .is_none_or(|v| v < 2)
        || recipe.get("linearFloat32").and_then(|v| v.as_bool()) != Some(true)
    {
        errors.push(format!(
            "{label}: no acredita un máster lineal float32 de receta v2"
        ));
    }
    let parameters = recipe.pointer("/recipe/parameters");
    if parameters
        .and_then(|v| v.as_object())
        .is_none_or(|v| v.is_empty())
    {
        errors.push(format!("{label}: faltan recipe.parameters reproducibles"));
    }
    if parameters
        .and_then(|v| v.get("optionalAbeScnr"))
        .and_then(|v| v.as_bool())
        != Some(false)
    {
        errors.push(format!(
            "{label}: ABE/SCNR debe estar desactivado en el máster de benchmark"
        ));
    }
    if let (Some(saved), Some(declared)) = (parameters, run.parameters.as_ref()) {
        if saved != declared {
            errors.push(format!(
                "{label}: parameters no coincide con la receta exportada"
            ));
        }
    }
    if recipe
        .pointer("/recipe/sourceFingerprint")
        .and_then(|v| v.as_str())
        .is_none_or(str::is_empty)
    {
        errors.push(format!("{label}: falta sourceFingerprint"));
    }
    if let Some(contract) = run.processing.as_ref() {
        let geometry = (
            recipe.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            recipe.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            recipe.get("channels").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
        );
        if geometry != (contract.width, contract.height, contract.channels) {
            errors.push(format!(
                "{label}: geometría de receta y processing no coincide"
            ));
        }
    }
    match recipe.get("resultId").and_then(|v| v.as_str()) {
        Some(id) if telemetry_jobs.contains(id) => {}
        _ => errors.push(format!(
            "{label}: resultId no está acreditado por la telemetría"
        )),
    }
}

fn validate_required_pipeline_phases(
    label: &str,
    domain: &str,
    phases: &std::collections::HashSet<String>,
    errors: &mut Vec<String>,
) {
    let required_exact: &[&str] = if domain == "deep_sky" {
        &[
            "calibrate_detect",
            "register",
            "normalize",
            "export",
            "complete",
        ]
    } else {
        &["analysis", "stacking", "complete"]
    };
    for phase in required_exact {
        if !phases.contains(*phase) {
            errors.push(format!("{label}: falta fase de telemetría '{phase}'"));
        }
    }
    if domain == "deep_sky"
        && !phases
            .iter()
            .any(|phase| phase.starts_with("integrate") || phase.starts_with("sigma_clip"))
    {
        errors.push(format!("{label}: falta fase de integración en telemetría"));
    }
}

fn validate_benchmark_manifest_data(manifest: &BenchmarkManifest) -> BenchmarkManifestValidation {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut comparator_count = 0usize;
    if manifest.suite_version.trim().is_empty() {
        errors.push("suiteVersion está vacío".into());
    }
    if manifest.datasets.is_empty() {
        errors.push("El manifest no contiene datasets".into());
    }
    if let Some(environment) = manifest.environment.as_ref() {
        if chrono::DateTime::parse_from_rfc3339(&environment.generated_at_utc).is_err() {
            errors.push("environment.generatedAtUtc no es RFC3339".into());
        }
    } else {
        errors.push("Falta environment obtenido con get_benchmark_environment".into());
    }
    let matrix = match parse_dataset_matrix() {
        Ok(matrix) => Some(matrix),
        Err(error) => {
            errors.push(error);
            None
        }
    };
    let scenario_domains: std::collections::HashMap<&str, &str> = matrix
        .as_ref()
        .map(|matrix| {
            matrix
                .required_scenarios
                .iter()
                .map(|scenario| (scenario.id.as_str(), scenario.domain.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let mut ids = std::collections::HashSet::new();
    for ds in &manifest.datasets {
        if ds.id.trim().is_empty() || !ids.insert(ds.id.clone()) {
            errors.push(format!("Dataset con id vacío o duplicado: '{}'", ds.id));
        }
        if !matches!(ds.domain.as_str(), "planetary" | "deep_sky") {
            errors.push(format!("{}: domain debe ser planetary o deep_sky", ds.id));
        }
        if let Some(expected) = scenario_domains.get(ds.id.as_str()) {
            if ds.domain != *expected {
                errors.push(format!(
                    "{}: domain '{}' contradice la matriz ('{}')",
                    ds.id, ds.domain, expected
                ));
            }
        }
        if ds.sources.is_empty() {
            errors.push(format!("{}: no contiene fuentes", ds.id));
        }
        for source in &ds.sources {
            validate_required_path(&format!("{} / fuente", ds.id), source, &mut errors);
        }
        if ds.id == "deep-sky-satellite-trails" {
            match ds.artifact_free_reference.as_deref() {
                Some(path) => {
                    validate_required_path(
                        &format!("{} / referencia limpia de rechazo", ds.id),
                        path,
                        &mut errors,
                    );
                }
                None => errors.push(format!(
                    "{}: falta artifactFreeReference lineal sin traza",
                    ds.id
                )),
            }
            match ds.artifact_mask.as_deref() {
                Some(path) => {
                    validate_required_path(
                        &format!("{} / máscara de traza", ds.id),
                        path,
                        &mut errors,
                    );
                }
                None => errors.push(format!("{}: falta artifactMask registrada", ds.id)),
            }
        }
        if let Some(output) = ds.zenith_output.as_deref() {
            if !path_is_absolute_existing(output) {
                warnings.push(format!(
                    "{}: zenithOutput opcional no existe o no es absoluto",
                    ds.id
                ));
            }
        }

        if !ds
            .baseline_cpu_seconds
            .is_some_and(|v| v.is_finite() && v > 0.0)
        {
            errors.push(format!("{}: falta baselineCpuSeconds válido", ds.id));
        }
        match ds.baseline_cpu_output.as_deref() {
            Some(path) => {
                validate_required_path(&format!("{} / baseline CPU", ds.id), path, &mut errors);
            }
            None => errors.push(format!("{}: falta baselineCpuOutput", ds.id)),
        }
        match ds.baseline_cpu_processing.as_ref() {
            Some(contract) => {
                let label = format!("{} / baseline CPU", ds.id);
                validate_output_contract(&label, &ds.domain, contract, &mut errors);
                if let Some(path) = ds
                    .baseline_cpu_output
                    .as_deref()
                    .filter(|path| path_is_absolute_existing(path))
                {
                    validate_actual_output_contract(&label, path, contract, &mut errors);
                }
            }
            None => errors.push(format!("{}: falta baselineCpuProcessing", ds.id)),
        }
        if !parameters_are_reproducible(&ds.baseline_cpu_parameters) {
            errors.push(format!("{}: falta baselineCpuParameters no vacío", ds.id));
        }
        match ds.baseline_cpu_log.as_deref() {
            Some(path) => {
                validate_required_path(&format!("{} / log baseline CPU", ds.id), path, &mut errors);
            }
            None => errors.push(format!("{}: falta baselineCpuLog", ds.id)),
        }

        let mut run_keys = std::collections::HashSet::new();
        for run in &ds.zenith_runs {
            let label = format!(
                "{} / Zenith {} {}",
                ds.id,
                run.mode,
                if run.cold_cache { "cold" } else { "warm" }
            );
            if run.mode.trim().is_empty() || !run_keys.insert((run.mode.clone(), run.cold_cache)) {
                errors.push(format!("{label}: modo vacío o corrida duplicada"));
            }
            if !run.elapsed_seconds.is_finite() || run.elapsed_seconds <= 0.0 {
                errors.push(format!("{label}: tiempo inválido"));
            }
            validate_required_path(&format!("{label} / salida"), &run.output, &mut errors);
            match run.processing.as_ref() {
                Some(contract) => {
                    validate_output_contract(&label, &ds.domain, contract, &mut errors);
                    if path_is_absolute_existing(&run.output) {
                        validate_actual_output_contract(&label, &run.output, contract, &mut errors);
                    }
                    if ds
                        .baseline_cpu_processing
                        .as_ref()
                        .is_some_and(|baseline| !contracts_match(baseline, contract))
                    {
                        errors.push(format!(
                            "{label}: escala/geometría/crop/drizzle difiere del baseline"
                        ));
                    }
                }
                None => errors.push(format!("{label}: falta processing")),
            }
            if !parameters_are_reproducible(&run.parameters) {
                errors.push(format!("{label}: falta parameters no vacío"));
            }
            if run
                .cache_preparation
                .as_deref()
                .is_none_or(|v| v.trim().is_empty())
            {
                errors.push(format!("{label}: falta cachePreparation verificable"));
            }
            if run.telemetry.is_empty() {
                errors.push(format!("{label}: falta telemetría"));
            }
            let mut phases = std::collections::HashSet::new();
            let mut jobs = std::collections::HashSet::new();
            let mut min_ms = u128::MAX;
            let mut max_ms = 0u128;
            for telemetry_path in &run.telemetry {
                let telemetry_label = format!("{label} / telemetría");
                if !validate_required_path(&telemetry_label, telemetry_path, &mut errors) {
                    continue;
                }
                match read_telemetry_export(telemetry_path) {
                    Ok(export) => {
                        let (these_phases, these_jobs) = validate_telemetry_content(
                            &telemetry_label,
                            &export,
                            &ds.domain,
                            manifest.environment.as_ref(),
                            &mut errors,
                        );
                        phases.extend(these_phases);
                        jobs.extend(these_jobs);
                        if let Some(first) = export.events.first() {
                            min_ms = min_ms.min(first.monotonic_ms);
                        }
                        if let Some(last) = export.events.last() {
                            max_ms = max_ms.max(last.monotonic_ms);
                        }
                    }
                    Err(error) => errors.push(format!("{label}: {error}")),
                }
            }
            validate_required_pipeline_phases(&label, &ds.domain, &phases, &mut errors);
            if min_ms != u128::MAX {
                let observed_seconds = max_ms.saturating_sub(min_ms) as f64 / 1000.0;
                if observed_seconds > run.elapsed_seconds * 1.25 + 2.0 {
                    errors.push(format!("{label}: telemetría dura más que elapsedSeconds"));
                }
            }
            if ds.domain == "deep_sky" {
                match run.recipe.as_deref() {
                    Some(path) => validate_deepsky_recipe(
                        &format!("{label} / receta"),
                        path,
                        run,
                        &jobs,
                        &mut errors,
                    ),
                    None => errors.push(format!("{label}: falta recipe JSON")),
                }
            }
        }
        if !ds.zenith_runs.iter().any(|run| run.cold_cache) {
            errors.push(format!(
                "{}: falta una corrida Zenith con caché fría",
                ds.id
            ));
        }
        if !ds.zenith_runs.iter().any(|run| !run.cold_cache) {
            errors.push(format!(
                "{}: falta una corrida Zenith con caché caliente",
                ds.id
            ));
        }

        for comparator in &ds.comparators {
            comparator_count += 1;
            let label = format!("{} / {}", ds.id, comparator.engine);
            if comparator.engine.trim().is_empty() || comparator.version.trim().is_empty() {
                errors.push(format!("{label}: engine/version deben estar indicados"));
            }
            if !comparator.elapsed_seconds.is_finite() || comparator.elapsed_seconds <= 0.0 {
                errors.push(format!("{label}: tiempo inválido"));
            }
            validate_required_path(
                &format!("{label} / salida"),
                &comparator.output,
                &mut errors,
            );
            match comparator.log.as_deref() {
                Some(path) => {
                    validate_required_path(&format!("{label} / log"), path, &mut errors);
                }
                None => errors.push(format!("{label}: falta log")),
            }
            match comparator.processing.as_ref() {
                Some(contract) => {
                    validate_output_contract(&label, &ds.domain, contract, &mut errors);
                    if path_is_absolute_existing(&comparator.output) {
                        validate_actual_output_contract(
                            &label,
                            &comparator.output,
                            contract,
                            &mut errors,
                        );
                    }
                    if ds
                        .baseline_cpu_processing
                        .as_ref()
                        .is_some_and(|baseline| !contracts_match(baseline, contract))
                    {
                        errors.push(format!(
                            "{label}: escala/geometría/crop/drizzle difiere del baseline"
                        ));
                    }
                }
                None => errors.push(format!("{label}: falta processing")),
            }
            if !parameters_are_reproducible(&comparator.parameters) {
                errors.push(format!("{label}: falta parameters no vacío"));
            }
        }
        if ds.comparators.is_empty() {
            errors.push(format!("{}: falta al menos un rival comparable", ds.id));
        }
    }
    let (_, missing) = benchmark_matrix_status(manifest);
    if !missing.is_empty() {
        errors.push(format!(
            "Matriz incompleta; faltan escenarios: {}",
            missing.join(", ")
        ));
    }
    BenchmarkManifestValidation {
        valid: errors.is_empty(),
        dataset_count: manifest.datasets.len(),
        comparator_count,
        errors,
        warnings,
    }
}

#[tauri::command]
pub fn get_benchmark_environment() -> BenchmarkEnvironment {
    use sysinfo::System;
    let mut sys = System::new_all();
    sys.refresh_memory();
    let gpu = crate::gpu_stack::gpu_info();
    BenchmarkEnvironment {
        app_version: env!("CARGO_PKG_VERSION").into(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        cpu_threads: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        memory_mb: sys.total_memory() / (1024 * 1024),
        gpu_available: gpu.available,
        gpu_name: gpu.name,
        gpu_backend: gpu.backend,
        gpu_budget_mb: gpu.vram_budget_mb,
        generated_at_utc: chrono::Utc::now().to_rfc3339(),
    }
}

fn active_benchmark_run() -> &'static Mutex<Option<ActiveBenchmarkRun>> {
    ACTIVE_BENCHMARK_RUN.get_or_init(|| Mutex::new(None))
}

fn benchmark_path_component(value: &str) -> String {
    let mut output = String::with_capacity(value.len().min(80));
    let mut previous_separator = false;
    for character in value.trim().chars().take(80) {
        let allowed = character.is_ascii_alphanumeric() || matches!(character, '-' | '_');
        if allowed {
            output.push(character);
            previous_separator = false;
        } else if !previous_separator && !output.is_empty() {
            output.push('-');
            previous_separator = true;
        }
    }
    output.trim_matches('-').to_string()
}

fn absolute_existing_file(path: &str, label: &str) -> Result<String, String> {
    let value = Path::new(path);
    if !value.is_absolute() {
        return Err(format!("{label}: la ruta debe ser absoluta: {path}"));
    }
    if !value.is_file() {
        return Err(format!("{label}: el archivo no existe: {path}"));
    }
    value
        .canonicalize()
        .map(|path| path.display().to_string())
        .map_err(|error| format!("{label}: no se pudo normalizar la ruta: {error}"))
}

fn write_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("Ruta de evidencia sin carpeta: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("Crear carpeta de evidencia '{}': {error}", parent.display()))?;
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("Serializar evidencia '{}': {error}", path.display()))?;
    let sequence = BENCHMARK_RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("benchmark.json");
    let temporary = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
    if let Err(error) = std::fs::write(&temporary, bytes) {
        return Err(format!(
            "Escribir evidencia temporal '{}': {error}",
            temporary.display()
        ));
    }
    let backup = parent.join(format!(
        ".{name}.{}.{}.previous",
        std::process::id(),
        sequence
    ));
    let had_previous = path.exists();
    if had_previous {
        if let Err(error) = std::fs::rename(path, &backup) {
            let _ = std::fs::remove_file(&temporary);
            return Err(format!(
                "Preservar evidencia previa '{}': {error}",
                path.display()
            ));
        }
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        let rollback = if had_previous {
            std::fs::rename(&backup, path).err()
        } else {
            None
        };
        let rollback = rollback
            .map(|error| format!("; además falló restaurar la versión previa: {error}"))
            .unwrap_or_default();
        return Err(format!(
            "Publicar evidencia '{}': {error}{rollback}",
            path.display()
        ));
    }
    if had_previous {
        let _ = std::fs::remove_file(backup);
    }
    Ok(())
}

#[tauri::command]
pub fn begin_benchmark_run(
    request: BeginBenchmarkRunRequest,
) -> Result<BenchmarkRunSessionHandle, String> {
    if request.dataset_id.trim().is_empty() {
        return Err("datasetId no puede estar vacío".into());
    }
    if !matches!(request.domain.as_str(), "planetary" | "deep_sky") {
        return Err("domain debe ser 'planetary' o 'deep_sky'".into());
    }
    if request.mode.trim().is_empty() {
        return Err("mode no puede estar vacío".into());
    }
    if request.cache_preparation.trim().is_empty() {
        return Err(
            "cachePreparation debe describir la preparación realizada antes de medir".into(),
        );
    }
    let dataset_component = benchmark_path_component(&request.dataset_id);
    if dataset_component.is_empty() {
        return Err("datasetId no produce un nombre de carpeta válido".into());
    }
    let output_root = Path::new(&request.output_directory);
    if !output_root.is_absolute() {
        return Err("outputDirectory debe ser una ruta absoluta".into());
    }

    let mut active = active_benchmark_run()
        .lock()
        .map_err(|_| "El estado de benchmark quedó bloqueado".to_string())?;
    if let Some(current) = active.as_ref() {
        return Err(format!(
            "Ya existe una corrida activa: {} ({})",
            current.handle.session_id, current.handle.dataset_id
        ));
    }

    std::fs::create_dir_all(output_root)
        .map_err(|error| format!("Crear outputDirectory: {error}"))?;
    let output_root = output_root
        .canonicalize()
        .map_err(|error| format!("Normalizar outputDirectory: {error}"))?;
    let sequence = BENCHMARK_RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let now = chrono::Utc::now();
    let session_id = format!(
        "{}-{}-{}-{}",
        dataset_component,
        now.format("%Y%m%dT%H%M%S%.3fZ"),
        std::process::id(),
        sequence
    );
    let directory: PathBuf = output_root.join(&dataset_component).join(&session_id);
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("Crear carpeta de corrida: {error}"))?;
    let environment = get_benchmark_environment();
    let environment_path = directory.join("environment.json");
    write_json_atomic(&environment_path, &environment)?;

    // Desde aquí empieza el wall time. No incluye preparar la caché ni escribir
    // los metadatos de la sesión, pero sí toda apertura/lectura/proceso/export.
    crate::pipeline::clear_telemetry(None);
    let started_at_utc = chrono::Utc::now().to_rfc3339();
    let started = Instant::now();
    let handle = BenchmarkRunSessionHandle {
        session_id,
        dataset_id: request.dataset_id.trim().to_string(),
        domain: request.domain,
        mode: request.mode.trim().to_string(),
        cold_cache: request.cold_cache,
        cache_preparation: request.cache_preparation.trim().to_string(),
        started_at_utc,
        directory: directory.display().to_string(),
        environment_path: environment_path.display().to_string(),
    };
    *active = Some(ActiveBenchmarkRun {
        handle: handle.clone(),
        environment,
        started,
    });
    Ok(handle)
}

#[tauri::command]
pub fn get_active_benchmark_run() -> Result<Option<BenchmarkRunSessionHandle>, String> {
    active_benchmark_run()
        .lock()
        .map(|active| active.as_ref().map(|run| run.handle.clone()))
        .map_err(|_| "El estado de benchmark quedó bloqueado".to_string())
}

#[tauri::command]
pub fn finish_benchmark_run(
    request: FinishBenchmarkRunRequest,
) -> Result<BenchmarkRunArtifact, String> {
    let finished = Instant::now();
    let finished_at_utc = chrono::Utc::now().to_rfc3339();
    let mut active_guard = active_benchmark_run()
        .lock()
        .map_err(|_| "El estado de benchmark quedó bloqueado".to_string())?;
    let active = active_guard
        .as_ref()
        .filter(|run| run.handle.session_id == request.session_id)
        .cloned()
        .ok_or_else(|| {
            format!(
                "No existe la sesión activa solicitada: {}",
                request.session_id
            )
        })?;

    let output = absolute_existing_file(&request.output, "Salida Zenith")?;
    let mut requested_jobs = Vec::new();
    let mut requested_job_set = std::collections::HashSet::new();
    for job_id in request.job_ids {
        let job_id = job_id.trim().to_string();
        if job_id.is_empty() || !requested_job_set.insert(job_id.clone()) {
            return Err("jobIds debe contener identificadores únicos y no vacíos".into());
        }
        requested_jobs.push(job_id);
    }
    if requested_jobs.is_empty() {
        return Err("jobIds es obligatorio para aislar la telemetría de esta corrida".into());
    }

    let events: Vec<_> = crate::pipeline::telemetry_snapshot(None)
        .into_iter()
        .filter(|event| requested_job_set.contains(&event.event.job_id))
        .collect();
    let phases = summarize_pipeline_telemetry(&events);
    let telemetry = PipelineTelemetryExport {
        schema_version: "zenith-pipeline-telemetry-v2".into(),
        environment: active.environment.clone(),
        job_id: (requested_jobs.len() == 1).then(|| requested_jobs[0].clone()),
        phases: phases.clone(),
        events,
    };

    let mut errors = Vec::new();
    validate_output_contract(
        "Corrida Zenith",
        &active.handle.domain,
        &request.processing,
        &mut errors,
    );
    validate_actual_output_contract("Corrida Zenith", &output, &request.processing, &mut errors);
    let parameters = Some(request.parameters);
    if !parameters_are_reproducible(&parameters) {
        errors.push("Corrida Zenith: parameters debe ser un objeto JSON no vacío".into());
    }
    let (phase_names, observed_jobs) = validate_telemetry_content(
        "Corrida Zenith / telemetría",
        &telemetry,
        &active.handle.domain,
        Some(&active.environment),
        &mut errors,
    );
    if observed_jobs != requested_job_set {
        let mut missing: Vec<_> = requested_job_set
            .difference(&observed_jobs)
            .cloned()
            .collect();
        missing.sort();
        errors.push(format!(
            "Corrida Zenith: falta telemetría para jobIds: {}",
            missing.join(", ")
        ));
    }
    validate_required_pipeline_phases(
        "Corrida Zenith",
        &active.handle.domain,
        &phase_names,
        &mut errors,
    );

    let recipe = match request.recipe.as_deref() {
        Some(path) => Some(absolute_existing_file(path, "Receta Zenith")?),
        None => None,
    };
    let directory = Path::new(&active.handle.directory);
    let telemetry_path = directory.join("telemetry.json");
    let record_path = directory.join("zenith-run.json");
    let elapsed_seconds = finished.duration_since(active.started).as_secs_f64();
    let zenith_run = ZenithBenchmarkRun {
        mode: active.handle.mode.clone(),
        cold_cache: active.handle.cold_cache,
        elapsed_seconds,
        output,
        telemetry: vec![telemetry_path.display().to_string()],
        cache_preparation: Some(active.handle.cache_preparation.clone()),
        processing: Some(request.processing),
        parameters,
        recipe,
    };
    if active.handle.domain == "deep_sky" {
        match zenith_run.recipe.as_deref() {
            Some(path) => validate_deepsky_recipe(
                "Corrida Zenith / receta",
                path,
                &zenith_run,
                &observed_jobs,
                &mut errors,
            ),
            None => errors.push("Corrida Zenith: cielo profundo requiere recipe JSON".into()),
        }
    }
    if !errors.is_empty() {
        return Err(format!(
            "La corrida no puede cerrarse como evidencia válida:\n- {}",
            errors.join("\n- ")
        ));
    }

    let artifact = BenchmarkRunArtifact {
        schema_version: "zenith-benchmark-run-v1".into(),
        session: active.handle.clone(),
        environment: active.environment,
        zenith_run,
        telemetry_path: telemetry_path.display().to_string(),
        record_path: record_path.display().to_string(),
        finished_at_utc,
        job_ids: requested_jobs.clone(),
        phases,
    };
    write_json_atomic(&telemetry_path, &telemetry)?;
    write_json_atomic(&record_path, &artifact)?;
    *active_guard = None;
    drop(active_guard);
    for job_id in requested_jobs {
        crate::pipeline::clear_telemetry(Some(&job_id));
    }
    Ok(artifact)
}

#[tauri::command]
pub fn abort_benchmark_run(
    request: AbortBenchmarkRunRequest,
) -> Result<AbortedBenchmarkRun, String> {
    let mut active_guard = active_benchmark_run()
        .lock()
        .map_err(|_| "El estado de benchmark quedó bloqueado".to_string())?;
    let active = active_guard
        .as_ref()
        .filter(|run| run.handle.session_id == request.session_id)
        .cloned()
        .ok_or_else(|| {
            format!(
                "No existe la sesión activa solicitada: {}",
                request.session_id
            )
        })?;
    *active_guard = None;
    drop(active_guard);
    crate::pipeline::clear_telemetry(None);

    let record_path = Path::new(&active.handle.directory).join("run-aborted.json");
    let aborted = AbortedBenchmarkRun {
        schema_version: "zenith-benchmark-run-aborted-v1".into(),
        session: active.handle,
        aborted_at_utc: chrono::Utc::now().to_rfc3339(),
        elapsed_seconds: active.started.elapsed().as_secs_f64(),
        reason: request
            .reason
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or_else(|| "Cancelada explícitamente".into()),
        record_path: record_path.display().to_string(),
    };
    write_json_atomic(&record_path, &aborted)?;
    Ok(aborted)
}

#[tauri::command]
pub fn clear_pipeline_telemetry(job_id: Option<String>) -> usize {
    crate::pipeline::clear_telemetry(job_id.as_deref())
}

#[tauri::command]
pub fn export_pipeline_telemetry(path: String, job_id: Option<String>) -> Result<String, String> {
    let events = crate::pipeline::telemetry_snapshot(job_id.as_deref());
    if events.is_empty() {
        return Err("No hay telemetría para el job solicitado".into());
    }
    let phases = summarize_pipeline_telemetry(&events);
    let export = PipelineTelemetryExport {
        schema_version: "zenith-pipeline-telemetry-v2".into(),
        environment: get_benchmark_environment(),
        job_id,
        phases,
        events,
    };
    let out = Path::new(&path);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Crear carpeta de telemetría: {e}"))?;
    }
    let bytes =
        serde_json::to_vec_pretty(&export).map_err(|e| format!("Serializar telemetría: {e}"))?;
    std::fs::write(out, bytes).map_err(|e| format!("Escribir telemetría: {e}"))?;
    Ok(out.display().to_string())
}

#[tauri::command]
pub fn validate_benchmark_manifest(path: String) -> Result<BenchmarkManifestValidation, String> {
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("Manifest: {e}"))?;
    let manifest: BenchmarkManifest =
        serde_json::from_str(&raw).map_err(|e| format!("Manifest JSON inválido: {e}"))?;
    Ok(validate_benchmark_manifest_data(&manifest))
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    values.retain(|v| v.is_finite());
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 0 {
        (values[mid - 1] + values[mid]) * 0.5
    } else {
        values[mid]
    })
}

fn measure_artifact_residual(
    clean_reference_path: &str,
    candidate_path: &str,
    mask_path: &str,
) -> Result<ArtifactResidualMeasurement, String> {
    let clean = crate::ds_read_image(clean_reference_path)?;
    let candidate = crate::ds_read_image(candidate_path)?;
    let mask = crate::ds_read_image(mask_path)?;
    let incompatible = |reason: String| ArtifactResidualMeasurement {
        compatible: false,
        masked_pixels: 0,
        unmasked_pixels: 0,
        masked_rmse_adu: 0.0,
        unmasked_rmse_adu: 0.0,
        fitted_scale: 1.0,
        fitted_offset: 0.0,
        reason: Some(reason),
    };
    if (clean.w, clean.h) != (candidate.w, candidate.h) || (clean.w, clean.h) != (mask.w, mask.h) {
        return Ok(incompatible(
            "Geometría incompatible entre truth/candidato/máscara".into(),
        ));
    }
    let luma = |image: &crate::DsImage| -> Vec<f64> {
        if image.ch == 1 {
            image
                .data
                .iter()
                .take(image.w * image.h)
                .map(|&v| v as f64)
                .collect()
        } else {
            image
                .data
                .chunks_exact(image.ch)
                .take(image.w * image.h)
                .map(|pixel| {
                    0.2126 * pixel[0] as f64 + 0.7152 * pixel[1] as f64 + 0.0722 * pixel[2] as f64
                })
                .collect()
        }
    };
    let truth = luma(&clean);
    let values = luma(&candidate);
    let mask_values = luma(&mask);
    let mut count = 0.0f64;
    let (mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for ((&x, &y), &masked) in values.iter().zip(&truth).zip(&mask_values) {
        if masked > 0.5 || !x.is_finite() || !y.is_finite() {
            continue;
        }
        count += 1.0;
        sx += x;
        sy += y;
        sxx += x * x;
        sxy += x * y;
    }
    if count < 32.0 {
        return Ok(incompatible(
            "La máscara no deja suficientes píxeles de fondo para normalizar".into(),
        ));
    }
    let denominator = count * sxx - sx * sx;
    let scale = if denominator.abs() > 1e-12 {
        (count * sxy - sx * sy) / denominator
    } else {
        1.0
    };
    let offset = (sy - scale * sx) / count;
    let (mut masked_se, mut unmasked_se) = (0.0f64, 0.0f64);
    let (mut masked_count, mut unmasked_count) = (0usize, 0usize);
    for ((&x, &y), &masked) in values.iter().zip(&truth).zip(&mask_values) {
        if !x.is_finite() || !y.is_finite() {
            continue;
        }
        let error = scale * x + offset - y;
        if masked > 0.5 {
            masked_se += error * error;
            masked_count += 1;
        } else {
            unmasked_se += error * error;
            unmasked_count += 1;
        }
    }
    if masked_count < 16 {
        return Ok(incompatible(
            "artifactMask contiene menos de 16 píxeles útiles".into(),
        ));
    }
    Ok(ArtifactResidualMeasurement {
        compatible: true,
        masked_pixels: masked_count,
        unmasked_pixels: unmasked_count,
        masked_rmse_adu: (masked_se / masked_count as f64).sqrt(),
        unmasked_rmse_adu: (unmasked_se / unmasked_count.max(1) as f64).sqrt(),
        fitted_scale: scale,
        fitted_offset: offset,
        reason: None,
    })
}

fn compare_artifact_rejection(
    reference: ArtifactResidualMeasurement,
    candidate: ArtifactResidualMeasurement,
) -> ArtifactRejectionComparison {
    let denominator = reference.masked_rmse_adu.max(0.01);
    let regression_percent = 100.0 * (candidate.masked_rmse_adu / denominator - 1.0);
    let pass = reference.compatible
        && candidate.compatible
        && candidate.masked_rmse_adu <= reference.masked_rmse_adu * 1.03 + 0.05;
    ArtifactRejectionComparison {
        reference,
        candidate,
        regression_percent,
        pass,
    }
}

fn benchmark_quality_pass(metrics: &LinearImageComparison) -> bool {
    metrics.compatible
        && metrics.fwhm_regression_percent <= 3.0
        && metrics.noise_regression_percent <= 3.0
        && metrics.flux_error_percent <= 3.0
        && metrics.registration_error_px <= 0.5
        && metrics.correlation >= 0.8
}

fn build_run_evidence(run: &ZenithBenchmarkRun) -> BenchmarkRunEvidence {
    let mut events = Vec::new();
    let mut job_ids = std::collections::BTreeSet::new();
    for path in &run.telemetry {
        if let Ok(export) = read_telemetry_export(path) {
            for event in export.events {
                job_ids.insert(event.event.job_id.clone());
                events.push(event);
            }
        }
    }
    events.sort_by_key(|event| event.monotonic_ms);
    BenchmarkRunEvidence {
        zenith_mode: run.mode.clone(),
        cold_cache: run.cold_cache,
        cache_preparation: run.cache_preparation.clone().unwrap_or_default(),
        elapsed_seconds: run.elapsed_seconds,
        telemetry_files: run.telemetry.clone(),
        job_ids: job_ids.into_iter().collect(),
        phases: summarize_pipeline_telemetry(&events),
    }
}

#[tauri::command]
pub fn generate_benchmark_report(
    manifest_path: String,
    output_path: String,
) -> Result<BenchmarkSuiteReport, String> {
    let raw = std::fs::read_to_string(&manifest_path).map_err(|e| format!("Manifest: {e}"))?;
    let manifest: BenchmarkManifest =
        serde_json::from_str(&raw).map_err(|e| format!("Manifest JSON inválido: {e}"))?;
    let validation = validate_benchmark_manifest_data(&manifest);
    let mut reports = Vec::new();
    let mut all_baseline = Vec::new();
    let mut fastest_ratios = Vec::new();
    let mut competitive_quality_cases: Vec<Vec<bool>> = Vec::new();
    let mut regression_guard = true;
    let (matrix_complete, missing) = benchmark_matrix_status(&manifest);
    let evidence_complete = matrix_complete && validation.valid;

    for ds in &manifest.datasets {
        let runs = ds.zenith_runs.iter().map(build_run_evidence).collect();
        let artifact_contract = ds
            .artifact_free_reference
            .as_deref()
            .zip(ds.artifact_mask.as_deref());
        let mut comparisons = Vec::new();
        let mut baseline_quality = Vec::new();
        let mut baseline_speedups = Vec::new();
        let mut warnings = Vec::new();
        for run in &ds.zenith_runs {
            if let Some(base) = ds.baseline_cpu_seconds.filter(|v| *v > 0.0) {
                let speedup = base / run.elapsed_seconds;
                baseline_speedups.push(speedup);
                all_baseline.push(speedup);
                if speedup < 1.0 / 1.10 {
                    regression_guard = false;
                }
            }
            if let Some(cpu_output) = ds.baseline_cpu_output.as_ref() {
                let metrics = compare_linear_masters(cpu_output.clone(), run.output.clone())?;
                let rejection_quality = if let Some((clean, mask)) = artifact_contract {
                    Some(compare_artifact_rejection(
                        measure_artifact_residual(clean, cpu_output, mask)?,
                        measure_artifact_residual(clean, &run.output, mask)?,
                    ))
                } else {
                    None
                };
                let quality_pass = benchmark_quality_pass(&metrics)
                    && rejection_quality
                        .as_ref()
                        .is_none_or(|quality| quality.pass);
                regression_guard &= quality_pass;
                baseline_quality.push(BaselineQualityResult {
                    zenith_mode: run.mode.clone(),
                    cold_cache: run.cold_cache,
                    quality_pass,
                    metrics,
                    rejection_quality,
                });
            }
            let mut per_run_ratios = Vec::new();
            let mut per_run_quality = Vec::new();
            for competitor in &ds.comparators {
                let metrics =
                    compare_linear_masters(competitor.output.clone(), run.output.clone())?;
                let rejection_quality = if let Some((clean, mask)) = artifact_contract {
                    Some(compare_artifact_rejection(
                        measure_artifact_residual(clean, &competitor.output, mask)?,
                        measure_artifact_residual(clean, &run.output, mask)?,
                    ))
                } else {
                    None
                };
                let quality_pass = benchmark_quality_pass(&metrics)
                    && rejection_quality
                        .as_ref()
                        .is_none_or(|quality| quality.pass);
                per_run_quality.push(quality_pass);
                let speed_ratio = competitor.elapsed_seconds / run.elapsed_seconds;
                per_run_ratios.push(speed_ratio);
                comparisons.push(BenchmarkComparisonResult {
                    zenith_mode: run.mode.clone(),
                    cold_cache: run.cold_cache,
                    competitor: competitor.engine.clone(),
                    competitor_version: competitor.version.clone(),
                    speed_ratio,
                    quality_pass,
                    metrics,
                    rejection_quality,
                });
            }
            if let Some(fastest) = per_run_ratios.into_iter().reduce(f64::min) {
                fastest_ratios.push(fastest);
            }
            if !per_run_quality.is_empty() {
                competitive_quality_cases.push(per_run_quality);
            }
        }
        if ds.zenith_runs.is_empty() {
            warnings.push("Sin zenithRuns: el dataset sólo puede validarse manualmente".into());
        }
        if !ds.zenith_runs.iter().any(|r| r.cold_cache) {
            warnings.push("Falta medición de caché fría".into());
        }
        if !ds.zenith_runs.iter().any(|r| !r.cold_cache) {
            warnings.push("Falta medición de caché caliente".into());
        }
        reports.push(BenchmarkDatasetReport {
            id: ds.id.clone(),
            domain: ds.domain.clone(),
            runs,
            baseline_speedups,
            comparisons,
            baseline_quality,
            warnings,
        });
    }
    let med_base = median(all_baseline);
    let med_comp = median(fastest_ratios);
    let (quality_total, _quality_passed, quality_rate, competitive_regression_guard) =
        competitive_quality_summary(&competitive_quality_cases);
    let target_2x = med_base.is_some_and(|v| v >= 2.0);
    let target_1_5x = med_comp.is_some_and(|v| v >= 1.5);
    let target_quality = quality_total > 0 && quality_rate >= 80.0;
    let combined_regression_guard = regression_guard && competitive_regression_guard;
    let environment = manifest
        .environment
        .clone()
        .unwrap_or_else(get_benchmark_environment);
    let report = BenchmarkSuiteReport {
        schema_version: "zenith-benchmark-report-v3".into(),
        suite_version: manifest.suite_version,
        generated_at_utc: chrono::Utc::now().to_rfc3339(),
        environment,
        validation,
        datasets: reports,
        acceptance: BenchmarkAcceptance {
            median_vs_zenith_cpu: med_base,
            median_vs_fastest_competitor: med_comp,
            quality_pass_rate_percent: quality_rate,
            regression_guard_pass: combined_regression_guard,
            target_2x_pass: target_2x,
            target_1_5x_pass: target_1_5x,
            target_quality_80pct_pass: target_quality,
            competitive_regression_guard_pass: competitive_regression_guard,
            matrix_complete,
            evidence_complete,
            publishable_claim: target_2x
                && target_1_5x
                && target_quality
                && combined_regression_guard
                && matrix_complete
                && evidence_complete,
        },
    };
    debug_assert_eq!(report.acceptance.matrix_complete, missing.is_empty());
    let out = Path::new(&output_path);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Crear carpeta del reporte: {e}"))?;
    }
    std::fs::write(
        out,
        serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("Escribir reporte: {e}"))?;
    Ok(report)
}

#[tauri::command]
pub fn compare_linear_masters(
    reference_path: String,
    candidate_path: String,
) -> Result<LinearImageComparison, String> {
    let reference = crate::ds_read_image(&reference_path)?;
    let candidate = crate::ds_read_image(&candidate_path)?;
    let incompatible =
        |reason: String, correlation: f64, registration_correction_px: f64| LinearImageComparison {
            compatible: false,
            width: candidate.w,
            height: candidate.h,
            channels: candidate.ch,
            rmse_adu: 0.0,
            normalized_rmse: 0.0,
            mean_absolute_error_adu: 0.0,
            max_absolute_error_adu: 0.0,
            flux_error_percent: 0.0,
            reference_noise_adu: 0.0,
            candidate_noise_adu: 0.0,
            noise_regression_percent: 0.0,
            reference_fwhm_px: 0.0,
            candidate_fwhm_px: 0.0,
            fwhm_regression_percent: 0.0,
            registration_error_px: 0.0,
            registration_correction_px,
            correlation,
            fitted_scale: 1.0,
            fitted_offset: 0.0,
            reason: Some(reason),
        };
    if reference.w != candidate.w || reference.h != candidate.h || reference.ch != candidate.ch {
        return Ok(incompatible(
            format!(
                "Geometría incompatible: referencia {}×{}×{}, candidato {}×{}×{}",
                reference.w, reference.h, reference.ch, candidate.w, candidate.h, candidate.ch
            ),
            0.0,
            0.0,
        ));
    }
    if reference.w == 0 || reference.h == 0 {
        return Err("Máster vacío".into());
    }

    let luma = |img: &crate::DsImage| -> Vec<f32> {
        if img.ch == 1 {
            img.data[..img.w * img.h].to_vec()
        } else {
            img.data
                .chunks_exact(img.ch)
                .take(img.w * img.h)
                .map(|p| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2])
                .collect()
        }
    };
    let ref_luma = luma(&reference);
    let cand_luma = luma(&candidate);

    // Alineación residual: la suite sólo admite la misma geometría/escala,
    // pero corrige una traslación entera pequeña antes de comparar fotometría.
    let sample_stride = ((reference.w * reference.h) as f64 / 80_000.0)
        .sqrt()
        .floor()
        .max(1.0) as usize;
    let mut best = (f64::NEG_INFINITY, 0i32, 0i32);
    let mut correlation_grid = std::collections::BTreeMap::new();
    for dy in -8i32..=8 {
        for dx in -8i32..=8 {
            let (mut sx, mut sy, mut sxx, mut syy, mut sxy, mut count) =
                (0.0, 0.0, 0.0, 0.0, 0.0, 0usize);
            for y in (0..reference.h).step_by(sample_stride) {
                let cy = y as i32 + dy;
                if cy < 0 || cy >= candidate.h as i32 {
                    continue;
                }
                for x in (0..reference.w).step_by(sample_stride) {
                    let cx = x as i32 + dx;
                    if cx < 0 || cx >= candidate.w as i32 {
                        continue;
                    }
                    let a = ref_luma[y * reference.w + x] as f64;
                    let b = cand_luma[cy as usize * candidate.w + cx as usize] as f64;
                    sx += a;
                    sy += b;
                    sxx += a * a;
                    syy += b * b;
                    sxy += a * b;
                    count += 1;
                }
            }
            if count > 32 {
                let nf = count as f64;
                let den = ((nf * sxx - sx * sx) * (nf * syy - sy * sy))
                    .max(0.0)
                    .sqrt();
                let corr = if den > 1e-12 {
                    (nf * sxy - sx * sy) / den
                } else {
                    0.0
                };
                correlation_grid.insert((dx, dy), corr);
                if corr > best.0 {
                    best = (corr, dx, dy);
                }
            }
        }
    }
    let (correlation, dx, dy) = best;
    let parabola_offset = |left: f64, center: f64, right: f64| {
        let den = left - 2.0 * center + right;
        if den.abs() > 1e-12 {
            (0.5 * (left - right) / den).clamp(-0.5, 0.5)
        } else {
            0.0
        }
    };
    let sub_x = match (
        correlation_grid.get(&(dx - 1, dy)),
        correlation_grid.get(&(dx + 1, dy)),
    ) {
        (Some(&l), Some(&r)) => parabola_offset(l, correlation, r),
        _ => 0.0,
    };
    let sub_y = match (
        correlation_grid.get(&(dx, dy - 1)),
        correlation_grid.get(&(dx, dy + 1)),
    ) {
        (Some(&l), Some(&r)) => parabola_offset(l, correlation, r),
        _ => 0.0,
    };
    let registration_error = (sub_x * sub_x + sub_y * sub_y).sqrt();
    let registration_correction =
        ((dx as f64 + sub_x).powi(2) + (dy as f64 + sub_y).powi(2)).sqrt();
    if !correlation.is_finite() || correlation < 0.5 {
        return Ok(incompatible(
            "Contenido, escala o estirado incompatible: correlación lineal insuficiente".into(),
            correlation.max(0.0),
            registration_error,
        ));
    }

    // Ajuste lineal candidato→referencia sobre el solape ya registrado.
    let (mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0, 0.0, 0.0);
    let mut aligned = Vec::new();
    for y in 0..reference.h {
        let cy = y as i32 + dy;
        if cy < 0 || cy >= candidate.h as i32 {
            continue;
        }
        for x in 0..reference.w {
            let cx = x as i32 + dx;
            if cx < 0 || cx >= candidate.w as i32 {
                continue;
            }
            for c in 0..reference.ch {
                let rv = reference.data[(y * reference.w + x) * reference.ch + c] as f64;
                let cv = candidate.data
                    [(cy as usize * candidate.w + cx as usize) * candidate.ch + c]
                    as f64;
                aligned.push((cv, rv));
                sx += cv;
                sy += rv;
                sxx += cv * cv;
                sxy += cv * rv;
            }
        }
    }
    let nf = aligned.len() as f64;
    if nf == 0.0 {
        return Err("Másters sin solape".into());
    }
    let den = nf * sxx - sx * sx;
    let scale = if den.abs() > 1e-12 {
        (nf * sxy - sx * sy) / den
    } else {
        1.0
    };
    let offset = (sy - scale * sx) / nf;
    let (mut se, mut sae, mut maxe) = (0.0f64, 0.0f64, 0.0f64);
    for &(x, y) in &aligned {
        let e = scale * x + offset - y;
        se += e * e;
        sae += e.abs();
        maxe = maxe.max(e.abs());
    }
    let rmse = (se / nf).sqrt();
    let (ref_bg, ref_noise) = crate::ds_bg_noise(&ref_luma);
    let (cand_bg, cand_noise) = crate::ds_bg_noise(&cand_luma);
    let ref_flux: f64 = ref_luma.iter().map(|&v| (v - ref_bg).max(0.0) as f64).sum();
    let cand_flux: f64 = cand_luma
        .iter()
        .map(|&v| (scale * (v - cand_bg) as f64).max(0.0))
        .sum();
    let ref_stars = crate::ds_detect_stars(&ref_luma, reference.w, reference.h, 200);
    let cand_stars = crate::ds_detect_stars(&cand_luma, candidate.w, candidate.h, 200);
    let ref_fwhm = crate::ds_frame_fwhm_proxy(&ref_luma, reference.w, reference.h, &ref_stars);
    let cand_fwhm = crate::ds_frame_fwhm_proxy(&cand_luma, candidate.w, candidate.h, &cand_stars);
    Ok(LinearImageComparison {
        compatible: true,
        width: reference.w,
        height: reference.h,
        channels: reference.ch,
        rmse_adu: rmse,
        normalized_rmse: rmse / 65535.0,
        mean_absolute_error_adu: sae / nf,
        max_absolute_error_adu: maxe,
        flux_error_percent: if ref_flux.abs() > 1e-9 {
            100.0 * (cand_flux - ref_flux).abs() / ref_flux.abs()
        } else {
            0.0
        },
        reference_noise_adu: ref_noise as f64,
        candidate_noise_adu: cand_noise as f64,
        noise_regression_percent: if ref_noise > 1e-6 {
            100.0 * (cand_noise / ref_noise - 1.0) as f64
        } else {
            0.0
        },
        reference_fwhm_px: ref_fwhm as f64,
        candidate_fwhm_px: cand_fwhm as f64,
        fwhm_regression_percent: if ref_fwhm > 1e-6 {
            100.0 * (cand_fwhm / ref_fwhm - 1.0) as f64
        } else {
            0.0
        },
        registration_error_px: registration_error,
        registration_correction_px: registration_correction,
        correlation,
        fitted_scale: scale,
        fitted_offset: offset,
        reason: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_environment() -> BenchmarkEnvironment {
        BenchmarkEnvironment {
            app_version: "test".into(),
            os: "test-os".into(),
            arch: "test-arch".into(),
            cpu_threads: 8,
            memory_mb: 16_384,
            gpu_available: true,
            gpu_name: "test-gpu".into(),
            gpu_backend: "test-backend".into(),
            gpu_budget_mb: 4_096,
            generated_at_utc: "2026-07-10T12:00:00Z".into(),
        }
    }

    fn test_event(
        job_id: &str,
        phase: &str,
        progress: f32,
    ) -> crate::pipeline::RecordedPipelineTelemetry {
        crate::pipeline::RecordedPipelineTelemetry {
            monotonic_ms: (progress * 10.0) as u128,
            captured_at_utc: "2026-07-10T12:00:00Z".into(),
            event: crate::pipeline::PipelineTelemetry {
                job_id: job_id.into(),
                domain: crate::pipeline::PipelineDomain::DeepSky,
                phase: phase.into(),
                engine: "Hybrid CPU + GPU wgpu".into(),
                progress,
                eta_seconds: None,
                items_done: progress as usize,
                items_total: 100,
                throughput: Some(5.0),
                cpu_percent: Some(35.0),
                gpu_percent: None,
                ram_mb: 512,
                vram_mb: 256,
                io_read_mb: progress as f64,
                io_write_mb: progress as f64 * 0.5,
                cache_hits: 2,
                cache_misses: 3,
                fallback_reason: None,
            },
        }
    }

    #[test]
    fn linear_master_comparison_measures_registration_noise_and_fwhm() {
        let (w, h) = (96usize, 80usize);
        let mut reference = vec![500.0f32; w * h];
        for (sx, sy, peak) in [
            (20usize, 20usize, 8000.0f32),
            (70, 25, 12000.0),
            (45, 60, 10000.0),
        ] {
            for y in sy - 4..=sy + 4 {
                for x in sx - 4..=sx + 4 {
                    let d2 = (x as f32 - sx as f32).powi(2) + (y as f32 - sy as f32).powi(2);
                    reference[y * w + x] += peak * (-d2 / 4.5).exp();
                }
            }
        }
        let mut candidate = vec![500.0f32; w * h];
        for y in 0..h {
            for x in 0..w - 2 {
                candidate[y * w + x + 2] = reference[y * w + x];
            }
        }
        let dir = std::env::temp_dir().join(format!("zas-benchmark-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rp = dir.join("reference.fits");
        let cp = dir.join("candidate.fits");
        crate::ds_save_float32_fits(&rp, &reference, w, h, 1, &[]).unwrap();
        crate::ds_save_float32_fits(&cp, &candidate, w, h, 1, &[]).unwrap();
        let metrics =
            compare_linear_masters(rp.display().to_string(), cp.display().to_string()).unwrap();
        assert!(metrics.compatible && metrics.correlation > 0.99);
        assert!((metrics.registration_correction_px - 2.0).abs() < 0.05);
        assert!(metrics.registration_error_px < 0.05);
        assert!(metrics.fwhm_regression_percent.abs() < 3.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn output_contract_is_checked_against_real_geometry_and_sample_format() {
        let dir =
            std::env::temp_dir().join(format!("zas-output-contract-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("actual.tiff");
        image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_pixel(3, 2, image::Luma([2048]))
            .save(&path)
            .unwrap();
        let declared = BenchmarkOutputContract {
            linear: true,
            stretched: false,
            width: 2,
            height: 2,
            channels: 1,
            sample_format: "float32".into(),
            drizzle_scale: 1.0,
            crop: BenchmarkCrop {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            post_processing: Vec::new(),
        };
        let mut errors = Vec::new();
        validate_actual_output_contract("fixture", path.to_str().unwrap(), &declared, &mut errors);
        assert!(errors.iter().any(|error| error.contains("3×2×1")));
        assert!(errors
            .iter()
            .any(|error| error.contains("archivo real usa uint16")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn competitive_claim_requires_complete_dataset_matrix_and_evidence() {
        let matrix_contract = parse_dataset_matrix().unwrap();
        let partial = BenchmarkManifest {
            suite_version: "test".into(),
            environment: None,
            datasets: vec![BenchmarkDataset {
                id: "planetary-ser-mono-surface".into(),
                domain: "planetary".into(),
                sources: vec!["/tmp/source.ser".into()],
                zenith_output: None,
                zenith_runs: Vec::new(),
                baseline_cpu_seconds: None,
                baseline_cpu_output: None,
                baseline_cpu_processing: None,
                baseline_cpu_parameters: None,
                baseline_cpu_log: None,
                artifact_free_reference: None,
                artifact_mask: None,
                comparators: Vec::new(),
            }],
        };
        let (matrix, missing) = benchmark_matrix_status(&partial);
        let evidence = matrix && validate_benchmark_manifest_data(&partial).valid;
        assert!(
            !matrix && !evidence && missing.len() == matrix_contract.required_scenarios.len() - 1
        );
    }

    #[test]
    fn embedded_dataset_matrix_is_authoritative_and_binds_domains() {
        let matrix = get_benchmark_dataset_matrix().unwrap();
        assert_eq!(matrix.schema_version, "zenith-dataset-matrix-v1");
        assert_eq!(matrix.required_scenarios.len(), 10);
        assert!(matrix.required_scenarios.iter().all(|scenario| {
            !scenario.required_tags.is_empty() && !scenario.acceptance.is_empty()
        }));
        let satellite = matrix
            .required_scenarios
            .iter()
            .find(|scenario| scenario.id == "deep-sky-satellite-trails")
            .unwrap();
        assert_eq!(satellite.domain, "deep_sky");
        assert!(satellite
            .required_evidence
            .iter()
            .any(|evidence| evidence == "artifactMask"));

        let wrong_domain = BenchmarkManifest {
            suite_version: "test".into(),
            environment: Some(test_environment()),
            datasets: vec![BenchmarkDataset {
                id: satellite.id.clone(),
                domain: "planetary".into(),
                sources: Vec::new(),
                zenith_output: None,
                zenith_runs: Vec::new(),
                baseline_cpu_seconds: None,
                baseline_cpu_output: None,
                baseline_cpu_processing: None,
                baseline_cpu_parameters: None,
                baseline_cpu_log: None,
                artifact_free_reference: None,
                artifact_mask: None,
                comparators: Vec::new(),
            }],
        };
        let validation = validate_benchmark_manifest_data(&wrong_domain);
        assert!(validation
            .errors
            .iter()
            .any(|error| error.contains("contradice la matriz")));
    }

    #[test]
    fn eighty_percent_quality_cannot_hide_one_major_competitive_regression() {
        let cases = vec![
            vec![true, true],
            vec![true],
            vec![true],
            vec![true],
            vec![false],
        ];
        let (total, passed, rate, no_regression) = competitive_quality_summary(&cases);
        assert_eq!((total, passed), (5, 4));
        assert!((rate - 80.0).abs() < 1e-9);
        assert!(
            !no_regression,
            "la tasa del 80% no debe ocultar una regresión individual"
        );

        let all_good = vec![vec![true, true], vec![true]];
        let (_, _, rate, no_regression) = competitive_quality_summary(&all_good);
        assert_eq!(rate, 100.0);
        assert!(no_regression);
    }

    #[test]
    fn telemetry_without_terminal_event_is_not_valid_evidence() {
        let events = vec![test_event("job-1", "register", 50.0)];
        let export = PipelineTelemetryExport {
            schema_version: "zenith-pipeline-telemetry-v2".into(),
            environment: test_environment(),
            job_id: Some("job-1".into()),
            phases: summarize_pipeline_telemetry(&events),
            events,
        };
        let mut errors = Vec::new();
        validate_telemetry_content(
            "test",
            &export,
            "deep_sky",
            Some(&test_environment()),
            &mut errors,
        );
        assert!(errors.iter().any(|error| error.contains("evento terminal")));

        let mut complete = export;
        complete.events.push(test_event("job-1", "complete", 100.0));
        complete.phases = summarize_pipeline_telemetry(&complete.events);
        let mut errors = Vec::new();
        validate_telemetry_content(
            "test",
            &complete,
            "deep_sky",
            Some(&test_environment()),
            &mut errors,
        );
        assert!(errors.is_empty(), "{errors:?}");

        complete.events.push(test_event("job-1", "export", 100.0));
        complete.phases = summarize_pipeline_telemetry(&complete.events);
        let mut errors = Vec::new();
        validate_telemetry_content(
            "test",
            &complete,
            "deep_sky",
            Some(&test_environment()),
            &mut errors,
        );
        assert!(errors
            .iter()
            .any(|error| error.contains("trabajo después de complete")));
    }

    #[test]
    fn deepsky_recipe_with_abe_scnr_cannot_authorize_benchmark() {
        let dir = std::env::temp_dir().join(format!(
            "zas-benchmark-recipe-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("thread")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recipe.json");
        let parameters = serde_json::json!({ "optionalAbeScnr": true, "rejection": "sigma" });
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "linearFloat32": true,
                "resultId": "job-1",
                "width": 32,
                "height": 24,
                "channels": 1,
                "recipe": {
                    "sourceFingerprint": "abc123",
                    "parameters": parameters,
                    "frames": [{ "path": "/tmp/light.fits", "used": true }]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let run = ZenithBenchmarkRun {
            mode: "hybrid-v2".into(),
            cold_cache: true,
            elapsed_seconds: 1.0,
            output: "/tmp/master.fits".into(),
            telemetry: Vec::new(),
            cache_preparation: Some("test".into()),
            processing: Some(BenchmarkOutputContract {
                linear: true,
                stretched: false,
                width: 32,
                height: 24,
                channels: 1,
                sample_format: "float32".into(),
                drizzle_scale: 1.0,
                crop: BenchmarkCrop {
                    x: 0,
                    y: 0,
                    width: 32,
                    height: 24,
                },
                post_processing: Vec::new(),
            }),
            parameters: Some(parameters),
            recipe: Some(path.display().to_string()),
        };
        let mut errors = Vec::new();
        validate_deepsky_recipe(
            "test recipe",
            path.to_str().unwrap(),
            &run,
            &std::collections::HashSet::from(["job-1".to_string()]),
            &mut errors,
        );
        assert!(errors.iter().any(|error| error.contains("ABE/SCNR")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn satellite_rejection_metric_enforces_three_percent_regression_guard() {
        let (width, height) = (48usize, 32usize);
        let clean: Vec<f32> = (0..width * height)
            .map(|index| 500.0 + (index % width) as f32 * 0.25)
            .collect();
        let mut mask = vec![0.0f32; width * height];
        for y in 10..14 {
            for x in 8..40 {
                mask[y * width + x] = 1.0;
            }
        }
        let with_residual = |amount: f32| -> Vec<f32> {
            clean
                .iter()
                .zip(&mask)
                .map(|(&value, &selected)| value + selected * amount)
                .collect()
        };
        let dir =
            std::env::temp_dir().join(format!("zas-rejection-metric-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clean_path = dir.join("clean.fits");
        let mask_path = dir.join("mask.fits");
        let baseline_path = dir.join("baseline.fits");
        let good_path = dir.join("good.fits");
        let bad_path = dir.join("bad.fits");
        crate::ds_save_float32_fits(&clean_path, &clean, width, height, 1, &[]).unwrap();
        crate::ds_save_float32_fits(&mask_path, &mask, width, height, 1, &[]).unwrap();
        crate::ds_save_float32_fits(&baseline_path, &with_residual(10.0), width, height, 1, &[])
            .unwrap();
        crate::ds_save_float32_fits(&good_path, &with_residual(10.2), width, height, 1, &[])
            .unwrap();
        crate::ds_save_float32_fits(&bad_path, &with_residual(12.0), width, height, 1, &[])
            .unwrap();
        let measure = |path: &std::path::Path| {
            measure_artifact_residual(
                clean_path.to_str().unwrap(),
                path.to_str().unwrap(),
                mask_path.to_str().unwrap(),
            )
            .unwrap()
        };
        let reference = measure(&baseline_path);
        let good = compare_artifact_rejection(reference.clone(), measure(&good_path));
        let bad = compare_artifact_rejection(reference, measure(&bad_path));
        assert!(good.pass && good.regression_percent < 3.0);
        assert!(!bad.pass && bad.regression_percent > 3.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn complete_synthetic_matrix_exercises_every_report_gate() {
        let root = std::env::temp_dir().join(format!(
            "zas-benchmark-full-matrix-{}-{}",
            std::process::id(),
            BENCHMARK_RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("synthetic-source.dat");
        let log = root.join("engine.log");
        std::fs::write(&source, b"deterministic synthetic fixture").unwrap();
        std::fs::write(&log, b"synthetic exact-version exact-parameters\n").unwrap();

        let (width, height) = (32usize, 24usize);
        let mut linear = vec![500.0f32; width * height];
        for (sx, sy, peak) in [
            (8usize, 7usize, 8000.0f32),
            (23, 9, 12000.0),
            (16, 18, 9000.0),
        ] {
            for y in sy.saturating_sub(3)..=(sy + 3).min(height - 1) {
                for x in sx.saturating_sub(3)..=(sx + 3).min(width - 1) {
                    let d2 = (x as f32 - sx as f32).powi(2) + (y as f32 - sy as f32).powi(2);
                    linear[y * width + x] += peak * (-d2 / 3.5).exp();
                }
            }
        }
        let deep_output = root.join("deep-master.fits");
        crate::ds_save_float32_fits(&deep_output, &linear, width, height, 1, &[]).unwrap();
        let planet_output = root.join("planet-master.tiff");
        let planet: Vec<u16> = linear
            .iter()
            .map(|value| value.clamp(0.0, 65535.0).round() as u16)
            .collect();
        image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_raw(
            width as u32,
            height as u32,
            planet,
        )
        .unwrap()
        .save(&planet_output)
        .unwrap();
        let mask_path = root.join("satellite-mask.fits");
        let mut mask = vec![0.0f32; width * height];
        for y in 10..14 {
            for x in 4..28 {
                mask[y * width + x] = 1.0;
            }
        }
        crate::ds_save_float32_fits(&mask_path, &mask, width, height, 1, &[]).unwrap();

        let event =
            |domain: crate::pipeline::PipelineDomain,
             job: &str,
             phase: &str,
             progress: f32,
             monotonic_ms: u128| crate::pipeline::RecordedPipelineTelemetry {
                monotonic_ms,
                captured_at_utc: "2026-07-10T12:00:00Z".into(),
                event: crate::pipeline::PipelineTelemetry {
                    job_id: job.into(),
                    domain,
                    phase: phase.into(),
                    engine: "Hybrid CPU + GPU wgpu".into(),
                    progress,
                    eta_seconds: None,
                    items_done: progress as usize,
                    items_total: 100,
                    throughput: Some(20.0),
                    cpu_percent: Some(30.0),
                    gpu_percent: None,
                    ram_mb: 512,
                    vram_mb: 256,
                    io_read_mb: monotonic_ms as f64 / 100.0,
                    io_write_mb: monotonic_ms as f64 / 200.0,
                    cache_hits: 2,
                    cache_misses: 1,
                    fallback_reason: None,
                },
            };
        let planet_events = vec![
            event(
                crate::pipeline::PipelineDomain::Planetary,
                "planet-analysis",
                "analysis",
                50.0,
                0,
            ),
            event(
                crate::pipeline::PipelineDomain::Planetary,
                "planet-analysis",
                "complete",
                100.0,
                100,
            ),
            event(
                crate::pipeline::PipelineDomain::Planetary,
                "planet-stack",
                "stacking",
                50.0,
                200,
            ),
            event(
                crate::pipeline::PipelineDomain::Planetary,
                "planet-stack",
                "complete",
                100.0,
                300,
            ),
        ];
        let deep_events = vec![
            event(
                crate::pipeline::PipelineDomain::DeepSky,
                "deep-job",
                "calibrate_detect",
                10.0,
                0,
            ),
            event(
                crate::pipeline::PipelineDomain::DeepSky,
                "deep-job",
                "register",
                30.0,
                100,
            ),
            event(
                crate::pipeline::PipelineDomain::DeepSky,
                "deep-job",
                "normalize",
                50.0,
                200,
            ),
            event(
                crate::pipeline::PipelineDomain::DeepSky,
                "deep-job",
                "integrate_sigma",
                70.0,
                300,
            ),
            event(
                crate::pipeline::PipelineDomain::DeepSky,
                "deep-job",
                "export",
                90.0,
                400,
            ),
            event(
                crate::pipeline::PipelineDomain::DeepSky,
                "deep-job",
                "complete",
                100.0,
                500,
            ),
        ];
        let write_telemetry =
            |name: &str,
             job_id: Option<String>,
             events: Vec<crate::pipeline::RecordedPipelineTelemetry>| {
                let path = root.join(name);
                let export = PipelineTelemetryExport {
                    schema_version: "zenith-pipeline-telemetry-v2".into(),
                    environment: test_environment(),
                    job_id,
                    phases: summarize_pipeline_telemetry(&events),
                    events,
                };
                write_json_atomic(&path, &export).unwrap();
                path
            };
        let planet_telemetry = write_telemetry("planet-telemetry.json", None, planet_events);
        let deep_telemetry =
            write_telemetry("deep-telemetry.json", Some("deep-job".into()), deep_events);

        let deep_parameters = serde_json::json!({
            "computePolicy": "hybrid",
            "profile": "balanced",
            "rejection": "sigma",
            "optionalAbeScnr": false
        });
        let recipe_path = root.join("deep-recipe.json");
        write_json_atomic(
            &recipe_path,
            &serde_json::json!({
                "schemaVersion": 2,
                "linearFloat32": true,
                "resultId": "deep-job",
                "width": width,
                "height": height,
                "channels": 1,
                "recipe": {
                    "sourceFingerprint": "synthetic-matrix-v1",
                    "parameters": deep_parameters,
                    "frames": [{"path": source.display().to_string(), "used": true}]
                }
            }),
        )
        .unwrap();

        let contract = |domain: &str| BenchmarkOutputContract {
            linear: true,
            stretched: false,
            width,
            height,
            channels: 1,
            sample_format: if domain == "deep_sky" {
                "float32".into()
            } else {
                "uint16".into()
            },
            drizzle_scale: 1.0,
            crop: BenchmarkCrop {
                x: 0,
                y: 0,
                width,
                height,
            },
            post_processing: Vec::new(),
        };
        let matrix = parse_dataset_matrix().unwrap();
        let datasets: Vec<BenchmarkDataset> = matrix
            .required_scenarios
            .iter()
            .map(|scenario| {
                let output = if scenario.domain == "deep_sky" {
                    &deep_output
                } else {
                    &planet_output
                };
                let telemetry = if scenario.domain == "deep_sky" {
                    &deep_telemetry
                } else {
                    &planet_telemetry
                };
                let parameters = if scenario.domain == "deep_sky" {
                    deep_parameters.clone()
                } else {
                    serde_json::json!({
                        "computePolicy": "hybrid",
                        "profile": "balanced",
                        "percent": 15
                    })
                };
                let make_run = |cold_cache: bool| ZenithBenchmarkRun {
                    mode: "hybrid-v2".into(),
                    cold_cache,
                    elapsed_seconds: 1.0,
                    output: output.display().to_string(),
                    telemetry: vec![telemetry.display().to_string()],
                    cache_preparation: Some(if cold_cache {
                        "Fixture sintético regenerado antes de la corrida fría".into()
                    } else {
                        "Fixture y caches conservados para la corrida caliente".into()
                    }),
                    processing: Some(contract(&scenario.domain)),
                    parameters: Some(parameters.clone()),
                    recipe: (scenario.domain == "deep_sky")
                        .then(|| recipe_path.display().to_string()),
                };
                BenchmarkDataset {
                    id: scenario.id.clone(),
                    domain: scenario.domain.clone(),
                    sources: vec![source.display().to_string()],
                    zenith_output: Some(output.display().to_string()),
                    zenith_runs: vec![make_run(true), make_run(false)],
                    baseline_cpu_seconds: Some(2.2),
                    baseline_cpu_output: Some(output.display().to_string()),
                    baseline_cpu_processing: Some(contract(&scenario.domain)),
                    baseline_cpu_parameters: Some(serde_json::json!({
                        "engine": "zenith-cpu-reference",
                        "profile": "balanced"
                    })),
                    baseline_cpu_log: Some(log.display().to_string()),
                    artifact_free_reference: (scenario.id == "deep-sky-satellite-trails")
                        .then(|| deep_output.display().to_string()),
                    artifact_mask: (scenario.id == "deep-sky-satellite-trails")
                        .then(|| mask_path.display().to_string()),
                    comparators: vec![ComparatorRun {
                        engine: "synthetic-comparator".into(),
                        version: "1.0-test".into(),
                        output: output.display().to_string(),
                        elapsed_seconds: 1.6,
                        log: Some(log.display().to_string()),
                        processing: Some(contract(&scenario.domain)),
                        parameters: Some(serde_json::json!({
                            "fixture": "deterministic",
                            "profile": "balanced"
                        })),
                    }],
                }
            })
            .collect();
        let manifest = BenchmarkManifest {
            suite_version: "synthetic-matrix-v1".into(),
            environment: Some(test_environment()),
            datasets,
        };
        let manifest_path = root.join("manifest.json");
        let report_path = root.join("report.json");
        write_json_atomic(&manifest_path, &manifest).unwrap();
        let report = generate_benchmark_report(
            manifest_path.display().to_string(),
            report_path.display().to_string(),
        )
        .unwrap();
        assert!(report.validation.valid, "{:?}", report.validation.errors);
        assert!(report.acceptance.matrix_complete);
        assert!(report.acceptance.evidence_complete);
        assert_eq!(report.acceptance.median_vs_zenith_cpu, Some(2.2));
        assert_eq!(report.acceptance.median_vs_fastest_competitor, Some(1.6));
        assert_eq!(report.acceptance.quality_pass_rate_percent, 100.0);
        assert!(report.acceptance.competitive_regression_guard_pass);
        assert!(report.acceptance.publishable_claim);
        assert!(report_path.is_file());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn benchmark_session_measures_and_exports_manifest_ready_evidence() {
        let root = std::env::temp_dir().join(format!(
            "zas-benchmark-session-test-{}-{}",
            std::process::id(),
            BENCHMARK_RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let begin = BeginBenchmarkRunRequest {
            dataset_id: "planetary-ser-mono-surface".into(),
            domain: "planetary".into(),
            mode: "hybrid-v2".into(),
            cold_cache: true,
            cache_preparation: "FrameStore eliminado antes de iniciar; caché del SO no manipulada"
                .into(),
            output_directory: root.display().to_string(),
        };
        let session = begin_benchmark_run(begin.clone()).unwrap();
        assert_eq!(
            get_active_benchmark_run().unwrap().unwrap().session_id,
            session.session_id
        );
        assert!(begin_benchmark_run(begin)
            .unwrap_err()
            .contains("corrida activa"));

        let output = root.join("master.tiff");
        image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_pixel(2, 2, image::Luma([1024]))
            .save(&output)
            .unwrap();
        let processing = || BenchmarkOutputContract {
            linear: true,
            stretched: false,
            width: 2,
            height: 2,
            channels: 1,
            sample_format: "uint16".into(),
            drizzle_scale: 1.0,
            crop: BenchmarkCrop {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            post_processing: Vec::new(),
        };
        let invalid = finish_benchmark_run(FinishBenchmarkRunRequest {
            session_id: session.session_id.clone(),
            output: output.display().to_string(),
            job_ids: Vec::new(),
            processing: processing(),
            parameters: serde_json::json!({ "percent": 15 }),
            recipe: None,
        })
        .unwrap_err();
        assert!(invalid.contains("jobIds es obligatorio"));
        assert!(get_active_benchmark_run().unwrap().is_some());

        let record = |job_id: &str, phase: &str, progress: f32| {
            crate::pipeline::record_pipeline_telemetry(&crate::pipeline::PipelineTelemetry {
                job_id: job_id.into(),
                domain: crate::pipeline::PipelineDomain::Planetary,
                phase: phase.into(),
                engine: "Hybrid CPU + GPU wgpu".into(),
                progress,
                eta_seconds: None,
                items_done: progress as usize,
                items_total: 100,
                throughput: Some(20.0),
                cpu_percent: Some(30.0),
                gpu_percent: None,
                ram_mb: 512,
                vram_mb: 256,
                io_read_mb: progress as f64,
                io_write_mb: progress as f64 * 0.25,
                cache_hits: 1,
                cache_misses: 1,
                fallback_reason: None,
            });
        };
        record("analysis-job", "analysis", 50.0);
        record("analysis-job", "complete", 100.0);
        record("stack-job", "stacking", 50.0);
        record("stack-job", "complete", 100.0);
        std::thread::sleep(std::time::Duration::from_millis(2));

        let artifact = finish_benchmark_run(FinishBenchmarkRunRequest {
            session_id: session.session_id,
            output: output.display().to_string(),
            job_ids: vec!["analysis-job".into(), "stack-job".into()],
            processing: processing(),
            parameters: serde_json::json!({ "percent": 15, "profile": "balanced" }),
            recipe: None,
        })
        .unwrap();
        assert!(artifact.zenith_run.elapsed_seconds > 0.0);
        assert!(Path::new(&artifact.telemetry_path).is_file());
        assert!(Path::new(&artifact.record_path).is_file());
        assert!(artifact.phases.contains_key("analysis"));
        assert!(artifact.phases.contains_key("stacking"));
        assert!(get_active_benchmark_run().unwrap().is_none());
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&artifact.record_path).unwrap()).unwrap();
        assert_eq!(
            saved.pointer("/zenithRun/mode").and_then(|v| v.as_str()),
            Some("hybrid-v2")
        );
        assert_eq!(
            saved
                .pointer("/zenithRun/telemetry/0")
                .and_then(|v| v.as_str()),
            Some(artifact.telemetry_path.as_str())
        );
        let abort_session = begin_benchmark_run(BeginBenchmarkRunRequest {
            dataset_id: "planetary-ser-mono-surface".into(),
            domain: "planetary".into(),
            mode: "cpu-only".into(),
            cold_cache: false,
            cache_preparation: "Cachés conservadas para corrida caliente".into(),
            output_directory: root.display().to_string(),
        })
        .unwrap();
        let aborted = abort_benchmark_run(AbortBenchmarkRunRequest {
            session_id: abort_session.session_id,
            reason: Some("Prueba de cancelación limpia".into()),
        })
        .unwrap();
        assert!(Path::new(&aborted.record_path).is_file());
        assert!(get_active_benchmark_run().unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
