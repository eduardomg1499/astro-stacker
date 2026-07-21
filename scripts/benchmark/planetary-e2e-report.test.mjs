import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { fileURLToPath } from "node:url";

const reporter = fileURLToPath(new URL("./planetary-e2e-report.mjs", import.meta.url));

const fixture = (overrides = {}) => ({
  schema: "zas-perf-trace-v1",
  job: "planetary_surface",
  source: "surface.mov",
  commit: "abc123",
  worktreeFingerprint: "tree-a",
  computePolicy: "auto",
  decodePolicy: "software",
  qualityPolicy: "adaptive",
  computeBackend: "cpu",
  decodeBackend: "ffmpeg-software",
  device: "test-host",
  completed: true,
  totalMs: 100,
  phases: [{ phase: "analysis", totalMs: 10 }],
  ...overrides,
});

const writeJson = (file, value) => writeFileSync(file, JSON.stringify(value));

const runReporter = (args) => spawnSync(process.execPath, [reporter, ...args], {
  encoding: "utf8",
});

test("cuenta JSON corrupto e incompletos, calcula p95 y separa cold/warm", () => {
  const root = mkdtempSync(path.join(tmpdir(), "zas-e2e-report-"));
  const traces = path.join(root, "traces");
  mkdirSync(traces);
  writeJson(path.join(traces, "cold.json"), fixture({ coldCache: true }));
  writeJson(path.join(traces, "warm.json"), fixture({
    coldCache: false,
    totalMs: 200,
    phases: [{ phase: "analysis", totalMs: 30 }],
  }));
  writeJson(path.join(traces, "incomplete.json"), fixture({ completed: false }));
  writeFileSync(path.join(traces, "truncated.json"), "{\"schema\":");

  const reportPath = path.join(root, "report.json");
  const strict = runReporter([traces, "--out", reportPath]);
  assert.equal(strict.status, 1, strict.stderr);

  const report = JSON.parse(readFileSync(reportPath, "utf8"));
  assert.equal(report.summary.attempts, 4);
  assert.equal(report.summary.completed, 2);
  assert.equal(report.summary.failed, 2);
  assert.equal(report.summary.invalidFiles.length, 1);
  assert.equal(report.jobs.planetary_surface.p95Ms, 195);
  assert.equal(report.jobs.planetary_surface.phases.analysis.p95Ms, 29);
  assert.equal(report.jobs.planetary_surface.cacheStates.cold.runs, 1);
  assert.equal(report.jobs.planetary_surface.cacheStates.warm.runs, 1);

  const allowed = runReporter([traces, "--allow-incomplete"]);
  assert.equal(allowed.status, 0, allowed.stderr);
});

test("decodePolicy forma parte de la identidad del grupo", () => {
  const root = mkdtempSync(path.join(tmpdir(), "zas-e2e-report-decode-"));
  const traces = path.join(root, "traces");
  mkdirSync(traces);
  writeJson(path.join(traces, "software.json"), fixture());
  writeJson(path.join(traces, "hardware.json"), fixture({
    decodePolicy: "hardware",
    decodeBackend: "videotoolbox",
  }));
  const reportPath = path.join(root, "report.json");
  const result = runReporter([traces, "--out", reportPath]);
  assert.equal(result.status, 0, result.stderr);
  const report = JSON.parse(readFileSync(reportPath, "utf8"));
  assert.equal(Object.keys(report.groups).length, 2);
  assert.deepEqual(
    Object.values(report.groups).map((group) => group.identity.decodePolicy).sort(),
    ["hardware", "software"]
  );
  assert.equal(report.jobs, undefined);
});

test("qualityPolicy separa cambios científicos de optimizaciones bit-exactas", () => {
  const root = mkdtempSync(path.join(tmpdir(), "zas-e2e-report-quality-"));
  const traces = path.join(root, "traces");
  mkdirSync(traces);
  writeJson(path.join(traces, "adaptive.json"), fixture());
  writeJson(path.join(traces, "maximum.json"), fixture({ qualityPolicy: "maximum" }));
  const reportPath = path.join(root, "report.json");
  const result = runReporter([traces, "--out", reportPath]);
  assert.equal(result.status, 0, result.stderr);
  const report = JSON.parse(readFileSync(reportPath, "utf8"));
  assert.equal(Object.keys(report.groups).length, 2);
  assert.deepEqual(
    Object.values(report.groups).map((group) => group.identity.qualityPolicy).sort(),
    ["adaptive", "maximum"]
  );
});

test("la huella del worktree separa binarios sucios no reproducibles", () => {
  const root = mkdtempSync(path.join(tmpdir(), "zas-e2e-report-worktree-"));
  const traces = path.join(root, "traces");
  mkdirSync(traces);
  writeJson(path.join(traces, "tree-a.json"), fixture());
  writeJson(path.join(traces, "tree-b.json"), fixture({ worktreeFingerprint: "tree-b" }));
  const reportPath = path.join(root, "report.json");
  const result = runReporter([traces, "--out", reportPath]);
  assert.equal(result.status, 0, result.stderr);
  const report = JSON.parse(readFileSync(reportPath, "utf8"));
  assert.equal(Object.keys(report.groups).length, 2);
  assert.deepEqual(
    Object.values(report.groups)
      .map((group) => group.identity.worktreeFingerprint)
      .sort(),
    ["tree-a", "tree-b"]
  );
});

test("el wildcard jobs de baseline se permite sólo para schema v1", () => {
  const root = mkdtempSync(path.join(tmpdir(), "zas-e2e-report-baseline-"));
  const traces = path.join(root, "traces");
  mkdirSync(traces);
  writeJson(path.join(traces, "run.json"), fixture());
  const jobs = { planetary_surface: { medianMs: 1000, phases: {} } };

  const v2 = path.join(root, "baseline-v2.json");
  writeJson(v2, { schema: "zas-planetary-e2e-report-v2", groups: {}, jobs });
  const againstV2 = runReporter([traces, "--baseline", v2]);
  assert.equal(againstV2.status, 0, againstV2.stderr);
  assert.doesNotMatch(againstV2.stdout, /10\.00x/);

  const v1 = path.join(root, "baseline-v1.json");
  writeJson(v1, { schema: "zas-planetary-e2e-report-v1", jobs });
  const againstV1 = runReporter([traces, "--baseline", v1]);
  assert.equal(againstV1.status, 0, againstV1.stderr);
  assert.match(againstV1.stdout, /10\.00x/);
});

test("calcula regret de Auto y puede exigir cinco corridas por celda", () => {
  const root = mkdtempSync(path.join(tmpdir(), "zas-e2e-report-regret-"));
  const traces = path.join(root, "traces");
  mkdirSync(traces);
  for (let i = 0; i < 5; i++) {
    writeJson(path.join(traces, `auto-${i}.json`), fixture({
      computePolicy: "auto",
      totalMs: 105,
    }));
    writeJson(path.join(traces, `cpu-${i}.json`), fixture({
      computePolicy: "cpu",
      totalMs: 100,
    }));
  }
  const reportPath = path.join(root, "report.json");
  const result = runReporter([
    traces,
    "--out",
    reportPath,
    "--require-five-runs",
    "--enforce-auto-regret",
  ]);
  assert.equal(result.status, 0, result.stderr);
  const report = JSON.parse(readFileSync(reportPath, "utf8"));
  assert.equal(report.autoRegret.comparedCells, 1);
  assert.equal(report.autoRegret.medianPercent, 5);
  assert.equal(report.autoRegret.withinFinalGate, true);
  assert.deepEqual(report.summary.cellsBelowFiveRuns, []);
});

test("un fallback conserva la política solicitada como identidad del benchmark", () => {
  const root = mkdtempSync(path.join(tmpdir(), "zas-e2e-report-fallback-"));
  const traces = path.join(root, "traces");
  mkdirSync(traces);
  writeJson(path.join(traces, "auto-to-cpu.json"), fixture({
    computePolicy: undefined,
    decodePolicy: undefined,
    meta: {
      compute_policy: "auto",
      decode_policy: "auto",
      effective_compute_policy: "cpu",
      effective_decode_policy: "software",
      compute_backend: "cpu",
      decode_backend: "ffmpeg-software",
      device: "test-host",
    },
  }));
  const reportPath = path.join(root, "report.json");
  const result = runReporter([traces, "--out", reportPath]);
  assert.equal(result.status, 0, result.stderr);
  const report = JSON.parse(readFileSync(reportPath, "utf8"));
  const [group] = Object.values(report.groups);
  assert.equal(group.identity.mode, "auto");
  assert.equal(group.identity.decodePolicy, "auto");
  assert.match(group.identity.backend, /compute:cpu/);
  assert.match(group.identity.backend, /decode:ffmpeg-software/);
});
