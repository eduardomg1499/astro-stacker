// ============================================================================
// Zenith Deep-Sky Annotations
// ============================================================================
//
// Read-only annotation engine for the active deep-sky result.  The scientific
// pixels and the post-stack revision graph are never changed: a bounded raster
// preview and a transparent overlay are derived from the active WCS.
//
// Online catalogue access is deliberately optional.  A deterministic SIMBAD
// TAP query is cached under <workDir>/zenith_astrometry_index and every network
// operation has a short connect/total timeout plus a hard response-size limit.

const DS_ANNOTATION_SCHEMA_VERSION: &str = "zenith-deepsky-annotations-v1";
const DS_ANNOTATION_CACHE_VERSION: &str = "simbad-useful-objects-v1";
const DS_ANNOTATION_SIMBAD_TAP: &str = "https://simbad.cds.unistra.fr/simbad/sim-tap/sync";
const DS_ANNOTATION_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const DS_ANNOTATION_DEFAULT_ROWS: usize = 240;
const DS_ANNOTATION_MAX_ROWS: usize = 600;
const DS_ANNOTATION_DEFAULT_PREVIEW_EDGE: usize = 1600;
const DS_ANNOTATION_MAX_PREVIEW_EDGE: usize = 2400;

#[derive(Clone, Copy, Debug, Default, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DeepSkyAnnotationStyle {
    /// Retícula ecuatorial, escala y objetos codificados por clase.
    Atlas,
    /// Círculos limpios por clase, pensado para inspección/catalogación.
    Survey,
    /// Jerarquía tipográfica con líderes hacia los objetos principales.
    Focus,
    /// Sólo los objetos de mayor prioridad y una marca central discreta.
    #[default]
    Minimal,
}

impl DeepSkyAnnotationStyle {
    fn id(self) -> &'static str {
        match self {
            Self::Atlas => "zenithAtlas",
            Self::Survey => "zenithSurvey",
            Self::Focus => "zenithFocus",
            Self::Minimal => "zenithMinimal",
        }
    }

    fn includes_grid(self) -> bool {
        matches!(self, Self::Atlas)
    }

    fn default_label_limit(self) -> usize {
        match self {
            Self::Atlas => 180,
            Self::Survey => 120,
            Self::Focus => 72,
            Self::Minimal => 28,
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DeepSkyAnnotationRequest {
    pub style: DeepSkyAnnotationStyle,
    /// Explicit consent for one bounded SIMBAD TAP request when no cache exists.
    pub allow_online: bool,
    /// Set false to render only the WCS grid/center without catalogue access.
    pub include_catalog: bool,
    /// Atlas can be rendered without a grid when a clean export is preferred.
    pub include_grid: bool,
    /// Destination parent for zenith_astrometry_index.  Cache is never written
    /// beside the immutable source unless the caller chooses that directory.
    pub work_dir: Option<String>,
    pub max_rows: usize,
    pub max_preview_edge: usize,
    /// Optional UI override, still capped to keep layout/runtime bounded.
    pub max_labels: Option<usize>,
    /// Ignore an existing cache and refresh it after explicit online consent.
    pub refresh_cache: bool,
}

impl Default for DeepSkyAnnotationRequest {
    fn default() -> Self {
        Self {
            style: DeepSkyAnnotationStyle::Minimal,
            allow_online: false,
            include_catalog: true,
            include_grid: true,
            work_dir: None,
            max_rows: DS_ANNOTATION_DEFAULT_ROWS,
            max_preview_edge: DS_ANNOTATION_DEFAULT_PREVIEW_EDGE,
            max_labels: None,
            refresh_cache: false,
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkyAnnotationExportRequest {
    pub annotation: DeepSkyAnnotationRequest,
    pub path: String,
    /// false exports an alpha overlay; true exports the annotated preview.
    #[serde(default)]
    pub composited: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkyAnnotationError {
    pub code: String,
    pub message: String,
    pub recoverable: bool,
    pub suggested_action: String,
}

impl DeepSkyAnnotationError {
    fn recoverable(code: &str, message: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            recoverable: true,
            suggested_action: action.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "annotationInternal".into(),
            message: message.into(),
            recoverable: false,
            suggested_action: "Conserva el máster y abre el diagnóstico técnico.".into(),
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkyCatalogObject {
    pub main_id: String,
    pub ra_deg: f64,
    pub dec_deg: f64,
    pub object_type: String,
    pub major_axis_arcmin: Option<f32>,
    pub minor_axis_arcmin: Option<f32>,
    pub position_angle_deg: Option<f32>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkyProjectedAnnotation {
    pub main_id: String,
    pub label: String,
    pub ra_deg: f64,
    pub dec_deg: f64,
    pub object_type: String,
    pub object_class: String,
    pub major_axis_arcmin: Option<f32>,
    pub minor_axis_arcmin: Option<f32>,
    pub x: f32,
    pub y: f32,
    pub radius_px: f32,
    pub label_visible: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DeepSkyAnnotationPrimitive {
    Circle {
        x: f32,
        y: f32,
        radius: f32,
        color: [u8; 4],
        width: f32,
        object_id: Option<String>,
    },
    Crosshair {
        x: f32,
        y: f32,
        radius: f32,
        color: [u8; 4],
        object_id: Option<String>,
    },
    Label {
        x: f32,
        y: f32,
        text: String,
        color: [u8; 4],
        size: f32,
        object_id: Option<String>,
    },
    Leader {
        points: Vec<[f32; 2]>,
        color: [u8; 4],
        width: f32,
        object_id: Option<String>,
    },
    GridLine {
        points: Vec<[f32; 2]>,
        color: [u8; 4],
        coordinate: String,
    },
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkyAnnotationDiagnostics {
    pub schema_version: String,
    pub result_id: String,
    pub result_generation: usize,
    pub wcs_source: String,
    pub wcs_rms_px: f32,
    pub style_id: String,
    pub catalog_source: String,
    pub cache_path: Option<String>,
    pub cache_hit: bool,
    pub online_attempted: bool,
    pub query_fingerprint: Option<String>,
    pub response_rows: usize,
    pub useful_rows: usize,
    pub projected_rows: usize,
    pub labels_drawn: usize,
    pub labels_decluttered: usize,
    pub elapsed_ms: u128,
    pub warnings: Vec<String>,
    pub attribution: String,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkyAnnotationPreview {
    pub result_id: String,
    pub source_width: usize,
    pub source_height: usize,
    pub preview_width: usize,
    pub preview_height: usize,
    pub style: DeepSkyAnnotationStyle,
    pub style_id: String,
    pub composited_preview: String,
    pub transparent_overlay: String,
    pub objects: Vec<DeepSkyProjectedAnnotation>,
    pub primitives: Vec<DeepSkyAnnotationPrimitive>,
    pub diagnostics: DeepSkyAnnotationDiagnostics,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepSkyAnnotationExportResult {
    pub path: String,
    pub composited: bool,
    pub bytes: u64,
    pub result_id: String,
    pub diagnostics: DeepSkyAnnotationDiagnostics,
}

#[derive(Clone)]
struct DsAnnotationSnapshot {
    result_id: String,
    generation: usize,
    source_width: usize,
    source_height: usize,
    base: image::RgbaImage,
    wcs: AstrometrySolution,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DsAnnotationCacheEnvelope {
    schema_version: String,
    endpoint: String,
    query_fingerprint: String,
    fetched_at_utc: String,
    objects: Vec<DeepSkyCatalogObject>,
}

fn ds_annotation_error_from_io(
    context: &str,
    error: impl std::fmt::Display,
) -> DeepSkyAnnotationError {
    DeepSkyAnnotationError::recoverable(
        "annotationIo",
        format!("{context}: {error}"),
        "Elige una carpeta de trabajo con permisos de escritura o continúa sin catálogo.",
    )
}

fn ds_annotation_validate_wcs(solution: &AstrometrySolution) -> Result<(), DeepSkyAnnotationError> {
    let ctype = solution.ctype.to_ascii_uppercase();
    if !ctype.contains("TAN") {
        return Err(DeepSkyAnnotationError::recoverable(
            "unsupportedWcsProjection",
            format!(
                "Las anotaciones requieren una proyección TAN válida; se recibió '{}'.",
                solution.ctype
            ),
            "Vuelve a Astrometría y resuelve o incrusta un WCS TAN.",
        ));
    }
    let values = [
        solution.crval1,
        solution.crval2,
        solution.crpix1,
        solution.crpix2,
        solution.cd11,
        solution.cd12,
        solution.cd21,
        solution.cd22,
    ];
    let det = solution.cd11 * solution.cd22 - solution.cd12 * solution.cd21;
    if values.iter().any(|value| !value.is_finite()) || det.abs() < 1.0e-15 {
        return Err(DeepSkyAnnotationError::recoverable(
            "invalidWcs",
            "La matriz WCS no es finita o no se puede invertir.",
            "Vuelve a resolver la astrometría antes de generar anotaciones.",
        ));
    }
    Ok(())
}

fn ds_annotation_percentile(mut values: Vec<f32>, quantile: f32) -> f32 {
    values.retain(|value| value.is_finite());
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let index = ((values.len() - 1) as f32 * quantile.clamp(0.0, 1.0)).round() as usize;
    values[index]
}

fn ds_annotation_stretch(value: f32, low: f32, high: f32) -> u8 {
    if !value.is_finite() || high <= low {
        return 0;
    }
    let normalized = ((value - low) / (high - low)).clamp(0.0, 1.0);
    let stretched = (1.0 + 9.0 * normalized).ln() / 10.0_f32.ln();
    (stretched.powf(0.86) * 255.0).round().clamp(0.0, 255.0) as u8
}

fn ds_annotation_snapshot(
    state: &State<'_, AppState>,
    max_edge: usize,
) -> Result<DsAnnotationSnapshot, DeepSkyAnnotationError> {
    let generation = state.result_generation.load(Ordering::Acquire);
    let guard = state.deep_sky_result.lock().map_err(|_| {
        DeepSkyAnnotationError::internal("No se pudo leer el máster activo para anotarlo.")
    })?;
    let result = guard.as_ref().ok_or_else(|| {
        DeepSkyAnnotationError::recoverable(
            "noActiveDeepSkyResult",
            "No hay un máster de cielo profundo activo.",
            "Abre un resultado de apilado o un máster lineal en Deep Sky Studio.",
        )
    })?;
    let solution = result.astrometry_solution.clone().ok_or_else(|| {
        DeepSkyAnnotationError::recoverable(
            "missingWcs",
            "El máster activo todavía no tiene una solución WCS validada.",
            "Resuelve la astrometría en el paso Astrometría y vuelve a Anotaciones.",
        )
    })?;
    ds_annotation_validate_wcs(&solution)?;

    if result.width == 0
        || result.height == 0
        || result.channels == 0
        || result.data.len()
            != result
                .width
                .saturating_mul(result.height)
                .saturating_mul(result.channels)
    {
        return Err(DeepSkyAnnotationError::internal(
            "El máster activo no conserva una geometría de píxeles válida.",
        ));
    }

    let max_edge = max_edge.clamp(320, DS_ANNOTATION_MAX_PREVIEW_EDGE);
    let scale = (max_edge as f64 / result.width.max(result.height) as f64).min(1.0);
    let preview_width = ((result.width as f64 * scale).round() as usize).max(1);
    let preview_height = ((result.height as f64 * scale).round() as usize).max(1);

    // Statistics are sampled deterministically; the full scientific buffer is
    // neither cloned nor mutated.
    let stats_edge = 320usize;
    let stats_step_x = (result.width / stats_edge).max(1);
    let stats_step_y = (result.height / stats_edge).max(1);
    let mut channel_samples = [Vec::<f32>::new(), Vec::<f32>::new(), Vec::<f32>::new()];
    for y in (0..result.height).step_by(stats_step_y) {
        for x in (0..result.width).step_by(stats_step_x) {
            let pixel = (y * result.width + x) * result.channels;
            let rgb = ds_annotation_source_rgb(&result.data, pixel, result.channels);
            for channel in 0..3 {
                channel_samples[channel].push(rgb[channel]);
            }
        }
    }
    let mut lows = [0.0; 3];
    let mut highs = [1.0; 3];
    for channel in 0..3 {
        lows[channel] = ds_annotation_percentile(channel_samples[channel].clone(), 0.005);
        highs[channel] =
            ds_annotation_percentile(std::mem::take(&mut channel_samples[channel]), 0.997);
        if highs[channel] <= lows[channel] {
            highs[channel] = lows[channel] + 1.0;
        }
    }

    let mut base = image::RgbaImage::new(preview_width as u32, preview_height as u32);
    for py in 0..preview_height {
        let sy = ((py as f64 + 0.5) / scale)
            .floor()
            .clamp(0.0, (result.height - 1) as f64) as usize;
        for px in 0..preview_width {
            let sx = ((px as f64 + 0.5) / scale)
                .floor()
                .clamp(0.0, (result.width - 1) as f64) as usize;
            let pixel = (sy * result.width + sx) * result.channels;
            let rgb = ds_annotation_source_rgb(&result.data, pixel, result.channels);
            base.put_pixel(
                px as u32,
                py as u32,
                image::Rgba([
                    ds_annotation_stretch(rgb[0], lows[0], highs[0]),
                    ds_annotation_stretch(rgb[1], lows[1], highs[1]),
                    ds_annotation_stretch(rgb[2], lows[2], highs[2]),
                    255,
                ]),
            );
        }
    }

    Ok(DsAnnotationSnapshot {
        result_id: result.id.clone(),
        generation,
        source_width: result.width,
        source_height: result.height,
        base,
        wcs: solution,
    })
}

fn ds_annotation_source_rgb(data: &[f32], offset: usize, channels: usize) -> [f32; 3] {
    match channels {
        1 => {
            let value = data[offset];
            [value, value, value]
        }
        2 => {
            let a = data[offset];
            let b = data[offset + 1];
            [a, b, (a + b) * 0.5]
        }
        _ => [data[offset], data[offset + 1], data[offset + 2]],
    }
}

fn ds_annotation_normalize_ra(mut ra_deg: f64) -> f64 {
    ra_deg %= 360.0;
    if ra_deg < 0.0 {
        ra_deg += 360.0;
    }
    ra_deg
}

fn ds_annotation_delta_ra_rad(ra_deg: f64, reference_deg: f64) -> f64 {
    let mut delta = (ra_deg - reference_deg).to_radians();
    while delta > std::f64::consts::PI {
        delta -= 2.0 * std::f64::consts::PI;
    }
    while delta < -std::f64::consts::PI {
        delta += 2.0 * std::f64::consts::PI;
    }
    delta
}

/// ICRS sky coordinate -> zero-based FITS pixel using a TAN projection and CD.
fn ds_annotation_world_to_pixel(
    solution: &AstrometrySolution,
    ra_deg: f64,
    dec_deg: f64,
) -> Option<(f64, f64)> {
    let dec0 = solution.crval2.to_radians();
    let dec = dec_deg.to_radians();
    let dra = ds_annotation_delta_ra_rad(ra_deg, solution.crval1);
    let cosc = dec0.sin() * dec.sin() + dec0.cos() * dec.cos() * dra.cos();
    if !cosc.is_finite() || cosc <= 1.0e-12 {
        return None;
    }
    let xi = dec.cos() * dra.sin() / cosc;
    let eta = (dec0.cos() * dec.sin() - dec0.sin() * dec.cos() * dra.cos()) / cosc;
    let xi_deg = xi.to_degrees();
    let eta_deg = eta.to_degrees();
    let det = solution.cd11 * solution.cd22 - solution.cd12 * solution.cd21;
    if det.abs() < 1.0e-15 {
        return None;
    }
    let dx = (solution.cd22 * xi_deg - solution.cd12 * eta_deg) / det;
    let dy = (-solution.cd21 * xi_deg + solution.cd11 * eta_deg) / det;
    let x = solution.crpix1 - 1.0 + dx;
    let y = solution.crpix2 - 1.0 + dy;
    if x.is_finite() && y.is_finite() {
        Some((x, y))
    } else {
        None
    }
}

/// Zero-based FITS pixel -> ICRS sky coordinate for field-radius/grid sampling.
fn ds_annotation_pixel_to_world(
    solution: &AstrometrySolution,
    x: f64,
    y: f64,
) -> Option<(f64, f64)> {
    let dx = x - (solution.crpix1 - 1.0);
    let dy = y - (solution.crpix2 - 1.0);
    let xi = (solution.cd11 * dx + solution.cd12 * dy).to_radians();
    let eta = (solution.cd21 * dx + solution.cd22 * dy).to_radians();
    let ra0 = solution.crval1.to_radians();
    let dec0 = solution.crval2.to_radians();
    let denominator = dec0.cos() - eta * dec0.sin();
    let ra = ra0 + xi.atan2(denominator);
    let dec =
        ((dec0.sin() + eta * dec0.cos()) / (xi * xi + denominator * denominator).sqrt()).atan();
    if ra.is_finite() && dec.is_finite() {
        Some((
            ds_annotation_normalize_ra(ra.to_degrees()),
            dec.to_degrees(),
        ))
    } else {
        None
    }
}

fn ds_annotation_angular_distance_deg(
    ra1_deg: f64,
    dec1_deg: f64,
    ra2_deg: f64,
    dec2_deg: f64,
) -> f64 {
    let d_ra = ds_annotation_delta_ra_rad(ra2_deg, ra1_deg);
    let dec1 = dec1_deg.to_radians();
    let dec2 = dec2_deg.to_radians();
    let cosine = dec1.sin() * dec2.sin() + dec1.cos() * dec2.cos() * d_ra.cos();
    cosine.clamp(-1.0, 1.0).acos().to_degrees()
}

fn ds_annotation_field_radius_deg(snapshot: &DsAnnotationSnapshot) -> f64 {
    let corners = [
        (0.0, 0.0),
        ((snapshot.source_width - 1) as f64, 0.0),
        (0.0, (snapshot.source_height - 1) as f64),
        (
            (snapshot.source_width - 1) as f64,
            (snapshot.source_height - 1) as f64,
        ),
    ];
    let mut radius: f64 = 0.0;
    for (x, y) in corners {
        if let Some((ra, dec)) = ds_annotation_pixel_to_world(&snapshot.wcs, x, y) {
            radius = radius.max(ds_annotation_angular_distance_deg(
                snapshot.wcs.crval1,
                snapshot.wcs.crval2,
                ra,
                dec,
            ));
        }
    }
    if radius <= 0.0 || !radius.is_finite() {
        radius = snapshot.wcs.scale_arcsec_px.abs()
            * ((snapshot.source_width.pow(2) + snapshot.source_height.pow(2)) as f64).sqrt()
            / 7200.0;
    }
    radius.clamp(0.02, 25.0)
}

fn ds_annotation_simbad_query(
    center_ra: f64,
    center_dec: f64,
    radius_deg: f64,
    max_rows: usize,
) -> String {
    let max_rows = max_rows.clamp(1, DS_ANNOTATION_MAX_ROWS);
    // TOP bounds work at the service, ORDER BY makes cache/results stable.
    // Object filtering happens both here and after parsing so an upstream type
    // alias cannot silently introduce thousands of field stars.
    format!(
        "SELECT TOP {max_rows} main_id, ra, dec, otype, \
         galdim_majaxis, galdim_minaxis, galdim_angle \
         FROM basic \
         WHERE CONTAINS(POINT('ICRS', ra, dec), \
         CIRCLE('ICRS', {center_ra:.10}, {center_dec:.10}, {radius_deg:.10})) = 1 \
         AND otype IN ('G','GiG','GiC','ClG','GClstr','QSO','AGN','SyG',\
         'HII','SNR','PN','Neb','RNe','DNe','GlC','OpC','Cl*','MolC') \
         ORDER BY main_id ASC"
    )
}

fn ds_annotation_query_fingerprint(query: &str) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(format!(
        "{DS_ANNOTATION_CACHE_VERSION}\n{query}"
    )))
}

fn ds_annotation_cache_path(
    work_dir: Option<&str>,
    fingerprint: &str,
) -> Result<std::path::PathBuf, DeepSkyAnnotationError> {
    let work_dir = work_dir
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            DeepSkyAnnotationError::recoverable(
                "annotationWorkDirRequired",
                "Para usar el catálogo se necesita una carpeta de trabajo para el caché local.",
                "Selecciona la carpeta de la sesión; el máster científico no se modificará.",
            )
        })?;
    let root = std::path::Path::new(work_dir).join("zenith_astrometry_index");
    std::fs::create_dir_all(&root).map_err(|error| {
        ds_annotation_error_from_io("No se pudo crear el caché astrométrico", error)
    })?;
    Ok(root.join(format!("simbad-{fingerprint}.json")))
}

fn ds_annotation_read_cache(
    path: &std::path::Path,
    fingerprint: &str,
) -> Result<Option<DsAnnotationCacheEnvelope>, DeepSkyAnnotationError> {
    if !path.exists() {
        return Ok(None);
    }
    let metadata = std::fs::metadata(path)
        .map_err(|error| ds_annotation_error_from_io("No se pudo inspeccionar el caché", error))?;
    if metadata.len() as usize > DS_ANNOTATION_MAX_RESPONSE_BYTES {
        return Err(DeepSkyAnnotationError::recoverable(
            "annotationCacheTooLarge",
            "El caché de anotaciones supera el límite seguro de 2 MB.",
            "Activa la actualización en línea para reconstruir sólo este índice.",
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| ds_annotation_error_from_io("No se pudo leer el caché", error))?;
    let envelope: DsAnnotationCacheEnvelope = serde_json::from_slice(&bytes).map_err(|error| {
        DeepSkyAnnotationError::recoverable(
            "annotationCacheInvalid",
            format!("El caché de anotaciones no es válido: {error}"),
            "Activa la actualización en línea para reconstruir sólo este índice.",
        )
    })?;
    if envelope.schema_version != DS_ANNOTATION_SCHEMA_VERSION
        || envelope.query_fingerprint != fingerprint
    {
        return Ok(None);
    }
    Ok(Some(envelope))
}

fn ds_annotation_write_cache_atomic(
    path: &std::path::Path,
    envelope: &DsAnnotationCacheEnvelope,
) -> Result<(), DeepSkyAnnotationError> {
    let bytes = serde_json::to_vec(envelope).map_err(|error| {
        DeepSkyAnnotationError::internal(format!("No se pudo serializar el caché: {error}"))
    })?;
    if bytes.len() > DS_ANNOTATION_MAX_RESPONSE_BYTES {
        return Err(DeepSkyAnnotationError::internal(
            "El índice de anotaciones generado supera el límite seguro.",
        ));
    }
    let suffix = new_job_id("annotation-cache");
    let temporary = path.with_extension(format!("json.{suffix}.part"));
    {
        let mut file = std::fs::File::create(&temporary).map_err(|error| {
            ds_annotation_error_from_io("No se pudo crear el caché temporal", error)
        })?;
        std::io::Write::write_all(&mut file, &bytes).map_err(|error| {
            ds_annotation_error_from_io("No se pudo escribir el caché temporal", error)
        })?;
        file.sync_all().map_err(|error| {
            ds_annotation_error_from_io("No se pudo confirmar el caché temporal", error)
        })?;
    }
    if path.exists() {
        #[cfg(target_os = "windows")]
        std::fs::remove_file(path).map_err(|error| {
            ds_annotation_error_from_io("No se pudo reemplazar el caché anterior", error)
        })?;
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(ds_annotation_error_from_io(
            "No se pudo publicar el caché atómicamente",
            error,
        ));
    }
    Ok(())
}

fn ds_annotation_value_f64(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

fn ds_annotation_parse_simbad_json(
    bytes: &[u8],
) -> Result<Vec<DeepSkyCatalogObject>, DeepSkyAnnotationError> {
    let root: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
        DeepSkyAnnotationError::recoverable(
            "catalogResponseInvalid",
            format!("SIMBAD devolvió JSON no válido: {error}"),
            "Reintenta más tarde o continúa con un índice local ya descargado.",
        )
    })?;
    let metadata = root
        .get("metadata")
        .and_then(|value| value.as_array())
        .ok_or_else(|| {
            DeepSkyAnnotationError::recoverable(
                "catalogResponseInvalid",
                "SIMBAD no devolvió metadatos TAP reconocibles.",
                "Reintenta más tarde o continúa con un índice local.",
            )
        })?;
    let mut columns = std::collections::BTreeMap::<String, usize>::new();
    for (index, item) in metadata.iter().enumerate() {
        if let Some(name) = item
            .get("name")
            .and_then(|value| value.as_str())
            .or_else(|| item.as_str())
        {
            columns.insert(name.to_ascii_lowercase(), index);
        }
    }
    let index = |name: &str| {
        columns.get(name).copied().ok_or_else(|| {
            DeepSkyAnnotationError::recoverable(
                "catalogResponseInvalid",
                format!("SIMBAD no devolvió la columna requerida '{name}'."),
                "Actualiza el índice cuando el servicio TAP vuelva a estar disponible.",
            )
        })
    };
    let main_id_i = index("main_id")?;
    let ra_i = index("ra")?;
    let dec_i = index("dec")?;
    let otype_i = index("otype")?;
    let major_i = columns.get("galdim_majaxis").copied();
    let minor_i = columns.get("galdim_minaxis").copied();
    let angle_i = columns.get("galdim_angle").copied();
    let rows = root
        .get("data")
        .and_then(|value| value.as_array())
        .ok_or_else(|| {
            DeepSkyAnnotationError::recoverable(
                "catalogResponseInvalid",
                "SIMBAD no devolvió filas TAP reconocibles.",
                "Reintenta más tarde o continúa con un índice local.",
            )
        })?;
    let mut objects = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(row) = row.as_array() else {
            continue;
        };
        let Some(main_id) = row.get(main_id_i).and_then(|value| value.as_str()) else {
            continue;
        };
        let Some(ra_deg) = row.get(ra_i).and_then(ds_annotation_value_f64) else {
            continue;
        };
        let Some(dec_deg) = row.get(dec_i).and_then(ds_annotation_value_f64) else {
            continue;
        };
        let object_type = row
            .get(otype_i)
            .and_then(|value| value.as_str())
            .unwrap_or("Unknown")
            .trim()
            .to_string();
        if !ds_annotation_useful_type(&object_type) {
            continue;
        }
        let optional_f32 = |column: Option<usize>| {
            column
                .and_then(|position| row.get(position))
                .and_then(ds_annotation_value_f64)
                .filter(|value| value.is_finite() && *value > 0.0)
                .map(|value| value as f32)
        };
        objects.push(DeepSkyCatalogObject {
            main_id: main_id.trim().replace('\0', ""),
            ra_deg: ds_annotation_normalize_ra(ra_deg),
            dec_deg,
            object_type,
            major_axis_arcmin: optional_f32(major_i),
            minor_axis_arcmin: optional_f32(minor_i),
            position_angle_deg: optional_f32(angle_i),
        });
    }
    objects.sort_by(|a, b| {
        a.main_id
            .cmp(&b.main_id)
            .then_with(|| a.ra_deg.total_cmp(&b.ra_deg))
            .then_with(|| a.dec_deg.total_cmp(&b.dec_deg))
    });
    objects.dedup_by(|a, b| {
        a.main_id == b.main_id
            && (a.ra_deg - b.ra_deg).abs() < 1.0e-10
            && (a.dec_deg - b.dec_deg).abs() < 1.0e-10
    });
    Ok(objects)
}

fn ds_annotation_useful_type(object_type: &str) -> bool {
    let normalized = object_type.trim().to_ascii_uppercase();
    matches!(
        normalized.as_str(),
        "G" | "GIG"
            | "GIC"
            | "CLG"
            | "GCLSTR"
            | "QSO"
            | "AGN"
            | "SYG"
            | "HII"
            | "SNR"
            | "PN"
            | "NEB"
            | "RNE"
            | "DNE"
            | "GLC"
            | "OPC"
            | "CL*"
            | "MOLC"
    )
}

fn ds_annotation_fetch_simbad(
    query: &str,
) -> Result<Vec<DeepSkyCatalogObject>, DeepSkyAnnotationError> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(2))
        .timeout(std::time::Duration::from_secs(7))
        .user_agent("Zenith-Astro-Stacker/annotations-v1")
        .build()
        .map_err(|error| {
            DeepSkyAnnotationError::recoverable(
                "catalogNetworkUnavailable",
                format!("No se pudo preparar la consulta SIMBAD: {error}"),
                "Comprueba la red o continúa con un índice local.",
            )
        })?;
    let mut response = client
        .post(DS_ANNOTATION_SIMBAD_TAP)
        .form(&[
            ("REQUEST", "doQuery"),
            ("PHASE", "RUN"),
            ("LANG", "ADQL"),
            ("FORMAT", "json"),
            ("QUERY", query),
        ])
        .send()
        .map_err(|error| {
            let timeout = error.is_timeout();
            DeepSkyAnnotationError::recoverable(
                if timeout {
                    "catalogNetworkTimeout"
                } else {
                    "catalogNetworkUnavailable"
                },
                format!("La consulta SIMBAD no se completó: {error}"),
                "Reintenta o continúa con el último índice local; la revisión no cambió.",
            )
        })?;
    if !response.status().is_success() {
        return Err(DeepSkyAnnotationError::recoverable(
            "catalogHttpError",
            format!("SIMBAD respondió HTTP {}.", response.status()),
            "Reintenta más tarde o continúa con el último índice local.",
        ));
    }
    if response
        .content_length()
        .map(|length| length as usize > DS_ANNOTATION_MAX_RESPONSE_BYTES)
        .unwrap_or(false)
    {
        return Err(DeepSkyAnnotationError::recoverable(
            "catalogResponseTooLarge",
            "SIMBAD anunció una respuesta mayor al límite seguro de 2 MB.",
            "Reduce el campo o el límite de objetos.",
        ));
    }
    let mut bounded =
        std::io::Read::take(&mut response, (DS_ANNOTATION_MAX_RESPONSE_BYTES + 1) as u64);
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut bounded, &mut bytes).map_err(|error| {
        DeepSkyAnnotationError::recoverable(
            "catalogNetworkUnavailable",
            format!("No se pudo leer la respuesta SIMBAD: {error}"),
            "Reintenta más tarde o continúa con el último índice local.",
        )
    })?;
    if bytes.len() > DS_ANNOTATION_MAX_RESPONSE_BYTES {
        return Err(DeepSkyAnnotationError::recoverable(
            "catalogResponseTooLarge",
            "La respuesta SIMBAD superó el límite seguro de 2 MB.",
            "Reduce el campo o el límite de objetos.",
        ));
    }
    ds_annotation_parse_simbad_json(&bytes)
}

fn ds_annotation_load_catalog(
    snapshot: &DsAnnotationSnapshot,
    request: &DeepSkyAnnotationRequest,
) -> Result<
    (
        Vec<DeepSkyCatalogObject>,
        String,
        Option<String>,
        bool,
        bool,
        Option<String>,
    ),
    DeepSkyAnnotationError,
> {
    if !request.include_catalog {
        return Ok((Vec::new(), "wcsOnly".into(), None, false, false, None));
    }
    let radius = ds_annotation_field_radius_deg(snapshot);
    let query = ds_annotation_simbad_query(
        snapshot.wcs.crval1,
        snapshot.wcs.crval2,
        radius,
        request.max_rows.clamp(1, DS_ANNOTATION_MAX_ROWS),
    );
    let fingerprint = ds_annotation_query_fingerprint(&query);
    let cache_path = ds_annotation_cache_path(request.work_dir.as_deref(), &fingerprint)?;
    if !request.refresh_cache {
        match ds_annotation_read_cache(&cache_path, &fingerprint) {
            Ok(Some(envelope)) => {
                return Ok((
                    envelope.objects,
                    "simbadCache".into(),
                    Some(cache_path.display().to_string()),
                    true,
                    false,
                    Some(fingerprint),
                ));
            }
            Ok(None) => {}
            Err(error) if request.allow_online => {
                // A broken cache is recoverable through the explicitly allowed
                // online refresh.  It is never used partially.
                let _ = error;
            }
            Err(error) => return Err(error),
        }
    }
    if !request.allow_online {
        return Err(DeepSkyAnnotationError::recoverable(
            "offlineCatalogCacheMiss",
            "No hay un índice SIMBAD local para este campo y el acceso web está desactivado.",
            "Activa “Consultar SIMBAD” una vez o genera sólo retícula WCS.",
        ));
    }
    let objects = ds_annotation_fetch_simbad(&query)?;
    let envelope = DsAnnotationCacheEnvelope {
        schema_version: DS_ANNOTATION_SCHEMA_VERSION.into(),
        endpoint: DS_ANNOTATION_SIMBAD_TAP.into(),
        query_fingerprint: fingerprint.clone(),
        fetched_at_utc: chrono::Utc::now().to_rfc3339(),
        objects: objects.clone(),
    };
    ds_annotation_write_cache_atomic(&cache_path, &envelope)?;
    Ok((
        objects,
        "simbadOnline".into(),
        Some(cache_path.display().to_string()),
        false,
        true,
        Some(fingerprint),
    ))
}

fn ds_annotation_class(object_type: &str) -> &'static str {
    match object_type.trim().to_ascii_uppercase().as_str() {
        "G" | "GIG" | "GIC" | "CLG" | "GCLSTR" | "SYG" | "AGN" | "QSO" => "galaxy",
        "HII" | "NEB" | "RNE" | "DNE" | "MOLC" => "nebula",
        "PN" => "planetaryNebula",
        "SNR" => "supernovaRemnant",
        "GLC" | "OPC" | "CL*" => "cluster",
        _ => "other",
    }
}

fn ds_annotation_color(object_class: &str, style: DeepSkyAnnotationStyle) -> [u8; 4] {
    if style == DeepSkyAnnotationStyle::Minimal {
        return [126, 236, 255, 225];
    }
    match object_class {
        "galaxy" => [255, 128, 177, 235],
        "nebula" => [255, 174, 91, 235],
        "planetaryNebula" => [109, 245, 158, 235],
        "supernovaRemnant" => [255, 102, 112, 235],
        "cluster" => [121, 221, 255, 235],
        _ => [222, 220, 255, 225],
    }
}

fn ds_annotation_catalog_priority(label: &str) -> i32 {
    let uppercase = label.to_ascii_uppercase();
    if uppercase.starts_with("M ") || uppercase.starts_with("MESSIER ") {
        0
    } else if uppercase.starts_with("NGC ") || uppercase.starts_with("NGC") {
        1
    } else if uppercase.starts_with("IC ") || uppercase.starts_with("IC") {
        2
    } else if uppercase.starts_with("SH2") || uppercase.starts_with("SH 2") {
        3
    } else {
        4
    }
}

fn ds_annotation_radius_px(object: &DeepSkyCatalogObject, snapshot: &DsAnnotationSnapshot) -> f32 {
    let preview_scale = snapshot.base.width() as f64 / snapshot.source_width as f64;
    let arcsec_px = snapshot.wcs.scale_arcsec_px.abs().max(1.0e-6);
    let catalog_radius = object
        .major_axis_arcmin
        .or(object.minor_axis_arcmin)
        .map(|axis| axis as f64 * 30.0 / arcsec_px * preview_scale)
        .unwrap_or(0.0);
    let fallback = match ds_annotation_class(&object.object_type) {
        "galaxy" => 13.0,
        "nebula" => 18.0,
        "planetaryNebula" => 10.0,
        "supernovaRemnant" => 20.0,
        "cluster" => 15.0,
        _ => 10.0,
    };
    (catalog_radius as f32).clamp(fallback, 72.0)
}

#[derive(Clone, Copy)]
struct DsAnnotationRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl DsAnnotationRect {
    fn overlaps(self, other: Self) -> bool {
        self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }
}

fn ds_annotation_label_rect(x: f32, y: f32, text: &str, size: f32) -> DsAnnotationRect {
    DsAnnotationRect {
        x,
        y,
        width: text.chars().count() as f32 * size * 0.62 + 8.0,
        height: size * 1.35,
    }
}

fn ds_annotation_project_objects(
    snapshot: &DsAnnotationSnapshot,
    objects: &[DeepSkyCatalogObject],
    request: &DeepSkyAnnotationRequest,
) -> Vec<DeepSkyProjectedAnnotation> {
    let scale_x = snapshot.base.width() as f64 / snapshot.source_width as f64;
    let scale_y = snapshot.base.height() as f64 / snapshot.source_height as f64;
    let width = snapshot.base.width() as f32;
    let height = snapshot.base.height() as f32;
    let mut projected = Vec::new();
    for object in objects {
        let Some((source_x, source_y)) =
            ds_annotation_world_to_pixel(&snapshot.wcs, object.ra_deg, object.dec_deg)
        else {
            continue;
        };
        let x = (source_x * scale_x) as f32;
        let y = (source_y * scale_y) as f32;
        if x < -2.0 || y < -2.0 || x > width + 2.0 || y > height + 2.0 {
            continue;
        }
        projected.push(DeepSkyProjectedAnnotation {
            main_id: object.main_id.clone(),
            label: object.main_id.clone(),
            ra_deg: object.ra_deg,
            dec_deg: object.dec_deg,
            object_type: object.object_type.clone(),
            object_class: ds_annotation_class(&object.object_type).into(),
            major_axis_arcmin: object.major_axis_arcmin,
            minor_axis_arcmin: object.minor_axis_arcmin,
            x,
            y,
            radius_px: ds_annotation_radius_px(object, snapshot),
            label_visible: false,
        });
    }
    projected.sort_by(|a, b| {
        ds_annotation_catalog_priority(&a.label)
            .cmp(&ds_annotation_catalog_priority(&b.label))
            .then_with(|| {
                b.major_axis_arcmin
                    .unwrap_or(0.0)
                    .total_cmp(&a.major_axis_arcmin.unwrap_or(0.0))
            })
            .then_with(|| a.label.cmp(&b.label))
    });

    let label_limit = request
        .max_labels
        .unwrap_or_else(|| request.style.default_label_limit())
        .clamp(1, 300);
    let size = match request.style {
        DeepSkyAnnotationStyle::Focus => 18.0,
        DeepSkyAnnotationStyle::Atlas => 13.0,
        DeepSkyAnnotationStyle::Survey => 14.0,
        DeepSkyAnnotationStyle::Minimal => 15.0,
    };
    let mut occupied = Vec::<DsAnnotationRect>::new();
    let margin = 4.0;
    let mut accepted = 0usize;
    for object in &mut projected {
        if accepted >= label_limit {
            break;
        }
        let x = (object.x + object.radius_px + 7.0).min(width - margin);
        let y = (object.y - size * 0.7).max(margin);
        let rect = ds_annotation_label_rect(x, y, &object.label, size);
        if rect.x + rect.width <= width - margin
            && rect.y + rect.height <= height - margin
            && !occupied.iter().any(|existing| existing.overlaps(rect))
        {
            object.label_visible = true;
            occupied.push(rect);
            accepted += 1;
        }
    }
    projected
}

fn ds_annotation_grid_spacing(radius_deg: f64) -> f64 {
    let diameter = radius_deg * 2.0;
    [0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0]
        .into_iter()
        .find(|spacing| diameter / spacing <= 9.0)
        .unwrap_or(30.0)
}

fn ds_annotation_grid_primitives(
    snapshot: &DsAnnotationSnapshot,
) -> Vec<DeepSkyAnnotationPrimitive> {
    let radius = ds_annotation_field_radius_deg(snapshot);
    let spacing = ds_annotation_grid_spacing(radius);
    let scale_x = snapshot.base.width() as f64 / snapshot.source_width as f64;
    let scale_y = snapshot.base.height() as f64 / snapshot.source_height as f64;
    let width = snapshot.base.width() as f32;
    let height = snapshot.base.height() as f32;
    let color = [136, 165, 220, 100];
    let mut primitives = Vec::new();
    let ra_cos = snapshot.wcs.crval2.to_radians().cos().abs().max(0.15);
    let ra_span = (radius / ra_cos).min(90.0);
    let ra_start = ((snapshot.wcs.crval1 - ra_span) / spacing).floor() as i32;
    let ra_end = ((snapshot.wcs.crval1 + ra_span) / spacing).ceil() as i32;
    for index in ra_start..=ra_end {
        let ra = ds_annotation_normalize_ra(index as f64 * spacing);
        let mut points = Vec::new();
        for sample in 0..=80 {
            let fraction = sample as f64 / 80.0;
            let dec = (snapshot.wcs.crval2 - radius + fraction * radius * 2.0).clamp(-89.9, 89.9);
            if let Some((x, y)) = ds_annotation_world_to_pixel(&snapshot.wcs, ra, dec) {
                let px = (x * scale_x) as f32;
                let py = (y * scale_y) as f32;
                if px >= -width && px <= width * 2.0 && py >= -height && py <= height * 2.0 {
                    points.push([px, py]);
                }
            }
        }
        if points.len() >= 2 {
            primitives.push(DeepSkyAnnotationPrimitive::GridLine {
                points,
                color,
                coordinate: format!("RA {:.2}°", ra),
            });
        }
    }
    let dec_start = ((snapshot.wcs.crval2 - radius) / spacing).floor() as i32;
    let dec_end = ((snapshot.wcs.crval2 + radius) / spacing).ceil() as i32;
    for index in dec_start..=dec_end {
        let dec = index as f64 * spacing;
        if !(-89.9..=89.9).contains(&dec) {
            continue;
        }
        let mut points = Vec::new();
        for sample in 0..=80 {
            let fraction = sample as f64 / 80.0;
            let ra = ds_annotation_normalize_ra(
                snapshot.wcs.crval1 - ra_span + fraction * ra_span * 2.0,
            );
            if let Some((x, y)) = ds_annotation_world_to_pixel(&snapshot.wcs, ra, dec) {
                let px = (x * scale_x) as f32;
                let py = (y * scale_y) as f32;
                if px >= -width && px <= width * 2.0 && py >= -height && py <= height * 2.0 {
                    points.push([px, py]);
                }
            }
        }
        if points.len() >= 2 {
            primitives.push(DeepSkyAnnotationPrimitive::GridLine {
                points,
                color,
                coordinate: format!("Dec {:+.2}°", dec),
            });
        }
    }
    primitives
}

fn ds_annotation_build_primitives(
    snapshot: &DsAnnotationSnapshot,
    objects: &[DeepSkyProjectedAnnotation],
    request: &DeepSkyAnnotationRequest,
) -> Vec<DeepSkyAnnotationPrimitive> {
    let mut primitives = Vec::new();
    if request.include_grid && request.style.includes_grid() {
        primitives.extend(ds_annotation_grid_primitives(snapshot));
    }
    let width = snapshot.base.width() as f32;
    let height = snapshot.base.height() as f32;
    primitives.push(DeepSkyAnnotationPrimitive::Crosshair {
        x: width * 0.5,
        y: height * 0.5,
        radius: 8.0,
        color: [187, 155, 255, 170],
        object_id: None,
    });
    for object in objects {
        let color = ds_annotation_color(&object.object_class, request.style);
        match request.style {
            DeepSkyAnnotationStyle::Minimal => {
                primitives.push(DeepSkyAnnotationPrimitive::Crosshair {
                    x: object.x,
                    y: object.y,
                    radius: object.radius_px.min(13.0),
                    color,
                    object_id: Some(object.main_id.clone()),
                });
            }
            _ => {
                primitives.push(DeepSkyAnnotationPrimitive::Circle {
                    x: object.x,
                    y: object.y,
                    radius: object.radius_px,
                    color,
                    width: if request.style == DeepSkyAnnotationStyle::Focus {
                        2.0
                    } else {
                        1.35
                    },
                    object_id: Some(object.main_id.clone()),
                });
            }
        }
        if object.label_visible {
            let size = match request.style {
                DeepSkyAnnotationStyle::Focus => {
                    if ds_annotation_catalog_priority(&object.label) <= 2 {
                        20.0
                    } else {
                        16.0
                    }
                }
                DeepSkyAnnotationStyle::Atlas => 13.0,
                DeepSkyAnnotationStyle::Survey => 14.0,
                DeepSkyAnnotationStyle::Minimal => 15.0,
            };
            let label_x = object.x + object.radius_px + 7.0;
            let label_y = object.y - size * 0.7;
            if request.style == DeepSkyAnnotationStyle::Focus {
                primitives.push(DeepSkyAnnotationPrimitive::Leader {
                    points: vec![
                        [object.x + object.radius_px * 0.7, object.y],
                        [label_x - 3.0, label_y + size * 0.55],
                    ],
                    color,
                    width: 1.25,
                    object_id: Some(object.main_id.clone()),
                });
            }
            primitives.push(DeepSkyAnnotationPrimitive::Label {
                x: label_x,
                y: label_y,
                text: object.label.clone(),
                color,
                size,
                object_id: Some(object.main_id.clone()),
            });
        }
    }
    primitives
}

fn ds_annotation_blend_pixel(target: &mut image::Rgba<u8>, source: image::Rgba<u8>) {
    let alpha = source[3] as f32 / 255.0;
    let inverse = 1.0 - alpha;
    for channel in 0..3 {
        target[channel] =
            (source[channel] as f32 * alpha + target[channel] as f32 * inverse).round() as u8;
    }
    target[3] = ((source[3] as f32 + target[3] as f32 * inverse).round()).clamp(0.0, 255.0) as u8;
}

fn ds_annotation_overlay_composite(
    base: &image::RgbaImage,
    overlay: &image::RgbaImage,
) -> image::RgbaImage {
    let mut composited = base.clone();
    for (target, source) in composited.pixels_mut().zip(overlay.pixels()) {
        ds_annotation_blend_pixel(target, *source);
    }
    composited
}

fn ds_annotation_draw_polyline(image: &mut image::RgbaImage, points: &[[f32; 2]], color: [u8; 4]) {
    for pair in points.windows(2) {
        let [x0, y0] = pair[0];
        let [x1, y1] = pair[1];
        if (x1 - x0).abs() > image.width() as f32 * 0.6
            || (y1 - y0).abs() > image.height() as f32 * 0.6
        {
            continue;
        }
        imageproc::drawing::draw_antialiased_line_segment_mut(
            image,
            (x0.round() as i32, y0.round() as i32),
            (x1.round() as i32, y1.round() as i32),
            image::Rgba(color),
            imageproc::pixelops::interpolate,
        );
    }
}

fn ds_annotation_draw_primitives(
    width: u32,
    height: u32,
    primitives: &[DeepSkyAnnotationPrimitive],
) -> image::RgbaImage {
    let mut overlay = image::RgbaImage::from_pixel(width, height, image::Rgba([0, 0, 0, 0]));
    let font = get_fallback_font();
    for primitive in primitives {
        match primitive {
            DeepSkyAnnotationPrimitive::Circle {
                x,
                y,
                radius,
                color,
                width,
                ..
            } => {
                let thickness = width.round().clamp(1.0, 3.0) as i32;
                for offset in 0..thickness {
                    imageproc::drawing::draw_hollow_circle_mut(
                        &mut overlay,
                        (x.round() as i32, y.round() as i32),
                        (radius.round() as i32 - offset).max(1),
                        image::Rgba(*color),
                    );
                }
            }
            DeepSkyAnnotationPrimitive::Crosshair {
                x,
                y,
                radius,
                color,
                ..
            } => {
                let gap = 3.0;
                let arms = [
                    [[x - radius, *y], [x - gap, *y]],
                    [[x + gap, *y], [x + radius, *y]],
                    [[*x, y - radius], [*x, y - gap]],
                    [[*x, y + gap], [*x, y + radius]],
                ];
                for arm in arms {
                    ds_annotation_draw_polyline(&mut overlay, &arm, *color);
                }
            }
            DeepSkyAnnotationPrimitive::Label {
                x,
                y,
                text,
                color,
                size,
                ..
            } => {
                if let Some(font) = font.as_ref() {
                    imageproc::drawing::draw_text_mut(
                        &mut overlay,
                        image::Rgba(*color),
                        x.round() as u32,
                        y.round() as u32,
                        rusttype::Scale::uniform(*size),
                        font,
                        text,
                    );
                }
            }
            DeepSkyAnnotationPrimitive::Leader { points, color, .. }
            | DeepSkyAnnotationPrimitive::GridLine { points, color, .. } => {
                ds_annotation_draw_polyline(&mut overlay, points, *color);
            }
        }
    }
    overlay
}

fn ds_annotation_png_bytes(image: &image::RgbaImage) -> Result<Vec<u8>, DeepSkyAnnotationError> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image.clone())
        .write_to(&mut cursor, image::ImageOutputFormat::Png)
        .map_err(|error| {
            DeepSkyAnnotationError::internal(format!(
                "No se pudo codificar la vista de anotaciones: {error}"
            ))
        })?;
    Ok(cursor.into_inner())
}

fn ds_annotation_png_data_url(image: &image::RgbaImage) -> Result<String, DeepSkyAnnotationError> {
    let bytes = ds_annotation_png_bytes(image)?;
    Ok(format!(
        "data:image/png;base64,{}",
        general_purpose::STANDARD.encode(bytes)
    ))
}

fn ds_annotation_generate(
    snapshot: DsAnnotationSnapshot,
    request: DeepSkyAnnotationRequest,
) -> Result<DeepSkyAnnotationPreview, DeepSkyAnnotationError> {
    let started = std::time::Instant::now();
    let (catalog, catalog_source, cache_path, cache_hit, online_attempted, query_fingerprint) =
        ds_annotation_load_catalog(&snapshot, &request)?;
    let response_rows = catalog.len();
    let mut objects = ds_annotation_project_objects(&snapshot, &catalog, &request);
    let projected_rows = objects.len();
    let labels_drawn = objects.iter().filter(|object| object.label_visible).count();
    let labels_decluttered = projected_rows.saturating_sub(labels_drawn);
    let primitives = ds_annotation_build_primitives(&snapshot, &objects, &request);
    let overlay =
        ds_annotation_draw_primitives(snapshot.base.width(), snapshot.base.height(), &primitives);
    let composite = ds_annotation_overlay_composite(&snapshot.base, &overlay);
    let mut warnings = Vec::new();
    if get_fallback_font().is_none() {
        warnings.push(
            "No se encontró una tipografía del sistema; se conservaron marcadores sin texto."
                .into(),
        );
    }
    if catalog.is_empty() && request.include_catalog {
        warnings.push("SIMBAD no devolvió objetos útiles dentro del campo.".into());
    }
    let diagnostics = DeepSkyAnnotationDiagnostics {
        schema_version: DS_ANNOTATION_SCHEMA_VERSION.into(),
        result_id: snapshot.result_id.clone(),
        result_generation: snapshot.generation,
        wcs_source: snapshot.wcs.source.clone(),
        wcs_rms_px: snapshot.wcs.rms_px,
        style_id: request.style.id().into(),
        catalog_source,
        cache_path,
        cache_hit,
        online_attempted,
        query_fingerprint,
        response_rows,
        useful_rows: catalog.len(),
        projected_rows,
        labels_drawn,
        labels_decluttered,
        elapsed_ms: started.elapsed().as_millis(),
        warnings,
        attribution: "Zenith native overlay · catalogue data: SIMBAD/CDS".into(),
    };
    // Keep a deterministic priority order in the public object contract.
    objects.sort_by(|a, b| {
        ds_annotation_catalog_priority(&a.label)
            .cmp(&ds_annotation_catalog_priority(&b.label))
            .then_with(|| a.label.cmp(&b.label))
    });
    Ok(DeepSkyAnnotationPreview {
        result_id: snapshot.result_id,
        source_width: snapshot.source_width,
        source_height: snapshot.source_height,
        preview_width: snapshot.base.width() as usize,
        preview_height: snapshot.base.height() as usize,
        style: request.style,
        style_id: request.style.id().into(),
        composited_preview: ds_annotation_png_data_url(&composite)?,
        transparent_overlay: ds_annotation_png_data_url(&overlay)?,
        objects,
        primitives,
        diagnostics,
    })
}

#[tauri::command(async)]
async fn deepsky_annotations_preview(
    state: State<'_, AppState>,
    request: DeepSkyAnnotationRequest,
) -> Result<DeepSkyAnnotationPreview, DeepSkyAnnotationError> {
    let max_edge = request
        .max_preview_edge
        .clamp(320, DS_ANNOTATION_MAX_PREVIEW_EDGE);
    let snapshot = ds_annotation_snapshot(&state, max_edge)?;
    tauri::async_runtime::spawn_blocking(move || ds_annotation_generate(snapshot, request))
        .await
        .map_err(|error| {
            DeepSkyAnnotationError::internal(format!(
                "El generador de anotaciones se interrumpió: {error}"
            ))
        })?
}

#[tauri::command(async)]
async fn deepsky_annotations_export(
    state: State<'_, AppState>,
    request: DeepSkyAnnotationExportRequest,
) -> Result<DeepSkyAnnotationExportResult, DeepSkyAnnotationError> {
    let path = request.path.trim();
    if path.is_empty() {
        return Err(DeepSkyAnnotationError::recoverable(
            "annotationExportPathRequired",
            "Selecciona dónde guardar la anotación PNG.",
            "Elige un archivo .png dentro de la carpeta de resultados.",
        ));
    }
    let destination = std::path::PathBuf::from(path);
    let max_edge = request
        .annotation
        .max_preview_edge
        .clamp(320, DS_ANNOTATION_MAX_PREVIEW_EDGE);
    let snapshot = ds_annotation_snapshot(&state, max_edge)?;
    let composited = request.composited;
    let annotation = request.annotation;
    tauri::async_runtime::spawn_blocking(move || {
        let preview = ds_annotation_generate(snapshot, annotation)?;
        let data_url = if composited {
            &preview.composited_preview
        } else {
            &preview.transparent_overlay
        };
        let encoded = data_url
            .strip_prefix("data:image/png;base64,")
            .ok_or_else(|| {
                DeepSkyAnnotationError::internal("La vista PNG no tiene un formato válido.")
            })?;
        let bytes = general_purpose::STANDARD.decode(encoded).map_err(|error| {
            DeepSkyAnnotationError::internal(format!(
                "No se pudo decodificar la vista PNG: {error}"
            ))
        })?;
        if destination
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| !value.eq_ignore_ascii_case("png"))
            .unwrap_or(true)
        {
            return Err(DeepSkyAnnotationError::recoverable(
                "annotationExportFormat",
                "Las anotaciones se exportan como PNG para conservar transparencia.",
                "Usa un nombre de archivo terminado en .png.",
            ));
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ds_annotation_error_from_io("No se pudo crear la carpeta de exportación", error)
            })?;
        }
        let temporary =
            destination.with_extension(format!("png.{}.part", new_job_id("annotation-export")));
        {
            let mut file = std::fs::File::create(&temporary).map_err(|error| {
                ds_annotation_error_from_io("No se pudo crear la exportación temporal", error)
            })?;
            std::io::Write::write_all(&mut file, &bytes).map_err(|error| {
                ds_annotation_error_from_io("No se pudo escribir la anotación", error)
            })?;
            file.sync_all().map_err(|error| {
                ds_annotation_error_from_io("No se pudo confirmar la anotación", error)
            })?;
        }
        if destination.exists() {
            #[cfg(target_os = "windows")]
            std::fs::remove_file(&destination).map_err(|error| {
                ds_annotation_error_from_io("No se pudo reemplazar la exportación anterior", error)
            })?;
        }
        if let Err(error) = std::fs::rename(&temporary, &destination) {
            let _ = std::fs::remove_file(&temporary);
            return Err(ds_annotation_error_from_io(
                "No se pudo publicar la anotación atómicamente",
                error,
            ));
        }
        let stored = std::fs::metadata(&destination)
            .map(|metadata| metadata.len())
            .unwrap_or(bytes.len() as u64);
        Ok(DeepSkyAnnotationExportResult {
            path: destination.display().to_string(),
            composited,
            bytes: stored,
            result_id: preview.result_id,
            diagnostics: preview.diagnostics,
        })
    })
    .await
    .map_err(|error| {
        DeepSkyAnnotationError::internal(format!(
            "La exportación de anotaciones se interrumpió: {error}"
        ))
    })?
}

#[cfg(test)]
mod deepsky_annotations_tests {
    use super::*;

    fn wcs(ra: f64) -> AstrometrySolution {
        AstrometrySolution {
            ctype: "RA---TAN/DEC--TAN".into(),
            crval1: ra,
            crval2: 20.0,
            crpix1: 501.0,
            crpix2: 401.0,
            cd11: -0.000_277_777_8,
            cd12: 0.0,
            cd21: 0.0,
            cd22: 0.000_277_777_8,
            rms_px: 0.24,
            inliers: 120,
            handedness: "normal".into(),
            scale_arcsec_px: 1.0,
            source: "localIndex".into(),
            cached: true,
        }
    }

    fn snapshot() -> DsAnnotationSnapshot {
        DsAnnotationSnapshot {
            result_id: "immutable-result".into(),
            generation: 7,
            source_width: 1000,
            source_height: 800,
            base: image::RgbaImage::from_pixel(500, 400, image::Rgba([12, 14, 20, 255])),
            wcs: wcs(120.0),
        }
    }

    fn catalog(id: &str, ra: f64, dec: f64) -> DeepSkyCatalogObject {
        DeepSkyCatalogObject {
            main_id: id.into(),
            ra_deg: ra,
            dec_deg: dec,
            object_type: "G".into(),
            major_axis_arcmin: Some(2.0),
            minor_axis_arcmin: Some(1.0),
            position_angle_deg: Some(30.0),
        }
    }

    #[test]
    fn tan_center_projects_to_zero_based_crpix() {
        let solution = wcs(120.0);
        let (x, y) = ds_annotation_world_to_pixel(&solution, 120.0, 20.0).unwrap();
        assert!((x - 500.0).abs() < 1.0e-8);
        assert!((y - 400.0).abs() < 1.0e-8);
    }

    #[test]
    fn tan_projection_handles_ra_wrap_without_a_discontinuity() {
        let solution = wcs(359.9);
        let (x, y) = ds_annotation_world_to_pixel(&solution, 0.1, 20.0).unwrap();
        assert!(x.is_finite());
        assert!((x - 500.0).abs() < 800.0, "x={x}");
        let (ra, dec) = ds_annotation_pixel_to_world(&solution, x, y).unwrap();
        assert!(ds_annotation_angular_distance_deg(ra, dec, 0.1, 20.0) < 1.0e-6);
    }

    #[test]
    fn declutter_is_deterministic_and_respects_style_limit() {
        let snapshot = snapshot();
        let objects = (0..20)
            .map(|index| catalog(&format!("NGC {}", 1000 + index), 120.0, 20.0))
            .collect::<Vec<_>>();
        let request = DeepSkyAnnotationRequest {
            style: DeepSkyAnnotationStyle::Minimal,
            include_catalog: false,
            max_labels: Some(5),
            ..Default::default()
        };
        let first = ds_annotation_project_objects(&snapshot, &objects, &request);
        let second = ds_annotation_project_objects(&snapshot, &objects, &request);
        let first_labels = first
            .iter()
            .filter(|object| object.label_visible)
            .map(|object| object.label.clone())
            .collect::<Vec<_>>();
        let second_labels = second
            .iter()
            .filter(|object| object.label_visible)
            .map(|object| object.label.clone())
            .collect::<Vec<_>>();
        assert_eq!(first_labels, second_labels);
        assert!(first_labels.len() <= 5);
        // All test objects share a position, so only one label can survive.
        assert_eq!(first_labels.len(), 1);
    }

    #[test]
    fn simbad_cache_json_parses_axes_and_filters_field_stars() {
        let response = serde_json::json!({
            "metadata": [
                {"name": "main_id"},
                {"name": "ra"},
                {"name": "dec"},
                {"name": "otype"},
                {"name": "galdim_majaxis"},
                {"name": "galdim_minaxis"},
                {"name": "galdim_angle"}
            ],
            "data": [
                ["NGC 7000", 312.5, 44.3, "HII", 120.0, 100.0, 12.0],
                ["Gaia DR3 1", 312.4, 44.2, "Star", null, null, null]
            ]
        });
        let objects =
            ds_annotation_parse_simbad_json(serde_json::to_vec(&response).unwrap().as_slice())
                .unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].main_id, "NGC 7000");
        assert_eq!(objects[0].major_axis_arcmin, Some(120.0));
    }

    #[test]
    fn cache_envelope_round_trips_through_the_atomic_local_store() {
        let root = std::env::temp_dir().join(new_job_id("zenith-annotation-cache-test"));
        std::fs::create_dir_all(&root).unwrap();
        let fingerprint = "abc123";
        let path = root.join("simbad-abc123.json");
        let envelope = DsAnnotationCacheEnvelope {
            schema_version: DS_ANNOTATION_SCHEMA_VERSION.into(),
            endpoint: DS_ANNOTATION_SIMBAD_TAP.into(),
            query_fingerprint: fingerprint.into(),
            fetched_at_utc: "2026-07-30T00:00:00Z".into(),
            objects: vec![catalog("NGC 7000", 312.5, 44.3)],
        };
        ds_annotation_write_cache_atomic(&path, &envelope).unwrap();
        let restored = ds_annotation_read_cache(&path, fingerprint)
            .unwrap()
            .unwrap();
        assert_eq!(restored.query_fingerprint, fingerprint);
        assert_eq!(restored.objects, envelope.objects);
        assert!(
            std::fs::read_dir(&root).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part")),
            "el publicador atómico no debe dejar temporales"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn complete_preview_generation_does_not_mutate_snapshot_pixels_or_wcs() {
        let snapshot = snapshot();
        let before = snapshot.base.clone().into_raw();
        let wcs_before = snapshot.wcs.clone();
        let request = DeepSkyAnnotationRequest {
            include_catalog: false,
            style: DeepSkyAnnotationStyle::Atlas,
            ..Default::default()
        };
        let preview = ds_annotation_generate(snapshot.clone(), request).unwrap();
        assert!(preview
            .composited_preview
            .starts_with("data:image/png;base64,"));
        assert!(preview
            .transparent_overlay
            .starts_with("data:image/png;base64,"));
        assert_eq!(snapshot.base.clone().into_raw(), before);
        assert_eq!(snapshot.wcs, wcs_before);
    }

    #[test]
    fn every_zenith_style_has_a_stable_public_identifier() {
        assert_eq!(DeepSkyAnnotationStyle::Atlas.id(), "zenithAtlas");
        assert_eq!(DeepSkyAnnotationStyle::Survey.id(), "zenithSurvey");
        assert_eq!(DeepSkyAnnotationStyle::Focus.id(), "zenithFocus");
        assert_eq!(DeepSkyAnnotationStyle::Minimal.id(), "zenithMinimal");
    }
}
