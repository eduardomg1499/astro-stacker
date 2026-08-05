import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const [main, html, en, es, fr, it, abSchema, deepskyRust, pipelineRust, poststackRust, spccRust, commandsCoreRust] = await Promise.all([
    readFile(new URL("../src/main.js", import.meta.url), "utf8"),
    readFile(new URL("../index.html", import.meta.url), "utf8"),
    readFile(new URL("../src/locales/en.json", import.meta.url), "utf8").then(JSON.parse),
    readFile(new URL("../src/locales/es.json", import.meta.url), "utf8").then(JSON.parse),
    readFile(new URL("../src/locales/fr.json", import.meta.url), "utf8").then(JSON.parse),
    readFile(new URL("../src/locales/it.json", import.meta.url), "utf8").then(JSON.parse),
    readFile(new URL("../benchmarks/deepsky-ab-run.schema.json", import.meta.url), "utf8").then(JSON.parse),
    readFile(new URL("../src-tauri/src/deepsky.rs", import.meta.url), "utf8"),
    readFile(new URL("../src-tauri/src/pipeline.rs", import.meta.url), "utf8"),
    readFile(new URL("../src-tauri/src/deepsky_poststack.rs", import.meta.url), "utf8"),
    readFile(new URL("../src-tauri/src/spcc.rs", import.meta.url), "utf8"),
    readFile(new URL("../src-tauri/src/commands_core.rs", import.meta.url), "utf8"),
]);
const styles = await readFile(new URL("../src/styles.css", import.meta.url), "utf8");

const requestBuilder = main.match(
    /function dsBuildStackRequest\([\s\S]*?\n}\n\nfunction dsIsMultibandSession/,
)?.[0];
const picker = main.match(
    /async function dsPick\(kind\)[\s\S]*?\n}\n\n\/\/ Carpeta recursiva/,
)?.[0];
const captureModeOptions = main.match(
    /function dsEnsureCaptureModeOptions\(\)[\s\S]*?\n}\n\nfunction dsBuildStackRequest/,
)?.[0];

test("scientific launchers serialize initialization and stack launch", () => {
    assert.match(main, /let dsRunLaunchPending = false/);
    assert.match(
        main,
        /btn-deepsky-run"\)\?\.addEventListener\("click", async \(\) => \{[\s\S]{0,420}if \(dsStacking \|\| dsRunLaunchPending\)/,
        "the Deep Sky handler must close the preflight double-click window",
    );
    assert.match(main, /dsRunLaunchPending = true/);
    assert.match(main, /finally \{[\s\S]{0,180}dsRunLaunchPending = false/);
    assert.match(main, /let milkyWayFlowPromise = null/);
    assert.match(main, /milkyWayFlowPromise \?\?= import\("\.\/milky_way_ui\.js"\)/);
    assert.match(main, /await milkyWayFlowPromise/);
});

test("Studio timeline maps explicitly to the scientific plan with one icon per step", () => {
    const block = main.match(/const DS_POSTSTACK_STEPS = \[[\s\S]*?\n\];/)?.[0] || "";
    const rows = [...block.matchAll(/\{ id: (\d+), planId: "([^"]+)"[^}]+icon: "([^"]+)"/g)]
        .map(match => ({ id: Number(match[1]), planId: match[2], icon: match[3] }));
    assert.deepEqual(rows.map(row => row.planId), [
        "crop",
        "background_gradient",
        "astrometry",
        "channels_color",
        "psf_deconvolution",
        "linear_denoise",
        "star_layers",
        "stretch",
        "curves_color",
        "detail",
        "finish",
        "export",
    ]);
    assert.equal(new Set(rows.map(row => row.icon)).size, rows.length,
        "each Studio step must have a distinct semantic icon");
    assert.match(main, /plannedSteps\.get\(step\.planId\)/);
    assert.match(main, /function dsPoststackPlanStep\(/);
    assert.match(html, /id="icon-curves"/);
});

test("scientific tooltips, confirmations and status marks use the shared accessible UI", () => {
    const tooltipKeys = [...html.matchAll(/data-i18n-title="deepsky\.(tip_[^"]+)"/g)]
        .map(match => match[1]);
    assert.equal(tooltipKeys.length, 25, "all scientific advanced controls need localized tooltips");
    assert.equal(new Set(tooltipKeys).size, tooltipKeys.length);
    for (const key of tooltipKeys) {
        assert.equal(typeof es.deepsky[key], "string", `missing es.deepsky.${key}`);
        assert.equal(typeof en.deepsky[key], "string", `missing en.deepsky.${key}`);
    }
    assert.doesNotMatch(main, /window\.confirm\(/,
        "scientific decisions must use the focus-trapped application dialog");
    assert.match(main, /await showCustomChoice\(/);
    assert.doesNotMatch(main, /⚠|✕/);
    assert.match(main, /id="ds-report-x"[\s\S]{0,700}href="#icon-cross"/);
});

test("deep-sky request carries dark-flats and strict capture defaults", () => {
    assert.ok(requestBuilder, "dsBuildStackRequest must remain identifiable");
    assert.match(requestBuilder, /schemaVersion:\s*DEEP_SKY_STACK_REQUEST_SCHEMA_VERSION/);
    assert.match(requestBuilder, /scientificProducts:\s*true/);
    assert.match(requestBuilder, /darkFlats:\s*dsCalibrationForIntegration\("darkFlats"/);
    assert.match(requestBuilder, /captureMode:\s*value\("sel-ds-capture-mode",\s*"auto"\)/);
    assert.match(requestBuilder, /calibrationPolicy:\s*value\("sel-ds-calibration-policy",\s*"strict"\)/);
    assert.match(requestBuilder, /integrationProducts:\s*dsBuildIntegrationProducts\(\)/);
});

test("deep-sky inherits General compute settings and keeps Expert overrides explicit", () => {
    assert.match(
        html,
        /id="sel-ds-compute"[\s\S]{0,180}<option value="global" selected data-i18n="deepsky\.compute_global"/,
    );
    assert.match(requestBuilder, /computePolicy:\s*dsResolvedComputePolicy\(\)/);
    assert.match(main, /resolveDeepSkyComputePolicy\([\s\S]{0,160}getComputePolicy\(\)/);
    assert.match(main, /dsSyncComputePolicyUi\(\{ refresh: true \}\)/);
    for (const locale of [en, es]) {
        assert.ok(locale.settings.general.gpu_title.toLowerCase().includes("planet"));
        assert.match(locale.settings.general.gpu_title, /deep sky|cielo profundo/i);
        assert.ok(locale.deepsky.compute_global);
        assert.ok(locale.deepsky.compute_global_effective);
        assert.ok(locale.deepsky.compute_override_effective);
    }
});

test("deep-sky UI discloses GPU conservatively and never invents GPU load", () => {
    const preview = main.match(/function dsRenderProcessPreview\(\)[\s\S]*?\n}\n\nfunction dsUpdateUI/)?.[0] || "";
    const resources = main.match(/function dsProgressRenderResources\(telemetry = null\)[\s\S]*?\n}\n\nfunction dsProgressRenderEditorSteps/)?.[0] || "";
    assert.match(preview, /describeDeepSkyComputePlan\(/);
    assert.match(preview, /tiled_rejection_cpu/);
    assert.doesNotMatch(preview, /GPU tiled|GPU calibración/);
    assert.match(resources, /deepSkyTelemetryComputeStatus\(/);
    assert.doesNotMatch(resources, /gpu_percent/);
    assert.match(main, /product\.computeEngine/);
    assert.match(main, /product\.computeReason/);
    const parityGate = deepskyRust.match(
        /let deep_gpu_parity_ok[\s\S]*?\n    };/,
    )?.[0] || "";
    assert.match(parityGate, /ensure_pixel_preprocess_parity/);
    assert.match(parityGate, /ensure_advanced_warp_parity/);
    assert.doesNotMatch(
        parityGate,
        /ensure_tiled_parity/,
        "a CPU-only tiled rejection gate must not disable valid GPU preprocessing",
    );
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.compute_gpu_active);
        assert.ok(locale.deepsky.compute_gpu_inactive);
        assert.ok(locale.deepsky.compute_product_cpu);
        assert.ok(locale.deepsky.compute_tiled_cpu);
    }
});

test("recipe v5 exposes Classic, NebulaFusion SCI, STRUCT and EIDR in parallel", () => {
    assert.match(main, /const DEEP_SKY_STACK_REQUEST_SCHEMA_VERSION = 5/);
    assert.match(html, /id="ds-product-controls-slot"/);
    assert.match(main, /function dsMountProductControls\(\)/);
    for (const id of [
        "chk-ds-product-classic",
        "chk-ds-product-nf",
        "chk-ds-product-struct",
        "chk-ds-product-eidr",
    ]) {
        assert.match(html, new RegExp(`id="${id}"`));
    }
    assert.match(html, /id="sel-ds-primary-product"/);
    assert.match(main, /function dsBuildIntegrationProducts\(\)/);
    assert.match(main, /primary === "struct" \? "nebula_fusion_struct"/);
    assert.match(pipelineRust, /DEEP_SKY_STACK_REQUEST_SCHEMA_VERSION:\s*u16\s*=\s*5/);
    assert.match(pipelineRust, /DEEP_SKY_STACK_REQUEST_MIN_READABLE_VERSION:\s*u16\s*=\s*4/);
    assert.match(pipelineRust, /Self::Struct => "struct"/);
    assert.match(pipelineRust, /\.then_some\("nebula_fusion_sci"\)/);
    assert.match(deepskyRust, /for \(product_index, product\) in products\.into_iter\(\)\.enumerate\(\)/);
    assert.match(main, /option\.dataset\.productKind = product\.product/);
    assert.match(main, /selectedProductKind === "struct"/);
    assert.match(main, /invoke\("deepsky_result_view", \{ kind: "struct" \}\)/);
    assert.match(html, /id="ds-product-combination-summary"/);
    assert.match(html, /id="ds-product-compatibility"/);
    assert.match(main, /if \(!selected\.size\)[\s\S]{0,180}classic\.checked = true/);
    assert.match(main, /mismas tomas y decisiones de calibración; integración separada; no se mezclan/);
    assert.match(main, /STRUCT calculará NebulaFusion Full internamente/);
    assert.match(main, /classicAddedForDrizzle/);
    assert.match(main, /flujos paralelos · másters separados/);
    assert.match(main, /function dsRequestedIntegrationBranchPlan\(\)/);
    assert.match(main, /effectiveDrizzle: product\.product === "classic" \? drizzle : 1/);
    assert.match(main, /branchProducts\.length > 1/);
    assert.match(main, /review_parallel_outputs/);
    assert.match(main, /dsWizardStep === 4 && dsPreparedPlan/);
    assert.match(deepskyRust, /product\.effective_drizzle\(request\.drizzle\)/);
    assert.match(deepskyRust, /fn ds_safe_drizzle_pixfrac\(/);
    assert.match(deepskyRust, /drizzleSamplingAdjustment/);
    assert.match(deepskyRust, /fn ds_product_runtime_disclosure\(/);
    assert.match(deepskyRust, /Some\(product_drizzle\)/);
    assert.match(deepskyRust, /let products = resolved\.resolved_integration_products\(\)/);
    assert.match(deepskyRust, /primary_product_id: primary_id\.clone\(\)/);
    assert.doesNotMatch(
        deepskyRust,
        /EIDR sustituye a drizzle: deja drizzle en 1×/,
        "EIDR y Classic Drizzle deben ejecutarse en ramas independientes",
    );
    assert.match(pipelineRust, /deepsky_v5_all_products_are_parallel_and_struct_requires_full_dependency/);
    assert.match(pipelineRust, /deepsky_v5_classic_drizzle_and_eidr_use_independent_sampling_branches/);
    assert.match(pipelineRust, /pub fn effective_drizzle\(/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.product_classic_hint);
        assert.ok(locale.deepsky.product_nf_hint);
        assert.ok(locale.deepsky.product_struct_hint);
        assert.ok(locale.deepsky.product_eidr_hint);
        assert.ok(locale.deepsky.primary_product_hint);
        assert.ok(locale.deepsky.products_compatibility_title);
        assert.ok(locale.deepsky.products_shared);
        assert.ok(locale.deepsky.products_separate);
        assert.ok(locale.deepsky.products_parameters);
        assert.ok(locale.deepsky.product_parallel_title);
        assert.ok(locale.deepsky.product_branch_classic);
        assert.ok(locale.deepsky.product_branch_eidr);
        assert.ok(locale.deepsky.review_parallel_outputs);
    }
});

test("comet mode requires timestamps and confirmation and publishes separate layers", () => {
    assert.match(html, /id="sel-ds-target-mode"/);
    assert.match(html, /id="btn-ds-detect-comet"/);
    assert.match(main, /invoke\("detect_deepsky_comet"/);
    assert.match(main, /function dsBuildCometRequest\(\)/);
    assert.match(pipelineRust, /pub struct CometStackRequest/);
    assert.match(pipelineRust, /pub struct CometStackResultHandle/);
    assert.match(deepskyRust, /fn ds_fit_comet_observations/);
    assert.match(deepskyRust, /translated_output/);
    assert.match(deepskyRust, /fn ds_comet_translation_in_stack_grid/);
    assert.match(deepskyRust, /detector_to_stack\.forward/);
    assert.match(deepskyRust, /"effectiveGrid"/);
    assert.match(deepskyRust, /no se aplicará una máscara en coordenadas supuestas/);
    assert.match(deepskyRust, /CometLayerKind::Stars/);
    assert.match(deepskyRust, /CometLayerKind::Comet/);
    assert.match(deepskyRust, /CometLayerKind::Combined/);
    assert.match(deepskyRust, /timestamp \+ exptime as f64 \* 0\.5/);
    assert.match(deepskyRust, /comet-cross-trajectory-residual/);
    assert.match(deepskyRust, /signed comet residual under soft mask/);
    assert.match(deepskyRust, /shared-input covariance/);
    assert.doesNotMatch(deepskyRust, /comet-star-model-subtracted/);
});

test("linear editor keeps an immutable source and Gaia PCC is idempotent", () => {
    assert.match(main, /const DS_POSTSTACK_STEPS = \[/);
    assert.match(main, /deepsky_poststack_apply_gradient/);
    assert.match(main, /solve_deepsky_astrometry/);
    assert.match(main, /pcc_gaia_calibrate/);
    assert.match(html, /#ds-poststack-editor/);
    assert.match(pipelineRust, /pub struct PostStackRecipe/);
    assert.match(pipelineRust, /pub source_data: Option<Vec<f32>>/);
    assert.match(poststackRust, /fn ds_poststack_recompute/);
    assert.match(poststackRust, /data_before_pcc/);
    assert.match(poststackRust, /PCC repetida no puede acumular ganancias/);
    assert.match(spccRust, /Es PCC, no SPCC/);
    assert.match(spccRust, /localIndex/);
    assert.match(spccRust, /gaiaOnline/);
    assert.match(spccRust, /fn spcc_pcc_block_reason/);
    assert.match(spccRust, /dualbandosc/);
    assert.match(spccRust, /fn spcc_channel_background/);
    assert.match(spccRust, /let mut patch = \[0\.0f32; SIDE \* SIDE\]/);
    assert.match(main, /function dsPccEligibilityForStackRequest/);
    assert.match(main, /function dsHooEligibilityForStackRequest/);
    assert.match(main, /pccEligible && hasWcs \? "" : "disabled"/);
    assert.match(deepskyRust, /fn ds_hoo_block_reason/);
    assert.match(deepskyRust, /HOO está bloqueado/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.pcc_gate_pending);
        assert.ok(locale.deepsky.pcc_gate_ready);
        assert.ok(locale.deepsky.pcc_gate_narrowband);
        assert.ok(locale.deepsky.pcc_gate_mono);
        assert.ok(locale.deepsky.hoo_gate_pending);
        assert.ok(locale.deepsky.hoo_gate_ready);
        assert.ok(locale.deepsky.hoo_gate_unavailable);
        assert.ok(locale.deepsky.hoo_gate_sii);
        assert.ok(locale.deepsky.hoo_gate_combined);
    }
});

test("guided editor keeps a fixed scientific workspace with explicit screen STF and crop actions", () => {
    assert.match(main, /function dsEnableFloatingDrag\(/);
    assert.match(main, /let dsStudioLayout = "studio"/);
    assert.doesNotMatch(main, /data-editor-action="layout-(?:docked|floating|studio)"/);
    assert.doesNotMatch(main, /data-editor-action="compare"/);
    assert.doesNotMatch(main, /dsEnableFloatingDrag\(editor/);
    assert.match(main, /function dsShowCropOverlay\(/);
    assert.match(main, /class="ds-crop-toolbar"/);
    assert.match(main, /data-crop-action="apply"/);
    assert.match(main, /data-editor-display-mode="linked"/);
    assert.match(main, /data-editor-display-mode="unlinked"/);
    assert.match(main, /data-editor-display-mode="linear"/);
    assert.match(main, /async function dsPoststackRefreshDisplayPreview/);
    assert.match(main, /invoke\("deepsky_restretch"/);
    assert.match(styles, /#ds-poststack-editor \.ds-editor-stf/);
    assert.match(styles, /#ds-crop-layer \.ds-crop-toolbar/);
    assert.equal(es.deepsky.stf_linked, "Vinculado");
    assert.equal(es.deepsky.stf_balanced, "Balanceado");
    assert.equal(en.deepsky.stf_linked, "Linked");
    assert.match(main, /function dsPoststackFixtureCommit\(/);
    assert.match(main, /function dsPoststackShowPreview\(/);
    assert.match(main, /deepsky_poststack_apply_crop/);
    assert.match(main, /deepsky_poststack_apply_dualband/);
    assert.match(main, /deepsky_poststack_apply_denoise/);
    assert.match(main, /deepsky_poststack_apply_stretch/);
    assert.match(main, /deepsky_poststack_apply_detail/);
    assert.match(main, /deepsky_poststack_apply_finish/);
    assert.doesNotMatch(main, /function dsOpenSharedPostprocess\(/);
    assert.match(html, /\.ds-poststack-workspace/);
    assert.match(html, /#ds-crop-layer/);
    assert.match(html, /\.ds-editor-apply-row/);
    assert.match(main, /data-editor-action="gradient-current"/);
    assert.match(main, /data-editor-action="gradient-model"/);
    assert.match(main, /data-editor-action="gradient-residual"/);
    assert.match(main, /aria-current="step"/);
    assert.match(pipelineRust, /pub struct PostStackCrop/);
    assert.match(pipelineRust, /pub struct PostStackSourceLayout/);
    for (const operation of [
        "Crop",
        "Gradient",
        "Astrometry",
        "DualBandPalette",
        "Denoise",
        "Stretch",
        "Detail",
        "Finish",
    ]) {
        assert.match(pipelineRust, new RegExp(`\\b${operation}\\s*\\{`));
    }
    assert.match(poststackRust, /crop_uses_one_exact_window_for_science_and_every_diagnostic_map/);
    assert.match(deepskyRust, /fn ds_apply_autocrop\(/);
    assert.match(deepskyRust, /test_ds_autocrop_disabled_is_an_exact_noop/);
    assert.match(poststackRust, /WCS por sí solo no debe clonar/);
    assert.match(poststackRust, /la transferencia debe conservar el orden tonal/);
});

test("native star layers keep parallel branch tools visible and recombine explicitly", () => {
    for (const action of [
        "layer-edit-object-restore",
        "layer-edit-object-denoise",
        "layer-edit-object-stretch",
        "layer-edit-object-detail",
        "layer-edit-object-finish",
        "layer-edit-stars-restore",
        "layer-edit-stars-denoise",
        "layer-edit-stars-stretch",
        "layer-edit-stars-detail",
        "layer-edit-stars-finish",
        "recombine-layers",
    ]) {
        assert.match(main, new RegExp(`data-editor-action="${action}"`));
    }
    assert.match(main, /function dsPoststackTargetHasStretch\(/);
    assert.match(main, /dsPoststackPreviewMode = explicitTarget \? `layer-\$\{explicitTarget\}` : "current"/);
    assert.match(main, /invoke\("deepsky_poststack_layer_preview", \{ target: explicitTarget \}\)/);
    assert.match(main, /target,\s*preset: String\(dsPoststackSettings\.stretchPreset\)/);
    assert.match(main, /deepsky_poststack_apply_finish", \{\s*target,/);
    assert.match(main, /data-editor-action="layer-return" data-editor-step="7" data-editor-return-layers="true"/);
    assert.match(main, /class="ds-studio-layer-home" data-editor-action="layer-return"/);
    assert.match(main, /if \(stepButton\.dataset\.editorReturnLayers === "true"\) \{\s*dsStudioLayerTarget = "combined"/);
    assert.match(main, /querySelectorAll\("\[data-editor-return-layers\]"\)\.forEach\(button => \{/);
    assert.match(main, /event\.stopPropagation\(\);\s*dsStudioLayerTarget = "combined";\s*dsPoststackStep = 7;/);
    assert.match(poststackRust, /fn ds_poststack_commit_operations\(/);
    assert.match(poststackRust, /fn ds_poststack_adaptive_analysis_for_data\(/);
    assert.match(poststackRust, /Crea las capas antes de estirar la rama Objeto/);
    assert.match(poststackRust, /Crea las capas antes de estirar la rama Estrellas/);
    assert.match(poststackRust, /No se puede mezclar una rama lineal con otra no lineal/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.editor_branch_stretch_body);
        assert.ok(locale.deepsky.editor_branch_finish_body);
        assert.ok(locale.deepsky.editor_domain_linear);
        assert.ok(locale.deepsky.editor_domain_processed);
        assert.ok(locale.deepsky.editor_refine_layers);
        assert.ok(locale.deepsky.editor_recalculate_layers);
    }
});

test("studio palettes require real channel masters and invalidate stale galleries", () => {
    const needsChannelMasters = main.match(
        /function dsStudioNeedsChannelMasters\([\s\S]*?\n}\n\nfunction dsOpenDeepSkyChannelCombiner/,
    )?.[0];
    assert.ok(needsChannelMasters, "dsStudioNeedsChannelMasters must remain identifiable");
    assert.match(needsChannelMasters, /source\.kind !== SOURCE_KINDS\.MONO_NARROWBAND/);
    assert.match(needsChannelMasters, /source\.independentComponentSources/);
    assert.match(needsChannelMasters, /\["HA", "OIII"\]/);
    assert.match(needsChannelMasters, /\["SII", "OIII"\]/);
    assert.match(needsChannelMasters, /rightSource !== leftSource/);
    assert.ok(
        (main.match(/dsStudioNeedsChannelMasters\(/g) || []).length >= 4,
        "the Quick action, label and palette panel must share the same mono-master gate",
    );

    const invalidation = main.match(
        /if \(\["dualBandProfile", "dualBandOiii", "dualBandCrosstalk"\]\.includes\(key\)\) \{[\s\S]*?\n            }/,
    )?.[0];
    assert.ok(invalidation, "palette-gallery invalidation must remain identifiable");
    assert.match(invalidation, /dsStudioPaletteGallery = null/);
    assert.match(invalidation, /dsStudioPaletteRequestSerial \+= 1/);
    assert.match(invalidation, /applyPalette\.disabled = true/);
    assert.match(invalidation, /paletteGallery\.dataset\.stale = "true"/);
});

test("editor session replacement is confirmed and ghost sessions are cleared", () => {
    const chooser = main.match(
        /async function dsChooseStandaloneMaster\(\)[\s\S]*?\n}\n\nfunction dsClearPoststackFrontendSession/,
    )?.[0];
    const clearSession = main.match(
        /function dsClearPoststackFrontendSession\(\)[\s\S]*?\n}\n\nasync function dsOpenDeepSkyEditorEntry/,
    )?.[0];
    const editorEntry = main.match(
        /async function dsOpenDeepSkyEditorEntry\(\)[\s\S]*?\n}\n\nasync function dsPoststackOpen/,
    )?.[0];
    const openSession = main.match(
        /async function dsPoststackOpen\(\)[\s\S]*?\n}\n\nfunction dsDismissPoststackWorkspace/,
    )?.[0];
    assert.ok(chooser && clearSession && editorEntry && openSession);

    assert.match(chooser, /if \(dsPoststackState && !dsIsPoststackFixture\(\)\)/);
    assert.match(chooser, /"deepsky\.editor_confirm_replace_session"/);
    assert.match(chooser, /if \(!confirmed\) return false/);
    assert.match(clearSession, /dsPoststackState = null/);
    assert.match(clearSession, /dsPoststackSourceDescriptor = null/);
    assert.match(clearSession, /dsStudioPaletteGallery = null/);
    assert.match(clearSession, /dsResultProducts = new Map\(\)/);
    assert.match(openSession, /dsClearPoststackFrontendSession\(\)/);
    assert.match(openSession, /dsDismissPoststackWorkspace\(\)/);
    assert.match(editorEntry, /const opened = await dsPoststackOpen\(\)/);
    assert.match(editorEntry, /if \(dsPoststackState\) return/);
    assert.match(editorEntry, /await dsChooseStandaloneMaster\(\)/);
});

test("applied palettes publish a derived product and preserve the immutable source product", () => {
    const applyPalette = main.match(
        /async function dsStudioApplyPalette\(button\)[\s\S]*?\n}\n\nasync function dsPoststackApplyDenoise/,
    )?.[0];
    assert.ok(applyPalette, "dsStudioApplyPalette must remain identifiable");
    assert.match(applyPalette, /product:\s*"studio_palette"/);
    assert.match(applyPalette, /result\.descriptor\?\.preservedProductId/);
    assert.match(applyPalette, /product:\s*"preserved_source"/);
    assert.match(applyPalette, /previewPath:\s*dsPoststackSourcePreview/);
    assert.match(applyPalette, /effectiveMethod:\s*"lossless_preserved_source"/);
    assert.match(applyPalette, /dsSetPrimaryProduct\(productId\)/);
    assert.match(applyPalette, /dsSyncResultProductOptions\(\)/);
    assert.match(main, /declared === "studio_palette"/);
    assert.match(main, /declared === "preserved_source"/);
    assert.match(main, /studio_palette:\s*"combinación lineal interpretativa derivada/);
    assert.match(main, /preserved_source:\s*"copia lossless del producto activo anterior/);
});

test("gradient protection preserves DQ and coverage exclusions and fails closed", () => {
    assert.match(poststackRust, /\*excluded \|= value\.is_finite\(\) && \*value > threshold/);
    assert.match(poststackRust, /!mask\[index\] && value\.is_finite\(\)/);
    assert.match(poststackRust, /gradient_mask_excludes_no_coverage_and_fatal_dq/);
    assert.match(poststackRust, /gradient_mask_fails_closed_for_incoherent_support_geometry/);
});

test("multiband sessions resume each scientific group product without preserving partial exports", () => {
    assert.match(deepskyRust, /fn ds_session_product_resume_fingerprint\(/);
    assert.match(deepskyRust, /multiband-scientific-group-v1/);
    assert.match(
        deepskyRust,
        /async fn run_deepsky_session\([\s\S]*?ds_try_restore_product_checkpoint\([\s\S]{0,180}&session_cache_base/,
    );
    assert.match(deepskyRust, /ds_mark_session_product_resume_recipe\(/);
    assert.match(deepskyRust, /group_output_guard\.preserve_child\(product_dir\.clone\(\)\)/);
    assert.match(deepskyRust, /output_guard\.preserve_child\(group_dir\.clone\(\)\)/);
    assert.match(deepskyRust, /resumed_from_checkpoint: cache_hit/);
});

test("stacking progress compares real outputs and hands the protected master to every guided module", () => {
    assert.match(html, /id="ds-prog-before-img"/);
    assert.match(html, /id="ds-prog-after-img"/);
    assert.match(html, /class="ds-progress-pane ds-progress-pane-before"/);
    assert.match(html, /class="ds-progress-pane ds-progress-pane-after"/);
    assert.match(html, /id="ds-prog-view-reset"/);
    assert.match(html, /id="ds-prog-viewport-status"[^>]+aria-live="polite"/);
    assert.match(main, /prog_compare_section_aria/);
    assert.match(main, /prog_compare_reference_pane_aria/);
    assert.match(main, /prog_compare_master_pane_aria/);
    assert.equal(es.deepsky.prog_reference_registered_label, "ANTES · referencia en el mismo encuadre");
    assert.equal(en.deepsky.prog_reference_registered_label, "BEFORE · reference in the same framing");
    assert.match(es.deepsky.prog_reference_alignment_unavailable, /referencia.+recorte del máster/);
    assert.match(en.deepsky.prog_reference_alignment_unavailable, /reference.+master crop/);
    assert.match(main, /function dsProgressResumeNote\(step\)/);
    assert.match(main, /restaurad\[oa\]\\s\+desde/);
    assert.match(main, /resumed:\s*Boolean\(current\.resumed \|\| resumeNote\)/);
    assert.match(main, /product\.resumedFromCheckpoint \|\| current\.resumed/);
    assert.match(deepskyRust, /"resumedFromCheckpoint": cache_hit/);
    assert.match(pipelineRust, /pub resumed_from_checkpoint: bool/);
    assert.equal(es.deepsky.prog_product_resumed_cache, "Reanudado desde caché compatible");
    assert.equal(en.deepsky.prog_product_resumed_cache, "Resumed from compatible cache");
    assert.match(html, /id="ds-prog-editor-handoff"/);
    assert.match(html, /id="ds-prog-edit-result"/);
    assert.match(main, /function dsProgressComplete\(/);
    assert.match(main, /function dsProgressBalancedMasterPreview\(/);
    assert.match(main, /mode:\s*"unlinked"/);
    assert.match(main, /ACTUAL · máster balanceado para comparar/);
    assert.match(main, /await dsProgressComplete\(result, progressRunToken\)/);
    assert.match(main, /let dsProgressRunToken = 0/);
    assert.match(main, /let dsProgressViewToken = 0/);
    assert.match(main, /const viewToken = \+\+dsProgressViewToken/);
    assert.match(main, /runToken === dsProgressRunToken\s*&& viewToken === dsProgressViewToken/);
    assert.match(main, /if \(requested === "master"\)/);
    assert.match(deepskyRust, /ds_poststack_preview_rgb16\([\s\S]+?result\.channels,[\s\S]+?None/);
    assert.match(deepskyRust, /DsScalarPreviewAggregation::Max/);
    assert.match(deepskyRust, /bits \|= dq\[y \* width \+ x\]/);
    assert.match(main, /function dsProgressOpenEditor\(step = 1\)/);
    assert.match(main, /if \(opened\) dsProgressStop\(\)/);
    assert.doesNotMatch(
        main.match(/async function dsProgressOpenEditor\(step = 1\) \{[\s\S]*?\n\}/)?.[0] || "",
        /\{\s*dsProgressStop\(\)/,
    );
    assert.match(styles, /grid-template-columns: repeat\(12, minmax\(0, 1fr\)\)/);
    assert.match(main, /invoke\("deepsky_frame_preview"/);
    assert.match(main, /invoke\("deepsky_result_view"/);
    assert.match(main, /invoke\("deepsky_poststack_source_preview"\)/);
    assert.match(main, /data-ds-progress-editor-step="\$\{step\.id\}"/);
    assert.match(main, /DS_POSTSTACK_STEPS\.map/);
    assert.match(main, /dsPoststackSourcePreview/);
    assert.match(main, /dsPoststackCurrentPreview/);
    assert.match(main, /let dsResultViewToken = 0/);
    assert.match(main, /function dsSetResultImage\(/);
    assert.match(main, /function dsProductDisclosure\(/);
    assert.match(main, /EIDR no validado · Classic conservado/);
    assert.match(html, /id="ds-result-toolbar"/);
    assert.match(html, /id="ds-result-editor"/);
    assert.match(main, /function dsProgressProductMarker\(/);
    assert.match(main, /function dsProgressEffectivePercent\(/);
    assert.match(deepskyRust, /"Producto \{\}\/\{\} · \{\}"/);
    assert.doesNotMatch(
        main,
        /dsProgressPhaseUpdate\(step,\s*!sessionMarker && pct >= 99\.5\)/,
        "una rama interna no puede declarar completo el proceso global",
    );
    assert.doesNotMatch(
        main,
        /dsProgressPhaseUpdate\(t\.phase \|\| "",\s*t\.phase === "complete"\)/,
        "la telemetría complete de un producto no puede cerrar las seis etapas",
    );
    assert.match(main, /dsProgressPhaseUpdate\("complete", true\)/);
    assert.match(html, /id="ds-prog-waiting-reference"/);
    assert.match(html, /class="ds-progress-wait-mark"/);
    assert.match(styles, /\.ds-progress-image\s*\{[\s\S]*?object-fit:\s*contain/);
    assert.match(styles, /\.ds-progress-compare\s*\{[\s\S]*?grid-template-columns:\s*repeat\(2,/);
    assert.match(styles, /left:\s*50%/);
    assert.match(styles, /--ds-progress-zoom/);
    assert.match(main, /function dsProgressBindViewport\(/);
    assert.match(main, /compare\.addEventListener\("wheel"/);
    assert.match(main, /compare\.addEventListener\("pointerdown"/);
    assert.match(main, /compare\.addEventListener\("dblclick"/);
    assert.match(main, /zoomDeepSkyProgressViewport/);
    assert.doesNotMatch(html, /id="ds-prog-compare"[^>]+type="range"/,
        "el comparador del progreso debe conservar dos paneles 50\/50, no un wipe desigual");
    assert.match(
        styles,
        /#ds-prog-steps \.ds-ph-mark \.zas-icon\s*\{[\s\S]*?margin:\s*0/,
        "los iconos de estado deben quedar centrados sin el margen global",
    );
    assert.match(deepskyRust, /EIDR CPU forward-model \+ PCG/);
    assert.match(deepskyRust, /EIDR · canal \{\}\/\{\}/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.prog_edit_title);
        assert.ok(locale.deepsky.prog_edit_hint);
        assert.ok(locale.deepsky.prog_source_safe);
        assert.ok(locale.deepsky.prog_ready_title);
        assert.ok(locale.deepsky.editor_compare_original);
        assert.ok(locale.deepsky.editor_compare_processed);
        assert.ok(locale.deepsky.prog_eta_calculating);
    }
});

test("astrometry is local-first, recoverable and only uses Gaia online explicitly", () => {
    assert.match(main, /allowOnline:\s*false/);
    assert.match(main, /allowOnline:\s*true/);
    assert.match(main, /ASTROMETRY_CATALOG_REQUIRED/);
    assert.match(spccRust, /allow_online:\s*Some\(false\)/);
    assert.match(spccRust, /ASTROMETRY_CATALOG_REQUIRED/);
    assert.match(spccRust, /ASTROMETRY_NETWORK/);
    assert.match(spccRust, /spcc_try_autosolve_result/);
    assert.match(spccRust, /WCS resuelto automáticamente con catálogo local/);
    assert.match(poststackRust, /!matches!\(&operation,\s*PostStackOperation::Astrometry/);
});

test("five-stage wizard blocks every transition on its visible requirement", () => {
    assert.match(html, /data-step="0"[\s\S]*?deepsky\.step_data/);
    assert.match(html, /data-step="1"[\s\S]*?deepsky\.step_calibrations/);
    assert.match(html, /data-step="2"[\s\S]*?deepsky\.step_quality/);
    assert.match(html, /data-step="3"[\s\S]*?deepsky\.step_method/);
    assert.match(html, /data-step="4"[\s\S]*?deepsky\.step_review/);
    assert.match(main, /function dsFirstIncompleteStep\(target\)/);
    assert.match(main, /function dsPlanCorrectionStep\(plan\)/);
    assert.match(main, /dsActiveLights\(\)\.length < 1\) return 0/);
    assert.match(main, /planInvalid && correctionStep <= 1\) return correctionStep/);
    assert.match(main, /!dsFrameInspection\?\.length\) return 2/);
    assert.match(main, /requested >= 4 && planInvalid\) return correctionStep/);
    assert.match(main, /dsWizardStep === 4/);
});

test("Essential is persistent, Expert is opt-in, and both share one request builder", () => {
    assert.match(html, /id="btn-ds-essential"/);
    assert.match(html, /id="btn-ds-expert"/);
    assert.match(html, /class="ds-preset ds-expert-only" data-preset="custom"/);
    assert.match(html, /id="ds-custom-essential-state"/);
    assert.match(main, /localStorage\.getItem\("zas_ds_experience"\)/);
    assert.match(main, /function dsSetExperienceMode\(mode/);
    assert.match(main, /localStorage\.setItem\("zas_ds_experience"/);
    assert.match(main, /customNotice\.hidden = !\(dsExperienceMode === "essential" && name === "custom"\)/);
    assert.equal((main.match(/function dsBuildStackRequest\(/g) || []).length, 1);
});

test("all deep-sky files remain searchable and reachable after item 120", () => {
    assert.match(main, /const DS_FILE_PAGE_SIZE = 120/);
    assert.match(main, /filtered\.slice\(first,\s*first \+ DS_FILE_PAGE_SIZE\)/);
    assert.match(main, /ds-file-list-search/);
    assert.match(main, /pageCount/);
    assert.doesNotMatch(main, /files\.slice\(0,\s*120\)/);
});

test("manual calibration never promotes an unsafe signature to compatible", () => {
    assert.match(deepskyRust, /fn ds_assess_manual_calibration_role/);
    assert.match(deepskyRust, /CalibrationAssignmentTier::ForcedUnsafe/);
    assert.match(deepskyRust, /scientific_eligible\s*=\s*false/);
    assert.match(deepskyRust, /fn ds_sanitize_manual_overrides_for_runtime/);
    assert.match(main, /forceUnsafe/);
    assert.match(main, /force_unsafe|forceUnsafe/);
    assert.match(main, /ds-apply-calibration-nights/);
});

test("cross-night flat reuse is gated by complete signatures and a backend stability fingerprint", () => {
    const tierFunction = main.match(
        /function dsBlockAssignmentTier\([\s\S]*?\n}\n\nfunction dsCalibrationChoice/,
    )?.[0];
    assert.ok(tierFunction);
    assert.doesNotMatch(tierFunction, /DistanceDays|<=\s*1/);
    assert.match(main, /function dsFlatReuseSignatureComplete/);
    assert.match(main, /block\.files\.length >= 3/);
    assert.match(pipelineRust, /pub struct CalibrationReuseEvidence/);
    assert.match(deepskyRust, /fn ds_measure_flat_reuse_evidence/);
    assert.match(deepskyRust, /fingerprint 32×24/);
    assert.match(deepskyRust, /maximum_profile_rms > 0\.015/);
    assert.match(deepskyRust, /maximum_profile_delta > 0\.08/);
    assert.match(deepskyRust, /reusable_candidate\.signature\.session = light\.signature\.session\.clone\(\)/);
    const comparator = deepskyRust.match(
        /fn ds_compare_probe_calibration\([\s\S]*?\n}\n\nfn ds_dark_core_compatible_for_scaling/,
    )?.[0];
    assert.ok(comparator, "calibration comparator must remain identifiable");
    assert.doesNotMatch(comparator, /ds_night_distance/);
    assert.match(comparator, /requiere reutilización explícita validada/);
    const runtimeFlatSelection = deepskyRust.match(
        /let flat_for_light = \|path: &str\|[\s\S]*?\n    };\n\n    \/\/ Inicialización única/,
    )?.[0];
    assert.ok(runtimeFlatSelection, "runtime flat selection must remain identifiable");
    assert.doesNotMatch(runtimeFlatSelection, /ds_night_distance|noche adyacente/i);
    assert.match(runtimeFlatSelection, /reutilización explícita validada/);
});

test("deep-sky dialog hides and inerts the background while open", () => {
    assert.match(main, /function dsSetBackgroundInert\(\s*active,\s*activeDialog/);
    assert.match(main, /child\.inert\s*=\s*true/);
    assert.match(main, /child\.setAttribute\("aria-hidden",\s*"true"\)/);
    assert.match(main, /dsTrapDialogFocus\(e,\s*modal\)/);
});

test("nested channel-combination dialog becomes interactive and restores the wizard", () => {
    assert.match(main, /if \(child === activeDialog\)/);
    assert.match(main, /child\.inert\s*=\s*false/);
    assert.match(main, /child\.removeAttribute\("aria-hidden"\)/);
    assert.match(main, /dsSetBackgroundInert\(true,\s*modal\)/);
    assert.match(main, /dsSetBackgroundInert\(true,\s*wizard\)/);
    assert.match(html, /id="ds-combine-modal"[\s\S]*?aria-hidden="true"/);
    assert.doesNotMatch(html, /id="ds-combine-modal"[^>]*z-index:\s*906/);
});

test("science picker exposes only linear FITS and TIFF inputs", () => {
    assert.ok(picker, "dsPick must remain identifiable");
    assert.match(picker, /"fits",\s*"fit",\s*"fts",\s*"tif",\s*"tiff"/);
    assert.doesNotMatch(picker, /"png"|"jpg"|"jpeg"/);
    const combinePicker = main.match(
        /function dsRenderCombineSlots\(\)[\s\S]*?\n}\n\n\(function initDeepSkyCombine/,
    )?.[0];
    assert.ok(combinePicker, "channel-combination picker must remain identifiable");
    assert.match(combinePicker, /"fits",\s*"fit",\s*"fts",\s*"tif",\s*"tiff"/);
    assert.doesNotMatch(combinePicker, /"png"|"jpg"|"jpeg"/);
    const combineCommand = deepskyRust.match(
        /async fn deepsky_combine_channels\([\s\S]*?\n}\n\n\/\/\/ Technical metadata/,
    )?.[0];
    assert.ok(combineCommand, "channel-combination backend must remain identifiable");
    assert.match(combineCommand, /ds_require_mono_channel_master/);
    assert.doesNotMatch(combineCommand, /ds_debayer_image|ds_luma/);
    assert.match(combineCommand, /gradient\.unwrap_or\(false\)/);
    assert.match(combineCommand, /"linearChannelCombination"/);
    assert.match(combineCommand, /"combinationMode": combination_mode/);
    assert.match(combineCommand, /lanczos3_with_bilinear_border/);
    assert.match(main, /gradient:\s*false/);
    assert.match(main, /combinationMode:\s*presetKey/);
    assert.doesNotMatch(main, /combine_run">Combinar y estirar/);
});

test("session folders are actions with a hierarchy separate from frame groups", () => {
    assert.match(html, /id="ds-session-actions" class="ds-session-actions"/);
    assert.match(html, /class="ds-frame-groups-head"/);
    assert.match(main, /id = "btn-ds-scan-folder"/);
    assert.match(main, /className = "ds-session-action ds-session-action-import"/);
    assert.match(main, /id = "btn-ds-work-folder"/);
    assert.match(main, /className = "ds-session-action ds-session-action-output"/);
    for (const locale of [en, es, fr, it]) {
        assert.ok(locale.deepsky.session_folders_aria);
        assert.ok(locale.deepsky.source_action);
        assert.ok(locale.deepsky.destination_action);
        assert.ok(locale.deepsky.frame_groups_title);
        assert.ok(locale.deepsky.frame_groups_hint);
    }
});

test("N.I.N.A. FlatWizard dark-flats use FITS evidence before generic filenames", () => {
    assert.match(deepskyRust, /IMAGETYP/);
    assert.match(deepskyRust, /OBJECT/);
    assert.match(deepskyRust, /flat_wizard/);
    assert.match(deepskyRust, /ds_classify_probe\(&pr\)/);
    assert.match(deepskyRust, /fn ds_calibration_role_from_object/);
    assert.match(
        deepskyRust,
        /Some\("LIGHT"\),\s*Some\("DARK 600SEG"\)/,
        "a contradictory LIGHT header needs matching OBJECT and path evidence",
    );
    assert.match(main, /frameType \|\| probe\.frame_type/);
    // Los dark-flats recuperados por evidencia FITS entran en SU categoría al
    // fusionar el escaneo (antes se asignaba directamente a `dsFiles`).
    assert.match(main, /darkFlats: classifiedDarkFlats,/);
    assert.match(main, /flats: classifiedFlats,/);
    assert.match(html, /id="ds-auto-classify-report"/);
});

test("wizard states, guide dock, recipe impact and compact review stay visible", () => {
    assert.match(html, /class="ds-step-state"/);
    assert.match(main, /function dsWizardStepState\(step\)/);
    assert.match(html, /id="ds-guide-dock" class="ds-guide-dock"/);
    assert.match(main, /getElementById\("ds-guide-dock"\)/);
    assert.doesNotMatch(html, /#ds-guide \{ position:absolute/);
    assert.match(html, /id="ds-recipe-impact"/);
    assert.match(main, /function dsRenderRecipeImpact\(plan\)/);
    assert.match(main, /function dsFormatReviewPlan\(plan\)/);
    assert.match(main, /class="ds-review-technical"/);
    for (const locale of [en, es, fr, it]) {
        assert.ok(locale.deepsky.state_pending);
        assert.ok(locale.deepsky.recipe_impact_title);
        assert.ok(locale.deepsky.review_technical);
    }
});

test("folder and clear actions meet the centered touch-target contract", () => {
    assert.match(main, /className = "ds-kind-action"/);
    assert.match(html, /\.ds-kind-action \{[\s\S]*?width:42px; height:42px/);
    assert.match(html, /\.ds-session-action-icon \.zas-icon \{[\s\S]*?margin:0 !important/);
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
    assert.match(main, /function dsCollectEligibility\(plan\)/);
    assert.match(main, /scientificEligibilityReasons/);
    assert.match(main, /ds-method-eligibility/);
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
    assert.match(
        html,
        /<b data-i18n="deepsky\.preset_balanced">Equilibrado<\/b>\s*<span data-i18n="deepsky\.profile_balanced_note">[^<]*Winsorized/,
    );
    assert.match(en.deepsky.profile_balanced_note, /Winsorized/);
    assert.match(es.deepsky.profile_balanced_note, /Winsorized/);
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
    assert.match(requestBuilder, /calibrationOverrides:\s*dsBuildCalibrationOverrides\(/);
    assert.match(main, /sel-ds-manual-darks/);
    assert.match(main, /sel-ds-manual-flats/);
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

// El ligado manual pasó de una fila por NOCHE (en un desplegable del paso de
// inspección) a una fila por GRUPO —noche × filtro × exposición— en la tabla de
// calibración del paso de datos: una noche con Ha y OIII ya no comparte una
// única elección de flats.
test("manual calibration linking works per light group with skippable batches", () => {
    assert.match(main, /function dsCalibrationRows\(lights\)/);
    assert.match(main, /function dsCalibrationBlocks\(kind\)/);
    assert.match(main, /function dsCalibrationBlockIdentity\(kind, file\)/);
    assert.match(main, /function dsCalibrationBlockHash\(value\)/);
    assert.match(main, /`\$\{night\}\|\$\{filter\}\|\$\{expKey\}`/);
    assert.match(main, /const relevantFlats = flatChoice/);
    assert.match(main, /data-ds-link/);
    assert.match(main, /data-ds-batch/);
    assert.match(main, /skipFlats/);
    assert.match(main, /skipDarks/);
    assert.match(main, /function dsBuildCalibrationOverrides\(/);
    // Las rutas del override se resuelven contra los bloques vigentes, no contra
    // un índice del último plan preparado.
    assert.match(main, /dsCalibrationBlocks\(kind\)\.map\(block => \[block\.id, block\]\)/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.linker_auto);
        assert.ok(locale.deepsky.linker_skip_flats);
        assert.ok(locale.deepsky.tier_candidate);
        assert.ok(locale.deepsky.wbpp_hint);
        assert.ok(locale.deepsky.blocks_compatible);
    }
});

test("date grouping rules expand a year into per-day sessions across paths and headers", () => {
    assert.match(main, /function dsFileGroupingText\(file\)/);
    assert.match(main, /function dsFileDateTags\(file\)/);
    assert.match(main, /function dsKeywordLooksLikeDatePrefix\(keyword\)/);
    assert.match(main, /dates\.filter\(date => date\.startsWith\(prefix\)\)/);
    assert.match(main, /function dsAutomaticSessionIdentity\(file\)/);
    assert.match(main, /function dsSessionIdentity\(file\) \{[\s\S]{0,500}groups\.find\(group => \/\^\(\?:19\|20\)/);
    assert.match(main, /dsFileGroups\(f\)\.includes\(dsSelectedGroup\)/);
    assert.match(main, /Una noche seleccionada filtra los lights/);
    assert.match(html, /Una regla temporal como «2026» crea automáticamente un grupo por día/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.keywords_help);
        assert.ok(locale.deepsky.keywords_no_match);
    }
});

test("dark-flats are presented as one session batch while Rust keeps strict submasters", () => {
    assert.match(main, /if \(kind === "darkFlats"\) \{[\s\S]{0,220}const sessions = new Map\(\)/);
    assert.match(main, /function dsDarkFlatSessionLabel\(files\)/);
    assert.match(main, /dsExactExposureMatch\(existing, value\)/);
    assert.match(main, /function dsFmtCalibrationExposure\(value\)/);
    assert.match(deepskyRust, /El desplegable presenta un lote dark-flat por sesión/);
    assert.match(deepskyRust, /test_dark_flat_session_batch_covers_multiple_flat_exposures_without_mixing_them/);
    assert.match(deepskyRust, /ds_group_darks_by_exposure\([\s\S]{0,180}CalibrationRole::DarkFlat/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.dark_flat_session_exposures);
        assert.ok(locale.deepsky.exposure_unverified);
    }
});

test("frame viewer overlays the deep-sky modal", () => {
    const viewer = main.match(/ds-frame-viewer[\s\S]{0,400}?z-index:(\d+)/);
    assert.ok(viewer, "viewer overlay style must be identifiable");
    assert.ok(Number(viewer[1]) > 10000,
        "el visor debe quedar por ENCIMA del modal (modal-overlay usa z-index 10000)");
});

test("interactive guide points the user at every blocker", () => {
    assert.match(main, /function dsRenderGuide\(plan\)/);
    assert.match(main, /function dsSpotlight\(target\)/);
    assert.match(main, /ds-guide-action/);
    assert.match(main, /dsRenderGuide\(plan\);/);
    assert.match(main, /dsRenderGuide\(dsPreparedPlan\)/);
    assert.match(main, /guide_recommendations/);
    assert.match(main, /Seleccionar o descartar lights/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.guide_title);
        assert.ok(locale.deepsky.guide_ready);
        assert.ok(locale.deepsky.guide_fix_linker);
        assert.ok(locale.deepsky.guide_strict);
        assert.ok(locale.deepsky.guide_recommendations);
    }
});

test("scanning another folder adds to the session instead of replacing it", () => {
    // Una sesión real vive en varias carpetas (una por noche, o lights y
    // calibración aparte): escanear la segunda no puede borrar la primera.
    assert.match(main, /function dsMergeScannedFiles\(scanned\)/);
    assert.match(main, /mergeClassifiedDeepSkyFrames\(dsFiles, scanned\)/,
        "debe deduplicar y reclasificar por ruta con el probe más reciente");
    assert.match(main, /const added = dsMergeScannedFiles\(\{/);
    assert.match(main, /added\.reclassified/,
        "debe informar cuando una cabecera corregida mueve la toma a otro grupo");
    // El escaneo ya no reinicia el ligado ni los descartes del usuario.
    assert.doesNotMatch(main, /dsFiles\.lights = \[\.\.\.\(cl\.lights \|\| \[\]\)\]/);
    const scan = main.match(/async function dsScanFolder\(\)[\s\S]*?\n}\n/)?.[0] || "";
    assert.ok(scan, "dsScanFolder debe seguir siendo identificable");
    assert.doesNotMatch(scan, /dsCalibAssignments\.clear\(\)/);
    assert.doesNotMatch(scan, /dsDiscardedPaths\.clear\(\)/);
    for (const locale of [en, es, fr, it]) {
        assert.ok(locale.deepsky.scan_added);
        assert.ok(locale.deepsky.scan_reclassified);
        assert.ok(locale.deepsky.scan_total);
        assert.ok(locale.deepsky.scan_accumulates);
    }
});

test("all four calibration batch kinds are selectable and reach the backend", () => {
    // Los cuatro roles se agrupan, se pueden excluir y ahora también ligar:
    // bias y dark-flat ya no son chips decorativos.
    assert.match(main, /kind: "darkFlats"[\s\S]{0,140}linkable: true/);
    assert.match(main, /kind: "bias"[\s\S]{0,140}linkable: true/);
    assert.match(main, /kind: "flats"[\s\S]{0,140}linkable: true/);
    assert.match(main, /kind: "darks"[\s\S]{0,140}linkable: true/);
    assert.match(main, /darkFlats:\s*\[\]/);
    assert.match(main, /bias:\s*\[\]/);
    assert.match(main, /skipDarkFlats:\s*false/);
    assert.match(main, /skipBias:\s*false/);
    assert.match(pipelineRust, /pub dark_flats: Vec<String>/);
    assert.match(pipelineRust, /pub bias: Vec<String>/);
    assert.match(pipelineRust, /pub skip_dark_flats: bool/);
    assert.match(pipelineRust, /pub skip_bias: bool/);
    assert.match(pipelineRust, /pub dark_flats: String/);
    assert.match(pipelineRust, /pub bias: String/);
    assert.match(pipelineRust, /pub calibration_state: String/);
    assert.match(deepskyRust, /override_dark_flat_masters/);
    assert.match(deepskyRust, /override_bias_masters/);
    assert.match(main, /e\.darkFlats/);
    assert.match(main, /e\.calibrationState/);
});

test("next-night flats can be user-verified without waiving known mismatches", () => {
    assert.match(main, /function dsFlatReuseAttestationStatus/);
    assert.match(main, /data-ds-scientific-attestation/);
    assert.match(main, /userVerifiedScientific/);
    assert.match(main, /userVerificationReason/);
    assert.match(pipelineRust, /UserVerified/);
    assert.match(pipelineRust, /pub user_verified_scientific: bool/);
    assert.match(deepskyRust, /fn ds_flat_reuse_signature_issues/);
    assert.match(deepskyRust, /requires? confirmación explícita|requiere confirmación explícita/);
    assert.match(deepskyRust, /CalibrationAssignmentTier::UserVerified/);
    assert.match(deepskyRust, /CalibrationAssignmentTier::ForcedUnsafe/);
    assert.match(deepskyRust, /fn ds_calibration_requires_classic/);
    assert.match(deepskyRust, /decision\.degraded \|\| !decision\.scientific_eligible/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.verify_scientific_title);
        assert.ok(locale.deepsky.verify_scientific_body);
        assert.ok(locale.deepsky.tier_user_verified);
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

test("deep-sky failures stay dismissible and return to the owning correction step", () => {
    assert.match(main, /const customModalQueue = \[\]/);
    assert.match(main, /function customModalDrainQueue\(\)/);
    assert.match(main, /dsSetBackgroundInert\(true, overlay\)/);
    assert.match(main, /overlay\.addEventListener\("keydown", onKeyDown\)/);
    assert.match(main, /customModalActive[\s\S]{0,500}activeDialog = customOverlay/);
    assert.match(main, /function dsPresentRunError\(modal, error\)/);
    assert.match(main, /await showCustomAlert\(presentation\.title, presentation\.message\)/);
    assert.match(main, /await dsPresentRunError\(\s*modal,/);
    assert.match(main, /await dsPresentRunError\(modal, e\)/);
    assert.match(main, /"error-modal"/);
    assert.match(html, /id="custom-modal-overlay"[\s\S]{0,180}role="dialog"[\s\S]{0,180}aria-modal="true"/);
    assert.match(deepskyRust, /fn ds_infer_shifted_flat_white_level/);
    assert.match(deepskyRust, /test_ds_flat_linearity_infers_left_shifted_14_bit_fits_codes/);
    assert.match(deepskyRust, /test_ds_flat_linearity_still_fails_closed_without_quantization_evidence/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.flat_validation_title);
        assert.ok(locale.deepsky.flat_validation_missing_white);
        assert.ok(locale.deepsky.flat_validation_action);
        assert.ok(locale.deepsky.calibration_error_action);
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

test("every literal deepsky translation callsite resolves in Spanish and English", () => {
    // This is intentionally extracted from the callsites instead of maintaining
    // another hand-written allow-list. The Studio grew by hundreds of strings
    // while the previous test covered only four, so missing keys silently fell
    // back to Spanish in the English UI.
    const callsiteKeys = new Set(
        [...main.matchAll(/\btr\(\s*["'`]deepsky\.([^"'`$]+)["'`]/g)]
            .map(match => match[1]),
    );
    assert.ok(callsiteKeys.size > 700, "the extractor must see the complete Deep Sky Studio surface");
    for (const [language, locale] of [["es", es], ["en", en]]) {
        const missing = [...callsiteKeys]
            .filter(key => locale.deepsky?.[key] === undefined)
            .sort();
        assert.deepEqual(
            missing,
            [],
            `${language}.json is missing ${missing.length} deepsky callsite key(s): ${missing.join(", ")}`,
        );
    }
});

test("every deep-sky mutation invalidates stale diagnostics before recomputing", () => {
    assert.match(main, /let dsSessionRevision = 0/);
    assert.match(main, /function dsInvalidatePreparedPlan\(\)/);
    assert.match(main, /dsSessionRevision \+= 1/);
    assert.match(main, /dsPreparedPlan = null/);
    assert.match(main, /revision !== dsSessionRevision/);
    assert.match(main, /function dsSchedulePreflight\(immediate = false, invalidate = true\)/);
    assert.match(main, /function dsUpdateUI\(\) \{[\s\S]{0,420}dsSchedulePreflight\(false\)/);
    assert.match(main, /dsSchedulePreflight\(true\);[\s\S]{0,120}dsRenderSessionOrganizer\(\)/,
        "manual linking must invalidate the old plan before repainting its row");
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.guide_refreshing);
        assert.ok(locale.deepsky.guide_refreshing_detail);
    }
});

test("source and destination use an explicit navigable folder browser", () => {
    assert.match(main, /function dsChooseDirectory\(\{/);
    assert.match(main, /Un clic abre una carpeta/);
    assert.match(main, /data-ds-folder-entry/);
    assert.match(main, /button\.addEventListener\("click", \(\) => load\(button\.dataset\.dsFolderEntry\)\)/);
    assert.match(main, /useButton\.addEventListener\("click", \(\) => finish\(listing\?\.current \|\| null\)\)/);
    assert.match(main, /purpose: "source"[\s\S]{0,180}recursive: true/);
    assert.match(main, /purpose: "destination"[\s\S]{0,180}recursive: true/);
    assert.match(html, /#ds-folder-picker/);
    assert.match(deepskyRust, /fn deepsky_browse_directories\(/);
    assert.match(deepskyRust, /fn ds_list_directories\(/);
    assert.match(deepskyRust, /std::fs::read_dir\(&current\)/);
    assert.match(main, /id="ds-folder-picker-new"/);
    assert.match(main, /data-ds-folder-rename/);
    assert.match(main, /invoke\("deepsky_create_directory"/);
    assert.match(main, /invoke\("deepsky_rename_directory"/);
    assert.match(main, /canCreateDirectories: true/);
    assert.match(deepskyRust, /fn deepsky_create_directory\(/);
    assert.match(deepskyRust, /fn deepsky_rename_directory\(/);
    assert.match(commandsCoreRust, /deepsky_browse_directories,/);
    assert.match(commandsCoreRust, /deepsky_create_directory,/);
    assert.match(commandsCoreRust, /deepsky_rename_directory,/);
    for (const locale of [en, es]) {
        assert.ok(locale.deepsky.folder_instruction_source);
        assert.ok(locale.deepsky.folder_instruction_destination);
        assert.ok(locale.deepsky.folder_use_source);
        assert.ok(locale.deepsky.folder_use_destination);
        assert.ok(locale.deepsky.folder_new);
        assert.ok(locale.deepsky.folder_rename);
    }
});
