export const MILKY_WAY_RECIPE_SCHEMA = "zenith-milky-way-recipe-v1";

export const MILKY_WAY_STEPS = Object.freeze([
  ["data", "Datos"],
  ["mask", "Cielo y suelo"],
  ["registration", "Registro"],
  ["integration", "Integración"],
  ["composition", "Composición"],
  ["publish", "Editar y exportar"],
]);

export const MILKY_WAY_MODES = Object.freeze({
  SKY_ONLY: "skyOnly",
  FREEZE_GROUND: "freezeGround",
  SEPARATE_LAYERS: "separateLayers",
});

const ROBUST_METHODS = new Set(["winsorized", "sigmaClip"]);
const MASK_STRATEGIES = new Set(["auto", "horizon", "brush", "fullSky"]);
const REGISTRATION_MODELS = new Set(["auto", "affine", "homography", "radialWide"]);

function clone(value) {
  return value == null ? value : JSON.parse(JSON.stringify(value));
}

function finite(value, fallback) {
  if (value == null || value === "") return fallback;
  const number = Number(value);
  return Number.isFinite(number) ? number : fallback;
}

function normalizedFrame(frame, index) {
  if (typeof frame === "string") {
    return { path: frame, name: frame.split(/[\\/]/).pop() || `Toma ${index + 1}` };
  }
  const path = String(frame?.path || frame?.sourcePath || "");
  return {
    path,
    name: String(frame?.name || path.split(/[\\/]/).pop() || `Toma ${index + 1}`),
    width: finite(frame?.width ?? frame?.w, null),
    height: finite(frame?.height ?? frame?.h, null),
    channels: finite(frame?.channels ?? frame?.ch, null),
    exposureSeconds: finite(frame?.exposureSeconds ?? frame?.exptime, null),
    iso: finite(frame?.iso ?? frame?.gain, null),
    timestampUnix: finite(frame?.timestampUnix, null),
    included: frame?.included !== false,
    ok: frame?.ok !== false,
    error: frame?.error ? String(frame.error) : null,
  };
}

function defaultMask() {
  return {
    strategy: "auto",
    confidence: 0,
    userConfirmed: false,
    featherPx: 48,
    horizonPoints: [],
    brushStrokes: [],
    brushTarget: "sky",
    brushRadiusPx: 42,
    protectedForeground: [],
  };
}

function defaultRegistration() {
  return {
    model: "auto",
    minInliers: 18,
    maxRmsPx: 2.2,
    distortionCorrection: "auto",
    singleResample: true,
    skySolved: false,
    groundSolved: false,
    inliers: 0,
    rmsPx: null,
    cornerResidualPx: null,
  };
}

function normalizedRegistration(seed = {}) {
  const registration = { ...defaultRegistration(), ...clone(seed || {}) };
  // `radialWide` always enables the radial warper in the native engine.  An
  // imported legacy recipe may contain the contradictory pair radialWide/off;
  // normalize it here so the visible control and the request cannot disagree.
  if (registration.model === "radialWide" && registration.distortionCorrection === "off") {
    registration.distortionCorrection = "auto";
  }
  return registration;
}

function defaultAnalysis(seed = {}) {
  return {
    frames: Array.isArray(seed?.frames) ? clone(seed.frames) : [],
    warnings: Array.isArray(seed?.warnings) ? seed.warnings.map(String) : [],
    degraded: seed?.degraded === true,
  };
}

export function createMilkyWayState(seed = {}) {
  const lights = (seed.lights || seed.frames || []).map(normalizedFrame);
  const mode = Object.values(MILKY_WAY_MODES).includes(seed.mode)
    ? seed.mode
    : MILKY_WAY_MODES.FREEZE_GROUND;
  const baseFramePath = String(seed.baseFramePath || lights[Math.floor(lights.length / 2)]?.path || "");
  // Coerce every numeric field the UI can write: the native engine expects
  // integers/floats and rejects the whole request over one stringified value.
  const mask = { ...defaultMask(), ...clone(seed.mask || {}) };
  mask.featherPx = Math.round(finite(mask.featherPx, 48));
  mask.brushRadiusPx = Math.round(finite(mask.brushRadiusPx, 42));
  mask.confidence = finite(mask.confidence, 0);
  const integration = {
    profile: "auto",
    method: "winsorized",
    skySigmaLow: 3.2,
    skySigmaHigh: 2.8,
    groundSigmaLow: 3.5,
    groundSigmaHigh: 3.5,
    normalization: "robustLinear",
    dynamicHotPixels: true,
    rejectTrails: true,
    ...clone(seed.integration || {}),
  };
  integration.skySigmaLow = finite(integration.skySigmaLow, 3.2);
  integration.skySigmaHigh = finite(integration.skySigmaHigh, 2.8);
  integration.groundSigmaLow = finite(integration.groundSigmaLow, 3.5);
  integration.groundSigmaHigh = finite(integration.groundSigmaHigh, 3.5);
  const composition = {
    keepSeparateLayers: true,
    groundSource: "stack",
    skySource: "stack",
    featherPx: 48,
    edgeDeghost: 0.65,
    colorMatch: "boundaryAware",
    preserveReflections: true,
    ...clone(seed.composition || {}),
  };
  composition.featherPx = Math.round(finite(composition.featherPx, 48));
  composition.edgeDeghost = finite(composition.edgeDeghost, 0.65);
  return {
    schema: MILKY_WAY_RECIPE_SCHEMA,
    jobId: String(seed.jobId || ""),
    presentationMode: seed.presentationMode === "expert" ? "expert" : "essential",
    activeStep: Math.max(0, Math.min(MILKY_WAY_STEPS.length - 1, finite(seed.activeStep, 0))),
    mode,
    lights,
    darks: (seed.darks || []).map(normalizedFrame),
    flats: (seed.flats || []).map(normalizedFrame),
    foregroundFrames: (seed.foregroundFrames || []).map(normalizedFrame),
    baseFramePath,
    outputDirectory: String(seed.outputDirectory || ""),
    unifyExposure: seed.unifyExposure === true,
    fallbackPolicy: seed.fallbackPolicy === "allowDegraded" ? "allowDegraded" : "strict",
    mask,
    registration: normalizedRegistration(seed.registration),
    analysis: defaultAnalysis(seed.analysis),
    integration,
    composition,
    publish: {
      exportLinearTiff: false,
      exportFitsLayers: true,
      exportMask: true,
      openEditor: true,
      ...clone(seed.publish || {}),
    },
    geometryId: String(seed.geometryId || "milkyway-source-v1"),
    revision: Math.max(0, finite(seed.revision, 0)),
    // Session-only review state is deliberately excluded from the native
    // request below, but it must survive navigation through setMilkyWayStep().
    // Otherwise an error, a synchronized crop, or the published product list
    // disappears merely because the user reviews another page of the wizard.
    runError: seed.runError ? String(seed.runError) : "",
    crop: seed.crop ? clone(seed.crop) : null,
    products: Array.isArray(seed.products) ? clone(seed.products) : [],
  };
}

function includedLights(state) {
  return state.lights.filter(frame => frame.included !== false && frame.path);
}

function uniqueFinite(frames, field, precision = 4) {
  return [...new Set(frames
    .map(frame => frame[field])
    .filter(Number.isFinite)
    .map(value => Number(value).toFixed(precision)))];
}

function geometryMismatch(frames) {
  const dimensions = frames
    .filter(frame => Number.isFinite(frame.width) && Number.isFinite(frame.height))
    .map(frame => `${frame.width}x${frame.height}x${frame.channels || "?"}`);
  return new Set(dimensions).size > 1;
}

function includedFrames(frames) {
  return frames.filter(frame => frame.included !== false && frame.path);
}

function numericCalibrationMismatch(calibrations, lights, field, precision = 4) {
  const lightValues = new Set(uniqueFinite(lights, field, precision));
  if (!lightValues.size) return false;
  return calibrations.some(frame => Number.isFinite(frame[field])
    && !lightValues.has(Number(frame[field]).toFixed(precision)));
}

function geometryCalibrationMismatch(calibrations, lights) {
  const lightGeometry = new Set(lights
    .filter(frame => Number.isFinite(frame.width)
      && Number.isFinite(frame.height)
      && Number.isFinite(frame.channels))
    .map(frame => `${frame.width}x${frame.height}x${frame.channels}`));
  if (!lightGeometry.size) return false;
  return calibrations.some(frame => Number.isFinite(frame.width)
    && Number.isFinite(frame.height)
    && Number.isFinite(frame.channels)
    && !lightGeometry.has(`${frame.width}x${frame.height}x${frame.channels}`));
}

function hasMissingCalibrationMetadata(frames, fields) {
  return frames.some(frame => fields.some(field => !Number.isFinite(frame[field])));
}

function maskIsRequired(mode) {
  return [MILKY_WAY_MODES.FREEZE_GROUND, MILKY_WAY_MODES.SEPARATE_LAYERS]
    .includes(mode);
}

export function validateMilkyWayState(input) {
  const state = createMilkyWayState(input);
  const lights = includedLights(state);
  const darks = includedFrames(state.darks);
  const flats = includedFrames(state.flats);
  const blockers = [];
  const warnings = [];
  const step = Object.fromEntries(MILKY_WAY_STEPS.map(([id]) => [id, { ready: true, reasons: [] }]));
  const block = (stepId, reason) => {
    step[stepId].ready = false;
    step[stepId].reasons.push(reason);
    blockers.push(reason);
  };
  const warn = (stepId, reason) => {
    step[stepId].reasons.push(reason);
    warnings.push(reason);
  };

  if (lights.length < 2) block("data", "Añade al menos dos tomas del cielo nocturno.");
  const unreadableIncluded = [...state.lights, ...state.darks, ...state.flats]
    .some(frame => frame.included !== false && frame.ok === false);
  if (unreadableIncluded) {
    block("data", "Hay archivos incluidos que no se pueden leer; quítalos o desmárcalos.");
  }
  if ([MILKY_WAY_MODES.FREEZE_GROUND, MILKY_WAY_MODES.SEPARATE_LAYERS].includes(state.mode)
      && lights.length < 4) {
    block("data", "Congelar suelo y rechazo robusto requieren al menos cuatro tomas.");
  }
  if (!state.baseFramePath || !lights.some(frame => frame.path === state.baseFramePath)) {
    block("data", "Elige una toma base incluida en la sesión.");
  }
  if (geometryMismatch(lights)) block("data", "Las tomas incluidas no comparten geometría y canales.");

  const exposures = uniqueFinite(lights, "exposureSeconds");
  const sensitivities = uniqueFinite(lights, "iso", 2);
  if ((exposures.length > 1 || sensitivities.length > 1) && !state.unifyExposure) {
    block("data", "Las tomas cambian exposición o ISO/gain; activa Unificar exposición o separa el lote.");
  } else if (exposures.length > 1 || sensitivities.length > 1) {
    warn("data", "Unificar exposición normalizará el lote y quedará registrado como concesión.");
  }

  if (numericCalibrationMismatch(darks, lights, "exposureSeconds")
    || numericCalibrationMismatch(darks, lights, "iso", 2)
    || geometryCalibrationMismatch(darks, lights)) {
    block("data", "Los darks conocidos no coinciden con exposición, ISO/gain o geometría de los lights.");
  }
  if (numericCalibrationMismatch(flats, lights, "iso", 2)
    || geometryCalibrationMismatch(flats, lights)) {
    block("data", "Los flats conocidos no coinciden con ISO/gain, geometría o canales de los lights.");
  }
  if (darks.length && (hasMissingCalibrationMetadata(darks, ["exposureSeconds", "iso", "width", "height", "channels"])
    || hasMissingCalibrationMetadata(lights, ["exposureSeconds", "iso", "width", "height", "channels"]))) {
    warn("data", "Darks con metadatos incompletos: Strict exigirá validación nativa antes de calibrar.");
  }
  if (flats.length && (hasMissingCalibrationMetadata(flats, ["iso", "width", "height", "channels"])
    || hasMissingCalibrationMetadata(lights, ["iso", "width", "height", "channels"]))) {
    warn("data", "Flats con metadatos incompletos: Strict exigirá validación nativa antes de calibrar.");
  }

  if (maskIsRequired(state.mode)) {
    if (!MASK_STRATEGIES.has(state.mask.strategy) || state.mask.strategy === "fullSky") {
      block("mask", "Congelar suelo necesita una máscara cielo/suelo.");
    }
    if (!state.mask.userConfirmed) block("mask", "Confirma la máscara para proteger el horizonte y el primer plano.");
    if (state.mask.strategy === "auto" && finite(state.mask.confidence, 0) < 0.72) {
      block("mask", "La máscara automática tiene baja confianza; corrígela con horizonte o pincel.");
    }
  }

  if (!REGISTRATION_MODELS.has(state.registration.model)) block("registration", "El modelo de registro no es válido.");
  if (!state.registration.skySolved) block("registration", "Falta validar el registro estelar del cielo.");
  if (finite(state.registration.inliers, 0) < finite(state.registration.minInliers, 18)) {
    block("registration", "El registro no conserva suficientes estrellas de control.");
  }
  if (Number.isFinite(state.registration.rmsPx)
    && state.registration.rmsPx > finite(state.registration.maxRmsPx, 2.2)) {
    block("registration", "El residuo del registro supera el límite configurado.");
  }
  if (maskIsRequired(state.mode) && !state.registration.groundSolved) {
    block("registration", "Falta validar la rama fija del suelo.");
  }
  if (state.registration.singleResample !== true) {
    block("registration", "Las transformaciones deben componerse antes de un único remuestreo.");
  }

  if (!ROBUST_METHODS.has(state.integration.method)) {
    block("integration", "Usa rechazo sigma o Winsorized para suprimir aviones, satélites y píxeles dinámicos.");
  }
  if (state.fallbackPolicy === "allowDegraded") {
    warn("integration", "AllowDegraded puede excluir tomas fallidas y marca el resultado como no científico.");
  }

  if (maskIsRequired(state.mode)) {
    const feather = finite(state.composition.featherPx, -1);
    if (feather < 0 || feather > 512) block("composition", "La transición cielo/suelo debe estar entre 0 y 512 px.");
    if (state.composition.groundSource === "separate" && state.foregroundFrames.length === 0) {
      block("composition", "Añade al menos una toma de primer plano o usa el suelo apilado/base.");
    }
  }
  if (!state.outputDirectory) block("publish", "Elige una carpeta de trabajo y salida.");
  if (!state.publish.exportLinearTiff && !state.publish.exportFitsLayers && !state.publish.openEditor) {
    block("publish", "Elige al menos una salida o abre el editor.");
  }

  let upstreamReady = true;
  const ordered = MILKY_WAY_STEPS.map(([id, label], index) => {
    const ownReady = step[id].ready;
    const reachable = index === 0 || upstreamReady;
    const result = { id, label, index, ready: ownReady, reachable, reasons: [...step[id].reasons] };
    upstreamReady = upstreamReady && ownReady;
    return result;
  });

  return {
    valid: blockers.length === 0,
    blockers: [...new Set(blockers)],
    warnings: [...new Set(warnings)],
    steps: ordered,
    effectiveLights: lights.length,
    products: state.mode === MILKY_WAY_MODES.SKY_ONLY
      ? ["sky", "mask", "skyVariance", "skyCoverage", "skyRejection", "recipe"]
      : [
          "sky", "ground", "composite", "mask",
          "skyVariance", "skyCoverage", "skyRejection",
          "groundVariance", "groundCoverage", "groundRejection",
          "recipe",
        ],
  };
}

export function buildMilkyWayRequest(input) {
  const state = createMilkyWayState(input);
  const validation = validateMilkyWayState(state);
  const request = {
    jobId: state.jobId || undefined,
    schema: MILKY_WAY_RECIPE_SCHEMA,
    mode: state.mode,
    lights: includedLights(state).map(frame => frame.path),
    darks: state.darks.filter(frame => frame.included !== false && frame.path).map(frame => frame.path),
    flats: state.flats.filter(frame => frame.included !== false && frame.path).map(frame => frame.path),
    foregroundFrames: state.foregroundFrames
      .filter(frame => frame.included !== false && frame.path)
      .map(frame => frame.path),
    baseFramePath: state.baseFramePath,
    outputDirectory: state.outputDirectory,
    unifyExposure: state.unifyExposure,
    mask: clone(state.mask),
    registration: clone(state.registration),
    integration: clone(state.integration),
    composition: { ...clone(state.composition), keepSeparateLayers: true },
    publish: clone(state.publish),
    fallbackPolicy: state.fallbackPolicy,
    geometryId: state.geometryId,
    requestedProducts: validation.products,
  };
  return request;
}

export function setMilkyWayStep(input, nextStep) {
  const state = createMilkyWayState(input);
  const validation = validateMilkyWayState(state);
  const target = Math.max(0, Math.min(MILKY_WAY_STEPS.length - 1, Number(nextStep) || 0));
  const targetState = validation.steps[target];
  if (!targetState.reachable && target > state.activeStep) {
    return { state, changed: false, blocker: validation.steps.find(item => !item.ready)?.reasons[0] || "Completa el paso anterior." };
  }
  state.activeStep = target;
  return { state, changed: true, blocker: null };
}

export function cropMilkyWayProducts(input, crop) {
  const state = createMilkyWayState(input);
  const normalized = {
    left: Math.max(0, Math.round(finite(crop?.left, 0))),
    top: Math.max(0, Math.round(finite(crop?.top, 0))),
    width: Math.max(1, Math.round(finite(crop?.width, 1))),
    height: Math.max(1, Math.round(finite(crop?.height, 1))),
  };
  state.geometryId = `${state.geometryId}:crop:${normalized.left},${normalized.top},${normalized.width},${normalized.height}`;
  state.crop = normalized;
  state.revision += 1;
  state.mask = { ...state.mask, geometryId: state.geometryId };
  state.products = validateMilkyWayState(state).products
    .map(kind => ({ kind, geometryId: state.geometryId, crop: normalized }));
  return state;
}
