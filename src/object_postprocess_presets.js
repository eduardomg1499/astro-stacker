const LINEAR_CURVE = Object.freeze([[0, 0], [1, 1]]);

/**
 * Recipes deliberately separate measured/natural rendering from interpretive
 * colour. They only drive existing 16-bit pipeline controls, so every preset
 * remains reversible through the post-process session history.
 */
export const OBJECT_FINISHING_PRESETS = Object.freeze({
  "lunar-relief": {
    label: "Relieve lunar",
    family: "lunar",
    intent: "scientific",
    description: "Detalle de cráteres con luces protegidas y color neutro.",
    pipeline: {
      w: [7.5, 6, 3.5, 1.5, 0, 0],
      d: [4, 3, 2, 1, 0, 0],
      deconv: { s: 1.15, i: 10, vs: 0, vi: 0 },
      edgeAwareWavelets: true,
      edgeAwareStrength: 68,
      autoMask: 56,
      masterDenoise: 4,
      blend: 92,
      advanced: {
        toneCurvePoints: [[0, 0], [.18, .14], [.5, .54], [.82, .84], [1, 1]],
        highlights: -.18,
        whites: -.08,
        texture: .24,
        clarity: .14,
        vibrance: 0,
      },
    },
  },
  "lunar-phase": {
    label: "Fase completa",
    family: "lunar",
    intent: "scientific",
    description: "Equilibra terminador, disco iluminado y gradientes suaves.",
    pipeline: {
      w: [5, 4, 2.5, 1, 0, 0],
      d: [4, 3, 2, 1, 0, 0],
      deconv: { s: 1.25, i: 8, vs: 0, vi: 0 },
      edgeAwareWavelets: true,
      edgeAwareStrength: 62,
      autoMask: 52,
      masterDenoise: 5,
      blend: 88,
      advanced: {
        toneCurvePoints: [[0, 0], [.16, .2], [.46, .5], [.78, .77], [1, 1]],
        shadows: .12,
        highlights: -.24,
        whites: -.1,
        clarity: .1,
        vibrance: 0,
      },
    },
  },
  "lunar-mineral": {
    label: "Luna mineral",
    family: "lunar",
    intent: "creative",
    colorRequired: true,
    description: "Amplifica diferencias cromáticas reales; resultado interpretativo.",
    pipeline: {
      w: [4.5, 3.5, 2, .8, 0, 0],
      d: [5, 4, 3, 1.5, 0, 0],
      deconv: { s: 1.2, i: 8, vs: 0, vi: 0 },
      edgeAwareWavelets: true,
      edgeAwareStrength: 64,
      autoMask: 60,
      masterDenoise: 7,
      blend: 86,
      advanced: {
        toneCurvePoints: [[0, 0], [.2, .17], [.5, .53], [.82, .86], [1, 1]],
        highlights: -.15,
        texture: .12,
        clarity: .08,
        vibrance: .62,
        hslSaturation: [.18, .3, .26, .08, .22, .34, .28, .2],
        hslLuminance: [0, .04, .05, 0, .02, -.04, -.02, 0],
      },
    },
  },
  "jupiter-natural": {
    label: "Júpiter natural",
    family: "planetary",
    intent: "scientific",
    description: "Bandas y óvalos con color contenido y borde protegido.",
    pipeline: {
      w: [8, 6, 3.2, 1.2, 0, 0],
      d: [5, 4, 2.5, 1, 0, 0],
      deconv: { s: 1.1, i: 11, vs: .8, vi: 2 },
      edgeAwareWavelets: true,
      edgeAwareStrength: 72,
      autoMask: 58,
      masterDenoise: 5,
      blend: 90,
      advanced: {
        toneCurvePoints: [[0, 0], [.2, .17], [.5, .53], [.84, .86], [1, 1]],
        highlights: -.12,
        texture: .18,
        clarity: .1,
        vibrance: .12,
      },
    },
  },
  "saturn-rings": {
    label: "Saturno y anillos",
    family: "planetary",
    intent: "scientific",
    description: "Preserva el globo y la división de anillos sin forzar halos.",
    pipeline: {
      w: [6.5, 5, 2.8, 1, 0, 0],
      d: [5, 4, 3, 1.5, 0, 0],
      deconv: { s: 1.25, i: 9, vs: 0, vi: 0 },
      edgeAwareWavelets: true,
      edgeAwareStrength: 78,
      autoMask: 64,
      masterDenoise: 6,
      blend: 86,
      advanced: {
        toneCurvePoints: [[0, 0], [.16, .12], [.5, .52], [.8, .8], [1, 1]],
        highlights: -.2,
        whites: -.08,
        clarity: .08,
        vibrance: .1,
      },
    },
  },
  "mars-detail": {
    label: "Marte detallado",
    family: "planetary",
    intent: "scientific",
    description: "Realza albedo y casquete con saturación moderada.",
    pipeline: {
      w: [7, 5.5, 3, 1, 0, 0],
      d: [5, 4, 2.5, 1, 0, 0],
      deconv: { s: 1.05, i: 12, vs: .75, vi: 2 },
      edgeAwareWavelets: true,
      edgeAwareStrength: 72,
      autoMask: 56,
      masterDenoise: 5,
      blend: 90,
      advanced: {
        toneCurvePoints: [[0, 0], [.2, .16], [.5, .55], [.82, .86], [1, 1]],
        texture: .2,
        clarity: .12,
        vibrance: .16,
        hslSaturation: [.08, .12, .08, 0, 0, .02, 0, .02],
      },
    },
  },
  "planet-cinematic": {
    label: "Planetario expresivo",
    family: "planetary",
    intent: "creative",
    description: "Contraste y color más intensos para una salida artística.",
    pipeline: {
      w: [8.5, 6.5, 3.5, 1.5, 0, 0],
      d: [4, 3, 2, 1, 0, 0],
      deconv: { s: 1.05, i: 12, vs: .8, vi: 2 },
      edgeAwareWavelets: true,
      edgeAwareStrength: 68,
      autoMask: 52,
      masterDenoise: 4,
      blend: 94,
      advanced: {
        toneCurvePoints: [[0, 0], [.18, .12], [.5, .56], [.82, .9], [1, 1]],
        texture: .24,
        clarity: .16,
        vibrance: .38,
        hslSaturation: [.14, .2, .16, .08, .1, .18, .12, .12],
      },
    },
  },
});

export function cloneObjectFinishingPreset(name) {
  const preset = OBJECT_FINISHING_PRESETS[name];
  if (!preset) return null;
  return typeof structuredClone === "function"
    ? structuredClone(preset)
    : JSON.parse(JSON.stringify(preset));
}

export function objectPresetApplicable(preset, { isMono = false } = {}) {
  if (!preset) return { applicable: false, reason: "Preset desconocido." };
  if (preset.colorRequired && isMono) {
    return {
      applicable: false,
      reason: "La Luna mineral necesita una fuente color; una captura mono no contiene diferencias minerales cromáticas.",
    };
  }
  return { applicable: true, reason: "" };
}

export function objectPresetToneCurve(preset) {
  return preset?.pipeline?.advanced?.toneCurvePoints || LINEAR_CURVE;
}
