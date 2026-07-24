const U16_MAX = 65535;

export function clampAdaptive(value, minimum = 0, maximum = 1) {
  const number = Number(value);
  if (!Number.isFinite(number)) return minimum;
  return Math.max(minimum, Math.min(maximum, number));
}

function normalizedU16(value, fallback) {
  const number = Number(value);
  if (!Number.isFinite(number)) return fallback;
  return clampAdaptive(number / U16_MAX);
}

/**
 * Converts the scientific histogram and artifact sampler into a small,
 * deterministic quality profile. Capture gain/exposure are deliberately not
 * guessed: their visible consequences are measured in the stacked 16-bit
 * signal instead (headroom, clipping, noise and ringing).
 */
export function normalizeAdaptivePostprocessAnalysis(input = {}) {
  const histogram = input.histogram || input;
  const artifacts = input.artifacts || {};
  const median = normalizedU16(histogram.median, 0.38);
  const percentileLow = normalizedU16(
    histogram.percentileLow ?? histogram.minimum,
    0.02,
  );
  const percentileHigh = normalizedU16(
    histogram.percentileHigh ?? histogram.maximum,
    0.92,
  );
  const robustRange = clampAdaptive(percentileHigh - percentileLow);
  const shadowClip = clampAdaptive(histogram.shadowClip);
  const highlightClip = clampAdaptive(histogram.highlightClip);
  const sampledPixels = Math.max(1, Number(artifacts.sampledPixels) || 1);
  const defectRate = clampAdaptive(
    ((Number(artifacts.hotPixels) || 0) + (Number(artifacts.deadPixels) || 0))
      / sampledPixels,
  );
  const denoiseNeed = clampAdaptive((Number(artifacts.suggestedDenoise) || 0) / 35);
  const ringing = clampAdaptive((Number(artifacts.ringingScore) || 0) / 18);
  const colorFringe = clampAdaptive((Number(artifacts.colorFringeScore) || 0) / 18);
  const highlightStress = clampAdaptive(
    Math.max(
      highlightClip * 320,
      (percentileHigh - 0.88) / 0.12,
    ),
  );
  const shadowStress = clampAdaptive(
    Math.max(
      shadowClip * 80,
      (0.025 - percentileLow) / 0.025,
    ),
  );
  const noiseStress = clampAdaptive(Math.max(denoiseNeed, defectRate * 900));
  const rangeConfidence = clampAdaptive((robustRange - 0.12) / 0.68);
  const stability = clampAdaptive(
    Number(input.capture?.qualityStability ?? input.qualityStability ?? 100) / 100,
    0,
    1,
  );
  const detailConfidence = clampAdaptive(
    0.98
      + rangeConfidence * 0.08
      + (stability - 0.75) * 0.12
      - noiseStress * 0.38
      - ringing * 0.3
      - highlightStress * 0.16,
    0.42,
    1.04,
  );
  const measured = Boolean(
    Number.isFinite(Number(histogram.median))
    || Number.isFinite(Number(histogram.percentileLow))
    || Number.isFinite(Number(histogram.percentileHigh))
    || (Array.isArray(histogram.luminance) && histogram.luminance.length > 0),
  );

  const safeguards = [];
  if (highlightStress > 0.18) safeguards.push("highlights");
  if (noiseStress > 0.18) safeguards.push("noise");
  if (ringing > 0.12) safeguards.push("ringing");
  if (shadowStress > 0.2) safeguards.push("shadows");
  if (!safeguards.length) safeguards.push("balanced");

  return Object.freeze({
    measured,
    isMono: Boolean(histogram.isMono),
    median,
    percentileLow,
    percentileHigh,
    robustRange,
    shadowClip,
    highlightClip,
    shadowStress,
    highlightStress,
    noiseStress,
    ringing,
    colorFringe,
    rangeConfidence,
    stability,
    detailConfidence,
    safeguards,
  });
}

export function roundAdaptive(value, digits = 2) {
  const scale = 10 ** digits;
  return Math.round(Number(value || 0) * scale) / scale;
}
