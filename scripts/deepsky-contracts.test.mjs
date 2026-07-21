import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const [main, html, en, es, abSchema] = await Promise.all([
    readFile(new URL("../src/main.js", import.meta.url), "utf8"),
    readFile(new URL("../index.html", import.meta.url), "utf8"),
    readFile(new URL("../src/locales/en.json", import.meta.url), "utf8").then(JSON.parse),
    readFile(new URL("../src/locales/es.json", import.meta.url), "utf8").then(JSON.parse),
    readFile(new URL("../benchmarks/deepsky-ab-run.schema.json", import.meta.url), "utf8").then(JSON.parse),
]);

const requestBuilder = main.match(
    /function dsBuildStackRequest\([\s\S]*?\n}\n\nfunction dsIsMultibandSession/,
)?.[0];
const picker = main.match(
    /async function dsPick\(kind\)[\s\S]*?\n}\n\n\/\/ Carpeta recursiva/,
)?.[0];
const captureModeOptions = main.match(
    /function dsEnsureCaptureModeOptions\(\)[\s\S]*?\n}\n\nfunction dsBuildStackRequest/,
)?.[0];

test("deep-sky v4 request carries dark-flats and strict capture defaults", () => {
    assert.ok(requestBuilder, "dsBuildStackRequest must remain identifiable");
    assert.match(requestBuilder, /schemaVersion:\s*DEEP_SKY_STACK_REQUEST_SCHEMA_VERSION/);
    assert.match(requestBuilder, /scientificProducts:\s*true/);
    assert.match(requestBuilder, /darkFlats:\s*dsCalibrationForIntegration\("darkFlats"/);
    assert.match(requestBuilder, /captureMode:\s*value\("sel-ds-capture-mode",\s*"auto"\)/);
    assert.match(requestBuilder, /calibrationPolicy:\s*value\("sel-ds-calibration-policy",\s*"strict"\)/);
});

test("science picker exposes only linear FITS and TIFF inputs", () => {
    assert.ok(picker, "dsPick must remain identifiable");
    assert.match(picker, /"fits",\s*"fit",\s*"fts",\s*"tif",\s*"tiff"/);
    assert.doesNotMatch(picker, /"png"|"jpg"|"jpeg"/);
});

test("deep-sky UI makes safe defaults and experimental engines explicit", () => {
    assert.match(html, /id="sel-ds-capture-mode"[\s\S]*?<option value="auto" selected/);
    assert.match(html, /id="sel-ds-calibration-policy"[\s\S]*?<option value="strict" selected/);
    assert.match(html, /option disabled data-i18n="deepsky\.method_experimental_group"/);
    assert.match(html, /option value="classic" selected/);
    assert.doesNotMatch(
        html,
        /<option[^>]+value="linearfit"/,
        "the rank-based approximation must not be offered as linear-fit clipping",
    );
    assert.match(html, /SCI, VAR, NEFF y DQ permanecen intactos y lineales/);
});

test("deep-sky UI exposes the localized broadband-mono capture contract", () => {
    assert.ok(captureModeOptions, "capture-mode extension must remain identifiable");
    assert.match(captureModeOptions, /option\.value\s*=\s*"broadbandMono"/);
    assert.match(captureModeOptions, /deepsky\.capture_broadband_mono/);
    assert.match(main, /dsEnsureCaptureModeOptions\(\);/);
    assert.equal(en.deepsky.capture_broadband_mono, "Broadband mono");
    assert.equal(es.deepsky.capture_broadband_mono, "Banda ancha mono");
    assert.ok(abSchema.properties.captureClass.enum.includes("broadbandMono"));
});

test("both locales describe the fifth calibration category and policies", () => {
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.pick_dark_flats);
        assert.ok(locale.deepsky.capture_mode);
        assert.ok(locale.deepsky.capture_broadband_mono);
        assert.ok(locale.deepsky.calibration_strict);
        assert.match(locale.deepsky.method_nebula_fusion, /experimental/i);
        assert.match(locale.deepsky.gradient, /SCI/i);
    }
});

test("preflight renders typed per-light calibration decisions", () => {
    assert.match(main, /function dsFormatCalibrationDecisions\(decisions\)/);
    assert.match(main, /dsFormatCalibrationDecisions\(plan\.calibrationDecisions\)/);
    assert.match(main, /decision\.darkFlatMasterPath/);
    assert.match(main, /decision\.darkScale/);
    assert.doesNotMatch(main, /warning\.startsWith\("CALIBRATION_DECISIONS="\)/);
});

test("preliminary calibration cards never claim scientific compatibility", () => {
    assert.match(main, /function dsExactExposureMatch\(a, b\)/);
    assert.match(main, /Math\.max\(0\.001,[\s\S]*?1e-6\)/);
    assert.match(main, /status === "candidate"/);
    assert.match(main, /la matriz tipada del backend es la autoridad/);
    assert.doesNotMatch(main, /BIAS: universal/);
    assert.doesNotMatch(main, /se escalarán automáticamente/);
    assert.match(es.deepsky.plan_scale_note, /No se aceptarán ni escalarán/);
    assert.match(en.deepsky.plan_scale_note, /will not be accepted or scaled/);
});

test("master quality UI exposes detector-pattern diagnostics", () => {
    assert.match(main, /const pattern = s\.detectorPattern/);
    assert.match(main, /pattern\.bandingSigma\.toFixed\(2\)/);
    assert.ok(es.deepsky.q_banding_detected);
    assert.ok(en.deepsky.q_banding_clear);
});

test("inspection consumes the typed report with dither prediction and pattern", () => {
    assert.match(main, /invoke\("inspect_deepsky_frames"/);
    assert.match(main, /report\?\.frames/);
    assert.match(main, /dither:\s*report\?\.dither/);
    assert.match(main, /detectorPattern:\s*report\?\.detectorPattern/);
    assert.match(main, /function dsFormatInspectionDiagnostics\(\)/);
    assert.match(main, /dither\.walkingNoiseRisk/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.dither_risk);
        assert.ok(locale.deepsky.dither_ok);
        assert.ok(locale.deepsky.dither_prediction_note);
        assert.ok(locale.deepsky.pattern_detected);
        assert.ok(locale.deepsky.pattern_ok);
    }
});

test("experimental engines are gated by scientific eligibility", () => {
    assert.match(main, /plan\?\.scientificEligible !== false/);
    assert.match(
        main,
        /\["nebula_fusion", "nebula_fusion_full", "nebula_fusion_struct", "eidr"\]/,
    );
    assert.match(main, /option\.disabled = !scientificEligible/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.method_blocked_nonlinear);
    }
});

test("balanced preset mirrors the backend resolved profile (winsorized)", () => {
    assert.match(
        main,
        /balanced:\s*\{[^}]*rejection:\s*"winsorized"/,
        "DS_PRESETS.balanced must match PipelineProfile::Balanced (winsorized)",
    );
    assert.match(html, /<b>Equilibrado<\/b>[^<]*Winsorized/);
});

test("session results render the typed scientific bundle manifest", () => {
    assert.match(main, /group\.scientificBundle/);
    assert.match(main, /bundle\.products/);
    assert.match(main, /product\.bunit/);
    assert.match(main, /bundle\.fallbacks/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.bundle_title);
        assert.ok(locale.deepsky.product_derived);
        assert.ok(locale.deepsky.product_linear);
    }
});

test("AUTO preset is the default and its resolved recipe is displayed", () => {
    assert.match(
        html,
        /<button type="button" class="ds-preset active" data-preset="auto" data-i18n="deepsky\.preset_auto">Auto<\/button>/,
        "Auto must be the first, active preset",
    );
    assert.doesNotMatch(
        html,
        /class="ds-preset active" data-preset="balanced"/,
        "balanced must no longer be the default preset",
    );
    assert.match(main, /let dsActivePreset = "auto";/);
    assert.match(main, /auto: "auto", fast: "fast", balanced: "balanced", max: "maximum_quality"/);
    assert.match(main, /plan\.resolvedRecipe/);
    assert.match(main, /deepsky\.resolved_recipe_title/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.preset_auto);
        assert.ok(locale.deepsky.profile_auto_note);
        assert.ok(locale.deepsky.resolved_recipe_title);
        assert.ok(locale.deepsky.resolved_signals);
    }
});

test("session request excludes manual discards and drops emptied groups", () => {
    assert.match(main, /files: group\.files\.filter\(f => !dsDiscardedPaths\.has\(f\.path\)\)/);
    assert.match(main, /\.filter\(group => group\.files\.length > 0\)/);
});

test("manual calibration assignment is wired end to end", () => {
    assert.ok(requestBuilder, "dsBuildStackRequest must remain identifiable");
    assert.match(requestBuilder, /calibrationOverrides:\s*\[/);
    assert.match(requestBuilder, /sel-ds-manual-darks/);
    assert.match(requestBuilder, /sel-ds-manual-flats/);
    assert.match(html, /id="sel-ds-manual-darks"[\s\S]*?<option value="auto" selected/);
    assert.match(html, /id="sel-ds-manual-flats"[\s\S]*?<option value="auto" selected/);
    assert.match(main, /decision\.manual/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.manual_darks_label);
        assert.ok(locale.deepsky.manual_flats_label);
        assert.ok(locale.deepsky.manual_hint);
        assert.ok(locale.deepsky.decision_manual);
    }
});

test("preflight floods are grouped and strict offers a degraded path", () => {
    assert.match(main, /function dsGroupAlertMessages\(messages\)/);
    assert.match(main, /btn-ds-proceed-degraded/);
    assert.match(main, /policy\.value = "allowDegraded"/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.proceed_degraded);
        assert.ok(locale.deepsky.proceed_hint);
        assert.ok(locale.deepsky.alert_expand);
    }
});

test("dynamic UI strings resolve through both locales", () => {
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.reclassify);
        assert.ok(locale.deepsky.export_float32);
        assert.ok(locale.deepsky.repeat_integration);
        assert.ok(locale.deepsky.repeat_integration_hint);
        assert.equal(locale.deepsky.rej_linearfit, undefined,
            "linearfit locale leftovers must stay removed");
    }
});
