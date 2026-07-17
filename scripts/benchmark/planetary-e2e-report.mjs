#!/usr/bin/env node
// Agregador del benchmark E2E planetario: lee los JSON de perf_trace
// (schema zas-perf-trace-v1) de un directorio de sesión, agrupa por tipo de
// job (analysis / stack) y reporta mediana, min/max e IC95 bootstrap del
// tiempo total, más la mediana por fase. Compara contra un baseline opcional.
//
// Uso: node planetary-e2e-report.mjs <dir-trazas> [--out reporte.json]
//        [--baseline benchmarks/baselines/planetary-timing-<host>.json]
//        [--save-baseline <ruta.json>]
import fs from "node:fs";
import path from "node:path";

const args = process.argv.slice(2);
if (args.length < 1) {
  console.error(
    "uso: planetary-e2e-report.mjs <dir-trazas> [--out r.json] [--baseline b.json] [--save-baseline b.json]"
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

const files = fs
  .readdirSync(dir)
  .filter((f) => f.endsWith(".json"))
  .map((f) => path.join(dir, f));

const runs = [];
for (const f of files) {
  try {
    const doc = JSON.parse(fs.readFileSync(f, "utf8"));
    if (doc.schema === "zas-perf-trace-v1" && doc.completed === true) {
      runs.push(doc);
    }
  } catch {
    /* fichero ajeno o truncado: ignorar */
  }
}
if (runs.length === 0) {
  console.error(`Sin trazas completas en ${dir} (¿corriste la app con ZAS_PERF_TRACE_DIR?)`);
  process.exit(1);
}

const median = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  const m = s.length >> 1;
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
};
// IC95 de la MEDIANA por bootstrap (suficiente para 5-20 reps; determinista).
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

const byJob = {};
for (const r of runs) (byJob[r.job] ??= []).push(r);

const report = { schema: "zas-planetary-e2e-report-v1", dir, generatedAt: new Date().toISOString(), jobs: {} };
for (const [job, list] of Object.entries(byJob)) {
  const totals = list.map((r) => r.totalMs);
  const [lo, hi] = bootstrapCI(totals);
  const phases = {};
  for (const r of list) {
    for (const p of r.phases ?? []) (phases[p.phase] ??= []).push(p.totalMs);
  }
  const phaseStats = Object.fromEntries(
    Object.entries(phases)
      .map(([k, v]) => [k, { medianMs: +median(v).toFixed(1), runs: v.length }])
      .sort((a, b) => b[1].medianMs - a[1].medianMs)
  );
  report.jobs[job] = {
    runs: list.length,
    medianMs: +median(totals).toFixed(1),
    minMs: +Math.min(...totals).toFixed(1),
    maxMs: +Math.max(...totals).toFixed(1),
    ci95Ms: [+lo.toFixed(1), +hi.toFixed(1)],
    sources: [...new Set(list.map((r) => r.source))],
    phases: phaseStats,
  };
}

const fmt = (ms) => (ms >= 60000 ? `${(ms / 60000).toFixed(2)} min` : `${(ms / 1000).toFixed(2)} s`);
for (const [job, j] of Object.entries(report.jobs)) {
  console.log(`\n== ${job} — ${j.runs} runs ==`);
  console.log(`total: mediana ${fmt(j.medianMs)}  [IC95 ${fmt(j.ci95Ms[0])} .. ${fmt(j.ci95Ms[1])}]  (min ${fmt(j.minMs)}, max ${fmt(j.maxMs)})`);
  console.log(`fases (mediana):`);
  for (const [ph, st] of Object.entries(j.phases)) {
    console.log(`  ${ph.padEnd(24)} ${fmt(st.medianMs).padStart(10)}`);
  }
}

if (baselinePath && fs.existsSync(baselinePath)) {
  const base = JSON.parse(fs.readFileSync(baselinePath, "utf8"));
  console.log(`\n== comparación vs baseline (${path.basename(baselinePath)}) ==`);
  for (const [job, j] of Object.entries(report.jobs)) {
    const b = base.jobs?.[job];
    if (!b) continue;
    const speedup = b.medianMs / j.medianMs;
    const dir = speedup >= 1 ? "más rápido" : "MÁS LENTO";
    console.log(`${job}: ${speedup.toFixed(2)}x ${dir} (baseline ${fmt(b.medianMs)} → ahora ${fmt(j.medianMs)})`);
    for (const [ph, st] of Object.entries(j.phases)) {
      const bp = b.phases?.[ph];
      if (!bp || bp.medianMs < 100) continue;
      const s = bp.medianMs / st.medianMs;
      if (Math.abs(s - 1) > 0.1) {
        console.log(`  ${ph}: ${s.toFixed(2)}x (${fmt(bp.medianMs)} → ${fmt(st.medianMs)})`);
      }
    }
  }
}

if (outPath) {
  fs.writeFileSync(outPath, JSON.stringify(report, null, 2));
  console.log(`\nreporte: ${outPath}`);
}
if (saveBaselinePath) {
  fs.mkdirSync(path.dirname(saveBaselinePath), { recursive: true });
  fs.writeFileSync(saveBaselinePath, JSON.stringify(report, null, 2));
  console.log(`baseline guardado: ${saveBaselinePath}`);
}
