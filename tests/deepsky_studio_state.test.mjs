import assert from "node:assert/strict";
import test from "node:test";

import {
  SOURCE_KINDS,
  STEP_DEFINITIONS,
  applyDeepSkyStudioOperation,
  buildDeepSkyStudioRequest,
  buildDeepSkyStudioGraphRequest,
  classifyDeepSkySource,
  createDeepSkyStudioState,
  getDeepSkyPaletteCandidates,
  planDeepSkyStudio,
  redoDeepSkyStudioOperation,
  undoDeepSkyStudioOperation,
  validateDeepSkyStudioGraph,
} from "../src/deepsky_studio_state.js";

const EXPECTED_STEPS = [
  "crop",
  "background_gradient",
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
];

test("classifies every supported source route from explicit scientific evidence", () => {
  assert.equal(classifyDeepSkySource({ captureMode: "broadband_rgb" }).kind, SOURCE_KINDS.BROADBAND_RGB);
  assert.equal(classifyDeepSkySource({ captureMode: "LRGB", components: ["L", "R", "G", "B"] }).kind, SOURCE_KINDS.MONO_BROADBAND);
  assert.equal(classifyDeepSkySource({ isMono: true, filters: ["Ha", "OIII", "SII"] }).kind, SOURCE_KINDS.MONO_NARROWBAND);
  assert.equal(classifyDeepSkySource({ captureMode: "dual_band_osc", filterProfile: "Ha+OIII" }).kind, SOURCE_KINDS.OSC_DUAL_BAND);
  assert.equal(classifyDeepSkySource({
    groups: [
      { filterProfile: "HA_OIII", componentFilters: ["HA", "OIII"] },
      { filterProfile: "SII_OIII", componentFilters: ["SII", "OIII"] },
    ],
  }).kind, SOURCE_KINDS.MULTI_FILTER_DUAL_BAND);
  assert.equal(classifyDeepSkySource({ alreadyCombined: true, components: ["R", "G", "B"] }).kind, SOURCE_KINDS.ALREADY_COMBINED);
  assert.equal(classifyDeepSkySource({}).kind, SOURCE_KINDS.UNKNOWN);
});

test("classifies the camel-case source kinds emitted by the Rust standalone loader", () => {
  assert.equal(
    classifyDeepSkySource({ kind: "rgbBroadband", channels: 3 }).kind,
    SOURCE_KINDS.BROADBAND_RGB,
  );
  assert.equal(
    classifyDeepSkySource({ kind: "mono", channels: 1 }).kind,
    SOURCE_KINDS.MONO_BROADBAND,
  );
  assert.equal(
    classifyDeepSkySource({
      kind: "dualBand",
      channels: 3,
      componentFilters: ["HA", "OIII"],
    }).kind,
    SOURCE_KINDS.OSC_DUAL_BAND,
  );
});

test("keeps the visible scientific workflow in its required order", () => {
  assert.deepEqual(STEP_DEFINITIONS.map(([id]) => id), EXPECTED_STEPS);
  const plan = planDeepSkyStudio({ captureMode: "broadband_rgb", wcsValid: true });
  assert.deepEqual(plan.steps.map(step => step.id), EXPECTED_STEPS);
  assert.equal(plan.steps.find(step => step.id === "background_gradient").recommended, true);
  assert.match(
    plan.steps.find(step => step.id === "background_gradient").reason,
    /una sola operación/i,
  );
  assert.equal(plan.steps.find(step => step.id === "astrometry").recommended, false);
});

test("a single mono master does not pretend to contain an LRGB combination", () => {
  const plan = planDeepSkyStudio({ isMono: true, filter: "L", channels: 1 });
  const color = plan.steps.find(step => step.id === "channels_color");
  assert.equal(plan.source.kind, SOURCE_KINDS.MONO_BROADBAND);
  assert.equal(color.applies, false);
  assert.match(color.reason, /máster mono aislado/i);
});

test("a single mono narrowband master never invents independent palette channels", () => {
  const descriptor = {
    sourceType: "mono_narrowband",
    channels: 1,
    path: "/masters/one-mono-master.fits",
    filterProfile: "Ha+OIII",
    components: ["HA", "OIII"],
  };
  const plan = planDeepSkyStudio(descriptor);

  assert.equal(plan.source.kind, SOURCE_KINDS.MONO_NARROWBAND);
  assert.equal(
    plan.palettes.some(palette => palette.scientificEligible),
    false,
    "metadata labels on one mono file cannot become independent spectral data",
  );
  const hoo = plan.palettes.find(palette => palette.id === "hooNatural");
  assert.equal(hoo.requiresIndependentMasters, true);
  assert.match(hoo.reason, /máster lineal distinto|canales espectrales independientes/i);
  const color = plan.steps.find(step => step.id === "channels_color");
  assert.equal(color.blocked, true);
  assert.match(color.reason, /máster lineal distinto|canales espectrales independientes/i);
});

test("mono narrowband palettes unlock only after distinct channel masters are added", () => {
  const sharedPath = getDeepSkyPaletteCandidates({
    sourceType: "mono_narrowband",
    channels: 1,
    haPaths: ["/masters/shared.fits"],
    oiiiPaths: ["/masters/shared.fits"],
  });
  assert.equal(
    sharedPath.find(palette => palette.id === "hooNatural").scientificEligible,
    false,
    "one file assigned twice is still one mono source",
  );

  const palettes = getDeepSkyPaletteCandidates({
    sourceType: "mono_narrowband",
    channels: 1,
    haPaths: ["/masters/ha.fits"],
    oiiiPaths: ["/masters/oiii.fits"],
  });
  const byId = Object.fromEntries(palettes.map(palette => [palette.id, palette]));

  assert.equal(byId.hooNatural.scientificEligible, true);
  assert.equal(byId.hooTeal.scientificEligible, true);
  assert.equal(byId.sho.scientificEligible, false);
  assert.deepEqual(byId.sho.missingComponents, ["SII"]);
});

test("a Ha plus OIII OSC source offers HOO but not palettes that require SII", () => {
  const palettes = getDeepSkyPaletteCandidates({
    captureMode: "dual_band_osc",
    filterProfile: "Ha+OIII",
  });
  const byId = Object.fromEntries(palettes.map(palette => [palette.id, palette]));

  assert.equal(byId.hooNatural.eligible, true);
  assert.equal(byId.hooTeal.eligible, true);
  assert.equal(byId.sho.eligible, false);
  assert.deepEqual(byId.sho.missingComponents, ["SII"]);
  assert.equal(byId.soo.eligible, false);
});

test("duplicated OIII is blocked until both filter sessions are reconciled", () => {
  const descriptor = {
    groups: [
      { filterProfile: "Ha+OIII" },
      { filterProfile: "SII+OIII" },
    ],
    instrumentProfile: { complete: true },
  };
  const plan = planDeepSkyStudio(descriptor);

  assert.equal(plan.source.kind, SOURCE_KINDS.MULTI_FILTER_DUAL_BAND);
  assert.deepEqual(plan.source.duplicatedComponents, ["OIII"]);
  assert.equal(plan.requiresOiiiReconciliation, true);
  for (const palette of plan.palettes) {
    assert.equal(palette.eligible, false, `${palette.id} must not silently choose one OIII`);
    assert.equal(palette.requiresOiiiReconciliation, true);
    assert.match(palette.reason, /reconciliar/i);
  }
  const color = plan.steps.find(step => step.id === "channels_color");
  assert.equal(color.blocked, true);
});

test("reconciled double dual-band enables the implemented HOO, SHO, HSO and SOO palettes", () => {
  const source = {
    groups: [{ filterProfile: "Ha+OIII" }, { filterProfile: "SII+OIII" }],
    oiiiReconciled: true,
  };
  const withoutProfile = Object.fromEntries(
    getDeepSkyPaletteCandidates(source).map(palette => [palette.id, palette]),
  );
  assert.equal(withoutProfile.hooNatural.eligible, true);
  assert.equal(withoutProfile.hooTeal.eligible, true);
  assert.equal(withoutProfile.sho.eligible, true);
  assert.equal(withoutProfile.hso.eligible, true);
  assert.equal(withoutProfile.soo.eligible, true);
  assert.equal("foraxx" in withoutProfile, false);
});

test("PCC makes astrometry required only while WCS remains unvalidated", () => {
  const settings = { channels_color: { mode: "pcc" } };
  const unresolved = planDeepSkyStudio(
    { captureMode: "broadband_rgb", wcsValid: false },
    settings,
  );
  const resolved = planDeepSkyStudio(
    { captureMode: "broadband_rgb", wcsValid: true },
    settings,
  );
  assert.equal(unresolved.steps.find(step => step.id === "astrometry").required, true);
  assert.equal(resolved.steps.find(step => step.id === "astrometry").required, false);
});

test("disabling stretch preserves a linear export and gates nonlinear detail and finish", () => {
  const plan = planDeepSkyStudio(
    { captureMode: "broadband_rgb" },
    { stretch: { enabled: false } },
  );
  assert.equal(plan.steps.find(step => step.id === "stretch").applies, true);
  assert.equal(plan.steps.find(step => step.id === "curves_color").applies, false);
  assert.equal(plan.steps.find(step => step.id === "detail").applies, false);
  assert.equal(plan.steps.find(step => step.id === "finish").applies, false);
  assert.equal(plan.steps.find(step => step.id === "export").required, true);
});

test("Quick and Expert emit identical requests for identical settings", () => {
  const source = {
    captureMode: "dual_band_osc",
    filterProfile: "Ha+OIII",
    path: "/masters/ha-oiii.fits",
  };
  const settings = {
    crop: { enabled: true, left: 12, top: 8, width: 4000, height: 2800 },
    background_gradient: { enabled: true, mode: "adaptive_samples" },
    channels_color: { enabled: true, palette: "hooNatural" },
    linear_denoise: { enabled: true, strength: 0.42 },
    stretch: { enabled: true, mode: "adaptive_ghs" },
  };

  const quick = buildDeepSkyStudioRequest({ source, settings, presentationMode: "quick" });
  const expert = buildDeepSkyStudioRequest({ source, settings, presentationMode: "expert" });
  assert.deepEqual(quick, expert);
  assert.equal("presentationMode" in quick, false);
  assert.equal(quick.source.references[0], "/masters/ha-oiii.fits");
  assert.deepEqual(
    quick.operations.map(operation => operation.id),
    ["crop", "background_gradient", "channels_color", "linear_denoise", "stretch", "export"],
  );
});

test("camel-case UI settings are normalized to the canonical step ids", () => {
  const request = buildDeepSkyStudioRequest({
    source: { captureMode: "broadband_rgb" },
    settings: {
      backgroundGradient: { enabled: true, mode: "adaptive" },
      channelsColor: { enabled: true, mode: "pcc" },
      linearDenoise: { enabled: false },
    },
  });

  assert.deepEqual(
    request.operations.map(operation => operation.id),
    ["background_gradient", "channels_color", "export"],
  );
});

test("request construction never mutates source descriptors or settings", () => {
  const source = {
    captureMode: "broadband_rgb",
    paths: ["/masters/rgb.fits"],
  };
  const settings = {
    background_gradient: { enabled: true, samples: [{ x: 0.2, y: 0.4 }] },
  };
  const sourceBefore = structuredClone(source);
  const settingsBefore = structuredClone(settings);
  const request = buildDeepSkyStudioRequest({ source, settings, presentationMode: "expert" });

  request.source.references.push("/mutated.fits");
  request.operations[0].settings.enabled = false;
  assert.deepEqual(source, sourceBefore);
  assert.deepEqual(settings, settingsBefore);
});

test("star separation publishes object, stars, mask and residual atomically without mutating the source", () => {
  const source = {
    path: "/masters/immutable-linear-master.fits",
    metadata: { channels: 3, bitDepth: 32 },
  };
  const sourceBefore = structuredClone(source);
  const initial = createDeepSkyStudioState(source);
  const separated = applyDeepSkyStudioOperation(initial, {
    id: "separate-1",
    type: "star_separation",
    settings: { sensitivity: 0.63 },
  });

  assert.deepEqual(source, sourceBefore);
  assert.deepEqual(initial.source, sourceBefore);
  assert.equal(initial.heads.object, null, "the input state stays untouched");
  assert.equal(separated.artifacts["artifact-source"].immutable, true);
  assert.deepEqual(
    separated.operations["separate-1"].outputArtifactIds,
    [
      "separate-1:object",
      "separate-1:stars",
      "separate-1:mask",
      "separate-1:residual",
    ],
  );
  assert.equal(separated.operations["separate-1"].atomic, true);
  assert.deepEqual(separated.branchSets["separate-1"], {
    id: "separate-1",
    sourceArtifactId: "artifact-source",
    objectArtifactId: "separate-1:object",
    starsArtifactId: "separate-1:stars",
    maskArtifactId: "separate-1:mask",
    residualArtifactId: "separate-1:residual",
  });
  assert.equal(separated.heads.object, "separate-1:object");
  assert.equal(separated.heads.stars, "separate-1:stars");
  assert.equal(separated.heads.main, "artifact-source");
  assert.equal(validateDeepSkyStudioGraph(separated).valid, true);
});

test("editing the object branch never advances the stars, main or source heads", () => {
  const initial = createDeepSkyStudioState({ path: "/masters/source.fits" });
  const separated = applyDeepSkyStudioOperation(initial, {
    id: "separate-branches",
    type: "star_layers",
  });
  const edited = applyDeepSkyStudioOperation(separated, {
    id: "restore-object",
    type: "psf_deconvolution",
    branch: "object",
    settings: { iterations: 12, regularization: 0.18 },
  });

  assert.equal(edited.heads.object, "restore-object:output");
  assert.equal(edited.heads.stars, separated.heads.stars);
  assert.equal(edited.heads.main, separated.heads.main);
  assert.equal(edited.artifacts["restore-object:output"].parentArtifactIds[0], separated.heads.object);
  assert.equal(edited.artifacts["restore-object:output"].separationId, "separate-branches");
  assert.equal(edited.artifacts["artifact-source"].parentOperationId, null);
  assert.equal(edited.artifacts["artifact-source"].immutable, true);
});

test("PSF deconvolution rejects a nonlinear revision", () => {
  const initial = createDeepSkyStudioState({ path: "/masters/source.fits" });
  const stretched = applyDeepSkyStudioOperation(initial, {
    id: "stretch-main",
    type: "stretch",
    settings: { mode: "adaptive_ghs" },
  });

  assert.equal(stretched.artifacts[stretched.heads.main].domain, "nonlinear");
  assert.throws(
    () => applyDeepSkyStudioOperation(stretched, {
      id: "invalid-deconvolution",
      type: "psf_deconvolution",
      branch: "main",
    }),
    /sólo puede aplicarse a datos lineales/i,
  );
});

test("recombination rejects unrelated separations and mixed processing domains", () => {
  const initial = createDeepSkyStudioState({ path: "/masters/source.fits" });
  const first = applyDeepSkyStudioOperation(initial, {
    id: "separate-first",
    type: "star_separation",
  });
  const second = applyDeepSkyStudioOperation(first, {
    id: "separate-second",
    type: "star_separation",
    branch: "main",
  });
  const unrelated = structuredClone(second);
  unrelated.heads.stars = first.heads.stars;

  assert.throws(
    () => applyDeepSkyStudioOperation(unrelated, {
      id: "recombine-unrelated",
      type: "recombine",
    }),
    /misma separación/i,
  );

  const objectStretched = applyDeepSkyStudioOperation(first, {
    id: "stretch-object",
    type: "stretch",
    branch: "object",
  });
  assert.throws(
    () => applyDeepSkyStudioOperation(objectStretched, {
      id: "recombine-mixed-domain",
      type: "star_recombination",
    }),
    /rama lineal con otra no lineal/i,
  );
});

test("crop is one atomic geometry operation for main, object, stars, mask and residual", () => {
  const initial = createDeepSkyStudioState({ path: "/masters/source.fits" });
  const separated = applyDeepSkyStudioOperation(initial, {
    id: "separate-before-crop",
    type: "star_separation",
  });
  const cropped = applyDeepSkyStudioOperation(separated, {
    id: "crop-global",
    type: "crop",
    branch: "global",
    settings: { x: 0.08, y: 0.04, width: 0.86, height: 0.9 },
  });
  const geometry = cropped.artifacts[cropped.heads.main].geometryId;
  const set = cropped.branchSets["separate-before-crop"];

  assert.match(geometry, /^geometry:crop-global$/);
  assert.equal(cropped.artifacts[cropped.heads.object].geometryId, geometry);
  assert.equal(cropped.artifacts[cropped.heads.stars].geometryId, geometry);
  assert.equal(cropped.artifacts[set.maskArtifactId].geometryId, geometry);
  assert.equal(cropped.artifacts[set.residualArtifactId].geometryId, geometry);
  assert.equal(cropped.operations["crop-global"].atomic, true);
  assert.equal(validateDeepSkyStudioGraph(cropped).valid, true);
});

test("crop rejects an isolated layer so separated branches can never drift", () => {
  const separated = applyDeepSkyStudioOperation(
    createDeepSkyStudioState({ path: "/masters/source.fits" }),
    { id: "separate-crop-guard", type: "star_separation" },
  );
  assert.throws(
    () => applyDeepSkyStudioOperation(separated, {
      id: "crop-stars-only",
      type: "crop",
      branch: "stars",
    }),
    /recorte es global/i,
  );
});

test("recombination rejects branches with mismatched geometry ids", () => {
  const separated = applyDeepSkyStudioOperation(
    createDeepSkyStudioState({ path: "/masters/source.fits" }),
    { id: "separate-geometry", type: "star_separation" },
  );
  const mismatched = structuredClone(separated);
  mismatched.artifacts[mismatched.heads.stars].geometryId = "geometry:foreign";
  assert.throws(
    () => applyDeepSkyStudioOperation(mismatched, {
      id: "recombine-geometry-mismatch",
      type: "recombine",
    }),
    /misma geometría global/i,
  );
});

test("Essential and Expert serialize the same scientific graph request", () => {
  let state = createDeepSkyStudioState({
    kind: "osc_dual_band",
    path: "/masters/ha-oiii.fits",
  });
  state = applyDeepSkyStudioOperation(state, {
    id: "separate-for-request",
    type: "star_separation",
    settings: { sensitivity: 0.6 },
  });
  state = applyDeepSkyStudioOperation(state, {
    id: "denoise-object",
    type: "linear_denoise",
    branch: "object",
    settings: { strength: 0.35 },
  });
  state = applyDeepSkyStudioOperation(state, {
    id: "recombine-for-request",
    type: "recombine",
    settings: { objectWeight: 1, starWeight: 0.9 },
  });

  const essential = structuredClone(state);
  essential.presentation.experience = "essential";
  essential.presentation.selectedArtifactId = essential.heads.recombined;
  const expert = structuredClone(state);
  expert.presentation.experience = "expert";
  expert.presentation.selectedArtifactId = expert.heads.object;

  const essentialRequest = buildDeepSkyStudioGraphRequest({
    state: essential,
    presentationMode: "essential",
  });
  const expertRequest = buildDeepSkyStudioGraphRequest({
    state: expert,
    presentationMode: "expert",
  });

  assert.deepEqual(essentialRequest, expertRequest);
  assert.equal("presentation" in essentialRequest, false);
  assert.deepEqual(
    essentialRequest.operations.map(operation => operation.id),
    ["separate-for-request", "denoise-object", "recombine-for-request"],
  );
  assert.deepEqual(essentialRequest.outputArtifactIds, ["recombine-for-request:output"]);
});

test("undo and redo restore every branch head without altering graph artifacts", () => {
  const initial = createDeepSkyStudioState({ path: "/masters/source.fits" });
  const separated = applyDeepSkyStudioOperation(initial, {
    id: "separate-undo",
    type: "star_separation",
  });
  const edited = applyDeepSkyStudioOperation(separated, {
    id: "denoise-object-undo",
    type: "linear_denoise",
    branch: "object",
  });
  const artifactCount = Object.keys(edited.artifacts).length;

  const undone = undoDeepSkyStudioOperation(edited);
  assert.deepEqual(undone.heads, separated.heads);
  assert.equal(undone.redo.length, 1);
  assert.equal(Object.keys(undone.artifacts).length, artifactCount);

  const redone = redoDeepSkyStudioOperation(undone);
  assert.deepEqual(redone.heads, edited.heads);
  assert.equal(redone.journal.length, edited.journal.length);
  assert.equal(redone.redo.length, 0);
  assert.equal(validateDeepSkyStudioGraph(redone).valid, true);
});
