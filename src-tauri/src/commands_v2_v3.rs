// ==========================================
// 7. ZENITH V2 IMPLEMENTATION (NEW)
// ==========================================
/// Cancellation belongs to one planetary job, not to the process-wide boolean.
/// A new analysis deliberately clears `cancel_requested`; therefore producers
/// must also observe that their request id is no longer active.  The closure
/// keeps this type testable without changing AppState's public layout.
#[derive(Clone)]
struct PlanetaryJobToken {
    request_id: usize,
    cancel_requested: Arc<std::sync::atomic::AtomicBool>,
    active_request: Arc<dyn Fn() -> usize + Send + Sync>,
}

impl PlanetaryJobToken {
    fn for_app(
        app: &tauri::AppHandle,
        request_id: usize,
        cancel_requested: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        let app = app.clone();
        Self {
            request_id,
            cancel_requested,
            active_request: Arc::new(move || {
                app.state::<AppState>()
                    .active_req_id
                    .load(Ordering::Acquire)
            }),
        }
    }

    #[cfg(test)]
    fn for_test(
        request_id: usize,
        active_request: Arc<AtomicUsize>,
        cancel_requested: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            request_id,
            cancel_requested,
            active_request: Arc::new(move || active_request.load(Ordering::Acquire)),
        }
    }

    #[inline]
    fn is_cancelled(&self) -> bool {
        self.cancel_requested.load(Ordering::Acquire)
            || (self.active_request)() != self.request_id
    }
}

/// Allocate a generation without changing the session-wide cancellation flag.
/// Used for phases that continue an existing batch: a cancellation between
/// phases must remain sticky.
#[inline]
fn next_planetary_generation(state: &AppState) -> usize {
    let _generation_guard = state
        .planetary_generation_gate
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    state
        .active_req_id
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1)
}

/// Start an explicit user operation. The first increment invalidates every
/// older worker before the global flag is re-armed; the second generation is
/// owned exclusively by the new job. This prevents the old 1→0→1 ABA race.
fn begin_planetary_user_job(state: &AppState) -> usize {
    let _generation_guard = state
        .planetary_generation_gate
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _ = state.active_req_id.fetch_add(1, Ordering::AcqRel);
    state.cancel_requested.store(false, Ordering::Release);
    state
        .active_req_id
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1)
}

#[inline]
fn cancel_planetary_jobs(state: &AppState) {
    let _generation_guard = state
        .planetary_generation_gate
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _ = state.active_req_id.fetch_add(1, Ordering::AcqRel);
    state.cancel_requested.store(true, Ordering::Release);
    // F8: el cancel global también barre los trabajos registrados por id
    // (stacks deep-sky NF; en el futuro, solvers EIDR).
    state.job_registry.cancel_all();
    state.deconv_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    state.wavelet_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    state.filter_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[derive(Debug, PartialEq, Eq)]
enum StreamDecodeOutcome {
    Complete { decoded: usize },
    Cancelled { decoded: usize },
    Failed { decoded: usize, error: String },
}

/// A streamed analysis may only consume a prefix when that prefix ended in a
/// clean EOF and the container's frame count was explicitly marked as an
/// estimate. Decoder errors and short authoritative streams are never accepted
/// merely because they produced at least one frame.
fn validate_stream_decode(
    outcome: StreamDecodeOutcome,
    expected_frames: usize,
    expected_is_exact: bool,
) -> Result<usize, String> {
    match outcome {
        StreamDecodeOutcome::Cancelled { decoded } => Err(format!(
            "Análisis cancelado después de decodificar {decoded} frames"
        )),
        StreamDecodeOutcome::Failed { decoded, error } => Err(format!(
            "FFmpeg falló después de {decoded} frames completos: {error}"
        )),
        StreamDecodeOutcome::Complete { decoded } if decoded == 0 => {
            Err("FFmpeg terminó sin entregar frames completos".into())
        }
        StreamDecodeOutcome::Complete { decoded }
            if expected_is_exact && decoded != expected_frames =>
        {
            Err(format!(
                "FFmpeg entregó {decoded} frames; el contenedor declara exactamente {expected_frames}"
            ))
        }
        StreamDecodeOutcome::Complete { decoded } => Ok(decoded),
    }
}

/// Resolve reference candidates by absolute decode position. FFmpeg seeks by
/// `index / fps` are not frame-exact for VFR/B-frame material. One ascending
/// batch walks the stream once and preserves candidate indices; if an estimated
/// frame count points past clean EOF, frame zero remains an exact safe seed.
/// PR-22: extrae UN frame cercano al índice pedido con `-ss` (seek rápido a
/// keyframe + decode de un solo GOP). No garantiza el índice exacto — vale
/// para usos NO científicos (frame ancla del análisis, preview) donde un
/// vecino a ±unos frames es equivalente y el camino exacto costaba minutos
/// (144 s el ancla + 122 s la preview a 20MP HEVC, baseline 2026-07-17).
fn ffmpeg_fast_seek_frame(
    reader: &FfmpegReader,
    path: &str,
    width: usize,
    height: usize,
    color_id: i32,
    index: usize,
) -> Option<Vec<u8>> {
    let fps = if reader.fps.is_finite() && reader.fps > 0.1 {
        reader.fps
    } else {
        30.0
    };
    // Margen de 0.2 s antes del instante pedido: garantiza material aunque el
    // demuxer redondee el seek hacia delante.
    let seconds = ((index as f64 / fps) - 0.2).max(0.0);
    let stream_bpp = if ffmpeg_stream_is_color(color_id) { 6 } else { 2 };
    let mut stream = FfmpegStreamIterator::new(
        path,
        width,
        height,
        0,
        0,
        width,
        height,
        color_id,
        &reader.ffmpeg_path,
        Some(format!("{seconds:.3}")),
        None,
        &reader.codec_name,
        reader.rotation,
    )
    .ok()?;
    let mut raw = vec![0u8; width * height * stream_bpp];
    stream.read_frame_into(&mut raw).then_some(raw)
}

/// PR-22 (cambio de flujo aprobado 2026-07-16; ZAS_EXACT_ANCHOR=1 lo
/// revierte): elegir el frame ANCLA muestreando por seek en vez de
/// decodificar 0..tf/2 con select exacto. El ancla es una imagen de
/// referencia — un frame real cercano al instante pedido es igual de válido
/// que el del índice exacto.
fn select_signal_frame_fast_seek(
    r: &VideoInput,
    path: &str,
    width: usize,
    height: usize,
    bpp: usize,
    color_id: i32,
    total: usize,
    preferred_idx: usize,
) -> Option<(usize, Vec<u8>)> {
    if std::env::var("ZAS_EXACT_ANCHOR").ok().as_deref() == Some("1") {
        return None;
    }
    let VideoInput::Ffmpeg(ref reader) = r else {
        return None;
    };
    let total = total.max(1);
    let preferred = preferred_idx.min(total - 1);
    let mut indices = vec![
        0,
        preferred,
        (total / 10).min(total - 1),
        (total / 4).min(total - 1),
    ];
    indices.sort_unstable();
    indices.dedup();

    let mut best: Option<(usize, f32, Vec<u8>)> = None;
    for index in indices {
        let Some(raw) = ffmpeg_fast_seek_frame(reader, path, width, height, color_id, index)
        else {
            continue;
        };
        let (max_v, avg) = estimate_raw_frame_signal(&raw, width, height, bpp);
        let score = max_v as f32 + avg * 8.0;
        if best.as_ref().map_or(true, |candidate| score > candidate.1) {
            best = Some((index, score, raw));
        }
    }
    best.map(|(index, _, raw)| (index, raw))
}

fn select_signal_frame_from_source(
    source: &UnifiedFrameSource,
    width: usize,
    height: usize,
    bpp: usize,
    preferred_idx: usize,
) -> Result<(usize, Vec<u8>), String> {
    let total = source.descriptor().frame_count.max(1);
    let preferred = preferred_idx.min(total - 1);
    let mut indices = vec![
        0,
        preferred,
        (total / 10).min(total - 1),
        (total / 4).min(total - 1),
    ];
    indices.sort_unstable();
    indices.dedup();

    let batch = match source.read_batch(&indices, None) {
        Ok(batch) => batch,
        Err(_) => source.read_batch(&[0], None)?,
    };
    let mut best: Option<(usize, f32, Vec<u8>)> = None;
    for (index, raw) in batch.indices.into_iter().zip(batch.frames) {
        let (max_v, avg) = estimate_raw_frame_signal(&raw, width, height, bpp);
        let score = max_v as f32 + avg * 8.0;
        if best.as_ref().map_or(true, |candidate| score > candidate.1) {
            best = Some((index, score, raw));
        }
    }
    best.map(|(index, _, raw)| (index, raw))
        .ok_or_else(|| "El origen no entregó candidatos de referencia".into())
}

fn read_exact_source_frame(source: &UnifiedFrameSource, index: usize) -> Result<Vec<u8>, String> {
    source
        .read_batch(&[index], None)?
        .frames
        .into_iter()
        .next()
        .ok_or_else(|| format!("El origen no entregó el frame exacto {index}"))
}

fn require_native_analysis_frame(
    result: Result<frame_source::FrameBatch, String>,
    index: usize,
) -> Result<Vec<u8>, String> {
    let mut batch = result.map_err(|error| {
        format!("No se pudo leer el frame nativo {index} para análisis: {error}")
    })?;
    if batch.indices.as_slice() != [index] {
        return Err(format!(
            "El lector nativo respondió índices {:?} al solicitar [{index}]",
            batch.indices
        ));
    }
    batch
        .frames
        .pop()
        .filter(|frame| !frame.is_empty())
        .ok_or_else(|| format!("El lector nativo no entregó bytes para el frame {index}"))
}

// ROBUST GEOMETRIC CENTERING (REPLACES COG FOR PLANETARY)
// Computes Bounding Box center ignoring 1% mass tails (noise).
fn is_small_planet(target_type: &str) -> bool {
    let t = target_type.to_lowercase();
    t.contains("planet") || t.contains("peque") || t.contains("small") || t.contains("grande") || t.contains("large")
}

fn is_surface_target(target_type: &str) -> bool {
    let t = target_type.to_lowercase();
    t.contains("superficie") || t.contains("surface") || t.contains("luna") || t.contains("sol")
}

fn zenith_target_key(target_type: &str, is_surface: bool) -> &'static str {
    if is_surface || is_surface_target(target_type) {
        "surface"
    } else {
        "planet_small"
    }
}

fn zenith_should_warp(_target_type: &str, _is_surface: bool, requested: bool) -> bool {
    // R11: planets now run the SAME per-AP warping pipeline as surface
    // (local seeing correction + per-AP frame selection + double pass).
    // The old rule force-disabled warping for anything containing "planet",
    // leaving Disco with global-only alignment.
    requested
}

fn requested_analysis_roi(
    width: usize,
    height: usize,
    target_type: &str,
    is_surface: bool,
    warping_analysis: bool,
    anchor_override: Option<&[i32]>,
) -> Rect {
    let (mut w, mut h) = if is_surface || warping_analysis {
        (width, height)
    } else if is_small_planet(target_type) {
        (width.min(512), height.min(512))
    } else {
        (width.min(800), height.min(800))
    };
    let (mut x, mut y) = ((width - w) / 2, (height - h) / 2);
    if let Some(anchor) = anchor_override.filter(|anchor| anchor.len() >= 2) {
        w = 256usize.min(width);
        h = 256usize.min(height);
        let cx = anchor[0].max(0) as usize;
        let cy = anchor[1].max(0) as usize;
        x = cx.saturating_sub(w / 2).min(width - w);
        y = cy.saturating_sub(h / 2).min(height - h);
    }
    Rect { x, y, w, h }
}

/// YUV422 comparte una muestra U/V por cada pareja horizontal. Recortar desde
/// una x impar (o terminar a mitad de pareja) cambia la interpretación de todos
/// los words siguientes. Mantener el ROI sobre parejas completas hace que la
/// referencia, cada frame analizado y el apilado vean exactamente los mismos
/// píxeles BT.601.
fn align_yuv422_analysis_roi(roi: Rect, full_width: usize) -> Rect {
    let paired_width = full_width & !1;
    if paired_width < 2 {
        return roi;
    }

    let x = (roi.x.min(paired_width - 2)) & !1;
    let requested_end = roi.x.saturating_add(roi.w).min(paired_width);
    let end = (requested_end.saturating_add(1) & !1).clamp(x + 2, paired_width);
    Rect {
        x,
        y: roi.y,
        w: end - x,
        h: roi.h,
    }
}

fn normalized_analysis_target(target_type: &str) -> String {
    target_type.trim().to_lowercase()
}

fn zenith_analysis_cache_suffix(
    target_type: &str,
    is_surface: bool,
    warping_analysis: bool,
    resolved_color_id: i32,
    anchor_override: Option<&[i32]>,
    requested_roi: Rect,
) -> String {
    let flow = if warping_analysis { "warp" } else { "global" };
    // "_a10" persiste y valida el contrato completo. El sufijo incluye los
    // valores que cambian los píxeles analizados, no sólo `anchor.is_some()`:
    // CFA efectivo, ROI inicial exacta, coordenadas completas del ancla y el
    // target normalizado (hash estable y compacto para nombres portables).
    // "_a9" = verde canónico MHC para Bayer y G nativo para RGB/BGR.
    // "_a8" = decode completo validado + referencia FFmpeg por
    // índice absoluto. Invalida cachés a7 que podían contener un prefijo tras
    // un decoder truncado. "_a7" añadió el scorer v2 (Laplaciano CRUDO multi-escala,
    // normalizado por área/brillo) + calidad local por caja de AP. Los
    // scores v6 no son comparables (el scorer v1 puntuaba el mapa
    // autonormalizado e invertía el ranking planetario). ("_a6" añadió el
    // fingerprint obligatorio de origen/algoritmo; "_a5", CoG pre-centering.)
    let target_hash = crc32fast::hash(normalized_analysis_target(target_type).as_bytes());
    let anchor_tag = anchor_override
        .filter(|anchor| !anchor.is_empty())
        .map(|anchor| {
            anchor
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join("x")
        })
        .unwrap_or_else(|| "none".into());
    let mut suffix = format!(
        "{}_zenith_ultimate_{}_a10_c{}_roi{}-{}-{}-{}_anc{}_t{:08x}",
        zenith_target_key(target_type, is_surface),
        flow,
        resolved_color_id,
        requested_roi.x,
        requested_roi.y,
        requested_roi.w,
        requested_roi.h,
        anchor_tag,
        target_hash,
    );
    // Contrato radiométrico YUV422: los cachés a10 antiguos contenían words
    // Y+U/Y+V tratados como intensidad mono. Sólo YUV necesita invalidación;
    // los demás formatos conservan sus cachés bit-idénticos.
    if ser::ser_color_is_yuv422(resolved_color_id) {
        suffix.push_str("_yuvg1");
    }
    // Mantener `mut` permite añadir tags futuros sin cambiar el orden estable.
    suffix.shrink_to_fit();
    suffix
}

#[derive(Clone, Debug)]
struct AnalysisCacheExpectation {
    source_fingerprint: u64,
    resolved_color_id: i32,
    target_type: String,
    is_surface: bool,
    warping_analysis: bool,
    anchor_override: Option<Vec<i32>>,
    requested_roi: Rect,
    width: usize,
    height: usize,
    declared_frame_count: usize,
    frame_count_exact: bool,
}

impl AnalysisCacheExpectation {
    fn contract(&self, resolved_roi: Rect) -> AnalysisCacheContract {
        AnalysisCacheContract {
            schema_version: ANALYSIS_CACHE_SCHEMA_VERSION,
            resolved_color_id: self.resolved_color_id,
            target_type: self.target_type.clone(),
            is_surface: self.is_surface,
            warping_analysis: self.warping_analysis,
            anchor_override: self.anchor_override.clone(),
            requested_roi: self.requested_roi,
            resolved_roi,
            width: self.width,
            height: self.height,
            declared_frame_count: self.declared_frame_count,
            frame_count_exact: self.frame_count_exact,
        }
    }
}

fn validate_analysis_cache(
    cached: &CachedAnalysis,
    expected: &AnalysisCacheExpectation,
) -> Result<(), String> {
    if cached.path_hash != expected.source_fingerprint
        || cached.width != Some(expected.width)
        || cached.height != Some(expected.height)
    {
        return Err("fingerprint o geometría no coinciden".into());
    }
    let contract = cached
        .contract
        .as_ref()
        .ok_or("falta el contrato a10")?;
    if contract != &expected.contract(contract.resolved_roi) {
        return Err("los parámetros del análisis no coinciden".into());
    }
    if cached.roi != contract.resolved_roi
        || cached.roi.w == 0
        || cached.roi.h == 0
        || cached.roi.x < expected.requested_roi.x
        || cached.roi.y < expected.requested_roi.y
        || cached.roi.x.saturating_add(cached.roi.w)
            > expected.requested_roi.x.saturating_add(expected.requested_roi.w)
        || cached.roi.y.saturating_add(cached.roi.h)
            > expected.requested_roi.y.saturating_add(expected.requested_roi.h)
    {
        return Err("ROI resuelta inválida o ajena a la solicitud".into());
    }
    let stats = cached
        .frame_stats
        .as_ref()
        .ok_or("faltan estadísticas por frame")?;
    if stats.is_empty() {
        return Err("secuencia de análisis vacía".into());
    }
    for (expected_idx, frame) in stats.iter().enumerate() {
        if frame.idx != expected_idx || frame.frame_idx != expected_idx {
            return Err(format!(
                "índices no contiguos en posición {expected_idx}: idx={}, frame_idx={}",
                frame.idx, frame.frame_idx
            ));
        }
    }
    if expected.frame_count_exact && stats.len() != expected.declared_frame_count {
        return Err(format!(
            "conteo autoritativo incumplido: caché={}, origen={}",
            stats.len(), expected.declared_frame_count
        ));
    }
    let quality = cached
        .quality_graph
        .as_ref()
        .ok_or("falta la gráfica de calidad")?;
    if quality.len() != stats.len() {
        return Err("la gráfica y las estadísticas tienen longitudes distintas".into());
    }
    let best = cached.best_frame_idx.ok_or("falta el mejor frame")?;
    if best >= stats.len() {
        return Err("el mejor frame queda fuera de la secuencia contigua".into());
    }
    Ok(())
}

/// Busca primero junto a la captura y después en el caché privado temporal.
/// Cada candidato se valida con el mismo contrato completo: un primario
/// corrupto/obsoleto no impide recuperar el fallback válido de solo lectura.
fn load_validated_analysis_cache(
    primary_cache_path: &str,
    expected: &AnalysisCacheExpectation,
) -> Option<(CachedAnalysis, String)> {
    analysis_cache_candidates(primary_cache_path)
        .into_iter()
        .find_map(|candidate| {
            let cached = load_cached_analysis(&candidate)?;
            validate_analysis_cache(&cached, expected).ok()?;
            Some((cached, candidate))
        })
}

/// Preview estable asociada al contrato/fingerprint del análisis. Un hit de
/// caché no debe volver a atravesar un GOP hasta `best_frame_idx`, convertir un
/// RGB de 20 MP y recomprimir un PNG de varios MiB sólo para mostrar lo mismo.
fn analysis_preview_cache_path(cache_path: &str, source_fingerprint: u64) -> PathBuf {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(cache_path.as_bytes());
    hasher.update(source_fingerprint.to_le_bytes());
    std::env::temp_dir()
        .join("astro_stacker_previews")
        .join(format!("analysis_cache_{}.png", hex::encode(hasher.finalize())))
}

fn load_analysis_preview_cache(cache_path: &str, source_fingerprint: u64) -> Option<String> {
    use std::io::Read;
    let path = analysis_preview_cache_path(cache_path, source_fingerprint);
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() || !(8..=512 * 1024 * 1024).contains(&metadata.len()) {
        return None;
    }
    let mut signature = [0u8; 8];
    std::fs::File::open(&path)
        .ok()?
        .read_exact(&mut signature)
        .ok()?;
    if signature != [137, 80, 78, 71, 13, 10, 26, 10] {
        return None;
    }
    Some(clean_windows_path(path))
}

fn save_analysis_preview_cache(
    png: &[u8],
    cache_path: &str,
    source_fingerprint: u64,
) -> Option<String> {
    if !png.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]) {
        return None;
    }
    let path = analysis_preview_cache_path(cache_path, source_fingerprint);
    std::fs::create_dir_all(path.parent()?).ok()?;
    let sequence = DECODE_CACHE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("png.tmp-{}-{sequence}", std::process::id()));
    std::fs::write(&temporary, png).ok()?;
    if path.exists() {
        let _ = std::fs::remove_file(&temporary);
    } else if std::fs::rename(&temporary, &path).is_err() {
        let _ = std::fs::remove_file(&temporary);
        return None;
    }
    load_analysis_preview_cache(cache_path, source_fingerprint)
}

#[derive(Debug)]
struct AnalysisCachePublishError {
    message: String,
    superseded: bool,
}

impl AnalysisCachePublishError {
    fn optional(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            superseded: false,
        }
    }

    fn superseded(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            superseded: true,
        }
    }
}

fn publish_analysis_cache_guarded(
    state: &tauri::State<'_, AppState>,
    job_token: &PlanetaryJobToken,
    cache_path: &str,
    cached: &CachedAnalysis,
    expected: &AnalysisCacheExpectation,
) -> Result<(), AnalysisCachePublishError> {
    if job_token.is_cancelled() {
        return Err(AnalysisCachePublishError::superseded(
            "Cancelado o sustituido antes de preparar el caché de análisis",
        ));
    }
    let temporary = stage_cached_analysis(cache_path, cached)
        .map_err(AnalysisCachePublishError::optional)?;

    // Releer/descomprimir/validar puede costar segundos para una captura larga.
    // Se hace FUERA del gate: Cancel, clear y un job nuevo nunca esperan a que
    // termine esta comprobación ni sufren el doble pico de RAM bajo el lock.
    let staged = fs::read(&temporary)
        .ok()
        .and_then(|raw| parse_cached_analysis(&raw))
        .ok_or_else(|| {
            let _ = fs::remove_file(&temporary);
            AnalysisCachePublishError::optional(
                "El caché temporal no superó la lectura de validación",
            )
        })?;
    if let Err(error) = validate_analysis_cache(&staged, expected) {
        let _ = fs::remove_file(&temporary);
        return Err(AnalysisCachePublishError::optional(format!(
            "El caché temporal no superó validación: {error}"
        )));
    }

    {
        let _generation_guard = state
            .planetary_generation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if job_token.is_cancelled() {
            let _ = fs::remove_file(&temporary);
            return Err(AnalysisCachePublishError::superseded(
                "Cancelado o sustituido antes de publicar el análisis",
            ));
        }
        // El lock cubre la última comprobación de propiedad y el rename: una
        // cancelación/supersession no puede intercalarse entre ambos.
        commit_staged_analysis(cache_path, &temporary)
            .map_err(AnalysisCachePublishError::optional)?;
    }
    prune_stale_analysis_caches(cache_path);
    Ok(())
}

/// El primario junto al video es best-effort; el fallback privado sí garantiza
/// que el análisis devuelto pueda ser consumido por el siguiente apilado. Una
/// SD/NAS de solo lectura funciona normalmente. Si tampoco se puede escribir
/// el temp de la aplicación, se devuelve un error explícito en vez de prometer
/// un análisis que el stack no podría abrir.
fn publish_analysis_cache_best_effort(
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
    job_token: &PlanetaryJobToken,
    cache_path: &str,
    cached: &CachedAnalysis,
    expected: &AnalysisCacheExpectation,
) -> Result<(), String> {
    let candidates = analysis_cache_candidates(cache_path);
    let mut failures = Vec::new();
    for (position, candidate) in candidates.iter().enumerate() {
        if position > 0 {
            if let Some(parent) = Path::new(candidate).parent() {
                if let Err(error) = fs::create_dir_all(parent) {
                    failures.push(format!("fallback '{}': {error}", parent.display()));
                    continue;
                }
            }
        }
        match publish_analysis_cache_guarded(state, job_token, candidate, cached, expected) {
            Ok(()) => {
                if position > 0 {
                    log_to_front(
                        app,
                        "WARN",
                        "La captura es de solo lectura; el análisis se conservó en el caché privado de la aplicación.",
                    );
                }
                return Ok(());
            }
            Err(error) if error.superseded || job_token.is_cancelled() => {
                return Err(error.message)
            }
            Err(error) => failures.push(error.message),
        }
    }
    Err(format!(
        "El análisis terminó, pero no pudo guardarse ni junto a la captura ni en el caché privado; no se iniciará un apilado inconsistente: {}",
        failures.join(" · ")
    ))
}

fn zenith_recommended_pct(target_type: &str, is_surface: bool) -> f32 {
    if is_surface || is_surface_target(target_type) {
        15.0
    } else {
        12.0
    }
}

/// SMART STACK PERCENTAGE: suggest the fraction of frames that yields the best
/// sharpness/SNR tradeoff. Two independent estimators, take the stricter:
///  (a) ABSOLUTE QUALITY: fraction of frames holding ≥62% of the best score —
///      steady seeing earns more, turbulence earns less.
///  (b) KNEE OF THE SORTED CURVE: sort scores best→worst and find the point of
///      maximum sag below the straight line between the endpoints — that is
///      where quality starts falling faster than frames are being added (the
///      classic "elbow"). Past the knee, extra frames blur more than they
///      denoise. Skipped when the curve is nearly linear (no clear knee).
/// Clamped to a sane lucky-imaging range; static default for tiny graphs.
fn compute_smart_stack_pct(quality_graph: &[f32], target_type: &str, is_surface: bool) -> f32 {
    if quality_graph.len() < 8 {
        return zenith_recommended_pct(target_type, is_surface);
    }
    let n = quality_graph.len();

    // (a) absolute-quality fraction
    let good = quality_graph.iter().filter(|&&v| v >= 62.0).count() as f32;
    let pct_quality = good / n as f32 * 100.0;

    // (b) knee of the sorted (descending) quality curve
    let mut sorted: Vec<f32> = quality_graph.to_vec();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let v0 = sorted[0];
    let vn = sorted[n - 1];
    let mut knee_idx = n - 1;
    let mut max_sag = 0.0f32;
    for (i, &v) in sorted.iter().enumerate() {
        let chord = v0 + (vn - v0) * i as f32 / (n - 1) as f32;
        let sag = chord - v; // curve below the chord = accelerating quality loss
        if sag > max_sag {
            max_sag = sag;
            knee_idx = i;
        }
    }
    // A sag under ~3 quality points means the curve is basically linear.
    let pct_knee = if max_sag >= 3.0 {
        (knee_idx + 1) as f32 / n as f32 * 100.0
    } else {
        100.0
    };

    pct_quality.min(pct_knee).clamp(8.0, 50.0)
}

fn compute_robust_geometric_center(
    mono: &[u16],
    w: usize,
    h: usize,
    offset_x: usize,
    offset_y: usize,
) -> (f32, f32) {
    let mut proj_x = vec![0.0f32; w];
    let mut proj_y = vec![0.0f32; h];

    // 1. Noise Floor Estimation (Median of small samples from the FOUR corners;
    // a single corner is biased when the disk drifts into it)
    let mut noise_sample = Vec::with_capacity(200);
    let block = 10usize.min(w.max(1)).min(h.max(1));
    for &by in &[0usize, h.saturating_sub(block)] {
        for &bx in &[0usize, w.saturating_sub(block)] {
            for y in (by..(by + block).min(h)).step_by(2) {
                for x in (bx..(bx + block).min(w)).step_by(2) {
                    noise_sample.push(mono[y * w + x]);
                }
            }
        }
    }
    noise_sample.sort_unstable();
    let noise_floor = noise_sample.get(noise_sample.len() / 2).cloned().unwrap_or(0) as f32;

    // 2. Adaptive thresholding: find peak value
    let mut max_val = 0u16;
    for &v in mono.iter().step_by(4) {
        if v > max_val { max_val = v; }
    }
    
    // We compute the center at 3 different thresholds and average them.
    // This makes the center extremely robust to limb darkening and phase effects.
    let thresholds = [0.12, 0.25, 0.45];
    let mut centers = Vec::with_capacity(3);

    for &t_pct in &thresholds {
        let threshold_val = (noise_floor + (max_val as f32 - noise_floor).max(0.0) * t_pct).max(noise_floor + 100.0);
        let mut total_mass = 0.0;
        proj_x.fill(0.0);
        proj_y.fill(0.0);

        for y in 0..h {
            let row_offset = y * w;
            for x in 0..w {
                let val = mono[row_offset + x] as f32;
                if val > threshold_val {
                    // CUBIC WEIGHTING: Extremely aggressive focus on the planet's core
                    // for maximum tracking stability in turbulent seeing.
                    let signal = val - threshold_val;
                    let weight = signal * signal * signal;
                    proj_x[x] += weight;
                    proj_y[y] += weight;
                    total_mass += weight;
                }
            }
        }

        if total_mass > 0.0 {
            let mass_threshold = total_mass * 0.005; // 0.5% mass tail rejection
            
            let mut min_x = 0;
            let mut acc = 0.0;
            for x in 0..w {
                acc += proj_x[x];
                if acc > mass_threshold { min_x = x; break; }
            }
            let mut max_x = w.saturating_sub(1);
            acc = 0.0;
            for x in (0..w).rev() {
                acc += proj_x[x];
                if acc > mass_threshold { max_x = x; break; }
            }
            let mut min_y = 0;
            acc = 0.0;
            for y in 0..h {
                acc += proj_y[y];
                if acc > mass_threshold { min_y = y; break; }
            }
            let mut max_y = h.saturating_sub(1);
            acc = 0.0;
            for y in (0..h).rev() {
                acc += proj_y[y];
                if acc > mass_threshold { max_y = y; break; }
            }
            centers.push(((min_x + max_x) as f32 / 2.0, (min_y + max_y) as f32 / 2.0));
        }
    }

    if centers.is_empty() {
        return (offset_x as f32 + w as f32 / 2.0, offset_y as f32 + h as f32 / 2.0);
    }

    let avg_x: f32 = centers.iter().map(|c| c.0).sum::<f32>() / centers.len() as f32;
    let avg_y: f32 = centers.iter().map(|c| c.1).sum::<f32>() / centers.len() as f32;

    (offset_x as f32 + avg_x, offset_y as f32 + avg_y)
}

/// **Large lunar-disc detector.**
/// Brightness CoG centering (`compute_robust_geometric_center`) is rock-solid for
/// a small, compact planet on a black sky, but it WOBBLES frame-to-frame on a big
/// lunar disc: the phase terminator and bright rayed craters (Tycho/Copernicus)
/// shift the brightness centroid as seeing/transparency vary, so the frames never
/// register consistently and the stack comes out soft. Such targets must instead
/// be aligned on SURFACE TEXTURE (the Superficie path). This returns `true` when
/// the lit disc fills a large fraction of the frame (Moon / large lunar phase),
/// and `false` for the tiny bright blob of a real planet — keeping small planets
/// on the proven CoG path.
fn is_large_lunar_disc(data: &[u16], w: usize, h: usize) -> bool {
    if data.len() < w * h || w < 24 || h < 24 {
        return false;
    }

    // FRAMING-INDEPENDENT detection via sampled percentiles. The first version
    // estimated the sky level from the four CORNER blocks — but a Moon close-up
    // that overflows the frame puts 3 of 4 corners ON the lit disc, so the
    // "noise floor" landed at lunar brightness, the threshold went sky-high and
    // the fill fraction came out tiny → large_disc=false → planetary CoG on a
    // partial disc (garbage centering) AND no normalization (dark result).
    // P5 lands on the sky when any sky exists, or on dark maria when the disc
    // fills everything — either way the lit fraction is measured correctly.
    let step = (data.len() / 200_000).max(1);
    let mut sample: Vec<u16> = data.iter().step_by(step).copied().collect();
    if sample.len() < 64 {
        return false;
    }
    sample.sort_unstable();
    let p05 = sample[sample.len() * 5 / 100] as f32;
    let p999 = sample[(sample.len() * 999 / 1000).min(sample.len() - 1)] as f32;
    let span = (p999 - p05).max(1.0);

    // Degenerate but real close-up case: the lit disc fills (nearly) the WHOLE
    // frame, so even P5 lands on the disc and the span collapses. A bright,
    // near-uniform frame with no sky IS a large disc — a planet on black sky
    // always keeps a huge span (sky P5 vs disc P99.9), and an empty/noise frame
    // fails the brightness bar.
    if span < p999 * 0.15 && p999 > 2000.0 {
        return true;
    }

    // ~10% above the dark level captures the lit lunar surface (incl. maria)
    // without counting background noise.
    let threshold = p05 + span * 0.10;

    let lit = sample.iter().filter(|&&v| (v as f32) > threshold).count();
    let fill = lit as f32 / sample.len() as f32;

    // Moon / large lunar phase fills a big chunk of the frame; a planet a tiny one.
    fill > 0.22
}

/// COMPACT OBJECT ON BLACK SKY (handheld/phone-video scene): a bright disc
/// occupying a SMALL fraction of a dominantly black frame. In that scene the
/// brightness centroid is a rock-solid, range-unlimited motion prior — used to
/// PRE-CENTER the SAD search so violent handheld motion (hundreds of px, far
/// beyond any fixed search window) still aligns. Distinct from
/// `is_large_lunar_disc`: a big disc has texture everywhere and small relative
/// motion, so it neither needs nor wants centroid assistance.
fn is_compact_object_on_black(data: &[u16], w: usize, h: usize) -> bool {
    if data.len() < w * h || w < 16 || h < 16 {
        return false;
    }
    let step = (data.len() / 200_000).max(1);
    let mut s: Vec<u16> = data.iter().step_by(step).copied().collect();
    if s.len() < 64 {
        return false;
    }
    s.sort_unstable();
    let p05 = s[s.len() * 5 / 100] as f32;
    let p999 = s[(s.len() * 999 / 1000).min(s.len() - 1)] as f32;
    if p999 < 2000.0 {
        return false; // no bright object at all
    }
    if p05 > p999 * 0.10 {
        return false; // no dominant black sky
    }
    let thr = p05 + (p999 - p05) * 0.10;
    let lit = s.iter().filter(|&&v| (v as f32) > thr).count() as f32 / s.len() as f32;
    // Compact: present but far from filling the frame.
    lit > 0.0002 && lit < 0.45
}

/// AP-SCORE INFORMATIVENESS (0..1): Pearson correlation between an AP's local
/// per-frame scores and the GLOBAL frame scores, remapped to a gate weight.
/// Seeing modulates the whole frame at once, so a real local quality signal
/// CORRELATES with the global one; an uncorrelated local score is noise
/// (faint/low-contrast AP: limb, dark maria, planet terminator). Informative
/// APs earn a strict local cutoff; noisy APs are ranked by the global score
/// and accept more frames (pure SNR — local selection can't help there).
fn pearson_informativeness(ap_scores: &[f32], global_scores: &[f32]) -> f32 {
    let n = ap_scores.len().min(global_scores.len());
    if n < 8 {
        return 1.0; // too few frames to judge — keep the strict local behaviour
    }
    let nf = n as f64;
    let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for i in 0..n {
        let x = ap_scores[i] as f64;
        let y = global_scores[i] as f64;
        sa += x;
        sb += y;
        saa += x * x;
        sbb += y * y;
        sab += x * y;
    }
    let ma = sa / nf;
    let mb = sb / nf;
    let va = saa / nf - ma * ma;
    let vb = sbb / nf - mb * mb;
    if va <= 1e-9 || vb <= 1e-9 {
        return 0.0; // flat scores carry no information
    }
    let r = ((sab / nf - ma * mb) / (va.sqrt() * vb.sqrt())) as f32;
    ((r - 0.15) / 0.40).clamp(0.0, 1.0)
}

// A local-quality lookup is cheap, but AP×frames (twice for planetary voting)
// can otherwise turn a corrupt/overambitious grid into hours of scalar work.
// Reject explicitly instead of silently sampling APs or frames and degrading
// the scientific result. 400 M lookups still admits e.g. 20k frames × 20k APs
// for the one-pass surface path, or 10k × 20k for two-pass planetary voting.
const MAX_AP_QUALITY_EVALUATIONS: u64 = 400_000_000;
const MAX_AP_ACCEPTANCE_MATRIX_BYTES: u64 = 512 * 1024 * 1024;
const MIN_AP_ACCEPTANCE_MATRIX_BUDGET_BYTES: u64 = 16 * 1024 * 1024;

fn ap_selection_memory_budget(
    frame_count: usize,
    ap_count: usize,
    quality_passes: usize,
    available_memory: u64,
) -> Result<usize, String> {
    let evaluations = (frame_count as u64)
        .checked_mul(ap_count as u64)
        .and_then(|value| value.checked_mul(quality_passes as u64))
        .ok_or("La carga de selección AP excede el espacio direccionable")?;
    if evaluations > MAX_AP_QUALITY_EVALUATIONS {
        return Err(format!(
            "La selección local requeriría {evaluations} evaluaciones de calidad ({} frames × {} APs × {} pasadas), por encima del límite seguro de {}. Reduce la malla AP o preselecciona menos frames; no se degradará la calidad mediante muestreo silencioso.",
            frame_count,
            ap_count,
            quality_passes,
            MAX_AP_QUALITY_EVALUATIONS,
        ));
    }

    // A fixed 768 MiB reserve made the AP path reject even a tiny matrix when
    // the machine was already under pressure. Reserve at most 25% of what is
    // actually available, and retain a small 16 MiB allocation window (capped
    // by usable RAM). The matrix allocation itself remains fallible.
    let os_reserve = PLANETARY_OS_RAM_RESERVE_BYTES.min(available_memory / 4);
    let usable = available_memory.saturating_sub(os_reserve);
    let budget = (usable / 4)
        .max(MIN_AP_ACCEPTANCE_MATRIX_BUDGET_BYTES.min(usable))
        .min(MAX_AP_ACCEPTANCE_MATRIX_BYTES);
    usize::try_from(budget)
        .map_err(|_| "El presupuesto de la matriz AP excede esta arquitectura".to_string())
}

/// Residual chroma noise of the stacked RGB result (ADU16 sigma), from the
/// horizontal second difference of the U plane sampled at ±2 px (skips the
/// debayer-correlated immediate neighbours). Drives the ADAPTIVE chroma
/// smoothing: a clean stack keeps its real color detail (lunar mineral
/// tinting) instead of being blurred by a fixed-radius pass.
fn estimate_stack_chroma_noise(rgb: &[u16], w: usize, h: usize) -> f32 {
    let n = w * h;
    if rgb.len() < n * 3 || w < 16 || h < 8 {
        return 0.0;
    }
    let u_at = |x: usize, y: usize| -> f32 {
        let i = (y * w + x) * 3;
        -0.14713 * rgb[i] as f32 - 0.28886 * rgb[i + 1] as f32 + 0.436 * rgb[i + 2] as f32
    };
    let mut res: Vec<f32> = Vec::with_capacity(200_000);
    let step_y = (h / 400).max(1);
    let mut y = 1;
    while y < h - 1 && res.len() < 200_000 {
        let mut x = 2;
        while x < w - 2 {
            let d = u_at(x, y) - 0.5 * (u_at(x - 2, y) + u_at(x + 2, y));
            res.push(d.abs());
            x += 7;
        }
        y += step_y;
    }
    if res.len() < 64 {
        return 0.0;
    }
    res.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mad = res[res.len() / 2];
    // The second difference of iid noise carries 1.5× the variance — undo it.
    mad * 1.4826 / 1.2247
}

/// Energía Laplaciana normalizada de UN nivel de escala: Σ lap² sobre los
/// píxeles con señal (brillo > suelo robusto) y lap por encima del gate de
/// ruido, dividida por el número de píxeles válidos y por el brillo medio
/// al cuadrado (la ganancia de transparencia escala lap linealmente, lap²
/// cuadráticamente — dividir por (brillo/8192)² desacopla la selección de
/// las nubes finas/extinción). El gate se deriva del propio frame (mediana
/// de |lap|, dominada por fondo/ruido), no de constantes fijas.
fn lap_energy_normalized(bright: &[u16], lap: &[u16], n_px: usize) -> f64 {
    if n_px == 0 || bright.len() < n_px || lap.len() < n_px {
        return 0.0;
    }
    // Suelo de brillo robusto: 2 % del p99 (NO del máximo: un solo píxel
    // caliente no debe definir el suelo).
    let mut bhist = [0u32; 256];
    for &v in &bright[..n_px] {
        bhist[(v >> 8) as usize] += 1;
    }
    let target = n_px as u64 * 99 / 100;
    let mut acc = 0u64;
    let mut p99_bin = 255usize;
    for (b, &c) in bhist.iter().enumerate() {
        acc += c as u64;
        if acc >= target {
            p99_bin = b;
            break;
        }
    }
    let p99 = ((p99_bin as u32 + 1) << 8) as f64;
    let floor = (p99 * 0.02).max(128.0);
    // Gate de lap adaptativo: mediana de |lap| (≈ ruido: la mayoría del
    // frame es fondo o zonas planas) escalada a sigma y multiplicada.
    let mut lhist = [0u32; 4096];
    for &v in &lap[..n_px] {
        lhist[(v >> 4) as usize] += 1;
    }
    let half_px = n_px as u64 / 2;
    let mut lacc = 0u64;
    let mut med_bin = 0usize;
    for (b, &c) in lhist.iter().enumerate() {
        lacc += c as u64;
        if lacc >= half_px {
            med_bin = b;
            break;
        }
    }
    let sigma_lap = (med_bin as f64 + 0.5) * 16.0 * 1.4826;
    let gate = (6.0 * sigma_lap).max(48.0);
    // La energía se suma sobre los píxeles gated (quita la contribución del
    // ruido), pero se divide por el ÁREA CON SEÑAL (píxeles sobre el suelo
    // de brillo), que es estable entre frames. Dividir por el número de
    // píxeles gated invierte el ranking: en un frame nítido pasan el gate
    // muchos píxeles de detalle débil (diluyen la media), en uno borroso
    // solo sobreviven los bordes más fuertes (media alta).
    let (mut energy, mut n_signal, mut bsum) = (0.0f64, 0u64, 0.0f64);
    for i in 0..n_px {
        let b = bright[i] as f64;
        if b < floor {
            continue;
        }
        n_signal += 1;
        bsum += b;
        let l = lap[i] as f64;
        if l > gate {
            energy += l * l;
        }
    }
    if n_signal < 20 {
        return 0.0;
    }
    let mean_b = (bsum / n_signal as f64).max(1.0);
    (energy / n_signal as f64) / (mean_b / 8192.0).powi(2)
}

/// **Scorer de calidad v2 (PR-1.1)** — métrica ÚNICA para planeta y
/// superficie, calculada sobre el Laplaciano CRUDO (el v1 puntuaba sobre el
/// mapa autonormalizado a 0..60000 y el baseline F0 midió Spearman = −1.0:
/// ordenaba los frames al revés). Multi-escala: energía a media resolución
/// (detalle fino, lo primero que mata el seeing) + energía a cuarto de
/// resolución (estructura), combinadas 65/35. Determinista y sin unsafe.
fn score_frame_quality_v2(
    blurred: &[u16],
    lap_raw: &[u16],
    w: usize,
    h: usize,
    scratch_quarter: &mut Vec<u16>,
) -> u64 {
    let n_px = w * h;
    let fine = lap_energy_normalized(blurred, lap_raw, n_px);
    // Nivel grueso: downscale 2× del blur y Laplaciano 8-vecinos al vuelo
    // (dos pasadas baratas a 1/4 de píxeles; sin buffer de lap adicional).
    let mut coarse = 0.0f64;
    if w >= 32 && h >= 32 {
        let (qw, qh) = crate::alignment::downscale_2x_into(blurred, w, h, scratch_quarter);
        let q = &scratch_quarter[..qw * qh];
        let lap_at = |x: usize, y: usize| -> u16 {
            let c = q[y * qw + x] as i32;
            let n_sum = q[y * qw + x - 1] as i32
                + q[y * qw + x + 1] as i32
                + q[(y - 1) * qw + x - 1] as i32
                + q[(y - 1) * qw + x] as i32
                + q[(y - 1) * qw + x + 1] as i32
                + q[(y + 1) * qw + x - 1] as i32
                + q[(y + 1) * qw + x] as i32
                + q[(y + 1) * qw + x + 1] as i32;
            (c * 8 - n_sum).unsigned_abs().min(65535) as u16
        };
        // Mismo contrato que lap_energy_normalized pero generando el lap al
        // vuelo: primero el gate (mediana) y el suelo, luego la energía.
        let inner_px = (qw - 2) * (qh - 2);
        if inner_px > 400 {
            let mut bhist = [0u32; 256];
            let mut lhist = [0u32; 4096];
            for y in 1..qh - 1 {
                for x in 1..qw - 1 {
                    bhist[(q[y * qw + x] >> 8) as usize] += 1;
                    lhist[(lap_at(x, y) >> 4) as usize] += 1;
                }
            }
            let target = inner_px as u64 * 99 / 100;
            let mut acc = 0u64;
            let mut p99_bin = 255usize;
            for (b, &c) in bhist.iter().enumerate() {
                acc += c as u64;
                if acc >= target {
                    p99_bin = b;
                    break;
                }
            }
            let floor = (((p99_bin as u32 + 1) << 8) as f64 * 0.02).max(128.0);
            let half_px = inner_px as u64 / 2;
            let mut lacc = 0u64;
            let mut med_bin = 0usize;
            for (b, &c) in lhist.iter().enumerate() {
                lacc += c as u64;
                if lacc >= half_px {
                    med_bin = b;
                    break;
                }
            }
            let gate = (6.0 * (med_bin as f64 + 0.5) * 16.0 * 1.4826).max(48.0);
            // Mismo contrato que lap_energy_normalized: energía gated,
            // denominador = área con señal (no los píxeles gated).
            let (mut energy, mut n_signal, mut bsum) = (0.0f64, 0u64, 0.0f64);
            for y in 1..qh - 1 {
                for x in 1..qw - 1 {
                    let b = q[y * qw + x] as f64;
                    if b < floor {
                        continue;
                    }
                    n_signal += 1;
                    bsum += b;
                    let l = lap_at(x, y) as f64;
                    if l > gate {
                        energy += l * l;
                    }
                }
            }
            if n_signal >= 20 {
                let mean_b = (bsum / n_signal as f64).max(1.0);
                coarse = (energy / n_signal as f64) / (mean_b / 8192.0).powi(2);
            }
        }
    }
    let score = 0.65 * fine + 0.35 * coarse;
    score.clamp(0.0, 1.0e18) as u64
}

/// Calidad LOCAL de un AP: media ponderada por área de las celdas de la
/// rejilla 40×40 que cubre la CAJA del AP. El v1 indexaba una única celda
/// por el centro del AP: en sensores grandes cada celda cubre ~100×70 px y
/// un AP podía puntuarse por una celda que apenas tocaba (auditoría F0).
fn ap_grid_quality(
    grid_scores: &[u64],
    w_in: f32,
    h_in: f32,
    ap: &crate::smart_grid::ApPoint,
) -> u64 {
    if grid_scores.len() != 1600 || w_in <= 0.0 || h_in <= 0.0 {
        return 0;
    }
    let cell_w = w_in / 40.0;
    let cell_h = h_in / 40.0;
    let half = ap.size as f32 * 0.5;
    let x0 = (ap.x - half).clamp(0.0, w_in);
    let x1 = (ap.x + half).clamp(0.0, w_in);
    let y0 = (ap.y - half).clamp(0.0, h_in);
    let y1 = (ap.y + half).clamp(0.0, h_in);
    let gx0 = ((x0 / cell_w).floor() as usize).min(39);
    let gx1 = ((x1 / cell_w).ceil() as usize).clamp(gx0 + 1, 40);
    let gy0 = ((y0 / cell_h).floor() as usize).min(39);
    let gy1 = ((y1 / cell_h).ceil() as usize).clamp(gy0 + 1, 40);
    let mut sum = 0.0f64;
    let mut wsum = 0.0f64;
    for gy in gy0..gy1 {
        let cy0 = gy as f32 * cell_h;
        let oy = (y1.min(cy0 + cell_h) - y0.max(cy0)).max(0.0);
        for gx in gx0..gx1 {
            let cx0 = gx as f32 * cell_w;
            let ox = (x1.min(cx0 + cell_w) - x0.max(cx0)).max(0.0);
            let area = (ox * oy) as f64;
            if area > 0.0 {
                sum += grid_scores[gy * 40 + gx] as f64 * area;
                wsum += area;
            }
        }
    }
    if wsum > 0.0 {
        (sum / wsum) as u64
    } else {
        0
    }
}

fn raw_to_analysis_mono_into(
    raw: &[u8],
    width: usize,
    height: usize,
    bpp: usize,
    color_id: i32,
    cfa_origin_x: usize,
    cfa_origin_y: usize,
    decode_scratch: &mut Vec<u16>,
    out: &mut Vec<u16>,
) {
    let needs_color_decode = ser::ser_color_is_bayer(color_id)
        || ser::ser_color_is_yuv422(color_id)
        || matches!(bpp, 3 | 6);
    if !needs_color_decode {
        raw_to_u16_buffer_into(raw, width, height, bpp, out);
        out.truncate(width.saturating_mul(height));
        return;
    }
    raw_to_u16_buffer_into(raw, width, height, bpp, decode_scratch);
    decoded_to_analysis_mono_into(
        decode_scratch,
        width,
        height,
        bpp,
        color_id,
        cfa_origin_x,
        cfa_origin_y,
        out,
    );
}

fn decoded_to_analysis_mono_into(
    decoded: &[u16],
    width: usize,
    height: usize,
    bpp: usize,
    color_id: i32,
    cfa_origin_x: usize,
    cfa_origin_y: usize,
    out: &mut Vec<u16>,
) {
    let pixels = width.saturating_mul(height);
    if ser::ser_color_is_bayer(color_id) {
        bayer_to_green_into(
            decoded,
            width,
            height,
            color_id,
            cfa_origin_x,
            cfa_origin_y,
            out,
        );
        return;
    }

    if ser::ser_color_is_yuv422(color_id) {
        // Comparte desempaquetado y ecuación BT.601 con el apilado, pero calcula
        // sólo G: paridad exacta sin el RGB temporal de 3× por worker.
        yuv422_to_green_into(decoded, width, height, color_id, out);
        return;
    }

    out.clear();
    if out.capacity() < pixels {
        out.reserve(pixels);
    }
    if matches!(bpp, 3 | 6) {
        // RGB y BGR comparten G en el índice 1. Usar verde, no (R+G+B)/3,
        // mantiene paridad con la alineación fina del apilado y evita que una
        // aberración cromática roja/azul cambie el ranking de seeing.
        out.extend((0..pixels).map(|i| decoded.get(i * 3 + 1).copied().unwrap_or(0)));
    } else {
        out.extend(decoded.iter().copied().take(pixels));
        out.resize(pixels, 0);
    }
}

/// Variante para el frame de referencia, que se conserva decodificado una
/// sola vez a tamaño completo. Recorta todas las componentes sin promediarlas
/// y después invoca la misma conversión canónica que los frames del stream.
fn raw_roi_to_analysis_mono_into(
    raw: &[u8],
    full_width: usize,
    full_height: usize,
    bpp: usize,
    color_id: i32,
    roi_x: usize,
    roi_y: usize,
    roi_width: usize,
    roi_height: usize,
    decode_scratch: &mut Vec<u16>,
    out: &mut Vec<u16>,
) {
    let channels = if matches!(bpp, 3 | 6) { 3usize } else { 1usize };
    let sample_bytes = if matches!(bpp, 2 | 6) { 2usize } else { 1usize };
    let samples = roi_width.saturating_mul(roi_height).saturating_mul(channels);
    decode_scratch.clear();
    decode_scratch.resize(samples, 0);

    if !matches!(bpp, 1 | 2 | 3 | 6)
        || roi_x.saturating_add(roi_width) > full_width
        || roi_y.saturating_add(roi_height) > full_height
    {
        out.clear();
        out.resize(roi_width.saturating_mul(roi_height), 0);
        return;
    }

    let src_row_bytes = full_width.saturating_mul(bpp);
    let copy_samples = roi_width.saturating_mul(channels);
    for y in 0..roi_height {
        let src_start = (roi_y + y)
            .saturating_mul(src_row_bytes)
            .saturating_add(roi_x.saturating_mul(bpp));
        let dst_start = y * copy_samples;
        for sample in 0..copy_samples {
            let src = src_start.saturating_add(sample.saturating_mul(sample_bytes));
            decode_scratch[dst_start + sample] = if sample_bytes == 1 {
                raw.get(src).copied().unwrap_or(0) as u16 * 257
            } else {
                let lo = raw.get(src).copied().unwrap_or(0) as u16;
                let hi = raw.get(src + 1).copied().unwrap_or(0) as u16;
                (hi << 8) | lo
            };
        }
    }
    decoded_to_analysis_mono_into(
        decode_scratch,
        roi_width,
        roi_height,
        bpp,
        color_id,
        roi_x,
        roi_y,
        out,
    );
}

// Helper function for Frame Analysis (Shared between Stream and Fallback)
fn process_analysis_frame(
    i: usize,
    raw: &[u8],
    buffers: &mut AnalysisBufferSet,
    roi_x: usize,
    roi_y: usize,
    roi_img_w: usize,
    roi_img_h: usize,
    width: usize,
    height: usize,
    bpp: usize,
    color_id: i32,
    is_surface: bool,
    warping_analysis: bool,
    anchor_pyramid_ref: Option<&Vec<u16>>,
    anchor_mono_ref: &Vec<u16>,
    // COG-ASSIST (compact object on black sky, e.g. handheld phone videos):
    // when true, `ref_cog_x/y` hold the reference frame's brightness centroid
    // (ROI coords, full-res) and each frame's centroid delta PRE-CENTERS the
    // SAD search — unbounded motion range, texture-refined precision.
    cog_assist: bool,
    ref_cog_x: f32,
    ref_cog_y: f32,
    _target_type: &str,
    // When true (large lunar disc), align on surface texture instead of the
    // brightness CoG — the disk is too big/asymmetric for stable centroiding.
    large_disc: bool,
    // Estado compartido del preprocesador wgpu. False = se puede usar; ante el
    // primer device loss/OOM/paridad operacional se marca y todos los workers
    // siguientes continúan por CPU sin repetir el fallo.
    gpu_analysis_failed: Option<&std::sync::atomic::AtomicBool>,
    // El coordinador por lotes puede entregar aquí un resultado ya calculado
    // junto con otros frames en el mismo submit/readback. `None` conserva el
    // camino unitario/CPU para compatibilidad y fallback.
    gpu_preprocessed_override: Option<crate::gpu_analysis::AnalysisGpuOutput>,
    prepared_mono_override: Option<Vec<u16>>,
) -> FrameAlignmentData {
    if raw.is_empty() {
        return FrameAlignmentData::empty(i);
    }

    let qual_score: u64;
    let dx: f32;
    let dy: f32;

    // ALL MODES now use Phase Correlation (SAD) and Noise-Resistant scoring.
    if let Some(mono) = prepared_mono_override {
        buffers.raw_u16 = mono;
    } else {
        raw_to_analysis_mono_into(
            raw,
            roi_img_w,
            roi_img_h,
            bpp,
            color_id,
            roi_x,
            roi_y,
            &mut buffers.raw_decode_u16,
            &mut buffers.raw_u16,
        );
    }

    // ANALYSIS @ HALF RESOLUTION (R13): scoring, global matching and the
    // 40×40 grid all run on a 2× reduced image — ~4× faster, with a noise-
    // robust quality ranking (industry standard: AS!4 scores on downsampled
    // gradients). Final alignment precision is owned by the STACKING stage
    // (per-frame coarse verification + per-AP sub-pixel search), so the
    // half-res global shift (±~1px) costs nothing in the final result.
    // A large lunar disc is treated like a surface for alignment, scoring and
    // grid quality — the same proven path that makes Superficie sharp on the Moon.
    let texture_align = is_surface || large_disc;
    let gpu_preprocessed = gpu_preprocessed_override.or_else(|| {
        gpu_analysis_failed
            .filter(|failed| !failed.load(std::sync::atomic::Ordering::Relaxed))
            .and_then(|failed| match crate::gpu_analysis::process_with_options(
                &buffers.raw_u16,
                roi_img_w,
                roi_img_h,
                texture_align,
                warping_analysis,
                !texture_align || cog_assist,
            ) {
                Ok(v) => Some(v),
                Err(_) => {
                    failed.store(true, std::sync::atomic::Ordering::Relaxed);
                    None
                }
            })
    });
    let gpu_frame_active = gpu_preprocessed.is_some();
    let mut gpu_center = None;
    let mut gpu_grid_scores = None;
    let (hw, hh) = if let Some(gpu) = gpu_preprocessed {
        gpu_center = gpu.geometric_center;
        gpu_grid_scores = gpu.grid_scores;
        buffers.half_u16 = gpu.half;
        buffers.blur_out = gpu.blurred;
        // lap CRUDO: finish_analysis_output ya no normaliza (scorer v2).
        buffers.lap_out = gpu.laplacian;
        (roi_img_w / 2, roi_img_h / 2)
    } else {
        let (hw, hh) = crate::alignment::downscale_2x_into(
            &buffers.raw_u16,
            roi_img_w,
            roi_img_h,
            &mut buffers.half_u16,
        );
        let _legacy_surface_score = enhance_and_lap_raw(
            &buffers.half_u16,
            hw,
            hh,
            &mut buffers.blur_temp,
            &mut buffers.blur_out,
            &mut buffers.lap_out,
        );
        (hw, hh)
    };

    // SCORER v2 (PR-1.1): métrica única planeta/superficie sobre el
    // Laplaciano CRUDO multi-escala, ANTES de normalizar el mapa para SAD.
    // El v1 puntuaba el mapa ya autonormalizado y ordenaba los frames AL
    // REVÉS (baseline F0: Spearman −1.0 contra verdad conocida). Ambos
    // caminos (CPU y GPU) puntúan aquí con la MISMA función sobre los
    // mismos buffers: paridad por construcción.
    let score_val = score_frame_quality_v2(
        &buffers.blur_out,
        &buffers.lap_out,
        hw,
        hh,
        &mut buffers.quarter_u16,
    );
    normalize_lap_for_sad(&mut buffers.lap_out);

    let search_w = hw / 2;
    let search_h = hh / 2;
    let search_x = (hw - search_w) / 2;
    let search_y = (hh - search_h) / 2;

    let (sdx, sdy) = if let Some(pyr) = anchor_pyramid_ref {
        // COG PRE-CENTERING: the fixed search window covers ~±128 full-res px.
        // Handheld phone videos move MUCH more — frames beyond the window got
        // garbage shifts and stacked as displaced ghosts. For a compact object
        // on black sky the centroid delta is an unbounded-range prior; the SAD
        // then only refines around it (texture precision preserved).
        let (init_dx, init_dy) = if cog_assist {
            let (fcx, fcy) = gpu_center
                .map(|(x, y)| (x * 0.5, y * 0.5))
                .unwrap_or_else(|| {
                    compute_robust_geometric_center(&buffers.half_u16, hw, hh, 0, 0)
                });
            (
                (fcx - ref_cog_x / 2.0).round() as isize,
                (fcy - ref_cog_y / 2.0).round() as isize,
            )
        } else {
            (0, 0)
        };
        // El GPU resuelve la superficie SAD gruesa en la pirámide 4× y sólo
        // devuelve la mejor semilla. CPU conserva la ventana fina SIMD y el
        // ajuste subpíxel, que es la validación numérica de la decisión GPU.
        let gpu_seed = if gpu_frame_active {
            let (sw, sh) = crate::alignment::downscale_2x_into(
                &buffers.lap_out,
                hw,
                hh,
                &mut buffers.quarter_u16,
            );
            // Un solo SadPoint gigante aquí (caja ¼×¼ del mapa, r=32) era UN
            // hilo GPU con cientos de millones de operaciones: el watchdog de
            // Metal lo mataba en el PRIMER frame y toda la sesión caía a
            // "scoring CPU". La variante paralela reparte los 65² candidatos
            // en hilos propios — mismo mínimo, bit-idéntico.
            match crate::gpu_analysis::search_sad_single_parallel(
                pyr,
                &buffers.quarter_u16,
                sw,
                sh,
                (search_x / 2 + search_w / 4) as i32,
                (search_y / 2 + search_h / 4) as i32,
                (search_x / 2 + search_w / 4) as i32 + (init_dx / 2) as i32,
                (search_y / 2 + search_h / 4) as i32 + (init_dy / 2) as i32,
                (search_w / 2) as i32,
                (search_h / 2) as i32,
                32,
            ) {
                Ok(v) => v.map(|m| {
                    (
                        (init_dx / 2 + m.dx as isize) * 2,
                        (init_dy / 2 + m.dy as isize) * 2,
                    )
                }),
                Err(_) => {
                    if let Some(failed) = gpu_analysis_failed {
                        failed.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    None
                }
            }
        } else {
            None
        };
        if let Some((guess_dx, guess_dy)) = gpu_seed {
            crate::alignment::refine_best_match_sad_offset(
                anchor_mono_ref,
                &buffers.lap_out,
                hw,
                hh,
                search_x,
                search_y,
                search_w,
                search_h,
                guess_dx,
                guess_dy,
                16,
            )
        } else {
            crate::alignment::find_best_match_sad_pyramid_offset(
                anchor_mono_ref,
                &buffers.lap_out,
                pyr,
                hw,
                hh,
                hw / 2,
                hh / 2,
                search_x,
                search_y,
                search_w,
                search_h,
                64,
                2,
                16,
                init_dx,
                init_dy,
            )
        }
    } else {
        (0.0, 0.0)
    };

    if texture_align {
        // TEXTURE ALIGNMENT (surface + large lunar disc): half-res SAD shift →
        // full-res coordinates. Locks onto real surface features regardless of
        // the lunar phase, so the frames register consistently → sharp stack.
        dx = sdx * 2.0;
        dy = sdy * 2.0;
    } else {
        // PLANETARY MODE: Robust CoG Centering (kept at FULL resolution —
        // single cheap pass, and disk centering precision matters here)
        let (cur_cog_x, cur_cog_y) = gpu_center
            .map(|(x, y)| (x + roi_x as f32, y + roi_y as f32))
            .unwrap_or_else(|| {
                compute_robust_geometric_center(
                    &buffers.raw_u16,
                    roi_img_w,
                    roi_img_h,
                    roi_x,
                    roi_y,
                )
            });
        dx = cur_cog_x - (width as f32 / 2.0);
        dy = cur_cog_y - (height as f32 / 2.0);
    }

    qual_score = score_val;

    let mut grid_scores = None;
    if warping_analysis {
        grid_scores = if gpu_grid_scores.is_some() {
            gpu_grid_scores.take()
        } else {
            let input_buffer = if texture_align {
                &buffers.blur_out
            } else {
                &buffers.half_u16
            };
            Some(calculate_grid_quality(
                input_buffer,
                hw,
                hh,
                40,
                texture_align,
            ))
        };
    }

    FrameAlignmentData {
        frame_idx: i,
        global_shift: (dx, dy),
        local_shifts: vec![],
        idx: i,
        x_shift: dx,
        y_shift: dy,
        score: qual_score,
        added: false,
        grid_scores,
    }
}

// Calcula cuántos hilos puede usar el análisis sin agotar la RAM. El pico de
// memoria escala con el número de hilos (cada uno reserva su AnalysisBufferSet
// + temporales por frame). Devolvemos todos los hilos por hardware, salvo que
// no quepan en la RAM libre; ahí bajamos lo justo para no abortar (0xc0000409).
// En equipos con RAM de sobra NO se reduce nada (sin pérdida de rendimiento).
fn ram_aware_analysis_threads(rw: usize, rh: usize, is_color: bool, is_ffmpeg: bool) -> usize {
    // FFmpeg decodifica en OTRO proceso con sus propios -threads: reservar 2
    // núcleos evita pelearse con él. Los lectores nativos (SER/AVI/FITS) son
    // mmap sin decodificador — basta 1 núcleo libre para SO/UI (+1 hilo de
    // análisis gratis en cada máquina).
    let reserve = if is_ffmpeg { 2 } else { 1 };
    let hw = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .saturating_sub(reserve)
        .max(2);
    // System::new() + refresh_memory: mismo available_memory que new_all()
    // sin enumerar todos los procesos del sistema.
    let mut sys = System::new();
    sys.refresh_memory();
    let available = sys.available_memory(); // bytes
    let os_reserve: u64 = 2 * 1024 * 1024 * 1024; // 2 GB para el SO/otros procesos
    let usable = available.saturating_sub(os_reserve);
    // Incluye el scratch u16 de CFA/RGB y el verde canónico separado. En RGB
    // directo el scratch ocupa 6 B/px; Bayer ocupa 2 B/px. El margen cubre los
    // temporales Rayon/GPU sin reducir hilos en entradas mono.
    let per_px: u64 = if is_color { 32 } else { 16 };
    let per_thread = (rw as u64)
        .saturating_mul(rh as u64)
        .saturating_mul(per_px)
        .max(1);
    let by_ram = (usable / per_thread).max(1) as usize;
    by_ram.min(hw)
}

#[derive(Clone, Debug)]
struct FfmpegDecodeProbe {
    prefer_hardware: bool,
    hardware_confirmed: bool,
    hardware_seconds: Option<f32>,
    cpu_seconds: Option<f32>,
    backend: Option<String>,
}

fn ffmpeg_runtime_identity(ffmpeg: &str) -> String {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static IDENTITIES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let identities = IDENTITIES.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(identity) = identities
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(ffmpeg)
        .cloned()
    {
        return identity;
    }
    let mut command = Command::new(ffmpeg);
    #[cfg(target_os = "windows")]
    command.creation_flags(0x08000000);
    command.arg("-version");
    let version = run_command_supervised(
        command,
        FFMPEG_TOOL_IDENTITY_TIMEOUT,
        None,
        "FFmpeg -version",
    )
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|stdout| stdout.lines().next().map(str::to_owned))
        .unwrap_or_else(|| "version-unavailable".into());
    let binary = std::fs::metadata(ffmpeg)
        .ok()
        .map(|metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            format!("{}:{}", metadata.len(), modified)
        })
        .unwrap_or_else(|| "path-resolved-by-os".into());
    let identity = format!("{ffmpeg}|{binary}|{version}");
    identities
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(ffmpeg.to_string(), identity.clone());
    identity
}

fn ffmpeg_decode_route_label(ffmpeg: &str, backend: Option<&str>) -> String {
    let route = backend
        .map(|backend| format!("hardware:{backend}"))
        .unwrap_or_else(|| "cpu".into());
    format!("{route}|{}", ffmpeg_runtime_identity(ffmpeg))
}

fn confirmed_ffmpeg_hardware_backend(log: &str, process_ok: bool) -> Option<String> {
    if !process_ok {
        return None;
    }
    let log = log.to_ascii_lowercase();
    // `-hwaccel auto` puede anunciar un dispositivo, fallar al inicializarlo y
    // terminar con exit=0 gracias al decoder software. Esas corridas no son
    // aceleración confirmada aunque contengan el nombre del backend.
    const FAILURE_MARKERS: &[&str] = &[
        "hwaccel initialisation returned error",
        "hwaccel initialization returned error",
        "failed setup for format",
        "hardware accelerator failed",
        "device creation failed",
        "no device available",
        "videotoolbox malfunction",
        "could not dynamically load",
        "cannot load libcuda",
        "failed to initialise",
        "failed to initialize",
        "falling back to software",
        "fallback to software",
        "using software decoding",
    ];
    if FAILURE_MARKERS.iter().any(|marker| log.contains(marker)) {
        return None;
    }
    // El valor retornado es también el argumento canónico de `-hwaccel`.
    // CUDA precede a NVDEC porque algunos logs contienen ambos términos pero
    // `nvdec` no está disponible como hwaccel en todos los builds FFmpeg.
    const BACKENDS: &[(&str, &str)] = &[
        ("videotoolbox", "videotoolbox"),
        ("d3d11va", "d3d11va"),
        ("dxva2", "dxva2"),
        ("cuda", "cuda"),
        ("qsv", "qsv"),
        ("vaapi", "vaapi"),
        ("vulkan", "vulkan"),
    ];
    BACKENDS
        .iter()
        .find(|(_, marker)| log.contains(marker))
        .map(|(backend, _)| (*backend).to_string())
}

/// Micro-benchmark reproducible del decode. `-hwaccel auto` por sí solo NO es
/// evidencia de GPU: se exige que FFmpeg anuncie un backend de hardware en su
/// log verbose y además que el bloque corto sea realmente más rápido que CPU.
/// La decisión se cachea por fingerprint de origen para que análisis/apilado
/// compartan el resultado sin repetir procesos de prueba.
fn benchmark_ffmpeg_decode_route(
    ffmpeg: &str,
    path: &str,
    codec: &str,
    color_id: i32,
    width: usize,
    height: usize,
    rotation: i32,
    cancel: Option<&PlanetaryJobToken>,
) -> FfmpegDecodeProbe {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, FfmpegDecodeProbe>>> = OnceLock::new();

    let source_fingerprint = planetary_source_fingerprint(path).unwrap_or_default();
    let key = format!(
        "ffmpeg-route-v3|{}|{}|{}|{}|{}|{}|{}|{}",
        ffmpeg_runtime_identity(ffmpeg),
        path,
        codec,
        source_fingerprint,
        color_id,
        width,
        height,
        rotation,
    );
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key).cloned() {
        return v;
    }

    let run = |hardware: bool| -> (Option<f32>, String, bool, bool) {
        use std::io::Read;
        let mut cmd = Command::new(ffmpeg);
        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x08000000);
        cmd.args(["-hide_banner", "-nostdin", "-loglevel", "verbose", "-benchmark"]);
        if hardware {
            cmd.args(["-hwaccel", "auto"]);
        }
        // El benchmark sólo necesita decidir qué decoder gana. Para vídeo de
        // color reproduce la ruta de análisis G16, no materializa RGB48: el
        // backend de codec es el mismo y se recortan 2/3 de los bytes de salida.
        let is_color_probe = ffmpeg_stream_is_color(color_id);
        let p_fmt = "gray16le";
        let mut filters = Vec::new();
        if let Some(rotation_filter) = ffmpeg_rotation_filter(rotation) {
            filters.push(rotation_filter.to_string());
        }
        filters.push(format!("scale={width}:{height}:flags=neighbor"));
        if is_color_probe {
            filters.push("format=rgb48le".into());
            filters.push("extractplanes=g".into());
        }
        filters.push(format!("format={p_fmt}"));
        let filter = filters.join(",");
        // Antes eran 24 frames por ruta sin importar resolución: a 3312×5888
        // equivalían a 2.6 GiB RGB por prueba. Acotar por bytes conserva varias
        // muestras para calentar el decoder y evita castigar vídeo 4K/8K.
        const PROBE_TARGET_BYTES: usize = 192 * 1024 * 1024;
        let output_bytes_per_frame = width.saturating_mul(height).saturating_mul(2).max(1);
        let probe_frames = PROBE_TARGET_BYTES
            .saturating_add(output_bytes_per_frame - 1)
            / output_bytes_per_frame;
        let probe_frames = probe_frames.clamp(2, 12).to_string();
        let logical = num_cpus::get();
        let threads = if logical <= 4 {
            logical.saturating_sub(1).max(1)
        } else {
            logical.saturating_sub(2).max(4)
        }
        .to_string();
        let started = std::time::Instant::now();
        let spawned = cmd
            .args([
                "-noautorotate",
                "-threads",
                &threads,
                "-i",
                path,
                "-map",
                "0:v:0",
                "-frames:v",
                &probe_frames,
                "-an",
                "-sn",
                "-fps_mode",
                "passthrough",
                "-vf",
                &filter,
                "-pix_fmt",
                p_fmt,
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            // Se mide toda la ruta que usa Zenith, incluida conversión y
            // descarga a memoria; los bytes no se retienen durante el probe.
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let Ok(mut child) = spawned else {
            return (None, String::new(), false, false);
        };
        let stderr = child.stderr.take();
        let log_thread = std::thread::spawn(move || {
            const MAX_LOG: usize = 2 * 1024 * 1024;
            let mut log = Vec::with_capacity(64 * 1024);
            if let Some(stderr) = stderr {
                let mut reader = std::io::BufReader::new(stderr);
                let mut chunk = [0u8; 8192];
                loop {
                    let Ok(read) = reader.read(&mut chunk) else { break };
                    if read == 0 {
                        break;
                    }
                    let keep = read.min(MAX_LOG.saturating_sub(log.len()));
                    log.extend_from_slice(&chunk[..keep]);
                }
            }
            String::from_utf8_lossy(&log).to_ascii_lowercase()
        });
        const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
        let (status, aborted) = loop {
            if cancel.is_some_and(PlanetaryJobToken::is_cancelled)
                || started.elapsed() >= PROBE_TIMEOUT
            {
                let _ = child.kill();
                let status = child.wait().ok();
                break (status, true);
            }
            match child.try_wait() {
                Ok(Some(status)) => break (Some(status), false),
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(_) => {
                    let _ = child.kill();
                    let status = child.wait().ok();
                    break (status, false);
                }
            }
        };
        let log = log_thread.join().unwrap_or_default();
        let ok = !aborted && status.is_some_and(|status| status.success());
        (
            ok.then_some(started.elapsed().as_secs_f32()),
            log,
            ok,
            aborted,
        )
    };

    let (hw_s, hw_log, hw_ok, hw_aborted) = run(true);
    let (cpu_s, _, cpu_ok, cpu_aborted) = if hw_aborted && cancel.is_some_and(PlanetaryJobToken::is_cancelled) {
        (None, String::new(), false, true)
    } else {
        run(false)
    };
    let backend = confirmed_ffmpeg_hardware_backend(&hw_log, hw_ok);
    // Algunos builds escriben el nombre del decoder como h264_videotoolbox,
    // hevc_qsv, etc.; los marcadores anteriores también los capturan.
    let confirmed = backend.is_some();
    let faster = match (hw_s, cpu_s) {
        // Un margen pequeño evita elegir GPU por ruido de creación de procesos
        // cuando ambas rutas son esencialmente equivalentes.
        (Some(h), Some(c)) => h <= c * 0.95,
        (Some(_), None) => true,
        _ => false,
    };
    let probe = FfmpegDecodeProbe {
        prefer_hardware: confirmed && faster,
        hardware_confirmed: confirmed,
        hardware_seconds: hw_s,
        cpu_seconds: cpu_s.filter(|_| cpu_ok),
        backend,
    };
    // Un timeout puede ser una decisión reproducible (CPU), pero una
    // cancelación pertenece sólo al job actual y nunca debe contaminar el
    // cache de selección de ruta para la siguiente operación.
    if !(hw_aborted || cpu_aborted) || !cancel.is_some_and(PlanetaryJobToken::is_cancelled) {
        cache.lock().unwrap_or_else(|e| e.into_inner()).insert(key, probe.clone());
    }
    probe
}

// PR-2.5: SÍNCRONA (antes async sin ningún .await interno): minutos de
// cómputo rayon bloqueaban un worker del runtime async de Tauri y retrasaban
// cualquier otro comando. Los comandos la envuelven en spawn_blocking.
fn perform_standardized_analysis(
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
    request_id: usize,
    path: &str,
    is_surface: bool,
    target_type: String,
    warping_analysis: bool,
    bayer_override: Option<i32>,
    anchor_override: Option<Vec<i32>>,
    progress_prefix: Option<String>,
    compute_policy: ComputePolicy,
) -> Result<AnalysisResult, String> {
    let is_surface = is_surface || is_surface_target(&target_type);
    let warping_analysis = zenith_should_warp(&target_type, is_surface, warping_analysis);
    let job_token =
        PlanetaryJobToken::for_app(app, request_id, state.cancel_requested.clone());
    let pipeline_job_id = new_job_id("planetary-analysis");
    let pipeline_started = std::time::Instant::now();
    // perf_trace: desglose por fase del análisis (volcado JSON al terminar;
    // el guard vuelca también en errores/cancelaciones con completed=false).
    let pt = crate::perf_trace::job_start("analysis", path);
    let pt_guard = crate::perf_trace::JobGuard::new(pt);

    let prefix_str = progress_prefix.clone().unwrap_or_default();
    let get_msg = |msg: &str| {
        if prefix_str.is_empty() {
            msg.to_string()
        } else {
            format!("{} {}", prefix_str, msg)
        }
    };
    #[cfg(target_arch = "x86_64")]
    let analysis_avx2 = is_x86_feature_detected!("avx2");
    #[cfg(not(target_arch = "x86_64"))]
    let analysis_avx2 = false;
    let analysis_msg = if analysis_avx2 {
        "Iniciando Analisis V2 (AVX2)..."
    } else {
        "Iniciando Analisis V2..."
    };
    emit_progress(app, &get_msg(analysis_msg), 0.0, None);
    let open_cancel = {
        let token = job_token.clone();
        Arc::new(move || token.is_cancelled()) as FfmpegCancelCheck
    };
    let r = {
        let _s = crate::perf_trace::span(pt, "open");
        VideoInput::open_cancelable(path, app, open_cancel)?
    };
    let (tw, th, tf, tbp) = (r.width(), r.height(), r.frame_count(), r.bpp());
    crate::perf_trace::job_meta(pt, "geometry", format!("{tw}x{th}"));
    crate::perf_trace::job_meta(pt, "frames", tf);
    crate::perf_trace::job_meta(pt, "reader", if r.is_ffmpeg() { "ffmpeg" } else { "native" });
    let source_fingerprint = planetary_source_fingerprint(path)?;
    if matches!(compute_policy, ComputePolicy::GpuOnly) {
        match crate::gpu_stack::gpu_runtime() {
            None => {
                return Err(
                    "GPU only solicitado para análisis, pero no existe un dispositivo wgpu compatible"
                        .into(),
                )
            }
            Some(_) if !crate::gpu_analysis::ensure_parity() => {
                return Err("GPU only: falló la paridad del preprocesado planetario".into())
            }
            Some(_) => {}
        }
    }
    let cid = r.resolve_bayer_override(bayer_override)?;
    let is_color = r.is_color_for(cid);
    let unified_source = std::sync::Arc::new(UnifiedFrameSource::from_input(r.clone(), cid));
    let source_descriptor = unified_source.descriptor();
    let reader_kind = if r.is_ffmpeg() { "FFmpeg" } else { "Rust nativo" };
    log_to_front(
        app,
        "INFO",
        &format!(
            "Analisis V2: lector={}, {}x{}, frames={}, bpp={}, color_id={}",
            reader_kind, tw, th, tf, tbp, cid
        ),
    );
    log_to_front(
        app,
        "INFO",
        &format!(
            "FrameSource: {} · muestra {}-bit · {} endian · rotación {}° · índices absolutos.",
            source_descriptor.source_kind,
            source_descriptor.sample_bits,
            if source_descriptor.little_endian { "little" } else { "big" },
            source_descriptor.rotation_degrees,
        ),
    );
    emit_progress(app, &get_msg("Preparando lector..."), 2.0, None);
    let mut requested_roi = requested_analysis_roi(
        tw,
        th,
        &target_type,
        is_surface,
        warping_analysis,
        anchor_override.as_deref(),
    );
    if ser::ser_color_is_yuv422(cid) {
        requested_roi = align_yuv422_analysis_roi(requested_roi, tw);
    }
    let cache_expectation = AnalysisCacheExpectation {
        source_fingerprint,
        resolved_color_id: cid,
        target_type: normalized_analysis_target(&target_type),
        is_surface,
        warping_analysis,
        anchor_override: anchor_override.clone(),
        requested_roi,
        width: tw,
        height: th,
        declared_frame_count: tf,
        frame_count_exact: r.frame_count_is_exact(),
    };
    let c_suffix = zenith_analysis_cache_suffix(
        &target_type,
        is_surface,
        warping_analysis,
        cid,
        anchor_override.as_deref(),
        requested_roi,
    );

    let c_path = get_analysis_cache_path(path, &c_suffix);
    if let Some((cached, _cache_location)) =
        load_validated_analysis_cache(&c_path, &cache_expectation)
    {
        if let (Some(_), Some(qg), Some(bi)) = (
                    &cached.frame_stats,
                    &cached.quality_graph,
                    cached.best_frame_idx,
                ) {
                    emit_progress(app, &get_msg("Cargando Caché..."), 100.0, None);
                    let (mut mi, mut ma, mut su) = (100.0f32, 0.0f32, 0.0f32);
                    for &v in qg {
                        mi = mi.min(v);
                        ma = ma.max(v);
                        su += v;
                    }
                    let aq = if qg.is_empty() {
                        0.0
                    } else {
                        su / qg.len() as f32
                    };
                    let preview_base64 = if let Some(preview) =
                        load_analysis_preview_cache(&c_path, source_fingerprint)
                    {
                        log_to_front(
                            app,
                            "SUCCESS",
                            "Caché de análisis: preview persistente reutilizada sin FFmpeg ni recompresión PNG.",
                        );
                        preview
                    } else {
                        // PR-10: caché de decode primero — la preview del
                        // caché de análisis ya no re-decodifica 0..best_idx.
                        let raw = match (if let VideoInput::Ffmpeg(ref fr) = r {
                            read_selected_from_decode_cache(
                                path,
                                &fr.ffmpeg_path,
                                &fr.codec_name,
                                fr.rotation,
                                tw,
                                th,
                                tbp,
                                cid,
                                &[bi],
                            )
                        } else {
                            None
                        }) {
                            Some(mut frames) => frames.pop().unwrap_or_default(),
                            None => {
                                // PR-22: preview cosmética por seek rápido
                                // también en el cache-hit del análisis.
                                let fast = if std::env::var("ZAS_EXACT_ANCHOR").ok().as_deref()
                                    != Some("1")
                                {
                                    if let VideoInput::Ffmpeg(ref fr) = r {
                                        ffmpeg_fast_seek_frame(fr, path, tw, th, cid, bi)
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };
                                match fast {
                                    Some(raw) => raw,
                                    None => read_exact_source_frame(&unified_source, bi)?,
                                }
                            }
                        };
                        let u16_raw = raw_to_u16_buffer(&raw, tw, th, tbp);
                        drop(raw);
                        if let Err(error) =
                            cache_smart_grid_frame(path, bi, tw, th, cid, &u16_raw)
                        {
                            log_to_front(
                                app,
                                "WARN",
                                &format!(
                                    "No se pudo preparar la caché RAM de Smart AP: {error}"
                                ),
                            );
                        }
                        let mut buf = Vec::new();
                        if is_color {
                            let mut u16 = debayer_to_rgb(&u16_raw, tw, th, cid);
                            auto_color_balance(&mut u16, tw, th);
                            image::DynamicImage::ImageRgb8(
                                image::RgbImage::from_raw(
                                    tw as u32,
                                    th as u32,
                                    to_8bit_preview_visual(&u16),
                                )
                                .unwrap(),
                            )
                            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
                            .unwrap();
                        } else {
                            let vis = to_8bit_visual(
                                &auto_contrast_stretch_u16(&u16_raw, tw, th),
                                1.0,
                            );
                            let mut rgba = Vec::with_capacity(tw * th * 4);
                            for p in vis {
                                rgba.extend_from_slice(&[p, p, p, 255]);
                            }
                            image::DynamicImage::ImageRgba8(
                                image::RgbaImage::from_raw(tw as u32, th as u32, rgba).unwrap(),
                            )
                            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
                            .unwrap();
                        }
                        save_analysis_preview_cache(&buf, &c_path, source_fingerprint)
                            .or_else(|| save_preview_png_to_temp(&buf, "analysis"))
                            .unwrap_or_else(|| {
                                format!(
                                    "data:image/png;base64,{}",
                                    general_purpose::STANDARD.encode(&buf)
                                )
                            })
                    };
                    let pn = ser::ser_pattern_name(cid);
                    emit_pipeline_telemetry(
                        app,
                        PipelineTelemetry {
                            job_id: pipeline_job_id.clone(),
                            domain: PipelineDomain::Planetary,
                            phase: "complete".into(),
                            engine: "Cache de análisis versionada".into(),
                            progress: 100.0,
                            eta_seconds: Some(0.0),
                            items_done: qg.len(),
                            items_total: qg.len(),
                            throughput: Some(qg.len() as f32 / pipeline_started.elapsed().as_secs_f32().max(0.001)),
                            cpu_percent: None,
                            gpu_percent: None,
                            ram_mb: 0,
                            vram_mb: 0,
                            io_read_mb: 0.0,
                            io_write_mb: 0.0,
                            cache_hits: qg.len(),
                            cache_misses: 0,
                            fallback_reason: None,
                        },
                    );
                    crate::perf_trace::job_meta(pt, "cache", "hit");
                    let _ = pt_guard.finish_ok();
                    return Ok(AnalysisResult {
                        metadata: VideoMetadata {
                            width: tw,
                            height: th,
                            frame_count: tf,
                            bpp: tbp * 8,
                            color_id: cid,
                            file_size_mb: 0.0,
                            pattern_name: pn.to_string(),
                            is_color,
                        },
                        stats: VideoStats {
                            min_pixel: 0,
                            max_pixel: 65535,
                            avg_brightness: 0.0,
                            dynamic_range_pct: 100.0,
                            std_dev: 0.0,
                            entropy: 0.0,
                            avg_quality: aq as f64,
                            quality_stability: if ma > 0.0 {
                                (mi / ma * 100.0) as f64
                            } else {
                                0.0
                            },
                            best_score: ma as f64,
                            worst_score: mi as f64,
                        },
                        recommended_pct: compute_smart_stack_pct(qg, &target_type, is_surface),
                        quality_graph: qg.iter().enumerate().map(|(i, &v)| (i, v as f64)).collect(),
                        preview_base64,
                        path: path.to_string(),
                        ap_points: vec![],
                        best_frame_idx: bi,
                    });
        }
    }
    // R11: with warping analysis the 40×40 grid scores must cover the FULL
    // frame — the stacking stage maps AP coordinates (full-frame) onto that
    // grid. A cropped analysis ROI would misalign every per-AP lookup.
    let (mut rx, mut ry, mut rw, mut rh) = (
        requested_roi.x,
        requested_roi.y,
        requested_roi.w,
        requested_roi.h,
    );

    // Planetary CoG of the reference frame (Calculated while the object is a solid disk)
    let mut ref_cog_cx = tw as f32 / 2.0;
    let mut ref_cog_cy = th as f32 / 2.0;
    // Decided once from the reference frame; threaded into every per-frame call.
    let mut large_disc = false;
    let mut analysis_probe_mono: Vec<u16> = Vec::new();
    // COG-ASSIST for handheld/phone videos (compact disc on black sky): the
    // centroid delta pre-centers each frame's SAD search → unbounded motion.
    let cog_assist;

    let (_analysis_ref_idx, analysis_ref_raw) = {
        let _s = crate::perf_trace::span(pt, "ref_select");
        if r.is_ffmpeg() {
            // PR-22: primero el muestreo por seek (~1 GOP por candidato);
            // el camino exacto (decodifica 0..tf/2) queda como fallback y
            // como modo forzable con ZAS_EXACT_ANCHOR=1.
            match select_signal_frame_fast_seek(&r, path, tw, th, tbp, cid, tf, tf / 2) {
                Some(anchor) => anchor,
                None => select_signal_frame_from_source(&unified_source, tw, th, tbp, tf / 2)?,
            }
        } else {
            let index = select_signal_frame_index(&r, tw, th, tbp, cid, tf / 2);
            (index, read_exact_source_frame(&unified_source, index)?)
        }
    };
    emit_progress(app, &get_msg("Preparando referencia..."), 4.0, None);

    let a_mono = {
        // Fase 4: decodificar el frame de referencia UNA sola vez y reusarlo.
        // En small-planet se re-extraía con otro get_frame -> doble decode (caro
        // en FFmpeg; en SER mmap es barato). Mismos píxeles -> resultado idéntico.
        let mut tmp = Vec::with_capacity(rw * rh);
        let mut reference_decode_scratch = Vec::new();
        raw_roi_to_analysis_mono_into(
            &analysis_ref_raw,
            tw,
            th,
            tbp,
            cid,
            rx,
            ry,
            rw,
            rh,
            &mut reference_decode_scratch,
            &mut tmp,
        );
        
        // Phase 1: Before applying the Laplacian edge filter, calculate the planet's actual CoG
        if !is_surface {
            let (cx, cy) = compute_robust_geometric_center(&tmp, rw, rh, rx, ry);
            ref_cog_cx = cx;
            ref_cog_cy = cy;

            // A large lunar disc (Moon / big phase) switches to texture alignment
            // and must stay full-frame like Superficie — NOT tightened to a planet
            // ROI. Genuine small planets keep the tight ROI + rock-solid CoG.
            large_disc = is_large_lunar_disc(&tmp, rw, rh);

            // OPTIMIZATION: For small planets, redefine ROI to be a tight box around the disk
            if is_small_planet(&target_type) && !large_disc {
                let planet_roi = find_planet_roi_from_u16(&tmp, rw, rh);
                // Shift planet ROI to absolute coordinates
                let mut absolute_roi = Rect {
                    x: rx + planet_roi.x,
                    y: ry + planet_roi.y,
                    w: planet_roi.w,
                    h: planet_roi.h,
                };
                if ser::ser_color_is_yuv422(cid) {
                    absolute_roi = align_yuv422_analysis_roi(absolute_roi, tw);
                }
                rx = absolute_roi.x;
                ry = absolute_roi.y;
                rw = absolute_roi.w;
                rh = absolute_roi.h;
                
                // Re-extract tighter reference mono (reusa el frame ya decodificado)
                tmp.clear();
                raw_roi_to_analysis_mono_into(
                    &analysis_ref_raw,
                    tw,
                    th,
                    tbp,
                    cid,
                    rx,
                    ry,
                    rw,
                    rh,
                    &mut reference_decode_scratch,
                    &mut tmp,
                );
                let (ncx, ncy) = compute_robust_geometric_center(&tmp, rw, rh, rx, ry);
                ref_cog_cx = ncx;
                ref_cog_cy = ncy;
            }
        }

        // COG-ASSIST DETECTION: for a compact bright object on black sky
        // (typical handheld phone capture) the reference centroid becomes the
        // motion prior for every frame's SAD search. Coordinates are ROI-local
        // (0,0-based) to match the per-frame centroid computed on the half-res
        // ROI buffer.
        cog_assist = is_compact_object_on_black(&tmp, rw, rh);
        if cog_assist {
            let (acx, acy) = compute_robust_geometric_center(&tmp, rw, rh, 0, 0);
            ref_cog_cx = acx;
            ref_cog_cy = acy;
            log_to_front(
                app,
                "INFO",
                "Objeto compacto sobre cielo negro: pre-centrado CoG activado (movimiento amplio soportado).",
            );
        }

        // Conserva una única muestra para decidir CPU vs GPU con el MISMO ROI
        // real, ya ajustado alrededor del planeta cuando aplica.
        analysis_probe_mono.clone_from(&tmp);

        // R13 HALF-RES: the anchor map must go through the SAME pipeline as
        // the per-frame maps (2× downscale → enhance) or the SAD would be
        // asymmetric. See process_analysis_frame.
        let mut tmp_half = Vec::new();
        let (a_hw, a_hh) = crate::alignment::downscale_2x_into(&tmp, rw, rh, &mut tmp_half);
        let (mut bt, mut bo, mut lo) = (
            vec![0u16; a_hw * a_hh],
            vec![0u16; a_hw * a_hh],
            vec![0u16; a_hw * a_hh],
        );
        // Phase 2: Convert the Reference Frame into a Noise-free, sharply enhanced edge map
        enhance_and_score_surface_buffered(&tmp_half, a_hw, a_hh, &mut bt, &mut bo, &mut lo);
        lo
    };

    let a_pyr = Some(downscale_integer(&a_mono, rw / 2, rh / 2, 2));

    // HYBRID ANALYSIS: la GPU produce half-res, blur, Laplaciano, métricas,
    // CoG, grilla AP y SAD grueso; CPU conserva selección, ventana fina,
    // refinamiento subpíxel y validación. Para ROI
    // pequeños Auto evita el readback (CPU es más rápida); Hybrid/GpuOnly
    // omiten sólo ese umbral, nunca la paridad ni los límites del dispositivo.
    // Auto conserva el umbral adaptativo. Hybrid es una petición explícita de
    // colaboración CPU+GPU y, como GpuOnly, no se degrada silenciosamente a
    // CPU sólo por el tamaño del ROI (sí conserva fallback por paridad/OOM).
    let gpu_analysis_profitable = rw.saturating_mul(rh) >= 300_000
        || matches!(compute_policy, ComputePolicy::Hybrid | ComputePolicy::GpuOnly);
    // Motivo del fallback a CPU (visible en la telemetría): el usuario no debe
    // adivinar por qué la etiqueta dice "scoring CPU".
    let mut gpu_off_reason: Option<String> = None;
    let gpu_analysis_enabled = if !compute_policy.allows_gpu() || !gpu_analysis_profitable {
        if compute_policy.allows_gpu() && !gpu_analysis_profitable {
            log_to_front(app, "INFO", "Análisis GPU omitido: ROI pequeña; CPU SIMD tiene menor latencia.");
            gpu_off_reason = Some("Auto: ROI pequeña, CPU más rápida".into());
        } else {
            gpu_off_reason = Some("CPU forzada por ajustes".into());
        }
        false
    } else {
        match crate::gpu_stack::gpu_runtime() {
            None if matches!(compute_policy, ComputePolicy::GpuOnly) => {
                return Err("GPU only solicitado para análisis, pero no existe un dispositivo wgpu compatible".into());
            }
            None => {
                gpu_off_reason = Some("sin GPU compatible".into());
                false
            }
            Some(_) if !crate::gpu_analysis::ensure_parity() => {
                if matches!(compute_policy, ComputePolicy::GpuOnly) {
                    return Err("GPU only: falló la paridad del preprocesado planetario".into());
                }
                log_to_front(app, "WARN", "Preprocesado GPU sin paridad; análisis completo en CPU.");
                gpu_off_reason = Some("paridad GPU no verificada".into());
                false
            }
            Some(rt) if matches!(compute_policy, ComputePolicy::Hybrid | ComputePolicy::GpuOnly) => {
                // Hybrid/GpuOnly NO usan el cronómetro: su resultado se
                // ignoraba (línea del `matches!` de abajo) y cualquier
                // tropiezo de la microprueba degradaba a CPU permanente.
                // Deciden paridad (ya validada) + presupuesto de VRAM
                // (aritmética pura, cero submits). El primer lote real
                // calienta los pipelines; un fallo operativo cae al fallback
                // por-lote existente (o error explícito en GPU only).
                match crate::gpu_analysis::validate_recommended_batch(rw, rh) {
                    Ok(vram) => {
                        log_to_front(
                            app,
                            "SUCCESS",
                            &format!(
                                "Análisis híbrido activo: GPU {} ({}) por lotes para pirámide/Laplaciano/métricas/CoG/SAD/AP + CPU validación y refinamiento · working-set {} MB.",
                                rt.backend,
                                rt.adapter_name,
                                vram / 1_048_576
                            ),
                        );
                        true
                    }
                    Err(e) => {
                        if matches!(compute_policy, ComputePolicy::GpuOnly) {
                            return Err(format!("GPU only: {e}"));
                        }
                        log_to_front(
                            app,
                            "WARN",
                            &format!("Hybrid sin VRAM suficiente para lotes ({e}); análisis en CPU."),
                        );
                        gpu_off_reason = Some("VRAM insuficiente para lotes".into());
                        false
                    }
                }
            }
            Some(rt) => {
                // AUTO: warm-up con la geometría real (crea buffers) y compara
                // una ejecución caliente contra la referencia CPU equivalente.
                // Aviso visible: la paridad + microprueba tardan un instante y
                // antes parecían una barra muerta en 0.
                emit_progress(
                    app,
                    &get_msg("Calibrando GPU (paridad y microprueba)..."),
                    4.0,
                    None,
                );
                let texture_probe = is_surface || large_disc;
                let run_gpu_probe = || -> Result<(), String> {
                    let g = crate::gpu_analysis::process_with_options(
                        &analysis_probe_mono,
                        rw,
                        rh,
                        texture_probe,
                        warping_analysis,
                        !texture_probe || cog_assist,
                    )?;
                    if let Some(pyr) = a_pyr.as_ref() {
                        let (hw, hh) = (rw / 2, rh / 2);
                        let mut quarter = Vec::new();
                        let (sw, sh) = crate::alignment::downscale_2x_into(
                            &g.laplacian,
                            hw,
                            hh,
                            &mut quarter,
                        );
                        // Microprueba SAD REPRESENTATIVA: rejilla 3×3 de puntos
                        // con caja y radio como los AP reales del análisis por
                        // frame. La versión anterior lanzaba UNA caja de
                        // sw/2×sh/2 con r=32: cada SadPoint es UN hilo GPU, y
                        // ese único hilo barría ~140-550 M de píxeles — medido
                        // 9.2 s a 1080p en Apple M5 (y ×4 a 4K, dos corridas).
                        // Eso disparaba el watchdog de Metal (command buffer
                        // asesinado → "scoring CPU" permanente) o dejaba el
                        // readback esperando para siempre (análisis SER
                        // congelado en 0 con Cancelar muerto).
                        let box_size = 24i32
                            .min((sw as i32 / 4).max(8))
                            .min((sh as i32 / 4).max(8));
                        let mut points = Vec::with_capacity(9);
                        for gy in 1..=3i32 {
                            for gx in 1..=3i32 {
                                let x = (sw as i32 * gx) / 4;
                                let y = (sh as i32 * gy) / 4;
                                points.push(crate::gpu_analysis::SadPoint::new(
                                    x, y, x, y, box_size, 16, true,
                                ));
                            }
                        }
                        let _ = crate::gpu_analysis::search_sad_points(
                            pyr,
                            &quarter,
                            sw,
                            sh,
                            &points,
                        )?;
                    }
                    Ok(())
                };
                let warm = {
                    let _s = crate::perf_trace::span(pt, "gpu_probe");
                    run_gpu_probe()
                };
                // MEDICIÓN JUSTA para Auto: el camino real procesa LOTES
                // (recommended_batch_len frames por submit/readback). Antes se
                // cronometraba UN solo frame — todo el overhead de submit+map
                // caía sobre él y Auto elegía CPU en equipos donde el lote
                // real es 2-4× más rápido (medido en M5: 4144×2822 → 26.6
                // ms/frame en lote de 4 vs ~64 ms/frame de scoring CPU).
                let probe_batch_len = crate::gpu_analysis::recommended_batch_len(rw, rh);
                let gpu_t0 = std::time::Instant::now();
                let gpu_probe = warm.and_then(|_| {
                    let frames: Vec<Vec<u16>> = (0..probe_batch_len)
                        .map(|_| analysis_probe_mono.clone())
                        .collect();
                    crate::gpu_analysis::process_batch_with_options(
                        &frames,
                        rw,
                        rh,
                        texture_probe,
                        warping_analysis,
                        !texture_probe || cog_assist,
                    )
                    .map(|_| ())
                });
                let gpu_s = gpu_t0.elapsed().as_secs_f32() / probe_batch_len.max(1) as f32;
                let mut half = vec![0u16; (rw / 2) * (rh / 2)];
                let mut bt = vec![0u16; half.len()];
                let mut bo = vec![0u16; half.len()];
                let mut lo = vec![0u16; half.len()];
                let cpu_t0 = std::time::Instant::now();
                let (pw, ph) = crate::alignment::downscale_2x_into(&analysis_probe_mono, rw, rh, &mut half);
                let _ = enhance_and_score_surface_buffered(&half, pw, ph, &mut bt, &mut bo, &mut lo);
                if !texture_probe || cog_assist {
                    let _ = compute_robust_geometric_center(&analysis_probe_mono, rw, rh, 0, 0);
                }
                if warping_analysis {
                    let input = if texture_probe { &bo } else { &half };
                    let _ = calculate_grid_quality(input, pw, ph, 40, texture_probe);
                }
                if let Some(pyr) = a_pyr.as_ref() {
                    let _ = crate::alignment::find_best_match_sad_pyramid_offset(
                        &a_mono,
                        &lo,
                        pyr,
                        pw,
                        ph,
                        pw / 2,
                        ph / 2,
                        pw / 4,
                        ph / 4,
                        pw / 2,
                        ph / 2,
                        64,
                        2,
                        16,
                        0,
                        0,
                    );
                }
                let cpu_s = cpu_t0.elapsed().as_secs_f32();
                if let Err(e) = gpu_probe {
                    log_to_front(app, "WARN", &format!("Análisis GPU no utilizable ({e}); CPU."));
                    gpu_off_reason = Some("GPU no utilizable en la microprueba".into());
                    false
                } else if gpu_s < cpu_s {
                    log_to_front(
                        app,
                        "SUCCESS",
                        &format!(
                            "Análisis híbrido activo (Auto): GPU {} ({}) {:.2} ms/frame en lote vs CPU {:.2} ms/frame · GPU por lotes para pirámide/Laplaciano/métricas/CoG/SAD/AP + CPU validación y refinamiento.",
                            rt.backend, rt.adapter_name, gpu_s * 1000.0, cpu_s * 1000.0
                        ),
                    );
                    true
                } else {
                    log_to_front(
                        app,
                        "INFO",
                        &format!(
                            "Análisis: CPU elegida por benchmark ({:.2} ms vs GPU {:.2} ms/frame en lote); sin regresión por overhead de readback.",
                            cpu_s * 1000.0, gpu_s * 1000.0
                        ),
                    );
                    gpu_off_reason = Some(format!(
                        "Auto eligió CPU ({:.0} ms vs GPU {:.0} ms/frame)",
                        cpu_s * 1000.0,
                        gpu_s * 1000.0
                    ));
                    false
                }
            }
        }
    };
    let gpu_analysis_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Motivo actual del scoring CPU para la telemetría: la decisión inicial o,
    // si la GPU murió a mitad, el fallo en caliente.
    let analysis_cpu_reason = {
        let failed = gpu_analysis_failed.clone();
        move || -> Option<String> {
            if failed.load(Ordering::Relaxed) {
                Some("la GPU falló en un lote; resto en CPU".into())
            } else {
                gpu_off_reason.clone()
            }
        }
    };

    let rt = r.clone();
    let app_c = app.clone();
    let ctr = std::sync::Arc::new(AtomicUsize::new(0));
    // Para vídeo comprimido de color, el análisis sólo necesita el canal verde
    // (la misma señal exacta que históricamente extraíamos del RGB48 en Rust).
    // Conservamos RGB48 únicamente cuando el vídeo completo cabe con garantía
    // en el caché NVMe y, por tanto, esa pasada evita un segundo decode al apilar.
    let ffmpeg_native_stream_bpp = if ffmpeg_stream_is_color(cid) {
        6usize
    } else {
        2usize
    };
    let ffmpeg_analysis_cache_plan: Option<(PathBuf, u64, usize, usize)> = if r.is_ffmpeg() {
        let full_frame_decode = rx == 0 && ry == 0 && rw == tw && rh == th;
        let cache_dir = decode_cache_root_dir();
        let _ = std::fs::create_dir_all(&cache_dir);
        let cache_budget = decode_cache_budget_bytes(&cache_dir);
        let expected_len = tw
            .saturating_mul(th)
            .saturating_mul(ffmpeg_native_stream_bpp / 2);
        // Usar el peor caso real del archivo LZ4 evita elegir RGB por una
        // compresión supuesta y acabar con un prefijo incompleto que no sirve a
        // la selección lucky. La transacción sigue midiendo bytes comprimidos.
        let guaranteed_video_bytes = decode_cache_max_file_bytes(expected_len)
            .saturating_mul(tf.min(u64::MAX as usize) as u64);
        if full_frame_decode
            && guaranteed_video_bytes > 0
            && guaranteed_video_bytes <= cache_budget
        {
            prune_decode_cache_to_budget(
                &cache_dir,
                cache_budget.saturating_sub(guaranteed_video_bytes),
            );
            Some((cache_dir, cache_budget, expected_len, tf))
        } else {
            None
        }
    } else {
        None
    };
    let ffmpeg_analysis_green = r.is_ffmpeg()
        && ffmpeg_stream_is_color(cid)
        && ffmpeg_analysis_cache_plan.is_none();
    let analysis_stream_bpp = if ffmpeg_analysis_green { 2 } else { tbp };
    // El pipe G16 ya es un plano mono terminado; no debe volver a pasar por la
    // lógica RGB/Bayer ni alterar el ColorID contractual del resto del pipeline.
    let analysis_stream_color_id = if ffmpeg_analysis_green { 0 } else { cid };
    let a_pyr_ref = &a_pyr;
    let a_mono_ref = &a_mono;
    let analyze_preprocessed = |i: usize,
                                raw: &[u8],
                                bufs: &mut AnalysisBufferSet,
                                gpu_bundle: Option<(crate::gpu_analysis::AnalysisGpuOutput, Vec<u16>)>|
     -> FrameAlignmentData {
        let (gpu, prepared_mono) = match gpu_bundle {
            Some((gpu, mono)) => (Some(gpu), Some(mono)),
            None => (None, None),
        };
        process_analysis_frame(
            i,
            raw,
            bufs,
            rx,
            ry,
            rw,
            rh,
            tw,
            th,
            analysis_stream_bpp,
            analysis_stream_color_id,
            is_surface,
            warping_analysis,
            a_pyr_ref.as_ref(),
            a_mono_ref,
            cog_assist,
            ref_cog_cx,
            ref_cog_cy,
            &target_type, // NEW
            large_disc,
            gpu_analysis_enabled.then_some(gpu_analysis_failed.as_ref()),
            gpu,
            prepared_mono,
        )
    };
    let analyze_fn = |i: usize, raw: &[u8], bufs: &mut AnalysisBufferSet| -> FrameAlignmentData {
        analyze_preprocessed(i, raw, bufs, None)
    };
    // Convierte luma en CPU y entrega 2–3 frames por submit. La conversión del
    // lote siguiente ocurre en el productor de decode mientras este closure
    // espera al GPU; CPU valida/refina los resultados después del readback.
    let preprocess_gpu_batch = |items: &[(usize, Vec<u8>)]|
     -> Result<Vec<(crate::gpu_analysis::AnalysisGpuOutput, Vec<u16>)>, String> {
        // Conversión raw→mono EN PARALELO: en serie costaba 50-90 ms por lote
        // de 6 frames de 11.7 Mpx con el pool rayon parado. Cada frame es
        // independiente y par_iter preserva el orden → salida bit-idéntica.
        let monos: Vec<Vec<u16>> = items
            .par_iter()
            .map_init(Vec::<u16>::new, |decode_scratch, (_, raw)| {
                let mut mono = Vec::with_capacity(rw * rh);
                raw_to_analysis_mono_into(
                    raw,
                    rw,
                    rh,
                    analysis_stream_bpp,
                    analysis_stream_color_id,
                    rx,
                    ry,
                    decode_scratch,
                    &mut mono,
                );
                mono
            })
            .collect();
        if monos.iter().any(|mono| mono.len() < rw * rh) {
            return Err("Frame truncado durante la preparación del lote GPU".into());
        }
        let outputs = crate::gpu_analysis::process_batch_with_options(
            &monos,
            rw,
            rh,
            is_surface || large_disc,
            warping_analysis,
            !(is_surface || large_disc) || cog_assist,
        )?;
        Ok(outputs.into_iter().zip(monos).collect())
    };
    // TELEMETRIA EN VIVO de la fase de analisis (evento stack_telemetry,
    // phase="analysis"): throughput y recursos visibles tambien aqui, en los
    // 3 formatos. En Hybrid se etiqueta la GPU sólo después de self-test.
    // Lote GPU adaptativo: 6/4/2 frames por submit según presupuesto de VRAM
    // (antes fijo en 3 — las GPUs amplias desperdiciaban amortización).
    let gpu_batch_len = if gpu_analysis_enabled {
        crate::gpu_analysis::recommended_batch_len(rw, rh)
    } else {
        3
    };
    let ana_start = std::time::Instant::now();
    let ana_reader_kind = r.reader_kind_label();
    let ana_sys: std::sync::Arc<std::sync::Mutex<System>> =
        std::sync::Arc::new(std::sync::Mutex::new(System::new()));
    let mut stats = Vec::new();
    if r.is_ffmpeg() {
        let bin = match rt {
            VideoInput::Ffmpeg(ref f) => f.ffmpeg_path.as_str(),
            _ => "ffmpeg",
        };
        // El micro-benchmark usa una muestra acotada por bytes; sin este aviso
        // incluso esa prueba corta parecía una barra muerta en 0.
        emit_progress(
            app,
            &get_msg("Midiendo decodificación GPU vs CPU (prueba corta)..."),
            4.0,
            None,
        );
        let decode_probe = {
            let _s = crate::perf_trace::span(pt, "decode_probe");
            benchmark_ffmpeg_decode_route(
                bin,
                path,
                rt.codec_name(),
                cid,
                tw,
                th,
                rt.rotation(),
                Some(&job_token),
            )
        };
        let fmt_probe = |v: Option<f32>| v.map(|s| format!("{:.2}s", s)).unwrap_or_else(|| "falló".into());
        if decode_probe.prefer_hardware {
            log_to_front(
                app,
                "SUCCESS",
                &format!(
                    "FFmpeg: decode {} confirmado y más rápido (GPU {} / CPU {}).",
                    decode_probe.backend.as_deref().unwrap_or("hardware"),
                    fmt_probe(decode_probe.hardware_seconds),
                    fmt_probe(decode_probe.cpu_seconds)
                ),
            );
        } else {
            log_to_front(
                app,
                "INFO",
                &format!(
                    "FFmpeg: se elige decode CPU (GPU confirmada={} · GPU {} / CPU {}).",
                    decode_probe.hardware_confirmed,
                    fmt_probe(decode_probe.hardware_seconds),
                    fmt_probe(decode_probe.cpu_seconds)
                ),
            );
        }
        // PR-2.1: sólo sembrar RGB16 cuando el vídeo completo cabe garantizado.
        // En capturas grandes usamos el pipe G16 exacto: 2 B/px, sin un caché
        // parcial inútil y sin triplicar el ring de análisis.
        let ana_cache_ctx = ffmpeg_analysis_cache_plan.clone();
        if ffmpeg_analysis_green {
            log_to_front(
                app,
                "SUCCESS",
                "Análisis FFmpeg G16 exacto: 66.7% menos transferencia y RAM de ring; el apilado conserva RGB/Bayer completo.",
            );
        }
        let decode_order = if decode_probe.prefer_hardware { [true, false] } else { [false, true] };
        let expected_is_exact = match &rt {
            VideoInput::Ffmpeg(reader) => reader.frame_count_exact,
            _ => true,
        };
        let mut decode_failures = Vec::<String>::new();
        for use_gpu in decode_order {
            ctr.store(0, Ordering::Relaxed);
            if use_gpu {
                emit_progress(app, "Modo Turbo (GPU)...", 0.0, None);
                log_to_front(app, "INFO", "Intentando decodificacion por hardware (GPU)...");
            } else {
                emit_progress(app, "Modo Turbo (CPU Fallback)...", 0.0, None);
                log_to_front(app, "WARNING", "Reintentando decodificacion por software (CPU fallback)...");
            }
            let decode_cancel = {
                let token = job_token.clone();
                Arc::new(move || token.is_cancelled()) as FfmpegCancelCheck
            };
            let iterator = if ffmpeg_analysis_green {
                FfmpegStreamIterator::new_cancelable_analysis_green(
                    path,
                    tw,
                    th,
                    rx,
                    ry,
                    rw,
                    rh,
                    cid,
                    bin,
                    None,
                    if use_gpu {
                        decode_probe.backend.as_deref()
                    } else {
                        None
                    },
                    rt.codec_name(),
                    rt.rotation(),
                    decode_cancel,
                )
            } else {
                FfmpegStreamIterator::new_cancelable(
                    path,
                    tw,
                    th,
                    rx,
                    ry,
                    rw,
                    rh,
                    cid,
                    bin,
                    None,
                    if use_gpu {
                        decode_probe.backend.as_deref()
                    } else {
                        None
                    },
                    rt.codec_name(),
                    rt.rotation(),
                    decode_cancel,
                )
            };
            let mut it = match iterator {
                Ok(iterator) => iterator,
                Err(error) => {
                    decode_failures.push(format!(
                        "No se pudo iniciar decode {}: {error}",
                        if use_gpu { "GPU" } else { "CPU" }
                    ));
                    continue;
                }
            };
                // Staging exclusivo por intento. El hardware y el fallback
                // CPU jamás comparten archivos pendientes; sólo el intento
                // cuyo EOF/exit status pasa la validación puede hacer commit.
                let decode_route = ffmpeg_decode_route_label(
                    bin,
                    use_gpu.then_some(
                        decode_probe.backend.as_deref().unwrap_or("unconfirmed")
                    ),
                );
                let route_key = ffmpeg_decode_cache_key(
                    path,
                    tw,
                    th,
                    r.bpp(),
                    cid,
                    rt.rotation(),
                    rt.codec_name(),
                    &decode_route,
                );
                let mut ana_cache_attempt = ana_cache_ctx.as_ref().and_then(
                    |(cache_dir, budget, expected_len, expected_frames)| {
                        DecodeFrameCacheTransaction::new(
                            cache_dir,
                            route_key,
                            *expected_len,
                            *budget,
                        )
                        .map(|transaction| {
                            transaction.with_expected_indices(0..*expected_frames)
                        })
                    },
                );
                let cs = ctr.clone();
                let sc = job_token.clone();
                let ac = app_c.clone();
                let ana_sys_c = ana_sys.clone();
                let ana_threads_c = ram_aware_analysis_threads(rw, rh, is_color, true);

                // OPT FFMPEG 2.5: TRUE ZERO-ALLOCATION (DUAL RING BUFFER)
                // Ring dimensionado por hilos y RAM: los 32 slots fijos eran
                // 1.6 GB residentes en 4K rgb48le. hilos×2+2 conserva el
                // solape decode/análisis; el presupuesto (25% de la RAM libre)
                // protege a los equipos justos de memoria.
                let frame_size = rw.saturating_mul(rh).saturating_mul(analysis_stream_bpp);
                let mut ring_slots = (ana_threads_c * 2 + 2).clamp(8, 32);
                let ring_budget = {
                    let mut sys_ring = System::new();
                    sys_ring.refresh_memory();
                    (sys_ring.available_memory() / 4).max(64 * 1024 * 1024)
                };
                while ring_slots > 4
                    && (ring_slots as u64).saturating_mul(frame_size as u64) > ring_budget
                {
                    ring_slots -= 1;
                }
                let (tx_full, rx_full) =
                    crossbeam_channel::bounded::<(usize, Vec<u8>)>(ring_slots);
                let (tx_empty, rx_empty) = crossbeam_channel::bounded::<Vec<u8>>(ring_slots);
                for _ in 0..ring_slots {
                    let _ = tx_empty.send(vec![0u8; frame_size]);
                }

                let producer_token = job_token.clone();
                let producer = std::thread::spawn(move || {
                    // perf_trace: tiempo de pared del hilo decodificador completo
                    // (incluye esperas de backpressure del ring).
                    let _s = crate::perf_trace::span(pt, "decode_thread");
                    let mut frame_idx = 0;
                    loop {
                        // recv con timeout: si el consumidor está dentro de un
                        // lote GPU largo y el usuario cancela, el productor debe
                        // despertar SOLO, romper el bucle y dropear `it` — el
                        // Drop mata y cosecha el proceso ffmpeg hijo (antes
                        // nadie lo mataba hasta que el consumidor drenara).
                        let mut buffer = match rx_empty
                            .recv_timeout(std::time::Duration::from_millis(200))
                        {
                            Ok(buffer) => buffer,
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                                if producer_token.is_cancelled() {
                                    return StreamDecodeOutcome::Cancelled { decoded: frame_idx };
                                }
                                continue;
                            }
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                                return if producer_token.is_cancelled() {
                                    StreamDecodeOutcome::Cancelled { decoded: frame_idx }
                                } else {
                                    StreamDecodeOutcome::Failed {
                                        decoded: frame_idx,
                                        error: "el consumidor cerró el ring antes de EOF".into(),
                                    }
                                };
                            }
                        };
                        if producer_token.is_cancelled() {
                            return StreamDecodeOutcome::Cancelled { decoded: frame_idx };
                        }
                        if it.read_frame_into(&mut buffer) {
                            let mut pending = (frame_idx, buffer);
                            loop {
                                match tx_full.send_timeout(
                                    pending,
                                    std::time::Duration::from_millis(200),
                                ) {
                                    Ok(()) => break,
                                    Err(crossbeam_channel::SendTimeoutError::Timeout(value)) => {
                                        if producer_token.is_cancelled() {
                                            return StreamDecodeOutcome::Cancelled {
                                                decoded: frame_idx,
                                            };
                                        }
                                        pending = value;
                                    }
                                    Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                                        return if producer_token.is_cancelled() {
                                            StreamDecodeOutcome::Cancelled { decoded: frame_idx }
                                        } else {
                                            StreamDecodeOutcome::Failed {
                                                decoded: frame_idx,
                                                error: "el consumidor cerró el canal de frames".into(),
                                            }
                                        };
                                    }
                                }
                            }
                            frame_idx += 1;
                        } else {
                            return match it.finish_status() {
                                Ok(()) => StreamDecodeOutcome::Complete { decoded: frame_idx },
                                Err(error) => StreamDecodeOutcome::Failed {
                                    decoded: frame_idx,
                                    error,
                                },
                            };
                        }
                    }
                });

                // Consumer Side (Lock-Free SIMD with Recycler)
                let analysis_pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(ana_threads_c)
                    .build()
                    .unwrap();
                let local_stats: Vec<FrameAlignmentData> = if gpu_analysis_enabled {
                    let mut stream = rx_full.into_iter();
                    let mut collected = Vec::with_capacity(tf);
                    loop {
                        // Lote adaptativo (recommended_batch_len): equilibra
                        // latencia, VRAM y amortización del submit. El productor
                        // continúa decodificando en los 32 buffers del ring
                        // mientras este lote está en GPU.
                        let dw_t0 = std::time::Instant::now();
                        let items: Vec<(usize, Vec<u8>)> =
                            stream.by_ref().take(gpu_batch_len).collect();
                        crate::perf_trace::add_ns(
                            pt,
                            crate::perf_trace::GLOBAL_PASS,
                            "decode_wait",
                            dw_t0.elapsed().as_nanos(),
                            items.len() as u64,
                        );
                        if items.is_empty() { break; }
                        // Cancelación a nivel de lote: evita mandar trabajo a la
                        // GPU cuando el usuario ya canceló (los workers CPU de
                        // abajo también lo comprueban por frame).
                        if sc.is_cancelled() { break; }
                        let overrides: Vec<Option<(crate::gpu_analysis::AnalysisGpuOutput, Vec<u16>)>> =
                            if gpu_analysis_failed.load(Ordering::Relaxed) {
                                (0..items.len()).map(|_| None).collect()
                            } else {
                                let _s = crate::perf_trace::span(pt, "gpu_preprocess")
                                    .items(items.len() as u64);
                                // PR-20: la conversión raw→mono corre DENTRO de
                                // analysis_pool — antes iba al pool global de
                                // rayon y competía con éste por los núcleos.
                                match analysis_pool.install(|| preprocess_gpu_batch(&items)) {
                                    Ok(outputs) => outputs.into_iter().map(Some).collect(),
                                    Err(e) => {
                                        gpu_analysis_failed.store(true, Ordering::Relaxed);
                                        log_to_front(
                                            &ac,
                                            "WARN",
                                            &format!("GPU Metal falló en un lote FFmpeg ({e}); continuando en CPU."),
                                        );
                                        (0..items.len()).map(|_| None).collect()
                                    }
                                }
                            };
                        let score_n = items.len() as u64;
                        let score_t0 = std::time::Instant::now();
                        let analyzed: Vec<(FrameAlignmentData, Vec<u8>)> = analysis_pool.install(|| {
                            items
                                .into_par_iter()
                                .zip(overrides.into_par_iter())
                                .map_init(
                                    || AnalysisBufferSet::new(rw * rh),
                                    |bs, ((i, raw), gpu)| {
                                        let c = cs.fetch_add(1, Ordering::Relaxed);
                                        if sc.is_cancelled() {
                                            return (FrameAlignmentData::empty(i), raw);
                                        }
                                        if c % 50 == 0 {
                                            // tf puede ser una ESTIMACION (duración×fps): el
                                            // total mostrado nunca queda por debajo del conteo
                                            // real y el % no se pasa de 100.
                                            let shown_total = tf.max(c + 1);
                                            emit_progress(
                                                &ac,
                                                &format!("Analizando: {}/{}", c, shown_total),
                                                5.0 + (c as f32 / shown_total as f32) * 90.0,
                                                None,
                                            );
                                            emit_analysis_telemetry(
                                                &ac,
                                                &pipeline_job_id,
                                                ana_reader_kind,
                                                Some(use_gpu && decode_probe.hardware_confirmed),
                                                !gpu_analysis_failed.load(Ordering::Relaxed),
                                                analysis_cpu_reason(),
                                                rw.saturating_mul(rh).saturating_mul(4),
                                                crate::gpu_analysis::estimated_batch_vram_mb(rw, rh, gpu_batch_len),
                                                c,
                                                shown_total,
                                                ana_start,
                                                ana_threads_c,
                                                &ana_sys_c,
                                            );
                                        }
                                        let result = analyze_preprocessed(i, &raw, bs, gpu);
                                        // PR-2.1: sembrar el caché del apilado.
                                        if let Some(transaction) = ana_cache_attempt.as_ref() {
                                            transaction.write_raw_le_u16(i, &raw);
                                        }
                                        (result, raw)
                                    },
                                )
                                .collect()
                        });
                        crate::perf_trace::add_ns(
                            pt,
                            crate::perf_trace::GLOBAL_PASS,
                            "score",
                            score_t0.elapsed().as_nanos(),
                            score_n,
                        );
                        for (result, raw) in analyzed {
                            collected.push(result);
                            let _ = tx_empty.send(raw);
                        }
                    }
                    collected
                } else {
                    // perf_trace: en la rama CPU decode y scoring van entrelazados
                    // (par_bridge); "cpu_pass" mide el conjunto y "decode_thread"
                    // el decodificador — la resta aproxima el coste de scoring.
                    let _s = crate::perf_trace::span(pt, "cpu_pass");
                    analysis_pool.install(|| rx_full
                        .into_iter()
                        .par_bridge()
                        .map_init(
                            || AnalysisBufferSet::new(rw * rh),
                            |bs, (i, raw): (usize, Vec<u8>)| {
                            let c = cs.fetch_add(1, Ordering::Relaxed);
                            // Cancel check on EVERY frame (atomic load ≈ free).
                            // The old per-50 check only skipped THAT one frame;
                            // all others still ran the full analysis (minutes
                            // of dead work after pressing Cancel).
                            if sc.is_cancelled() {
                                // Guaranteed recycler path: supersession clears
                                // the global cancel flag, so failing to return
                                // this Vec exhausted the ring and deadlocked the
                                // producer forever.
                                let _ = tx_empty.send(raw);
                                return FrameAlignmentData::empty(i);
                            }
                            if c % 50 == 0 {
                                // tf estimado (duración×fps): mismo saneo que la rama GPU.
                                let shown_total = tf.max(c + 1);
                                emit_progress(
                                    &ac,
                                    &format!("Analizando: {}/{}", c, shown_total),
                                    5.0 + (c as f32 / shown_total as f32) * 90.0,
                                    None,
                                );
                                // use_gpu = rama activa del decode FFmpeg
                                // (VideoToolbox/NVDEC/D3D11VA) o su fallback CPU.
                                emit_analysis_telemetry(
                                    &ac, &pipeline_job_id, ana_reader_kind, Some(use_gpu && decode_probe.hardware_confirmed),
                                    gpu_analysis_enabled && !gpu_analysis_failed.load(Ordering::Relaxed),
                                    analysis_cpu_reason(),
                                    rw.saturating_mul(rh).saturating_mul(4),
                                    crate::gpu_analysis::estimated_batch_vram_mb(rw, rh, gpu_batch_len), c, shown_total,
                                    ana_start, ana_threads_c, &ana_sys_c,
                                );
                            }
                            let result = analyze_fn(i, &raw, bs);
                            // PR-2.1: sembrar el caché del apilado.
                            if let Some(transaction) = ana_cache_attempt.as_ref() {
                                transaction.write_raw_le_u16(i, &raw);
                            }
                            // Send empty block back to the producer pool
                            let _ = tx_empty.send(raw);
                            result
                            },
                        )
                        .collect())
                };

                let outcome = producer.join().unwrap_or_else(|_| StreamDecodeOutcome::Failed {
                    decoded: local_stats.len(),
                    error: "panic en el productor de decodificación".into(),
                });
                match validate_stream_decode(outcome, tf, expected_is_exact) {
                    Ok(decoded) if local_stats.len() == decoded => {
                        if job_token.is_cancelled() {
                            return Err("Cancelado o sustituido por otro análisis".into());
                        }
                        if let Some(transaction) = ana_cache_attempt.as_mut() {
                            transaction.commit();
                        }
                        stats = local_stats;
                        break;
                    }
                    Ok(decoded) => decode_failures.push(format!(
                        "FFmpeg entregó {decoded} frames, pero sólo se analizaron {}",
                        local_stats.len()
                    )),
                    Err(error) => {
                        if job_token.is_cancelled() {
                            return Err("Cancelado o sustituido por otro análisis".into());
                        }
                        decode_failures.push(error);
                    }
                }
        }
        if stats.is_empty() && !decode_failures.is_empty() {
            return Err(format!(
                "No se pudo completar la decodificación sin pérdida de frames: {}",
                decode_failures.join(" · ")
            ));
        }
    }
    if stats.is_empty() {
        let threads = ram_aware_analysis_threads(rw, rh, is_color, r.is_ffmpeg());
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        let cf = ctr.clone();
        let sc = job_token.clone();
        let ac = app_c.clone();
        let ana_sys_c = ana_sys.clone();
        emit_progress(app, &get_msg("Analizando frames..."), 5.0, None);
        stats = if gpu_analysis_enabled {
            let (tx, rx_batches) = crossbeam_channel::bounded::<Result<Vec<(usize, Vec<u8>)>, String>>(2);
            let source = unified_source.clone();
            let producer_token = job_token.clone();
            let producer = std::thread::spawn(move || {
                for start in (0..tf).step_by(gpu_batch_len) {
                    if producer_token.is_cancelled() { break; }
                    let indices: Vec<usize> = (start..(start + gpu_batch_len).min(tf)).collect();
                    let result = source
                        .read_batch(
                            &indices,
                            Some(FrameRoi { x: rx, y: ry, width: rw, height: rh }),
                        )
                        .map(|batch| {
                            debug_assert_eq!(batch.indices, indices);
                            debug_assert_eq!((batch.descriptor.width, batch.descriptor.height), (tw, th));
                            debug_assert!(batch.roi.is_some_and(|roi| roi.width == rw && roi.height == rh));
                            batch.indices.into_iter().zip(batch.frames).collect()
                        });
                    // send con timeout: si el consumidor quedara retenido (lote
                    // GPU al límite del deadline) y el usuario cancela, este
                    // hilo debe poder terminar por sí mismo.
                    let mut pending = result;
                    loop {
                        match tx.send_timeout(pending, std::time::Duration::from_millis(200)) {
                            Ok(()) => break,
                            Err(crossbeam_channel::SendTimeoutError::Timeout(value)) => {
                                if producer_token.is_cancelled() {
                                    return;
                                }
                                pending = value;
                            }
                            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => return,
                        }
                    }
                }
            });
            let mut collected = Vec::with_capacity(tf);
            emit_analysis_telemetry(
                app,
                &pipeline_job_id,
                ana_reader_kind,
                None,
                true,
                None,
                rw.saturating_mul(rh).saturating_mul(4),
                crate::gpu_analysis::estimated_batch_vram_mb(rw, rh, gpu_batch_len),
                0,
                tf,
                ana_start,
                threads,
                &ana_sys,
            );
            loop {
                // recv con timeout: CANCELAR debe responder aunque el productor
                // o la GPU se atasquen (antes un recv() bloqueante ignoraba el
                // flag hasta el siguiente lote... que podía no llegar jamás).
                let result = match rx_batches
                    .recv_timeout(std::time::Duration::from_millis(200))
                {
                    Ok(result) => result,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        if sc.is_cancelled() {
                            break;
                        }
                        continue;
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                };
                let items = match result {
                    Ok(items) => items,
                    Err(error) => {
                        drop(rx_batches);
                        let _ = producer.join();
                        return Err(error);
                    }
                };
                if collected.is_empty() {
                    emit_progress(app, "SER: primer lote recibido; procesando en GPU...", 5.0, None);
                }
                let overrides: Vec<Option<(crate::gpu_analysis::AnalysisGpuOutput, Vec<u16>)>> =
                    if gpu_analysis_failed.load(Ordering::Relaxed) {
                        (0..items.len()).map(|_| None).collect()
                    } else {
                        let _s = crate::perf_trace::span(pt, "gpu_preprocess")
                            .items(items.len() as u64);
                        // PR-20: conversión dentro del pool acotado del análisis
                        // (no en el global) — un solo presupuesto de hilos.
                        match pool.install(|| preprocess_gpu_batch(&items)) {
                            Ok(outputs) => outputs.into_iter().map(Some).collect(),
                            Err(e) => {
                                gpu_analysis_failed.store(true, Ordering::Relaxed);
                                log_to_front(
                                    app,
                                    "WARN",
                                    &format!("GPU Metal falló en un lote SER/AVI/FITS ({e}); continuando en CPU."),
                                );
                                (0..items.len()).map(|_| None).collect()
                            }
                        }
                    };
                let score_n = items.len() as u64;
                let score_t0 = std::time::Instant::now();
                let batch_stats: Vec<FrameAlignmentData> = pool.install(|| {
                    items
                        .into_par_iter()
                        .zip(overrides.into_par_iter())
                        .map_init(
                            || AnalysisBufferSet::new(rw * rh),
                            |bs, ((i, raw), gpu)| {
                                let c = cf.fetch_add(1, Ordering::Relaxed);
                                if sc.is_cancelled() {
                                    return FrameAlignmentData::empty(i);
                                }
                                if c % 10 == 0 {
                                    emit_progress(
                                        &ac,
                                        &format!("Analizando: {}/{}", c, tf),
                                        5.0 + (c as f32 / tf as f32) * 90.0,
                                        None,
                                    );
                                    emit_analysis_telemetry(
                                        &ac,
                                        &pipeline_job_id,
                                        ana_reader_kind,
                                        None,
                                        !gpu_analysis_failed.load(Ordering::Relaxed),
                                        analysis_cpu_reason(),
                                        rw.saturating_mul(rh).saturating_mul(4),
                                        crate::gpu_analysis::estimated_batch_vram_mb(rw, rh, gpu_batch_len),
                                        c,
                                        tf,
                                        ana_start,
                                        threads,
                                        &ana_sys_c,
                                    );
                                }
                                analyze_preprocessed(i, &raw, bs, gpu)
                            },
                        )
                        .collect()
                });
                crate::perf_trace::add_ns(
                    pt,
                    crate::perf_trace::GLOBAL_PASS,
                    "score",
                    score_t0.elapsed().as_nanos(),
                    score_n,
                );
                collected.extend(batch_stats);
                if sc.is_cancelled() { break; }
            }
            drop(rx_batches);
            let _ = producer.join();
            collected
        } else {
            let _s = crate::perf_trace::span(pt, "cpu_pass").items(tf as u64);
            let native_stats: Result<Vec<FrameAlignmentData>, String> = pool.install(|| {
                (0..tf)
                    .into_par_iter()
                    .with_min_len(32)
                    .map_init(
                        || (unified_source.clone(), AnalysisBufferSet::new(rw * rh)),
                        |(rl, bs), i| {
                        let c = cf.fetch_add(1, Ordering::Relaxed);
                        // Cancel check on EVERY frame — this is the SER path:
                        // the old per-10 check only skipped that single frame,
                        // so cancelling a SER analysis still burned through the
                        // whole remaining video.
                        if sc.is_cancelled() {
                            return Ok(FrameAlignmentData::empty(i));
                        }
                        if c % 10 == 0 {
                            emit_progress(
                                &ac,
                                &format!("Analizando: {}/{}", c, tf),
                                5.0 + (c as f32 / tf as f32) * 90.0,
                                None,
                            );
                            // Lector nativo (SER/AVI/FITS): mmap directo, sin
                            // decode → None (no aplica GPU de decodificacion).
                            emit_analysis_telemetry(
                                &ac, &pipeline_job_id, ana_reader_kind, None,
                                gpu_analysis_enabled && !gpu_analysis_failed.load(Ordering::Relaxed),
                                analysis_cpu_reason(),
                                rw.saturating_mul(rh).saturating_mul(4),
                                crate::gpu_analysis::estimated_batch_vram_mb(rw, rh, gpu_batch_len), c, tf, ana_start,
                                threads, &ana_sys_c,
                            );
                        }
                        let raw = require_native_analysis_frame(
                            rl.read_batch(
                                &[i],
                                Some(FrameRoi { x: rx, y: ry, width: rw, height: rh }),
                            ),
                            i,
                        )?;
                        Ok(analyze_fn(i, &raw, bs))
                        },
                    )
                    .collect()
            });
            native_stats?
        };
    }
    if gpu_analysis_enabled && gpu_analysis_failed.load(Ordering::Relaxed) {
        if matches!(compute_policy, ComputePolicy::GpuOnly) {
            return Err(
                "GPU only: el dispositivo falló durante el preprocesado del análisis; el resultado CPU de respaldo fue descartado."
                    .into(),
            );
        }
        log_to_front(
            app,
            "WARN",
            "La GPU falló durante el análisis; los frames restantes y el frame afectado se procesaron por CPU.",
        );
    }
    if stats.is_empty() {
        return Err("Error: Sin frames.".into());
    }
    // Cancelacion: los workers devolvieron frames vacios a partir del aviso —
    // abortar AHORA, antes de escribir un cache de analisis corrupto.
    if job_token.is_cancelled() {
        return Err("Cancelado o sustituido por otro análisis".into());
    }
    // par_bridge no promete orden de salida. Canonicalizar por índice antes
    // de construir quality_graph y rechazar huecos/duplicados evita publicar
    // un caché aparentemente válido que luego asocia score al frame equivocado.
    stats.sort_unstable_by_key(|frame| frame.idx);
    let mut qg: Vec<f32> = stats.iter().map(|s| s.score as f32).collect();
    let mut si: Vec<usize> = (0..stats.len()).collect();
    si.sort_by(|&a, &b| stats[b].score.cmp(&stats[a].score));
    let best_stat_pos = si[0];
    let best_frame_idx = stats[best_stat_pos].idx;
    let ma = stats[best_stat_pos].score as f32;
    let mi = stats[si[stats.len() - 1]].score as f32;
    if ma > 0.0 {
        for v in &mut qg {
            *v = (*v / ma) * 100.0;
        }
    }
    let aq = if qg.is_empty() {
        0.0
    } else {
        qg.iter().sum::<f32>() / qg.len() as f32
    };
    let cached = CachedAnalysis {
        scores: vec![],
        roi: Rect {
            x: rx,
            y: ry,
            w: rw,
            h: rh,
        },
        path_hash: source_fingerprint,
        frame_stats: Some(stats.clone()),
        quality_graph: Some(qg.clone()),
        width: Some(tw),
        height: Some(th),
        best_frame_idx: Some(best_frame_idx),
        ap_points: None,
        contract: Some(cache_expectation.contract(Rect {
            x: rx,
            y: ry,
            w: rw,
            h: rh,
        })),
    };
    // bincode+LZ4 (el JSON con grid_scores 40×40/frame superaba los 100 MB
    // en videos largos y se re-parseaba en CADA apilado).
    publish_analysis_cache_best_effort(
        app,
        state,
        &job_token,
        &c_path,
        &cached,
        &cache_expectation,
    )?;
    emit_progress(app, "Finalizando...", 100.0, None);
    let mut buf = Vec::new();
    let raw = {
        let _s = crate::perf_trace::span(pt, "preview_decode");
        // PR-10: caché de decode primero — evita re-decodificar 0..best_idx
        // cuando el análisis sembró el caché (o una pasada anterior lo dejó).
        let cached = if let VideoInput::Ffmpeg(ref fr) = r {
            read_selected_from_decode_cache(
                path,
                &fr.ffmpeg_path,
                &fr.codec_name,
                fr.rotation,
                tw,
                th,
                tbp,
                cid,
                &[best_frame_idx],
            )
        } else {
            None
        };
        match cached {
            Some(mut frames) => frames.pop().unwrap_or_default(),
            None => {
                // PR-22: la preview es cosmética — un frame vecino por seek
                // rápido evita re-decodificar 0..best (122 s medidos a 20MP).
                let fast = if std::env::var("ZAS_EXACT_ANCHOR").ok().as_deref() != Some("1") {
                    if let VideoInput::Ffmpeg(ref fr) = r {
                        ffmpeg_fast_seek_frame(fr, path, tw, th, cid, best_frame_idx)
                    } else {
                        None
                    }
                } else {
                    None
                };
                match fast {
                    Some(raw) => raw,
                    None => read_exact_source_frame(&unified_source, best_frame_idx)?,
                }
            }
        }
    };
    let u16_raw = raw_to_u16_buffer(&raw, tw, th, tbp);
    drop(raw);
    if let Err(error) =
        cache_smart_grid_frame(path, best_frame_idx, tw, th, cid, &u16_raw)
    {
        log_to_front(
            app,
            "WARN",
            &format!("No se pudo preparar la caché RAM de Smart AP: {error}"),
        );
    }

    if is_color {
        let mut u16 = debayer_to_rgb(&u16_raw, tw, th, cid);
        auto_color_balance(&mut u16, tw, th);
        image::DynamicImage::ImageRgb8(
            image::RgbImage::from_raw(tw as u32, th as u32, to_8bit_preview_visual(&u16)).unwrap(),
        )
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    } else {
        let stretched = auto_contrast_stretch_u16(&u16_raw, tw, th);
        let vis = to_8bit_visual(&stretched, 1.0);
        let mut rgba = Vec::with_capacity(tw * th * 4);
        for p in vis {
            rgba.extend_from_slice(&[p, p, p, 255]);
        }
        image::DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(tw as u32, th as u32, rgba).unwrap(),
        )
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    }
    if job_token.is_cancelled() {
        return Err("Cancelado o sustituido por otro análisis".into());
    }
    emit_pipeline_telemetry(
        app,
        PipelineTelemetry {
            job_id: pipeline_job_id,
            domain: PipelineDomain::Planetary,
            phase: "complete".into(),
            engine: if gpu_analysis_enabled && !gpu_analysis_failed.load(Ordering::Relaxed) {
                "Hybrid GPU por lotes + CPU refinamiento".into()
            } else {
                format!("CPU {}", simd_backend_label())
            },
            progress: 100.0,
            eta_seconds: Some(0.0),
            items_done: stats.len(),
            // tf era estimado en vídeos comprimidos; al terminar, el conteo
            // real de frames analizados es la única verdad.
            items_total: stats.len().max(1),
            throughput: Some(stats.len() as f32 / pipeline_started.elapsed().as_secs_f32().max(0.001)),
            cpu_percent: None,
            gpu_percent: None,
            ram_mb: 0,
            vram_mb: 0,
            io_read_mb: 0.0,
            io_write_mb: 0.0,
            cache_hits: 0,
            cache_misses: stats.len(),
            fallback_reason: gpu_analysis_failed
                .load(Ordering::Relaxed)
                .then_some("Fallback CPU tras fallo de preprocesado GPU".into()),
        },
    );
    let _ = pt_guard.finish_ok();
    Ok(AnalysisResult {
        metadata: VideoMetadata {
            width: tw,
            height: th,
            frame_count: tf,
            bpp: tbp * 8,
            color_id: cid,
            file_size_mb: fs::metadata(path)
                .map(|m| m.len() as f64 / 1_048_576.0)
                .unwrap_or(0.0),
            pattern_name: ser::ser_pattern_name(cid).to_string(),
            is_color,
        },
        stats: VideoStats {
            min_pixel: 0,
            max_pixel: 65535,
            avg_brightness: 0.0,
            dynamic_range_pct: 100.0,
            std_dev: 0.0,
            entropy: 0.0,
            avg_quality: aq as f64,
            quality_stability: if ma > 0.0 {
                (mi / ma * 100.0) as f64
            } else {
                0.0
            },
            best_score: 100.0,
            worst_score: if ma > 0.0 {
                (mi / ma * 100.0) as f64
            } else {
                0.0
            },
        },
        recommended_pct: compute_smart_stack_pct(&qg, &target_type, is_surface),
        quality_graph: qg.iter().enumerate().map(|(i, &v)| (i, v as f64)).collect(),
        // PR-2.5: preview por archivo temporal + asset protocol (patrón del
        // apilado). El data-URL base64 a resolución completa duplicaba el
        // pico de RAM del WebView y cruzaba el IPC como string de varios MB
        // (WebView2 es frágil con strings >50 MB). Fallback a base64 solo si
        // el temp no es escribible; setImageAndWait/toDisplaySrc ya
        // normalizan rutas planas con convertFileSrc.
        preview_base64: save_analysis_preview_cache(&buf, &c_path, source_fingerprint)
            .or_else(|| save_preview_png_to_temp(&buf, "analysis"))
            .unwrap_or_else(|| {
                format!(
                    "data:image/png;base64,{}",
                    general_purpose::STANDARD.encode(&buf)
                )
            }),
        best_frame_idx,
        path: path.to_string(),
        ap_points: vec![],
    })
}

#[tauri::command]
async fn analyze_video_v2(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    is_surface: bool,
    target_type: String, // NEW
    warping_analysis: bool, // NEW
    bayer_override: Option<i32>,
    anchor_override: Option<Vec<i32>>,
    progress_prefix: Option<String>,
) -> Result<AnalysisResult, String> {
    let request_id = begin_planetary_user_job(&state);
    // PR-2.5: el análisis corre en el pool blocking (no ocupa un worker del
    // runtime async — antes otros comandos se encolaban minutos).
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        perform_standardized_analysis(
            &app,
            &state,
            request_id,
            &path,
            is_surface,
            target_type, // NEW
            warping_analysis,
            bayer_override,
            anchor_override,
            progress_prefix,
            ComputePolicy::Hybrid,
        )
    })
    .await
    .map_err(|e| format!("El hilo de análisis terminó inesperadamente: {e}"))?
}

/// Contrato tipado del análisis planetario. El wrapper legado permanece para
/// compatibilidad durante una versión.
#[tauri::command]
async fn analyze_planetary(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    request: PlanetaryAnalysisRequest,
) -> Result<AnalysisResult, String> {
    let request = request.resolved_profile();
    let request_id = begin_planetary_user_job(&state);
    // PR-2.5: pool blocking (ver analyze_video_v2).
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        perform_standardized_analysis(
            &app,
            &state,
            request_id,
            &request.path,
            request.is_surface,
            request.target_type,
            request.warping_analysis,
            request.bayer_override,
            request.anchor_override,
            request.progress_prefix,
            request.compute_policy,
        )
    })
    .await
    .map_err(|e| format!("El hilo de análisis terminó inesperadamente: {e}"))?
}

#[tauri::command]
fn stop_analysis(state: State<'_, AppState>) {
    cancel_planetary_jobs(&state);
}

#[tauri::command]
fn activate_license(state: State<'_, AppState>, key: String) -> String {
    // Determine device name
    let device_name = match std::env::var("COMPUTERNAME") {
        Ok(name) => name,
        Err(_) => "ZenithPC".to_string(),
    };
    match state.license_manager.activate_license(&key, &device_name) {
        Ok(_) => "Licencia activada correctamente. Reinicia la aplicacion.".to_string(),
        Err(e) => format!("Error al activar: {}", e),
    }
}

#[tauri::command]
fn ver_licencia(state: State<'_, AppState>) -> AppStatus {
    state.license_manager.get_status()
}

// WRAPPER: PARALLEL LOAD
/// Persistent-stream FFmpeg batch feeder: ONE sequential decode pass serves ALL
/// RAM batches of a stacking pass. The previous per-batch loader opened a new
/// ffmpeg process per batch and decoded from frame 0 every time — with N
/// batches that is O(N²) redundant decoding (a 28-batch stack decoded ~14× the
/// video). Batches arrive in ascending order (indices are sorted), so a single
/// stream can feed them all; si la selección completa cabe, LZ4 elimina el
/// decode de la segunda pasada.
#[allow(clippy::too_many_arguments)]
// ===========================================================================
// CACHE DE DECODE POR-FRAME PERSISTENTE (videos comprimidos / FFmpeg).
//
// El cache anterior era POR-LOTE con clave = hash de la seleccion exacta de
// indices, y el directorio entero se borraba al final de cada apilado: cambiar
// el % (o cualquier ajuste) obligaba a re-decodificar TODO el video. Ahora
// cada frame decodificado se guarda individualmente con clave versionada por
// contenido + geometría + codec + CFA/color + rotación — re-apilar el mismo video con la misma o
// menor seleccion, o tras cambiar ajustes que no tocan la seleccion (drizzle,
// malla AP, sharpening), sirve todos los frames desde disco SIN decodificar.
// Presupuesto LRU adaptado a RAM/disco (con override ZAS_DECODE_CACHE_GB).
// Para vídeo inter-frame la admisión es todo-o-nada: un conjunto parcial no
// evita atravesar H.264/HEVC desde el inicio y sólo añade LZ4/E/S. Si toda la
// selección cabe, la segunda pasada se sirve íntegra desde NVMe; si no, cada
// pasada usa un único stream secuencial sin trabajo de caché inútil.
// ===========================================================================
// PR-14 (decisión del usuario 2026-07-16): presupuesto auto por DISCO libre
// (40% del disponible, suelo 12GB, techo 256GB) en vez del antiguo RAM/8
// clamp 3..12GB que jamás admitía una selección 20MP color (~60-120GB).
const DECODE_CACHE_MIN_AUTO_BYTES: u64 = 12 * 1024 * 1024 * 1024;
const DECODE_CACHE_MAX_AUTO_BYTES: u64 = 256 * 1024 * 1024 * 1024;
const DECODE_CACHE_ALGORITHM_VERSION: &str = "planetary-ffmpeg-decode-v6-route-crc32";
const DECODE_CACHE_PAYLOAD_MAGIC: &[u8; 8] = b"ZDCFv5\0\0";
static DECODE_CACHE_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static DECODE_CACHE_PRUNE_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();
static DECODE_CACHE_RESERVATION_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();
static DECODE_CACHE_USAGE_LEDGERS: std::sync::OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<
            PathBuf,
            std::sync::Weak<std::sync::atomic::AtomicU64>,
        >,
    >,
> = std::sync::OnceLock::new();

/// Publicación transaccional de frames FFmpeg.
///
/// Un decoder hardware puede entregar frames completos y fallar al cerrar o
/// varias decenas de frames después. Esos bytes no entran en el caché estable
/// hasta que TODO el intento ha sido validado. Si hay cancelación, panic, EOF
/// prematuro o fallback, `Drop` elimina el staging y el siguiente intento parte
/// de los únicos hits que ya estaban validados antes de comenzar.
struct DecodeFrameCacheTransaction {
    cache_dir: PathBuf,
    staging_dir: PathBuf,
    key: u64,
    expected_len: usize,
    budget_bytes: u64,
    used_bytes: Arc<std::sync::atomic::AtomicU64>,
    staged_bytes: std::sync::atomic::AtomicU64,
    expected_indices: Option<std::collections::HashSet<usize>>,
    staged_indices: std::sync::Mutex<std::collections::HashSet<usize>>,
    committed: bool,
}

impl DecodeFrameCacheTransaction {
    fn new(
        cache_dir: &Path,
        key: u64,
        expected_len: usize,
        budget_bytes: u64,
    ) -> Option<Self> {
        if budget_bytes == 0 || expected_len == 0 {
            return None;
        }
        let sequence = DECODE_CACHE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let staging_dir = cache_dir.join(format!(
            ".decode-attempt-{}-{sequence}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir(&staging_dir).ok()?;
        let used_bytes = decode_cache_usage_ledger(cache_dir);
        Some(Self {
            cache_dir: cache_dir.to_path_buf(),
            staging_dir,
            key,
            expected_len,
            budget_bytes,
            used_bytes,
            staged_bytes: std::sync::atomic::AtomicU64::new(0),
            expected_indices: None,
            staged_indices: std::sync::Mutex::new(std::collections::HashSet::new()),
            committed: false,
        })
    }

    fn with_expected_indices<I>(mut self, indices: I) -> Self
    where
        I: IntoIterator<Item = usize>,
    {
        self.expected_indices = Some(indices.into_iter().collect());
        self
    }

    fn write_frame(&self, idx: usize, frame: &[u16]) {
        if frame.len() != self.expected_len {
            return;
        }
        // PR-14: con admisión por sufijo solo se cachean los índices del
        // conjunto esperado — comprimir un prefijo que jamás se publicará
        // sería exactamente la E/S inútil que la admisión intenta evitar.
        if let Some(expected) = self.expected_indices.as_ref() {
            if !expected.contains(&idx) {
                return;
            }
        }
        let final_path = decode_cache_frame_path(&self.cache_dir, self.key, idx);
        if read_cached_frame(&final_path, self.expected_len).is_some() {
            return;
        }
        let staged_path = decode_cache_frame_path(&self.staging_dir, self.key, idx);
        let written = write_cached_frame_budgeted(
            &staged_path,
            frame,
            self.used_bytes.as_ref(),
            self.budget_bytes,
        );
        if written > 0 {
            self.staged_bytes.fetch_add(written, Ordering::AcqRel);
            self.staged_indices
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(idx);
        }
    }

    fn write_raw_le_u16(&self, idx: usize, raw: &[u8]) {
        if raw.len() != self.expected_len.saturating_mul(2) {
            return;
        }
        let mut frame = Vec::with_capacity(self.expected_len);
        frame.extend(
            raw.chunks_exact(2)
                .map(|sample| u16::from_le_bytes([sample[0], sample[1]])),
        );
        self.write_frame(idx, &frame);
    }

    /// Commit best-effort: un fallo de caché nunca invalida el resultado ya
    /// calculado. El rename ocurre en el mismo volumen/directorio padre. Un
    /// destino válido ganado por otro job prevalece; uno corrupto se sustituye.
    fn commit(&mut self) {
        // El contrato del vídeo inter-frame es realmente todo-o-nada. Si una
        // reserva, escritura o ENOSPC dejó aunque sea un índice ausente, no se
        // publica ningún staging: un subconjunto ocupa GiB pero no evita volver
        // a recorrer los mismos GOP en la siguiente pasada.
        if let Some(expected) = self.expected_indices.as_ref() {
            let staged = self
                .staged_indices
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let complete = expected.iter().all(|&idx| {
                staged.contains(&idx)
                    || read_cached_frame(
                        &decode_cache_frame_path(&self.cache_dir, self.key, idx),
                        self.expected_len,
                    )
                    .is_some()
            });
            if !complete {
                return;
            }
        }
        let entries = match std::fs::read_dir(&self.staging_dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let staged = entry.path();
            if !is_decode_cache_entry(&staged) {
                continue;
            }
            let Some(name) = staged.file_name() else {
                continue;
            };
            let final_path = self.cache_dir.join(name);
            if read_cached_frame(&final_path, self.expected_len).is_some() {
                let _ = std::fs::remove_file(&staged);
                continue;
            }
            if final_path.exists() {
                // El destino ya es inválido; retirarlo permite el rename en
                // Windows. No se pierde un hit utilizable.
                let _ = std::fs::remove_file(&final_path);
            }
            if std::fs::rename(&staged, &final_path).is_err() {
                // Carrera: si otro job publicó un frame válido, conservarlo.
                if read_cached_frame(&final_path, self.expected_len).is_none() {
                    let _ = std::fs::remove_file(&staged);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.staging_dir);
        self.committed = true;
        self.staged_bytes.store(0, Ordering::Release);
        prune_decode_cache_to_budget(&self.cache_dir, self.budget_bytes);
    }
}

impl Drop for DecodeFrameCacheTransaction {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _reservation_guard = DECODE_CACHE_RESERVATION_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = std::fs::remove_dir_all(&self.staging_dir);
        // Reconciliar bajo el mismo mutex que las reservas cubre tanto el
        // rollback normal como entradas duplicadas retiradas durante commit.
        self.used_bytes
            .store(decode_cache_total_bytes(&self.cache_dir), Ordering::Release);
    }
}

fn is_decode_cache_entry(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    path.extension().and_then(|ext| ext.to_str()) == Some("lz4")
        && (name.starts_with("f_") || name.starts_with("batch_"))
}

fn decode_cache_tree_bytes(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if metadata.file_type().is_symlink() {
        return 0;
    }
    if metadata.is_file() {
        return metadata.len();
    }
    if !metadata.is_dir() {
        return 0;
    }
    std::fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| decode_cache_tree_bytes(&entry.path()))
        .sum()
}

fn is_decode_staging_dir(path: &Path) -> bool {
    path.is_dir()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".decode-attempt-"))
}

/// Un crash no ejecuta Drop. Detectar el PID propietario permite retirar en
/// la siguiente sesión los staging huérfanos (que pueden medir varios GiB) sin
/// tocar transacciones de otra instancia todavía viva.
fn cleanup_orphan_decode_staging(dir: &Path) {
    let system = System::new_all();
    let now = std::time::SystemTime::now();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_decode_staging_dir(&path) {
            continue;
        }
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
        let owner = name
            .strip_prefix(".decode-attempt-")
            .and_then(|tail| tail.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok());
        let owner_alive = owner
            .and_then(|pid| system.process(sysinfo::Pid::from_u32(pid)))
            .is_some();
        let age = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .unwrap_or_default();
        let malformed_and_old = owner.is_none() && age >= std::time::Duration::from_secs(3600);
        if (!owner_alive && owner.is_some()) || malformed_and_old {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn decode_cache_total_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            let path = entry.path();
            is_decode_cache_entry(&path) || is_decode_staging_dir(&path)
        })
        .map(|entry| decode_cache_tree_bytes(&entry.path()))
        .sum()
}

fn decode_cache_usage_ledger(dir: &Path) -> Arc<std::sync::atomic::AtomicU64> {
    let key = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut ledgers = DECODE_CACHE_USAGE_LEDGERS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ledgers.retain(|_, ledger| ledger.strong_count() > 0);
    if let Some(ledger) = ledgers.get(&key).and_then(std::sync::Weak::upgrade) {
        return ledger;
    }
    let ledger = Arc::new(std::sync::atomic::AtomicU64::new(
        decode_cache_total_bytes(dir),
    ));
    ledgers.insert(key, Arc::downgrade(&ledger));
    ledger
}

/// Presupuesto por máquina, no una constante de portátil. Por defecto usa
/// 1/8 de la RAM (3–12 GiB) y nunca invade la reserva segura del volumen. El
/// override permite a estaciones NVMe grandes dedicar hasta 64 GiB.
/// PR-14: directorio del caché de decode. Override `ZAS_DECODE_CACHE_DIR`
/// para llevarlo a otro volumen (p. ej. un SSD externo cuando el interno va
/// lleno: con <12GB libres la salvaguarda desactiva el caché y las pasadas
/// vuelven a re-decodificar).
fn decode_cache_root_dir() -> std::path::PathBuf {
    match std::env::var_os("ZAS_DECODE_CACHE_DIR") {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::env::temp_dir().join("astro_stacker_cache"),
    }
}

/// PR-14: decide QUÉ índices de la selección se cachean. Completa si cabe en
/// presupuesto; si no, el SUFIJO que cabe (la pasada siguiente decodifica el
/// prefijo y el decoder se corta ahí). Un sufijo <5% de la selección no
/// compensa la E/S de sembrado.
fn decode_cache_expected_set(
    exact_selected: &[usize],
    max_cache_file: u64,
    cache_budget: u64,
) -> Vec<usize> {
    if exact_selected.is_empty() || max_cache_file == 0 {
        return Vec::new();
    }
    let selection_worst_case = max_cache_file.saturating_mul(exact_selected.len() as u64);
    if selection_worst_case <= cache_budget {
        return exact_selected.to_vec();
    }
    let suffix_capacity = (cache_budget / max_cache_file) as usize;
    if suffix_capacity.saturating_mul(20) >= exact_selected.len() && suffix_capacity > 0 {
        exact_selected[exact_selected.len() - suffix_capacity..].to_vec()
    } else {
        Vec::new()
    }
}

/// PR-10: sirve índices exactos desde el caché de decode por-frame (sembrado
/// por el análisis o por una pasada anterior) SIN tocar el códec. La clave
/// incluye la ruta de decode con la que se sembró; se prueban la CPU y los
/// backends HW plausibles de la plataforma (stats fallidos ≈ gratis).
/// Todo-o-nada: cualquier índice ausente/corrupto devuelve None y el caller
/// decodifica como siempre. Frames en bytes LE (contrato de read_batch).
fn read_selected_from_decode_cache(
    path: &str,
    ffmpeg_path: &str,
    codec_name: &str,
    rotation: i32,
    width: usize,
    height: usize,
    bpp: usize,
    color_id: i32,
    indices: &[usize],
) -> Option<Vec<Vec<u8>>> {
    if indices.is_empty() {
        return None;
    }
    let cache_dir = decode_cache_root_dir();
    let is_color_stream = ffmpeg_stream_is_color(color_id);
    let cached_frame_len = width * height * (if is_color_stream { 3 } else { 1 });
    let mut routes: Vec<String> = vec![ffmpeg_decode_route_label(ffmpeg_path, None)];
    let hw_candidates: &[&str] = if cfg!(target_os = "macos") {
        &["videotoolbox"]
    } else if cfg!(target_os = "windows") {
        &["d3d11va", "dxva2", "cuda", "qsv"]
    } else {
        &["vaapi", "cuda"]
    };
    for backend in hw_candidates {
        routes.push(ffmpeg_decode_route_label(ffmpeg_path, Some(backend)));
    }
    for route in routes {
        let key = ffmpeg_decode_cache_key(
            path, width, height, bpp, color_id, rotation, codec_name, &route,
        );
        // Sonda barata antes de leer todo: el primer índice debe existir.
        let first = decode_cache_frame_path(&cache_dir, key, indices[0]);
        if !first.exists() {
            continue;
        }
        let frames: Option<Vec<Vec<u8>>> = indices
            .par_iter()
            .map(|&idx| {
                read_cached_frame(
                    &decode_cache_frame_path(&cache_dir, key, idx),
                    cached_frame_len,
                )
                .map(|frame| {
                    let mut raw = Vec::with_capacity(frame.len() * 2);
                    for value in frame {
                        raw.extend_from_slice(&value.to_le_bytes());
                    }
                    raw
                })
            })
            .collect();
        if let Some(frames) = frames {
            return Some(frames);
        }
    }
    None
}

fn decode_cache_budget_bytes(dir: &Path) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    let override_bytes = std::env::var("ZAS_DECODE_CACHE_GB")
        .ok()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .map(|gb| (gb.clamp(0.0, 512.0) * GIB as f64) as u64);
    if override_bytes == Some(0) {
        return 0;
    }

    let target = std::fs::canonicalize(dir)
        .or_else(|_| std::fs::canonicalize(dir.parent().unwrap_or(dir)))
        .unwrap_or_else(|_| dir.to_path_buf());
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let disk = disks
        .list()
        .iter()
        .filter(|disk| target.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len());
    let current = decode_cache_total_bytes(dir);

    let configured = override_bytes.unwrap_or_else(|| {
        // PR-14: auto según DISCO libre — 40% del disponible (contando lo ya
        // cacheado, que es reutilizable), clamp [12GB, 256GB]. El techo
        // antiguo de 12GB por RAM hacía imposible servir la 2ª pasada de un
        // 20MP color desde NVMe.
        match disk {
            Some(d) => (current.saturating_add(d.available_space()) * 2 / 5)
                .clamp(DECODE_CACHE_MIN_AUTO_BYTES, DECODE_CACHE_MAX_AUTO_BYTES),
            None => DECODE_CACHE_MIN_AUTO_BYTES,
        }
    });
    if configured == 0 {
        return 0;
    }
    let Some(disk) = disk else {
        return configured;
    };
    let reserve = (disk.total_space() / 20).max(2 * GIB); // 5% o 2 GiB
    let safe_total = current
        .saturating_add(disk.available_space())
        .saturating_sub(reserve);
    configured.min(safe_total)
}

fn ffmpeg_decode_cache_key(
    path: &str,
    width: usize,
    height: usize,
    bpp: usize,
    color_id: i32,
    rotation: i32,
    codec: &str,
    decode_route: &str,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    DECODE_CACHE_ALGORITHM_VERSION.hash(&mut hasher);
    planetary_source_fingerprint(path)
        .unwrap_or_else(|_| {
            // El caller ya abrió el origen; este fallback sólo evita que un
            // fallo de muestreo convierta la caché en un error del apilado.
            let mut fallback = std::collections::hash_map::DefaultHasher::new();
            path.hash(&mut fallback);
            fallback.finish()
        })
        .hash(&mut hasher);
    width.hash(&mut hasher);
    height.hash(&mut hasher);
    bpp.hash(&mut hasher);
    color_id.hash(&mut hasher);
    rotation.rem_euclid(360).hash(&mut hasher);
    codec.to_ascii_lowercase().hash(&mut hasher);
    // CPU y hardware son contratos numéricos distintos. En particular, un
    // retry CPU nunca debe completar misses usando hits producidos por un
    // intento VideoToolbox/NVDEC/D3D11VA anterior.
    decode_route.to_ascii_lowercase().hash(&mut hasher);
    hasher.finish()
}

fn decode_cache_frame_path(dir: &Path, path_hash: u64, idx: usize) -> PathBuf {
    dir.join(format!("f_{:016x}_{:06}.lz4", path_hash, idx))
}

fn decode_cache_max_file_bytes(expected_len: usize) -> u64 {
    // bincode Vec<u16> = longitud u64 + muestras; payload añade magic+CRC.
    // LZ4 block worst-case es aproximadamente n+n/255+16, más su prefijo.
    let payload = DECODE_CACHE_PAYLOAD_MAGIC
        .len()
        .saturating_add(4)
        .saturating_add(8)
        .saturating_add(expected_len.saturating_mul(2));
    payload
        .saturating_add(payload / 255)
        .saturating_add(64)
        .min(u64::MAX as usize) as u64
}

/// Lee un frame cacheado. `expected_len` (en u16) valida el archivo: un frame
/// de otra resolucion/formato o corrupto devuelve None → se decodifica normal.
fn read_cached_frame(p: &Path, expected_len: usize) -> Option<Vec<u16>> {
    let metadata = std::fs::metadata(p).ok()?;
    if !metadata.is_file() || metadata.len() > decode_cache_max_file_bytes(expected_len) {
        return None;
    }
    let raw = std::fs::read(p).ok()?;
    // Guard del prefijo de tamano LZ4 (mismo peligro que parse_cached_analysis):
    // un archivo ajeno/corrupto podria declarar GB — validar antes de asignar.
    if raw.len() < 4 {
        return None;
    }
    let declared = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
    if declared > expected_len.saturating_mul(2).saturating_add(1088) {
        return None;
    }
    let payload = lz4_flex::decompress_size_prepended(&raw).ok()?;
    if payload.len() < DECODE_CACHE_PAYLOAD_MAGIC.len() + 4
        || &payload[..DECODE_CACHE_PAYLOAD_MAGIC.len()] != DECODE_CACHE_PAYLOAD_MAGIC
    {
        return None;
    }
    let checksum_start = DECODE_CACHE_PAYLOAD_MAGIC.len();
    let bin_start = checksum_start + 4;
    let expected_crc = u32::from_le_bytes(
        payload[checksum_start..bin_start]
            .try_into()
            .ok()?,
    );
    if crc32fast::hash(&payload[bin_start..]) != expected_crc {
        return None;
    }
    let v: Vec<u16> = bincode::deserialize(&payload[bin_start..]).ok()?;
    if v.len() != expected_len {
        return None;
    }
    // LRU real: una lectura válida renueva la edad, pero no hace una escritura
    // de metadatos por cada pasada si el hit ya se tocó hace menos de un minuto.
    if std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age >= std::time::Duration::from_secs(60))
    {
        if let Ok(file) = std::fs::OpenOptions::new().write(true).open(p) {
            let times = std::fs::FileTimes::new().set_modified(std::time::SystemTime::now());
            let _ = file.set_times(times);
        }
    }
    Some(v)
}

fn encode_cached_frame(frame: &[u16]) -> Option<Vec<u8>> {
    let bin = bincode::serialize(&frame).ok()?;
    let checksum = crc32fast::hash(&bin);
    let mut payload = Vec::with_capacity(DECODE_CACHE_PAYLOAD_MAGIC.len() + 4 + bin.len());
    payload.extend_from_slice(DECODE_CACHE_PAYLOAD_MAGIC);
    payload.extend_from_slice(&checksum.to_le_bytes());
    payload.extend_from_slice(&bin);
    Some(lz4_flex::compress_prepend_size(&payload))
}

fn write_cache_bytes_atomically(p: &Path, compressed: &[u8]) -> bool {
    // Dos análisis solapados del mismo origen pueden intentar publicar el
    // mismo frame. Un temporal basado sólo en PID hacía que ambos writers
    // compartieran archivo y uno pudiera renombrar bytes aún incompletos del
    // otro. La secuencia por proceso conserva el rename atómico sin colisión.
    let sequence = DECODE_CACHE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp = p.with_extension(format!(
        "lz4.tmp-{}-{sequence}",
        std::process::id()
    ));
    if std::fs::write(&tmp, compressed).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    if p.exists() {
        let _ = std::fs::remove_file(&tmp);
        return true;
    }
    match std::fs::rename(&tmp, p) {
        Ok(()) => true,
        Err(_) if p.exists() => {
            let _ = std::fs::remove_file(&tmp);
            true
        }
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            false
        }
    }
}

#[cfg(test)]
fn write_cached_frame(p: &Path, frame: &[u16]) {
    if p.exists() {
        return;
    }
    if let Some(compressed) = encode_cached_frame(frame) {
        // Escritura atómica: un cierre/ENOSPC nunca deja un .lz4 parcial que
        // parezca un hit válido en la siguiente sesión.
        let _ = write_cache_bytes_atomically(p, &compressed);
    }
}

fn write_cached_frame_budgeted(
    p: &Path,
    frame: &[u16],
    used_bytes: &std::sync::atomic::AtomicU64,
    budget_bytes: u64,
) -> u64 {
    if budget_bytes == 0 || p.exists() {
        return 0;
    }
    // La admisión all-or-nothing del caller ya descarta selecciones cuyo peor
    // caso no cabe, antes de llegar aquí. Dentro de una transacción usamos el
    // tamaño comprimido real: exigir de nuevo el peor caso por frame dejaría
    // capacidad válida sin usar y rompería presupuestos pequeños pero exactos.
    // La reserva atómica posterior sigue siendo la autoridad entre writers.
    let Some(compressed) = encode_cached_frame(frame) else {
        return 0;
    };
    let len = compressed.len() as u64;
    let _reservation_guard = DECODE_CACHE_RESERVATION_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let reserved = used_bytes
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(len).filter(|&next| next <= budget_bytes)
        })
        .is_ok();
    if !reserved {
        return 0;
    }
    if !write_cache_bytes_atomically(p, &compressed) {
        used_bytes.fetch_sub(len, Ordering::AcqRel);
        0
    } else {
        len
    }
}

/// PR-2.1 (decode único): escribe al caché por-frame un frame CRUDO del
/// stream FFmpeg (gray16le o rgb48le) tal y como lo decodificó el ANÁLISIS,
/// en el MISMO formato que usa el apilado (Vec<u16>, misma clave): el primer
/// apilado del mismo vídeo se sirve del caché sin re-decodificar. Idempotente
/// (si el archivo ya existe no se reescribe) y silencioso ante fallos de E/S.
#[cfg(test)]
fn cache_decoded_raw_frame(dir: &Path, key: u64, idx: usize, raw: &[u8]) {
    let p = decode_cache_frame_path(dir, key, idx);
    if p.exists() {
        return;
    }
    let mut v = Vec::with_capacity(raw.len() / 2);
    for ch in raw.chunks_exact(2) {
        v.push(u16::from_le_bytes([ch[0], ch[1]]));
    }
    write_cached_frame(&p, &v);
}

/// Poda LRU por mtime hasta quedar bajo `max_bytes`. Tambien recoge los
/// archivos del formato por-lote antiguo (batch_*.bin.lz4) con el tiempo.
fn prune_decode_cache_to_budget(dir: &Path, max_bytes: u64) {
    let _prune_guard = DECODE_CACHE_PRUNE_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let usage = decode_cache_usage_ledger(dir);
    let _reservation_guard = DECODE_CACHE_RESERVATION_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cleanup_orphan_decode_staging(dir);
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            if !is_decode_cache_entry(&e.path()) {
                return None;
            }
            let m = e.metadata().ok()?;
            if !m.is_file() {
                return None;
            }
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    // Los staging de procesos vivos cuentan contra el presupuesto aunque no
    // sean candidatos de poda. Así nunca quedan invisibles al cálculo global.
    let mut total = decode_cache_total_bytes(dir);
    if total <= max_bytes {
        usage.store(total, Ordering::Release);
        return;
    }
    files.sort_by_key(|f| f.0); // mas viejos primero
    for (_, len, p) in files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(&p).is_ok() {
            total -= len;
        }
    }
    usage.store(decode_cache_total_bytes(dir), Ordering::Release);
}

fn stream_frames_ffmpeg_chunked(
    reader: &FfmpegReader,
    path: &str,
    app: &tauri::AppHandle,
    chunks: &[Vec<usize>],
    tx: &std::sync::mpsc::SyncSender<
        Result<std::collections::HashMap<usize, Vec<u16>>, String>,
    >,
    width: usize,
    height: usize,
    bpp: usize,
    color_id: i32,
    pass_label: &str,
    total_batches: usize,
    cancel: &PlanetaryJobToken,
    cache_hits: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
    force_cpu_decode: bool,
    decode_route_hardware: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), String> {
    let cache_dir = decode_cache_root_dir();
    if !cache_dir.exists() {
        let _ = std::fs::create_dir_all(&cache_dir);
    }
    let cache_budget = decode_cache_budget_bytes(&cache_dir);
    let is_color_stream = ffmpeg_stream_is_color(color_id);
    let stream_bpp = if is_color_stream { 6usize } else { 2usize };
    let mut raw_buf = vec![0u8; width * height * stream_bpp];

    // Frames almacenados como Vec<u16>: w*h (mono) o w*h*3 (color).
    let cached_frame_len = width * height * (stream_bpp / 2);
    let mut it: Option<FfmpegStreamIterator> = None;
    let mut pos: usize = 0; // next frame index the stream will yield
    // Reutiliza el mismo micro-benchmark que el análisis: el apilado no vuelve
    // a asumir que `-hwaccel auto` significa GPU ni que siempre será más rápido.
    // La clave incluye path/tamaño/mtime/codec, por lo que normalmente es un
    // lookup en caché y análisis/apilado conservan exactamente la misma ruta.
    // Si el análisis se saltó (cache), el probe corre AQUI por primera vez:
    // avisar para que la espera no parezca una barra muerta.
    emit_progress(
        app,
        &format!("{}Midiendo decodificación GPU vs CPU...", pass_label),
        1.0,
        None,
    );
    let decode_probe = benchmark_ffmpeg_decode_route(
        &reader.ffmpeg_path,
        path,
        &reader.codec_name,
        color_id,
        width,
        height,
        reader.rotation,
        Some(cancel),
    );
    let use_gpu = !force_cpu_decode && decode_probe.prefer_hardware;
    let decode_route = ffmpeg_decode_route_label(
        &reader.ffmpeg_path,
        use_gpu.then_some(decode_probe.backend.as_deref().unwrap_or("unconfirmed")),
    );
    let path_hash = ffmpeg_decode_cache_key(
        path,
        width,
        height,
        bpp,
        color_id,
        reader.rotation,
        &reader.codec_name,
        &decode_route,
    );
    // Un stream H.264/HEVC secuencial sólo evita el decode cuando TODA la
    // selección está cacheada. Un caché parcial obliga a atravesar igualmente
    // el video y antes además comprimía LZ4 cada frame restante aunque jamás
    // pudiera caber (112 GiB RGB16 para la captura 20 MP reportada). Admisión
    // all-or-nothing: leemos un conjunto completo o decodificamos sin E/S inútil.
    let requested_indices: std::collections::HashSet<usize> =
        chunks.iter().flatten().copied().collect();
    let mut exact_selected_indices: Vec<usize> = requested_indices.iter().copied().collect();
    exact_selected_indices.sort_unstable();
    let decoded_span = exact_selected_indices
        .last()
        .copied()
        .map(|index| index.saturating_add(1))
        .unwrap_or(0);
    // `select` sigue decodificando los GOP para conservar exactitud, pero los
    // frames descartados ya no pasan por scale/format RGB48 ni por stdout. Se
    // activa cuando ahorra al menos 5 % del pipe y la expresión cabe también
    // en el límite estricto de CreateProcess de Windows.
    let use_exact_selected_pipe = !exact_selected_indices.is_empty()
        && exact_selected_indices.len().saturating_mul(20)
            <= decoded_span.saturating_mul(19)
        && ffmpeg_exact_frame_select_filter(&exact_selected_indices).is_ok();
    let max_cache_file = decode_cache_max_file_bytes(cached_frame_len);
    let cache_file_is_plausible = |idx: usize| {
        std::fs::metadata(decode_cache_frame_path(&cache_dir, path_hash, idx))
            .ok()
            .is_some_and(|metadata| {
                metadata.is_file() && metadata.len() > 0 && metadata.len() <= max_cache_file
            })
    };
    let plausible_cached = requested_indices
        .iter()
        .filter(|&&idx| cache_file_is_plausible(idx))
        .count();
    let mut complete_cache_candidate =
        !requested_indices.is_empty() && plausible_cached == requested_indices.len();
    let missing_frames = requested_indices.len().saturating_sub(plausible_cached);
    let selection_worst_case = max_cache_file.saturating_mul(requested_indices.len() as u64);
    // PR-14: conjunto a SEMBRAR — la selección completa si cabe; si no, el
    // SUFIJO que cabe (la pasada siguiente decodifica sólo el prefijo y corta
    // el decoder ahí). Un sufijo <5% de la selección no merece la E/S.
    let cache_expected: Vec<usize> =
        decode_cache_expected_set(&exact_selected_indices, max_cache_file, cache_budget);
    let can_publish_cache =
        !complete_cache_candidate && missing_frames > 0 && !cache_expected.is_empty();
    let mut cache_transaction = if can_publish_cache {
        let expected_worst_case = max_cache_file.saturating_mul(cache_expected.len() as u64);
        prune_decode_cache_to_budget(
            &cache_dir,
            cache_budget.saturating_sub(expected_worst_case),
        );
        DecodeFrameCacheTransaction::new(
            &cache_dir,
            path_hash,
            cached_frame_len,
            cache_budget,
        )
        .map(|transaction| {
            transaction.with_expected_indices(cache_expected.iter().copied())
        })
    } else {
        None
    };
    // PR-14: modo SUFIJO-SERVIDO — el mayor sufijo de la selección con TODOS
    // sus frames ya plausibles en caché (sembrado por la pasada anterior).
    // Esos lotes salen de NVMe y el stream sólo decodifica el prefijo.
    let suffix_start_index: Option<usize> = if !complete_cache_candidate
        && !exact_selected_indices.is_empty()
    {
        let mut from = exact_selected_indices.len();
        for i in (0..exact_selected_indices.len()).rev() {
            if cache_file_is_plausible(exact_selected_indices[i]) {
                from = i;
            } else {
                break;
            }
        }
        let covered = exact_selected_indices.len() - from;
        (covered.saturating_mul(20) >= exact_selected_indices.len())
            .then(|| exact_selected_indices[from])
    } else {
        None
    };
    let mut stream_cutoff = suffix_start_index.unwrap_or(usize::MAX);
    let last_stream_index = exact_selected_indices
        .iter()
        .copied()
        .filter(|&index| index < stream_cutoff)
        .max();
    decode_route_hardware.store(use_gpu, Ordering::Release);
    log_to_front(
        app,
        "INFO",
        &format!(
            "FFmpeg apilado: decode {} según prueba corta{}; pipe {}.",
            if use_gpu { "hardware" } else { "CPU" },
            decode_probe
                .backend
                .as_deref()
                .map(|backend| format!(" ({backend})"))
                .unwrap_or_default(),
            if use_exact_selected_pipe {
                format!(
                    "select exacto {}/{} (se omiten {} frames antes de RGB48)",
                    exact_selected_indices.len(),
                    decoded_span,
                    decoded_span.saturating_sub(exact_selected_indices.len())
                )
            } else {
                "secuencial completo".to_string()
            }
        ),
    );
    if !complete_cache_candidate && cache_transaction.is_none() {
        log_to_front(
            app,
            "INFO",
            &format!(
                "Caché FFmpeg omitida: la selección completa requiere hasta {} MB y el presupuesto es {} MB (ni siquiera un sufijo ≥5% cabe); se evita comprimir un caché parcial inútil.",
                selection_worst_case.div_ceil(1024 * 1024),
                cache_budget / (1024 * 1024),
            ),
        );
    } else if can_publish_cache && cache_expected.len() < exact_selected_indices.len() {
        log_to_front(
            app,
            "INFO",
            &format!(
                "Caché FFmpeg por sufijo: se sembrarán los últimos {} de {} frames (presupuesto {} MB); la siguiente pasada decodificará sólo el prefijo.",
                cache_expected.len(),
                exact_selected_indices.len(),
                cache_budget / (1024 * 1024),
            ),
        );
    }
    if let Some(cutoff) = suffix_start_index {
        log_to_front(
            app,
            "INFO",
            &format!(
                "Caché FFmpeg: sufijo desde el frame {} servido desde NVMe; el decoder se detendrá al agotar el prefijo.",
                cutoff
            ),
        );
    }
    let t_start = std::time::Instant::now();

    for (batch_idx, indices) in chunks.iter().enumerate() {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let prefix = format!(
            "{}Cargando Lote {}/{}",
            pass_label,
            batch_idx + 1,
            total_batches
        );

        // 1. CACHÉ COMPLETA primero: sólo se activa cuando TODOS los frames de
        // la selección existen. Así la segunda pasada evita FFmpeg por completo;
        // un conjunto parcial se ignora porque el stream inter-frame tendría
        // que recorrer de todos modos los mismos GOP.
        let mut map = std::collections::HashMap::with_capacity(indices.len());
        if complete_cache_candidate {
            let cached: Vec<Option<(usize, Vec<u16>)>> = indices
                .par_iter()
                .map(|&idx| {
                    read_cached_frame(
                        &decode_cache_frame_path(&cache_dir, path_hash, idx),
                        cached_frame_len,
                    )
                    .map(|f| (idx, f))
                })
                .collect();
            map.extend(cached.into_iter().flatten());
            if !map.is_empty() {
                cache_hits.fetch_add(map.len(), std::sync::atomic::Ordering::Relaxed);
            }
            if !indices.is_empty() && map.len() == indices.len() {
                emit_progress(
                    app,
                    &format!("{}: Recuperado de cache NVMe (sin decode)", prefix),
                    100.0,
                    None,
                );
                if tx.send(Ok(map)).is_err() {
                    return Ok(());
                }
                continue;
            }
            // Un candidato corrupto invalida el modo caché completo. Los lotes
            // siguientes se sirven del mismo stream exacto. Retiramos sólo las
            // entradas que ya fallaron CRC/longitud y abrimos una transacción
            // de reparación: los frames válidos previos cuentan para el set
            // completo y el corrupto se vuelve a publicar al terminar el pase.
            complete_cache_candidate = false;
            let invalid_indices: Vec<usize> = indices
                .iter()
                .copied()
                .filter(|index| !map.contains_key(index))
                .collect();
            for index in invalid_indices {
                let invalid = decode_cache_frame_path(&cache_dir, path_hash, index);
                if read_cached_frame(&invalid, cached_frame_len).is_none() {
                    let _ = std::fs::remove_file(invalid);
                }
            }
            prune_decode_cache_to_budget(&cache_dir, cache_budget);
            if cache_transaction.is_none() {
                cache_transaction = DecodeFrameCacheTransaction::new(
                    &cache_dir,
                    path_hash,
                    cached_frame_len,
                    cache_budget,
                )
                .map(|transaction| {
                    transaction.with_expected_indices(requested_indices.iter().copied())
                });
            }
            map.clear();
        }

        // PR-14: sufijo servido desde NVMe (índices ≥ stream_cutoff).
        if stream_cutoff != usize::MAX {
            let from_cache: Vec<usize> = indices
                .iter()
                .copied()
                .filter(|&idx| idx >= stream_cutoff && !map.contains_key(&idx))
                .collect();
            if !from_cache.is_empty() {
                let cached: Vec<Option<(usize, Vec<u16>)>> = from_cache
                    .par_iter()
                    .map(|&idx| {
                        read_cached_frame(
                            &decode_cache_frame_path(&cache_dir, path_hash, idx),
                            cached_frame_len,
                        )
                        .map(|frame| (idx, frame))
                    })
                    .collect();
                let mut corrupt = false;
                let mut served = 0usize;
                for item in cached {
                    match item {
                        Some((idx, frame)) => {
                            map.insert(idx, frame);
                            served += 1;
                        }
                        None => corrupt = true,
                    }
                }
                cache_hits.fetch_add(served, std::sync::atomic::Ordering::Relaxed);
                if corrupt {
                    // Entrada corrupta en el sufijo: abandonar el modo sufijo
                    // y recuperar lo que falte con el stream recreado SIN
                    // cutoff (decodifica de más una vez; la corrección manda).
                    log_to_front(
                        app,
                        "WARN",
                        "Caché FFmpeg: entrada de sufijo corrupta; se recupera por decode.",
                    );
                    stream_cutoff = usize::MAX;
                    it = None;
                }
            }
        }

        // 2. Serve from the persistent sequential stream.
        let wanted: std::collections::HashSet<usize> = indices
            .iter()
            .copied()
            .filter(|&idx| idx < stream_cutoff)
            .collect();
        let last_wanted = wanted.iter().copied().max().unwrap_or(0);
        let mut cancelled = false;

        while map.len() < indices.len() {
            if it.is_none() {
                let stream_cancel = {
                    let token = cancel.clone();
                    Arc::new(move || token.is_cancelled()) as FfmpegCancelCheck
                };
                let hardware_backend = if use_gpu {
                    decode_probe.backend.as_deref()
                } else {
                    None
                };
                let first_needed = indices.first().copied().unwrap_or(0);
                let remaining_selected: Vec<usize> = if use_exact_selected_pipe {
                    exact_selected_indices
                        .iter()
                        .copied()
                        // PR-14: los índices del sufijo cacheado NO pasan por
                        // el stream — el decoder sólo recorre el prefijo.
                        .filter(|&index| index >= first_needed && index < stream_cutoff)
                        .collect()
                } else {
                    Vec::new()
                };
                let stream_result = if use_exact_selected_pipe {
                    FfmpegStreamIterator::new_cancelable_selected(
                        path,
                        width,
                        height,
                        0,
                        0,
                        width,
                        height,
                        color_id,
                        &reader.ffmpeg_path,
                        hardware_backend,
                        &reader.codec_name,
                        reader.rotation,
                        &remaining_selected,
                        stream_cancel,
                    )
                } else {
                    FfmpegStreamIterator::new_cancelable(
                        path,
                        width,
                        height,
                        0,
                        0,
                        width,
                        height,
                        color_id,
                        &reader.ffmpeg_path,
                        None, // sequential from 0: exact indices
                        hardware_backend,
                        &reader.codec_name,
                        reader.rotation,
                        stream_cancel,
                    )
                };
                match stream_result {
                    Ok(v) => {
                        it = Some(v);
                        pos = 0;
                    }
                    Err(e) => {
                        return Err(format!(
                            "FFmpeg no pudo abrir la ruta de decode {} exacta para el lote {}: {e}",
                            if use_gpu { "hardware" } else { "CPU" },
                            batch_idx + 1,
                        ));
                    }
                }
            }

            let s = it.as_mut().expect("stream just ensured");
            let mut stream_ok = true;
            if use_exact_selected_pipe {
                // PR-14: sólo los índices del prefijo salen del stream (el
                // filtro select del proceso excluye el sufijo cacheado).
                for &index in indices.iter().filter(|&&idx| idx < stream_cutoff) {
                    if cancel.is_cancelled() {
                        cancelled = true;
                        break;
                    }
                    if !s.read_frame_into(&mut raw_buf) {
                        stream_ok = false;
                        break;
                    }
                    map.insert(index, raw_to_u16_buffer(&raw_buf, width, height, bpp));
                    pos = index.saturating_add(1);
                    if map.len() % 25 == 0 || map.len() == indices.len() {
                        let elapsed = t_start.elapsed().as_secs_f32().max(0.001);
                        emit_progress(
                            app,
                            &format!(
                                "{} - {}/{} frames (escaneo {:.0} FPS)",
                                prefix,
                                map.len(),
                                indices.len(),
                                pos as f32 / elapsed
                            ),
                            (map.len() as f32 / indices.len() as f32) * 100.0,
                            None,
                        );
                    }
                }
            } else {
                while pos <= last_wanted {
                    if pos % 32 == 0 && cancel.is_cancelled() {
                        cancelled = true;
                        break;
                    }
                    if !s.read_frame_into(&mut raw_buf) {
                        stream_ok = false;
                        break;
                    }
                    if wanted.contains(&pos) && !map.contains_key(&pos) {
                        map.insert(pos, raw_to_u16_buffer(&raw_buf, width, height, bpp));
                        if map.len() % 25 == 0 || map.len() == indices.len() {
                            let elapsed = t_start.elapsed().as_secs_f32().max(0.001);
                            emit_progress(
                                app,
                                &format!(
                                    "{} - {}/{} frames (escaneo {:.0} FPS)",
                                    prefix,
                                    map.len(),
                                    indices.len(),
                                    pos as f32 / elapsed
                                ),
                                (map.len() as f32 / indices.len() as f32) * 100.0,
                                None,
                            );
                        }
                    }
                    pos += 1;
                }
            }

            if cancelled || map.len() == indices.len() {
                break;
            }
            if !stream_ok {
                // El intento completo se descarta. El caller reinicia TODO el
                // pase por CPU cuando falló hardware; no mezcla en el mismo
                // acumulador frames de una ruta que terminó con error.
                return Err(format!(
                    "FFmpeg {} terminó antes de completar el lote {}: {}/{} frames exactos (siguiente índice de decode {})",
                    if use_gpu { "hardware" } else { "CPU" },
                    batch_idx + 1,
                    map.len(),
                    indices.len(),
                    pos
                ));
            }
        }

        // PR-14: prefijo agotado — matar el decoder YA (Drop termina y
        // cosecha el proceso ffmpeg); el resto del pase se sirve de NVMe. La
        // ruta HW se valida antes de soltar el stream (la validación final
        // sobre `it` ya no lo verá). Sólo aplica mientras el modo sufijo siga
        // activo (una corrupción lo desactiva y el stream vuelve a ser dueño
        // de todos los índices).
        if stream_cutoff != usize::MAX {
            if let Some(last) = last_stream_index {
                if pos > last {
                    if let Some(stream) = it.as_ref() {
                        stream.validate_hardware_route()?;
                    }
                    it = None;
                }
            }
        }

        if cancelled {
            return Ok(());
        }
        if map.len() != indices.len() || indices.iter().any(|idx| !map.contains_key(idx)) {
            return Err(format!(
                "Lote FFmpeg incompleto: {}/{} frames; no se publicará ni cacheará",
                map.len(),
                indices.len()
            ));
        }

        // 3. Preparar los frames en staging. Sólo se publican cuando TODOS los
        // lotes del intento terminan correctamente.
        if let Some(transaction) = cache_transaction.as_ref() {
            map.par_iter().for_each(|(&idx, frame)| {
                transaction.write_frame(idx, frame);
            });
        }

        if tx.send(Ok(map)).is_err() {
            return Ok(());
        }
    }
    if cancel.is_cancelled() {
        return Ok(());
    }
    if let Some(stream) = it.as_ref() {
        stream.validate_hardware_route()?;
    }
    if let Some(transaction) = cache_transaction.as_mut() {
        transaction.commit();
    }
    Ok(())
}

const PLANETARY_OS_RAM_RESERVE_BYTES: u64 = 768 * 1024 * 1024;
const PLANETARY_MIN_OPERATION_BUDGET_BYTES: u64 = 16 * 1024 * 1024;
const PLANETARY_IN_FLIGHT_BATCHES: u64 = 3;
const MAX_ROBUST_REFERENCE_FRAMES: usize = 20;

#[derive(Clone, Copy, Debug)]
struct PlanetaryRamInputs {
    available_ram: u64,
    width_in: usize,
    height_in: usize,
    width_out: usize,
    height_out: usize,
    source_bytes_per_pixel: usize,
    is_color: bool,
    double_pass: bool,
    use_warp_map: bool,
    surface_or_large_disc: bool,
    robust_reference_frames: usize,
    hardware_threads: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlanetaryRamPlan {
    stack_threads: usize,
    frames_per_batch: usize,
    robust_reference_frames: usize,
    working_budget: u64,
    stack_fixed_bytes: u64,
    bytes_per_frame: u64,
    per_thread_scratch_bytes: u64,
    transition_peak_bytes: u64,
    post_peak_bytes: u64,
    robust_reference_peak_bytes: u64,
    estimated_stack_peak_bytes: u64,
}

#[inline]
fn checked_ram_mul(a: u64, b: u64, label: &str) -> Result<u64, String> {
    a.checked_mul(b)
        .ok_or_else(|| format!("El cálculo de RAM planetaria se desbordó en {label}"))
}

#[inline]
fn checked_ram_add(a: u64, b: u64, label: &str) -> Result<u64, String> {
    a.checked_add(b)
        .ok_or_else(|| format!("El cálculo de RAM planetaria se desbordó en {label}"))
}

fn try_filled_vec<T: Clone>(len: usize, value: T, label: &str) -> Result<Vec<T>, String> {
    let bytes = len
        .checked_mul(std::mem::size_of::<T>())
        .ok_or_else(|| format!("La reserva para {label} excede el espacio direccionable"))?;
    let mut values = Vec::new();
    values.try_reserve_exact(len).map_err(|error| {
        format!(
            "No se pudo reservar {bytes} bytes para {label}: {error}. Reduce drizzle/ROI o libera RAM."
        )
    })?;
    values.resize(len, value);
    Ok(values)
}

/// `sysinfo` 0.30 calcula `available_memory()` en macOS como
/// free+inactive+purgeable-compressed. En equipos con mucha memoria comprimida
/// esa resta puede saturar a cero aunque `total-used` todavía muestre varios
/// GiB reclamables. Usamos la mejor de ambas lecturas, limitada siempre por la
/// RAM física. En Linux `available` ya incluye page cache reclamable y gana;
/// en Windows ambas magnitudes son equivalentes.
fn resolve_planetary_available_memory(reported: u64, total: u64, used: u64) -> u64 {
    // `used == 0` con RAM total no nula suele significar que host_statistics
    // falló; en ese caso no asumimos peligrosamente que toda la RAM está libre.
    let accounted_available = if total > 0 && used > 0 {
        total.saturating_sub(used.min(total))
    } else {
        0
    };
    let resolved = reported.max(accounted_available);
    if total > 0 {
        resolved.min(total)
    } else {
        resolved
    }
}

fn planetary_available_memory_snapshot() -> u64 {
    let mut system = System::new();
    system.refresh_memory();
    resolve_planetary_available_memory(
        system.available_memory(),
        system.total_memory(),
        system.used_memory(),
    )
}

/// Normaliza cualquier frame canónico a RGB16 sin confundir RGB/BGR directo
/// (`3 * width * height` muestras) con un mosaico Bayer de un solo plano. La
/// reserva se hace de forma fallible antes de entrar al demosaico; al entregar
/// un buffer con capacidad suficiente `debayer_into_buffer` no vuelve a
/// reservar. También preserva la conversión BGR->RGB definida por ColorID.
fn reference_frame_to_rgb(
    master: &[u16],
    width: usize,
    height: usize,
    is_color: bool,
    color_id: i32,
) -> Result<Vec<u16>, String> {
    let pixels = width
        .checked_mul(height)
        .ok_or("La referencia robusta excede el espacio direccionable")?;
    let rgb_len = pixels
        .checked_mul(3)
        .ok_or("La referencia RGB excede el espacio direccionable")?;
    if is_color {
        if master.len() != pixels && master.len() != rgb_len {
            return Err(format!(
                "El frame de referencia no coincide con la geometría: recibió {} muestras; se esperaban {} para mono/CFA o {} para RGB/BGR",
                master.len(), pixels, rgb_len
            ));
        }
        let mut reference = try_filled_vec(rgb_len, 0u16, "referencia RGB robusta")?;
        debayer_into_buffer(master, width, height, color_id, &mut reference);
        if reference.len() != rgb_len {
            return Err("El debayer de la referencia produjo una geometría RGB inválida".into());
        }
        Ok(reference)
    } else {
        if master.len() != pixels {
            return Err(format!(
                "El frame mono de referencia no coincide con la geometría: recibió {} muestras y se esperaban {}",
                master.len(), pixels
            ));
        }
        let mut reference = try_filled_vec(
            rgb_len,
            0u16,
            "fallback RGB de la referencia robusta",
        )?;
        for (rgb, &value) in reference.chunks_exact_mut(3).zip(master.iter()) {
            rgb.fill(value);
        }
        Ok(reference)
    }
}

/// Variante `owned` del normalizador de referencia. Los lectores FFmpeg/SER
/// RGB directo ya entregan exactamente `3 * width * height` muestras u16: en
/// ese caso reutilizamos la asignación en vez de reservar y copiar otro frame
/// de ~112 MiB (3312x5888 RGB16). BGR conserva la misma permutación que
/// `debayer_into_buffer`, pero se hace in-place.
fn reference_frame_to_rgb_owned(
    mut master: Vec<u16>,
    width: usize,
    height: usize,
    is_color: bool,
    color_id: i32,
) -> Result<Vec<u16>, String> {
    let pixels = width
        .checked_mul(height)
        .ok_or("La referencia robusta excede el espacio direccionable")?;
    let rgb_len = pixels
        .checked_mul(3)
        .ok_or("La referencia RGB excede el espacio direccionable")?;
    if is_color && master.len() == rgb_len {
        if ser::ser_color_is_direct_bgr(color_id) {
            for pixel in master.chunks_exact_mut(3) {
                pixel.swap(0, 2);
            }
        }
        return Ok(master);
    }
    reference_frame_to_rgb(&master, width, height, is_color, color_id)
}

/// Extrae la señal verde contractual sin materializar un RGB intermedio cuando
/// el origen ya es RGB/BGR directo. Para CFA/YUV delega al camino histórico,
/// manteniendo exactamente el mismo demosaico y los mismos samples.
fn reference_frame_to_green(
    master: &[u16],
    width: usize,
    height: usize,
    is_color: bool,
    color_id: i32,
) -> Result<Vec<u16>, String> {
    let pixels = width
        .checked_mul(height)
        .ok_or("La referencia robusta excede el espacio direccionable")?;
    if !is_color {
        if master.len() != pixels {
            return Err(format!(
                "El frame mono de referencia no coincide con la geometría: recibió {} muestras y se esperaban {}",
                master.len(), pixels
            ));
        }
        return Ok(master.to_vec());
    }
    if master.len() == pixels.saturating_mul(3) {
        return Ok(master.chunks_exact(3).map(|pixel| pixel[1]).collect());
    }
    Ok(reference_frame_to_rgb(master, width, height, true, color_id)?
        .chunks_exact(3)
        .map(|pixel| pixel[1])
        .collect())
}

fn robust_reference_frame_limit(active_frames: usize, is_v3: bool, small_planet: bool) -> usize {
    if active_frames == 0 {
        return 0;
    }
    let requested = if is_v3 && small_planet {
        (active_frames / 8).clamp(4, 12)
    } else if is_v3 {
        (active_frames / 5).clamp(4, MAX_ROBUST_REFERENCE_FRAMES)
    } else {
        (active_frames / 10).clamp(2, 12)
    };
    requested.min(active_frames)
}

/// Orden total unico para cualquier decision de "mejor frame": mayor score y,
/// en empate, menor indice absoluto. Usar el mismo orden en `best_idx` y en el
/// top-N evita que un empate deje al mejor ancla fuera del lote robusto.
fn planetary_frame_quality_order(
    a: &FrameAlignmentData,
    b: &FrameAlignmentData,
) -> std::cmp::Ordering {
    b.score.cmp(&a.score).then_with(|| a.idx.cmp(&b.idx))
}

/// La entrada ya está ordenada. Las desviaciones absolutas a la mediana forman
/// dos secuencias ordenadas (desde el centro hacia cada extremo), por lo que su
/// mediana se obtiene con un merge de sólo `n/2 + 1` pasos. Evita el segundo
/// sort por canal/píxel sin aproximar MAD ni cambiar el orden de la suma final.
#[inline]
fn median_abs_deviation_from_sorted(vals: &[f32], med: f32) -> f32 {
    let n = vals.len();
    debug_assert!(n > 0);
    let lower_rank = (n - 1) / 2;
    let upper_rank = n / 2;
    let mut left = ((n - 1) / 2) as isize;
    let mut right = (n + 1) / 2;
    let (mut lower, mut upper) = (0.0f32, 0.0f32);
    for rank in 0..=upper_rank {
        let left_deviation = if left >= 0 {
            (vals[left as usize] - med).abs()
        } else {
            f32::INFINITY
        };
        let right_deviation = if right < n {
            (vals[right] - med).abs()
        } else {
            f32::INFINITY
        };
        let next = if left_deviation <= right_deviation {
            left -= 1;
            left_deviation
        } else {
            right += 1;
            right_deviation
        };
        if rank == lower_rank {
            lower = next;
        }
        if rank == upper_rank {
            upper = next;
        }
    }
    if n % 2 == 1 {
        upper
    } else {
        0.5 * (lower + upper)
    }
}

/// Media sigma-clipped robusta. Conserva el sort y la suma ascendente de la
/// implementación histórica (salida f32 bit-exacta); sólo reemplaza el sort
/// redundante de desviaciones por el merge exacto anterior.
#[inline]
fn robust_ref_combine(vals: &mut [f32]) -> f32 {
    let n = vals.len();
    if n == 0 {
        return 0.0;
    }
    if n < 5 {
        return vals.iter().sum::<f32>() / n as f32;
    }
    // NaN-safe: total_cmp es idéntico a partial_cmp para floats normales y no
    // puede hacer panic si un píxel degenerado (0/0) cuela un NaN.
    vals.sort_by(f32::total_cmp);
    let med = if n % 2 == 1 {
        vals[n / 2]
    } else {
        0.5 * (vals[n / 2 - 1] + vals[n / 2])
    };
    let mad = median_abs_deviation_from_sorted(vals, med);
    let tol = (3.0 * 1.4826 * mad).max(med.abs() * 0.001 + 8.0);
    let (mut sum, mut kept) = (0.0f32, 0u32);
    for &value in vals.iter() {
        if (value - med).abs() <= tol {
            sum += value;
            kept += 1;
        }
    }
    if kept > 0 {
        sum / kept as f32
    } else {
        med
    }
}

struct RobustReferenceRow<'a> {
    pixels: &'a [u16],
    dx: f32,
    fy: f32,
    row0_offset: usize,
    row1_offset: usize,
}

/// Muestrea los canales de un píxel para todos los frames que cubren la fila.
/// Los offsets verticales y la validez Y se calculan una sola vez por fila; el
/// orden de frames, pesos y operaciones f32 permanece idéntico al histórico.
#[inline]
fn sample_robust_reference_pixel(
    row_refs: &[RobustReferenceRow<'_>],
    x: usize,
    width: usize,
    vals_r: &mut [f32; MAX_ROBUST_REFERENCE_FRAMES],
    vals_g: &mut [f32; MAX_ROBUST_REFERENCE_FRAMES],
    vals_b: &mut [f32; MAX_ROBUST_REFERENCE_FRAMES],
) -> usize {
    let mut count = 0usize;
    for frame in row_refs {
        let sxf = x as f32 - frame.dx;
        if sxf < 0.0 || sxf >= (width - 1) as f32 {
            continue;
        }
        debug_assert!(count < MAX_ROBUST_REFERENCE_FRAMES);
        let x0 = sxf as usize;
        let fx = sxf - x0 as f32;
        let one_minus_fx = 1.0 - fx;
        let one_minus_fy = 1.0 - frame.fy;
        let w00 = one_minus_fx * one_minus_fy;
        let w10 = fx * one_minus_fy;
        let w01 = one_minus_fx * frame.fy;
        let w11 = fx * frame.fy;
        let i00 = frame.row0_offset + x0 * 3;
        let i01 = frame.row1_offset + x0 * 3;
        vals_r[count] = frame.pixels[i00] as f32 * w00
            + frame.pixels[i00 + 3] as f32 * w10
            + frame.pixels[i01] as f32 * w01
            + frame.pixels[i01 + 3] as f32 * w11;
        vals_g[count] = frame.pixels[i00 + 1] as f32 * w00
            + frame.pixels[i00 + 4] as f32 * w10
            + frame.pixels[i01 + 1] as f32 * w01
            + frame.pixels[i01 + 4] as f32 * w11;
        vals_b[count] = frame.pixels[i00 + 2] as f32 * w00
            + frame.pixels[i00 + 5] as f32 * w10
            + frame.pixels[i01 + 2] as f32 * w01
            + frame.pixels[i01 + 5] as f32 * w11;
        count += 1;
    }
    count
}

/// Plan puro del pico de memoria. Nunca inventa un worker o un frame cuando el
/// presupuesto ya está agotado: si no caben las partidas fijas, un scratch CPU
/// (necesario también como fallback de GPU) y el mínimo real del prefetcher,
/// rechaza antes de que un `vec!` grande pueda abortar el proceso.
fn plan_planetary_ram(input: PlanetaryRamInputs) -> Result<PlanetaryRamPlan, String> {
    if input.width_in == 0
        || input.height_in == 0
        || input.width_out == 0
        || input.height_out == 0
    {
        return Err("No se puede planificar RAM para una geometría vacía".into());
    }
    if input.source_bytes_per_pixel == 0 || input.hardware_threads == 0 {
        return Err("El plan de RAM requiere bytes por píxel e hilos disponibles válidos".into());
    }

    // La reserva nominal no debe convertirse en un umbral mínimo artificial:
    // con presión de memoria una captura pequeña puede caber aunque el snapshot
    // sea menor a 768 MiB. Apartamos como máximo 1/4 de lo disponible y damos
    // un suelo pequeño al plan; todas las reservas grandes son fallibles, por
    // lo que un OOM real sigue regresando Err sin abortar el proceso.
    let os_reserve = PLANETARY_OS_RAM_RESERVE_BYTES.min(input.available_ram / 4);
    let usable_ram = input.available_ram.saturating_sub(os_reserve);
    let scaled_budget = checked_ram_mul(usable_ram, 85, "presupuesto de trabajo")? / 100;
    // El suelo evita decisiones absurdamente pequeñas para trabajos diminutos,
    // pero nunca inventa RAM que el snapshot no reportó como utilizable.
    let working_budget = scaled_budget.max(PLANETARY_MIN_OPERATION_BUDGET_BYTES.min(usable_ram));
    if working_budget == 0 {
        return Err(
            "No hay RAM reclamable disponible para iniciar el apilado planetario; cierra otras aplicaciones o reduce la ROI"
                .into(),
        );
    }

    let n_in = checked_ram_mul(
        input.width_in as u64,
        input.height_in as u64,
        "píxeles de entrada",
    )?;
    let n_out = checked_ram_mul(
        input.width_out as u64,
        input.height_out as u64,
        "píxeles de salida",
    )?;
    let channels = if input.is_color { 3 } else { 1 };
    // El prefetch conserva el formato canónico del lector, no el resultado del
    // debayer. SER8/16 Bayer es un único plano u16 aunque `is_color=true`;
    // FFmpeg RGB48/AVI RGB sí contienen tres canales.
    let cached_u16_channels = if matches!(input.source_bytes_per_pixel, 3 | 6) {
        3
    } else {
        1
    };
    let expanded_frame = checked_ram_mul(
        checked_ram_mul(n_in, cached_u16_channels, "canales almacenados por frame")?,
        2,
        "frame u16",
    )?;
    // Vec/HashMap/alineación añaden cabeceras y buffers transitorios. 8/5 es
    // el mismo 1.6x histórico, expresado con enteros y sin casts saturantes.
    let bytes_per_frame = checked_ram_add(
        checked_ram_mul(expanded_frame, 8, "margen por frame")?,
        4,
        "redondeo por frame",
    )? / 5;

    // Scratch CPU real: RGB/mono + tres mapas de alineación + pirámide; cuatro
    // planos f32 de salida en color o dos en mono. Se presupuesta incluso con
    // GPU porque el contrato Auto/Hybrid debe poder caer a CPU sin OOM.
    // Incluye DenseQualityMap temporal por frame además de los Vec persistentes
    // de LiquidScratch. Redondear hacia arriba mantiene margen de asignador.
    let input_scratch_bpp = if input.is_color { 16 } else { 10 };
    let output_scratch_bpp = if input.is_color { 16 } else { 8 };
    let per_thread_scratch_bytes = checked_ram_add(
        checked_ram_mul(n_in, input_scratch_bpp, "scratch de entrada")?,
        checked_ram_mul(n_out, output_scratch_bpp, "scratch de salida")?,
        "scratch por hilo",
    )?;

    // Fases reales (no coexistentes): durante el stack pass 1 tracked usa
    // 32 B/px/canal; los bounds (+8) se construyen DESPUÉS de soltar scratch y
    // prefetch. Pass 2 usa 16+8. El plan anterior sumaba 40 + postprocesado al
    // scratch activo y por eso limitó falsamente 3312×5888 a 2/10 hilos.
    let stack_accumulator_bpp = if input.double_pass { 32 } else { 16 };
    let global_stack_accum = checked_ram_mul(
        checked_ram_mul(n_out, stack_accumulator_bpp, "acumuladores de stack")?,
        channels,
        "canales de acumulación de stack",
    )?;
    let idw_k = if input.surface_or_large_disc { 4 } else { 8 };
    let warp_map = if input.use_warp_map {
        checked_ram_mul(
            checked_ram_mul(n_out, 6, "mapa IDW")?,
            idw_k,
            "vecinos IDW",
        )?
    } else {
        0
    };
    // Pico post-stack real (los acumuladores siguen vivos): RGB f32 lineal,
    // salida u16, planos temporales de RGB Align, reparación de cobertura y
    // sharpening/normalización. Mono también materializa una salida RGB para
    // mantener el contrato de la UI, pero evita los planos de alineación de
    // color. El margen incluye asignadores/temporales de una pasada completa.
    let post_output_bpp = if input.is_color { 112 } else { 72 };
    let post_output = checked_ram_mul(n_out, post_output_bpp, "salidas post-apilado")?;
    // En color conviven la referencia RGB limpia y de alineación, mono,
    // bordes, pirámide y mapas auxiliares. En mono se ahorran algunos planos,
    // pero la referencia robusta sigue publicándose internamente como RGB.
    let persistent_master_bpp = if input.is_color { 40 } else { 28 };
    let persistent_master = checked_ram_mul(
        n_in,
        persistent_master_bpp,
        "referencias persistentes",
    )?;
    let stack_fixed_bytes = checked_ram_add(
        checked_ram_add(global_stack_accum, warp_map, "acumuladores y warp")?,
        persistent_master,
        "partidas fijas durante el apilado",
    )?;
    let transition_accumulator_bpp = if input.double_pass { 40 } else { 16 };
    let transition_accum = checked_ram_mul(
        checked_ram_mul(n_out, transition_accumulator_bpp, "transición de acumuladores")?,
        channels,
        "canales de transición",
    )?;
    let transition_peak_bytes = checked_ram_add(
        checked_ram_add(transition_accum, warp_map, "transición y warp")?,
        persistent_master,
        "pico de transición entre pasadas",
    )?;
    let post_accumulator_bpp = if input.double_pass { 24 } else { 16 };
    let post_accum = checked_ram_mul(
        checked_ram_mul(n_out, post_accumulator_bpp, "acumuladores del post")?,
        channels,
        "canales del post",
    )?;
    // Conservador: el mapa warp sigue vivo por alcance hasta acabar el stack.
    // Aun así no se mezcla con scratch/prefetch, que era el gran sobreconteo.
    let post_peak_bytes = checked_ram_add(
        checked_ram_add(
            checked_ram_add(post_accum, post_output, "acumuladores y salida post")?,
            warp_map,
            "post y warp",
        )?,
        persistent_master,
        "pico de postprocesado",
    )?;

    for (phase, peak) in [
        ("transición entre pasadas", transition_peak_bytes),
        ("postprocesado", post_peak_bytes),
    ] {
        if peak >= working_budget {
            return Err(format!(
                "RAM insuficiente para {phase}: necesita ~{} MB de {} MB seguros. Reduce drizzle/ROI o libera RAM.",
                peak.div_ceil(1024 * 1024),
                working_budget / (1024 * 1024),
            ));
        }
    }

    // Durante la referencia robusta los RAW restantes conviven con los RGB ya
    // expandidos, pero NO existen N copias simultáneas de RAW+RGB: a medida que
    // un frame se convierte se libera su RAW. Presupuestamos el mayor conjunto
    // residente (N * max(raw, RGB)) más el transitorio de una conversión. El
    // cálculo anterior sumaba N*(raw+RGB), casi 2x en FFmpeg RGB48, y rechazaba
    // capturas que sí cabían.
    let raw_frame = checked_ram_mul(
        n_in,
        input.source_bytes_per_pixel as u64,
        "frame RAW de referencia",
    )?;
    let rgb_reference_frame = checked_ram_mul(n_in, 6, "RGB de referencia")?;
    let source_u16_frame = checked_ram_mul(
        n_in,
        if matches!(input.source_bytes_per_pixel, 3 | 6) {
            6
        } else {
            2
        },
        "frame u16 de referencia",
    )?;
    let conversion_extra = if matches!(input.source_bytes_per_pixel, 3 | 6) {
        0
    } else {
        rgb_reference_frame
    };
    let conversion_transient = checked_ram_add(
        checked_ram_add(raw_frame, source_u16_frame, "RAW+u16 de referencia")?,
        conversion_extra,
        "conversión RGB de referencia",
    )?;
    let robust_base = checked_ram_mul(n_in, 16, "buffers base de referencia")?;
    if robust_base > working_budget {
        return Err(format!(
            "RAM insuficiente incluso para una referencia de un solo frame: necesita ~{} MB y el presupuesto seguro es {} MB. Reduce resolución/ROI.",
            robust_base.div_ceil(1024 * 1024),
            working_budget / (1024 * 1024)
        ));
    }
    let resident_per_frame = raw_frame.max(rgb_reference_frame);
    let reference_room = working_budget
        .saturating_sub(robust_base)
        .saturating_sub(conversion_transient);
    let max_reference_frames = if resident_per_frame == 0 {
        0
    } else {
        (reference_room / resident_per_frame).min(usize::MAX as u64) as usize
    };
    let robust_reference_frames = input
        .robust_reference_frames
        .min(max_reference_frames);
    let robust_reference_peak_bytes = if robust_reference_frames == 0 {
        robust_base
    } else {
        checked_ram_add(
            checked_ram_add(robust_base, conversion_transient, "base+conversión robusta")?,
            checked_ram_mul(
                resident_per_frame,
                robust_reference_frames as u64,
                "frames residentes de referencia robusta",
            )?,
            "pico de referencia robusta",
        )?
    };

    let minimum_prefetch = checked_ram_mul(
        bytes_per_frame,
        PLANETARY_IN_FLIGHT_BATCHES,
        "mínimo del prefetcher",
    )?;
    let fixed_plus_minimum = checked_ram_add(
        stack_fixed_bytes,
        minimum_prefetch,
        "fijo más prefetch mínimo",
    )?;
    if fixed_plus_minimum >= working_budget {
        return Err(format!(
            "RAM insuficiente para el lienzo planetario: las partidas fijas y un frame por lote requieren ~{} MB de {} MB seguros; no se iniciará para evitar OOM.",
            fixed_plus_minimum / (1024 * 1024),
            working_budget / (1024 * 1024)
        ));
    }
    let thread_room = working_budget - fixed_plus_minimum;
    let max_threads_by_ram = thread_room / per_thread_scratch_bytes;
    if max_threads_by_ram == 0 {
        return Err(format!(
            "RAM insuficiente: no cabe ni un scratch de apilado ({} MB) después de reservar lienzo y prefetch.",
            per_thread_scratch_bytes / (1024 * 1024)
        ));
    }
    let stack_threads = input.hardware_threads.min(max_threads_by_ram as usize);
    if stack_threads == 0 {
        return Err("RAM insuficiente: el plan produjo cero hilos seguros".into());
    }
    let scratch_total = checked_ram_mul(
        per_thread_scratch_bytes,
        stack_threads as u64,
        "scratch total",
    )?;
    let ram_for_frames = working_budget
        .checked_sub(stack_fixed_bytes)
        .and_then(|remaining| remaining.checked_sub(scratch_total))
        .ok_or("RAM insuficiente después de reservar scratch y partidas fijas")?;
    let per_batch_budget = ram_for_frames / PLANETARY_IN_FLIGHT_BATCHES;
    let mut frames_per_batch = (per_batch_budget / bytes_per_frame) as usize;
    if frames_per_batch == 0 {
        return Err("RAM insuficiente: no cabe un frame por lote del prefetcher".into());
    }
    let batch_cap = if input.width_in >= 3000 {
        1000
    } else if input.width_in >= 1920 {
        2000
    } else {
        3000
    };
    frames_per_batch = frames_per_batch.min(batch_cap).max(1);
    let frame_batches_peak = checked_ram_mul(
        checked_ram_mul(bytes_per_frame, frames_per_batch as u64, "lote de frames")?,
        PLANETARY_IN_FLIGHT_BATCHES,
        "lotes simultáneos",
    )?;
    let dynamic_stack_peak = checked_ram_add(
        checked_ram_add(stack_fixed_bytes, scratch_total, "fijo+scratch")?,
        frame_batches_peak,
        "pico de apilado",
    )?;
    let estimated_stack_peak_bytes = dynamic_stack_peak
        .max(transition_peak_bytes)
        .max(post_peak_bytes);
    debug_assert!(estimated_stack_peak_bytes <= working_budget);

    Ok(PlanetaryRamPlan {
        stack_threads,
        frames_per_batch,
        robust_reference_frames,
        working_budget,
        stack_fixed_bytes,
        bytes_per_frame,
        per_thread_scratch_bytes,
        transition_peak_bytes,
        post_peak_bytes,
        robust_reference_peak_bytes,
        estimated_stack_peak_bytes,
    })
}


#[tauri::command]
async fn stack_video_liquid_warping(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    percent: f32,
    custom_points: Vec<ApPoint>,
    drizzle: f32,
    is_surface: bool,
    bayer_override: Option<i32>,
    ap_size: u32,
    sharpened: bool,
    sharpen_intensity: f32,
    double_pass: bool,
    warping_analysis: bool,            // NEW
    anchor_override: Option<Vec<i32>>, // NEW
    stacking_roi: Option<Vec<u32>>,    // CUSTOM ROI
    normalize_colors: bool,
    is_v3: bool,
    target_type: String, // NEW
    keep_full_frame: Option<bool>, // NEW: no recortar bordes (mantener encuadre completo)
    align_rgb: Option<bool>,       // NEW: alineacion RGB automatica (switch de usuario)
    gpu_mode: Option<String>,      // GPU compute: "auto" | "gpu" | "cpu" (None = auto)
) -> Result<String, String> {
    validate_planetary_stack_parameters(
        &path,
        percent,
        &custom_points,
        drizzle,
        ap_size,
        sharpen_intensity,
        anchor_override.as_deref(),
        stacking_roi.as_deref(),
    )?;
    let request_id = begin_planetary_user_job(&state);
    // PR-2.5: pool blocking (ver analyze_video_v2).
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        stack_video_liquid_warping_impl(
            &app,
            &state,
            request_id,
            path,
            percent,
            custom_points,
            drizzle,
            is_surface,
            bayer_override,
            ap_size,
            sharpened,
            sharpen_intensity,
            double_pass,
            warping_analysis,
            anchor_override,
            stacking_roi,
            normalize_colors,
            is_v3,
            target_type,
            None,
            keep_full_frame,
            align_rgb,
            gpu_mode,
        )
    })
    .await
    .map_err(|e| format!("El hilo de apilado terminó inesperadamente: {e}"))?
}

/// Contrato tipado del segundo paso planetario. Toda decisión de GPU se delega
/// al motor común, incluida la semántica estricta de GpuOnly.
#[tauri::command]
async fn run_planetary_stack(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    request: PlanetaryStackRequest,
) -> Result<String, String> {
    request.validate_static()?;
    let request = request.resolved_profile();
    request.validate_static()?;
    let request_id = begin_planetary_user_job(&state);
    // PR-2.5: pool blocking (ver analyze_video_v2).
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        stack_video_liquid_warping_impl(
            &app,
            &state,
            request_id,
            request.path,
            request.percent,
            request.custom_points,
            request.drizzle,
            request.is_surface,
            request.bayer_override,
            request.ap_size,
            request.sharpened,
            request.sharpen_intensity,
            request.double_pass,
            request.warping_analysis,
            request.anchor_override,
            request.stacking_roi,
            request.normalize_colors,
            request.is_v3,
            request.target_type,
            None,
            request.keep_full_frame,
            request.align_rgb,
            Some(request.compute_policy.legacy_value().into()),
        )
    })
    .await
    .map_err(|e| format!("El hilo de apilado terminó inesperadamente: {e}"))?
}

/// Internal engine entry point — callable from other commands (batch mode)
/// with `&AppHandle`/`&State` (same pattern as perform_standardized_analysis).
/// `progress_prefix` lets batch mode tag progress messages ("[2/7] …").
#[allow(clippy::too_many_arguments)]
// PR-2.5: SÍNCRONA (antes async sin ningún .await interno) — ver
// perform_standardized_analysis; los comandos usan spawn_blocking.
fn stack_video_liquid_warping_impl(
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
    request_id: usize,
    path: String,
    percent: f32,
    custom_points: Vec<ApPoint>,
    drizzle: f32,
    is_surface: bool,
    bayer_override: Option<i32>,
    ap_size: u32,
    sharpened: bool,
    sharpen_intensity: f32,
    double_pass: bool,
    warping_analysis: bool,
    anchor_override: Option<Vec<i32>>,
    stacking_roi: Option<Vec<u32>>,
    normalize_colors: bool,
    is_v3: bool,
    target_type: String,
    progress_prefix: Option<String>,
    keep_full_frame: Option<bool>,
    align_rgb: Option<bool>,
    gpu_mode: Option<String>,
) -> Result<String, String> {
    validate_planetary_stack_parameters(
        &path,
        percent,
        &custom_points,
        drizzle,
        ap_size,
        sharpen_intensity,
        anchor_override.as_deref(),
        stacking_roi.as_deref(),
    )?;
    let app = app.clone();
    let job_token =
        PlanetaryJobToken::for_app(&app, request_id, state.cancel_requested.clone());
    if job_token.is_cancelled() {
        return Err("Cancelado o sustituido por otro apilado".into());
    }
    let pipeline_job_id = new_job_id("planetary-stack");
    let pipeline_started = std::time::Instant::now();
    // perf_trace: desglose por fase del apilado (JSON al terminar; el guard
    // vuelca también en errores/cancelaciones con completed=false).
    let pt = crate::perf_trace::job_start("stack", &path);
    let pt_guard = crate::perf_trace::JobGuard::new(pt);
    crate::perf_trace::job_meta(pt, "double_pass", double_pass);
    crate::perf_trace::job_meta(pt, "drizzle", drizzle);
    let is_surface = is_surface || is_surface_target(&target_type);
    let warping_analysis = zenith_should_warp(&target_type, is_surface, warping_analysis);
    let target_category = if is_surface {
        TargetCategory::Surface
    } else {
        TargetCategory::from_str(&target_type)
    };
    let mut category_profile = target_category.profile();
    #[cfg(target_arch = "x86_64")]
    let use_avx2 = is_x86_feature_detected!("avx2");
    #[cfg(not(target_arch = "x86_64"))]
    let use_avx2 = false;

    let pp = progress_prefix.unwrap_or_default();
    let init_msg = if use_avx2 {
        format!("{}Inicializando Zenith Presicion Ultimate (AVX2)...", pp)
    } else {
        format!("{}Inicializando Zenith Presicion Ultimate...", pp)
    };
    emit_progress(&app, &init_msg, 0.0, None);

    // 1. Load Analysis Cache (V2)
    let open_cancel = {
        let token = job_token.clone();
        Arc::new(move || token.is_cancelled()) as FfmpegCancelCheck
    };
    let r = VideoInput::open_cancelable(&path, &app, open_cancel)?;
    let w_in = r.width();
    let h_in = r.height();
    validate_planetary_stack_geometry(
        w_in,
        h_in,
        &custom_points,
        anchor_override.as_deref(),
        stacking_roi.as_deref(),
    )?;
    let (planned_w_out, planned_h_out) =
        planetary_output_dimensions(w_in, h_in, stacking_roi.as_deref(), drizzle)?;
    let resolved_color_id = r.resolve_bayer_override(bayer_override)?;
    let mut requested_roi = requested_analysis_roi(
        w_in,
        h_in,
        &target_type,
        is_surface,
        warping_analysis,
        anchor_override.as_deref(),
    );
    if ser::ser_color_is_yuv422(resolved_color_id) {
        requested_roi = align_yuv422_analysis_roi(requested_roi, w_in);
    }
    let source_fingerprint = planetary_source_fingerprint(&path)?;
    let cache_expectation = AnalysisCacheExpectation {
        source_fingerprint,
        resolved_color_id,
        target_type: normalized_analysis_target(&target_type),
        is_surface,
        warping_analysis,
        anchor_override: anchor_override.clone(),
        requested_roi,
        width: w_in,
        height: h_in,
        declared_frame_count: r.frame_count(),
        frame_count_exact: r.frame_count_is_exact(),
    };
    let suffix = zenith_analysis_cache_suffix(
        &target_type,
        is_surface,
        warping_analysis,
        resolved_color_id,
        anchor_override.as_deref(),
        requested_roi,
    );

    let cache_path = get_analysis_cache_path(&path, &suffix);

    let (mut cached, _cache_location) =
        load_validated_analysis_cache(&cache_path, &cache_expectation).ok_or(
            "No se encontró un análisis Zenith válido para este origen/ROI/CFA. Vuelve a analizar el video.",
        )?;

    // Update cache with current AP points for future use (e.g. Batch mode).
    // SOLO si realmente cambiaron: antes se reescribia el cache COMPLETO (con
    // los grid_scores de todos los frames) en CADA clic de apilar.
    if !custom_points.is_empty()
        && cached.ap_points.as_deref() != Some(custom_points.as_slice())
    {
        cached.ap_points = Some(custom_points.clone());
        publish_analysis_cache_best_effort(
            &app,
            state,
            &job_token,
            &cache_path,
            &cached,
            &cache_expectation,
        )?;
    }

    // 2. Identify Frames
    let all_stats = cached
        .frame_stats
        .as_ref()
        .ok_or("Datos de analisis corruptos")?;
    let total = all_stats.len();
    let num_to_stack = ((total as f32 * percent / 100.0).ceil() as usize).max(1);
    // Contrato comun de cobertura local: el recorte global posterior debe
    // respetar el mismo suelo que la seleccion por AP.
    let per_ap_min_keep = (num_to_stack / 6)
        .clamp(4, 16)
        .min(num_to_stack.max(1));

    let mut global_active_indices = std::collections::HashSet::new();
    global_active_indices.try_reserve(total).map_err(|error| {
        format!("No se pudo reservar la selección global de frames: {error}")
    })?;

    let source_sample_bits = r.sample_bits();
    let source_adu_gain = r.native_sample_to_u16_gain();
    if source_adu_gain > 1.000_1 {
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Normalización radiométrica: fuente nativa de {} bits → ADU16 (ganancia {:.6}).",
                source_sample_bits, source_adu_gain
            ),
        );
    }
    // PHASE 4.0: ELITE V4 - DENSE LOCAL QUALITY (declarations)
    // R13 FUSION: the quarter-res dense quality map is now built INLINE in the
    // stacking loop from the frame buffer the prefetcher already loaded
    // (sc.mono_buf) — the previous implementation RE-READ and RE-DECODED the
    // whole video a second time just to build these maps. Same math, one full
    // IO pass less, and every stacked frame has its map in every pass.
    const DQ_DOWNSCALE: usize = 4;
    let dq_source_w = (w_in / DQ_DOWNSCALE).max(1);
    let dq_source_h = (h_in / DQ_DOWNSCALE).max(1);

    let t_low = target_type.to_lowercase();
    let is_surface_logic = t_low.contains("superficie") || t_low.contains("surface") || t_low.contains("luna") || t_low.contains("sol");

    // LARGE-DISC EARLY DETECTION (BEFORE frame selection): a big lunar disc in
    // Disco must use the PURE per-AP frame selection below — the AS!4 "stack by
    // parts" that makes Superficie sharp. The planetary grid-voting path adds a
    // global-popularity filter that dilutes each AP's local picks (softer
    // stacks on big discs). Detected once from the best analysis frame; small
    // planets keep grid-voting untouched.
    let large_disc = if !is_surface_logic && !custom_points.is_empty() {
        let det_idx = all_stats
            .iter()
            .max_by_key(|f| f.score)
            .map(|f| f.idx)
            .unwrap_or(0);
        let det_raw = r.get_frame(det_idx, r.color_id());
        let mut det = raw_to_u16_buffer(&det_raw, w_in, h_in, r.bpp());
        let px = w_in * h_in;
        if det.len() >= px * 3 {
            // True-color: collapse interleaved RGB to green before measuring.
            for i in 0..px {
                det[i] = det[i * 3 + 1];
            }
            det.truncate(px);
        }
        is_large_lunar_disc(&det, w_in, h_in)
    } else {
        false
    };
    if large_disc && !is_surface_logic {
        category_profile = TargetCategory::PlanetLarge.profile();
    }

    // One contiguous, bit-packed matrix replaces one Vec allocation per frame.
    // The explicit work/RAM preflight fails loudly for configurations that
    // would otherwise run for hours or let the allocator abort the process.
    let quality_passes = if warping_analysis && !custom_points.is_empty() {
        usize::from(!(is_surface_logic || large_disc)) + 1
    } else {
        0
    };
    let mut selection_system = System::new();
    selection_system.refresh_memory();
    let acceptance_budget = ap_selection_memory_budget(
        total,
        custom_points.len(),
        quality_passes,
        selection_system.available_memory(),
    )?;
    let mut frame_acceptance_masks =
        crate::planetary_quality::CompactFrameAcceptance::try_new(
            all_stats.iter().map(|frame| frame.idx),
            total,
            custom_points.len(),
            acceptance_budget,
        )?;
    if quality_passes > 0 {
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Selección AP: matriz compacta {} MB, {} pasada(s), límite explícito de {} M evaluaciones.",
                frame_acceptance_masks.estimated_bytes().div_ceil(1024 * 1024),
                quality_passes,
                MAX_AP_QUALITY_EVALUATIONS / 1_000_000,
            ),
        );
    }

    if warping_analysis && !custom_points.is_empty() {
        // 1. Calculate Per-AP Acceptance (The Secret to Max Sharpness)
        // For surface, we skip regional grid-voting and give each AP its absolute top-tier frames.

        if is_surface_logic || large_disc {
            // PURE PER-AP SELECTION (Ultra Selective) — surface AND large lunar
            // discs: every AP independently stacks its own locally-best frames.
            //
            // ADAPTIVE CUTOFF BY LOCAL SNR: a fixed 0.70 cutoff assumes the AP's
            // scores measure seeing. In faint/low-contrast APs (limb, dark
            // maria, planet terminator) the scores are NOISE — a strict cutoff
            // there rejects frames at random: less SNR, zero sharpness gain.
            // `pearson_informativeness` (local-vs-global score correlation)
            // gates each AP: informative → strict local cutoff (0.70, current
            // behaviour); noisy → rank by GLOBAL score with a relaxed cutoff
            // (0.45) so the AP simply averages the globally best frames. A
            // minimum per-AP count prevents starved, visibly noisier patches.
            let mut g_scores = Vec::new();
            g_scores.try_reserve_exact(total).map_err(|error| {
                format!("No se pudo reservar el ranking global AP: {error}")
            })?;
            g_scores.extend(all_stats.iter().map(|f| f.score as f32));
            let g_best = g_scores.iter().cloned().fold(1.0f32, f32::max);
            for (ap_idx, ap) in custom_points.iter().enumerate() {
                if ap_idx & 0x0f == 0 && job_token.is_cancelled() {
                    return Err("Selección local cancelada o sustituida".into());
                }
                let mut ap_scores = Vec::new();
                ap_scores.try_reserve_exact(total).map_err(|error| {
                    format!("No se pudo reservar el ranking del AP #{ap_idx}: {error}")
                })?;
                for (frame_position, f) in all_stats.iter().enumerate() {
                    if frame_position & 0x0fff == 0 && job_token.is_cancelled() {
                        return Err("Selección local cancelada o sustituida".into());
                    }
                    ap_scores.push(
                        (if let Some(gs) = &f.grid_scores {
                            ap_grid_quality(gs, w_in as f32, h_in as f32, ap)
                        } else {
                            f.score
                        }) as f32,
                    );
                }
                let ap_best = ap_scores.iter().cloned().fold(1.0f32, f32::max);
                let w_info = pearson_informativeness(&ap_scores, &g_scores);
                let cutoff_frac = 0.45 + 0.25 * w_info;

                let mut ranked = Vec::new();
                ranked.try_reserve_exact(total).map_err(|error| {
                    format!("No se pudo reservar la selección del AP #{ap_idx}: {error}")
                })?;
                for (k, f) in all_stats.iter().enumerate() {
                    let blended = w_info * (ap_scores[k] / ap_best)
                        + (1.0 - w_info) * (g_scores[k] / g_best);
                    ranked.push((f.idx, blended));
                }
                let rank_order = |a: &(usize, f32), b: &(usize, f32)| {
                    b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0))
                };
                // Only the best N can be accepted in this branch. Partial
                // selection preserves that exact set while avoiding a full
                // O(frames log frames) sort for every AP.
                if num_to_stack < ranked.len() {
                    ranked.select_nth_unstable_by(num_to_stack, rank_order);
                    ranked.truncate(num_to_stack);
                }
                ranked.sort_unstable_by(rank_order);
                let best_blend = ranked.first().map(|&(_, s)| s).unwrap_or(1.0);
                let ap_cutoff = best_blend * cutoff_frac;

                // Every AP takes its own top N% independently.
                let mut accepted = 0usize;
                for &(idx, s) in ranked.iter().take(num_to_stack) {
                    if s < ap_cutoff && accepted >= per_ap_min_keep {
                        break; // quality cliff reached — but never starve the AP
                    }
                    global_active_indices.insert(idx);
                    frame_acceptance_masks.set_accepted(idx, ap_idx)?;
                    accepted += 1;
                }
            }
        } else {
            // REGIONAL GRID-VOTING (Optimized for Planet)
            // Porting v4 Adaptive Logic to Planets
            let mut frequency_map = std::collections::HashMap::new();
            frequency_map.try_reserve(total).map_err(|error| {
                format!("No se pudo reservar el mapa de votación AP: {error}")
            })?;
            
            // Step 1: Preliminary vote with adaptive quality check
            for (ap_idx, ap) in custom_points.iter().enumerate() {
                if ap_idx & 0x0f == 0 && job_token.is_cancelled() {
                    return Err("Selección local cancelada o sustituida".into());
                }
                let mut ap_stats = Vec::new();
                ap_stats.try_reserve_exact(total).map_err(|error| {
                    format!("No se pudo reservar la votación del AP #{ap_idx}: {error}")
                })?;
                for (frame_position, f) in all_stats.iter().enumerate() {
                    if frame_position & 0x0fff == 0 && job_token.is_cancelled() {
                        return Err("Selección local cancelada o sustituida".into());
                    }
                    let s = if let Some(gs) = &f.grid_scores {
                        ap_grid_quality(gs, w_in as f32, h_in as f32, ap)
                    } else {
                        f.score
                    };
                    ap_stats.push((f.idx, s));
                }
                let rank_order = |a: &(usize, u64), b: &(usize, u64)| {
                    b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0))
                };
                if num_to_stack < ap_stats.len() {
                    ap_stats.select_nth_unstable_by(num_to_stack, rank_order);
                    ap_stats.truncate(num_to_stack);
                }
                ap_stats.sort_unstable_by(rank_order);

                // Quality cutoff for voting. Planetary APs use a softer floor so
                // faint moons/transits are not discarded just because they are local.
                let best_ap_score = if let Some(&(_, s)) = ap_stats.first() { s as f32 } else { 0.0 };
                let ap_cutoff = best_ap_score * if is_surface_logic { 0.60 } else { 0.52 };

                for rank in 0..num_to_stack {
                    if let Some(&(idx, s)) = ap_stats.get(rank) {
                        if (s as f32) < ap_cutoff { break; }
                        *frequency_map.entry(idx).or_insert(0) += 1;
                    }
                }
            }

            let efficiency_factor = if is_surface_logic { 1.6 } else { 2.0 };
            let global_limit = ((total as f32 * percent / 100.0) * efficiency_factor).ceil() as usize;
            let mut hit_list = Vec::new();
            hit_list.try_reserve_exact(frequency_map.len()).map_err(|error| {
                format!("No se pudo reservar el ranking de votos AP: {error}")
            })?;
            hit_list.extend(frequency_map);
            hit_list.sort_unstable_by(|a, b| {
                b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0))
            });
            let mut allowed_indices = std::collections::HashSet::new();
            allowed_indices
                .try_reserve(global_limit.min(hit_list.len()))
                .map_err(|error| format!("No se pudo reservar el filtro global AP: {error}"))?;
            allowed_indices.extend(
                hit_list
                    .into_iter()
                    .take(global_limit)
                    .map(|(idx, _)| idx),
            );

            // Step 2: Final Acceptance with Moon Detection
            for (ap_idx, ap) in custom_points.iter().enumerate() {
                if ap_idx & 0x0f == 0 && job_token.is_cancelled() {
                    return Err("Selección local cancelada o sustituida".into());
                }
                let mut ap_stats = Vec::new();
                ap_stats.try_reserve_exact(total).map_err(|error| {
                    format!("No se pudo reservar la selección final del AP #{ap_idx}: {error}")
                })?;
                for (frame_position, f) in all_stats.iter().enumerate() {
                    if frame_position & 0x0fff == 0 && job_token.is_cancelled() {
                        return Err("Selección local cancelada o sustituida".into());
                    }
                    let s = if let Some(gs) = &f.grid_scores {
                        ap_grid_quality(gs, w_in as f32, h_in as f32, ap)
                    } else {
                        f.score
                    };
                    ap_stats.push((f.idx, s));
                }
                let rank_order = |a: &(usize, u64), b: &(usize, u64)| {
                    b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0))
                };
                let best_ap_score = ap_stats
                    .iter()
                    .map(|&(_, score)| score)
                    .max()
                    .unwrap_or(0) as f32;
                let ap_cutoff = best_ap_score * if is_surface_logic { 0.65 } else { 0.55 };

                // The old loop can only accept the first `per_ap_min_keep`
                // local ranks plus frames admitted by the global vote. Build
                // exactly that union, then sort it; sorting every rejected
                // frame for every AP was the dominant large-grid bottleneck.
                let protected_count = per_ap_min_keep.min(ap_stats.len());
                if protected_count < ap_stats.len() {
                    ap_stats.select_nth_unstable_by(protected_count, rank_order);
                }
                let protected_ids: Vec<usize> = ap_stats
                    .iter()
                    .take(protected_count)
                    .map(|&(idx, _)| idx)
                    .collect();
                let mut eligible = Vec::new();
                eligible
                    .try_reserve_exact(
                        allowed_indices
                            .len()
                            .saturating_add(protected_count)
                            .min(ap_stats.len()),
                    )
                    .map_err(|error| {
                        format!("No se pudo reservar el filtro final del AP #{ap_idx}: {error}")
                    })?;
                eligible.extend(ap_stats.into_iter().filter(|&(idx, score)| {
                    (score as f32) >= ap_cutoff
                        && (allowed_indices.contains(&idx) || protected_ids.contains(&idx))
                }));
                eligible.sort_unstable_by(rank_order);

                let mut accepted_for_this_ap = 0;
                for &(idx, s) in &eligible {
                    // Cobertura local mínima, no una falsa "detección de luna":
                    // el código anterior marcaba TODOS los AP planetarios como
                    // lunas y saltaba el filtro global hasta N/2. Conservamos
                    // sólo el suelo explícito de cada AP; el trim multicoverage
                    // posterior garantiza que esos frames no desaparezcan.
                    let locally_protected = accepted_for_this_ap < per_ap_min_keep;

                    if allowed_indices.contains(&idx) || locally_protected {
                        if (s as f32) < ap_cutoff { break; }

                        global_active_indices.insert(idx);
                        frame_acceptance_masks.set_accepted(idx, ap_idx)?;
                        accepted_for_this_ap += 1;
                    }
                    if accepted_for_this_ap >= num_to_stack { break; }
                }
            }
        }
    } else {
        // GLOBAL SELECTION (Standard)
        let mut sorted_stats = all_stats.clone();
        sorted_stats.sort_by(|a, b| b.score.cmp(&a.score));
        let active: Vec<usize> = sorted_stats
            .iter()
            .take(num_to_stack)
            .map(|f| f.idx)
            .collect();
        for &idx in &active {
            global_active_indices.insert(idx);
            frame_acceptance_masks.set_all_accepted(idx)?;
        }
    }

    // FRAME COUNT CON COBERTURA: la union por AP puede superar el porcentaje.
    // Un `truncate` por score global borraba todos los frames exclusivos de APs
    // tenues (limbo, terminador o lunas) aunque fueran sus mejores mediciones.
    // El recorte ahora construye primero un nucleo multicoverage que conserva el
    // suelo por AP y solo despues rellena por calidad global. Superficie mantiene
    // 1.25x de margen; si el cap es incompatible con la cobertura, gana la
    // cobertura (sin parches subexpuestos/ruidosos).
    let global_active_indices = if global_active_indices.len() > num_to_stack {
        let scored: Vec<(usize, u64)> = all_stats
            .iter()
            .filter(|f| global_active_indices.contains(&f.idx))
            .map(|f| (f.idx, f.score))
            .collect();
        let max_allowed = if is_surface_logic || large_disc {
            (num_to_stack as f32 * 1.25).ceil() as usize
        } else {
            num_to_stack
        };
        let kept = crate::planetary_quality::coverage_aware_frame_trim_compact(
            &scored,
            &frame_acceptance_masks,
            max_allowed,
            per_ap_min_keep,
            || job_token.is_cancelled(),
        )?;
        if kept.len() > max_allowed {
            log_to_front(
                &app,
                "INFO",
                &format!(
                    "Seleccion local: se preservan {} frames (cap {}), porque reducir mas dejaria APs por debajo de {} tomas.",
                    kept.len(), max_allowed, per_ap_min_keep
                ),
            );
        }
        kept
    } else {
        global_active_indices
    };

    let mut active_frames_indices: Vec<usize> = global_active_indices.into_iter().collect();
    active_frames_indices.sort_unstable(); // For Sequential IO

    let mut active_frames_data = Vec::with_capacity(active_frames_indices.len());
    for &idx in &active_frames_indices {
        if let Some(data) = all_stats.iter().find(|f| f.idx == idx) {
            active_frames_data.push(data.clone());
        }
    }
    if active_frames_data.is_empty() {
        return Err(
            "La selección de calidad no produjo frames apilables; aumenta el porcentaje o vuelve a analizar"
                .into(),
        );
    }

    // FIX: best_idx must be the frame with HIGHEST SCORE, not lowest frame index
    let best_idx = active_frames_data
        .iter()
        .min_by(|a, b| planetary_frame_quality_order(a, b))
        .map(|f| f.idx)
        .unwrap_or(0);
    let bpp = r.bpp();
    let color_id = resolved_color_id;
    let is_color_video = r.is_color_for(color_id);
    // MONO BRANCH: grayscale videos are stacked in a single channel end-to-end
    // (no RGB triplication) and expanded to RGB only for the post pipeline.
    let is_mono_stack = !is_color_video;

    // 3. Prepare Master Reference (Low-Noise Stack)

    // OPTIMIZATION: RAM BUFFERING (HashMap Strategy)
    // We load frames into a HashMap<usize, Vec<u16>> for fast access by Index.
    // We load them sequentially to maximize disk speed.
    // This buffer is used BOTH for Master Generation AND Final Stacking (Liquid).

    // Identify needed indices
    // OPTIMIZATION: DYNAMIC RAM BATCHING (Unified Logic)
    let available_ram = planetary_available_memory_snapshot();
    let requested_robust_reference_frames = if double_pass || is_v3 {
        robust_reference_frame_limit(
            active_frames_data.len(),
            is_v3,
            is_small_planet(&target_type),
        )
    } else {
        0
    };
    let hw_threads = rayon::current_num_threads();
    let ram_plan = plan_planetary_ram(PlanetaryRamInputs {
        available_ram,
        width_in: w_in,
        height_in: h_in,
        width_out: planned_w_out,
        height_out: planned_h_out,
        source_bytes_per_pixel: bpp,
        is_color: is_color_video,
        double_pass,
        use_warp_map: !custom_points.is_empty(),
        surface_or_large_disc: is_surface_logic || large_disc,
        robust_reference_frames: requested_robust_reference_frames,
        hardware_threads: hw_threads,
    })?;
    let stack_threads = ram_plan.stack_threads;
    let frames_per_batch = ram_plan.frames_per_batch;
    let robust_reference_frames = ram_plan.robust_reference_frames;

    if robust_reference_frames < requested_robust_reference_frames {
        log_to_front(
            &app,
            "WARN",
            &format!(
                "Referencia robusta adaptada a RAM: {} de {} frames. Se conserva el mejor frame como respaldo para todo píxel sin cobertura.",
                robust_reference_frames, requested_robust_reference_frames
            ),
        );
    }

    let total_active = active_frames_data.len();
    let total_batches = (total_active + frames_per_batch - 1) / frames_per_batch;

    emit_progress(
        &app,
        &format!(
            "Modo Dinámico: {} GB RAM libres → lotes de {} frames ({}/{} total)",
            available_ram / 1024 / 1024 / 1024,
            frames_per_batch,
            total_batches,
            total_active
        ),
        10.0,
        Some(format!(
            "Aceleracion: {} · apilado {} de {} hilos · pico RAM planificado {} MB (referencia {} MB){}",
            get_accel_label(),
            stack_threads,
            hw_threads,
            ram_plan.estimated_stack_peak_bytes / (1024 * 1024),
            ram_plan.robust_reference_peak_bytes / (1024 * 1024),
            // FFmpeg decodes in ITS OWN process with its own threads — Task
            // Manager shows those cores busy on top of the stacking pool.
            if r.is_ffmpeg() { " + decodificador FFmpeg" } else { "" }
        )),
    );

    // --- STEP 3: MASTER REFERENCE GENERATION ---
    emit_progress(&app, "Generando Referencia Maestra...", 12.0, None);

    // best_idx already correctly computed above as the frame with HIGHEST SCORE

    // El lector ya fue abierto y validado al iniciar el apilado. Reabrirlo aquí
    // repetía FFprobe (y en contenedores difíciles, el conteo de paquetes) justo
    // antes de la referencia maestra sin aportar ninguna geometría nueva.
    let master_source = UnifiedFrameSource::from_input(r.clone(), color_id);
    // En referencia robusta, `best_idx` pertenece al top-N. La implementación
    // anterior recorría el MOV exacto una vez para ese frame y una SEGUNDA vez
    // para el lote top-N. Pedir el lote una sola vez conserva índices absolutos
    // y extrae de él el mejor frame sin clonar sus ~117 MB RGB48.
    let prepared_ref_indices: Vec<usize> = if (double_pass || is_v3)
        && robust_reference_frames > 0
    {
        let mut sorted_by_score = active_frames_data.clone();
        sorted_by_score.sort_by(planetary_frame_quality_order);
        let mut indices: Vec<usize> = sorted_by_score
            .iter()
            .take(robust_reference_frames)
            .map(|frame| frame.idx)
            .collect();
        indices.sort_unstable();
        indices
    } else {
        Vec::new()
    };
    let (master_u16_best, preloaded_reference_batch) = if prepared_ref_indices.is_empty() {
        // Always use absolute decode position. A timestamp seek can be the wrong
        // B/VFR frame and silently poison every downstream offset.
        let master_raw = read_exact_source_frame(&master_source, best_idx)?;
        let master = raw_to_u16_buffer(&master_raw, w_in, h_in, bpp);
        drop(master_raw);
        (master, None)
    } else {
        let batch = {
            let _s = crate::perf_trace::span(pt, "ref_decode")
                .items(prepared_ref_indices.len() as u64);
            // PR-10: caché de decode por-frame PRIMERO — en re-ejecuciones y
            // cachés admitidos la referencia deja de recorrer el códec.
            let cached = if let VideoInput::Ffmpeg(ref fr) = r {
                read_selected_from_decode_cache(
                    &path,
                    &fr.ffmpeg_path,
                    &fr.codec_name,
                    fr.rotation,
                    w_in,
                    h_in,
                    bpp,
                    color_id,
                    &prepared_ref_indices,
                )
            } else {
                None
            };
            match cached {
                Some(frames) => {
                    log_to_front(
                        &app,
                        "SUCCESS",
                        "Referencia robusta servida desde caché NVMe (sin decode).",
                    );
                    crate::frame_source::FrameBatch {
                        descriptor: master_source.descriptor(),
                        roi: None,
                        indices: prepared_ref_indices.clone(),
                        frames,
                    }
                }
                None => master_source.read_batch(&prepared_ref_indices, None)?,
            }
        };
        let best_position = batch
            .indices
            .iter()
            .position(|&index| index == best_idx)
            .ok_or("El lote robusto no contiene el mejor frame")?;
        let master = raw_to_u16_buffer(&batch.frames[best_position], w_in, h_in, bpp);
        (master, Some(batch))
    };
    if job_token.is_cancelled() {
        return Err("Generación de referencia cancelada o sustituida".into());
    }

    let (master_clean_rgb, ref_mean, _ref_std) = if double_pass || is_v3 {
        emit_progress(
            &app,
            if is_v3 { "Generando Referencia Maestra V3 (Alta Precision)..." } else { "Creando Referencia Low-Noise (Doble Pasada)..." },
            13.0,
            None,
        );
        // El mismo límite alimenta el plan de pico RAM y la selección real;
        // así nunca se presupuestan menos frames de los que después se retienen.
        // Prepare anchor for this mini-alignment
        let anchor_mono = reference_frame_to_green(
            &master_u16_best,
            w_in,
            h_in,
            is_color_video,
            color_id,
        )?;
        let anchor_enhanced = enhance_for_alignment(&anchor_mono, w_in, h_in);
        let anchor_pyramid = downscale_integer(&anchor_enhanced, w_in, h_in, 4);

        // COG-ASSIST for the master itself: with handheld motion beyond the
        // SAD window, the reference frames would stack displaced and SMEAR the
        // master — poisoning every downstream alignment. Same centroid prior
        // as the analysis.
        let master_cog_assist = is_compact_object_on_black(&anchor_mono, w_in, h_in);
        let anchor_cog = if master_cog_assist {
            Some(compute_robust_geometric_center(&anchor_mono, w_in, h_in, 0, 0))
        } else {
            None
        };

        // One ascending exact batch: FFmpeg walks 0..max(index) once instead of
        // spawning/seeking once per reference frame. This is both frame-exact
        // for VFR/B-frames and faster for the 12–20 frame robust master.
        let ref_pairs: Vec<(usize, Vec<u8>)> = preloaded_reference_batch
            .into_iter()
            .flat_map(|batch| batch.indices.into_iter().zip(batch.frames))
            .collect();
        // PR-24: alinear los frames de referencia EN PARALELO — cada uno solo
        // depende del ancla compartida (bit-exacto por construcción). La
        // concurrencia se acota a ~4 para que el pico transitorio raw+RGB no
        // desborde el plan de RAM en máquinas justas.
        let ref_chunk = ref_pairs.len().div_ceil(4).max(1);
        let ref_data: Vec<(Vec<u16>, (f32, f32))> = ref_pairs
            .into_par_iter()
            .with_min_len(ref_chunk)
            .map(|(_idx, f)| -> Result<(Vec<u16>, (f32, f32)), String> {
                // Cancelacion durante la generacion del master (decodifica y
                // alinea ~20 frames grandes): frame vacio → el guard de slices
                // lo ignora y el check de fase posterior aborta con Err.
                if job_token.is_cancelled() {
                    return Ok((Vec::new(), (0.0f32, 0.0f32)));
                }
                let px_u16 = raw_to_u16_buffer(&f, w_in, h_in, bpp);
                // `f` posee el RAW del lote; sin un drop explícito su buffer
                // puede vivir hasta el final del closure mientras se crean
                // mono/enhanced. Liberarlo aquí mantiene el pico conforme al
                // plan RAM (una conversión transitoria, no RAW+RGB+scratch).
                drop(f);

                // Frame NEGRO (seek FFmpeg fallido u origen truncado): mejor
                // perder un frame de referencia que promediar oscuridad en el
                // máster — el placeholder vacío ya se ignora al acumular.
                if px_u16.iter().all(|&v| v == 0) {
                    return Ok((Vec::new(), (0.0f32, 0.0f32)));
                }

                let px_expanded = reference_frame_to_rgb_owned(
                    px_u16,
                    w_in,
                    h_in,
                    is_color_video,
                    color_id,
                )?;

                let px_mono = px_expanded.chunks(3).map(|p| p[1]).collect::<Vec<u16>>();
                let px_enh = enhance_for_alignment(&px_mono, w_in, h_in);

                let (init_dx, init_dy) = if let Some((acx, acy)) = anchor_cog {
                    let (fcx, fcy) =
                        compute_robust_geometric_center(&px_mono, w_in, h_in, 0, 0);
                    ((fcx - acx).round() as isize, (fcy - acy).round() as isize)
                } else {
                    (0, 0)
                };
                let shift = crate::alignment::find_best_match_sad_pyramid_offset(
                    &anchor_enhanced,
                    &px_enh,
                    &anchor_pyramid,
                    w_in,
                    h_in,
                    w_in / 4,
                    h_in / 4,
                    w_in / 4,
                    h_in / 4,
                    w_in / 2,
                    h_in / 2,
                    32,
                    4,
                    4,
                    init_dx,
                    init_dy,
                );

                Ok((px_expanded, shift))
            })
            .collect::<Result<Vec<_>, String>>()?;

        if job_token.is_cancelled() {
            return Err("Generación de referencia cancelada o sustituida".into());
        }

        // SUBPIXEL MASTER ACCUMULATION: the old integer-rounded shifts
        // (dx.round()) landed every reference frame with up to ±0.5 px of
        // jitter — the master came out convolved with ~1 px of blur, and EVERY
        // per-AP alignment then refined against that slightly soft reference.
        // Bilinear sub-pixel placement keeps the reference crisp (AS!4-style),
        // which sharpens the whole downstream chain for free.
        // PR-1.2: REFERENCIA ROBUSTA. La media pura dejaba pasar cualquier
        // transitorio del top-N (satélite, avión, rayo cósmico, ráfaga de
        // píxeles calientes) directo a la referencia que siembra TODA la
        // alineación posterior. Cada píxel combina ahora las muestras
        // bilineales de los frames de referencia con MEDIA SIGMA-CLIPPED
        // centrada en la MEDIANA (tolerancia 3·1.4826·MAD): SNR ≈ media,
        // transitorios fuera. ref_data ya retiene los frames completos en
        // RAM, así que no hay coste de memoria adicional (scratch por fila).
        let final_ref_len = w_in
            .checked_mul(h_in)
            .and_then(|pixels| pixels.checked_mul(3))
            .ok_or("La referencia RGB excede el espacio direccionable")?;
        // Sembrar TODO el raster con el mejor frame. La combinación robusta
        // sobrescribe únicamente píxeles con cobertura bilineal válida; así
        // bordes, esquinas y huecos parciales nunca quedan negros. El fallback
        // anterior sólo actuaba cuando el lienzo COMPLETO quedaba vacío.
        let mut final_ref_clean = reference_frame_to_rgb(
            &master_u16_best,
            w_in,
            h_in,
            is_color_video,
            color_id,
        )?;
        debug_assert_eq!(final_ref_clean.len(), final_ref_len);
        let covered = std::sync::atomic::AtomicBool::new(false);
        debug_assert!(ref_data.len() <= MAX_ROBUST_REFERENCE_FRAMES);
        final_ref_clean
            .par_chunks_mut(w_in * 3)
            .enumerate()
            .for_each(|(y, out_row)| {
                if job_token.is_cancelled() {
                    return;
                }
                // Geometría por fila y por frame (syf solo depende de y). Los
                // offsets de ambas filas eliminan dos multiplicaciones y las
                // ramas Option/empty del bucle por píxel.
                let row_refs: Vec<RobustReferenceRow<'_>> = ref_data
                    .iter()
                    .filter_map(|(px_rgb, (dx, dy))| {
                        if px_rgb.is_empty() {
                            return None;
                        }
                        let syf = y as f32 - dy;
                        if syf < 0.0 || syf >= (h_in - 1) as f32 {
                            None
                        } else {
                            let y0 = syf as usize;
                            Some(RobustReferenceRow {
                                pixels: px_rgb,
                                dx: *dx,
                                fy: syf - syf.floor(),
                                row0_offset: y0 * w_in * 3,
                                row1_offset: (y0 + 1) * w_in * 3,
                            })
                        }
                    })
                    .collect();
                let mut vals_r = [0.0f32; MAX_ROBUST_REFERENCE_FRAMES];
                let mut vals_g = [0.0f32; MAX_ROBUST_REFERENCE_FRAMES];
                let mut vals_b = [0.0f32; MAX_ROBUST_REFERENCE_FRAMES];
                let mut row_covered = false;
                for x in 0..w_in {
                    let samples = sample_robust_reference_pixel(
                        &row_refs,
                        x,
                        w_in,
                        &mut vals_r,
                        &mut vals_g,
                        &mut vals_b,
                    );
                    if samples == 0 {
                        continue;
                    }
                    row_covered = true;
                    out_row[x * 3] = (robust_ref_combine(&mut vals_r[..samples]) + 0.5)
                        .clamp(0.0, 65535.0) as u16;
                    out_row[x * 3 + 1] =
                        (robust_ref_combine(&mut vals_g[..samples]) + 0.5)
                            .clamp(0.0, 65535.0) as u16;
                    out_row[x * 3 + 2] =
                        (robust_ref_combine(&mut vals_b[..samples]) + 0.5)
                            .clamp(0.0, 65535.0) as u16;
                }
                if row_covered {
                    covered.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            });
        if job_token.is_cancelled() {
            return Err("Generación de referencia cancelada o sustituida".into());
        }
        let any_ref_frame = covered.load(std::sync::atomic::Ordering::Relaxed);
        // Si TODOS los frames se descartaron, el buffer ya contiene el mejor
        // frame individual. Conservamos el aviso sin volver a asignar otro RGB.
        if !any_ref_frame {
            log_to_front(
                &app,
                "WARN",
                "Máster multi-frame sin datos (frames de referencia vacíos); usando el mejor frame individual como referencia.",
            );
        }

        // Calculate Clean Statistics BEFORE boost
        let (mean, std) = {
            let mut sum = 0.0;
            let mut sum_sq = 0.0;
            let mut count = 0.0;
            for &v in &final_ref_clean {
                let vf = v as f32;
                sum += vf;
                sum_sq += vf * vf;
                count += 1.0;
            }
            let m = if count > 0.0 { sum / count } else { 0.0 };
            let v = if count > 0.0 { (sum_sq / count) - (m * m) } else { 0.0 };
            (m, v.max(0.0).sqrt())
        };

        (final_ref_clean, mean, std)
    } else {
        // Standard Single-Pass Reference
        let final_ref_clean = reference_frame_to_rgb(
            &master_u16_best,
            w_in,
            h_in,
            is_color_video,
            color_id,
        )?;

        let (mean, std) = {
            let mut sum = 0.0;
            let mut sum_sq = 0.0;
            let mut count = 0.0;
            for &v in &final_ref_clean {
                let vf = v as f32;
                sum += vf;
                sum_sq += vf * vf;
                count += 1.0;
            }
            let m = if count > 0.0 { sum / count } else { 0.0 };
            let v = if count > 0.0 { (sum_sq / count) - (m * m) } else { 0.0 };
            (m, v.max(0.0).sqrt())
        };
        (final_ref_clean, mean, std)
    };
    drop(master_u16_best);

    // --- STEP 3b: Alignment reference ---
    // AS!4-style: the alignment reference is the LOW-NOISE STACK of the best
    // frames, not a single frame. A single frame carries its own seeing
    // distortion (every AP then warps toward that frame's deformation) and its
    // pixel noise directly degrades SAD sub-pixel fits. The averaged reference
    // has ~sqrt(N) less noise and represents the mean (true) geometry, so the
    // per-AP shifts converge to the undistorted Sun/Moon.
    // `mut`: the true two-pass mode rebuilds this reference from the first
    // pass's stacked result.
    let mut master_mono = master_clean_rgb
        .chunks(3)
        .map(|pixel| pixel[1])
        .collect::<Vec<u16>>();

    let master_cog = if !is_surface_logic {
        // FIX COG: Use 15% of frame peak (not 55% of mean) for robust disk detection.
        // For small/faint planets (Mars, Uranus) ref_mean can be near-zero causing threshold=256
        // which captures noise. Peak-based threshold correctly brackets the disk.
        let planet_peak = master_mono.iter().copied().max().unwrap_or(2000) as f32;
        let planet_cog_threshold = (planet_peak * 0.15).max(512.0) as u16;
        crate::alignment::calculate_center_of_gravity(&master_mono, w_in, h_in, planet_cog_threshold)
    } else {
        None
    };


    // Extract mono edges for alignment
    // SURFACE FIX (washed detail): the per-frame edge maps are built with
    // enhance_for_alignment() only. The master MUST go through the exact same
    // pipeline — any asymmetric pre-boost (micro-contrast) shifts the SAD
    // minimum and degrades sub-pixel accuracy, blurring the stacked texture.
    // Surface uses a gentler high-pass (4×): solar granulation is low-contrast
    // and 6× amplifies frame noise into the SAD surface, costing sub-pixel
    // precision. Master and frames always share the same amount.
    // R12: SYMMETRIC for planets too. The micro-contrast pre-boost was only
    // applied to the master (frames go through enhance_for_alignment alone) —
    // the ground-truth harness proved for surface that ANY asymmetric
    // preprocessing shifts the SAD minimum and costs sub-pixel accuracy.
    // Master and frames now share the exact same pipeline in every category.
    // Large lunar discs use the surface enhancement amount: 6× over-boosts a
    // texture-rich disc (amplified noise → false SAD minima → soft warp).
    let align_amount: f32 = if is_surface_logic || large_disc { 4.0 } else { 6.0 };
    let mut master_edges =
        enhance_for_alignment_with_amount(&master_mono, w_in, h_in, align_amount);

    // Pre-downscale master edges for pyramidal search
    let mut master_ds_buf = Vec::new();
    let (mut master_ds_w, _master_ds_h) = downscale_4x(&master_edges, w_in, h_in, &mut master_ds_buf);
    
    // --- STEP 4: PREPARE STACKING ---
    // Elite V4 / Precision V3: Always allow Multi-point alignment if APs are present.
    // This is critical for capturing moons (e.g. Ganymede, Io) which move independently.
    let use_liquid = !custom_points.is_empty();
    if !use_liquid {
        emit_progress(
            &app,
            "WARN: Sin Puntos AP. Usando Global Stacking...",
            0.0,
            None,
        );
    }

    let (ref_dx, ref_dy) = active_frames_data
        .iter()
        .find(|f| f.idx == best_idx)
        .map(|f| (f.x_shift, f.y_shift))
        .unwrap_or((0.0, 0.0));

    let (roi_offset_x, roi_offset_y) = match stacking_roi.as_deref() {
        Some([x, y, _, _]) => (*x as f32, *y as f32),
        Some(_) => unreachable!("la ROI se validó antes de abrir el análisis"),
        None => (0.0, 0.0),
    };
    let (mut w_out, mut h_out) = (planned_w_out, planned_h_out);
    // El mapa que consume el acumulador vive en coordenadas del raster de
    // REFERENCIA (ROI), no en todo el sensor. Asi `dq_w / w_out` sigue siendo
    // correcto tambien con ROI+drizzle; el recorte del mapa fuente se hace por
    // frame mas abajo y el shift global queda como un offset independiente.
    let dq_w = (((w_out as f64 / drizzle.max(1e-6) as f64)
        / DQ_DOWNSCALE as f64)
        .floor() as usize)
        .max(1);
    let dq_h = (((h_out as f64 / drizzle.max(1e-6) as f64)
        / DQ_DOWNSCALE as f64)
        .floor() as usize)
        .max(1);

    // let acc_r = vec![0.0f32; w_out * h_out];
    // let acc_g = vec![0.0f32; w_out * h_out];
    // let acc_b = vec![0.0f32; w_out * h_out];
    // let acc_w = vec![0.0f32; w_out * h_out];

    // AP signal validity: keep true low-SNR targets (moons/transits) but reject
    // pure sky APs that create false local vectors and ghost edges.
    // Each AP also gets a sky_ratio (fraction of TRUE background in its box)
    // so limb-straddling APs are damped without distorting the warp field.
    //
    // SKY vs DARK-FEATURE DISCRIMINATION: a plain brightness threshold cannot
    // tell background sky apart from dark filaments / sunspot umbrae — and
    // misclassifying a filament AP as "limb" used to kill its tangential
    // alignment (washed filaments). True sky is connected to the frame
    // border: compute_surface_signal_mask_f32 isolates exactly that
    // (1.0 = disk/feature, 0.0 = border-connected background).
    // LARGE LUNAR DISC: a Moon stacked in the Disco category is a bright disc on
    // black sky, so it needs the LIMB-DOT protections (edge-normal projection +
    // limb AP damping) that are otherwise surface-only. Without them the sharp,
    // texture-aligned Moon shows periodic limb scallops/dots (aperture problem of
    // APs straddling the sky/disk boundary). Primary source: the EARLY detection
    // (best analysis frame, before frame selection). ROBUSTNESS: for FFmpeg
    // inputs get_frame can return a black buffer on a failed seek/spawn — the
    // early detection would silently come back false and shut down the whole
    // large-disc path (dark + blurry stack). The master mono is built
    // unconditionally from frames that demonstrably decoded, so OR-ing it in
    // guarantees the flag even if the early probe failed. (Only the frame
    // selection upstream depends on the early value alone.)
    let large_disc = (large_disc
        || (!is_surface_logic && is_large_lunar_disc(&master_mono, w_in, h_in)))
        && use_liquid;
    // Drives ONLY the limb-dot mitigations (NOT normalization, cutoffs, the
    // outlier filter, etc.) so the planetary look/behaviour is preserved.
    let limb_protect = is_surface_logic || large_disc;

    let surface_sky_mask: Option<Vec<f32>> = if use_liquid && is_surface_logic {
        let luma: Vec<f32> = master_mono.iter().map(|&v| v as f32).collect();
        let m = compute_border_connected_sky_mask(&luma, w_in, h_in, 10);
        if m.iter().all(|&v| v >= 0.999) {
            None // target fills the frame: no sky → no limb handling needed
        } else {
            Some(m)
        }
    } else {
        None
    };

    let (ap_signal_valid, ap_dark_ratio): (Vec<bool>, Vec<f32>) = if use_liquid {
        let max_v = master_mono.iter().copied().max().unwrap_or(1);
        let sky_thresh = if is_surface_logic { max_v / 5 } else { max_v / 8 };

        let mut valids = Vec::with_capacity(custom_points.len());
        let mut darks = Vec::with_capacity(custom_points.len());
        for ap in custom_points.iter() {
            let half = (ap_size / 2) as i32;
            let ax = ap.x as i32;
            let ay = ap.y as i32;
            let y0 = (ay - half).max(0) as usize;
            let y1 = ((ay + half) as usize).min(h_in);
            let x0 = (ax - half).max(0) as usize;
            let x1 = ((ax + half) as usize).min(w_in);

            if is_surface_logic {
                // Surface: sky fraction from the border-connected mask only.
                // Dark filaments/umbrae score mask=1.0 → keep full 2-D matching.
                let (mut sky, mut total) = (0u32, 0u32);
                if let Some(mask) = &surface_sky_mask {
                    for yy in (y0..y1).step_by(3) {
                        let row = yy * w_in;
                        for xx in (x0..x1).step_by(3) {
                            total += 1;
                            if mask[row + xx] < 0.5 {
                                sky += 1;
                            }
                        }
                    }
                }
                let sky_ratio = if total > 0 { sky as f32 / total as f32 } else { 0.0 };
                valids.push(sky_ratio < 0.55);
                darks.push(sky_ratio);
                continue;
            }

            // Planetary: brightness statistics (disk over black sky).
            let mut dark = 0u32;
            let mut total = 0u32;
            let mut sum = 0.0f32;
            let mut sum_sq = 0.0f32;
            let mut local_max = 0u16;
            for yy in (y0..y1).step_by(3) {
                let row = yy * w_in;
                for xx in (x0..x1).step_by(3) {
                    let v = master_mono[row + xx];
                    let vf = v as f32;
                    total += 1;
                    sum += vf;
                    sum_sq += vf * vf;
                    if v > local_max { local_max = v; }
                    if v < sky_thresh { dark += 1; }
                }
            }
            if total == 0 {
                valids.push(false);
                darks.push(1.0);
                continue;
            }
            let mean = sum / total as f32;
            let var = (sum_sq / total as f32) - mean * mean;
            let std = var.max(0.0).sqrt();
            let dark_ratio = dark as f32 / total as f32;
            let peak_signal = local_max as f32 - sky_thresh as f32;
            valids.push(dark_ratio < 0.995 && (peak_signal > 32.0 || std > 4.0));
            darks.push(dark_ratio);
        }
        (valids, darks)
    } else {
        (Vec::new(), Vec::new())
    };
    // La mascara de cielo a resolucion completa solo sirve para clasificar APs.
    // Soltarla antes de construir el warp evita retener otro plano f32 (~74 MiB
    // a 3312x5888) durante las dos pasadas de superficie.
    drop(surface_sky_mask);

    // SURFACE FIX (limb dots): physically REMOVE invalid (sky) APs so they can
    // neither occupy IDW top-K slots nor create global-fallback discontinuities
    // around their centers. Pixels they covered now blend smoothly from the
    // nearest valid disk APs (or the global shift via spatial filtering).
    let (custom_points, frame_acceptance_masks, ap_signal_valid, ap_dark_ratio) =
        if use_liquid && limb_protect && ap_signal_valid.iter().any(|&v| !v) {
            let keep: Vec<usize> = (0..custom_points.len())
                .filter(|&i| ap_signal_valid[i])
                .collect();
            if keep.len() >= 3 {
                let new_points: Vec<ApPoint> =
                    keep.iter().map(|&i| custom_points[i].clone()).collect();
                frame_acceptance_masks.remap_aps_in_place(&keep)?;
                let new_dark: Vec<f32> = keep.iter().map(|&i| ap_dark_ratio[i]).collect();
                (
                    new_points,
                    frame_acceptance_masks,
                    vec![true; keep.len()],
                    new_dark,
                )
            } else {
                (custom_points, frame_acceptance_masks, ap_signal_valid, ap_dark_ratio)
            }
        } else {
            (custom_points, frame_acceptance_masks, ap_signal_valid, ap_dark_ratio)
        };

    // Limb APs (partially over sky) keep reduced influence on the warp field.
    let ap_quality_weights: Vec<f32> = if limb_protect {
        ap_dark_ratio
            .iter()
            .zip(ap_signal_valid.iter())
            .map(|(&d, &v)| if !v { 0.08 } else { (1.0 - d * 0.85).clamp(0.15, 1.0) })
            .collect()
    } else {
        ap_signal_valid
            .iter()
            .map(|&valid| if valid { 1.0 } else { 0.08 })
            .collect()
    };

    // LIMB APERTURE FIX: APs that straddle the limb measure their shift mostly
    // from a 1-D edge — SAD is nearly flat ALONG the limb, so the tangential
    // component is noise. It produced the periodic dot/scallop artifacts at the
    // sky/disk boundary. We precompute each limb AP's edge-normal direction
    // (gradient of the smoothed master) and later keep only the radial
    // component of its measured shift.
    //
    // EXTENDIDO A PLANETAS PEQUEÑOS (reporte del usuario: "manchas en bordes de
    // planetas"): antes solo corría con limb_protect (superficie/disco grande),
    // dejando los APs de limbo de un planeta con matching 2-D → el mismo
    // aperture-problem → puntitos en el borde. Ahora corre para CUALQUIER
    // objeto con APs. Es QUIRURGICO: solo proyecta APs que cruzan el borde
    // disco/cielo (dark_ratio ≥ 0.08); el interior del disco (bandas, detalle
    // real) devuelve None → matching 2-D completo, look planetario intacto.
    // Los rasgos oscuros del disco (bandas de Júpiter, albedo marciano) están
    // MUY por encima de sky_thresh (max/8) → dark_ratio ≈ 0 → nunca se tocan.
    let ap_limb_normal: Vec<Option<(f32, f32)>> = if use_liquid {
        custom_points
            .iter()
            .zip(ap_dark_ratio.iter())
            .map(|(ap, &d)| {
                if d < 0.08 {
                    return None; // fully on the disk: full 2-D matching is fine
                }
                let x = (ap.x as usize).clamp(8, w_in.saturating_sub(9));
                let y = (ap.y as usize).clamp(8, h_in.saturating_sub(9));
                let s = 6usize;
                let mut gx = 0.0f32;
                let mut gy = 0.0f32;
                for oy in -2i32..=2 {
                    for ox in -2i32..=2 {
                        let cx = (x as i32 + ox) as usize;
                        let cy = (y as i32 + oy) as usize;
                        gx += master_mono[cy * w_in + cx + s] as f32
                            - master_mono[cy * w_in + cx - s] as f32;
                        gy += master_mono[(cy + s) * w_in + cx] as f32
                            - master_mono[(cy - s) * w_in + cx] as f32;
                    }
                }
                let norm = (gx * gx + gy * gy).sqrt();
                if norm < 1.0 {
                    None
                } else {
                    Some((gx / norm, gy / norm))
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    // --- OPT 1+B: PRE-COMPUTE TOP-K IDW WARP MAP (on the FILTERED AP set) ---
    // Surface uses K=4 nearest APs: with the dense overlapping grid this makes
    // the warp field follow individual seeing cells instead of low-passing
    // them across ~8 AP spans (sharper granulation/filaments). Planets keep
    // K=8 (sparser grids need the smoothing).
    // Large lunar discs use the surface IDW tuning: with a dense grid over the
    // disc, K=4 keeps the warp local (follows seeing cells → sharper texture);
    // K=8 stays for sparse small-planet grids that need the smoothing.
    let idw_power_v3 = if is_surface_logic || large_disc { 1.55 } else { 1.75 };
    let idw_top_k = if is_surface_logic || large_disc { 4 } else { 8 };
    let (warp_indices, warp_weights) = compute_idw_map_for_output(
        w_out,
        h_out,
        drizzle,
        roi_offset_x,
        roi_offset_y,
        &custom_points,
        idw_power_v3,
        idw_top_k,
    )?;

    // Phase-boundary cancel check: master generation and the IDW map are the
    // two long pre-stacking stages — abort here instead of starting the passes.
    if job_token.is_cancelled() {
        return Err("Cancelado por el usuario".into());
    }

    // --- STEP 5: BATCH PROCESSING LOOP ---
    // GLOBAL PROGRESS COUNTER
    let global_align_counter = std::sync::atomic::AtomicUsize::new(0);

    // Extract ffmpeg_cmd if applicable, for use in pre-loader thread
    let ffmpeg_cmd = if let VideoInput::Ffmpeg(ref fr) = r {
        fr.ffmpeg_path.clone()
    } else {
        "ffmpeg".to_string()
    };

    // Create a list of just indices for the pre-loader
    let active_frames_indices: Vec<usize> = active_frames_data.iter().map(|f| f.idx).collect();

    // --- PROACTIVE PRE-FETCHER (Elite IO) ---
    // Spawns a loader thread that feeds frame batches into RAM while the
    // current batch is being processed. Re-callable: the true two-pass mode
    // streams the whole selection twice (the LZ4 batch cache makes the second
    // pass mostly disk-bound reads).
    let pre_chunks_template: Vec<Vec<usize>> = active_frames_indices
        .chunks(frames_per_batch)
        .map(|c| c.to_vec())
        .collect();
    // Telemetria: lotes servidos desde el cache de decode (sin re-decodificar).
    let tele_cache_hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Si hardware falla, el intento entero se descarta y la misma pasada se
    // reinicia por CPU. Estas banderas coordinan el prefetcher detached con el
    // bucle dueño del acumulador sin mezclar rutas dentro de un resultado.
    let ffmpeg_force_cpu_decode =
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ffmpeg_decode_route_hardware =
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let spawn_prefetcher = |pass_label: String|
        -> std::sync::mpsc::Receiver<
            Result<std::collections::HashMap<usize, Vec<u16>>, String>,
        > {
        let (tx, rx) = std::sync::mpsc::sync_channel::<
            Result<std::collections::HashMap<usize, Vec<u16>>, String>,
        >(1);
        let pre_path = path.clone();
        let pre_app = app.clone();
        let pre_chunks = pre_chunks_template.clone();
        let pre_total_batches = total_batches;
        let pre_w = w_in;
        let pre_h = h_in;
        let pre_bpp = bpp;
        let pre_color_id = color_id;
        let pre_ffmpeg_cmd = ffmpeg_cmd.clone();
        let pre_r_template = r.clone();
        let pre_cancel = job_token.clone();
        let pre_cache_hits = tele_cache_hits.clone();
        let pre_force_cpu_decode = ffmpeg_force_cpu_decode.clone();
        let pre_decode_route_hardware = ffmpeg_decode_route_hardware.clone();

        std::thread::spawn(move || {
            let _ = &pre_ffmpeg_cmd;
            if let VideoInput::Ffmpeg(ref fr) = pre_r_template {
                // PERSISTENT STREAM: one sequential decode pass feeds every
                // batch of this stacking pass (batches are ascending). The old
                // per-batch loader re-decoded from frame 0 for each batch —
                // O(N²) work that dominated wall time on compressed videos.
                if let Err(error) = stream_frames_ffmpeg_chunked(
                    fr,
                    &pre_path,
                    &pre_app,
                    &pre_chunks,
                    &tx,
                    pre_w,
                    pre_h,
                    pre_bpp,
                    pre_color_id,
                    &pass_label,
                    pre_total_batches,
                    &pre_cancel,
                    &pre_cache_hits,
                    pre_force_cpu_decode.load(Ordering::Acquire),
                    &pre_decode_route_hardware,
                ) {
                    let _ = tx.send(Err(error));
                }
                return;
            }

            // PR-11: lectores nativos PERSISTENTES por pasada. El diseño
            // anterior abría un VideoInput por GRUPO y por LOTE (hasta 8 ×
            // n_lotes × 2 pasadas reaperturas ≈ 300 en un SER típico, cada una
            // re-parseando cabecera + escaneando el trailer de timestamps).
            // Ahora n_io lectores se abren UNA vez y sirven todos los lotes de
            // la pasada; los grupos vacíos del último lote ya no abren nada.
            let n_io = rayon::current_num_threads().clamp(1, 8);
            let readers: Vec<VideoInput> = match (0..n_io)
                .map(|_| VideoInput::open(&pre_path, &pre_app))
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(readers) => readers,
                Err(error) => {
                    let _ = tx.send(Err(format!(
                        "No se pudo abrir el lector nativo persistente: {error}"
                    )));
                    return;
                }
            };
            for (batch_idx, indices_to_load) in pre_chunks.into_iter().enumerate() {
                if pre_cancel.is_cancelled() {
                    break; // cancelado: no cargar mas lotes
                }
                let load_prefix = format!(
                    "{}Cargando Lote {}/{}",
                    pass_label,
                    batch_idx + 1,
                    pre_total_batches
                );
                let _ = &load_prefix;

                let frame_map = {
                    // Direct Load (SER/AVI/FITS) — PARALLEL.
                    // Was a single-threaded for-loop: on a multicore box the whole
                    // selection loaded serially at ~1% CPU BEFORE any stacking ran
                    // (the parallel stacker then sat idle waiting on rx.recv()).
                    // mmap-backed readers scale across cores; cada slot par usa SU
                    // lector persistente (zip 1:1, sin aliasing de estado). IO
                    // fan-out is capped so it overlaps with — instead of starving —
                    // the stacking pass that runs on the same rayon pool.
                    let mut groups: Vec<Vec<usize>> = (0..n_io).map(|_| Vec::new()).collect();
                    for (k, &idx) in indices_to_load.iter().enumerate() {
                        groups[k % n_io].push(idx);
                    }
                    groups
                        .par_iter()
                        .zip(readers.par_iter())
                        .map(|(group, r_local)| {
                            let mut m =
                                std::collections::HashMap::with_capacity(group.len());
                            for &idx in group {
                                // SER cancel: without this, cancelling mid-load
                                // waited for the whole batch to finish reading.
                                if pre_cancel.is_cancelled() {
                                    break;
                                }
                                let raw = r_local.get_frame(idx, pre_color_id);
                                if !raw.is_empty() {
                                    m.insert(
                                        idx,
                                        raw_to_u16_buffer(&raw, pre_w, pre_h, pre_bpp),
                                    );
                                }
                            }
                            m
                        })
                        .reduce(
                            std::collections::HashMap::new,
                            |mut a, b| {
                                a.extend(b);
                                a
                            },
                        )
                };
                if frame_map.len() != indices_to_load.len()
                    || indices_to_load.iter().any(|idx| !frame_map.contains_key(idx))
                {
                    let _ = tx.send(Err(format!(
                        "El lector nativo entregó un lote incompleto: {}/{} frames",
                        frame_map.len(),
                        indices_to_load.len()
                    )));
                    return;
                }
                if tx.send(Ok(frame_map)).is_err() {
                    break;
                }
            }
        });
        rx
    };

    // --- STEP 4b: V3 AP PRE-STATS (Scintillation Normalization) ---
    let _ap_ref_stats: Vec<(f32, f32)> = if is_v3 && use_liquid {
        // Use clean master for AP stats
        custom_points.iter().map(|ap| {
            let half = (ap_size / 2) as i32;
            let ax = ap.x as i32;
            let ay = ap.y as i32;
            let mut sum = 0.0f32;
            let mut sum_sq = 0.0f32;
            let mut count = 0.0f32;
            for yy in (ay - half)..=(ay + half) {
                if yy < 0 || yy >= h_in as i32 { continue; }
                let row = yy as usize * w_in;
                for xx in (ax - half)..=(ax + half) {
                    if xx < 0 || xx >= w_in as i32 { continue; }
                    let v = master_mono[row + xx as usize] as f32;
                    sum += v;
                    sum_sq += v * v;
                    count += 1.0;
                }
            }
            if count > 0.0 {
                let mean = sum / count;
                let var = (sum_sq / count) - (mean * mean);
                (mean, var.max(1.0).sqrt())
            } else {
                (0.0, 1.0)
            }
        }).collect()
    } else {
        Vec::new()
    };
 
    let mut sorted_all = active_frames_data.clone();
    sorted_all.sort_by(planetary_frame_quality_order);
    // Peso por CALIDAD medida. El ranking ordinal amplificaba ruido: dos
    // frames con score idéntico podían acabar en 1.0 y 0.1 sólo por su orden.
    // La sigmoide preserva igualdad, es monotona y usa el perfil del objetivo.
    // La referencia P95..P99 winsorizada evita que un unico frame con score
    // espurio comprima todos los pesos sanos hacia 0.1.
    let quality_scores: Vec<u64> = sorted_all.iter().map(|f| f.score).collect();
    let robust_quality_score =
        crate::planetary_quality::robust_quality_reference(&quality_scores);
    let quality_weight_by_idx: std::collections::HashMap<usize, f32> = sorted_all
        .iter()
        .map(|f| {
            (
                f.idx,
                crate::planetary_quality::sigmoidal_frame_weight(
                    f.score,
                    robust_quality_score,
                    category_profile.rejection_percentile,
                    category_profile.sigmoid_steepness,
                ),
            )
        })
        .collect();
    let _lucky_threshold_v3 = if !sorted_all.is_empty() {
        let limit_idx = (num_to_stack.saturating_sub(1)).min(sorted_all.len().saturating_sub(1));
        sorted_all[limit_idx].score as f32
    } else {
        0.0
    };

    // Per-frame exposure matching reference — also for large lunar discs:
    // without it, transparency/exposure variations between frames average into
    // a dimmer, lower-contrast (blurrier-looking) stack. Surface always had it.
    let surface_ref_p90 = if is_surface_logic || large_disc {
        surface_luma_percentile_rgb(&master_clean_rgb, 90)
    } else {
        // PR-1.4: los planetas también se normalizan per-frame, pero el
        // percentil se mide SOLO en el disco (el p90 global de un planeta
        // pequeño cae en el cielo negro).
        planetary_disc_luma_percentile_rgb(&master_clean_rgb, 90)
    };
    
    // 6. ELITE V4 ACCUMULATION (MULTI-PASS CAPABLE)
    // TRUE TWO-PASS (AS!4 "stack reference"): with Doble Pasada active on
    // surface targets, pass 1 stacks against the low-noise averaged master and
    // pass 2 re-aligns every AP against the PASS-1 STACK itself — a reference
    // with both high SNR and true mean geometry. Restricted to drizzle 1× and
    // full-frame output so the stack lives in input coordinates.
    // R11: planets earn the true two-pass too (pass 2 re-aligns against the
    // pass-1 stack — pass 2 is even SYMMETRIC for planets, since the rebuilt
    // master skips the micro-contrast pre-boost of pass 1).
    // PR-1.3: la doble pasada (y con ella la rejection kappa-sigma) se
    // habilita en TODOS los modos. La restricción histórica a drizzle 1× y
    // sin ROI existía porque la referencia de la pasada 2 se reconstruía
    // asumiendo lienzo de salida == geometría de entrada; el rebuild ahora
    // mapea el lienzo (drizzle/ROI) de vuelta a coordenadas de entrada. Los
    // modos drizzle 1.5×/3× — el caso estrella de resolución — apilaban
    // SIEMPRE en pasada única y sin rechazo de outliers.
    let total_passes = if double_pass { 2 } else { 1 };

    // RAM: direct-only stackers (Poisson reconstruction is disabled in this
    // path) and channel-less R/B accumulators for mono videos.
    let mut acc_grad_r = GradientDomainStacker::new_empty();
    let mut acc_grad_g = GradientDomainStacker::new_empty();
    let mut acc_grad_b = GradientDomainStacker::new_empty();

    // KAPPA-SIGMA REJECTION (AS!4-grade robustness at high stack %): pass 1
    // additionally tracks the per-pixel second moment; between passes we build
    // per-pixel winsorization bounds (mean ± k·σ) and pass 2 clamps every
    // frame contribution to them. Transient artifacts (satellites, birds,
    // dust, compression glitches) are statistical outliers at their pixels and
    // get clamped to plausible values; real detail lives inside ±kσ of the
    // seeing distribution and passes untouched. PR-1.3: activa con la doble
    // pasada en CUALQUIER modo (drizzle/ROI incluidos; las estadísticas y los
    // bounds viven en el lienzo de salida, que es idéntico en ambas pasadas).
    let sigma_clip_enabled = total_passes == 2;
    let sigma_clip_k = category_profile.kappa_sigma.clamp(2.0, 6.0);
    const SIGMA_CLIP_FLOOR: f32 = 6.0; // ADU16: guards zero-variance pixels
    let mut clip_r: Option<(Vec<f32>, Vec<f32>)> = None;
    let mut clip_g: Option<(Vec<f32>, Vec<f32>)> = None;
    let mut clip_b: Option<(Vec<f32>, Vec<f32>)> = None;

    // Pool ACOTADO para el apilado: fija el numero de workers a `stack_threads`
    // (calculado por la RAM), asi el scratch por-hilo NO desborda la memoria en
    // equipos de muchos nucleos. No se cae al pool global: podría tener más
    // workers que el plan y violar el límite de RAM recién calculado.
    let stack_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(stack_threads)
        .build()
        .map_err(|error| format!("No se pudo crear el pool planetario acotado: {error}"))?;

    // ================== GPU COMPUTE: DECISION DEL APILADO ==================
    // La etapa de acumulacion (warp + merge + sigma-clip) puede correr en GPU
    // (Metal/DX12/Vulkan via wgpu) mientras la CPU alinea el frame siguiente.
    // Solo se usa si: hay runtime, el canvas amerita el coste fijo de
    // upload/dispatch, los buffers caben en el presupuesto de VRAM y el
    // self-test de PARIDAD numerica GPU-vs-CPU esta en verde. Cualquier "no"
    // se registra con su motivo, y la ruta CPU queda intacta como siempre.
    let drop_size_pass: f32 = if drizzle > 1.01 { 0.75 } else { 1.0 };
    let mut gpu_disabled_this_stack = false;
    // Compatibilidad: durante una version seguimos aceptando los strings
    // historicos, pero toda decision interna usa el contrato ComputePolicy.
    // GpuOnly omite solamente el umbral de rentabilidad: nunca omite paridad,
    // limites del dispositivo ni presupuesto de VRAM.
    let compute_policy = ComputePolicy::from_legacy(gpu_mode.as_deref());
    // La búsqueda SAD gruesa por AP usa buffers 4× mucho menores que la
    // acumulación de lienzo completo. Antes se desactivaba junto con Metal
    // cuando el acumulador no cabía (caso 20 MP), obligando a CPU a explorar
    // toda la ventana de ~1 700 AP por frame. Son decisiones independientes.
    let gpu_sad_enabled = std::sync::atomic::AtomicBool::new(
        compute_policy.allows_gpu() && crate::gpu_analysis::ensure_parity(),
    );
    let gpu_rt: Option<&'static crate::gpu_stack::GpuRuntime> = if !compute_policy.allows_gpu() {
        log_to_front(&app, "INFO", "GPU compute desactivada por ajustes del usuario — acumulacion en CPU.");
        None
    } else {
        use crate::gpu_stack as g;
        let canvas_px = (w_out as u64) * (h_out as u64);
        // Las dos pasadas tienen estados mutuamente excluyentes: la primera
        // acumula M2 SIN bounds; la segunda usa bounds SIN M2. El probe viejo
        // sumaba ambos en una configuracion imposible y descartaba Metal en
        // canvases de 20 MP aunque cada pasada real sí cupiera.
        let probe_pass1 = g::GpuPassConfig {
            w_in,
            h_in,
            w_out,
            h_out,
            is_color: !is_mono_stack,
            track_m2: sigma_clip_enabled,
            use_bounds: false,
            coverage_weighting: drizzle > 1.01,
            true_drizzle: drizzle > 1.01,
            use_warp: use_liquid,
            k: idw_top_k,
            n_aps: custom_points.len(),
            drizzle,
            roi_off_x: roi_offset_x,
            roi_off_y: roi_offset_y,
            drop_size: drop_size_pass,
            global_fallback_weight: 1.0,
            dq_w,
            dq_h,
        };
        let probe_pass2 = sigma_clip_enabled.then(|| g::GpuPassConfig {
            track_m2: false,
            use_bounds: true,
            ..probe_pass1.clone()
        });
        let probe_vram = probe_pass2
            .as_ref()
            .map(|pass2| pass2.vram_needed())
            .unwrap_or(0)
            .max(probe_pass1.vram_needed());
        let probe_binding = probe_pass2
            .as_ref()
            .map(|pass2| pass2.largest_binding())
            .unwrap_or(0)
            .max(probe_pass1.largest_binding());
        match g::gpu_runtime() {
            None => {
                if !compute_policy.allows_fallback() {
                    return Err(
                        "GPU only solicitado, pero no existe un dispositivo wgpu compatible. Cambia a Auto/Hybrid o revisa el controlador grafico."
                            .to_string(),
                    );
                }
                log_to_front(&app, "INFO", "GPU compute: no disponible — acumulacion en CPU (SIMD).");
                None
            }
            Some(_) if canvas_px < 500_000 && !matches!(compute_policy, ComputePolicy::GpuOnly) => {
                log_to_front(
                    &app,
                    "INFO",
                    &format!(
                        "GPU descartada: canvas {:.2} Mpx < umbral (el overhead de upload supera la ganancia) — CPU. (Ajustes → Forzar GPU para ignorar el umbral.)",
                        canvas_px as f64 / 1e6
                    ),
                );
                None
            }
            Some(rt) if probe_binding > rt.max_binding || probe_vram > rt.vram_budget =>
            {
                let reason = format!(
                    "el pase necesita {} MB de VRAM (binding maximo {} MB), pero {} permite {} MB de presupuesto y {} MB por binding",
                    probe_vram / (1024 * 1024),
                    probe_binding / (1024 * 1024),
                    rt.adapter_name,
                    rt.vram_budget / (1024 * 1024),
                    rt.max_binding / (1024 * 1024),
                );
                if !compute_policy.allows_fallback() {
                    return Err(format!("GPU only no puede ejecutar este apilado: {reason}."));
                }
                log_to_front(
                    &app,
                    "INFO",
                    &format!("GPU descartada: {reason} — CPU."),
                );
                None
            }
            Some(rt) => {
                emit_progress(&app, "Verificando paridad GPU-vs-CPU...", 48.0, None);
                if g::ensure_parity() {
                    log_to_front(
                        &app,
                        "SUCCESS",
                        &format!(
                            "GPU {} ({}): ACTIVA para la acumulacion — canvas {:.1} Mpx, VRAM {} MB de {} MB presupuestados. Paridad numerica: OK.",
                            rt.backend,
                            rt.adapter_name,
                            canvas_px as f64 / 1e6,
                            probe_vram / (1024 * 1024),
                            rt.vram_budget / (1024 * 1024)
                        ),
                    );
                    Some(rt)
                } else {
                    if !compute_policy.allows_fallback() {
                        return Err(
                            "GPU only solicitado, pero el self-test de paridad numerica fallo; se cancela para no producir un resultado distinto a CPU."
                                .to_string(),
                        );
                    }
                    log_to_front(&app, "WARN", "GPU deshabilitada: el self-test de paridad numerica no paso — se usa CPU (resultado identico garantizado).");
                    None
                }
            }
        }
    };

    let stack_peak_vram = std::sync::atomic::AtomicU64::new(0);
    for pass in 0..total_passes {
    let pass_label = if total_passes > 1 {
        format!("[Pasada {}/{}] ", pass + 1, total_passes)
    } else {
        String::new()
    };
    emit_progress(&app, &format!("{}Zenith Elite V4: Iniciando Acumulacion Robusta...", pass_label), 50.0, None);
    // REINTENTO DEL PASE: si la GPU falla a mitad (device lost / OOM), el
    // pase COMPLETO se reinicia en CPU — barato porque el cache de decode
    // por-frame re-sirve los frames desde disco, y el resultado es identico
    // al de una corrida 100% CPU (misma seleccion y misma matematica).
    'pass_attempt: loop {
    let use_gpu_pass = gpu_rt.is_some() && !gpu_disabled_this_stack;
    let rx = spawn_prefetcher(pass_label.clone());
    // perf_trace: pase 1-based; el span "total" cubre el intento completo.
    let ptag = (pass + 1) as u8;
    let _pass_span = crate::perf_trace::span_pass(pt, ptag, "total");
    global_align_counter.store(0, std::sync::atomic::Ordering::Relaxed);

    // ===== TELEMETRIA EN VIVO del pase (evento "stack_telemetry") =====
    let tele_pass_start = std::time::Instant::now();
    let tele_align_ns = std::sync::atomic::AtomicU64::new(0);
    let tele_accum_ns = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let tele_upload_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let tele_vram = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let tele_sys = std::sync::Mutex::new(System::new());
    let tele_mode = if use_gpu_pass {
        gpu_rt
            .map(|rt| format!("GPU {} ({})", rt.backend, rt.adapter_name))
            .unwrap_or_else(|| "GPU".to_string())
    } else {
        format!("CPU ({})", get_accel_label())
    };

    // SHARED per-channel accumulators con STRIPING por bandas de filas
    // (StripedAccum): el mutex full-canvas por canal serializaba el merge y
    // limitaba el escalado con muchos nucleos; ahora dos workers solo chocan
    // si mergean la misma banda a la vez. Sigue sin multiplicar la RAM por el
    // numero de hilos (un solo canvas f64 por canal).
    // Pass 1 of a double-pass run tracks the second moment (m2) for the
    // sigma-clip statistics; pass 2 and single-pass runs use the lean variant.
    let track_variance = sigma_clip_enabled && pass == 0;
    let mk_acc = |active: bool| -> Result<GradientDomainStacker, String> {
        if !active {
            Ok(GradientDomainStacker::new_empty())
        } else if track_variance {
            GradientDomainStacker::try_new_direct_tracked(w_out, h_out)
        } else {
            GradientDomainStacker::try_new_direct_only(w_out, h_out)
        }
    };
    // STRIPING: bandas de filas con lock propio (ver StripedAccum). 4 bandas
    // por hilo del pool acotado — suficiente para que la probabilidad de que
    // dos workers pidan la misma banda a la vez sea baja, sin fragmentar el
    // recorrido en trozos tan finos que el overhead de lock domine.
    let n_bands = (stack_threads * 4).clamp(4, 64);
    // Drizzle >1x: la COBERTURA del drop (que viaja en sc.ww desde
    // accumulate_frame_liquid) pondera tambien ENTRE frames — el peso overlap
    // del drizzle clasico. Con drizzle 1x queda apagado: ponderacion historica.
    let coverage_weighting = drizzle > 1.01;
    // En modo GPU los acumuladores CPU quedan VACIOS (0 RAM): el resultado
    // del pase llega por la descarga de la GPU al final.
    let cpu_acc = !use_gpu_pass;
    let acc_r_sh = StripedAccum::new(mk_acc(cpu_acc && !is_mono_stack)?, n_bands);
    let acc_g_sh = StripedAccum::new(mk_acc(cpu_acc)?, n_bands);
    let acc_b_sh = StripedAccum::new(mk_acc(cpu_acc && !is_mono_stack)?, n_bands);
    // Reserva fallible ANTES del paralelo: `vec!` dentro de cada worker podía
    // abortar todo el proceso si el estado de RAM cambió después del plan.
    let mut scratch_buffers = Vec::new();
    scratch_buffers
        .try_reserve_exact(stack_threads)
        .map_err(|error| format!("No se pudo reservar el pool de scratch planetario: {error}"))?;
    for _ in 0..stack_threads {
        scratch_buffers.push(LiquidScratch::try_new(
            w_in,
            h_in,
            w_out,
            h_out,
            is_mono_stack,
            !use_gpu_pass,
        )?);
    }
    let scratch_pool_mx: std::sync::Mutex<Vec<LiquidScratch>> =
        std::sync::Mutex::new(scratch_buffers);

    // ============== SUBMITTER GPU (un hilo posee el acumulador wgpu) ==============
    // Los workers rayon alinean y ENCOLAN jobs (canal acotado = backpressure
    // natural); el submitter sube+despacha; al cerrar el canal descarga los
    // planos y los devuelve por el JoinHandle. Cualquier error marca
    // gpu_failed y el pase se reinicia en CPU.
    let gpu_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gpu_px_pool: std::sync::Arc<std::sync::Mutex<Vec<Vec<u16>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    // Invariante: use_gpu_pass implica runtime. Si se rompiera, degradar a
    // reintento CPU del pase (gpu_failed) — nunca abortar el apilado.
    let gpu_rt_for_pass = if use_gpu_pass {
        if gpu_rt.is_none() {
            eprintln!("[gpu] use_gpu_pass sin runtime wgpu; el pase se reintenta en CPU");
            gpu_failed.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        gpu_rt
    } else {
        None
    };
    let (gpu_tx, gpu_join) = if let Some(rt) = gpu_rt_for_pass {
        let cfg = crate::gpu_stack::GpuPassConfig {
            w_in,
            h_in,
            w_out,
            h_out,
            is_color: !is_mono_stack,
            track_m2: track_variance,
            use_bounds: clip_g.is_some(),
            coverage_weighting,
            true_drizzle: drizzle > 1.01,
            use_warp: use_liquid,
            k: idw_top_k,
            n_aps: custom_points.len(),
            drizzle,
            roi_off_x: roi_offset_x,
            roi_off_y: roi_offset_y,
            drop_size: drop_size_pass,
            global_fallback_weight: 1.0,
            dq_w,
            dq_h,
        };
        match crate::gpu_stack::GpuPassAccumulator::new(rt, cfg, &warp_indices, &warp_weights) {
            Ok(acc) => {
                // Pasada 2: subir los limites de winsorizacion sigma-clip.
                // clip_* ausente = invariante rota: se trata como error del
                // pase GPU (fallback CPU integro), nunca panic en caliente.
                let bounds_res = (|| -> Result<(), String> {
                    if clip_g.is_none() {
                        return Ok(());
                    }
                    let missing =
                        |name: &str| format!("{name} ausente en pasada 2 (invariante sigma-clip)");
                    let chans: Vec<(&[f32], &[f32])> = if !is_mono_stack {
                        let r = clip_r.as_ref().ok_or_else(|| missing("clip_r"))?;
                        let g = clip_g.as_ref().ok_or_else(|| missing("clip_g"))?;
                        let b = clip_b.as_ref().ok_or_else(|| missing("clip_b"))?;
                        vec![
                            (r.0.as_slice(), r.1.as_slice()),
                            (g.0.as_slice(), g.1.as_slice()),
                            (b.0.as_slice(), b.1.as_slice()),
                        ]
                    } else {
                        let g = clip_g.as_ref().ok_or_else(|| missing("clip_g"))?;
                        vec![(g.0.as_slice(), g.1.as_slice())]
                    };
                    acc.set_sigma_bounds(&chans)
                })();
                if let Err(e) = bounds_res {
                    if !compute_policy.allows_fallback() {
                        return Err(format!(
                            "GPU only fallo al subir los limites de sigma-clip: {e}"
                        ));
                    }
                    log_to_front(&app, "WARN", &format!("GPU: fallo subiendo bounds ({e}) — pase en CPU."));
                    gpu_disabled_this_stack = true;
                    continue 'pass_attempt;
                }
                let (tx, rx_jobs) =
                    std::sync::mpsc::sync_channel::<crate::gpu_stack::GpuFrameJob>(3);
                let fail = gpu_failed.clone();
                let pool = gpu_px_pool.clone();
                let cancel2 = job_token.clone();
                tele_vram.store(acc.vram_bytes, std::sync::atomic::Ordering::Relaxed);
                stack_peak_vram.fetch_max(
                    acc.vram_bytes,
                    std::sync::atomic::Ordering::Relaxed,
                );
                let t_accum = tele_accum_ns.clone();
                let t_upload = tele_upload_bytes.clone();
                let handle = std::thread::spawn(move || -> Option<crate::gpu_stack::GpuDownload> {
                    let mut acc = acc;
                    while let Ok(job) = rx_jobs.recv() {
                        if cancel2.is_cancelled()
                            || fail.load(std::sync::atomic::Ordering::Relaxed)
                        {
                            continue; // drenar sin trabajar
                        }
                        let t0 = std::time::Instant::now();
                        let job_bytes = (job.pixels.len() * 2
                            + job.apq.len() * 16
                            + job.dq_map.len() * 4) as u64;
                        match acc.accumulate_frame(&job) {
                            Ok(()) => {
                                t_accum.fetch_add(
                                    t0.elapsed().as_nanos() as u64,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                crate::perf_trace::add_ns(pt, ptag, "gpu_accum", t0.elapsed().as_nanos(), 1);
                                t_upload
                                    .fetch_add(job_bytes, std::sync::atomic::Ordering::Relaxed);
                                // devolver el buffer de pixeles al pool
                                pool.lock().unwrap_or_else(|e| e.into_inner()).push(job.pixels);
                            }
                            Err(e) => {
                                eprintln!("[gpu] fallo acumulando frame: {e}");
                                fail.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    }
                    if fail.load(std::sync::atomic::Ordering::Relaxed)
                        || cancel2.is_cancelled()
                    {
                        return None;
                    }
                    let t_fin = std::time::Instant::now();
                    let out = match acc.finish() {
                        Ok(d) => Some(d),
                        Err(e) => {
                            eprintln!("[gpu] fallo la descarga del pase: {e}");
                            fail.store(true, std::sync::atomic::Ordering::Relaxed);
                            None
                        }
                    };
                    crate::perf_trace::add_ns(pt, ptag, "gpu_finish", t_fin.elapsed().as_nanos(), 1);
                    out
                });
                (Some(tx), Some(handle))
            }
            Err(e) => {
                if !compute_policy.allows_fallback() {
                    return Err(format!(
                        "GPU only no pudo inicializar el acumulador de este pase: {e}"
                    ));
                }
                log_to_front(&app, "WARN", &format!("GPU no disponible para este pase ({e}) — usando CPU."));
                gpu_disabled_this_stack = true;
                continue 'pass_attempt;
            }
        }
    } else {
        (None, None)
    };

    // PERF: master-side AP statistics are constant within a pass — compute
    // once instead of per frame × AP (recomputed per pass: the double-pass
    // rebuilds the master from the pass-1 stack).
    let (ap_master_contrast, ap_master_lap) = {
        let _s = crate::perf_trace::span_pass(pt, (pass + 1) as u8, "ap_master_stats");
        precompute_ap_master_stats(
            &master_edges,
            &master_mono,
            w_in,
            h_in,
            &custom_points,
            ap_size as usize,
        )
    };

    // TEXTURE-TRUST GATE (solo limb_protect: superficie / disco lunar grande):
    // con mallas finas (24px) y umbral bajo, el generador pone APs en zonas SIN
    // textura real (terminador oscuro, cielo residual) — ahi el SAD mide RUIDO
    // y esos miles de vectores caoticos parcheaban el warp (artefactos en
    // bloques junto al limbo/terminador, reporte del usuario con 5882 APs).
    // Un AP cuyo Laplaciano del MASTER es < 8% de la MEDIANA de todos los APs
    // no puede medir seeing: se marca q=0 (sin medicion) → excluido del blend
    // IDW (sus pixeles siguen a los vecinos validos / al shift global) y se
    // AHORRA todo su coste por frame (SAD fino + LK). Con la malla 32px
    // recomendada apenas hay APs bajo el umbral: comportamiento intacto.
    let ap_lap_floor: f32 = if limb_protect && !ap_master_lap.is_empty() {
        let mut sorted_lap = ap_master_lap.clone();
        sorted_lap.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        (sorted_lap[sorted_lap.len() / 2] * 0.08).max(1.0)
    } else {
        0.0
    };

    let mut prefetch_error: Option<String> = None;
    let mut worker_error: Option<String> = None;
    for (_batch_idx, chunk) in active_frames_data.chunks(frames_per_batch).enumerate() {
        if job_token.is_cancelled() {
            break; // cancelado por el usuario
        }
        // recv con timeout: CANCELAR debe despertar al apilador aunque el hilo
        // cargador esté a mitad de un lote grande (antes esperaba el recv()
        // bloqueante hasta que el lote entero terminara de decodificarse).
        let dw_t0 = std::time::Instant::now();
        let frame_map = loop {
            match rx.recv_timeout(std::time::Duration::from_millis(250)) {
                Ok(Ok(m)) => break Some(m),
                Ok(Err(error)) => {
                    prefetch_error = Some(error);
                    break None;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if job_token.is_cancelled() {
                        break None;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    if !job_token.is_cancelled() {
                        prefetch_error = Some(
                            "El cargador de frames terminó antes de entregar todos los lotes"
                                .to_string(),
                        );
                    }
                    break None;
                }
            }
        };
        crate::perf_trace::add_ns(pt, ptag, "decode_wait", dw_t0.elapsed().as_nanos(), 1);
        let Some(frame_map) = frame_map else { break };
        if frame_map.is_empty() {
            prefetch_error = Some("El cargador publicó un lote de frames vacío".to_string());
            break;
        }

        let counter_ref = &global_align_counter;
        let total_frames_all = total_active;
        let app_ref = &app;
        // Telemetria: refs para el worker (medicion + emision cada 25 frames).
        let tele_start_ref = &tele_pass_start;
        let tele_align_ref = &tele_align_ns;
        let tele_accum_ref = &tele_accum_ns;
        let tele_upload_ref = &tele_upload_bytes;
        let tele_vram_ref = &tele_vram;
        let tele_sys_ref = &tele_sys;
        let tele_mode_ref = &tele_mode;
        let tele_job_id_ref = &pipeline_job_id;
        let tele_hits_ref = &tele_cache_hits;
        let acceptance_ref = &frame_acceptance_masks;
        let signal_valid_ref = &ap_signal_valid;
        let dark_ratio_ref = &ap_dark_ratio;
        let limb_normal_ref = &ap_limb_normal;
        let ap_mc_ref = &ap_master_contrast;
        let ap_ml_ref = &ap_master_lap;
        // Sigma-clip bounds: None in pass 1 (statistics being gathered),
        // Some(...) in pass 2 (winsorized accumulation).
        let clip_r_ref = clip_r.as_ref();
        let clip_g_ref = clip_g.as_ref();
        let clip_b_ref = clip_b.as_ref();

        let scratch_starved = std::sync::atomic::AtomicBool::new(false);
        let run_batch = || chunk.par_iter().for_each(|frame_data| {
                if job_token.is_cancelled() {
                    return; // cancelado: no procesar mas frames
                }
                // Un lease por frame evita que un estado `for_each_init`
                // sobreviva a un split de Rayon. Nunca se bloquea esperando
                // scratch: si una invariancia futura rompe el pool acotado,
                // se descarta el pase completo con Err en vez de panic/deadlock.
                let Some(mut lease) = ScratchLease::try_take(&scratch_pool_mx) else {
                    scratch_starved.store(true, Ordering::Release);
                    return;
                };
                let Some(sc) = lease.sc.as_mut() else {
                    scratch_starved.store(true, Ordering::Release);
                    return;
                };
                let completed = counter_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let t_frame_start = std::time::Instant::now();
                if completed % 25 == 0 {
                    emit_progress(app_ref, &format!("Zenith Elite V4: Procesando Frame {}/{}...", completed, total_frames_all), (completed as f32 / total_frames_all as f32) * 40.0 + 50.0, None);
                    // TELEMETRIA EN VIVO: modo, rendimiento y recursos.
                    let elapsed = tele_start_ref.elapsed().as_secs_f32().max(0.001);
                    let done = completed.max(1) as u64;
                    let (ram_mb, cpu_percent, io_read_mb, io_write_mb) = {
                        let mut s = tele_sys_ref.lock().unwrap_or_else(|e| e.into_inner());
                        // PR-2.5: refresh_all() enumeraba TODOS los procesos
                        // del sistema bajo mutex cada 25 frames — 10-40
                        // escaneos completos de la tabla de procesos POR
                        // SEGUNDO en SER mono rápidos, robando CPU justo en
                        // la fase que debe saturar el warp (mismo arreglo que
                        // ya tenía la telemetría del análisis).
                        s.refresh_memory();
                        s.refresh_cpu();
                        if let Ok(pid) = sysinfo::get_current_pid() {
                            s.refresh_process(pid);
                        }
                        let process = sysinfo::get_current_pid()
                            .ok()
                            .and_then(|pid| s.process(pid));
                        let disk = process.map(|process| process.disk_usage());
                        (
                            process
                                .map(|process| process.memory() / (1024 * 1024))
                                .unwrap_or_else(|| s.used_memory() / (1024 * 1024)),
                            process.map(|process| process.cpu_usage()),
                            disk.map(|usage| usage.total_read_bytes as f64 / 1_048_576.0)
                                .unwrap_or(0.0),
                            disk.map(|usage| usage.total_written_bytes as f64 / 1_048_576.0)
                                .unwrap_or(0.0),
                        )
                    };
                    let _ = app_ref.emit(
                        "stack_telemetry",
                        StackTelemetry {
                            phase: "stacking".to_string(),
                            mode: tele_mode_ref.clone(),
                            decode_gpu: None,
                            compute_gpu: use_gpu_pass,
                            frames_done: completed,
                            frames_total: total_frames_all,
                            fps: completed as f32 / elapsed,
                            align_ms: tele_align_ref.load(std::sync::atomic::Ordering::Relaxed)
                                as f32
                                / done as f32
                                / 1.0e6,
                            accum_ms: tele_accum_ref.load(std::sync::atomic::Ordering::Relaxed)
                                as f32
                                / done as f32
                                / 1.0e6,
                            upload_mbps: tele_upload_ref
                                .load(std::sync::atomic::Ordering::Relaxed)
                                as f32
                                / elapsed
                                / (1024.0 * 1024.0),
                            ram_mb,
                            vram_mb: tele_vram_ref.load(std::sync::atomic::Ordering::Relaxed)
                                / (1024 * 1024),
                            cache_hits: tele_hits_ref.load(std::sync::atomic::Ordering::Relaxed),
                            threads: stack_threads,
                        },
                    );
                    emit_pipeline_telemetry(
                        app_ref,
                        PipelineTelemetry {
                            job_id: tele_job_id_ref.to_string(),
                            domain: PipelineDomain::Planetary,
                            phase: "stacking".into(),
                            engine: tele_mode_ref.clone(),
                            progress: completed as f32 / total_frames_all.max(1) as f32 * 100.0,
                            eta_seconds: if completed > 0 && completed < total_frames_all {
                                Some(elapsed / completed as f32 * (total_frames_all - completed) as f32)
                            } else { None },
                            items_done: completed,
                            items_total: total_frames_all,
                            throughput: Some(completed as f32 / elapsed),
                            cpu_percent,
                            gpu_percent: None,
                            ram_mb,
                            vram_mb: tele_vram_ref.load(std::sync::atomic::Ordering::Relaxed) / (1024 * 1024),
                            io_read_mb,
                            io_write_mb,
                            cache_hits: tele_hits_ref.load(std::sync::atomic::Ordering::Relaxed),
                            cache_misses: completed.saturating_sub(tele_hits_ref.load(std::sync::atomic::Ordering::Relaxed)),
                            fallback_reason: None,
                        },
                    );
                }

                if let Some(u16_data) = frame_map.get(&frame_data.idx) {
                    // --- ALIGNMENT LOGIC (Unified '+' convention: Target = Ref + Shift) ---
                    // Shift convention: global_dx = SourcePos - MasterPos
                    let mut render_dx = frame_data.x_shift - ref_dx;
                    let mut render_dy = frame_data.y_shift - ref_dy;

                    if is_mono_stack {
                        // MONO BRANCH: single-channel working copy, no triplication.
                        sc.mono_buf.copy_from_slice(u16_data);
                        if is_surface_logic || large_disc {
                            normalize_surface_frame_exposure_mono_inplace(&mut sc.mono_buf, surface_ref_p90);
                        } else {
                            // PR-1.4: normalización per-frame también en planetas.
                            normalize_planetary_frame_exposure_mono_inplace(&mut sc.mono_buf, surface_ref_p90);
                        }
                    } else {
                        let t_deb = std::time::Instant::now();
                        debayer_into_buffer(u16_data, w_in, h_in, color_id, &mut sc.rgb_buf);
                        crate::perf_trace::add_ns(pt, ptag, "debayer", t_deb.elapsed().as_nanos(), 1);
                        if is_surface_logic || large_disc {
                            normalize_surface_frame_exposure_inplace(&mut sc.rgb_buf, surface_ref_p90, false);
                        } else {
                            normalize_planetary_frame_exposure_rgb_inplace(&mut sc.rgb_buf, surface_ref_p90);
                        }
                        // PR-2.2: deinterleave NEON en aarch64 (antes gather
                        // escalar stride-3 por frame y por pasada).
                        extract_green_channel_into(&sc.rgb_buf, &mut sc.mono_buf[..w_in * h_in]);
                    }

                    // PLANETARY per-frame CoG re-centering — SMALL discs only.
                    // A large lunar disc SKIPS this: its brightness centroid
                    // wobbles with the phase (round-14 lesson) and would
                    // override the texture-based analysis shift; it gets the
                    // surface-style 4× verification below instead. Computed
                    // from sc.mono_buf (already-debayered green) — the old code
                    // ran a SECOND full debayer per frame just for this.
                    if !is_surface_logic && !large_disc {
                        if let Some(m_cog) = master_cog {
                            // FIX COG PER-FRAME: Use peak-based threshold (same as master)
                            // to ensure consistent disk detection across all frames.
                            let f_peak = sc.mono_buf.iter().copied().max().unwrap_or(2000) as f32;
                            let f_cog_thresh = (f_peak * 0.15).max(512.0) as u16;
                            if let Some(f_cog) = crate::alignment::calculate_center_of_gravity(&sc.mono_buf, w_in, h_in, f_cog_thresh) {
                                // Refined Shift: Source_CoG - Master_CoG
                                render_dx = f_cog.0 - m_cog.0;
                                render_dy = f_cog.1 - m_cog.1;
                            }
                        }
                    }

                    let q_weight_opt = Some(
                        *quality_weight_by_idx.get(&frame_data.idx).unwrap_or(&1.0),
                    );

                    if let Some(q_weight_raw) = q_weight_opt {
                        let q_weight = if is_surface_logic || large_disc {
                            q_weight_raw.powf(1.35).max(0.03)
                        } else {
                            q_weight_raw
                        };
                        // R13 FUSION: dense quality map built INLINE from the frame
                        // buffer the prefetcher already loaded — identical math to
                        // the old pre-pass, minus one full video read. Every stacked
                        // frame has its map, in every pass.
                        let (dq_ds, _dq_dw, _dq_dh) =
                            downscale_u16_to_f32_box(&sc.mono_buf, w_in, h_in, DQ_DOWNSCALE);
                        let dq_source_map =
                            DenseQualityMap::build(&dq_ds, dq_source_w, dq_source_h, 3);
                        let box_size = ap_size as usize;
                        // Pass 2 re-aligns against the pass-1 stack: residual shifts
                        // are tiny, so a tight search window suppresses false SAD
                        // minima on low-contrast detail (sharper convergence).
                        // Large lunar discs use the (cheaper) surface windows: they
                        // align by texture with a verified global shift, so the wide
                        // planetary window only adds cost and false minima.
                        let search_r: i32 = if !is_surface_logic && !large_disc {
                            if pass == 0 { 32 } else { 12 }
                        } else if pass == 0 {
                            24
                        } else {
                            10
                        };

                        let t_enh = std::time::Instant::now();
                        enhance_for_alignment_into_amount(&sc.mono_buf, w_in, h_in, &mut sc.f_s1, &mut sc.f_s2, &mut sc.f_edges, align_amount);
                        downscale_4x(&sc.f_edges, w_in, h_in, &mut sc.f_ds);
                        crate::perf_trace::add_ns(pt, ptag, "enhance", t_enh.elapsed().as_nanos(), 1);

                        // FRAME-LEVEL ALIGNMENT VERIFICATION: the cached analysis
                        // shift can be wrong for individual frames (clouds, seeing
                        // bursts, tracking jumps). A wrong base shift is beyond the
                        // ±search_r AP window and stacks a displaced GHOST copy
                        // (doubled limb). A cheap 4×-downscaled re-match against the
                        // master confirms or corrects it before the AP pass.
                        // PR-1.5: TODOS los objetivos la reciben. Antes los planetas
                        // pequeños quedaban excluidos y su CoG per-frame entraba SIN
                        // red de seguridad: en objetivos tenues (Neptuno, tránsitos)
                        // un salto de CoG por ruido/ráfaga apilaba una copia fantasma.
                        {
                            let w_ds = w_in / 4;
                            let h_ds = h_in / 4;
                            let cx = w_ds / 2;
                            let cy = h_ds / 2;
                            let est_x = (cx as f32 + render_dx / 4.0).round() as i32;
                            let est_y = (cy as f32 + render_dy / 4.0).round() as i32;
                            let vbox = (w_ds.min(h_ds) / 2).max(16);
                            if est_x > 0
                                && est_y > 0
                                && (est_x as usize) < w_ds
                                && (est_y as usize) < h_ds
                            {
                                let t_sg = std::time::Instant::now();
                                let (cdx, cdy, _) = crate::alignment::find_best_match_sad(
                                    &master_ds_buf,
                                    &sc.f_ds,
                                    w_ds,
                                    cx,
                                    cy,
                                    est_x as usize,
                                    est_y as usize,
                                    vbox,
                                    16,
                                );
                                crate::perf_trace::add_ns(pt, ptag, "sad_global", t_sg.elapsed().as_nanos(), 1);
                                let measured_dx = (est_x as f32 - cx as f32 + cdx) * 4.0;
                                let measured_dy = (est_y as f32 - cy as f32 + cdy) * 4.0;
                                if (measured_dx - render_dx).abs() > 6.0
                                    || (measured_dy - render_dy).abs() > 6.0
                                {
                                    render_dx = measured_dx;
                                    render_dy = measured_dy;
                                }
                            }
                        }

                        // SAD GRUESO POR AP EN GPU: una sola subida de los dos
                        // mapas 4× y un dispatch para toda la malla. Los AP sin
                        // señal/textura se desactivan antes del kernel. Ante
                        // device-loss/OOM el pase completo se reintenta en CPU.
                        let gpu_coarse_shifts = if gpu_sad_enabled.load(Ordering::Relaxed)
                            && search_r > 3
                            && master_ds_w > 0
                            && !custom_points.is_empty()
                        {
                            let coarse_box = (box_size / 4).max(4) as i32;
                            let coarse_r = (search_r / 4).max(2);
                            let points: Vec<crate::gpu_analysis::SadPoint> = custom_points
                                .iter()
                                .enumerate()
                                .map(|(ap_i, ap)| {
                                    let enabled = signal_valid_ref.get(ap_i).copied().unwrap_or(true)
                                        && (ap_lap_floor <= 0.0
                                            || ap_ml_ref.get(ap_i).copied().unwrap_or(f32::MAX)
                                                >= ap_lap_floor);
                                    let fx = (ap.x + render_dx).round() as i32;
                                    let fy = (ap.y + render_dy).round() as i32;
                                    crate::gpu_analysis::SadPoint::new(
                                        ap.x as i32 / 4,
                                        ap.y as i32 / 4,
                                        fx / 4,
                                        fy / 4,
                                        coarse_box,
                                        coarse_r,
                                        enabled,
                                    )
                                })
                                .collect();
                            let t_sap = std::time::Instant::now();
                            let sap = match crate::gpu_analysis::search_sad_points(
                                &master_ds_buf,
                                &sc.f_ds,
                                master_ds_w,
                                sc.f_ds.len() / master_ds_w,
                                &points,
                            ) {
                                Ok(v) => Some(v),
                                Err(e) => {
                                    if gpu_sad_enabled.swap(false, Ordering::Relaxed) {
                                        eprintln!(
                                            "[gpu] fallo SAD batched por AP ({e}); refinamiento CPU exacto"
                                        );
                                    }
                                    None
                                }
                            };
                            crate::perf_trace::add_ns(pt, ptag, "sad_ap_gpu", t_sap.elapsed().as_nanos(), 1);
                            sap
                        } else {
                            None
                        };

                        let t_ls = std::time::Instant::now();
                        let local_shifts = compute_frame_local_shifts(
                            &master_edges,
                            ap_mc_ref,
                            ap_ml_ref,
                            &master_ds_buf,
                            master_ds_w,
                            &sc.f_edges,
                            &sc.mono_buf,
                            &sc.f_ds,
                            w_in,
                            h_in,
                            &custom_points,
                            signal_valid_ref,
                            dark_ratio_ref,
                            limb_normal_ref,
                            render_dx,
                            render_dy,
                            box_size,
                            search_r,
                            // Large discs use the surface spatial-outlier filter
                            // (gentler fallback): the planetary variant's harsh
                            // rejection pushed too many APs onto the global
                            // fallback vector → locally smeared warp.
                            is_surface_logic || large_disc,
                            limb_protect,
                            ap_lap_floor,
                            chunk.len() < stack_threads && custom_points.len() >= 128,
                            gpu_coarse_shifts.as_deref(),
                        );
                        crate::perf_trace::add_ns(pt, ptag, "local_shifts", t_ls.elapsed().as_nanos(), 1);

                        let combined_ap_mask: Vec<bool> = (0..custom_points.len())
                            .map(|i| {
                                let frame_ok = acceptance_ref
                                    .accepts(frame_data.idx, i)
                                    .unwrap_or(true);
                                let signal_ok = signal_valid_ref.get(i).copied().unwrap_or(true);
                                frame_ok && signal_ok
                            })
                            .collect();

                        let drop_size = if drizzle > 1.01 { 0.75f32 } else { 1.0f32 };
                        // Dense-quality registration (CPU/GPU identica): primero
                        // recorta/reproyecta el mapa de sensor a la ROI de
                        // referencia; despues expresa el shift en unidades de ese
                        // mapa. Asi el lookup `out*dq/out + off` incorpora las
                        // tres transformaciones correctas: ROI + 1/drizzle + shift.
                        let dq_map = dq_source_map.into_reference_raster(
                            w_in,
                            h_in,
                            w_out,
                            h_out,
                            drizzle,
                            roi_offset_x,
                            roi_offset_y,
                            dq_w,
                            dq_h,
                        );
                        let d_quality: &[f32] = &dq_map.scores;
                        let q_off_x =
                            DenseQualityMap::shift_offset(render_dx, dq_w, w_out, drizzle);
                        let q_off_y =
                            DenseQualityMap::shift_offset(render_dy, dq_h, h_out, drizzle);

                        // La alineacion CPU de este frame termina aqui.
                        tele_align_ref.fetch_add(
                            t_frame_start.elapsed().as_nanos() as u64,
                            std::sync::atomic::Ordering::Relaxed,
                        );

                        // ===== RAMA GPU: el warp+acumulacion de este frame va al
                        // submitter (canal acotado); la CPU sigue alineando el
                        // siguiente frame en paralelo. apq pre-combina los gates
                        // del blend (acceptance ∧ validez → 0; ap_w·clamp(q)).
                        if let Some(gtx) = gpu_tx.as_ref() {
                            let mut px_buf =
                                gpu_px_pool.lock().unwrap_or_else(|e| e.into_inner()).pop().unwrap_or_default();
                            px_buf.clear();
                            if is_mono_stack {
                                px_buf.extend_from_slice(&sc.mono_buf);
                            } else {
                                px_buf.extend_from_slice(&sc.rgb_buf);
                            }
                            let apq: Vec<[f32; 4]> = local_shifts
                                .iter()
                                .enumerate()
                                .map(|(i, &(dx, dy, q))| {
                                    let ok = combined_ap_mask.get(i).copied().unwrap_or(true);
                                    let apw =
                                        ap_quality_weights.get(i).copied().unwrap_or(1.0);
                                    if !ok || q <= 0.0 {
                                        [0.0; 4]
                                    } else {
                                        [dx, dy, apw * q.clamp(0.05, 1.0), 0.0]
                                    }
                                })
                                .collect();
                            // send() con canal cerrado = submitter caido: el pase
                            // se reintentara en CPU, no hay nada que hacer aqui.
                            let t_sub = std::time::Instant::now();
                            let _ = gtx.send(crate::gpu_stack::GpuFrameJob {
                                pixels: px_buf,
                                apq,
                                dq_map: dq_map.scores.clone(),
                                gw: q_weight,
                                render_dx,
                                render_dy,
                                q_off_x,
                                q_off_y,
                            });
                            crate::perf_trace::add_ns(pt, ptag, "gpu_submit_wait", t_sub.elapsed().as_nanos(), 1);
                            return;
                        }

                        let t_acc = std::time::Instant::now();
                        if is_mono_stack {
                            sc.wg.fill(0.0);
                            sc.ww.fill(0.0);
                            accumulate_frame_liquid_mono(&mut sc.wg, &mut sc.ww, &sc.mono_buf, w_in, h_in, w_out, h_out, drizzle, roi_offset_x, roi_offset_y, render_dx, render_dy, &local_shifts, &warp_indices, &warp_weights, &combined_ap_mask, &ap_quality_weights, 1.0, drop_size);

                            for i in 0..w_out * h_out {
                                let w_val = sc.ww[i];
                                if w_val > 1e-9 {
                                    sc.wg[i] /= w_val;
                                }
                            }
                            // Pass 2: kappa-sigma rejection against the pass-1
                            // statistics (kills satellites/birds/glitches; the
                            // clean frames keep full weight → no holes).
                            if let Some((lo, hi)) = clip_g_ref {
                                apply_sigma_rejection(&sc.wg, &mut sc.ww, lo, hi);
                            }
                            acc_g_sh.accumulate(&sc.wg, &sc.ww, d_quality, dq_w, dq_h, q_off_x, q_off_y, q_weight, frame_data.idx, coverage_weighting);
                        } else {
                            sc.wr.fill(0.0);
                            sc.wg.fill(0.0);
                            sc.wb.fill(0.0);
                            sc.ww.fill(0.0);
                            accumulate_frame_liquid(&mut sc.wr, &mut sc.wg, &mut sc.wb, &mut sc.ww, &sc.rgb_buf, w_in, h_in, w_out, h_out, drizzle, roi_offset_x, roi_offset_y, render_dx, render_dy, &local_shifts, &warp_indices, &warp_weights, &combined_ap_mask, &ap_quality_weights, &custom_points, 1.0, drop_size);

                            for i in 0..w_out * h_out {
                                let w_val = sc.ww[i];
                                if w_val > 1e-9 {
                                    sc.wr[i] /= w_val;
                                    sc.wg[i] /= w_val;
                                    sc.wb[i] /= w_val;
                                }
                            }
                            // Pass 2: kappa-sigma rejection against the pass-1
                            // statistics. A transient (satellite/bird/glitch)
                            // is achromatic: if ANY channel is an outlier the
                            // whole pixel of this frame is dropped (shared
                            // coverage plane) — the clean frames fill it in.
                            if let Some((lo, hi)) = clip_r_ref {
                                apply_sigma_rejection(&sc.wr, &mut sc.ww, lo, hi);
                            }
                            if let Some((lo, hi)) = clip_g_ref {
                                apply_sigma_rejection(&sc.wg, &mut sc.ww, lo, hi);
                            }
                            if let Some((lo, hi)) = clip_b_ref {
                                apply_sigma_rejection(&sc.wb, &mut sc.ww, lo, hi);
                            }
                            // STRIPED MERGE: locks por banda de filas en vez del
                            // mutex full-canvas por canal — dos workers solo
                            // chocan en la misma banda, y el offset por frame
                            // decorrelaciona el orden de recorrido (sin convoy).
                            acc_r_sh.accumulate(&sc.wr, &sc.ww, d_quality, dq_w, dq_h, q_off_x, q_off_y, q_weight, frame_data.idx, coverage_weighting);
                            acc_g_sh.accumulate(&sc.wg, &sc.ww, d_quality, dq_w, dq_h, q_off_x, q_off_y, q_weight, frame_data.idx, coverage_weighting);
                            acc_b_sh.accumulate(&sc.wb, &sc.ww, d_quality, dq_w, dq_h, q_off_x, q_off_y, q_weight, frame_data.idx, coverage_weighting);
                        }
                        crate::perf_trace::add_ns(pt, ptag, "warp_accum_cpu", t_acc.elapsed().as_nanos(), 1);
                    }
                }
            }
        );
        // Ejecutar exclusivamente en el pool que respeta el plan de RAM.
        stack_pool.install(run_batch);
        if scratch_starved.load(Ordering::Acquire) {
            worker_error = Some(
                "El pool planetario agotó su scratch acotado; el pase parcial fue descartado"
                    .to_string(),
            );
            break;
        }
    }

    // Cierre del submitter GPU: cerrar el canal, esperar la descarga.
    let gpu_download: Option<crate::gpu_stack::GpuDownload> = {
        drop(gpu_tx); // los workers terminaron: el canal se cierra
        match gpu_join {
            Some(h) => h.join().ok().flatten(),
            None => None,
        }
    };
    if let Some(error) = worker_error {
        return Err(error);
    }
    if let Some(error) = prefetch_error {
        if r.is_ffmpeg()
            && ffmpeg_decode_route_hardware.load(Ordering::Acquire)
            && !ffmpeg_force_cpu_decode.swap(true, Ordering::AcqRel)
            && !job_token.is_cancelled()
        {
            log_to_front(
                &app,
                "WARN",
                &format!(
                    "Decode hardware falló ({error}); descartando la pasada completa y reintentando desde frame 0 por CPU."
                ),
            );
            continue 'pass_attempt;
        }
        return Err(format!(
            "No se pudo completar la carga exacta de frames; el pase parcial fue descartado: {error}"
        ));
    }
    if use_gpu_pass
        && gpu_download.is_none()
        && !job_token.is_cancelled()
    {
        if !compute_policy.allows_fallback() {
            return Err(
                "GPU only: el dispositivo fallo durante el pase (device loss, OOM o error de descarga); el trabajo se cancelo sin fingir aceleracion CPU."
                    .to_string(),
            );
        }
        // FALLBACK EN CALIENTE: el pase completo se repite en CPU. El cache
        // de decode por-frame hace el reintento barato, y el resultado es el
        // de una corrida 100% CPU.
        log_to_front(
            &app,
            "WARN",
            "GPU fallo durante el pase — reiniciando el pase en CPU (los frames se re-sirven del cache).",
        );
        let _ = gpu_failed.load(std::sync::atomic::Ordering::Relaxed);
        gpu_disabled_this_stack = true;
        continue 'pass_attempt;
    }

    // Collect the shared accumulators for this pass (acceso exclusivo de
    // nuevo: los workers del pase ya terminaron). En modo GPU, los planos
    // vienen de la descarga (punto fijo → f64) y TODO lo downstream es
    // identico a la ruta CPU.
    if let Some(d) = gpu_download {
        let mk_gs = |direct: Vec<f64>, w: Vec<f64>, w2: Vec<f64>, m2: Vec<f64>| GradientDomainStacker {
            grad_x: Vec::new(),
            grad_y: Vec::new(),
            weight: Vec::new(),
            direct,
            direct_w: w,
            direct_w2: w2,
            m2,
            width: w_out,
            height: h_out,
        };
        if !is_mono_stack {
            acc_grad_g = mk_gs(
                d.direct_g,
                d.direct_w.clone(),
                d.direct_w2.clone(),
                d.m2_g,
            );
            acc_grad_r = mk_gs(
                d.direct_r,
                d.direct_w.clone(),
                d.direct_w2.clone(),
                d.m2_r,
            );
            acc_grad_b = mk_gs(d.direct_b, d.direct_w, d.direct_w2, d.m2_b);
        } else {
            acc_grad_g = mk_gs(d.direct_g, d.direct_w, d.direct_w2, d.m2_g);
            acc_grad_r = GradientDomainStacker::new_empty();
            acc_grad_b = GradientDomainStacker::new_empty();
        }
    } else {
        acc_grad_r = acc_r_sh.into_inner();
        acc_grad_g = acc_g_sh.into_inner();
        acc_grad_b = acc_b_sh.into_inner();
    }
    drop(scratch_pool_mx);
    break 'pass_attempt;
    } // fin 'pass_attempt

    // Between passes: rebuild the alignment reference from the fresh stack
    // (its geometry matches the master because drizzle/ROI are disabled here).
    if pass + 1 < total_passes {
        // Cancelacion a mitad de pase (en modo GPU deja los planos vacios):
        // no reconstruir nada — el check tras el bucle devuelve Err limpio.
        if job_token.is_cancelled()
            || acc_grad_g.direct.is_empty()
        {
            break;
        }
        let _s = crate::perf_trace::span(pt, "rebuild_ref");
        // KAPPA-SIGMA: freeze the pass-1 per-pixel statistics into winsorization
        // bounds for pass 2. Built BEFORE the accumulators are recreated; the
        // m2 planes are released with the pass-1 stackers right after.
        if sigma_clip_enabled {
            emit_progress(
                &app,
                "Doble Pasada: construyendo mapa sigma-clip (anti-artefactos)...",
                91.0,
                None,
            );
            if !acc_grad_g.m2.is_empty() {
                clip_g = Some(build_sigma_clip_bounds(
                    &acc_grad_g,
                    sigma_clip_k,
                    SIGMA_CLIP_FLOOR,
                )?);
            }
            if !is_mono_stack {
                if !acc_grad_r.m2.is_empty() {
                    clip_r = Some(build_sigma_clip_bounds(
                        &acc_grad_r,
                        sigma_clip_k,
                        SIGMA_CLIP_FLOOR,
                    )?);
                }
                if !acc_grad_b.m2.is_empty() {
                    clip_b = Some(build_sigma_clip_bounds(
                        &acc_grad_b,
                        sigma_clip_k,
                        SIGMA_CLIP_FLOOR,
                    )?);
                }
            }
        }
        emit_progress(&app, "Doble Pasada: regenerando referencia desde el apilado...", 92.0, None);
        // PR-1.3: el lienzo de la pasada 1 puede llevar drizzle/ROI; el
        // máster de alineación vive en geometría de ENTRADA. Convención del
        // acumulador: in = out·inv_drizzle + roi_offset ⇒ out = (in −
        // roi_offset)·drizzle. Muestreo bilineal ponderado por cobertura;
        // fuera de cobertura (ROI, esquinas) se CONSERVA el máster previo
        // para que la alineación siga teniendo referencia allí.
        {
            let same_geometry = (drizzle - 1.0).abs() < 0.01 && stacking_roi.is_none();
            for y in 0..h_in {
                for x in 0..w_in {
                    let i = y * w_in + x;
                    if same_geometry {
                        let w_g = acc_grad_g.direct_w[i].max(1e-9);
                        master_mono[i] =
                            ((acc_grad_g.direct[i] / w_g) as f32).clamp(0.0, 65535.0) as u16;
                        continue;
                    }
                    let ox = (x as f32 - roi_offset_x) * drizzle;
                    let oy = (y as f32 - roi_offset_y) * drizzle;
                    if ox < 0.0 || oy < 0.0 {
                        continue;
                    }
                    let x0 = ox as usize;
                    let y0 = oy as usize;
                    if x0 + 1 >= w_out || y0 + 1 >= h_out {
                        continue;
                    }
                    let fx = ox - x0 as f32;
                    let fy = oy - y0 as f32;
                    let mut acc_v = 0.0f32;
                    let mut acc_cov = 0.0f32;
                    for (dy2, wy) in [(0usize, 1.0 - fy), (1usize, fy)] {
                        for (dx2, wx) in [(0usize, 1.0 - fx), (1usize, fx)] {
                            let j = (y0 + dy2) * w_out + (x0 + dx2);
                            let cov = acc_grad_g.direct_w[j];
                            if cov > 1e-6 {
                                let wgt = wx * wy;
                                acc_v += (acc_grad_g.direct[j] / cov) as f32 * wgt;
                                acc_cov += wgt;
                            }
                        }
                    }
                    if acc_cov > 0.05 {
                        master_mono[i] = (acc_v / acc_cov).clamp(0.0, 65535.0) as u16;
                    }
                }
            }
        }
        master_edges = enhance_for_alignment_with_amount(&master_mono, w_in, h_in, align_amount);
        let (ds_w, _ds_h) = downscale_4x(&master_edges, w_in, h_in, &mut master_ds_buf);
        master_ds_w = ds_w;
        // Los planos tracked de pasada 1 ya se convirtieron en bounds y en la
        // nueva referencia. Liberarlos ANTES de crear los acumuladores de
        // pasada 2 evita que ambos juegos convivan fuera del pico presupuestado.
        acc_grad_r = GradientDomainStacker::new_empty();
        acc_grad_g = GradientDomainStacker::new_empty();
        acc_grad_b = GradientDomainStacker::new_empty();
    }
    } // end multi-pass loop

    if job_token.is_cancelled() {
        return Err("Cancelado por el usuario".into());
    }

    // Elite V4: High-Fidelity Multi-Point Stack
    // (Poisson gradient stacking removed for this path: it smoothed surface
    //  micro-detail; the stackers run in direct-only mode to save RAM.)
    let stacked_len = w_out
        .checked_mul(h_out)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or("El máster RGB f32 excede el espacio direccionable")?;
    let mut stacked_f32 = try_filled_vec(stacked_len, 0.0f32, "máster RGB f32")?;
    if is_mono_stack {
        for i in 0..w_out * h_out {
            let w_g = acc_grad_g.direct_w[i].max(1e-9);
            let v = (acc_grad_g.direct[i] / w_g) as f32;
            stacked_f32[i*3]   = v;
            stacked_f32[i*3+1] = v;
            stacked_f32[i*3+2] = v;
        }
    } else {
        for i in 0..w_out * h_out {
            let w_r = acc_grad_r.direct_w[i].max(1e-9);
            let w_g = acc_grad_g.direct_w[i].max(1e-9);
            let w_b = acc_grad_b.direct_w[i].max(1e-9);
            stacked_f32[i*3]   = (acc_grad_r.direct[i] / w_r) as f32;
            stacked_f32[i*3+1] = (acc_grad_g.direct[i] / w_g) as f32;
            stacked_f32[i*3+2] = (acc_grad_b.direct[i] / w_b) as f32;
        }
    }

    if is_surface_logic {
        repair_stack_coverage_holes_rgb_f32(&mut stacked_f32, &acc_grad_g.direct_w, w_out, h_out);

        // EDGE ARTIFACT MITIGATION (AS!4-style cropped output): output borders
        // covered by only a few shifted frames show seams, exposure steps and
        // noise. Trim border rows/cols whose coverage is far below the median.
        // Recorte AS!4 de bordes de baja cobertura. Se OMITE si el usuario pidió
        // mantener el encuadre completo (keep_full_frame) o si hay un ROI de
        // apilado manual (que ya define el frame de salida).
        let crop_box = if !keep_full_frame.unwrap_or(false) && stacking_roi.is_none() {
            compute_low_coverage_crop(&acc_grad_g.direct_w, w_out, h_out, 0.06)
        } else {
            None
        };
        if let Some((cx0, cy0, cx1, cy1)) = crop_box {
            let nw = cx1 - cx0;
            let nh = cy1 - cy0;
            let cropped_len = nw
                .checked_mul(nh)
                .and_then(|pixels| pixels.checked_mul(3))
                .ok_or("El recorte RGB excede el espacio direccionable")?;
            let mut cropped = try_filled_vec(cropped_len, 0.0f32, "recorte RGB")?;
            for y in 0..nh {
                let src = ((y + cy0) * w_out + cx0) * 3;
                let dst = y * nw * 3;
                cropped[dst..dst + nw * 3].copy_from_slice(&stacked_f32[src..src + nw * 3]);
            }
            stacked_f32 = cropped;
            w_out = nw;
            h_out = nh;
        }
    }

    // AUTO RGB ALIGN (AS!4 parity): correct atmospheric-dispersion channel
    // shifts on the stacked result. Measured on the stack (huge SNR) so even
    // sub-pixel dispersion is detected; a no-op when channels already match.
    // User-switchable (chk-rgb-align, default ON); guarded by measurement-site
    // selection and the improvement gate inside align_stack_rgb_channels.
    if !is_mono_stack && align_rgb.unwrap_or(true) {
        emit_progress(&app, "Alineacion RGB automatica (dispersion atmosferica)...", 94.0, None);
        let (rdx, rdy, bdx, bdy) = align_stack_rgb_channels(&mut stacked_f32, w_out, h_out);
        if rdx.abs().max(rdy.abs()).max(bdx.abs()).max(bdy.abs()) >= 0.05 {
            log_to_front(
                &app,
                "INFO",
                &format!(
                    "RGB Align automatico: R({:+.2}, {:+.2}) px · B({:+.2}, {:+.2}) px",
                    rdx, rdy, bdx, bdy
                ),
            );
        } else {
            log_to_front(
                &app,
                "INFO",
                "RGB Align: sin correccion (canales ya alineados o medicion no concluyente).",
            );
        }
    }

    // El resultado base es deliberadamente lineal. La ruta anterior anunciaba
    // PSF/deconvolución y calculaba tres mapas de frame completo que después no
    // se consumían: coste O(N) y picos de RAM sin cambiar un solo píxel.
    emit_progress(&app, "Finalizando máster planetario lineal...", 95.0, None);
    
    // Elite V4: Standard High-Fidelity Raw Stack (No Post-Processing)
    // As requested: more like AutoStakkert. The user will sharpen externally.
    //
    // BIT-DEPTH FIX: scale to the full 16-bit range in FLOATING POINT and
    // round. 10/12-bit captures (2-byte container, values ≤4095) used to be
    // truncated to their native quantization here and expanded only at the
    // very end — discarding the sub-LSB precision gained by stacking, i.e.
    // exactly the faint filaments and smooth gray transitions.
    // Nunca inferir la profundidad a partir del brillo de la escena: una Luna
    // subexpuesta de 16 bits no es una captura de 12 bits. El lector declara
    // la precisión nativa y los decoders que ya normalizan reportan ganancia 1.
    let bd_gain_f = source_adu_gain;
    let final_len = w_out
        .checked_mul(h_out)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or("El máster RGB u16 excede el espacio direccionable")?;
    let mut final_u16 = try_filled_vec(final_len, 0u16, "máster RGB u16")?;
    for (dst, &src) in final_u16.iter_mut().zip(stacked_f32.iter()) {
        *dst = (src * bd_gain_f + 0.5).clamp(0.0, 65535.0) as u16;
    }

    // FIX NOISE: Reject hot pixels / cosmic rays BEFORE sharpening.
    // SURFACE EXCEPTION: real solar/lunar detail lives at 1-2 px scale
    // (spicules, prominence fringes, rilles) and is statistically identical to
    // a hot pixel for this filter — it was ERASING genuine limb detail. With
    // dozens of warped frames stacked, sensor hot pixels are already diluted,
    // so surface stacks skip this stage entirely. Large lunar discs skip it for
    // the same reason (crater rims/rilles ARE 1-2 px detail) — and it saves a
    // full pass over a potentially 20-Mpx canvas.
    if !is_surface_logic && !large_disc {
        emit_progress(&app, "Eliminando pixeles calientes y ruido residual...", 92.0, None);
        reject_spatial_outliers_u16(&mut final_u16, w_out, h_out);
    }

    // POST-STACK CHROMA NOISE REDUCTION — ADAPTIVE (Mejora 5 revisada):
    // the fixed radius (2 surface / 1 planet) blurred REAL color detail
    // (lunar mineral tinting, planetary band hues) even when stacking had
    // already averaged the chroma noise away. Measure the residual chroma
    // noise and smooth only as much as the data actually needs.
    if is_color_video {
        let chroma_sigma = estimate_stack_chroma_noise(&final_u16, w_out, h_out);
        let chroma_r: usize = if chroma_sigma < 40.0 {
            0
        } else if chroma_sigma < 120.0 {
            1
        } else {
            2
        };
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Ruido croma del stack: {:.0} ADU → {}",
                chroma_sigma,
                if chroma_r == 0 {
                    "suavizado omitido (croma limpio, color preservado)".to_string()
                } else {
                    format!("suavizado radio {}", chroma_r)
                }
            ),
        );
        if chroma_r > 0 {
            smooth_chroma_inplace(&mut final_u16, w_out, h_out, chroma_r);
        }
    }

    // Apply stack sharpening if enabled via UI toggle ("Sharpened" checkbox)
    if sharpened {
        emit_progress(&app, "Aplicando Sharpening (Wavelet Multi-Band)...", 96.0, None);
        let _s = crate::perf_trace::span(pt, "sharpen");
        apply_autostakkert_sharpening(&mut final_u16, w_out, h_out, sharpen_intensity, &target_type);
    }

    // Exposure / dynamic-range normalization
    {
        // Large lunar discs use the SURFACE normalization: the planetary
        // exposure-match leaves the stack at the (deliberately low) capture
        // exposure, and with the LINEAR result preview the Moon looked
        // "attenuated" next to the max-normalized source view. Real planets
        // keep the gentle exposure match (no black-point lift on Jupiter etc.).
        if is_surface_logic || large_disc {
            normalize_surface_stack_channelsafe(&mut final_u16);
        } else {
            // Planetary: match exposure against the (bd-scaled) clean reference.
            let gain = if (bd_gain_f - 1.0).abs() < 0.01 {
                compute_planetary_exposure_gain(&final_u16, &master_clean_rgb, ref_mean)
            } else {
                let scaled_ref: Vec<u16> = master_clean_rgb
                    .iter()
                    .map(|&v| ((v as f32) * bd_gain_f).min(65535.0) as u16)
                    .collect();
                compute_planetary_exposure_gain(&final_u16, &scaled_ref, ref_mean * bd_gain_f)
            };
            for v in &mut final_u16 { *v = (*v as f32 * gain).clamp(0.0, 65535.0) as u16; }
        }
    }

    if is_color_video && normalize_colors { correct_white_balance(&mut final_u16, w_out, h_out); }

    // (Bit-depth expansion now happens in floating point BEFORE the u16
    //  conversion — see bd_gain_f above. The old post-hoc u16 expansion
    //  re-quantized the stack and destroyed the precision gained by stacking.)

    if job_token.is_cancelled() {
        return Err("Cancelado o sustituido por otro apilado".into());
    }
    emit_progress(&app, "Guardando resultado...", 98.0, None);
    let preview_src = {
        // PREVIEW FIDELITY: the stack DATA is already normalized upstream
        // (auto black point + max anchored at 95%, clipping impossible by
        // construction). Applying a second auto-stretch here burned the
        // highlights and crushed the shadows in the view — the preview is now
        // LINEAR and shows exactly what was stacked, as the user recorded it.
        let vis = to_8bit_visual(&final_u16, 1.0);
        let mut rgba = Vec::with_capacity(w_out * h_out * 4);
        for i in 0..(w_out * h_out) {
            let off = i * 3;
            rgba.push(vis[off]); rgba.push(vis[off+1]); rgba.push(vis[off+2]); rgba.push(255u8);
        }
        let img_out = RgbaImage::from_raw(w_out as u32, h_out as u32, rgba).unwrap();
        let mut enc = Vec::new();
        DynamicImage::ImageRgba8(img_out)
            .write_to(&mut Cursor::new(&mut enc), image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        // ASSET PROTOCOL: el preview se sirve como ARCHIVO temporal y el
        // frontend lo carga con convertFileSrc. Transportarlo como data-URL
        // base64 por el IPC duplicaba el pico de RAM del WebView (WebView2 es
        // especialmente fragil con strings de >50 MB en canvases con drizzle).
        // Fallback al data-URL solo si el temp no es escribible.
        save_preview_png_to_temp(&enc, "stack").unwrap_or_else(|| {
            format!("data:image/png;base64,{}", general_purpose::STANDARD.encode(&enc))
        })
    };

    {
        let _generation_guard = state
            .planetary_generation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if job_token.is_cancelled() {
            return Err("Cancelado o sustituido por otro apilado".into());
        }
        let mut res = state.stacked_image.lock().unwrap_or_else(|e| e.into_inner());
        *res = Some(StackResult {
            width: w_out, height: h_out,
            data: final_u16,
            is_mono: is_mono_stack,
            is_surface: is_surface,
        });
        state.deconv_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
        state.wavelet_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
        state.filter_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    // CACHE DE DECODE PERSISTENTE: ya NO se borra al terminar — re-apilar el
    // mismo video (otro %, otra malla, otro drizzle) sirve los frames desde
    // disco sin re-decodificar. La poda LRU se adapta a RAM y espacio libre.
    let cache_dir = decode_cache_root_dir();
    let cache_budget = decode_cache_budget_bytes(&cache_dir);
    prune_decode_cache_to_budget(&cache_dir, cache_budget);

    let (final_ram_mb, final_cpu_percent, final_io_read_mb, final_io_write_mb) = {
        let mut system = System::new();
        system.refresh_memory();
        system.refresh_cpu();
        let pid = sysinfo::get_current_pid().ok();
        if let Some(pid) = pid {
            system.refresh_process(pid);
        }
        let process = pid.and_then(|pid| system.process(pid));
        let disk = process.map(|process| process.disk_usage());
        (
            process
                .map(|process| process.memory() / (1024 * 1024))
                .unwrap_or_else(|| system.used_memory() / (1024 * 1024)),
            process.map(|process| process.cpu_usage()),
            disk.map(|usage| usage.total_read_bytes as f64 / 1_048_576.0)
                .unwrap_or(0.0),
            disk.map(|usage| usage.total_written_bytes as f64 / 1_048_576.0)
                .unwrap_or(0.0),
        )
    };
    if job_token.is_cancelled() {
        return Err("Cancelado o sustituido por otro apilado".into());
    }
    emit_pipeline_telemetry(
        &app,
        PipelineTelemetry {
            job_id: pipeline_job_id,
            domain: PipelineDomain::Planetary,
            phase: "complete".into(),
            engine: if gpu_rt.is_some() && !gpu_disabled_this_stack {
                "Hybrid CPU+GPU wgpu".into()
            } else {
                format!("CPU ({})", get_accel_label())
            },
            progress: 100.0,
            eta_seconds: Some(0.0),
            items_done: total_active,
            items_total: total_active,
            throughput: Some(total_active as f32 / pipeline_started.elapsed().as_secs_f32().max(0.001)),
            cpu_percent: final_cpu_percent,
            gpu_percent: None,
            ram_mb: final_ram_mb,
            vram_mb: stack_peak_vram.load(std::sync::atomic::Ordering::Relaxed) / (1024 * 1024),
            io_read_mb: final_io_read_mb,
            io_write_mb: final_io_write_mb,
            cache_hits: tele_cache_hits.load(std::sync::atomic::Ordering::Relaxed),
            cache_misses: total_active.saturating_sub(tele_cache_hits.load(std::sync::atomic::Ordering::Relaxed)),
            fallback_reason: gpu_disabled_this_stack.then_some("GPU no rentable, sin paridad o fallback operativo".into()),
        },
    );

    let _ = pt_guard.finish_ok();
    Ok(preview_src)
}

/// Per-AP statistics of the master that are constant across frames.
/// PERF: these were recomputed for every frame × AP (two full box scans
/// each) — precomputing them once per pass removes ~40% of the AP-loop cost.
fn precompute_ap_master_stats(
    master_edges: &[u16],
    master_mono: &[u16],
    w: usize,
    h: usize,
    points: &[ApPoint],
    box_size: usize,
) -> (Vec<f32>, Vec<f32>) {
    // PR-24: cada AP es independiente — par_iter + unzip conserva el orden
    // (bit-exacto). Con miles de APs a 20MP esto corre por pasada.
    points
        .par_iter()
        .map(|ap| {
            (
                get_area_complexity(
                    master_edges, w, h, ap.x as usize, ap.y as usize, box_size, 100.0,
                ),
                get_area_quality_laplacian(
                    master_mono, w, h, ap.x as usize, ap.y as usize, box_size,
                ),
            )
        })
        .unzip()
}

/// Estimates the per-AP residual shifts of ONE frame against the master.
/// Pure function (no app/IO state) so the entire alignment core can be
/// validated against synthetic ground truth in unit tests.
/// Returns (dx, dy, quality) per AP, already spatially filtered.
#[allow(clippy::too_many_arguments)]
fn compute_frame_local_shifts(
    master_edges: &[u16],
    ap_master_contrast: &[f32],
    ap_master_lap: &[f32],
    master_ds: &[u16],
    master_ds_w: usize,
    f_edges: &[u16],
    f_mono: &[u16],
    f_ds: &[u16],
    w_in: usize,
    h_in: usize,
    custom_points: &[ApPoint],
    ap_signal_valid: &[bool],
    ap_dark_ratio: &[f32],
    ap_limb_normal: &[Option<(f32, f32)>],
    render_dx: f32,
    render_dy: f32,
    box_size: usize,
    search_r: i32,
    is_surface: bool,
    // Enables the limb-AP damping for surface AND large lunar discs, without
    // changing the (planetary) spatial outlier filter selected by `is_surface`.
    limb_protect: bool,
    // TEXTURE-TRUST GATE: Laplaciano minimo del master para que un AP mida
    // (0.0 = gate apagado). Ver el comentario en el setup del pase.
    ap_lap_floor: f32,
    // Sólo activa AP-parallel cuando el lote no tiene suficientes frames para
    // ocupar el pool exterior. Nunca crea nuevos scratches.
    parallel_aps: bool,
    // Semillas gruesas 4× calculadas por wgpu. None conserva la ruta CPU
    // completa; cada entrada None cae localmente al matcher piramidal CPU.
    gpu_coarse: Option<&[Option<crate::gpu_analysis::SadMatch>]>,
) -> Vec<(f32, f32, f32)> {
    // En lotes normales se paraleliza sólo por frame. En capturas muy cortas,
    // el caller puede repartir los APs entre los workers ociosos; el lease es
    // por frame y los trabajos AP no toman scratch, por lo que no hay ciclo de
    // espera ni sobreasignación. Ambos caminos son indexados y conservan orden.
    let compute_ap = |(ap_i, ap): (usize, &ApPoint)| {
        if ap_i < ap_signal_valid.len() && !ap_signal_valid[ap_i] {
            return (0.0, 0.0, 0.0);
        }
        // AP sin textura real en el master (terminador oscuro, cielo residual):
        // su SAD mediria ruido → q=0 (sin medicion, el warp lo ignora) y se
        // ahorra TODO su coste por frame (SAD fino + LK).
        if ap_lap_floor > 0.0
            && ap_master_lap.get(ap_i).copied().unwrap_or(f32::MAX) < ap_lap_floor
        {
            return (0.0, 0.0, 0.0);
        }
        let ax = ap.x as f32;
        let ay = ap.y as f32;
        // Map Master AP to Source Frame: Source = Master + Shift
        let fx_i = (ax + render_dx).round() as i32;
        let fy_i = (ay + render_dy).round() as i32;
        let half_box = (box_size / 2) as i32;

        if fx_i >= half_box
            && fx_i < (w_in as i32 - half_box)
            && fy_i >= half_box
            && fy_i < (h_in as i32 - half_box)
        {
            let (dx, dy, sad) = if let Some(seed) = gpu_coarse
                .and_then(|v| v.get(ap_i))
                .copied()
                .flatten()
            {
                crate::alignment::refine_best_match_sad_from_coarse(
                    master_edges,
                    f_edges,
                    w_in,
                    h_in,
                    ax as usize,
                    ay as usize,
                    fx_i as usize,
                    fy_i as usize,
                    box_size,
                    seed.dx as f32,
                    seed.dy as f32,
                    4.0,
                )
            } else {
                find_best_match_sad_pyramid_fast(
                    master_edges, f_edges, w_in, h_in,
                    ax as usize, ay as usize, fx_i as usize, fy_i as usize,
                    box_size, search_r,
                    master_ds, f_ds, master_ds_w,
                )
            };
            // CRITICAL SUB-PIXEL FIX: the matcher measures the shift relative
            // to the ROUNDED search center (fx_i, fy_i), but the warp applies
            // it on top of the UNROUNDED (ax + render). The dropped rounding
            // fraction (uniform ±0.5 px, different per AP per frame) was never
            // compensated — equivalent to convolving the whole stack with a
            // 1-px box blur. Re-express the shift relative to (ax + render):
            let dx = dx - ((ax + render_dx) - fx_i as f32);
            let dy = dy - ((ay + render_dy) - fy_i as f32);
            // LK POLISH: pulido Gauss-Newton sobre los mismos mapas de bordes
            // partiendo del estimado SAD+equiangular (~0.1-0.2 px de residuo)
            // hasta ~0.02-0.05 px. Trabaja en el shift TOTAL master→frame y se
            // re-expresa como residuo local. Con patch plano, borde 1-D puro
            // (aperture problem) o divergencia devuelve None y el estimado SAD
            // queda intacto — LK nunca re-decide el matching, solo lo afina.
            let (dx, dy) = match refine_shift_lucas_kanade(
                master_edges,
                f_edges,
                w_in,
                h_in,
                ax as usize,
                ay as usize,
                render_dx + dx,
                render_dy + dy,
                box_size,
                3,
            ) {
                Some((tdx, tdy)) => (tdx - render_dx, tdy - render_dy),
                None => (dx, dy),
            };
            let avg_diff = sad as f32 / (box_size * box_size) as f32;
            let m_contrast = ap_master_contrast.get(ap_i).copied().unwrap_or(1000.0);
            let sensitivity = (m_contrast / 1000.0).clamp(0.5, 5.0) * 3500.0;
            let base_q = (-(avg_diff / sensitivity)).exp().clamp(0.001, 1.0);

            let local_lap =
                get_area_quality_laplacian(f_mono, w_in, h_in, ax as usize, ay as usize, box_size);
            let master_lap = ap_master_lap.get(ap_i).copied().unwrap_or(1.0);
            let regional_boost = (local_lap / master_lap.max(1.0)).clamp(0.1, 1.4);
            // Limb APs: project the measured shift onto the edge-normal — the
            // tangential component of a 1-D edge match is noise (aperture problem).
            let (dx, dy) = if let Some(Some((nx, ny))) = ap_limb_normal.get(ap_i) {
                let r = dx * nx + dy * ny;
                (r * nx, r * ny)
            } else {
                (dx, dy)
            };
            // Limb APs also keep a damped vote in the warp field.
            let limb_damp = if limb_protect {
                let d = ap_dark_ratio.get(ap_i).copied().unwrap_or(0.0);
                (1.0 - d * 0.8).clamp(0.2, 1.0)
            } else {
                1.0
            };
            let nq = (base_q * regional_boost * limb_damp).clamp(0.001, 1.0);
            (dx, dy, nq)
        } else {
            (0.0, 0.0, 0.0)
        }
        };
    let mut local_shifts: Vec<(f32, f32, f32)> = if parallel_aps {
        custom_points
            .par_iter()
            .enumerate()
            .map(&compute_ap)
            .collect()
    } else {
        custom_points.iter().enumerate().map(compute_ap).collect()
    };

    if !local_shifts.is_empty() {
        let (valid_mask, fallback_vectors) = if is_surface {
            crate::alignment::filter_ap_shifts_spatial_with_options(
                &local_shifts, custom_points, box_size, 5.0, 1.35, 1.0,
            )
        } else {
            crate::alignment::filter_ap_shifts_spatial(&local_shifts, custom_points, box_size, 3.5)
        };
        for i in 0..local_shifts.len() {
            if i < ap_signal_valid.len() && !ap_signal_valid[i] {
                local_shifts[i] = (0.0, 0.0, 0.0);
            } else if !valid_mask[i] {
                local_shifts[i].0 = fallback_vectors[i].0;
                local_shifts[i].1 = fallback_vectors[i].1;
                local_shifts[i].2 *= if is_surface { 0.65 } else { 0.35 };
            }
        }
    }

    local_shifts
}

/// AUTO RGB ALIGN (AS!4 "RGB Align"): atmospheric dispersion displaces the R
/// and B channels relative to G — from fractions of a pixel to several px at
/// low altitude — smearing color detail that the mono-luma alignment cannot
/// see. Measures each channel's global sub-pixel shift against G on the
/// STACKED result (huge SNR → precise measurement) and re-samples the channel
/// bilinearly. Shifts beyond ±12 px are treated as a failed measurement and
/// skipped; shifts below 0.05 px are a no-op. Returns (rdx, rdy, bdx, bdy).
fn align_stack_rgb_channels(stacked: &mut [f32], w: usize, h: usize) -> (f32, f32, f32, f32) {
    let n = w * h;
    if stacked.len() < n * 3 || w < 64 || h < 64 {
        return (0.0, 0.0, 0.0, 0.0);
    }

    let mut r = vec![0u16; n];
    let mut g = vec![0u16; n];
    let mut b = vec![0u16; n];
    for i in 0..n {
        r[i] = stacked[i * 3].clamp(0.0, 65535.0) as u16;
        g[i] = stacked[i * 3 + 1].clamp(0.0, 65535.0) as u16;
        b[i] = stacked[i * 3 + 2].clamp(0.0, 65535.0) as u16;
    }

    // MEASUREMENT SITE BY GRADIENT ENERGY: measuring at the blind image center
    // fails on a smooth disc interior (flat SAD minimum → noisy multi-px
    // "shift") and mis-corrected stacks showed a huge blue limb fringe. The
    // channel misalignment is only measurable where there is STRUCTURE — pick
    // the candidate window with the highest gradient energy (usually the limb
    // or a crater field).
    let roi_half = (w.min(h) / 6).clamp(24, 256);
    let box_size = (roi_half * 2).clamp(48, 1024);
    let margin = box_size / 2 + 12;
    let mut best_energy = -1.0f64;
    let mut cx = w / 2;
    let mut cy = h / 2;
    let grid_n = 6usize;
    for gy in 0..grid_n {
        for gx in 0..grid_n {
            let px = margin + (w - 2 * margin) * gx / (grid_n - 1).max(1);
            let py = margin + (h - 2 * margin) * gy / (grid_n - 1).max(1);
            let mut e = 0.0f64;
            let mut yy = py.saturating_sub(roi_half).max(2);
            while yy < (py + roi_half).min(h - 3) {
                let mut xx = px.saturating_sub(roi_half).max(2);
                while xx < (px + roi_half).min(w - 3) {
                    let i = yy * w + xx;
                    e += (g[i + 2] as i32 - g[i] as i32).unsigned_abs() as f64
                        + (g[i + 2 * w] as i32 - g[i] as i32).unsigned_abs() as f64;
                    xx += 4;
                }
                yy += 4;
            }
            if e > best_energy {
                best_energy = e;
                cx = px;
                cy = py;
            }
        }
    }

    let measure = |plane: &[u16]| -> (f32, f32) {
        let (dx, dy, _sad) =
            crate::alignment::find_best_match_sad(&g, plane, w, cx, cy, cx, cy, box_size, 8);
        // Dispersion is small; a large "measurement" is a failed match.
        if dx.is_finite() && dy.is_finite() && dx.abs() <= 6.0 && dy.abs() <= 6.0 {
            (dx, dy)
        } else {
            (0.0, 0.0)
        }
    };

    // SUB-PIXEL REFINEMENT: find_best_match_sad resolves to integer pixels,
    // but dispersion is typically a FRACTION of a pixel. Two-stage local grid
    // search (0.25 px → 0.05 px) on a bilinear-resampled SAD over the SAME
    // high-gradient ROI nails the fractional part.
    let gf: Vec<f32> = stacked.iter().skip(1).step_by(3).copied().collect();
    let (rx0, rx1) = (cx - roi_half, cx + roi_half);
    let (ry0, ry1) = (cy - roi_half, cy + roi_half);
    let sad_range = |plane: &[f32], dx: f32, dy: f32, y_from: usize, y_to: usize| -> f64 {
        let mut s = 0.0f64;
        let mut y = y_from;
        while y < y_to {
            let syf = y as f32 + dy;
            if syf >= 1.0 && syf < (h - 2) as f32 {
                let yy = syf as usize;
                let fy = syf - yy as f32;
                let mut x = rx0;
                while x < rx1 {
                    let sxf = x as f32 + dx;
                    if sxf >= 1.0 && sxf < (w - 2) as f32 {
                        let xx = sxf as usize;
                        let fx = sxf - xx as f32;
                        let i00 = yy * w + xx;
                        let v = plane[i00] * (1.0 - fx) * (1.0 - fy)
                            + plane[i00 + 1] * fx * (1.0 - fy)
                            + plane[i00 + w] * (1.0 - fx) * fy
                            + plane[i00 + w + 1] * fx * fy;
                        s += ((v - gf[y * w + x]) as f64).abs();
                    }
                    x += 2;
                }
            }
            y += 2;
        }
        s
    };
    let refine = |plane: &[f32], ix: f32, iy: f32| -> (f32, f32) {
        let mut best = (ix, iy);
        let mut best_s = f64::MAX;
        for &(step, span) in &[(0.25f32, 1.0f32), (0.05, 0.25)] {
            let (bx, by) = best;
            let mut sx = -span;
            while sx <= span + 1e-6 {
                let mut sy = -span;
                while sy <= span + 1e-6 {
                    let s = sad_range(plane, bx + sx, by + sy, ry0, ry1);
                    if s < best_s {
                        best_s = s;
                        best = (bx + sx, by + sy);
                    }
                    sy += step;
                }
                sx += step;
            }
        }
        best
    };

    let rf: Vec<f32> = stacked.iter().skip(0).step_by(3).copied().collect();
    let bfp: Vec<f32> = stacked.iter().skip(2).step_by(3).copied().collect();

    // IMPROVEMENT GATE (split-half validated): the correction is applied only
    // if it improves the channel match by ≥10% in BOTH independent halves of
    // the ROI. A real dispersion shift improves everywhere; a noise-driven
    // minimum (flat SAD on structure-less or lens-CA-dominated data — the case
    // that once painted a huge blue limb fringe) does not survive both halves.
    let validate = |plane: &[f32], dx: f32, dy: f32| -> (f32, f32) {
        if dx.abs() < 0.05 && dy.abs() < 0.05 {
            return (0.0, 0.0);
        }
        if dx.abs() > 6.0 || dy.abs() > 6.0 {
            return (0.0, 0.0);
        }
        let mid = (ry0 + ry1) / 2;
        let s0a = sad_range(plane, 0.0, 0.0, ry0, mid);
        let s1a = sad_range(plane, dx, dy, ry0, mid);
        let s0b = sad_range(plane, 0.0, 0.0, mid, ry1);
        let s1b = sad_range(plane, dx, dy, mid, ry1);
        // SMOOTHING-BIAS COMPENSATION: bilinear resampling of a FRACTIONAL
        // shift averages 4 neighbours and shrinks the plane's noise, lowering
        // the SAD by up to ~13% with NO real alignment gain. The expected
        // noise-only shrink is √((1+Σw²)/2) for the bilinear weights — scale
        // the acceptance threshold by it so fractional shifts must beat the
        // smoothing, not ride on it.
        let sw2 = {
            let fx = (dx - dx.floor()) as f64;
            let fy = (dy - dy.floor()) as f64;
            let (w00, w10, w01, w11) = (
                (1.0 - fx) * (1.0 - fy),
                fx * (1.0 - fy),
                (1.0 - fx) * fy,
                fx * fy,
            );
            w00 * w00 + w10 * w10 + w01 * w01 + w11 * w11
        };
        let smoothing_comp = ((1.0 + sw2) / 2.0).sqrt();
        let thresh = 0.90 * smoothing_comp;
        if s1a < s0a * thresh && s1b < s0b * thresh {
            (dx, dy)
        } else {
            (0.0, 0.0)
        }
    };
    let (ri_x, ri_y) = measure(&r);
    let (bi_x, bi_y) = measure(&b);
    let (rdx, rdy) = {
        let v = refine(&rf, ri_x, ri_y);
        validate(&rf, v.0, v.1)
    };
    let (bdx, bdy) = {
        let v = refine(&bfp, bi_x, bi_y);
        validate(&bfp, v.0, v.1)
    };

    // find_best_match_sad follows the render convention (shift = channel_pos −
    // ref_pos), so the correction samples aligned(p) = ch(p + shift) — the same
    // way the stacking warp consumes render_dx. Verified by the synthetic
    // regression test (known physical shift → aligned residual ≈ 0).
    let shift_channel_inplace = |stacked: &mut [f32], ch: usize, dx: f32, dy: f32| {
        if dx.abs() < 0.05 && dy.abs() < 0.05 {
            return;
        }
        let plane: Vec<f32> = stacked.iter().skip(ch).step_by(3).copied().collect();
        stacked
            .par_chunks_mut(w * 3)
            .enumerate()
            .for_each(|(y, row)| {
                let syf = y as f32 + dy;
                if syf < 0.0 || syf >= (h - 1) as f32 {
                    return; // keep original border rows
                }
                let y0 = syf as usize;
                let fy = syf - y0 as f32;
                for x in 0..w {
                    let sxf = x as f32 + dx;
                    if sxf < 0.0 || sxf >= (w - 1) as f32 {
                        continue;
                    }
                    let x0 = sxf as usize;
                    let fx = sxf - x0 as f32;
                    let i00 = y0 * w + x0;
                    row[x * 3 + ch] = plane[i00] * (1.0 - fx) * (1.0 - fy)
                        + plane[i00 + 1] * fx * (1.0 - fy)
                        + plane[i00 + w] * (1.0 - fx) * fy
                        + plane[i00 + w + 1] * fx * fy;
                }
            });
    };
    shift_channel_inplace(stacked, 0, rdx, rdy);
    shift_channel_inplace(stacked, 2, bdx, bdy);

    (rdx, rdy, bdx, bdy)
}

/// KAPPA-SIGMA REJECTION BOUNDS (AS!4-grade robustness at high stack %):
/// per-pixel [mean − k·σ, mean + k·σ] acceptance window from the PASS-1
/// weighted statistics. `direct_w2` supplies the Kish effective sample size
/// N_eff=(Σw)²/Σw², so drizzle coverage and lucky weights cannot masquerade as
/// many independent observations. σ is the population spread; applying a
/// Bessel correction here would let the tested outlier inflate its own window.
/// Instead `sigma_clip_effective_k` approximates an externally studentized
/// threshold, narrowing only small-N windows and tending to the requested k.
/// `sigma_floor` keeps zero-variance pixels useful. Insufficient N_eff leaves
/// (-∞,+∞) bounds: no rejection is safer than a statistically unsupported one.
const MIN_SIGMA_CLIP_N_EFF: f64 = 4.5;

#[inline]
fn sigma_clip_effective_k(k: f64, n_eff: f64) -> f64 {
    if !k.is_finite() || !n_eff.is_finite() || k <= 0.0 || n_eff <= 1.0 {
        return 0.0;
    }
    // Aproximación leave-one-out / externally studentized:
    //   k_eff = k·sqrt((N_eff−1)/(N_eff+k²)).
    // Un outlier incluido en N=5 ya no puede autoensanchar μ±kσ hasta contenerse;
    // para N grande el factor converge monotonamente a 1.
    k * ((n_eff - 1.0) / (n_eff + k * k)).max(0.0).sqrt()
}

fn build_sigma_clip_bounds(
    acc: &GradientDomainStacker,
    k: f32,
    sigma_floor: f32,
) -> Result<(Vec<f32>, Vec<f32>), String> {
    let n = acc.direct.len();
    let mut lo = try_filled_vec(n, f32::MIN, "límites sigma inferiores")?;
    let mut hi = try_filled_vec(n, f32::MAX, "límites sigma superiores")?;
    if acc.m2.len() != n || acc.direct_w2.len() != n {
        return Ok((lo, hi));
    }
    let k = (k as f64).max(0.0);
    let sigma_floor = (sigma_floor as f64).max(0.0);
    for i in 0..n {
        let w = acc.direct_w[i];
        let w2 = acc.direct_w2[i];
        if !w.is_finite() || !w2.is_finite() || w <= 1e-12 || w2 <= 1e-18 {
            continue;
        }
        let n_eff = w * w / w2;
        if !n_eff.is_finite() || n_eff < MIN_SIGMA_CLIP_N_EFF {
            continue;
        }
        let k_eff = sigma_clip_effective_k(k, n_eff);
        if k_eff <= 0.0 {
            continue;
        }
        let mean = acc.direct[i] / w;
        let var_population = (acc.m2[i] / w - mean * mean).max(0.0);
        let sigma = var_population.sqrt().max(sigma_floor);
        lo[i] = (mean - k_eff * sigma) as f32;
        hi[i] = (mean + k_eff * sigma) as f32;
    }
    Ok((lo, hi))
}

/// HARD REJECTION against the per-pixel bounds: an out-of-bounds pixel gets
/// its coverage weight zeroed, so that frame contributes NOTHING there and the
/// stacked pixel becomes the mean of the clean frames only. Rejection (not
/// clamping) matters because an extreme outlier inflates the pass-1 σ of its
/// own pixel — clamping to μ±kσ would still leave most of the artifact energy
/// in; zero-weighting removes it completely. The remaining frames keep full
/// weight → no holes, no seams (a pixel would need ALL frames rejected to go
/// empty, which needs a real scene change, not a transient).
fn apply_sigma_rejection(vals: &[f32], coverage: &mut [f32], lo: &[f32], hi: &[f32]) {
    let n = vals.len().min(lo.len()).min(hi.len()).min(coverage.len());
    for i in 0..n {
        if coverage[i] > 1e-9 {
            let v = vals[i];
            if v < lo[i] || v > hi[i] {
                coverage[i] = 0.0;
            }
        }
    }
}

/// Detects low-coverage border rows/columns of the stacked output.
/// A border line is "bad" when fewer than half of its pixels reach 35% of the
/// median per-pixel stacking weight. Crop is capped to `max_crop_frac` per
/// side so the framing never changes drastically. Returns None when no crop
/// is needed.
fn compute_low_coverage_crop(
    weights: &[f64],
    w: usize,
    h: usize,
    max_crop_frac: f32,
) -> Option<(usize, usize, usize, usize)> {
    if w < 64 || h < 64 || weights.len() < w * h {
        return None;
    }

    // Median of positive weights (sampled)
    let mut sample: Vec<f64> = weights
        .iter()
        .step_by(7)
        .copied()
        .filter(|&v| v > 1e-9)
        .collect();
    if sample.len() < 64 {
        return None;
    }
    sample.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sample[sample.len() / 2];
    let thresh = median * 0.35;

    let row_is_bad = |y: usize| -> bool {
        let row = y * w;
        let mut good = 0usize;
        for x in (0..w).step_by(2) {
            if weights[row + x] >= thresh {
                good += 1;
            }
        }
        good * 2 < w / 2 // fewer than 50% of sampled pixels are well covered
    };
    let col_is_bad = |x: usize| -> bool {
        let mut good = 0usize;
        for y in (0..h).step_by(2) {
            if weights[y * w + x] >= thresh {
                good += 1;
            }
        }
        good * 2 < h / 2
    };

    let max_crop_x = ((w as f32 * max_crop_frac) as usize).max(1);
    let max_crop_y = ((h as f32 * max_crop_frac) as usize).max(1);

    let mut x0 = 0;
    while x0 < max_crop_x && col_is_bad(x0) {
        x0 += 1;
    }
    let mut x1 = w;
    while x1 > w - max_crop_x && col_is_bad(x1 - 1) {
        x1 -= 1;
    }
    let mut y0 = 0;
    while y0 < max_crop_y && row_is_bad(y0) {
        y0 += 1;
    }
    let mut y1 = h;
    while y1 > h - max_crop_y && row_is_bad(y1 - 1) {
        y1 -= 1;
    }

    if x0 == 0 && y0 == 0 && x1 == w && y1 == h {
        return None;
    }
    if x1 <= x0 + 32 || y1 <= y0 + 32 {
        return None; // degenerate, keep original
    }
    Some((x0, y0, x1, y1))
}

/// Spatial sigma-clipping: detects pixels >2.5sigma from neighborhood median.
fn reject_spatial_outliers_u16(img: &mut [u16], w: usize, h: usize) {
    let npix = w * h;
    if npix < 9 { return; }
    let mut fixes: Vec<(usize, u16)> = Vec::new();
    for ch in 0..3 {
        fixes.clear();
        for y in 1..(h - 1) {
            for x in 1..(w - 1) {
                let center_idx = (y * w + x) * 3 + ch;
                let center = img[center_idx] as f32;
                let mut neighbors = [0.0f32; 8];
                let mut n = 0;
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        if dy == 0 && dx == 0 { continue; }
                        let ny = (y as i32 + dy) as usize;
                        let nx = (x as i32 + dx) as usize;
                        neighbors[n] = img[(ny * w + nx) * 3 + ch] as f32;
                        n += 1;
                    }
                }
                let mut sum = 0.0f32; let mut sum_sq = 0.0f32;
                for &v in &neighbors { sum += v; sum_sq += v * v; }
                let mean = sum / 8.0;
                let std = ((sum_sq / 8.0) - (mean * mean)).max(0.0).sqrt();
                let threshold = (std * 2.5).max(200.0);
                if (center - mean).abs() > threshold {
                    neighbors.sort_unstable_by(f32::total_cmp);
                    let median = (neighbors[3] + neighbors[4]) / 2.0;
                    fixes.push((center_idx, median.clamp(0.0, 65535.0) as u16));
                }
            }
        }
        for &(idx, val) in &fixes { img[idx] = val; }
    }
}

fn repair_stack_coverage_holes_rgb_f32(stacked: &mut [f32], weights: &[f64], w: usize, h: usize) {
    let n = w * h;
    if stacked.len() < n * 3 || weights.len() < n || w < 3 || h < 3 {
        return;
    }

    for _ in 0..4 {
        let prev = stacked.to_vec();
        let mut fixes: Vec<(usize, [f32; 3])> = Vec::new();

        for y in 1..h - 1 {
            for x in 1..w - 1 {
                let i = y * w + x;
                let base = i * 3;
                let has_weight = weights[i] > 1e-8;
                let looks_empty = prev[base + 1] <= 1.0;
                if has_weight && !looks_empty {
                    continue;
                }

                let mut sum = [0.0f32; 3];
                let mut count = 0.0f32;
                for dy in -1isize..=1 {
                    for dx in -1isize..=1 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let ni = (y as isize + dy) as usize * w + (x as isize + dx) as usize;
                        let nb = ni * 3;
                        if prev[nb + 1] > 1.0 {
                            sum[0] += prev[nb];
                            sum[1] += prev[nb + 1];
                            sum[2] += prev[nb + 2];
                            count += 1.0;
                        }
                    }
                }

                if count >= 3.0 {
                    fixes.push((base, [sum[0] / count, sum[1] / count, sum[2] / count]));
                }
            }
        }

        if fixes.is_empty() {
            break;
        }
        for (base, rgb) in fixes {
            stacked[base] = rgb[0];
            stacked[base + 1] = rgb[1];
            stacked[base + 2] = rgb[2];
        }
    }

    if w >= 2 && h >= 2 {
        for x in 0..w {
            let top = x * 3;
            let below = (w + x) * 3;
            if stacked[top + 1] <= 1.0 && stacked[below + 1] > 1.0 {
                let rgb = [stacked[below], stacked[below + 1], stacked[below + 2]];
                stacked[top..top + 3].copy_from_slice(&rgb);
            }
            let bottom = ((h - 1) * w + x) * 3;
            let above = ((h - 2) * w + x) * 3;
            if stacked[bottom + 1] <= 1.0 && stacked[above + 1] > 1.0 {
                let rgb = [stacked[above], stacked[above + 1], stacked[above + 2]];
                stacked[bottom..bottom + 3].copy_from_slice(&rgb);
            }
        }
        for y in 0..h {
            let left = (y * w) * 3;
            let right_neighbor = (y * w + 1) * 3;
            if stacked[left + 1] <= 1.0 && stacked[right_neighbor + 1] > 1.0 {
                let rgb = [
                    stacked[right_neighbor],
                    stacked[right_neighbor + 1],
                    stacked[right_neighbor + 2],
                ];
                stacked[left..left + 3].copy_from_slice(&rgb);
            }
            let right = (y * w + (w - 1)) * 3;
            let left_neighbor = (y * w + (w - 2)) * 3;
            if stacked[right + 1] <= 1.0 && stacked[left_neighbor + 1] > 1.0 {
                let rgb = [
                    stacked[left_neighbor],
                    stacked[left_neighbor + 1],
                    stacked[left_neighbor + 2],
                ];
                stacked[right..right + 3].copy_from_slice(&rgb);
            }
        }
    }
}

#[allow(dead_code)]
fn compute_surface_signal_mask_u16(mono: &[u16], w: usize, h: usize, feather: usize) -> Vec<f32> {
    let n = w * h;
    if mono.len() < n || n == 0 {
        return vec![1.0; n];
    }
    let luma: Vec<f32> = mono.iter().take(n).map(|&v| v as f32).collect();
    compute_surface_signal_mask_f32(&luma, w, h, feather)
}

#[allow(dead_code)]
fn compute_surface_signal_mask_f32(luma: &[f32], w: usize, h: usize, feather: usize) -> Vec<f32> {
    let n = w * h;
    if luma.len() < n || n == 0 || w < 8 || h < 8 {
        return vec![1.0; n];
    }

    let coarse = atrous_smooth(luma, w, h, 4);
    let thresh = otsu_threshold(&coarse);
    let mut binary = vec![0.0f32; n];
    let mut fg = 0usize;
    for (i, &v) in coarse.iter().enumerate() {
        if v > thresh {
            binary[i] = 1.0;
            fg += 1;
        }
    }

    let fg_ratio = fg as f32 / n as f32;
    let mut border_dark = 0usize;
    let mut border_total = 0usize;
    for x in 0..w {
        border_total += 2;
        if coarse[x] <= thresh { border_dark += 1; }
        if coarse[(h - 1) * w + x] <= thresh { border_dark += 1; }
    }
    for y in 1..h.saturating_sub(1) {
        border_total += 2;
        if coarse[y * w] <= thresh { border_dark += 1; }
        if coarse[y * w + (w - 1)] <= thresh { border_dark += 1; }
    }
    let border_dark_ratio = border_dark as f32 / border_total.max(1) as f32;

    // If there is no border-connected sky, a threshold would split real solar
    // texture/dark filaments. In that case expose the full field to AP/refinement.
    if border_dark_ratio < 0.20 || fg_ratio < 0.08 || fg_ratio > 0.92 {
        return vec![1.0; n];
    }

    let close_radius = 3usize.min(w.min(h) / 16).max(1);
    let closed = morphological_erode(
        &morphological_dilate(&binary, w, h, close_radius),
        w,
        h,
        close_radius,
    );
    let dist = compute_distance_transform(&closed, w, h);
    let feather = feather.max(1) as f32;
    dist.iter()
        .map(|&d| {
            if d >= feather {
                1.0
            } else {
                let t = (d / feather).clamp(0.0, 1.0);
                t * t * (3.0 - 2.0 * t)
            }
        })
        .collect()
}

#[allow(dead_code)]
fn apply_surface_mono_intelligent_refinement(img: &mut [u16], w: usize, h: usize) {
    let n = w * h;
    if img.len() < n * 3 || n == 0 || w < 16 || h < 16 {
        return;
    }

    let mut luma = vec![0.0f32; n];
    for i in 0..n {
        luma[i] = img[i * 3 + 1] as f32;
    }

    let surface_mask = compute_surface_signal_mask_f32(&luma, w, h, 14);
    let deconv_mask = morphological_erode(&surface_mask, w, h, 3);
    let noise = estimate_stack_noise(&luma, w, h).max(1.0);
    let snr_map = compute_snr_map(&luma, w, h, noise);

    // Compact PSF: enough to undo mild seeing blur without RL ringing on the limb.
    let psf_radius = 2usize;
    let psf = PsfEstimator { psf_radius }.estimate_gaussian_airy(1.45, 0.10);
    let deconvolver = TvRlDeconvolver {
        max_iterations: 5,
        tv_lambda: 0.035,
        convergence_eps: 0.00008,
        snr_floor: 5.0,
    };
    let deconv = deconvolver.deconvolve(
        &luma,
        w,
        h,
        &psf,
        psf_radius * 2 + 1,
        &snr_map,
        &deconv_mask,
    );

    let structure_ref = atrous_smooth(&luma, w, h, 2);
    let mut refined = vec![0.0f32; n];
    for i in 0..n {
        let snr_gate = ((snr_map[i] - 4.0) / 18.0).clamp(0.0, 1.0).powf(1.2);
        let mask_gate = deconv_mask[i].powf(1.35);
        let local_structure = (luma[i] - structure_ref[i]).abs();
        let limit = (noise * 5.0 + local_structure * 1.5 + 128.0).max(noise * 4.0);
        let delta = (deconv[i] - luma[i]).clamp(-limit, limit);
        refined[i] = luma[i] + delta * 0.24 * snr_gate * mask_gate;
    }

    let decomp = AtrousDecomposition::decompose(&refined, w, h, 5);
    let mut result = decomp.coarse_residual.clone();
    let scale_targets = [1.04f32, 1.58, 1.46, 1.23, 1.08];

    for (scale, detail) in decomp.detail_planes.iter().enumerate() {
        let target_amp = scale_targets.get(scale).copied().unwrap_or(1.0);
        let noise_thresh = noise * (1.35_f32).powi(scale as i32) * if scale == 0 { 2.2 } else { 1.15 };

        for (i, (&d, out)) in detail.iter().zip(result.iter_mut()).enumerate() {
            if target_amp <= 1.0 {
                *out += d;
                continue;
            }

            let abs_d = d.abs();
            let signal_gate = if abs_d > noise_thresh {
                ((abs_d - noise_thresh) / (noise_thresh * 2.0 + 1.0))
                    .clamp(0.0, 1.0)
                    .powf(0.65)
            } else {
                0.0
            };
            let snr_gate = ((snr_map[i] - 3.0) / 15.0).clamp(0.0, 1.0).powf(1.1);
            let mask_gate = surface_mask[i].powf(if scale <= 1 { 1.7 } else { 1.25 });
            let amp = 1.0 + (target_amp - 1.0) * signal_gate * snr_gate * mask_gate;
            *out += d * amp;
        }
    }

    let coarse_guard = atrous_smooth(&luma, w, h, 3);
    for i in 0..n {
        let local_structure = (luma[i] - coarse_guard[i]).abs();
        let delta_limit = noise * 8.0 + local_structure * 2.0 + luma[i].abs() * 0.12 + 256.0;
        let delta = (result[i] - luma[i]).clamp(-delta_limit, delta_limit);
        let final_luma = (luma[i] + delta * surface_mask[i].powf(1.25)).clamp(0.0, 65535.0) as u16;
        img[i * 3] = final_luma;
        img[i * 3 + 1] = final_luma;
        img[i * 3 + 2] = final_luma;
    }
}


/// Edge-aware USM: scales sharpening amount by local edge strength.
/// High-contrast edges get less sharpening (avoids halos),
/// low-contrast texture gets full sharpening (reveals planetary detail).
fn apply_edge_aware_usm_u16(img: &[u16], w: usize, h: usize, radius: f32, amount: f32) -> Vec<u16> {
    // 1. Split into planar channels
    let mut r = Vec::with_capacity(w * h);
    let mut g = Vec::with_capacity(w * h);
    let mut b = Vec::with_capacity(w * h);
    for i in 0..(w * h) {
        r.push(img[i * 3] as f32);
        g.push(img[i * 3 + 1] as f32);
        b.push(img[i * 3 + 2] as f32);
    }

    // 2. Gaussian blur (same as standard USM)
    let r_blur = apply_gaussian_blur_safe(&r, w, h, radius);
    let g_blur = apply_gaussian_blur_safe(&g, w, h, radius);
    let b_blur = apply_gaussian_blur_safe(&b, w, h, radius);

    // 3. Compute per-pixel edge strength from green channel gradient magnitude
    // Use simple Sobel-like gradient on green (most detail in Bayer)
    let mut edge_map = vec![0.0f32; w * h];
    for y in 1..(h - 1) {
        for x in 1..(w - 1) {
            let idx = y * w + x;
            // Horizontal gradient
            let gx = g[idx + 1] - g[idx - 1];
            // Vertical gradient
            let gy = g[idx + w] - g[idx - w];
            edge_map[idx] = (gx * gx + gy * gy).sqrt();
        }
    }

    let mut edge_values: Vec<f32> = edge_map.iter().copied().filter(|&v| v > 0.0).collect();
    let p95 = if edge_values.len() > 10 {
        let idx_95 = edge_values.len() * 95 / 100;
        edge_values.select_nth_unstable_by(idx_95, f32::total_cmp);
        edge_values[idx_95]
    } else {
        1000.0
    };
    let edge_threshold = p95.max(100.0);

    // 5. Apply edge-aware USM: reduce amount near strong edges
    let mut out = Vec::with_capacity(img.len());
    for i in 0..(w * h) {
        // Edge attenuation: 1.0 at no edge, 0.15 at strong edge (never fully zero)
        let edge_factor = 1.0 - (edge_map[i] / edge_threshold).min(0.85);
        let local_amount = amount * edge_factor;

        let rv = r[i] + (r[i] - r_blur[i]) * local_amount;
        let gv = g[i] + (g[i] - g_blur[i]) * local_amount;
        let bv = b[i] + (b[i] - b_blur[i]) * local_amount;

        out.push(rv.clamp(0.0, 65535.0) as u16);
        out.push(gv.clamp(0.0, 65535.0) as u16);
        out.push(bv.clamp(0.0, 65535.0) as u16);
    }
    out
}

fn apply_unsharp_mask_u16(img: &[u16], w: usize, h: usize, radius: f32, amount: f32) -> Vec<u16> {
    let mut planar = vec![0.0f32; img.len()];
    for i in 0..img.len() {
        planar[i] = img[i] as f32;
    }

    let mut r = Vec::with_capacity(w * h);
    let mut g = Vec::with_capacity(w * h);
    let mut b = Vec::with_capacity(w * h);

    for i in 0..w * h {
        r.push(planar[i * 3]);
        g.push(planar[i * 3 + 1]);
        b.push(planar[i * 3 + 2]);
    }

    let r_blur = apply_gaussian_blur_safe(&r, w, h, radius);
    let g_blur = apply_gaussian_blur_safe(&g, w, h, radius);
    let b_blur = apply_gaussian_blur_safe(&b, w, h, radius);

    for i in 0..w * h {
        r[i] = r[i] + (r[i] - r_blur[i]) * amount;
        g[i] = g[i] + (g[i] - g_blur[i]) * amount;
        b[i] = b[i] + (b[i] - b_blur[i]) * amount;
    }

    let mut out = Vec::with_capacity(img.len());
    for i in 0..w * h {
        out.push(r[i].clamp(0.0, 65535.0) as u16);
        out.push(g[i].clamp(0.0, 65535.0) as u16);
        out.push(b[i].clamp(0.0, 65535.0) as u16);
    }
    out
}

fn auto_contrast_stretch_u16(img: &[u16], _w: usize, _h: usize) -> Vec<u16> {
    let mut hist = vec![0u32; 65536];
    for &val in img {
        hist[val as usize] += 1;
    }

    let total_pixels = img.len() as u32;
    let cutoff = (total_pixels as f32 * 0.0005) as u32;

    let mut min_val = 0;
    let mut count = 0;
    for i in 0..65536 {
        count += hist[i];
        if count > cutoff {
            min_val = i as u16;
            break;
        }
    }

    let mut max_val = 65535;
    count = 0;
    for i in (0..65536).rev() {
        count += hist[i];
        if count > cutoff {
            max_val = i as u16;
            break;
        }
    }

    if max_val <= min_val {
        return img.to_vec();
    }

    let range = (max_val - min_val) as f32;
    let scale = 65535.0 / range;

    let mut out = Vec::with_capacity(img.len());
    for &val in img {
        let v_f = val as f32;
        let stretched = (v_f - min_val as f32) * scale;
        out.push(stretched.clamp(0.0, 65535.0) as u16);
    }
    out
}

// ==========================================
// PLANETARY EXPOSURE GAIN (Fix 1)
// ==========================================
/// Computes the exposure scaling gain for planetary targets using a P90-based approach.
/// Unlike global mean matching, this ignores the black sky background that dominates
/// small planet images (Mars, Uranus, Neptune) and matches only the bright disk pixels.
///
/// Returns a gain factor in [0.85, 1.25] to safely brighten or correct the stack
/// without saturating the planetary disk.
fn compute_planetary_exposure_gain(stack: &[u16], ref_rgb: &[u16], _ref_mean: f32) -> f32 {
    if stack.is_empty() || ref_rgb.is_empty() {
        return 1.0;
    }

    // FIX: Use adaptive sky_cutoff based on actual data range, not a hardcoded 800.
    // For 8-bit data (0-255 as u16) a cutoff of 800 discards ALL pixels → black result.
    // We use 5% of the frame maximum, which correctly brackets the sky for any bit depth.
    let stack_max = stack.iter().copied().max().unwrap_or(1) as f32;
    let ref_max   = ref_rgb.iter().copied().max().unwrap_or(1) as f32;
    let sky_cutoff_stack = (stack_max * 0.05).max(1.0);
    let sky_cutoff_ref   = (ref_max   * 0.05).max(1.0);

    // P90 of stack disk pixels
    let mut stack_bright: Vec<f32> = stack.iter()
        .filter(|&&v| v as f32 > sky_cutoff_stack)
        .map(|&v| v as f32)
        .collect();

    if stack_bright.is_empty() {
        return 1.0;
    }
    stack_bright.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p90_idx   = (stack_bright.len() * 90 / 100).min(stack_bright.len() - 1);
    let stack_p90 = stack_bright[p90_idx];
    if stack_p90 < 1.0 { return 1.0; }

    // P90 of reference disk pixels
    let mut ref_bright: Vec<f32> = ref_rgb.iter()
        .filter(|&&v| v as f32 > sky_cutoff_ref)
        .map(|&v| v as f32)
        .collect();

    let ref_p90 = if ref_bright.is_empty() {
        stack_p90  // Identity: no change if reference is empty
    } else {
        ref_bright.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = (ref_bright.len() * 90 / 100).min(ref_bright.len() - 1);
        ref_bright[idx]
    };

    // Clamp: never darken (>= 1.0), allow up to 2x brightening for faint planets
    (ref_p90 / stack_p90).clamp(1.0, 2.0)
}

fn percentile_u16(data: &[u16], percentile: usize) -> f32 {
    if data.is_empty() {
        return 0.0;
    }
    let mut sample: Vec<u16> = data.iter().step_by(3).copied().collect();
    if sample.is_empty() {
        return 0.0;
    }
    sample.sort_unstable();
    let idx = (sample.len() * percentile / 100).min(sample.len() - 1);
    sample[idx] as f32
}

/// Percentil de UN canal (offset 0=R, 1=G, 2=B) de un buffer RGB interleaved.
/// Submuestreado (cada `stride` px) para no ordenar millones de valores.
fn percentile_channel(data: &[u16], channel: usize, percentile: usize) -> f32 {
    let n = data.len() / 3;
    if n == 0 {
        return 0.0;
    }
    let stride = (n / 40000).max(1);
    let mut sample: Vec<u16> = (0..n)
        .step_by(stride)
        .map(|i| data[i * 3 + channel])
        .collect();
    if sample.is_empty() {
        return 0.0;
    }
    sample.sort_unstable();
    let idx = (sample.len() * percentile / 100).min(sample.len() - 1);
    sample[idx] as f32
}

/// Percentil de la LUMINANCIA (0.299R+0.587G+0.114B) — para el punto negro y
/// los tests de "hay fondo oscuro", sin sesgo hacia un canal concreto.
fn percentile_luma_rgb(data: &[u16], percentile: usize) -> f32 {
    let n = data.len() / 3;
    if n == 0 {
        return 0.0;
    }
    let stride = (n / 40000).max(1);
    let mut sample: Vec<f32> = (0..n)
        .step_by(stride)
        .map(|i| {
            let o = i * 3;
            0.299 * data[o] as f32 + 0.587 * data[o + 1] as f32 + 0.114 * data[o + 2] as f32
        })
        .collect();
    if sample.is_empty() {
        return 0.0;
    }
    sample.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = (sample.len() * percentile / 100).min(sample.len() - 1);
    sample[idx]
}

/// Normalizacion "Normalize Stack" estilo AS!4 CANAL-SEGURA para superficie /
/// disco lunar grande. Fija el bug del quemado: el punto blanco es el maximo
/// GLOBAL sobre los tres canales (antes percentile_u16 usaba solo el rojo, y
/// verde/azul mas brillantes se recortaban duro = manchas amarillo-verdes).
/// El punto negro se resta POR CANAL cuando hay cielo oscuro real, dejando el
/// fondo neutro (mata el cast azul del pedestal). Garantia: ningun canal se
/// pasa de 65535 (el pico global cae en 0.97·full) → sin recorte, sin quemado.
fn normalize_surface_stack_channelsafe(final_u16: &mut [u16]) {
    let ch_max = [
        percentile_channel(final_u16, 0, 100),
        percentile_channel(final_u16, 1, 100),
        percentile_channel(final_u16, 2, 100),
    ];
    let p_max = ch_max[0].max(ch_max[1]).max(ch_max[2]).max(1.0);
    let luma_p1 = percentile_luma_rgb(final_u16, 1);
    let luma_p99 = percentile_luma_rgb(final_u16, 99).max(1.0);
    let has_dark_sky = luma_p1 < luma_p99 * 0.15;
    let black_ch: [f32; 3] = if has_dark_sky {
        [
            percentile_channel(final_u16, 0, 1) * 0.95,
            percentile_channel(final_u16, 1, 1) * 0.95,
            percentile_channel(final_u16, 2, 1) * 0.95,
        ]
    } else {
        [0.0, 0.0, 0.0]
    };
    // min de los negros → denominador mas grande → ganancia conservadora que
    // garantiza que NINGUN canal (ni el mas brillante) supere 0.97·full.
    let max_black = black_ch[0].min(black_ch[1]).min(black_ch[2]);
    let gain = ((0.97 * 65535.0) / (p_max - max_black).max(1.0)).clamp(1.0, 8.0);
    if gain > 1.01 || has_dark_sky {
        for px in final_u16.chunks_exact_mut(3) {
            for c in 0..3 {
                let nv = ((px[c] as f32) - black_ch[c]).max(0.0) * gain + 0.5;
                px[c] = nv.min(65535.0) as u16;
            }
        }
    }
}

fn compute_surface_exposure_gain(stack: &[u16], ref_rgb: &[u16]) -> f32 {
    let stack_p92 = percentile_u16(stack, 92);
    let ref_p92 = percentile_u16(ref_rgb, 92);

    if stack_p92 < 1.0 || ref_p92 < 1.0 {
        return 1.0;
    }

    // Surface stacks should never be darkened during integration. Low-light Moon/Sun
    // captures need highlight-preserving lift so the post pipeline has signal to denoise.
    (ref_p92 / stack_p92).clamp(1.0, 1.60)
}

// ==========================================
// ELITE V4: DENSE QUALITY MAP (Per-Pixel Precision)
// ==========================================

pub struct DenseQualityMap {
    /// scores[pixel_idx] = calidad local normalizada [0,1]
    pub scores: Vec<f32>,
    pub width: usize,
    pub height: usize,
    /// `false` significa que el frame no contiene contraste espacial suficiente
    /// para distinguir textura real de energia estacionaria de ruido. En ese
    /// caso `scores` es exactamente 1.0 (peso neutral).
    pub reliable: bool,
}

impl DenseQualityMap {
    pub fn build(frame: &[f32], w: usize, h: usize, radius: usize) -> Self {
        let Some(n) = w.checked_mul(h) else {
            return Self { scores: Vec::new(), width: w, height: h, reliable: false };
        };
        let neutral = || Self {
            scores: vec![1.0; n],
            width: w,
            height: h,
            reliable: false,
        };
        // Ademas de evitar underflow en `1..h-1`, esta guarda convierte un mapa
        // incompleto/NaN en no-op: nunca debe eliminar señal por metadatos malos.
        if n == 0
            || w < 3
            || h < 3
            || frame.len() < n
            || frame[..n].iter().any(|v| !v.is_finite())
        {
            return neutral();
        }

        let mut lap = vec![0.0f32; n];
        for y in 1..h - 1 {
            let row = y * w;
            let prev = (y - 1) * w;
            let next = (y + 1) * w;
            for x in 1..w - 1 {
                let l = -8.0 * frame[row + x]
                    + frame[row + x - 1]
                    + frame[row + x + 1]
                    + frame[prev + x]
                    + frame[next + x]
                    + frame[prev + x - 1]
                    + frame[prev + x + 1]
                    + frame[next + x - 1]
                    + frame[next + x + 1];
                lap[row + x] = l * l;
            }
        }

        let sat = Self::build_sat(&lap, w, h);
        let mut quality_map = vec![0.0f32; w * h];
        for y in 0..h {
            let row = y * w;
            for x in 0..w {
                let x0 = x.saturating_sub(radius);
                let y0 = y.saturating_sub(radius);
                let x1 = (x + radius).min(w - 1);
                let y1 = (y + radius).min(h - 1);
                let area = ((x1 - x0 + 1) * (y1 - y0 + 1)) as f32;
                quality_map[row + x] = Self::query_sat(&sat, w, x0, y0, x1, y1) / area;
            }
        }

        // Percentiles robustos O(N): el sort completo O(N log N) por frame era
        // visible en capturas 4K/8K. Repetir select_nth para cinco cuantiles
        // conserva la definicion y sigue siendo lineal con memoria constante.
        let percentile = |values: &mut [f32], q: f32| -> f32 {
            let idx = ((values.len() - 1) as f32 * q.clamp(0.0, 1.0)).floor() as usize;
            let (_, value, _) = values.select_nth_unstable_by(idx, |a, b| a.total_cmp(b));
            *value
        };
        let mut order = quality_map.clone();
        let p02 = percentile(&mut order, 0.02);
        let p10 = percentile(&mut order, 0.10);
        let p50 = percentile(&mut order, 0.50);
        let p90 = percentile(&mut order, 0.90);
        let p98 = percentile(&mut order, 0.98);
        let range = p98 - p02;

        // TEXTURE RELIABILITY GATE. En un campo plano con ruido blanco, la
        // energia laplaciana local varia algo por azar y P02..P98 la estiraba a
        // [0,1], seleccionando el ruido como si fuera detalle. La textura real
        // produce una cola/estructura espacial mucho mas marcada. El gate es
        // relativo (independiente de bits/ganancia): si no hay separacion robusta
        // devolvemos unos, que deja intactos el peso global y la radiometria.
        let floor = p50.max(p02).max(f32::MIN_POSITIVE);
        let upper_tail_ratio = p98 / floor;
        let middle_spread = (p90 - p10).max(0.0) / floor;
        let reliable = p98.is_finite()
            && range.is_finite()
            && p98 > 0.0
            && range > p98.abs() * 0.10
            && (upper_tail_ratio >= 2.20 || middle_spread >= 1.20);
        if !reliable {
            return neutral();
        }

        for v in &mut quality_map {
            *v = ((*v - p02) / range.max(f32::MIN_POSITIVE)).clamp(0.0, 1.0);
        }

        Self { scores: quality_map, width: w, height: h, reliable: true }
    }

    /// Reproyecta el mapa de sensor al raster de referencia (ROI antes del
    /// shift). Despues el acumulador puede seguir usando su lookup barato
    /// `out * dq/out_size + q_off`, tanto en CPU como en GPU.
    #[allow(clippy::too_many_arguments)]
    pub fn into_reference_raster(
        self,
        source_w: usize,
        source_h: usize,
        output_w: usize,
        output_h: usize,
        drizzle: f32,
        roi_offset_x: f32,
        roi_offset_y: f32,
        target_w: usize,
        target_h: usize,
    ) -> Self {
        let Some(target_n) = target_w.checked_mul(target_h) else {
            return Self {
                scores: Vec::new(),
                width: target_w,
                height: target_h,
                reliable: false,
            };
        };
        if target_n == 0
            || source_w == 0
            || source_h == 0
            || output_w == 0
            || output_h == 0
            || !drizzle.is_finite()
            || drizzle <= 0.0
            || self.scores.len() < self.width.saturating_mul(self.height)
            || self.width == 0
            || self.height == 0
        {
            return Self {
                scores: vec![1.0; target_n],
                width: target_w,
                height: target_h,
                reliable: false,
            };
        }
        if !self.reliable {
            return Self {
                scores: vec![1.0; target_n],
                width: target_w,
                height: target_h,
                reliable: false,
            };
        }

        // Fast path de frame completo: evita una copia de ~0.7 M muestras por
        // frame cuando la geometria ya coincide exactamente.
        let map_step_x = output_w as f32 * self.width as f32
            / (target_w as f32 * drizzle * source_w as f32);
        let map_step_y = output_h as f32 * self.height as f32
            / (target_h as f32 * drizzle * source_h as f32);
        if target_w == self.width
            && target_h == self.height
            && roi_offset_x.abs() < 1e-6
            && roi_offset_y.abs() < 1e-6
            && (map_step_x - 1.0).abs() < 1e-6
            && (map_step_y - 1.0).abs() < 1e-6
        {
            return self;
        }

        let mut registered = vec![1.0f32; target_n];
        let source_to_map_x = self.width as f32 / source_w as f32;
        let source_to_map_y = self.height as f32 / source_h as f32;
        let output_per_map_x = output_w as f32 / target_w as f32;
        let output_per_map_y = output_h as f32 / target_h as f32;
        let max_x = (self.width - 1) as f32;
        let max_y = (self.height - 1) as f32;

        for y in 0..target_h {
            let output_y = y as f32 * output_per_map_y;
            let map_y = ((roi_offset_y + output_y / drizzle) * source_to_map_y)
                .clamp(0.0, max_y);
            let y0 = map_y.floor() as usize;
            let y1 = (y0 + 1).min(self.height - 1);
            let fy = map_y - y0 as f32;
            for x in 0..target_w {
                let output_x = x as f32 * output_per_map_x;
                let map_x = ((roi_offset_x + output_x / drizzle) * source_to_map_x)
                    .clamp(0.0, max_x);
                let x0 = map_x.floor() as usize;
                let x1 = (x0 + 1).min(self.width - 1);
                let fx = map_x - x0 as f32;
                let a = self.scores[y0 * self.width + x0];
                let b = self.scores[y0 * self.width + x1];
                let c = self.scores[y1 * self.width + x0];
                let d = self.scores[y1 * self.width + x1];
                registered[y * target_w + x] = a * (1.0 - fx) * (1.0 - fy)
                    + b * fx * (1.0 - fy)
                    + c * (1.0 - fx) * fy
                    + d * fx * fy;
            }
        }
        Self {
            scores: registered,
            width: target_w,
            height: target_h,
            reliable: true,
        }
    }

    /// Convierte un shift en pixeles fuente a unidades del mapa ROI. El ROI ya
    /// esta incorporado en `into_reference_raster`; drizzle se conserva aqui.
    #[inline]
    fn shift_offset(shift: f32, map_len: usize, output_len: usize, drizzle: f32) -> f32 {
        if output_len == 0 || !shift.is_finite() || !drizzle.is_finite() {
            0.0
        } else {
            shift * map_len as f32 * drizzle / output_len as f32
        }
    }

    fn build_sat(data: &[f32], w: usize, h: usize) -> Vec<f64> {
        let mut sat = vec![0.0f64; w * h];
        for y in 0..h {
            let row = y * w;
            for x in 0..w {
                let left = if x > 0 { sat[row + x - 1] } else { 0.0 };
                let up = if y > 0 { sat[row - w + x] } else { 0.0 };
                let diag = if x > 0 && y > 0 { sat[row - w + x - 1] } else { 0.0 };
                sat[row + x] = data[row + x] as f64 + left + up - diag;
            }
        }
        sat
    }

    fn query_sat(sat: &[f64], w: usize, x0: usize, y0: usize, x1: usize, y1: usize) -> f32 {
        let a = sat[y1 * w + x1];
        let b = if x0 > 0 { sat[y1 * w + (x0 - 1)] } else { 0.0 };
        let c = if y0 > 0 { sat[(y0 - 1) * w + x1] } else { 0.0 };
        let d = if x0 > 0 && y0 > 0 { sat[(y0 - 1) * w + (x0 - 1)] } else { 0.0 };
        (a - b - c + d) as f32
    }
}

/// Box-downscale of an f32 image (used to keep DenseQualityMap at 1/4
/// resolution: the map is smooth by construction, and full-res copies cost
/// ~9 MB per analyzed frame on typical planetary cameras).
pub fn downscale_f32_box(src: &[f32], w: usize, h: usize, factor: usize) -> (Vec<f32>, usize, usize) {
    let f = factor.max(1);
    let nw = (w / f).max(1);
    let nh = (h / f).max(1);
    let mut out = vec![0.0f32; nw * nh];
    let area = (f * f) as f32;
    for y in 0..nh {
        let sy = y * f;
        for x in 0..nw {
            let sx = x * f;
            let mut sum = 0.0f32;
            for ky in 0..f {
                let row = (sy + ky).min(h - 1) * w;
                for kx in 0..f {
                    sum += src[row + (sx + kx).min(w - 1)];
                }
            }
            out[y * nw + x] = sum / area;
        }
    }
    (out, nw, nh)
}

/// Box-downscale a u16 image directly into f32 (dense-map input) without
/// allocating a full-resolution f32 intermediate copy.
pub fn downscale_u16_to_f32_box(
    src: &[u16],
    w: usize,
    h: usize,
    factor: usize,
) -> (Vec<f32>, usize, usize) {
    let f = factor.max(1);
    let nw = (w / f).max(1);
    let nh = (h / f).max(1);
    let mut out = vec![0.0f32; nw * nh];
    let area = (f * f) as f32;
    for y in 0..nh {
        let sy = y * f;
        for x in 0..nw {
            let sx = x * f;
            let mut sum = 0.0f32;
            for ky in 0..f {
                let row = (sy + ky).min(h - 1) * w;
                for kx in 0..f {
                    sum += src[row + (sx + kx).min(w - 1)] as f32;
                }
            }
            out[y * nw + x] = sum / area;
        }
    }
    (out, nw, nh)
}

/// Per-thread scratch for the liquid stacking loop. All buffers are allocated
/// once per rayon worker instead of once per frame (the previous code
/// allocated ~9 large vectors per frame, fragmenting RAM on long videos).
/// In mono mode the RGB-only buffers stay empty (zero cost).
// RAM: the per-thread scratch deliberately holds NO canvas accumulators. The
// old per-thread GradientDomainStacker trio (f64 direct + direct_w × 3) was
// ~48 bytes/px_out PER THREAD — gigabytes on many-core machines, the root
// cause of the OOM/paging freeze. Frames now merge into SHARED per-channel
// accumulators (one short-lived lock per frame; merge cost is small next to
// the per-frame alignment+warp work), so threads scale without RAM blowup.
struct LiquidScratch {
    rgb_buf: Vec<u16>,
    mono_buf: Vec<u16>,
    f_s1: Vec<u16>,
    f_s2: Vec<u16>,
    f_edges: Vec<u16>,
    f_ds: Vec<u16>,
    wr: Vec<f32>,
    wg: Vec<f32>,
    wb: Vec<f32>,
    ww: Vec<f32>,
}

impl LiquidScratch {
    /// `with_output`: en modo GPU los workers NO renderizan el canvas (el
    /// warp+acumulacion corre en la GPU), asi que los 4 planos f32 de salida
    /// (16 B/px_out POR HILO) no se asignan — RAM libre para mas hilos de
    /// alineacion en canvases grandes con drizzle.
    fn try_new(
        w_in: usize,
        h_in: usize,
        w_out: usize,
        h_out: usize,
        is_mono: bool,
        with_output: bool,
    ) -> Result<Self, String> {
        let n_in = w_in
            .checked_mul(h_in)
            .ok_or("El scratch de entrada excede el espacio direccionable")?;
        let n_out = if with_output {
            w_out
                .checked_mul(h_out)
                .ok_or("El scratch de salida excede el espacio direccionable")?
        } else {
            0
        };
        let rgb_len = n_in
            .checked_mul(3)
            .ok_or("El scratch RGB excede el espacio direccionable")?;
        let downscaled_len = (w_in / 4)
            .checked_mul(h_in / 4)
            .ok_or("La pirámide de alineación excede el espacio direccionable")?;
        Ok(Self {
            rgb_buf: if is_mono {
                Vec::new()
            } else {
                try_filled_vec(rgb_len, 0u16, "scratch RGB")?
            },
            mono_buf: try_filled_vec(n_in, 0u16, "scratch mono")?,
            f_s1: try_filled_vec(n_in, 0u16, "scratch blur 1")?,
            f_s2: try_filled_vec(n_in, 0u16, "scratch blur 2")?,
            f_edges: try_filled_vec(n_in, 0u16, "scratch de bordes")?,
            f_ds: try_filled_vec(downscaled_len, 0u16, "scratch piramidal")?,
            wr: if is_mono || n_out == 0 {
                Vec::new()
            } else {
                try_filled_vec(n_out, 0.0f32, "scratch R de salida")?
            },
            wg: try_filled_vec(n_out, 0.0f32, "scratch G de salida")?,
            wb: if is_mono || n_out == 0 {
                Vec::new()
            } else {
                try_filled_vec(n_out, 0.0f32, "scratch B de salida")?
            },
            ww: try_filled_vec(n_out, 0.0f32, "scratch de pesos")?,
        })
    }
}

/// Bounded lease pool: workers borrow a LiquidScratch and return it on drop,
/// so live scratches never exceed the real thread concurrency (rayon's
/// fold/for_each_init would otherwise allocate one per work-split).
struct ScratchLease<'a> {
    pool: &'a std::sync::Mutex<Vec<LiquidScratch>>,
    sc: Option<LiquidScratch>,
}

impl<'a> ScratchLease<'a> {
    fn try_take(pool: &'a std::sync::Mutex<Vec<LiquidScratch>>) -> Option<Self> {
        let sc = pool
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()?;
        Some(Self { pool, sc: Some(sc) })
    }
}

impl<'a> Drop for ScratchLease<'a> {
    fn drop(&mut self) {
        if let Some(sc) = self.sc.take() {
            self.pool.lock().unwrap_or_else(|e| e.into_inner()).push(sc);
        }
    }
}

// ==========================================
// ELITE V4: GRADIENT DOMAIN STACKING (Poisson)
// ==========================================

/// Acumulador compartido con LOCKS POR BANDA de filas (striping).
///
/// El diseño anterior usaba UN Mutex<GradientDomainStacker> por canal: cada
/// worker serializaba el merge del canvas COMPLETO bajo el lock, y con muchos
/// nucleos (Windows HEDT) los hilos hacian cola — el merge era la seccion de
/// Amdahl que limitaba el escalado. Con bandas de filas, dos workers solo
/// chocan si mergean LA MISMA banda a la vez; cada lock se sostiene una
/// fraccion 1/n_bands del tiempo. La MATEMATICA por pixel es identica (misma
/// suma f64 por pixel; el orden de suma entre frames ya era no-determinista
/// con el mutex unico), solo cambia la granularidad del lock.
///
/// SEGURIDAD: mismo patron de punteros crudos con filas disjuntas que
/// ds_warp_accumulate (deepsky.rs). Los punteros se capturan al construir y
/// las Vec nunca se redimensionan; cada banda de filas es escrita SOLO bajo
/// su mutex, y la lectura final ocurre tras into_inner() (acceso exclusivo).
/// Solo soporta los modos direct-only/direct-tracked del camino liquid (los
/// buffers de gradiente Sobel estan siempre vacios en esta ruta).
struct StripedAccum {
    acc: GradientDomainStacker,
    direct_ptr: usize,   // 0 = canal inactivo (mono: R/B vacios)
    direct_w_ptr: usize,
    direct_w2_ptr: usize, // 0 = sin Σw² (solo existe en modo tracked)
    m2_ptr: usize,       // 0 = sin tracking de varianza (sigma-clip off)
    w: usize,
    h: usize,
    rows_per_band: usize,
    bands: Vec<std::sync::Mutex<()>>,
}

impl StripedAccum {
    fn new(mut acc: GradientDomainStacker, n_bands: usize) -> Self {
        debug_assert!(
            acc.grad_x.is_empty(),
            "StripedAccum no implementa el merge de gradientes Sobel (el camino liquid es direct-only)"
        );
        let (direct_ptr, direct_w_ptr, direct_w2_ptr, m2_ptr) = if acc.direct.is_empty() {
            (0, 0, 0, 0)
        } else {
            (
                acc.direct.as_mut_ptr() as usize,
                acc.direct_w.as_mut_ptr() as usize,
                if acc.direct_w2.is_empty() {
                    0
                } else {
                    acc.direct_w2.as_mut_ptr() as usize
                },
                if acc.m2.is_empty() { 0 } else { acc.m2.as_mut_ptr() as usize },
            )
        };
        let w = acc.width;
        let h = acc.height;
        let n_bands = n_bands.clamp(1, h.max(1));
        let rows_per_band = (h + n_bands - 1) / n_bands.max(1);
        Self {
            acc,
            direct_ptr,
            direct_w_ptr,
            direct_w2_ptr,
            m2_ptr,
            w,
            h,
            rows_per_band: rows_per_band.max(1),
            bands: (0..n_bands).map(|_| std::sync::Mutex::new(())).collect(),
        }
    }

    /// Merge de un frame ya warpeado. Matematica identica a
    /// GradientDomainStacker::accumulate (rama direct); `band_offset` rota el
    /// orden de bandas por worker para que dos hilos no avancen en fase
    /// pidiendo siempre la misma banda (decorrelacion del convoy).
    /// `weight_by_coverage` (solo drizzle >1x): pondera cada pixel del frame
    /// tambien por su plano de pesos (clamp a 1) — que en el camino drizzle
    /// lleva la COBERTURA del drop (× la confianza IDW del warp): el peso
    /// entre frames del drizzle clasico. En el camino Lanczos queda apagado
    /// para no cambiar la ponderacion historica (solo q²·gw).
    #[allow(clippy::too_many_arguments)]
    fn accumulate(
        &self,
        frame: &[f32],
        frame_w: &[f32],
        quality_map: &[f32],
        q_w: usize,
        q_h: usize,
        q_off_x: f32,
        q_off_y: f32,
        global_w: f32,
        band_offset: usize,
        weight_by_coverage: bool,
    ) {
        if self.direct_ptr == 0 {
            return; // canal inactivo (mono)
        }
        let w = self.w;
        let h = self.h;
        if w < 3 || h < 3 {
            return; // igual que el bucle original 1..h-1 / 1..w-1 (vacio)
        }
        let gw = global_w as f64;
        let scale_x = if q_w > 0 { q_w as f32 / w as f32 } else { 0.0 };
        let scale_y = if q_h > 0 { q_h as f32 / h as f32 } else { 0.0 };
        let n_bands = self.bands.len();
        let direct = self.direct_ptr as *mut f64;
        let direct_w = self.direct_w_ptr as *mut f64;
        let direct_w2 = self.direct_w2_ptr as *mut f64;
        let m2 = self.m2_ptr as *mut f64;

        for k in 0..n_bands {
            let b = (k + band_offset) % n_bands;
            let y0 = (b * self.rows_per_band).max(1);
            let y1 = ((b + 1) * self.rows_per_band).min(h - 1);
            if y0 >= y1 {
                continue;
            }
            let _guard = self.bands[b].lock().unwrap_or_else(|e| e.into_inner());
            for y in y0..y1 {
                let row = y * w;
                for x in 1..w - 1 {
                    let i = row + x;
                    if !frame_w.is_empty() && frame_w[i] < 1e-9 {
                        continue;
                    }
                    let q = if !quality_map.is_empty() && q_w > 0 && q_h > 0 {
                        let qx = ((x as f32 * scale_x + q_off_x).round().max(0.0) as usize)
                            .min(q_w - 1);
                        let qy = ((y as f32 * scale_y + q_off_y).round().max(0.0) as usize)
                            .min(q_h - 1);
                        quality_map.get(qy * q_w + qx).copied().unwrap_or(1.0) as f64
                    } else {
                        1.0
                    };
                    let cov_w = if weight_by_coverage && !frame_w.is_empty() {
                        (frame_w[i] as f64).min(1.0)
                    } else {
                        1.0
                    };
                    let combined_w = q * q * gw * cov_w; // Quadratic focus (igual que el original)
                    if combined_w < 1e-7 {
                        continue;
                    }
                    // SAFETY: escritura solo dentro de las filas [y0, y1) de la
                    // banda b, bajo su mutex; los punteros provienen de Vecs
                    // vivas en self.acc que nunca se redimensionan.
                    unsafe {
                        *direct.add(i) += frame[i] as f64 * combined_w;
                        *direct_w.add(i) += combined_w;
                        if self.direct_w2_ptr != 0 {
                            *direct_w2.add(i) += combined_w * combined_w;
                        }
                        if self.m2_ptr != 0 {
                            let v = frame[i] as f64;
                            *m2.add(i) += v * v * combined_w;
                        }
                    }
                }
            }
        }
    }

    fn into_inner(self) -> GradientDomainStacker {
        self.acc
    }
}

pub struct GradientDomainStacker {
    pub grad_x: Vec<f64>,
    pub grad_y: Vec<f64>,
    pub weight: Vec<f64>,
    pub direct: Vec<f64>,
    pub direct_w: Vec<f64>,
    /// Suma de pesos al cuadrado (Σw²), sólo en modo tracked. Permite calcular
    /// N_eff=(Σw)²/Σw² aun con drizzle/cobertura y pesos de calidad desiguales.
    pub direct_w2: Vec<f64>,
    /// Weighted sum of squared values (Σ w·v²). Allocated only in sigma-clip
    /// tracked mode: together with `direct`/`direct_w` it yields the per-pixel
    /// weighted variance of the frame distribution — the statistical basis for
    /// the kappa-sigma artifact rejection in pass 2.
    pub m2: Vec<f64>,
    pub width: usize,
    pub height: usize,
}

impl GradientDomainStacker {
    pub fn new(w: usize, h: usize) -> Self {
        let n = w * h;
        Self {
            grad_x: vec![0.0; n],
            grad_y: vec![0.0; n],
            weight: vec![0.0; n],
            direct: vec![0.0; n],
            direct_w: vec![0.0; n],
            direct_w2: Vec::new(),
            m2: Vec::new(),
            width: w,
            height: h,
        }
    }

    /// Direct-mean only variant. The three gradient buffers (24 bytes/px) are
    /// consumed exclusively by the Poisson reconstruction, which the liquid
    /// stacking path keeps disabled — skipping them cuts per-thread RAM by 60%.
    pub fn new_direct_only(w: usize, h: usize) -> Self {
        Self::try_new_direct_only(w, h).expect("direct-only test allocation")
    }

    fn try_new_direct_only(w: usize, h: usize) -> Result<Self, String> {
        let n = w
            .checked_mul(h)
            .ok_or("El acumulador direct-only excede el espacio direccionable")?;
        Ok(Self {
            grad_x: Vec::new(),
            grad_y: Vec::new(),
            weight: Vec::new(),
            direct: try_filled_vec(n, 0.0f64, "acumulador planetario")?,
            direct_w: try_filled_vec(n, 0.0f64, "pesos planetarios")?,
            direct_w2: Vec::new(),
            m2: Vec::new(),
            width: w,
            height: h,
        })
    }

    /// Direct-mean variant that ALSO tracks the per-pixel second moment for
    /// sigma-clip statistics (used by pass 1 of the double-pass stack).
    pub fn new_direct_tracked(w: usize, h: usize) -> Self {
        Self::try_new_direct_tracked(w, h).expect("direct-tracked test allocation")
    }

    fn try_new_direct_tracked(w: usize, h: usize) -> Result<Self, String> {
        let n = w
            .checked_mul(h)
            .ok_or("El acumulador tracked excede el espacio direccionable")?;
        Ok(Self {
            grad_x: Vec::new(),
            grad_y: Vec::new(),
            weight: Vec::new(),
            direct: try_filled_vec(n, 0.0f64, "acumulador tracked")?,
            direct_w: try_filled_vec(n, 0.0f64, "pesos tracked")?,
            direct_w2: try_filled_vec(n, 0.0f64, "pesos cuadrados tracked")?,
            m2: try_filled_vec(n, 0.0f64, "segundo momento tracked")?,
            width: w,
            height: h,
        })
    }

    /// Zero-size placeholder for unused channels (mono stacking mode).
    pub fn new_empty() -> Self {
        Self {
            grad_x: Vec::new(),
            grad_y: Vec::new(),
            weight: Vec::new(),
            direct: Vec::new(),
            direct_w: Vec::new(),
            direct_w2: Vec::new(),
            m2: Vec::new(),
            width: 0,
            height: 0,
        }
    }

    /// `quality_map` ya esta reproyectado a la ROI de referencia;
    /// `q_off_x`/`q_off_y` expresan el shift fuente en unidades de ese mapa.
    /// Separar ROI de shift mantiene correcta la escala con cualquier drizzle.
    pub fn accumulate(&mut self, frame: &[f32], frame_w: &[f32], quality_map: &[f32], q_w: usize, q_h: usize, q_off_x: f32, q_off_y: f32, global_w: f32) {
        if self.direct.is_empty() {
            return;
        }
        let w = self.width;
        let h = self.height;
        let gw = global_w as f64;
        let use_gradients = !self.grad_x.is_empty();

        let scale_x = if q_w > 0 { q_w as f32 / w as f32 } else { 0.0 };
        let scale_y = if q_h > 0 { q_h as f32 / h as f32 } else { 0.0 };

        for y in 1..h - 1 {
            let row = y * w;
            for x in 1..w - 1 {
                let i = row + x;

                if !frame_w.is_empty() && frame_w[i] < 1e-9 { continue; }

                let q = if !quality_map.is_empty() && q_w > 0 && q_h > 0 {
                    let qx = ((x as f32 * scale_x + q_off_x).round().max(0.0) as usize).min(q_w - 1);
                    let qy = ((y as f32 * scale_y + q_off_y).round().max(0.0) as usize).min(q_h - 1);
                    quality_map.get(qy * q_w + qx).copied().unwrap_or(1.0) as f64
                } else {
                    1.0
                };

                let combined_w = q * q * gw; // Quadratic focus (Increased selectivity for micro-detail)

                if combined_w < 1e-7 { continue; }

                if use_gradients {
                    // Sobel Gradient (3x3)
                    let gx = (frame[i + 1] as f64 - frame[i - 1] as f64
                        + 2.0 * (frame[i + w + 1] as f64 - frame[i + w - 1] as f64)
                        + frame[i - w + 1] as f64 - frame[i - w - 1] as f64) / 8.0;

                    let gy = (frame[i + w] as f64 - frame[i - w] as f64
                        + 2.0 * (frame[i + w + 1] as f64 - frame[i - w + 1] as f64)
                        + frame[i + w - 1] as f64 - frame[i - w - 1] as f64) / 8.0;

                    self.grad_x[i] += gx * combined_w;
                    self.grad_y[i] += gy * combined_w;
                    self.weight[i] += combined_w;
                }
                self.direct[i] += frame[i] as f64 * combined_w;
                self.direct_w[i] += combined_w;
                if !self.direct_w2.is_empty() {
                    self.direct_w2[i] += combined_w * combined_w;
                }
                if !self.m2.is_empty() {
                    let v = frame[i] as f64;
                    self.m2[i] += v * v * combined_w;
                }
            }
        }
    }

    pub fn merge(&mut self, other: &Self) {
        for (a, b) in self.grad_x.iter_mut().zip(other.grad_x.iter()) { *a += b; }
        for (a, b) in self.grad_y.iter_mut().zip(other.grad_y.iter()) { *a += b; }
        for (a, b) in self.weight.iter_mut().zip(other.weight.iter()) { *a += b; }
        for (a, b) in self.direct.iter_mut().zip(other.direct.iter()) { *a += b; }
        for (a, b) in self.direct_w.iter_mut().zip(other.direct_w.iter()) { *a += b; }
        for (a, b) in self.direct_w2.iter_mut().zip(other.direct_w2.iter()) { *a += b; }
        for (a, b) in self.m2.iter_mut().zip(other.m2.iter()) { *a += b; }
    }

    pub fn reconstruct(&self, iterations: usize) -> Vec<f32> {
        let w = self.width;
        let h = self.height;
        let n = w * h;

        let mut avg_gx = vec![0.0f64; n];
        let mut avg_gy = vec![0.0f64; n];
        let mut result = vec![0.0f64; n];

        for i in 0..n {
            if self.weight[i] > 1e-9 {
                avg_gx[i] = self.grad_x[i] / self.weight[i];
                avg_gy[i] = self.grad_y[i] / self.weight[i];
            }
            if self.direct_w[i] > 1e-9 {
                result[i] = self.direct[i] / self.direct_w[i];
            }
        }

        let mut divergence = vec![0.0f64; n];
        for y in 1..h - 1 {
            let row = y * w;
            for x in 1..w - 1 {
                divergence[row + x] = (avg_gx[row + x + 1] - avg_gx[row + x - 1]) / 2.0
                                    + (avg_gy[row + w + x] - avg_gy[row - w + x]) / 2.0;
            }
        }

        // Gauss-Seidel with Successive Over-Relaxation (SOR)
        for iter in 0..iterations {
            let omega = if iter < 20 { 1.0 } else { 1.85 };
            for y in 1..h - 1 {
                let row = y * w;
                for x in 1..w - 1 {
                    let i = row + x;
                    let laplacian_neighbors = result[i - 1] + result[i + 1] + result[i - w] + result[i + w];
                    let new_val = (laplacian_neighbors - divergence[i]) / 4.0;
                    result[i] = result[i] + omega * (new_val - result[i]);
                }
            }
        }

        result.iter().map(|&v| v.clamp(0.0, 65535.0) as f32).collect()
    }
}

// ==========================================
// ELITE V4: ADAPTIVE COHERENCE WAVELETS
// ==========================================

pub struct AtrousDecomposition {
    pub detail_planes: Vec<Vec<f32>>,
    pub coarse_residual: Vec<f32>,
}

impl AtrousDecomposition {
    pub fn decompose(image: &[f32], w: usize, h: usize, levels: usize) -> Self {
        let mut detail_planes = Vec::with_capacity(levels);
        let mut current = image.to_vec();
        for level in 0..levels {
            let next = atrous_smooth(&current, w, h, level);
            let detail = current.iter().zip(next.iter()).map(|(&c, &n)| c - n).collect();
            detail_planes.push(detail);
            current = next;
        }
        Self { detail_planes, coarse_residual: current }
    }
}

pub fn atrous_smooth(img: &[f32], w: usize, h: usize, level: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; img.len()];
    let step = 1 << level;
    let kernel = [1.0/16.0, 4.0/16.0, 6.0/16.0, 4.0/16.0, 1.0/16.0];
    
    // Separable convolution
    let mut temp = vec![0.0f32; img.len()];
    
    // Horizontal
    for y in 0..h {
        let row_offset = y * w;
        for x in 0..w {
            let mut val = 0.0;
            for (i, &k) in kernel.iter().enumerate() {
                let dx = (i as isize - 2) * step;
                let nx = (x as isize + dx).clamp(0, w as isize - 1) as usize;
                val += img[row_offset + nx] * k;
            }
            temp[row_offset + x] = val;
        }
    }
    
    // Vertical
    for x in 0..w {
        for y in 0..h {
            let mut val = 0.0;
            for (i, &k) in kernel.iter().enumerate() {
                let dy = (i as isize - 2) * step;
                let ny = (y as isize + dy).clamp(0, h as isize - 1) as usize;
                val += temp[ny * w + x] * k;
            }
            out[y * w + x] = val;
        }
    }
    out
}

pub fn estimate_stack_noise(stacked: &[f32], w: usize, h: usize) -> f32 {
    let s0_smooth = atrous_smooth(stacked, w, h, 0);
    let mut residuals: Vec<f32> = stacked.iter().zip(s0_smooth.iter()).map(|(&s, &sm)| (s - sm).abs()).collect();
    residuals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    residuals[residuals.len() / 2] * 1.4826 // MAD to Sigma
}

pub fn compute_coherence_map(details: &[Vec<f32>], w: usize, h: usize) -> Vec<f32> {
    let n = details.len();
    if n < 2 { return vec![0.8; w * h]; }
    let count = w * h;
    let mut mean = vec![0.0f64; count];
    let mut m2 = vec![0.0f64; count];

    for detail in details {
        for (i, &d) in detail.iter().enumerate() {
            let delta = d as f64 - mean[i];
            mean[i] += delta / n as f64;
            let delta2 = d as f64 - mean[i];
            m2[i] += delta * delta2;
        }
    }

    (0..count).map(|i| {
        let std_dev = (m2[i] / (n as f64 - 1.0)).sqrt();
        let signal = mean[i].abs() + 1e-6;
        let cv = std_dev / signal;
        (1.0 / (1.0 + 3.0 * cv)).clamp(0.0, 1.0) as f32
    }).collect()
}

pub fn sharpen_adaptive_atrous(
    stacked: &[u16],
    w: usize,
    h: usize,
    all_frame_details: Option<&[Vec<Vec<f32>>]>,
    aggressiveness: f32,
) -> Vec<u16> {
    let mut f_stack = vec![0.0f32; stacked.len()];
    for i in 0..stacked.len() { f_stack[i] = stacked[i] as f32; }

    let n = w * h;
    let mut channels = [vec![0.0f32; n], vec![0.0f32; n], vec![0.0f32; n]];
    for i in 0..n {
        channels[0][i] = f_stack[i * 3];
        channels[1][i] = f_stack[i * 3 + 1];
        channels[2][i] = f_stack[i * 3 + 2];
    }

    let noise_sigma = estimate_stack_noise(&channels[1], w, h);
    let mut out_channels = [vec![0.0f32; n], vec![0.0f32; n], vec![0.0f32; n]];

    for c in 0..3 {
        let decomp = AtrousDecomposition::decompose(&channels[c], w, h, 6);
        let mut res = decomp.coarse_residual.clone();

        for scale in 0..decomp.detail_planes.len() {
            let detail = &decomp.detail_planes[scale];
            let base_amp = match scale {
                0 => 0.0,
                1 => 1.0 + 2.4 * aggressiveness,
                2 => 1.0 + 1.8 * aggressiveness,
                3 => 1.0 + 1.0 * aggressiveness,
                4 => 1.0 + 0.5 * aggressiveness,
                _ => 1.0,
            };

            if base_amp <= 1.0 {
                for (r, &d) in res.iter_mut().zip(detail.iter()) { *r += d; }
                continue;
            }

            let coherence_map = all_frame_details.map(|frames| {
                let scale_details: Vec<Vec<f32>> = frames.iter().map(|f| f[scale].clone()).collect();
                compute_coherence_map(&scale_details, w, h)
            });

            for (i, (&d, r)) in detail.iter().zip(res.iter_mut()).enumerate() {
                let coherence = coherence_map.as_ref().map(|cm| cm[i]).unwrap_or(0.7);
                let noise_threshold = noise_sigma * (1.0 + scale as f32 * 0.5);
                let confidence = if d.abs() > noise_threshold {
                    ((d.abs() - noise_threshold) / (noise_threshold * 2.0)).clamp(0.0, 1.0)
                } else { 0.0 };

                let amp = 1.0 + (base_amp - 1.0) * coherence * confidence;
                *r += d * amp;
            }
        }
        out_channels[c] = res;
    }

    let mut out = Vec::with_capacity(stacked.len());
    for i in 0..n {
        out.push(out_channels[0][i].clamp(0.0, 65535.0) as u16);
        out.push(out_channels[1][i].clamp(0.0, 65535.0) as u16);
        out.push(out_channels[2][i].clamp(0.0, 65535.0) as u16);
    }
    out
}

fn gaussian_kernel_2d(size: usize, sigma: f32) -> Vec<f32> {
    let mut kernel = vec![0.0f32; size * size];
    let center = (size as f32 - 1.0) / 2.0;
    let mut sum = 0.0f32;
    let s2 = 2.0 * sigma * sigma;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let val = (- (dx*dx + dy*dy) / s2).exp();
            kernel[y * size + x] = val;
            sum += val;
        }
    }
    for v in &mut kernel { *v /= sum; }
    kernel
}

fn convolve_spatial(img: &[f32], w: usize, h: usize, kernel: &[f32], k_size: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; img.len()];
    let half = (k_size / 2) as isize;

    for y in 0..h {
        for x in 0..w {
            let mut sum = 0.0f32;
            for ky in 0..k_size {
                for kx in 0..k_size {
                    let py = (y as isize + ky as isize - half).clamp(0, h as isize - 1) as usize;
                    let px = (x as isize + kx as isize - half).clamp(0, w as isize - 1) as usize;
                    sum += img[py * w + px] * kernel[ky * k_size + kx];
                }
            }
            out[y * w + x] = sum;
        }
    }
    out
}

// [ApPoint moved to smart_grid.rs]

// ==========================================
// ELITE V4: ADVANCED MATH & PSF ESTIMATION
// ==========================================

/// Aproximación de Bessel J1 — necesaria para PSF Airy
#[inline]
fn bessel_j1_approx(x: f32) -> f32 {
    if x.abs() < 1e-6 { return 0.5; }
    let x2 = x * x;
    // Aproximación polinomial para J1(x)/x
    0.5 - x2/16.0 + x2*x2/384.0 - x2*x2*x2/18432.0
}

fn otsu_threshold(image: &[f32]) -> f32 {
    if image.is_empty() { return 0.0; }
    let max_val = image.iter().cloned().fold(f32::MIN, f32::max);
    let min_val = image.iter().cloned().fold(f32::MAX, f32::min);
    let bins = 256usize;
    let mut hist = vec![0u32; bins];

    for &v in image {
        let bin = (((v - min_val) / (max_val - min_val + 1e-9)) * (bins - 1) as f32) as usize;
        hist[bin.min(bins - 1)] += 1;
    }

    let total = image.len() as f32;
    let mut best_thresh = 0.0f32;
    let mut best_var = 0.0f32;
    let mut w0 = 0.0f32;
    let mut sum0 = 0.0f32;
    let total_sum: f32 = hist.iter().enumerate().map(|(i, &c)| i as f32 * c as f32).sum();

    for t in 0..bins {
        w0 += hist[t] as f32 / total;
        sum0 += t as f32 * hist[t] as f32;
        let w1 = 1.0 - w0;
        if w0 < 1e-6 || w1 < 1e-6 { continue; }
        let mu0 = sum0 / (w0 * total);
        let mu1 = (total_sum - sum0) / (w1 * total);
        let between_var = w0 * w1 * (mu0 - mu1).powi(2);
        if between_var > best_var {
            best_var = between_var;
            best_thresh = min_val + (t as f32 / bins as f32) * (max_val - min_val);
        }
    }
    best_thresh
}

/// Sky mask via TRUE border connectivity (flood fill).
/// Returns 1.0 = disk/feature (including dark filaments and sunspot umbrae),
/// 0.0 = background sky, with a smooth feather at the limb.
///
/// Unlike a brightness threshold or a morphological closing, WIDE dark
/// features in the disk interior can never be classified as sky here,
/// because they are not connected to the frame border.
fn compute_border_connected_sky_mask(
    luma: &[f32],
    w: usize,
    h: usize,
    feather: usize,
) -> Vec<f32> {
    let n = w * h;
    if luma.len() < n || n == 0 || w < 8 || h < 8 {
        return vec![1.0; n];
    }

    let coarse = atrous_smooth(luma, w, h, 4);
    let thresh = otsu_threshold(&coarse);

    // BFS flood fill of below-threshold pixels seeded at the frame border.
    let mut is_sky = vec![false; n];
    let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    for x in 0..w {
        for &y in &[0usize, h - 1] {
            let i = y * w + x;
            if coarse[i] <= thresh && !is_sky[i] {
                is_sky[i] = true;
                queue.push_back(i);
            }
        }
    }
    for y in 0..h {
        for &x in &[0usize, w - 1] {
            let i = y * w + x;
            if coarse[i] <= thresh && !is_sky[i] {
                is_sky[i] = true;
                queue.push_back(i);
            }
        }
    }
    let mut sky_count = queue.len();
    while let Some(i) = queue.pop_front() {
        let x = i % w;
        let y = i / w;
        if x > 0 {
            let j = i - 1;
            if !is_sky[j] && coarse[j] <= thresh {
                is_sky[j] = true;
                sky_count += 1;
                queue.push_back(j);
            }
        }
        if x + 1 < w {
            let j = i + 1;
            if !is_sky[j] && coarse[j] <= thresh {
                is_sky[j] = true;
                sky_count += 1;
                queue.push_back(j);
            }
        }
        if y > 0 {
            let j = i - w;
            if !is_sky[j] && coarse[j] <= thresh {
                is_sky[j] = true;
                sky_count += 1;
                queue.push_back(j);
            }
        }
        if y + 1 < h {
            let j = i + w;
            if !is_sky[j] && coarse[j] <= thresh {
                is_sky[j] = true;
                sky_count += 1;
                queue.push_back(j);
            }
        }
    }

    // No meaningful border-connected sky → the target fills the frame.
    if sky_count < n / 100 {
        return vec![1.0; n];
    }

    let binary: Vec<f32> = is_sky.iter().map(|&s| if s { 0.0 } else { 1.0 }).collect();
    let dist = compute_distance_transform(&binary, w, h);
    let feather_f = feather.max(1) as f32;
    binary
        .iter()
        .zip(dist.iter())
        .map(|(&b, &d)| {
            if b < 0.5 {
                0.0
            } else if d >= feather_f {
                1.0
            } else {
                let t = (d / feather_f).clamp(0.0, 1.0);
                t * t * (3.0 - 2.0 * t)
            }
        })
        .collect()
}

fn morphological_dilate(mask: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut max_val = 0.0f32;
            for dy in -(r as isize)..=r as isize {
                for dx in -(r as isize)..=r as isize {
                    if dx * dx + dy * dy > (r * r) as isize { continue; }
                    let nx = (x as isize + dx).clamp(0, w as isize - 1) as usize;
                    let ny = (y as isize + dy).clamp(0, h as isize - 1) as usize;
                    max_val = max_val.max(mask[ny * w + nx]);
                }
            }
            out[y * w + x] = max_val;
        }
    }
    out
}

fn morphological_erode(mask: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut min_val = 1.0f32;
            for dy in -(r as isize)..=r as isize {
                for dx in -(r as isize)..=r as isize {
                    if dx * dx + dy * dy > (r * r) as isize { continue; }
                    let nx = (x as isize + dx).clamp(0, w as isize - 1) as usize;
                    let ny = (y as isize + dy).clamp(0, h as isize - 1) as usize;
                    min_val = min_val.min(mask[ny * w + nx]);
                }
            }
            out[y * w + x] = min_val;
        }
    }
    out
}

fn compute_distance_transform(mask: &[f32], w: usize, h: usize) -> Vec<f32> {
    crate::planetary_quality::distance_transform_truncated(mask, w, h, 32.0)
}

pub struct PsfEstimator {
    pub psf_radius: usize,
}

impl PsfEstimator {
    pub fn estimate_from_limb(
        &self,
        stacked: &[f32],
        width: usize,
        height: usize,
        limb_mask: &[f32],
        gradient_threshold: f32,
    ) -> Vec<f32> {
        let r = self.psf_radius;
        let psf_size = 2 * r + 1;
        let mut psf_acc = vec![0.0f64; psf_size * psf_size];
        let mut psf_weight = 0.0f64;

        let mut gradient_mag = vec![0.0f32; width * height];
        for y in 1..height - 1 {
            for x in 1..width - 1 {
                let i = y * width + x;
                let gx = stacked[i + 1] - stacked[i - 1];
                let gy = stacked[i + width] - stacked[i - width];
                gradient_mag[i] = (gx * gx + gy * gy).sqrt();
            }
        }

        let mut limb_grads: Vec<f32> = gradient_mag.iter().zip(limb_mask.iter())
            .filter(|(_, &m)| m > 0.5)
            .map(|(&g, _)| g).collect();
        limb_grads.sort_unstable_by(f32::total_cmp);
        let grad_thresh = limb_grads.get((limb_grads.len() as f32 * gradient_threshold) as usize)
            .copied().unwrap_or(0.0);

        for y in r..height - r {
            for x in r..width - r {
                let i = y * width + x;
                if limb_mask[i] < 0.5 || gradient_mag[i] < grad_thresh { continue; }

                let gx = stacked[i + 1] - stacked[i - 1];
                let gy = stacked[i + width] - stacked[i - width];
                let mag = (gx * gx + gy * gy).sqrt().max(1e-6);
                let nx = gx / mag;
                let ny = gy / mag;

                for dy in -(r as isize)..=r as isize {
                    for dx in -(r as isize)..=r as isize {
                        let px = (x as isize + dx) as usize;
                        let py = (y as isize + dy) as usize;
                        let proj = dx as f32 * nx + dy as f32 * ny;
                        let psf_x = (proj + r as f32) as usize;
                        let psf_y = r;
                        if psf_x < psf_size {
                            psf_acc[psf_y * psf_size + psf_x] += stacked[py * width + px] as f64 * mag as f64;
                            psf_weight += mag as f64;
                        }
                    }
                }
            }
        }

        if psf_weight > 0.0 { psf_acc.iter_mut().for_each(|v| *v /= psf_weight); }

        let mut psf = vec![0.0f32; psf_size * psf_size];
        for y in 0..psf_size {
            for x in 1..psf_size - 1 {
                let i = y * psf_size + x;
                psf[i] = (psf_acc[i + 1] - psf_acc[i - 1]).abs() as f32 / 2.0;
            }
        }

        let profile_1d: Vec<f32> = (0..psf_size).map(|x| psf[r * psf_size + x]).collect();
        let mut psf_2d = vec![0.0f32; psf_size * psf_size];
        for dy in -(r as isize)..=r as isize {
            for dx in -(r as isize)..=r as isize {
                let dist = ((dx * dx + dy * dy) as f32).sqrt();
                let idx_1d = (dist + r as f32) as usize;
                if idx_1d < psf_size {
                    let py = (dy + r as isize) as usize;
                    let px = (dx + r as isize) as usize;
                    psf_2d[py * psf_size + px] = profile_1d[idx_1d];
                }
            }
        }

        let psf_sum: f32 = psf_2d.iter().sum();
        if psf_sum > 1e-9 { psf_2d.iter_mut().for_each(|v| *v /= psf_sum); }
        psf_2d
    }

    pub fn estimate_gaussian_airy(&self, fwhm: f32, airy_weight: f32) -> Vec<f32> {
        let r = self.psf_radius;
        let psf_size = 2 * r + 1;
        let sigma = fwhm / 2.355;
        let mut psf = vec![0.0f32; psf_size * psf_size];

        for dy in -(r as isize)..=r as isize {
            for dx in -(r as isize)..=r as isize {
                let dist = ((dx * dx + dy * dy) as f32).sqrt();
                let i = (dy + r as isize) as usize * psf_size + (dx + r as isize) as usize;

                let gauss = (-dist * dist / (2.0 * sigma * sigma)).exp();
                let airy_radius = fwhm * 0.514;
                let airy = if dist < 1e-6 { 1.0 } else {
                    let x = std::f32::consts::PI * dist / airy_radius;
                    let j1 = bessel_j1_approx(x);
                    (2.0 * j1 / x).powi(2)
                };
                psf[i] = (1.0 - airy_weight) * gauss + airy_weight * airy.max(0.0);
            }
        }

        let psf_sum: f32 = psf.iter().sum();
        psf.iter_mut().for_each(|v| *v /= psf_sum.max(1e-9));
        psf
    }
}

// ==========================================
// ELITE V4: TV-REGULARIZED DECONVOLUTION
// ==========================================

pub struct TvRlDeconvolver {
    pub max_iterations: u32,
    pub tv_lambda: f32,
    pub convergence_eps: f32,
    pub snr_floor: f32,
}

impl TvRlDeconvolver {
    pub fn deconvolve(
        &self,
        image: &[f32],
        width: usize,
        height: usize,
        psf: &[f32],
        psf_size: usize,
        snr_map: &[f32],
        planet_mask: &[f32],
    ) -> Vec<f32> {
        let n = width * height;
        let mut flipped_psf = vec![0.0f32; psf_size * psf_size];
        for y in 0..psf_size {
            for x in 0..psf_size {
                flipped_psf[(psf_size - 1 - y) * psf_size + (psf_size - 1 - x)] = psf[y * psf_size + x];
            }
        }

        let mut u = image.to_vec();
        let mut u_prev = u.clone();

        for iter in 0..self.max_iterations {
            let h_u = self.convolve_direct(&u, width, height, psf, psf_size);
            let mut ratio = vec![1.0f32; n];
            for i in 0..n {
                if planet_mask[i] > 0.01 {
                    ratio[i] = (image[i] / (h_u[i] + 1e-7)).clamp(0.01, 100.0);
                }
            }

            let correction = self.convolve_direct(&ratio, width, height, &flipped_psf, psf_size);

            for i in 0..n {
                if planet_mask[i] < 0.01 { continue; }
                let snr_factor = (snr_map[i] / self.snr_floor).clamp(0.0, 1.0).powi(2);
                let rl_update = u[i] * correction[i];
                u[i] = (rl_update * snr_factor + u[i] * (1.0 - snr_factor)).clamp(0.0, 65535.0);
            }

            let mut diff = 0.0f32;
            let mut count = 0.0f32;
            for i in 0..n {
                if planet_mask[i] > 0.5 {
                    diff += ((u[i] - u_prev[i]) / (u_prev[i] + 1.0)).abs();
                    count += 1.0;
                }
            }
            if iter > 5 && (diff / count.max(1.0)) < self.convergence_eps { break; }
            u_prev.copy_from_slice(&u);
        }
        
        // Optional smoothing pass to replace the TV regularizer
        if self.tv_lambda > 0.001 {
            let mut u_smooth = u.clone();
            for y in 1..height-1 {
                for x in 1..width-1 {
                    let i = y * width + x;
                    if planet_mask[i] > 0.01 {
                        let sum = u[i-1] + u[i+1] + u[i-width] + u[i+width] + u[i] * 4.0;
                        u_smooth[i] = sum / 8.0;
                        u_smooth[i] = u[i] * (1.0 - self.tv_lambda) + u_smooth[i] * self.tv_lambda;
                    }
                }
            }
            u = u_smooth;
        }

        u
    }
    // Unused: compute_tv_gradient

    fn convolve_direct(&self, img: &[f32], w: usize, h: usize, kernel: &[f32], ks: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; w * h];
        let half = (ks / 2) as isize;
        for y in 0..h {
            for x in 0..w {
                let mut sum = 0.0f32;
                let mut wsum = 0.0f32;
                for ky in 0..ks {
                    let sy = y as isize + ky as isize - half;
                    if sy < 0 || sy >= h as isize { continue; }
                    for kx in 0..ks {
                        let sx = x as isize + kx as isize - half;
                        if sx < 0 || sx >= w as isize { continue; }
                        let k = kernel[ky * ks + kx];
                        sum += k * img[sy as usize * w + sx as usize];
                        wsum += k;
                    }
                }
                out[y * w + x] = if wsum > 1e-9 { sum / wsum } else { 0.0 };
            }
        }
        out
    }
}

// ==========================================
// ELITE V4: SIGMA CLIPPING & SNR
// ==========================================



pub fn compute_snr_map(stacked: &[f32], w: usize, h: usize, noise_sigma: f32) -> Vec<f32> {
    let smoothed = atrous_smooth(stacked, w, h, 2);
    stacked.iter().zip(smoothed.iter()).map(|(&raw, &smooth)| {
        let signal = smooth.max(0.0);
        let local_noise = (raw - smooth).abs().max(noise_sigma);
        (signal / local_noise).clamp(0.0, 100.0)
    }).collect()
}

pub fn compute_planet_mask(stacked: &[f32], w: usize, h: usize, feather: usize) -> Vec<f32> {
    let coarse = atrous_smooth(stacked, w, h, 4);
    let thresh = otsu_threshold(&coarse);
    let binary: Vec<f32> = coarse.iter().map(|&v| if v > thresh { 1.0 } else { 0.0 }).collect();
    let closed = morphological_dilate(&morphological_erode(&binary, w, h, 8), w, h, 8);
    let dist = compute_distance_transform(&closed, w, h);
    dist.iter().map(|&d| {
        if d >= feather as f32 { 1.0 }
        else { (d / feather as f32).clamp(0.0, 1.0).powf(0.5) }
    }).collect()
}


#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct EliteConfig {
    pub psf_radius: usize,
    pub fwhm_pixels: f32,
    pub airy_weight: f32,
    pub auto_psf_from_limb: bool,
    pub deconv_iterations: u32,
    pub tv_lambda: f32,
    pub snr_floor: f32,
    pub rejection_threshold: f32,
    pub post_sharpen: f32,
    pub feather_px: usize,
}


// ══════════════════════════════════════════════════════════════════
// ARCHITECTURE BY CATEGORY
// ══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TargetCategory {
    PlanetLarge,
    PlanetSmall,
    Surface,
}

impl TargetCategory {
    pub fn from_str(s: &str) -> Self {
        let key = s.to_lowercase();
        if key.contains("planet_large") || key.contains("grande") || key.contains("large") {
            TargetCategory::PlanetLarge
        } else if key.contains("planet")
            || key.contains("peque")
            || key.contains("small")
            || key.contains("fase")
            || key.contains("phase")
        {
            TargetCategory::PlanetSmall
        } else {
            TargetCategory::Surface
        }
    }

    pub fn profile(&self) -> CategoryProfile {
        match self {
            TargetCategory::PlanetLarge  => CategoryProfile::planet_large(),
            TargetCategory::PlanetSmall  => CategoryProfile::planet_small(),
            TargetCategory::Surface      => CategoryProfile::surface(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CategoryProfile {
    // ── Selección de frames ────────────────────────────────────────────
    pub rejection_percentile:    f32,
    pub sigmoid_steepness:       f32,
    pub quality_window_radius:   usize,

    // ── Stack ──────────────────────────────────────────────────────────
    pub kappa_sigma:             f32,
    pub poisson_iterations:      usize,
    pub poisson_weight:          f32,
    pub drizzle_drop_size:       f32,

    // ── PSF ───────────────────────────────────────────────────────────
    pub psf_radius:              usize,
    pub psf_fwhm_pixels:         f32,
    pub psf_airy_weight:         f32,
    pub use_limb_psf:            bool,

    // ── Deconvolución ────────
    pub deconv_enabled:          bool,
    pub deconv_iterations:       u32,
    pub tv_lambda:               f32,
    pub snr_floor_deconv:        f32,
    pub deconv_blend:            f32,

    // ── Sharpening à trous ────────────────────────────────────────────
    pub atrous_scales:           usize,
    pub atrous_weights:          [f32; 6],
    pub atrous_aggressiveness:   f32,

    // ── Canal de procesamiento ────────────────────────────────────────
    pub use_luma_only:           bool,
    pub luma_sharpen_boost:      f32,
    pub chroma_blur_sigma:       f32,

    // ── Máscara del objeto ────────────────────────────────────────────
    pub mask_feather_px:         usize,
    pub mask_dilation_px:        usize,
}

impl CategoryProfile {
    pub fn planet_large() -> Self {
        Self {
            rejection_percentile:  0.70,
            sigmoid_steepness:     12.0,
            quality_window_radius: 10,
            kappa_sigma:           2.8,
            poisson_iterations:    100,
            poisson_weight:        0.55,
            drizzle_drop_size:     0.7,
            psf_radius:            9,
            psf_fwhm_pixels:       2.5,
            psf_airy_weight:       0.35,
            use_limb_psf:          true,
            deconv_enabled:        true,
            deconv_iterations:     12,
            tv_lambda:             0.025,
            snr_floor_deconv:      8.0,
            deconv_blend:          0.35,
            atrous_scales:         5,
            atrous_weights:        [0.0, 1.85, 1.40, 0.80, 0.30, 1.0],
            atrous_aggressiveness: 0.85,
            use_luma_only:         true,
            luma_sharpen_boost:    1.2,
            chroma_blur_sigma:     0.8,
            mask_feather_px:       14,
            mask_dilation_px:      8,
        }
    }

    pub fn planet_small() -> Self {
        Self {
            rejection_percentile:  0.80,
            sigmoid_steepness:     15.0,
            quality_window_radius: 6,
            kappa_sigma:           3.0,
            poisson_iterations:    60,
            poisson_weight:        0.40,
            drizzle_drop_size:     0.6,
            psf_radius:            5,
            psf_fwhm_pixels:       1.8,
            psf_airy_weight:       0.55,
            use_limb_psf:          false,
            deconv_enabled:        false,
            deconv_iterations:     8,
            tv_lambda:             0.04,
            snr_floor_deconv:      12.0,
            deconv_blend:          0.0,
            atrous_scales:         4,
            atrous_weights:        [0.0, 2.10, 1.20, 0.50, 0.20, 1.0],
            atrous_aggressiveness: 0.70,
            use_luma_only:         true,
            luma_sharpen_boost:    1.4,
            chroma_blur_sigma:     1.2,
            mask_feather_px:       6,
            mask_dilation_px:      4,
        }
    }

    pub fn surface() -> Self {
        Self {
            rejection_percentile:  0.85,
            sigmoid_steepness:     14.0,
            quality_window_radius: 8,
            kappa_sigma:           2.5,
            poisson_iterations:    150,
            poisson_weight:        0.70,
            drizzle_drop_size:     0.65,
            psf_radius:            7,
            psf_fwhm_pixels:       2.0,
            psf_airy_weight:       0.45,
            use_limb_psf:          true,
            deconv_enabled:        true,
            deconv_iterations:     10,
            tv_lambda:             0.018,
            snr_floor_deconv:      6.0,
            deconv_blend:          0.28,
            atrous_scales:         6,
            atrous_weights:        [0.0, 1.60, 1.80, 1.20, 0.60, 0.20],
            atrous_aggressiveness: 1.0,
            use_luma_only:         true,
            luma_sharpen_boost:    1.0,
            chroma_blur_sigma:     0.5,
            mask_feather_px:       0,
            mask_dilation_px:      0,
        }
    }
}

// ══════════════════════════════════════════════════════════════════
// LUMA CHROMA SEPARATION
// ══════════════════════════════════════════════════════════════════


// ══════════════════════════════════════════════════════════════════
// PIPELINE COMPONENTS
// ══════════════════════════════════════════════════════════════════

fn sharpen_atrous_masked(
    luma: &[f32],
    width: usize,
    height: usize,
    snr_map: &[f32],
    planet_mask: &[f32],
    profile: &CategoryProfile,
    noise_floor: f32,
) -> Vec<f32> {
    let decomp = AtrousDecomposition::decompose(luma, width, height, profile.atrous_scales);
    let mut result = decomp.coarse_residual.clone();

    for (scale, detail) in decomp.detail_planes.iter().enumerate() {
        let base_amp = profile.atrous_weights[scale.min(5)];

        if base_amp <= 0.0 || scale == 0 {
            for (r, &d) in result.iter_mut().zip(detail.iter()) {
                *r += d;
            }
            continue;
        }

        let noise_thresh = noise_floor * (1.5_f32).powi(scale as i32);

        for (i, (&d, r)) in detail.iter().zip(result.iter_mut()).enumerate() {
            let abs_d = d.abs();

            let signal_gate = if abs_d > noise_thresh {
                ((abs_d - noise_thresh) / (noise_thresh + 1.0))
                    .clamp(0.0, 1.0)
                    .powf(0.7)
            } else {
                0.0
            };

            let snr_gate = ((snr_map[i] - 3.0) / 12.0).clamp(0.0, 1.0).powf(1.2);
            let mask_gate = planet_mask[i];

            let effective_amp = 1.0 + (base_amp - 1.0)
                * signal_gate * snr_gate * mask_gate
                * profile.luma_sharpen_boost;

            *r += d * effective_amp;
        }
    }

    result.iter().map(|&v| v.clamp(0.0, 65535.0)).collect()
}

fn psf_is_valid(psf: &[f32], radius: usize) -> bool {
    let size = 2 * radius + 1;
    let sum: f32 = psf.iter().sum();
    if (sum - 1.0).abs() > 0.3 { return false; }

    let center = radius * size + radius;
    let center_val = psf.get(center).copied().unwrap_or(0.0);
    let max_val = psf.iter().cloned().fold(f32::MIN, f32::max);

    (center_val - max_val).abs() < max_val * 0.2
}

fn compute_limb_mask(
    planet_mask: &[f32],
    width: usize,
    height: usize,
    limb_width: usize,
) -> Vec<f32> {
    let eroded = morphological_erode(planet_mask, width, height, limb_width);
    planet_mask.iter()
        .zip(eroded.iter())
        .map(|(&m, &e)| (m - e).clamp(0.0, 1.0))
        .collect()
}


fn percentile_f32(data: &[f32], p: f32) -> f32 {
    if data.is_empty() { return 0.0; }
    let mut sorted = data.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((sorted.len() - 1) as f32 * p).round() as usize;
    sorted[idx]
}

/// Remuestreo Lanczos-3 de la ruta experimental histórica. Un campo vacío es
/// identidad; `2*w*h` representa desplazamientos fuente (dx,dy) por píxel y
/// seis valores representan una afín salida→fuente. Ya no existe el antiguo
/// stub que devolvía un lienzo negro si esta función se alcanzaba por API.
fn warp_frame_lanczos3(frame: &[f32], w: usize, h: usize, warp: &[f32]) -> Vec<f32> {
    let n = w.saturating_mul(h);
    if frame.len() < n || n == 0 {
        return vec![0.0; n];
    }
    if warp.is_empty() {
        return frame[..n].to_vec();
    }
    let lanczos = |x: f32| -> f32 {
        let x = x.abs();
        if x < 1.0e-6 {
            1.0
        } else if x >= 3.0 {
            0.0
        } else {
            let pix = std::f32::consts::PI * x;
            (pix.sin() / pix) * ((pix / 3.0).sin() / (pix / 3.0))
        }
    };
    let source_at = |x: usize, y: usize| -> (f32, f32) {
        if warp.len() >= n * 2 {
            let offset = (y * w + x) * 2;
            (x as f32 + warp[offset], y as f32 + warp[offset + 1])
        } else if warp.len() >= 6 {
            (
                warp[0] * x as f32 + warp[1] * y as f32 + warp[2],
                warp[3] * x as f32 + warp[4] * y as f32 + warp[5],
            )
        } else {
            (x as f32, y as f32)
        }
    };
    let mut output = vec![0.0f32; n];
    output
        .par_chunks_mut(w)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, value) in row.iter_mut().enumerate() {
                let (sx, sy) = source_at(x, y);
                if !sx.is_finite() || !sy.is_finite() {
                    continue;
                }
                let (x0, y0) = (sx.floor() as i32, sy.floor() as i32);
                let (mut sum, mut weight) = (0.0f64, 0.0f64);
                for ky in -2..=3 {
                    let py = y0 + ky;
                    if py < 0 || py >= h as i32 {
                        continue;
                    }
                    let wy = lanczos(sy - py as f32) as f64;
                    for kx in -2..=3 {
                        let px = x0 + kx;
                        if px < 0 || px >= w as i32 {
                            continue;
                        }
                        let ww = wy * lanczos(sx - px as f32) as f64;
                        sum += frame[py as usize * w + px as usize] as f64 * ww;
                        weight += ww;
                    }
                }
                if weight.abs() > 1.0e-9 {
                    *value = (sum / weight) as f32;
                }
            }
        });
    output
}




// ==========================================
// QA REGRESSION TESTS (ZAS quality helpers)
// ==========================================
#[cfg(test)]
mod zas_v3_tests {
    use super::*;

    #[test]
    fn decode_cache_expected_set_admits_full_suffix_or_nothing() {
        let sel: Vec<usize> = (0..1000).map(|i| i * 2).collect();
        // Cabe entera → selección completa.
        assert_eq!(decode_cache_expected_set(&sel, 100, 100_000), sel);
        // No cabe entera pero sí un sufijo (500 frames) → últimos 500.
        let suffix = decode_cache_expected_set(&sel, 100, 50_000);
        assert_eq!(suffix.len(), 500);
        assert_eq!(suffix[0], sel[500]);
        assert_eq!(*suffix.last().unwrap(), *sel.last().unwrap());
        // Sufijo <5% de la selección → nada (E/S inútil).
        assert!(decode_cache_expected_set(&sel, 100, 4_000).is_empty());
        // Bordes.
        assert!(decode_cache_expected_set(&[], 100, 100_000).is_empty());
        assert!(decode_cache_expected_set(&sel, 0, 100_000).is_empty());
    }

    #[test]
    fn robust_ref_combine_survives_nan_and_keeps_median_semantics() {
        // Un NaN colado (p.ej. 0/0 en un píxel sin cobertura) NO debe hacer
        // panic; total_cmp lo ordena al final y la mediana sigue siendo sana.
        let mut with_nan = vec![3.0f32, f32::NAN, 1.0, 2.0, 5.0, 4.0, 6.0];
        let med = robust_ref_combine(&mut with_nan);
        assert!(med.is_finite(), "la mediana no debe ser NaN: {med}");
        // Sin NaN: semántica idéntica a la anterior (mediana centrada).
        let mut clean = vec![9.0f32, 1.0, 5.0, 3.0, 7.0];
        assert_eq!(robust_ref_combine(&mut clean), 5.0);
    }

    #[test]
    fn ap_selection_workload_guard_accepts_boundary_and_rejects_excess() {
        let ram = 16 * 1024 * 1024 * 1024u64;
        let budget = ap_selection_memory_budget(20_000, 20_000, 1, ram).unwrap();
        assert!(budget > 0);
        let error = ap_selection_memory_budget(20_001, 20_000, 1, ram).unwrap_err();
        assert!(error.contains("400000000") || error.contains("400.000.000"), "{error}");
        assert!(error.contains("muestreo silencioso"), "{error}");
    }

    #[test]
    fn ap_selection_budget_reserves_os_proportionally_and_caps_matrix_memory() {
        let low = ap_selection_memory_budget(100, 100, 1, 64 * 1024 * 1024).unwrap();
        assert_eq!(low as u64, MIN_AP_ACCEPTANCE_MATRIX_BUDGET_BYTES);
        let critically_low = ap_selection_memory_budget(100, 100, 1, 8 * 1024 * 1024).unwrap();
        assert_eq!(critically_low as u64, 6 * 1024 * 1024);
        let high = ap_selection_memory_budget(100, 100, 1, u64::MAX).unwrap();
        assert_eq!(high as u64, MAX_AP_ACCEPTANCE_MATRIX_BYTES);
    }

    fn ram_inputs() -> PlanetaryRamInputs {
        PlanetaryRamInputs {
            available_ram: 16 * 1024 * 1024 * 1024,
            width_in: 1920,
            height_in: 1080,
            width_out: 2880,
            height_out: 1620,
            source_bytes_per_pixel: 2,
            is_color: true,
            double_pass: true,
            use_warp_map: true,
            surface_or_large_disc: false,
            robust_reference_frames: 12,
            hardware_threads: 16,
        }
    }

    #[test]
    fn planetary_ram_plan_rejects_fixed_canvas_before_forcing_a_worker() {
        let mut input = ram_inputs();
        input.available_ram = 2 * 1024 * 1024 * 1024;
        input.width_out = 8_000;
        input.height_out = 8_000;
        input.robust_reference_frames = 0;
        let error = plan_planetary_ram(input).unwrap_err();
        assert!(
            error.contains("lienzo")
                || error.contains("transición")
                || error.contains("postprocesado"),
            "{error}"
        );
    }

    #[test]
    fn planetary_ram_plan_adapts_robust_reference_to_safe_budget() {
        let mut input = ram_inputs();
        input.available_ram = 2 * 1024 * 1024 * 1024;
        input.width_in = 4_000;
        input.height_in = 3_000;
        input.width_out = 640;
        input.height_out = 480;
        input.source_bytes_per_pixel = 6;
        input.robust_reference_frames = MAX_ROBUST_REFERENCE_FRAMES;
        let plan = plan_planetary_ram(input).unwrap();
        assert!(plan.robust_reference_frames < MAX_ROBUST_REFERENCE_FRAMES);
        assert!(plan.robust_reference_peak_bytes <= plan.working_budget);
    }

    #[test]
    fn planetary_ram_plan_valid_peak_never_exceeds_budget() {
        let plan = plan_planetary_ram(ram_inputs()).unwrap();
        assert!(plan.stack_threads >= 1);
        assert!(plan.frames_per_batch >= 1);
        assert!(plan.estimated_stack_peak_bytes <= plan.working_budget);
        assert!(plan.robust_reference_peak_bytes <= plan.working_budget);
    }

    #[test]
    fn planetary_ram_plan_allows_tiny_job_under_memory_pressure() {
        let mut input = ram_inputs();
        input.available_ram = 1 * 1024 * 1024;
        input.width_in = 32;
        input.height_in = 32;
        input.width_out = 32;
        input.height_out = 32;
        input.is_color = false;
        input.double_pass = false;
        input.use_warp_map = false;
        input.robust_reference_frames = 0;
        input.hardware_threads = 4;
        let plan = plan_planetary_ram(input).unwrap();
        assert!(plan.working_budget <= input.available_ram);
        assert!(plan.working_budget > 0);
        assert!(plan.estimated_stack_peak_bytes <= plan.working_budget);
    }

    #[test]
    fn planetary_ram_plan_separates_stack_transition_and_post_peaks() {
        let color_input = ram_inputs();
        let color = plan_planetary_ram(color_input).unwrap();
        let n_in = (color_input.width_in * color_input.height_in) as u64;
        let n_out = (color_input.width_out * color_input.height_out) as u64;
        assert!(color.stack_fixed_bytes >= n_out * 32 * 3 + n_in * 40);
        assert!(color.transition_peak_bytes >= n_out * 40 * 3 + n_in * 40);
        assert!(color.post_peak_bytes >= n_out * (24 * 3 + 112) + n_in * 40);

        let mut mono_input = color_input;
        mono_input.is_color = false;
        let mono = plan_planetary_ram(mono_input).unwrap();
        assert!(mono.stack_fixed_bytes >= n_out * 32 + n_in * 28);
        assert!(mono.post_peak_bytes >= n_out * (24 + 72) + n_in * 28);
        assert!(color.stack_fixed_bytes > mono.stack_fixed_bytes);
    }

    #[test]
    fn reported_ser8_surface_geometry_uses_workers_instead_of_reserving_future_post() {
        let gib = 1024 * 1024 * 1024u64;
        let input = PlanetaryRamInputs {
            available_ram: 10 * gib,
            width_in: 3312,
            height_in: 5888,
            width_out: 3312,
            height_out: 5888,
            source_bytes_per_pixel: 1,
            is_color: true,
            double_pass: true,
            use_warp_map: true,
            surface_or_large_disc: true,
            robust_reference_frames: 12,
            hardware_threads: 10,
        };
        let plan = plan_planetary_ram(input).unwrap();
        assert!(plan.stack_threads >= 7, "plan={plan:?}");
        assert!(plan.estimated_stack_peak_bytes <= plan.working_budget);
        assert!(plan.transition_peak_bytes <= plan.working_budget);
        assert!(plan.post_peak_bytes <= plan.working_budget);

        let mut rgb48 = input;
        rgb48.source_bytes_per_pixel = 6;
        let rgb48_plan = plan_planetary_ram(rgb48).unwrap();
        // Cada cálculo redondea su margen 1.6x de forma independiente.
        assert!(
            rgb48_plan.bytes_per_frame.abs_diff(plan.bytes_per_frame * 3) <= 2,
            "ser8={plan:?}, rgb48={rgb48_plan:?}"
        );
    }

    #[test]
    fn robust_reference_ties_keep_the_same_deterministic_best_in_top_n() {
        let mut frames: Vec<FrameAlignmentData> = (0..25)
            .map(|index| {
                let mut frame = FrameAlignmentData::empty(index);
                frame.score = 42;
                frame
            })
            .collect();
        let best = frames
            .iter()
            .min_by(|a, b| planetary_frame_quality_order(a, b))
            .unwrap()
            .idx;
        frames.sort_by(planetary_frame_quality_order);
        let top_n: Vec<usize> = frames.iter().take(20).map(|frame| frame.idx).collect();
        assert_eq!(best, 0);
        assert!(top_n.contains(&best));
        assert_eq!(top_n, (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn robust_reference_keeps_best_frame_in_each_uncovered_pixel() {
        let master = vec![11u16, 22, 33, 44, 55, 66];
        let mut reference =
            reference_frame_to_rgb(&master, 3, 2, false, 0).unwrap();
        // Simula un único píxel cubierto por la combinación robusta. Los otros
        // cinco (incluidos todos los bordes) deben conservar el mejor frame.
        reference[3..6].copy_from_slice(&[100, 101, 102]);
        assert_eq!(&reference[0..3], &[11, 11, 11]);
        assert_eq!(&reference[3..6], &[100, 101, 102]);
        assert_eq!(&reference[15..18], &[66, 66, 66]);
    }

    #[test]
    fn robust_reference_accepts_direct_rgb_and_bgr_geometry_without_redemosaic() {
        let rgb = vec![10u16, 20, 30, 40, 50, 60];
        assert_eq!(
            reference_frame_to_rgb(&rgb, 2, 1, true, 100).unwrap(),
            rgb
        );

        let bgr = vec![30u16, 20, 10, 60, 50, 40];
        assert_eq!(
            reference_frame_to_rgb(&bgr, 2, 1, true, 101).unwrap(),
            vec![10u16, 20, 30, 40, 50, 60]
        );

        assert_eq!(
            reference_frame_to_rgb_owned(rgb.clone(), 2, 1, true, 100).unwrap(),
            rgb
        );
        assert_eq!(
            reference_frame_to_rgb_owned(bgr, 2, 1, true, 101).unwrap(),
            vec![10u16, 20, 30, 40, 50, 60]
        );
        assert_eq!(
            reference_frame_to_green(&[10u16, 20, 30, 40, 50, 60], 2, 1, true, 100)
                .unwrap(),
            vec![20u16, 50]
        );
    }

    #[test]
    fn robust_reference_rejects_only_truly_malformed_geometry() {
        let error = reference_frame_to_rgb(&[1u16; 5], 2, 1, true, 100).unwrap_err();
        assert!(error.contains("recibió 5 muestras"), "{error}");
        assert!(error.contains("2 para mono/CFA o 6 para RGB/BGR"), "{error}");
    }

    #[test]
    fn macos_zero_available_snapshot_recovers_accounted_reclaimable_ram() {
        let gib = 1024 * 1024 * 1024u64;
        assert_eq!(resolve_planetary_available_memory(0, 16 * gib, 9 * gib), 7 * gib);
        assert_eq!(
            resolve_planetary_available_memory(5 * gib, 16 * gib, 9 * gib),
            7 * gib
        );
        assert_eq!(resolve_planetary_available_memory(3 * gib, 0, 0), 3 * gib);
        assert_eq!(resolve_planetary_available_memory(0, 16 * gib, 0), 0);
    }

    #[test]
    fn reported_mac_geometry_no_longer_collapses_to_sixteen_mb_budget() {
        let gib = 1024 * 1024 * 1024u64;
        let mut input = ram_inputs();
        input.available_ram = resolve_planetary_available_memory(0, 24 * gib, 8 * gib);
        input.width_in = 3312;
        input.height_in = 5888;
        input.width_out = 3312;
        input.height_out = 5888;
        input.source_bytes_per_pixel = 6;
        input.robust_reference_frames = MAX_ROBUST_REFERENCE_FRAMES;
        input.hardware_threads = 10;
        let plan = plan_planetary_ram(input).unwrap();
        assert!(plan.working_budget > 12 * gib);
        assert!(plan.robust_reference_frames > 0);
        assert!(plan.robust_reference_peak_bytes <= plan.working_budget);
    }

    #[test]
    fn empty_scratch_pool_is_an_error_path_not_a_panic() {
        let pool: std::sync::Mutex<Vec<LiquidScratch>> =
            std::sync::Mutex::new(Vec::new());
        assert!(ScratchLease::try_take(&pool).is_none());
    }

    #[test]
    fn fallible_large_plane_rejects_address_space_overflow() {
        let error = try_filled_vec::<u64>(usize::MAX, 0, "plano de prueba").unwrap_err();
        assert!(error.contains("espacio direccionable"), "{error}");
    }

    #[test]
    fn robust_reference_combine_is_bit_exact_without_per_pixel_devs_allocation() {
        fn legacy(vals: &mut Vec<f32>) -> f32 {
            let n = vals.len();
            if n == 0 {
                return 0.0;
            }
            if n < 5 {
                return vals.iter().sum::<f32>() / n as f32;
            }
            vals.sort_by(f32::total_cmp);
            let med = if n % 2 == 1 {
                vals[n / 2]
            } else {
                0.5 * (vals[n / 2 - 1] + vals[n / 2])
            };
            let mut devs: Vec<f32> = vals.iter().map(|&v| (v - med).abs()).collect();
            devs.sort_by(f32::total_cmp);
            let mad = if n % 2 == 1 {
                devs[n / 2]
            } else {
                0.5 * (devs[n / 2 - 1] + devs[n / 2])
            };
            let tol = (3.0 * 1.4826 * mad).max(med.abs() * 0.001 + 8.0);
            let (mut sum, mut kept) = (0.0f32, 0u32);
            for &value in vals.iter() {
                if (value - med).abs() <= tol {
                    sum += value;
                    kept += 1;
                }
            }
            if kept > 0 { sum / kept as f32 } else { med }
        }

        let cases = vec![
            vec![],
            vec![42.0],
            vec![1.0, 2.0, 3.0, 4.0],
            vec![100.0, 101.0, 99.0, 100.0, 50_000.0],
            vec![1.0, 2.0, 2.0, 3.0, 4.0, 40_000.0],
            (0..MAX_ROBUST_REFERENCE_FRAMES)
                .map(|index| 1_000.0 + (index % 7) as f32 * 0.25)
                .collect(),
        ];
        for case in cases {
            let mut expected_values = case.clone();
            let expected = legacy(&mut expected_values);
            let mut actual_values = case;
            let actual = robust_ref_combine(&mut actual_values);
            assert_eq!(actual.to_bits(), expected.to_bits());
        }

        // Cobertura determinista amplia: duplicados, outliers y todos los N
        // permitidos. Sólo se generan f32 finitos, igual que la interpolación
        // bilineal u16 del pipeline real.
        let mut state = 0xA5A5_0123_89AB_CDEFu64;
        for n in 5..=MAX_ROBUST_REFERENCE_FRAMES {
            for iteration in 0..2_000usize {
                let mut case = Vec::with_capacity(n);
                for sample in 0..n {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    let mut value = ((state >> 24) & 0xffff) as f32;
                    if sample > 0 && sample % 5 == 0 {
                        value = case[sample - 1];
                    }
                    if sample + 1 == n && iteration % 13 == 0 {
                        value = 65_535.0;
                    }
                    case.push(value);
                }
                let mut expected_values = case.clone();
                let expected = legacy(&mut expected_values);
                let actual = robust_ref_combine(&mut case);
                assert_eq!(
                    actual.to_bits(),
                    expected.to_bits(),
                    "paridad robusta falló con n={n}, iteración={iteration}"
                );
            }
        }
    }

    #[test]
    fn robust_reference_row_sampling_is_bit_exact_at_edges_and_subpixel_shifts() {
        let (width, height) = (13usize, 9usize);
        let ref_data: Vec<(Vec<u16>, (f32, f32))> = (0..MAX_ROBUST_REFERENCE_FRAMES)
            .map(|frame| {
                let pixels = (0..width * height * 3)
                    .map(|sample| ((sample * 977 + frame * 7_919) & 0xffff) as u16)
                    .collect();
                let dx = (frame % 7) as f32 * 0.375 - 1.125;
                let dy = (frame % 5) as f32 * 0.25 - 0.5;
                (pixels, (dx, dy))
            })
            .collect();

        for y in 0..height {
            let row_geo: Vec<Option<(usize, f32)>> = ref_data
                .iter()
                .map(|(pixels, (_dx, dy))| {
                    if pixels.is_empty() {
                        return None;
                    }
                    let syf = y as f32 - dy;
                    (syf >= 0.0 && syf < (height - 1) as f32)
                        .then_some((syf as usize, syf - syf.floor()))
                })
                .collect();
            let row_refs: Vec<RobustReferenceRow<'_>> = ref_data
                .iter()
                .filter_map(|(pixels, (dx, dy))| {
                    let syf = y as f32 - dy;
                    if pixels.is_empty() || syf < 0.0 || syf >= (height - 1) as f32 {
                        return None;
                    }
                    let y0 = syf as usize;
                    Some(RobustReferenceRow {
                        pixels,
                        dx: *dx,
                        fy: syf - syf.floor(),
                        row0_offset: y0 * width * 3,
                        row1_offset: (y0 + 1) * width * 3,
                    })
                })
                .collect();

            for x in 0..width {
                let (mut legacy_r, mut legacy_g, mut legacy_b) =
                    (Vec::new(), Vec::new(), Vec::new());
                for (frame_idx, (pixels, (dx, _dy))) in ref_data.iter().enumerate() {
                    let Some((y0, fy)) = row_geo[frame_idx] else {
                        continue;
                    };
                    let sxf = x as f32 - dx;
                    if sxf < 0.0 || sxf >= (width - 1) as f32 {
                        continue;
                    }
                    let x0 = sxf as usize;
                    let fx = sxf - x0 as f32;
                    let w00 = (1.0 - fx) * (1.0 - fy);
                    let w10 = fx * (1.0 - fy);
                    let w01 = (1.0 - fx) * fy;
                    let w11 = fx * fy;
                    let i00 = (y0 * width + x0) * 3;
                    let i01 = i00 + width * 3;
                    legacy_r.push(
                        pixels[i00] as f32 * w00
                            + pixels[i00 + 3] as f32 * w10
                            + pixels[i01] as f32 * w01
                            + pixels[i01 + 3] as f32 * w11,
                    );
                    legacy_g.push(
                        pixels[i00 + 1] as f32 * w00
                            + pixels[i00 + 4] as f32 * w10
                            + pixels[i01 + 1] as f32 * w01
                            + pixels[i01 + 4] as f32 * w11,
                    );
                    legacy_b.push(
                        pixels[i00 + 2] as f32 * w00
                            + pixels[i00 + 5] as f32 * w10
                            + pixels[i01 + 2] as f32 * w01
                            + pixels[i01 + 5] as f32 * w11,
                    );
                }

                let (mut actual_r, mut actual_g, mut actual_b) = (
                    [0.0f32; MAX_ROBUST_REFERENCE_FRAMES],
                    [0.0; MAX_ROBUST_REFERENCE_FRAMES],
                    [0.0; MAX_ROBUST_REFERENCE_FRAMES],
                );
                let count = sample_robust_reference_pixel(
                    &row_refs,
                    x,
                    width,
                    &mut actual_r,
                    &mut actual_g,
                    &mut actual_b,
                );
                assert_eq!(count, legacy_r.len());
                for sample in 0..count {
                    assert_eq!(actual_r[sample].to_bits(), legacy_r[sample].to_bits());
                    assert_eq!(actual_g[sample].to_bits(), legacy_g[sample].to_bits());
                    assert_eq!(actual_b[sample].to_bits(), legacy_b[sample].to_bits());
                }
            }
        }
    }

    fn native_yuv422_fixture(color_id: i32, width: usize, height: usize) -> Vec<u8> {
        assert_eq!(width % 2, 0, "YUV422 requiere parejas horizontales");
        let mut raw = Vec::with_capacity(width * height * 2);
        for y in 0..height {
            for x in (0..width).step_by(2) {
                let y0 = (24 + (y * width + x) * 17 % 208) as u8;
                let y1 = (24 + (y * width + x + 1) * 29 % 208) as u8;
                let u = (16 + (y * 31 + x * 13) % 224) as u8;
                let v = (16 + (y * 19 + x * 23) % 224) as u8;
                if color_id == 103 {
                    raw.extend_from_slice(&[u, y0, v, y1]);
                } else {
                    raw.extend_from_slice(&[y0, u, y1, v]);
                }
            }
        }
        raw
    }

    fn cache_fixture(frame_count: usize, exact: bool) -> (AnalysisCacheExpectation, CachedAnalysis) {
        let roi = Rect {
            x: 12,
            y: 18,
            w: 96,
            h: 80,
        };
        let expected = AnalysisCacheExpectation {
            source_fingerprint: 0xA55A_1234_9876,
            resolved_color_id: 8,
            target_type: "planet_small".into(),
            is_surface: false,
            warping_analysis: true,
            anchor_override: Some(vec![60, 58]),
            requested_roi: roi,
            width: 128,
            height: 120,
            declared_frame_count: frame_count,
            frame_count_exact: exact,
        };
        let stats: Vec<FrameAlignmentData> = (0..frame_count)
            .map(|idx| {
                let mut frame = FrameAlignmentData::empty(idx);
                frame.score = 1000 + idx as u64;
                frame
            })
            .collect();
        let cached = CachedAnalysis {
            scores: Vec::new(),
            roi,
            path_hash: expected.source_fingerprint,
            frame_stats: Some(stats),
            quality_graph: Some((0..frame_count).map(|idx| idx as f32).collect()),
            width: Some(expected.width),
            height: Some(expected.height),
            best_frame_idx: Some(frame_count.saturating_sub(1)),
            ap_points: None,
            contract: Some(expected.contract(roi)),
        };
        (expected, cached)
    }

    #[test]
    fn analysis_cache_suffix_keys_resolved_cfa_anchor_roi_and_target() {
        let roi = Rect {
            x: 10,
            y: 20,
            w: 300,
            h: 240,
        };
        let base = zenith_analysis_cache_suffix(
            "planet_small",
            false,
            true,
            8,
            Some(&[155, 141]),
            roi,
        );
        assert_ne!(
            base,
            zenith_analysis_cache_suffix(
                "planet_small",
                false,
                true,
                9,
                Some(&[155, 141]),
                roi,
            )
        );
        assert_ne!(
            base,
            zenith_analysis_cache_suffix(
                "planet_small",
                false,
                true,
                8,
                Some(&[156, 141]),
                roi,
            )
        );
        assert_ne!(
            base,
            zenith_analysis_cache_suffix(
                "planet_small",
                false,
                true,
                8,
                Some(&[155, 141]),
                Rect { x: 11, ..roi },
            )
        );
        assert_ne!(
            base,
            zenith_analysis_cache_suffix(
                "planet_large",
                false,
                true,
                8,
                Some(&[155, 141]),
                roi,
            )
        );
        assert!(base.contains("_c8_roi10-20-300-240_anc155x141_"));
    }

    #[test]
    fn analysis_cache_requires_contiguous_indices_and_authoritative_count() {
        let (exact_expected, mut cached) = cache_fixture(4, true);
        assert!(validate_analysis_cache(&cached, &exact_expected).is_ok());

        cached.frame_stats.as_mut().unwrap().remove(1);
        cached.quality_graph.as_mut().unwrap().remove(1);
        assert!(
            validate_analysis_cache(&cached, &exact_expected).is_err(),
            "un hueco nunca puede reutilizarse"
        );

        let (mut estimated_expected, mut estimated) = cache_fixture(4, false);
        estimated.frame_stats.as_mut().unwrap().pop();
        estimated.quality_graph.as_mut().unwrap().pop();
        estimated.best_frame_idx = Some(2);
        assert!(
            validate_analysis_cache(&estimated, &estimated_expected).is_ok(),
            "duration×fps puede diferir de un EOF limpio"
        );
        estimated_expected.frame_count_exact = true;
        estimated.contract = Some(estimated_expected.contract(estimated.roi));
        assert!(
            validate_analysis_cache(&estimated, &estimated_expected).is_err(),
            "un conteo autoritativo exige igualdad exacta"
        );
    }

    #[test]
    fn analysis_cache_contract_rejects_anchor_and_resolved_roi_mismatch() {
        let (mut expected, mut cached) = cache_fixture(3, true);
        expected.anchor_override = Some(vec![61, 58]);
        assert!(validate_analysis_cache(&cached, &expected).is_err());

        expected.anchor_override = Some(vec![60, 58]);
        cached.roi.x += 1;
        assert!(validate_analysis_cache(&cached, &expected).is_err());
    }

    #[test]
    fn analysis_cache_staging_is_unique_and_final_path_is_never_partial() {
        let (expected, cached) = cache_fixture(3, true);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "zas-analysis-atomic-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("capture.ser_planet_a10.analysis_v2");
        let target_s = target.to_string_lossy().into_owned();
        let first = stage_cached_analysis(&target_s, &cached).unwrap();
        let second = stage_cached_analysis(&target_s, &cached).unwrap();
        assert_ne!(first, second, "cada writer obtiene su propio temporal");
        assert!(!target.exists(), "stage/cancelación no publica el destino");
        std::fs::remove_file(second).unwrap();
        commit_staged_analysis(&target_s, &first).unwrap();
        let published = load_cached_analysis(&target_s).expect("caché final legible");
        assert!(validate_analysis_cache(&published, &expected).is_ok());
        assert!(!first.exists(), "rename consume el temporal");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn analysis_cache_envelope_bounds_and_crc_reject_corruption() {
        let (_expected, cached) = cache_fixture(5, true);
        let encoded = encode_cached_analysis(&cached).expect("envelope");
        assert!(parse_cached_analysis(&encoded).is_some());

        // Payload que sigue siendo LZ4/bincode descomprimible, pero cuyos
        // bytes ya no coinciden con el CRC del envelope.
        let compressed = &encoded[ANALYSIS_CACHE_HEADER_BYTES..];
        let mut bin = lz4_flex::decompress_size_prepended(compressed).unwrap();
        let last = bin.len() - 1;
        bin[last] ^= 0x01;
        let altered = lz4_flex::compress_prepend_size(&bin);
        let mut plausible = Vec::new();
        plausible.extend_from_slice(ANALYSIS_CACHE_PAYLOAD_MAGIC);
        plausible.extend_from_slice(&(bin.len() as u64).to_le_bytes());
        plausible.extend_from_slice(&(altered.len() as u64).to_le_bytes());
        plausible.extend_from_slice(&encoded[24..28]); // conservar CRC viejo
        plausible.extend_from_slice(&altered);
        assert!(parse_cached_analysis(&plausible).is_none());

        // El tamaño se rechaza antes de invocar LZ4, sin reservar GiB.
        let mut oversized = Vec::new();
        oversized.extend_from_slice(ANALYSIS_CACHE_PAYLOAD_MAGIC);
        oversized.extend_from_slice(
            &((ANALYSIS_CACHE_MAX_DECOMPRESSED_BYTES as u64) + 1).to_le_bytes(),
        );
        oversized.extend_from_slice(&4u64.to_le_bytes());
        oversized.extend_from_slice(&0u32.to_le_bytes());
        oversized.extend_from_slice(&0u32.to_le_bytes());
        assert!(parse_cached_analysis(&oversized).is_none());

        let mut legacy_bomb = vec![0u8; 32];
        legacy_bomb[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_cached_analysis(&legacy_bomb).is_none());
    }

    #[test]
    fn analysis_cache_fallback_is_discovered_when_primary_is_unwritable() {
        let (expected, cached) = cache_fixture(3, true);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let primary = std::env::temp_dir()
            .join(format!("zas-readonly-source-{nonce}"))
            .join("capture.ser_planet_a10.analysis_v2")
            .to_string_lossy()
            .into_owned();
        let fallback = get_analysis_cache_fallback_path(&primary);
        let _ = std::fs::remove_file(&fallback);
        std::fs::create_dir_all(Path::new(&fallback).parent().unwrap()).unwrap();
        let staged = stage_cached_analysis(&fallback, &cached).unwrap();
        commit_staged_analysis(&fallback, &staged).unwrap();

        let (loaded, location) =
            load_validated_analysis_cache(&primary, &expected).expect("fallback válido");
        assert_eq!(location, fallback);
        assert_eq!(loaded.path_hash, cached.path_hash);
        let _ = std::fs::remove_file(location);
    }

    #[test]
    fn native_analysis_read_error_is_not_converted_to_black() {
        let error = require_native_analysis_frame(Err("fallo de I/O deliberado".into()), 17)
            .expect_err("el error debe propagarse");
        assert!(error.contains("frame nativo 17"));
        assert!(error.contains("fallo de I/O deliberado"));
    }

    #[test]
    fn analysis_direct_color_is_exactly_the_stack_green_channel() {
        let (width, height) = (4usize, 3usize);
        for &(color_id, bpp) in &[(100, 3usize), (101, 3), (100, 6), (101, 6)] {
            let mut raw = Vec::with_capacity(width * height * bpp);
            let mut decoded = Vec::with_capacity(width * height * 3);
            for i in 0..width * height {
                let samples = [
                    (1000 + i * 97) as u16,
                    (9000 + i * 211) as u16,
                    (30000 + i * 131) as u16,
                ];
                decoded.extend_from_slice(&samples);
                if bpp == 3 {
                    raw.extend(samples.map(|v| (v / 257) as u8));
                } else {
                    for sample in samples {
                        raw.extend_from_slice(&sample.to_le_bytes());
                    }
                }
            }
            if bpp == 3 {
                for sample in &mut decoded {
                    *sample = (*sample / 257) * 257;
                }
            }

            let stack_rgb = debayer_to_rgb(&decoded, width, height, color_id);
            let expected: Vec<u16> = stack_rgb
                .chunks_exact(3)
                .map(|pixel| pixel[1])
                .collect();
            let mut scratch = Vec::new();
            let mut actual = Vec::new();
            raw_to_analysis_mono_into(
                &raw,
                width,
                height,
                bpp,
                color_id,
                0,
                0,
                &mut scratch,
                &mut actual,
            );
            assert_eq!(actual, expected, "RGB/BGR {bpp} B/px debe usar G");
        }
    }

    #[test]
    fn analysis_yuv422_is_exactly_the_stack_green_for_every_native_layout() {
        let (width, height) = (8usize, 3usize);
        for color_id in [12, 20, 102, 103] {
            let raw = native_yuv422_fixture(color_id, width, height);
            let decoded = raw_to_u16_buffer(&raw, width, height, 2);
            let expected: Vec<u16> = debayer_to_rgb(&decoded, width, height, color_id)
                .chunks_exact(3)
                .map(|pixel| pixel[1])
                .collect();

            let mut scratch = Vec::new();
            let mut actual = Vec::with_capacity(width * height);
            let mono_capacity = actual.capacity();
            raw_to_analysis_mono_into(
                &raw,
                width,
                height,
                2,
                color_id,
                0,
                0,
                &mut scratch,
                &mut actual,
            );
            assert_eq!(
                actual, expected,
                "análisis y apilado deben compartir G BT.601 exacto para CID={color_id}"
            );
            assert_eq!(
                actual.capacity(), mono_capacity,
                "el análisis YUV no debe reservar un RGB temporal"
            );
        }
    }

    #[test]
    fn analysis_yuv422_roi_preserves_pair_phase_and_stack_green() {
        let (width, height) = (8usize, 4usize);
        let roi = align_yuv422_analysis_roi(
            Rect {
                x: 1,
                y: 1,
                w: 5,
                h: 2,
            },
            width,
        );
        assert_eq!((roi.x, roi.w), (0, 6));
        assert_eq!(roi.x % 2, 0);
        assert_eq!(roi.w % 2, 0);

        for color_id in [12, 20, 102, 103] {
            let raw = native_yuv422_fixture(color_id, width, height);
            let decoded = raw_to_u16_buffer(&raw, width, height, 2);
            let stack_rgb = debayer_to_rgb(&decoded, width, height, color_id);
            let mut expected = Vec::with_capacity(roi.w * roi.h);
            for y in roi.y..roi.y + roi.h {
                for x in roi.x..roi.x + roi.w {
                    expected.push(stack_rgb[(y * width + x) * 3 + 1]);
                }
            }

            let (mut reference_scratch, mut reference) = (Vec::new(), Vec::new());
            raw_roi_to_analysis_mono_into(
                &raw,
                width,
                height,
                2,
                color_id,
                roi.x,
                roi.y,
                roi.w,
                roi.h,
                &mut reference_scratch,
                &mut reference,
            );

            let mut cropped = Vec::with_capacity(roi.w * roi.h * 2);
            for y in roi.y..roi.y + roi.h {
                let start = (y * width + roi.x) * 2;
                cropped.extend_from_slice(&raw[start..start + roi.w * 2]);
            }
            let (mut stream_scratch, mut stream) = (Vec::new(), Vec::new());
            raw_to_analysis_mono_into(
                &cropped,
                roi.w,
                roi.h,
                2,
                color_id,
                roi.x,
                roi.y,
                &mut stream_scratch,
                &mut stream,
            );

            assert_eq!(reference, expected, "referencia YUV CID={color_id}");
            assert_eq!(stream, expected, "stream YUV CID={color_id}");
        }
    }

    #[test]
    fn analysis_yuv422_cache_contract_invalidates_legacy_packed_luma() {
        let roi = Rect {
            x: 0,
            y: 0,
            w: 128,
            h: 96,
        };
        let yuv = zenith_analysis_cache_suffix(
            "planet_small",
            false,
            true,
            12,
            None,
            roi,
        );
        let mono = zenith_analysis_cache_suffix(
            "planet_small",
            false,
            true,
            0,
            None,
            roi,
        );
        assert!(yuv.ends_with("_yuvg1"));
        assert!(!mono.contains("_yuvg1"));
    }

    #[test]
    fn reference_roi_and_stream_prepare_identical_bayer_green() {
        let (full_width, full_height) = (14usize, 12usize);
        let (roi_x, roi_y, roi_width, roi_height) = (3usize, 1usize, 9usize, 8usize);
        let samples: Vec<u16> = (0..full_width * full_height)
            .map(|i| ((i * 977 + 1234) & 0xFFFF) as u16)
            .collect();
        let full_raw: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let mut cropped_raw = Vec::with_capacity(roi_width * roi_height * 2);
        for y in roi_y..roi_y + roi_height {
            for x in roi_x..roi_x + roi_width {
                cropped_raw.extend_from_slice(&samples[y * full_width + x].to_le_bytes());
            }
        }

        for color_id in 8..=11 {
            let (mut ref_scratch, mut reference) = (Vec::new(), Vec::new());
            raw_roi_to_analysis_mono_into(
                &full_raw,
                full_width,
                full_height,
                2,
                color_id,
                roi_x,
                roi_y,
                roi_width,
                roi_height,
                &mut ref_scratch,
                &mut reference,
            );
            let (mut stream_scratch, mut stream) = (Vec::new(), Vec::new());
            raw_to_analysis_mono_into(
                &cropped_raw,
                roi_width,
                roi_height,
                2,
                color_id,
                roi_x,
                roi_y,
                &mut stream_scratch,
                &mut stream,
            );
            assert_eq!(
                reference, stream,
                "referencia y CPU/GPU batch deben recibir el mismo verde CID={color_id}"
            );
        }
    }

    #[test]
    fn analysis_mono_samples_are_unchanged() {
        let values = [0u16, 1, 4095, 32768, 65535, 17777];
        let raw: Vec<u8> = values
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let (mut scratch, mut mono) = (Vec::new(), Vec::new());
        raw_to_analysis_mono_into(
            &raw,
            3,
            2,
            2,
            0,
            0,
            0,
            &mut scratch,
            &mut mono,
        );
        assert_eq!(mono, values);
        assert!(scratch.is_empty(), "mono no necesita scratch de color");
    }

    #[test]
    fn analysis_job_token_observes_supersession_even_when_global_cancel_is_cleared() {
        let active = Arc::new(AtomicUsize::new(41));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let token = PlanetaryJobToken::for_test(41, active.clone(), cancelled.clone());
        assert!(!token.is_cancelled());

        // Starting the next request clears the process-wide flag, which was the
        // exact old deadlock scenario. The per-job generation still cancels 41.
        cancelled.store(false, Ordering::Release);
        active.store(42, Ordering::Release);
        assert!(token.is_cancelled());
    }

    #[test]
    fn stream_decode_never_accepts_a_partial_or_failed_prefix() {
        assert!(validate_stream_decode(
            StreamDecodeOutcome::Failed {
                decoded: 17,
                error: "invalid data".into(),
            },
            100,
            false,
        )
        .is_err());
        assert!(validate_stream_decode(
            StreamDecodeOutcome::Complete { decoded: 99 },
            100,
            true,
        )
        .is_err());
        assert_eq!(
            validate_stream_decode(
                StreamDecodeOutcome::Complete { decoded: 99 },
                100,
                false,
            ),
            Ok(99),
            "un EOF limpio puede corregir una estimación duration×fps"
        );
        assert!(validate_stream_decode(
            StreamDecodeOutcome::Cancelled { decoded: 99 },
            100,
            false,
        )
        .is_err());
    }

    #[test]
    fn ffmpeg_hw_probe_rejects_successful_software_fallback() {
        let failed_videotoolbox = "Using auto hwaccel type videotoolbox with new default device.\n\
            VideoToolbox malfunction.\n\
            Failed setup for format videotoolbox_vld: hwaccel initialisation returned error.";
        assert_eq!(
            confirmed_ffmpeg_hardware_backend(failed_videotoolbox, true),
            None,
            "exit=0 no demuestra GPU cuando FFmpeg cayó a software"
        );
        assert_eq!(
            confirmed_ffmpeg_hardware_backend(
                "Using auto hwaccel type videotoolbox with new default device.",
                true,
            ),
            Some("videotoolbox".into())
        );
        assert_eq!(
            confirmed_ffmpeg_hardware_backend(
                "Decoder h264_cuda selected; CUDA frames context initialized.",
                true,
            ),
            Some("cuda".into())
        );
        assert_eq!(
            confirmed_ffmpeg_hardware_backend(
                "Decoder h264_cuda selected; falling back to software decoding.",
                true,
            ),
            None
        );
        assert_eq!(
            confirmed_ffmpeg_hardware_backend(
                "Using auto hwaccel type d3d11va with new default device.",
                false,
            ),
            None
        );
    }

    /// EXPERIMENTO BAYER-AWARE (decision con datos, no a ciegas): ¿la
    /// alineacion per-AP del apilado mejora si en videos color se mide sobre
    /// el verde CFA sin interpolar en vez del verde debayerizado?
    ///
    /// El debayer interpola el verde en los sitios R/B → dos clases de pixel
    /// con MTF distinta alineadas a la rejilla CFA (peine espacial) que puede
    /// sesgar el minimo SAD/LK hacia posiciones enteras. Se comparan, sobre
    /// un mosaico RGGB sintetico con shift sub-pixel conocido y el MISMO
    /// pipeline de medicion que produccion (enhance + SAD entero + LK):
    ///   A) verde debayerizado full-res (produccion actual, sc.mono_buf);
    ///   B) super-pixel (G1+G2)/2 a media resolucion, shift reescalado ×2;
    ///   C) super-pixel re-upsampleado bilinealmente a full-res (MTF uniforme,
    ///      coordenadas full-res preservadas — cableado barato si ganara).
    ///
    /// VEREDICTO (2026-07-06, este mismo test): A = 0.088 px, B = 0.098 px,
    /// C = 0.310 px de error medio. LA RUTA ACTUAL GANA: el sesgo del peine
    /// de debayer tras enhance+LK es MENOR que la perdida de media resolucion
    /// (B) o el blur de doble interpolacion (C). El item "Bayer-aware
    /// alignment" del roadmap queda CERRADO con datos: no cablear B ni C.
    /// El assert fija ademas 0.15 px como cota de regresion para la ruta de
    /// produccion sobre datos CFA.
    #[test]
    fn test_bayer_aware_alignment_experiment() {
        let (w, h) = (128usize, 128usize);
        // Escena CUASI-PERIODICA: 4 ondas planas inconmensurables en distintas
        // orientaciones — el pico de autocorrelacion es unico dentro de la
        // ventana de busqueda (una escena puramente periodica generaba falsos
        // minimos SAD a un periodo de distancia y el experimento media eso,
        // no la calidad sub-pixel de las rutas).
        let tau = std::f32::consts::TAU;
        let f = move |x: f32, y: f32| -> f32 {
            6000.0
                + 1800.0 * (x * tau / 5.3).sin() * (y * tau / 6.1).cos()
                + 1500.0 * ((x * 0.83 + y * 0.55) * tau / 7.7).sin()
                + 1400.0 * ((x * 0.31 - y * 0.95) * tau / 11.3).sin()
                + 1200.0 * ((x * 0.66 + y * 0.75) * tau / 17.9).cos()
        };
        // Mosaico RGGB con ganancias de color (R 0.75 / G 1.0 / B 0.55) y
        // promedio de caja 1x1 (supersampleo 4x4) como apertura del sensor.
        let render_cfa = |dx: f32, dy: f32| -> Vec<u16> {
            let mut raw = vec![0u16; w * h];
            for y in 0..h {
                for x in 0..w {
                    let gain = match (y & 1, x & 1) {
                        (0, 0) => 0.75, // R
                        (1, 1) => 0.55, // B
                        _ => 1.0,       // G (quincunx)
                    };
                    let mut s = 0.0f32;
                    for oy in 0..4 {
                        for ox in 0..4 {
                            let sx = x as f32 - dx - 0.375 + ox as f32 * 0.25;
                            let sy = y as f32 - dy - 0.375 + oy as f32 * 0.25;
                            s += f(sx, sy);
                        }
                    }
                    raw[y * w + x] = (s / 16.0 * gain).clamp(0.0, 65535.0) as u16;
                }
            }
            raw
        };

        let superpixel_green = |raw: &[u16]| -> Vec<u16> {
            let (hw, hh) = (w / 2, h / 2);
            let mut out = vec![0u16; hw * hh];
            for by in 0..hh {
                for bx in 0..hw {
                    let (x0, y0) = (bx * 2, by * 2);
                    let g1 = raw[y0 * w + x0 + 1] as u32; // G en (1,0) RGGB
                    let g2 = raw[(y0 + 1) * w + x0] as u32; // G en (0,1)
                    out[by * hw + bx] = ((g1 + g2) / 2) as u16;
                }
            }
            out
        };
        // Upsample bilineal half→full respetando la fase del super-pixel
        // (centro del bloque 2x2 en (2bx+1, 2by+1) - 0.5 = (x-0.5)/2 - 0.25).
        let upsample_uniform = |half: &[u16]| -> Vec<u16> {
            let (hw, hh) = (w / 2, h / 2);
            let mut out = vec![0u16; w * h];
            for y in 0..h {
                for x in 0..w {
                    let sx = ((x as f32 - 0.5) / 2.0 - 0.25).clamp(0.0, hw as f32 - 1.001);
                    let sy = ((y as f32 - 0.5) / 2.0 - 0.25).clamp(0.0, hh as f32 - 1.001);
                    let (x0, y0) = (sx as usize, sy as usize);
                    let (fx, fy) = (sx - x0 as f32, sy - y0 as f32);
                    let v = half[y0 * hw + x0] as f32 * (1.0 - fx) * (1.0 - fy)
                        + half[y0 * hw + x0 + 1] as f32 * fx * (1.0 - fy)
                        + half[(y0 + 1) * hw + x0] as f32 * (1.0 - fx) * fy
                        + half[(y0 + 1) * hw + x0 + 1] as f32 * fx * fy;
                    out[y * w + x] = v as u16;
                }
            }
            out
        };

        // Medicion identica a produccion: enhance + SAD entero + pulido LK.
        let measure = |master: &[u16], target: &[u16], mw: usize, mh: usize| -> (f32, f32) {
            let me = enhance_for_alignment(master, mw, mh);
            let te = enhance_for_alignment(target, mw, mh);
            let (cx, cy) = (mw / 2, mh / 2);
            let bx = if mw >= 96 { 48 } else { 24 };
            // Busqueda ±3: el shift verdadero es sub-pixel (como el residuo
            // per-AP tras el shift global del analisis en produccion).
            let (idx, idy, _sad) =
                crate::alignment::find_best_match_sad(&me, &te, mw, cx, cy, cx, cy, bx, 3);
            crate::alignment::refine_shift_lucas_kanade(
                &me, &te, mw, mh, cx, cy, idx, idy, bx, 3,
            )
            .unwrap_or((idx, idy))
        };

        let master_raw = render_cfa(0.0, 0.0);
        let m_green: Vec<u16> = debayer_to_rgb(&master_raw, w, h, 8)
            .chunks(3)
            .map(|p| p[1])
            .collect();
        let m_half = superpixel_green(&master_raw);
        let m_unif = upsample_uniform(&m_half);

        let shifts = [
            (0.10f32, 0.45f32), (0.30, 0.15), (0.50, 0.75),
            (0.70, 0.35), (0.90, 0.60), (0.25, 0.25),
        ];
        let (mut ea, mut eb, mut ec) = (0.0f32, 0.0f32, 0.0f32);
        for &(sx, sy) in &shifts {
            let t_raw = render_cfa(sx, sy);
            // A) verde debayerizado (produccion)
            let t_green: Vec<u16> = debayer_to_rgb(&t_raw, w, h, 8)
                .chunks(3)
                .map(|p| p[1])
                .collect();
            let (ax, ay) = measure(&m_green, &t_green, w, h);
            ea += ((ax - sx).powi(2) + (ay - sy).powi(2)).sqrt();
            // B) super-pixel half-res (shift medido ×2)
            let t_half = superpixel_green(&t_raw);
            let (bx2, by2) = measure(&m_half, &t_half, w / 2, h / 2);
            eb += ((bx2 * 2.0 - sx).powi(2) + (by2 * 2.0 - sy).powi(2)).sqrt();
            // C) verde uniforme full-res
            let t_unif = upsample_uniform(&t_half);
            let (ucx, ucy) = measure(&m_unif, &t_unif, w, h);
            ec += ((ucx - sx).powi(2) + (ucy - sy).powi(2)).sqrt();
        }
        let n = shifts.len() as f32;
        eprintln!(
            "BAYER-AWARE — error medio de alineacion (px full-res):\n  A verde debayer (actual): {:.4}\n  B super-pixel half-res:   {:.4}\n  C verde uniforme full-res: {:.4}",
            ea / n, eb / n, ec / n
        );
        // Cota de REGRESION para la ruta de produccion sobre CFA (medida
        // historica: 0.088 px) + sanidad de las rutas experimentales.
        assert!(ea / n < 0.15, "ruta A (produccion) degradada: {}", ea / n);
        assert!(eb / n < 0.35, "ruta B degenerada: {}", eb / n);
        assert!(ec / n < 0.50, "ruta C degenerada: {}", ec / n);
    }

    #[test]
    fn test_planetary_source_fingerprint_changes_with_same_path_and_size() {
        let dir = std::env::temp_dir().join(format!(
            "zas-source-fingerprint-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("capture.ser");
        std::fs::write(&source, vec![0x15u8; 128 * 1024]).unwrap();
        let first = planetary_source_fingerprint(source.to_str().unwrap()).unwrap();
        std::fs::write(&source, vec![0xA7u8; 128 * 1024]).unwrap();
        let second = planetary_source_fingerprint(source.to_str().unwrap()).unwrap();
        assert_ne!(first, second, "el contenido sobrescrito no puede reutilizar caché");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_legacy_lanczos_warp_identity_is_not_a_black_stub() {
        let (w, h) = (24usize, 20usize);
        let frame: Vec<f32> = (0..w * h).map(|index| 100.0 + index as f32).collect();
        assert_eq!(warp_frame_lanczos3(&frame, w, h, &[]), frame);
        let affine_identity = [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0];
        let warped = warp_frame_lanczos3(&frame, w, h, &affine_identity);
        for y in 3..h - 3 {
            for x in 3..w - 3 {
                let index = y * w + x;
                assert!((warped[index] - frame[index]).abs() < 1.0e-3);
            }
        }
    }

    /// PR-2.1 (decode único): un frame CRUDO del stream FFmpeg escrito por el
    /// ANÁLISIS (cache_decoded_raw_frame) debe leerse por la ruta del APILADO
    /// (read_cached_frame) con los mismos valores u16, tanto mono (gray16le)
    /// como color (rgb48le), y ser idempotente si el archivo ya existe.
    #[test]
    fn test_analysis_seeded_cache_matches_stack_reader() {
        let dir = std::env::temp_dir().join(format!("zas_seed_cache_{}", std::process::id()));
        // Aislamiento: partir de un directorio limpio (una corrida anterior
        // pudo dejar el frame idempotente escrito con otra longitud).
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let key = 0xABCD_EF01_2345_6789u64;
        // Color rgb48le 4x2: 24 muestras u16 little-endian.
        let vals: Vec<u16> = (0..24).map(|i| (i * 2749 + 13) as u16).collect();
        let mut raw = Vec::with_capacity(vals.len() * 2);
        for &v in &vals {
            raw.extend_from_slice(&v.to_le_bytes());
        }
        cache_decoded_raw_frame(&dir, key, 7, &raw);
        let read = read_cached_frame(&decode_cache_frame_path(&dir, key, 7), vals.len())
            .expect("frame sembrado por el análisis legible por el apilado");
        assert_eq!(read, vals);
        // Idempotencia: reescritura con contenido distinto NO debe pisar.
        let raw2 = vec![0u8; raw.len()];
        cache_decoded_raw_frame(&dir, key, 7, &raw2);
        let read2 = read_cached_frame(&decode_cache_frame_path(&dir, key, 7), vals.len()).unwrap();
        assert_eq!(read2, vals, "el archivo existente no debe reescribirse");
        // Longitud inesperada → None (validación del lector intacta).
        assert!(read_cached_frame(&decode_cache_frame_path(&dir, key, 7), vals.len() + 1).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_decode_cache_transaction_rolls_back_and_repairs_only_on_commit() {
        let dir = std::env::temp_dir().join(format!(
            "zas_dcache_transaction_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = 0xCAFE_BABE_1234_5678;
        let frame: Vec<u16> = (0..256).map(|i| (i * 211) as u16).collect();
        let final_path = decode_cache_frame_path(&dir, key, 4);

        {
            let tx = DecodeFrameCacheTransaction::new(&dir, key, frame.len(), 64 * 1024)
                .expect("staging");
            tx.write_frame(4, &frame);
            assert!(!final_path.exists(), "un intento no validado no se publica");
            // Drop sin commit simula decoder fallido/cancelado.
        }
        assert!(!final_path.exists(), "rollback debe retirar todo el staging");

        // Un destino corrupto tampoco bloquea para siempre la reparación.
        std::fs::write(&final_path, b"cache-corrupto").unwrap();
        let mut tx = DecodeFrameCacheTransaction::new(&dir, key, frame.len(), 64 * 1024)
            .expect("staging");
        tx.write_frame(4, &frame);
        assert!(read_cached_frame(&final_path, frame.len()).is_none());
        tx.commit();
        assert_eq!(read_cached_frame(&final_path, frame.len()).unwrap(), frame);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_decode_cache_expected_set_never_commits_a_partial_video() {
        let dir = std::env::temp_dir().join(format!(
            "zas_dcache_complete_set_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = 0xACED_0000_0000_0001;
        let frame = vec![321u16; 256];
        {
            let mut tx = DecodeFrameCacheTransaction::new(&dir, key, frame.len(), 1 << 20)
                .unwrap()
                .with_expected_indices([4usize, 5usize]);
            tx.write_frame(4, &frame);
            tx.commit();
            // commit detecta que falta #5; Drop debe retirar #4 del staging.
        }
        assert!(
            read_cached_frame(&decode_cache_frame_path(&dir, key, 4), frame.len()).is_none()
        );
        assert!(
            read_cached_frame(&decode_cache_frame_path(&dir, key, 5), frame.len()).is_none()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_decode_cache_failed_hardware_attempt_cannot_feed_cpu_retry() {
        let dir = std::env::temp_dir().join(format!(
            "zas_dcache_routes_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("capture.mp4");
        std::fs::write(&source, b"route provenance fixture").unwrap();
        let source = source.to_str().unwrap();
        let hardware_key = ffmpeg_decode_cache_key(
            source,
            16,
            16,
            2,
            0,
            0,
            "h264",
            "hardware:videotoolbox",
        );
        let cpu_key = ffmpeg_decode_cache_key(source, 16, 16, 2, 0, 0, "h264", "cpu");
        let frame0 = vec![111u16; 256];
        let frame1 = vec![222u16; 256];

        // Simula un intento hardware que produjo bytes parciales pero falló
        // antes del EOF/status validado: Drop hace rollback.
        {
            let tx = DecodeFrameCacheTransaction::new(&dir, hardware_key, 256, 1 << 20)
                .expect("staging hardware");
            tx.write_frame(0, &frame0);
            tx.write_frame(1, &frame1);
        }
        assert!(read_cached_frame(
            &decode_cache_frame_path(&dir, hardware_key, 0),
            256
        )
        .is_none());
        assert!(read_cached_frame(&decode_cache_frame_path(&dir, cpu_key, 0), 256).is_none());

        let mut cpu = DecodeFrameCacheTransaction::new(&dir, cpu_key, 256, 1 << 20)
            .expect("staging CPU");
        cpu.write_frame(0, &frame0);
        cpu.write_frame(1, &frame1);
        cpu.commit();
        assert_eq!(
            read_cached_frame(&decode_cache_frame_path(&dir, cpu_key, 1), 256).unwrap(),
            frame1
        );
        assert!(read_cached_frame(
            &decode_cache_frame_path(&dir, hardware_key, 1),
            256
        )
        .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_decode_cache_prune_removes_crashed_staging_directory() {
        let dir = std::env::temp_dir().join(format!(
            "zas_dcache_orphan_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let orphan = dir.join(".decode-attempt-4294967294-1-1");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("f_dead_000001.lz4"), vec![7u8; 4096]).unwrap();
        assert!(decode_cache_total_bytes(&dir) >= 4096);
        prune_decode_cache_to_budget(&dir, u64::MAX);
        assert!(!orphan.exists(), "un PID inexistente debe recogerse incluso bajo presupuesto");
        assert_eq!(decode_cache_total_bytes(&dir), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_decode_cache_budget_is_shared_across_transactions() {
        let dir = std::env::temp_dir().join(format!(
            "zas_dcache_shared_budget_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let frame: Vec<u16> = (0..8192)
            .map(|i| ((i * 4051 + i * i * 17) & 0xffff) as u16)
            .collect();
        let one = encode_cached_frame(&frame).unwrap().len() as u64;
        let budget = one + 32;
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = [0x1111u64, 0x2222u64]
            .into_iter()
            .map(|key| {
                let dir = dir.clone();
                let frame = frame.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut tx =
                        DecodeFrameCacheTransaction::new(&dir, key, frame.len(), budget)
                            .unwrap();
                    barrier.wait();
                    tx.write_frame(0, &frame);
                    tx.commit();
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let stable = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| is_decode_cache_entry(&entry.path()))
            .count();
        assert_eq!(stable, 1, "dos transacciones no pueden reservar el mismo presupuesto");
        assert!(decode_cache_total_bytes(&dir) <= budget);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_decode_frame_cache_roundtrip_and_prune() {
        let dir = std::env::temp_dir().join(format!("zas_dcache_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let source = dir.join("source.mp4");
        std::fs::write(&source, b"AAAA-same-sized-source").unwrap();
        let source_path = source.to_str().unwrap();
        let key = ffmpeg_decode_cache_key(source_path, 64, 48, 2, 0, 0, "h264", "cpu");
        assert_ne!(key, ffmpeg_decode_cache_key(source_path, 48, 64, 2, 0, 90, "h264", "cpu"));
        assert_ne!(key, ffmpeg_decode_cache_key(source_path, 64, 48, 6, 100, 0, "h264", "cpu"));
        assert_ne!(key, ffmpeg_decode_cache_key(source_path, 64, 48, 2, 0, 0, "hevc", "cpu"));
        assert_ne!(
            key,
            ffmpeg_decode_cache_key(
                source_path,
                64,
                48,
                2,
                0,
                0,
                "h264",
                "hardware:videotoolbox",
            ),
            "un fallback CPU no puede consumir hits de una ruta hardware"
        );
        std::fs::write(&source, b"BBBB-same-sized-source").unwrap();
        assert_ne!(
            key,
            ffmpeg_decode_cache_key(source_path, 64, 48, 2, 0, 0, "h264", "cpu"),
            "sobrescribir contenido conservando ruta/tamaño debe invalidar"
        );

        let frame: Vec<u16> = (0..64u32 * 48).map(|v| (v * 7 % 65536) as u16).collect();
        let p0 = decode_cache_frame_path(&dir, 0xABCD, 0);
        write_cached_frame(&p0, &frame);

        // Roundtrip fiel
        let back = read_cached_frame(&p0, frame.len()).expect("roundtrip");
        assert_eq!(back, frame);
        // Corrupción que todavía forma un stream LZ4 válido: el CRC32 interno
        // debe detectarla antes de deserializar/publicar píxeles incorrectos.
        let encoded = encode_cached_frame(&frame).unwrap();
        let mut payload = lz4_flex::decompress_size_prepended(&encoded).unwrap();
        let last = payload.len() - 1;
        payload[last] ^= 0x01;
        let checksum_bad = lz4_flex::compress_prepend_size(&payload);
        let p_checksum_bad = dir.join("f_checksum_bad.lz4");
        std::fs::write(&p_checksum_bad, checksum_bad).unwrap();
        assert!(
            read_cached_frame(&p_checksum_bad, frame.len()).is_none(),
            "un payload descomprimible pero alterado no puede ser cache hit"
        );
        // Longitud inesperada → rechazado (protege contra mezcla de formatos)
        assert!(read_cached_frame(&p0, frame.len() + 1).is_none());
        // Prefijo LZ4 gigante → rechazado sin intentar asignar GB
        let mut evil = vec![0xFFu8; 64];
        evil[0..4].copy_from_slice(&(u32::MAX).to_le_bytes());
        let p_evil = dir.join("evil.lz4");
        std::fs::write(&p_evil, &evil).unwrap();
        assert!(read_cached_frame(&p_evil, frame.len()).is_none());
        // Un sparse/archivo hostil se rechaza por metadata ANTES de fs::read.
        let p_sparse = dir.join("f_sparse_000000.lz4");
        let sparse = File::create(&p_sparse).unwrap();
        sparse
            .set_len(decode_cache_max_file_bytes(frame.len()) + 1)
            .unwrap();
        drop(sparse);
        assert!(read_cached_frame(&p_sparse, frame.len()).is_none());
        let _ = std::fs::remove_file(&p_sparse);

        // Poda LRU: 3 archivos con mtimes escalonados; presupuesto para ~1.
        let p1 = decode_cache_frame_path(&dir, 0xABCD, 1);
        let p2 = decode_cache_frame_path(&dir, 0xABCD, 2);
        write_cached_frame(&p1, &frame);
        // mtime de p0 al pasado (el mas viejo → primero en podarse)
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let _ = filetime_set(&p0, old);
        write_cached_frame(&p2, &frame);
        let one_file = std::fs::metadata(&p2).unwrap().len();
        prune_decode_cache_to_budget(&dir, one_file * 2 + 16);
        assert!(!p0.exists(), "el mas viejo debe podarse primero");
        assert!(p2.exists(), "lo recien escrito debe sobrevivir");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_decode_cache_partial_budget_keeps_valid_atomic_entries() {
        let dir = std::env::temp_dir().join(format!(
            "zas_dcache_budget_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let frame_a: Vec<u16> = (0..4096).map(|i| (i * 37) as u16).collect();
        let frame_b: Vec<u16> = (0..4096).map(|i| (i * 91 + 7) as u16).collect();
        let one_len = encode_cached_frame(&frame_a).unwrap().len() as u64;
        let used = std::sync::atomic::AtomicU64::new(0);
        let p_a = dir.join("a.lz4");
        let p_b = dir.join("b.lz4");
        write_cached_frame_budgeted(&p_a, &frame_a, &used, one_len);
        write_cached_frame_budgeted(&p_b, &frame_b, &used, one_len);
        assert!(p_a.exists(), "el primer frame que cabe debe persistir");
        assert!(!p_b.exists(), "el presupuesto no puede sobrepasarse");
        assert_eq!(read_cached_frame(&p_a, frame_a.len()).unwrap(), frame_a);
        assert!(std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .all(|entry| !entry.path().to_string_lossy().contains(".tmp-")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn target_category_routes_large_small_and_surface_profiles() {
        assert_eq!(TargetCategory::from_str("planet_large"), TargetCategory::PlanetLarge);
        assert_eq!(TargetCategory::from_str("planeta_grande"), TargetCategory::PlanetLarge);
        assert_eq!(TargetCategory::from_str("planet_small"), TargetCategory::PlanetSmall);
        assert_eq!(TargetCategory::from_str("fase_lunar"), TargetCategory::PlanetSmall);
        assert_eq!(TargetCategory::from_str("solar_surface"), TargetCategory::Surface);
        assert!((TargetCategory::PlanetLarge.profile().kappa_sigma - 2.8).abs() < f32::EPSILON);
        assert!((TargetCategory::PlanetSmall.profile().kappa_sigma - 3.0).abs() < f32::EPSILON);
        assert!((TargetCategory::Surface.profile().kappa_sigma - 2.5).abs() < f32::EPSILON);
    }

    // Helper del test: fija el mtime de un archivo sin dependencia externa
    // (abrir en append y truncar no cambia contenido; usamos File::set_times).
    fn filetime_set(p: &Path, t: std::time::SystemTime) -> std::io::Result<()> {
        let f = std::fs::OpenOptions::new().append(true).open(p)?;
        let times = std::fs::FileTimes::new().set_modified(t);
        f.set_times(times)
    }

    /// Reproduce el bug del quemado: una "Luna" donde verde y azul son mas
    /// brillantes que el rojo (tipico de captura de movil). La normalizacion
    /// vieja (punto blanco = solo canal rojo) recortaba verde/azul a 65535 en
    /// las altas luces = manchas quemadas. La version canal-segura NO debe
    /// recortar en masa ningun canal y debe conservar el detalle de highlights.
    #[test]
    fn test_surface_normalize_no_channel_burn() {
        let (w, h) = (64usize, 64usize);
        let mut buf = vec![0u16; w * h * 3];
        // Gradiente radial: centro brillante. R llega a 30000, G a 52000, B a
        // 48000 en el pico → con la norma vieja, G y B se habrian quemado.
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let rmax = (cx * cx + cy * cy).sqrt();
        for y in 0..h {
            for x in 0..w {
                let d = 1.0 - ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt() / rmax;
                let t = d.clamp(0.0, 1.0);
                let o = (y * w + x) * 3;
                buf[o] = (2000.0 + t * 28000.0) as u16; // R hasta 30000
                buf[o + 1] = (3000.0 + t * 49000.0) as u16; // G hasta 52000
                buf[o + 2] = (2500.0 + t * 45500.0) as u16; // B hasta 48000
            }
        }
        normalize_surface_stack_channelsafe(&mut buf);

        // NINGUN canal debe tener un cumulo de pixeles pegados a 65535 (=quemado).
        // Se permite el pico exacto (unos pocos px), pero no una mancha.
        let mut clipped = [0usize; 3];
        let mut peak = [0u16; 3];
        for px in buf.chunks_exact(3) {
            for c in 0..3 {
                if px[c] >= 65535 {
                    clipped[c] += 1;
                }
                peak[c] = peak[c].max(px[c]);
            }
        }
        let total = w * h;
        for c in 0..3 {
            assert!(
                clipped[c] * 100 < total, // < 1% del canal recortado
                "canal {c}: {} px quemados de {total} (recorte en masa)",
                clipped[c]
            );
        }
        // El canal MAS brillante (verde) debe llegar al rango alto (norma
        // efectiva) sin quemarse; los canales mas tenues se quedan
        // PROPORCIONALMENTE mas bajos — eso es color CORRECTO, no un defecto.
        assert!(peak[1] > 60000, "verde: pico {} — norma insuficiente", peak[1]);
        // Rojo (el mas tenue) NO debe alcanzar el maximo (preserva la relacion
        // de color): si estuviera pegado a 65535 seria el bug de quemado.
        assert!(peak[0] < peak[1], "rojo no debe igualar al verde (color roto)");
    }

    #[test]
    fn test_striped_accum_coverage_weighting() {
        // Dos "frames" con distinta cobertura de drop en un pixel interior:
        // con weight_by_coverage el frame de cobertura 0.25 debe pesar 4x
        // menos (media 120); apagado, ambos pesan igual (media 150). Verifica
        // ademas que el striping preserva la matematica del merge.
        let (w, h) = (8usize, 8usize);
        let mk = || StripedAccum::new(GradientDomainStacker::new_direct_only(w, h), 4);
        let f_a = vec![100.0f32; w * h];
        let f_b = vec![200.0f32; w * h];
        let w_a = vec![1.0f32; w * h];
        let w_b = vec![0.25f32; w * h];
        let i = 3 * w + 3; // pixel interior (el merge omite el borde de 1 px)

        let acc = mk();
        acc.accumulate(&f_a, &w_a, &[], 0, 0, 0.0, 0.0, 1.0, 0, true);
        acc.accumulate(&f_b, &w_b, &[], 0, 0, 0.0, 0.0, 1.0, 1, true);
        let g = acc.into_inner();
        let v = (g.direct[i] / g.direct_w[i]) as f32;
        assert!((v - 120.0).abs() < 1e-3, "cov ON: esperado 120, obtenido {v}");

        let acc = mk();
        acc.accumulate(&f_a, &w_a, &[], 0, 0, 0.0, 0.0, 1.0, 0, false);
        acc.accumulate(&f_b, &w_b, &[], 0, 0, 0.0, 0.0, 1.0, 1, false);
        let g = acc.into_inner();
        let v = (g.direct[i] / g.direct_w[i]) as f32;
        assert!((v - 150.0).abs() < 1e-3, "cov OFF: esperado 150, obtenido {v}");

        // En modo tracked Σw² debe usar el peso FINAL, incluida la cobertura:
        // W=1+0.25 y W2=1²+0.25². Éste es el N_eff real del drizzle.
        let acc = StripedAccum::new(GradientDomainStacker::new_direct_tracked(w, h), 4);
        acc.accumulate(&f_a, &w_a, &[], 0, 0, 0.0, 0.0, 1.0, 0, true);
        acc.accumulate(&f_b, &w_b, &[], 0, 0, 0.0, 0.0, 1.0, 1, true);
        let tracked = acc.into_inner();
        assert!((tracked.direct_w[i] - 1.25).abs() < 1e-12);
        assert!((tracked.direct_w2[i] - 1.0625).abs() < 1e-12);
    }

    #[test]
    fn dense_quality_is_neutral_on_flat_and_stationary_noise() {
        let (w, h) = (128usize, 96usize);
        let flat = DenseQualityMap::build(&vec![32_000.0; w * h], w, h, 3);
        assert!(!flat.reliable);
        assert!(flat.scores.iter().all(|&q| q == 1.0));

        // Ruido blanco determinista: su energia laplaciana local fluctua, pero
        // no contiene regiones de textura coherentes que deban ganar peso.
        let mut state = 0x1234_5678u32;
        let mut noisy = vec![0.0f32; w * h];
        for value in &mut noisy {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = ((state >> 24) as i32 - 128) as f32;
            *value = 32_000.0 + noise;
        }
        let noise_map = DenseQualityMap::build(&noisy, w, h, 3);
        assert!(!noise_map.reliable, "el ruido estacionario no es textura local fiable");
        assert!(noise_map.scores.iter().all(|&q| q == 1.0));
    }

    #[test]
    fn dense_quality_keeps_a_real_structured_edge() {
        let (w, h) = (128usize, 96usize);
        let mut edge = vec![1_000.0f32; w * h];
        for y in 0..h {
            for x in w / 2..w {
                edge[y * w + x] = 50_000.0;
            }
        }
        let map = DenseQualityMap::build(&edge, w, h, 3);
        assert!(map.reliable, "un borde coherente debe activar el mapa denso");
        assert!(map.scores.iter().any(|&q| q < 0.05));
        assert!(map.scores.iter().any(|&q| q > 0.95));
        assert!(map.scores.iter().all(|q| q.is_finite() && (0.0..=1.0).contains(q)));
    }

    #[test]
    fn dense_quality_registration_composes_roi_drizzle_and_shift() {
        // Mapa fuente 1/4 de un sensor 80x48. El valor codifica (x,y) para
        // comprobar exactamente que coordenada termina leyendo el acumulador.
        let (source_map_w, source_map_h) = (20usize, 12usize);
        let mut scores = vec![0.0f32; source_map_w * source_map_h];
        for y in 0..source_map_h {
            for x in 0..source_map_w {
                scores[y * source_map_w + x] = (100 * y + x) as f32;
            }
        }
        let source = DenseQualityMap {
            scores,
            width: source_map_w,
            height: source_map_h,
            reliable: true,
        };

        // ROI fuente (20,8), span efectivo 40x24; raster 60x36 a drizzle 1.5.
        let registered = source.into_reference_raster(
            80, 48, 60, 36, 1.5, 20.0, 8.0, 10, 6,
        );
        assert_eq!(registered.scores[0], 205.0); // sensor (20,8) -> dq (5,2)
        assert_eq!(registered.scores[5 * 10 + 9], 714.0);

        let q_off_x = DenseQualityMap::shift_offset(8.0, 10, 60, 1.5);
        let q_off_y = DenseQualityMap::shift_offset(-4.0, 6, 36, 1.5);
        assert!((q_off_x - 2.0).abs() < 1e-6);
        assert!((q_off_y + 1.0).abs() < 1e-6);

        // Pixel de salida (15,12): source=(15/1.5+20+8,
        // 12/1.5+8-4)=(38,12), es decir dq~(10,3). En el mapa ROI eso es
        // indice (5,1), cuyo valor codificado debe ser 310.
        let qx = ((15.0f32 * 10.0 / 60.0 + q_off_x).round() as usize).min(9);
        let qy = ((12.0f32 * 6.0 / 36.0 + q_off_y).round() as usize).min(5);
        assert_eq!(registered.scores[qy * 10 + qx], 310.0);

        // Y verifica la ruta real, no solo la formula del test: StripedAccum
        // debe aplicar q² exactamente en esa coordenada registrada.
        let acc = StripedAccum::new(GradientDomainStacker::new_direct_only(60, 36), 4);
        let frame = vec![1.0f32; 60 * 36];
        let coverage = vec![1.0f32; 60 * 36];
        acc.accumulate(
            &frame,
            &coverage,
            &registered.scores,
            registered.width,
            registered.height,
            q_off_x,
            q_off_y,
            1.0,
            0,
            false,
        );
        let merged = acc.into_inner();
        let i = 12 * 60 + 15;
        assert!((merged.direct_w[i] - 310.0f64.powi(2)).abs() < 1e-6);
    }

    #[test]
    fn test_large_lunar_disc_vs_small_planet() {
        let w = 256usize;
        let h = 256usize;

        // A big disc filling most of the frame (Moon-like) -> texture alignment.
        let mut moon = vec![300u16; w * h]; // background noise floor
        let (cx, cy, rad) = (w as f32 / 2.0, h as f32 / 2.0, 110.0f32);
        for y in 0..h {
            for x in 0..w {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                if dx * dx + dy * dy <= rad * rad {
                    moon[y * w + x] = 45000;
                }
            }
        }
        assert!(
            is_large_lunar_disc(&moon, w, h),
            "a disc filling most of the frame must use texture alignment"
        );

        // A tiny bright planet on a black sky -> keep CoG centering.
        let mut planet = vec![300u16; w * h];
        let prad = 16.0f32;
        for y in 0..h {
            for x in 0..w {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                if dx * dx + dy * dy <= prad * prad {
                    planet[y * w + x] = 60000;
                }
            }
        }
        assert!(
            !is_large_lunar_disc(&planet, w, h),
            "a small planetary disc must keep brightness CoG centering"
        );

        // Moon OVERFLOWING the frame (close-up: disc touches 3 corners, sky only
        // in the fourth). The old corner-based noise floor landed ON the disc and
        // broke the detection — this is the exact framing from the user report.
        let mut overflow = vec![300u16; w * h];
        let (ox, oy, orad) = (w as f32 * 0.72, h as f32 * 0.72, w as f32 * 0.95);
        for y in 0..h {
            for x in 0..w {
                let dx = x as f32 - ox;
                let dy = y as f32 - oy;
                if dx * dx + dy * dy <= orad * orad {
                    overflow[y * w + x] = 42000;
                }
            }
        }
        assert!(
            is_large_lunar_disc(&overflow, w, h),
            "a lunar disc overflowing the frame must still use texture alignment"
        );
    }

    #[test]
    fn test_gaussian_blur_effective_sigma_matches_request() {
        // Impulse response: blur a delta and measure the standard deviation of
        // the resulting kernel. The old box formula (missing /n) produced an
        // EFFECTIVE sigma ~1.73× the requested one — every wavelet band and the
        // RL/VC deconvolution PSF were coarser than labeled. The old 5-tap
        // "safe" variant ignored sigma entirely (always ~1.0).
        let w = 129usize;
        let h = 129usize;
        let measure = |img: &[f32]| -> f32 {
            let (mut sum, mut var) = (0.0f64, 0.0f64);
            let c = 64.0f64;
            for y in 0..h {
                for x in 0..w {
                    let v = img[y * w + x] as f64;
                    sum += v;
                    var += v * ((x as f64 - c).powi(2) + (y as f64 - c).powi(2));
                }
            }
            ((var / sum / 2.0).sqrt()) as f32 // isotropic: σ² per axis = var/2
        };
        let mut delta = vec![0.0f32; w * h];
        delta[64 * w + 64] = 10000.0;

        for &(req, tol) in &[(1.0f32, 0.35f32), (4.0, 0.6), (8.0, 1.0)] {
            let out = apply_gaussian_blur(&delta, w, h, req);
            let eff = measure(&out);
            assert!(
                (eff - req).abs() <= tol,
                "apply_gaussian_blur σ={} produced effective σ={}",
                req,
                eff
            );
        }

        // The "safe" variant must honour sigma too (it used to ignore it).
        let out = apply_gaussian_blur_safe(&delta, w, h, 6.0);
        let eff = measure(&out);
        assert!(
            (eff - 6.0).abs() <= 1.0,
            "apply_gaussian_blur_safe σ=6 produced effective σ={}",
            eff
        );
    }

    #[test]
    fn test_pearson_informativeness_gates_ap_selection() {
        // Correlated local scores (real seeing signal) → full weight (strict
        // local cutoff). Uncorrelated noise or flat scores → zero weight
        // (global ranking, relaxed cutoff).
        let n = 200usize;
        let global: Vec<f32> = (0..n).map(|i| 1000.0 + (i as f32) * 7.3).collect();

        // Strongly correlated (scaled + offset copy of global).
        let correlated: Vec<f32> = global.iter().map(|g| g * 0.6 + 300.0).collect();
        assert!(
            pearson_informativeness(&correlated, &global) > 0.95,
            "correlated scores must be treated as informative"
        );

        // Deterministic pseudo-noise, uncorrelated with the global ramp.
        let noise: Vec<f32> = (0..n)
            .map(|i| ((i as u32).wrapping_mul(2654435761) >> 16) as f32 % 977.0)
            .collect();
        assert!(
            pearson_informativeness(&noise, &global) < 0.25,
            "noise scores must be treated as uninformative"
        );

        // Flat scores carry no information at all.
        let flat = vec![500.0f32; n];
        assert_eq!(pearson_informativeness(&flat, &global), 0.0);
    }

    #[test]
    fn test_chroma_noise_estimator_gates_smoothing() {
        // Clean color stack → near-zero chroma noise (smoothing skipped);
        // noisy chroma → clearly above the radius-1 threshold.
        let w = 320usize;
        let h = 240usize;
        let n = w * h;

        // Clean: smooth luminance gradient, perfectly neutral color.
        let mut clean = vec![0u16; n * 3];
        for i in 0..n {
            let v = 8000 + ((i % w) * 40) as u16;
            clean[i * 3] = v;
            clean[i * 3 + 1] = v;
            clean[i * 3 + 2] = v;
        }
        let sigma_clean = estimate_stack_chroma_noise(&clean, w, h);
        assert!(
            sigma_clean < 20.0,
            "neutral stack must measure ~0 chroma noise (got {})",
            sigma_clean
        );

        // Noisy: independent per-channel noise ±600 → strong chroma noise.
        let mut noisy = clean.clone();
        for i in 0..n * 3 {
            let h32 = (i as u32).wrapping_mul(2654435761) >> 14;
            let d = (h32 % 1201) as i32 - 600;
            noisy[i] = (noisy[i] as i32 + d).clamp(0, 65535) as u16;
        }
        let sigma_noisy = estimate_stack_chroma_noise(&noisy, w, h);
        assert!(
            sigma_noisy > 120.0,
            "noisy chroma must trigger radius-2 smoothing (got {})",
            sigma_noisy
        );
    }

    #[test]
    fn test_auto_rgb_align_corrects_dispersion() {
        // Atmospheric dispersion: R and B physically displaced vs G by known
        // sub-pixel/px shifts. After align_stack_rgb_channels the channels
        // must coincide with G (interior), and the measured shifts must match
        // the injected ones.
        let w = 256usize;
        let h = 256usize;
        let pattern = |x: f32, y: f32| -> f32 {
            5000.0 + 3000.0 * (x * 0.11).sin() * (y * 0.07).sin() + 1500.0 * (x * 0.031 + y * 0.023).sin()
        };
        // Physically shifted channel: content moved by +s ⇒ ch(p) = pattern(p − s).
        let make = |sdx: f32, sdy: f32| -> Vec<f32> {
            (0..w * h)
                .map(|i| {
                    let x = (i % w) as f32;
                    let y = (i / w) as f32;
                    pattern(x - sdx, y - sdy)
                })
                .collect()
        };
        let (r_s, b_s) = ((1.6f32, -0.8f32), (-1.2f32, 0.6f32));
        let g_p = make(0.0, 0.0);
        let r_p = make(r_s.0, r_s.1);
        let b_p = make(b_s.0, b_s.1);

        let mut stacked = vec![0.0f32; w * h * 3];
        for i in 0..w * h {
            stacked[i * 3] = r_p[i];
            stacked[i * 3 + 1] = g_p[i];
            stacked[i * 3 + 2] = b_p[i];
        }

        let (rdx, rdy, bdx, bdy) = align_stack_rgb_channels(&mut stacked, w, h);
        assert!(
            (rdx - r_s.0).abs() < 0.35 && (rdy - r_s.1).abs() < 0.35,
            "R shift mismeasured: got ({}, {}) want ({}, {})",
            rdx, rdy, r_s.0, r_s.1
        );
        assert!(
            (bdx - b_s.0).abs() < 0.35 && (bdy - b_s.1).abs() < 0.35,
            "B shift mismeasured: got ({}, {}) want ({}, {})",
            bdx, bdy, b_s.0, b_s.1
        );

        // Interior residual after correction.
        let mut sum_r = 0.0f64;
        let mut sum_b = 0.0f64;
        let mut cnt = 0.0f64;
        for y in 8..h - 8 {
            for x in 8..w - 8 {
                let i = y * w + x;
                sum_r += ((stacked[i * 3] - stacked[i * 3 + 1]) as f64).powi(2);
                sum_b += ((stacked[i * 3 + 2] - stacked[i * 3 + 1]) as f64).powi(2);
                cnt += 1.0;
            }
        }
        let rms_r = (sum_r / cnt).sqrt();
        let rms_b = (sum_b / cnt).sqrt();
        assert!(rms_r < 80.0, "R residual too high after align: {}", rms_r);
        assert!(rms_b < 80.0, "B residual too high after align: {}", rms_b);

        // NO-FALSE-POSITIVE: already-aligned channels over smooth data with
        // per-channel noise (the flat SAD case that once painted a blue limb
        // fringe) must NOT be "corrected" — the improvement gate keeps it out.
        let mut aligned = vec![0.0f32; w * h * 3];
        // Non-linear hash (murmur3 finalizer): a plain multiplicative hash is
        // LINEAR, so constant seed deltas give constant hash deltas — a shifted
        // channel then genuinely matched the noise pattern and fooled the test.
        let hash = |seed: u32| -> u32 {
            let mut z = seed ^ 0x9E37_79B9;
            z ^= z >> 16;
            z = z.wrapping_mul(0x85EB_CA6B);
            z ^= z >> 13;
            z = z.wrapping_mul(0xC2B2_AE35);
            z ^ (z >> 16)
        };
        for i in 0..w * h {
            let x = (i % w) as f32;
            let y = (i / w) as f32;
            let base = 20000.0 + 500.0 * (x * 0.01).sin() * (y * 0.008).sin(); // very smooth
            aligned[i * 3] = base + (hash((i * 3) as u32) % 400) as f32 - 200.0;
            aligned[i * 3 + 1] = base + (hash((i * 3 + 1) as u32) % 400) as f32 - 200.0;
            aligned[i * 3 + 2] = base + (hash((i * 3 + 2) as u32) % 400) as f32 - 200.0;
        }
        let (zr_x, zr_y, zb_x, zb_y) = align_stack_rgb_channels(&mut aligned, w, h);
        assert!(
            zr_x.abs() < 0.3 && zr_y.abs() < 0.3 && zb_x.abs() < 0.3 && zb_y.abs() < 0.3,
            "aligned channels must not be corrected (got R({},{}) B({},{}))",
            zr_x, zr_y, zb_x, zb_y
        );
    }

    #[test]
    fn test_cog_precentering_recovers_large_handheld_motion() {
        // Handheld phone video of the Moon: a compact textured disc on black
        // sky jumping ~280 px between frames — far beyond the fixed SAD window
        // (~±128 px), which is exactly what produced displaced "ghost" stacks.
        // The centroid delta must pre-center the search so SAD refines to the
        // true shift.
        let w = 512usize;
        let h = 512usize;
        let disc = |cx: f32, cy: f32| -> Vec<u16> {
            let mut v = vec![300u16; w * h];
            for y in 0..h {
                for x in 0..w {
                    let dx = x as f32 - cx;
                    let dy = y as f32 - cy;
                    if dx * dx + dy * dy <= 40.0 * 40.0 {
                        // Textured disc so the SAD has structure to refine on.
                        v[y * w + x] = 30000 + ((x * 7 + y * 11) % 2000) as u16;
                    }
                }
            }
            v
        };
        let reference = disc(150.0, 150.0);
        let (true_dx, true_dy) = (280.0f32, 170.0f32);
        let target = disc(150.0 + true_dx, 150.0 + true_dy);

        assert!(
            is_compact_object_on_black(&reference, w, h),
            "small disc on black sky must enable CoG assist"
        );
        assert!(
            !is_compact_object_on_black(&vec![30000u16; w * h], w, h),
            "a full-frame bright field must NOT enable CoG assist"
        );

        let (rcx, rcy) = compute_robust_geometric_center(&reference, w, h, 0, 0);
        let (tcx, tcy) = compute_robust_geometric_center(&target, w, h, 0, 0);
        let init_dx = (tcx - rcx).round() as isize;
        let init_dy = (tcy - rcy).round() as isize;

        let ref_small = crate::alignment::downscale_integer(&reference, w, h, 4);
        let (dx, dy) = crate::alignment::find_best_match_sad_pyramid_offset(
            &reference, &target, &ref_small, w, h,
            w / 4, h / 4,          // small dims
            w / 4, h / 4,          // ROI origin (central box)
            w / 2, h / 2,          // ROI size
            32, 4, 4,              // same params as the master mini-alignment
            init_dx, init_dy,
        );

        assert!(
            (dx - true_dx).abs() < 3.0 && (dy - true_dy).abs() < 3.0,
            "CoG-assisted SAD must recover the large shift (got {}, {})",
            dx,
            dy
        );
    }

    fn tracked_scalar_samples(samples: &[(f32, f32)]) -> (GradientDomainStacker, usize) {
        let (w, h) = (5usize, 5usize);
        let i = 2 * w + 2;
        let mut acc = GradientDomainStacker::new_direct_tracked(w, h);
        let coverage = vec![1.0f32; w * h];
        for &(value, weight) in samples {
            acc.accumulate(
                &vec![value; w * h],
                &coverage,
                &[],
                0,
                0,
                0.0,
                0.0,
                weight,
            );
        }
        (acc, i)
    }

    #[test]
    fn sigma_clip_rejects_one_outlier_among_five_samples() {
        let (acc, i) = tracked_scalar_samples(&[
            (100.0, 1.0),
            (100.0, 1.0),
            (100.0, 1.0),
            (100.0, 1.0),
            (1_000.0, 1.0),
        ]);
        assert!((acc.direct_w[i] - 5.0).abs() < 1e-12);
        assert!((acc.direct_w2[i] - 5.0).abs() < 1e-12);
        let (lo, hi) = build_sigma_clip_bounds(&acc, 3.0, 1.0).unwrap();
        assert!((lo[i]..=hi[i]).contains(&100.0), "la señal limpia debe sobrevivir");
        assert!(1_000.0 > hi[i], "el outlier N=5 debe quedar fuera; hi={}", hi[i]);

        let mut coverage = vec![1.0f32; 25];
        apply_sigma_rejection(&vec![1_000.0; 25], &mut coverage, &lo, &hi);
        assert_eq!(coverage[i], 0.0);
    }

    #[test]
    fn sigma_clip_preserves_a_clean_five_sample_distribution() {
        let values = [98.0f32, 99.0, 100.0, 101.0, 102.0];
        let samples: Vec<(f32, f32)> = values.iter().map(|&v| (v, 1.0)).collect();
        let (acc, i) = tracked_scalar_samples(&samples);
        let (lo, hi) = build_sigma_clip_bounds(&acc, 3.0, 1.0).unwrap();
        for value in values {
            assert!(
                (lo[i]..=hi[i]).contains(&value),
                "muestra limpia {value} fuera de [{},{}]",
                lo[i],
                hi[i]
            );
        }
    }

    #[test]
    fn sigma_clip_opens_bounds_when_effective_sample_size_is_too_low() {
        // Cinco observaciones nominales, pero una domina: N_eff≈1.43. Contar W
        // como si fueran cinco frames produciría una falsa confianza.
        let (acc, i) = tracked_scalar_samples(&[
            (100.0, 1.0),
            (101.0, 0.05),
            (99.0, 0.05),
            (102.0, 0.05),
            (98.0, 0.05),
        ]);
        let n_eff = acc.direct_w[i] * acc.direct_w[i] / acc.direct_w2[i];
        assert!(n_eff < MIN_SIGMA_CLIP_N_EFF);
        let (lo, hi) = build_sigma_clip_bounds(&acc, 3.0, 1.0).unwrap();
        assert_eq!(lo[i], f32::MIN);
        assert_eq!(hi[i], f32::MAX);
    }

    #[test]
    fn sigma_clip_effective_k_converges_to_requested_k_at_high_n() {
        let k = 3.0;
        let k_eff_5 = sigma_clip_effective_k(k, 5.0);
        let k_eff_10k = sigma_clip_effective_k(k, 10_000.0);
        assert!(k_eff_5 < k * 0.60, "N=5 debe corregir fuerte: {k_eff_5}");
        assert!(
            k_eff_10k > k * 0.999,
            "N alto debe converger a k: {k_eff_10k}"
        );
    }

    #[test]
    fn test_sigma_clip_rejects_transient_artifact() {
        // Realistic lucky-imaging regime: a satellite/bird streak crosses 2 of
        // 60 frames. Pass 1 gathers per-pixel statistics, pass 2 rejects the
        // outliers — the streak must vanish while clean pixels stay identical.
        let w = 64usize;
        let h = 64usize;
        let n = w * h;
        let frames_total = 60usize;
        let streak_frames = [11usize, 37usize];
        let streak_row = 32usize;
        let streak_cols = 10usize..54;

        let base: Vec<f32> = (0..n)
            .map(|i| {
                let x = i % w;
                let y = i / w;
                5000.0 + ((x * 13 + y * 7) % 800) as f32
            })
            .collect();
        // Deterministic pseudo-noise in ±100 (σ ≈ 58).
        let noise = |f: usize, i: usize| -> f32 {
            let h32 = ((f * 31 + i) as u32).wrapping_mul(2654435761);
            ((h32 >> 16) % 201) as f32 - 100.0
        };
        let make_frame = |f: usize| -> Vec<f32> {
            let mut v: Vec<f32> = (0..n).map(|i| base[i] + noise(f, i)).collect();
            if streak_frames.contains(&f) {
                for x in streak_cols.clone() {
                    v[streak_row * w + x] = 60000.0; // satellite
                }
            }
            v
        };
        let cov = vec![1.0f32; n];

        // PASS 1: tracked accumulation (mean + second moment).
        let mut acc1 = GradientDomainStacker::new_direct_tracked(w, h);
        for f in 0..frames_total {
            acc1.accumulate(&make_frame(f), &cov, &[], 0, 0, 0.0, 0.0, 1.0);
        }
        let (lo, hi) = build_sigma_clip_bounds(&acc1, 4.0, 6.0).unwrap();

        // PASS 2: rejection against the pass-1 window.
        let mut acc2 = GradientDomainStacker::new_direct_only(w, h);
        for f in 0..frames_total {
            let vals = make_frame(f);
            let mut c = cov.clone();
            apply_sigma_rejection(&vals, &mut c, &lo, &hi);
            acc2.accumulate(&vals, &c, &[], 0, 0, 0.0, 0.0, 1.0);
        }

        let mean_of = |acc: &GradientDomainStacker, i: usize| -> f32 {
            (acc.direct[i] / acc.direct_w[i].max(1e-9)) as f32
        };

        // Streak pixels (interior): pass 1 is visibly contaminated, pass 2 clean.
        for x in [20usize, 32, 45] {
            let i = streak_row * w + x;
            let m1 = mean_of(&acc1, i);
            let m2 = mean_of(&acc2, i);
            assert!(
                m1 - base[i] > 1000.0,
                "pass 1 should be contaminated at streak pixel (got +{})",
                m1 - base[i]
            );
            assert!(
                (m2 - base[i]).abs() < 300.0,
                "pass 2 must reject the streak (residual {})",
                m2 - base[i]
            );
        }

        // Clean pixels: rejection must not distort normal signal.
        for &(x, y) in &[(20usize, 20usize), (45, 12), (8, 50)] {
            let i = y * w + x;
            let d = (mean_of(&acc2, i) - mean_of(&acc1, i)).abs();
            assert!(d < 60.0, "clean pixel distorted by {}", d);
        }
    }

    #[test]
    fn test_low_coverage_crop_trims_dead_border() {
        let w = 200;
        let h = 200;
        let mut weights = vec![1.0f64; w * h];
        // Kill the 8 left-most columns (zero coverage)
        for y in 0..h {
            for x in 0..8 {
                weights[y * w + x] = 0.0;
            }
        }
        let crop = compute_low_coverage_crop(&weights, w, h, 0.06);
        assert_eq!(crop, Some((8, 0, w, h)));
    }

    #[test]
    fn test_low_coverage_crop_no_crop_when_uniform() {
        let w = 128;
        let h = 128;
        let weights = vec![2.5f64; w * h];
        assert_eq!(compute_low_coverage_crop(&weights, w, h, 0.06), None);
    }

    #[test]
    fn test_mono_exposure_normalization_gain_clamped() {
        let mut data = vec![1000u16; 4096];
        // Target far above current p90: gain must clamp at 1.45
        normalize_surface_frame_exposure_mono_inplace(&mut data, 10_000.0);
        assert_eq!(data[0], 1450);
        // No-op when target matches
        let mut data2 = vec![1000u16; 4096];
        normalize_surface_frame_exposure_mono_inplace(&mut data2, 1000.0);
        assert_eq!(data2[0], 1000);
    }

    #[test]
    fn test_downscale_f32_box_dims_and_mean() {
        let src = vec![4.0f32; 16 * 16];
        let (out, nw, nh) = downscale_f32_box(&src, 16, 16, 4);
        assert_eq!((nw, nh), (4, 4));
        assert!(out.iter().all(|&v| (v - 4.0).abs() < 1e-6));
    }

    #[test]
    fn test_gradient_stacker_direct_only_mean() {
        let w = 8;
        let h = 8;
        let mut s = GradientDomainStacker::new_direct_only(w, h);
        let frame_a = vec![100.0f32; w * h];
        let frame_b = vec![300.0f32; w * h];
        s.accumulate(&frame_a, &[], &[], 0, 0, 0.0, 0.0, 1.0);
        s.accumulate(&frame_b, &[], &[], 0, 0, 0.0, 0.0, 1.0);
        let i = 3 * w + 3; // interior pixel
        let mean = s.direct[i] / s.direct_w[i];
        assert!((mean - 200.0).abs() < 1e-6);
        // merge with empty must not panic and must not alter values
        s.merge(&GradientDomainStacker::new_empty());
        assert!((s.direct[i] / s.direct_w[i] - 200.0).abs() < 1e-6);
    }

    #[test]
    fn test_sky_mask_keeps_wide_dark_filament_as_signal() {
        // Synthetic Hα scene: sky strip on top (border-connected), bright disk,
        // and a WIDE dark filament in the disk interior. The mask must mark
        // the sky as background (0) but the filament as signal (1) — a plain
        // brightness threshold or morphological closing fails this.
        let w = 128usize;
        let h = 128usize;
        let mut luma = vec![30000.0f32; w * h];
        for y in 0..30 {
            for x in 0..w {
                luma[y * w + x] = 500.0; // sky
            }
        }
        for y in 50..86 {
            for x in 20..108 {
                luma[y * w + x] = 300.0; // wide dark filament (interior)
            }
        }
        let mask = compute_border_connected_sky_mask(&luma, w, h, 10);
        assert!(mask[4 * w + 64] < 0.5, "sky must be background");
        assert!(mask[67 * w + 64] > 0.5, "wide dark filament must stay signal");
        assert!(mask[110 * w + 64] > 0.5, "bright disk must stay signal");
    }

    // =================================================================
    // END-TO-END GROUND-TRUTH HARNESS: synthetic surface + known warps.
    // Validates the COMPLETE alignment+warp+accumulation core numerically:
    // every AP shift must be recovered with sub-pixel accuracy and the
    // stacked output must reconstruct the ground truth.
    // =================================================================
    fn lcg_next(state: &mut u64) -> f32 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((*state >> 33) & 0x7FFF_FFFF) as f32 / 2147483647.0 // [0,1)
    }

    /// Synthetic solar-like texture: smooth cells + fibril-scale waves.
    fn truth_at(x: f32, y: f32) -> f32 {
        18000.0
            + 5200.0 * (x * 0.043).sin() * (y * 0.037).cos()
            + 3100.0 * (x * 0.011 + y * 0.017).sin()
            + 1600.0 * (x * 0.31).sin() * (y * 0.27).sin()
            + 900.0 * (x * 0.71 + 1.3).sin() * (y * 0.63).cos()
    }

    #[test]
    fn test_end_to_end_synthetic_stack_recovers_ground_truth() {
        let w = 320usize;
        let h = 320usize;
        let n_frames = 8usize;
        let ap_size = 32usize;

        let mut master = vec![0u16; w * h];
        for y in 0..h {
            for x in 0..w {
                master[y * w + x] = truth_at(x as f32, y as f32) as u16;
            }
        }

        // Overlapping AP grid like production (step ≈ 0.67·ap_size)
        let mut points = Vec::new();
        let mut yy = 24usize;
        while yy < h - 24 {
            let mut xx = 24usize;
            while xx < w - 24 {
                points.push(ApPoint { x: xx as f32, y: yy as f32, size: ap_size });
                xx += 21;
            }
            yy += 21;
        }
        let n_aps = points.len();
        let all_valid = vec![true; n_aps];
        let no_dark = vec![0.0f32; n_aps];
        let no_normals: Vec<Option<(f32, f32)>> = vec![None; n_aps];

        // EXPERIMENT TOGGLE: match on raw intensity vs enhanced maps
        let use_enhance = std::env::var("ZAS_E2E_RAW").is_err();
        let master_edges = if use_enhance {
            enhance_for_alignment_with_amount(&master, w, h, 4.0)
        } else {
            master.clone()
        };
        let mut master_ds = Vec::new();
        let (mds_w, _mds_h) = downscale_4x(&master_edges, w, h, &mut master_ds);

        let (warp_idx, warp_wts) =
            compute_idw_map_for_output(w, h, 1.0, 0.0, 0.0, &points, 1.55, 4).unwrap();
        let accept = vec![true; n_aps];
        let ap_wts = vec![1.0f32; n_aps];

        let mut acc = vec![0.0f32; w * h];
        let mut acc_n = 0.0f32;
        let mut unaligned = vec![0.0f64; w * h];
        let mut shift_errs: Vec<f32> = Vec::new();
        let mut rng: u64 = 0xDEAD_BEEF;

        for _f in 0..n_frames {
            let gx = (lcg_next(&mut rng) * 2.0 - 1.0) * 5.0;
            let gy = (lcg_next(&mut rng) * 2.0 - 1.0) * 5.0;
            let p1 = lcg_next(&mut rng) * 3.14;
            let p2 = lcg_next(&mut rng) * 3.14;
            let p3 = lcg_next(&mut rng) * 3.14;
            let p4 = lcg_next(&mut rng) * 3.14;
            let sx = |x: f32, y: f32| {
                1.8 * ((y / 150.0 * 6.283 + p1).sin() * (x / 170.0 * 6.283 + p2).cos())
            };
            let sy = |x: f32, y: f32| {
                1.8 * ((x / 160.0 * 6.283 + p3).sin() * (y / 145.0 * 6.283 + p4).cos())
            };

            // Convention "Source = Master + Shift": frame(q) = truth(q − g − s(q))
            let mut frame = vec![0u16; w * h];
            for y in 0..h {
                for x in 0..w {
                    let xf = x as f32;
                    let yf = y as f32;
                    let v = truth_at(xf - gx - sx(xf, yf), yf - gy - sy(xf, yf))
                        + (lcg_next(&mut rng) * 2.0 - 1.0) * 120.0;
                    frame[y * w + x] = v.clamp(0.0, 65535.0) as u16;
                    unaligned[y * w + x] += frame[y * w + x] as f64;
                }
            }

            let f_edges = if use_enhance {
                enhance_for_alignment_with_amount(&frame, w, h, 4.0)
            } else {
                frame.clone()
            };
            let mut f_ds = Vec::new();
            downscale_4x(&f_edges, w, h, &mut f_ds);

            let (ap_mc, ap_ml) =
                precompute_ap_master_stats(&master_edges, &master, w, h, &points, ap_size);
            let shifts = compute_frame_local_shifts(
                &master_edges, &ap_mc, &ap_ml, &master_ds, mds_w,
                &f_edges, &frame, &f_ds, w, h,
                &points, &all_valid, &no_dark, &no_normals,
                gx, gy, ap_size, 12, true, true,
                0.0, // gate de textura apagado: el harness mide TODOS los APs
                false,
                None,
            );

            // Milimetric per-AP accuracy vs the injected warp field
            for (i, ap) in points.iter().enumerate() {
                let ts_x = sx(ap.x + gx, ap.y + gy);
                let ts_y = sy(ap.x + gx, ap.y + gy);
                let e = ((shifts[i].0 - ts_x).powi(2) + (shifts[i].1 - ts_y).powi(2)).sqrt();
                shift_errs.push(e);
            }

            let mut wg = vec![0.0f32; w * h];
            let mut ww = vec![0.0f32; w * h];
            accumulate_frame_liquid_mono(
                &mut wg, &mut ww, &frame, w, h, w, h, 1.0, 0.0, 0.0,
                gx, gy, &shifts, &warp_idx, &warp_wts, &accept, &ap_wts, 1.0, 1.0,
            );
            for i in 0..w * h {
                if ww[i] > 1e-9 {
                    acc[i] += wg[i] / ww[i];
                }
            }
            acc_n += 1.0;
        }

        shift_errs.sort_by(f32::total_cmp);
        let median_err = shift_errs[shift_errs.len() / 2];
        let p90_err = shift_errs[shift_errs.len() * 9 / 10];

        let margin = 28usize;
        let mut se_stack = 0.0f64;
        let mut se_unaligned = 0.0f64;
        let mut count = 0.0f64;
        for y in margin..h - margin {
            for x in margin..w - margin {
                let i = y * w + x;
                let t = truth_at(x as f32, y as f32) as f64;
                let s = (acc[i] / acc_n) as f64;
                let u = unaligned[i] / acc_n as f64;
                se_stack += (s - t) * (s - t);
                se_unaligned += (u - t) * (u - t);
                count += 1.0;
            }
        }
        let rmse_stack = (se_stack / count).sqrt();
        let rmse_unaligned = (se_unaligned / count).sqrt();
        println!(
            "E2E: median_shift_err={median_err:.3}px p90={p90_err:.3}px rmse_stack={rmse_stack:.1} rmse_unaligned={rmse_unaligned:.1}"
        );

        assert!(
            median_err < 0.35,
            "median AP shift error too high: {median_err:.3}px (p90 {p90_err:.3}px)"
        );
        assert!(
            rmse_stack < rmse_unaligned * 0.35,
            "stack RMSE {rmse_stack:.1} vs unaligned {rmse_unaligned:.1}: alignment not converging"
        );
    }

    #[test]
    fn test_limb_normal_projection_kills_tangential() {
        // Edge normal pointing up (0, -1): a measured shift of (3.0 tangential,
        // 1.5 radial) must keep only the radial component.
        let (nx, ny) = (0.0f32, -1.0f32);
        let (dx, dy) = (3.0f32, -1.5f32);
        let r = dx * nx + dy * ny;
        let (pdx, pdy) = (r * nx, r * ny);
        assert!((pdx - 0.0).abs() < 1e-6);
        assert!((pdy - (-1.5)).abs() < 1e-6);
    }
}
