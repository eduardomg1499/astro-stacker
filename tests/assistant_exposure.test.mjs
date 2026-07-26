import assert from "node:assert/strict";
import test from "node:test";

import { computeSafeExposureEv } from "../src/adaptive_postprocess.js";
import { DEFAULT_RULES, evaluateGuide } from "../src/zenith_guide.js";

function binsWith(entries, size = 1024) {
  const bins = new Array(size).fill(0);
  for (const [index, count] of entries) bins[index] = count;
  return bins;
}

test("the safe exposure cap protects a small planet on a black sky", () => {
  // 99.85% cielo oscuro + un planeta pequeño con señal real a ~0.645.
  const bins = binsWith([[10, 999_000], [660, 1_500]]);
  const safe = computeSafeExposureEv(bins);
  const expected = Math.log2(0.92 / (661 / 1024));
  assert.ok(
    Math.abs(safe - expected) < 0.05,
    `the cap must anchor on the planet, not the sky median (${safe} vs ${expected})`,
  );
});

test("a handful of hot pixels cannot hijack the exposure cap", () => {
  // Mismo cielo, 6 píxeles calientes casi blancos: por debajo del suelo de 24.
  const bins = binsWith([[30, 500_000], [1_020, 6]]);
  const safe = computeSafeExposureEv(bins);
  assert.ok(safe > 3.5, `a genuinely dark frame keeps a generous cap (${safe})`);
});

test("the cap is zero without measurable data", () => {
  assert.equal(computeSafeExposureEv([]), 0);
  assert.equal(computeSafeExposureEv(null), 0);
  assert.equal(computeSafeExposureEv(binsWith([])), 0);
});

const darkContext = {
  hasResult: true,
  histogramAvailable: true,
  medianLevel: 0.02,
  shadowClip: 0,
  highlightClip: 0,
  recommendedExposureEv: 0.75,
};

test("dark-result fires only while a safe lift remains", () => {
  const active = evaluateGuide(darkContext, DEFAULT_RULES, new Set());
  assert.ok(
    active.some((rule) => rule.id === "dark-result"),
    "with safe headroom the recommendation must appear",
  );

  const exhausted = evaluateGuide(
    { ...darkContext, recommendedExposureEv: 0.04 },
    DEFAULT_RULES,
    new Set(),
  );
  assert.ok(
    !exhausted.some((rule) => rule.id === "dark-result"),
    "without safe headroom the card must disappear instead of re-applying",
  );

  const clipped = evaluateGuide(
    { ...darkContext, highlightClip: 0.002 },
    DEFAULT_RULES,
    new Set(),
  );
  assert.ok(
    !clipped.some((rule) => rule.id === "dark-result"),
    "with measured highlight clipping the lift must never be suggested",
  );
});
