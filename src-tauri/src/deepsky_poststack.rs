// Editor lineal no destructivo de cielo profundo.
//
// Este archivo se incluye después de `deepsky.rs`: reutiliza su render STF y
// publica en `stacked_image`, pero nunca usa ese espejo u16 como fuente. Cada
// revisión se recalcula desde `DeepSkyResult::source_data`.

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostStackNormalizedRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostStackGradientRequest {
    #[serde(default)]
    mode: PostStackBackgroundMode,
    #[serde(default = "poststack_true")]
    protect_extended_objects: bool,
    #[serde(default = "poststack_true")]
    chromatic: bool,
    /// Sensibilidad de la protección automática, 0..1. Valores altos
    /// protegen más señal extensa y usan menos muestras de fondo.
    #[serde(default = "poststack_default_sensitivity")]
    sensitivity: f32,
    /// Grado polinómico 1..=4. Ausente conserva el grado 2 histórico.
    #[serde(default)]
    degree: Option<usize>,
    /// Muestras circulares normalizadas usadas sólo en modo `samples`.
    #[serde(default)]
    samples: Vec<PostStackBackgroundSample>,
    /// Un ajuste inestable se bloquea de forma predeterminada. El modo
    /// experto puede conservarlo únicamente con esta concesión explícita.
    #[serde(default)]
    allow_unstable: bool,
    /// Compatibilidad de lectura con recetas/UI anteriores. Estas regiones
    /// siguen siendo exclusiones, nunca se reinterpretan como muestras.
    #[serde(default)]
    exclusion_rects: Vec<PostStackNormalizedRect>,
}

fn poststack_true() -> bool {
    true
}

fn poststack_default_sensitivity() -> f32 {
    0.55
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostStackCropRequest {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostStackDenoiseRequest {
    #[serde(default)]
    target: PostStackLayerTarget,
    #[serde(default = "poststack_half")]
    strength: f32,
    #[serde(default = "poststack_default_detail_protection")]
    detail_protection: f32,
    #[serde(default = "poststack_default_chroma_strength")]
    chroma_strength: f32,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostStackDualBandRequest {
    #[serde(default = "poststack_default_palette")]
    palette: String,
    #[serde(default = "poststack_default_instrument_profile")]
    profile: String,
    #[serde(default = "poststack_default_oiii_weight")]
    oiii_green_weight: f32,
    #[serde(default)]
    crosstalk_suppression: f32,
    #[serde(default = "poststack_true")]
    neutralize: bool,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostStackStretchRequest {
    #[serde(default)]
    target: PostStackLayerTarget,
    #[serde(default = "poststack_default_preset")]
    preset: String,
    #[serde(default)]
    stretch: Option<f32>,
    #[serde(default)]
    symmetry: Option<f32>,
    #[serde(default)]
    local_intensity: Option<f32>,
    #[serde(default)]
    black_point: Option<f32>,
    #[serde(default)]
    white_point: Option<f32>,
    #[serde(default = "poststack_true")]
    linked: bool,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostStackCurvesRequest {
    #[serde(default)]
    target: PostStackLayerTarget,
    #[serde(flatten)]
    curves: PostStackCurves,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostStackDetailRequest {
    #[serde(default)]
    target: PostStackLayerTarget,
    #[serde(default = "poststack_default_detail_amount")]
    amount: f32,
    #[serde(default = "poststack_default_detail_radius")]
    radius: usize,
    #[serde(default = "poststack_default_star_protection")]
    star_protection: f32,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostStackFinishRequest {
    #[serde(default)]
    target: PostStackLayerTarget,
    #[serde(default)]
    saturation: f32,
    #[serde(default)]
    contrast: f32,
    #[serde(default = "poststack_default_highlight_protection")]
    highlight_protection: f32,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostStackDeconvolutionRequest {
    #[serde(default)]
    target: PostStackLayerTarget,
    #[serde(default)]
    psf: Option<PostStackPsfModel>,
    /// Concesión experta explícita cuando no fue posible medir una PSF fiable.
    /// La receta conserva `measured=false`; nunca se presenta como medición.
    #[serde(default)]
    manual_confirmed: bool,
    #[serde(default = "poststack_default_deconv_iterations")]
    iterations: usize,
    #[serde(default = "poststack_default_deconv_regularization")]
    regularization: f32,
    #[serde(default = "poststack_default_deconv_deringing")]
    deringing: f32,
    #[serde(default = "poststack_true")]
    flux_conservation: bool,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostStackStarSeparationRequest {
    #[serde(default = "poststack_default_star_engine")]
    engine: String,
    #[serde(default = "poststack_default_star_sensitivity")]
    sensitivity: f32,
    #[serde(default = "poststack_default_star_scale")]
    scale: f32,
    #[serde(default = "poststack_default_halo_protection")]
    halo_protection: f32,
    #[serde(default = "poststack_default_faint_star_protection")]
    faint_star_protection: f32,
    #[serde(default = "poststack_true")]
    preserve_residual: bool,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostStackStarAdjustmentRequest {
    #[serde(default = "poststack_default_star_reduction")]
    reduction: f32,
    #[serde(default)]
    saturation: f32,
    #[serde(default = "poststack_default_halo_suppression")]
    halo_suppression: f32,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostStackRecombineRequest {
    #[serde(default = "poststack_one")]
    object_weight: f32,
    #[serde(default = "poststack_one")]
    star_weight: f32,
    #[serde(default = "poststack_one")]
    residual_weight: f32,
}

fn poststack_half() -> f32 {
    0.5
}

fn poststack_default_detail_protection() -> f32 {
    0.65
}

fn poststack_default_chroma_strength() -> f32 {
    0.35
}

fn poststack_default_preset() -> String {
    "autoNatural".to_string()
}

fn poststack_default_palette() -> String {
    "hooNatural".to_string()
}

fn poststack_default_instrument_profile() -> String {
    "metadata".to_string()
}

fn poststack_default_oiii_weight() -> f32 {
    0.65
}

fn poststack_default_detail_amount() -> f32 {
    0.25
}

fn poststack_default_detail_radius() -> usize {
    1
}

fn poststack_default_star_protection() -> f32 {
    0.75
}

fn poststack_default_highlight_protection() -> f32 {
    0.8
}

fn poststack_one() -> f32 {
    1.0
}

fn poststack_default_deconv_iterations() -> usize {
    8
}

fn poststack_default_deconv_regularization() -> f32 {
    0.08
}

fn poststack_default_deconv_deringing() -> f32 {
    0.72
}

fn poststack_default_star_engine() -> String {
    "nativePsfMultiscale".to_string()
}

fn poststack_default_star_sensitivity() -> f32 {
    0.58
}

fn poststack_default_star_scale() -> f32 {
    1.0
}

fn poststack_default_halo_protection() -> f32 {
    0.78
}

fn poststack_default_faint_star_protection() -> f32 {
    0.62
}

fn poststack_default_star_reduction() -> f32 {
    0.18
}

fn poststack_default_halo_suppression() -> f32 {
    0.25
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PostStackGradientResult {
    state: PostStackRevisionState,
    model_preview: String,
    residual_preview: String,
    mode: PostStackBackgroundMode,
    sample_count: usize,
    degree: usize,
    protected_percent: f32,
    split_half_ratio: f32,
    overfit_warning: Option<String>,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostStackLoadMasterRequest {
    path: String,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PostStackLoadMasterResult {
    source: PostStackSourceDescriptor,
    state: PostStackRevisionState,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PostStackAdaptiveAnalysis {
    black_point: f32,
    background: f32,
    noise_sigma: f32,
    white_point: f32,
    star_fraction: f32,
    suggested_stretch: PostStackStretch,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PostStackPsfAnalysis {
    psf: PostStackPsfModel,
    eligible: bool,
    reason: String,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PostStackLayerPreviewResult {
    target: String,
    preview: String,
    mask_preview: Option<String>,
    reconstruction_error: f32,
    star_fraction: f32,
}

fn ds_poststack_operation_name(operation: &PostStackOperation) -> &'static str {
    match operation {
        PostStackOperation::Crop { .. } => "crop",
        PostStackOperation::Gradient { .. } => "gradient",
        PostStackOperation::Astrometry { .. } => "astrometry",
        PostStackOperation::GaiaPcc { .. } => "gaiaPcc",
        PostStackOperation::DualBandPalette { .. } => "dualBandPalette",
        PostStackOperation::Deconvolution { .. } => "deconvolution",
        PostStackOperation::Denoise { .. } => "denoise",
        PostStackOperation::StarSeparation { .. } => "starSeparation",
        PostStackOperation::Stretch { .. } => "stretch",
        PostStackOperation::Curves { .. } => "curves",
        PostStackOperation::Detail { .. } => "detail",
        PostStackOperation::StarAdjustment { .. } => "starAdjustment",
        PostStackOperation::Recombine { .. } => "recombine",
        PostStackOperation::Finish { .. } => "finish",
    }
}

fn ds_poststack_operation_target(operation: &PostStackOperation) -> PostStackLayerTarget {
    match operation {
        PostStackOperation::Deconvolution { target, .. }
        | PostStackOperation::Denoise { target, .. }
        | PostStackOperation::Stretch { target, .. }
        | PostStackOperation::Curves { target, .. }
        | PostStackOperation::Detail { target, .. }
        | PostStackOperation::Finish { target, .. } => *target,
        _ => PostStackLayerTarget::Combined,
    }
}

fn ds_poststack_operation_slot(operation: &PostStackOperation) -> String {
    match operation {
        PostStackOperation::Deconvolution { target, .. }
        | PostStackOperation::Denoise { target, .. }
        | PostStackOperation::Stretch { target, .. }
        | PostStackOperation::Curves { target, .. }
        | PostStackOperation::Detail { target, .. }
        | PostStackOperation::Finish { target, .. } => {
            format!(
                "{}:{}",
                ds_poststack_operation_name(operation),
                target.as_str()
            )
        }
        _ => ds_poststack_operation_name(operation).to_string(),
    }
}

fn ds_poststack_ensure_source(result: &mut DeepSkyResult) {
    if result.source_data.is_none() {
        result.source_data = Some(result.data.clone());
        result.source_variance = result.variance.clone();
        result.source_layout = Some(PostStackSourceLayout {
            width: result.width,
            height: result.height,
            coverage: result.coverage.clone(),
            weight: result.weight.clone(),
            rejection_low: result.rejection_low.clone(),
            rejection_high: result.rejection_high.clone(),
            registration_residuals: result.registration_residuals.clone(),
            neff: result.neff.clone(),
            dq: result.dq.clone(),
            struct_map: result.struct_map.clone(),
            struct_residual: result.struct_residual.clone(),
            recoverability: result.recoverability.clone(),
        });
    }
}

fn ds_poststack_operation_rank(operation: &PostStackOperation) -> usize {
    match operation {
        PostStackOperation::Crop { .. } => 0,
        PostStackOperation::Gradient { .. } => 10,
        PostStackOperation::Astrometry { .. } => 20,
        PostStackOperation::GaiaPcc { .. } => 30,
        PostStackOperation::DualBandPalette { .. } => 35,
        PostStackOperation::Deconvolution { target, .. } => match target {
            PostStackLayerTarget::Combined => 40,
            PostStackLayerTarget::Object | PostStackLayerTarget::Stars => 60,
        },
        PostStackOperation::Denoise { target, .. } => match target {
            PostStackLayerTarget::Combined => 45,
            PostStackLayerTarget::Object | PostStackLayerTarget::Stars => 62,
        },
        PostStackOperation::StarSeparation { .. } => 55,
        PostStackOperation::Stretch { target, .. } => match target {
            PostStackLayerTarget::Combined => 70,
            PostStackLayerTarget::Object | PostStackLayerTarget::Stars => 64,
        },
        PostStackOperation::Curves { target, .. } => match target {
            PostStackLayerTarget::Combined => 75,
            PostStackLayerTarget::Object | PostStackLayerTarget::Stars => 65,
        },
        PostStackOperation::Detail { target, .. } => match target {
            PostStackLayerTarget::Combined => 80,
            PostStackLayerTarget::Object | PostStackLayerTarget::Stars => 66,
        },
        PostStackOperation::StarAdjustment { .. } => 67,
        PostStackOperation::Recombine { .. } => 69,
        PostStackOperation::Finish { target, .. } => match target {
            PostStackLayerTarget::Combined => 100,
            PostStackLayerTarget::Object | PostStackLayerTarget::Stars => 68,
        },
    }
}

/// Reemplaza una operación singleton sin acumularla. Si el usuario había
/// deshecho revisiones, la nueva decisión descarta únicamente la rama de redo.
fn ds_poststack_set_operation(result: &mut DeepSkyResult, operation: PostStackOperation) {
    // Astrometría sólo añade/valida WCS: no toca píxeles. Evitamos clonar aquí
    // SCI/VAR/NEFF/DQ completos (un máster de 60 MP puede ocupar varios GB).
    // La primera operación que sí altere píxeles tomará entonces la instantánea
    // inmutable del máster.
    if !matches!(&operation, PostStackOperation::Astrometry { .. }) {
        ds_poststack_ensure_source(result);
    }
    result
        .post_stack_recipe
        .operations
        .truncate(result.post_stack_recipe.cursor);
    let kind = ds_poststack_operation_name(&operation);
    let slot = ds_poststack_operation_slot(&operation);
    // Cambiar geometría invalida cualquier modelo o solución medidos sobre la
    // ventana anterior. Mantenerlos sería cómodo visualmente pero
    // científicamente falso, así que se exige recalcularlos.
    if kind == "crop" {
        result.post_stack_recipe.operations.retain(|current| {
            matches!(
                current,
                // El recorte nuevo reemplaza este slot más abajo.
                PostStackOperation::Crop { .. }
                    // La mezcla espectral depende de metadata, no de la ventana.
                    | PostStackOperation::DualBandPalette { .. }
                    // Estas herramientas se vuelven a ejecutar desde la fuente
                    // recortada y vuelven a medir sus estadísticas locales.
                    | PostStackOperation::Denoise { .. }
                    | PostStackOperation::StarSeparation { .. }
                    | PostStackOperation::Stretch { .. }
                    | PostStackOperation::Curves { .. }
                    | PostStackOperation::Detail { .. }
                    | PostStackOperation::StarAdjustment { .. }
                    | PostStackOperation::Recombine { .. }
                    | PostStackOperation::Finish { .. }
            )
        });
    }
    if let Some(index) = result
        .post_stack_recipe
        .operations
        .iter()
        .position(|current| ds_poststack_operation_slot(current) == slot)
    {
        result.post_stack_recipe.operations[index] = operation;
    } else {
        result.post_stack_recipe.operations.push(operation);
    }
    result
        .post_stack_recipe
        .operations
        .sort_by_key(ds_poststack_operation_rank);
    result.post_stack_recipe.cursor = result.post_stack_recipe.operations.len();
}

/// Actualiza la receta como una transacción. Reproducir puede fallar por una
/// geometría, dominio o dependencia inválidos; en ese caso se restaura la
/// receta anterior y se vuelve a publicar el último estado válido. Esto evita
/// que un botón fallido deje al editor encerrado en una rama rota.
fn ds_poststack_commit_operations(
    result: &mut DeepSkyResult,
    operations: impl IntoIterator<Item = PostStackOperation>,
) -> Result<(), String> {
    let previous_recipe = result.post_stack_recipe.clone();
    for operation in operations {
        ds_poststack_set_operation(result, operation);
    }
    if let Err(error) = ds_poststack_recompute(result) {
        result.post_stack_recipe = previous_recipe;
        if let Err(restore_error) = ds_poststack_recompute(result) {
            return Err(format!(
                "{error}. Además no se pudo restaurar la revisión anterior: {restore_error}"
            ));
        }
        return Err(error);
    }
    Ok(())
}

fn ds_poststack_set_cursor_transactional(
    result: &mut DeepSkyResult,
    cursor: usize,
) -> Result<(), String> {
    let previous = result.post_stack_recipe.cursor;
    result.post_stack_recipe.cursor = cursor.min(result.post_stack_recipe.operations.len());
    if let Err(error) = ds_poststack_recompute(result) {
        result.post_stack_recipe.cursor = previous;
        if let Err(restore_error) = ds_poststack_recompute(result) {
            return Err(format!(
                "{error}. Además no se pudo restaurar la revisión anterior: {restore_error}"
            ));
        }
        return Err(error);
    }
    Ok(())
}

fn ds_poststack_model_from_contract(
    model: &PostStackBackgroundModel,
) -> crate::deepsky_background::BgModel {
    crate::deepsky_background::BgModel {
        degree: model.degree,
        coeffs: model.coeffs.clone(),
        level: model.level.clone(),
        w: model.width,
        h: model.height,
        ch: model.channels,
    }
}

fn ds_poststack_sync_recipe(result: &mut DeepSkyResult) -> Result<(), String> {
    let object = result
        .recipe
        .as_object_mut()
        .ok_or("La receta del máster no es un objeto JSON")?;
    object.insert(
        "postStackRecipe".into(),
        serde_json::to_value(&result.post_stack_recipe).map_err(|error| error.to_string())?,
    );
    match &result.astrometry_solution {
        Some(solution) => {
            object.insert(
                "wcs".into(),
                serde_json::json!({
                    "ctype": solution.ctype,
                    "crval1": solution.crval1,
                    "crval2": solution.crval2,
                    "crpix1": solution.crpix1,
                    "crpix2": solution.crpix2,
                    "cd11": solution.cd11,
                    "cd12": solution.cd12,
                    "cd21": solution.cd21,
                    "cd22": solution.cd22,
                    "rmsPx": solution.rms_px,
                    "inliers": solution.inliers,
                    "handedness": solution.handedness,
                    "scaleArcsecPx": solution.scale_arcsec_px,
                    "source": solution.source,
                    "cached": solution.cached,
                    "postStackDerived": true,
                }),
            );
        }
        None => {
            let remove_derived = object
                .get("wcs")
                .and_then(|value| value.get("postStackDerived"))
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if remove_derived {
                object.remove("wcs");
            }
        }
    }
    Ok(())
}

fn ds_poststack_crop_interleaved<T: Copy>(
    data: &[T],
    source_width: usize,
    source_height: usize,
    channels: usize,
    crop: &PostStackCrop,
) -> Result<Vec<T>, String> {
    if channels == 0
        || data.len()
            != source_width
                .saturating_mul(source_height)
                .saturating_mul(channels)
        || crop.x.saturating_add(crop.width) > source_width
        || crop.y.saturating_add(crop.height) > source_height
    {
        return Err("El recorte no coincide con la geometría de la imagen".into());
    }
    let mut out = Vec::with_capacity(
        crop.width
            .saturating_mul(crop.height)
            .saturating_mul(channels),
    );
    for y in crop.y..crop.y + crop.height {
        let start = (y * source_width + crop.x) * channels;
        let end = start + crop.width * channels;
        out.extend_from_slice(&data[start..end]);
    }
    Ok(out)
}

fn ds_poststack_crop_plane<T: Copy>(
    data: &[T],
    source_width: usize,
    source_height: usize,
    crop: &PostStackCrop,
) -> Result<Vec<T>, String> {
    ds_poststack_crop_interleaved(data, source_width, source_height, 1, crop)
}

fn ds_poststack_apply_crop(result: &mut DeepSkyResult, crop: &PostStackCrop) -> Result<(), String> {
    if crop.source_width != result.width || crop.source_height != result.height {
        return Err("El recorte fue definido sobre otra geometría del máster".into());
    }
    result.data = ds_poststack_crop_interleaved(
        &result.data,
        result.width,
        result.height,
        result.channels,
        crop,
    )?;
    if let Some(variance) = result.variance.take() {
        result.variance = Some(ds_poststack_crop_interleaved(
            &variance,
            result.width,
            result.height,
            result.channels,
            crop,
        )?);
    }
    if let Some(neff) = result.neff.take() {
        result.neff = Some(ds_poststack_crop_interleaved(
            &neff,
            result.width,
            result.height,
            result.channels,
            crop,
        )?);
    }
    if let Some(dq) = result.dq.take() {
        result.dq = Some(ds_poststack_crop_plane(
            &dq,
            result.width,
            result.height,
            crop,
        )?);
    }
    result.coverage = ds_poststack_crop_plane(&result.coverage, result.width, result.height, crop)?;
    result.weight = ds_poststack_crop_plane(&result.weight, result.width, result.height, crop)?;
    result.rejection_low =
        ds_poststack_crop_plane(&result.rejection_low, result.width, result.height, crop)?;
    result.rejection_high =
        ds_poststack_crop_plane(&result.rejection_high, result.width, result.height, crop)?;
    if !result.registration_residuals.is_empty() {
        result.registration_residuals = ds_poststack_crop_plane(
            &result.registration_residuals,
            result.width,
            result.height,
            crop,
        )?;
    }
    if let Some(map) = result.struct_map.take() {
        result.struct_map = Some(ds_poststack_crop_plane(
            &map,
            result.width,
            result.height,
            crop,
        )?);
    }
    if let Some(map) = result.struct_residual.take() {
        result.struct_residual = Some(ds_poststack_crop_plane(
            &map,
            result.width,
            result.height,
            crop,
        )?);
    }
    if let Some(map) = result.recoverability.take() {
        result.recoverability = Some(ds_poststack_crop_plane(
            &map,
            result.width,
            result.height,
            crop,
        )?);
    }
    result.width = crop.width;
    result.height = crop.height;
    Ok(())
}

fn ds_poststack_percentiles(data: &[f32], channels: usize) -> (f32, f32, f32, f32, f32) {
    let pixels = data.len() / channels.max(1);
    let step = (pixels / 400_000).max(1);
    let mut sample = Vec::with_capacity((pixels / step).saturating_add(1));
    for pixel in (0..pixels).step_by(step) {
        let count = channels.min(3).max(1);
        let mut sum = 0.0f32;
        let mut valid = 0usize;
        for channel in 0..count {
            let value = data[pixel * channels + channel];
            if value.is_finite() {
                sum += value;
                valid += 1;
            }
        }
        if valid > 0 {
            sample.push(sum / valid as f32);
        }
    }
    if sample.is_empty() {
        return (0.0, 0.0, 1.0, 1.0, 0.0);
    }
    sample.sort_by(|a, b| a.total_cmp(b));
    let at = |fraction: f32| -> f32 {
        let index = ((sample.len() - 1) as f32 * fraction.clamp(0.0, 1.0)).round() as usize;
        sample[index]
    };
    let background = at(0.5);
    let mut deviations = sample
        .iter()
        .map(|value| (value - background).abs())
        .collect::<Vec<_>>();
    deviations.sort_by(|a, b| a.total_cmp(b));
    let noise = deviations[deviations.len() / 2] * 1.4826;
    let star_threshold = background + noise.max(1e-6) * 6.0;
    let star_fraction = sample
        .iter()
        .filter(|value| **value > star_threshold)
        .count() as f32
        / sample.len() as f32;
    (
        at(0.001),
        background,
        noise.max(1e-6),
        at(0.9995).max(background + noise.max(1e-6) * 8.0),
        star_fraction,
    )
}

fn ds_poststack_adaptive_analysis_for_data(
    data: &[f32],
    channels: usize,
) -> PostStackAdaptiveAnalysis {
    let (raw_black, background, noise, white, star_fraction) =
        ds_poststack_percentiles(data, channels);
    let black = (background - noise * 2.8).max(raw_black);
    let range = (white - black).max(noise * 16.0).max(1e-6);
    let normalized_background = ((background - black) / range).clamp(0.005, 0.35);
    let stretch = (0.16 / normalized_background).clamp(1.2, 18.0);
    PostStackAdaptiveAnalysis {
        black_point: black,
        background,
        noise_sigma: noise,
        white_point: white,
        star_fraction,
        suggested_stretch: PostStackStretch {
            preset: "autoNatural".into(),
            stretch,
            symmetry: normalized_background.clamp(0.03, 0.35),
            local_intensity: if star_fraction > 0.08 { 0.25 } else { 0.38 },
            black_point: black,
            white_point: white,
            linked: true,
        },
    }
}

fn ds_poststack_adaptive_analysis_for(result: &DeepSkyResult) -> PostStackAdaptiveAnalysis {
    ds_poststack_adaptive_analysis_for_data(&result.data, result.channels)
}

fn ds_poststack_apply_stretch_preset(stretch: &mut PostStackStretch) {
    match stretch.preset.as_str() {
        "faintNebula" => {
            stretch.stretch = (stretch.stretch * 1.35).min(64.0);
            stretch.local_intensity = 0.52;
        }
        "galaxy" => {
            stretch.stretch = (stretch.stretch * 0.92).max(0.001);
            stretch.local_intensity = 0.32;
        }
        "starField" => {
            stretch.stretch = (stretch.stretch * 0.72).max(0.001);
            stretch.local_intensity = 0.18;
        }
        "narrowband" => {
            stretch.stretch = (stretch.stretch * 1.18).min(64.0);
            stretch.local_intensity = 0.46;
        }
        _ => {}
    }
}

fn ds_poststack_apply_denoise(
    data: &mut Vec<f32>,
    width: usize,
    height: usize,
    channels: usize,
    strength: f32,
    detail_protection: f32,
    chroma_strength: f32,
) {
    if width < 2 || height < 2 || channels == 0 || strength <= 0.0 {
        return;
    }
    let source = data.clone();
    let (_, _, noise, _, _) = ds_poststack_percentiles(&source, channels);
    let range_sigma = noise * (1.4 + detail_protection.clamp(0.0, 1.0) * 7.0);
    let strength = strength.clamp(0.0, 1.0);
    let chroma = chroma_strength.clamp(0.0, 1.0);
    data.par_chunks_mut(channels)
        .enumerate()
        .for_each(|(pixel, target)| {
            let x = pixel % width;
            let y = pixel / width;
            let neighbors = [
                pixel,
                y * width + x.saturating_sub(1),
                y * width + (x + 1).min(width - 1),
                y.saturating_sub(1) * width + x,
                (y + 1).min(height - 1) * width + x,
            ];
            for channel in 0..channels {
                let center = source[pixel * channels + channel];
                if !center.is_finite() {
                    target[channel] = center;
                    continue;
                }
                let mut sum = 0.0f32;
                let mut weights = 0.0f32;
                for neighbor in neighbors {
                    let value = source[neighbor * channels + channel];
                    if !value.is_finite() {
                        continue;
                    }
                    let normalized = (value - center) / range_sigma.max(1e-6);
                    let weight = 1.0 / (1.0 + normalized * normalized);
                    sum += value * weight;
                    weights += weight;
                }
                let filtered = if weights > 0.0 { sum / weights } else { center };
                let channel_strength = if channels >= 3 && channel > 0 {
                    strength * (0.65 + 0.35 * chroma)
                } else {
                    strength
                };
                target[channel] = center + (filtered - center) * channel_strength;
            }
        });
}

fn ds_poststack_ghs_value(value: f32, stretch: &PostStackStretch) -> f32 {
    if !value.is_finite() {
        return value;
    }
    let low = stretch.black_point;
    let high = stretch.white_point.max(low + 1e-6);
    let t_raw = (value - low) / (high - low);
    let t = t_raw.clamp(0.0, 1.0);
    let symmetry = stretch.symmetry.clamp(0.005, 0.995);
    let li = stretch.local_intensity.clamp(0.0, 1.0);
    let d = stretch.stretch.clamp(0.001, 64.0);
    // Datos por debajo del punto negro: no colapsarlos todos a cero. Aunque
    // el punto negro sea una decisión de presentación, el replay del editor
    // continúa en float32 y herramientas posteriores (curvas, color, A/B)
    // necesitan distinguir esos valores. Se prolonga la curva con pendiente
    // positiva y continuidad C0 en t=0; el recorte pertenece únicamente al
    // render de presentación/exportación entera.
    if t_raw < 0.0 {
        let asinh_d = d.asinh();
        let slope_h = d / asinh_d;
        let slope_l = 1.0 / (1.0 + li * 2.5);
        let slope = ((1.0 - li) * slope_h + li * slope_l) * 0.5 / symmetry;
        return t_raw * slope * 65535.0;
    }
    // Altas luces por encima del punto blanco: extensión lineal con la
    // pendiente de la curva en t=1. El estirado se mantiene estrictamente
    // monótono, así que los núcleos estelares conservan su separación (y su
    // color en modo unlinked) en lugar de aplastarse a un blanco plano
    // idéntico en los tres canales. El recorte a rango de presentación ocurre
    // solo al publicar la vista (los casts a u16 saturan por sí mismos).
    if t_raw > 1.0 {
        let asinh_d = d.asinh();
        let slope_h = d / (asinh_d * (1.0 + d * d).sqrt());
        let slope_l = 1.0 / (1.0 + li * 2.5);
        let slope = ((1.0 - li) * slope_h + li * slope_l) * 0.5 / (1.0 - symmetry);
        return (1.0 + slope * (t_raw - 1.0)) * 65535.0;
    }
    let pivoted = if t <= symmetry {
        0.5 * t / symmetry
    } else {
        0.5 + 0.5 * (t - symmetry) / (1.0 - symmetry)
    };
    let hyperbolic = (d * pivoted).asinh() / d.asinh();
    let local = pivoted.powf(1.0 / (1.0 + li * 2.5));
    let mixed = hyperbolic * (1.0 - li) + local * li;
    mixed * 65535.0
}

fn ds_poststack_apply_stretch(data: &mut [f32], channels: usize, stretch: &PostStackStretch) {
    if channels == 0 {
        return;
    }
    if stretch.linked || channels < 3 {
        data.par_iter_mut()
            .for_each(|value| *value = ds_poststack_ghs_value(*value, stretch));
        return;
    }
    let mut bounds = vec![(stretch.black_point, stretch.white_point); channels];
    for channel in 0..channels.min(3) {
        let channel_data = data
            .iter()
            .skip(channel)
            .step_by(channels)
            .copied()
            .collect::<Vec<_>>();
        let (black, background, noise, white, _) = ds_poststack_percentiles(&channel_data, 1);
        bounds[channel] = ((background - noise * 2.8).max(black), white);
    }
    // Un PostStackStretch por canal, construido UNA vez: clonarlo dentro del
    // bucle por píxel (posee un String) costaba ~72 M asignaciones en un
    // máster de 24 MP. Solo cambian los puntos negro/blanco por canal.
    let channel_stretches: Vec<PostStackStretch> = bounds
        .iter()
        .map(|&(black, white)| {
            let mut local = stretch.clone();
            local.black_point = black;
            local.white_point = white;
            local
        })
        .collect();
    data.par_chunks_mut(channels).for_each(|pixel| {
        for (channel, value) in pixel.iter_mut().enumerate().take(channels) {
            *value = ds_poststack_ghs_value(*value, &channel_stretches[channel]);
        }
    });
}

const POSTSTACK_CURVE_SCALE: f32 = 65_535.0;

fn ds_poststack_curve_is_identity(points: &[PostStackCurvePoint]) -> bool {
    points.is_empty()
        || points
            .iter()
            .all(|PostStackCurvePoint(x, y)| (*x - *y).abs() <= 1e-7)
}

fn ds_poststack_curves_are_identity(curves: &PostStackCurves) -> bool {
    ds_poststack_curve_is_identity(&curves.master)
        && ds_poststack_curve_is_identity(&curves.luminance)
        && ds_poststack_curve_is_identity(&curves.red)
        && ds_poststack_curve_is_identity(&curves.green)
        && ds_poststack_curve_is_identity(&curves.blue)
        && ds_poststack_curve_is_identity(&curves.saturation)
        && curves.hue_shift.abs() <= 1e-7
        && (curves.saturation_scale - 1.0).abs() <= 1e-7
        && curves.lightness.abs() <= 1e-7
        && curves.selective.iter().all(|adjustment| {
            adjustment.hue_shift.abs() <= 1e-7
                && adjustment.saturation.abs() <= 1e-7
                && adjustment.lightness.abs() <= 1e-7
        })
}

fn ds_poststack_validate_curve(label: &str, points: &[PostStackCurvePoint]) -> Result<(), String> {
    if points.is_empty() {
        return Ok(());
    }
    if points.len() < 2 {
        return Err(format!("La curva {label} necesita al menos dos puntos"));
    }
    for (index, PostStackCurvePoint(x, y)) in points.iter().enumerate() {
        if !x.is_finite() || !y.is_finite() || !(0.0..=1.0).contains(x) || !(0.0..=1.0).contains(y)
        {
            return Err(format!(
                "La curva {label} contiene un punto fuera de 0..1 en la posición {}",
                index + 1
            ));
        }
        if index > 0 && *x <= points[index - 1].0 {
            return Err(format!(
                "Los puntos de la curva {label} deben tener X estrictamente creciente"
            ));
        }
    }
    if points
        .first()
        .map(|point| point.0.abs() > 1e-6)
        .unwrap_or(false)
        || points
            .last()
            .map(|point| (point.0 - 1.0).abs() > 1e-6)
            .unwrap_or(false)
    {
        return Err(format!(
            "La curva {label} debe cubrir el dominio completo con X=0 y X=1"
        ));
    }
    Ok(())
}

fn ds_poststack_normalized_hue_name(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "red" | "rojo" => Some("red"),
        "orange" | "naranja" => Some("orange"),
        "yellow" | "amarillo" => Some("yellow"),
        "green" | "verde" => Some("green"),
        "cyan" | "cian" => Some("cyan"),
        "blue" | "azul" => Some("blue"),
        "magenta" => Some("magenta"),
        "violet" | "violeta" => Some("violet"),
        _ => None,
    }
}

fn ds_poststack_validate_curves(curves: &PostStackCurves, channels: usize) -> Result<(), String> {
    for (label, points) in [
        ("K", curves.master.as_slice()),
        ("luminancia", curves.luminance.as_slice()),
        ("rojo", curves.red.as_slice()),
        ("verde", curves.green.as_slice()),
        ("azul", curves.blue.as_slice()),
        ("saturación", curves.saturation.as_slice()),
    ] {
        ds_poststack_validate_curve(label, points)?;
    }
    if !curves.hue_shift.is_finite()
        || !(-180.0..=180.0).contains(&curves.hue_shift)
        || !curves.saturation_scale.is_finite()
        || !(0.0..=3.0).contains(&curves.saturation_scale)
        || !curves.lightness.is_finite()
        || !(-1.0..=1.0).contains(&curves.lightness)
    {
        return Err(
            "Los ajustes globales requieren hueShift -180..180°, saturationScale 0..3 y lightness -1..1"
                .into(),
        );
    }
    if curves.selective.len() > 8 {
        return Err("Sólo existen ocho rangos HSL selectivos".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for adjustment in &curves.selective {
        let normalized_name = ds_poststack_normalized_hue_name(&adjustment.name).ok_or_else(|| {
            format!(
                "Rango HSL desconocido '{}'; usa red, orange, yellow, green, cyan, blue, magenta o violet",
                adjustment.name
            )
        })?;
        if !seen.insert(normalized_name) {
            return Err(format!(
                "El rango HSL {normalized_name} aparece más de una vez"
            ));
        }
        if !adjustment.center.is_finite()
            || !(0.0..360.0).contains(&adjustment.center)
            || !adjustment.width.is_finite()
            || !(1.0..=180.0).contains(&adjustment.width)
            || !adjustment.hue_shift.is_finite()
            || !(-180.0..=180.0).contains(&adjustment.hue_shift)
            || !adjustment.saturation.is_finite()
            || !(-1.0..=2.0).contains(&adjustment.saturation)
            || !adjustment.lightness.is_finite()
            || !(-1.0..=1.0).contains(&adjustment.lightness)
        {
            return Err(format!(
                "El rango HSL {} tiene límites inválidos",
                adjustment.name
            ));
        }
    }
    if channels < 3
        && (!ds_poststack_curve_is_identity(&curves.red)
            || !ds_poststack_curve_is_identity(&curves.green)
            || !ds_poststack_curve_is_identity(&curves.blue)
            || !ds_poststack_curve_is_identity(&curves.saturation)
            || curves.hue_shift.abs() > 1e-7
            || (curves.saturation_scale - 1.0).abs() > 1e-7
            || curves.lightness.abs() > 1e-7
            || curves.selective.iter().any(|adjustment| {
                adjustment.hue_shift.abs() > 1e-7
                    || adjustment.saturation.abs() > 1e-7
                    || adjustment.lightness.abs() > 1e-7
            }))
    {
        return Err(
            "Un máster mono sólo admite las curvas K y L; RGB, saturación y HSL requieren tres canales"
                .into(),
        );
    }
    Ok(())
}

/// Interpolación lineal con extrapolación del primer/último segmento. No
/// recorta píxeles fuera de 0..1: la receta conserva valores firmados y altas
/// luces por encima del blanco nominal.
fn ds_poststack_curve_value(value: f32, points: &[PostStackCurvePoint]) -> f32 {
    if !value.is_finite() || ds_poststack_curve_is_identity(points) {
        return value;
    }
    let segment = if value <= points[0].0 {
        0
    } else if value >= points[points.len() - 1].0 {
        points.len() - 2
    } else {
        match points.binary_search_by(|point| point.0.total_cmp(&value)) {
            Ok(index) => return points[index].1,
            Err(index) => index.saturating_sub(1).min(points.len() - 2),
        }
    };
    let PostStackCurvePoint(x0, y0) = points[segment];
    let PostStackCurvePoint(x1, y1) = points[segment + 1];
    y0 + (value - x0) * (y1 - y0) / (x1 - x0)
}

fn ds_poststack_apply_curve_scalar(value: f32, points: &[PostStackCurvePoint]) -> f32 {
    if !value.is_finite() || ds_poststack_curve_is_identity(points) {
        value
    } else {
        ds_poststack_curve_value(value / POSTSTACK_CURVE_SCALE, points) * POSTSTACK_CURVE_SCALE
    }
}

fn ds_poststack_rgb_hue_degrees(r: f32, g: f32, b: f32) -> f32 {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    if !delta.is_finite() || delta.abs() <= 1e-12 {
        return 0.0;
    }
    let raw = if max == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    raw.rem_euclid(360.0)
}

fn ds_poststack_hue_weight(hue: f32, center: f32, width: f32) -> f32 {
    let distance = (hue - center + 180.0).rem_euclid(360.0) - 180.0;
    let normalized = (distance.abs() / width.max(1e-6)).clamp(0.0, 1.0);
    // Borde coseno: evita bandas duras entre los ocho rangos.
    0.5 + 0.5 * (std::f32::consts::PI * normalized).cos()
}

fn ds_poststack_adjust_luma(luma: f32, amount: f32) -> f32 {
    if amount >= 0.0 {
        luma + (1.0 - luma) * amount
    } else {
        luma * (1.0 + amount)
    }
}

fn ds_poststack_apply_curves(
    data: &mut [f32],
    channels: usize,
    curves: &PostStackCurves,
) -> Result<(), String> {
    ds_poststack_validate_curves(curves, channels)?;
    if channels == 0 || ds_poststack_curves_are_identity(curves) {
        return Ok(());
    }
    data.par_chunks_mut(channels).for_each(|pixel| {
        for value in pixel.iter_mut() {
            *value = ds_poststack_apply_curve_scalar(*value, &curves.master);
        }
        if channels < 3 {
            pixel[0] = ds_poststack_apply_curve_scalar(pixel[0], &curves.luminance);
            return;
        }
        pixel[0] = ds_poststack_apply_curve_scalar(pixel[0], &curves.red);
        pixel[1] = ds_poststack_apply_curve_scalar(pixel[1], &curves.green);
        pixel[2] = ds_poststack_apply_curve_scalar(pixel[2], &curves.blue);
        if !pixel[0].is_finite() || !pixel[1].is_finite() || !pixel[2].is_finite() {
            // Ninguna mezcla propaga el NaN de un canal a los otros. El valor
            // no finito original ya fue preservado por las curvas escalares.
            return;
        }

        let mut r = pixel[0] / POSTSTACK_CURVE_SCALE;
        let mut g = pixel[1] / POSTSTACK_CURVE_SCALE;
        let mut b = pixel[2] / POSTSTACK_CURVE_SCALE;
        let luma_before = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        if !ds_poststack_curve_is_identity(&curves.luminance) {
            // La curva L reescala el píxel completo: (r,g,b) *= L(Y)/Y. Un
            // offset aditivo (el comportamiento anterior) colapsa los ratios
            // R:G:B de las sombras — levantar +0.05 sobre (0.02,0.01,0.15)
            // convierte un ratio 15:1 en ~3:1 — desaturando el color físico
            // y lavando las altas luces. El escalado multiplicativo conserva
            // la cromaticidad exacta en todo el rango tonal.
            //
            // Para Y <= eps el píxel se deja intacto: la cromaticidad de un
            // píxel (casi) negro o de Y negativa (los datos firmados de la
            // calibración se conservan) no está definida, y el cociente
            // L(Y)/Y sólo amplificaría ruido de lectura puro.
            const POSTSTACK_LUMA_EPS: f32 = 1e-6;
            if luma_before > POSTSTACK_LUMA_EPS {
                let mapped = ds_poststack_curve_value(luma_before, &curves.luminance);
                let scale = mapped / luma_before.max(POSTSTACK_LUMA_EPS);
                r *= scale;
                g *= scale;
                b *= scale;
            }
        }

        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let hue = ds_poststack_rgb_hue_degrees(r, g, b);

        // Cb/Cr Rec.709: rotar/escalar este plano conserva exactamente Y
        // antes del ajuste de luminosidad explícito y evita clamp RGB oculto.
        let cb = (b - luma) / (2.0 * (1.0 - 0.0722));
        let cr = (r - luma) / (2.0 * (1.0 - 0.2126));
        // La curva de saturación se mide y se aplica en el MISMO modelo:
        // croma YCbCr c = sqrt(cb²+cr²) y proxy s = c/max(|Y|,eps) acotado a
        // [0,1]. El comportamiento anterior medía s en HSV ((max−min)/max,
        // sin sentido con RGB negativos por el max.abs()) y aplicaba el
        // cociente sobre el croma YCbCr: mezclar dos definiciones de
        // saturación produce un refuerzo salvajemente no uniforme por tono.
        // El factor se acota a [0,4] para que un píxel casi negro (|Y|
        // minúsculo con croma residual de ruido) no explote el croma.
        const POSTSTACK_SAT_EPS: f32 = 1e-6;
        let saturation_curve_scale = if !ds_poststack_curve_is_identity(&curves.saturation) {
            let chroma = (cb * cb + cr * cr).sqrt();
            let s = (chroma / luma.abs().max(POSTSTACK_SAT_EPS)).clamp(0.0, 1.0);
            if s > 1e-7 {
                let mapped = ds_poststack_curve_value(s, &curves.saturation);
                (mapped / s).clamp(0.0, 4.0)
            } else {
                // Un píxel acromático no tiene saturación que remapear.
                1.0
            }
        } else {
            1.0
        };
        let mut hue_shift = curves.hue_shift;
        let mut saturation_scale = curves.saturation_scale * saturation_curve_scale;
        let mut lightness = curves.lightness;
        for adjustment in &curves.selective {
            let weight = ds_poststack_hue_weight(hue, adjustment.center, adjustment.width);
            hue_shift += adjustment.hue_shift * weight;
            saturation_scale *= 1.0 + adjustment.saturation * weight;
            lightness += adjustment.lightness * weight;
        }
        let angle = hue_shift.to_radians();
        let cos = angle.cos();
        let sin = angle.sin();
        let cb_rotated = (cb * cos - cr * sin) * saturation_scale;
        let cr_rotated = (cb * sin + cr * cos) * saturation_scale;
        let target_luma = ds_poststack_adjust_luma(luma, lightness);
        r = target_luma + 2.0 * (1.0 - 0.2126) * cr_rotated;
        b = target_luma + 2.0 * (1.0 - 0.0722) * cb_rotated;
        g = (target_luma - 0.2126 * r - 0.0722 * b) / 0.7152;
        pixel[0] = r * POSTSTACK_CURVE_SCALE;
        pixel[1] = g * POSTSTACK_CURVE_SCALE;
        pixel[2] = b * POSTSTACK_CURVE_SCALE;
    });
    Ok(())
}

fn ds_poststack_validate_layer_geometry(
    workspace: &crate::deepsky_studio_layers::StarLayerWorkspace,
    width: usize,
    height: usize,
    channels: usize,
) -> Result<(), String> {
    let samples = width.saturating_mul(height).saturating_mul(channels);
    let pixels = width.saturating_mul(height);
    if workspace.width != width
        || workspace.height != height
        || workspace.channels != channels
        || workspace.object.len() != samples
        || workspace.stars.len() != samples
        || workspace.residual.len() != samples
        || workspace.mask.len() != pixels
    {
        return Err(
            "Objeto, Estrellas y residual no comparten la geometría global activa; la revisión se descartó"
                .into(),
        );
    }
    Ok(())
}

fn ds_poststack_apply_detail(
    data: &mut Vec<f32>,
    width: usize,
    height: usize,
    channels: usize,
    amount: f32,
    radius: usize,
    star_protection: f32,
) {
    if width < 2 || height < 2 || channels == 0 || amount <= 0.0 {
        return;
    }
    let source = data.clone();
    let (_, background, noise, white, _) = ds_poststack_percentiles(&source, channels);
    let radius = radius.clamp(1, 3);
    let star_start =
        background + (white - background) * (0.55 - 0.35 * star_protection.clamp(0.0, 1.0));
    data.par_chunks_mut(channels)
        .enumerate()
        .for_each(|(pixel, target)| {
            let x = pixel % width;
            let y = pixel / width;
            let mut luma = 0.0f32;
            for channel in 0..channels.min(3) {
                luma += source[pixel * channels + channel];
            }
            luma /= channels.min(3).max(1) as f32;
            let star_guard = if luma > star_start {
                (1.0 - star_protection.clamp(0.0, 1.0)).max(0.08)
            } else {
                1.0
            };
            for channel in 0..channels {
                let center = source[pixel * channels + channel];
                if !center.is_finite() {
                    target[channel] = center;
                    continue;
                }
                let mut sum = 0.0f32;
                let mut count = 0.0f32;
                for delta in 1..=radius {
                    for (nx, ny) in [
                        (x.saturating_sub(delta), y),
                        ((x + delta).min(width - 1), y),
                        (x, y.saturating_sub(delta)),
                        (x, (y + delta).min(height - 1)),
                    ] {
                        let value = source[(ny * width + nx) * channels + channel];
                        if value.is_finite() {
                            sum += value;
                            count += 1.0;
                        }
                    }
                }
                let blur = if count > 0.0 { sum / count } else { center };
                let detail = center - blur;
                let noise_guard = (detail.abs() / (noise * 1.5).max(1e-6)).clamp(0.0, 1.0);
                target[channel] =
                    center + detail * amount.clamp(0.0, 1.5) * star_guard * noise_guard;
            }
        });
}

fn ds_poststack_apply_finish(
    data: &mut [f32],
    channels: usize,
    saturation: f32,
    contrast: f32,
    highlight_protection: f32,
) {
    if channels == 0 {
        return;
    }
    let (_, median, _, white, _) = ds_poststack_percentiles(data, channels);
    let range = white.max(median + 1e-6);
    let contrast = contrast.clamp(-0.5, 0.8);
    let saturation = saturation.clamp(-1.0, 1.5);
    let highlight = highlight_protection.clamp(0.0, 1.0);
    data.par_chunks_mut(channels).for_each(|pixel| {
        let luma = if channels >= 3 {
            0.2126 * pixel[0] + 0.7152 * pixel[1] + 0.0722 * pixel[2]
        } else {
            pixel[0]
        };
        if !luma.is_finite() {
            return;
        }
        let normalized = (luma / range).clamp(0.0, 1.0);
        let guard = 1.0 - highlight * normalized.powi(4);
        for channel in 0..channels {
            if !pixel[channel].is_finite() {
                continue;
            }
            let mut value = median + (pixel[channel] - median) * (1.0 + contrast * guard);
            if channels >= 3 && channel < 3 {
                value = luma + (value - luma) * (1.0 + saturation * guard);
            }
            pixel[channel] = value;
        }
    });
}

/// Reproduce exactamente la rama activa desde el máster fuente. Las
/// correcciones aditivas no cambian VAR; PCC multiplica VAR por ganancia².
fn ds_poststack_replay(
    result: &mut DeepSkyResult,
) -> Result<Option<crate::deepsky_studio_layers::StarLayerWorkspace>, String> {
    let Some(source) = result.source_data.as_ref() else {
        // Una revisión exclusivamente astrométrica no necesita instantánea de
        // píxeles, pero sí debe reflejar la solución activa en el resultado y
        // en su exportación.
        result.astrometry_solution = result
            .post_stack_recipe
            .operations
            .iter()
            .take(result.post_stack_recipe.cursor)
            .filter_map(|operation| match operation {
                PostStackOperation::Astrometry { solution } => Some(solution.clone()),
                _ => None,
            })
            .last();
        ds_poststack_sync_recipe(result)?;
        return Ok(None);
    };
    result.data.clone_from(source);
    result.variance = result.source_variance.clone();
    if let Some(layout) = result.source_layout.as_ref() {
        result.width = layout.width;
        result.height = layout.height;
        result.coverage.clone_from(&layout.coverage);
        result.weight.clone_from(&layout.weight);
        result.rejection_low.clone_from(&layout.rejection_low);
        result.rejection_high.clone_from(&layout.rejection_high);
        result
            .registration_residuals
            .clone_from(&layout.registration_residuals);
        result.neff = layout.neff.clone();
        result.dq = layout.dq.clone();
        result.struct_map = layout.struct_map.clone();
        result.struct_residual = layout.struct_residual.clone();
        result.recoverability = layout.recoverability.clone();
    }
    result.astrometry_solution = None;
    let active_count = result
        .post_stack_recipe
        .cursor
        .min(result.post_stack_recipe.operations.len());
    let mut layers: Option<crate::deepsky_studio_layers::StarLayerWorkspace> = None;
    // Clonar el prefijo completo retenía a la vez una segunda copia de todos
    // los modelos de gradiente/muestras y recetas. Se clona sólo la operación
    // que se va a ejecutar; así el préstamo de la receta termina antes de
    // mutar `result` y el pico queda acotado a un único paso.
    for operation_index in 0..active_count {
        let operation = result.post_stack_recipe.operations[operation_index].clone();
        match operation {
            PostStackOperation::Crop { crop } => {
                ds_poststack_apply_crop(result, &crop)?;
            }
            PostStackOperation::Gradient { model } => {
                if model.width != result.width
                    || model.height != result.height
                    || model.channels != result.channels
                {
                    return Err(
                        "El modelo de gradiente no coincide con la geometría del máster".into(),
                    );
                }
                let correction = ds_poststack_model_from_contract(&model).render_correction();
                if correction.len() != result.data.len() {
                    return Err("La corrección de gradiente tiene una longitud inválida".into());
                }
                result
                    .data
                    .par_iter_mut()
                    .zip(correction.par_iter())
                    .for_each(|(value, correction)| {
                        if value.is_finite() && correction.is_finite() {
                            *value -= *correction;
                        }
                    });
            }
            PostStackOperation::Astrometry { solution } => {
                result.astrometry_solution = Some(solution);
            }
            PostStackOperation::GaiaPcc {
                gain_r,
                gain_g,
                gain_b,
                ..
            } => {
                if result.channels < 3 {
                    return Err("PCC Gaia requiere un máster RGB".into());
                }
                let gains = [gain_r as f32, gain_g as f32, gain_b as f32];
                result
                    .data
                    .par_chunks_mut(result.channels)
                    .for_each(|pixel| {
                        for channel in 0..3 {
                            if pixel[channel].is_finite() {
                                pixel[channel] *= gains[channel];
                            }
                        }
                    });
                if let Some(variance) = result.variance.as_mut() {
                    variance.par_chunks_mut(result.channels).for_each(|pixel| {
                        for channel in 0..3 {
                            if pixel[channel].is_finite() {
                                pixel[channel] *= gains[channel] * gains[channel];
                            }
                        }
                    });
                }
            }
            PostStackOperation::DualBandPalette {
                palette,
                oiii_green_weight,
                crosstalk_suppression,
                neutralize,
                ..
            } => {
                if result.channels < 3 {
                    return Err("La paleta dual-band requiere un máster OSC/RGB Ha+OIII".into());
                }
                if let Some(reason) = ds_hoo_block_reason(&result.recipe) {
                    return Err(reason);
                }
                let (ha, oiii) = ds_extract_dual_band_planes(
                    &result.data,
                    result.channels,
                    oiii_green_weight.clamp(0.0, 1.0),
                    crosstalk_suppression.clamp(0.0, 1.0),
                )?;
                let pixels = result.width * result.height;
                let mut mapped = vec![0.0f32; pixels * 3];
                for pixel in 0..pixels {
                    let h = ha[pixel];
                    let o = oiii[pixel];
                    let (r, g, b) = match palette.as_str() {
                        "foraxx" => (
                            0.76 * h + 0.24 * o,
                            0.22 * h + 0.78 * o,
                            0.08 * h + 0.92 * o,
                        ),
                        "hooTeal" => (h, 0.82 * o + 0.18 * h, o),
                        _ => (h, o, o),
                    };
                    mapped[pixel * 3] = r;
                    mapped[pixel * 3 + 1] = g;
                    mapped[pixel * 3 + 2] = b;
                }
                if neutralize {
                    ds_neutralize_background(&mut mapped, result.width, result.height, 3);
                }
                result.data = mapped;
                result.channels = 3;
                // Sin curvas de transmisión completas no se publica una VAR de
                // canales mezclados como si conociéramos su covarianza.
                result.variance = None;
                result.neff = None;
            }
            PostStackOperation::Deconvolution {
                target,
                psf,
                iterations,
                regularization,
                deringing,
                flux_conservation,
            } => {
                if target == PostStackLayerTarget::Combined {
                    if layers.is_some() {
                        return Err(
                            "Restaura la imagen combinada antes de separar estrellas o selecciona una rama".into(),
                        );
                    }
                    crate::deepsky_studio_layers::deconvolve_linear(
                        &mut result.data,
                        result.width,
                        result.height,
                        result.channels,
                        &psf,
                        iterations,
                        regularization,
                        deringing,
                        flux_conservation,
                    )?;
                } else {
                    let workspace = layers
                        .as_mut()
                        .ok_or("Crea las capas estelares antes de restaurar Objeto o Estrellas")?;
                    let target_data = workspace.target_mut(target)?;
                    crate::deepsky_studio_layers::deconvolve_linear(
                        target_data,
                        result.width,
                        result.height,
                        result.channels,
                        &psf,
                        iterations,
                        regularization,
                        deringing,
                        flux_conservation,
                    )?;
                }
                // La deconvolución correlaciona el ruido. Hasta implementar su
                // propagación por operador no se conserva una VAR engañosa.
                result.variance = None;
                result.neff = None;
            }
            PostStackOperation::Denoise {
                target,
                strength,
                detail_protection,
                chroma_strength,
            } => {
                if target == PostStackLayerTarget::Combined {
                    if layers.is_some() {
                        return Err(
                            "La reducción común debe aplicarse antes de separar o sobre una rama concreta".into(),
                        );
                    }
                    ds_poststack_apply_denoise(
                        &mut result.data,
                        result.width,
                        result.height,
                        result.channels,
                        strength,
                        detail_protection,
                        chroma_strength,
                    );
                } else {
                    let workspace = layers
                        .as_mut()
                        .ok_or("Crea las capas antes de reducir ruido por rama")?;
                    let target_data = workspace.target_mut(target)?;
                    ds_poststack_apply_denoise(
                        target_data,
                        result.width,
                        result.height,
                        result.channels,
                        strength,
                        detail_protection,
                        chroma_strength,
                    );
                }
                // Denoise sigue en dominio radiométrico lineal, pero altera la
                // covarianza: conservar la VAR de entrada sería científicamente falso.
                result.variance = None;
                result.neff = None;
            }
            PostStackOperation::StarSeparation {
                engine,
                sensitivity,
                scale,
                halo_protection,
                faint_star_protection,
                preserve_residual: _,
            } => {
                if engine != "nativePsfMultiscale" {
                    return Err(format!(
                        "Motor de separación estelar no disponible: {engine}. Usa nativePsfMultiscale"
                    ));
                }
                let workspace = crate::deepsky_studio_layers::separate_stars_native(
                    &result.data,
                    result.width,
                    result.height,
                    result.channels,
                    sensitivity,
                    scale,
                    halo_protection,
                    faint_star_protection,
                    None,
                )?;
                ds_poststack_validate_layer_geometry(
                    &workspace,
                    result.width,
                    result.height,
                    result.channels,
                )?;
                // `result.data` ya contiene exactamente la revisión que se
                // separó. No publicamos una suma de ramas hasta que exista una
                // operación Recombine explícita.
                layers = Some(workspace);
            }
            PostStackOperation::Stretch { target, stretch } => {
                if target == PostStackLayerTarget::Combined {
                    ds_poststack_apply_stretch(&mut result.data, result.channels, &stretch);
                } else {
                    let workspace = layers
                        .as_mut()
                        .ok_or("Crea las capas antes de estirar una rama")?;
                    let target_data = workspace.target_mut(target)?;
                    ds_poststack_apply_stretch(target_data, result.channels, &stretch);
                    match target {
                        PostStackLayerTarget::Object => workspace.object_nonlinear = true,
                        PostStackLayerTarget::Stars => workspace.stars_nonlinear = true,
                        PostStackLayerTarget::Combined => {}
                    }
                }
                // Después de la transferencia radiométrica la VAR lineal deja
                // de describir la revisión. Se conserva intacta en la fuente.
                result.variance = None;
                result.neff = None;
            }
            PostStackOperation::Curves { target, curves } => {
                if target == PostStackLayerTarget::Combined {
                    ds_poststack_apply_curves(&mut result.data, result.channels, &curves)?;
                } else {
                    let workspace = layers
                        .as_mut()
                        .ok_or("Crea las capas antes de aplicar curvas a una rama")?;
                    ds_poststack_validate_layer_geometry(
                        workspace,
                        result.width,
                        result.height,
                        result.channels,
                    )?;
                    let target_data = workspace.target_mut(target)?;
                    ds_poststack_apply_curves(target_data, result.channels, &curves)?;
                    match target {
                        PostStackLayerTarget::Object => workspace.object_nonlinear = true,
                        PostStackLayerTarget::Stars => workspace.stars_nonlinear = true,
                        PostStackLayerTarget::Combined => {}
                    }
                }
                result.variance = None;
                result.neff = None;
            }
            PostStackOperation::Detail {
                target,
                amount,
                radius,
                star_protection,
            } => {
                if target == PostStackLayerTarget::Combined {
                    ds_poststack_apply_detail(
                        &mut result.data,
                        result.width,
                        result.height,
                        result.channels,
                        amount,
                        radius,
                        star_protection,
                    );
                } else {
                    let workspace = layers
                        .as_mut()
                        .ok_or("Crea las capas antes de realzar una rama")?;
                    let target_data = workspace.target_mut(target)?;
                    ds_poststack_apply_detail(
                        target_data,
                        result.width,
                        result.height,
                        result.channels,
                        amount,
                        radius,
                        star_protection,
                    );
                }
                result.variance = None;
                result.neff = None;
            }
            PostStackOperation::StarAdjustment {
                reduction,
                saturation,
                halo_suppression,
            } => {
                let workspace = layers
                    .as_mut()
                    .ok_or("Crea las capas antes de ajustar Estrellas")?;
                workspace.stars_modified = true;
                crate::deepsky_studio_layers::adjust_star_layer(
                    &mut workspace.stars,
                    result.width,
                    result.height,
                    result.channels,
                    reduction,
                    saturation,
                    halo_suppression,
                );
                result.variance = None;
                result.neff = None;
            }
            PostStackOperation::Recombine {
                object_weight,
                star_weight,
                residual_weight,
            } => {
                let workspace = layers
                    .as_mut()
                    .ok_or("No existen capas Objeto/Estrellas para recombinar")?;
                ds_poststack_validate_layer_geometry(
                    workspace,
                    result.width,
                    result.height,
                    result.channels,
                )?;
                if workspace.object_nonlinear != workspace.stars_nonlinear {
                    // La edición de la primera rama debe poder guardarse. El
                    // combinado permanece en su último estado seguro y la UI
                    // explica qué rama falta llevar al mismo dominio.
                    workspace.recombined = false;
                    continue;
                }
                workspace.weights = (
                    object_weight.clamp(0.0, 2.0),
                    star_weight.clamp(0.0, 2.0),
                    residual_weight.clamp(0.0, 2.0),
                );
                workspace.recombined = true;
                result.data = workspace.compose();
                if !workspace.exact_restore() {
                    result.variance = None;
                    result.neff = None;
                }
            }
            PostStackOperation::Finish {
                target,
                saturation,
                contrast,
                highlight_protection,
            } => {
                if target == PostStackLayerTarget::Combined {
                    ds_poststack_apply_finish(
                        &mut result.data,
                        result.channels,
                        saturation,
                        contrast,
                        highlight_protection,
                    );
                } else {
                    let workspace = layers
                        .as_mut()
                        .ok_or("Crea las capas antes de acabar una rama")?;
                    let target_data = workspace.target_mut(target)?;
                    ds_poststack_apply_finish(
                        target_data,
                        result.channels,
                        saturation,
                        contrast,
                        highlight_protection,
                    );
                    match target {
                        PostStackLayerTarget::Object => workspace.object_nonlinear = true,
                        PostStackLayerTarget::Stars => workspace.stars_nonlinear = true,
                        PostStackLayerTarget::Combined => {}
                    }
                }
                result.variance = None;
                result.neff = None;
            }
        }
    }
    if let Some(workspace) = layers.as_ref() {
        ds_poststack_validate_layer_geometry(
            workspace,
            result.width,
            result.height,
            result.channels,
        )?;
        if let Some(object) = result.recipe.as_object_mut() {
            object.insert(
                "postStackLayerDiagnostics".into(),
                serde_json::json!({
                    "engine": "nativePsfMultiscale",
                    "objectModified": workspace.object_modified,
                    "starsModified": workspace.stars_modified,
                    "recombined": workspace.recombined,
                    "exactRestore": workspace.exact_restore(),
                    "domain": if workspace.object_nonlinear || workspace.stars_nonlinear {
                        "presentation"
                    } else {
                        "linear"
                    },
                    "objectDomain": if workspace.object_nonlinear { "presentation" } else { "linear" },
                    "starsDomain": if workspace.stars_nonlinear { "presentation" } else { "linear" },
                    "recombineEligible": workspace.object_nonlinear == workspace.stars_nonlinear,
                    "recombineBlockReason": if workspace.object_nonlinear != workspace.stars_nonlinear {
                        Some("Objeto y Estrellas están en dominios distintos. Aplica el mismo estirado/acabado a la otra rama o deshaz la operación no lineal.")
                    } else {
                        None
                    },
                    "reconstructionError": workspace.reconstruction_error,
                    "starFraction": workspace.star_fraction,
                    "psf": workspace.psf,
                }),
            );
        }
    } else if let Some(object) = result.recipe.as_object_mut() {
        object.remove("postStackLayerDiagnostics");
    }
    ds_poststack_sync_recipe(result)?;
    Ok(layers)
}

fn ds_poststack_recompute(result: &mut DeepSkyResult) -> Result<(), String> {
    ds_poststack_replay(result).map(|_| ())
}

/// Copia de análisis justo antes de PCC. Es necesaria al recalibrar color:
/// medir un máster que ya contiene las ganancias anteriores produciría una
/// oscilación (ganancia, casi-identidad, ganancia...) aunque la publicación
/// final no multiplicase in-place.
fn ds_poststack_data_before_pcc(result: &DeepSkyResult) -> Result<Vec<f32>, String> {
    let mut data = result.source_data.as_ref().unwrap_or(&result.data).clone();
    let mut width = result
        .source_layout
        .as_ref()
        .map(|layout| layout.width)
        .unwrap_or(result.width);
    let mut height = result
        .source_layout
        .as_ref()
        .map(|layout| layout.height)
        .unwrap_or(result.height);
    for operation in result
        .post_stack_recipe
        .operations
        .iter()
        .take(result.post_stack_recipe.cursor)
    {
        match operation {
            PostStackOperation::Crop { crop } => {
                data = ds_poststack_crop_interleaved(&data, width, height, result.channels, crop)?;
                width = crop.width;
                height = crop.height;
            }
            PostStackOperation::Gradient { model } => {
                if model.width != width
                    || model.height != height
                    || model.channels != result.channels
                {
                    return Err(
                        "El modelo de gradiente no coincide con la geometría del máster".into(),
                    );
                }
                let correction = ds_poststack_model_from_contract(model).render_correction();
                data.par_iter_mut()
                    .zip(correction.par_iter())
                    .for_each(|(value, correction)| {
                        if value.is_finite() && correction.is_finite() {
                            *value -= *correction;
                        }
                    });
            }
            PostStackOperation::Astrometry { .. }
            | PostStackOperation::GaiaPcc { .. }
            | PostStackOperation::DualBandPalette { .. }
            | PostStackOperation::Deconvolution { .. }
            | PostStackOperation::Denoise { .. }
            | PostStackOperation::StarSeparation { .. }
            | PostStackOperation::Stretch { .. }
            | PostStackOperation::Curves { .. }
            | PostStackOperation::Detail { .. }
            | PostStackOperation::StarAdjustment { .. }
            | PostStackOperation::Recombine { .. }
            | PostStackOperation::Finish { .. } => {
                if matches!(operation, PostStackOperation::GaiaPcc { .. }) {
                    break;
                }
            }
        }
    }
    Ok(data)
}

/// Fuente autoritativa justo antes del gradiente: máster inmutable más el
/// recorte activo. El modelo anterior nunca se mide sobre su propio residual.
fn ds_poststack_data_before_gradient(result: &mut DeepSkyResult) -> Result<Vec<f32>, String> {
    ds_poststack_ensure_source(result);
    let mut data = result
        .source_data
        .as_ref()
        .ok_or_else(|| "El máster lineal inmutable no está disponible".to_string())?
        .clone();
    let mut width = result
        .source_layout
        .as_ref()
        .map(|layout| layout.width)
        .unwrap_or(result.width);
    let mut height = result
        .source_layout
        .as_ref()
        .map(|layout| layout.height)
        .unwrap_or(result.height);
    for operation in result
        .post_stack_recipe
        .operations
        .iter()
        .take(result.post_stack_recipe.cursor)
    {
        if let PostStackOperation::Crop { crop } = operation {
            data = ds_poststack_crop_interleaved(&data, width, height, result.channels, crop)?;
            width = crop.width;
            height = crop.height;
        }
        if matches!(operation, PostStackOperation::Gradient { .. }) {
            break;
        }
    }
    if width != result.width || height != result.height {
        return Err("La geometría activa no coincide con el recorte de la receta".into());
    }
    Ok(data)
}

fn ds_poststack_is_linear(result: &DeepSkyResult) -> bool {
    !result
        .post_stack_recipe
        .operations
        .iter()
        .take(result.post_stack_recipe.cursor)
        .any(|operation| {
            matches!(
                operation,
                PostStackOperation::Stretch { .. }
                    | PostStackOperation::Curves { .. }
                    | PostStackOperation::Detail { .. }
                    | PostStackOperation::Finish { .. }
            )
        })
}

fn ds_poststack_layer_state(result: &DeepSkyResult) -> Option<PostStackLayerState> {
    let diagnostics = result.recipe.get("postStackLayerDiagnostics")?;
    Some(PostStackLayerState {
        available: true,
        engine: diagnostics
            .get("engine")
            .and_then(|value| value.as_str())
            .unwrap_or("nativePsfMultiscale")
            .to_string(),
        object_modified: diagnostics
            .get("objectModified")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        stars_modified: diagnostics
            .get("starsModified")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        recombined: diagnostics
            .get("recombined")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        exact_restore: diagnostics
            .get("exactRestore")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        domain: diagnostics
            .get("domain")
            .and_then(|value| value.as_str())
            .unwrap_or("linear")
            .to_string(),
        object_domain: diagnostics
            .get("objectDomain")
            .and_then(|value| value.as_str())
            .unwrap_or("linear")
            .to_string(),
        stars_domain: diagnostics
            .get("starsDomain")
            .and_then(|value| value.as_str())
            .unwrap_or("linear")
            .to_string(),
        recombine_eligible: diagnostics
            .get("recombineEligible")
            .and_then(|value| value.as_bool())
            .unwrap_or(true),
        recombine_block_reason: diagnostics
            .get("recombineBlockReason")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        reconstruction_error: diagnostics
            .get("reconstructionError")
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0) as f32,
        star_fraction: diagnostics
            .get("starFraction")
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0) as f32,
        psf: diagnostics
            .get("psf")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok()),
    })
}

fn ds_poststack_source_active_view(
    result: &DeepSkyResult,
) -> Result<(Vec<f32>, usize, usize), String> {
    let mut data = result.source_data.as_ref().unwrap_or(&result.data).clone();
    let mut width = result
        .source_layout
        .as_ref()
        .map(|layout| layout.width)
        .unwrap_or(result.width);
    let mut height = result
        .source_layout
        .as_ref()
        .map(|layout| layout.height)
        .unwrap_or(result.height);
    for operation in result
        .post_stack_recipe
        .operations
        .iter()
        .take(result.post_stack_recipe.cursor)
    {
        if let PostStackOperation::Crop { crop } = operation {
            data = ds_poststack_crop_interleaved(&data, width, height, result.channels, crop)?;
            width = crop.width;
            height = crop.height;
        }
    }
    Ok((data, width, height))
}

fn ds_poststack_recipe_text<'a>(
    recipe: &'a serde_json::Value,
    paths: &[&[&str]],
) -> Option<&'a str> {
    paths.iter().find_map(|path| {
        let mut value = recipe;
        for key in *path {
            value = value.get(*key)?;
        }
        value.as_str()
    })
}

fn ds_poststack_source_descriptor_for(result: &DeepSkyResult) -> PostStackSourceDescriptor {
    if let Some(value) = result.recipe.get("postStackSource") {
        if let Ok(mut descriptor) =
            serde_json::from_value::<PostStackSourceDescriptor>(value.clone())
        {
            descriptor.channels = result.channels;
            descriptor.has_variance = result.variance.is_some();
            descriptor.has_dq = result.dq.is_some();
            return descriptor;
        }
    }

    let capture_mode = ds_poststack_recipe_text(
        &result.recipe,
        &[&["captureMode"], &["parameters", "captureMode"]],
    )
    .unwrap_or("auto");
    let normalized_mode = capture_mode
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    let filter = ds_poststack_recipe_text(
        &result.recipe,
        &[
            &["filterProfile"],
            &["outputCalibrationSignature", "filter"],
            &["parameters", "filterProfile"],
        ],
    )
    .map(str::to_string);
    let filter_profile = filter
        .as_deref()
        .map(|value| ds_filter_token(value).unwrap_or(value).to_ascii_uppercase());
    let component_filters = filter_profile
        .as_deref()
        .map(ds_filter_components)
        .unwrap_or_default();
    let kind = if normalized_mode == "dualbandosc" || component_filters.len() > 1 {
        PostStackSourceKind::DualBand
    } else if matches!(normalized_mode.as_str(), "broadbandmono" | "mononarrowband")
        || result.channels == 1
    {
        PostStackSourceKind::Mono
    } else if normalized_mode == "broadbandosc" || result.channels == 3 {
        PostStackSourceKind::RgbBroadband
    } else {
        PostStackSourceKind::Unknown
    };
    let routing_reason = match kind {
        PostStackSourceKind::RgbBroadband => {
            "Máster RGB lineal: ruta de color broadband/PCC disponible.".into()
        }
        PostStackSourceKind::Mono => {
            "Máster monocromo: conservar como canal o combinar con otros másteres.".into()
        }
        PostStackSourceKind::DualBand => {
            "Máster OSC dual-band: ofrece separación espectral y galería de paletas.".into()
        }
        PostStackSourceKind::Unknown => "La metadata no determina una ruta de color segura.".into(),
    };
    PostStackSourceDescriptor {
        id: result.id.clone(),
        origin: PostStackSourceOrigin::IntegrationProduct,
        path: None,
        file_name: None,
        kind,
        channels: result.channels,
        linear: true,
        cfa: false,
        filter_profile,
        component_filters,
        camera: ds_poststack_recipe_text(
            &result.recipe,
            &[&["outputCalibrationSignature", "camera"]],
        )
        .map(str::to_string),
        instrument: None,
        has_variance: result.variance.is_some(),
        has_dq: result.dq.is_some(),
        routing_reason,
    }
}

fn ds_poststack_state_for(result: &DeepSkyResult, preview: String) -> PostStackRevisionState {
    PostStackRevisionState {
        width: result.width,
        height: result.height,
        cursor: result.post_stack_recipe.cursor,
        total: result.post_stack_recipe.operations.len(),
        can_undo: result.post_stack_recipe.cursor > 0,
        can_redo: result.post_stack_recipe.cursor < result.post_stack_recipe.operations.len(),
        linear: ds_poststack_is_linear(result),
        operations: result
            .post_stack_recipe
            .operations
            .iter()
            .take(result.post_stack_recipe.cursor)
            .map(|operation| {
                let name = ds_poststack_operation_name(operation);
                let target = ds_poststack_operation_target(operation);
                if target == PostStackLayerTarget::Combined {
                    name.to_string()
                } else {
                    format!("{name}:{}", target.as_str())
                }
            })
            .collect(),
        crop: result
            .post_stack_recipe
            .operations
            .iter()
            .take(result.post_stack_recipe.cursor)
            .find_map(|operation| match operation {
                PostStackOperation::Crop { crop } => Some(crop.clone()),
                _ => None,
            }),
        astrometry: result.astrometry_solution.clone(),
        astrometry_status: result.recipe.get("astrometryStatus").cloned(),
        source: ds_poststack_source_descriptor_for(result),
        layers: ds_poststack_layer_state(result),
        preview,
    }
}

/// Recupera un mutex envenenado y limpia el bit de poison. Las operaciones
/// post-stack son transaccionales, y undo/redo/reset existen precisamente
/// para volver al último replay válido después de un fallo. Devolver sólo un
/// `map_err` aquí dejaría el editor bloqueado para siempre en ese proceso.
fn ds_poststack_recover_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            let guard = poisoned.into_inner();
            mutex.clear_poison();
            guard
        }
    }
}

fn ds_poststack_publish(state: &AppState) -> Result<PostStackRevisionState, String> {
    let (stacked, revision) = {
        let result_guard = ds_poststack_recover_mutex(&state.deep_sky_result);
        let result = result_guard
            .as_ref()
            .ok_or("No hay máster lineal de cielo profundo")?;
        // `ds_preview_for_linear_result` materializa tres búferes a resolución
        // completa además del StackResult. El espejo u16 sí debe conservar la
        // geometría científica para el visor principal; el PNG del editor no.
        let stacked = ds_stack_result_for_linear(result)?;
        let preview = ds_poststack_master_preview(
            &result.data,
            result.width,
            result.height,
            result.channels,
            "deepsky-poststack",
        )?;
        let revision = ds_poststack_state_for(result, preview);
        (stacked, revision)
    };
    {
        let mut current = ds_poststack_recover_mutex(&state.stacked_image);
        *current = Some(stacked);
    }
    // La invalidación debe sobrevivir a un mutex envenenado: recupera el guard
    // y limpia igualmente en vez de entrar en pánico en cascada.
    ds_poststack_recover_mutex(&state.deconv_cache).clear();
    ds_poststack_recover_mutex(&state.wavelet_cache).clear();
    ds_poststack_recover_mutex(&state.filter_cache).clear();
    Ok(revision)
}

fn ds_poststack_luma(data: &[f32], pixels: usize, channels: usize) -> Vec<f32> {
    (0..pixels)
        .map(|pixel| {
            let mut sum = 0.0f32;
            let count = channels.min(3).max(1);
            for channel in 0..count {
                let value = data[pixel * channels + channel];
                if value.is_finite() {
                    sum += value;
                }
            }
            sum / count as f32
        })
        .collect()
}

fn ds_poststack_exclusion_mask(
    data: &[f32],
    width: usize,
    height: usize,
    channels: usize,
    request: &PostStackGradientRequest,
    coverage: Option<&[f32]>,
    dq: Option<&[u32]>,
) -> Vec<bool> {
    let pixels = width * height;
    let luma = ds_poststack_luma(data, pixels, channels);
    let mut mask = vec![false; pixels];
    if let Some(coverage) = coverage {
        if coverage.len() != pixels {
            // Una geometría de cobertura incoherente invalida el ajuste de
            // fondo completo: continuar podría usar píxeles sin soporte como
            // muestras científicas.
            mask.fill(true);
        } else {
            mask.par_iter_mut()
                .zip(coverage.par_iter())
                .for_each(|(excluded, value)| {
                    *excluded = !value.is_finite() || *value <= 0.0;
                });
        }
    }
    if let Some(dq) = dq {
        if dq.len() != pixels {
            mask.fill(true);
        } else {
            let fatal = crate::deepsky_variance::dq::SATURATED
                | crate::deepsky_variance::dq::NONLINEAR
                | crate::deepsky_variance::dq::HOT_COLD
                | crate::deepsky_variance::dq::COSMIC
                | crate::deepsky_variance::dq::NAN_INPUT
                | crate::deepsky_variance::dq::FLAT_INVALID
                | crate::deepsky_variance::dq::NO_COVERAGE
                | crate::deepsky_variance::dq::DEGRADED_CALIBRATION;
            mask.par_iter_mut()
                .zip(dq.par_iter())
                .for_each(|(excluded, flags)| {
                    *excluded |= *flags & fatal != 0;
                });
        }
    }
    if request.protect_extended_objects {
        let step = (pixels / 300_000).max(1);
        let mut sample = luma
            .iter()
            .enumerate()
            .step_by(step)
            .filter_map(|(index, value)| (!mask[index] && value.is_finite()).then_some(*value))
            .collect::<Vec<_>>();
        sample.sort_by(|a, b| a.total_cmp(b));
        if !sample.is_empty() {
            let median = sample[sample.len() / 2];
            let mut deviation = sample
                .iter()
                .map(|value| (value - median).abs())
                .collect::<Vec<_>>();
            deviation.sort_by(|a, b| a.total_cmp(b));
            let sigma = (deviation[deviation.len() / 2] * 1.4826).max(1e-6);
            // Sensibilidad 0 protege sólo señal muy obvia; 1 protege también
            // halos y nebulosidad tenue. El rango evita convertir ruido puro
            // en máscara de objeto.
            let sigma_factor = 4.2 - request.sensitivity.clamp(0.0, 1.0) * 2.4;
            let threshold = median + sigma_factor * sigma;
            mask.par_iter_mut()
                .zip(luma.par_iter())
                .for_each(|(excluded, value)| {
                    *excluded |= value.is_finite() && *value > threshold;
                });
        }
    }
    for rect in &request.exclusion_rects {
        // Un ancho/alto negativo (arrastre invertido o payload corrupto) no
        // debe invertir el slice: ordena extremos y descarta no-finitos en vez
        // de entrar en pánico con el lock del resultado tomado.
        let rx0 = rect.x.min(rect.x + rect.width);
        let rx1 = rect.x.max(rect.x + rect.width);
        let ry0 = rect.y.min(rect.y + rect.height);
        let ry1 = rect.y.max(rect.y + rect.height);
        if ![rx0, rx1, ry0, ry1].iter().all(|value| value.is_finite()) {
            continue;
        }
        let x0 = (rx0.clamp(0.0, 1.0) * width as f32).floor() as usize;
        let y0 = (ry0.clamp(0.0, 1.0) * height as f32).floor() as usize;
        let x1 = (rx1.clamp(0.0, 1.0) * width as f32).ceil() as usize;
        let y1 = (ry1.clamp(0.0, 1.0) * height as f32).ceil() as usize;
        for y in y0.min(height)..y1.min(height) {
            mask[y * width + x0.min(width)..y * width + x1.min(width)].fill(true);
        }
    }
    mask
}

fn ds_poststack_neutralize_model(model: &mut crate::deepsky_background::BgModel) {
    let valid = model
        .coeffs
        .iter()
        .enumerate()
        .filter(|(_, coeffs)| !coeffs.is_empty())
        .map(|(index, coeffs)| (index, coeffs.clone()))
        .collect::<Vec<_>>();
    if valid.is_empty() {
        return;
    }
    let terms = valid[0].1.len();
    if valid.iter().any(|(_, coeffs)| coeffs.len() != terms) {
        return;
    }
    let mut average = vec![0.0f64; terms];
    let mut level = 0.0f64;
    for (index, coeffs) in &valid {
        for (term, value) in coeffs.iter().enumerate() {
            average[term] += value / valid.len() as f64;
        }
        level += model.level[*index] / valid.len() as f64;
    }
    for channel in 0..model.ch {
        model.coeffs[channel] = average.clone();
        model.level[channel] = level;
    }
}

fn ds_poststack_split_half_ratio(
    data: &[f32],
    width: usize,
    height: usize,
    channels: usize,
    base_mask: &[bool],
    chromatic: bool,
    degree: usize,
) -> f32 {
    let mut mask_a = base_mask.to_vec();
    let mut mask_b = base_mask.to_vec();
    for y in 0..height {
        for x in 0..width {
            let cell_x = x * 32 / width.max(1);
            let cell_y = y * 32 / height.max(1);
            if (cell_x + cell_y) % 2 == 0 {
                mask_b[y * width + x] = true;
            } else {
                mask_a[y * width + x] = true;
            }
        }
    }
    let Some(mut model_a) = crate::deepsky_background::fit_background_model_masked_degree(
        data,
        width,
        height,
        channels,
        Some(&mask_a),
        degree,
    ) else {
        return f32::INFINITY;
    };
    let Some(mut model_b) = crate::deepsky_background::fit_background_model_masked_degree(
        data,
        width,
        height,
        channels,
        Some(&mask_b),
        degree,
    ) else {
        return f32::INFINITY;
    };
    if !chromatic {
        ds_poststack_neutralize_model(&mut model_a);
        ds_poststack_neutralize_model(&mut model_b);
    }
    let split_rms = crate::deepsky_background::bg_split_half_rms(&model_a, &model_b) as f32;
    let luma = ds_poststack_luma(data, width * height, channels);
    let noise = ds_bg_noise(&luma).1.max(1e-6);
    split_rms / noise
}

fn ds_poststack_sample_split_half_ratio(
    data: &[f32],
    width: usize,
    height: usize,
    channels: usize,
    samples: &[PostStackBackgroundSample],
    degree: usize,
    base_mask: &[bool],
    chromatic: bool,
) -> f32 {
    let mut ordered = samples
        .iter()
        .filter(|sample| sample.enabled)
        .cloned()
        .collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        left.y
            .total_cmp(&right.y)
            .then_with(|| left.x.total_cmp(&right.x))
    });
    let mut half_a = Vec::with_capacity(ordered.len().div_ceil(2));
    let mut half_b = Vec::with_capacity(ordered.len() / 2);
    let row_tolerance = ordered
        .iter()
        .map(|sample| sample.radius)
        .fold(0.01f32, f32::max)
        .clamp(0.01, 0.08);
    let mut row = 0usize;
    let mut column = 0usize;
    let mut row_anchor = ordered.first().map(|sample| sample.y).unwrap_or(0.0);
    for sample in ordered {
        if (sample.y - row_anchor).abs() > row_tolerance {
            row += 1;
            column = 0;
            row_anchor = sample.y;
        }
        if (row + column) % 2 == 0 {
            half_a.push(sample);
        } else {
            half_b.push(sample);
        }
        column += 1;
    }
    let Ok(mut model_a) = crate::deepsky_background::fit_background_model_samples(
        data,
        width,
        height,
        channels,
        &half_a,
        degree,
        Some(base_mask),
    ) else {
        return f32::INFINITY;
    };
    let Ok(mut model_b) = crate::deepsky_background::fit_background_model_samples(
        data,
        width,
        height,
        channels,
        &half_b,
        degree,
        Some(base_mask),
    ) else {
        return f32::INFINITY;
    };
    if !chromatic {
        ds_poststack_neutralize_model(&mut model_a);
        ds_poststack_neutralize_model(&mut model_b);
    }
    let split_rms = crate::deepsky_background::bg_split_half_rms(&model_a, &model_b) as f32;
    let luma = ds_poststack_luma(data, width * height, channels)
        .into_iter()
        .enumerate()
        .filter_map(|(index, value)| (!base_mask[index] && value.is_finite()).then_some(value))
        .collect::<Vec<_>>();
    if luma.is_empty() {
        return f32::INFINITY;
    }
    let noise = ds_bg_noise(&luma).1.max(1e-6);
    split_rms / noise
}

fn ds_poststack_gradient_stability_error(
    split_half_ratio: f32,
    allow_unstable: bool,
) -> Option<String> {
    if allow_unstable || (split_half_ratio.is_finite() && split_half_ratio <= 0.2) {
        return None;
    }
    Some(if split_half_ratio.is_finite() {
        format!(
            "El modelo de fondo es inestable entre mitades ({split_half_ratio:.2}σ > 0.20σ) y no se aplicó. Reubica muestras/protecciones o confirma allowUnstable en modo experto."
        )
    } else {
        "No fue posible validar el modelo de fondo con mitades independientes y no se aplicó. Añade muestras/protecciones o confirma allowUnstable en modo experto.".into()
    })
}

/// Las vistas del editor son auxiliares de presentación, no el producto
/// científico. Limitar el borde largo evita materializar simultáneamente un
/// RGB16, un RGB8, un RGBA y un PNG a 60 MP (más de 700 MiB transitorios),
/// mientras el máster float32 y todos sus mapas permanecen a resolución
/// completa e inmutables.
const DS_POSTSTACK_PREVIEW_MAX_EDGE: usize = 1600;

fn ds_poststack_preview_geometry(width: usize, height: usize) -> (usize, usize, usize) {
    let factor = width
        .max(height)
        .div_ceil(DS_POSTSTACK_PREVIEW_MAX_EDGE)
        .max(1);
    (
        factor,
        width.div_ceil(factor).max(1),
        height.div_ceil(factor).max(1),
    )
}

/// Promedio de área a una geometría acotada. `range` convierte diagnósticos
/// firmados a 0..65535 después del promedio; `None` conserva el contrato
/// lineal 0..65535 de la vista maestra. Los no-finitos no contaminan el bloque.
fn ds_poststack_preview_rgb16(
    data: &[f32],
    width: usize,
    height: usize,
    channels: usize,
    range: Option<(f32, f32)>,
) -> (Vec<u16>, usize, usize) {
    let (factor, preview_width, preview_height) = ds_poststack_preview_geometry(width, height);
    let mut rgb16 = vec![0u16; preview_width * preview_height * 3];
    rgb16
        .par_chunks_mut(3)
        .enumerate()
        .for_each(|(preview_pixel, rgb)| {
            let px = preview_pixel % preview_width;
            let py = preview_pixel / preview_width;
            let x0 = px * factor;
            let y0 = py * factor;
            let x1 = (x0 + factor).min(width);
            let y1 = (y0 + factor).min(height);
            for channel in 0..3 {
                let source_channel = channel.min(channels - 1);
                let mut sum = 0.0f64;
                let mut count = 0usize;
                for y in y0..y1 {
                    for x in x0..x1 {
                        let value = data[(y * width + x) * channels + source_channel];
                        if value.is_finite() {
                            sum += value as f64;
                            count += 1;
                        }
                    }
                }
                if count == 0 {
                    rgb[channel] = 0;
                    continue;
                }
                let value = (sum / count as f64) as f32;
                rgb[channel] = if let Some((low, high)) = range {
                    (((value - low) / (high - low).max(1e-6)).clamp(0.0, 1.0) * 65535.0).round()
                        as u16
                } else {
                    value.clamp(0.0, 65535.0) as u16
                };
            }
        });
    (rgb16, preview_width, preview_height)
}

fn ds_poststack_diagnostic_preview(
    data: &[f32],
    width: usize,
    height: usize,
    channels: usize,
    prefix: &str,
) -> Result<String, String> {
    let pixels = width * height;
    if width == 0 || height == 0 || channels == 0 || data.len() < pixels * channels {
        return Err("La vista diagnóstica no coincide con el máster".into());
    }
    let step = (pixels / 300_000).max(1);
    let mut sample = (0..pixels)
        .step_by(step)
        .flat_map(|pixel| (0..channels.min(3)).map(move |channel| data[pixel * channels + channel]))
        .filter(|value| value.is_finite())
        .collect::<Vec<_>>();
    if sample.is_empty() {
        return Err("La vista diagnóstica no contiene valores finitos".into());
    }
    sample.sort_by(|a, b| a.total_cmp(b));
    let low = sample[sample.len() / 100];
    let high = sample[sample.len() - 1 - sample.len() / 100].max(low + 1e-6);
    let (rgb16, preview_width, preview_height) =
        ds_poststack_preview_rgb16(data, width, height, channels, Some((low, high)));
    let preview8 = ds_render_stretch(&rgb16, preview_width, preview_height, false, 2.8, 0.35);
    let preview_pixels = preview_width * preview_height;
    let mut rgba = Vec::with_capacity(preview_pixels * 4);
    for pixel in 0..preview_pixels {
        rgba.extend_from_slice(&[
            preview8[pixel * 3],
            preview8[pixel * 3 + 1],
            preview8[pixel * 3 + 2],
            255,
        ]);
    }
    let image = RgbaImage::from_raw(preview_width as u32, preview_height as u32, rgba)
        .ok_or("No se pudo construir la vista diagnóstica")?;
    let mut encoded = Vec::new();
    DynamicImage::ImageRgba8(image)
        .write_to(&mut Cursor::new(&mut encoded), image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(
        save_preview_png_to_temp(&encoded, prefix).unwrap_or_else(|| {
            format!(
                "data:image/png;base64,{}",
                general_purpose::STANDARD.encode(&encoded)
            )
        }),
    )
}

fn ds_poststack_master_preview(
    data: &[f32],
    width: usize,
    height: usize,
    channels: usize,
    prefix: &str,
) -> Result<String, String> {
    let pixels = width.saturating_mul(height);
    if width == 0 || height == 0 || channels == 0 || data.len() < pixels.saturating_mul(channels) {
        return Err("La revisión no coincide con el máster".into());
    }
    let (rgb16, preview_width, preview_height) =
        ds_poststack_preview_rgb16(data, width, height, channels, None);
    let preview8 = ds_render_stretch(&rgb16, preview_width, preview_height, false, 2.8, 0.25);
    let preview_pixels = preview_width * preview_height;
    let mut rgba = Vec::with_capacity(preview_pixels * 4);
    for pixel in 0..preview_pixels {
        rgba.extend_from_slice(&[
            preview8[pixel * 3],
            preview8[pixel * 3 + 1],
            preview8[pixel * 3 + 2],
            255,
        ]);
    }
    let image = RgbaImage::from_raw(preview_width as u32, preview_height as u32, rgba)
        .ok_or("No se pudo construir la vista de revisión")?;
    let mut encoded = Vec::new();
    DynamicImage::ImageRgba8(image)
        .write_to(&mut Cursor::new(&mut encoded), image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(
        save_preview_png_to_temp(&encoded, prefix).unwrap_or_else(|| {
            format!(
                "data:image/png;base64,{}",
                general_purpose::STANDARD.encode(&encoded)
            )
        }),
    )
}

fn ds_poststack_has_nonlinear_marker(value: &str) -> bool {
    let lowered = value.to_ascii_lowercase();
    [
        "nonlinear",
        "non-linear",
        "stretched",
        "estirado",
        "histogramtransformation",
        "curvestransformation",
        "gamma corrected",
        "gamma-corrected",
        "srgb",
    ]
    .iter()
    .any(|marker| lowered.contains(marker))
}

fn ds_poststack_linear_header_is_false(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "f" | "false" | "no" | "nonlinear" | "stretched"
    )
}

fn ds_poststack_validate_fits_master(
    path: &str,
    image: &DsImage,
) -> Result<(Option<String>, Option<String>, Option<String>), String> {
    let fits = fitrs::Fits::open(path).map_err(|error| format!("FITS open: {error:?}"))?;
    let hdu = fits.iter().next().ok_or("FITS sin HDU primario")?;
    if image.bayer.is_some()
        || ds_hdr_str(&hdu, "BAYERPAT").is_some()
        || ds_hdr_str(&hdu, "BAYERPATN").is_some()
    {
        return Err(
            "El editor autónomo requiere un máster ya integrado; no admite un FITS CFA/Bayer sin debayerizar."
                .into(),
        );
    }
    let frame_type = ds_hdr_str(&hdu, "IMAGETYP")
        .or_else(|| ds_hdr_str(&hdu, "OBSTYPE"))
        .or_else(|| ds_hdr_str(&hdu, "FRAME"));
    let object = ds_hdr_str(&hdu, "OBJECT");
    if let Some(role) = ds_classify_header(path, frame_type.as_deref(), object.as_deref()) {
        if role != "lights" {
            return Err(format!(
                "El archivo se identifica como calibración ({role}); abre un máster de lights lineal."
            ));
        }
    }
    for key in ["LINEAR", "ZASLIN"] {
        if ds_hdr_str(&hdu, key).is_some_and(|value| ds_poststack_linear_header_is_false(&value)) {
            return Err(format!(
                "El encabezado FITS declara {key}=false/no lineal; el Studio no altera ese archivo."
            ));
        }
    }
    if ds_hdr_num(&hdu, "GAMMA").is_some_and(|value| (value - 1.0).abs() > 1e-6) {
        return Err("El FITS declara una gamma distinta de 1; no es un máster lineal.".into());
    }
    for key in ["STRETCH", "TRANSFER", "COLORSPC", "COLORSPACE"] {
        if ds_hdr_str(&hdu, key).is_some_and(|value| {
            ds_poststack_has_nonlinear_marker(&value)
                || (!matches!(key, "COLORSPC" | "COLORSPACE")
                    && !matches!(
                        value.trim().to_ascii_lowercase().as_str(),
                        "" | "none" | "off" | "false" | "identity" | "linear" | "0"
                    ))
        }) {
            return Err(format!(
                "El encabezado FITS declara una transformación no lineal en {key}."
            ));
        }
    }
    let filter = ds_hdr_str(&hdu, "FILTER").or_else(|| {
        std::path::Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(ds_filter_token)
            .map(str::to_string)
    });
    let camera = ds_hdr_str(&hdu, "CAMERA")
        .or_else(|| ds_hdr_str(&hdu, "INSTRUME"))
        .or_else(|| ds_hdr_str(&hdu, "DETECTOR"));
    let instrument = ds_hdr_str(&hdu, "TELESCOP").or_else(|| ds_hdr_str(&hdu, "OPTIC"));
    Ok((filter, camera, instrument))
}

fn ds_poststack_validate_tiff_master(path: &str) -> Result<(), String> {
    use tiff::decoder::Decoder;
    use tiff::tags::Tag;

    let file = std::fs::File::open(path).map_err(|error| format!("TIFF open: {error}"))?;
    let mut decoder =
        Decoder::new(std::io::BufReader::new(file)).map_err(|error| format!("TIFF: {error}"))?;
    let bits = match decoder
        .colortype()
        .map_err(|error| format!("TIFF color: {error}"))?
    {
        tiff::ColorType::Gray(bits)
        | tiff::ColorType::GrayA(bits)
        | tiff::ColorType::RGB(bits)
        | tiff::ColorType::RGBA(bits) => bits,
        tiff::ColorType::Palette(_) | tiff::ColorType::CMYK(_) => {
            return Err("TIFF con paleta/CMYK no es un máster científico lineal.".into())
        }
    };
    if bits < 16 {
        return Err(
            "TIFF de 8 bits no conserva rango científico suficiente; usa FITS/TIFF lineal de 16 o 32 bits."
                .into(),
        );
    }
    for tag in [Tag::ImageDescription, Tag::Software] {
        if decoder
            .get_tag_ascii_string(tag)
            .ok()
            .is_some_and(|value| ds_poststack_has_nonlinear_marker(&value))
        {
            return Err("La metadata TIFF declara una transformación no lineal/estirada.".into());
        }
    }
    Ok(())
}

fn ds_poststack_descriptor_for_file(
    path: &str,
    id: &str,
    image: &DsImage,
) -> Result<PostStackSourceDescriptor, String> {
    let file_name = std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string);
    if file_name
        .as_deref()
        .is_some_and(ds_poststack_has_nonlinear_marker)
    {
        return Err(
            "El nombre del archivo lo identifica como estirado/no lineal; abre el máster lineal."
                .into(),
        );
    }
    let lower = path.to_ascii_lowercase();
    let (filter, camera, instrument) = if lower.ends_with(".fits") || lower.ends_with(".fit") {
        ds_poststack_validate_fits_master(path, image)?
    } else if lower.ends_with(".tif") || lower.ends_with(".tiff") {
        ds_poststack_validate_tiff_master(path)?;
        (None, None, None)
    } else {
        return Err("El Studio autónomo sólo admite másteres FITS/TIFF lineales.".into());
    };
    if image.bayer.is_some() {
        return Err(
            "El editor autónomo no admite datos CFA/Bayer; integra y debayeriza primero.".into(),
        );
    }
    if !matches!(image.ch, 1 | 3)
        || image.w == 0
        || image.h == 0
        || image.data.len() != image.w.saturating_mul(image.h).saturating_mul(image.ch)
        || image.data.iter().any(|value| !value.is_finite())
    {
        return Err("El máster no tiene una geometría lineal mono/RGB válida.".into());
    }
    let (minimum, maximum) = image
        .data
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), value| {
            (lo.min(*value), hi.max(*value))
        });
    if !minimum.is_finite() || !maximum.is_finite() || maximum - minimum <= f32::EPSILON {
        return Err("El máster no contiene rango de señal medible.".into());
    }
    let filter_profile = filter
        .as_deref()
        .map(|value| ds_filter_token(value).unwrap_or(value).to_ascii_uppercase());
    let component_filters = filter_profile
        .as_deref()
        .map(ds_filter_components)
        .unwrap_or_default();
    let kind = if image.ch == 1 {
        PostStackSourceKind::Mono
    } else if component_filters.len() > 1 {
        PostStackSourceKind::DualBand
    } else {
        PostStackSourceKind::RgbBroadband
    };
    let routing_reason = match kind {
        PostStackSourceKind::Mono => {
            "Máster mono lineal importado: se ofrece como canal y no se inventa color.".into()
        }
        PostStackSourceKind::DualBand => {
            "Filtro dual-band reconocido: se habilita separación espectral y paletas.".into()
        }
        PostStackSourceKind::RgbBroadband => {
            "Máster RGB lineal importado: se habilita la ruta broadband/PCC.".into()
        }
        PostStackSourceKind::Unknown => "No se pudo clasificar la fuente.".into(),
    };
    Ok(PostStackSourceDescriptor {
        id: id.to_string(),
        origin: PostStackSourceOrigin::StandaloneMaster,
        path: Some(
            dunce::canonicalize(path)
                .unwrap_or_else(|_| std::path::PathBuf::from(path))
                .to_string_lossy()
                .to_string(),
        ),
        file_name,
        kind,
        channels: image.ch,
        linear: true,
        cfa: false,
        filter_profile,
        component_filters,
        camera,
        instrument,
        has_variance: false,
        has_dq: true,
        routing_reason,
    })
}

#[derive(Clone, Debug, PartialEq)]
enum DsPostStackEmbeddedWcs {
    Missing,
    Candidate(AstrometrySolution),
    Validated(AstrometrySolution),
    Rejected(String),
}

fn ds_poststack_header_dimension(hdu: &fitrs::Hdu, key: &str) -> Option<usize> {
    ds_hdr_num(hdu, key).and_then(|value| {
        let rounded = value.round();
        (value.is_finite()
            && rounded >= 1.0
            && (value - rounded).abs() <= 1.0e-6
            && rounded <= usize::MAX as f64)
            .then_some(rounded as usize)
    })
}

fn ds_poststack_embedded_wcs(
    path: &str,
    image_width: usize,
    image_height: usize,
) -> DsPostStackEmbeddedWcs {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "fit" | "fits") {
        return DsPostStackEmbeddedWcs::Missing;
    }
    let fits = match fitrs::Fits::open(path) {
        Ok(fits) => fits,
        Err(error) => {
            return DsPostStackEmbeddedWcs::Rejected(format!(
                "No se pudo inspeccionar la cabecera FITS: {error:?}"
            ));
        }
    };
    let Some(hdu) = fits.iter().next() else {
        return DsPostStackEmbeddedWcs::Rejected("FITS sin HDU primario".into());
    };
    if let Some(reason) = ds_hdr_str(&hdu, "ZASWCSIV") {
        return DsPostStackEmbeddedWcs::Rejected(format!(
            "La cabecera marca el WCS como inválido ({reason})"
        ));
    }

    let ctype1 = ds_hdr_str(&hdu, "CTYPE1");
    let ctype2 = ds_hdr_str(&hdu, "CTYPE2");
    let crval1 = ds_hdr_num(&hdu, "CRVAL1");
    let crval2 = ds_hdr_num(&hdu, "CRVAL2");
    let crpix1 = ds_hdr_num(&hdu, "CRPIX1");
    let crpix2 = ds_hdr_num(&hdu, "CRPIX2");
    let direct_cd = [
        ds_hdr_num(&hdu, "CD1_1"),
        ds_hdr_num(&hdu, "CD1_2"),
        ds_hdr_num(&hdu, "CD2_1"),
        ds_hdr_num(&hdu, "CD2_2"),
    ];
    let cdelt = [ds_hdr_num(&hdu, "CDELT1"), ds_hdr_num(&hdu, "CDELT2")];
    let pc = [
        ds_hdr_num(&hdu, "PC1_1"),
        ds_hdr_num(&hdu, "PC1_2"),
        ds_hdr_num(&hdu, "PC2_1"),
        ds_hdr_num(&hdu, "PC2_2"),
    ];
    let has_wcs = ctype1.is_some()
        || ctype2.is_some()
        || crval1.is_some()
        || crval2.is_some()
        || crpix1.is_some()
        || crpix2.is_some()
        || direct_cd.iter().any(Option::is_some)
        || cdelt.iter().any(Option::is_some)
        || pc.iter().any(Option::is_some);
    if !has_wcs {
        return DsPostStackEmbeddedWcs::Missing;
    }

    let (Some(header_width), Some(header_height)) = (
        ds_poststack_header_dimension(&hdu, "NAXIS1"),
        ds_poststack_header_dimension(&hdu, "NAXIS2"),
    ) else {
        return DsPostStackEmbeddedWcs::Rejected(
            "El WCS no puede ligarse a una geometría NAXIS1/NAXIS2 válida".into(),
        );
    };
    if header_width != image_width || header_height != image_height {
        return DsPostStackEmbeddedWcs::Rejected(format!(
            "El WCS pertenece a {header_width}×{header_height}, pero la imagen cargada es {image_width}×{image_height}"
        ));
    }

    let normalize_ctype = |value: Option<String>| {
        value.map(|value| value.trim().to_ascii_uppercase().replace(' ', ""))
    };
    if normalize_ctype(ctype1).as_deref() != Some("RA---TAN")
        || normalize_ctype(ctype2).as_deref() != Some("DEC--TAN")
    {
        return DsPostStackEmbeddedWcs::Rejected(
            "Sólo se importa una proyección CTYPE1=RA---TAN / CTYPE2=DEC--TAN sin distorsión no modelada"
                .into(),
        );
    }

    let (Some(crval1), Some(crval2), Some(crpix1), Some(crpix2)) = (crval1, crval2, crpix1, crpix2)
    else {
        return DsPostStackEmbeddedWcs::Rejected(
            "El WCS no contiene CRVAL1/2 y CRPIX1/2 completos".into(),
        );
    };
    if ![crval1, crval2, crpix1, crpix2]
        .iter()
        .all(|value| value.is_finite())
        || !(0.0..360.0).contains(&crval1)
        || !(-90.0..=90.0).contains(&crval2)
    {
        return DsPostStackEmbeddedWcs::Rejected(
            "CRVAL/CRPIX no son finitos o las coordenadas celestes están fuera de rango".into(),
        );
    }

    let matrix = if direct_cd.iter().all(Option::is_some) {
        [
            direct_cd[0].unwrap_or_default(),
            direct_cd[1].unwrap_or_default(),
            direct_cd[2].unwrap_or_default(),
            direct_cd[3].unwrap_or_default(),
        ]
    } else if direct_cd.iter().any(Option::is_some) {
        return DsPostStackEmbeddedWcs::Rejected(
            "La matriz CD está incompleta; se requieren CD1_1, CD1_2, CD2_1 y CD2_2".into(),
        );
    } else if let [Some(cdelt1), Some(cdelt2)] = cdelt {
        let pc11 = pc[0].unwrap_or(1.0);
        let pc12 = pc[1].unwrap_or(0.0);
        let pc21 = pc[2].unwrap_or(0.0);
        let pc22 = pc[3].unwrap_or(1.0);
        [cdelt1 * pc11, cdelt1 * pc12, cdelt2 * pc21, cdelt2 * pc22]
    } else {
        return DsPostStackEmbeddedWcs::Rejected(
            "El WCS necesita una matriz CD completa o PC combinada con CDELT1/2".into(),
        );
    };
    if !matrix.iter().all(|value| value.is_finite()) {
        return DsPostStackEmbeddedWcs::Rejected("La matriz WCS contiene NaN/Inf".into());
    }
    let determinant = matrix[0] * matrix[3] - matrix[1] * matrix[2];
    if !determinant.is_finite() || determinant.abs() <= 1.0e-18 {
        return DsPostStackEmbeddedWcs::Rejected(
            "La matriz WCS es singular y no puede proyectar coordenadas".into(),
        );
    }
    let Some(scale_arcsec_px) = spcc_cd_scale_arcsec_px(matrix[0], matrix[1], matrix[2], matrix[3])
    else {
        return DsPostStackEmbeddedWcs::Rejected(
            "La matriz WCS no produce una escala angular válida".into(),
        );
    };
    let mut solution = AstrometrySolution {
        ctype: "RA---TAN / DEC--TAN".into(),
        crval1,
        crval2,
        crpix1,
        crpix2,
        cd11: matrix[0],
        cd12: matrix[1],
        cd21: matrix[2],
        cd22: matrix[3],
        rms_px: ds_hdr_num(&hdu, "ZASWCSR")
            .filter(|value| value.is_finite() && *value >= 0.0)
            .unwrap_or(0.0) as f32,
        inliers: ds_hdr_num(&hdu, "ZASWCSI")
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|value| value.round() as usize)
            .unwrap_or(0),
        handedness: if determinant < 0.0 {
            "normal".into()
        } else {
            "mirrored".into()
        },
        scale_arcsec_px,
        source: "embeddedCandidate".into(),
        cached: true,
    };

    let Some(expected_fingerprint) = ds_hdr_str(&hdu, "ZASWCSFP") else {
        return DsPostStackEmbeddedWcs::Candidate(solution);
    };
    let actual_fingerprint = spcc_wcs_geometry_fingerprint(&solution, image_width, image_height);
    if !expected_fingerprint.eq_ignore_ascii_case(&actual_fingerprint) {
        return DsPostStackEmbeddedWcs::Rejected(format!(
            "El fingerprint WCS no coincide ({expected_fingerprint} ≠ {actual_fingerprint})"
        ));
    }
    solution.source = "embeddedFingerprintVerified".into();
    DsPostStackEmbeddedWcs::Validated(solution)
}

fn ds_poststack_set_embedded_wcs_status(result: &mut DeepSkyResult, state: &str, message: &str) {
    if let Some(recipe) = result.recipe.as_object_mut() {
        recipe.insert(
            "astrometryStatus".into(),
            serde_json::json!({
                "state": state,
                "message": message,
                "source": "embeddedFits",
                "onlineAvailable": true,
            }),
        );
    }
}

fn ds_poststack_apply_embedded_wcs(
    result: &mut DeepSkyResult,
    imported: DsPostStackEmbeddedWcs,
) -> Result<(), String> {
    match imported {
        DsPostStackEmbeddedWcs::Missing => {
            ds_poststack_set_embedded_wcs_status(
                result,
                "missing",
                "El archivo no contiene WCS. Puedes resolver astrometría localmente o con Gaia.",
            );
        }
        DsPostStackEmbeddedWcs::Rejected(reason) => {
            ds_poststack_set_embedded_wcs_status(
                result,
                "embeddedRejected",
                &format!("El WCS embebido no se usó: {reason}"),
            );
        }
        DsPostStackEmbeddedWcs::Candidate(solution) => {
            let mut candidate =
                serde_json::to_value(&solution).map_err(|error| error.to_string())?;
            if let Some(object) = candidate.as_object_mut() {
                object.insert("pixelWidth".into(), serde_json::json!(result.width));
                object.insert("pixelHeight".into(), serde_json::json!(result.height));
                object.insert(
                    "validationState".into(),
                    serde_json::json!("requiresStarValidation"),
                );
            }
            if let Some(recipe) = result.recipe.as_object_mut() {
                recipe.insert("wcsCandidate".into(), candidate);
            }
            ds_poststack_set_embedded_wcs_status(
                result,
                "candidate",
                "WCS FITS coherente, pero sin fingerprint Zenith. Se conservará sólo como semilla hasta validarlo contra estrellas.",
            );
        }
        DsPostStackEmbeddedWcs::Validated(solution) => {
            ds_poststack_set_operation(
                result,
                PostStackOperation::Astrometry {
                    solution: solution.clone(),
                },
            );
            ds_poststack_recompute(result)?;
            spcc_stamp_wcs_geometry(result);
            ds_poststack_set_embedded_wcs_status(
                result,
                "validatedEmbedded",
                "WCS FITS y fingerprint Zenith verificados; la solución quedó ligada a esta geometría.",
            );
        }
    }
    Ok(())
}

fn ds_poststack_result_from_master(
    id: String,
    image: DsImage,
    source: &PostStackSourceDescriptor,
) -> DeepSkyResult {
    let pixels = image.w * image.h;
    DeepSkyResult {
        id,
        data: image.data,
        width: image.w,
        height: image.h,
        channels: image.ch,
        coverage: vec![1.0; pixels],
        weight: vec![1.0; pixels],
        rejection_low: vec![0.0; pixels],
        rejection_high: vec![0.0; pixels],
        registration_residuals: vec![0.0; pixels],
        engine: "standalone_linear_master".into(),
        method: "immutable_import".into(),
        frames_used: 1,
        frames_rejected: 0,
        elapsed_seconds: 0.0,
        recipe: serde_json::json!({
            "schemaVersion": pipeline::DEEP_SKY_RECIPE_SCHEMA_VERSION,
            "operation": "standaloneLinearMaster",
            "postStackSource": source,
            "scientificProducts": {
                "SCI": {"present": true, "linear": true, "unit": "ADU"},
                "VAR": {"present": false, "missingReason": "El máster importado no incluye VAR"},
                "NEFF": {"present": false, "missingReason": "El máster importado no incluye NEFF"},
                "DQ": {"present": true, "origin": "importCoverageOnly"},
                "coverage": {"present": true, "origin": "fullImportedGeometry"}
            }
        }),
        variance: None,
        neff: None,
        dq: Some(vec![0; pixels]),
        struct_map: None,
        struct_residual: None,
        recoverability: None,
        source_data: None,
        source_variance: None,
        source_layout: None,
        post_stack_recipe: PostStackRecipe::default(),
        astrometry_solution: None,
    }
}

#[tauri::command]
fn deepsky_poststack_load_master(
    state: State<'_, AppState>,
    req: PostStackLoadMasterRequest,
) -> Result<PostStackLoadMasterResult, String> {
    let path = req.path.trim();
    if path.is_empty() {
        return Err("Selecciona un máster FITS/TIFF lineal.".into());
    }
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("No se puede abrir el máster seleccionado: {error}"))?;
    if !metadata.is_file() {
        return Err("La fuente del Studio debe ser un archivo, no una carpeta.".into());
    }
    let image = ds_read_image(path)?;
    let embedded_wcs = ds_poststack_embedded_wcs(path, image.w, image.h);
    let id = new_job_id("ds-studio-master");
    let source = ds_poststack_descriptor_for_file(path, &id, &image)?;
    let mut result = ds_poststack_result_from_master(id, image, &source);
    ds_poststack_apply_embedded_wcs(&mut result, embedded_wcs)?;

    {
        let mut guard = state
            .deep_sky_result
            .lock()
            .map_err(|_| "No se pudo abrir la sesión de edición")?;
        *guard = Some(result);
    }
    state
        .deep_sky_products
        .lock()
        .map_err(|_| "No se pudo limpiar la sesión anterior")?
        .clear();
    *state
        .deep_sky_active_product
        .lock()
        .map_err(|_| "No se pudo activar el máster importado")? = None;
    state.result_generation.fetch_add(1, Ordering::AcqRel);
    let state_result = ds_poststack_publish(&state)?;
    Ok(PostStackLoadMasterResult {
        source,
        state: state_result,
    })
}

#[tauri::command]
fn deepsky_poststack_apply_gradient(
    state: State<'_, AppState>,
    req: PostStackGradientRequest,
) -> Result<PostStackGradientResult, String> {
    let degree = req.degree.unwrap_or(2);
    if !(1..=4).contains(&degree) {
        return Err("El grado del modelo de fondo debe estar entre 1 y 4".into());
    }
    let sample_count = req.samples.iter().filter(|sample| sample.enabled).count();
    let (model_preview, protected_percent, split_half_ratio) = {
        let mut guard = state
            .deep_sky_result
            .lock()
            .map_err(|_| "No se pudo bloquear el máster lineal")?;
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        let (width, height, channels) = (result.width, result.height, result.channels);
        let source = ds_poststack_data_before_gradient(result)?;
        let mask = ds_poststack_exclusion_mask(
            &source,
            width,
            height,
            channels,
            &req,
            Some(&result.coverage),
            result.dq.as_deref(),
        );
        let protected_percent =
            100.0 * mask.iter().filter(|value| **value).count() as f32 / mask.len().max(1) as f32;
        let mut model = match req.mode {
            PostStackBackgroundMode::Auto => {
                crate::deepsky_background::fit_background_model_masked_degree(
                    &source,
                    width,
                    height,
                    channels,
                    Some(&mask),
                    degree,
                )
                .ok_or("No hay suficientes muestras de fondo sin proteger; reduce las máscaras")?
            }
            PostStackBackgroundMode::Samples => {
                crate::deepsky_background::fit_background_model_samples(
                    &source,
                    width,
                    height,
                    channels,
                    &req.samples,
                    degree,
                    Some(&mask),
                )?
            }
        };
        if !req.chromatic {
            ds_poststack_neutralize_model(&mut model);
        }
        let split_half_ratio = match req.mode {
            PostStackBackgroundMode::Auto => ds_poststack_split_half_ratio(
                &source,
                width,
                height,
                channels,
                &mask,
                req.chromatic,
                degree,
            ),
            PostStackBackgroundMode::Samples => ds_poststack_sample_split_half_ratio(
                &source,
                width,
                height,
                channels,
                &req.samples,
                degree,
                &mask,
                req.chromatic,
            ),
        };
        if let Some(error) =
            ds_poststack_gradient_stability_error(split_half_ratio, req.allow_unstable)
        {
            return Err(error);
        }
        let correction = model.render_correction();
        let model_preview = ds_poststack_diagnostic_preview(
            &correction,
            width,
            height,
            channels,
            "deepsky-gradient-model",
        )?;
        let contract = PostStackBackgroundModel {
            mode: req.mode,
            samples: if matches!(req.mode, PostStackBackgroundMode::Samples) {
                req.samples.clone()
            } else {
                Vec::new()
            },
            degree: model.degree,
            coeffs: model.coeffs,
            level: model.level,
            width: model.w,
            height: model.h,
            channels: model.ch,
            chromatic: req.chromatic,
            protected_percent,
            split_half_ratio,
        };
        ds_poststack_commit_operations(result, [PostStackOperation::Gradient { model: contract }])?;
        (model_preview, protected_percent, split_half_ratio)
    };
    let state_result = ds_poststack_publish(&state)?;
    let residual_preview = state_result.preview.clone();
    let overfit_warning = if !split_half_ratio.is_finite() {
        Some("No fue posible validar el modelo con mitades independientes.".to_string())
    } else if split_half_ratio > 0.2 {
        Some(format!(
            "El modelo no es estable entre mitades ({split_half_ratio:.2}σ > 0.20σ). Añade máscaras o no lo apliques."
        ))
    } else {
        None
    };
    Ok(PostStackGradientResult {
        state: state_result,
        model_preview,
        residual_preview,
        mode: req.mode,
        sample_count,
        degree,
        protected_percent,
        split_half_ratio,
        overfit_warning,
    })
}

fn ds_poststack_refresh_adaptive_stretches_after_crop(
    result: &mut DeepSkyResult,
) -> Result<(), String> {
    let stretches = result
        .post_stack_recipe
        .operations
        .iter()
        .enumerate()
        .filter_map(|(index, operation)| match operation {
            PostStackOperation::Stretch { target, stretch }
                if !matches!(
                    stretch.preset.trim().to_ascii_lowercase().as_str(),
                    "custom" | "manual" | "expert"
                ) =>
            {
                Some((index, *target, stretch.preset.clone(), stretch.linked))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let final_cursor = result.post_stack_recipe.operations.len();
    for (index, target, preset, linked) in stretches {
        // Medimos justo antes del propio estirado, sobre la geometría nueva.
        // Se usa el mismo resultado temporal para no duplicar varios másteres
        // float32 de decenas de megapíxeles.
        result.post_stack_recipe.cursor = index;
        let layers = ds_poststack_replay(result)?;
        let analysis = match target {
            PostStackLayerTarget::Combined => ds_poststack_adaptive_analysis_for(result),
            PostStackLayerTarget::Object => {
                let workspace =
                    layers.ok_or("El recorte perdió la capa Objeto antes de reanalizarla")?;
                ds_poststack_adaptive_analysis_for_data(&workspace.object, result.channels)
            }
            PostStackLayerTarget::Stars => {
                let workspace =
                    layers.ok_or("El recorte perdió la capa Estrellas antes de reanalizarla")?;
                ds_poststack_adaptive_analysis_for_data(&workspace.stars, result.channels)
            }
        };
        let mut stretch = analysis.suggested_stretch;
        stretch.preset = preset;
        stretch.linked = linked;
        ds_poststack_apply_stretch_preset(&mut stretch);
        result.post_stack_recipe.operations[index] =
            PostStackOperation::Stretch { target, stretch };
    }
    result.post_stack_recipe.cursor = final_cursor;
    Ok(())
}

fn ds_poststack_commit_crop(result: &mut DeepSkyResult, crop: PostStackCrop) -> Result<(), String> {
    let previous_recipe = result.post_stack_recipe.clone();
    ds_poststack_set_operation(result, PostStackOperation::Crop { crop });
    let outcome = (|| {
        ds_poststack_refresh_adaptive_stretches_after_crop(result)?;
        result.post_stack_recipe.cursor = result.post_stack_recipe.operations.len();
        ds_poststack_recompute(result)
    })();
    if let Err(error) = outcome {
        result.post_stack_recipe = previous_recipe;
        if let Err(restore_error) = ds_poststack_recompute(result) {
            return Err(format!(
                "{error}. Además no se pudo restaurar la revisión anterior: {restore_error}"
            ));
        }
        return Err(error);
    }
    Ok(())
}

#[tauri::command]
fn deepsky_poststack_apply_crop(
    state: State<'_, AppState>,
    req: PostStackCropRequest,
) -> Result<PostStackRevisionState, String> {
    {
        let mut guard = state
            .deep_sky_result
            .lock()
            .map_err(|_| "No se pudo bloquear el máster lineal")?;
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        ds_poststack_ensure_source(result);
        let layout = result
            .source_layout
            .as_ref()
            .ok_or("No se conservó la geometría fuente")?;
        let x0 = (req.x.clamp(0.0, 1.0) * layout.width as f32).floor() as usize;
        let y0 = (req.y.clamp(0.0, 1.0) * layout.height as f32).floor() as usize;
        let x1 = ((req.x + req.width).clamp(0.0, 1.0) * layout.width as f32).ceil() as usize;
        let y1 = ((req.y + req.height).clamp(0.0, 1.0) * layout.height as f32).ceil() as usize;
        let crop = PostStackCrop {
            x: x0.min(layout.width.saturating_sub(1)),
            y: y0.min(layout.height.saturating_sub(1)),
            width: x1.saturating_sub(x0).min(layout.width.saturating_sub(x0)),
            height: y1.saturating_sub(y0).min(layout.height.saturating_sub(y0)),
            source_width: layout.width,
            source_height: layout.height,
        };
        if crop.width < 16 || crop.height < 16 {
            return Err("El recorte debe conservar al menos 16×16 píxeles".into());
        }
        ds_poststack_commit_crop(result, crop)?;
    }
    ds_poststack_publish(&state)
}

fn ds_poststack_before_rank(result: &DeepSkyResult, rank: usize) -> DeepSkyResult {
    let mut working = result.clone();
    if let Some(index) = working
        .post_stack_recipe
        .operations
        .iter()
        .position(|operation| ds_poststack_operation_rank(operation) >= rank)
    {
        working.post_stack_recipe.cursor = index;
    }
    working
}

#[tauri::command]
fn deepsky_poststack_analyze(
    state: State<'_, AppState>,
) -> Result<PostStackAdaptiveAnalysis, String> {
    let guard = state
        .deep_sky_result
        .lock()
        .map_err(|_| "No se pudo bloquear el máster lineal")?;
    let result = guard
        .as_ref()
        .ok_or("No hay máster lineal de cielo profundo")?;
    let mut working = ds_poststack_before_rank(result, 70);
    ds_poststack_recompute(&mut working)?;
    Ok(ds_poststack_adaptive_analysis_for(&working))
}

#[tauri::command]
fn deepsky_poststack_analyze_psf(
    state: State<'_, AppState>,
) -> Result<PostStackPsfAnalysis, String> {
    let guard = state
        .deep_sky_result
        .lock()
        .map_err(|_| "No se pudo bloquear el máster lineal")?;
    let result = guard
        .as_ref()
        .ok_or("No hay máster lineal de cielo profundo")?;
    let mut working = result.clone();
    if let Some(index) = working
        .post_stack_recipe
        .operations
        .iter()
        .position(|operation| {
            matches!(
                operation,
                PostStackOperation::Stretch { .. }
                    | PostStackOperation::Curves { .. }
                    | PostStackOperation::Detail { .. }
                    | PostStackOperation::Finish { .. }
            )
        })
    {
        working.post_stack_recipe.cursor = index;
        ds_poststack_recompute(&mut working)?;
    }
    let psf = crate::deepsky_studio_layers::estimate_poststack_psf(
        &working.data,
        working.width,
        working.height,
        working.channels,
    );
    let eligible = psf.measured && psf.stars_used >= 6 && psf.confidence >= 0.18;
    let reason = if eligible {
        format!(
            "PSF medida con {} estrellas · FWHM {:.2}×{:.2} px",
            psf.stars_used, psf.fwhm_x, psf.fwhm_y
        )
    } else {
        "No hay suficientes estrellas válidas para una restauración automática segura; ajusta la PSF en Experto o conserva la imagen sin deconvolución.".into()
    };
    Ok(PostStackPsfAnalysis {
        psf,
        eligible,
        reason,
    })
}

#[tauri::command]
fn deepsky_poststack_apply_deconvolution(
    state: State<'_, AppState>,
    req: PostStackDeconvolutionRequest,
) -> Result<PostStackRevisionState, String> {
    {
        let mut guard = state
            .deep_sky_result
            .lock()
            .map_err(|_| "No se pudo bloquear el máster lineal")?;
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        let psf = req.psf.unwrap_or_else(|| {
            crate::deepsky_studio_layers::estimate_poststack_psf(
                &result.data,
                result.width,
                result.height,
                result.channels,
            )
        });
        if !psf.measured && !req.manual_confirmed {
            return Err(
                "No se pudo medir una PSF segura. Confirma una FWHM manual en Experto o continúa sin restauración."
                    .into(),
            );
        }
        if !psf.fwhm_x.is_finite()
            || !psf.fwhm_y.is_finite()
            || !(0.6..=15.0).contains(&psf.fwhm_x)
            || !(0.6..=15.0).contains(&psf.fwhm_y)
        {
            return Err("La FWHM de la PSF debe estar entre 0.6 y 15 píxeles".into());
        }
        if psf.measured && (psf.stars_used < 6 || psf.confidence < 0.18) {
            return Err(
                "La PSF automática no tiene suficientes estrellas o confianza. Mide una PSF manual en Experto o conserva la revisión sin deconvolución."
                    .into(),
            );
        }
        ds_poststack_commit_operations(
            result,
            [PostStackOperation::Deconvolution {
                target: req.target,
                psf,
                iterations: req.iterations.clamp(1, 32),
                regularization: req.regularization.clamp(0.0, 0.35),
                deringing: req.deringing.clamp(0.0, 1.0),
                flux_conservation: req.flux_conservation,
            }],
        )?;
    }
    ds_poststack_publish(&state)
}

#[tauri::command]
fn deepsky_poststack_separate_stars(
    state: State<'_, AppState>,
    req: PostStackStarSeparationRequest,
) -> Result<PostStackRevisionState, String> {
    {
        let mut guard = state
            .deep_sky_result
            .lock()
            .map_err(|_| "No se pudo bloquear el máster lineal")?;
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        if req.engine != "nativePsfMultiscale" {
            return Err(
                "El separador solicitado no está instalado. El motor nativo PSF multiescala sí está disponible."
                    .into(),
            );
        }
        ds_poststack_commit_operations(
            result,
            [PostStackOperation::StarSeparation {
                engine: req.engine,
                sensitivity: req.sensitivity.clamp(0.0, 1.0),
                scale: req.scale.clamp(0.65, 1.8),
                halo_protection: req.halo_protection.clamp(0.0, 1.0),
                faint_star_protection: req.faint_star_protection.clamp(0.0, 1.0),
                preserve_residual: req.preserve_residual,
            }],
        )?;
    }
    ds_poststack_publish(&state)
}

#[tauri::command]
fn deepsky_poststack_adjust_stars(
    state: State<'_, AppState>,
    req: PostStackStarAdjustmentRequest,
) -> Result<PostStackRevisionState, String> {
    {
        let mut guard = state
            .deep_sky_result
            .lock()
            .map_err(|_| "No se pudo bloquear el máster lineal")?;
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        ds_poststack_commit_operations(
            result,
            [PostStackOperation::StarAdjustment {
                reduction: req.reduction.clamp(0.0, 0.85),
                saturation: req.saturation.clamp(-1.0, 1.5),
                halo_suppression: req.halo_suppression.clamp(0.0, 1.0),
            }],
        )?;
    }
    ds_poststack_publish(&state)
}

#[tauri::command]
fn deepsky_poststack_recombine(
    state: State<'_, AppState>,
    req: PostStackRecombineRequest,
) -> Result<PostStackRevisionState, String> {
    {
        let mut guard = state
            .deep_sky_result
            .lock()
            .map_err(|_| "No se pudo bloquear el máster lineal")?;
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        let layers = ds_poststack_replay(result)?
            .ok_or("No existen capas Objeto/Estrellas para recombinar")?;
        if layers.object_nonlinear != layers.stars_nonlinear {
            return Err(
                "No se puede mezclar una rama lineal con otra no lineal. Iguala el estirado/acabado de Objeto y Estrellas o deshaz la operación pendiente."
                    .into(),
            );
        }
        ds_poststack_commit_operations(
            result,
            [PostStackOperation::Recombine {
                object_weight: req.object_weight.clamp(0.0, 2.0),
                star_weight: req.star_weight.clamp(0.0, 2.0),
                residual_weight: req.residual_weight.clamp(0.0, 2.0),
            }],
        )?;
    }
    ds_poststack_publish(&state)
}

#[tauri::command]
fn deepsky_poststack_layer_preview(
    state: State<'_, AppState>,
    target: String,
) -> Result<PostStackLayerPreviewResult, String> {
    let (preview, mask_preview, reconstruction_error, star_fraction, normalized_target) = {
        let mut guard = state
            .deep_sky_result
            .lock()
            .map_err(|_| "No se pudo bloquear el máster lineal")?;
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        let layers = ds_poststack_replay(result)?
            .ok_or("Crea las capas Objeto/Estrellas antes de abrir esta vista")?;
        let normalized = target.trim().to_ascii_lowercase();
        let (data, channels) = match normalized.as_str() {
            "object" | "starless" => (layers.object.as_slice(), result.channels),
            "stars" => (layers.stars.as_slice(), result.channels),
            "residual" => (layers.residual.as_slice(), result.channels),
            "mask" => (layers.mask.as_slice(), 1),
            "combined" => (result.data.as_slice(), result.channels),
            _ => return Err("Vista de capa desconocida".into()),
        };
        let preview = if normalized == "mask" {
            ds_poststack_diagnostic_preview(
                data,
                result.width,
                result.height,
                channels,
                "deepsky-star-mask",
            )?
        } else {
            ds_poststack_master_preview(
                data,
                result.width,
                result.height,
                channels,
                &format!("deepsky-layer-{normalized}"),
            )?
        };
        let mask_preview = if normalized == "mask" {
            None
        } else {
            Some(ds_poststack_diagnostic_preview(
                &layers.mask,
                result.width,
                result.height,
                1,
                "deepsky-star-mask",
            )?)
        };
        (
            preview,
            mask_preview,
            layers.reconstruction_error,
            layers.star_fraction,
            normalized,
        )
    };
    Ok(PostStackLayerPreviewResult {
        target: normalized_target,
        preview,
        mask_preview,
        reconstruction_error,
        star_fraction,
    })
}

#[tauri::command]
fn deepsky_poststack_apply_dualband(
    state: State<'_, AppState>,
    req: PostStackDualBandRequest,
) -> Result<PostStackRevisionState, String> {
    {
        let mut guard = state
            .deep_sky_result
            .lock()
            .map_err(|_| "No se pudo bloquear el máster lineal")?;
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        if let Some(reason) = ds_hoo_block_reason(&result.recipe) {
            return Err(reason);
        }
        ds_poststack_commit_operations(
            result,
            [PostStackOperation::DualBandPalette {
                palette: req.palette,
                profile: req.profile,
                oiii_green_weight: req.oiii_green_weight.clamp(0.0, 1.0),
                crosstalk_suppression: req.crosstalk_suppression.clamp(0.0, 1.0),
                neutralize: req.neutralize,
            }],
        )?;
    }
    ds_poststack_publish(&state)
}

#[tauri::command]
fn deepsky_poststack_apply_denoise(
    state: State<'_, AppState>,
    req: PostStackDenoiseRequest,
) -> Result<PostStackRevisionState, String> {
    {
        let mut guard = state
            .deep_sky_result
            .lock()
            .map_err(|_| "No se pudo bloquear el máster lineal")?;
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        ds_poststack_commit_operations(
            result,
            [PostStackOperation::Denoise {
                target: req.target,
                strength: req.strength.clamp(0.0, 1.0),
                detail_protection: req.detail_protection.clamp(0.0, 1.0),
                chroma_strength: req.chroma_strength.clamp(0.0, 1.0),
            }],
        )?;
    }
    ds_poststack_publish(&state)
}

#[tauri::command]
fn deepsky_poststack_apply_stretch(
    state: State<'_, AppState>,
    req: PostStackStretchRequest,
) -> Result<PostStackRevisionState, String> {
    {
        let mut guard = state
            .deep_sky_result
            .lock()
            .map_err(|_| "No se pudo bloquear el máster lineal")?;
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        let mut working = ds_poststack_before_rank(
            result,
            if req.target == PostStackLayerTarget::Combined {
                70
            } else {
                64
            },
        );
        let layers = ds_poststack_replay(&mut working)?;
        let analysis = match req.target {
            PostStackLayerTarget::Combined => ds_poststack_adaptive_analysis_for(&working),
            PostStackLayerTarget::Object => {
                let layers = layers.ok_or("Crea las capas antes de estirar la rama Objeto")?;
                ds_poststack_adaptive_analysis_for_data(&layers.object, working.channels)
            }
            PostStackLayerTarget::Stars => {
                let layers = layers.ok_or("Crea las capas antes de estirar la rama Estrellas")?;
                ds_poststack_adaptive_analysis_for_data(&layers.stars, working.channels)
            }
        };
        let mut stretch = analysis.suggested_stretch;
        stretch.preset = req.preset;
        stretch.linked = req.linked;
        // El preset adapta primero la propuesta automática a la clase de
        // imagen. Los controles explícitos del usuario se aplican después y
        // nunca quedan sobrescritos silenciosamente por el preset.
        ds_poststack_apply_stretch_preset(&mut stretch);
        if let Some(value) = req.stretch {
            stretch.stretch = value.clamp(0.001, 64.0);
        }
        if let Some(value) = req.symmetry {
            stretch.symmetry = value.clamp(0.005, 0.995);
        }
        if let Some(value) = req.local_intensity {
            stretch.local_intensity = value.clamp(0.0, 1.0);
        }
        if let Some(value) = req.black_point {
            stretch.black_point = value;
        }
        if let Some(value) = req.white_point {
            stretch.white_point = value.max(stretch.black_point + 1e-6);
        }
        ds_poststack_commit_operations(
            result,
            [PostStackOperation::Stretch {
                target: req.target,
                stretch,
            }],
        )?;
    }
    ds_poststack_publish(&state)
}

#[tauri::command]
fn deepsky_poststack_apply_curves(
    state: State<'_, AppState>,
    req: PostStackCurvesRequest,
) -> Result<PostStackRevisionState, String> {
    {
        let mut guard = state
            .deep_sky_result
            .lock()
            .map_err(|_| "No se pudo bloquear el máster lineal")?;
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        let active = result
            .post_stack_recipe
            .operations
            .iter()
            .take(result.post_stack_recipe.cursor)
            .collect::<Vec<_>>();
        if !active.iter().any(|operation| {
            matches!(
                operation,
                PostStackOperation::Stretch { target, .. } if *target == req.target
            )
        }) {
            return Err(format!(
                "Aplica primero el estirado de la rama {} antes de usar Curvas",
                req.target.as_str()
            ));
        }
        if req.target != PostStackLayerTarget::Combined
            && !active
                .iter()
                .any(|operation| matches!(operation, PostStackOperation::StarSeparation { .. }))
        {
            return Err("Crea las capas antes de aplicar curvas a Objeto o Estrellas".into());
        }
        ds_poststack_validate_curves(&req.curves, result.channels)?;
        ds_poststack_commit_operations(
            result,
            [PostStackOperation::Curves {
                target: req.target,
                curves: req.curves,
            }],
        )?;
    }
    ds_poststack_publish(&state)
}

#[tauri::command]
fn deepsky_poststack_apply_detail(
    state: State<'_, AppState>,
    req: PostStackDetailRequest,
) -> Result<PostStackRevisionState, String> {
    {
        let mut guard = state
            .deep_sky_result
            .lock()
            .map_err(|_| "No se pudo bloquear el máster lineal")?;
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        ds_poststack_commit_operations(
            result,
            [PostStackOperation::Detail {
                target: req.target,
                amount: req.amount.clamp(0.0, 1.5),
                radius: req.radius.clamp(1, 3),
                star_protection: req.star_protection.clamp(0.0, 1.0),
            }],
        )?;
    }
    ds_poststack_publish(&state)
}

#[tauri::command]
fn deepsky_poststack_apply_finish(
    state: State<'_, AppState>,
    req: PostStackFinishRequest,
) -> Result<PostStackRevisionState, String> {
    {
        let mut guard = state
            .deep_sky_result
            .lock()
            .map_err(|_| "No se pudo bloquear el máster lineal")?;
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        ds_poststack_commit_operations(
            result,
            [PostStackOperation::Finish {
                target: req.target,
                saturation: req.saturation.clamp(-1.0, 1.5),
                contrast: req.contrast.clamp(-0.5, 0.8),
                highlight_protection: req.highlight_protection.clamp(0.0, 1.0),
            }],
        )?;
    }
    ds_poststack_publish(&state)
}

#[tauri::command]
fn deepsky_poststack_state(state: State<'_, AppState>) -> Result<PostStackRevisionState, String> {
    ds_poststack_publish(&state)
}

#[tauri::command]
fn deepsky_poststack_undo(state: State<'_, AppState>) -> Result<PostStackRevisionState, String> {
    {
        // Undo/redo/reset son las rutas explícitas de recuperación. Además de
        // no entrar en pánico, limpian el poison para que el resto del editor
        // vuelva a aceptar operaciones después de restaurar un replay válido.
        let mut guard = ds_poststack_recover_mutex(&state.deep_sky_result);
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        if result.post_stack_recipe.cursor > 0 {
            ds_poststack_set_cursor_transactional(
                result,
                result.post_stack_recipe.cursor.saturating_sub(1),
            )?;
        }
    }
    ds_poststack_publish(&state)
}

#[tauri::command]
fn deepsky_poststack_redo(state: State<'_, AppState>) -> Result<PostStackRevisionState, String> {
    {
        let mut guard = ds_poststack_recover_mutex(&state.deep_sky_result);
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        if result.post_stack_recipe.cursor < result.post_stack_recipe.operations.len() {
            ds_poststack_set_cursor_transactional(result, result.post_stack_recipe.cursor + 1)?;
        }
    }
    ds_poststack_publish(&state)
}

#[tauri::command]
fn deepsky_poststack_reset(state: State<'_, AppState>) -> Result<PostStackRevisionState, String> {
    {
        let mut guard = ds_poststack_recover_mutex(&state.deep_sky_result);
        let result = guard
            .as_mut()
            .ok_or("No hay máster lineal de cielo profundo")?;
        ds_poststack_set_cursor_transactional(result, 0)?;
    }
    ds_poststack_publish(&state)
}

#[tauri::command]
fn deepsky_poststack_source_preview(state: State<'_, AppState>) -> Result<String, String> {
    let guard = state
        .deep_sky_result
        .lock()
        .map_err(|_| "No se pudo bloquear el máster lineal")?;
    let result = guard
        .as_ref()
        .ok_or("No hay máster lineal de cielo profundo")?;
    let (source, width, height) = ds_poststack_source_active_view(result)?;
    ds_poststack_master_preview(
        &source,
        width,
        height,
        result.channels,
        "deepsky-poststack-source",
    )
}

#[cfg(test)]
mod poststack_tests {
    use super::*;

    fn fixture() -> DeepSkyResult {
        DeepSkyResult {
            id: "poststack-fixture".into(),
            data: vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0],
            width: 2,
            height: 1,
            channels: 3,
            coverage: vec![1.0; 2],
            weight: vec![1.0; 2],
            rejection_low: vec![0.0; 2],
            rejection_high: vec![0.0; 2],
            registration_residuals: vec![0.0; 2],
            engine: "fixture".into(),
            method: "fixture".into(),
            frames_used: 10,
            frames_rejected: 0,
            elapsed_seconds: 0.0,
            recipe: serde_json::json!({}),
            variance: Some(vec![1.0; 6]),
            neff: None,
            dq: None,
            struct_map: None,
            struct_residual: None,
            recoverability: None,
            source_data: None,
            source_variance: None,
            source_layout: None,
            post_stack_recipe: PostStackRecipe::default(),
            astrometry_solution: None,
        }
    }

    fn mapped_fixture() -> DeepSkyResult {
        let mut result = fixture();
        result.width = 4;
        result.height = 3;
        result.data = (0..36).map(|value| value as f32).collect();
        result.variance = Some((100..136).map(|value| value as f32).collect());
        result.neff = Some((200..236).map(|value| value as f32).collect());
        result.coverage = (0..12).map(|value| value as f32).collect();
        result.weight = (20..32).map(|value| value as f32).collect();
        result.rejection_low = (40..52).map(|value| value as f32).collect();
        result.rejection_high = (60..72).map(|value| value as f32).collect();
        result.registration_residuals = (80..92).map(|value| value as f32).collect();
        result.dq = Some((100..112).map(|value| value as u32).collect());
        result.struct_map = Some((120..132).map(|value| value as f32).collect());
        result.struct_residual = Some((140..152).map(|value| value as f32).collect());
        result.recoverability = Some((160..172).map(|value| value as f32).collect());
        result
    }

    #[test]
    fn poststack_png_preview_is_area_averaged_and_bounded() {
        let (width, height, channels) = (3201usize, 9usize, 1usize);
        let data = (0..width * height)
            .map(|index| (index % 65536) as f32)
            .collect::<Vec<_>>();
        let (factor, preview_width, preview_height) = ds_poststack_preview_geometry(width, height);
        assert_eq!(factor, 3);
        assert_eq!((preview_width, preview_height), (1067, 3));
        assert!(preview_width.max(preview_height) <= DS_POSTSTACK_PREVIEW_MAX_EDGE);

        let path = ds_poststack_master_preview(
            &data,
            width,
            height,
            channels,
            "poststack-bounded-preview-test",
        )
        .unwrap();
        assert!(
            !path.starts_with("data:"),
            "el test local debe publicar por archivo y evitar base64"
        );
        let image = image::open(&path).unwrap();
        assert_eq!(
            (image.width() as usize, image.height() as usize),
            (preview_width, preview_height)
        );
        let _ = std::fs::remove_file(path);
    }

    fn astrometry_fixture() -> AstrometrySolution {
        AstrometrySolution {
            ctype: "RA---TAN / DEC--TAN".into(),
            crval1: 274.7,
            crval2: -13.8,
            crpix1: 100.0,
            crpix2: 80.0,
            cd11: -0.0003,
            cd12: 0.0,
            cd21: 0.0,
            cd22: 0.0003,
            rms_px: 0.42,
            inliers: 128,
            handedness: "normal".into(),
            scale_arcsec_px: 1.08,
            source: "localIndex".into(),
            cached: true,
        }
    }

    fn embedded_wcs_fixture_path(
        name: &str,
        solution: &AstrometrySolution,
        extra: Vec<(&'static str, String)>,
    ) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "zas-poststack-wcs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{name}.fits"));
        let mut metadata = vec![
            ("CTYPE1", "'RA---TAN'".into()),
            ("CTYPE2", "'DEC--TAN'".into()),
            ("CRVAL1", format!("{:.10}", solution.crval1)),
            ("CRVAL2", format!("{:.10}", solution.crval2)),
            ("CRPIX1", format!("{:.4}", solution.crpix1)),
            ("CRPIX2", format!("{:.4}", solution.crpix2)),
            ("CD1_1", format!("{:.10E}", solution.cd11)),
            ("CD1_2", format!("{:.10E}", solution.cd12)),
            ("CD2_1", format!("{:.10E}", solution.cd21)),
            ("CD2_2", format!("{:.10E}", solution.cd22)),
        ];
        metadata.extend(extra);
        ds_save_float32_fits(&path, &[1.0; 12], 4, 3, 1, &metadata).unwrap();
        path
    }

    #[test]
    fn embedded_wcs_with_matching_fingerprint_is_imported_and_persists_as_an_operation() {
        let mut solution = astrometry_fixture();
        solution.crpix1 = 2.5;
        solution.crpix2 = 2.0;
        let fingerprint = spcc_wcs_geometry_fingerprint(&solution, 4, 3);
        let path = embedded_wcs_fixture_path(
            "validated",
            &solution,
            vec![("ZASWCSFP", format!("'{fingerprint}'"))],
        );
        let imported = ds_poststack_embedded_wcs(path.to_str().unwrap(), 4, 3);
        let DsPostStackEmbeddedWcs::Validated(imported_solution) = imported else {
            panic!("el fingerprint correcto debe validar el WCS");
        };
        assert_eq!(imported_solution.source, "embeddedFingerprintVerified");
        assert!((imported_solution.scale_arcsec_px - 1.08).abs() < 1.0e-9);

        let mut result = mapped_fixture();
        ds_poststack_apply_embedded_wcs(
            &mut result,
            DsPostStackEmbeddedWcs::Validated(imported_solution.clone()),
        )
        .unwrap();
        assert_eq!(result.astrometry_solution, Some(imported_solution));
        assert_eq!(result.post_stack_recipe.operations.len(), 1);
        assert_eq!(
            result
                .recipe
                .pointer("/astrometryStatus/state")
                .and_then(serde_json::Value::as_str),
            Some("validatedEmbedded")
        );
        assert!(result.recipe.pointer("/wcs/geometryFingerprint").is_some());
        assert!(
            spcc_solution_from_recipe(&result.recipe).is_some(),
            "el fingerprint verificado debe sobrevivir una reapertura sin inventar RMS/inliers"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn embedded_wcs_without_fingerprint_remains_a_seed_not_a_solution() {
        let mut solution = astrometry_fixture();
        solution.crpix1 = 2.5;
        solution.crpix2 = 2.0;
        let path = embedded_wcs_fixture_path("candidate", &solution, Vec::new());
        let imported = ds_poststack_embedded_wcs(path.to_str().unwrap(), 4, 3);
        assert!(matches!(imported, DsPostStackEmbeddedWcs::Candidate(_)));

        let mut result = mapped_fixture();
        ds_poststack_apply_embedded_wcs(&mut result, imported).unwrap();
        assert!(result.astrometry_solution.is_none());
        assert!(result.recipe.get("wcs").is_none());
        assert_eq!(
            result
                .recipe
                .pointer("/wcsCandidate/validationState")
                .and_then(serde_json::Value::as_str),
            Some("requiresStarValidation")
        );
        assert!(spcc_seed_from_recipe(&result.recipe).is_some());
        assert!(spcc_solution_from_recipe(&result.recipe).is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn embedded_wcs_accepts_pc_cdelt_but_rejects_wrong_geometry_and_fingerprint() {
        let solution = AstrometrySolution {
            cd11: 0.0,
            cd12: -0.0003,
            cd21: 0.0003,
            cd22: 0.0,
            crpix1: 2.5,
            crpix2: 2.0,
            ..astrometry_fixture()
        };
        let path = embedded_wcs_fixture_path("pc-cdelt", &solution, Vec::new());
        // Reemplazar las tarjetas CD en un segundo fixture con PC*CDELT.
        let pc_path = path.parent().unwrap().join("pc-cdelt-only.fits");
        ds_save_float32_fits(
            &pc_path,
            &[1.0; 12],
            4,
            3,
            1,
            &[
                ("CTYPE1", "'RA---TAN'".into()),
                ("CTYPE2", "'DEC--TAN'".into()),
                ("CRVAL1", format!("{:.10}", solution.crval1)),
                ("CRVAL2", format!("{:.10}", solution.crval2)),
                ("CRPIX1", format!("{:.4}", solution.crpix1)),
                ("CRPIX2", format!("{:.4}", solution.crpix2)),
                ("CDELT1", "-0.0003".into()),
                ("CDELT2", "0.0003".into()),
                ("PC1_1", "0.0".into()),
                ("PC1_2", "1.0".into()),
                ("PC2_1", "1.0".into()),
                ("PC2_2", "0.0".into()),
            ],
        )
        .unwrap();
        let pc_import = ds_poststack_embedded_wcs(pc_path.to_str().unwrap(), 4, 3);
        let DsPostStackEmbeddedWcs::Candidate(pc_solution) = pc_import else {
            panic!("PC*CDELT válido sin fingerprint debe quedar candidato");
        };
        assert!((pc_solution.cd12 + 0.0003).abs() < 1.0e-12);
        assert!((pc_solution.cd21 - 0.0003).abs() < 1.0e-12);

        assert!(matches!(
            ds_poststack_embedded_wcs(pc_path.to_str().unwrap(), 3, 3),
            DsPostStackEmbeddedWcs::Rejected(reason) if reason.contains("4×3")
        ));

        let mismatched = embedded_wcs_fixture_path(
            "bad-fingerprint",
            &solution,
            vec![("ZASWCSFP", "'wcs-grid-0000000000000000'".into())],
        );
        assert!(matches!(
            ds_poststack_embedded_wcs(mismatched.to_str().unwrap(), 4, 3),
            DsPostStackEmbeddedWcs::Rejected(reason) if reason.contains("fingerprint")
        ));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let _ = std::fs::remove_dir_all(mismatched.parent().unwrap());
    }

    #[test]
    fn pcc_replacement_is_idempotent_and_variance_uses_gain_squared() {
        let mut result = fixture();
        let operation = PostStackOperation::GaiaPcc {
            gain_r: 2.0,
            gain_g: 1.0,
            gain_b: 0.5,
            reference: "averageSpiral".into(),
            matched_stars: 42,
            rms_px: 0.4,
        };
        ds_poststack_set_operation(&mut result, operation.clone());
        ds_poststack_recompute(&mut result).unwrap();
        let once = result.data.clone();
        ds_poststack_set_operation(&mut result, operation);
        ds_poststack_recompute(&mut result).unwrap();
        assert_eq!(
            result.data, once,
            "PCC repetida no puede acumular ganancias"
        );
        assert_eq!(result.post_stack_recipe.operations.len(), 1);
        assert_eq!(result.variance.as_ref().unwrap()[0], 4.0);
        assert_eq!(result.variance.as_ref().unwrap()[2], 0.25);
    }

    #[test]
    fn gradient_refit_reads_the_immutable_master_not_the_previous_residual() {
        let mut result = fixture();
        let source = result.data.clone();
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Gradient {
                model: PostStackBackgroundModel {
                    mode: PostStackBackgroundMode::Auto,
                    samples: Vec::new(),
                    degree: 1,
                    coeffs: vec![vec![0.0, 10.0, 0.0]; 3],
                    level: vec![0.0; 3],
                    width: 2,
                    height: 1,
                    channels: 3,
                    chromatic: true,
                    protected_percent: 0.0,
                    split_half_ratio: 0.0,
                },
            },
        );
        ds_poststack_recompute(&mut result).unwrap();
        assert_ne!(
            result.data, source,
            "la revisión de prueba debe producir un residual distinto"
        );
        assert_eq!(
            ds_poststack_data_before_gradient(&mut result).unwrap(),
            source.as_slice(),
            "recalcular gradiente sobre el residual anterior restauraría el gradiente original"
        );
    }

    #[test]
    fn undo_and_redo_rebuild_exactly_from_the_immutable_source() {
        let mut result = fixture();
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::GaiaPcc {
                gain_r: 2.0,
                gain_g: 1.0,
                gain_b: 0.5,
                reference: "g2v".into(),
                matched_stars: 20,
                rms_px: 0.3,
            },
        );
        ds_poststack_recompute(&mut result).unwrap();
        let edited = result.data.clone();
        result.post_stack_recipe.cursor = 0;
        ds_poststack_recompute(&mut result).unwrap();
        assert_eq!(result.data, vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0]);
        result.post_stack_recipe.cursor = 1;
        ds_poststack_recompute(&mut result).unwrap();
        assert_eq!(result.data, edited);
    }

    #[test]
    fn crop_uses_one_exact_window_for_science_and_every_diagnostic_map() {
        let mut result = mapped_fixture();
        let original_data = result.data.clone();
        let original_dq = result.dq.clone();
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Crop {
                crop: PostStackCrop {
                    x: 1,
                    y: 1,
                    width: 2,
                    height: 2,
                    source_width: 4,
                    source_height: 3,
                },
            },
        );
        ds_poststack_recompute(&mut result).unwrap();
        assert_eq!((result.width, result.height), (2, 2));
        assert_eq!(
            result.data,
            vec![15.0, 16.0, 17.0, 18.0, 19.0, 20.0, 27.0, 28.0, 29.0, 30.0, 31.0, 32.0]
        );
        assert_eq!(result.coverage, vec![5.0, 6.0, 9.0, 10.0]);
        assert_eq!(result.dq, Some(vec![105, 106, 109, 110]));
        assert_eq!(result.struct_map, Some(vec![125.0, 126.0, 129.0, 130.0]));

        result.post_stack_recipe.cursor = 0;
        ds_poststack_recompute(&mut result).unwrap();
        assert_eq!((result.width, result.height), (4, 3));
        assert_eq!(
            result.data, original_data,
            "deshacer restaura SCI bit a bit"
        );
        assert_eq!(result.dq, original_dq, "deshacer restaura DQ bit a bit");
    }

    #[test]
    fn astrometry_only_does_not_duplicate_or_modify_the_master_pixels() {
        let mut result = fixture();
        let source = result.data.clone();
        let solution = astrometry_fixture();
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Astrometry {
                solution: solution.clone(),
            },
        );
        ds_poststack_recompute(&mut result).unwrap();
        assert!(
            result.source_data.is_none(),
            "WCS por sí solo no debe clonar un máster potencialmente enorme"
        );
        assert_eq!(result.data, source);
        assert_eq!(result.astrometry_solution, Some(solution));
    }

    #[test]
    fn generalized_stretch_is_monotonic_and_keeps_the_linear_master_immutable() {
        let mut result = fixture();
        let source = result.data.clone();
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Stretch {
                target: PostStackLayerTarget::Combined,
                stretch: PostStackStretch {
                    preset: "autoNatural".into(),
                    stretch: 6.0,
                    symmetry: 0.12,
                    local_intensity: 0.35,
                    black_point: 0.0,
                    white_point: 100.0,
                    linked: true,
                },
            },
        );
        ds_poststack_recompute(&mut result).unwrap();
        assert!(result.data.iter().all(|value| value.is_finite()));
        assert!(
            result.data.windows(2).all(|pair| pair[0] <= pair[1]),
            "la transferencia debe conservar el orden tonal"
        );
        assert_eq!(result.source_data, Some(source));
        assert!(
            result.variance.is_none(),
            "la revisión no lineal no publica VAR"
        );
    }

    fn identity_curve() -> Vec<PostStackCurvePoint> {
        vec![PostStackCurvePoint(0.0, 0.0), PostStackCurvePoint(1.0, 1.0)]
    }

    fn contrast_curve() -> Vec<PostStackCurvePoint> {
        vec![
            PostStackCurvePoint(0.0, 0.0),
            PostStackCurvePoint(0.5, 0.36),
            PostStackCurvePoint(1.0, 1.0),
        ]
    }

    #[test]
    fn curves_identity_is_bit_exact_and_preserves_nan() {
        let mut result = fixture();
        result.data[2] = f32::NAN;
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Stretch {
                target: PostStackLayerTarget::Combined,
                stretch: PostStackStretch {
                    preset: "autoNatural".into(),
                    stretch: 4.0,
                    symmetry: 0.1,
                    local_intensity: 0.3,
                    black_point: 0.0,
                    white_point: 100.0,
                    linked: true,
                },
            },
        );
        ds_poststack_recompute(&mut result).unwrap();
        let stretched = result.data.clone();
        let identity = PostStackCurves {
            master: identity_curve(),
            luminance: identity_curve(),
            red: identity_curve(),
            green: identity_curve(),
            blue: identity_curve(),
            saturation: identity_curve(),
            ..PostStackCurves::default()
        };
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Curves {
                target: PostStackLayerTarget::Combined,
                curves: identity,
            },
        );
        ds_poststack_recompute(&mut result).unwrap();
        for (actual, expected) in result.data.iter().zip(stretched.iter()) {
            if expected.is_nan() {
                assert!(actual.is_nan());
            } else {
                assert_eq!(actual.to_bits(), expected.to_bits());
            }
        }
        assert_eq!(
            result
                .post_stack_recipe
                .operations
                .iter()
                .filter(|operation| matches!(operation, PostStackOperation::Curves { .. }))
                .count(),
            1,
            "Curves es singleton por rama"
        );
    }

    #[test]
    fn mono_curves_accept_only_master_and_luminance() {
        let mut mono = vec![0.0, 16_000.0, f32::NAN, 65_535.0];
        let safe = PostStackCurves {
            master: contrast_curve(),
            luminance: identity_curve(),
            ..PostStackCurves::default()
        };
        ds_poststack_apply_curves(&mut mono, 1, &safe).unwrap();
        assert!(mono[2].is_nan());

        let invalid = PostStackCurves {
            red: contrast_curve(),
            ..PostStackCurves::default()
        };
        assert!(ds_poststack_apply_curves(&mut mono, 1, &invalid)
            .unwrap_err()
            .contains("mono"));
    }

    #[test]
    fn selective_hsl_changes_only_its_hue_zone_and_preserves_luminance() {
        let mut data = vec![
            40_000.0, 10_000.0, 10_000.0, // rojo
            10_000.0, 10_000.0, 40_000.0, // azul
        ];
        let before = data.clone();
        let curves = PostStackCurves {
            selective: vec![PostStackSelectiveHsl {
                name: "red".into(),
                center: 0.0,
                width: 45.0,
                hue_shift: 0.0,
                saturation: 0.5,
                lightness: 0.0,
            }],
            ..PostStackCurves::default()
        };
        ds_poststack_apply_curves(&mut data, 3, &curves).unwrap();
        assert_ne!(&data[..3], &before[..3]);
        for channel in 3..6 {
            assert!(
                (data[channel] - before[channel]).abs() < 0.02,
                "el rango rojo no debe teñir azul"
            );
        }
        let luma = |pixel: &[f32]| 0.2126 * pixel[0] + 0.7152 * pixel[1] + 0.0722 * pixel[2];
        assert!(
            (luma(&data[..3]) - luma(&before[..3])).abs() < 0.02,
            "saturación selectiva debe conservar Y"
        );
    }

    #[test]
    fn luminance_curve_lifts_shadows_multiplicatively_preserving_color_ratio() {
        // C4a: la curva L aditiva colapsaba el ratio R:G:B de las sombras
        // (subir +0.05 sobre (0.02,0.01,0.15) convertía 15:1 en ~3:1). El
        // escalado multiplicativo (r,g,b) *= L(Y)/Y conserva la cromaticidad.
        let mut data = vec![0.02 * 65_535.0, 0.01 * 65_535.0, 0.15 * 65_535.0];
        let before = data.clone();
        let curves = PostStackCurves {
            luminance: vec![
                PostStackCurvePoint(0.0, 0.0),
                PostStackCurvePoint(0.05, 0.10),
                PostStackCurvePoint(1.0, 1.0),
            ],
            ..PostStackCurves::default()
        };
        ds_poststack_apply_curves(&mut data, 3, &curves).unwrap();
        assert!(
            data.iter().zip(before.iter()).all(|(after, b)| after > b),
            "la subida de sombras debe elevar los tres canales"
        );
        for (i, j) in [(0usize, 1usize), (0, 2), (1, 2)] {
            let ratio_before = before[i] / before[j];
            let ratio_after = data[i] / data[j];
            assert!(
                (ratio_after / ratio_before - 1.0).abs() < 0.01,
                "ratio {i}:{j} colapsado: antes {ratio_before}, después {ratio_after}"
            );
        }
    }

    #[test]
    fn luminance_identity_curve_is_bitexact_noop() {
        // C4a: identidad explícita en L -> no-op bit-exacto, incluidos los
        // valores firmados y por encima del blanco nominal que la receta
        // conserva sin clipping.
        let mut data = vec![-120.0, 0.0, 40_000.0, 1_500.0, 70_000.0, 2.5];
        let before = data.clone();
        let curves = PostStackCurves {
            luminance: identity_curve(),
            ..PostStackCurves::default()
        };
        ds_poststack_apply_curves(&mut data, 3, &curves).unwrap();
        for (actual, expected) in data.iter().zip(before.iter()) {
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
    }

    #[test]
    fn saturation_identity_curve_is_bitexact_noop() {
        // C4b: identidad explícita en la curva de saturación -> no-op
        // bit-exacto también con RGB firmado y altas luces sin recortar.
        let mut data = vec![-120.0, 0.0, 40_000.0, 1_500.0, 70_000.0, 2.5];
        let before = data.clone();
        let curves = PostStackCurves {
            saturation: identity_curve(),
            ..PostStackCurves::default()
        };
        ds_poststack_apply_curves(&mut data, 3, &curves).unwrap();
        for (actual, expected) in data.iter().zip(before.iter()) {
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
    }

    #[test]
    fn saturation_curve_doubling_midtones_doubles_ycbcr_chroma() {
        // C4b: la curva se mide y se aplica en YCbCr. Un píxel de tono medio
        // con s = c/|Y| = 0.2 mapeado a 0.4 debe salir con croma exactamente
        // duplicado e Y intacta (nada de mezclar la s de HSV con croma YCbCr).
        let y = 0.5f32;
        let cr = 0.1f32;
        let cb = 0.0f32;
        let r = y + 2.0 * (1.0 - 0.2126) * cr;
        let b = y + 2.0 * (1.0 - 0.0722) * cb;
        let g = (y - 0.2126 * r - 0.0722 * b) / 0.7152;
        let mut data = vec![r * 65_535.0, g * 65_535.0, b * 65_535.0];
        let curves = PostStackCurves {
            saturation: vec![
                PostStackCurvePoint(0.0, 0.0),
                PostStackCurvePoint(0.5, 1.0),
                PostStackCurvePoint(1.0, 1.0),
            ],
            ..PostStackCurves::default()
        };
        ds_poststack_apply_curves(&mut data, 3, &curves).unwrap();
        let r_after = data[0] / 65_535.0;
        let g_after = data[1] / 65_535.0;
        let b_after = data[2] / 65_535.0;
        let luma_after = 0.2126 * r_after + 0.7152 * g_after + 0.0722 * b_after;
        let cb_after = (b_after - luma_after) / (2.0 * (1.0 - 0.0722));
        let cr_after = (r_after - luma_after) / (2.0 * (1.0 - 0.2126));
        let chroma_after = (cb_after * cb_after + cr_after * cr_after).sqrt();
        assert!(
            (chroma_after / 0.1 - 2.0).abs() < 1e-3,
            "croma esperado x2 (0.2), obtenido {chroma_after}"
        );
        assert!(
            (luma_after - y).abs() < 1e-4,
            "la curva de saturación no debe mover Y: {luma_after}"
        );
    }

    #[test]
    fn saturation_curve_near_black_pixel_has_bounded_factor() {
        // C4b: en un píxel casi negro el cociente s'/s ingenuo daría ~x10;
        // el factor acotado a [0,4] evita que el croma residual de ruido
        // explote. El píxel debe quedar finito, casi negro y con croma <= x4.
        let r = 1.1e-5f32;
        let g = 1.0e-5f32;
        let b = 1.0e-5f32;
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let cb = (b - luma) / (2.0 * (1.0 - 0.0722));
        let cr = (r - luma) / (2.0 * (1.0 - 0.2126));
        let chroma_before = (cb * cb + cr * cr).sqrt();
        let mut data = vec![r * 65_535.0, g * 65_535.0, b * 65_535.0];
        let curves = PostStackCurves {
            // Levantón brutal de saturación en s bajas: en s ~ 0.05 el
            // cociente sin acotar sería ~10.
            saturation: vec![
                PostStackCurvePoint(0.0, 0.0),
                PostStackCurvePoint(0.01, 0.5),
                PostStackCurvePoint(1.0, 1.0),
            ],
            ..PostStackCurves::default()
        };
        ds_poststack_apply_curves(&mut data, 3, &curves).unwrap();
        assert!(data.iter().all(|value| value.is_finite()));
        assert!(
            data.iter().all(|value| value.abs() < 2.0),
            "un píxel casi negro debe seguir casi negro: {data:?}"
        );
        let r_after = data[0] / 65_535.0;
        let g_after = data[1] / 65_535.0;
        let b_after = data[2] / 65_535.0;
        let luma_after = 0.2126 * r_after + 0.7152 * g_after + 0.0722 * b_after;
        let cb_after = (b_after - luma_after) / (2.0 * (1.0 - 0.0722));
        let cr_after = (r_after - luma_after) / (2.0 * (1.0 - 0.2126));
        let chroma_after = (cb_after * cb_after + cr_after * cr_after).sqrt();
        let factor = chroma_after / chroma_before;
        assert!(
            (factor - 4.0).abs() < 0.1,
            "el clamp [0,4] debe haber actuado (ingenuo ~10): factor {factor}"
        );
    }

    #[test]
    fn curves_request_uses_the_flat_camel_case_contract() {
        let request: PostStackCurvesRequest = serde_json::from_value(serde_json::json!({
            "target": "object",
            "master": [[0.0, 0.0], [1.0, 1.0]],
            "luminance": [],
            "red": [],
            "green": [],
            "blue": [],
            "saturation": [],
            "hueShift": 5.0,
            "saturationScale": 1.1,
            "lightness": 0.0,
            "selective": [{
                "name": "cyan",
                "center": 180.0,
                "width": 45.0,
                "hueShift": 0.0,
                "saturation": 0.2,
                "lightness": 0.0
            }]
        }))
        .unwrap();
        assert_eq!(request.target, PostStackLayerTarget::Object);
        assert_eq!(request.curves.master, identity_curve());
        assert_eq!(request.curves.saturation_scale, 1.1);
        assert_eq!(request.curves.selective[0].name, "cyan");
    }

    #[test]
    fn legacy_gradient_request_keeps_safe_new_defaults() {
        let request: PostStackGradientRequest = serde_json::from_value(serde_json::json!({
            "protectExtendedObjects": true,
            "chromatic": false,
            "sensitivity": 0.7,
            "exclusionRects": []
        }))
        .unwrap();
        assert_eq!(request.mode, PostStackBackgroundMode::Auto);
        assert_eq!(request.degree, None);
        assert!(request.samples.is_empty());
        assert!(!request.allow_unstable);
    }

    #[test]
    fn gradient_mask_excludes_no_coverage_and_fatal_dq() {
        let request: PostStackGradientRequest =
            serde_json::from_value(serde_json::json!({"protectExtendedObjects": true})).unwrap();
        let data = vec![10.0f32; 16];
        let mut coverage = vec![1.0f32; 16];
        coverage[3] = 0.0;
        let mut dq = vec![0u32; 16];
        dq[7] = crate::deepsky_variance::dq::NONLINEAR;
        dq[8] = crate::deepsky_variance::dq::INTERPOLATED;
        let mask =
            ds_poststack_exclusion_mask(&data, 4, 4, 1, &request, Some(&coverage), Some(&dq));
        assert!(mask[3]);
        assert!(mask[7]);
        assert!(
            !mask[8],
            "interpolación válida por sí sola no elimina fondo medible"
        );
    }

    #[test]
    fn ghs_is_strictly_monotonic_and_does_not_flatten_star_cores() {
        let stretch = PostStackStretch {
            preset: "autoNatural".into(),
            stretch: 6.0,
            symmetry: 0.12,
            local_intensity: 0.35,
            black_point: 0.0,
            white_point: 100.0,
            linked: true,
        };
        // Monotonicidad estricta a lo largo de todo el dominio, incluidos los
        // valores firmados bajo el punto negro y el tramo por encima del punto
        // blanco (antes: clamps planos a 0 y 65535, respectivamente).
        let samples: Vec<f32> = (-200..=400).map(|i| i as f32 * 0.5).collect();
        let mut previous = f32::MIN;
        for &value in &samples {
            let out = ds_poststack_ghs_value(value, &stretch);
            assert!(
                out > previous,
                "GHS no monótono en {value}: {out} <= {previous}"
            );
            previous = out;
        }
        // Dos núcleos estelares por encima del punto blanco deben conservar
        // separación; el histórico los mandaba a 65535 idéntico.
        let core_a = ds_poststack_ghs_value(120.0, &stretch);
        let core_b = ds_poststack_ghs_value(150.0, &stretch);
        assert!(core_a > 65_535.0 && core_b > core_a);
        let shadow_a = ds_poststack_ghs_value(-20.0, &stretch);
        let shadow_b = ds_poststack_ghs_value(-10.0, &stretch);
        assert!(shadow_a < shadow_b && shadow_b < 0.0);
        // Continuidad en el punto blanco (sin salto entre curva y extensión).
        let at_white = ds_poststack_ghs_value(100.0, &stretch);
        let just_above = ds_poststack_ghs_value(100.001, &stretch);
        assert!((at_white - 65_535.0).abs() < 1.0);
        assert!(just_above - at_white < 8.0);
    }

    #[test]
    fn gradient_mask_survives_inverted_exclusion_rects() {
        let request: PostStackGradientRequest = serde_json::from_value(serde_json::json!({
            "protectExtendedObjects": false,
            "exclusionRects": [
                {"x": 0.75, "y": 0.75, "width": -0.5, "height": -0.5}
            ]
        }))
        .unwrap();
        let data = vec![10.0f32; 16];
        // Antes: el ancho negativo invertía el slice y el comando entraba en
        // pánico con el lock del resultado tomado.
        let mask = ds_poststack_exclusion_mask(&data, 4, 4, 1, &request, None, None);
        // El rect invertido equivale a (0.25..0.75)^2 y se rasteriza igual.
        assert!(mask[4 + 1] && mask[2 * 4 + 2]);
        assert!(!mask[0] && !mask[15]);
    }

    #[test]
    fn gradient_mask_ignores_degenerate_and_non_finite_rects_without_panicking() {
        let mut request: PostStackGradientRequest = serde_json::from_value(serde_json::json!({
            "protectExtendedObjects": false,
            "exclusionRects": []
        }))
        .unwrap();
        request.exclusion_rects = vec![
            PostStackNormalizedRect {
                x: 0.5,
                y: 0.5,
                width: 0.0,
                height: 0.4,
            },
            PostStackNormalizedRect {
                x: f32::NAN,
                y: 0.0,
                width: 0.5,
                height: 0.5,
            },
            PostStackNormalizedRect {
                x: 0.0,
                y: 0.0,
                width: f32::INFINITY,
                height: 0.5,
            },
        ];
        let data = vec![10.0f32; 16];
        let mask = ds_poststack_exclusion_mask(&data, 4, 4, 1, &request, None, None);
        assert!(mask.iter().all(|excluded| !excluded));
    }

    #[test]
    fn poststack_mutex_recovery_clears_poison_for_followup_commands() {
        let mutex = Mutex::new(7usize);
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = mutex.lock().unwrap();
            *guard = 11;
            panic!("fallo simulado dentro del comando post-stack");
        }));
        assert!(poisoned.is_err());
        assert!(mutex.is_poisoned());
        {
            let mut guard = ds_poststack_recover_mutex(&mutex);
            assert_eq!(*guard, 11);
            *guard = 13;
        }
        assert!(!mutex.is_poisoned());
        assert_eq!(*mutex.lock().unwrap(), 13);
    }

    #[test]
    fn gradient_mask_fails_closed_for_incoherent_support_geometry() {
        let request: PostStackGradientRequest =
            serde_json::from_value(serde_json::json!({"protectExtendedObjects": true})).unwrap();
        let data = vec![10.0f32; 16];
        let short_coverage = vec![1.0f32; 15];
        let short_dq = vec![0u32; 15];
        let coverage_mask =
            ds_poststack_exclusion_mask(&data, 4, 4, 1, &request, Some(&short_coverage), None);
        let dq_mask = ds_poststack_exclusion_mask(&data, 4, 4, 1, &request, None, Some(&short_dq));
        assert!(coverage_mask.iter().all(|excluded| *excluded));
        assert!(dq_mask.iter().all(|excluded| *excluded));
    }

    #[test]
    fn sample_mode_fits_real_background_points_instead_of_rectangles() {
        let (width, height) = (128usize, 128usize);
        let data = (0..width * height)
            .map(|pixel| {
                let x = (pixel % width) as f32 / width as f32;
                let y = (pixel / width) as f32 / height as f32;
                let noise = ((pixel * 37 % 17) as f32 - 8.0) * 1.5;
                1000.0 + 180.0 * x - 90.0 * y + 25.0 * x * y + noise
            })
            .collect::<Vec<_>>();
        let samples = (0..8)
            .flat_map(|row| {
                (0..8).map(move |column| PostStackBackgroundSample {
                    x: 0.06 + column as f32 * 0.125,
                    y: 0.06 + row as f32 * 0.125,
                    radius: 0.018,
                    enabled: true,
                    weight: 1.0,
                })
            })
            .collect::<Vec<_>>();
        let model = crate::deepsky_background::fit_background_model_samples(
            &data, width, height, 1, &samples, 2, None,
        )
        .unwrap();
        let measured_delta = model.eval(0, 0.9, 0.1) - model.eval(0, 0.1, 0.9);
        let expected_delta = (180.0 * 0.9 - 90.0 * 0.1 + 25.0 * 0.9 * 0.1)
            - (180.0 * 0.1 - 90.0 * 0.9 + 25.0 * 0.1 * 0.9);
        assert!((measured_delta - expected_delta).abs() < 5.0);
        let split_ratio = ds_poststack_sample_split_half_ratio(
            &data,
            width,
            height,
            1,
            &samples,
            2,
            &vec![false; width * height],
            true,
        );
        assert!(
            split_ratio.is_finite() && split_ratio <= 0.2,
            "un plano bien muestreado debe superar el gate split-half: {split_ratio}"
        );
    }

    #[test]
    fn unstable_gradient_fails_closed_without_explicit_override() {
        assert!(ds_poststack_gradient_stability_error(0.21, false).is_some());
        assert!(ds_poststack_gradient_stability_error(f32::INFINITY, false).is_some());
        assert!(ds_poststack_gradient_stability_error(0.21, true).is_none());
        assert!(ds_poststack_gradient_stability_error(0.19, false).is_none());
    }

    fn layer_fixture() -> DeepSkyResult {
        let (width, height) = (32usize, 32usize);
        let mut result = fixture();
        result.width = width;
        result.height = height;
        result.channels = 1;
        result.data = (0..width * height)
            .map(|index| {
                let x = (index % width) as f32;
                let y = (index / width) as f32;
                let r2 = (x - 15.5).powi(2) + (y - 15.5).powi(2);
                -3.0 + 900.0 * (-r2 / (2.0 * 1.6f32.powi(2))).exp()
            })
            .collect();
        result.variance = Some(vec![1.0; width * height]);
        result.coverage = vec![1.0; width * height];
        result.weight = vec![1.0; width * height];
        result.rejection_low = vec![0.0; width * height];
        result.rejection_high = vec![0.0; width * height];
        result.registration_residuals = vec![0.0; width * height];
        result
    }

    fn layer_separation() -> PostStackOperation {
        PostStackOperation::StarSeparation {
            engine: "nativePsfMultiscale".into(),
            sensitivity: 0.6,
            scale: 1.0,
            halo_protection: 0.75,
            faint_star_protection: 0.6,
            preserve_residual: true,
        }
    }

    fn branch_stretch(target: PostStackLayerTarget) -> PostStackOperation {
        PostStackOperation::Stretch {
            target,
            stretch: PostStackStretch {
                preset: "autoNatural".into(),
                stretch: 4.0,
                symmetry: 0.12,
                local_intensity: 0.3,
                black_point: -5.0,
                white_point: 900.0,
                linked: true,
            },
        }
    }

    fn branch_curves(target: PostStackLayerTarget) -> PostStackOperation {
        PostStackOperation::Curves {
            target,
            curves: PostStackCurves {
                master: contrast_curve(),
                ..PostStackCurves::default()
            },
        }
    }

    #[test]
    fn failed_recipe_update_rolls_back_to_the_last_valid_revision() {
        let mut result = layer_fixture();
        let original = result.data.clone();
        let error = ds_poststack_commit_operations(
            &mut result,
            [PostStackOperation::Denoise {
                target: PostStackLayerTarget::Object,
                strength: 0.4,
                detail_protection: 0.7,
                chroma_strength: 0.3,
            }],
        )
        .unwrap_err();
        assert!(error.contains("Crea las capas"));
        assert!(result.post_stack_recipe.operations.is_empty());
        assert_eq!(result.data, original);
    }

    #[test]
    fn crop_replays_safe_star_layers_and_invalidates_geometry_dependent_models() {
        let mut result = layer_fixture();
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Astrometry {
                solution: astrometry_fixture(),
            },
        );
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::GaiaPcc {
                gain_r: 1.1,
                gain_g: 1.0,
                gain_b: 0.9,
                reference: "g2v".into(),
                matched_stars: 12,
                rms_px: 0.4,
            },
        );
        ds_poststack_set_operation(&mut result, layer_separation());
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Deconvolution {
                target: PostStackLayerTarget::Object,
                psf: PostStackPsfModel {
                    measured: true,
                    stars_used: 10,
                    confidence: 0.8,
                    ..PostStackPsfModel::default()
                },
                iterations: 2,
                regularization: 0.1,
                deringing: 0.8,
                flux_conservation: true,
            },
        );
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Detail {
                target: PostStackLayerTarget::Object,
                amount: 0.3,
                radius: 1,
                star_protection: 0.8,
            },
        );
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Recombine {
                object_weight: 1.0,
                star_weight: 1.0,
                residual_weight: 1.0,
            },
        );
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Crop {
                crop: PostStackCrop {
                    x: 1,
                    y: 1,
                    width: 30,
                    height: 30,
                    source_width: 32,
                    source_height: 32,
                },
            },
        );
        assert!(result
            .post_stack_recipe
            .operations
            .iter()
            .any(|operation| matches!(operation, PostStackOperation::StarSeparation { .. })));
        assert!(result
            .post_stack_recipe
            .operations
            .iter()
            .any(|operation| matches!(operation, PostStackOperation::Recombine { .. })));
        assert!(result.post_stack_recipe.operations.iter().any(|operation| {
            matches!(
                operation,
                PostStackOperation::Detail {
                    target: PostStackLayerTarget::Object,
                    ..
                }
            )
        }));
        assert!(
            !result.post_stack_recipe.operations.iter().any(|operation| {
                matches!(
                    operation,
                    PostStackOperation::Astrometry { .. }
                        | PostStackOperation::GaiaPcc { .. }
                        | PostStackOperation::Deconvolution { .. }
                )
            })
        );
        let layers = ds_poststack_replay(&mut result).unwrap().unwrap();
        assert_eq!((result.width, result.height), (30, 30));
        ds_poststack_validate_layer_geometry(&layers, result.width, result.height, result.channels)
            .unwrap();
    }

    #[test]
    fn every_crop_undo_redo_prefix_is_a_closed_graph() {
        let mut result = layer_fixture();
        ds_poststack_set_operation(&mut result, layer_separation());
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Detail {
                target: PostStackLayerTarget::Object,
                amount: 0.25,
                radius: 1,
                star_protection: 0.8,
            },
        );
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Recombine {
                object_weight: 1.0,
                star_weight: 1.0,
                residual_weight: 1.0,
            },
        );
        ds_poststack_commit_crop(
            &mut result,
            PostStackCrop {
                x: 2,
                y: 2,
                width: 28,
                height: 28,
                source_width: 32,
                source_height: 32,
            },
        )
        .unwrap();
        let total = result.post_stack_recipe.operations.len();
        for cursor in 0..=total {
            ds_poststack_set_cursor_transactional(&mut result, cursor).unwrap();
            let layers = ds_poststack_replay(&mut result).unwrap();
            if let Some(workspace) = layers {
                ds_poststack_validate_layer_geometry(
                    &workspace,
                    result.width,
                    result.height,
                    result.channels,
                )
                .unwrap();
            }
        }
        for cursor in (0..=total).rev() {
            ds_poststack_set_cursor_transactional(&mut result, cursor).unwrap();
        }
    }

    #[test]
    fn star_separation_keeps_combined_safe_until_explicit_recombination() {
        let mut result = layer_fixture();
        let source = result.data.clone();
        ds_poststack_commit_operations(&mut result, [layer_separation()]).unwrap();
        assert_eq!(result.data, source);
        assert_eq!(result.post_stack_recipe.operations.len(), 1);
        let state = ds_poststack_layer_state(&result).unwrap();
        assert!(!state.recombined);
        assert!(state.recombine_eligible);
    }

    #[test]
    fn sequential_branch_domains_can_be_edited_without_a_dead_end() {
        let mut result = layer_fixture();
        ds_poststack_commit_operations(&mut result, [layer_separation()]).unwrap();
        ds_poststack_commit_operations(
            &mut result,
            [PostStackOperation::Recombine {
                object_weight: 1.0,
                star_weight: 1.0,
                residual_weight: 1.0,
            }],
        )
        .unwrap();
        ds_poststack_commit_operations(&mut result, [branch_stretch(PostStackLayerTarget::Object)])
            .unwrap();
        let pending = ds_poststack_layer_state(&result).unwrap();
        assert!(!pending.recombined);
        assert!(!pending.recombine_eligible);
        assert!(pending.recombine_block_reason.is_some());

        ds_poststack_commit_operations(&mut result, [branch_stretch(PostStackLayerTarget::Stars)])
            .unwrap();
        let reconciled = ds_poststack_layer_state(&result).unwrap();
        assert!(reconciled.recombined);
        assert!(reconciled.recombine_eligible);
        assert_eq!(reconciled.object_domain, "presentation");
        assert_eq!(reconciled.stars_domain, "presentation");
    }

    #[test]
    fn curves_edit_only_the_requested_star_layer() {
        let mut result = layer_fixture();
        ds_poststack_commit_operations(&mut result, [layer_separation()]).unwrap();
        ds_poststack_commit_operations(&mut result, [branch_stretch(PostStackLayerTarget::Object)])
            .unwrap();
        let before = ds_poststack_replay(&mut result).unwrap().unwrap();
        ds_poststack_commit_operations(&mut result, [branch_curves(PostStackLayerTarget::Object)])
            .unwrap();
        let after = ds_poststack_replay(&mut result).unwrap().unwrap();
        assert_ne!(after.object, before.object);
        assert_eq!(
            after.stars, before.stars,
            "Curves:Object no debe modificar Estrellas"
        );
        assert!(after.object_nonlinear);
        assert!(!after.stars_nonlinear);
    }

    #[test]
    fn curves_sort_after_stretch_and_before_detail_and_finish() {
        let mut result = fixture();
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Finish {
                target: PostStackLayerTarget::Combined,
                saturation: 0.0,
                contrast: 0.1,
                highlight_protection: 0.8,
            },
        );
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Detail {
                target: PostStackLayerTarget::Combined,
                amount: 0.2,
                radius: 1,
                star_protection: 0.8,
            },
        );
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Curves {
                target: PostStackLayerTarget::Combined,
                curves: PostStackCurves {
                    master: contrast_curve(),
                    ..PostStackCurves::default()
                },
            },
        );
        ds_poststack_set_operation(
            &mut result,
            PostStackOperation::Stretch {
                target: PostStackLayerTarget::Combined,
                stretch: PostStackStretch {
                    preset: "autoNatural".into(),
                    stretch: 4.0,
                    symmetry: 0.1,
                    local_intensity: 0.3,
                    black_point: 0.0,
                    white_point: 100.0,
                    linked: true,
                },
            },
        );
        let names = result
            .post_stack_recipe
            .operations
            .iter()
            .map(ds_poststack_operation_name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["stretch", "curves", "detail", "finish"]);
    }

    #[test]
    fn curves_undo_redo_rebuilds_exactly_from_source() {
        let mut result = fixture();
        ds_poststack_commit_operations(
            &mut result,
            [PostStackOperation::Stretch {
                target: PostStackLayerTarget::Combined,
                stretch: PostStackStretch {
                    preset: "autoNatural".into(),
                    stretch: 4.0,
                    symmetry: 0.1,
                    local_intensity: 0.3,
                    black_point: 0.0,
                    white_point: 100.0,
                    linked: true,
                },
            }],
        )
        .unwrap();
        let stretched = result.data.clone();
        ds_poststack_commit_operations(
            &mut result,
            [PostStackOperation::Curves {
                target: PostStackLayerTarget::Combined,
                curves: PostStackCurves {
                    master: contrast_curve(),
                    ..PostStackCurves::default()
                },
            }],
        )
        .unwrap();
        let edited = result.data.clone();
        assert_ne!(edited, stretched);
        ds_poststack_commit_operations(
            &mut result,
            [PostStackOperation::Curves {
                target: PostStackLayerTarget::Combined,
                curves: PostStackCurves {
                    master: contrast_curve(),
                    ..PostStackCurves::default()
                },
            }],
        )
        .unwrap();
        assert_eq!(
            result.data, edited,
            "repetir Curves debe recalcular desde fuente, no acumularla"
        );
        assert_eq!(result.post_stack_recipe.operations.len(), 2);
        ds_poststack_set_cursor_transactional(&mut result, 1).unwrap();
        assert_eq!(result.data, stretched);
        ds_poststack_set_cursor_transactional(&mut result, 2).unwrap();
        assert_eq!(result.data, edited);
    }

    #[test]
    fn crop_reanalyzes_nonmanual_stretches_on_the_new_geometry() {
        let mut result = layer_fixture();
        ds_poststack_commit_operations(&mut result, [layer_separation()]).unwrap();
        ds_poststack_commit_operations(&mut result, [branch_stretch(PostStackLayerTarget::Object)])
            .unwrap();
        let before = result
            .post_stack_recipe
            .operations
            .iter()
            .find_map(|operation| match operation {
                PostStackOperation::Stretch {
                    target: PostStackLayerTarget::Object,
                    stretch,
                } => Some(stretch.clone()),
                _ => None,
            })
            .unwrap();
        ds_poststack_commit_crop(
            &mut result,
            PostStackCrop {
                x: 0,
                y: 0,
                width: 16,
                height: 16,
                source_width: 32,
                source_height: 32,
            },
        )
        .unwrap();
        let after = result
            .post_stack_recipe
            .operations
            .iter()
            .find_map(|operation| match operation {
                PostStackOperation::Stretch {
                    target: PostStackLayerTarget::Object,
                    stretch,
                } => Some(stretch.clone()),
                _ => None,
            })
            .unwrap();
        assert_ne!(
            after, before,
            "un preset adaptativo debe volver a medir la ventana recortada"
        );
        assert!(after.black_point.is_finite() && after.white_point.is_finite());
        let layers = ds_poststack_replay(&mut result).unwrap().unwrap();
        ds_poststack_validate_layer_geometry(&layers, result.width, result.height, result.channels)
            .unwrap();
    }

    #[test]
    fn branch_stretch_analysis_uses_each_layer_instead_of_the_combined_master() {
        let mut result = layer_fixture();
        ds_poststack_commit_operations(&mut result, [layer_separation()]).unwrap();
        let mut working = ds_poststack_before_rank(&result, 64);
        let layers = ds_poststack_replay(&mut working).unwrap().unwrap();
        let object = ds_poststack_adaptive_analysis_for_data(&layers.object, working.channels);
        let stars = ds_poststack_adaptive_analysis_for_data(&layers.stars, working.channels);
        assert!(object.background.is_finite() && stars.background.is_finite());
        assert!(
            (object.background - stars.background).abs() > 1e-3
                || (object.white_point - stars.white_point).abs() > 1e-3,
            "Objeto y Estrellas deben medir sus propios límites adaptativos"
        );
    }

    #[test]
    fn revisiting_branch_deconvolution_replays_it_before_the_branch_stretch() {
        let mut result = layer_fixture();
        ds_poststack_commit_operations(&mut result, [layer_separation()]).unwrap();
        ds_poststack_commit_operations(&mut result, [branch_stretch(PostStackLayerTarget::Object)])
            .unwrap();
        ds_poststack_commit_operations(
            &mut result,
            [PostStackOperation::Deconvolution {
                target: PostStackLayerTarget::Object,
                psf: PostStackPsfModel {
                    fwhm_x: 2.8,
                    fwhm_y: 2.8,
                    theta: 0.0,
                    beta: 2.5,
                    stars_used: 12,
                    confidence: 0.9,
                    measured: true,
                },
                iterations: 3,
                regularization: 0.12,
                deringing: 0.8,
                flux_conservation: true,
            }],
        )
        .unwrap();
        let deconvolution = result
            .post_stack_recipe
            .operations
            .iter()
            .position(|operation| {
                matches!(
                    operation,
                    PostStackOperation::Deconvolution {
                        target: PostStackLayerTarget::Object,
                        ..
                    }
                )
            })
            .unwrap();
        let stretch = result
            .post_stack_recipe
            .operations
            .iter()
            .position(|operation| {
                matches!(
                    operation,
                    PostStackOperation::Stretch {
                        target: PostStackLayerTarget::Object,
                        ..
                    }
                )
            })
            .unwrap();
        assert!(
            deconvolution < stretch,
            "volver a deconvolución debe recalcular la rama lineal antes del estirado"
        );
    }

    fn poststack_test_fits_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("{}-{label}.fits", new_job_id("poststack-test")))
    }

    #[test]
    fn standalone_master_descriptor_routes_dualband_without_touching_source() {
        let path = poststack_test_fits_path("dualband");
        let data = (0..64).map(|value| value as f32 + 10.0).collect::<Vec<_>>();
        ds_save_float32_fits(
            &path,
            &data,
            8,
            8,
            1,
            &[
                ("LINEAR", "                   T".into()),
                ("FILTER", "'SV220 Ha OIII'".into()),
                ("INSTRUME", "'SV220'".into()),
            ],
        )
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        let image = ds_read_image(path.to_string_lossy().as_ref()).unwrap();
        let descriptor = ds_poststack_descriptor_for_file(
            path.to_string_lossy().as_ref(),
            "standalone-test",
            &image,
        )
        .unwrap();
        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            before, after,
            "abrir el Studio no puede reescribir el máster"
        );
        assert_eq!(descriptor.kind, PostStackSourceKind::Mono);
        assert_eq!(descriptor.component_filters, vec!["HA", "OIII"]);
        assert_eq!(descriptor.origin, PostStackSourceOrigin::StandaloneMaster);
        let result = ds_poststack_result_from_master("standalone-test".into(), image, &descriptor);
        let state = ds_poststack_state_for(&result, "preview".into());
        assert_eq!(state.source, descriptor);
        assert!(state.linear);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn standalone_loader_rejects_bayer_and_declared_nonlinear_fits() {
        let bayer_path = poststack_test_fits_path("bayer");
        let data = (0..64).map(|value| value as f32 + 1.0).collect::<Vec<_>>();
        ds_save_float32_fits(
            &bayer_path,
            &data,
            8,
            8,
            1,
            &[("BAYERPAT", "'RGGB'".into())],
        )
        .unwrap();
        let image = ds_read_image(bayer_path.to_string_lossy().as_ref()).unwrap();
        assert!(ds_poststack_descriptor_for_file(
            bayer_path.to_string_lossy().as_ref(),
            "bayer",
            &image
        )
        .unwrap_err()
        .contains("CFA/Bayer"));

        let nonlinear_path = poststack_test_fits_path("declared-stretch");
        ds_save_float32_fits(
            &nonlinear_path,
            &data,
            8,
            8,
            1,
            &[("LINEAR", "                   F".into())],
        )
        .unwrap();
        let image = ds_read_image(nonlinear_path.to_string_lossy().as_ref()).unwrap();
        assert!(ds_poststack_descriptor_for_file(
            nonlinear_path.to_string_lossy().as_ref(),
            "nonlinear",
            &image
        )
        .unwrap_err()
        .contains("LINEAR"));
        let _ = std::fs::remove_file(bayer_path);
        let _ = std::fs::remove_file(nonlinear_path);
    }
}
