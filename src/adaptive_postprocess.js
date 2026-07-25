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
  const chromaSampledPixels = Math.max(0, Number(artifacts.chromaSampledPixels) || 0);
  const meanSaturation = clampAdaptive(Number(artifacts.meanSaturation) || 0);
  const p95Saturation = clampAdaptive(Number(artifacts.p95Saturation) || 0);
  const chromaticFraction = clampAdaptive(Number(artifacts.chromaticFraction) || 0);
  const chromaMeasured = !Boolean(histogram.isMono)
    && chromaSampledPixels >= 64
    && (
      Number.isFinite(Number(artifacts.meanSaturation))
      || Number.isFinite(Number(artifacts.p95Saturation))
    );
  const chromaEvidence = chromaMeasured
    ? clampAdaptive(
        clampAdaptive((meanSaturation - 0.004) / 0.08) * 0.35
          + clampAdaptive((p95Saturation - 0.012) / 0.22) * 0.45
          + clampAdaptive((chromaticFraction - 0.04) / 0.55) * 0.2,
      )
    : 0.45;
  // Un p99.9 alto NO es prueba de recorte: en un máster solar o lunar bien
  // expuesto el disco ocupa legítimamente la parte alta del rango. Medir el
  // estrés desde 0.88 saturaba a 1.0 en casi cualquier máster sano y disparaba
  // la protección máxima (compresión de altas, recorte del extremo de la curva
  // y menos color) sobre datos que no la necesitaban: adaptativo acababa
  // significando "aplanado". La única evidencia directa es la fracción de
  // píxeles realmente recortada; la falta de headroom entra como señal
  // secundaria, más tardía y acotada, incapaz de saturar por sí sola.
  const highlightHeadroom = clampAdaptive((percentileHigh - 0.985) / 0.015);
  const highlightStress = clampAdaptive(
    Math.max(
      highlightClip * 320,
      highlightHeadroom * 0.45,
    ),
  );
  const shadowStress = clampAdaptive(
    Math.max(
      shadowClip * 80,
      (0.025 - percentileLow) / 0.025,
    ),
  );
  const noiseStress = clampAdaptive(Math.max(denoiseNeed, defectRate * 900));
  const chromaStress = clampAdaptive(
    colorFringe * 0.45
      + noiseStress * 0.4
      + clampAdaptive((p95Saturation - 0.5) / 0.3) * 0.3,
  );
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
  // Umbral por encima de lo que puede producir la mera falta de headroom sin
  // recorte medido: anunciar "highlights" en un máster sano describía una
  // protección que no hacía falta (y que antes sí se aplicaba).
  if (highlightStress > 0.3) safeguards.push("highlights");
  if (noiseStress > 0.18) safeguards.push("noise");
  if (ringing > 0.12) safeguards.push("ringing");
  if (shadowStress > 0.2) safeguards.push("shadows");
  if (!histogram.isMono && (chromaStress > 0.32 || (chromaMeasured && chromaEvidence < 0.18))) {
    safeguards.push("chroma");
  }
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
    chromaSampledPixels,
    meanSaturation,
    p95Saturation,
    chromaticFraction,
    chromaMeasured,
    chromaEvidence,
    chromaStress,
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
