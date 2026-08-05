import assert from "node:assert/strict";
import test from "node:test";

import {
  MILKY_WAY_RECIPE_SCHEMA,
  MILKY_WAY_MODES,
  MILKY_WAY_STEPS,
  buildMilkyWayRequest,
  createMilkyWayState,
  cropMilkyWayProducts,
  setMilkyWayStep,
  validateMilkyWayState,
} from "../src/milky_way_state.js";

const frame = (name, overrides = {}) => ({
  path: `/night/${name}.fits`,
  name,
  width: 6000,
  height: 4000,
  channels: 3,
  exposureSeconds: 10,
  iso: 3200,
  ...overrides,
});

function readyState(overrides = {}) {
  const lights = Array.from({ length: 8 }, (_, index) => frame(`mw-${index + 1}`));
  return createMilkyWayState({
    lights,
    baseFramePath: lights[4].path,
    outputDirectory: "/night/output",
    mask: { strategy: "auto", confidence: 0.94, userConfirmed: true },
    registration: {
      skySolved: true,
      groundSolved: true,
      inliers: 126,
      rmsPx: 0.74,
      singleResample: true,
    },
    ...overrides,
  });
}

test("request never ships stringified numbers even if the UI writes them", () => {
  const state = readyState({
    mask: {
      strategy: "auto",
      confidence: "0.94",
      userConfirmed: true,
      featherPx: "54",
      brushRadiusPx: "42",
    },
    integration: { skySigmaLow: "3.2", skySigmaHigh: "2.8" },
    composition: { featherPx: "128", edgeDeghost: "0.65" },
  });
  const request = buildMilkyWayRequest(state);
  assert.equal(request.mask.featherPx, 54);
  assert.equal(request.mask.brushRadiusPx, 42);
  assert.equal(request.mask.confidence, 0.94);
  assert.equal(request.integration.skySigmaLow, 3.2);
  assert.equal(request.integration.skySigmaHigh, 2.8);
  assert.equal(request.composition.featherPx, 128);
  assert.equal(request.composition.edgeDeghost, 0.65);
  const numericLeaks = [];
  const walk = (value, path) => {
    if (value == null) return;
    if (typeof value === "string" && value !== "" && Number.isFinite(Number(value))) {
      numericLeaks.push(path);
    } else if (Array.isArray(value)) {
      value.forEach((item, index) => walk(item, `${path}[${index}]`));
    } else if (typeof value === "object") {
      for (const [key, item] of Object.entries(value)) walk(item, `${path}.${key}`);
    }
  };
  for (const section of ["mask", "integration", "composition"]) walk(request[section], section);
  assert.deepEqual(numericLeaks, []);
});

test("keeps the Milky Way workflow in an explicit six-step order", () => {
  assert.equal(MILKY_WAY_RECIPE_SCHEMA, "zenith-milky-way-recipe-v1");
  assert.deepEqual(MILKY_WAY_STEPS.map(([id]) => id), [
    "data", "mask", "registration", "integration", "composition", "publish",
  ]);
});

test("freeze-ground requires four frames and a confirmed mask", () => {
  const state = createMilkyWayState({
    lights: [frame("a"), frame("b"), frame("c")],
    baseFramePath: "/night/b.fits",
    outputDirectory: "/night/output",
  });
  const result = validateMilkyWayState(state);
  assert.equal(result.valid, false);
  assert.match(result.blockers.join(" "), /cuatro tomas/i);
  assert.match(result.blockers.join(" "), /confirma la máscara/i);
});

test("sky-only does not require a foreground mask or ground registration", () => {
  const state = readyState({
    mode: MILKY_WAY_MODES.SKY_ONLY,
    mask: { strategy: "fullSky", confidence: 1, userConfirmed: false },
    registration: { skySolved: true, groundSolved: false, inliers: 80, rmsPx: 0.9 },
  });
  const result = validateMilkyWayState(state);
  assert.equal(result.valid, true);
  assert.deepEqual(result.products, [
    "sky", "mask", "skyVariance", "skyCoverage", "skyRejection", "recipe",
  ]);
});

test("different exposures are blocked until unify-exposure is explicit", () => {
  const state = readyState();
  state.lights[2].exposureSeconds = 13;
  assert.match(validateMilkyWayState(state).blockers.join(" "), /unificar exposición/i);
  state.unifyExposure = true;
  const validated = validateMilkyWayState(state);
  assert.equal(validated.valid, true);
  assert.match(validated.warnings.join(" "), /concesión/i);
});

test("an unreadable included file blocks the recipe until it is deselected", () => {
  const state = readyState();
  state.lights[1].ok = false;
  state.lights[1].error = "cabecera FITS inválida";
  assert.match(validateMilkyWayState(state).blockers.join(" "), /no se pueden leer/i);
  state.lights[1].included = false;
  assert.equal(validateMilkyWayState(state).valid, true);
});

test("unsupported capture modes fall back to the safe freeze-ground workflow", () => {
  const state = readyState({ mode: "starTrails" });
  assert.equal(state.mode, MILKY_WAY_MODES.FREEZE_GROUND);
  assert.equal(validateMilkyWayState(state).valid, true);
});

test("Essential and Expert emit byte-equivalent processing requests", () => {
  const essential = readyState({ presentationMode: "essential" });
  const expert = readyState({ presentationMode: "expert" });
  assert.deepEqual(buildMilkyWayRequest(essential), buildMilkyWayRequest(expert));
  assert.equal("presentationMode" in buildMilkyWayRequest(essential), false);
});

test("defaults to Strict and never lets a legacy layer toggle suppress scientific products", () => {
  const state = readyState({ composition: { keepSeparateLayers: false } });
  const request = buildMilkyWayRequest(state);
  assert.equal(state.fallbackPolicy, "strict");
  assert.equal(request.fallbackPolicy, "strict");
  assert.equal(request.composition.keepSeparateLayers, true);

  const degraded = readyState({ fallbackPolicy: "allowDegraded" });
  assert.equal(buildMilkyWayRequest(degraded).fallbackPolicy, "allowDegraded");
  assert.match(validateMilkyWayState(degraded).warnings.join(" "), /no científico/i);
});

test("normalizes the contradictory radial-wide plus distortion-off pair", () => {
  const state = readyState({
    registration: {
      model: "radialWide",
      distortionCorrection: "off",
      skySolved: true,
      groundSolved: true,
      inliers: 126,
      rmsPx: 0.74,
      singleResample: true,
    },
  });
  assert.equal(state.registration.distortionCorrection, "auto");
  assert.equal(buildMilkyWayRequest(state).registration.distortionCorrection, "auto");
});

test("blocks known incompatible calibration metadata and warns on unknown metadata", () => {
  const darkMismatch = readyState({ darks: [frame("dark", { exposureSeconds: 30 })] });
  assert.match(validateMilkyWayState(darkMismatch).blockers.join(" "), /darks conocidos/i);

  const flatMismatch = readyState({ flats: [frame("flat", { iso: 800 })] });
  assert.match(validateMilkyWayState(flatMismatch).blockers.join(" "), /flats conocidos/i);

  const unknown = readyState({
    darks: [{ path: "/night/dark-unknown.fits", width: 6000, height: 4000, channels: 3 }],
    flats: [{ path: "/night/flat-unknown.fits", width: 6000, height: 4000, channels: 3 }],
  });
  const warningText = validateMilkyWayState(unknown).warnings.join(" ");
  assert.match(warningText, /Darks con metadatos incompletos/i);
  assert.match(warningText, /Flats con metadatos incompletos/i);
  assert.equal(unknown.darks[0].exposureSeconds, null);
  assert.equal(unknown.darks[0].iso, null);
});

test("forward navigation cannot jump over an unresolved step", () => {
  const state = createMilkyWayState({ lights: [] });
  const jump = setMilkyWayStep(state, 4);
  assert.equal(jump.changed, false);
  assert.match(jump.blocker, /dos tomas/i);
  const back = setMilkyWayStep({ ...state, activeStep: 3 }, 0);
  assert.equal(back.changed, true);
  assert.equal(back.state.activeStep, 0);
});

test("step navigation preserves session errors, crop and published product review", () => {
  const state = readyState({
    runError: "falló el preflight sin modificar productos",
    crop: { left: 12, top: 18, width: 5800, height: 3800 },
    products: [{ kind: "sky", path: "/night/output/sky.fits" }],
  });
  const moved = setMilkyWayStep(state, 1);
  assert.equal(moved.changed, true);
  assert.equal(moved.state.runError, state.runError);
  assert.deepEqual(moved.state.crop, state.crop);
  assert.deepEqual(moved.state.products, state.products);

  const request = buildMilkyWayRequest(moved.state);
  assert.equal("runError" in request, false);
  assert.equal("crop" in request, false);
  assert.equal("products" in request, false);
});

test("crop applies one geometry id to sky, ground, composite and maps", () => {
  const cropped = cropMilkyWayProducts(readyState(), {
    left: 120,
    top: 80,
    width: 5200,
    height: 3400,
  });
  assert.equal(cropped.products.length, 11);
  assert.equal(new Set(cropped.products.map(product => product.geometryId)).size, 1);
  assert.equal(cropped.mask.geometryId, cropped.geometryId);
});
