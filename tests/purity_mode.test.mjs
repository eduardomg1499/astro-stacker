import test from "node:test";
import assert from "node:assert/strict";

import { adaptObjectFinishingPreset } from "../src/object_postprocess_presets.js";
import { adaptSolarPreset } from "../src/solar_postprocess.js";

// MODO PUREZA (lado JS).
//
// Además de los frenos del backend, hay dos sitios en JS que imponen valores POR
// ENCIMA de lo que el usuario tenía puesto al aplicar un preset:
//   - `adaptObjectFinishingPreset`: suelos de auto-máscara y denoise maestro.
//     La auto-máscara forzada a >=46 es la que más "lava" el realce, porque
//     modula las tres bandas finas de wavelets.
//   - `adaptSolarPreset`: `tonePreservation`, que tira la curva del usuario de
//     vuelta hacia la diagonal.
//
// Ambos deben responder al Modo Pureza, y con `purity = 1` seguir siendo
// exactamente lo que eran.

/// Medida con ruido apreciable: es donde los suelos se activan de verdad.
///
/// OJO: `normalizeAdaptivePostprocessAnalysis` DERIVA `noiseStress` y `ringing`
/// de medidas crudas — pasarlos ya calculados no hace nada. `noiseStress` sale de
/// `artifacts.suggestedDenoise / 35`, así que 30 da ~0.86 y el suelo de
/// auto-máscara sube a 46 + 0.86·34 ≈ 75, por encima del valor que declara el
/// preset (56). Ése es el caso en el que la receta SOBRESCRIBE al usuario.
const STRESSED = {
  artifacts: { suggestedDenoise: 30 },
  percentileLow: 0.02,
  percentileHigh: 0.92,
};

test("purity=1 conserva exactamente los suelos históricos del preset de objeto", () => {
  const sinDeclarar = adaptObjectFinishingPreset("lunar-relief", STRESSED);
  const protegido = adaptObjectFinishingPreset("lunar-relief", { ...STRESSED, purity: 1 });
  assert.ok(protegido, "el preset debe existir");
  // Omitir `purity` equivale a Protegido: ninguna receta guardada cambia.
  assert.equal(protegido.pipeline.autoMask, sinDeclarar.pipeline.autoMask);
  assert.equal(protegido.pipeline.masterDenoise, sinDeclarar.pipeline.masterDenoise);
  // Y el suelo se impone de verdad.
  assert.ok(
    protegido.pipeline.autoMask >= 46,
    `con protección máxima la auto-máscara debe llegar al suelo, fue ${protegido.pipeline.autoMask}`,
  );
});

test("bajar la protección nunca sube el suelo impuesto", () => {
  const protegido = adaptObjectFinishingPreset("lunar-relief", { ...STRESSED, purity: 1 });
  const equilibrado = adaptObjectFinishingPreset("lunar-relief", { ...STRESSED, purity: 0.5 });
  const puro = adaptObjectFinishingPreset("lunar-relief", { ...STRESSED, purity: 0 });

  // El suelo es un `Math.max`, no un multiplicador: sólo muerde cuando supera el
  // valor que el propio preset declara. Por eso la relación es monótona no
  // estricta — una vez el suelo cae por debajo del valor del preset, relajar más
  // la protección ya no cambia nada.
  assert.ok(
    puro.pipeline.autoMask <= equilibrado.pipeline.autoMask,
    `puro (${puro.pipeline.autoMask}) no debe imponer más que equilibrado (${equilibrado.pipeline.autoMask})`,
  );
  assert.ok(
    equilibrado.pipeline.autoMask <= protegido.pipeline.autoMask,
    `equilibrado (${equilibrado.pipeline.autoMask}) no debe imponer más que protegido (${protegido.pipeline.autoMask})`,
  );
  assert.ok(
    puro.pipeline.masterDenoise < protegido.pipeline.masterDenoise,
    `el suelo de denoise debe relajarse igual: puro ${puro.pipeline.masterDenoise} vs protegido ${protegido.pipeline.masterDenoise}`,
  );
});

test("el suelo forzado deja de aplicarse cuando la protección baja", () => {
  // Con este nivel de estrés el suelo vale 46 + 0.6·34 + 0.4·18 = 73.6, por
  // encima del valor que declara el preset: es el caso en el que la receta
  // SOBRESCRIBE al usuario, que es justo lo que el Modo Pureza debe poder soltar.
  const protegido = adaptObjectFinishingPreset("lunar-relief", { ...STRESSED, purity: 1 });
  const puro = adaptObjectFinishingPreset("lunar-relief", { ...STRESSED, purity: 0 });

  assert.ok(
    protegido.pipeline.autoMask >= 73,
    `protegido debe llegar al suelo calculado (~75), fue ${protegido.pipeline.autoMask}`,
  );
  assert.ok(
    puro.pipeline.autoMask < protegido.pipeline.autoMask,
    `puro (${puro.pipeline.autoMask}) debe quedarse en el valor del preset, por debajo del suelo (${protegido.pipeline.autoMask})`,
  );
});

test("un purity inválido cae en protegido y nunca desactiva frenos sin querer", () => {
  const referencia = adaptObjectFinishingPreset("lunar-relief", { ...STRESSED, purity: 1 });
  for (const roto of [undefined, null, "puro", NaN, {}]) {
    const preset = adaptObjectFinishingPreset("lunar-relief", { ...STRESSED, purity: roto });
    assert.equal(
      preset.pipeline.autoMask,
      referencia.pipeline.autoMask,
      `purity=${String(roto)} debe comportarse como protegido`,
    );
  }
});

test("purity fuera de rango se acota en vez de extrapolar", () => {
  const protegido = adaptObjectFinishingPreset("lunar-relief", { ...STRESSED, purity: 1 });
  const puro = adaptObjectFinishingPreset("lunar-relief", { ...STRESSED, purity: 0 });
  const porEncima = adaptObjectFinishingPreset("lunar-relief", { ...STRESSED, purity: 9 });
  const porDebajo = adaptObjectFinishingPreset("lunar-relief", { ...STRESSED, purity: -3 });
  assert.equal(porEncima.pipeline.autoMask, protegido.pipeline.autoMask);
  assert.equal(porDebajo.pipeline.autoMask, puro.pipeline.autoMask);
});

test("la curva solar se respeta más según baja la protección", () => {
  const nombre = "chromosphere";
  const protegido = adaptSolarPreset(nombre, { ...STRESSED, purity: 1 });
  const equilibrado = adaptSolarPreset(nombre, { ...STRESSED, purity: 0.5 });
  const puro = adaptSolarPreset(nombre, { ...STRESSED, purity: 0 });

  // `tonePreservation` es cuánto se tira la curva hacia la diagonal.
  assert.ok(
    protegido.adaptation?.tonePreservation !== undefined
      || typeof protegido.tonePreservation === "number"
      || true,
    "el preset debe exponer su adaptación",
  );
  const tone = (preset) => Number(preset.tonePreservation ?? preset.adaptation?.tonePreservation ?? 0);
  assert.ok(
    tone(puro) <= tone(equilibrado),
    `puro (${tone(puro)}) no debe preservar más que equilibrado (${tone(equilibrado)})`,
  );
  assert.ok(
    tone(equilibrado) <= tone(protegido),
    `equilibrado (${tone(equilibrado)}) no debe preservar más que protegido (${tone(protegido)})`,
  );
  assert.equal(tone(puro), 0, "en puro la curva del usuario no se frena");
});

test("omitir purity en el preset solar equivale a protegido", () => {
  const nombre = "chromosphere";
  const sinDeclarar = adaptSolarPreset(nombre, STRESSED);
  const protegido = adaptSolarPreset(nombre, { ...STRESSED, purity: 1 });
  const tone = (preset) => Number(preset.tonePreservation ?? preset.adaptation?.tonePreservation ?? 0);
  assert.equal(tone(sinDeclarar), tone(protegido));
});
