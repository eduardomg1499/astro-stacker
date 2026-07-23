import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { PostProcessSession, recipesEqual, unwrapPreviewReference } from "../src/postprocess_session.js";
import { evaluateGuide } from "../src/zenith_guide.js";

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
  assert.equal(html.includes("Supera al sharpening"), false);
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
