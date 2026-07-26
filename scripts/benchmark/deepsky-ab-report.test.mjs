import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

const repoRoot = path.resolve(import.meta.dirname, "../..");
const script = path.join(repoRoot, "scripts/benchmark/deepsky-ab-report.mjs");
const matrix = path.join(repoRoot, "benchmarks/dataset-matrix.json");
const matrixDocument = JSON.parse(fs.readFileSync(matrix, "utf8"));
const deepSkyScenarioIds = matrixDocument.requiredScenarios
  .filter((scenario) => scenario.domain === "deep_sky")
  .map((scenario) => scenario.id);
const hash = (value) => createHash("sha256").update(value).digest("hex");

const baseMetrics = (stacker, superior = true) => {
  const zenith = superior && stacker === "Zenith";
  return {
    flatResidualPercent: 0.2,
    darkPatternResidualPercent: 0.4,
    syntheticPhotometryBiasPercent: 0.2,
    realPhotometryBiasPercent: 0.4,
    registrationP95Px: 0.1,
    eidrGeometryP95Px: 0.01,
    varianceCoverageErrorPoints: 2.0,
    tileSeamSigma: 0.1,
    readAmplification: 1.2,
    readAmplificationHard: 1.8,
    rssFraction: 0.5,
    gpuReadbackFraction: 0.05,
    coverageNoSignalViolations: 0,
    artificialFrequencyViolations: 0,
    negativeJacobianViolations: 0,
    allocationFailureCount: 0,
    gpuOffered: 1,
    gpuSpeedup: 1.4,
    snr: zenith ? 110 : 100,
    fwhmPx: 2.0,
    backgroundRms: 8.0,
    photometryBiasPercent: 0.2,
    artifactScore: 0.1,
  };
};

function scenarioClass(scenarioId) {
  if (scenarioId.includes("dualband")) return "dualBandOsc";
  if (scenarioId.includes("mono-sho") || scenarioId.includes("eidr") || scenarioId.includes("nebulafusion")) {
    return "monoNarrowband";
  }
  if (scenarioId.includes("broadband-mono") || scenarioId.includes("mono-multisession")) {
    return "broadbandMono";
  }
  return "broadbandOsc";
}

function datasetDefinitions(omittedScenarioId = null) {
  const definitions = new Map();
  for (const captureClass of ["broadbandOsc", "broadbandMono", "dualBandOsc", "monoNarrowband"]) {
    definitions.set(captureClass, {
      datasetId: "synthetic-" + captureClass,
      captureClass,
      scenarioIds: [],
    });
  }
  for (const scenarioId of deepSkyScenarioIds) {
    if (scenarioId !== omittedScenarioId) definitions.get(scenarioClass(scenarioId)).scenarioIds.push(scenarioId);
  }
  return [...definitions.values()];
}

function writeArtifacts(directory) {
  const artifactDirectory = path.join(directory, "artifacts");
  fs.mkdirSync(artifactDirectory);
  const contents = {
    "master.fits": "linear science master",
    "variance.fits": "variance plane",
    "dq.fits": "data quality plane",
    "recipe.json": "{\"schema\":\"test-recipe\"}",
    "run.log": "deterministic stack log",
  };
  const artifacts = {};
  for (const [name, content] of Object.entries(contents)) {
    fs.writeFileSync(path.join(artifactDirectory, name), content);
    artifacts[name] = { path: path.join("artifacts", name), sha256: hash(content) };
  }
  return artifacts;
}

function writeCorpus(overrides = () => ({}), options = {}) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "zas-deepsky-ab-"));
  const artifacts = writeArtifacts(directory);
  let sequence = 0;
  for (const dataset of datasetDefinitions(options.omittedScenarioId)) {
    for (const stacker of ["Zenith", "WBPP", "DSS", "APP"]) {
      for (const cacheState of ["cold", "warm"]) {
        for (let runIndex = 1; runIndex <= 5; runIndex++) {
          const run = {
            schema: "zenith-deepsky-ab-run-v2",
            runId: [dataset.datasetId, stacker, cacheState, runIndex].join(":"),
            runIndex,
            datasetId: dataset.datasetId,
            scenarioIds: [...dataset.scenarioIds],
            captureClass: dataset.captureClass,
            rawSetSha256: hash(dataset.datasetId + "|raws"),
            stacker,
            stackerVersion: stacker + "-fixed-version",
            parametersSha256: hash(dataset.datasetId + "|" + stacker + "|parameters"),
            hardwareId: "mac-test-fixed",
            cacheState,
            cropId: "crop-128x128-0-0",
            linearScaleId: "adu-native-1x",
            completed: true,
            elapsedSeconds: 10 + runIndex,
            decisionSha256: hash(dataset.datasetId + "|decision"),
            cfaSha256: hash(dataset.datasetId + "|cfa"),
            scientificMetricsSha256: hash(dataset.datasetId + "|" + stacker + "|science"),
            metrics: baseMetrics(stacker, options.superior !== false),
            outputs: [{ ...artifacts["master.fits"] }],
            evidence: {
              masters: [{ ...artifacts["master.fits"] }],
              variance: { ...artifacts["variance.fits"] },
              dq: { ...artifacts["dq.fits"] },
              recipe: { ...artifacts["recipe.json"] },
              logs: [{ ...artifacts["run.log"] }],
            },
          };
          Object.assign(run, overrides({ ...dataset, stacker, cacheState, runIndex, sequence, run }));
          const name = String(sequence++).padStart(4, "0") + "-" + stacker + "-" + cacheState + ".json";
          fs.writeFileSync(path.join(directory, name), JSON.stringify(run));
        }
      }
    }
  }
  return directory;
}

function execute(directory) {
  const out = path.join(directory, "report.json");
  const result = spawnSync(
    process.execPath,
    [script, directory, "--matrix", matrix, "--out", out],
    { cwd: repoRoot, encoding: "utf8" },
  );
  return {
    ...result,
    report: fs.existsSync(out) ? JSON.parse(fs.readFileSync(out, "utf8")) : null,
  };
}

test("accepts the complete matrix with five cold/warm runs and verified evidence", () => {
  const result = execute(writeCorpus());
  assert.equal(result.status, 0, result.stderr + result.stdout);
  assert.equal(result.report.releaseEligible, true);
  assert.equal(result.report.superiorityEligible, true);
  assert.equal(result.report.inputRunCount, 160);
  assert.deepEqual(result.report.missingDeepSkyScenarioIds, []);
  assert.ok(Object.values(result.report.superiorityByClass).every((value) => value.eligible));
});

test("fails closed when a scientific absolute gate regresses", () => {
  const directory = writeCorpus(({ stacker, runIndex, cacheState, captureClass }) => (
    stacker === "Zenith" && runIndex === 1 && cacheState === "cold" && captureClass === "broadbandOsc"
      ? { metrics: { ...baseMetrics(stacker), registrationP95Px: 0.25 } }
      : {}
  ));
  const result = execute(directory);
  assert.equal(result.status, 1);
  assert.equal(result.report.releaseEligible, false);
  assert.ok(result.report.releaseFailures.some((failure) => failure.includes("registrationP95Px")));
});

test("fails closed instead of ignoring a run with the wrong schema", () => {
  const directory = writeCorpus(({ sequence }) => sequence === 0 ? { schema: "zenith-deepsky-ab-run-v1" } : {});
  const result = execute(directory);
  assert.equal(result.status, 1);
  assert.ok(result.report.validationFailures.some((failure) => failure.includes("schema debe ser")));
});

test("fails closed when a mandatory metric is absent", () => {
  const directory = writeCorpus(({ stacker, sequence }) => {
    if (stacker !== "Zenith" || sequence !== 0) return {};
    const metrics = baseMetrics(stacker);
    delete metrics.flatResidualPercent;
    return { metrics };
  });
  const result = execute(directory);
  assert.equal(result.status, 1);
  assert.ok(result.report.validationFailures.some((failure) => failure.includes("falta métrica obligatoria flatResidualPercent")));
});

test("fails closed when a numeric metric overflows to a non-finite value", () => {
  const directory = writeCorpus();
  const firstRun = fs.readdirSync(directory).filter((name) => /^0000-/.test(name))[0];
  const file = path.join(directory, firstRun);
  const source = fs.readFileSync(file, "utf8").replace('"flatResidualPercent":0.2', '"flatResidualPercent":1e400');
  fs.writeFileSync(file, source);
  const result = execute(directory);
  assert.equal(result.status, 1);
  assert.ok(result.report.validationFailures.some((failure) => failure.includes("métrica no finita flatResidualPercent")));
});

test("fails when the corpus does not cover every deep-sky scenario in dataset-matrix", () => {
  const omittedScenarioId = deepSkyScenarioIds.at(-1);
  const result = execute(writeCorpus(() => ({}), { omittedScenarioId }));
  assert.equal(result.status, 1);
  assert.equal(result.report.releaseEligible, false);
  assert.deepEqual(result.report.missingDeepSkyScenarioIds, [omittedScenarioId]);
  assert.ok(result.report.releaseFailures.some((failure) => failure.includes(omittedScenarioId)));
});

test("fails when parameters diverge between cold and warm for one tool", () => {
  const directory = writeCorpus(({ stacker, cacheState, captureClass }) => (
    stacker === "WBPP" && cacheState === "warm" && captureClass === "dualBandOsc"
      ? { parametersSha256: "f".repeat(64) }
      : {}
  ));
  const result = execute(directory);
  assert.equal(result.status, 1);
  assert.ok(result.report.validationFailures.some((failure) => failure.includes("versión o parámetros cambiaron")));
});

test("allows release without conflating it with a superiority claim", () => {
  const result = execute(writeCorpus(() => ({}), { superior: false }));
  assert.equal(result.status, 0, result.stderr + result.stdout);
  assert.equal(result.report.releaseEligible, true);
  assert.equal(result.report.passed, true);
  assert.equal(result.report.superiorityEligible, false);
  assert.ok(result.report.superiorityFailures.length > 0);
  assert.deepEqual(result.report.failures, []);
});

test("fails closed when required VAR/DQ/recipe/log evidence is missing", () => {
  const directory = writeCorpus(({ sequence, run }) => {
    if (sequence !== 0) return {};
    return { evidence: { ...run.evidence, logs: [] } };
  });
  const result = execute(directory);
  assert.equal(result.status, 1);
  assert.ok(result.report.validationFailures.some((failure) => failure.includes("evidence.logs")));
});

test("fails closed when a content-addressed evidence file is tampered", () => {
  const directory = writeCorpus();
  fs.appendFileSync(path.join(directory, "artifacts", "variance.fits"), "tampered");
  const result = execute(directory);
  assert.equal(result.status, 1);
  assert.ok(result.report.validationFailures.some((failure) => failure.includes("evidence.variance no coincide con SHA-256")));
});

for (const [label, mutation, expected] of [
  ["invalid dataset id", { datasetId: "../outside" }, "datasetId inválido"],
  ["invalid hash", { rawSetSha256: "not-a-sha256" }, "rawSetSha256 no es SHA-256"],
  ["invalid enum", { captureClass: "unknownCapture" }, "captureClass inválida"],
  ["unknown scenario id", { scenarioIds: ["deep-sky-not-in-matrix"] }, "scenarioId desconocido/no deep_sky"],
]) {
  test("fails closed for " + label, () => {
    const directory = writeCorpus(({ sequence }) => sequence === 0 ? mutation : {});
    const result = execute(directory);
    assert.equal(result.status, 1);
    assert.ok(result.report.validationFailures.some((failure) => failure.includes(expected)));
  });
}

test("fails closed when warm-cache Zenith decisions differ", () => {
  const directory = writeCorpus(({ stacker, cacheState, runIndex, captureClass }) => (
    stacker === "Zenith" && cacheState === "warm" && runIndex === 5 && captureClass === "monoNarrowband"
      ? { decisionSha256: "f".repeat(64) }
      : {}
  ));
  const result = execute(directory);
  assert.equal(result.status, 1);
  assert.ok(result.report.validationFailures.some((failure) => failure.includes("decisionSha256")));
});
