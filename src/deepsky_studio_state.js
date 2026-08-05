const SOURCE_KINDS = Object.freeze({
  BROADBAND_RGB: "broadband_rgb",
  MONO_BROADBAND: "mono_broadband_lrgb",
  MONO_NARROWBAND: "mono_narrowband",
  OSC_DUAL_BAND: "osc_dual_band",
  MULTI_FILTER_DUAL_BAND: "multi_filter_dual_band",
  ALREADY_COMBINED: "already_combined",
  UNKNOWN: "unknown",
});

const STEP_DEFINITIONS = Object.freeze([
  ["crop", "Recortar"],
  ["background_gradient", "Fondo y gradientes"],
  ["astrometry", "Astrometría"],
  ["channels_color", "Canales y color"],
  ["psf_deconvolution", "Restaurar PSF"],
  ["linear_denoise", "Ruido lineal"],
  ["star_layers", "Capas estelares"],
  ["stretch", "Estirar"],
  ["curves_color", "Curvas y color"],
  ["detail", "Detalle"],
  ["finish", "Acabado"],
  ["export", "Exportar"],
]);

const COMPONENT_ORDER = Object.freeze(["L", "R", "G", "B", "HA", "OIII", "SII"]);

const PALETTE_DEFINITIONS = Object.freeze([
  {
    id: "hooNatural",
    label: "HOO natural",
    description: "Ha en rojo y OIII en verde/azul.",
    requiredComponents: ["HA", "OIII"],
  },
  {
    id: "hooTeal",
    label: "HOO cyan",
    description: "Ha cálido y OIII cian con separación cromática alta.",
    requiredComponents: ["HA", "OIII"],
  },
  {
    id: "hooSoft",
    label: "HOO suave",
    description: "Transiciones Ha/OIII contenidas y estrellas menos cian.",
    requiredComponents: ["HA", "OIII"],
  },
  {
    id: "hooGold",
    label: "HOO dorada",
    description: "Volumen Ha con contrapunto frío OIII.",
    requiredComponents: ["HA", "OIII"],
  },
  {
    id: "sho",
    label: "SHO",
    description: "Paleta Hubble: SII, Ha y OIII.",
    requiredComponents: ["SII", "HA", "OIII"],
  },
  {
    id: "shoBalanced",
    label: "SHO equilibrada",
    description: "SHO con transición SII/Ha más continua.",
    requiredComponents: ["SII", "HA", "OIII"],
  },
  {
    id: "hso",
    label: "HSO",
    description: "Ha, SII y OIII con contraste cálido.",
    requiredComponents: ["HA", "SII", "OIII"],
  },
  {
    id: "soo",
    label: "SOO",
    description: "SII en rojo y OIII en verde/azul.",
    requiredComponents: ["SII", "OIII"],
  },
]);

const STEP_SETTING_ALIASES = Object.freeze({
  background_gradient: "backgroundGradient",
  channels_color: "channelsColor",
  linear_denoise: "linearDenoise",
  psf_deconvolution: "psfDeconvolution",
  star_layers: "starLayers",
  curves_color: "curvesColor",
});

function compactToken(value) {
  return String(value ?? "")
    .trim()
    .toUpperCase()
    .replace(/[ÁÀÄ]/g, "A")
    .replace(/[ÍÌÏ]/g, "I")
    .replace(/[^A-Z0-9]+/g, "_")
    .replace(/^_+|_+$/g, "");
}

function componentFromToken(value) {
  const token = compactToken(value);
  if (!token) return [];
  if (["L", "LUM", "LUMA", "LUMINANCE", "LUMINANCIA"].includes(token)) return ["L"];
  if (["R", "RED", "ROJO"].includes(token)) return ["R"];
  if (["G", "GREEN", "VERDE"].includes(token)) return ["G"];
  if (["B", "BLUE", "AZUL"].includes(token)) return ["B"];
  if (["HA", "H_ALPHA", "HALPHA", "HYDROGEN_ALPHA"].includes(token)) return ["HA"];
  if (["OIII", "O_III", "O3", "OXYGEN_III"].includes(token)) return ["OIII"];
  if (["SII", "S_II", "S2", "SULFUR_II", "SULPHUR_II"].includes(token)) return ["SII"];

  const parts = token.split("_");
  const components = [];
  if (parts.includes("LRGB") || parts.includes("RGB")) components.push("R", "G", "B");
  if (parts.includes("LRGB")) components.unshift("L");
  if (parts.includes("HA") || token.includes("HALPHA")) components.push("HA");
  if (parts.includes("OIII") || parts.includes("O3")) components.push("OIII");
  if (parts.includes("SII") || parts.includes("S2")) components.push("SII");
  return [...new Set(components)];
}

function collectEvidence(descriptor = {}) {
  const filterSets = [];
  const componentSources = Object.fromEntries(
    COMPONENT_ORDER.map(component => [component, new Set()]),
  );
  const pushSet = (values) => {
    const found = [...new Set(values.flatMap(componentFromToken))];
    if (found.length > 0) filterSets.push(found);
    return found;
  };
  const recordSource = (sourceKey, values) => {
    if (sourceKey == null || sourceKey === "") return;
    const found = [...new Set(values.flatMap(componentFromToken))];
    for (const component of found) {
      componentSources[component]?.add(String(sourceKey));
    }
  };

  const directValues = [
    descriptor.filter,
    descriptor.filterProfile,
    ...(Array.isArray(descriptor.components) ? descriptor.components : []),
    ...(Array.isArray(descriptor.componentFilters) ? descriptor.componentFilters : []),
  ];
  const directComponents = pushSet(directValues);
  const directReference = descriptor.path
    || descriptor.sourcePath
    || (Array.isArray(descriptor.sourceReferences) && descriptor.sourceReferences.length === 1
      ? descriptor.sourceReferences[0]
      : null);
  recordSource(directReference, directComponents);

  for (const collection of [descriptor.filters, descriptor.filterProfiles]) {
    if (!Array.isArray(collection)) continue;
    const references = Array.isArray(descriptor.paths)
      ? descriptor.paths
      : Array.isArray(descriptor.sourceReferences)
        ? descriptor.sourceReferences
        : [];
    for (const [index, value] of collection.entries()) {
      const found = pushSet([value]);
      if (references.length === collection.length) {
        recordSource(references[index], found);
      }
    }
  }

  for (const [component, property] of [
    ["HA", "haPaths"],
    ["OIII", "oiiiPaths"],
    ["SII", "siiPaths"],
  ]) {
    const paths = Array.isArray(descriptor[property])
      ? descriptor[property]
      : descriptor[property]
        ? [descriptor[property]]
        : [];
    for (const path of paths) {
      pushSet([component]);
      // The path—not the property name—is the identity. Reusing one file as
      // both Ha and OIII must not manufacture two independent mono channels.
      recordSource(`path:${String(path)}`, [component]);
    }
  }

  for (const [collectionName, collection] of [
    ["masters", descriptor.masters],
    ["groups", descriptor.groups],
    ["sources", descriptor.sources],
  ]) {
    if (!Array.isArray(collection)) continue;
    for (const [index, entry] of collection.entries()) {
      if (typeof entry === "string") {
        const found = pushSet([entry]);
        recordSource(`path:${entry}`, found);
        continue;
      }
      if (!entry || typeof entry !== "object") continue;
      const entryValues = [entry.filter, entry.filterProfile, entry.band, entry.channel];
      const explicit = entry.componentFilters || entry.components;
      if (Array.isArray(explicit)) entryValues.push(...explicit);
      if (entryValues.every(value => value == null || value === "")) entryValues.push(entry.id);
      const found = pushSet(entryValues);
      const sourceKey = entry.path
        || entry.sourcePath
        || entry.masterPath
        || entry.id
        || `${collectionName}:${index}`;
      recordSource(`source:${String(sourceKey)}`, found);
    }
  }

  const components = filterSets.flat();
  const counts = components.reduce((result, component) => {
    result[component] = (result[component] || 0) + 1;
    return result;
  }, {});

  return {
    components: COMPONENT_ORDER.filter(component => counts[component] > 0),
    componentCounts: counts,
    componentSources: Object.fromEntries(
      Object.entries(componentSources).map(([component, sources]) => [
        component,
        [...sources],
      ]),
    ),
    filterSets,
    hasHaOiii: filterSets.some(set => set.includes("HA") && set.includes("OIII")),
    hasSiiOiii: filterSets.some(set => set.includes("SII") && set.includes("OIII")),
  };
}

function isExplicitlyMono(descriptor) {
  if (descriptor?.isMono === true || Number(descriptor?.channels) === 1) return true;
  return ["MONO", "MONO_BROADBAND", "MONO_NARROWBAND", "LRGB"]
    .includes(compactToken(descriptor?.captureMode || descriptor?.sourceType));
}

function instrumentProfileIsSufficient(descriptor) {
  return descriptor?.instrumentProfileSufficient === true
    || descriptor?.instrumentProfile?.complete === true
    || descriptor?.instrumentProfile?.scientific === true;
}

function oiiiIsReconciled(descriptor) {
  return descriptor?.oiiiReconciled === true
    || descriptor?.channelReconciliation?.oiii?.status === "validated";
}

function sortedComponents(components) {
  const unique = new Set(components || []);
  return COMPONENT_ORDER.filter(component => unique.has(component));
}

function hasDistinctComponentSources(componentSources, requiredComponents) {
  const ordered = [...requiredComponents]
    .sort((left, right) =>
      (componentSources[left]?.length || 0) - (componentSources[right]?.length || 0));

  const assign = (index, usedSources) => {
    if (index >= ordered.length) return true;
    const sources = componentSources[ordered[index]] || [];
    for (const source of sources) {
      if (usedSources.has(source)) continue;
      usedSources.add(source);
      if (assign(index + 1, usedSources)) return true;
      usedSources.delete(source);
    }
    return false;
  };

  return assign(0, new Set());
}

function settingsForStep(settings, id) {
  if (settings?.[id] && typeof settings[id] === "object") return settings[id];
  const alias = STEP_SETTING_ALIASES[id];
  if (alias && settings?.[alias] && typeof settings[alias] === "object") {
    return settings[alias];
  }
  return {};
}

export function classifyDeepSkySource(descriptor = {}) {
  const evidence = collectEvidence(descriptor);
  const explicitType = compactToken(
    descriptor.sourceClass || descriptor.sourceType || descriptor.captureMode || descriptor.kind,
  );
  const explicitlyCombined = descriptor.alreadyCombined === true
    || descriptor.isCombined === true
    || ["ALREADY_COMBINED", "COMBINED", "CHANNEL_COMBINATION"].includes(explicitType);

  let kind = SOURCE_KINDS.UNKNOWN;
  if (explicitlyCombined) {
    kind = SOURCE_KINDS.ALREADY_COMBINED;
  } else if (
    evidence.hasHaOiii
    && evidence.hasSiiOiii
  ) {
    kind = SOURCE_KINDS.MULTI_FILTER_DUAL_BAND;
  } else if (
    ["DUALBANDOSC", "DUAL_BAND_OSC", "OSC_DUAL_BAND", "DUALBAND", "DUAL_BAND"].includes(explicitType)
    || (!isExplicitlyMono(descriptor) && (evidence.hasHaOiii || evidence.hasSiiOiii))
  ) {
    kind = SOURCE_KINDS.OSC_DUAL_BAND;
  } else if (
    ["MONONARROWBAND", "MONO_NARROWBAND"].includes(explicitType)
    || (isExplicitlyMono(descriptor)
      && evidence.components.some(component => ["HA", "OIII", "SII"].includes(component)))
  ) {
    kind = SOURCE_KINDS.MONO_NARROWBAND;
  } else if (
    ["MONOBROADBAND", "MONO_BROADBAND", "LRGB"].includes(explicitType)
    || isExplicitlyMono(descriptor)
  ) {
    kind = SOURCE_KINDS.MONO_BROADBAND;
  } else if (
    ["BROADBANDRGB", "BROADBAND_RGB", "RGBBROADBAND", "RGB_BROADBAND", "OSC", "OSC_BROADBAND", "RGB"].includes(explicitType)
    || Number(descriptor.channels) >= 3
    || evidence.components.filter(component => ["R", "G", "B"].includes(component)).length === 3
  ) {
    kind = SOURCE_KINDS.BROADBAND_RGB;
  }

  const duplicatedComponents = sortedComponents(
    Object.entries(evidence.componentCounts)
      .filter(([, count]) => count > 1)
      .map(([component]) => component),
  );

  return {
    kind,
    components: evidence.components,
    duplicatedComponents,
    hasHaOiii: evidence.hasHaOiii,
    hasSiiOiii: evidence.hasSiiOiii,
    oiiiReconciled: oiiiIsReconciled(descriptor),
    instrumentProfileSufficient: instrumentProfileIsSufficient(descriptor),
    independentComponentSources: evidence.componentSources,
  };
}

export function getDeepSkyPaletteCandidates(descriptor = {}) {
  const source = classifyDeepSkySource(descriptor);
  const available = new Set(source.components);
  const duplicatedOiii = source.kind === SOURCE_KINDS.MULTI_FILTER_DUAL_BAND
    || source.duplicatedComponents.includes("OIII");

  return PALETTE_DEFINITIONS.map((definition) => {
    const missingComponents = definition.requiredComponents
      .filter(component => !available.has(component));
    const needsReconciliation = duplicatedOiii && definition.requiredComponents.includes("OIII");
    const missingProfile = definition.requiresInstrumentProfile
      && !source.instrumentProfileSufficient;
    const missingIndependentMasters = source.kind === SOURCE_KINDS.MONO_NARROWBAND
      && !hasDistinctComponentSources(
        source.independentComponentSources,
        definition.requiredComponents,
      );

    let reason = "";
    if (missingComponents.length > 0) {
      reason = `Faltan ${missingComponents.join(", ")}.`;
    } else if (missingIndependentMasters) {
      reason = "Un máster mono aislado no contiene canales espectrales independientes; añade un máster lineal distinto para cada canal requerido.";
    } else if (needsReconciliation && !source.oiiiReconciled) {
      reason = "Las dos señales OIII deben reconciliarse y validarse antes de crear una paleta científica.";
    } else if (missingProfile) {
      reason = "Foraxx requiere un perfil instrumental completo y validado.";
    }

    return {
      ...definition,
      requiredComponents: [...definition.requiredComponents],
      missingComponents,
      eligible: reason === "",
      scientificEligible: reason === "",
      reason: reason || "Todos los canales y requisitos científicos están disponibles.",
      requiresIndependentMasters: missingIndependentMasters,
      requiresOiiiReconciliation: needsReconciliation,
      requiresInstrumentProfile: definition.requiresInstrumentProfile === true,
    };
  });
}

function stepState(id, source, descriptor, settings, palettes) {
  const hasKnownSource = source.kind !== SOURCE_KINDS.UNKNOWN;
  const stretchEnabled = settingsForStep(settings, "stretch").enabled !== false;
  const wantsPcc = ["pcc", "gaia_pcc"].includes(
    compactToken(settingsForStep(settings, "channels_color").mode).toLowerCase(),
  );
  const hasRgb = ["R", "G", "B"].every(component => source.components.includes(component));
  const paletteEligible = palettes.some(palette => palette.eligible);

  switch (id) {
    case "crop": {
      const borders = descriptor?.hasInvalidBorders === true
        || Number(descriptor?.coverageFraction) < 1;
      return {
        applies: hasKnownSource,
        required: false,
        recommended: borders,
        reason: borders
          ? "Hay bordes sin cobertura; se recomienda definir el encuadre sin alterar el máster."
          : "Opcional: define una revisión de encuadre.",
      };
    }
    case "background_gradient":
      return {
        applies: hasKnownSource,
        required: false,
        recommended: descriptor?.backgroundCorrected !== true,
        reason: descriptor?.backgroundCorrected === true
          ? "Ya existe una corrección registrada; puede revisarse el modelo y residual."
          : "Modela el fondo en una sola operación automática o mediante muestras editables.",
      };
    case "astrometry": {
      const wcsValid = descriptor?.wcsValid === true;
      return {
        applies: hasKnownSource,
        required: wantsPcc && !wcsValid,
        recommended: !wcsValid,
        reason: wcsValid
          ? "WCS validado; no es necesario resolver de nuevo."
          : wantsPcc
            ? "PCC requiere una solución WCS validada."
            : "Recomendado para catalogación y operaciones fotométricas posteriores.",
      };
    }
    case "channels_color": {
      if (source.kind === SOURCE_KINDS.UNKNOWN) {
        return {
          applies: false,
          required: false,
          recommended: false,
          reason: "Identifica primero el tipo de cámara, filtros y canales.",
        };
      }
      if (source.kind === SOURCE_KINDS.ALREADY_COMBINED) {
        return {
          applies: false,
          required: false,
          recommended: false,
          reason: "La fuente ya es una combinación terminada; no se remapea automáticamente.",
        };
      }
      if (source.kind === SOURCE_KINDS.MONO_BROADBAND && !hasRgb) {
        return {
          applies: false,
          required: false,
          recommended: false,
          reason: "Un máster mono aislado no contiene los canales R, G y B necesarios para LRGB.",
        };
      }
      if (
        [SOURCE_KINDS.MONO_NARROWBAND, SOURCE_KINDS.OSC_DUAL_BAND,
          SOURCE_KINDS.MULTI_FILTER_DUAL_BAND].includes(source.kind)
        && !paletteEligible
      ) {
        return {
          applies: true,
          required: false,
          recommended: true,
          blocked: true,
          reason: palettes.find(palette => palette.missingComponents.length === 0)?.reason
            || "No hay una paleta científicamente elegible con los canales disponibles.",
        };
      }
      return {
        applies: true,
        required: false,
        recommended: true,
        reason: source.kind === SOURCE_KINDS.BROADBAND_RGB
          ? "Valida el color fotométrico; los píxeles originales permanecen inmutables."
          : "Combina o mapea canales en una revisión derivada.",
      };
    }
    case "linear_denoise":
      return {
        applies: hasKnownSource,
        required: false,
        recommended: hasKnownSource,
        reason: "Reduce ruido sobre datos lineales antes del estirado.",
      };
    case "psf_deconvolution": {
      const confidence = Number(descriptor?.psfConfidence);
      const measured = descriptor?.psfMeasured === true
        || (Number.isFinite(confidence) && confidence >= 0.18);
      return {
        applies: hasKnownSource,
        required: false,
        recommended: measured || descriptor?.psfMeasured == null,
        reason: measured
          ? "Restaura detalle lineal con la PSF medida y protección contra halos."
          : "Zenith medirá la PSF; si la confianza es baja conservará la revisión sin restaurar.",
      };
    }
    case "star_layers":
      return {
        applies: hasKnownSource,
        required: false,
        recommended: hasKnownSource,
        reason: "Separa Objeto y Estrellas de forma aditiva para editarlos sin tocar el máster.",
      };
    case "stretch":
      return {
        applies: hasKnownSource,
        required: false,
        recommended: true,
        reason: "Opcional: crea una rama no lineal; la rama científica lineal se conserva.",
      };
    case "curves_color":
      return {
        applies: hasKnownSource && stretchEnabled,
        required: false,
        recommended: false,
        reason: stretchEnabled
          ? source.kind === SOURCE_KINDS.MONO_BROADBAND
            ? "Ajusta la curva maestra y la luminancia sin inventar canales de color."
            : "Curvas K/L/R/G/B/S y color selectivo sobre una revisión float32."
          : "Las curvas de presentación se habilitan después del estirado.",
      };
    case "detail":
      return {
        applies: hasKnownSource && stretchEnabled,
        required: false,
        recommended: false,
        reason: stretchEnabled
          ? "Realce opcional sobre la revisión no lineal."
          : "Activa Estirar para usar el realce no lineal de detalle.",
      };
    case "finish":
      return {
        applies: hasKnownSource && stretchEnabled,
        required: false,
        recommended: false,
        reason: stretchEnabled
          ? "Ajustes finales opcionales de presentación."
          : "La rama lineal puede exportarse sin acabado.",
      };
    case "export":
      return {
        applies: hasKnownSource,
        required: hasKnownSource,
        recommended: hasKnownSource,
        reason: "Exporta una revisión o el máster lineal inmutable con su receta.",
      };
    default:
      return { applies: false, required: false, recommended: false, reason: "" };
  }
}

export function planDeepSkyStudio(descriptor = {}, settings = {}) {
  const source = classifyDeepSkySource(descriptor);
  const palettes = getDeepSkyPaletteCandidates(descriptor);
  const steps = STEP_DEFINITIONS.map(([id, label], index) => ({
    id,
    label,
    order: index + 1,
    ...stepState(id, source, descriptor, settings, palettes),
  }));

  return {
    source,
    steps,
    palettes,
    requiresOiiiReconciliation: source.kind === SOURCE_KINDS.MULTI_FILTER_DUAL_BAND
      && !source.oiiiReconciled,
  };
}

function cloneSerializable(value) {
  if (value === undefined) return undefined;
  if (typeof structuredClone === "function") return structuredClone(value);
  return JSON.parse(JSON.stringify(value));
}

/**
 * Quick and Expert are presentation modes only. Given the same source and
 * settings they must emit byte-for-byte equivalent scientific requests.
 */
export function buildDeepSkyStudioRequest({
  source = {},
  settings = {},
  presentationMode: _presentationMode,
} = {}) {
  const plan = planDeepSkyStudio(source, settings);
  const sourceReferences = Array.isArray(source.sourceReferences)
    ? source.sourceReferences
    : Array.isArray(source.paths)
      ? source.paths
      : source.path
        ? [source.path]
        : [];

  return {
    schema: "zenith-deepsky-poststack-recipe-v1",
    source: {
      kind: plan.source.kind,
      references: cloneSerializable(sourceReferences),
      components: [...plan.source.components],
      oiiiReconciled: plan.source.oiiiReconciled,
      instrumentProfileSufficient: plan.source.instrumentProfileSufficient,
    },
    operations: plan.steps
      .filter((step) => {
        const operationSettings = settingsForStep(settings, step.id);
        return step.applies
          && !step.blocked
          && (operationSettings.enabled === true
            || (step.id === "export" && operationSettings.enabled !== false));
      })
      .map(step => ({
        id: step.id,
        settings: cloneSerializable(settingsForStep(settings, step.id)),
      })),
  };
}

const GRAPH_LINEAR_MODULES = new Set([
  "crop",
  "background_gradient",
  "channels_color",
  "psf_deconvolution",
  "linear_denoise",
  "star_layers",
  "star_adjustment",
  "star_recombination",
]);

function graphClone(value) {
  return cloneSerializable(value);
}

function graphArtifactId(operationId, role = "output") {
  return `${String(operationId)}:${role}`;
}

/**
 * Estado científico v2: los píxeles viven en el backend/caché; aquí sólo
 * existen artefactos, dependencias y heads de ramas. `presentation` jamás
 * forma parte del request ni del hash científico.
 */
export function createDeepSkyStudioState(source = {}) {
  const sourceId = "artifact-source";
  return {
    schema: "zenith-deepsky-studio-state-v2",
    source: graphClone(source),
    artifacts: {
      [sourceId]: {
        id: sourceId,
        role: "source",
        branch: "main",
        domain: "linear",
        geometryId: "geometry-source",
        immutable: true,
        parentOperationId: null,
        parentArtifactIds: [],
        separationId: null,
      },
    },
    operations: {},
    branchSets: {},
    heads: {
      main: sourceId,
      object: null,
      stars: null,
      recombined: null,
    },
    journal: [],
    redo: [],
    presentation: {
      experience: "essential",
      selectedArtifactId: sourceId,
      selectedModuleId: null,
    },
  };
}

function assertGraphOperation(state, operation) {
  if (!operation?.id || !operation?.type) {
    throw new Error("Cada operación necesita id y type.");
  }
  if (state.operations[operation.id]) {
    throw new Error(`La operación ${operation.id} ya existe.`);
  }
}

function graphHeadForBranch(state, branch) {
  const normalized = branch === "starless" ? "object" : (branch || "main");
  const id = state.heads[normalized];
  if (!id || !state.artifacts[id]) {
    throw new Error(`La rama ${normalized} todavía no existe.`);
  }
  return [normalized, state.artifacts[id]];
}

export function applyDeepSkyStudioOperation(inputState, operation) {
  const state = graphClone(inputState);
  assertGraphOperation(state, operation);
  const beforeHeads = graphClone(state.heads);
  const type = String(operation.type);

  if (type === "star_layers" || type === "star_separation") {
    const [, input] = graphHeadForBranch(state, operation.branch || "main");
    if (input.domain !== "linear") {
      throw new Error("La separación estelar nativa requiere una revisión lineal.");
    }
    const separationId = operation.separationId || operation.id;
    const roles = ["object", "stars", "mask", "residual"];
    const outputArtifactIds = roles.map(role => graphArtifactId(operation.id, role));
    for (const [index, role] of roles.entries()) {
    state.artifacts[outputArtifactIds[index]] = {
        id: outputArtifactIds[index],
        role,
        branch: role === "object" || role === "stars" ? role : "diagnostic",
        domain: "linear",
        geometryId: input.geometryId || "geometry-source",
        immutable: true,
        parentOperationId: operation.id,
        parentArtifactIds: [input.id],
        separationId,
      };
    }
    state.branchSets[separationId] = {
      id: separationId,
      sourceArtifactId: input.id,
      objectArtifactId: outputArtifactIds[0],
      starsArtifactId: outputArtifactIds[1],
      maskArtifactId: outputArtifactIds[2],
      residualArtifactId: outputArtifactIds[3],
    };
    state.heads.object = outputArtifactIds[0];
    state.heads.stars = outputArtifactIds[1];
    state.heads.recombined = null;
    state.operations[operation.id] = {
      ...graphClone(operation),
      inputArtifactIds: [input.id],
      outputArtifactIds,
      domain: "linear",
      atomic: true,
    };
  } else if (type === "star_recombination" || type === "recombine") {
    const [, object] = graphHeadForBranch(state, "object");
    const [, stars] = graphHeadForBranch(state, "stars");
    if (!object.separationId || object.separationId !== stars.separationId) {
      throw new Error("Objeto y Estrellas deben proceder de la misma separación.");
    }
    if (object.domain !== stars.domain) {
      throw new Error("No se puede mezclar una rama lineal con otra no lineal.");
    }
    if (
      (object.geometryId || "geometry-source")
      !== (stars.geometryId || "geometry-source")
    ) {
      throw new Error("Objeto y Estrellas no comparten la misma geometría global.");
    }
    const outputId = graphArtifactId(operation.id);
    state.artifacts[outputId] = {
      id: outputId,
      role: "recombined",
      branch: "recombined",
      domain: object.domain,
      geometryId: object.geometryId || "geometry-source",
      immutable: true,
      parentOperationId: operation.id,
      parentArtifactIds: [object.id, stars.id],
      separationId: object.separationId,
    };
    state.operations[operation.id] = {
      ...graphClone(operation),
      inputArtifactIds: [object.id, stars.id],
      outputArtifactIds: [outputId],
      domain: object.domain,
    };
    state.heads.recombined = outputId;
    state.heads.main = outputId;
  } else if (type === "crop") {
    const requestedBranch = operation.branch || "main";
    if (!["main", "combined", "global"].includes(requestedBranch)) {
      throw new Error(
        "El recorte es global: se aplica a Objeto, Estrellas, máscara y residual como una sola operación.",
      );
    }
    const geometryId = operation.geometryId || `geometry:${operation.id}`;
    const activeSeparation = state.heads.object
      ? state.artifacts[state.heads.object]?.separationId
      : null;
    const branchSet = activeSeparation ? state.branchSets[activeSeparation] : null;
    const roles = [
      ["main", state.heads.main],
      ["object", state.heads.object],
      ["stars", state.heads.stars],
      ["mask", branchSet?.maskArtifactId],
      ["residual", branchSet?.residualArtifactId],
    ].filter(([, artifactId]) => artifactId && state.artifacts[artifactId]);
    const outputArtifactIds = [];
    const outputByRole = {};
    for (const [role, artifactId] of roles) {
      const input = state.artifacts[artifactId];
      const outputId = graphArtifactId(operation.id, role);
      outputArtifactIds.push(outputId);
      outputByRole[role] = outputId;
      state.artifacts[outputId] = {
        ...graphClone(input),
        id: outputId,
        role: input.role,
        geometryId,
        immutable: true,
        parentOperationId: operation.id,
        parentArtifactIds: [input.id],
      };
    }
    if (outputByRole.main) state.heads.main = outputByRole.main;
    if (outputByRole.object) state.heads.object = outputByRole.object;
    if (outputByRole.stars) state.heads.stars = outputByRole.stars;
    if (outputByRole.main && state.heads.recombined) {
      state.heads.recombined = outputByRole.main;
    }
    if (branchSet) {
      state.branchSets[activeSeparation] = {
        ...branchSet,
        objectArtifactId: outputByRole.object || branchSet.objectArtifactId,
        starsArtifactId: outputByRole.stars || branchSet.starsArtifactId,
        maskArtifactId: outputByRole.mask || branchSet.maskArtifactId,
        residualArtifactId: outputByRole.residual || branchSet.residualArtifactId,
        geometryId,
      };
    }
    state.operations[operation.id] = {
      ...graphClone(operation),
      branch: "global",
      geometryId,
      atomic: true,
      inputArtifactIds: roles.map(([, artifactId]) => artifactId),
      outputArtifactIds,
      domain: state.artifacts[state.heads.main]?.domain || "linear",
    };
  } else {
    const [branch, input] = graphHeadForBranch(state, operation.branch || "main");
    if (type === "psf_deconvolution" && input.domain !== "linear") {
      throw new Error("La deconvolución PSF sólo puede aplicarse a datos lineales.");
    }
    if (
      type === "psf_deconvolution"
      && operation.psfArtifactId
      && operation.psfSourceArtifactId !== input.id
    ) {
      throw new Error("La PSF no fue medida sobre la geometría de esta revisión.");
    }
    const domain = GRAPH_LINEAR_MODULES.has(type) ? input.domain : "nonlinear";
    const outputId = graphArtifactId(operation.id);
    state.artifacts[outputId] = {
      id: outputId,
      role: input.role,
      branch,
      domain,
      geometryId: input.geometryId || "geometry-source",
      immutable: true,
      parentOperationId: operation.id,
      parentArtifactIds: [input.id],
      separationId: input.separationId || null,
    };
    state.operations[operation.id] = {
      ...graphClone(operation),
      inputArtifactIds: [input.id],
      outputArtifactIds: [outputId],
      domain,
    };
    state.heads[branch] = outputId;
    if (branch === "object" || branch === "stars") state.heads.recombined = null;
  }

  state.journal.push({
    operationId: operation.id,
    beforeHeads,
    afterHeads: graphClone(state.heads),
  });
  state.redo = [];
  return state;
}

export function undoDeepSkyStudioOperation(inputState) {
  const state = graphClone(inputState);
  const entry = state.journal.pop();
  if (!entry) return state;
  state.redo.push(entry);
  state.heads = graphClone(entry.beforeHeads);
  return state;
}

export function redoDeepSkyStudioOperation(inputState) {
  const state = graphClone(inputState);
  const entry = state.redo.pop();
  if (!entry) return state;
  state.journal.push(entry);
  state.heads = graphClone(entry.afterHeads);
  return state;
}

export function validateDeepSkyStudioGraph(state) {
  const errors = [];
  for (const [name, artifactId] of Object.entries(state?.heads || {})) {
    if (artifactId && !state.artifacts?.[artifactId]) {
      errors.push(`Head ${name} referencia un artefacto inexistente.`);
    }
  }
  for (const operation of Object.values(state?.operations || {})) {
    for (const parentId of operation.inputArtifactIds || []) {
      if (!state.artifacts?.[parentId]) {
        errors.push(`La operación ${operation.id} referencia ${parentId} inexistente.`);
      }
    }
  }
  const object = state?.heads?.object
    ? state.artifacts?.[state.heads.object]
    : null;
  const stars = state?.heads?.stars
    ? state.artifacts?.[state.heads.stars]
    : null;
  if (
    object
    && stars
    && (object.geometryId || "geometry-source")
      !== (stars.geometryId || "geometry-source")
  ) {
    errors.push("Objeto y Estrellas tienen geometrías incompatibles.");
  }
  return { valid: errors.length === 0, errors };
}

function graphVisit(state, artifactId, visited, order) {
  const artifact = state.artifacts[artifactId];
  if (!artifact || visited.has(artifactId)) return;
  for (const parentId of artifact.parentArtifactIds || []) {
    graphVisit(state, parentId, visited, order);
  }
  visited.add(artifactId);
  if (artifact.parentOperationId && !order.includes(artifact.parentOperationId)) {
    order.push(artifact.parentOperationId);
  }
}

export function buildDeepSkyStudioGraphRequest({
  state,
  outputIds,
  presentationMode: _presentationMode,
} = {}) {
  const selected = Array.isArray(outputIds) && outputIds.length
    ? outputIds
    : [state?.heads?.main].filter(Boolean);
  const order = [];
  const visited = new Set();
  for (const artifactId of selected) graphVisit(state, artifactId, visited, order);
  return {
    schema: "zenith-deepsky-poststack-recipe-v2",
    source: graphClone(state?.source || {}),
    operations: order.map(id => graphClone(state.operations[id])),
    outputArtifactIds: graphClone(selected),
  };
}

export {
  SOURCE_KINDS,
  STEP_DEFINITIONS,
};
