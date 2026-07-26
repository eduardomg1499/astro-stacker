// perf_trace.rs — cronómetros de pared por fase para el pipeline planetario.
//
// Objetivo: saber en qué fase se va el tiempo de un run real (análisis o
// apilado) sin depender de un profiler externo y sin coste medible: cada
// span es un Instant + un lock corto sobre un HashMap pequeño. El volcado
// es un JSON por job (schema zas-perf-trace-v1) que los scripts de
// benchmark (scripts/benchmark/planetary_e2e.*) leen para el desglose por
// fase. NO toca PipelineTelemetry (pipeline.rs es compartido con deepsky).
//
// Convenciones de fase: el pase va como prefijo numérico ("p1/", "p2/");
// las fases globales usan pass=0 y se emiten sin prefijo. Los bucles
// calientes deben medir con Instant local y llamar a add_ns() una vez por
// frame (o por lote), no crear un Span por píxel.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const SCHEMA: &str = "zas-perf-trace-v1";
/// Fases con pass=GLOBAL_PASS se emiten sin prefijo "pN/".
pub const GLOBAL_PASS: u8 = 0;

struct PhaseAgg {
    total_ns: u128,
    count: u64,
    // `wall`: elapsed critical-path span; `work`: sum of worker/lane samples.
    // They are intentionally not additive when work overlaps across lanes.
    measurement: &'static str,
}

struct JobTrace {
    kind: &'static str,
    source: String,
    started: Instant,
    started_epoch_ms: u128,
    // Orden de inserción estable para que el JSON se lea en orden de pipeline.
    phases: Vec<((u8, &'static str), PhaseAgg)>,
    meta: BTreeMap<String, String>,
}

static NEXT_JOB: AtomicU64 = AtomicU64::new(1);
static JOBS: OnceLock<Mutex<std::collections::HashMap<u64, JobTrace>>> = OnceLock::new();

fn jobs() -> std::sync::MutexGuard<'static, std::collections::HashMap<u64, JobTrace>> {
    JOBS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Abre un job de traza y devuelve su id (0 nunca se emite).
pub fn job_start(kind: &'static str, source: &str) -> u64 {
    let id = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
    let mut meta = BTreeMap::new();
    meta.insert("commit".into(), env!("ZAS_GIT_COMMIT").into());
    meta.insert("dirty".into(), env!("ZAS_GIT_DIRTY").into());
    meta.insert(
        "worktree_fingerprint".into(),
        env!("ZAS_WORKTREE_FINGERPRINT").into(),
    );
    meta.insert("app_version".into(), env!("CARGO_PKG_VERSION").into());
    meta.insert("os".into(), std::env::consts::OS.into());
    meta.insert("architecture".into(), std::env::consts::ARCH.into());
    let trace = JobTrace {
        kind,
        source: source.to_string(),
        started: Instant::now(),
        started_epoch_ms: epoch_ms(),
        phases: Vec::with_capacity(24),
        meta,
    };
    jobs().insert(id, trace);
    id
}

/// Anota metadatos del job (frames, geometría, policy, pases...).
pub fn job_meta(job: u64, key: &str, value: impl ToString) {
    if job == 0 {
        return;
    }
    if let Some(t) = jobs().get_mut(&job) {
        t.meta.insert(key.to_string(), value.to_string());
    }
}

/// Acumula tiempo ya medido (ns) e items en una fase de un pase.
fn add_ns_with_measurement(
    job: u64,
    pass: u8,
    phase: &'static str,
    ns: u128,
    items: u64,
    measurement: &'static str,
) {
    if job == 0 {
        return;
    }
    let mut guard = jobs();
    let Some(t) = guard.get_mut(&job) else {
        return;
    };
    let key = (pass, phase);
    if let Some((_, agg)) = t.phases.iter_mut().find(|(k, _)| *k == key) {
        agg.total_ns += ns;
        agg.count += items;
        if agg.measurement != measurement {
            agg.measurement = "mixed";
        }
    } else {
        t.phases.push((
            key,
            PhaseAgg {
                total_ns: ns,
                count: items,
                measurement,
            },
        ));
    }
}

/// Acumula trabajo de CPU/GPU ya medido. Si varias lanes se solapan, este
/// total puede ser mayor que la pared del job y no debe restarse del E2E.
pub fn add_ns(job: u64, pass: u8, phase: &'static str, ns: u128, items: u64) {
    add_ns_with_measurement(job, pass, phase, ns, items, "work");
}

/// Registra una duración de pared medida manualmente (cuando el caller también
/// necesita conservarla para StageTelemetry y no puede usar sólo RAII).
pub fn add_wall_ns(job: u64, pass: u8, phase: &'static str, ns: u128, items: u64) {
    add_ns_with_measurement(job, pass, phase, ns, items, "wall");
}

/// Span RAII: mide desde su creación hasta el drop.
pub struct Span {
    job: u64,
    pass: u8,
    phase: &'static str,
    t0: Instant,
    items: u64,
    measurement: &'static str,
}

impl Span {
    /// Cambia el nº de items que este span representa (default 1).
    pub fn items(mut self, items: u64) -> Self {
        self.items = items;
        self
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        add_ns_with_measurement(
            self.job,
            self.pass,
            self.phase,
            self.t0.elapsed().as_nanos(),
            self.items,
            self.measurement,
        );
    }
}

/// Span global (sin pase).
pub fn span(job: u64, phase: &'static str) -> Span {
    span_pass(job, GLOBAL_PASS, phase)
}

/// Span asociado a un pase (1-based; 0 = global).
pub fn span_pass(job: u64, pass: u8, phase: &'static str) -> Span {
    Span {
        job,
        pass,
        phase,
        t0: Instant::now(),
        items: 1,
        measurement: "wall",
    }
}

fn trace_dir() -> std::path::PathBuf {
    match std::env::var_os("ZAS_PERF_TRACE_DIR") {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::env::temp_dir().join("zenith-perf-trace"),
    }
}

fn phase_label(pass: u8, phase: &str) -> String {
    if pass == GLOBAL_PASS {
        phase.to_string()
    } else {
        format!("p{pass}/{phase}")
    }
}

/// Cierra el job, escribe el JSON (best-effort) y devuelve la ruta escrita.
/// `completed=false` marca runs cancelados/fallidos (la traza parcial sigue
/// siendo útil para diagnóstico).
pub fn job_finish(job: u64, completed: bool) -> Option<std::path::PathBuf> {
    if job == 0 {
        return None;
    }
    let trace = jobs().remove(&job)?;
    let retry_overhead_ms = trace
        .meta
        .get("retry_overhead_ms")
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or(0.0);
    // Los intentos internos fallidos se descartan para no duplicar jobs, pero
    // su pared forma parte del E2E de la política solicitada. El wrapper la
    // propaga al intento canónico mediante retry_overhead_ms.
    let total_ms = trace.started.elapsed().as_secs_f64() * 1e3 + retry_overhead_ms;

    let mut phases: Vec<serde_json::Value> = trace
        .phases
        .iter()
        .map(|((pass, phase), agg)| {
            let total = agg.total_ns as f64 / 1e6;
            serde_json::json!({
                "phase": phase_label(*pass, phase),
                "totalMs": (total * 1e3).round() / 1e3,
                "count": agg.count,
                "avgMs": if agg.count > 0 { ((total / agg.count as f64) * 1e6).round() / 1e6 } else { 0.0 },
                "measurement": agg.measurement,
            })
        })
        .collect();
    if retry_overhead_ms > 0.0 {
        phases.insert(
            0,
            serde_json::json!({
                "phase": "retry_overhead",
                "totalMs": (retry_overhead_ms * 1e3).round() / 1e3,
                "count": 1,
                "avgMs": (retry_overhead_ms * 1e3).round() / 1e3,
                "measurement": "wall",
            }),
        );
    }

    let doc = serde_json::json!({
        "schema": SCHEMA,
        "job": trace.kind,
        "source": trace.source,
        "startedAtEpochMs": trace.started_epoch_ms as u64,
        "completed": completed,
        "totalMs": (total_ms * 1e3).round() / 1e3,
        "meta": trace.meta,
        "phases": phases,
    });

    let dir = trace_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let path = dir.join(format!(
        "{}-{}-{}.json",
        trace.kind, job, trace.started_epoch_ms
    ));
    let payload = match serde_json::to_vec_pretty(&doc) {
        Ok(p) => p,
        Err(_) => return None,
    };
    if std::fs::write(&path, payload).is_err() {
        return None;
    }
    eprintln!(
        "[perf-trace] {} {:.1}s -> {}",
        trace.kind,
        total_ms / 1e3,
        path.display()
    );
    Some(path)
}

/// Guardia RAII: garantiza el volcado también en returns tempranos/panics.
/// Llamar a `finish_ok()` en el camino de éxito; el Drop marca incompleto.
pub struct JobGuard {
    job: u64,
    done: bool,
}

impl JobGuard {
    pub fn new(job: u64) -> Self {
        Self { job, done: false }
    }
    pub fn id(&self) -> u64 {
        self.job
    }
    pub fn finish_ok(mut self) -> Option<std::path::PathBuf> {
        self.done = true;
        job_finish(self.job, true)
    }

    /// Descarta una traza que representa sólo un intento interno reiniciable,
    /// no un trabajo de usuario terminado. El intento exterior emitirá la
    /// traza canónica; así el reportador no cuenta dos jobs por un fallback.
    pub fn discard(mut self) {
        self.done = true;
        let _ = jobs().remove(&self.job);
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        if !self.done {
            let _ = job_finish(self.job, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Un único test: ZAS_PERF_TRACE_DIR es process-global y dos tests en
    // paralelo se pisarían la variable entre sí.
    #[test]
    fn job_lifecycle_guard_and_json() {
        let tmp = std::env::temp_dir().join(format!("zas-perf-trace-test-{}", std::process::id()));
        std::env::set_var("ZAS_PERF_TRACE_DIR", &tmp);

        let job = job_start("test", "synthetic.ser");
        job_meta(job, "frames", 3u32);
        {
            let _s = span(job, "open");
        }
        add_ns(job, 1, "debayer", 2_000_000, 2);
        add_ns(job, 1, "debayer", 1_000_000, 1);
        {
            let _s = span_pass(job, 2, "warp").items(4);
        }
        let path = job_finish(job, true).expect("json escrito");
        let raw = std::fs::read_to_string(&path).expect("leer json");
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("json valido");
        assert_eq!(doc["schema"], SCHEMA);
        assert_eq!(doc["completed"], true);
        assert_eq!(doc["meta"]["frames"], "3");
        assert!(doc["meta"]["commit"]
            .as_str()
            .is_some_and(|v| !v.is_empty()));
        assert!(doc["meta"]["worktree_fingerprint"]
            .as_str()
            .is_some_and(|v| !v.is_empty()));
        assert_eq!(doc["meta"]["app_version"], env!("CARGO_PKG_VERSION"));
        let phases = doc["phases"].as_array().expect("phases");
        let deb = phases
            .iter()
            .find(|p| p["phase"] == "p1/debayer")
            .expect("fase debayer");
        assert_eq!(deb["count"], 3);
        assert_eq!(deb["measurement"], "work");
        assert!(deb["totalMs"].as_f64().unwrap() >= 3.0 - 1e-6);
        assert!(phases
            .iter()
            .any(|p| p["phase"] == "open" && p["measurement"] == "wall"));
        assert!(phases
            .iter()
            .any(|p| p["phase"] == "p2/warp" && p["measurement"] == "wall"));
        // El id ya no existe: add posterior es no-op y un segundo finish es None.
        add_ns(job, 1, "debayer", 1, 1);
        assert!(job_finish(job, true).is_none());

        // JobGuard: al soltarse sin finish_ok() vuelca con completed=false.
        let job2 = job_start("test_guard", "x");
        {
            let _g = JobGuard::new(job2);
        }
        assert!(job_finish(job2, true).is_none());
        let mut found_incomplete = false;
        if let Ok(entries) = std::fs::read_dir(&tmp) {
            for e in entries.flatten() {
                if let Ok(raw) = std::fs::read_to_string(e.path()) {
                    if raw.contains("\"test_guard\"") && raw.contains("\"completed\": false") {
                        found_incomplete = true;
                    }
                }
            }
        }
        assert!(found_incomplete, "esperaba un volcado incompleto del guard");

        let job3 = job_start("test_discard", "x");
        JobGuard::new(job3).discard();
        assert!(job_finish(job3, true).is_none());

        let job4 = job_start("test_retry", "x");
        job_meta(job4, "retry_overhead_ms", 125.0);
        let retry_path = job_finish(job4, true).expect("traza con retry");
        let retry_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(retry_path).expect("leer retry"))
                .expect("json retry");
        assert!(retry_doc["totalMs"].as_f64().unwrap() >= 125.0);
        assert!(retry_doc["phases"].as_array().unwrap().iter().any(|phase| {
            phase["phase"] == "retry_overhead" && phase["totalMs"].as_f64().unwrap() == 125.0
        }));
        std::env::remove_var("ZAS_PERF_TRACE_DIR");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
