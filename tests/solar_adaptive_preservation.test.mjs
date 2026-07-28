import assert from "node:assert/strict";
import test from "node:test";

import {
  SOLAR_CURVE_PRESETS,
  adaptSolarPreset,
  cloneSolarPreset,
  evaluateSolarCurve,
  solarPaletteCeiling,
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

test("a clean solar master retains filament strength without veiling the disk", () => {
  const base = cloneSolarPreset("ha-natural");
  const adapted = adaptSolarPreset("ha-natural", cleanMaster);

  assert.ok(adapted.filamentAmount >= base.filamentAmount * 0.9);
  assert.ok(adapted.prominenceAmount >= base.prominenceAmount * 0.88);
  assert.ok(adapted.adaptation.tonePreservation < 0.15);

  // CONTRATO CORREGIDO. Este test exigía antes
  // `highlightCompression < base * 0.8`, es decir: "si el máster está limpio,
  // recorta la compresión". Esa premisa era justamente la causa del quemado —
  // la compresión no depende de lo limpia que venga la ENTRADA, sino de dónde
  // dejan la SALIDA la curva y la paleta de falso color. Un máster impecable
  // cuya curva sube el limbo por encima del punto de aplanado de la paleta
  // necesita MÁS compresión, no menos.
  //
  // Lo que aquel assert protegía de verdad era que el hombro no invadiera los
  // medios y velara el disco. Eso se comprueba directamente: la rodilla está en
  // 0.82 y el techo tiene que quedar por encima de ella.
  const techo = 1 - adapted.highlightCompression * 0.16;
  assert.ok(
    techo > 0.82,
    `el hombro no puede alcanzar la rodilla y aplanar el disco (techo ${techo.toFixed(3)})`,
  );
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
  // CONTRATO ACTUALIZADO. Antes se exigía `adapted.at(-1)[1] < baseline.at(-1)[1]`,
  // es decir que el `endpointCap` RECORTARA el extremo. Al bajar los últimos
  // puntos de las curvas de preset (el penúltimo levantaba hasta +0.070 sobre la
  // diagonal y quemaba la superficie), el extremo ya nace por debajo del cap: no
  // hay nada que recortar. Lo que importaba era el MARGEN reservado, y eso se
  // comprueba directamente.
  assert.ok(
    clipped.curvePoints.at(-1)[1] <= 0.94,
    `el extremo debe reservar margen 16-bit (${clipped.curvePoints.at(-1)[1]})`,
  );
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

test("a bright active result does not re-lift an already bright disk", () => {
  const adapted = adaptSolarPreset("filaments", brightResult);
  // CONTRATO ACTUALIZADO. Antes se exigía que el freno de exposición se ACTIVARA
  // (`exposureGuard > 0.3`). Al bajar los últimos puntos de las curvas, la receta
  // ya no sube la mediana lo suficiente como para necesitarlo: el freno marca 0
  // porque no hay nada que frenar. Lo que se protegía —que el disco no se
  // re-ilumine— se cumple mejor que antes, y es lo que se comprueba aquí.
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

// ===========================================================================
// MODO PUREZA sobre las garantías anti-quemado.
//
// Los dos frenos de la curva solar tienen naturaleza distinta y por eso se
// gradúan distinto:
//   - Deriva de medios: criterio estético ("este disco ya venía brillante"). En
//     Puro se suelta del todo y manda la curva del usuario.
//   - Techo del blanco robusto: protege el margen 16-bit del limbo. Mapear el
//     blanco medido contra 1.0 es pérdida de dato irreversible, así que ni en
//     Puro se suelta entero — sube a 0.985, indistinguible a la vista.
// ===========================================================================

const brightMaster = {
  histogram: {
    median: 45_000,
    percentileLow: 900,
    percentileHigh: 61_000,
    shadowClip: 0,
    highlightClip: 0.002,
    isMono: true,
  },
  artifacts: { sampledPixels: 200_000, suggestedDenoise: 3, ringingScore: 0.2 },
};

test("purity=1 deja las garantías anti-quemado exactamente como estaban", () => {
  for (const name of ["ha-natural", "chromosphere", "prominence", "filaments"]) {
    const sinDeclarar = adaptSolarPreset(name, brightMaster);
    const protegido = adaptSolarPreset(name, { ...brightMaster, purity: 1 });
    assert.deepEqual(
      protegido.curvePoints,
      sinDeclarar.curvePoints,
      `${name}: omitir purity debe equivaler a Protegido`,
    );
  }
});

test("el techo del blanco conserva margen 16-bit en los tres modos", () => {
  const percentileHigh = brightMaster.histogram.percentileHigh / 65_535;
  const median = brightMaster.histogram.median / 65_535;
  const brightAnchor = Math.max(percentileHigh, median + 0.05);

  for (const name of ["ha-natural", "chromosphere", "prominence", "filaments"]) {
    const mapeado = {};
    for (const [modo, purity] of [["protegido", 1], ["equilibrado", 0.5], ["puro", 0]]) {
      const adapted = adaptSolarPreset(name, { ...brightMaster, purity });
      mapeado[modo] = evaluateSolarCurve(adapted.curvePoints, brightAnchor);
    }
    // LO QUE SÍ ES INVARIANTE: ni siquiera en Puro se manda el blanco medido a
    // saturación. Es la única garantía que se mantiene en los tres modos, porque
    // protege dato, no gusto.
    for (const [modo, valor] of Object.entries(mapeado)) {
      assert.ok(
        valor <= 0.9855,
        `${name} @${modo}: el blanco medido llega a ${valor}, debe quedar bajo 0.985`,
      );
    }

    // LO QUE NO ES INVARIANTE: la DIRECCIÓN del cambio. `tonePreservation` tira
    // la curva hacia la diagonal, así que soltarlo sube las curvas que aclaran y
    // BAJA las que oscurecen. `prominence` oscurece en este anclaje, de modo que
    // Puro queda por debajo de Protegido — y es correcto: la curva del usuario
    // manda. Lo que sí se acota es que el cambio sea moderado, no un salto.
    assert.ok(
      Math.abs(mapeado.puro - mapeado.protegido) < 0.25,
      `${name}: el cambio entre modos debe ser gradual (${mapeado.protegido} → ${mapeado.puro})`,
    );
    assert.ok(
      Math.min(mapeado.protegido, mapeado.puro) - 1e-6 <= mapeado.equilibrado
        && mapeado.equilibrado <= Math.max(mapeado.protegido, mapeado.puro) + 1e-6,
      `${name}: Equilibrado (${mapeado.equilibrado}) debe quedar entre Protegido (${mapeado.protegido}) y Puro (${mapeado.puro})`,
    );
  }
});

test("la curva sigue siendo monótona con las protecciones relajadas", () => {
  for (const purity of [1, 0.5, 0]) {
    for (const name of ["ha-natural", "chromosphere", "prominence", "filaments"]) {
      const adapted = adaptSolarPreset(name, { ...brightMaster, purity });
      const samples = Array.from({ length: 32 }, (_, i) =>
        evaluateSolarCurve(adapted.curvePoints, i / 31));
      for (let i = 1; i < samples.length; i += 1) {
        assert.ok(
          samples[i] >= samples[i - 1] - 1e-4,
          `${name} @purity=${purity}: la curva debe seguir siendo monótona (${samples[i - 1]} → ${samples[i]})`,
        );
      }
      // Y no puede salirse del rango representable.
      assert.ok(samples[0] >= -1e-4 && samples[samples.length - 1] <= 1 + 1e-4);
    }
  }
});

// ===========================================================================
// LUCES QUEMADAS EN FALSO COLOR.
//
// `interpolate_solar_color` (Rust) escala el color hasta alcanzar la luminancia
// pedida (`scale = value / chroma_luma`). Por encima de la luminancia del color
// de luces el canal dominante se sale de gama, `fit_to_gamut` desatura para caber
// y los valores altos convergen al mismo tono: el degradado del limbo desaparece.
//
// El punto es DISTINTO en cada preset (0.863 en Prominencias, 0.914 en H-alpha
// dorado), así que la compresión no puede ser un número único: apunta a la paleta
// de cada uno. El techo lo fija `compress_solar_highlights`: `1 - amount*0.16`.
// ===========================================================================

const SOLAR_CEILING_FACTOR = 0.16;

const masterLimpio = {
  histogram: {
    median: 33_000,
    percentileLow: 700,
    percentileHigh: 55_700, // p99.8 ≈ 0.85, sin recorte
    shadowClip: 0,
    highlightClip: 0,
    isMono: true,
  },
  artifacts: { sampledPixels: 200_000, suggestedDenoise: 2, ringingScore: 0.15 },
};

const masterRecortado = {
  histogram: {
    median: 40_000,
    percentileLow: 900,
    percentileHigh: 65_000,
    shadowClip: 0,
    highlightClip: 0.004,
    isMono: true,
  },
  artifacts: { sampledPixels: 200_000, suggestedDenoise: 3, ringingScore: 0.3 },
};

const presetsActivos = Object.entries(SOLAR_CURVE_PRESETS).filter(([, p]) => p.enabled);

test("ningún preset deja el techo por encima del punto de aplanado de su paleta", () => {
  for (const [etiqueta, master] of [["limpio", masterLimpio], ["recortado", masterRecortado]]) {
    for (const [name, preset] of presetsActivos) {
      const adapted = adaptSolarPreset(name, master);
      const techo = 1 - adapted.highlightCompression * SOLAR_CEILING_FACTOR;
      const paleta = preset.colorize ? solarPaletteCeiling(preset.highlightColor) : 1;
      assert.ok(
        techo <= paleta,
        `${name} @${etiqueta}: techo ${techo.toFixed(3)} supera la paleta ${paleta.toFixed(3)} — el limbo pierde degradado`,
      );
    }
  }
});

test("un máster ya recortado recibe MÁS compresión que uno limpio", () => {
  for (const [name] of presetsActivos) {
    const limpio = adaptSolarPreset(name, masterLimpio).highlightCompression;
    const recortado = adaptSolarPreset(name, masterRecortado).highlightCompression;
    assert.ok(
      recortado >= limpio,
      `${name}: un máster con recorte medido (${recortado}) no puede comprimir menos que uno limpio (${limpio})`,
    );
  }
});

test("la compresión no se dispara: sigue habiendo recorrido en el decil alto", () => {
  // Con el techo `1 - a*0.16`, el hombro arranca en 0.82. Si la compresión
  // llegara al tope el limbo se aplanaría; el clamp de 0.92 lo impide.
  for (const [name] of presetsActivos) {
    for (const master of [masterLimpio, masterRecortado]) {
      const a = adaptSolarPreset(name, master).highlightCompression;
      assert.ok(a <= 0.92, `${name}: compresión ${a} fuera del rango permitido`);
      const techo = 1 - a * SOLAR_CEILING_FACTOR;
      assert.ok(
        techo > 0.82,
        `${name}: el techo ${techo.toFixed(3)} cae en la rodilla — el limbo quedaría plano`,
      );
    }
  }
});

test("el punto de aplanado se lee de cada paleta y no es un valor único", () => {
  assert.ok(Math.abs(solarPaletteCeiling("#ffe39a") - 0.8927) < 0.001);
  assert.ok(Math.abs(solarPaletteCeiling("#ffd995") - 0.8634) < 0.001);
  // Entradas inválidas no deben apagar la protección: 1 = "sin paleta que cuidar".
  for (const roto of [null, undefined, "", "#zzz", "no-es-color", 123]) {
    assert.equal(solarPaletteCeiling(roto), 1, `entrada inválida: ${String(roto)}`);
  }
});
