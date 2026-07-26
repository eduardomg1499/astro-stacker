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

// Máster/resultado que YA llega brillante (exposición o curvas previas): las
// recetas no pueden re-iluminar el disco ni empujar el blanco robusto contra
// el hombro de compresión. Es la garantía anti-quemado, visible en la curva.
const brightResult = {
  histogram: {
    median: 45_875, // 0.70
    percentileLow: 2_621, // 0.04
    percentileHigh: 60_948, // 0.93
    shadowClip: 0,
    highlightClip: 0,
    isMono: true,
  },
  artifacts: {
    sampledPixels: 200_000,
    suggestedDenoise: 2,
    ringingScore: 0.3,
  },
};

test("a bright active result engages the exposure guard instead of re-lifting the disk", () => {
  const adapted = adaptSolarPreset("filaments", brightResult);
  assert.ok(
    adapted.adaptation.exposureGuard > 0.3,
    `the guard must engage on a bright result (got ${adapted.adaptation.exposureGuard})`,
  );
  const median = 45_875 / 65_535;
  const mapped = evaluateSolarCurve(adapted.curvePoints, median);
  assert.ok(
    mapped <= median + 0.02,
    `the disk median must stay near identity (${mapped} vs ${median})`,
  );
});

test("every preset honours the no-burn invariants on dark and bright results", () => {
  const darkResult = {
    histogram: {
      median: 22_937, // 0.35
      percentileLow: 1_311, // 0.02
      percentileHigh: 55_704, // 0.85
      shadowClip: 0,
      highlightClip: 0,
      isMono: true,
    },
    artifacts: { sampledPixels: 200_000, suggestedDenoise: 2, ringingScore: 0.3 },
  };
  const presets = ["ha-natural", "ha-gold", "ha-inverted", "chromosphere", "prominence", "dual-range", "filaments"];
  for (const input of [darkResult, brightResult]) {
    const median = input.histogram.median / 65_535;
    const percentileHigh = input.histogram.percentileHigh / 65_535;
    const brightAnchor = Math.max(percentileHigh, median + 0.05);
    const burnCeiling = Math.min(0.952, brightAnchor + (1 - brightAnchor) * 0.5);
    const allowedMidLift = Math.min(0.22, Math.max(0, (0.58 - median) * 0.55)) + 0.015;
    for (const name of presets) {
      const adapted = adaptSolarPreset(name, input);
      // El blanco robusto medido conserva margen 16-bit.
      const mappedHigh = evaluateSolarCurve(adapted.curvePoints, brightAnchor);
      assert.ok(
        mappedHigh <= burnCeiling + 0.006,
        `${name}: mapped white ${mappedHigh} must stay under ${burnCeiling}`,
      );
      // La mediana no puede subir más de lo permitido para su brillo medido.
      const mappedMedian = evaluateSolarCurve(adapted.curvePoints, median);
      assert.ok(
        mappedMedian - median <= allowedMidLift + 0.006,
        `${name}: median lift ${(mappedMedian - median).toFixed(3)} must stay under ${allowedMidLift.toFixed(3)}`,
      );
      // Y la curva sigue siendo monótona tras los frenos.
      let previous = -1;
      for (const value of [0, 0.1, 0.25, 0.4, 0.55, 0.7, 0.85, 1]) {
        const output = evaluateSolarCurve(adapted.curvePoints, value);
        assert.ok(output >= previous - 1e-4, `${name}: curve must stay monotonic at ${value}`);
        previous = output;
      }
    }
  }
});
