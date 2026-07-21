#!/usr/bin/env node
// Agregador del benchmark E2E planetario. Lee trazas zas-perf-trace-v1,
// conserva los intentos incompletos/fallidos y calcula tiempos solamente con
// ejecuciones completas. Nunca mezcla commit, fuente, políticas
// compute/decode/calidad, backend o equipo.
//
// Uso: node planetary-e2e-report.mjs <dir-trazas> [--out reporte.json]
//        [--baseline benchmarks/baselines/planetary-timing-<host>.json]
//        [--save-baseline <ruta.json>] [--allow-incomplete]
//        [--require-five-runs] [--enforce-auto-regret]
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const args = process.argv.slice(2);
if (args.length < 1) {
  console.error(
    "uso: planetary-e2e-report.mjs <dir-trazas> [--out r.json] [--baseline b.json] [--save-baseline b.json] [--allow-incomplete] [--require-five-runs] [--enforce-auto-regret]"
  );
  process.exit(2);
}

const dir = args[0];
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 && i + 1 < args.length ? args[i + 1] : null;
};
const outPath = flag("--out");
const baselinePath = flag("--baseline");
const saveBaselinePath = flag("--save-baseline");
const allowIncomplete = args.includes("--allow-incomplete");
const requireFiveRuns = args.includes("--require-five-runs");
const enforceAutoRegret = args.includes("--enforce-auto-regret");

const files = fs
  .readdirSync(dir)
  .filter((f) => f.endsWith(".json"))
  .map((f) => path.join(dir, f));

const traces = [];
const invalidFiles = [];
let ignoredFiles = 0;
for (const file of files) {
  try {
    const doc = JSON.parse(fs.readFileSync(file, "utf8"));
    if (doc.schema !== "zas-perf-trace-v1") {
      ignoredFiles++;
      continue;
    }
    traces.push({ ...doc, __file: path.basename(file) });
  } catch {
    invalidFiles.push({ file: path.basename(file), reason: "invalid_json" });
  }
}

const median = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  const m = s.length >> 1;
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
};

const percentile = (xs, p) => {
  if (xs.length === 0) return undefined;
  const sorted = [...xs].sort((a, b) => a - b);
  if (sorted.length === 1) return sorted[0];
  const position = (sorted.length - 1) * p;
  const lower = Math.floor(position);
  const upper = Math.ceil(position);
  const weight = position - lower;
  return sorted[lower] * (1 - weight) + sorted[upper] * weight;
};

const finiteNumber = (value) => {
  if (typeof value === "number") return Number.isFinite(value) ? value : null;
  if (typeof value !== "string" || value.trim() === "") return null;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
};

// IC95 de la mediana por bootstrap; determinista para que el mismo input
// produzca el mismo reporte.
const bootstrapCI = (xs, iters = 2000) => {
  if (xs.length < 2) return [xs[0] ?? 0, xs[0] ?? 0];
  let seed = 0x5eed;
  const rnd = () => {
    seed = (seed * 1103515245 + 12345) & 0x7fffffff;
    return seed / 0x7fffffff;
  };
  const meds = [];
  for (let i = 0; i < iters; i++) {
    const sample = Array.from({ length: xs.length }, () => xs[(rnd() * xs.length) | 0]);
    meds.push(median(sample));
  }
  meds.sort((a, b) => a - b);
  return [meds[(iters * 0.025) | 0], meds[(iters * 0.975) | 0]];
};

const firstValue = (doc, keys, fallback = "unknown") => {
  const meta = doc.meta && typeof doc.meta === "object" ? doc.meta : {};
  for (const key of keys) {
    const value = meta[key] ?? doc[key];
    if (value !== undefined && value !== null && String(value).trim() !== "") {
      return String(value).trim();
    }
  }
  return fallback;
};

const rawValue = (doc, keys) => {
  const meta = doc.meta && typeof doc.meta === "object" ? doc.meta : {};
  for (const key of keys) {
    if (meta[key] !== undefined && meta[key] !== null) return meta[key];
    if (doc[key] !== undefined && doc[key] !== null) return doc[key];
  }
  return undefined;
};

// Las trazas antiguas no declaraban estado de caché. Sólo se separa cuando la
// señal existe de forma explícita; nunca se infiere "cold" por orden de archivo.
const cacheState = (doc) => {
  const cold = rawValue(doc, ["cold_cache", "coldCache"]);
  if (typeof cold === "boolean") return cold ? "cold" : "warm";
  const warm = rawValue(doc, ["warm_cache", "warmCache"]);
  if (typeof warm === "boolean") return warm ? "warm" : "cold";
  const state = rawValue(doc, [
    "cache_state",
    "cacheState",
    "cache_mode",
    "cacheMode",
    "run_type",
    "runType",
  ]);
  if (state === undefined) return null;
  const normalized = String(state).trim().toLowerCase();
  if (normalized.includes("cold")) return "cold";
  if (normalized.includes("warm")) return "warm";
  return null;
};

const traceIdentity = (doc) => {
  const computeBackend = firstValue(
    doc,
    ["compute_backend", "computeBackend", "backend", "engine"],
    "unknown"
  );
  const decodeBackend = firstValue(
    doc,
    ["decode_backend", "decodeBackend", "decode_route", "decodeRoute"],
    "unknown"
  );
  const backend = computeBackend === "unknown" && decodeBackend === "unknown"
    ? "unknown"
    : `compute:${computeBackend}|decode:${decodeBackend}`;
  return {
    commit: firstValue(doc, ["commit", "git_commit", "gitCommit", "revision"]),
    dirty: firstValue(doc, ["dirty", "worktree_dirty", "worktreeDirty"], "unknown"),
    worktreeFingerprint: firstValue(
      doc,
      ["worktree_fingerprint", "worktreeFingerprint"],
      "unknown"
    ),
    source: firstValue(doc, ["source"], "unknown"),
    mode: firstValue(doc, ["compute_policy", "computePolicy", "mode", "gpu_mode", "gpuMode"]),
    decodePolicy: firstValue(doc, ["decode_policy", "decodePolicy"], "unknown"),
    qualityPolicy: firstValue(doc, ["quality_policy", "qualityPolicy"], "adaptive"),
    backend,
    device: firstValue(
      doc,
      ["device", "device_name", "deviceName", "adapter", "adapter_name", "adapterName", "gpu_name", "host"]
    ),
  };
};

const identityKey = (identity) => createHash("sha256")
  .update(JSON.stringify(identity))
  .digest("hex")
  .slice(0, 16);

const failureReason = (trace) => {
  if (trace.completed === true && finiteNumber(trace.totalMs) === null) return "invalid_total";
  return firstValue(trace, ["failure_reason", "failureReason", "error"], "incomplete");
};

const rawGroups = new Map();
for (const trace of traces) {
  const identity = traceIdentity(trace);
  const key = identityKey(identity);
  if (!rawGroups.has(key)) rawGroups.set(key, { identity, jobs: new Map() });
  const group = rawGroups.get(key);
  const job = firstValue(trace, ["job"], "unknown");
  if (!group.jobs.has(job)) group.jobs.set(job, { attempts: [], completed: [], failures: [] });
  const bucket = group.jobs.get(job);
  bucket.attempts.push(trace);
  if (trace.completed === true && finiteNumber(trace.totalMs) !== null) {
    bucket.completed.push(trace);
  } else {
    bucket.failures.push({ file: trace.__file, reason: failureReason(trace) });
  }
}

const summarizeTimings = (totals) => {
  if (totals.length === 0) return null;
  const [lo, hi] = bootstrapCI(totals);
  return {
    runs: totals.length,
    medianMs: +median(totals).toFixed(1),
    p95Ms: +percentile(totals, 0.95).toFixed(1),
    minMs: +Math.min(...totals).toFixed(1),
    maxMs: +Math.max(...totals).toFixed(1),
    ci95Ms: [+lo.toFixed(1), +hi.toFixed(1)],
  };
};

const summarizeJob = (bucket) => {
  const totals = bucket.completed.map((run) => finiteNumber(run.totalMs));
  const phases = {};
  for (const run of bucket.completed) {
    for (const phase of run.phases ?? []) {
      const value = finiteNumber(phase?.totalMs);
      if (!phase?.phase || value === null) continue;
      (phases[phase.phase] ??= []).push(value);
    }
  }
  const phaseStats = Object.fromEntries(
    Object.entries(phases)
      .map(([name, values]) => [name, {
        medianMs: +median(values).toFixed(1),
        p95Ms: +percentile(values, 0.95).toFixed(1),
        runs: values.length,
      }])
      .sort((a, b) => b[1].medianMs - a[1].medianMs)
  );
  const stats = {
    attempts: bucket.attempts.length,
    runs: bucket.completed.length,
    completed: bucket.completed.length,
    failed: bucket.failures.length,
    completionRate: bucket.attempts.length
      ? +(bucket.completed.length / bucket.attempts.length).toFixed(4)
      : 0,
    failures: bucket.failures,
    phases: phaseStats,
  };

  const overall = summarizeTimings(totals);
  if (overall) Object.assign(stats, overall);

  const cacheStates = {};
  for (const state of ["cold", "warm"]) {
    const stateTotals = bucket.completed
      .filter((run) => cacheState(run) === state)
      .map((run) => finiteNumber(run.totalMs));
    const timing = summarizeTimings(stateTotals);
    if (timing) cacheStates[state] = timing;
  }
  if (Object.keys(cacheStates).length > 0) stats.cacheStates = cacheStates;
  return stats;
};

const report = {
  schema: "zas-planetary-e2e-report-v3",
  inputSchema: "zas-perf-trace-v1",
  dir,
  generatedAt: new Date().toISOString(),
  summary: {
    traceFiles: traces.length + invalidFiles.length,
    validTraceFiles: traces.length,
    attempts: traces.length + invalidFiles.length,
    completed: 0,
    failed: invalidFiles.length,
    invalidFiles,
    ignoredFiles,
  },
  groups: {},
};

for (const [key, raw] of [...rawGroups.entries()].sort(([a], [b]) => a.localeCompare(b))) {
  const jobs = {};
  for (const [job, bucket] of [...raw.jobs.entries()].sort(([a], [b]) => a.localeCompare(b))) {
    const stats = summarizeJob(bucket);
    jobs[job] = stats;
    report.summary.completed += stats.completed;
    report.summary.failed += stats.failed;
  }
  report.groups[key] = { identity: raw.identity, jobs };
}
report.summary.completionRate = report.summary.attempts
  ? +(report.summary.completed / report.summary.attempts).toFixed(4)
  : 0;

// Regret de Auto respecto al modo manual seguro más rápido dentro de la misma
// celda (commit/fuente/decode/equipo/job). Backend y modo se excluyen a
// propósito: son precisamente las decisiones que Auto debe competir.
const comparisonCells = new Map();
for (const [groupKey, group] of Object.entries(report.groups)) {
  for (const [job, stats] of Object.entries(group.jobs)) {
    if (!Number.isFinite(stats.medianMs)) continue;
    const cellIdentity = {
      commit: group.identity.commit,
      dirty: group.identity.dirty,
      source: group.identity.source,
      decodePolicy: group.identity.decodePolicy,
      qualityPolicy: group.identity.qualityPolicy,
      device: group.identity.device,
      job,
    };
    const cellKey = identityKey(cellIdentity);
    if (!comparisonCells.has(cellKey)) {
      comparisonCells.set(cellKey, { identity: cellIdentity, entries: [] });
    }
    comparisonCells.get(cellKey).entries.push({
      groupKey,
      mode: String(group.identity.mode).toLowerCase(),
      backend: group.identity.backend,
      medianMs: stats.medianMs,
      p95Ms: stats.p95Ms,
      runs: stats.runs,
    });
  }
}

const autoRegretCells = [];
for (const [cellKey, cell] of comparisonCells) {
  const autos = cell.entries.filter((entry) => entry.mode === "auto");
  const manuals = cell.entries.filter((entry) => entry.mode !== "auto");
  if (autos.length === 0 || manuals.length === 0) continue;
  const auto = autos.reduce((best, entry) => entry.medianMs < best.medianMs ? entry : best);
  const manual = manuals.reduce((best, entry) => entry.medianMs < best.medianMs ? entry : best);
  autoRegretCells.push({
    cellKey,
    identity: cell.identity,
    auto,
    fastestManual: manual,
    regretPercent: +((auto.medianMs / manual.medianMs - 1) * 100).toFixed(2),
    withinTenPercent: auto.medianMs <= manual.medianMs * 1.10,
  });
}
const regretValues = autoRegretCells.map((cell) => cell.regretPercent);
report.autoRegret = {
  cells: autoRegretCells,
  comparedCells: autoRegretCells.length,
  medianPercent: regretValues.length ? +median(regretValues).toFixed(2) : null,
  maxPercent: regretValues.length ? +Math.max(...regretValues).toFixed(2) : null,
  withinFinalGate: regretValues.length > 0
    && autoRegretCells.every((cell) => cell.withinTenPercent)
    && median(regretValues) <= 5,
};
report.summary.cellsBelowFiveRuns = Object.entries(report.groups).flatMap(([groupKey, group]) =>
  Object.entries(group.jobs)
    .filter(([, stats]) => stats.completed < 5)
    .map(([job, stats]) => ({ groupKey, job, completed: stats.completed }))
);

// Compatibilidad de salida para sesiones antiguas de una sola identidad. No se
// crea un rollup cuando hay varias identidades porque volveria a mezclar datos.
const groupValues = Object.values(report.groups);
if (groupValues.length === 1) report.jobs = groupValues[0].jobs;

const fmt = (ms) => (ms >= 60000 ? `${(ms / 60000).toFixed(2)} min` : `${(ms / 1000).toFixed(2)} s`);
console.log(
  `\n== ${report.summary.attempts} intentos: ${report.summary.completed} completos, ${report.summary.failed} fallidos/incompletos ==`
);
for (const [key, group] of Object.entries(report.groups)) {
  const id = group.identity;
  console.log(`\n-- grupo ${key} --`);
  console.log(
    `commit=${id.commit}  dirty=${id.dirty}  compute=${id.mode}  decode=${id.decodePolicy}  quality=${id.qualityPolicy}  backend=${id.backend}  device=${id.device}`
  );
  console.log(`source=${id.source}`);
  for (const [job, stats] of Object.entries(group.jobs)) {
    console.log(`\n== ${job} — ${stats.completed}/${stats.attempts} completas ==`);
    if (stats.failed > 0) console.log(`fallidas/incompletas: ${stats.failed}`);
    if (!Number.isFinite(stats.medianMs)) continue;
    console.log(
      `total: mediana ${fmt(stats.medianMs)}  p95 ${fmt(stats.p95Ms)}  [IC95 ${fmt(stats.ci95Ms[0])} .. ${fmt(stats.ci95Ms[1])}]  (min ${fmt(stats.minMs)}, max ${fmt(stats.maxMs)})`
    );
    for (const [state, stateStats] of Object.entries(stats.cacheStates ?? {})) {
      console.log(
        `  ${state.padEnd(6)} mediana ${fmt(stateStats.medianMs)}  p95 ${fmt(stateStats.p95Ms)}  (${stateStats.runs} corridas)`
      );
    }
    console.log("fases (mediana):");
    for (const [phase, phaseStats] of Object.entries(stats.phases)) {
      console.log(
        `  ${phase.padEnd(24)} ${fmt(phaseStats.medianMs).padStart(10)}  p95 ${fmt(phaseStats.p95Ms).padStart(10)}`
      );
    }
  }
}
if (report.autoRegret.comparedCells > 0) {
  console.log(
    `\nAuto regret: mediana ${report.autoRegret.medianPercent.toFixed(2)}%, máximo ${report.autoRegret.maxPercent.toFixed(2)}% (${report.autoRegret.comparedCells} celdas; gate=${report.autoRegret.withinFinalGate ? "OK" : "NO"})`
  );
}

const baselineJob = (base, key, job) => {
  if (base.groups?.[key]?.jobs?.[job]) return base.groups[key].jobs[job];
  // Sólo el esquema v1 carecía de identidad. `jobs` también existe como alias
  // de conveniencia en v2, pero nunca debe actuar como wildcard entre equipos.
  if (base.schema === "zas-planetary-e2e-report-v1") return base.jobs?.[job] ?? null;
  return null;
};

if (baselinePath && fs.existsSync(baselinePath)) {
  const base = JSON.parse(fs.readFileSync(baselinePath, "utf8"));
  console.log(`\n== comparacion vs baseline (${path.basename(baselinePath)}) ==`);
  if (base.schema === "zas-planetary-e2e-report-v1") {
    console.log("baseline v1 sin identidad: comparacion wildcard por job");
  }
  for (const [key, group] of Object.entries(report.groups)) {
    for (const [job, stats] of Object.entries(group.jobs)) {
      const previous = baselineJob(base, key, job);
      if (!previous || !Number.isFinite(previous.medianMs) || !Number.isFinite(stats.medianMs)) continue;
      const speedup = previous.medianMs / stats.medianMs;
      const direction = speedup >= 1 ? "mas rapido" : "MAS LENTO";
      console.log(
        `${key}/${job}: ${speedup.toFixed(2)}x ${direction} (baseline ${fmt(previous.medianMs)} -> ahora ${fmt(stats.medianMs)})`
      );
      for (const [phase, phaseStats] of Object.entries(stats.phases)) {
        const oldPhase = previous.phases?.[phase];
        if (!oldPhase || oldPhase.medianMs < 100) continue;
        const phaseSpeedup = oldPhase.medianMs / phaseStats.medianMs;
        if (Math.abs(phaseSpeedup - 1) > 0.1) {
          console.log(
            `  ${phase}: ${phaseSpeedup.toFixed(2)}x (${fmt(oldPhase.medianMs)} -> ${fmt(phaseStats.medianMs)})`
          );
        }
      }
    }
  }
}

if (outPath) {
  fs.writeFileSync(outPath, JSON.stringify(report, null, 2));
  console.log(`\nreporte: ${outPath}`);
}
if (
  saveBaselinePath
  && report.summary.completed > 0
  && (report.summary.failed === 0 || allowIncomplete)
) {
  fs.mkdirSync(path.dirname(saveBaselinePath), { recursive: true });
  fs.writeFileSync(saveBaselinePath, JSON.stringify(report, null, 2));
  console.log(`baseline guardado: ${saveBaselinePath}`);
}

if (report.summary.completed === 0) {
  console.error(`Sin trazas completas en ${dir}; consulta summary.failed y groups[].jobs[].failures.`);
  process.exitCode = 1;
} else if (report.summary.failed > 0 && !allowIncomplete) {
  console.error(
    `La sesión contiene ${report.summary.failed} intento(s) fallido(s), incompleto(s) o corrupto(s). Usa --allow-incomplete sólo si aceptas explícitamente ese reporte parcial.`
  );
  process.exitCode = 1;
}
if (requireFiveRuns && report.summary.cellsBelowFiveRuns.length > 0) {
  console.error(
    `${report.summary.cellsBelowFiveRuns.length} celda(s) no alcanzan cinco corridas completas.`
  );
  process.exitCode = 1;
}
if (enforceAutoRegret && !report.autoRegret.withinFinalGate) {
  console.error("Auto no satisface regret <=10% por celda y <=5% mediano, o faltan comparaciones manuales.");
  process.exitCode = 1;
}
