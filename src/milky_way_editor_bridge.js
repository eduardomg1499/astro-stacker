export const MILKY_WAY_EDITOR_LAYER_KEYS = Object.freeze([
  "sky",
  "ground",
  "composite",
  "mask",
  "skyVariance",
  "groundVariance",
  "skyCoverage",
  "groundCoverage",
  "skyRejection",
  "groundRejection",
]);

export const MILKY_WAY_MASK_SENSITIVE_STEP_IDS = Object.freeze([
  "background_gradient",
  "psf_deconvolution",
  "linear_denoise",
  "star_layers",
]);

const MILKY_WAY_COMPOSITE_STEP_IDS = new Set([
  "astrometry",
  "channels_color",
  "psf_deconvolution",
  "linear_denoise",
  "star_layers",
  "stretch",
  "curves_color",
  "detail",
  "finish",
  "export",
]);

const MILKY_WAY_MASK_CAPABILITY_KEYS = Object.freeze({
  background_gradient: "backgroundGradient",
  psf_deconvolution: "psfDeconvolution",
  linear_denoise: "linearDenoise",
  star_layers: "starLayers",
});

const RESULT_ALIASES = Object.freeze({
  composite: ["compositePath", "composite_path", "masterPath", "master_path"],
  sky: ["skyPath", "sky_path"],
  ground: ["groundPath", "ground_path"],
  mask: ["maskPath", "mask_path"],
  skyVariance: ["skyVariancePath", "sky_variance_path"],
  groundVariance: ["groundVariancePath", "ground_variance_path"],
  skyCoverage: ["skyCoveragePath", "sky_coverage_path", "coveragePath", "coverage_path"],
  groundCoverage: ["groundCoveragePath", "ground_coverage_path"],
  skyRejection: ["skyRejectionPath", "sky_rejection_path", "rejectionPath", "rejection_path"],
  groundRejection: ["groundRejectionPath", "ground_rejection_path"],
  coverage: ["skyCoveragePath", "sky_coverage_path", "coveragePath", "coverage_path"],
  rejection: ["skyRejectionPath", "sky_rejection_path", "rejectionPath", "rejection_path"],
  recipe: ["recipePath", "recipe_path"],
});

const OUTPUT_ALIASES = Object.freeze({
  composite: ["composite"],
  sky: ["skyMaster", "sky_master", "primary"],
  ground: ["groundMaster", "ground_master"],
  mask: ["skyMask", "sky_mask"],
  skyVariance: ["skyVariance", "sky_variance"],
  groundVariance: ["groundVariance", "ground_variance"],
  skyCoverage: ["skyCoverage", "sky_coverage"],
  groundCoverage: ["groundCoverage", "ground_coverage"],
  skyRejection: ["skyRejection", "sky_rejection"],
  groundRejection: ["groundRejection", "ground_rejection"],
  coverage: ["skyCoverage", "sky_coverage"],
  rejection: ["skyRejection", "sky_rejection"],
  recipe: ["recipe"],
});

function nonEmptyPath(value) {
  return typeof value === "string" && value.trim() ? value.trim() : "";
}

export function isMilkyWayEditorDescriptor(descriptor = {}) {
  const workflow = String(descriptor?.workflow || "").trim().toLowerCase();
  const captureMode = String(descriptor?.captureMode || descriptor?.capture_mode || "")
    .trim()
    .toLowerCase();
  return workflow === "milky_way"
    || captureMode === "milky_way"
    || captureMode === "milkyway";
}

function milkyWayGeometryId(descriptor = {}) {
  return String(
    descriptor?.milkyWayGeometryId
      || descriptor?.geometryId
      || descriptor?.geometry_id
      || descriptor?.milkyWayRoot?.geometryId
      || "milky-way-source-geometry",
  );
}

function milkyWayRecipePath(descriptor = {}) {
  return nonEmptyPath(descriptor?.recipePath)
    || nonEmptyPath(descriptor?.milkyWayRoot?.recipePath);
}

function milkyWayStepIsHorizonMaskAware(descriptor, stepId) {
  const capability = MILKY_WAY_MASK_CAPABILITY_KEYS[stepId];
  return !!capability
    && descriptor?.milkyWayEditorCapabilities?.horizonMaskAware?.[capability] === true;
}

function milkyWayCompositeReason(step, descriptor) {
  const geometryId = milkyWayGeometryId(descriptor);
  const recipePath = milkyWayRecipePath(descriptor);
  switch (step.id) {
    case "astrometry":
      return descriptor?.wcsValid === true
        ? `WCS validado con las estrellas del cielo y vinculado al Compuesto (${geometryId}); no modifica píxeles.`
        : `Resuelve con las estrellas visibles del cielo en el Compuesto y vincula el WCS a su geometryId (${geometryId}); no modifica píxeles.`;
    case "channels_color":
      return "Color y canales crean una revisión del Compuesto; Cielo, Suelo, Máscara y mapas fuente permanecen inmutables.";
    case "stretch":
      return "El estirado crea una revisión de presentación del Compuesto; las raíces lineales de Cielo y Suelo permanecen inmutables.";
    case "curves_color":
      return "Curvas y color derivan únicamente del Compuesto activo; no reescriben Cielo, Suelo, Máscara ni mapas científicos.";
    case "detail":
      return "El realce deriva únicamente del Compuesto procesado; las raíces lineales permanecen inmutables.";
    case "finish":
      return "El acabado deriva únicamente del Compuesto de presentación; las raíces lineales permanecen inmutables.";
    case "export":
      return `Exporta la revisión del Compuesto conservando el vínculo con recipePath${recipePath ? "" : " (pendiente)"} y geometryId ${geometryId}.`;
    default:
      return step.reason || "";
  }
}

/**
 * El backend post-stack actual carga el Compuesto como una sola imagen. Hasta
 * que una operación declare explícitamente que consume la máscara de horizonte,
 * no debe presentarse como una recomendación automática para Vía Láctea.
 */
export function applyMilkyWayEditorSafetyPlan(plan, descriptor = {}) {
  if (!isMilkyWayEditorDescriptor(descriptor)) return plan;
  const maskSensitive = new Set(MILKY_WAY_MASK_SENSITIVE_STEP_IDS);
  const geometryId = milkyWayGeometryId(descriptor);
  const recipePath = milkyWayRecipePath(descriptor);
  return {
    ...plan,
    workflow: "milky_way",
    editTarget: "composite",
    geometryId,
    recipePath: recipePath || null,
    steps: (plan?.steps || []).map((step) => {
      if (step.id === "crop") {
        return {
          ...step,
          target: "synchronized_products",
          atomicGeometry: true,
          immutableRoot: true,
        };
      }
      if (maskSensitive.has(step.id)) {
        const horizonMaskAware = milkyWayStepIsHorizonMaskAware(descriptor, step.id);
        if (!horizonMaskAware) {
          return {
            ...step,
            required: false,
            recommended: false,
            target: "composite",
            horizonMaskAware: false,
            essentialAuto: false,
            immutableRoot: true,
            reason: "Opcional sobre el Compuesto. Esta herramienta aún no consume la Máscara cielo-suelo; revisa el horizonte y no se ejecuta en la edición rápida.",
          };
        }
        return {
          ...step,
          target: "composite",
          horizonMaskAware: true,
          essentialAuto: !!(step.required || step.recommended),
          immutableRoot: true,
          reason: `${step.reason || "Operación disponible."} La máscara cielo-suelo se consume explícitamente.`,
        };
      }
      if (MILKY_WAY_COMPOSITE_STEP_IDS.has(step.id)) {
        return {
          ...step,
          target: "composite",
          immutableRoot: true,
          essentialAuto: !!(step.required || step.recommended),
          reason: milkyWayCompositeReason(step, descriptor),
        };
      }
      return { ...step, immutableRoot: true };
    }),
  };
}

export function milkyWayEditorShouldAutoApplyStep(step) {
  return !!step
    && step.applies !== false
    && step.blocked !== true
    && step.essentialAuto !== false
    && (step.required === true || step.recommended === true);
}

export function preserveMilkyWayRecipeIdentity(recipe, descriptor = {}) {
  if (!isMilkyWayEditorDescriptor(descriptor)) return recipe;
  const geometryId = milkyWayGeometryId(descriptor);
  const root = descriptor?.milkyWayRoot || {};
  const recipePath = milkyWayRecipePath(descriptor);
  const rootReferences = Object.values(root.layers || descriptor?.milkyWayLayers || {})
    .map(nonEmptyPath)
    .filter(Boolean);
  return {
    ...recipe,
    workflow: "milky_way",
    source: {
      ...(recipe?.source || {}),
      workflow: "milky_way",
      editTarget: "composite",
      recipePath: recipePath || null,
      geometryId,
      rootGeometryId: String(root.geometryId || geometryId),
      immutableRootReferences: rootReferences,
    },
  };
}

export function attachMilkyWayOutputIdentity(output, descriptor = {}) {
  if (!isMilkyWayEditorDescriptor(descriptor) || !output || typeof output !== "object") {
    return output;
  }
  const geometryId = milkyWayGeometryId(descriptor);
  return {
    ...output,
    sourceIdentity: {
      workflow: "milky_way",
      editTarget: "composite",
      recipePath: milkyWayRecipePath(descriptor) || null,
      geometryId,
      wcsGeometryId: descriptor?.milkyWayWcsGeometryId || null,
    },
  };
}

export function resolveMilkyWayResultPath(result, key) {
  for (const name of RESULT_ALIASES[key] || [key]) {
    const path = nonEmptyPath(result?.[name]);
    if (path) return path;
  }
  for (const name of OUTPUT_ALIASES[key] || [key]) {
    const output = result?.outputs?.[name];
    const path = nonEmptyPath(typeof output === "string" ? output : output?.path);
    if (path) return path;
  }
  if (result?.products && !Array.isArray(result.products)) {
    const product = result.products[key];
    const path = nonEmptyPath(typeof product === "string" ? product : product?.path);
    if (path) return path;
  }
  const product = Array.isArray(result?.products)
    ? result.products.find(item => (
        String(item?.kind || item?.product || "").toLowerCase() === String(key).toLowerCase()
      ))
    : null;
  return nonEmptyPath(product?.path || product?.fitsPath || product?.fits_path);
}

export function milkyWayLayerPaths(result) {
  return Object.fromEntries(
    MILKY_WAY_EDITOR_LAYER_KEYS
      .map(kind => [kind, resolveMilkyWayResultPath(result, kind)])
      .filter(([, path]) => !!path),
  );
}

export function milkyWayPrimaryPath(result) {
  return resolveMilkyWayResultPath(result, "composite")
    || resolveMilkyWayResultPath(result, "sky");
}

export function normalizedMilkyWayCropToPixels(rect, sourceWidth, sourceHeight) {
  const widthPx = Math.max(1, Math.floor(Number(sourceWidth) || 0));
  const heightPx = Math.max(1, Math.floor(Number(sourceHeight) || 0));
  const x = Math.max(0, Math.min(1, Number(rect?.x) || 0));
  const y = Math.max(0, Math.min(1, Number(rect?.y) || 0));
  const normalizedWidth = Math.max(0, Math.min(1 - x, Number(rect?.width) || 0));
  const normalizedHeight = Math.max(0, Math.min(1 - y, Number(rect?.height) || 0));
  const left = Math.min(widthPx - 1, Math.max(0, Math.round(x * widthPx)));
  const top = Math.min(heightPx - 1, Math.max(0, Math.round(y * heightPx)));
  const width = Math.min(widthPx - left, Math.max(1, Math.round(normalizedWidth * widthPx)));
  const height = Math.min(heightPx - top, Math.max(1, Math.round(normalizedHeight * heightPx)));
  return { left, top, width, height };
}

export function buildMilkyWayAtomicCropRequest(layers, rect, sourceWidth, sourceHeight) {
  const required = ["sky", "mask", "skyVariance", "skyCoverage", "skyRejection"];
  if (nonEmptyPath(layers?.ground)) {
    required.push("groundVariance", "groundCoverage", "groundRejection");
  }
  const missing = required.filter(kind => !nonEmptyPath(layers?.[kind]));
  if (missing.length) {
    throw new Error(`Faltan productos sincronizados para recortar: ${missing.join(", ")}`);
  }
  return {
    skyPath: nonEmptyPath(layers.sky),
    groundPath: nonEmptyPath(layers.ground) || null,
    compositePath: nonEmptyPath(layers.composite) || null,
    maskPath: nonEmptyPath(layers.mask),
    skyVariancePath: nonEmptyPath(layers.skyVariance),
    groundVariancePath: nonEmptyPath(layers.groundVariance) || null,
    skyCoveragePath: nonEmptyPath(layers.skyCoverage),
    groundCoveragePath: nonEmptyPath(layers.groundCoverage) || null,
    skyRejectionPath: nonEmptyPath(layers.skyRejection),
    groundRejectionPath: nonEmptyPath(layers.groundRejection) || null,
    ...normalizedMilkyWayCropToPixels(rect, sourceWidth, sourceHeight),
  };
}

export function describeMilkyWayGeometryRevision(descriptor, croppedResult) {
  const nextLayers = milkyWayLayerPaths(croppedResult);
  const root = descriptor?.milkyWayRoot || {
    layers: { ...(descriptor?.milkyWayLayers || {}) },
    geometryId: descriptor?.milkyWayGeometryId || "milky-way-source-geometry",
    wcsValid: !!descriptor?.wcsValid,
    recipePath: descriptor?.recipePath,
  };
  const geometryId = String(croppedResult?.geometryId || croppedResult?.geometry_id || "");
  const wcsPreserved = !!(croppedResult?.wcsPreserved ?? croppedResult?.wcs_preserved);
  return {
    ...descriptor,
    path: milkyWayPrimaryPath(croppedResult),
    sourceReferences: Object.values(nextLayers),
    milkyWayLayers: nextLayers,
    milkyWayRoot: root,
    milkyWayGeometryId: geometryId,
    milkyWayWcsGeometryId: wcsPreserved ? geometryId : null,
    milkyWayCrop: {
      sourceWidth: Number(croppedResult?.sourceWidth || croppedResult?.source_width || 0),
      sourceHeight: Number(croppedResult?.sourceHeight || croppedResult?.source_height || 0),
      width: Number(croppedResult?.width || 0),
      height: Number(croppedResult?.height || 0),
    },
    recipePath: resolveMilkyWayResultPath(croppedResult, "recipe")
      || descriptor?.recipePath,
    wcsValid: wcsPreserved,
  };
}
