#!/usr/bin/env node
// Validador reproducible del A/B de cielo profundo.
//
// El contrato es deliberadamente fail-closed: cualquier JSON desconocido,
// identidad mezclada, evidencia sin verificar o métrica ausente/no finita hace
// que el corpus no sea elegible. La liberación y una afirmación de superioridad
// son gates distintos.

import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const RUN_SCHEMA = "zenith-deepsky-ab-run-v2";
const REPORT_SCHEMA = "zenith-deepsky-ab-report-v2";
const STACKERS = ["Zenith", "WBPP", "DSS", "APP"];
const CACHE_STATES = ["cold", "warm"];
const CAPTURE_CLASSES = ["broadbandOsc", "broadbandMono", "dualBandOsc", "monoNarrowband"];
const HASH_PATTERN = /^[0-9a-f]{64}$/i;
const ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,255}$/;
const REQUIRED_METRICS = [
  "flatResidualPercent",
  "darkPatternResidualPercent",
  "syntheticPhotometryBiasPercent",
  "realPhotometryBiasPercent",
  "registrationP95Px",
  "eidrGeometryP95Px",
  "varianceCoverageErrorPoints",
  "tileSeamSigma",
  "readAmplification",
  "readAmplificationHard",
  "rssFraction",
  "gpuReadbackFraction",
  "coverageNoSignalViolations",
  "artificialFrequencyViolations",
  "negativeJacobianViolations",
  "allocationFailureCount",
  "gpuOffered",
  "gpuSpeedup",
  "snr",
  "fwhmPx",
  "backgroundRms",
  "photometryBiasPercent",
  "artifactScore",
];
const PRIMARY_DEFINITIONS = {
  snr: "higher",
  fwhmPx: "lower",
  backgroundRms: "lower",
  photometryBiasPercent: "lower",
  artifactScore: "lower",
};
const TOP_LEVEL_FIELDS = new Set([
  "schema", "runId", "runIndex", "datasetId", "scenarioIds", "captureClass",
  "rawSetSha256", "stacker", "stackerVersion", "parametersSha256", "hardwareId",
  "cacheState", "cropId", "linearScaleId", "completed", "elapsedSeconds",
  "decisionSha256", "cfaSha256", "scientificMetricsSha256", "metrics", "outputs",
  "evidence",
]);

function usage(message = null) {
  if (message) console.error(message);
  console.error("uso: deepsky-ab-report.mjs <dir-runs> [--out reporte.json] [--matrix benchmarks/dataset-matrix.json]");
  process.exit(2);
}

function parseArguments(argv) {
  if (argv.length < 1 || argv[0].startsWith("--")) usage();
  const parsed = { inputDir: path.resolve(argv[0]), outPath: null, matrixPath: path.resolve("benchmarks/dataset-matrix.json") };
  for (let index = 1; index < argv.length; index += 2) {
    const option = argv[index];
    const value = argv[index + 1];
    if (!value || value.startsWith("--")) usage("falta valor para " + option);
    if (option === "--out") parsed.outPath = path.resolve(value);
    else if (option === "--matrix") parsed.matrixPath = path.resolve(value);
    else usage("opción desconocida: " + option);
  }
  return parsed;
}

const { inputDir, outPath, matrixPath } = parseArguments(process.argv.slice(2));

const sha256File = (file) => createHash("sha256").update(fs.readFileSync(file)).digest("hex");
const canonical = (value) => {
  if (Array.isArray(value)) return "[" + value.map(canonical).join(",") + "]";
  if (value && typeof value === "object") {
    return "{" + Object.keys(value).sort().map((key) => JSON.stringify(key) + ":" + canonical(value[key])).join(",") + "}";
  }
  return JSON.stringify(value);
};
const sha256Value = (value) => createHash("sha256").update(canonical(value)).digest("hex");
const finite = (value) => typeof value === "number" && Number.isFinite(value);
const median = (values) => {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = sorted.length >> 1;
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
};
const percentile = (values, p) => {
  const sorted = [...values].sort((a, b) => a - b);
  const position = (sorted.length - 1) * p;
  const low = Math.floor(position);
  const high = Math.ceil(position);
  return sorted[low] * (high - position) + sorted[high] * (position - low);
};
const bootstrapImprovement = (zenith, competitor, direction, iterations = 4000) => {
  let seed = 0x51e1ab;
  const random = () => {
    seed = (seed * 1664525 + 1013904223) >>> 0;
    return seed / 0x100000000;
  };
  const changes = [];
  for (let iteration = 0; iteration < iterations; iteration++) {
    const z = median(Array.from({ length: zenith.length }, () => zenith[(random() * zenith.length) | 0]));
    const c = median(Array.from({ length: competitor.length }, () => competitor[(random() * competitor.length) | 0]));
    const denominator = Math.max(Math.abs(c), 1.0e-12);
    const improvement = direction === "lower" ? (c - z) / denominator : (z - c) / denominator;
    changes.push(100 * improvement);
  }
  return {
    medianPercent: median(changes),
    ci95Percent: [percentile(changes, 0.025), percentile(changes, 0.975)],
  };
};

function loadMatrix(file) {
  let matrix;
  try {
    matrix = JSON.parse(fs.readFileSync(file, "utf8"));
  } catch (error) {
    throw new Error("dataset-matrix inválida: " + error.message);
  }
  if (matrix?.schemaVersion !== "zenith-dataset-matrix-v3") {
    throw new Error("dataset-matrix: schemaVersion debe ser zenith-dataset-matrix-v3");
  }
  const policy = matrix?.claimPolicy?.deepSkyScientific;
  if (!policy || typeof policy !== "object") {
    throw new Error("dataset-matrix sin claimPolicy.deepSkyScientific");
  }
  const policyKeys = [
    "maxFlatResidualPercent", "maxDarkPatternResidualPercent",
    "maxSyntheticPhotometryBiasPercent", "maxRealPhotometryBiasPercent",
    "maxRegistrationP95Px", "maxEidrGeometryP95Px",
    "maxVarianceCoverageErrorPoints", "maxTileSeamSigma", "maxReadAmplification",
    "maxReadAmplificationHard", "maxRamFraction", "minGpuSpeedup",
    "maxGpuReadbackFraction",
  ];
  for (const key of policyKeys) {
    if (!finite(policy[key]) || policy[key] < 0) {
      throw new Error("dataset-matrix: política inválida " + key);
    }
  }
  if (!Array.isArray(matrix.requiredScenarios)) {
    throw new Error("dataset-matrix sin requiredScenarios");
  }
  const ids = new Set();
  const deepSkyIds = [];
  for (const [index, scenario] of matrix.requiredScenarios.entries()) {
    if (!scenario || typeof scenario !== "object" || !ID_PATTERN.test(scenario.id ?? "")) {
      throw new Error("dataset-matrix: escenario inválido en índice " + index);
    }
    if (ids.has(scenario.id)) throw new Error("dataset-matrix: id duplicado " + scenario.id);
    ids.add(scenario.id);
    if (scenario.domain === "deep_sky") deepSkyIds.push(scenario.id);
  }
  if (deepSkyIds.length === 0) throw new Error("dataset-matrix sin escenarios deep_sky");
  return { matrix, policy, deepSkyIds };
}

let loadedMatrix;
try {
  loadedMatrix = loadMatrix(matrixPath);
} catch (error) {
  console.error(error.message);
  process.exit(2);
}
const { policy, deepSkyIds } = loadedMatrix;
const deepSkyIdSet = new Set(deepSkyIds);

const validationFailures = [];
const releaseFailures = [];
const superiorityFailures = [];
const artifactHashCache = new Map();

function addFailure(file, message) {
  validationFailures.push(path.basename(file) + ": " + message);
}

function validateArtifact(artifact, label, runFile) {
  if (!artifact || typeof artifact !== "object" || Array.isArray(artifact)) {
    addFailure(runFile, label + " debe ser un artefacto");
    return null;
  }
  const keys = Object.keys(artifact);
  if (keys.some((key) => !["path", "sha256"].includes(key))) {
    addFailure(runFile, label + " contiene campos no permitidos");
  }
  if (typeof artifact.path !== "string" || artifact.path.trim() === "" || path.isAbsolute(artifact.path)) {
    addFailure(runFile, label + ".path debe ser una ruta relativa no vacía");
    return null;
  }
  if (!HASH_PATTERN.test(artifact.sha256 ?? "")) {
    addFailure(runFile, label + ".sha256 inválido");
    return null;
  }
  const runDirectory = path.resolve(path.dirname(runFile));
  const artifactPath = path.resolve(runDirectory, artifact.path);
  if (artifactPath !== runDirectory && !artifactPath.startsWith(runDirectory + path.sep)) {
    addFailure(runFile, label + " escapa del directorio de la corrida");
    return null;
  }
  if (!fs.existsSync(artifactPath) || !fs.statSync(artifactPath).isFile()) {
    addFailure(runFile, label + " no existe: " + artifact.path);
    return null;
  }
  const cacheKey = artifactPath + "\0" + artifact.sha256.toLowerCase();
  let matches = artifactHashCache.get(cacheKey);
  if (matches === undefined) {
    matches = sha256File(artifactPath) === artifact.sha256.toLowerCase();
    artifactHashCache.set(cacheKey, matches);
  }
  if (!matches) addFailure(runFile, label + " no coincide con SHA-256: " + artifact.path);
  return { path: artifact.path, sha256: artifact.sha256.toLowerCase() };
}

function validateArtifactList(value, label, runFile) {
  if (!Array.isArray(value) || value.length === 0) {
    addFailure(runFile, label + " debe contener al menos un artefacto");
    return [];
  }
  const artifacts = value.map((artifact, index) => validateArtifact(artifact, label + "[" + index + "]", runFile));
  const validArtifacts = artifacts.filter(Boolean);
  if (new Set(validArtifacts.map((artifact) => artifact.path)).size !== validArtifacts.length) {
    addFailure(runFile, label + " contiene rutas duplicadas");
  }
  return validArtifacts;
}

function validateRun(run, file) {
  const startFailureCount = validationFailures.length;
  if (!run || typeof run !== "object" || Array.isArray(run)) {
    addFailure(file, "la raíz debe ser un objeto");
    return false;
  }
  for (const key of Object.keys(run)) {
    if (!TOP_LEVEL_FIELDS.has(key)) addFailure(file, "campo no permitido " + key);
  }
  if (run.schema !== RUN_SCHEMA) addFailure(file, "schema debe ser " + RUN_SCHEMA);
  if (!ID_PATTERN.test(run.runId ?? "")) addFailure(file, "runId inválido");
  if (!Number.isInteger(run.runIndex) || run.runIndex < 1 || run.runIndex > 5) {
    addFailure(file, "runIndex debe ser entero entre 1 y 5");
  }
  if (!ID_PATTERN.test(run.datasetId ?? "")) addFailure(file, "datasetId inválido");
  if (!Array.isArray(run.scenarioIds) || run.scenarioIds.length === 0) {
    addFailure(file, "scenarioIds ausente o vacío");
  } else {
    const seen = new Set();
    for (const scenarioId of run.scenarioIds) {
      if (typeof scenarioId !== "string" || !deepSkyIdSet.has(scenarioId)) {
        addFailure(file, "scenarioId desconocido/no deep_sky: " + String(scenarioId));
      }
      if (seen.has(scenarioId)) addFailure(file, "scenarioId duplicado: " + scenarioId);
      seen.add(scenarioId);
    }
  }
  if (!CAPTURE_CLASSES.includes(run.captureClass)) addFailure(file, "captureClass inválida");
  if (!STACKERS.includes(run.stacker)) addFailure(file, "stacker inválido");
  if (!CACHE_STATES.includes(run.cacheState)) addFailure(file, "cacheState inválido");
  for (const key of ["stackerVersion", "hardwareId", "cropId", "linearScaleId"]) {
    if (typeof run[key] !== "string" || run[key].trim() === "") addFailure(file, "falta " + key);
  }
  for (const key of [
    "rawSetSha256", "parametersSha256", "decisionSha256", "cfaSha256",
    "scientificMetricsSha256",
  ]) {
    if (!HASH_PATTERN.test(run[key] ?? "")) addFailure(file, key + " no es SHA-256");
  }
  if (run.completed !== true) addFailure(file, "completed debe ser true");
  if (!finite(run.elapsedSeconds) || run.elapsedSeconds <= 0) addFailure(file, "elapsedSeconds inválido");

  if (!run.metrics || typeof run.metrics !== "object" || Array.isArray(run.metrics)) {
    addFailure(file, "metrics ausente");
  } else {
    for (const metric of REQUIRED_METRICS) {
      if (!Object.hasOwn(run.metrics, metric)) addFailure(file, "falta métrica obligatoria " + metric);
    }
    for (const [metric, value] of Object.entries(run.metrics)) {
      if (!finite(value)) addFailure(file, "métrica no finita " + metric);
    }
    for (const metric of REQUIRED_METRICS.filter((key) => key !== "gpuOffered")) {
      if (finite(run.metrics[metric]) && run.metrics[metric] < 0) {
        addFailure(file, "métrica negativa fuera de dominio " + metric);
      }
    }
    if (![0, 1].includes(run.metrics.gpuOffered)) addFailure(file, "gpuOffered debe ser 0 o 1");
    if (finite(run.metrics.rssFraction) && run.metrics.rssFraction > 1) addFailure(file, "rssFraction debe estar entre 0 y 1");
    if (finite(run.metrics.gpuReadbackFraction) && run.metrics.gpuReadbackFraction > 1) {
      addFailure(file, "gpuReadbackFraction debe estar entre 0 y 1");
    }
    for (const metric of ["coverageNoSignalViolations", "artificialFrequencyViolations", "negativeJacobianViolations", "allocationFailureCount"]) {
      if (finite(run.metrics[metric]) && !Number.isInteger(run.metrics[metric])) {
        addFailure(file, metric + " debe ser entero");
      }
    }
  }

  const outputs = validateArtifactList(run.outputs, "outputs", file);
  if (!run.evidence || typeof run.evidence !== "object" || Array.isArray(run.evidence)) {
    addFailure(file, "evidence ausente");
  } else {
    const evidenceKeys = Object.keys(run.evidence);
    if (evidenceKeys.some((key) => !["masters", "variance", "dq", "recipe", "logs"].includes(key))) {
      addFailure(file, "evidence contiene campos no permitidos");
    }
    const masters = validateArtifactList(run.evidence.masters, "evidence.masters", file);
    const variance = validateArtifact(run.evidence.variance, "evidence.variance", file);
    const dq = validateArtifact(run.evidence.dq, "evidence.dq", file);
    const recipe = validateArtifact(run.evidence.recipe, "evidence.recipe", file);
    validateArtifactList(run.evidence.logs, "evidence.logs", file);
    const roleArtifacts = [variance, dq, recipe].filter(Boolean);
    if (new Set(roleArtifacts.map((artifact) => artifact.path)).size !== roleArtifacts.length) {
      addFailure(file, "VAR, DQ y receta deben ser artefactos distintos");
    }
    const outputKeys = new Set(outputs.map((artifact) => artifact.path + "\0" + artifact.sha256));
    if (!masters.some((artifact) => outputKeys.has(artifact.path + "\0" + artifact.sha256))) {
      addFailure(file, "outputs debe vincular al menos un master de evidence.masters");
    }
  }
  return validationFailures.length === startFailureCount;
}

if (!fs.existsSync(inputDir) || !fs.statSync(inputDir).isDirectory()) usage("directorio de corridas inexistente: " + inputDir);
const files = fs.readdirSync(inputDir)
  .filter((name) => name.endsWith(".json"))
  .map((name) => path.join(inputDir, name))
  .filter((file) => !outPath || path.resolve(file) !== outPath)
  .sort();
if (files.length === 0) validationFailures.push("no hay archivos JSON de corrida");

const runs = [];
const inputFiles = [];
for (const file of files) {
  inputFiles.push({ file: path.basename(file), sha256: sha256File(file) });
  let run;
  try {
    run = JSON.parse(fs.readFileSync(file, "utf8"));
  } catch (error) {
    addFailure(file, "JSON inválido: " + error.message);
    continue;
  }
  if (validateRun(run, file)) {
    Object.defineProperty(run, "__file", { value: file, enumerable: false });
    runs.push(run);
  }
}

const duplicateRunIds = new Set();
const runIds = new Set();
for (const run of runs) {
  if (runIds.has(run.runId)) duplicateRunIds.add(run.runId);
  runIds.add(run.runId);
}
for (const runId of duplicateRunIds) validationFailures.push("runId duplicado: " + runId);

const datasetGroups = new Map();
for (const run of runs) {
  if (!datasetGroups.has(run.datasetId)) datasetGroups.set(run.datasetId, []);
  datasetGroups.get(run.datasetId).push(run);
}

const groups = new Map();
for (const run of runs) {
  const key = [run.datasetId, run.captureClass, run.stacker, run.cacheState].join("|");
  if (!groups.has(key)) groups.set(key, []);
  groups.get(key).push(run);
}

for (const [datasetId, datasetRuns] of datasetGroups) {
  const captureSet = new Set(datasetRuns.map((run) => run.captureClass));
  if (captureSet.size !== 1) validationFailures.push(datasetId + ": datasetId mezcla captureClass");
  const captureClass = [...captureSet][0];
  const identities = new Set(datasetRuns.map((run) => canonical({
    rawSetSha256: run.rawSetSha256,
    hardwareId: run.hardwareId,
    cropId: run.cropId,
    linearScaleId: run.linearScaleId,
    scenarioIds: [...run.scenarioIds].sort(),
  })));
  if (identities.size !== 1) {
    validationFailures.push(datasetId + ": cold/warm o herramientas mezclan raws, hardware, crop, escala o escenarios");
  }
  for (const stacker of STACKERS) {
    const toolRuns = datasetRuns.filter((run) => run.stacker === stacker);
    const versions = new Set(toolRuns.map((run) => run.stackerVersion));
    const parameters = new Set(toolRuns.map((run) => run.parametersSha256));
    if (versions.size !== 1 || parameters.size !== 1) {
      validationFailures.push(datasetId + "|" + stacker + ": versión o parámetros cambiaron entre cold/warm");
    }
    for (const cacheState of CACHE_STATES) {
      const key = [datasetId, captureClass, stacker, cacheState].join("|");
      const bucket = groups.get(key) ?? [];
      if (bucket.length !== 5) {
        validationFailures.push(key + ": se requieren exactamente 5 corridas, hay " + bucket.length);
      }
      const indices = new Set(bucket.map((run) => run.runIndex));
      if (bucket.length === 5 && (indices.size !== 5 || ![1, 2, 3, 4, 5].every((index) => indices.has(index)))) {
        validationFailures.push(key + ": runIndex debe cubrir exactamente 1..5");
      }
    }
  }
}

const coveredScenarioIds = new Set(runs.flatMap((run) => run.scenarioIds));
const missingScenarioIds = deepSkyIds.filter((scenarioId) => !coveredScenarioIds.has(scenarioId));
if (missingScenarioIds.length > 0) {
  releaseFailures.push("faltan escenarios deep_sky de dataset-matrix.json: " + missingScenarioIds.join(", "));
}
const captureClasses = [...new Set(runs.map((run) => run.captureClass))].sort();
const missingCaptureClasses = CAPTURE_CLASSES.filter((captureClass) => !captureClasses.includes(captureClass));
if (missingCaptureClasses.length > 0) {
  releaseFailures.push("faltan clases de captura: " + missingCaptureClasses.join(", "));
}
const datasets = [...datasetGroups.keys()].sort();
const zenithRuns = runs.filter((run) => run.stacker === "Zenith");

const absoluteGates = [
  ["flatResidualPercent", policy.maxFlatResidualPercent],
  ["darkPatternResidualPercent", policy.maxDarkPatternResidualPercent],
  ["syntheticPhotometryBiasPercent", policy.maxSyntheticPhotometryBiasPercent],
  ["realPhotometryBiasPercent", policy.maxRealPhotometryBiasPercent],
  ["registrationP95Px", policy.maxRegistrationP95Px],
  ["eidrGeometryP95Px", policy.maxEidrGeometryP95Px],
  ["varianceCoverageErrorPoints", policy.maxVarianceCoverageErrorPoints],
  ["tileSeamSigma", policy.maxTileSeamSigma],
  ["readAmplification", policy.maxReadAmplification],
  ["readAmplificationHard", policy.maxReadAmplificationHard],
  ["rssFraction", policy.maxRamFraction],
  ["gpuReadbackFraction", policy.maxGpuReadbackFraction],
];
const gateResults = [];
for (const [metric, limit] of absoluteGates) {
  const values = zenithRuns.map((run) => run.metrics[metric]);
  const worst = values.length > 0 ? Math.max(...values) : null;
  const passed = values.length === zenithRuns.length && values.length > 0 && worst <= limit;
  gateResults.push({ metric, limit, worst, observedRuns: values.length, passed });
  if (!passed) releaseFailures.push("gate " + metric + ": " + (worst ?? "sin datos") + " > " + limit);
}
for (const metric of [
  "coverageNoSignalViolations", "artificialFrequencyViolations",
  "negativeJacobianViolations", "allocationFailureCount",
]) {
  const values = zenithRuns.map((run) => run.metrics[metric]);
  const worst = values.length > 0 ? Math.max(...values) : null;
  const passed = values.length === zenithRuns.length && values.length > 0 && worst === 0;
  gateResults.push({ metric, limit: 0, worst, observedRuns: values.length, passed });
  if (!passed) releaseFailures.push("gate " + metric + ": debe ser cero");
}
const gpuOffered = zenithRuns.filter((run) => run.metrics.gpuOffered === 1);
const gpuSpeedupWorst = gpuOffered.length > 0 ? Math.min(...gpuOffered.map((run) => run.metrics.gpuSpeedup)) : null;
const gpuSpeedupPassed = gpuOffered.length === 0 || gpuSpeedupWorst >= policy.minGpuSpeedup;
gateResults.push({
  metric: "gpuSpeedupWhenOffered",
  limit: policy.minGpuSpeedup,
  comparison: "minimum",
  worst: gpuSpeedupWorst,
  observedRuns: gpuOffered.length,
  passed: gpuSpeedupPassed,
});
if (!gpuSpeedupPassed) releaseFailures.push("GPU ofrecida sin speedup mínimo " + policy.minGpuSpeedup + "x");

// La caché fría/caliente de Zenith debe conservar decisiones, CFA y métricas
// científicas. elapsedSeconds queda deliberadamente fuera de estos hashes.
for (const [datasetId, datasetRuns] of datasetGroups) {
  const zenithDatasetRuns = datasetRuns.filter((run) => run.stacker === "Zenith");
  for (const key of ["decisionSha256", "cfaSha256", "scientificMetricsSha256"]) {
    const hashes = new Set(zenithDatasetRuns.map((run) => run[key]));
    if (zenithDatasetRuns.length !== 10 || hashes.size !== 1) {
      validationFailures.push(datasetId + ": caché Zenith no equivalente para " + key);
    }
  }
}

const comparisons = [];
for (const [datasetId, datasetRuns] of datasetGroups) {
  const captureClass = datasetRuns[0]?.captureClass;
  const z = datasetRuns.filter((run) => run.stacker === "Zenith");
  for (const competitor of STACKERS.filter((stacker) => stacker !== "Zenith")) {
    const c = datasetRuns.filter((run) => run.stacker === competitor);
    for (const [metric, direction] of Object.entries(PRIMARY_DEFINITIONS)) {
      const zv = z.map((run) => run.metrics[metric]);
      const cv = c.map((run) => run.metrics[metric]);
      if (zv.length === 0 || cv.length === 0) continue;
      const improvement = bootstrapImprovement(zv, cv, direction);
      const regression = improvement.medianPercent < -3.0;
      const superior = improvement.medianPercent >= 5.0 && improvement.ci95Percent[0] > 0.0;
      if (regression) releaseFailures.push(datasetId + "|" + competitor + "|" + metric + ": regresión >3%");
      comparisons.push({ datasetId, captureClass, competitor, metric, direction, ...improvement, regression, superior });
    }
  }
}

const superiorityByClass = {};
for (const captureClass of CAPTURE_CLASSES) {
  const classDatasets = datasets.filter((datasetId) => datasetGroups.get(datasetId)?.[0]?.captureClass === captureClass);
  let winningMetric = null;
  if (classDatasets.length > 0) {
    for (const metric of Object.keys(PRIMARY_DEFINITIONS)) {
      const requiredComparisons = comparisons.filter((comparison) => comparison.captureClass === captureClass && comparison.metric === metric);
      const expectedCount = classDatasets.length * (STACKERS.length - 1);
      if (requiredComparisons.length === expectedCount && requiredComparisons.every((comparison) => comparison.superior)) {
        winningMetric = metric;
        break;
      }
    }
  }
  superiorityByClass[captureClass] = { eligible: winningMetric !== null, winningMetric };
  if (winningMetric === null) {
    superiorityFailures.push(captureClass + ": sin métrica >=5% con IC95 favorable contra todos los competidores y datasets");
  }
}

const releaseEligible = validationFailures.length === 0 && releaseFailures.length === 0;
const superiorityEligible = releaseEligible && superiorityFailures.length === 0;
const failures = [...validationFailures, ...releaseFailures];
const report = {
  schema: REPORT_SCHEMA,
  generatedAtUtc: new Date().toISOString(),
  matrixPath: path.relative(process.cwd(), matrixPath) || path.basename(matrixPath),
  matrixSha256: sha256File(matrixPath),
  inputFileCount: files.length,
  inputRunCount: runs.length,
  inputFingerprint: sha256Value(inputFiles),
  datasets,
  captureClasses,
  requiredDeepSkyScenarioIds: deepSkyIds,
  coveredDeepSkyScenarioIds: [...coveredScenarioIds].sort(),
  missingDeepSkyScenarioIds: missingScenarioIds,
  absoluteGates: gateResults,
  comparisons,
  superiorityByClass,
  validationFailures,
  releaseFailures,
  superiorityFailures,
  releaseEligible,
  superiorityEligible,
  // Alias de compatibilidad: `passed` significa que el corpus puede liberar,
  // no que esté autorizado a proclamar superioridad.
  passed: releaseEligible,
  failures,
};

const serialized = JSON.stringify(report, null, 2) + "\n";
if (outPath) {
  fs.mkdirSync(path.dirname(outPath), { recursive: true });
  fs.writeFileSync(outPath, serialized);
}
process.stdout.write(serialized);
process.exitCode = releaseEligible ? 0 : 1;
