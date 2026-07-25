import assert from "node:assert/strict";
import test from "node:test";

import {
  adaptSolarPreset,
  cloneSolarPreset,
  evaluateSolarCurve,
} from "../src/solar_postprocess.js";

const cleanMaster = {
  histogram: {
    median: 42_500,
    percentileLow: 850,
    percentileHigh: 61_500,
    shadowClip: 0,
    highlightClip: 0,
    isMono: true,
  },
  artifacts: {
    sampledPixels: 200_000,
    suggestedDenoise: 1,
    ringingScore: 0.2,
  },
  capture: { qualityStability: 96 },
};

test("a clean solar master retains filament strength and avoids generic compression", () => {
  const base = cloneSolarPreset("ha-natural");
  const adapted = adaptSolarPreset("ha-natural", cleanMaster);

  assert.ok(adapted.filamentAmount >= base.filamentAmount * 0.9);
  assert.ok(adapted.prominenceAmount >= base.prominenceAmount * 0.88);
  assert.ok(adapted.highlightCompression < base.highlightCompression * 0.8);
  assert.ok(adapted.adaptation.tonePreservation < 0.15);
});

test("narrow solar data boosts structure while preserving black-mid-white order", () => {
  const base = cloneSolarPreset("prominence");
  const adapted = adaptSolarPreset("prominence", {
    histogram: {
      median: 18_000,
      percentileLow: 400,
      percentileHigh: 25_000,
      shadowClip: 0.001,
      highlightClip: 0,
      isMono: true,
    },
    artifacts: {
      sampledPixels: 200_000,
      suggestedDenoise: 3,
      ringingScore: 0.5,
    },
  });

  assert.ok(adapted.prominenceAmount >= base.prominenceAmount);
  assert.ok(adapted.filamentAmount >= base.filamentAmount * 0.9);
  const samples = [0.02, 0.2, 0.5, 0.8, 0.98]
    .map((value) => evaluateSolarCurve(adapted.curvePoints, value));
  for (let index = 1; index < samples.length; index += 1) {
    assert.ok(samples[index] > samples[index - 1], "the protected curve must remain monotonic");
  }
});

test("measured clipping adds compression without collapsing the detail floor", () => {
  const clean = adaptSolarPreset("chromosphere", cleanMaster);
  const clipped = adaptSolarPreset("chromosphere", {
    histogram: {
      median: 49_000,
      percentileLow: 300,
      percentileHigh: 65_535,
      shadowClip: 0.002,
      highlightClip: 0.008,
      isMono: true,
    },
    artifacts: {
      sampledPixels: 200_000,
      suggestedDenoise: 8,
      ringingScore: 2,
    },
  });

  assert.ok(clipped.highlightCompression > clean.highlightCompression);
  assert.ok(clipped.curvePoints.at(-1)[1] < clean.curvePoints.at(-1)[1]);
  assert.ok(
    clipped.filamentAmount >= cloneSolarPreset("chromosphere").filamentAmount * 0.7,
  );
});

test("unmeasured input leaves the declared recipe untouched", () => {
  const base = cloneSolarPreset("ha-gold");
  const adapted = adaptSolarPreset("ha-gold", {});
  assert.deepEqual(adapted.curvePoints, base.curvePoints);
  assert.equal(adapted.filamentAmount, base.filamentAmount);
  assert.equal(adapted.highlightCompression, base.highlightCompression);
  assert.equal(adapted.adaptation.measured, false);
});
