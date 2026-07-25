import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { PostProcessSession, recipesEqual, unwrapPreviewReference } from "../src/postprocess_session.js";
import { evaluateGuide } from "../src/zenith_guide.js";
import {
  adaptSolarPreset,
  cloneSolarPreset,
  evaluateSolarCurve,
  evaluateToneCurve,
  normalizeSolarCurvePoints,
  normalizeToneCurvePoints,
  resolveToneCurveGeometry,
  toneCurvePointFromClient,
} from "../src/solar_postprocess.js";
import { resolvePostprocessHelp } from "../src/postprocess_help.js";
import {
  adaptObjectFinishingPreset,
  cloneObjectFinishingPreset,
  objectPresetApplicable,
} from "../src/object_postprocess_presets.js";
import { normalizeAdaptivePostprocessAnalysis } from "../src/adaptive_postprocess.js";

test("a new result atomically discards the previous history", () => {
  const session = new PostProcessSession();
  session.beginResult({ generation: 1, source: "stack", recipe: { gamma: 1 }, preview: "a" });
  session.commit({ gamma: 1.2 }, { preview: "b" });
  session.beginResult({ generation: 2, source: "mosaic", recipe: { gamma: 1 }, preview: "c" });
  assert.deepEqual(session.getState(), {
    generation: 2,
    source: "mosaic",
    index: 0,
    length: 1,
    canUndo: false,
    canRedo: false,
    canCompare: false,
  });
  assert.equal(session.current().preview, "c");
});

test("undo, redo and divergent edits behave predictably", () => {
  const session = new PostProcessSession();
  session.beginResult({ generation: 1, recipe: { gamma: 1 }, preview: "a" });
  session.commit({ gamma: 1.1 }, { preview: "b" });
  session.commit({ gamma: 1.2 }, { preview: "c" });
  assert.equal(session.undo().recipe.gamma, 1.1);
  assert.equal(session.redo().recipe.gamma, 1.2);
  session.undo();
  session.commit({ gamma: 0.9 }, { preview: "d" });
  assert.equal(session.canRedo(), false);
  assert.equal(session.getCompareEntry("source").preview, "a");
  assert.equal(session.getCompareEntry("previous").preview, "b");
});

test("undo and redo restore the exact recipe and preview in both directions", () => {
  const session = new PostProcessSession();
  const original = { deconv: { s: 0, i: 0 }, advanced: { exposure: 0 } };
  const restored = { deconv: { s: 1.2, i: 11 }, advanced: { exposure: -0.12 } };
  session.beginResult({ generation: 1, recipe: original, preview: "original-16bit" });
  session.commit(restored, { preview: "deconvolved-16bit", label: "Deconvolución" });

  assert.deepEqual(session.undo().recipe, original);
  assert.equal(session.current().preview, "original-16bit");
  assert.deepEqual(session.redo().recipe, restored);
  assert.equal(session.current().preview, "deconvolved-16bit");
});

test("continuous edits coalesce without losing their A/B baseline", () => {
  const session = new PostProcessSession();
  session.beginResult({ generation: 1, recipe: { gamma: 1 }, preview: "original", label: "Apilado original" });
  session.commit({ gamma: 1.1 }, { preview: "first", label: "Tono profesional" });
  session.commit({ gamma: 1.2 }, { preview: "second", label: "Tono profesional" });

  assert.equal(session.getState().length, 2);
  assert.equal(session.current().preview, "second");
  assert.equal(session.getCompareEntry("previous").preview, "original");

  session.commit({ gamma: 1.2, deconv: 8 }, { preview: "deconv", label: "Deconvolución · Suave" });
  session.commit({ gamma: 1.3, deconv: 8 }, { preview: "tone-again", label: "Tono profesional" });
  assert.equal(session.getState().length, 4, "returning to the module later must remain undoable");
});

test("the immutable stacked source survives history eviction", () => {
  const session = new PostProcessSession({ limit: 4 });
  session.beginResult({ generation: 1, recipe: { step: 0 }, preview: "original", label: "Apilado original" });
  for (let step = 1; step <= 7; step += 1) {
    session.commit({ step }, { preview: `preview-${step}`, label: `Ajuste ${step}` });
  }

  assert.equal(session.getState().length, 4);
  assert.equal(session.getCompareEntry("source").preview, "original");
  assert.equal(session.current().preview, "preview-7");
  assert.equal(session.getCompareEntry("previous").preview, "preview-6");
});

test("A/B is unavailable at the original and accepts Tauri file envelopes", () => {
  const session = new PostProcessSession();
  session.beginResult({ generation: 1, recipe: { gamma: 1 }, preview: "file_path:/tmp/original.png" });
  assert.equal(session.getCompareEntry("previous"), null);
  assert.equal(session.getState().canCompare, false);
  session.commit({ gamma: 1.1 }, { preview: "/tmp/processed.png" });
  assert.equal(session.getState().canCompare, true);
  assert.equal(unwrapPreviewReference(session.getCompareEntry("source").preview), "/tmp/original.png");
});

test("recipe equality is independent of object key order", () => {
  assert.equal(recipesEqual({ b: 2, a: [1, 2] }, { a: [1, 2], b: 2 }), true);
});

test("guide prioritises context without changing data", () => {
  const suggestions = evaluateGuide({
    hasSource: true,
    hasAnalysis: true,
    hasResult: true,
    isMono: false,
    histogramAvailable: true,
    historyLength: 1,
    canCompare: false,
    shadowClip: 0.02,
    highlightClip: 0,
  });
  assert.deepEqual(suggestions.map((item) => item.id), ["clipping", "result-ready"]);
});

test("the assistant exposes A/B as an executable action when history exists", () => {
  const suggestions = evaluateGuide({
    hasSource: true,
    hasResult: true,
    histogramAvailable: true,
    historyLength: 2,
    canCompare: true,
    shadowClip: 0,
    highlightClip: 0,
    dynamicRange: 0.8,
  });
  const compare = suggestions.find((item) => item.id === "compare-ready");
  assert.equal(compare?.target, "#btn-post-compare");
  assert.equal(compare?.activate, true);
});

test("the assistant follows batch, mosaic and deep-sky workflow stages", () => {
  const batchScan = evaluateGuide({ flow: "batch", stage: "scan", hasSource: false, hasResult: false });
  assert.equal(batchScan[0]?.id, "batch-scan");
  assert.match(batchScan[0]?.message || "", /subcarpetas/i);

  const batchLoad = evaluateGuide({ flow: "batch", stage: "load", itemCount: 3, hasResult: false });
  assert.equal(batchLoad[0]?.id, "batch-load");
  assert.match(batchLoad[0]?.message || "", /3 videos detectados/i);

  const batch = evaluateGuide({ flow: "batch", stage: "analyze", hasSource: true, hasResult: false });
  assert.equal(batch[0]?.id, "batch-analyze");
  assert.equal(batch[0]?.target, "#btn-run-analysis");
  assert.equal(batch[0]?.activate, true);

  const mosaic = evaluateGuide({ flow: "mosaic", stage: "stack", hasResult: false });
  assert.equal(mosaic[0]?.id, "mosaic-stack");
  assert.equal(mosaic[0]?.target, "#btn-mosaic-stack-all");

  const deepSky = evaluateGuide({
    flow: "deepsky",
    stage: "blocked",
    workflowStep: 3,
    hasResult: false,
  });
  assert.equal(deepSky[0]?.id, "deepsky-step");
  assert.equal(deepSky[0]?.level, "warning");
});

test("assistant recommendations are actionable and dismissible per result", () => {
  const context = {
    hasSource: true,
    hasResult: true,
    histogramAvailable: true,
    historyLength: 1,
    canCompare: false,
    shadowClip: 0.03,
    highlightClip: 0,
    robustDynamicRange: 0.7,
    medianLevel: 0.2,
  };
  const clipping = evaluateGuide(context).find((item) => item.id === "clipping");
  assert.equal(clipping?.applyAction, "protect-range");
  assert.equal(clipping?.target, "#sl-level-mid");
  const dismissed = evaluateGuide(context, undefined, new Set(["clipping"]));
  assert.equal(dismissed.some((item) => item.id === "clipping"), false);
});

test("mono solar assistant offers an executable recipe and retires it after activation", () => {
  const context = {
    hasSource: true,
    hasResult: true,
    isMono: true,
    solarActive: false,
    histogramAvailable: true,
    historyLength: 1,
    canCompare: false,
    shadowClip: 0,
    highlightClip: 0,
    robustDynamicRange: 0.42,
    medianLevel: 0.2,
  };
  const suggestion = evaluateGuide(context).find((item) => item.id === "solar-mono-workflow");
  assert.equal(suggestion?.target, "#solar-mono-module");
  assert.equal(suggestion?.applyAction, "solar-auto");
  assert.equal(
    evaluateGuide({ ...context, solarActive: true }).some((item) => item.id === "solar-mono-workflow"),
    false,
  );
});

test("solar curves remain bounded and presets are independent copies", () => {
  const normalized = normalizeSolarCurvePoints([[0.8, 0.95], [0.2, 0.05]]);
  assert.deepEqual(normalized[0], [0, 0]);
  assert.deepEqual(normalized.at(-1), [1, 1]);
  assert.ok(evaluateSolarCurve(normalized, 0.5) > 0.05);
  assert.ok(evaluateSolarCurve(normalized, 0.5) < 0.95);
  assert.equal(evaluateSolarCurve([[0, 0], [1, 1]], 0.375), 0.375);
  const first = cloneSolarPreset("ha-gold");
  const second = cloneSolarPreset("ha-gold");
  first.curvePoints[1][1] = 1;
  assert.notEqual(first.curvePoints[1][1], second.curvePoints[1][1]);
  for (const name of ["ha-natural", "ha-gold", "ha-inverted", "chromosphere", "prominence", "dual-range", "filaments"]) {
    const preset = cloneSolarPreset(name);
    assert.ok(preset.backgroundProtect >= 0.98, `${name} must keep measured sky protected`);
    assert.ok(preset.highlightCompression > 0 && preset.highlightCompression <= 1);
    assert.ok(preset.curvePoints.at(-1)[1] < 1, `${name} must retain highlight headroom`);
  }
});

test("adaptive analysis responds to clipping, noise, ringing, and stable signal", () => {
  assert.equal(
    normalizeAdaptivePostprocessAnalysis({ histogram: {} }).measured,
    false,
    "an empty backend response must not be presented as a measured adaptation",
  );

  const stressed = normalizeAdaptivePostprocessAnalysis({
    histogram: {
      median: 48_000,
      percentileLow: 0,
      percentileHigh: 65_500,
      shadowClip: 0.02,
      highlightClip: 0.01,
      isMono: true,
    },
    artifacts: {
      sampledPixels: 100_000,
      hotPixels: 120,
      deadPixels: 80,
      ringingScore: 12,
      suggestedDenoise: 28,
    },
    capture: { qualityStability: 62 },
  });
  const clean = normalizeAdaptivePostprocessAnalysis({
    histogram: {
      median: 25_000,
      percentileLow: 1_200,
      percentileHigh: 55_000,
      shadowClip: 0,
      highlightClip: 0,
    },
    artifacts: {
      sampledPixels: 100_000,
      ringingScore: 0.2,
      suggestedDenoise: 1,
    },
    capture: { qualityStability: 96 },
  });
  assert.ok(stressed.highlightStress > clean.highlightStress);
  assert.ok(stressed.noiseStress > clean.noiseStress);
  assert.ok(stressed.detailConfidence < clean.detailConfidence);
  assert.ok(stressed.safeguards.includes("highlights"));
  assert.ok(stressed.safeguards.includes("noise"));
});

test("solar recipes adapt to the measured master without losing bounded headroom", () => {
  const baseline = cloneSolarPreset("ha-gold");
  const adapted = adaptSolarPreset("ha-gold", {
    histogram: {
      median: 46_000,
      percentileLow: 0,
      percentileHigh: 65_535,
      shadowClip: 0.04,
      highlightClip: 0.008,
      isMono: true,
    },
    artifacts: {
      sampledPixels: 100_000,
      ringingScore: 10,
      suggestedDenoise: 24,
    },
  });
  assert.equal(adapted.adaptation.measured, true);
  assert.ok(adapted.highlightProtect >= baseline.highlightProtect);
  assert.ok(adapted.highlightCompression >= baseline.highlightCompression);
  assert.ok(adapted.filamentAmount < baseline.filamentAmount);
  assert.ok(adapted.noiseGuard >= baseline.noiseGuard);
  assert.ok(adapted.curvePoints.at(-1)[1] < baseline.curvePoints.at(-1)[1]);
  assert.ok(adapted.curvePoints.every(([, y]) => y >= 0 && y <= 1));
});

test("a well-exposed master without measured clipping keeps its detail budget", () => {
  // Un p99.9 alto es lo NORMAL en un máster solar: el disco ocupa la parte alta
  // del rango. Medir el estrés de luces desde 0.88 saturaba a 1.0 sin un solo
  // píxel recortado y disparaba la protección máxima —compresión de altas,
  // recorte del extremo de la curva y menos color— sobre datos sanos. Ser
  // adaptativo tiene que significar conservar la calidad, no aplanarla.
  const input = (histogram) => ({
    histogram: { median: 30_000, percentileLow: 900, shadowClip: 0, isMono: true, ...histogram },
    artifacts: { sampledPixels: 100_000, ringingScore: 0.3, suggestedDenoise: 1 },
    capture: { qualityStability: 95 },
  });
  const healthy = normalizeAdaptivePostprocessAnalysis(
    input({ percentileHigh: 65_000, highlightClip: 0 }),
  );
  const clipped = normalizeAdaptivePostprocessAnalysis(
    input({ percentileHigh: 65_535, highlightClip: 0.01 }),
  );
  assert.ok(
    healthy.highlightStress < 0.35,
    `un máster sin recorte no puede recibir la protección máxima (${healthy.highlightStress})`,
  );
  assert.equal(clipped.highlightStress, 1, "el recorte medido sí es evidencia directa");
  assert.ok(!healthy.safeguards.includes("highlights"));

  const baseline = cloneSolarPreset("ha-gold");
  const adapted = adaptSolarPreset("ha-gold", input({ percentileHigh: 65_000, highlightClip: 0 }));
  assert.equal(adapted.adaptation.measured, true);
  assert.deepEqual(
    adapted.curvePoints.at(-1),
    baseline.curvePoints.at(-1),
    "sin recorte medido, el extremo de la curva no se recorta",
  );
  assert.ok(
    adapted.highlightCompression < baseline.highlightCompression + 0.06,
    "la compresión de altas no puede dispararse sobre un máster sano",
  );
  assert.ok(
    adapted.colorStrength > baseline.colorStrength * 0.94,
    "el color esperado del preset debe sobrevivir a la adaptación",
  );
});

test("object finishing presets separate natural and interpretive colour contracts", () => {
  const lunar = cloneObjectFinishingPreset("lunar-relief");
  const mineral = cloneObjectFinishingPreset("lunar-mineral");
  assert.equal(lunar.intent, "scientific");
  assert.equal(mineral.intent, "creative");
  assert.ok(lunar.pipeline.deconv.i >= 6);
  assert.equal(objectPresetApplicable(mineral, { isMono: true }).applicable, false);
  assert.equal(objectPresetApplicable(mineral, { isMono: false }).applicable, true);
  mineral.pipeline.w[0] = 99;
  assert.notEqual(cloneObjectFinishingPreset("lunar-mineral").pipeline.w[0], 99);
});

test("moon and planet recipes reduce risky detail and add measured protection", () => {
  const baseline = cloneObjectFinishingPreset("jupiter-natural");
  const adapted = adaptObjectFinishingPreset("jupiter-natural", {
    histogram: {
      median: 42_000,
      percentileLow: 20,
      percentileHigh: 65_500,
      shadowClip: 0.01,
      highlightClip: 0.006,
      isMono: false,
    },
    artifacts: {
      sampledPixels: 80_000,
      hotPixels: 120,
      deadPixels: 90,
      ringingScore: 11,
      suggestedDenoise: 25,
    },
    capture: { qualityStability: 68 },
  });
  assert.equal(adapted.adaptation.measured, true);
  assert.ok(adapted.pipeline.deconv.i < baseline.pipeline.deconv.i);
  assert.ok(adapted.pipeline.w[0] < baseline.pipeline.w[0]);
  assert.ok(adapted.pipeline.autoMask >= baseline.pipeline.autoMask);
  assert.ok(adapted.pipeline.masterDenoise >= baseline.pipeline.masterDenoise);
  assert.ok(adapted.pipeline.advanced.highlights <= baseline.pipeline.advanced.highlights);
  assert.ok(adapted.pipeline.blend >= 68 && adapted.pipeline.blend <= 100);
});

test("mineral moon scales colour from measured chroma instead of neutral noise", () => {
  const noisyNeutral = adaptObjectFinishingPreset("lunar-mineral", {
    histogram: {
      median: 30_000,
      percentileLow: 900,
      percentileHigh: 61_000,
      isMono: false,
    },
    artifacts: {
      sampledPixels: 100_000,
      chromaSampledPixels: 82_000,
      meanSaturation: 0.006,
      p95Saturation: 0.016,
      chromaticFraction: 0.03,
      colorFringeScore: 9,
      suggestedDenoise: 18,
    },
  });
  const chromaticMaster = adaptObjectFinishingPreset("lunar-mineral", {
    histogram: {
      median: 30_000,
      percentileLow: 900,
      percentileHigh: 61_000,
      isMono: false,
    },
    artifacts: {
      sampledPixels: 100_000,
      chromaSampledPixels: 82_000,
      meanSaturation: 0.055,
      p95Saturation: 0.16,
      chromaticFraction: 0.62,
      colorFringeScore: 2,
      suggestedDenoise: 3,
    },
  });
  assert.equal(noisyNeutral.adaptation.chromaMeasured, true);
  assert.ok(noisyNeutral.adaptation.safeguards.includes("chroma"));
  assert.ok(noisyNeutral.adaptation.colorScale < chromaticMaster.adaptation.colorScale);
  assert.ok(
    noisyNeutral.pipeline.advanced.vibrance
      < chromaticMaster.pipeline.advanced.vibrance,
  );
  // Relativo a la receta, no en absoluto: el contrato es "se repliega sobre
  // croma neutra y nunca supera lo que pide el preset", y así sobrevive a los
  // retoques de intensidad sin dejar de detectar un color desbocado.
  const recipeMax = Math.max(
    ...cloneObjectFinishingPreset("lunar-mineral").pipeline.advanced.hslSaturation,
  );
  const noisyMax = Math.max(...noisyNeutral.pipeline.advanced.hslSaturation);
  const chromaticMax = Math.max(...chromaticMaster.pipeline.advanced.hslSaturation);
  assert.ok(
    noisyMax <= recipeMax * 0.3,
    `sobre croma casi neutra el mineral debe replegarse (${noisyMax} de ${recipeMax})`,
  );
  assert.ok(
    chromaticMax <= recipeMax,
    "la adaptación nunca puede superar el color que declara la receta",
  );
  assert.ok(chromaticMax > noisyMax * 2);
});

test("the intense mineral moon reaches its colour without inventing it", () => {
  const chromaticMaster = {
    histogram: {
      median: 28_000, percentileLow: 800, percentileHigh: 62_000,
      shadowClip: 0, highlightClip: 0, isMono: false,
    },
    artifacts: {
      sampledPixels: 90_000, chromaSampledPixels: 90_000,
      meanSaturation: 0.07, p95Saturation: 0.2, chromaticFraction: 0.5,
      ringingScore: 0.4, suggestedDenoise: 2,
    },
    capture: { qualityStability: 94 },
  };
  const contained = adaptObjectFinishingPreset("lunar-mineral", chromaticMaster);
  const intense = adaptObjectFinishingPreset("lunar-mineral-intense", chromaticMaster);

  // La separación mineral que se pide (maria azules, tierras altas rojas) tiene
  // que sobrevivir a la adaptación, no quedarse en el techo de la receta suave.
  assert.ok(
    intense.adaptation.colorScale > contained.adaptation.colorScale,
    `la intensa debe sostener más color (${intense.adaptation.colorScale} vs ${contained.adaptation.colorScale})`,
  );
  assert.ok(intense.pipeline.advanced.vibrance > contained.pipeline.advanced.vibrance);
  const [red, , , green, , blue] = intense.pipeline.advanced.hslSaturation;
  const [containedRed] = contained.pipeline.advanced.hslSaturation;
  assert.ok(red > 0.5 && blue > 0.5, `rojo y azul deben quedar altos (${red}, ${blue})`);
  assert.ok(red > containedRed * 3, "la separación mineral debe ser claramente mayor que la contenida");
  // El mineral vive del color, no de apurar el detalle: saturar así hace visible
  // cualquier exceso de nitidez.
  assert.ok(intense.pipeline.w[0] < 4, `la nitidez base debe ser contenida (${intense.pipeline.w[0]})`);
  assert.ok(intense.pipeline.deconv.i <= 6);
  assert.equal(green, 0, "la Luna no tiene mineral verde: subirlo sólo amplifica ruido");

  // Pero sigue siendo una lectura de la croma MEDIDA: sobre un máster casi
  // neutro y ruidoso el color se retira solo.
  const neutralNoisy = adaptObjectFinishingPreset("lunar-mineral-intense", {
    histogram: { median: 30_000, percentileLow: 900, percentileHigh: 61_000, isMono: false },
    artifacts: {
      sampledPixels: 90_000, chromaSampledPixels: 90_000,
      meanSaturation: 0.002, p95Saturation: 0.006, chromaticFraction: 0.01,
      ringingScore: 9, suggestedDenoise: 26,
    },
    capture: { qualityStability: 60 },
  });
  assert.ok(
    neutralNoisy.adaptation.colorScale < intense.adaptation.colorScale,
    "sin croma medida el mineral intenso debe replegarse",
  );
  assert.equal(objectPresetApplicable(intense, { isMono: true }).applicable, false);
});

test("the shared tone curve is exact when linear and the assistant can propose it", () => {
  assert.equal(evaluateToneCurve(normalizeToneCurvePoints([[0, 0], [1, 1]]), 0.625), 0.625);
  const suggestion = evaluateGuide({
    hasResult: true,
    histogramAvailable: true,
    toneCurveActive: false,
    shadowClip: 0,
    highlightClip: 0,
    robustDynamicRange: 0.6,
  }).find((item) => item.id === "tone-curve-opportunity");
  assert.equal(suggestion?.target, "#tone-curve-free");
  assert.equal(suggestion?.applyAction, "tone-curve-auto");
});

test("tone-curve pointer geometry matches the visible canvas at narrow widths", () => {
  const geometry = resolveToneCurveGeometry(
    { left: 100, top: 40, width: 230, height: 172 },
    {
      borderLeft: "1px",
      borderRight: "1px",
      borderTop: "1px",
      borderBottom: "1px",
    },
  );
  assert.equal(geometry.left, 101);
  assert.equal(geometry.top, 41);
  assert.equal(geometry.width, 228);
  assert.equal(geometry.height, 170);
  assert.ok(geometry.width < 260, "the editor must not expand a narrow visible canvas internally");

  const clientX = geometry.left + geometry.pad + (geometry.width - geometry.pad * 2) * 0.25;
  const clientY = geometry.top + geometry.pad + (geometry.height - geometry.pad * 2) * 0.75;
  const [x, y] = toneCurvePointFromClient(clientX, clientY, geometry);
  assert.ok(Math.abs(x - 0.25) < 1e-9);
  assert.ok(Math.abs(y - 0.25) < 1e-9);
});

test("contextual control descriptions stay short and concrete", () => {
  for (const id of ["sl-level-black", "sl-deconv-iter", "sl-usm-amt", "sl-solar-filament"]) {
    const info = resolvePostprocessHelp({ id, dataset: {} });
    assert.ok(info);
    assert.ok(info.summary.length <= 90, `${id} summary is too long`);
    assert.ok(info.effect.length <= 100, `${id} effect is too long`);
    assert.ok(info.caution.length <= 100, `${id} caution is too long`);
  }
});

test("only the scientific 16-bit histogram and advanced modules remain in the panel", async () => {
  const html = await readFile(new URL("../index.html", import.meta.url), "utf8");
  assert.equal((html.match(/id="post-histogram"/g) || []).length, 1);
  assert.equal(html.includes('id="hist-canvas"'), false);
  assert.equal(html.includes('id="sl-levels-black"'), false);
  for (const control of ["texture", "clarity", "scnrGreen"]) {
    assert.ok(html.includes(`data-advanced-control="${control}"`), `${control} must be wired`);
  }
  assert.equal((html.match(/data-hsl-component="hue"/g) || []).length, 8);
  assert.equal((html.match(/data-hsl-component="saturation"/g) || []).length, 8);
  assert.equal((html.match(/data-hsl-component="luminance"/g) || []).length, 8);
  assert.ok(html.includes('id="chk-linked-wavelets"'));
  assert.ok(html.includes('id="chk-adaptive-usm"'));
  assert.ok(html.includes('id="detail-response-summary"'));
  assert.ok(html.includes('id="tone-curve-free"'));
  assert.ok(html.includes('id="post-tone-curve-editor"'));
  assert.equal((html.match(/data-tone-preset=/g) || []).length, 4);
  assert.ok(html.includes('id="solar-mono-module"'));
  assert.ok(html.includes('id="solar-tone-curve"'));
  assert.equal((html.match(/data-solar-preset=/g) || []).length, 8);
  assert.ok(html.includes('id="sl-solar-background-protect"'));
  assert.ok(html.includes('id="sl-solar-prominence"'));
  assert.ok(html.includes('id="sl-solar-highlight-compression"'));
  assert.ok(html.includes('data-deconv-preset="solar"'));
  assert.ok(html.includes('data-deconv-preset="solar-limb"'));
  assert.equal(html.includes('id="btn-deconv-compare"'), false);
  assert.ok(html.includes('id="object-finishing-module"'));
  assert.equal((html.match(/data-object-preset=/g) || []).length, 8);
  assert.ok(html.includes('data-object-preset="lunar-mineral-intense"'));
  assert.ok(html.includes('id="btn-object-original"'));
  assert.ok(html.includes('id="ds-step-assistant"'));
  assert.equal(html.includes("Supera al sharpening"), false);
});

test("history playback invalidates stale renders before waiting for its preview", async () => {
  const main = await readFile(new URL("../src/main.js", import.meta.url), "utf8");
  const start = main.indexOf("async function applyPostHistoryEntry");
  const end = main.indexOf("async function beginNewPostprocessResult", start);
  const block = main.slice(start, end);
  const invalidate = block.indexOf("const requestId = ++pipelineRequestId");
  const waitForPreview = block.indexOf("await setImageAndWait");
  assert.ok(invalidate >= 0 && waitForPreview > invalidate,
    "undo/redo must cancel the in-flight render before awaiting the stored preview");
  assert.ok(block.includes("playbackNonce !== historyPlaybackNonce"));
});

test("new post-processing text is available in Spanish, English, Italian, and French", async () => {
  const files = ["es", "en", "it", "fr"];
  const locales = {};
  for (const language of files) {
    locales[language] = JSON.parse(await readFile(
      new URL(`../src/locales/${language}.json`, import.meta.url),
      "utf8",
    ));
  }
  for (const language of files) {
    assert.ok(locales[language].wavelets?.adaptive?.measuring);
    assert.ok(locales[language].wavelets?.solar?.recover_prominences);
    assert.ok(locales[language].wavelets?.object_lab?.original);
    assert.ok(locales[language].wavelets?.history?.redo);
  }
  const i18nSource = await readFile(new URL("../src/i18n.js", import.meta.url), "utf8");
  for (const language of files) {
    assert.ok(i18nSource.includes(`'${language}'`));
  }
});

test("the intelligent assistant and deep-sky session organizer have English contracts", async () => {
  const english = JSON.parse(await readFile(
    new URL("../src/locales/en.json", import.meta.url),
    "utf8",
  ));
  const html = await readFile(new URL("../index.html", import.meta.url), "utf8");
  const main = await readFile(new URL("../src/main.js", import.meta.url), "utf8");
  assert.equal(english.assistant?.name, "Intelligent Assistant");
  assert.equal(
    english.assistant?.rules?.["clipping"]?.applyLabel,
    "Apply safe correction",
  );
  assert.equal(english.deepsky?.step_data, "Data");
  assert.ok(english.deepsky?.sessions_hint?.includes("Strict preflight"));
  assert.equal(english.deepsky?.pick_lights, "Lights (object frames)");
  assert.equal(english.deepsky?.scan_folder, "Scan folder (auto-classify)");
  assert.equal(english.deepsky?.session_quality_title, "Session quality control");
  assert.equal(english.deepsky?.psf_inspection_title, "Pre-stack PSF inspection");
  assert.equal(english.settings?.general?.gpu_budget?.includes("presupuestada"), false);
  assert.ok(html.includes('id="ds-session-organizer"'));
  assert.ok(html.includes('data-i18n-aria-label="deepsky.steps_aria"'));
  assert.ok(html.includes('data-i18n="general.add_fits_sequence"'));
  assert.ok(html.includes('data-i18n="general.status_ready"'));
  assert.ok(main.includes("function dsRenderSessionOrganizer()"));
  assert.ok(main.includes("strictEntries"));
  assert.ok(main.includes('class="ds-calibration-chip ${state}"'));
  assert.ok(main.includes('tr("deepsky.psf_inspection_title"'));
  assert.ok(main.includes('tr("deepsky.session_quality_title"'));
});

test("assistant corrections surface in the panel that owns them", async () => {
  const main = await readFile(new URL("../src/main.js", import.meta.url), "utf8");
  const html = await readFile(new URL("../index.html", import.meta.url), "utf8");

  // `levelsBlack: 0` y `levelsWhite: 1` son EXACTAMENTE los `data-default` de los
  // sliders (0 y 65535 a escala 65535): escribirlos no movía nada, así que el
  // panel de niveles se quedaba quieto mientras la imagen sí cambiaba.
  assert.ok(html.includes('data-advanced-control="levelsBlack"') && html.includes('data-default="0"'));
  assert.ok(html.includes('data-advanced-control="levelsWhite"'));
  const protectRange = main.slice(
    main.indexOf('if (action === "protect-range")'),
    main.indexOf('if (action === "auto-levels")'),
  );
  assert.ok(protectRange.length > 0, "la acción protect-range debe existir");
  assert.ok(
    !/values\.levelsBlack = 0;/.test(protectRange) && !/values\.levelsWhite = 1;/.test(protectRange),
    "protect-range no puede escribir el valor neutro de los niveles como si fuera una corrección",
  );
  assert.ok(
    protectRange.includes("advanced.levelsBlack > 0.0005")
      && protectRange.includes("advanced.levelsWhite < 0.9995"),
    "los niveles sólo se tocan cuando son la causa del recorte",
  );

  // Toda corrección revela y resalta los controles que movió, y retira las
  // marcas de preset que dejan de describir la imagen.
  assert.ok(main.includes("function revealAssistantEdits(names, label)"));
  assert.ok(main.includes("function assistantInvalidateFinishingClaim()"));
  const helper = main.slice(
    main.indexOf("function applyAssistantToneAdjustments("),
    main.indexOf("async function applyAssistantRecommendation("),
  );
  assert.ok(helper.includes("constrainLevelControls"), "los niveles deben respetar su invariante");
  assert.ok(helper.includes("assistantInvalidateFinishingClaim()"));
  assert.ok(helper.includes("revealAssistantEdits(names, label)"));
});

test("deep-sky calibration is linked from one WBPP-style table per light group", async () => {
  const main = await readFile(new URL("../src/main.js", import.meta.url), "utf8");
  const html = await readFile(new URL("../index.html", import.meta.url), "utf8");

  // Una fila por (noche × filtro × exposición) y bloques derivados de los
  // ficheros cargados: ligar no puede depender de que exista un plan preparado.
  assert.ok(main.includes("function dsCalibrationRows(lights)"));
  assert.ok(main.includes("function dsCalibrationBlocks(kind)"));
  assert.ok(main.includes("function dsBlockFitsRow(kind, block, row)"));
  assert.ok(main.includes('`${night}|${filter}|${expKey}`'));
  assert.ok(main.includes('data-ds-link="${escapeHtml(row.key)}"'));
  assert.ok(html.includes(".ds-wbpp-table"));

  // El ligado antiguo por noche desaparece por completo: convivir con la tabla
  // significaba dos escrituras con claves distintas sobre el mismo mapa.
  assert.ok(!main.includes("dsFormatCalibrationLinker"), "el ligador por noche debe estar retirado");
  assert.ok(!main.includes("dsNightPaths"), "los índices del plan ya no gobiernan el ligado");
  assert.ok(!main.includes("dsBatchIndex"));

  // Sólo se ofrece desplegable para los roles que el backend sabe forzar.
  assert.ok(main.includes('{ kind: "flats", labelKey: "deepsky.step_flat_s", fallback: "Flats", icon: "icon-lightbulb", linkable: true }'));
  assert.ok(main.includes('{ kind: "bias", labelKey: "deepsky.step_bias_s", fallback: "Bias", icon: "icon-film", linkable: false }'));

  for (const lang of ["es", "en", "fr", "it"]) {
    const locale = JSON.parse(await readFile(
      new URL(`../src/locales/${lang}.json`, import.meta.url),
      "utf8",
    ));
    for (const key of ["wbpp_hint", "groups", "state", "blocks_compatible", "blocks_other", "groups_show_all"]) {
      assert.ok(locale.deepsky?.[key], `falta deepsky.${key} en ${lang}.json`);
    }
  }
});

test("planetary defaults and mono-only availability are explicit", async () => {
  const html = await readFile(new URL("../index.html", import.meta.url), "utf8");
  assert.match(html, /<option[^>]*value="maximum"[^>]*selected[^>]*>/);
  assert.ok(html.includes('id="planetary-normalize-option"'));
  assert.ok(html.includes('id="planetary-rgb-align-option"'));
  assert.ok(html.includes("planetary-option-availability"));
});

test("standard stacking, colour grading and mosaic share the corrected contracts", async () => {
  const main = await readFile(new URL("../src/main.js", import.meta.url), "utf8");
  const mosaic = await readFile(new URL("../src/mosaic_manager.js", import.meta.url), "utf8");
  assert.match(main, /invoke\("stack_video",[\s\S]*?alignRgb:[\s\S]*?qualityPolicy:/);
  assert.ok(main.includes('if (control.type === "color") return;'));
  assert.ok(main.includes("forceFastPreview: true"));
  const metadataAssignment = main.indexOf("currentFileMetadata = res.metadata");
  const stackRefresh = main.indexOf("updateStackButtonState();", metadataAssignment);
  assert.ok(metadataAssignment >= 0 && stackRefresh > metadataAssignment,
    "stack readiness must refresh after successful analysis metadata is stored");
  const batchReset = main.indexOf("resetDataAcquisitionUI();", main.indexOf("ui.btnBatchMode.addEventListener"));
  const batchScanGuide = main.indexOf('stage: "scan"', batchReset);
  const batchScanChoice = main.indexOf("showCustomChoice(", batchScanGuide);
  assert.ok(batchReset >= 0 && batchScanGuide > batchReset && batchScanChoice > batchScanGuide,
    "batch assistant must explain scan scope before the recursion choice opens");
  assert.ok(mosaic.includes('document.getElementById("chk-rgb-align")'));
  assert.ok(mosaic.includes("adaptiveUsm: p?.adaptiveUsm"));
});

test("the 16-bit white-level range can represent the exact neutral endpoint", async () => {
  const html = await readFile(new URL("../index.html", import.meta.url), "utf8");
  const tag = html.match(/<input\s+id="sl-level-white"[^>]+>/)?.[0];
  assert.ok(tag, "white-level slider must exist");
  const attr = (name) => Number(tag.match(new RegExp(`${name}="([0-9]+)"`))?.[1]);
  const min = attr("min");
  const max = attr("max");
  const step = attr("step");
  const value = attr("value");
  assert.equal(max, 65535);
  assert.equal(value, max);
  assert.equal((max - min) % step, 0);
});
