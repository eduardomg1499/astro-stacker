import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { PostProcessSession, recipesEqual, unwrapPreviewReference } from "../src/postprocess_session.js";
import { evaluateGuide } from "../src/zenith_guide.js";
import {
  cloneSolarPreset,
  evaluateSolarCurve,
  evaluateToneCurve,
  normalizeSolarCurvePoints,
  normalizeToneCurvePoints,
  resolveToneCurveGeometry,
  toneCurvePointFromClient,
} from "../src/solar_postprocess.js";
import { resolvePostprocessHelp } from "../src/postprocess_help.js";

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
  assert.equal((html.match(/data-solar-preset=/g) || []).length, 5);
  assert.equal(html.includes("Supera al sharpening"), false);
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
