import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  applyMilkyWayEditorSafetyPlan,
  attachMilkyWayOutputIdentity,
  buildMilkyWayAtomicCropRequest,
  describeMilkyWayGeometryRevision,
  milkyWayEditorShouldAutoApplyStep,
  milkyWayLayerPaths,
  milkyWayPrimaryPath,
  normalizedMilkyWayCropToPixels,
  preserveMilkyWayRecipeIdentity,
  resolveMilkyWayResultPath,
} from "../src/milky_way_editor_bridge.js";

test("resolves the typed Rust result and its nested output inventory", () => {
  const result = {
    skyPath: "/stack/Cielo.fits",
    outputs: {
      composite: "/stack/Compuesto.fits",
      skyMask: "/stack/Mascara.fits",
      skyVariance: "/stack/Varianza_Cielo.fits",
      groundVariance: "/stack/Varianza_Suelo.fits",
      skyCoverage: "/stack/Cobertura.fits",
      groundCoverage: "/stack/Cobertura_Suelo.fits",
      skyRejection: "/stack/Rechazo.fits",
      groundRejection: "/stack/Rechazo_Suelo.fits",
      recipe: "/stack/Receta.json",
    },
  };
  assert.equal(resolveMilkyWayResultPath(result, "sky"), "/stack/Cielo.fits");
  assert.equal(resolveMilkyWayResultPath(result, "composite"), "/stack/Compuesto.fits");
  assert.equal(resolveMilkyWayResultPath(result, "mask"), "/stack/Mascara.fits");
  assert.equal(resolveMilkyWayResultPath(result, "skyVariance"), "/stack/Varianza_Cielo.fits");
  assert.equal(resolveMilkyWayResultPath(result, "groundCoverage"), "/stack/Cobertura_Suelo.fits");
  assert.equal(resolveMilkyWayResultPath(result, "groundRejection"), "/stack/Rechazo_Suelo.fits");
  assert.equal(resolveMilkyWayResultPath(result, "recipe"), "/stack/Receta.json");
  assert.deepEqual(Object.keys(milkyWayLayerPaths(result)), [
    "sky",
    "composite",
    "mask",
    "skyVariance",
    "groundVariance",
    "skyCoverage",
    "groundCoverage",
    "skyRejection",
    "groundRejection",
  ]);
});

test("sky-only output is a valid Studio primary", () => {
  const result = { skyPath: "/stack/Cielo.fits", compositePath: null };
  assert.equal(milkyWayPrimaryPath(result), "/stack/Cielo.fits");
});

test("resolves camel-case scientific layers from an array product inventory", () => {
  const result = {
    products: [
      { kind: "skyVariance", path: "/stack/Varianza_Cielo.fits" },
      { product: "groundCoverage", fits_path: "/stack/Cobertura_Suelo.fits" },
    ],
  };
  assert.equal(resolveMilkyWayResultPath(result, "skyVariance"), "/stack/Varianza_Cielo.fits");
  assert.equal(resolveMilkyWayResultPath(result, "groundCoverage"), "/stack/Cobertura_Suelo.fits");
});

test("normalizes one crop window without leaving the source geometry", () => {
  assert.deepEqual(
    normalizedMilkyWayCropToPixels({ x: 0.1, y: 0.2, width: 0.5, height: 0.6 }, 100, 50),
    { left: 10, top: 10, width: 50, height: 30 },
  );
  assert.deepEqual(
    normalizedMilkyWayCropToPixels({ x: 0.98, y: 0.98, width: 0.5, height: 0.5 }, 100, 50),
    { left: 98, top: 49, width: 2, height: 1 },
  );
});

test("atomic crop request applies the same integer window to every branch", () => {
  const request = buildMilkyWayAtomicCropRequest({
    sky: "/stack/Cielo.fits",
    ground: "/stack/Suelo.fits",
    composite: "/stack/Compuesto.fits",
    mask: "/stack/Mascara.fits",
    skyVariance: "/stack/Varianza_Cielo.fits",
    groundVariance: "/stack/Varianza_Suelo.fits",
    skyCoverage: "/stack/Cobertura_Cielo.fits",
    groundCoverage: "/stack/Cobertura_Suelo.fits",
    skyRejection: "/stack/Rechazo_Cielo.fits",
    groundRejection: "/stack/Rechazo_Suelo.fits",
  }, { x: 0.25, y: 0.1, width: 0.5, height: 0.8 }, 400, 200);
  assert.deepEqual(request, {
    skyPath: "/stack/Cielo.fits",
    groundPath: "/stack/Suelo.fits",
    compositePath: "/stack/Compuesto.fits",
    maskPath: "/stack/Mascara.fits",
    skyVariancePath: "/stack/Varianza_Cielo.fits",
    groundVariancePath: "/stack/Varianza_Suelo.fits",
    skyCoveragePath: "/stack/Cobertura_Cielo.fits",
    groundCoveragePath: "/stack/Cobertura_Suelo.fits",
    skyRejectionPath: "/stack/Rechazo_Cielo.fits",
    groundRejectionPath: "/stack/Rechazo_Suelo.fits",
    left: 100,
    top: 20,
    width: 200,
    height: 160,
  });
});

test("sky-only crop keeps optional ground and composite absent", () => {
  const request = buildMilkyWayAtomicCropRequest({
    sky: "/stack/Cielo.fits",
    mask: "/stack/Mascara.fits",
    skyVariance: "/stack/Varianza_Cielo.fits",
    skyCoverage: "/stack/Cobertura_Cielo.fits",
    skyRejection: "/stack/Rechazo_Cielo.fits",
  }, { x: 0, y: 0, width: 1, height: 1 }, 120, 80);
  assert.equal(request.groundPath, null);
  assert.equal(request.compositePath, null);
  assert.equal(request.groundVariancePath, null);
  assert.equal(request.groundCoveragePath, null);
  assert.equal(request.groundRejectionPath, null);
  assert.deepEqual(
    { left: request.left, top: request.top, width: request.width, height: request.height },
    { left: 0, top: 0, width: 120, height: 80 },
  );
});

test("crop is blocked when a required scientific map is absent", () => {
  assert.throws(() => buildMilkyWayAtomicCropRequest({
    sky: "/stack/Cielo.fits",
    mask: "/stack/Mascara.fits",
  }, { x: 0, y: 0, width: 1, height: 1 }, 100, 100), /skyVariance, skyCoverage, skyRejection/);
});

test("ground branch cannot be cropped without its variance, coverage and rejection maps", () => {
  assert.throws(() => buildMilkyWayAtomicCropRequest({
    sky: "/stack/Cielo.fits",
    ground: "/stack/Suelo.fits",
    mask: "/stack/Mascara.fits",
    skyVariance: "/stack/Varianza_Cielo.fits",
    skyCoverage: "/stack/Cobertura_Cielo.fits",
    skyRejection: "/stack/Rechazo_Cielo.fits",
  }, { x: 0, y: 0, width: 1, height: 1 }, 100, 100), /groundVariance, groundCoverage, groundRejection/);
});

test("derived geometry keeps immutable root paths and replaces all visible layers", () => {
  const original = {
    workflow: "milky_way",
    milkyWayLayers: {
      sky: "/original/Cielo.fits",
      ground: "/original/Suelo.fits",
      composite: "/original/Compuesto.fits",
      mask: "/original/Mascara.fits",
      skyVariance: "/original/Varianza_Cielo.fits",
      groundVariance: "/original/Varianza_Suelo.fits",
      skyCoverage: "/original/Cobertura_Cielo.fits",
      groundCoverage: "/original/Cobertura_Suelo.fits",
      skyRejection: "/original/Rechazo_Cielo.fits",
      groundRejection: "/original/Rechazo_Suelo.fits",
    },
    wcsValid: true,
    recipePath: "/original/Receta.json",
  };
  const derived = describeMilkyWayGeometryRevision(original, {
    geometryId: "mw-crop-1",
    sourceWidth: 400,
    sourceHeight: 200,
    width: 300,
    height: 150,
    skyPath: "/derived/Cielo.fits",
    groundPath: "/derived/Suelo.fits",
    compositePath: "/derived/Compuesto.fits",
    maskPath: "/derived/Mascara.fits",
    skyVariancePath: "/derived/Varianza_Cielo.fits",
    groundVariancePath: "/derived/Varianza_Suelo.fits",
    skyCoveragePath: "/derived/Cobertura_Cielo.fits",
    groundCoveragePath: "/derived/Cobertura_Suelo.fits",
    skyRejectionPath: "/derived/Rechazo_Cielo.fits",
    groundRejectionPath: "/derived/Rechazo_Suelo.fits",
    recipePath: "/derived/Receta.json",
    wcsPreserved: true,
  });
  assert.deepEqual(derived.milkyWayRoot.layers, original.milkyWayLayers);
  assert.equal(derived.milkyWayLayers.composite, "/derived/Compuesto.fits");
  assert.equal(derived.milkyWayLayers.groundVariance, "/derived/Varianza_Suelo.fits");
  assert.equal(derived.milkyWayLayers.skyCoverage, "/derived/Cobertura_Cielo.fits");
  assert.equal(derived.milkyWayGeometryId, "mw-crop-1");
  assert.equal(derived.path, "/derived/Compuesto.fits");
  assert.equal(derived.milkyWayRoot.recipePath, "/original/Receta.json");
  assert.equal(derived.recipePath, "/derived/Receta.json");
  assert.equal(derived.wcsValid, true);
  assert.equal(derived.milkyWayWcsGeometryId, "mw-crop-1");
});

function studioPlanFixture() {
  const ids = [
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
  return {
    source: { kind: "broadband_rgb" },
    steps: ids.map((id, index) => ({
      id,
      order: index + 1,
      applies: true,
      required: id === "export",
      recommended: !["crop", "curves_color", "detail", "finish"].includes(id),
      reason: `generic:${id}`,
    })),
  };
}

test("Milky Way safety removes mask-unaware tools from Essential auto flow", () => {
  const plan = applyMilkyWayEditorSafetyPlan(studioPlanFixture(), {
    workflow: "milky_way",
    milkyWayGeometryId: "mw-geometry-7",
    recipePath: "/stack/Receta_Via_Lactea.json",
    wcsValid: false,
  });
  assert.equal(plan.editTarget, "composite");
  for (const id of [
    "background_gradient",
    "psf_deconvolution",
    "linear_denoise",
    "star_layers",
  ]) {
    const step = plan.steps.find(item => item.id === id);
    assert.equal(step.applies, true, `${id} stays manually available`);
    assert.equal(step.recommended, false, `${id} is not recommended`);
    assert.equal(step.required, false, `${id} is not required`);
    assert.equal(step.horizonMaskAware, false);
    assert.equal(milkyWayEditorShouldAutoApplyStep(step), false);
    assert.match(step.reason, /aún no consume la Máscara cielo-suelo/);
  }
  assert.equal(milkyWayEditorShouldAutoApplyStep(
    plan.steps.find(item => item.id === "astrometry"),
  ), true);
  assert.match(
    plan.steps.find(item => item.id === "astrometry").reason,
    /estrellas visibles del cielo.*geometryId.*mw-geometry-7/,
  );
});

test("Milky Way safety preserves an explicitly declared real mask-aware capability", () => {
  const plan = applyMilkyWayEditorSafetyPlan(studioPlanFixture(), {
    captureMode: "milky_way",
    milkyWayEditorCapabilities: {
      horizonMaskAware: { backgroundGradient: true },
    },
  });
  const background = plan.steps.find(item => item.id === "background_gradient");
  const denoise = plan.steps.find(item => item.id === "linear_denoise");
  assert.equal(background.horizonMaskAware, true);
  assert.equal(background.recommended, true);
  assert.equal(milkyWayEditorShouldAutoApplyStep(background), true);
  assert.equal(denoise.horizonMaskAware, false);
  assert.equal(milkyWayEditorShouldAutoApplyStep(denoise), false);
});

test("compound-derived steps and publication retain immutable Milky Way identity", () => {
  const descriptor = {
    workflow: "milky_way",
    milkyWayGeometryId: "mw-crop-42",
    milkyWayWcsGeometryId: "mw-crop-42",
    recipePath: "/derived/Receta_Via_Lactea.json",
    milkyWayRoot: {
      geometryId: "mw-root",
      recipePath: "/root/Receta_Via_Lactea.json",
      layers: { sky: "/root/Cielo.fits", ground: "/root/Suelo.fits" },
    },
    wcsValid: true,
  };
  const plan = applyMilkyWayEditorSafetyPlan(studioPlanFixture(), descriptor);
  for (const id of ["channels_color", "stretch", "curves_color", "detail", "finish", "export"]) {
    const step = plan.steps.find(item => item.id === id);
    assert.equal(step.target, "composite");
    assert.equal(step.immutableRoot, true);
  }
  assert.match(plan.steps.find(item => item.id === "export").reason, /recipePath.*mw-crop-42/);

  const recipe = preserveMilkyWayRecipeIdentity({
    schema: "zenith-deepsky-poststack-recipe-v1",
    source: { kind: "broadband_rgb", references: ["/derived/Compuesto.fits"] },
    operations: [],
  }, descriptor);
  assert.equal(recipe.workflow, "milky_way");
  assert.equal(recipe.source.editTarget, "composite");
  assert.equal(recipe.source.recipePath, "/derived/Receta_Via_Lactea.json");
  assert.equal(recipe.source.geometryId, "mw-crop-42");
  assert.equal(recipe.source.rootGeometryId, "mw-root");
  assert.deepEqual(recipe.source.immutableRootReferences, [
    "/root/Cielo.fits",
    "/root/Suelo.fits",
  ]);

  const annotation = attachMilkyWayOutputIdentity({ resultId: "ann-1" }, descriptor);
  assert.deepEqual(annotation.sourceIdentity, {
    workflow: "milky_way",
    editTarget: "composite",
    recipePath: "/derived/Receta_Via_Lactea.json",
    geometryId: "mw-crop-42",
    wcsGeometryId: "mw-crop-42",
  });
});

test("non-Milky-Way plans and recipes remain byte-for-byte untouched", () => {
  const plan = studioPlanFixture();
  const recipe = { source: { kind: "broadband_rgb" }, operations: [] };
  assert.equal(applyMilkyWayEditorSafetyPlan(plan, { workflow: "deep_sky" }), plan);
  assert.equal(preserveMilkyWayRecipeIdentity(recipe, { workflow: "deep_sky" }), recipe);
});

test("main wires the safety plan, optional auto-skip and geometry-bound annotations", () => {
  const main = readFileSync(new URL("../src/main.js", import.meta.url), "utf8");
  assert.match(main, /return applyMilkyWayEditorSafetyPlan\(plan, descriptor\)/);
  assert.match(main, /dsIsMilkyWayWorkflow\(\) && !milkyWayEditorShouldAutoApplyStep\(current\)/);
  assert.match(main, /Edición activa · Compuesto derivado; Cielo, Suelo y mapas · sólo lectura/);
  assert.match(main, /milkyWayWcsGeometryId !== geometryId/);
  assert.match(main, /attachMilkyWayOutputIdentity/);
  assert.doesNotMatch(main, /Vía Láctea[^\n]{0,120}(?:máscara protegida|protección por máscara)/i);
});
