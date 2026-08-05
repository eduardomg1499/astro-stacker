import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  normalizeMilkyWayMaskDetection,
  normalizeMilkyWayProgress,
  summarizeMilkyWayPreflight,
  summarizeMilkyWayRegistrationAnalysis,
  summarizeMilkyWayStackResult,
} from "../src/milky_way_ui.js";

const uiSource = readFileSync(new URL("../src/milky_way_ui.js", import.meta.url), "utf8");
const es = JSON.parse(readFileSync(new URL("../src/locales/es.json", import.meta.url), "utf8"));
const en = JSON.parse(readFileSync(new URL("../src/locales/en.json", import.meta.url), "utf8"));

test("registration review preserves warnings and branch exclusions", () => {
  const review = summarizeMilkyWayRegistrationAnalysis({
    frames: [
      { path: "/night/ok.fits", sky: { excluded: false }, ground: { excluded: false } },
      { path: "/night/rejected.fits", sky: { excluded: true, reason: "RMS alto" }, ground: { excluded: false } },
    ],
    warnings: ["Registro degradado"],
  });
  assert.equal(review.degraded, true);
  assert.equal(review.excludedFrames, 1);
  assert.deepEqual(review.messages, ["Registro degradado", "RMS alto"]);
});

test("stack result status cannot hide a non-scientific result or warnings", () => {
  const degraded = summarizeMilkyWayStackResult({
    scientific: false,
    warnings: ["Se excluyó una toma"],
    frames: [{ path: "/night/a.fits", sky: { excluded: true, reason: "sin estrellas" } }],
  });
  assert.equal(degraded.scientific, false);
  assert.equal(degraded.degraded, true);
  assert.equal(degraded.excludedFrames, 1);

  const clean = summarizeMilkyWayStackResult({ scientific: true, warnings: [], frames: [] });
  assert.equal(clean.scientific, true);
  assert.equal(clean.degraded, false);

  const informational = summarizeMilkyWayStackResult({
    scientific: true,
    warnings: ["No se solicitaron darks opcionales"],
    frames: [],
  });
  assert.equal(informational.scientific, true);
  assert.equal(informational.degraded, false);
  assert.equal(informational.messages.length, 1);
});

test("rendered-control contract keeps selected values and locks only future steps accessibly", () => {
  assert.match(uiSource, /value="auto" \$\{r\.distortionCorrection === "auto" \? "selected"/);
  assert.match(uiSource, /value="none" \$\{i\.normalization === "none" \? "selected"/);
  assert.match(uiSource, /value="off" \$\{state\.composition\.colorMatch === "off" \? "selected"/);
  assert.match(uiSource, /aria-disabled="\$\{locked \? "true" : "false"\}" \$\{locked \? "disabled" : ""\}/);
  assert.match(uiSource, /const locked = !info\.reachable && index > state\.activeStep/);
  assert.match(uiSource, /r\.model === "radialWide" \? "disabled"/);
});

test("change handler coerces range and number inputs before writing state", () => {
  // `change` fires after `input` on slider release; without this coercion the
  // last write is a string and serde rejects the whole native request.
  assert.match(uiSource, /event\.target\.type === "range" \|\| event\.target\.type === "number"\s*\?\s*Number\(event\.target\.value\)/);
});

test("opening the Milky Way modal twice cannot poison the inert snapshot", () => {
  assert.match(uiSource, /const alreadyOpen = modal\.hidden === false/);
  assert.match(uiSource, /const resume = alreadyOpen \|\| initial\.resume === true/);
  assert.match(uiSource, /if \(!alreadyOpen\) setBackgroundInert\(true\)/);
});

test("native mask detection keeps the soft preview, confidence and warnings", () => {
  const detection = normalizeMilkyWayMaskDetection({
    confidence: 0.21,
    skyFraction: 0.64,
    previewPngBase64: "YWJj",
    warnings: ["Horizonte dudoso"],
  });
  assert.equal(detection.confidence, 0.21);
  assert.equal(detection.skyFraction, 0.64);
  assert.equal(detection.preview, "data:image/png;base64,YWJj");
  assert.deepEqual(detection.warnings, ["Horizonte dudoso"]);
  assert.match(uiSource, /showDetectedMask = !!maskDetection\.preview/);
  assert.match(uiSource, /strategy: "auto",\s*confidence: maskDetection\.confidence/);
});

test("progress reports item counts and ETA without adopting another job", () => {
  const normalized = normalizeMilkyWayProgress({
    jobId: "ours",
    phase: "Registro",
    progress: 50,
    itemsDone: 4,
    itemsTotal: 8,
  }, { startedAtMs: 1_000, nowMs: 11_000 });
  assert.equal(normalized.itemsDone, 4);
  assert.equal(normalized.itemsTotal, 8);
  assert.equal(normalized.etaSeconds, 10);
  assert.equal(normalized.phaseCode, "registration");
  assert.match(uiSource, /if \(!activeJobId \|\| !eventJobId \|\| eventJobId !== activeJobId\) return/);
  assert.doesNotMatch(uiSource, /if \(!activeJobId && payload\.jobId\) activeJobId = payload\.jobId/);
  assert.match(uiSource, /translateProgress/);
  assert.match(uiSource, /milkyway\.progress_phase_/);
});

test("run performs native preflight before stacking and exposes its estimates", () => {
  const plan = summarizeMilkyWayPreflight({
    jobId: "mw-1",
    width: 6000,
    height: 4000,
    estimatedWorkingBytes: 1024,
    estimatedOutputBytes: 2048,
    warnings: ["Sin darks"],
    products: ["Cielo"],
  });
  assert.equal(plan.jobId, "mw-1");
  assert.equal(plan.estimatedWorkingBytes, 1024);
  assert.deepEqual(plan.warnings, ["Sin darks"]);
  const preflightCall = uiSource.indexOf('invoke?.("prepare_milky_way_stack"');
  const runCall = uiSource.indexOf('invoke?.("run_milky_way_stack"');
  assert.ok(preflightCall >= 0 && runCall > preflightCall);
});

test("Escape and the close control cannot silently abandon an active run", () => {
  assert.match(uiSource, /if \(busy && !force\) \{\s*closePrompt = true/);
  assert.match(uiSource, /case "continue-background"/);
  assert.match(uiSource, /case "cancel-run": closePrompt = false; await cancelRun\(\)/);
  assert.match(uiSource, /if \(event\.key === "Escape"\) \{ event\.preventDefault\(\); close\(\); return; \}/);
});

test("UI exposes one dual-branch mode, an explicit sky-only mask, and immutable scientific layers", () => {
  assert.doesNotMatch(uiSource, /modeCard\(MILKY_WAY_MODES\.SEPARATE_LAYERS/);
  assert.doesNotMatch(uiSource, /data-mw-field="composition\.keepSeparateLayers"/);
  assert.match(uiSource, /mask_sky_only_title/);
  assert.match(uiSource, /strategy: "fullSky"/);
  assert.doesNotMatch(uiSource, /\["fullSky",t\("milkyway\.mask_full_sky"/);
  assert.match(uiSource, /fallbackPolicy/);
});

test("Milky Way UI additions have exact Spanish and English locale parity", () => {
  const keys = [
    "editor_cropping_layers",
    "mask_sky_only_title",
    "radial_requires_distortion",
    "registration_degraded",
    "fallback_policy",
    "fallback_strict_body",
    "fallback_degraded_body",
    "result_scientific",
    "result_not_scientific",
    "reason_dark_mismatch",
    "reason_flat_metadata",
    "progress_phase_registration",
    "progress_phase_integration",
  ];
  for (const key of keys) {
    assert.equal(typeof es.milkyway[key], "string", `missing es.milkyway.${key}`);
    assert.equal(typeof en.milkyway[key], "string", `missing en.milkyway.${key}`);
  }
  assert.deepEqual(Object.keys(es.milkyway).sort(), Object.keys(en.milkyway).sort());
  assert.deepEqual(
    [es.deepsky.editor_step_9, es.deepsky.editor_step_10, es.deepsky.editor_step_11, es.deepsky.editor_step_12],
    ["Curvas y color", "Realzar detalle", "Acabado", "Anotar y exportar"],
  );
  assert.deepEqual(
    [en.deepsky.editor_step_9, en.deepsky.editor_step_10, en.deepsky.editor_step_11, en.deepsky.editor_step_12],
    ["Curves and color", "Enhance detail", "Finish", "Annotate and export"],
  );
});
