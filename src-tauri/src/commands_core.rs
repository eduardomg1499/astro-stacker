// ==========================================
// 6. COMANDOS TAURI (EXPORTADOS)
// ==========================================

#[tauri::command]
fn check_ffmpeg_status(app: tauri::AppHandle) -> bool {
    let cmd_path = get_ffmpeg_command(&app);
    let output = {
        let mut cmd = Command::new(&cmd_path);
        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x08000000);
        cmd.arg("-version").output()
    };
    match output {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

#[tauri::command]
fn cancel_processing(app: tauri::AppHandle, state: State<'_, AppState>) {
    log_to_front(&app, "WARNING", "CancelaciÃ³n solicitada por el usuario.");
    // Generación monotónica + flag cooperativo: ningún trabajo anterior puede
    // revivir cuando una operación posterior rearma la cancelación.
    cancel_planetary_jobs(&state);
    // 3. F3: liberar las cachés DERIVADAS pesadas. Antes quedaban retenidas
    //    hasta que la SIGUIENTE operación llamara clear_app_memory: cancelar
    //    y no continuar dejaba cientos de MB anclados. Son recomputables
    //    (deconv/wavelets/filtros se regeneran del máster al primer render).
    // La invalidación y estos clears ya ocurren juntos dentro de
    // `cancel_planetary_jobs`, bajo el gate generacional.
}

/// Espacio libre del volumen que contiene `path` — la UI lo muestra junto a
/// la carpeta de trabajo para que el usuario vea si su disco externo aguanta
/// los cachés de calibración (varios GB).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DiskSpaceInfo {
    available_mb: u64,
    total_mb: u64,
    mount: String,
}

#[tauri::command]
fn disk_space_info(path: String) -> Result<DiskSpaceInfo, String> {
    let target = std::fs::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let best = disks
        .list()
        .iter()
        .filter(|d| target.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .ok_or("No se encontró el volumen de esa ruta")?;
    Ok(DiskSpaceInfo {
        available_mb: best.available_space() / 1_048_576,
        total_mb: best.total_space() / 1_048_576,
        mount: best.mount_point().display().to_string(),
    })
}

#[tauri::command]
fn get_available_fonts() -> Vec<String> {
    let map = get_font_map();
    map.iter().map(|(name, _)| name.clone()).collect()
}

const PLANETARY_BATCH_OUTPUT_MARKER: &str = ".zenith-planetary-batch-output";
const PLANETARY_BATCH_MANIFEST: &str = "Zenith_Batch_manifest.json";
static PLANETARY_BATCH_MANIFEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
static PLANETARY_BATCH_MANIFEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchOutputError {
    code: String,
    path: Option<String>,
    message: String,
}

impl BatchOutputError {
    fn new(code: &str, path: Option<&Path>, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            path: path.map(|value| clean_windows_path(value.to_path_buf())),
            message: message.into(),
        }
    }

    fn io(operation: &str, path: &Path, error: std::io::Error) -> Self {
        let code = match (error.kind(), error.raw_os_error()) {
            (std::io::ErrorKind::PermissionDenied, _) => "permission_denied",
            (_, Some(28 | 112)) => "no_space",
            // ERROR_WRITE_PROTECT=19 en Windows; EROFS=30 en Unix.
            (_, Some(19 | 30)) => "read_only_volume",
            (std::io::ErrorKind::NotFound, _) => "not_found",
            (std::io::ErrorKind::AlreadyExists, _) => "already_exists",
            _ => "io_error",
        };
        Self::new(
            code,
            Some(path),
            format!("{operation} en {}: {error}", path.display()),
        )
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct NormalizedBatchApPoint {
    x: f32,
    y: f32,
    size: f32,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchOutputEntry {
    source_path: String,
    output_folder: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchOutputPlan {
    schema_version: u32,
    session_id: String,
    policy: String,
    animation_folder: String,
    entries: Vec<BatchOutputEntry>,
    sequence_plan: PlanetarySequencePlan,
    normalized_ap_points: Vec<NormalizedBatchApPoint>,
}

#[derive(Debug, Clone)]
struct PreparedBatchSource {
    source_path: String,
    parent: PathBuf,
    relative_parent: PathBuf,
    source_name: PathBuf,
}

fn is_planetary_batch_output_dir(path: &Path) -> bool {
    path.join(PLANETARY_BATCH_OUTPUT_MARKER).is_file()
}

fn scan_video_directory(root: &Path, recursive: bool) -> std::io::Result<Vec<String>> {
    fn visit_dirs(dir: &Path, files: &mut Vec<String>, recursive: bool) -> std::io::Result<()> {
        if !dir.is_dir() || is_planetary_batch_output_dir(dir) {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if recursive && !is_planetary_batch_output_dir(&path) {
                    visit_dirs(&path, files, recursive)?;
                }
            } else if let Some(ext) = path.extension() {
                let ext_str = ext.to_string_lossy().to_lowercase();
                if matches!(ext_str.as_str(), "ser" | "avi" | "mp4" | "mov" | "mkv") {
                    files.push(clean_windows_path(path));
                }
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    visit_dirs(root, &mut files, recursive)?;
    files.sort();
    Ok(files)
}

fn canonical_batch_directory(value: &str, label: &str) -> Result<PathBuf, BatchOutputError> {
    let path = PathBuf::from(value);
    if value.trim().is_empty() || !path.is_absolute() {
        return Err(BatchOutputError::new(
            "invalid_path",
            Some(&path),
            format!("{label} debe ser una ruta absoluta."),
        ));
    }
    let canonical = dunce::canonicalize(&path)
        .map_err(|error| BatchOutputError::io(&format!("Abrir {label}"), &path, error))?;
    if !canonical.is_dir() {
        return Err(BatchOutputError::new(
            "not_directory",
            Some(&canonical),
            format!("{label} no es una carpeta."),
        ));
    }
    Ok(canonical)
}

fn next_planetary_batch_session_id() -> String {
    let mut uuid: [u8; 16] = rand::random();
    // UUID v4 / RFC 4122. El nombre no revela rutas y mantiene la misma sesión
    // identificable en todos los volúmenes que contienen fuentes del lote.
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    format!(
        "Zenith_Batch_{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        uuid[0], uuid[1], uuid[2], uuid[3],
        uuid[4], uuid[5], uuid[6], uuid[7],
        uuid[8], uuid[9], uuid[10], uuid[11],
        uuid[12], uuid[13], uuid[14], uuid[15],
    )
}

fn normalize_batch_ap_points(
    points: &[ApPoint],
    canvas: [u32; 2],
) -> Result<Vec<NormalizedBatchApPoint>, BatchOutputError> {
    let width = canvas[0] as f32;
    let height = canvas[1] as f32;
    let scale = width.min(height);
    if width <= 1.0 || height <= 1.0 || scale <= 1.0 {
        return Err(BatchOutputError::new(
            "invalid_reference_canvas",
            None,
            "El canvas de referencia del lote no tiene dimensiones válidas.",
        ));
    }
    if points.is_empty() {
        return Err(BatchOutputError::new(
            "missing_reference_ap",
            None,
            "La referencia del lote no contiene puntos AP para congelar.",
        ));
    }
    let mut normalized = Vec::with_capacity(points.len());
    for point in points {
        if !point.x.is_finite() || !point.y.is_finite() || point.size == 0 {
            return Err(BatchOutputError::new(
                "invalid_reference_ap",
                None,
                "La referencia contiene un punto AP inválido.",
            ));
        }
        normalized.push(NormalizedBatchApPoint {
            x: (point.x / (width - 1.0)).clamp(0.0, 1.0),
            y: (point.y / (height - 1.0)).clamp(0.0, 1.0),
            size: (point.size as f32 / scale).clamp(8.0 / scale, 1.0),
        });
    }
    Ok(normalized)
}

fn materialize_batch_ap_points(
    points: &[NormalizedBatchApPoint],
    width: usize,
    height: usize,
) -> Vec<ApPoint> {
    if width <= 1 || height <= 1 {
        return Vec::new();
    }
    let scale = width.min(height) as f32;
    points
        .iter()
        .map(|point| ApPoint {
            x: (point.x.clamp(0.0, 1.0) * (width.saturating_sub(1)) as f32)
                .clamp(0.0, width.saturating_sub(1) as f32),
            y: (point.y.clamp(0.0, 1.0) * (height.saturating_sub(1)) as f32)
                .clamp(0.0, height.saturating_sub(1) as f32),
            size: (point.size.clamp(0.0, 1.0) * scale)
                .round()
                .clamp(8.0, scale.max(8.0)) as usize,
        })
        .collect()
}

fn batch_rgb16_to_mono(rgb: &[u16]) -> Vec<u16> {
    rgb.chunks_exact(3)
        .map(|pixel| {
            ((pixel[0] as u32 + pixel[1] as u32 + pixel[2] as u32) / 3) as u16
        })
        .collect()
}

fn batch_planet_disc_is_reliable(
    disc: &crate::derotation::PlanetDisc,
    mono: &[u16],
    width: usize,
    height: usize,
) -> bool {
    if mono.len() != width.saturating_mul(height) || width < 16 || height < 16 {
        return false;
    }
    let min_dim = width.min(height) as f64;
    if !disc.cx.is_finite()
        || !disc.cy.is_finite()
        || !disc.radius_x.is_finite()
        || !disc.radius_y.is_finite()
        || disc.radius_x < min_dim * 0.02
        || disc.radius_y < min_dim * 0.02
        || disc.radius_x > width as f64 * 0.49
        || disc.radius_y > height as f64 * 0.49
        || disc.cx - disc.radius_x < 0.0
        || disc.cy - disc.radius_y < 0.0
        || disc.cx + disc.radius_x >= width as f64
        || disc.cy + disc.radius_y >= height as f64
    {
        return false;
    }

    let sample_step = ((width.saturating_mul(height) / 120_000).max(1) as f64)
        .sqrt()
        .ceil() as usize;
    let mut inside_sum = 0u64;
    let mut outside_sum = 0u64;
    let mut inside_count = 0usize;
    let mut outside_count = 0usize;
    for y in (0..height).step_by(sample_step.max(1)) {
        let dy = (y as f64 - disc.cy) / disc.radius_y.max(1.0);
        for x in (0..width).step_by(sample_step.max(1)) {
            let dx = (x as f64 - disc.cx) / disc.radius_x.max(1.0);
            let radius2 = dx * dx + dy * dy;
            let value = mono[y * width + x] as u64;
            if radius2 <= 0.64 {
                inside_sum = inside_sum.saturating_add(value);
                inside_count += 1;
            } else if (1.15..=1.80).contains(&radius2) {
                outside_sum = outside_sum.saturating_add(value);
                outside_count += 1;
            }
        }
    }
    if inside_count < 16 || outside_count < 16 {
        return false;
    }
    let inside = inside_sum as f64 / inside_count as f64;
    let outside = outside_sum as f64 / outside_count as f64;
    inside >= outside * 1.08 + 32.0
}

fn batch_warp_disc_to_reference(
    rgb: &[u16],
    width: usize,
    height: usize,
    current: &crate::derotation::PlanetDisc,
    reference: &crate::derotation::PlanetDisc,
    max_scale_delta: f32,
    max_roll_degrees: f32,
) -> Vec<u16> {
    let current_radius = (current.radius_x * current.radius_y).sqrt().max(1.0);
    let reference_radius = (reference.radius_x * reference.radius_y).sqrt().max(1.0);
    let scale_limit = max_scale_delta.clamp(0.0, 0.25) as f64;
    let scale = (reference_radius / current_radius).clamp(1.0 - scale_limit, 1.0 + scale_limit);
    let roll = (reference.angle_deg - current.angle_deg)
        .clamp(-(max_roll_degrees as f64), max_roll_degrees as f64)
        .to_radians();
    let cos_roll = roll.cos();
    let sin_roll = roll.sin();
    let mut output = vec![0u16; width.saturating_mul(height).saturating_mul(3)];
    for y in 0..height {
        for x in 0..width {
            let target_x = (x as f64 - reference.cx) / scale;
            let target_y = (y as f64 - reference.cy) / scale;
            let source_x = current.cx + target_x * cos_roll + target_y * sin_roll;
            let source_y = current.cy - target_x * sin_roll + target_y * cos_roll;
            if source_x < 0.0
                || source_y < 0.0
                || source_x >= width.saturating_sub(1) as f64
                || source_y >= height.saturating_sub(1) as f64
            {
                continue;
            }
            let x0 = source_x.floor() as usize;
            let y0 = source_y.floor() as usize;
            let wx = (source_x - x0 as f64) as f32;
            let wy = (source_y - y0 as f64) as f32;
            let dst = (y * width + x) * 3;
            for channel in 0..3 {
                let v00 = rgb[(y0 * width + x0) * 3 + channel] as f32;
                let v10 = rgb[(y0 * width + x0 + 1) * 3 + channel] as f32;
                let v01 = rgb[((y0 + 1) * width + x0) * 3 + channel] as f32;
                let v11 = rgb[((y0 + 1) * width + x0 + 1) * 3 + channel] as f32;
                output[dst + channel] = ((v00 * (1.0 - wx) + v10 * wx) * (1.0 - wy)
                    + (v01 * (1.0 - wx) + v11 * wx) * wy)
                    .round()
                    .clamp(0.0, 65_535.0) as u16;
            }
        }
    }
    output
}

fn batch_common_rgb_luminance_scalar(reference: &[u16], current: &[u16]) -> f32 {
    if reference.len() != current.len() || reference.len() < 3 {
        return 1.0;
    }
    let pixel_count = reference.len() / 3;
    let step = (pixel_count / 200_000).max(1);
    let mut pairs = Vec::with_capacity((pixel_count / step).max(1));
    let mut reference_peak = 0u16;
    let mut current_peak = 0u16;
    for pixel in (0..pixel_count).step_by(step) {
        let index = pixel * 3;
        let reference_luma = ((reference[index] as u32
            + reference[index + 1] as u32
            + reference[index + 2] as u32)
            / 3) as u16;
        let current_luma = ((current[index] as u32
            + current[index + 1] as u32
            + current[index + 2] as u32)
            / 3) as u16;
        reference_peak = reference_peak.max(reference_luma);
        current_peak = current_peak.max(current_luma);
        pairs.push((reference_luma, current_luma));
    }
    let reference_floor = (reference_peak as f32 * 0.08).max(32.0) as u16;
    let current_floor = (current_peak as f32 * 0.08).max(32.0) as u16;
    let reference_ceiling = (reference_peak as f32 * 0.985).min(65_000.0) as u16;
    let current_ceiling = (current_peak as f32 * 0.985).min(65_000.0) as u16;
    let mut reference_samples = Vec::new();
    let mut current_samples = Vec::new();
    for (reference_luma, current_luma) in pairs {
        if reference_luma >= reference_floor
            && current_luma >= current_floor
            && reference_luma <= reference_ceiling
            && current_luma <= current_ceiling
        {
            reference_samples.push(reference_luma);
            current_samples.push(current_luma);
        }
    }
    if reference_samples.len() < 32 {
        return 1.0;
    }
    reference_samples.sort_unstable();
    current_samples.sort_unstable();
    let middle = reference_samples.len() / 2;
    let reference_median = reference_samples[middle] as f32;
    let current_median = current_samples[middle].max(1) as f32;
    (reference_median / current_median).clamp(0.5, 2.0)
}

fn batch_apply_common_rgb_scalar(rgb: &mut [u16], scalar: f32) {
    if !scalar.is_finite() || (scalar - 1.0).abs() <= f32::EPSILON {
        return;
    }
    rgb.iter_mut().for_each(|value| {
        *value = (*value as f32 * scalar).round().clamp(0.0, 65_535.0) as u16;
    });
}

fn rollback_batch_output_directories(created: &[PathBuf]) {
    for directory in created.iter().rev() {
        // Cada ruta fue creada con `create_dir` por este intento y el nombre de
        // sesión es único. Nunca se elimina una carpeta elegida por el usuario.
        let _ = std::fs::remove_dir_all(directory);
    }
}

#[cfg(target_os = "windows")]
fn atomic_replace_batch_file(temporary: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;
    let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: ambas cadenas están terminadas en NUL y viven durante la llamada.
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
fn atomic_replace_batch_file(temporary: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(temporary, target)
}

fn probe_batch_output_directory_impl<F>(
    directory: &Path,
    session_id: &str,
    after_rename: F,
) -> std::io::Result<()>
where
    F: FnOnce() -> std::io::Result<()>,
{
    let temporary = directory.join(format!(".zenith-write-probe-{session_id}.tmp"));
    let committed = directory.join(format!(".zenith-write-probe-{session_id}.committed"));
    let result = (|| -> std::io::Result<()> {
        let mut probe = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        probe.write_all(b"ok")?;
        probe.flush()?;
        probe.sync_all()?;
        drop(probe);
        std::fs::rename(&temporary, &committed)?;
        after_rename()?;
        std::fs::remove_file(&committed)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
        let _ = std::fs::remove_file(&committed);
    }
    result
}

fn probe_batch_output_directory(directory: &Path, session_id: &str) -> std::io::Result<()> {
    probe_batch_output_directory_impl(directory, session_id, || Ok(()))
}

fn write_batch_manifest_value(
    directory: &Path,
    value: &serde_json::Value,
) -> Result<(), BatchOutputError> {
    let target = directory.join(PLANETARY_BATCH_MANIFEST);
    let sequence = PLANETARY_BATCH_MANIFEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(
        ".Zenith_Batch_manifest.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        BatchOutputError::new(
            "manifest_encode_failed",
            Some(&target),
            format!("No se pudo codificar el manifiesto del lote: {error}"),
        )
    })?;
    let write_result = (|| -> std::io::Result<()> {
        let mut staged = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        staged.write_all(&bytes)?;
        staged.flush()?;
        staged.sync_all()?;
        drop(staged);
        atomic_replace_batch_file(&temporary, &target)
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(BatchOutputError::io(
            "Publicar manifiesto del lote",
            &target,
            error,
        ));
    }
    Ok(())
}

fn initialize_batch_output_directory(
    directory: &Path,
    session_id: &str,
    sequence_plan: &PlanetarySequencePlan,
    normalized_ap_points: &[NormalizedBatchApPoint],
) -> Result<(), BatchOutputError> {
    let marker_path = directory.join(PLANETARY_BATCH_OUTPUT_MARKER);
    let marker_body = serde_json::to_vec_pretty(&serde_json::json!({
        "schemaVersion": 1,
        "kind": "planetaryBatchOutput",
        "sessionId": session_id,
        "sequencePlan": sequence_plan,
        "normalizedApPoints": normalized_ap_points,
    }))
    .map_err(|error| {
        BatchOutputError::new(
            "manifest_encode_failed",
            Some(&marker_path),
            format!("No se pudo codificar el manifiesto del lote: {error}"),
        )
    })?;
    let mut marker = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker_path)
        .map_err(|error| BatchOutputError::io("Crear marcador de salida", &marker_path, error))?;
    marker
        .write_all(&marker_body)
        .and_then(|_| marker.flush())
        .and_then(|_| marker.sync_all())
        .map_err(|error| BatchOutputError::io("Escribir marcador de salida", &marker_path, error))?;

    if let Err(error) = probe_batch_output_directory(directory, session_id) {
        return Err(BatchOutputError::io(
            "Comprobar escritura del lote",
            directory,
            error,
        ));
    }
    Ok(())
}

fn prepare_batch_output_impl(
    files: Vec<String>,
    source_root: String,
    policy: String,
    single_directory: Option<String>,
    reference_canvas: [u32; 2],
    reference_ap_points: Vec<ApPoint>,
) -> Result<BatchOutputPlan, BatchOutputError> {
    if files.is_empty() {
        return Err(BatchOutputError::new(
            "empty_batch",
            None,
            "El lote no contiene vídeos.",
        ));
    }
    if policy != "sourceAdjacent" && policy != "singleDirectory" {
        return Err(BatchOutputError::new(
            "invalid_policy",
            None,
            "La política de salida debe ser sourceAdjacent o singleDirectory.",
        ));
    }

    let normalized_ap_points = normalize_batch_ap_points(&reference_ap_points, reference_canvas)?;
    let reference_source = files[0].clone();
    let source_root = canonical_batch_directory(&source_root, "la carpeta fuente del lote")?;
    let single_base = if policy == "singleDirectory" {
        let selected = single_directory.as_deref().ok_or_else(|| {
            BatchOutputError::new(
                "missing_destination",
                None,
                "Elige una carpeta única para la salida del lote.",
            )
        })?;
        Some(canonical_batch_directory(selected, "la carpeta de salida elegida")?)
    } else {
        None
    };

    let mut sources = Vec::with_capacity(files.len());
    let mut seen_sources = std::collections::HashSet::with_capacity(files.len());
    for original in files {
        let path = PathBuf::from(&original);
        if !path.is_absolute() {
            return Err(BatchOutputError::new(
                "invalid_source_path",
                Some(&path),
                "La ruta de un vídeo del lote no es absoluta.",
            ));
        }
        let canonical = dunce::canonicalize(&path)
            .map_err(|error| BatchOutputError::io("Abrir vídeo del lote", &path, error))?;
        if !canonical.is_file() {
            return Err(BatchOutputError::new(
                "source_not_file",
                Some(&canonical),
                "Una fuente del lote no es un archivo.",
            ));
        }
        if !seen_sources.insert(canonical.clone()) {
            return Err(BatchOutputError::new(
                "duplicate_source",
                Some(&canonical),
                "El lote contiene el mismo vídeo más de una vez.",
            ));
        }
        let parent = canonical.parent().ok_or_else(|| {
            BatchOutputError::new(
                "source_without_parent",
                Some(&canonical),
                "No se pudo determinar la carpeta del vídeo.",
            )
        })?;
        let relative_parent = if policy == "singleDirectory" {
            parent
                .strip_prefix(&source_root)
                .map(Path::to_path_buf)
                .map_err(|_| {
                    BatchOutputError::new(
                        "source_outside_root",
                        Some(&canonical),
                        "Una fuente no pertenece a la carpeta raíz del lote.",
                    )
                })?
        } else {
            PathBuf::new()
        };
        let source_name = canonical
            .file_name()
            .map(PathBuf::from)
            .ok_or_else(|| {
                BatchOutputError::new(
                    "source_without_name",
                    Some(&canonical),
                    "No se pudo determinar el nombre del vídeo.",
                )
            })?;
        sources.push(PreparedBatchSource {
            source_path: original,
            parent: parent.to_path_buf(),
            relative_parent,
            source_name,
        });
    }

    let animation_base = single_base.clone().unwrap_or_else(|| source_root.clone());
    for _ in 0..32 {
        let session_id = next_planetary_batch_session_id();
        let sequence_plan = PlanetarySequencePlan {
            plan_id: session_id.clone(),
            reference_source: reference_source.clone(),
            fixed_canvas: reference_canvas,
            frozen: true,
            ..PlanetarySequencePlan::default()
        };
        sequence_plan.validate().map_err(|message| {
            BatchOutputError::new("invalid_sequence_plan", None, message)
        })?;
        let mut bases = std::collections::BTreeSet::new();
        bases.insert(animation_base.clone());
        if policy == "sourceAdjacent" {
            for source in &sources {
                bases.insert(source.parent.clone());
            }
        }

        let mut created_roots = Vec::with_capacity(bases.len());
        let mut collision = false;
        for base in &bases {
            let target = base.join(&session_id);
            match std::fs::create_dir(&target) {
                Ok(()) => created_roots.push(target),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    collision = true;
                    break;
                }
                Err(error) => {
                    rollback_batch_output_directories(&created_roots);
                    return Err(BatchOutputError::io(
                        "Crear carpeta de salida del lote",
                        &target,
                        error,
                    ));
                }
            }
        }
        if collision {
            rollback_batch_output_directories(&created_roots);
            continue;
        }

        for directory in &created_roots {
            if let Err(error) = initialize_batch_output_directory(
                directory,
                &session_id,
                &sequence_plan,
                &normalized_ap_points,
            ) {
                rollback_batch_output_directories(&created_roots);
                return Err(error);
            }
        }

        let target_by_base: std::collections::HashMap<PathBuf, PathBuf> = bases
            .into_iter()
            .map(|base| {
                let target = base.join(&session_id);
                (base, target)
            })
            .collect();
        let mut entry_targets = Vec::with_capacity(sources.len());
        let mut unique_targets = std::collections::HashSet::with_capacity(sources.len());
        for source in &sources {
            let session_root = if policy == "singleDirectory" {
                target_by_base
                    .get(single_base.as_ref().expect("singleDirectory validado"))
            } else {
                target_by_base.get(&source.parent)
            }
            .expect("cada base fue materializada");
            let output_folder = if policy == "singleDirectory" {
                session_root
                    .join(&source.relative_parent)
                    .join(&source.source_name)
            } else {
                session_root.join(&source.source_name)
            };
            if !unique_targets.insert(output_folder.clone()) {
                rollback_batch_output_directories(&created_roots);
                return Err(BatchOutputError::new(
                    "output_collision",
                    Some(&output_folder),
                    "Dos fuentes del lote producirían el mismo directorio de salida.",
                ));
            }
            if let Err(error) = std::fs::create_dir_all(&output_folder) {
                rollback_batch_output_directories(&created_roots);
                return Err(BatchOutputError::io(
                    "Crear carpeta individual de la fuente",
                    &output_folder,
                    error,
                ));
            }
            entry_targets.push(BatchOutputEntry {
                source_path: source.source_path.clone(),
                output_folder: clean_windows_path(output_folder),
            });
        }
        let animation_folder = clean_windows_path(
            target_by_base
                .get(&animation_base)
                .cloned()
                .expect("la base de animación fue materializada"),
        );
        let plan = BatchOutputPlan {
            schema_version: 1,
            session_id,
            policy,
            animation_folder,
            entries: entry_targets,
            sequence_plan,
            normalized_ap_points,
        };
        let manifest = serde_json::json!({
            "schemaVersion": plan.schema_version,
            "kind": "planetaryBatchSession",
            "sessionId": plan.session_id,
            "policy": plan.policy,
            "animationFolder": plan.animation_folder,
            "entries": plan.entries,
            "sequencePlan": plan.sequence_plan,
            "normalizedApPoints": plan.normalized_ap_points,
            "completedResults": [],
        });
        if let Err(error) = write_batch_manifest_value(
            Path::new(&plan.animation_folder),
            &manifest,
        ) {
            rollback_batch_output_directories(&created_roots);
            return Err(error);
        }
        return Ok(plan);
    }

    Err(BatchOutputError::new(
        "name_collision",
        Some(&animation_base),
        "No se pudo reservar un nombre único para la sesión del lote.",
    ))
}

#[tauri::command]
fn prepare_batch_output(
    state: State<'_, AppState>,
    files: Vec<String>,
    source_root: String,
    policy: String,
    single_directory: Option<String>,
    reference_canvas: [u32; 2],
    reference_ap_points: Vec<ApPoint>,
) -> Result<BatchOutputPlan, BatchOutputError> {
    state
        .license_manager
        .check_access()
        .map_err(|message| BatchOutputError::new("license", None, message))?;
    prepare_batch_output_impl(
        files,
        source_root,
        policy,
        single_directory,
        reference_canvas,
        reference_ap_points,
    )
}

fn register_batch_output_result_impl(
    animation_folder: &str,
    session_id: &str,
    source_path: &str,
    prepared_path: &str,
    linear_master_path: &str,
) -> Result<(), BatchOutputError> {
    let directory = canonical_batch_directory(animation_folder, "la sesión de salida del lote")?;
    if !directory.join(PLANETARY_BATCH_OUTPUT_MARKER).is_file() {
        return Err(BatchOutputError::new(
            "unmanaged_batch_folder",
            Some(&directory),
            "La carpeta no pertenece a una sesión batch administrada.",
        ));
    }
    let manifest_path = directory.join(PLANETARY_BATCH_MANIFEST);
    let _manifest_guard = PLANETARY_BATCH_MANIFEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let bytes = std::fs::read(&manifest_path)
        .map_err(|error| BatchOutputError::io("Leer manifiesto del lote", &manifest_path, error))?;
    let mut manifest: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        BatchOutputError::new(
            "manifest_decode_failed",
            Some(&manifest_path),
            format!("El manifiesto del lote no es válido: {error}"),
        )
    })?;
    if manifest.get("sessionId").and_then(|value| value.as_str()) != Some(session_id) {
        return Err(BatchOutputError::new(
            "session_mismatch",
            Some(&manifest_path),
            "El resultado no pertenece a la sesión batch activa.",
        ));
    }
    let output_folder = manifest
        .get("entries")
        .and_then(|value| value.as_array())
        .and_then(|entries| {
            entries.iter().find_map(|entry| {
                (entry.get("sourcePath").and_then(|value| value.as_str()) == Some(source_path))
                    .then(|| entry.get("outputFolder").and_then(|value| value.as_str()))
                    .flatten()
            })
        })
        .ok_or_else(|| {
            BatchOutputError::new(
                "source_not_in_session",
                None,
                "La fuente terminada no existe en el plan de la sesión.",
            )
        })?;
    let output_folder = canonical_batch_directory(output_folder, "la salida individual del vídeo")?;
    let validate_result = |value: &str, label: &str| -> Result<PathBuf, BatchOutputError> {
        let path = PathBuf::from(value);
        let canonical = dunce::canonicalize(&path)
            .map_err(|error| BatchOutputError::io(label, &path, error))?;
        if !canonical.is_file() || !canonical.starts_with(&output_folder) {
            return Err(BatchOutputError::new(
                "result_outside_source_folder",
                Some(&canonical),
                format!("{label} no está dentro de la carpeta reservada para la fuente."),
            ));
        }
        Ok(canonical)
    };
    let prepared = validate_result(prepared_path, "La salida preparada")?;
    let linear_master = validate_result(linear_master_path, "El máster lineal RGB16")?;
    let completed = manifest
        .as_object_mut()
        .ok_or_else(|| {
            BatchOutputError::new(
                "manifest_shape_invalid",
                Some(&manifest_path),
                "El manifiesto batch no contiene un objeto raíz.",
            )
        })?
        .entry("completedResults")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let completed = completed.as_array_mut().ok_or_else(|| {
        BatchOutputError::new(
            "manifest_shape_invalid",
            Some(&manifest_path),
            "completedResults no es una lista válida.",
        )
    })?;
    completed.retain(|entry| {
        entry.get("sourcePath").and_then(|value| value.as_str()) != Some(source_path)
    });
    completed.push(serde_json::json!({
        "sourcePath": source_path,
        "preparedPath": clean_windows_path(prepared),
        "linearMasterPath": clean_windows_path(linear_master),
        "completedAt": chrono::Utc::now().to_rfc3339(),
    }));
    write_batch_manifest_value(&directory, &manifest)
}

#[tauri::command]
fn register_batch_output_result(
    state: State<'_, AppState>,
    animation_folder: String,
    session_id: String,
    source_path: String,
    prepared_path: String,
    linear_master_path: String,
) -> Result<(), BatchOutputError> {
    state
        .license_manager
        .check_access()
        .map_err(|message| BatchOutputError::new("license", None, message))?;
    register_batch_output_result_impl(
        &animation_folder,
        &session_id,
        &source_path,
        &prepared_path,
        &linear_master_path,
    )
}

#[tauri::command]
async fn scan_directory(
    _app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    recursive: bool,
) -> Result<Vec<String>, String> {
    state.license_manager.check_access()?;

    let root = Path::new(&path);
    if !root.exists() {
        return Err("La carpeta no existe".into());
    }
    if !root.is_dir() {
        return Err("La ruta de escaneo no es una carpeta".into());
    }
    scan_video_directory(root, recursive).map_err(|error| error.to_string())
}

#[cfg(test)]
mod batch_output_tests {
    use super::{
        batch_apply_common_rgb_scalar, batch_common_rgb_luminance_scalar,
        batch_planet_disc_is_reliable, batch_warp_disc_to_reference,
        materialize_batch_ap_points, next_planetary_batch_session_id,
        prepare_batch_output_impl, probe_batch_output_directory_impl,
        register_batch_output_result_impl, scan_video_directory, PLANETARY_BATCH_MANIFEST,
        PLANETARY_BATCH_OUTPUT_MARKER,
    };
    use crate::smart_grid::ApPoint;
    use std::path::{Path, PathBuf};

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "zenith-batch-output-{label}-{}",
                next_planetary_batch_session_id()
            ));
            std::fs::create_dir_all(&path).expect("crear raíz temporal");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_source(path: &Path) -> String {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("crear parent de fuente");
        }
        std::fs::write(path, b"fixture").expect("escribir fuente");
        path.display().to_string()
    }

    fn reference_points() -> Vec<ApPoint> {
        vec![
            ApPoint { x: 32.0, y: 24.0, size: 16 },
            ApPoint { x: 96.0, y: 72.0, size: 24 },
        ]
    }

    fn canonical(path: &Path) -> PathBuf {
        dunce::canonicalize(path).expect("canonicalizar fixture")
    }

    #[test]
    fn source_adjacent_creates_a_named_source_folder_below_each_session_root() {
        let root = TestRoot::new("adjacent");
        let first = write_source(&root.path().join("jupiter.ser"));
        let second = write_source(&root.path().join("night-2").join("jupiter.mov"));
        let third = write_source(&root.path().join("jupiter.mov"));

        let plan = prepare_batch_output_impl(
            vec![first.clone(), second.clone(), third.clone()],
            root.path().display().to_string(),
            "sourceAdjacent".to_string(),
            None,
            [128, 96],
            reference_points(),
        )
        .expect("preflight sourceAdjacent");

        assert_eq!(plan.schema_version, 1);
        assert_eq!(plan.policy, "sourceAdjacent");
        assert_eq!(plan.entries.len(), 3);
        assert!(plan.sequence_plan.frozen);
        assert_eq!(plan.sequence_plan.reference_source, first);
        assert_eq!(plan.sequence_plan.fixed_canvas, [128, 96]);
        assert_eq!(plan.normalized_ap_points.len(), 2);
        assert_eq!(plan.entries[0].source_path, first);
        assert_eq!(plan.entries[1].source_path, second);
        assert_eq!(plan.entries[2].source_path, third);

        let first_output = PathBuf::from(&plan.entries[0].output_folder);
        let second_output = PathBuf::from(&plan.entries[1].output_folder);
        let third_output = PathBuf::from(&plan.entries[2].output_folder);
        let animation_output = PathBuf::from(&plan.animation_folder);
        assert!(animation_output
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("Zenith_Batch_"));
        assert_eq!(animation_output.parent(), Some(canonical(root.path()).as_path()));
        assert_eq!(first_output.parent(), Some(animation_output.as_path()));
        assert_eq!(first_output.file_name().unwrap(), "jupiter.ser");
        assert_eq!(third_output.parent(), Some(animation_output.as_path()));
        assert_eq!(third_output.file_name().unwrap(), "jupiter.mov");
        let second_session = second_output.parent().expect("sesión de fuente anidada");
        assert_eq!(second_session.file_name(), animation_output.file_name());
        assert_eq!(
            second_session.parent(),
            Some(canonical(&root.path().join("night-2")).as_path())
        );
        assert_eq!(second_output.file_name().unwrap(), "jupiter.mov");
        assert!(first_output.is_dir());
        assert!(second_output.is_dir());
        assert!(third_output.is_dir());
        assert!(animation_output.join(PLANETARY_BATCH_OUTPUT_MARKER).is_file());
        assert!(second_session.join(PLANETARY_BATCH_OUTPUT_MARKER).is_file());
        assert!(animation_output.join(PLANETARY_BATCH_MANIFEST).is_file());

        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(animation_output.join(PLANETARY_BATCH_MANIFEST))
                .expect("leer manifiesto"),
        )
        .expect("decodificar manifiesto");
        assert_eq!(manifest["sequencePlan"]["frozen"], true);
        assert_eq!(manifest["sequencePlan"]["fixedCanvas"], serde_json::json!([128, 96]));
        assert_eq!(manifest["normalizedApPoints"].as_array().unwrap().len(), 2);
        assert_eq!(manifest["entries"].as_array().unwrap().len(), 3);
        assert!(manifest["completedResults"].as_array().unwrap().is_empty());
    }

    #[test]
    fn single_directory_preserves_relative_paths_for_homonymous_sources() {
        let root = TestRoot::new("single-source");
        let destination = TestRoot::new("single-destination");
        let first = write_source(&root.path().join("night-a").join("jupiter.ser"));
        let second = write_source(&root.path().join("night-b").join("jupiter.ser"));
        let third = write_source(&root.path().join("moon.mov"));

        let plan = prepare_batch_output_impl(
            vec![first, second, third],
            root.path().display().to_string(),
            "singleDirectory".to_string(),
            Some(destination.path().display().to_string()),
            [128, 96],
            reference_points(),
        )
        .expect("preflight singleDirectory");

        assert_eq!(plan.policy, "singleDirectory");
        assert_eq!(plan.entries.len(), 3);
        let session_root = PathBuf::from(&plan.animation_folder);
        assert!(session_root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("Zenith_Batch_"));
        assert_eq!(session_root.parent(), Some(canonical(destination.path()).as_path()));
        assert_eq!(
            PathBuf::from(&plan.entries[0].output_folder),
            session_root.join("night-a").join("jupiter.ser")
        );
        assert_eq!(
            PathBuf::from(&plan.entries[1].output_folder),
            session_root.join("night-b").join("jupiter.ser")
        );
        assert_eq!(
            PathBuf::from(&plan.entries[2].output_folder),
            session_root.join("moon.mov")
        );
        assert!(plan
            .entries
            .iter()
            .all(|entry| Path::new(&entry.output_folder).is_dir()));
        assert!(session_root.join(PLANETARY_BATCH_OUTPUT_MARKER).is_file());
        assert!(session_root.join(PLANETARY_BATCH_MANIFEST).is_file());
    }

    #[test]
    fn failed_write_probe_removes_temporary_and_committed_names() {
        let root = TestRoot::new("probe-cleanup");
        let session_id = "test-session";
        let temporary = root
            .path()
            .join(format!(".zenith-write-probe-{session_id}.tmp"));
        let committed = root
            .path()
            .join(format!(".zenith-write-probe-{session_id}.committed"));

        let error = probe_batch_output_directory_impl(root.path(), session_id, || {
            Err(std::io::Error::other("fallo inyectado después del rename"))
        })
        .expect_err("el fallo inyectado debe propagarse");

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(!temporary.exists());
        assert!(!committed.exists());
    }

    #[test]
    fn completed_pair_is_recorded_atomically_in_the_session_manifest() {
        let root = TestRoot::new("manifest-result");
        let source = write_source(&root.path().join("jupiter.ser"));
        let plan = prepare_batch_output_impl(
            vec![source.clone()],
            root.path().display().to_string(),
            "sourceAdjacent".to_string(),
            None,
            [128, 96],
            reference_points(),
        )
        .expect("preflight");
        let output = PathBuf::from(&plan.entries[0].output_folder).join("Stack_fixture");
        std::fs::create_dir(&output).expect("crear publicación simulada");
        let prepared = output.join("jupiter_Prepared_RGB16.png");
        let master = output.join("jupiter_Linear_Master_RGB16.tiff");
        std::fs::write(&prepared, b"png").expect("escribir preparada");
        std::fs::write(&master, b"tiff").expect("escribir master");

        register_batch_output_result_impl(
            &plan.animation_folder,
            &plan.session_id,
            &source,
            &prepared.display().to_string(),
            &master.display().to_string(),
        )
        .expect("registrar resultado");

        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(Path::new(&plan.animation_folder).join(PLANETARY_BATCH_MANIFEST))
                .expect("leer manifiesto"),
        )
        .expect("decodificar manifiesto");
        let completed = manifest["completedResults"].as_array().unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0]["sourcePath"], source);
        assert_eq!(
            completed[0]["preparedPath"],
            canonical(&prepared).display().to_string()
        );
        assert_eq!(
            completed[0]["linearMasterPath"],
            canonical(&master).display().to_string()
        );
        assert!(std::fs::read_dir(&plan.animation_folder)
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".Zenith_Batch_manifest.")));
    }

    #[test]
    fn recursive_scan_ignores_marked_output_directories_and_their_videos() {
        let root = TestRoot::new("scan-marker");
        let source = write_source(&root.path().join("capture.ser"));
        let nested_source = write_source(&root.path().join("inputs").join("capture.mov"));
        let output = root.path().join("Animacion_previous");
        std::fs::create_dir_all(&output).expect("crear salida anterior");
        std::fs::write(output.join(PLANETARY_BATCH_OUTPUT_MARKER), b"marker")
            .expect("marcar salida anterior");
        write_source(&output.join("animacion.mp4"));

        let recursive = scan_video_directory(root.path(), true).expect("scan recursivo");
        assert_eq!(recursive, vec![source, nested_source]);
        assert!(scan_video_directory(&output, true)
            .expect("scan directo de salida")
            .is_empty());

        let root_only = scan_video_directory(root.path(), false).expect("scan raíz");
        assert_eq!(root_only, vec![root.path().join("capture.ser").display().to_string()]);
    }

    #[test]
    fn invalid_policy_fails_before_creating_any_session_directory() {
        let root = TestRoot::new("invalid-policy");
        let source = write_source(&root.path().join("capture.ser"));
        let entries_before = std::fs::read_dir(root.path()).unwrap().count();

        let error = prepare_batch_output_impl(
            vec![source],
            root.path().display().to_string(),
            "somewhereElse".to_string(),
            None,
            [128, 96],
            reference_points(),
        )
        .expect_err("política inválida");

        assert_eq!(error.code, "invalid_policy");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), entries_before);
    }

    #[test]
    fn normalized_ap_geometry_scales_without_changing_the_grid_topology() {
        let normalized = super::normalize_batch_ap_points(&reference_points(), [128, 96])
            .expect("normalizar AP");
        let round_trip = materialize_batch_ap_points(&normalized, 128, 96);
        let materialized = materialize_batch_ap_points(&normalized, 256, 192);

        assert_eq!(round_trip.len(), 2);
        assert!((round_trip[0].x - 32.0).abs() <= 0.0001);
        assert!((round_trip[0].y - 24.0).abs() <= 0.0001);
        assert_eq!(round_trip[0].size, 16);
        assert_eq!(materialized.len(), 2);
        assert!((materialized[0].x - 64.0).abs() <= 1.0);
        assert!((materialized[0].y - 48.0).abs() <= 1.0);
        assert_eq!(materialized[0].size, 32);
        assert!((materialized[1].x - 192.0).abs() <= 1.0);
        assert!((materialized[1].y - 144.0).abs() <= 1.0);
        assert_eq!(materialized[1].size, 48);
    }

    #[test]
    fn limb_transform_recenters_a_reliable_disc_without_texture_matching() {
        let (width, height) = (64usize, 64usize);
        let mut mono = vec![100u16; width * height];
        for y in 0..height {
            for x in 0..width {
                let dx = x as f64 - 21.0;
                let dy = y as f64 - 27.0;
                if dx * dx + dy * dy <= 10.0 * 10.0 {
                    mono[y * width + x] = 12_000;
                }
            }
        }
        let current = crate::derotation::detect_planet_disc(&mono, width, height);
        assert!(batch_planet_disc_is_reliable(
            &current, &mono, width, height
        ));
        let reference = crate::derotation::PlanetDisc {
            cx: width as f64 / 2.0,
            cy: height as f64 / 2.0,
            radius_x: current.radius_x,
            radius_y: current.radius_y,
            angle_deg: current.angle_deg,
            phase: current.phase,
        };
        let rgb: Vec<u16> = mono
            .iter()
            .flat_map(|value| [*value, *value, *value])
            .collect();
        let aligned = batch_warp_disc_to_reference(
            &rgb,
            width,
            height,
            &current,
            &reference,
            0.02,
            1.0,
        );
        let aligned_mono: Vec<u16> = aligned.chunks_exact(3).map(|pixel| pixel[0]).collect();
        let detected = crate::derotation::detect_planet_disc(&aligned_mono, width, height);
        assert!((detected.cx - reference.cx).abs() <= 1.0);
        assert!((detected.cy - reference.cy).abs() <= 1.0);
    }

    #[test]
    fn photometric_normalization_uses_one_scalar_for_all_rgb_channels() {
        let mut reference = Vec::new();
        let mut current = Vec::new();
        for value in 1_000u16..2_000u16 {
            reference.extend_from_slice(&[value, value, value]);
            current.extend_from_slice(&[value / 2, value / 2, value / 2]);
        }
        let scalar = batch_common_rgb_luminance_scalar(&reference, &current);
        assert!((scalar - 2.0).abs() <= 0.01);
        let mut rgb = vec![1_000u16, 2_000, 3_000];
        batch_apply_common_rgb_scalar(&mut rgb, scalar);
        assert_eq!(rgb, vec![2_000, 4_000, 6_000]);
    }
}

#[tauri::command]
async fn create_gif_animation(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    folder: String,
    delay_ms: u64,
    boomerang: bool,
    format: String,
    rotation: i32,
    brightness: f32,
    contrast: f32,
    saturation: f32,
    gamma: f32,           // Nuevo: Correccion gamma
    levels_black: f32,    // Nuevo: Punto negro (0.0 - 1.0)
    levels_white: f32,    // Nuevo: Punto blanco (0.0 - 1.0)
    hue_shift: f32,       // Nuevo: Desplazamiento de matiz en grados
    color_filter: String, // Nuevo: Filtro de color (ej. "solar-orange", "inv-solar")
    overlay_mode: String,
    watermark_text: String,
    watermark_opacity: f32,
    frame_line_top: String,
    frame_line_bottom: String,
    font_name: String,
) -> Result<String, String> {
    state.license_manager.check_access()?;
    use image::{Delay, Frame};
    use std::fs::File;

    emit_progress(&app, "Generando Archivo...", 0.0, None);
    let mut files: Vec<PathBuf> = fs::read_dir(&folder)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |e| e == "png"))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err("No hay imagenes".into());
    }

    if boomerang && files.len() > 2 {
        let mut reverse_files = files.clone();
        reverse_files.reverse();
        if reverse_files.len() > 1 {
            reverse_files.remove(0);
        }
        if reverse_files.len() > 0 {
            reverse_files.pop();
        }
        files.extend(reverse_files);
    }

    let is_video = format == "mp4" || format == "avi" || format == "mov";
    let ext = if is_video { format.as_str() } else { "gif" };
    let filename = format!("animacion_video.{}", ext);
    let out_path = Path::new(&folder).join(filename);
    let out_path_str = clean_windows_path(out_path.clone());

    let font_map = get_font_map();
    let font_path_str = font_map
        .iter()
        .find(|(k, _)| k == &font_name)
        .map(|(_, v)| v)
        .unwrap_or(&font_map[0].1);
    let font_opt = if overlay_mode != "none" {
        if let Some(f) = load_font_from_path(font_path_str) {
            Some(f)
        } else {
            get_fallback_font()
        }
    } else {
        None
    };

    let mut ffmpeg_child = if is_video {
        let fps = 1000.0 / (delay_ms as f64);
        let cmd_path = get_ffmpeg_command(&app);

        let mut args = vec![
            "-y".to_string(),
            "-f".to_string(),
            "image2pipe".to_string(),
            "-vcodec".to_string(),
            "png".to_string(),
            "-r".to_string(),
            fps.to_string(),
            "-i".to_string(),
            "-".to_string(),
        ];

        // Codec specs
        if format == "avi" {
            args.extend_from_slice(&[
                "-c:v".to_string(),
                "mpeg4".to_string(),
                "-q:v".to_string(),
                "5".to_string(), // Quality for AVI
            ]);
        } else {
            // MP4 default
            args.extend_from_slice(&[
                "-c:v".to_string(),
                "libx264".to_string(),
                "-pix_fmt".to_string(),
                "yuv420p".to_string(),
                "-crf".to_string(),
                "18".to_string(),
                "-preset".to_string(),
                "slow".to_string(),
            ]);
        }

        // Scale filter (must be divisible by 2)
        args.extend_from_slice(&[
            "-vf".to_string(),
            "scale=trunc(iw/2)*2:trunc(ih/2)*2".to_string(),
            out_path_str.clone(),
        ]);

        let child_res = {
            let mut cmd = Command::new(&cmd_path);
            #[cfg(target_os = "windows")]
            cmd.creation_flags(0x08000000);
            cmd.args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        };

        match child_res {
            Ok(child) => Some(child),
            Err(e) => return Err(format!("Error FFmpeg: {}", e)),
        }
    } else {
        None
    };

    let file_out = if !is_video {
        Some(File::create(&out_path).map_err(|e| e.to_string())?)
    } else {
        None
    };
    let mut gif_encoder = if let Some(f) = file_out {
        let writer = BufWriter::new(f);
        let mut enc = image::codecs::gif::GifEncoder::new(writer);
        enc.set_repeat(image::codecs::gif::Repeat::Infinite)
            .map_err(|e| e.to_string())?;
        Some(enc)
    } else {
        None
    };

    for (i, p) in files.iter().enumerate() {
        emit_progress(
            &app,
            "Procesando frame",
            (i as f32 / files.len() as f32) * 100.0,
            None,
        );
        let mut img = image::open(p).map_err(|e| e.to_string())?;
        img = match rotation {
            90 => img.rotate90(),
            180 => img.rotate180(),
            270 => img.rotate270(),
            _ => img,
        };
        if brightness.abs() > 0.01 {
            img = img.brighten((brightness * 20.0) as i32);
        }
        if (contrast - 1.0).abs() > 0.01 {
            img = img.adjust_contrast(contrast);
        }
        if (saturation - 1.0).abs() > 0.01 {
            apply_saturation_inplace(&mut img, saturation);
        }
        // Nuevos Filtros Visuales
        if (gamma - 1.0).abs() > 0.01 {
            // Simple lambda gamma impl or usage of image crate
            // image crate doesn't have direct gamma? We can do manual pixel iter
            // Inverted Gamma request by User: Higher Gamma = Darker Image?
            // Standard: powf(1/gamma). User wants inverted?
            // If user says "inverted effect", maybe they want powf(gamma).
            // Let's swap to powf(gamma).
            let p_val = gamma;
            let lut: Vec<u8> = (0..256)
                .map(|i| ((i as f32 / 255.0).powf(p_val) * 255.0).clamp(0.0, 255.0) as u8)
                .collect();
            for p in img.as_mut_rgb8().unwrap().pixels_mut() {
                p[0] = lut[p[0] as usize];
                p[1] = lut[p[1] as usize];
                p[2] = lut[p[2] as usize];
            }
        }
        // Levels (Black/White point)
        if levels_black > 0.0 || levels_white < 1.0 {
            let low = (levels_black * 255.0).max(0.0);
            let high = (levels_white * 255.0).min(255.0);
            let range = (high - low).max(1.0);
            for p in img.as_mut_rgb8().unwrap().pixels_mut() {
                for c in 0..3 {
                    let v = p[c] as f32;
                    p[c] = ((v - low) / range * 255.0).clamp(0.0, 255.0) as u8;
                }
            }
        }
        // Hue Shift
        if hue_shift.abs() > 0.1 {
            image::imageops::colorops::huerotate_in_place(&mut img, hue_shift as i32);
        }
        // Color Filter (Pseudo-Color for Mono)
        if !color_filter.is_empty() && color_filter != "none" {
            // Apply gradient mapping or tinting
            // Example: Solar Orange
            let (tr, tg, tb) = match color_filter.as_str() {
                "solar-orange" => (1.0, 0.6, 0.2), // Naranja solar
                "solar-yellow" => (1.0, 0.9, 0.3), // Amarillo
                "h-alpha" => (1.0, 0.2, 0.2),      // Rojo profundo
                "calcium-k" => (0.3, 0.2, 1.0),    // Violeta/Azul
                _ => (1.0, 1.0, 1.0),
            };
            if tr != 1.0 || tg != 1.0 || tb != 1.0 {
                // Convert to grayscale first (luminance) then tint
                let gray = image::imageops::colorops::grayscale(&img);
                let mut rgb = image::RgbImage::new(gray.width(), gray.height());
                for (x, y, p) in gray.enumerate_pixels() {
                    let l = p[0] as f32;
                    let r = (l * tr).clamp(0.0, 255.0) as u8;
                    let g = (l * tg).clamp(0.0, 255.0) as u8;
                    let b = (l * tb).clamp(0.0, 255.0) as u8;
                    rgb.put_pixel(x, y, image::Rgb([r, g, b]));
                }
                img = DynamicImage::ImageRgb8(rgb);
            }
        }

        if let Some(font) = &font_opt {
            if overlay_mode == "frame" {
                let w = img.width();
                let h = img.height();
                let border_h = (h as f32 * 0.15) as u32;
                let new_h = h + border_h;
                let mut canvas =
                    RgbaImage::from_pixel(w, new_h, Rgba([255u8, 255u8, 255u8, 255u8]));
                image::imageops::overlay(&mut canvas, &img, 0, 0);
                let scale_title = Scale::uniform(border_h as f32 * 0.4);
                let scale_detail = Scale::uniform(border_h as f32 * 0.20);
                draw_text_mut(
                    &mut canvas,
                    Rgba([0u8, 0u8, 0u8, 255u8]),
                    (w as f32 * 0.05) as u32,
                    (h + border_h / 4) as u32,
                    scale_title,
                    font,
                    &frame_line_top,
                );
                draw_text_mut(
                    &mut canvas,
                    Rgba([0u8, 0u8, 0u8, 255u8]),
                    (w as f32 * 0.05) as u32,
                    (h + (border_h as f32 * 0.65) as u32) as u32,
                    scale_detail,
                    font,
                    &frame_line_bottom,
                );
                img = DynamicImage::ImageRgba8(canvas);
            } else if overlay_mode == "watermark" {
                let w = img.width();
                let h = img.height();
                let scale = Scale::uniform((h as f32 * 0.05).max(12.0));
                let color = Rgba([255, 255, 255, (255.0 * watermark_opacity) as u8]);
                let mut canvas = img.to_rgba8();
                let text_w = watermark_text.len() as f32 * (scale.x * 0.5);
                let x = (w as f32 - text_w - 20.0).max(0.0) as u32;
                let y = (h as f32 - scale.y - 20.0).max(0.0) as u32;
                draw_text_mut(&mut canvas, color, x, y, scale, font, &watermark_text);
                img = DynamicImage::ImageRgba8(canvas);
            }
        }
        let rgba_img = img.to_rgba8();
        if is_video {
            if let Some(child) = &mut ffmpeg_child {
                if let Some(stdin) = &mut child.stdin {
                    let mut buf = Vec::new();
                    let _ = DynamicImage::ImageRgba8(rgba_img.clone())
                        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png);
                    let _ = stdin.write_all(&buf);
                }
            }
        } else {
            if let Some(enc) = &mut gif_encoder {
                let frame = Frame::from_parts(
                    rgba_img,
                    0,
                    0,
                    Delay::from_numer_denom_ms(delay_ms as u32, 1),
                );
                let _ = enc.encode_frame(frame);
            }
        }
    }
    if is_video {
        if let Some(mut child) = ffmpeg_child {
            drop(child.stdin.take());
            let _ = child.wait();
        }
    }
    emit_progress(&app, "Listo", 100.0, None);
    Ok(clean_windows_path(out_path))
}

fn solar_ha_gold_channel_value(gray: f32, channel_value: f32, color_strength: f32, highlight_protect: f32) -> f32 {
    let strength = color_strength.clamp(0.0, 1.0);
    let protect = highlight_protect.clamp(0.0, 1.0);
    let highlight_weight = ((gray - 0.68) / 0.32).clamp(0.0, 1.0) * protect;
    let protected_channel = channel_value * (1.0 - highlight_weight) + gray * highlight_weight;
    gray * (1.0 - strength) + protected_channel * strength
}

fn solar_ha_gold_gradient(v: f32, color_strength: f32, highlight_protect: f32) -> (f32, f32, f32) {
    const STOPS: [(f32, [f32; 3]); 5] = [
        (0.0, [0.05, 0.00, 0.00]),
        (0.35, [0.24, 0.06, 0.00]),
        (0.62, [0.58, 0.28, 0.02]),
        (0.86, [0.95, 0.76, 0.06]),
        (1.0, [1.00, 1.00, 0.55]),
    ];

    let v = v.clamp(0.0, 1.0);
    for pair in STOPS.windows(2) {
        let (a_pos, a_rgb) = pair[0];
        let (b_pos, b_rgb) = pair[1];
        if v <= b_pos {
            let t = ((v - a_pos) / (b_pos - a_pos)).clamp(0.0, 1.0);
            let r = a_rgb[0] + (b_rgb[0] - a_rgb[0]) * t;
            let g = a_rgb[1] + (b_rgb[1] - a_rgb[1]) * t;
            let b = a_rgb[2] + (b_rgb[2] - a_rgb[2]) * t;
            return (
                solar_ha_gold_channel_value(v, r, color_strength, highlight_protect),
                solar_ha_gold_channel_value(v, g, color_strength, highlight_protect),
                solar_ha_gold_channel_value(v, b, color_strength, highlight_protect),
            );
        }
    }

    (
        solar_ha_gold_channel_value(v, 1.0, color_strength, highlight_protect),
        solar_ha_gold_channel_value(v, 1.0, color_strength, highlight_protect),
        solar_ha_gold_channel_value(v, 0.55, color_strength, highlight_protect),
    )
}

fn mix_tint_factor(factor: f32, color_strength: f32) -> f32 {
    1.0 + (factor - 1.0) * color_strength.clamp(0.0, 1.0)
}

const ANIMATION_FFMPEG_STDERR_LIMIT: usize = 256 * 1024;
const ANIMATION_FFMPEG_PIPE_IDLE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);
const ANIMATION_FFMPEG_EXIT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(60);
const ANIMATION_FFMPEG_REAP_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);
const ANIMATION_FFMPEG_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(25);
static ANIMATION_EXPORT_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// Drain the whole FFmpeg diagnostic pipe while retaining only its tail.  The
/// drain is deliberately independent from the retained size: verbose FFmpeg
/// output must never fill the OS pipe and deadlock an otherwise valid encode.
fn drain_animation_ffmpeg_stderr<R: Read>(mut reader: R) -> Vec<u8> {
    let mut retained = Vec::with_capacity(ANIMATION_FFMPEG_STDERR_LIMIT);
    let mut chunk = [0u8; 8192];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if read >= ANIMATION_FFMPEG_STDERR_LIMIT {
            retained.clear();
            retained.extend_from_slice(&chunk[read - ANIMATION_FFMPEG_STDERR_LIMIT..read]);
            continue;
        }
        let overflow = retained
            .len()
            .saturating_add(read)
            .saturating_sub(ANIMATION_FFMPEG_STDERR_LIMIT);
        if overflow > 0 {
            retained.drain(..overflow);
        }
        retained.extend_from_slice(&chunk[..read]);
    }
    retained
}

enum AnimationFfmpegWriterCommand {
    Frame(Vec<u8>),
    Finish,
}

enum AnimationFfmpegWriterEvent {
    Progress,
    FrameFinished(Result<(), String>),
    StreamFinished(Result<(), String>),
}

/// Keep potentially blocking pipe I/O outside the command thread.  Progress
/// events make a genuinely slow consumer distinguishable from a wedged one;
/// cancellation or an idle deadline can therefore kill FFmpeg even while the
/// writer is blocked in an OS `write`/`flush` call.
fn spawn_animation_ffmpeg_writer(
    mut stdin: std::process::ChildStdin,
) -> (
    crossbeam_channel::Sender<AnimationFfmpegWriterCommand>,
    crossbeam_channel::Receiver<AnimationFfmpegWriterEvent>,
    std::thread::JoinHandle<()>,
) {
    // One frame is enough to decouple image preparation without allowing an
    // unbounded queue of full-resolution RGB frames in RAM.
    let (command_tx, command_rx) = crossbeam_channel::bounded(1);
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let thread = std::thread::spawn(move || {
        while let Ok(command) = command_rx.recv() {
            match command {
                AnimationFfmpegWriterCommand::Frame(bytes) => {
                    let mut offset = 0usize;
                    let result = (|| -> Result<(), String> {
                        while offset < bytes.len() {
                            match stdin.write(&bytes[offset..]) {
                                Ok(0) => {
                                    return Err(
                                        "FFmpeg cerro su entrada durante un frame".to_string()
                                    )
                                }
                                Ok(written) => {
                                    offset = offset.saturating_add(written);
                                    let _ = event_tx.send(AnimationFfmpegWriterEvent::Progress);
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                                Err(error) => {
                                    return Err(format!(
                                        "Error escribiendo un frame a FFmpeg: {error}"
                                    ))
                                }
                            }
                        }
                        Ok(())
                    })();
                    let failed = result.is_err();
                    let _ = event_tx.send(AnimationFfmpegWriterEvent::FrameFinished(result));
                    if failed {
                        break;
                    }
                }
                AnimationFfmpegWriterCommand::Finish => {
                    let result = stdin
                        .flush()
                        .map_err(|error| format!("Error vaciando la entrada de FFmpeg: {error}"));
                    let _ = event_tx.send(AnimationFfmpegWriterEvent::StreamFinished(result));
                    break;
                }
            }
        }
        // Dropping ChildStdin is the EOF signal consumed by FFmpeg.
    });
    (command_tx, event_rx, thread)
}

fn terminate_animation_ffmpeg_child(child: &mut std::process::Child) -> bool {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return true;
    }
    let _ = child.kill();
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(_) => return false,
        }
        if started.elapsed() >= ANIMATION_FFMPEG_REAP_TIMEOUT {
            return false;
        }
        std::thread::sleep(ANIMATION_FFMPEG_POLL_INTERVAL);
    }
}

/// Owns the complete FFmpeg lifecycle.  Any early return (decode error,
/// cancellation, broken pipe, supersession or timeout) reaches Drop, which
/// kills/reaps the encoder and joins both pipe-draining threads.
struct AnimationFfmpegProcess {
    child: Option<std::process::Child>,
    writer_tx: Option<crossbeam_channel::Sender<AnimationFfmpegWriterCommand>>,
    writer_rx: crossbeam_channel::Receiver<AnimationFfmpegWriterEvent>,
    writer_thread: Option<std::thread::JoinHandle<()>>,
    stderr_thread: Option<std::thread::JoinHandle<Vec<u8>>>,
}

impl AnimationFfmpegProcess {
    fn spawn(mut command: Command) -> Result<Self, String> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("Error iniciando FFmpeg: {error}"))?;
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                let _ = terminate_animation_ffmpeg_child(&mut child);
                return Err("FFmpeg no expuso su canal de diagnostico".into());
            }
        };
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let _ = terminate_animation_ffmpeg_child(&mut child);
                return Err("FFmpeg no expuso su canal de entrada".into());
            }
        };
        let (writer_tx, writer_rx, writer_thread) = spawn_animation_ffmpeg_writer(stdin);
        let stderr_thread = std::thread::spawn(move || drain_animation_ffmpeg_stderr(stderr));
        Ok(Self {
            child: Some(child),
            writer_tx: Some(writer_tx),
            writer_rx,
            writer_thread: Some(writer_thread),
            stderr_thread: Some(stderr_thread),
        })
    }

    fn submit_writer_command(
        &mut self,
        mut command: AnimationFfmpegWriterCommand,
        job_token: &PlanetaryJobToken,
        operation: &str,
    ) -> Result<(), String> {
        let started = std::time::Instant::now();
        loop {
            if job_token.is_cancelled() {
                self.abort();
                return Err(format!("Exportacion cancelada durante {operation}"));
            }
            let sender = self
                .writer_tx
                .as_ref()
                .ok_or_else(|| "El escritor FFmpeg ya fue cerrado".to_string())?;
            match sender.send_timeout(command, ANIMATION_FFMPEG_POLL_INTERVAL) {
                Ok(()) => return Ok(()),
                Err(crossbeam_channel::SendTimeoutError::Timeout(returned)) => {
                    command = returned;
                    if started.elapsed() >= ANIMATION_FFMPEG_PIPE_IDLE_TIMEOUT {
                        self.abort();
                        return Err(format!(
                            "FFmpeg no acepto {operation} durante {} s; se termino el proceso",
                            ANIMATION_FFMPEG_PIPE_IDLE_TIMEOUT.as_secs()
                        ));
                    }
                }
                Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                    self.abort();
                    return Err("El escritor FFmpeg termino antes de tiempo".to_string());
                }
            }
        }
    }

    fn wait_writer_event(
        &mut self,
        job_token: &PlanetaryJobToken,
        finish_stream: bool,
    ) -> Result<(), String> {
        let mut last_progress = std::time::Instant::now();
        loop {
            if job_token.is_cancelled() {
                self.abort();
                return Err("Exportacion cancelada mientras FFmpeg consumia datos".into());
            }
            match self.writer_rx.recv_timeout(ANIMATION_FFMPEG_POLL_INTERVAL) {
                Ok(AnimationFfmpegWriterEvent::Progress) => {
                    last_progress = std::time::Instant::now();
                }
                Ok(AnimationFfmpegWriterEvent::FrameFinished(result)) if !finish_stream => {
                    return result;
                }
                Ok(AnimationFfmpegWriterEvent::StreamFinished(result)) if finish_stream => {
                    return result;
                }
                Ok(_) => {
                    self.abort();
                    return Err("FFmpeg devolvio un evento de escritura inesperado".into());
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if let Some(child) = self.child.as_mut() {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                self.abort();
                                return Err(format!(
                                    "FFmpeg termino prematuramente con estado {status}"
                                ));
                            }
                            Ok(None) => {}
                            Err(error) => {
                                self.abort();
                                return Err(format!("No se pudo consultar FFmpeg: {error}"));
                            }
                        }
                    }
                    if last_progress.elapsed() >= ANIMATION_FFMPEG_PIPE_IDLE_TIMEOUT {
                        self.abort();
                        return Err(format!(
                            "FFmpeg dejo de consumir la tuberia durante {} s; se termino el proceso",
                            ANIMATION_FFMPEG_PIPE_IDLE_TIMEOUT.as_secs()
                        ));
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    self.abort();
                    return Err("El escritor FFmpeg termino sin confirmar la operacion".into());
                }
            }
        }
    }

    fn write_frame(
        &mut self,
        bytes: Vec<u8>,
        job_token: &PlanetaryJobToken,
    ) -> Result<(), String> {
        self.submit_writer_command(
            AnimationFfmpegWriterCommand::Frame(bytes),
            job_token,
            "la entrega de un frame",
        )?;
        self.wait_writer_event(job_token, false)
    }

    fn join_stderr(&mut self) -> Vec<u8> {
        self.stderr_thread
            .take()
            .and_then(|thread| thread.join().ok())
            .unwrap_or_default()
    }

    fn abort(&mut self) {
        // Closing the sender alone is not sufficient when the worker is
        // blocked in write/flush. Kill the reader first so the OS wakes it.
        self.writer_tx.take();
        let reaped = self
            .child
            .take()
            .map(|mut child| terminate_animation_ffmpeg_child(&mut child))
            .unwrap_or(true);
        if reaped {
            if let Some(thread) = self.writer_thread.take() {
                let _ = thread.join();
            }
            let _ = self.join_stderr();
        } else {
            // A pathological kernel/device state must not freeze the UI. The
            // detached drainers own no application state and will end once the
            // OS closes the abandoned process pipes.
            self.writer_thread.take();
            self.stderr_thread.take();
        }
    }

    fn finish(mut self, job_token: &PlanetaryJobToken) -> Result<(), String> {
        self.submit_writer_command(
            AnimationFfmpegWriterCommand::Finish,
            job_token,
            "el cierre de la tuberia",
        )?;
        self.wait_writer_event(job_token, true)?;
        self.writer_tx.take();
        if let Some(thread) = self.writer_thread.take() {
            thread
                .join()
                .map_err(|_| "El escritor FFmpeg termino inesperadamente".to_string())?;
        }

        let exit_started = std::time::Instant::now();
        let status = loop {
            if job_token.is_cancelled() {
                self.abort();
                return Err("Exportacion cancelada mientras FFmpeg finalizaba".into());
            }
            match self.child.as_mut().expect("FFmpeg vigente").try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(error) => {
                    self.abort();
                    return Err(format!("Error esperando a FFmpeg: {error}"));
                }
            }
            if exit_started.elapsed() >= ANIMATION_FFMPEG_EXIT_TIMEOUT {
                self.abort();
                return Err(format!(
                    "FFmpeg no finalizo en {} s despues del EOF; se termino el proceso",
                    ANIMATION_FFMPEG_EXIT_TIMEOUT.as_secs()
                ));
            }
            std::thread::sleep(ANIMATION_FFMPEG_POLL_INTERVAL);
        };
        self.child.take();
        let stderr = self.join_stderr();
        if !status.success() {
            let diagnostics = String::from_utf8_lossy(&stderr);
            return Err(format!(
                "FFmpeg termino con estado {}: {}",
                status,
                diagnostics.trim()
            ));
        }
        Ok(())
    }
}

impl Drop for AnimationFfmpegProcess {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Sibling staging makes publication a single-filesystem rename.  Until
/// `publish` succeeds there is no user-visible output, and Drop removes every
/// interrupted or failed encode.
struct StagedAnimationExport {
    temporary_path: PathBuf,
    final_path: PathBuf,
    published: bool,
}

impl StagedAnimationExport {
    fn reserve(parent: &Path, extension: &str) -> Result<(Self, File), String> {
        for _ in 0..64 {
            let sequence = ANIMATION_EXPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            let suffix = format!(
                "{}_p{}_{}_{}",
                chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f"),
                std::process::id(),
                nanos,
                sequence
            );
            let final_path = parent.join(format!("export_{suffix}.{extension}"));
            if final_path.exists() {
                continue;
            }
            // Keep the real extension last so FFmpeg can infer its muxer.
            let temporary_path = parent.join(format!(".export_{suffix}.part.{extension}"));
            match std::fs::OpenOptions::new()
                .write(true)
                .read(true)
                .create_new(true)
                .open(&temporary_path)
            {
                Ok(file) => {
                    return Ok((
                        Self {
                            temporary_path,
                            final_path,
                            published: false,
                        },
                        file,
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "No se pudo reservar la salida de animacion: {error}"
                    ));
                }
            }
        }
        Err("No se pudo obtener un nombre unico para la animacion".into())
    }

    fn sync_payload(&self) -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.temporary_path)
            .map_err(|error| format!("No se pudo reabrir la animacion temporal: {error}"))?;
        let length = file
            .metadata()
            .map_err(|error| format!("No se pudo validar la animacion temporal: {error}"))?
            .len();
        if length == 0 {
            return Err("El codificador produjo una animacion vacia".into());
        }
        // image 0.23 delegates GIF finalization to Drop and the enabled
        // `raii_no_panic` feature cannot return a trailer-write error. Verify
        // the mandatory GIF trailer explicitly before reporting success.
        if self
            .temporary_path
            .extension()
            .and_then(|extension| extension.to_str())
            == Some("gif")
        {
            std::io::Seek::seek(&mut file, std::io::SeekFrom::End(-1))
                .map_err(|error| format!("No se pudo validar el trailer GIF: {error}"))?;
            let mut trailer = [0u8; 1];
            file.read_exact(&mut trailer)
                .map_err(|error| format!("No se pudo leer el trailer GIF: {error}"))?;
            if trailer[0] != 0x3b {
                return Err("El archivo GIF quedo incompleto (falta el trailer)".into());
            }
        }
        file.sync_all()
            .map_err(|error| format!("No se pudo sincronizar la animacion temporal: {error}"))
    }

    fn publish(&mut self) -> Result<(), String> {
        if self.final_path.exists() {
            return Err(format!(
                "La salida de animacion ya existe: {}",
                self.final_path.display()
            ));
        }
        std::fs::rename(&self.temporary_path, &self.final_path)
            .map_err(|error| format!("No se pudo publicar la animacion: {error}"))?;

        #[cfg(not(target_os = "windows"))]
        if let Some(parent) = self.final_path.parent() {
            if let Err(error) = File::open(parent).and_then(|directory| directory.sync_all()) {
                let _ = std::fs::remove_file(&self.final_path);
                return Err(format!(
                    "No se pudo sincronizar la carpeta de salida: {error}"
                ));
            }
        }
        self.published = true;
        Ok(())
    }
}

impl Drop for StagedAnimationExport {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.temporary_path);
        }
    }
}

#[inline]
fn animation_export_checkpoint(job_token: &PlanetaryJobToken, stage: &str) -> Result<(), String> {
    if job_token.is_cancelled() {
        Err(format!(
            "Exportacion cancelada o sustituida durante {stage}"
        ))
    } else {
        Ok(())
    }
}

#[tauri::command]
async fn export_animation_video(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    folder: String,
    files: Option<Vec<String>>, // NEW: Explicit file list
    delay_ms: u64,
    boomerang: bool,
    format: String,
    rotation: f32,
    brightness: f32,
    contrast: f32,
    saturation: f32,
    gamma: f32,
    levels_black: f32,
    levels_white: f32,
    hue_shift: f32,
    color_filter: String,
    color_strength: f32,
    highlight_protect: f32,
    overlay_mode: String,
    watermark_text: String,
    watermark_opacity: f32,
    frame_line_top: String,
    frame_line_bottom: String,
    font_name: String,
    manual_tint_r: f32,
    manual_tint_g: f32,
    manual_tint_b: f32,
    rescale_factor: f32,
    quality_preset: String,
    crop_x: f32,
    crop_y: f32,
    crop_w: f32,
    crop_h: f32,
) -> Result<String, String> {
    state.license_manager.check_access()?;

    // 0. Preparar Paths
    let p = Path::new(&folder);
    if !p.is_dir() {
        return Err("Carpeta no existe".into());
    }
    if !matches!(format.as_str(), "mp4" | "avi" | "gif") {
        return Err(format!("Formato de animacion no compatible: {format}"));
    }

    let request_id = begin_planetary_user_job(&state);
    let job_token =
        PlanetaryJobToken::for_app(&app, request_id, state.cancel_requested.clone());
    emit_progress(&app, "Iniciando exportacion...", 0.0, None);

    // 1. Validar FFmpeg si es video
    let is_video = matches!(format.as_str(), "mp4" | "avi");
    let ffmpeg_cmd = if is_video {
        get_ffmpeg_command(&app) // Uses "ffmpeg" or resolved path
    } else {
        String::new()
    };

    // 2. Obtener lista de archivos
    // PRIORIDAD: Si 'files' viene del frontend, usarlos DIRECTAMENTE (respetar orden y filtros)
    let mut files_to_process = if let Some(f_list) = files {
        if f_list.is_empty() {
            return Err("La lista de archivos esta vacia.".into());
        }
        f_list.iter().map(|s| PathBuf::from(s)).collect::<Vec<_>>()
    } else {
        // FALLBACK: Escaneo de carpeta (Legacy behavior)
        let mut f_list = Vec::new();
        let entries = fs::read_dir(&folder).map_err(|e| e.to_string())?;
        for entry in entries {
            animation_export_checkpoint(&job_token, "el escaneo de imagenes")?;
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension() {
                    let ext_str = ext.to_string_lossy().to_lowercase();
                    if ext_str == "png"
                        || ext_str == "jpg"
                        || ext_str == "jpeg"
                        || ext_str == "tif"
                        || ext_str == "tiff"
                    {
                        f_list.push(path);
                    }
                }
            }
        }
        if f_list.is_empty() {
            return Err("No se encontraron imagenes en la carpeta.".into());
        }
        // Ordenar alfabÃ©ticamente si escaneamos disco
        f_list.sort();
        f_list
    };

    // Boomerang Logic (Solo duplicar referencias)
    if boomerang {
        animation_export_checkpoint(&job_token, "la preparacion boomerang")?;
        let mut rev = files_to_process.clone();
        rev.reverse();
        // Remove first and last to avoid duplication at turning points
        if rev.len() > 2 {
            rev.remove(0);
            rev.remove(rev.len() - 1);
            files_to_process.extend(rev);
        }
    }

    let (mut staged_export, reserved_staging_file) =
        StagedAnimationExport::reserve(p, &format)?;
    let out_file = staged_export.final_path.clone();

    // 3. Preparar Encoder (Deferred for Video)
    let mut gif_encoder: Option<image::codecs::gif::GifEncoder<File>> = None;
    let mut ffmpeg_process: Option<AnimationFfmpegProcess> = None;
    let fps = 1000.0 / (delay_ms.max(20) as f64);

    // Canonical format to ensure FFmpeg stream homogeneity
    let mut canonical_w = 0u32;
    let mut canonical_h = 0u32;
    let mut canonical_depth_16 = false;

    if !is_video {
        // El archivo temporal ya fue reservado con create_new.
        let mut enc = image::codecs::gif::GifEncoder::new(reserved_staging_file);
        enc.set_repeat(image::codecs::gif::Repeat::Infinite)
            .map_err(|e| e.to_string())?;
        gif_encoder = Some(enc);
    } else {
        // FFmpeg reabre la reserva unica con -y; mantenerla cerrada evita
        // diferencias de comparticion de archivos en Windows.
        drop(reserved_staging_file);
    }

    // 4. Preparar Font
    let font_map = get_font_map();
    let font_path_str = font_map
        .iter()
        .find(|(k, _)| k == &font_name)
        .map(|(_, v)| v)
        .unwrap_or(&font_map[0].1);
    let font_opt = if overlay_mode != "none" {
        load_font_from_path(font_path_str).or_else(get_fallback_font)
    } else {
        None
    };

    // 5. Loop Procesamiento
    for (i, fpath) in files_to_process.iter().enumerate() {
        animation_export_checkpoint(&job_token, "el procesamiento de frames")?;
        emit_progress(
            &app,
            "Procesando frame",
            (i as f32 / files_to_process.len() as f32) * 100.0,
            None,
        );

        // A. Cargar Imagen y NormalizaciÃ³n Inicial
        let mut img = image::open(fpath)
            .map_err(|error| format!("No se pudo abrir {}: {error}", fpath.display()))?;
        let mut is_high_depth = match img.color() {
            ColorType::Rgb16 | ColorType::Rgba16 | ColorType::L16 | ColorType::La16 => true,
            _ => false,
        };

        // A.1 Recorte no destructivo: solo se aplica al frame en memoria durante exportacion.
        if crop_x >= 0.0 && crop_y >= 0.0 && crop_w > 0.001 && crop_h > 0.001 {
            let iw = img.width();
            let ih = img.height();
            let x0 = (crop_x.clamp(0.0, 1.0) * iw as f32).floor() as u32;
            let y0 = (crop_y.clamp(0.0, 1.0) * ih as f32).floor() as u32;
            let x1 = ((crop_x + crop_w).clamp(0.0, 1.0) * iw as f32).ceil() as u32;
            let y1 = ((crop_y + crop_h).clamp(0.0, 1.0) * ih as f32).ceil() as u32;

            if x0 < iw && y0 < ih {
                let cw = x1.saturating_sub(x0).min(iw - x0);
                let ch = y1.saturating_sub(y0).min(ih - y0);
                if cw >= 2 && ch >= 2 {
                    img = img.crop_imm(x0, y0, cw, ch);
                }
            }
        }

        // B. Rotacion
        let r_mod = (rotation as i32 % 360 + 360) % 360;
        img = match r_mod {
            90 => img.rotate90(),
            180 => img.rotate180(),
            270 => img.rotate270(),
            _ => img,
        };

        // C. Rescaling Individual
        if (rescale_factor - 1.0).abs() > 0.01 {
            let nw = (img.width() as f32 * rescale_factor) as u32;
            let nh = (img.height() as f32 * rescale_factor) as u32;
            if nw > 0 && nh > 0 {
                img = img.resize(nw, nh, image::imageops::FilterType::Lanczos3);
            }
        }
        animation_export_checkpoint(&job_token, "la geometria de un frame")?;

        // D. Canonical Sync (Fixes "color snow/noise" by ensuring stream homogeneity)
        if i > 0 && is_video {
            if img.width() != canonical_w || img.height() != canonical_h {
                img = img.resize_exact(
                    canonical_w,
                    canonical_h,
                    image::imageops::FilterType::Lanczos3,
                );
            }
            if canonical_depth_16 {
                if !is_high_depth {
                    img = DynamicImage::ImageRgb16(img.to_rgb16());
                    is_high_depth = true;
                }
            } else {
                if is_high_depth {
                    img = DynamicImage::ImageRgb8(img.to_rgb8());
                    is_high_depth = false;
                }
            }
        }

        // E. FILTERS PIPELINE
        let total_gain = (1.0 + brightness) * gamma;
        let apply_levels = levels_black > 0.001 || levels_white < 0.999;
        let apply_tint_manual = (manual_tint_r - 1.0).abs() > 0.01
            || (manual_tint_g - 1.0).abs() > 0.01
            || (manual_tint_b - 1.0).abs() > 0.01;
        let apply_tint_preset = !color_filter.is_empty() && color_filter != "none";
        let is_solar_ha_gold = color_filter == "solar-ha-gold";

        let (tr, tg, tb) = if apply_tint_preset {
            match color_filter.as_str() {
                "solar-ha-gold" => (1.0, 1.0, 1.0),
                "solar-orange" => (1.0, 0.6, 0.2),
                "solar-yellow" => (1.0, 0.9, 0.3),
                "h-alpha" => (1.0, 0.2, 0.2),
                "calcium-k" => (0.3, 0.2, 1.0),
                _ => (1.0, 1.0, 1.0),
            }
        } else {
            (1.0, 1.0, 1.0)
        };

        let hue_rad = hue_shift * std::f32::consts::PI / 180.0;
        let cos_h = hue_rad.cos();
        let sin_h = hue_rad.sin();
        let has_hue = hue_shift.abs() > 0.1;

        if is_high_depth {
            let mut rgb = img.to_rgb16();
            let l_min = (levels_black * 65535.0).max(0.0);
            let l_max = (levels_white * 65535.0).min(65535.0);
            let l_range = (l_max - l_min).max(1.0);
            for (pixel_index, p) in rgb.pixels_mut().enumerate() {
                if pixel_index & 0xffff == 0 {
                    animation_export_checkpoint(&job_token, "los filtros de 16 bits")?;
                }
                let mut r = p[0] as f32;
                let mut g = p[1] as f32;
                let mut b = p[2] as f32;
                if (total_gain - 1.0).abs() > 0.01 {
                    r *= total_gain;
                    g *= total_gain;
                    b *= total_gain;
                }
                if (contrast - 1.0).abs() > 0.01 {
                    r = ((r / 65535.0 - 0.5) * contrast + 0.5) * 65535.0;
                    g = ((g / 65535.0 - 0.5) * contrast + 0.5) * 65535.0;
                    b = ((b / 65535.0 - 0.5) * contrast + 0.5) * 65535.0;
                }
                if has_hue {
                    let nr = (0.213 + cos_h * 0.787 - sin_h * 0.213) * r
                        + (0.715 - cos_h * 0.715 - sin_h * 0.715) * g
                        + (0.072 - cos_h * 0.072 + sin_h * 0.928) * b;
                    let ng = (0.213 - cos_h * 0.213 + sin_h * 0.143) * r
                        + (0.715 + cos_h * 0.285 + sin_h * 0.140) * g
                        + (0.072 - cos_h * 0.072 - sin_h * 0.283) * b;
                    let nb = (0.213 - cos_h * 0.213 - sin_h * 0.787) * r
                        + (0.715 - cos_h * 0.715 + sin_h * 0.715) * g
                        + (0.072 + cos_h * 0.928 + sin_h * 0.072) * b;
                    r = nr;
                    g = ng;
                    b = nb;
                }
                if (saturation - 1.0).abs() > 0.01 {
                    let l = 0.299 * r + 0.587 * g + 0.114 * b;
                    r = l + (r - l) * saturation;
                    g = l + (g - l) * saturation;
                    b = l + (b - l) * saturation;
                }
                if apply_levels {
                    r = (r - l_min) / l_range * 65535.0;
                    g = (g - l_min) / l_range * 65535.0;
                    b = (b - l_min) / l_range * 65535.0;
                }
                if is_solar_ha_gold {
                    let l = ((0.299 * r + 0.587 * g + 0.114 * b) / 65535.0).clamp(0.0, 1.0);
                    let (sr, sg, sb) = solar_ha_gold_gradient(l, color_strength, highlight_protect);
                    r = sr * 65535.0;
                    g = sg * 65535.0;
                    b = sb * 65535.0;
                } else if apply_tint_preset || apply_tint_manual {
                    let mr = if apply_tint_manual {
                        mix_tint_factor(manual_tint_r, color_strength)
                    } else {
                        1.0
                    };
                    let mg = if apply_tint_manual {
                        mix_tint_factor(manual_tint_g, color_strength)
                    } else {
                        1.0
                    };
                    let mb = if apply_tint_manual {
                        mix_tint_factor(manual_tint_b, color_strength)
                    } else {
                        1.0
                    };
                    r *= mix_tint_factor(tr, color_strength) * mr;
                    g *= mix_tint_factor(tg, color_strength) * mg;
                    b *= mix_tint_factor(tb, color_strength) * mb;
                }
                p[0] = r.clamp(0.0, 65535.0) as u16;
                p[1] = g.clamp(0.0, 65535.0) as u16;
                p[2] = b.clamp(0.0, 65535.0) as u16;
            }
            img = image::DynamicImage::ImageRgb16(rgb);
        } else {
            let mut rgb = img.to_rgb8();
            let l_min = (levels_black * 255.0).max(0.0);
            let l_max = (levels_white * 255.0).min(255.0);
            let l_range = (l_max - l_min).max(1.0);
            for (pixel_index, p) in rgb.pixels_mut().enumerate() {
                if pixel_index & 0xffff == 0 {
                    animation_export_checkpoint(&job_token, "los filtros de 8 bits")?;
                }
                let mut r = p[0] as f32;
                let mut g = p[1] as f32;
                let mut b = p[2] as f32;
                if (total_gain - 1.0).abs() > 0.01 {
                    r *= total_gain;
                    g *= total_gain;
                    b *= total_gain;
                }
                if (contrast - 1.0).abs() > 0.01 {
                    r = ((r / 255.0 - 0.5) * contrast + 0.5) * 255.0;
                    g = ((g / 255.0 - 0.5) * contrast + 0.5) * 255.0;
                    b = ((b / 255.0 - 0.5) * contrast + 0.5) * 255.0;
                }
                if has_hue {
                    let nr = (0.213 + cos_h * 0.787 - sin_h * 0.213) * r
                        + (0.715 - cos_h * 0.715 - sin_h * 0.715) * g
                        + (0.072 - cos_h * 0.072 + sin_h * 0.928) * b;
                    let ng = (0.213 - cos_h * 0.213 + sin_h * 0.143) * r
                        + (0.715 + cos_h * 0.285 + sin_h * 0.140) * g
                        + (0.072 - cos_h * 0.072 - sin_h * 0.283) * b;
                    let nb = (0.213 - cos_h * 0.213 - sin_h * 0.787) * r
                        + (0.715 - cos_h * 0.715 + sin_h * 0.715) * g
                        + (0.072 + cos_h * 0.928 + sin_h * 0.072) * b;
                    r = nr;
                    g = ng;
                    b = nb;
                }
                if (saturation - 1.0).abs() > 0.01 {
                    let l = 0.299 * r + 0.587 * g + 0.114 * b;
                    r = l + (r - l) * saturation;
                    g = l + (g - l) * saturation;
                    b = l + (b - l) * saturation;
                }
                if apply_levels {
                    r = (r - l_min) / l_range * 255.0;
                    g = (g - l_min) / l_range * 255.0;
                    b = (b - l_min) / l_range * 255.0;
                }
                if is_solar_ha_gold {
                    let l = ((0.299 * r + 0.587 * g + 0.114 * b) / 255.0).clamp(0.0, 1.0);
                    let (sr, sg, sb) = solar_ha_gold_gradient(l, color_strength, highlight_protect);
                    r = sr * 255.0;
                    g = sg * 255.0;
                    b = sb * 255.0;
                } else if apply_tint_preset || apply_tint_manual {
                    let mr = if apply_tint_manual {
                        mix_tint_factor(manual_tint_r, color_strength)
                    } else {
                        1.0
                    };
                    let mg = if apply_tint_manual {
                        mix_tint_factor(manual_tint_g, color_strength)
                    } else {
                        1.0
                    };
                    let mb = if apply_tint_manual {
                        mix_tint_factor(manual_tint_b, color_strength)
                    } else {
                        1.0
                    };
                    r *= mix_tint_factor(tr, color_strength) * mr;
                    g *= mix_tint_factor(tg, color_strength) * mg;
                    b *= mix_tint_factor(tb, color_strength) * mb;
                }
                p[0] = r.clamp(0.0, 255.0) as u8;
                p[1] = g.clamp(0.0, 255.0) as u8;
                p[2] = b.clamp(0.0, 255.0) as u8;
            }
            img = image::DynamicImage::ImageRgb8(rgb);
        }

        // F. Overlays (High-Fidelity)
        if let Some(font) = &font_opt {
            if overlay_mode == "frame" {
                let w = img.width();
                let h = img.height();
                let border_h = (h as f32 * 0.15) as u32;
                let new_h = h + border_h;

                if is_high_depth {
                    let mut canvas = image::ImageBuffer::from_pixel(
                        w,
                        new_h,
                        image::Rgba([65535, 65535, 65535, 65535]),
                    );
                    image::imageops::overlay(&mut canvas, &img.to_rgba16(), 0, 0);
                    let scale_title = Scale::uniform(border_h as f32 * 0.4);
                    let scale_detail = Scale::uniform(border_h as f32 * 0.20);
                    let black = image::Rgba([0u16, 0u16, 0u16, 65535u16]);
                    draw_text_mut(
                        &mut canvas,
                        black,
                        (w as f32 * 0.05) as u32,
                        (h + border_h / 4) as u32,
                        scale_title,
                        font,
                        &frame_line_top,
                    );
                    draw_text_mut(
                        &mut canvas,
                        black,
                        (w as f32 * 0.05) as u32,
                        (h + (border_h as f32 * 0.65) as u32) as u32,
                        scale_detail,
                        font,
                        &frame_line_bottom,
                    );
                    img = DynamicImage::ImageRgba16(canvas);
                } else {
                    let mut canvas = RgbaImage::from_pixel(w, new_h, Rgba([255, 255, 255, 255]));
                    image::imageops::overlay(&mut canvas, &img.to_rgba8(), 0, 0);
                    let scale_title = Scale::uniform(border_h as f32 * 0.4);
                    let scale_detail = Scale::uniform(border_h as f32 * 0.20);
                    let black = Rgba([0, 0, 0, 255]);
                    draw_text_mut(
                        &mut canvas,
                        black,
                        (w as f32 * 0.05) as u32,
                        (h + border_h / 4) as u32,
                        scale_title,
                        font,
                        &frame_line_top,
                    );
                    draw_text_mut(
                        &mut canvas,
                        black,
                        (w as f32 * 0.05) as u32,
                        (h + (border_h as f32 * 0.65) as u32) as u32,
                        scale_detail,
                        font,
                        &frame_line_bottom,
                    );
                    img = DynamicImage::ImageRgba8(canvas);
                }
            } else if overlay_mode == "watermark" {
                let w = img.width();
                let h = img.height();
                let scale = Scale::uniform((h as f32 * 0.05).max(12.0));
                let text_w = watermark_text.len() as f32 * (scale.x * 0.5);
                let x = (w as f32 - text_w - 20.0).max(0.0) as u32;
                let y = (h as f32 - scale.y - 20.0).max(0.0) as u32;

                if is_high_depth {
                    let mut canvas = img.to_rgba16();
                    let color = image::Rgba([
                        65535u16,
                        65535u16,
                        65535u16,
                        (65535.0 * watermark_opacity) as u16,
                    ]);
                    draw_text_mut(&mut canvas, color, x, y, scale, font, &watermark_text);
                    img = DynamicImage::ImageRgba16(canvas);
                } else {
                    let mut canvas = img.to_rgba8();
                    let color = Rgba([255, 255, 255, (255.0 * watermark_opacity) as u8]);
                    draw_text_mut(&mut canvas, color, x, y, scale, font, &watermark_text);
                    img = DynamicImage::ImageRgba8(canvas);
                }
            }
        }
        animation_export_checkpoint(&job_token, "la composicion del frame")?;

        // G. Establish Canonical Format & Init FFmpeg (First Frame)
        if i == 0 && is_video {
            canonical_w = img.width();
            canonical_h = img.height();
            canonical_depth_16 = is_high_depth;

            // Use rawvideo for absolute synchronization and noise-free stream
            let pix_fmt = if canonical_depth_16 {
                "rgb48be"
            } else {
                "rgb24"
            };

            let mut args = vec![
                "-y".to_string(),
                "-f".to_string(),
                "rawvideo".to_string(),
                "-pixel_format".to_string(),
                pix_fmt.to_string(),
                "-video_size".to_string(),
                format!("{}x{}", canonical_w, canonical_h),
                "-r".to_string(),
                fps.to_string(),
                "-i".to_string(),
                "-".to_string(),
            ];

            if format == "avi" {
                args.extend_from_slice(&[
                    "-c:v".to_string(),
                    "mpeg4".to_string(),
                    "-q:v".to_string(),
                    "2".to_string(),
                ]);
            } else {
                let (crf, preset) = match quality_preset.as_str() {
                    "faster" => ("28", "faster"),
                    "balanced" => ("20", "medium"),
                    "quality" | _ => ("10", "slow"),
                };
                args.extend_from_slice(&[
                    "-c:v".to_string(),
                    "libx264".to_string(),
                    "-pix_fmt".to_string(),
                    "yuv420p".to_string(),
                    "-crf".to_string(),
                    crf.to_string(),
                    "-preset".to_string(),
                    preset.to_string(),
                    "-color_primaries".to_string(),
                    "bt709".to_string(),
                    "-color_trc".to_string(),
                    "bt709".to_string(),
                    "-colorspace".to_string(),
                    "bt709".to_string(),
                ]);
            }

            args.extend_from_slice(&[
                "-vf".to_string(),
                "scale=trunc(iw/2)*2:trunc(ih/2)*2".to_string(),
            ]);
            args.push(clean_windows_path(staged_export.temporary_path.clone()));

            let mut command = Command::new(&ffmpeg_cmd);
            #[cfg(target_os = "windows")]
            command.creation_flags(0x08000000);
            command.args(&args);
            ffmpeg_process = Some(AnimationFfmpegProcess::spawn(command)?);
        }

        // H. Output (Pipe or GIF)
        if is_video {
            let process = ffmpeg_process
                .as_mut()
                .ok_or_else(|| "FFmpeg no fue inicializado".to_string())?;
            // Send RAW pixels to FFmpeg. Every broken-pipe/write failure is
            // fatal; the process guard performs kill+wait during unwinding.
            if canonical_depth_16 {
                // rgb48be (16-bit Big-Endian RGB)
                let rgb = img.to_rgb16();
                let raw = rgb.as_raw();
                let mut be_bytes = Vec::with_capacity(raw.len() * 2);
                for &v in raw {
                    be_bytes.extend_from_slice(&v.to_be_bytes());
                }
                process.write_frame(be_bytes, &job_token)?;
            } else {
                // rgb24 (8-bit RGB)
                let rgb = img.to_rgb8();
                process.write_frame(rgb.into_raw(), &job_token)?;
            }
        } else {
            if let Some(enc) = &mut gif_encoder {
                let frame = Frame::from_parts(
                    img.to_rgba8(),
                    0,
                    0,
                    Delay::from_numer_denom_ms(delay_ms as u32, 1),
                );
                enc.encode_frame(frame)
                    .map_err(|error| format!("Error codificando un frame GIF: {error}"))?;
            } else {
                return Err("El codificador GIF no fue inicializado".into());
            }
        }
        animation_export_checkpoint(&job_token, "la escritura de frames")?;
    } // End Loop

    animation_export_checkpoint(&job_token, "la finalizacion del codificador")?;

    // Finalize encoder. FFmpeg's pipe must close before wait; GIF's trailer is
    // emitted by Drop, whose I/O panic is translated into a command error.
    if is_video {
        ffmpeg_process
            .take()
            .ok_or_else(|| "FFmpeg no produjo ningun frame".to_string())?
            .finish(&job_token)?;
    } else {
        let encoder = gif_encoder
            .take()
            .ok_or_else(|| "El codificador GIF no produjo ningun frame".to_string())?;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(encoder)))
            .map_err(|_| "Error finalizando el archivo GIF".to_string())?;
    }

    animation_export_checkpoint(&job_token, "la sincronizacion de la salida")?;
    staged_export.sync_payload()?;
    with_current_planetary_job(
        &state.planetary_generation_gate,
        &job_token,
        "la publicacion de la animacion",
        || staged_export.publish(),
    )??;

    emit_progress(&app, "ExportaciÃ³n Finalizada", 100.0, None);
    Ok(clean_windows_path(out_file))
}

#[cfg(test)]
mod animation_export_atomic_tests {
    use super::{
        animation_export_checkpoint, drain_animation_ffmpeg_stderr, PlanetaryJobToken,
        StagedAnimationExport, ANIMATION_FFMPEG_STDERR_LIMIT,
    };
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    fn temporary_directory(label: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "zas_animation_export_{label}_{}_{}",
            std::process::id(),
            super::ANIMATION_EXPORT_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn ffmpeg_stderr_drain_is_bounded_and_keeps_tail() {
        let mut input = vec![b'a'; ANIMATION_FFMPEG_STDERR_LIMIT + 97];
        let tail = input.len() - 4;
        input[tail..].copy_from_slice(b"TAIL");
        let retained = drain_animation_ffmpeg_stderr(std::io::Cursor::new(input));
        assert_eq!(retained.len(), ANIMATION_FFMPEG_STDERR_LIMIT);
        assert!(retained.ends_with(b"TAIL"));
    }

    #[test]
    fn unpublished_animation_staging_rolls_back_on_drop() {
        let directory = temporary_directory("rollback");
        let (staged, mut file) = StagedAnimationExport::reserve(&directory, "gif").unwrap();
        let temporary_path = staged.temporary_path.clone();
        let final_path = staged.final_path.clone();
        file.write_all(b"GIF89a;").unwrap();
        drop(file);
        drop(staged);
        assert!(!temporary_path.exists());
        assert!(!final_path.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn animation_publication_is_a_single_rename() {
        let directory = temporary_directory("publish");
        let (mut staged, mut file) = StagedAnimationExport::reserve(&directory, "gif").unwrap();
        let temporary_path = staged.temporary_path.clone();
        let final_path = staged.final_path.clone();
        file.write_all(b"GIF89a;").unwrap();
        file.sync_all().unwrap();
        drop(file);
        staged.sync_payload().unwrap();
        staged.publish().unwrap();
        assert!(!temporary_path.exists());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"GIF89a;");
        drop(staged);
        assert!(final_path.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn incomplete_gif_is_never_publishable() {
        let directory = temporary_directory("gif_trailer");
        let (staged, mut file) = StagedAnimationExport::reserve(&directory, "gif").unwrap();
        let temporary_path = staged.temporary_path.clone();
        file.write_all(b"GIF89a").unwrap();
        drop(file);
        assert!(staged.sync_payload().is_err());
        drop(staged);
        assert!(!temporary_path.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn superseded_animation_generation_cannot_reach_publication() {
        let active = Arc::new(AtomicUsize::new(17));
        let cancelled = Arc::new(AtomicBool::new(false));
        let token = PlanetaryJobToken::for_test(17, active.clone(), cancelled);
        assert!(animation_export_checkpoint(&token, "el test").is_ok());
        active.store(18, Ordering::Release);
        assert!(animation_export_checkpoint(&token, "la publicacion").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_animation_pipe_kills_and_reaps_encoder_promptly() {
        let mut command = std::process::Command::new("/bin/sleep");
        command.arg("30");
        let mut process = super::AnimationFfmpegProcess::spawn(command).unwrap();
        let token = PlanetaryJobToken::for_test(
            23,
            Arc::new(AtomicUsize::new(23)),
            Arc::new(AtomicBool::new(true)),
        );
        let started = std::time::Instant::now();
        assert!(process.write_frame(vec![0u8; 4096], &token).is_err());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "la cancelacion de una tuberia bloqueada no fue inmediata"
        );
    }
}

fn robust_image_peak_u16(image: &DynamicImage) -> f32 {
    // Un histograma fijo sustituye el Vec<f32>+sort de todos los píxeles. La
    // conversión temporal es de una sola imagen y el muestreo queda acotado a
    // ~2 Mpx, por lo que una secuencia 8K no se precarga ni ordena en RAM.
    let luma = image.to_luma16();
    let total_pixels = luma.len().max(1);
    let step = total_pixels.saturating_add(1_999_999) / 2_000_000;
    let mut histogram = vec![0u32; 65_536];
    let mut sampled = 0usize;
    for sample in luma.as_raw().iter().step_by(step.max(1)) {
        histogram[*sample as usize] += 1;
        sampled += 1;
    }
    if sampled == 0 {
        return 0.0;
    }
    let wanted = (sampled / 1000).max(1).min(1000).min(sampled);
    let mut remaining = wanted;
    let mut sum = 0u64;
    for value in (0..histogram.len()).rev() {
        let take = remaining.min(histogram[value] as usize);
        sum = sum.saturating_add((value as u64).saturating_mul(take as u64));
        remaining -= take;
        if remaining == 0 {
            break;
        }
    }
    sum as f32 / wanted as f32
}

fn apply_image_gain_preserving_layout(image: &mut DynamicImage, gain: f32) {
    let gain8 = |value: u8| (value as f32 * gain).round().clamp(0.0, 255.0) as u8;
    let gain16 = |value: u16| (value as f32 * gain).round().clamp(0.0, 65_535.0) as u16;
    match image {
        DynamicImage::ImageLuma8(buffer) => buffer.as_mut().iter_mut().for_each(|v| *v = gain8(*v)),
        DynamicImage::ImageLumaA8(buffer) => buffer
            .as_mut()
            .chunks_exact_mut(2)
            .for_each(|px| px[0] = gain8(px[0])),
        DynamicImage::ImageRgb8(buffer) => buffer
            .as_mut()
            .iter_mut()
            .for_each(|v| *v = gain8(*v)),
        DynamicImage::ImageBgr8(buffer) => buffer
            .as_mut()
            .iter_mut()
            .for_each(|v| *v = gain8(*v)),
        DynamicImage::ImageRgba8(buffer) => buffer
            .as_mut()
            .chunks_exact_mut(4)
            .for_each(|px| px[..3].iter_mut().for_each(|v| *v = gain8(*v))),
        DynamicImage::ImageBgra8(buffer) => buffer
            .as_mut()
            .chunks_exact_mut(4)
            .for_each(|px| px[..3].iter_mut().for_each(|v| *v = gain8(*v))),
        DynamicImage::ImageLuma16(buffer) => buffer.as_mut().iter_mut().for_each(|v| *v = gain16(*v)),
        DynamicImage::ImageLumaA16(buffer) => buffer
            .as_mut()
            .chunks_exact_mut(2)
            .for_each(|px| px[0] = gain16(px[0])),
        DynamicImage::ImageRgb16(buffer) => buffer
            .as_mut()
            .iter_mut()
            .for_each(|v| *v = gain16(*v)),
        DynamicImage::ImageRgba16(buffer) => buffer
            .as_mut()
            .chunks_exact_mut(4)
            .for_each(|px| px[..3].iter_mut().for_each(|v| *v = gain16(*v))),
    }
}

/// Rename a directory only if the destination does not exist.  POSIX rename
/// normally replaces an existing empty directory, which is unsafe for an
/// output transaction even when names contain a random suffix.
fn portable_unique_directory_rename(
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    if destination.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "el destino ya existe",
        ));
    }
    // This path is used only when the filesystem rejects the platform's
    // no-replace extension (notably macOS exFAT).  The destination contains
    // PID+clock+sequence, so it is private to this single-instance process;
    // the rename itself remains atomic and never crosses filesystems.
    std::fs::rename(source, destination)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn no_replace_extension_is_unsupported(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
    ) || error.raw_os_error().is_some_and(|code| {
        code == libc::EINVAL || code == libc::ENOSYS || code == libc::ENOTSUP
    })
}

#[cfg(target_os = "linux")]
fn rename_directory_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source_c = CString::new(source.as_os_str().as_bytes())?;
    let destination_c = CString::new(destination.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source_c.as_ptr(),
            libc::AT_FDCWD,
            destination_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if no_replace_extension_is_unsupported(&error) {
            portable_unique_directory_rename(source, destination)
        } else {
            Err(error)
        }
    }
}

#[cfg(target_os = "macos")]
fn rename_directory_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source_c = CString::new(source.as_os_str().as_bytes())?;
    let destination_c = CString::new(destination.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::renamex_np(
            source_c.as_ptr(),
            destination_c.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if no_replace_extension_is_unsupported(&error) {
            portable_unique_directory_rename(source, destination)
        } else {
            Err(error)
        }
    }
}

#[cfg(any(target_os = "windows", not(any(target_os = "linux", target_os = "macos"))))]
fn rename_directory_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    // MoveFile/MoveFileEx without MOVEFILE_REPLACE_EXISTING is exclusive on
    // Windows.  The preflight is retained for less common targets.
    portable_unique_directory_rename(source, destination)
}

/// Multi-file outputs are encoded and fsynced below a hidden sibling
/// directory.  One exclusive directory rename publishes the complete set;
/// cancellation or any encode error removes the entire hidden tree.
struct StagedPlanetaryDirectory {
    temporary_dir: PathBuf,
    final_dir: PathBuf,
    published: bool,
}

impl StagedPlanetaryDirectory {
    fn create(parent: &Path, tag: &str) -> Result<Self, String> {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        for _ in 0..64 {
            let suffix = derot_unique_suffix();
            let final_dir = parent.join(format!("{tag}_{suffix}"));
            let temporary_dir = parent.join(format!(".{tag}_{suffix}.tmp"));
            if final_dir.exists() {
                continue;
            }
            match std::fs::create_dir(&temporary_dir) {
                Ok(()) => {
                    return Ok(Self {
                        temporary_dir,
                        final_dir,
                        published: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
        Err(format!("No se pudo reservar una carpeta de salida para {tag}"))
    }

    fn temporary_path(&self, file_name: &str) -> PathBuf {
        self.temporary_dir.join(file_name)
    }

    fn final_path(&self, file_name: &str) -> PathBuf {
        self.final_dir.join(file_name)
    }

    fn publish(&mut self) -> Result<(), String> {
        if self.final_dir.exists() {
            return Err(format!(
                "La carpeta de salida ya existe y no se sobrescribira: {}",
                self.final_dir.display()
            ));
        }
        // Cada PNG/TIFF ya fue flush+fsync. El fsync del directorio es una
        // barrera adicional en plataformas que lo soportan, pero exFAT y
        // algunos volúmenes de red devuelven EINVAL/ENOTSUP. No debe invalidar
        // horas de cómputo cuando la publicación atómica por rename sí es viable.
        // En macOS, abrir una carpeta gestionada por File Provider (por ejemplo
        // Escritorio/iCloud) puede bloquear indefinidamente dentro de `open(2)`.
        // Los archivos ya fueron flush+fsync y el rename sigue siendo atómico;
        // reservamos el fsync de directorio para Unix donde esta barrera es
        // fiable y no congela el comando/UI.
        #[cfg(all(unix, not(target_os = "macos")))]
        if let Ok(directory) = File::open(&self.temporary_dir) {
            let _ = directory.sync_all();
        }
        rename_directory_no_replace(&self.temporary_dir, &self.final_dir)
            .map_err(|error| format!("No se pudo publicar el lote completo: {error}"))?;
        sync_parent_directory(&self.final_dir);
        self.published = true;
        Ok(())
    }
}

impl Drop for StagedPlanetaryDirectory {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_dir_all(&self.temporary_dir);
        }
    }
}

#[cfg(test)]
mod planetary_directory_transaction_tests {
    use super::{StagedPlanetaryArtifact, StagedPlanetaryDirectory};

    fn test_parent(label: &str) -> std::path::PathBuf {
        let parent = std::env::temp_dir().join(format!(
            "zas_planetary_transaction_{label}_{}_{}",
            std::process::id(),
            super::ANIMATION_EXPORT_SEQUENCE
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&parent).unwrap();
        parent
    }

    #[test]
    fn incomplete_multi_file_output_is_completely_rolled_back() {
        let parent = test_parent("rollback");
        let staged = StagedPlanetaryDirectory::create(&parent, "Mosaic_Result").unwrap();
        let temporary = staged.temporary_dir.clone();
        let final_dir = staged.final_dir.clone();
        std::fs::write(staged.temporary_path("master.tiff"), b"master").unwrap();
        drop(staged);
        assert!(!temporary.exists());
        assert!(!final_dir.exists());
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn multi_file_output_appears_with_one_directory_commit() {
        let parent = test_parent("publish");
        let mut staged = StagedPlanetaryDirectory::create(&parent, "Mosaic_Result").unwrap();
        let temporary = staged.temporary_dir.clone();
        let final_dir = staged.final_dir.clone();
        std::fs::write(staged.temporary_path("master.tiff"), b"master").unwrap();
        std::fs::write(staged.temporary_path("preview.png"), b"preview").unwrap();
        assert!(!final_dir.exists());
        staged.publish().unwrap();
        assert!(!temporary.exists());
        assert_eq!(std::fs::read(final_dir.join("master.tiff")).unwrap(), b"master");
        assert_eq!(std::fs::read(final_dir.join("preview.png")).unwrap(), b"preview");
        drop(staged);
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn rgb16_prepared_and_linear_master_publish_as_one_pair() {
        let parent = test_parent("rgb16-pair");
        let mut pair = StagedPlanetaryDirectory::create(&parent, "Stack_Jupiter").unwrap();
        let rgb = vec![
            100u16, 200, 300, 400, 500, 600,
            700, 800, 900, 1_000, 1_100, 1_200,
        ];
        let prepared_name = "Jupiter_Prepared_RGB16.png";
        let master_name = "Jupiter_Linear_Master_RGB16.tiff";
        let prepared_final = pair.final_path(prepared_name);
        let master_final = pair.final_path(master_name);
        let mut prepared = StagedPlanetaryArtifact::encode_rgb16_png(
            pair.temporary_path(prepared_name),
            &rgb,
            2,
            2,
        )
        .unwrap();
        prepared.publish(false).unwrap();
        let mut master = StagedPlanetaryArtifact::encode_rgb16_tiff(
            pair.temporary_path(master_name),
            &rgb,
            2,
            2,
        )
        .unwrap();
        master.publish(false).unwrap();
        assert!(!prepared_final.exists());
        assert!(!master_final.exists());

        pair.publish().unwrap();

        assert_eq!(image::open(&prepared_final).unwrap().to_rgb16().as_raw(), &rgb);
        assert_eq!(image::open(&master_final).unwrap().to_rgb16().as_raw(), &rgb);
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn optional_external_volume_publishes_the_complete_pair() {
        let Some(root) = std::env::var_os("ZAS_BATCH_EXTERNAL_TEST_ROOT") else {
            return;
        };
        let parent = std::path::PathBuf::from(root).join(format!(
            ".zas-batch-publish-test-{}-{}",
            std::process::id(),
            super::ANIMATION_EXPORT_SEQUENCE
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&parent).unwrap();
        let mut pair = StagedPlanetaryDirectory::create(&parent, "Stack_External").unwrap();
        std::fs::write(pair.temporary_path("prepared.png"), b"prepared").unwrap();
        std::fs::write(pair.temporary_path("master.tiff"), b"master").unwrap();
        let final_dir = pair.final_dir.clone();
        pair.publish().unwrap();
        assert_eq!(std::fs::read(final_dir.join("prepared.png")).unwrap(), b"prepared");
        assert_eq!(std::fs::read(final_dir.join("master.tiff")).unwrap(), b"master");
        std::fs::remove_dir_all(parent).unwrap();
    }
}

fn safe_sequence_stem(source: &Path) -> String {
    let stem = source
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("frame");
    let sanitized: String = stem
        .chars()
        .take(80)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "frame".to_string()
    } else {
        sanitized
    }
}

fn encode_sequence_png(
    directory: &Path,
    source: &Path,
    index: usize,
    tag: &str,
    image: &DynamicImage,
) -> Result<String, String> {
    // PNG conserva L/LA/RGB/RGBA y 8/16-bit sin cuantización JPEG.  El
    // índice hace el nombre único aunque dos fuentes compartan stem.
    let file_name = format!(
        "{:06}_{}_{}.png",
        index.saturating_add(1),
        safe_sequence_stem(source),
        tag
    );
    let path = directory.join(&file_name);
    if path.exists() {
        return Err(format!(
            "El archivo temporal de secuencia ya existe: {}",
            path.display()
        ));
    }
    image.save(&path).map_err(|error| error.to_string())?;
    File::open(&path)
        .and_then(|file| file.sync_all())
        .map_err(|error| error.to_string())?;
    Ok(file_name)
}

#[tauri::command]
async fn normalize_batch_brightness(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> Result<Vec<String>, String> {
    state.license_manager.check_access()?;
    if paths.is_empty() {
        return Err("No hay imagenes para normalizar".into());
    }
    let request_id = begin_planetary_user_job(&state);
    let job_token = PlanetaryJobToken::for_app(&app, request_id, state.cancel_requested.clone());
    emit_progress(&app, "Midiendo brillo...", 0.0, None);

    let mut brightness_values = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        planetary_derotation_checkpoint(&job_token, "la medicion de brillo")?;
        let image = image::open(path)
            .map_err(|error| format!("No se pudo abrir {}: {error}", path))?;
        brightness_values.push(robust_image_peak_u16(&image));
        emit_progress(
            &app,
            "Midiendo brillo...",
            45.0 * (index + 1) as f32 / paths.len() as f32,
            None,
        );
    }
    let mut finite_peaks: Vec<f32> = brightness_values
        .iter()
        .copied()
        .filter(|value| value.is_finite() && *value > 10.0)
        .collect();
    if finite_peaks.is_empty() {
        return Err("Las imagenes no contienen señal suficiente para normalizar".into());
    }
    let middle = finite_peaks.len() / 2;
    finite_peaks.select_nth_unstable_by(middle, |a, b| {
        a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
    });
    let target_peak = finite_peaks[middle];

    let output_parent = Path::new(&paths[0])
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let mut staged_directory =
        StagedPlanetaryDirectory::create(output_parent, "Zenith_Normalized")?;
    let mut staged_names = Vec::with_capacity(paths.len());
    for (index, (path, current_peak)) in paths.iter().zip(brightness_values).enumerate() {
        planetary_derotation_checkpoint(&job_token, "el ajuste de brillo")?;
        if !current_peak.is_finite() || current_peak <= 10.0 {
            return Err(format!("{} no contiene señal suficiente", path));
        }
        let mut image = image::open(path)
            .map_err(|error| format!("No se pudo reabrir {}: {error}", path))?;
        let gain = (target_peak / current_peak).clamp(0.25, 4.0);
        if (gain - 1.0).abs() > 0.005 {
            apply_image_gain_preserving_layout(&mut image, gain);
        }
        staged_names.push(encode_sequence_png(
            &staged_directory.temporary_dir,
            Path::new(path),
            index,
            "Normalized",
            &image,
        )?);
        emit_progress(
            &app,
            "Codificando copias normalizadas...",
            45.0 + 50.0 * (index + 1) as f32 / paths.len() as f32,
            None,
        );
    }

    let results = with_current_planetary_job(
        &state.planetary_generation_gate,
        &job_token,
        "la publicacion del lote normalizado",
        || -> Result<Vec<String>, String> {
            let results = staged_names
                .iter()
                .map(|name| clean_windows_path(staged_directory.final_path(name)))
                .collect();
            staged_directory.publish()?;
            Ok(results)
        },
    )??;
    emit_progress(&app, "Normalizacion completa", 100.0, None);
    Ok(results)
}

#[cfg(test)]
mod brightness_normalization_tests {
    use super::{apply_image_gain_preserving_layout, robust_image_peak_u16};
    use image::{DynamicImage, ImageBuffer, LumaA, Rgba};

    #[test]
    fn gain_preserves_16_bit_lumaa_and_alpha() {
        let buffer = ImageBuffer::from_pixel(2, 1, LumaA([10_000u16, 42_000u16]));
        let mut image = DynamicImage::ImageLumaA16(buffer);
        apply_image_gain_preserving_layout(&mut image, 2.0);
        let pixel = image.as_luma_alpha16().unwrap().get_pixel(0, 0);
        assert_eq!(pixel.0, [20_000, 42_000]);
    }

    #[test]
    fn gain_preserves_rgba16_and_alpha() {
        let buffer = ImageBuffer::from_pixel(1, 1, Rgba([1_000u16, 2_000, 3_000, 55_000]));
        let mut image = DynamicImage::ImageRgba16(buffer);
        apply_image_gain_preserving_layout(&mut image, 1.5);
        assert_eq!(image.as_rgba16().unwrap().get_pixel(0, 0).0, [1_500, 3_000, 4_500, 55_000]);
    }

    #[test]
    fn histogram_peak_uses_bright_tail_without_pixel_sort() {
        let mut buffer = image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_pixel(
            100,
            100,
            image::Luma([100u16]),
        );
        for x in 0..10 {
            buffer.put_pixel(x, 0, image::Luma([50_000]));
        }
        assert_eq!(robust_image_peak_u16(&DynamicImage::ImageLuma16(buffer)), 50_000.0);
    }
}

fn animation_alignment_mono_u16(image: &DynamicImage) -> Result<Vec<u16>, String> {
    let width = image.width() as usize;
    let height = image.height() as usize;
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| "Frame de animacion demasiado grande".to_string())?;
    let rgb = image.to_rgb16();
    let mut mono = Vec::new();
    mono.try_reserve_exact(pixels)
        .map_err(|error| format!("Memoria insuficiente para alinear el frame: {error}"))?;
    mono.extend(rgb.pixels().map(|pixel| {
        let value = pixel[0] as u32 * 299 + pixel[1] as u32 * 587 + pixel[2] as u32 * 114;
        ((value + 500) / 1000) as u16
    }));
    Ok(mono)
}

fn bilinear_shift_animation_frame(
    image: &DynamicImage,
    dx: f32,
    dy: f32,
    job_token: &PlanetaryJobToken,
) -> Result<DynamicImage, String> {
    let width = image.width();
    let height = image.height();
    let is_16bit = matches!(
        image,
        DynamicImage::ImageLuma16(_)
            | DynamicImage::ImageLumaA16(_)
            | DynamicImage::ImageRgb16(_)
            | DynamicImage::ImageRgba16(_)
    );

    macro_rules! interpolate {
        ($source:expr, $output:expr, $sample:ty, $max:expr) => {{
            for y_out in 0..height {
                if y_out % 32 == 0 {
                    planetary_derotation_checkpoint(job_token, "el remuestreo de la animacion")?;
                }
                for x_out in 0..width {
                    let src_x = x_out as f32 + dx;
                    let src_y = y_out as f32 + dy;
                    if src_x < 0.0
                        || src_x >= width.saturating_sub(1) as f32
                        || src_y < 0.0
                        || src_y >= height.saturating_sub(1) as f32
                    {
                        continue;
                    }
                    let x0 = src_x.floor() as u32;
                    let y0 = src_y.floor() as u32;
                    let wx = src_x - x0 as f32;
                    let wy = src_y - y0 as f32;
                    let p00 = $source.get_pixel(x0, y0);
                    let p10 = $source.get_pixel(x0 + 1, y0);
                    let p01 = $source.get_pixel(x0, y0 + 1);
                    let p11 = $source.get_pixel(x0 + 1, y0 + 1);
                    let mut pixel = image::Rgb([0 as $sample; 3]);
                    for channel in 0..3 {
                        let top = p00[channel] as f32 * (1.0 - wx) + p10[channel] as f32 * wx;
                        let bottom = p01[channel] as f32 * (1.0 - wx) + p11[channel] as f32 * wx;
                        pixel[channel] = (top * (1.0 - wy) + bottom * wy)
                            .round()
                            .clamp(0.0, $max) as $sample;
                    }
                    $output.put_pixel(x_out, y_out, pixel);
                }
            }
        }};
    }

    if is_16bit {
        let source = image.to_rgb16();
        let mut shifted = image::ImageBuffer::<image::Rgb<u16>, Vec<u16>>::new(width, height);
        interpolate!(source, shifted, u16, 65_535.0);
        Ok(DynamicImage::ImageRgb16(shifted))
    } else {
        // Una sola conversión por frame. La implementación anterior ejecutaba
        // to_rgb8() dentro del bucle de cada píxel (O(P²) copias de memoria).
        let source = image.to_rgb8();
        let mut shifted = image::RgbImage::new(width, height);
        interpolate!(source, shifted, u8, 255.0);
        Ok(DynamicImage::ImageRgb8(shifted))
    }
}

#[cfg(test)]
mod animation_realign_tests {
    use super::{
        animation_alignment_mono_u16, bilinear_shift_animation_frame, PlanetaryJobToken,
    };
    use image::{DynamicImage, ImageBuffer, Rgb};
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::Arc;

    fn token(active_value: usize, request_id: usize) -> PlanetaryJobToken {
        PlanetaryJobToken::for_test(
            request_id,
            Arc::new(AtomicUsize::new(active_value)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn alignment_analysis_preserves_native_sixteen_bit_luminance() {
        let image = DynamicImage::ImageRgb16(ImageBuffer::from_pixel(
            2,
            2,
            Rgb([10_000u16, 20_000, 30_000]),
        ));
        let mono = animation_alignment_mono_u16(&image).unwrap();
        assert_eq!(mono, vec![18_150u16; 4]);
    }

    #[test]
    fn bilinear_realign_is_cancelable_and_keeps_sixteen_bit_samples() {
        let mut source = ImageBuffer::<Rgb<u16>, Vec<u16>>::new(3, 3);
        for y in 0..3 {
            for x in 0..3 {
                source.put_pixel(x, y, Rgb([1_000 + x as u16, 2_000 + y as u16, 3_000]));
            }
        }
        let image = DynamicImage::ImageRgb16(source);
        let aligned = bilinear_shift_animation_frame(&image, 0.0, 0.0, &token(4, 4)).unwrap();
        assert_eq!(aligned.to_rgb16().get_pixel(1, 1).0, [1_001, 2_001, 3_000]);
        assert!(bilinear_shift_animation_frame(&image, 0.0, 0.0, &token(5, 4)).is_err());
    }
}

#[tauri::command]
async fn realign_animation_frames(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    paths: Vec<String>,
    mode_type: String,                                // "planetary" or "surface"
    custom_roi: Option<(usize, usize, usize, usize)>, // NEW: User selected ROI (x, y, w, h)
) -> Result<Vec<String>, String> {
    state.license_manager.check_access()?;
    if !matches!(mode_type.as_str(), "planetary" | "surface") {
        return Err(format!("Modo de alineacion no compatible: {mode_type}"));
    }
    let request_id = begin_planetary_user_job(&state);
    let job_token = PlanetaryJobToken::for_app(&app, request_id, state.cancel_requested.clone());

    #[cfg(target_arch = "x86_64")]
    let use_avx2 = is_x86_feature_detected!("avx2");
    #[cfg(not(target_arch = "x86_64"))]
    let use_avx2 = false;

    let start_msg = if use_avx2 {
        "Alineando frames (AVX2)..."
    } else {
        "Alineando frames..."
    };

    emit_progress(&app, start_msg, 0.0, None);

    if paths.is_empty() {
        return Err("No hay frames para alinear".into());
    }

    // 1. Cargar Anchor (Primer Frame)
    let p0 = Path::new(&paths[0]);
    let img0 = image::open(p0).map_err(|e| e.to_string())?;
    let w = img0.width() as usize;
    let h = img0.height() as usize;
    if w < 2 || h < 2 {
        return Err("Los frames deben medir al menos 2x2 pixeles".into());
    }

    // Convertir anchor a mono u16 para SAD
    // Convertir anchor a mono u16 para SAD
    let anchor_mono = {
        let m = animation_alignment_mono_u16(&img0)?;
        if mode_type == "surface" {
            enhance_solar_surface(&m, w, h)
        } else {
            enhance_for_alignment(&m, w, h)
        }
    };

    // PYRAMID FOR BATCH PREVIEW
    // Configurar parametros segun modo para el anchor tambien
    let scale_factor = if mode_type == "surface" { 2 } else { 4 };
    let anchor_pyramid = downscale_integer(&anchor_mono, w, h, scale_factor);

    // ROI SETUP
    let (roi_x, roi_y, roi_w, roi_h) = if let Some((cx, cy, cw, ch)) = custom_roi {
        // User Defined ROI
        (cx, cy, cw, ch)
    } else {
        // Default Logic
        let (rw, rh) = if mode_type == "surface" {
            ((w as f32 * 0.40) as usize, (h as f32 * 0.40) as usize) // Increased to 40%
        } else {
            ((w as f32 * 0.70) as usize, (h as f32 * 0.70) as usize)
        };
        let rx = w.saturating_sub(rw) / 2;
        let ry = h.saturating_sub(rh) / 2;
        (rx, ry, rw, rh)
    };

    // Bounds check for safety
    let roi_w = roi_w.min(w);
    let roi_h = roi_h.min(h);
    if roi_w == 0 || roi_h == 0 {
        return Err("El ROI de alineacion esta vacio".into());
    }
    let stab_x = roi_x.min(w - roi_w);
    let stab_y = roi_y.min(h - roi_h);

    emit_progress(&app, "Analizando movimientos...", 10.0, None);

    // 2. Procesar con concurrencia acotada por la RAM disponible. Cada worker
    // conserva fuente+mono+realzado+salida; evitar un worker por núcleo es vital
    // con masters lunares de decenas o cientos de megapíxeles.
    let pixels = w
        .checked_mul(h)
        .ok_or_else(|| "Frame de animacion demasiado grande".to_string())?;
    // Peor caso 16-bit: DynamicImage (6-8 B/px), RGB16 de análisis (6),
    // mono+realzado (4), pirámide, salida RGB16 (6) y margen del decoder.
    let per_worker_bytes = (pixels as u64)
        .saturating_mul(28)
        .saturating_add(32 * 1024 * 1024);
    let mut system = System::new();
    system.refresh_memory();
    let available = system.available_memory();
    let reserve = (available / 5)
        .clamp(256 * 1024 * 1024, 1024 * 1024 * 1024)
        .min(available / 2);
    let safe_workers = available
        .saturating_sub(reserve)
        .checked_div(per_worker_bytes.max(1))
        .unwrap_or(1)
        .max(1) as usize;
    let worker_count = safe_workers
        .min(paths.len())
        .min(rayon::current_num_threads().max(1));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_count)
        .thread_name(|index| format!("planetary-animation-align-{index}"))
        .build()
        .map_err(|error| format!("No se pudo crear el pool de alineacion: {error}"))?;

    let output_parent = Path::new(&paths[0])
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let mut staged_directory =
        StagedPlanetaryDirectory::create(output_parent, "Zenith_Aligned")?;
    let staging_temp = staged_directory.temporary_dir.clone();

    let staged_result: Result<Vec<String>, String> = pool.install(|| {
        paths
            .par_iter()
            .enumerate()
            .map(|(i, p)| {
                planetary_derotation_checkpoint(&job_token, "la alineacion de animacion")?;

            let path = Path::new(p);
            let img = image::open(path).map_err(|e| e.to_string())?;

            if img.width() as usize != w || img.height() as usize != h {
                return Err("Dimensiones inconsistentes detectadas en los frames de la animaciÃ³n. AsegÃºrate de que todas las imÃ¡genes tengan el mismo tamaÃ±o o recÃ³rtalas.".to_string());
            }

            if i == 0 {
                return encode_sequence_png(&staging_temp, path, i, "Aligned", &img);
            }

            // Convert to mono for alignment analysis only (doesn't change original img)
            let current_mono = {
                let m = animation_alignment_mono_u16(&img)?;

                if mode_type == "surface" {
                    enhance_solar_surface(&m, w, h)
                } else {
                    enhance_for_alignment(&m, w, h)
                }
            };

            // Configurar parametros segun modo
            let (scale_factor, fine_range) = if mode_type == "surface" {
                (2, 64) // Surface: Increased range
            } else {
                (4, 4) // Planetary
            };

            // Buscar desplazamiento
            let (dx, dy) = find_best_match_sad_pyramid(
                &anchor_mono,
                &current_mono,
                &anchor_pyramid,
                w,
                h,
                w / scale_factor,
                h / scale_factor,
                stab_x,
                stab_y,
                roi_w,
                roi_h,
                512, // Search Range
                scale_factor,
                fine_range,
            );

            let final_img = bilinear_shift_animation_frame(&img, dx, dy, &job_token)?;
            encode_sequence_png(&staging_temp, path, i, "Aligned", &final_img)
        })
        .collect()
    });
    let staged_names = staged_result?;
    emit_progress(&app, "Publicando alineacion...", 95.0, None);
    let results = with_current_planetary_job(
        &state.planetary_generation_gate,
        &job_token,
        "la publicacion de la animacion alineada",
        || -> Result<Vec<String>, String> {
            let results = staged_names
                .iter()
                .map(|name| clean_windows_path(staged_directory.final_path(name)))
                .collect();
            staged_directory.publish()?;
            Ok(results)
        },
    )??;
    emit_progress(&app, "Alineacion completada", 100.0, None);
    Ok(results)
}

/// Center-crop or zero-pad an interleaved RGB16 image to target dimensions.
/// R13: keeps batch timelapse frames at identical canvas size even when the
/// engine's coverage auto-crop varies a few pixels between files.
/// NOTE: internal helper only (takes `&[u16]`); it is NOT a Tauri command.
fn center_crop_or_pad_rgb(src: &[u16], w: usize, h: usize, tw: usize, th: usize) -> Vec<u16> {
    let mut out = vec![0u16; tw * th * 3];
    let copy_w = w.min(tw);
    let copy_h = h.min(th);
    let src_x0 = (w - copy_w) / 2;
    let src_y0 = (h - copy_h) / 2;
    let dst_x0 = (tw - copy_w) / 2;
    let dst_y0 = (th - copy_h) / 2;
    for y in 0..copy_h {
        let s = ((src_y0 + y) * w + src_x0) * 3;
        let d = ((dst_y0 + y) * tw + dst_x0) * 3;
        out[d..d + copy_w * 3].copy_from_slice(&src[s..s + copy_w * 3]);
    }
    out
}

/// Contrato único para los ajustes que dependen de las propiedades del máster.
/// Preview, exportación y lote deben resolver exactamente lo mismo: un cero
/// introducido por el usuario es identidad, nunca una orden implícita de aplicar
/// un preset. El único ajuste automático es neutralizar WB en un máster mono,
/// donde los multiplicadores R/B no tienen significado cromático.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ResolvedPostProcessingContract {
    usm_amount: f32,
    usm_radius: f32,
    lce_amount: f32,
    r_bal: f32,
    b_bal: f32,
}

fn resolve_post_processing_contract(
    is_mono: bool,
    usm_amount: f32,
    usm_radius: f32,
    lce_amount: f32,
    r_bal: f32,
    b_bal: f32,
) -> ResolvedPostProcessingContract {
    let (r_bal, b_bal) = if is_mono { (0.0, 0.0) } else { (r_bal, b_bal) };
    ResolvedPostProcessingContract {
        usm_amount,
        usm_radius,
        lce_amount,
        r_bal,
        b_bal,
    }
}

#[cfg(test)]
mod wysiwyg_contract_tests {
    use super::resolve_post_processing_contract;

    #[test]
    fn zero_sharpening_remains_zero_for_mono_surface() {
        let resolved = resolve_post_processing_contract(true, 0.0, 0.0, 0.0, 0.35, -0.2);
        assert_eq!(resolved.usm_amount, 0.0);
        assert_eq!(resolved.usm_radius, 0.0);
        assert_eq!(resolved.lce_amount, 0.0);
        assert_eq!(resolved.r_bal, 0.0);
        assert_eq!(resolved.b_bal, 0.0);
    }

    #[test]
    fn color_master_preserves_the_user_recipe_exactly() {
        let resolved = resolve_post_processing_contract(false, 0.65, 1.25, 12.0, 0.08, -0.04);
        assert_eq!(resolved.usm_amount, 0.65);
        assert_eq!(resolved.usm_radius, 1.25);
        assert_eq!(resolved.lce_amount, 12.0);
        assert_eq!(resolved.r_bal, 0.08);
        assert_eq!(resolved.b_bal, -0.04);
    }
}

#[tauri::command]
async fn process_batch_entry(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    file_path: String,
    output_folder: String,
    stack_pct: f32,
    drizzle: f32,
    align_mode: String,
    u1: f32,
    u2: f32,
    u3: f32,
    u4: f32,
    u5: f32,
    w1: f32,
    w2: f32,
    w3: f32,
    w4: f32,
    w5: f32,
    w6: f32,
    d1: f32,
    d2: f32,
    d3: f32,
    d4: f32,
    d5: f32,
    d6: f32,
    gamma: f32,
    saturation: f32,
    r_x: f32,
    r_y: f32,
    b_x: f32,
    b_y: f32,
    deringing_mode: i32,
    deringing_radius: f32,
    deringing_dark: f32,
    deringing_light: f32,
    deringing_mask: bool,
    crisp: f32,
    deconv_iter: usize,
    deconv_sigma: f32,
    vc_iter: usize,
    vc_sigma: f32,
    usm_amount: f32,
    usm_radius: f32,
    lce_amount: f32,
    blend: f32,
    batch_mode: String,
    contrast: f32,
    brightness: f32,
    r_bal: f32,
    b_bal: f32,
    master_denoise: f32, // PHASE 23
    master_denoise_detail: f32,
    master_denoise_chroma: f32,
    use_rgb_sharpening: bool,
    bayer_override: Option<i32>,
    double_pass: bool,
    warping_analysis: bool, // NEW
    anchor_override: Option<Vec<i32>>,
    sharpened: bool,
    sharpen_intensity: f32,
    normalize_colors: bool,
    progress_prefix: Option<String>,
    is_v3: Option<bool>, // NEW
    target_type: Option<String>, // NEW
    ap_grid_size: Option<u32>,  // R13: AP size del flujo Zenith (32 por defecto)
    ap_threshold: Option<f32>,  // R13: umbral de malla del flujo Zenith
    align_rgb: Option<bool>,    // switch de alineacion RGB automatica
    gpu_mode: Option<String>,   // GPU compute: "auto" | "gpu" | "cpu"
    // Modo Pureza: "protected" (historico, por defecto) | "balanced" | "pure".
    // Controla la fuerza de las protecciones automaticas (altas luces, croma,
    // frenos de deconvolucion, suelo de denoise, rodillas de USM).
    purity_mode: Option<String>,
    compute_policy: Option<String>, // contrato nuevo; gpu_mode queda legado
    decode_policy: Option<String>,  // FFmpeg: auto/software/hardware
    quality_policy: Option<String>, // rigor AP: adaptive/standard/maximum
    sequence_plan: Option<PlanetarySequencePlan>,
    normalized_ap_points: Option<Vec<NormalizedBatchApPoint>>,
    edge_aware_wavelets: Option<bool>, // B: wavelets edge-aware
    psf_from_limb: Option<bool>,       // A: deconv con PSF medida
    edge_aware_strength: Option<f32>,  // B+: intensidad edge-aware (0..100)
    auto_mask: Option<f32>,            // Calidad: sharpening adaptativo por SNR
    adaptive_usm: Option<AdaptiveUsmParams>, // USM adaptativo por luminancia de entrada
    levels_black: Option<f32>,         // Niveles: punto negro (0..1)
    levels_white: Option<f32>,         // Niveles: punto blanco (0..1)
    levels_gamma: Option<f32>,         // Niveles: gamma medios (0.1..5)
    advanced: Option<AdvancedColorParams>,
) -> Result<BatchEntryResult, String> {
    state.license_manager.check_access()?;
    // El flag se rearma una sola vez al iniciar la sesión mediante
    // clear_app_memory. Nunca se limpia por entrada: así una cancelación
    // solicitada entre dos vídeos no puede perderse antes del siguiente.
    if state
        .cancel_requested
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Err("Cancelado por el usuario".to_string());
    }
    let target_type = target_type.unwrap_or_else(|| {
        if batch_mode.contains("surface") || batch_mode.contains("solar") {
            "surface".to_string()
        } else {
            "planet_small".to_string()
        }
    });
    let is_surface_batch =
        batch_mode.contains("surface") || batch_mode.contains("solar") || is_surface_target(&target_type);
    let warping_analysis = zenith_should_warp(&target_type, is_surface_batch, warping_analysis);
    let is_v3 = is_v3.unwrap_or_else(|| align_mode.contains("v3") || batch_mode.contains("v3"));
    let requested_quality_policy = match quality_policy
        .as_deref()
        .unwrap_or("adaptive")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "standard" => QualityPolicy::Standard,
        "maximum" => QualityPolicy::Maximum,
        _ => QualityPolicy::Adaptive,
    };
    if let Some(plan) = sequence_plan.as_ref() {
        plan.validate()?;
        if !plan.frozen || !plan.normalized_ap_geometry {
            return Err("El plan de secuencia batch debe llegar congelado y con AP normalizados."
                .into());
        }
    }
    let sequence_common_rgb_scalar = sequence_plan
        .as_ref()
        .is_some_and(|plan| plan.common_rgb_luminance_scalar);
    let sequence_max_scale_delta = sequence_plan
        .as_ref()
        .map(|plan| plan.max_scale_delta)
        .unwrap_or(0.0);
    let sequence_max_roll_degrees = sequence_plan
        .as_ref()
        .map(|plan| plan.max_roll_degrees)
        .unwrap_or(0.0);
    // Removed: let _ = (deconv_iter, deconv_sigma, vc_iter, vc_sigma);
    // Now using these parameters to apply deconvolution in batch mode
    if drizzle > 1.0 && !state.license_manager.is_pro() {
        return Err("Drizzle > 1.0 requiere licencia PRO.".into());
    }

    let path_obj = Path::new(&file_path);
    let fname = path_obj
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "frame".to_string());
    let prefix_str = progress_prefix.clone().unwrap_or_default();
    let get_msg = |msg: &str| {
        if prefix_str.is_empty() {
            msg.to_string()
        } else {
            format!("{} {}", prefix_str, msg)
        }
    };

    let r = VideoInput::open(&file_path, &app)?;
    let total = r.frame_count();
    let w = r.width();
    let h = r.height();
    let bpp = r.bpp();
    let cid = r.resolve_bayer_override(bayer_override)?;

    let ref_idx = select_signal_frame_index(&r, w, h, bpp, cid, total / 2);

    // --- CACHE & SCORE LOGIC ---
    let requested_roi = requested_analysis_roi(
        w,
        h,
        &target_type,
        is_surface_batch,
        warping_analysis,
        anchor_override.as_deref(),
    );
    let suffix = zenith_analysis_cache_suffix(
        &target_type,
        is_surface_batch,
        warping_analysis,
        cid,
        anchor_override.as_deref(),
        requested_roi,
    );
    let cache_path = get_analysis_cache_path(&file_path, &suffix);
    let source_fingerprint = planetary_source_fingerprint(&file_path)?;
    let cache_expectation = AnalysisCacheExpectation {
        source_fingerprint,
        resolved_color_id: cid,
        target_type: normalized_analysis_target(&target_type),
        is_surface: is_surface_batch,
        warping_analysis,
        anchor_override: anchor_override.clone(),
        requested_roi,
        width: w,
        height: h,
        declared_frame_count: total,
        frame_count_exact: r.frame_count_is_exact(),
    };
    let mut cached_opt: Option<CachedAnalysis> =
        load_validated_analysis_cache(&cache_path, &cache_expectation)
            .map(|(cached, _location)| cached);

    if cached_opt.is_none() {
        let analysis_request_id = next_planetary_generation(&state);
        let _ = perform_standardized_analysis(
            &app,
            &state,
            analysis_request_id,
            &file_path,
            is_surface_batch,
            target_type.clone(), // NEW
            warping_analysis,
            bayer_override,
            anchor_override.clone(),
            progress_prefix,
            // El lote respeta el mismo selector GPU de Ajustes que el flujo
            // individual (antes forzaba Auto e ignoraba la elección).
            ComputePolicy::from_planetary_legacy(
                compute_policy.as_deref().or(gpu_mode.as_deref()),
            ),
            DecodePolicy::from_legacy(decode_policy.as_deref()),
            requested_quality_policy,
        )?; // PR-2.5: síncrona (el lote ya corre por entrada, sin .await)
        cached_opt = load_validated_analysis_cache(&cache_path, &cache_expectation)
            .map(|(cached, _location)| cached);
    }

    if cached_opt.is_none() {
        return Err("Error: AnÃ¡lisis no disponible y fallo la generaciÃ³n automÃ¡tica.".into());
    }

    // ============================================================
    // R13: BATCH = MOTOR ZENITH (unificado)
    // El lote apila con stack_video_liquid_warping_impl — exactamente el
    // mismo motor del flujo interactivo (seleccion por-AP, doble pasada,
    // subpixel insesgado, normalizacion). El motor legacy fue eliminado.
    // ============================================================

    // 1. Puntos AP por archivo: reutiliza los del cache (archivo de referencia
    //    tuneado por el usuario) o genera la malla automatica del flujo Zenith.
    let mut custom_points: Vec<ApPoint> = cached_opt
        .as_ref()
        .and_then(|c| c.ap_points.clone())
        .unwrap_or_default();
    if warping_analysis && custom_points.is_empty() {
        let best_idx = cached_opt
            .as_ref()
            .and_then(|c| c.best_frame_idx)
            .unwrap_or(ref_idx)
            .min(total.saturating_sub(1));
        let raw_best = r.get_frame(best_idx, cid);
        let u16_best = raw_to_u16_buffer(&raw_best, w, h, bpp);
        let grid_mode = if is_surface_batch { "surface" } else { "planetary" };
        let g_size = ap_grid_size.unwrap_or(32).max(8) as usize;
        let g_thresh = ap_threshold.unwrap_or(if is_surface_batch { 0.04 } else { 0.08 });
        custom_points =
            smart_grid::generate_smart_grid_internal(&u16_best, w, h, g_size, g_thresh, grid_mode);
        log_to_front(
            &app,
            "INFO",
            &format!("Batch: {} puntos AP generados para {}", custom_points.len(), fname),
        );
    }
    if let Some(normalized) = normalized_ap_points.as_deref() {
        let planned_points = materialize_batch_ap_points(normalized, w, h);
        if planned_points.is_empty() {
            return Err("El plan de secuencia no pudo materializar sus AP normalizados.".into());
        }
        custom_points = planned_points;
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Batch: {} AP de secuencia materializados desde la referencia congelada",
                custom_points.len()
            ),
        );
    }

    // 2. El motor borra el cache temporal de frames al terminar: el lector
    //    local debe cerrarse antes y no debe usarse despues.
    drop(r);

    let ap_size_px = ap_grid_size.unwrap_or(32).max(8);
    let stack_request_id = next_planetary_generation(&state);
    let _engine_preview = stack_video_liquid_warping_impl(
        &app,
        &state,
        stack_request_id,
        file_path.clone(),
        stack_pct,
        custom_points,
        drizzle,
        is_surface_batch,
        bayer_override,
        ap_size_px,
        sharpened,
        sharpen_intensity,
        double_pass,
        warping_analysis,
        anchor_override.clone(),
        None, // stacking_roi: el lote siempre apila el frame completo
        normalize_colors,
        is_v3,
        target_type.clone(),
        Some(get_msg("")),
        None, // keep_full_frame: el lote usa el recorte por defecto
        align_rgb, // switch de usuario (mismo toggle que el flujo individual)
        ComputePolicy::from_planetary_legacy(
            compute_policy.as_deref().or(gpu_mode.as_deref()),
        ),
        DecodePolicy::from_legacy(decode_policy.as_deref()),
        requested_quality_policy,
    )?; // PR-2.5: síncrona (el lote ya corre por entrada, sin .await)

    // 3. Recoger el resultado del motor (y liberar el slot compartido).
    let engine_stack = {
        let _generation_guard = state
            .planetary_generation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.cancel_requested.load(Ordering::Acquire)
            || state.active_req_id.load(Ordering::Acquire) != stack_request_id
        {
            return Err("Cancelado o sustituido por otro apilado".into());
        }
        state
            .stacked_image
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .ok_or("El motor Zenith no produjo resultado")?
    };
    let engine_is_mono = engine_stack.is_mono;
    let stacked_data = engine_stack.data;
    let out_w = engine_stack.width;
    let out_h = engine_stack.height;

    // El post del lote recibe una generación propia sin rearmar el flag: una
    // cancelación entre apilado y filtros permanece sticky.
    if state
        .cancel_requested
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Err("Cancelado por el usuario".to_string());
    }
    let batch_postprocess_request_id = next_planetary_generation(&state);
    let batch_postprocess_token = PlanetaryJobToken::for_app(
        &app,
        batch_postprocess_request_id,
        state.cancel_requested.clone(),
    );

    // La secuencia nunca persigue la textura interior entre vídeos: filamentos,
    // manchas y detalle joviano pueden moverse físicamente. Superficie conserva
    // sólo un canvas fijo; los discos visibles continúan por el recenter CoG de
    // la rama inferior. El anchor de referencia queda inmutable para telemetría
    // y nunca acumula deriva de una entrada a la siguiente.
    let (centered_data, cw, ch) = if is_surface_batch {
        // En modo solar/surface, maximizamos el area.
        // Recortamos un margen minimo de seguridad (Protection Frame) para estabilizacion
        // Reducimos el recorte a algo minimo (ej. 8-10px) para maximizar FOV
        let cut_x = 12.min(out_w / 20);
        let cut_y = 12.min(out_h / 20);

        let safe_w = out_w.saturating_sub(cut_x * 2);
        let safe_h = out_h.saturating_sub(cut_y * 2);

        let mut solar_out = vec![0u16; safe_w * safe_h * 3];
        for y in 0..safe_h {
            for x in 0..safe_w {
                let src_idx = ((y + cut_y) * out_w + (x + cut_x)) * 3;
                let dst_idx = (y * safe_w + x) * 3;
                solar_out[dst_idx] = stacked_data[src_idx];
                solar_out[dst_idx + 1] = stacked_data[src_idx + 1];
                solar_out[dst_idx + 2] = stacked_data[src_idx + 2];
            }
        }

        let (fixed_w, fixed_h) = with_current_planetary_job(
            &state.planetary_generation_gate,
            &batch_postprocess_token,
            "la congelacion del canvas batch",
            || {
                let mut dims = state
                    .batch_anchor_dims
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if dims.0 == 0 || dims.1 == 0 {
                    *dims = (safe_w, safe_h);
                }
                *dims
            },
        )?;
        if fixed_w == 0 || fixed_h == 0 {
            return Err("El canvas batch tiene dimensiones invalidas".into());
        }
        let fixed = if fixed_w == safe_w && fixed_h == safe_h {
            solar_out
        } else {
            center_crop_or_pad_rgb(&solar_out, safe_w, safe_h, fixed_w, fixed_h)
        };
        with_current_planetary_job(
            &state.planetary_generation_gate,
            &batch_postprocess_token,
            "la referencia inmutable del lote",
            || {
                let mut anchor = state
                    .batch_anchor
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if anchor.is_none() {
                    *anchor = Some(fixed.clone());
                }
            },
        )?;
        (fixed, fixed_w, fixed_h)
    } else {
        // La consistencia del disco se obtiene del limbo, no de la textura
        // interior. Esto evita que bandas de Júpiter o cráteres lunares muevan
        // el centro entre capturas. Escala/roll sólo se aplican cuando tanto la
        // detección actual como la referencia superan el gate de contraste.
        let (tw, th) = with_current_planetary_job(
            &state.planetary_generation_gate,
            &batch_postprocess_token,
            "las dimensiones del lote planetario",
            || {
                let mut dims_guard = state
                    .batch_anchor_dims
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if dims_guard.0 == 0 || dims_guard.1 == 0 {
                    *dims_guard = (out_w, out_h);
                }
                *dims_guard
            },
        )?;

        let base = if tw == out_w && th == out_h {
            stacked_data.clone()
        } else {
            center_crop_or_pad_rgb(&stacked_data, out_w, out_h, tw, th)
        };

        let mono = batch_rgb16_to_mono(&base);
        let current_disc = crate::derotation::detect_planet_disc(&mono, tw, th);
        let current_reliable = batch_planet_disc_is_reliable(&current_disc, &mono, tw, th);
        let anchor_snapshot = with_current_planetary_job(
            &state.planetary_generation_gate,
            &batch_postprocess_token,
            "la lectura de referencia de limbo",
            || {
                state
                    .batch_anchor
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone()
            },
        )?;
        let reference_disc = anchor_snapshot
            .as_ref()
            .filter(|anchor| anchor.len() == base.len())
            .and_then(|anchor| {
                let reference_mono = batch_rgb16_to_mono(anchor);
                let disc = crate::derotation::detect_planet_disc(&reference_mono, tw, th);
                batch_planet_disc_is_reliable(&disc, &reference_mono, tw, th).then_some(disc)
            });
        let out = if current_reliable {
            let target_disc = reference_disc.unwrap_or_else(|| crate::derotation::PlanetDisc {
                cx: tw as f64 / 2.0,
                cy: th as f64 / 2.0,
                radius_x: current_disc.radius_x,
                radius_y: current_disc.radius_y,
                angle_deg: current_disc.angle_deg,
                phase: current_disc.phase,
            });
            batch_warp_disc_to_reference(
                &base,
                tw,
                th,
                &current_disc,
                &target_disc,
                if anchor_snapshot.is_some() {
                    sequence_max_scale_delta
                } else {
                    0.0
                },
                if anchor_snapshot.is_some() {
                    sequence_max_roll_degrees
                } else {
                    0.0
                },
            )
        } else {
            log_to_front(
                &app,
                "WARNING",
                "Batch: limbo no confiable; se conserva el frame sin escala ni roll.",
            );
            base
        };
        with_current_planetary_job(
            &state.planetary_generation_gate,
            &batch_postprocess_token,
            "la referencia planetaria inmutable del lote",
            || {
                let mut anchor = state
                    .batch_anchor
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if anchor.is_none() {
                    *anchor = Some(out.clone());
                }
            },
        )?;
        (out, tw, th)
    };

    // El máster de secuencia se congela antes de wavelets, deconvolución,
    // niveles o normalización fotométrica. Ambos artefactos se codifican bajo
    // una carpeta oculta y aparecen juntos mediante un único rename.
    let mut staged_output = StagedPlanetaryDirectory::create(
        Path::new(&output_folder),
        &format!("Stack_{fname}"),
    )?;
    let prepared_file_name = format!("{fname}_Prepared_RGB16.png");
    let linear_master_file_name = format!("{fname}_Linear_Master_RGB16.tiff");
    let prepared_path = staged_output.final_path(&prepared_file_name);
    let linear_master_path = staged_output.final_path(&linear_master_file_name);
    let mut staged_master = StagedPlanetaryArtifact::encode_rgb16_tiff(
        staged_output.temporary_path(&linear_master_file_name),
        &centered_data,
        cw,
        ch,
    )?;
    staged_master.publish(false)?;

    // --- R13: la salida del motor Zenith YA incluye rechazo de outliers
    // (planetas), balance de blancos sin clipping, normalizacion de rango y
    // sharpening opcional. Los antiguos bloques "parity" duplicaban WB y
    // sharpening (doble aplicacion) y dependian del master legacy: eliminados.
    let mut pre_processed = centered_data;
    if sequence_common_rgb_scalar {
        let reference = with_current_planetary_job(
            &state.planetary_generation_gate,
            &batch_postprocess_token,
            "la referencia fotometrica del lote",
            || {
                state
                    .batch_anchor
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone()
            },
        )?;
        if let Some(reference) = reference.filter(|value| value.len() == pre_processed.len()) {
            let scalar = batch_common_rgb_luminance_scalar(&reference, &pre_processed);
            batch_apply_common_rgb_scalar(&mut pre_processed, scalar);
            log_to_front(
                &app,
                "INFO",
                &format!(
                    "Batch: normalización fotométrica común RGB {:.4} (máster lineal intacto)",
                    scalar
                ),
            );
        }
    }
    let (safe_w, safe_h) = (cw, ch);
    let temp_res = StackResult {
        data: pre_processed, // NOW using the enhanced base
        width: safe_w,
        height: safe_h,
        is_mono: engine_is_mono,
        is_surface: is_surface_batch,
    };

    // Apply deconvolution parameters from reference image configuration
    let (use_d_iter, use_d_sigma, use_v_iter, use_v_sigma) =
        (deconv_iter, deconv_sigma, vc_iter, vc_sigma);

    let resolved_post = resolve_post_processing_contract(
        temp_res.is_mono,
        usm_amount,
        usm_radius,
        lce_amount,
        r_bal,
        b_bal,
    );

    // Batch: la descomposición GPU interactiva no aplica aquí (el apilado ya usa
    // su propio motor GPU de acumulación); el post se re-aplica en CPU.
    let gpu_allowed = false;
    let mut processed = run_processing_pipeline(
        &app,
        &state,
        batch_postprocess_request_id,
        &temp_res,
        safe_w,
        safe_h,
        [u1, u2, u3, u4, u5],
        [w1, w2, w3, w4, w5, w6],
        [d1, d2, d3, d4, d5, d6],
        gamma,
        saturation,
        r_x,
        r_y,
        b_x,
        b_y,
        deringing_mode,
        deringing_radius,
        deringing_dark,
        deringing_light,
        deringing_mask,
        crisp,
        use_d_iter,
        use_d_sigma,
        use_v_iter,
        use_v_sigma,
        resolved_post.usm_amount,
        resolved_post.usm_radius,
        adaptive_usm.unwrap_or_default(),
        resolved_post.lce_amount,
        blend,
        contrast,
        brightness,
        resolved_post.r_bal,
        resolved_post.b_bal,
        master_denoise,     // PHASE 23
        master_denoise_detail,
        master_denoise_chroma,
        use_rgb_sharpening, // PHASE 15
        edge_aware_wavelets.unwrap_or(false), // B
        psf_from_limb.unwrap_or(false),        // A
        edge_aware_strength.unwrap_or(50.0),   // B+
        auto_mask.unwrap_or(0.0),              // adaptativo
        gpu_allowed,                           // Velocidad: GPU wavelets (paridad+fallback)
        levels_black.unwrap_or(0.0),           // Niveles
        levels_white.unwrap_or(1.0),
        levels_gamma.unwrap_or(1.0),
        // Lote: `original` YA es el master completo → medir aqui es correcto y
        // mantiene el resultado bit-identico al historico.
        None,
        ProtectionProfile::from_mode(purity_mode.as_deref()),
    );

    if processed.is_empty() {
        return Err("Cancelado por el usuario".to_string());
    }
    if let Some(ref advanced_params) = advanced {
        apply_advanced_postprocess_with(
            &mut processed,
            temp_res.width,
            temp_res.height,
            temp_res.is_mono,
            advanced_params,
            ProtectionProfile::from_mode(purity_mode.as_deref()),
        );
    }
    planetary_derotation_checkpoint(&batch_postprocess_token, "el postprocesado batch")?;
    let mut staged_prepared = StagedPlanetaryArtifact::encode_rgb16_png(
        staged_output.temporary_path(&prepared_file_name),
        &processed,
        safe_w,
        safe_h,
    )?;
    staged_prepared.publish(false)?;
    with_current_planetary_job(
        &state.planetary_generation_gate,
        &batch_postprocess_token,
        "la publicacion batch",
        || staged_output.publish(),
    )??;

    // ASSET PROTOCOL: la UI (batch y mosaico) carga `path` con convertFileSrc,
    // asi que ya no se genera el preview base64 por entrada. Antes cada archivo
    // del lote pagaba un PNG 8-bit + base64 (CPU) y megabytes de IPC, y el
    // frontend RETENIA todos esos strings en RAM para el reproductor — en lotes
    // grandes empujaba el heap del WebView a >1 GB (crash del renderer).
    Ok(BatchEntryResult {
        path: clean_windows_path(
            dunce::canonicalize(&prepared_path).unwrap_or(prepared_path),
        ),
        master_path: clean_windows_path(
            dunce::canonicalize(&linear_master_path).unwrap_or(linear_master_path),
        ),
        preview_base64: String::new(),
    })
}

fn apply_gaussian_blur_safe(data: &[f32], w: usize, h: usize, sigma: f32) -> Vec<f32> {
    // FIX (sigma was IGNORED): this was a fixed 5-tap kernel (sigma ~1.0) no
    // matter what the caller asked — the high-pass always used sigma 1 instead
    // of 3, the Smart Sharpen RADIUS slider did nothing, and LCE/CLAHE
    // (dynamic sigma up to 30) degenerated into a fine high-pass instead of
    // true local contrast. Delegate to the corrected 3-pass box Gaussian:
    // separable, parallel, O(n) regardless of sigma (no freeze risk), and it
    // honours the requested sigma.
    apply_gaussian_blur(data, w, h, sigma.clamp(0.5, 40.0))
}

// NUEVO: Optimized Surface Enhancement + Quality Scoring (AutoStakkert-like)
// Uses Integer Math instead of F32 for speed.
// Returns (Enhanced Image, Quality Score)
// STRUCT FOR MEMORY REUSE
struct AnalysisBufferSet {
    pub raw_u16: Vec<u16>,   // Verde/mono canónico a resolución completa
    pub raw_decode_u16: Vec<u16>, // Scratch CFA/RGB decodificado; evita alloc por frame
    pub half_u16: Vec<u16>,  // 2× downscaled working image (analysis runs here)
    pub blur_temp: Vec<u16>, // Reuse for separable blur intermediate
    pub blur_out: Vec<u16>,  // Reuse for blur result
    pub lap_out: Vec<u16>,   // Reuse for Laplacian result
    pub quarter_u16: Vec<u16>, // Pirámide 4× para el SAD grueso GPU
}

impl AnalysisBufferSet {
    fn new(size: usize) -> Self {
        Self {
            raw_u16: vec![0u16; size],
            raw_decode_u16: Vec::new(), // lazy: mono no paga este buffer
            half_u16: vec![0u16; size / 4 + 4],
            blur_temp: vec![0u16; size],
            blur_out: vec![0u16; size],
            lap_out: vec![0u16; size],
            quarter_u16: vec![0u16; size / 16 + 4],
        }
    }
}

// NUEVO: Buffered version of enhance_and_score_surface
// Returns reference to the enhanced buffer (which is inside lap_out) and the score.
/// Estira el mapa Laplaciano a 0..60000 para que el SAD compare mapas con
/// el mismo rango entre frames. OJO: destruye la magnitud absoluta — toda
/// métrica de NITIDEZ debe calcularse ANTES de esta llamada (el baseline F0
/// demostró que puntuar sobre el mapa normalizado invierte el ranking:
/// Spearman −1.0 contra la verdad conocida).
fn normalize_lap_for_sad(lap_out: &mut [u16]) {
    let mut min_val = 65535u16;
    let mut max_val = 0u16;
    for &v in lap_out.iter() {
        if v < min_val {
            min_val = v;
        }
        if v > max_val {
            max_val = v;
        }
    }
    if max_val > min_val {
        let range = (max_val - min_val) as f32;
        let scale = 60000.0 / range;
        for v in lap_out.iter_mut() {
            if *v > 0 {
                let f = (*v as f32 - min_val as f32) * scale;
                *v = f as u16;
            }
        }
    }
}

/// Compatibilidad: blur + Laplaciano + score de superficie legado, dejando
/// lap_out NORMALIZADO para SAD (comportamiento histórico). Los llamadores
/// que necesiten magnitudes reales usan `enhance_and_lap_raw` + score v2 +
/// `normalize_lap_for_sad` por separado.
fn enhance_and_score_surface_buffered(
    input: &[u16],
    width: usize,
    height: usize,
    blur_temp: &mut Vec<u16>,
    blur_out: &mut Vec<u16>,
    lap_out: &mut Vec<u16>,
) -> u64 {
    let score = enhance_and_lap_raw(input, width, height, blur_temp, blur_out, lap_out);
    normalize_lap_for_sad(lap_out);
    score
}

/// Blur gaussiano entero + Laplaciano 8-vecinos con lap_out CRUDO (sin
/// normalizar). Devuelve el score de superficie legado (Σ lap² con gate
/// fijo >100) que hoy solo se usa como fallback/diagnóstico.
fn enhance_and_lap_raw(
    input: &[u16],
    width: usize,
    height: usize,
    blur_temp: &mut Vec<u16>,
    blur_out: &mut Vec<u16>,
    lap_out: &mut Vec<u16>,
) -> u64 {
    // Ensure buffers are ready
    let len = width * height;
    if blur_temp.len() != len {
        blur_temp.resize(len, 0);
    }
    if blur_out.len() != len {
        blur_out.resize(len, 0);
    }
    if lap_out.len() != len {
        lap_out.resize(len, 0);
    }

    // 1. Integer Gaussian Blur (Sigma ~1.0-1.5, Kernel 5x1 Separable)
    apply_separable_gaussian_blur_u16_buffered(input, width, height, blur_temp, blur_out);

    // 2. Laplacian + Score
    let mut total_score: u64 = 0;
    let blurred = &blur_out; // Source
                             // We write to lap_out

    // Use i32 for Laplacian calc to handle negatives
    // OPT 6: Flattened Loop with chunking for strict LLVM Auto-Vectorization
    // Since we ignore the 1-pixel border, we iterate through the valid inner rect.
    let w = width;
    let lap_slice = &mut lap_out[w..w * (height - 1)];
    let blur_slice = &blurred[w..w * (height - 1)];
    let blur_up = &blurred[0..w * (height - 2)];
    let blur_down = &blurred[w * 2..w * height];

    for (_y_idx, (out_row, (c_row, (u_row, d_row)))) in lap_slice
        .chunks_exact_mut(w)
        .zip(
            blur_slice
                .chunks_exact(w)
                .zip(blur_up.chunks_exact(w).zip(blur_down.chunks_exact(w))),
        )
        .enumerate()
    {
        let out_inner = &mut out_row[1..w - 1];
        let c_inner = &c_row[1..w - 1];
        let c_left = &c_row[0..w - 2];
        let c_right = &c_row[2..w];

        let u_inner = &u_row[1..w - 1];
        let u_left = &u_row[0..w - 2];
        let u_right = &u_row[2..w];

        let d_inner = &d_row[1..w - 1];
        let d_left = &d_row[0..w - 2];
        let d_right = &d_row[2..w];

        for i in 0..out_inner.len() {
            let c = c_inner[i] as i32;
            let n_sum = c_left[i] as i32
                + c_right[i] as i32
                + u_inner[i] as i32
                + d_inner[i] as i32
                + u_left[i] as i32
                + u_right[i] as i32
                + d_left[i] as i32
                + d_right[i] as i32;

            let lap = (c * 8 - n_sum).abs();

            if lap > 100 {
                total_score += lap as u64 * lap as u64;
            }

            out_inner[i] = lap as u16;
        }
    }

    total_score
}

// Buffered Blur Helper
fn apply_separable_gaussian_blur_u16_buffered(
    input: &[u16],
    width: usize,
    height: usize,
    temp_buf: &mut [u16],
    out_buf: &mut [u16],
) {
    // 1-Pass Horizontal -> temp_buf
    // OPT 6: Flattened Loop with chunking for strict LLVM Auto-Vectorization
    for (in_row, temp_row) in input
        .chunks_exact(width)
        .zip(temp_buf.chunks_exact_mut(width))
    {
        let in_inner = &in_row[2..width - 2];
        let in_ll = &in_row[0..width - 4];
        let in_l = &in_row[1..width - 3];
        let in_r = &in_row[3..width - 1];
        let in_rr = &in_row[4..width];
        let out_inner = &mut temp_row[2..width - 2];

        for i in 0..out_inner.len() {
            let sum: u32 = in_ll[i] as u32 * 1
                + in_l[i] as u32 * 4
                + in_inner[i] as u32 * 6
                + in_r[i] as u32 * 4
                + in_rr[i] as u32 * 1;
            out_inner[i] = (sum >> 4) as u16;
        }
    }

    // 2-Pass Vertical -> out_buf
    let w = width;
    let temp_slice = &temp_buf[w * 2..w * (height - 2)];
    let temp_ll = &temp_buf[0..w * (height - 4)];
    let temp_l = &temp_buf[w..w * (height - 3)];
    let temp_r = &temp_buf[w * 3..w * (height - 1)];
    let temp_rr = &temp_buf[w * 4..w * height];
    let out_slice = &mut out_buf[w * 2..w * (height - 2)];

    for (out_row, (in_row, (in_ll, (in_l, (in_r, in_rr))))) in out_slice.chunks_exact_mut(w).zip(
        temp_slice.chunks_exact(w).zip(
            temp_ll.chunks_exact(w).zip(
                temp_l
                    .chunks_exact(w)
                    .zip(temp_r.chunks_exact(w).zip(temp_rr.chunks_exact(w))),
            ),
        ),
    ) {
        let out_inner = &mut out_row[2..w - 2];
        let in_inner = &in_row[2..w - 2];
        let in_ll_inner = &in_ll[2..w - 2];
        let in_l_inner = &in_l[2..w - 2];
        let in_r_inner = &in_r[2..w - 2];
        let in_rr_inner = &in_rr[2..w - 2];

        for i in 0..out_inner.len() {
            let sum: u32 = in_ll_inner[i] as u32 * 1
                + in_l_inner[i] as u32 * 4
                + in_inner[i] as u32 * 6
                + in_r_inner[i] as u32 * 4
                + in_rr_inner[i] as u32 * 1;
            out_inner[i] = (sum >> 4) as u16;
        }
    }
}

// OLD FUNCTIONS KEPT FOR COMPATIBILITY OR REFERENCE

fn enhance_solar_surface(input: &[u16], width: usize, height: usize) -> Vec<u16> {
    // FILTRO ESPECIAL PARA "SURFACE/SOLAR" IMPROVED (LoG):
    // 1. Convertir a F32
    // 2. Gaussian Blur (Sigma 2.0) para eliminar ruido de alta frecuencia (seeing/pixel noise)
    // 3. Laplacian para detectar bordes estructurales (Granulacion, Manchas)

    let len = input.len();
    let mut f32_buf = Vec::with_capacity(len);
    for &v in input {
        f32_buf.push(v as f32);
    }

    // Paso 2: Blur (Suavizado previo para estabilidad)
    // REDUCED SIGMA: 2.0 -> 1.0 para retener mas detalle de granulacion fina
    let blurred = apply_gaussian_blur_safe(&f32_buf, width, height, 1.0);

    // Paso 3: Laplacian (Edge Detection)
    // Kernel 3x3:
    // -1 -1 -1
    // -1  8 -1
    // -1 -1 -1
    let mut out = vec![0u16; len];

    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let idx = y * width + x;

            // Usamos la imagen borrosa para el Laplacian
            let c = blurred[idx];
            let n_sum = blurred[idx - 1]
                + blurred[idx + 1]
                + blurred[idx - width]
                + blurred[idx + width]
                + blurred[idx - width - 1]
                + blurred[idx - width + 1]
                + blurred[idx + width - 1]
                + blurred[idx + width + 1];

            // Laplacian: 8*Center - SumNeighbors
            // Queremos magnitud de borde
            let lap = (c * 8.0 - n_sum).abs();
            out[idx] = lap as u16; // Store temporarily
        }
    }

    // 4. NORMALIZATION (CRITICAL FOR SAD)
    // Stretch contrast to use full u16 range. PREVENTS WEAK LOCKS.
    let mut min_val = 65535.0;
    let mut max_val = 0.0;
    for &v in &out {
        let f = v as f32;
        if f < min_val {
            min_val = f;
        }
        if f > max_val {
            max_val = f;
        }
    }

    if max_val > min_val {
        let range = max_val - min_val;
        let scale = 60000.0 / range; // Target ~60k range
        for v in &mut out {
            let f = *v as f32;
            *v = ((f - min_val) * scale) as u16;
        }
    }

    out
}

#[tauri::command]
async fn crop_stacked_image(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
) -> Result<String, String> {
    state.license_manager.check_access()?;
    let request_id = begin_planetary_user_job(&state);
    let job_token = PlanetaryJobToken::for_app(
        &app,
        request_id,
        state.cancel_requested.clone(),
    );

    emit_progress(&app, "Recortando...", 0.0, None);

    let (new_data, new_w, new_h, is_mono, is_surface) = {
        let guard = state.stacked_image.lock().unwrap_or_else(|e| e.into_inner());
        let img = match &*guard {
            Some(i) => i,
            None => return Err("Sin imagen para recortar".into()),
        };

        let orig_w = img.width;
        let orig_h = img.height;

        if x + w > orig_w || y + h > orig_h {
            return Err("Coordenadas fuera de rango".into());
        }

        let mut cropped = Vec::with_capacity(w * h * 3);
        for row in y..(y + h) {
            let start = (row * orig_w + x) * 3;
            let end = start + w * 3;
            cropped.extend_from_slice(&img.data[start..end]);
        }

        (cropped, w, h, img.is_mono, img.is_surface)
    };

    let cropped_result = StackResult {
        data: new_data.clone(),
        width: new_w,
        height: new_h,
        is_mono,
        is_surface,
    };
    with_current_planetary_job(
        &state.planetary_generation_gate,
        &job_token,
        "publicar el recorte",
        || {
            *state
                .stacked_image
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(cropped_result);
            state.deconv_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
            state.wavelet_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
            state.filter_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
        },
    )?;

    emit_progress(&app, "Actualizando vista...", 50.0, None);

    let vis = to_8bit_visual(&new_data, 1.0);
    let mut png = Vec::new();
    image::png::PngEncoder::new(&mut Cursor::new(&mut png))
        .encode(&vis, new_w as u32, new_h as u32, image::ColorType::Rgb8)
        .map_err(|e| e.to_string())?;

    emit_progress(&app, "Listo", 100.0, None);
    Ok(format!(
        "data:image/png;base64,{}",
        general_purpose::STANDARD.encode(&png)
    ))
}

#[tauri::command]
async fn preview_video(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    bayer_override: Option<i32>,
) -> Result<PreviewResult, String> {
    state.license_manager.check_access()?;

    log_to_front(&app, "INFO", &format!("Cargando preview: {}", path));

    let reader = VideoInput::open(&path, &app)?;
    let w = reader.width();
    let h = reader.height();
    let count = reader.frame_count();
    let bpp = reader.bpp();
    let cid = reader.resolve_bayer_override(bayer_override)?;

    let idx = select_signal_frame_index(&reader, w, h, bpp, cid, 0);
    if idx != 0 {
        log_to_front(
            &app,
            "INFO",
            &format!("Frame 0 oscuro: usando frame {} para vista previa.", idx),
        );
    }
    let raw = reader.get_frame(idx, cid);
    log_to_front(
        &app,
        "INFO",
        &format!(
            "Preview SER/Video: {}x{}, frames={}, bpp={}, color_id={}, frame={}, raw_len={}",
            w,
            h,
            count,
            bpp,
            cid,
            idx,
            raw.len()
        ),
    );

    let u16s = raw_to_u16_buffer(&raw, w, h, bpp);
    let suggested_target = suggest_target_from_frame(&u16s);
    let mut rgb = debayer_to_rgb(&u16s, w, h, cid);
    if cid >= 8 && cid <= 11 {
        auto_color_balance(&mut rgb, w, h);
    }
    let is_color = reader.is_color_for(cid);
    let vis = if is_color {
        to_8bit_preview_visual(&rgb)
    } else {
        to_8bit_visual(&auto_contrast_stretch_u16(&rgb, w, h), 1.0)
    };

    let mut png = Vec::new();
    image::png::PngEncoder::new(&mut Cursor::new(&mut png))
        .encode(&vis, w as u32, h as u32, image::ColorType::Rgb8)
        .map_err(|e| e.to_string())?;

    let p = Path::new(&path);
    let fname = p
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    Ok(PreviewResult {
        width: w,
        height: h,
        frame_count: count,
        preview_base64: format!(
            "data:image/png;base64,{}",
            general_purpose::STANDARD.encode(&png)
        ),
        filename: fname,
        is_color,
        suggested_target,
    })
}

#[tauri::command]
async fn analyze_video(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    mode: String,
    target_type: String, // NEW
    warping_analysis: bool, // NEW
    bayer_override: Option<i32>,
    anchor_override: Option<Vec<i32>>,
    progress_prefix: Option<String>, // Added
) -> Result<AnalysisResult, String> {
    state.license_manager.check_access()?;

    // Compatibilidad de una versión: todos los nombres de modo, incluidos los
    // v1 históricos, entran al motor tipado Hybrid v2. El bloque antiguo queda
    // sólo para poder retirar formatos de caché heredados sin cambiar hoy la
    // firma pública del comando.
    let is_surface_mode = matches!(
        mode.as_str(),
        "surface_v3" | "surface_v2" | "zenith_ultimate_surface" | "surface" | "surface_v1"
    );
    // Se conserva como decisión explícita para retirar el cuerpo v1 en la
    // siguiente versión sin romper hoy su formato de llamada.
    let compatibility_uses_hybrid_v2 = |_requested_mode: &str| true;

    if compatibility_uses_hybrid_v2(&mode) {
        return analyze_video_v2(
            app,
            state,
            path,
            is_surface_mode,
            target_type, // NEW
            warping_analysis,
            bayer_override,
            anchor_override,
            progress_prefix,
        )
        .await;
    }

    log_to_front(
        &app,
        "INFO",
        &format!("Analizando (Modo: {}): {}", mode, path),
    );
    let prefix_str = progress_prefix.unwrap_or_default();
    let get_msg = |msg: &str| {
        if prefix_str.is_empty() {
            msg.to_string()
        } else {
            format!("{} {}", prefix_str, msg)
        }
    };

    emit_progress(&app, &get_msg("Preparando..."), 0.0, None);

    let reader = VideoInput::open(&path, &app)?;
    let total = reader.frame_count();
    let w = reader.width();
    let h = reader.height();
    let bpp = reader.bpp();
    let cid = reader.resolve_bayer_override(bayer_override)?;

    // DEBUG YUY2/ColorID
    log_to_front(
        &app,
        "INFO",
        &format!(
            "DEBUG VIDEO ANALYSIS: W={} H={} BPP={} CID={} Color={}",
            w,
            h,
            bpp,
            cid,
            reader.is_color()
        ),
    );
    let req_id = begin_planetary_user_job(&state);

    let _ctr = Arc::new(AtomicUsize::new(0));
    let tf = total as f32;

    let mode_suffix = if mode == "surface" {
        "surface"
    } else {
        "planetary"
    };
    // USE UNIFIED HELPER
    let cache_path = get_analysis_cache_path(&path, mode_suffix);
    if Path::new(&cache_path).exists() {
        if let Ok(file) = File::open(&cache_path) {
            let buf_reader = BufReader::new(file);
            if let Ok(cached) = bincode::deserialize_from::<_, CachedAnalysis>(buf_reader) {
                log_to_front(&app, "SUCCESS", "Analisis cargado desde disco.");
                let best_idx = cached
                    .scores
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, &(_, s))| s)
                    .map(|(_, &(i, _))| i)
                    .unwrap_or(0);
                let raw = reader.get_frame(best_idx, cid);
                let u16s = raw_to_u16_buffer(&raw, w, h, bpp);
                let mut rgb = debayer_to_rgb(&u16s, w, h, cid);
                if cid >= 8 && cid <= 11 {
                    auto_color_balance(&mut rgb, w, h);
                }
                let vis = to_8bit_preview_visual(&rgb);
                let mut png = Vec::new();
                image::png::PngEncoder::new(&mut Cursor::new(&mut png))
                    .encode(&vis, w as u32, h as u32, image::ColorType::Rgb8)
                    .map_err(|e| e.to_string())?;

                let pname = ser::ser_pattern_name(cid).to_string();

                // CRITICAL: is_color MUST respect user override to avoid UI automation resetting to color mode
                let final_is_color = reader.is_color_for(cid);

                let mut sorted_scores: Vec<u64> = cached.scores.iter().map(|&(_, s)| s).collect();
                sorted_scores.sort_unstable_by(|a, b| b.cmp(a));

                let total_frames = sorted_scores.len();
                // Absolute Min/Max for Stretcher Range
                let max_ref = sorted_scores.first().copied().unwrap_or(1) as f64;
                let min_ref = sorted_scores.last().copied().unwrap_or(0) as f64;
                let range = (max_ref - min_ref).max(1.0);







                // --- SMART RECOMMENDATION (Elbow Method / Kneedle Algorithm simplified) ---
                let mut max_dist = 0.0;
                let mut best_cut_idx = 0;

                for (i, &score) in sorted_scores.iter().enumerate() {
                    let y = if range > 0.0 {
                        (score as f64 - min_ref) / range
                    } else {
                        0.0
                    };
                    let x = i as f64 / total_frames as f64;
                    let y_line = 1.0 - x;
                    let dist = y - y_line;

                    if dist > max_dist {
                        max_dist = dist;
                        best_cut_idx = i;
                    }
                }

                let recommended_pct = if max_dist <= 0.05 {
                    20 // Default fallback
                } else {
                    ((best_cut_idx as f64 / total_frames as f64) * 100.0).round() as u8
                };

                // Send recommendation to front context
                let _ = app.emit("analysis-recommendation", recommended_pct);

                // --- CALCULATE STATS ---
                let sum_score: u64 = sorted_scores.iter().sum();
                let avg_raw = if total_frames > 0 {
                    sum_score as f64 / total_frames as f64
                } else {
                    0.0
                };

                let normalize = |v: f64| -> f64 {
                    if range <= 0.0001 {
                        return 0.0;
                    }
                    let n = ((v - min_ref) / range) * 100.0;
                    n.clamp(0.0, 100.0)
                };

                let norm_avg = normalize(avg_raw);
                let stability = norm_avg;

                // Graph: send normalized points
                let graph: Vec<(usize, f64)> = cached
                    .scores
                    .iter()
                    .map(|&(i, s)| {
                        let val = normalize(s as f64);
                        (i, val)
                    })
                    .collect();

                let rec_pct = if total_frames < 10 {
                    50.0
                } else {
                    let p = (best_cut_idx as f32 / total_frames as f32) * 100.0;
                    // Safety clamp: Min 5%, Max 80%
                    p.clamp(5.0, 80.0)
                };

                // DEBUG LOG TO GUI
                log_to_front(
                    &app,
                    "INFO",
                    &format!("DEBUG: analyze_video (cached) Best Index = {}", best_idx),
                );

                let fsize = fs::metadata(&path)
                    .map(|m| m.len() as f64 / 1_048_576.0)
                    .unwrap_or(0.0);
                return Ok(AnalysisResult {
                    metadata: VideoMetadata {
                        width: w,
                        height: h,
                        frame_count: total,
                        bpp: bpp * 8,
                        color_id: cid,
                        pattern_name: pname,
                        file_size_mb: fsize,
                        is_color: final_is_color,
                    },
                    stats: VideoStats {
                        min_pixel: 0,
                        max_pixel: 65535,
                        avg_brightness: 0.5,
                        dynamic_range_pct: 100.0,
                        best_score: 100.0,
                        worst_score: 0.0, // En min-max forzamos el rango
                        avg_quality: norm_avg,
                        quality_stability: stability,
                        std_dev: 0.0,
                        entropy: 0.0,
                    }, // Use the stats variable which should be defined as cached
                    quality_graph: graph,
                    preview_base64: format!(
                        "data:image/png;base64,{}",
                        general_purpose::STANDARD.encode(&png)
                    ),
                    path,
                    recommended_pct: rec_pct,
                    ap_points: vec![],
                    best_frame_idx: best_idx,
                    execution_plan: None,
                    stage_telemetry: Vec::new(),
                });
            }
        }
    }

    // Estrategia ROI segun modo
    let roi = if mode == "surface" {
        // En modo superficie usamos todo el cuadro (o un recorte central seguro para evitar panning)
        // Usaremos el frame central para validar
        Rect {
            x: 0,
            y: 0,
            w: w,
            h: h,
        }
    } else {
        // Modo Planetario clasico: Buscar el blob brillante
        let roi_ref_idx = select_signal_frame_index(&reader, w, h, bpp, cid, total / 2);
        let center_frame = reader.get_frame(roi_ref_idx, cid);
        find_planet_roi(&center_frame, w, h, bpp)
    };

    // OPTIMIZACION: Si es FFmpeg, usamos un step para acelerar el analisis (Sampling)
    // El costo de proceso de FFmpeg es alto, asi que cada 2 o 4 frames es mucho mas rapido.
    let step_an = if reader.is_ffmpeg() && total > 400 {
        if total > 4000 {
            4
        } else {
            2
        }
    } else {
        1
    };

    let needs_sequential_ffmpeg = reader.is_ffmpeg();

    let mut scores_tuples: Vec<(usize, u64)> = vec![];
    let mut streaming_success = false;

    if needs_sequential_ffmpeg {
        // --- SEQUENTIAL STREAMING ANALYSIS (ULTRA FAST) ---
        // Instead of re-opening FFmpeg 1000 times, we open it once and stream the frames.
        log_to_front(
            &app,
            "INFO",
            "Iniciando analisis secuencial de alta velocidad...",
        );

        let ffmpeg_path = get_ffmpeg_command(&app);
        let mut args = vec![];
        args.extend_from_slice(&["-hwaccel", "auto", "-i", &path]);

        // FIX: STRICT PIXEL FORMAT MAPPING
        // FFmpeg pipe must match the expected byte buffer size exactly.
        // reader.bpp() returns 1 (Gray8), 2 (Gray16), 3 (RGB24), 6 (RGB48).
        let is_color = reader.is_color();
        let stream_bpp = bpp; // This bpp comes from reader.bpp()

        // Determine input pixel format for FFmpeg filter 'format'
        // This forces FFmpeg to convert input to our desired raw structure.
        let p_fmt_in = match stream_bpp {
            1 => "gray",
            2 => "gray16le",
            3 => "rgb24",
            6 => "rgb48le",
            _ => {
                if is_color {
                    "rgb24"
                } else {
                    "gray"
                }
            } // Fallback
        };

        // Fix Aspect Ratio: Force consistency with reader dimensions
        let filter = format!(
            "select='not(mod(n,{}))',format={},scale={}:{}:flags=lanczos",
            step_an, p_fmt_in, w, h
        );

        args.extend_from_slice(&[
            "-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", p_fmt_in, "-vf", &filter, "-vsync",
            "0", // Important: output exactly what is selected
            "pipe:1",
        ]);

        let child_result = {
            let mut cmd = std::process::Command::new(&ffmpeg_path);
            #[cfg(target_os = "windows")]
            cmd.creation_flags(0x08000000);
            cmd.args(&args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
        };

        match child_result {
            Ok(mut child) => {
                let mut stdout = child.stdout.take().unwrap();
                // Ensure expected bytes match exactly
                let expected_bytes = w * h * stream_bpp;

                let mut results = Vec::new();
                let mut buffer = vec![0u8; expected_bytes];
                let mut idx = 0;

                use std::io::Read;

                loop {
                    // Check Cancellation
                    if check_cancel(&state, req_id) {
                        let _ = child.kill(); // Kill ffmpeg
                        return Err("Analisis cancelado por el usuario".into());
                    }

                    if let Err(_) = stdout.read_exact(&mut buffer) {
                        break; // End of stream
                    }

                    // Process frame `buffer`
                    let real_idx = idx * step_an;
                    if real_idx >= total {
                        break;
                    } // Safety

                    let score = if mode == "surface" {
                        let safe_roi = Rect {
                            x: w / 4,
                            y: h / 4,
                            w: w / 2,
                            h: h / 2,
                        };
                        calculate_quality_metric(&buffer, w, h, stream_bpp, &safe_roi)
                    } else {
                        calculate_quality_metric(&buffer, w, h, stream_bpp, &roi)
                    };

                    results.push((real_idx, score));

                    idx += 1;
                    if idx % 10 == 0 {
                        emit_progress(
                            &app,
                            "Analizando (Stream)",
                            (real_idx as f32 / total as f32) * 100.0,
                            None,
                        );
                    }
                }

                // Clean up
                let _ = child.kill();
                if !results.is_empty() {
                    scores_tuples = results;
                    streaming_success = true;
                } else {
                    log_to_front(
                        &app,
                        "WARN",
                        "Analisis secuencial retorno 0 frames. Reintentando modo seguro...",
                    );
                }
            }
            Err(e) => {
                log_to_front(
                    &app,
                    "ERROR",
                    &format!("Fallo iniciando FFmpeg stream: {}", e),
                );
                // Fallthrough to fallback
            }
        }
    }

    // FALLBACK: If streaming failed or wasn't needed
    if !streaming_success {
        // --- PARALLEL ANALYSIS (SER/AVI/IMAGES) ---
        let chunk_size = 64;
        let path_ref = path.clone();
        let app_clone = app.clone();
        scores_tuples = (0..total)
            .into_par_iter()
            .step_by(step_an)
            .with_min_len(chunk_size)
            .map_init(
                move || {
                    VideoInput::open(&path_ref, &app_clone)
                        .expect("Error al reabrir archivo en hilo")
                },
                |r_local, i| {
                    let c = _ctr.fetch_add(1, Ordering::Relaxed);
                    if c % 50 == 0 {
                        emit_progress(
                            &app,
                            "Analizando",
                            (c as f32 / tf) * 100.0,
                            Some(format!("{} de {} frames", c, total)),
                        );
                    }
                    let raw = r_local.get_frame(i, cid);
                    if raw.is_empty() {
                        return (i, 0);
                    }
                    // Si es superficie, calculamos calidad en el centro para evitar artefactos de borde por drift
                    if mode == "surface" {
                        let safe_roi = Rect {
                            x: w / 4,
                            y: h / 4,
                            w: w / 2,
                            h: h / 2,
                        };
                        (i, calculate_quality_metric(&raw, w, h, bpp, &safe_roi))
                    } else {
                        (i, calculate_quality_metric(&raw, w, h, bpp, &roi))
                    }
                },
            )
            .collect();
    };

    let cache_data = CachedAnalysis {
        scores: scores_tuples.clone(),
        roi: roi.clone(),
        path_hash: planetary_source_fingerprint(&path)?,
        frame_stats: None,
        quality_graph: None,
        width: None,
        height: None,
        best_frame_idx: None,
        ap_points: None,
        contract: None,
    };
    if let Ok(file) = File::create(&cache_path) {
        let mut writer = BufWriter::new(file);
        let _ = bincode::serialize_into(&mut writer, &cache_data);
    }

    let scores: Vec<u64> = scores_tuples.iter().map(|&(_, s)| s).collect();
    let best_idx = scores_tuples
        .iter()
        .max_by_key(|&(_, s)| s)
        .map(|&(i, _)| i)
        .unwrap_or(0);
    let raw = reader.get_frame(best_idx, cid);
    let u16s = raw_to_u16_buffer(&raw, w, h, bpp);
    let mut rgb = debayer_to_rgb(&u16s, w, h, cid);
    if cid >= 8 && cid <= 11 {
        auto_color_balance(&mut rgb, w, h);
    }
    let vis = to_8bit_preview_visual(&rgb);
    let mut png = Vec::new();
    image::png::PngEncoder::new(&mut Cursor::new(&mut png))
        .encode(&vis, w as u32, h as u32, image::ColorType::Rgb8)
        .map_err(|e| e.to_string())?;

    emit_progress(&app, "Listo", 100.0, None);
    let pname = ser::ser_pattern_name(cid).to_string();
    let mut sorted_scores = scores.clone();
    sorted_scores.sort_unstable_by(|a, b| b.cmp(a));
    let max_score = sorted_scores.first().copied().unwrap_or(1) as f64;
    let min_score_graph = sorted_scores.last().copied().unwrap_or(0) as f64;
    let range = (max_score - min_score_graph).max(1.0);

    let graph = scores_tuples
        .iter()
        .map(|&(i, s)| (i, ((s as f64 - min_score_graph) / range) * 100.0))
        .collect();

    let min_score = sorted_scores.last().copied().unwrap_or(0) as f64;
    let sum_score: u64 = scores.iter().sum();
    let avg_score = if scores.is_empty() {
        0.0
    } else {
        sum_score as f64 / scores.len() as f64
    };
    let stability = if max_score > 0.0 {
        (avg_score / max_score) * 100.0
    } else {
        0.0
    };

    let norm_worst = if max_score > 0.0 {
        (min_score / max_score) * 100.0
    } else {
        0.0
    };
    let norm_avg = if max_score > 0.0 {
        (avg_score / max_score) * 100.0
    } else {
        0.0
    };

    // --- Calculate Stats from Best Frame (u16s) ---
    // 1. Avg Brightness
    let mut sum_bri: f64 = 0.0;
    let mut sum_sq: f64 = 0.0;
    let mut hist = [0u32; 65536];

    for &val in &u16s {
        let v_f = val as f64;
        sum_bri += v_f;
        sum_sq += v_f * v_f;
        hist[val as usize] += 1;
    }
    let pixel_count = u16s.len() as f64;
    let avg_bri = sum_bri / pixel_count;

    // 2. Std Dev
    let variance = (sum_sq / pixel_count) - (avg_bri * avg_bri);
    let std_dev_val = variance.sqrt();

    // 3. Entropy
    let mut entropy_val = 0.0;
    for &count in &hist {
        if count > 0 {
            let p = count as f64 / pixel_count;
            entropy_val -= p * p.log2();
        }
    }

    // --- Dynamic Stacking Recommendation ---
    // Stability (avg/best) is a good proxy for seeing quality.
    // If stability is high (e.g. 90%), we can stack 50%+.
    // If stability is low (e.g. 30%), we should stack 5-10%.
    // Formula: 5.0 + (stability * 0.45) -> range ~5% to 50%
    let rec_pct = (5.0 + (stability * 0.45)).clamp(3.0, 75.0);

    let _stats = VideoStats {
        min_pixel: 0,
        max_pixel: 65535,
        avg_brightness: avg_bri as f32,
        dynamic_range_pct: 100.0,
        best_score: 100.0,
        worst_score: norm_worst.clamp(0.0, 100.0),
        avg_quality: norm_avg.clamp(0.0, 100.0),
        quality_stability: stability.clamp(0.0, 100.0),
        std_dev: std_dev_val,
        entropy: entropy_val,
    };

    // DEBUG LOG TO GUI
    log_to_front(
        &app,
        "INFO",
        &format!("DEBUG: analyze_video (fresh) Best Index = {}", best_idx),
    );

    let final_is_color = reader.is_color_for(cid);

    Ok(AnalysisResult {
        metadata: VideoMetadata {
            width: w,
            height: h,
            frame_count: total,
            bpp: bpp * 8,
            color_id: cid,
            file_size_mb: 0.0,
            pattern_name: pname,
            is_color: final_is_color,
        },
        stats: _stats,
        quality_graph: graph,
        preview_base64: format!(
            "data:image/png;base64,{}",
            general_purpose::STANDARD.encode(&png)
        ),
        path,
        recommended_pct: rec_pct as f32,
        ap_points: vec![],
        best_frame_idx: best_idx,
        execution_plan: None,
        stage_telemetry: Vec::new(),
    })
}

// Helper to calculate Local Entropy/Variance for Surface Mode
fn get_area_complexity(
    data: &[u16],
    w: usize,
    h: usize,
    cx: usize,
    cy: usize,
    ap_size: usize,
    avg_brightness: f32,
) -> f32 {
    let half = ap_size / 2;
    let start_x = cx.saturating_sub(half);
    let start_y = cy.saturating_sub(half);
    let end_x = (cx + half).min(w);
    let end_y = (cy + half).min(h);

    let mut sum_sq_diff = 0.0;
    let mut count = 0.0;

    for y in start_y..end_y {
        let row_start = y * w;
        for x in start_x..end_x {
            let val = data[row_start + x] as f32;
            let diff = val - avg_brightness;
            sum_sq_diff += diff * diff;
            count += 1.0;
        }
    }

    if count == 0.0 {
        return 0.0;
    }
    // Variance
    sum_sq_diff / count
}

// PHASE 9: Regional Quality Estimator (Laplacian-based)
// Measures local sharpness in a specific box to prioritize crisp regions over global quality.
fn get_area_quality_laplacian(
    data: &[u16],
    w: usize,
    h: usize,
    cx: usize,
    cy: usize,
    size: usize,
) -> f32 {
    let half = size / 2;
    let start_x = cx.saturating_sub(half);
    let start_y = cy.saturating_sub(half);
    let end_x = (cx + half).min(w - 1);
    let end_y = (cy + half).min(h - 1);

    let mut total_lap: f32 = 0.0;
    let mut count = 0;

    // Use a 5x5 Laplacian kernel to measure sharpness (Step 2 for speed)
    for y in (start_y + 2..end_y.saturating_sub(2)).step_by(2) {
        let row_off = y * w;
        let row_up = (y - 2) * w;
        let row_down = (y + 2) * w;
        for x in (start_x + 2..end_x.saturating_sub(2)).step_by(2) {
            let v = data[row_off + x] as i32;
            let v_l = data[row_off + x - 2] as i32;
            let v_r = data[row_off + x + 2] as i32;
            let v_u = data[row_up + x] as i32;
            let v_d = data[row_down + x] as i32;

            let lap = (4 * v - (v_l + v_r + v_u + v_d)).abs();
            total_lap += lap as f32;
            count += 1;
        }
    }
    if count == 0 {
        return 0.0;
    }
    total_lap / count as f32
}

#[tauri::command]
async fn generate_grid(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    ap_size: usize,
    min_bright: f32, // User override % (or 0 for auto)
    ref_frame_idx: Option<usize>,
    is_surface: bool, // NEW param to distinguish modes
) -> Result<Vec<(f32, f32, f32)>, String> {
    state.license_manager.check_access()?;

    let r = VideoInput::open(&path, &app)?;
    let w = r.width();
    let h = r.height();
    let bpp = r.bpp();

    let best_idx = ref_frame_idx.unwrap_or(0);

    emit_progress(&app, "Generando Malla Inteligente...", 50.0, None);

    let cid = r.color_id();
    let f = r.get_frame(best_idx, cid);
    let u16s = raw_to_u16_buffer(&f, w, h, bpp);

    // --- SMART THRESHOLDING STRATEGY ---
    let th: f32;
    let min_complexity: f32;

    if is_surface {
        // SURFACE MODE:
        // We need to avoid "flat shadows" (Mare without craters).
        // Strategy: Use average brightness as base, but enforce Variance check.
        // User 'min_bright' acts as a sensitivity modifier for variance.

        // 1. Calc Global Stats
        let mut sum = 0.0;
        for &v in &u16s {
            sum += v as f32;
        }
        let avg = sum / u16s.len() as f32;

        th = avg * 0.2; // Very low brightness threshold (just to avoid pure black)

        // Complexity Threshold (Variance)
        // If user sends 0 -> Default sensitivity
        // If user sends 100 -> High sensitivity (needs more contrast)
        // RELAXED: 50.0 -> 20.0 to allow more points in lunar mares
        let sensitivity = if min_bright <= 0.0 { 15.0 } else { min_bright };
        min_complexity = 20.0 * (sensitivity / 10.0).max(0.5);
    } else {
        // PLANETARY MODE:
        // Noise Floor Detection (avoid stars/noise in background)
        // 1. Build Histogram
        let mut hist = vec![0usize; 65536];
        let mut max_val = 0.0f32;
        for &v in &u16s {
            hist[v as usize] += 1;
            if v as f32 > max_val {
                max_val = v as f32;
            }
        }

        // 2. Find Noise Peak (first major peak) strategy usually works,
        // but simpler: Find background level (mode of lower 20%)
        // Or just use the User % of Max.

        let pct = if min_bright <= 0.0 { 8.0 } else { min_bright };

        // Smart "Auto-Black" boost:
        // If image is mostly black, ensure threshold is above the "grass"
        // Heuristic: Scan corners to find noise level?
        // Simpler: Just rely on Max % for Planet, gives control.
        th = max_val * (pct / 100.0);
        min_complexity = 0.0; // Not used for planet
    }

    // Optimization: ROI computation (Planetary only)
    // For Surface, we scan whole image usually.
    let mut min_x = w;
    let mut max_x = 0;
    let mut min_y = h;
    let mut max_y = 0;

    let scan_step = 8;
    for y in (0..h).step_by(scan_step) {
        let row_start = y * w;
        for x in (0..w).step_by(scan_step) {
            if u16s[row_start + x] as f32 > th {
                if x < min_x {
                    min_x = x;
                }
                if x > max_x {
                    max_x = x;
                }
                if y < min_y {
                    min_y = y;
                }
                if y > max_y {
                    max_y = y;
                }
            }
        }
    }

    // Safety padding
    if min_x > max_x || min_y > max_y {
        return Ok(vec![]); // No object found
    }

    // Grid Spacing
    let pad = ap_size / 2;
    let start_x = min_x.saturating_sub(pad);
    let end_x = (max_x + pad).min(w);
    let start_y = min_y.saturating_sub(pad);
    let end_y = (max_y + pad).min(h);

    let mut pts = Vec::new();

    // Adaptive Step (Surface optimization)
    let st = if is_surface && (w > 2500 || h > 2000) {
        96.max((ap_size as f32 * 0.75) as usize)
    } else {
        (ap_size as f32 * 0.75) as usize
    };

    // GENERATE POINTS
    for y in (start_y..end_y).step_by(st) {
        for x in (start_x..end_x).step_by(st) {
            // 1. Brightness Check
            let bri = get_area_brightness(&f, w, h, bpp, x, y, ap_size);
            if bri > th {
                // 2. Complexity Check (Surface Only)
                if is_surface {
                    let complexity = get_area_complexity(&u16s, w, h, x, y, ap_size, bri);
                    if complexity > min_complexity {
                        pts.push((x as f32, y as f32, ap_size as f32));
                    }
                } else {
                    pts.push((x as f32, y as f32, ap_size as f32));
                }
            }
        }
    }

    emit_progress(
        &app,
        &format!("Malla generada: {} puntos", pts.len()),
        100.0,
        None,
    );

    Ok(pts)
}

/// PHASE 12: AutoStakkert!-Style Sharpening (Noise-Free)
/// Key principles from AS!:
/// 1. LUMINANCE-ONLY: Sharpen only the L channel -> zero chromatic noise.
/// 2. CONSERVATIVE AMPLIFICATION: 1-6x per band, not 35-55x.
/// 3. ADAPTIVE SOFT CORING on ALL bands: only amplify detail > local noise floor.
/// 4. LOCAL SNR MASKING: suppress sharpening in background/sky pixels.
fn apply_autostakkert_sharpening(buffer: &mut [u16], width: usize, height: usize, intensity: f32, target_type: &str) {
    let npix = width * height;
    if npix == 0 || buffer.len() < npix * 3 { return; }

    let target = target_type.to_lowercase();
    let is_large_planet = target.contains("grande") || target.contains("large");
    let is_small_planet = target.contains("peque") || target.contains("small");

    // Conservative wavelet amplification scales (like AS!)
    let (s1_amp, s2_amp, s3_amp, s4_amp, s5_amp) = if is_large_planet {
        (2.0 * intensity, 3.5 * intensity, 2.0 * intensity, 1.0 * intensity, 0.3 * intensity)
    } else if is_small_planet {
        (3.5 * intensity, 2.5 * intensity, 1.0 * intensity, 0.4 * intensity, 0.1 * intensity)
    } else {
        (3.0 * intensity, 2.8 * intensity, 1.5 * intensity, 0.8 * intensity, 0.2 * intensity)
    };

    // PR-33: los bucles por píxel del sharpening final van en paralelo (cada
    // píxel es independiente; misma aritmética → bit-exacto). Antes esta fase
    // era 100% serie con la GPU y el resto de núcleos parados.
    use rayon::prelude::*;

    // 1. EXTRACT LUMINANCE
    let mut lum = vec![0.0f32; npix];
    lum.par_iter_mut().enumerate().for_each(|(i, l)| {
        let r = buffer[i * 3] as f32;
        let g = buffer[i * 3 + 1] as f32;
        let b = buffer[i * 3 + 2] as f32;
        *l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    });

    // 2. ESTIMATE NOISE from background corners (MAD estimator)
    let mut noise_samples = Vec::with_capacity(200);
    let margin = 8.min(width / 4).min(height / 4);
    for y in 0..margin {
        for x in 0..margin { noise_samples.push(lum[y * width + x]); }
        for x in (width - margin)..width { noise_samples.push(lum[y * width + x]); }
    }
    for y in (height - margin)..height {
        for x in 0..margin { noise_samples.push(lum[y * width + x]); }
        for x in (width - margin)..width { noise_samples.push(lum[y * width + x]); }
    }
    noise_samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_bg = noise_samples.get(noise_samples.len() / 2).cloned().unwrap_or(500.0);
    let mut abs_devs: Vec<f32> = noise_samples.iter().map(|v| (v - median_bg).abs()).collect();
    abs_devs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let noise_sigma = abs_devs.get(abs_devs.len() / 2).cloned().unwrap_or(100.0) * 1.4826;
    // El umbral de coring sigue al RUIDO (propiedad del dato), nunca a la
    // intensidad que pide el usuario.
    //
    // Antes era `noise_sigma * (2.5 + intensity)` con suelo `120·√intensity`: al
    // subir el slider subia tambien el umbral que descarta detalle, y con el
    // coring DURO de mas abajo el efecto no era solo sublineal, era una inversion.
    // Con σ pequeña (regimen del suelo absoluto) y un coeficiente de banda 1 de
    // 130 ADU: a intensidad 0.25 el umbral era 72 y aportaba (130−72)·3.0·0.25 =
    // 43.5; a intensidad 1.00 el umbral era 144 y aportaba CERO. Subir el
    // deslizador borraba el detalle que decia realzar.
    let base_threshold = (noise_sigma * 2.5).max(60.0);
    let signal_threshold = median_bg + noise_sigma * 2.0;
    // Ancho de la transicion SNR: el corte duro en `signal_threshold` producia un
    // escalon visible justo donde el fondo se convierte en señal.
    let snr_transition = (noise_sigma * 2.0).max(1.0);

    // 3. WAVELET DECOMPOSITION (luminance only)
    let b1_blur = apply_gaussian_blur_f32(&lum, width, height, 1.0);
    let b2_blur = apply_gaussian_blur_f32(&b1_blur, width, height, 2.0);
    let b3_blur = apply_gaussian_blur_f32(&b2_blur, width, height, 4.0);
    let b4_blur = apply_gaussian_blur_f32(&b3_blur, width, height, 8.0);
    let b5_blur = apply_gaussian_blur_f32(&b4_blur, width, height, 16.0);

    let band1: Vec<f32> = lum.par_iter().zip(b1_blur.par_iter()).map(|(a, b)| a - b).collect();
    let band2: Vec<f32> = b1_blur.par_iter().zip(b2_blur.par_iter()).map(|(a, b)| a - b).collect();
    let band3: Vec<f32> = b2_blur.par_iter().zip(b3_blur.par_iter()).map(|(a, b)| a - b).collect();
    let band4: Vec<f32> = b3_blur.par_iter().zip(b4_blur.par_iter()).map(|(a, b)| a - b).collect();
    let band5: Vec<f32> = b4_blur.par_iter().zip(b5_blur.par_iter()).map(|(a, b)| a - b).collect();

    // 4. SHARPEN LUMINANCE with adaptive soft coring + SNR masking
    let mut sharp_lum = vec![0.0f32; npix];
    sharp_lum.par_iter_mut().enumerate().for_each(|(i, sl)| {
        let orig_l = lum[i];

        // Mascara SNR: no realzar el fondo.
        //
        // La forma anterior (`if orig_l < signal_threshold { 0.0 } else {
        // (orig_l - signal_threshold) / (sigma*5+1) }`) ya era CONTINUA: valia
        // exactamente 0 en el umbral. El smoothstep no arregla un escalon — no lo
        // habia — sino que ademas hace continua la DERIVADA, con lo que la
        // transicion fondo→objeto no tiene el codo que dejaba la rampa lineal.
        // Mejora marginal; el invariante que importa (nunca un salto) lo fija
        // `test_prestack_snr_mask_has_no_discontinuity`.
        let snr_weight = post_smoothstep(
            signal_threshold - snr_transition,
            signal_threshold + snr_transition,
            orig_l,
        );

        if snr_weight < 0.001 {
            *sl = orig_l;
            return;
        }

        // Contraccion suave: `c · c²/(c²+t²)`. Continua y monotona — atenua el
        // ruido por debajo del umbral sin BORRAR de golpe los coeficientes justo
        // por encima, que es lo que hacia el coring duro `(|c|−t)·signo(c)`.
        // Preserva la propiedad esencial (|c| ≫ t pasa casi intacto) y elimina la
        // discontinuidad que hacia desaparecer detalle al subir la intensidad.
        let core = |coeff: f32, thresh: f32| -> f32 {
            let t2 = thresh * thresh;
            if t2 <= f32::EPSILON {
                return coeff;
            }
            let c2 = coeff * coeff;
            coeff * (c2 / (c2 + t2))
        };

        let d1 = core(band1[i], base_threshold * 1.2) * s1_amp;
        let d2 = core(band2[i], base_threshold * 0.8) * s2_amp;
        let d3 = core(band3[i], base_threshold * 0.5) * s3_amp;
        let d4 = core(band4[i], base_threshold * 0.3) * s4_amp;
        let d5 = core(band5[i], base_threshold * 0.2) * s5_amp;

        let total = (d1 + d2 + d3 + d4 + d5) * snr_weight;
        *sl = (orig_l + total).clamp(0.0, 65535.0);
    });

    // 5. APPLY back to RGB preserving chrominance (zero chromatic noise)
    buffer[..npix * 3]
        .par_chunks_mut(3)
        .enumerate()
        .for_each(|(i, px)| {
            let orig_l = lum[i];
            let new_l = sharp_lum[i];
            if orig_l < 1.0 {
                let v = new_l.clamp(0.0, 65535.0) as u16;
                px[0] = v;
                px[1] = v;
                px[2] = v;
            } else {
                let ratio = new_l / orig_l;
                px[0] = (px[0] as f32 * ratio).clamp(0.0, 65535.0) as u16;
                px[1] = (px[1] as f32 * ratio).clamp(0.0, 65535.0) as u16;
                px[2] = (px[2] as f32 * ratio).clamp(0.0, 65535.0) as u16;
            }
        });
}

// True separable Gaussian blur honoring sigma.
// FIX CRITICO: la versión anterior IGNORABA `sigma` (cada llamada desenfocaba
// ~σ1.5 fijo), colapsando la pirámide wavelet del sharpening: las bandas 3-5
// quedaban vacías y el "multi-escala" era en realidad mono-escala. Por eso el
// sharpening integrado no recuperaba estructura media/gruesa como AS!4.
fn apply_gaussian_blur_f32(data: &[f32], w: usize, h: usize, sigma: f32) -> Vec<f32> {
    if sigma <= 0.0 || data.is_empty() || w == 0 || h == 0 {
        return data.to_vec();
    }
    let radius = (sigma * 3.0).ceil() as usize;
    let mut kernel = Vec::with_capacity(radius * 2 + 1);
    let s2 = 2.0 * sigma * sigma;
    let mut sum = 0.0f32;
    for i in -(radius as isize)..=(radius as isize) {
        let v = (-((i * i) as f32) / s2).exp();
        kernel.push(v);
        sum += v;
    }
    for v in &mut kernel {
        *v /= sum;
    }

    // PR-33: ambas pasadas separables en paralelo por FILAS de salida — cada
    // elemento se calcula con la misma suma en el mismo orden que la versión
    // serial (bit-exacto); solo cambia qué hilo lo escribe. A 20MP con σ=16
    // (radio 48) la cascada del sharpening pasa de decenas de segundos a ~1 s.
    use rayon::prelude::*;
    let mut tmp = vec![0.0f32; w * h];
    let mut out = vec![0.0f32; w * h];

    // Horizontal pass
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, trow)| {
        let row = y * w;
        for x in 0..w {
            let mut acc = 0.0f32;
            for (k, &kv) in kernel.iter().enumerate() {
                let xx =
                    (x as isize + k as isize - radius as isize).clamp(0, w as isize - 1) as usize;
                acc += data[row + xx] * kv;
            }
            trow[x] = acc;
        }
    });
    // Vertical pass (misma aritmética elemento a elemento que el barrido
    // x-mayor anterior; el orden de escritura no afecta al resultado).
    out.par_chunks_mut(w).enumerate().for_each(|(y, orow)| {
        for x in 0..w {
            let mut acc = 0.0f32;
            for (k, &kv) in kernel.iter().enumerate() {
                let yy =
                    (y as isize + k as isize - radius as isize).clamp(0, h as isize - 1) as usize;
                acc += tmp[yy * w + x] * kv;
            }
            orow[x] = acc;
        }
    });
    out
}

#[tauri::command]
async fn stack_video(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    percent: f32,
    mode: String,
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
    stacking_roi: Option<Vec<u32>>,    // NEW
    normalize_colors: bool,
    is_v3: bool,                       // NEW
    target_type: String,               // NEW
    keep_full_frame: Option<bool>,     // NEW: mantener encuadre completo (no recortar)
    gpu_mode: Option<String>,          // GPU compute: "auto" | "gpu" | "cpu"
    compute_policy: Option<String>,    // contrato nuevo; prevalece sobre gpu_mode
    decode_policy: Option<String>,     // FFmpeg decode independiente
    align_rgb: Option<bool>,           // respeta el switch también en el motor global
    quality_policy: Option<String>,    // rigor AP: standard/adaptive/maximum
) -> Result<String, String> {
    state.license_manager.check_access()?;
 
    // UNIFIED ENGINE (Round 10):
    // All modes (liquid_warping, global, zenith_map) now use the "Liquid Warping V2" engine.
    // This provides Inverse Warping, Linear Match, Auto-USM, and Sharpened Bicubic Kernel to all modes.
    // Legacy mapping:
    // - "liquid_warping" / "liquid_v3": Uses custom_points (Liquid Warping).
    // - "global" or "zenith_map" / "zenith_v3": Uses empty points (Global Alignment Only).
    let effective_points = if mode == "liquid_warping" || mode == "liquid_v3" || mode == "zenith_ultimate" {
        custom_points
    } else {
        vec![] // Force Global Alignment
    };
 
    return stack_video_liquid_warping(
        app,
        state,
        path,
        percent,
        effective_points,
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
        target_type, // NEW
        keep_full_frame,
        align_rgb,
        gpu_mode,
        compute_policy,
        decode_policy,
        quality_policy,
    )
    .await;
}

#[tauri::command]
async fn apply_wavelets(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    _req_id: usize,
    u1: f32,
    u2: f32,
    u3: f32,
    u4: f32,
    u5: f32,
    w1: f32,
    w2: f32,
    w3: f32,
    w4: f32,
    w5: f32,
    w6: f32,
    d1: f32,
    d2: f32,
    d3: f32,
    d4: f32,
    d5: f32,
    d6: f32,
    gamma: f32,
    saturation: f32,
    r_x: f32,
    r_y: f32,
    b_x: f32,
    b_y: f32,
    blend: f32,
    deringing_mode: i32,
    deringing_radius: f32,
    deringing_dark: f32,
    deringing_light: f32,
    deringing_mask: bool,
    crisp: f32,
    deconv_iter: usize,
    deconv_sigma: f32,
    vc_iter: usize,
    vc_sigma: f32,
    usm_amount: f32,
    usm_radius: f32,
    lce_amount: f32,
    contrast: f32,
    brightness: f32,
    r_bal: f32,
    b_bal: f32,
    master_denoise: f32, // PHASE 23
    master_denoise_detail: f32,
    master_denoise_chroma: f32,
    use_rgb_sharpening: bool,
    edge_aware_wavelets: Option<bool>, // B: wavelets edge-aware
    psf_from_limb: Option<bool>,       // A: deconv con PSF medida
    edge_aware_strength: Option<f32>,  // B+: intensidad edge-aware (0..100)
    auto_mask: Option<f32>,            // Calidad: sharpening adaptativo por SNR
    adaptive_usm: Option<AdaptiveUsmParams>, // USM adaptativo por luminancia de entrada
    // Interactividad: recuadro de trabajo [x, y, w, h] en pixeles del master. Se
    // procesa a resolucion NATIVA (mas una guarda que se descarta), asi que lo que
    // se ve dentro es bit-identico al render completo y a la exportacion. Sustituye
    // al antiguo `preview_downscale`, que procesaba a 1/N con los mismos sigmas en
    // pixeles y por tanto amplificaba contenido espacial distinto.
    preview_roi: Option<[u32; 4]>,
    gpu_mode: Option<String>,          // Velocidad: "auto"|"gpu"|"cpu" (descomposicion GPU)
    // Modo Pureza: "protected" (historico, por defecto) | "balanced" | "pure".
    // Controla la fuerza de las protecciones automaticas (altas luces, croma,
    // frenos de deconvolucion, suelo de denoise, rodillas de USM).
    purity_mode: Option<String>,
    levels_black: Option<f32>,         // Niveles: punto negro (0..1)
    levels_white: Option<f32>,         // Niveles: punto blanco (0..1)
    levels_gamma: Option<f32>,         // Niveles: gamma medios (0.1..5)
    advanced: Option<AdvancedColorParams>,
    result_id: Option<usize>,
) -> Result<String, String> {
    state.license_manager.check_access()?;
    if let Some(expected) = result_id {
        let current = state.result_generation.load(Ordering::SeqCst);
        if expected != current {
            return Err("Resultado sustituido: se descartó una solicitud de postprocesado antigua.".into());
        }
    }
    let backend_request_id = begin_planetary_user_job(&state);
    let original = {
        let s = state.stacked_image.lock().unwrap_or_else(|e| e.into_inner());
        match &*s {
            Some(img) => img.clone(),
            None => return Err("Sin imagen".into()),
        }
    };

    let resolved_post = resolve_post_processing_contract(
        original.is_mono,
        usm_amount,
        usm_radius,
        lce_amount,
        r_bal,
        b_bal,
    );

    // ESTADISTICAS DEL MASTER COMPLETO. Se miden sobre `original` (imagen entera)
    // ANTES de cualquier recorte/reduccion y se cachean por `result_generation`.
    // Dos motivos:
    //   - Correccion: el recuadro interactivo procesa un recorte; medir p99, el
    //     pivote tonal o la PSF del limbo de el haria que mover el recuadro
    //     cambiase el resultado.
    //   - Velocidad: p99 ordena el master entero y la PSF recorre mascaras de
    //     disco/limbo. Recalcularlo en cada arrastre de slider era puro gasto.
    let wants_psf = psf_from_limb.unwrap_or(false);
    let global_stats = {
        let generation = state.result_generation.load(Ordering::SeqCst);
        let mut guard = state
            .global_stats_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let usable = guard.as_ref().is_some_and(|(gen, stats)| {
            *gen == generation && stats.matches_psf_request(wants_psf, deconv_sigma)
        });
        if !usable {
            let (stats, note) = GlobalStats::measure(
                &original.data,
                original.width,
                original.height,
                wants_psf,
                deconv_sigma,
            );
            if let Some(note) = note {
                log_to_front(&app, "INFO", note);
            }
            *guard = Some((generation, stats));
        }
        guard.as_ref().map(|(_, stats)| stats.clone()).unwrap()
    };

    // RECUADRO DE TRABAJO (interactividad). Durante el arrastre el front manda un
    // rectangulo; se procesa a resolucion NATIVA mas una guarda que luego se
    // descarta. Coste proporcional a los pixeles del recuadro, no a la resolucion
    // del master — igual que el antiguo downscale, pero con cada sigma en su
    // escala real, asi que lo que se ve dentro ES lo que exporta.
    //
    // La guarda es adaptativa: 3σ de la banda mas gruesa activa (ver `roi_guard_px`).
    // El modulo solar avanzado mide cuantiles GLOBALES que describen la escena
    // entera: el piso de ruido del paso-alto (`quantile_at(0.25)`/`(0.90)`) y el
    // techo de fondo del cielo (`p05/p25/p80`). En un recuadro sobre el interior
    // del disco no hay cielo, asi que esas medidas saldrian invalidas y el
    // recuadro NO podria ser exacto. Antes que prometer un WYSIWYG que no se
    // cumple, ahi se renderiza la imagen completa.
    let solar_needs_full_frame = advanced
        .as_ref()
        .map(|a| {
            a.solar.enabled
                && (a.solar.filament_amount.abs() > 1e-6
                    || a.solar.prominence_amount.abs() > 1e-6
                    || a.solar.background_protect.abs() > 1e-6)
        })
        .unwrap_or(false);

    let roi = preview_roi.filter(|_| !solar_needs_full_frame).and_then(|[rx, ry, rw, rh]| {
        let x = (rx as usize).min(original.width.saturating_sub(1));
        let y = (ry as usize).min(original.height.saturating_sub(1));
        let w = (rw as usize).min(original.width - x);
        let h = (rh as usize).min(original.height - y);
        // Por debajo de 64 px el recuadro no compensa el coste fijo del recorte.
        (w >= 64 && h >= 64 && (w < original.width || h < original.height))
            .then_some((x, y, w, h))
    });

    let roi_holder;
    // `visible` = offset del recuadro DENTRO del buffer procesado, para recortar
    // la guarda al final.
    let (proc_ref, pw, ph, visible): (&StackResult, usize, usize, Option<(usize, usize, usize, usize)>) =
        match roi {
            Some((vx, vy, vw, vh)) => {
                let guard = roi_guard_px(
                    &[u1, u2, u3, u4, u5],
                    &[w1, w2, w3, w4, w5, w6],
                    &[d1, d2, d3, d4, d5, d6],
                    resolved_post.lce_amount,
                    original.width,
                    original.height,
                    deconv_sigma,
                    deconv_iter,
                    vc_sigma,
                    vc_iter,
                    resolved_post.usm_radius,
                    resolved_post.usm_amount,
                    crisp,
                    deringing_radius,
                    deringing_mode,
                );
                // Expandir por la guarda, recortando contra los bordes del master.
                let cx = vx.saturating_sub(guard);
                let cy = vy.saturating_sub(guard);
                let cw = (vx + vw + guard).min(original.width) - cx;
                let ch = (vy + vh + guard).min(original.height) - cy;
                let mut crop = vec![0u16; cw * ch * 3];
                for row in 0..ch {
                    let src = ((cy + row) * original.width + cx) * 3;
                    let dst = row * cw * 3;
                    crop[dst..dst + cw * 3]
                        .copy_from_slice(&original.data[src..src + cw * 3]);
                }
                roi_holder = StackResult {
                    data: crop,
                    width: cw,
                    height: ch,
                    is_mono: original.is_mono,
                    is_surface: original.is_surface,
                };
                (&roi_holder, cw, ch, Some((vx - cx, vy - cy, vw, vh)))
            }
            None => (&original, original.width, original.height, None),
        };
    let use_roi = visible.is_some();

    // WYSIWYG: el recuadro usa el MISMO backend CPU que exportacion y lote
    // (`gpu_allowed = false` en ambas). Reservar la GPU al preview hacia que la
    // vista interactiva y el archivo exportado salieran de rutas distintas.
    let _ = &gpu_mode;
    let gpu_allowed = false;
    let protection = ProtectionProfile::from_mode(purity_mode.as_deref());

    let mut final_u16 = run_processing_pipeline(
        &app,
        &state,
        backend_request_id,
        proc_ref,
        pw,
        ph,
        [u1, u2, u3, u4, u5],
        [w1, w2, w3, w4, w5, w6],
        [d1, d2, d3, d4, d5, d6],
        gamma,
        saturation,
        r_x,
        r_y,
        b_x,
        b_y,
        deringing_mode,
        deringing_radius,
        deringing_dark,
        deringing_light,
        deringing_mask,
        crisp,
        deconv_iter,
        deconv_sigma,
        vc_iter,
        vc_sigma,
        resolved_post.usm_amount,
        resolved_post.usm_radius,
        adaptive_usm.unwrap_or_default(),
        resolved_post.lce_amount,
        blend,
        contrast,
        brightness,
        resolved_post.r_bal,
        resolved_post.b_bal,
        master_denoise,     // PHASE 23
        master_denoise_detail,
        master_denoise_chroma,
        use_rgb_sharpening, // PHASE 15
        edge_aware_wavelets.unwrap_or(false), // B
        psf_from_limb.unwrap_or(false),        // A
        edge_aware_strength.unwrap_or(50.0),   // B+
        auto_mask.unwrap_or(0.0),              // adaptativo
        gpu_allowed,                           // Velocidad: GPU wavelets (paridad+fallback)
        levels_black.unwrap_or(0.0),           // Niveles
        levels_white.unwrap_or(1.0),
        levels_gamma.unwrap_or(1.0),
        Some(&global_stats),
        protection,
    );

    if final_u16.is_empty() {
        return Err("Cancelled".into());
    }
    if check_cancel(&state, backend_request_id) {
        return Err("Cancelled".into());
    }

    if let Some(ref advanced_params) = advanced {
        apply_advanced_postprocess_with(
            &mut final_u16,
            pw,
            ph,
            original.is_mono,
            advanced_params,
            protection,
        );
    }

    if let Some(expected) = result_id {
        let current = state.result_generation.load(Ordering::SeqCst);
        if expected != current {
            return Err("Resultado sustituido: se descartó una vista calculada fuera de sesión.".into());
        }
    }

    // El render del recuadro cubre solo una zona; no debe sustituir el búfer
    // 16-bit de tamaño completo que consumen histograma, cuentagotas y export.
    // Al soltar el control llega el render completo y entonces sí se publica.
    if !use_roi {
        let mut processed = state.processed_image.lock().unwrap();
        *processed = Some(StackResult {
            data: final_u16.clone(),
            width: original.width,
            height: original.height,
            is_mono: original.is_mono,
            is_surface: original.is_surface,
        });
    }

    emit_progress(&app, "Generando vista...", 97.0, None);
    // Descartar la guarda: se proceso solo para que las colas de las Gaussianas
    // entraran correctas dentro de la zona visible.
    let (vis, out_w, out_h) = match visible {
        Some((ox, oy, vw, vh)) => {
            let mut cropped = vec![0u16; vw * vh * 3];
            for row in 0..vh {
                let src = ((oy + row) * pw + ox) * 3;
                let dst = row * vw * 3;
                cropped[dst..dst + vw * 3].copy_from_slice(&final_u16[src..src + vw * 3]);
            }
            (to_8bit_visual(&cropped, 1.0), vw, vh)
        }
        None => (to_8bit_visual(&final_u16, 1.0), original.width, original.height),
    };
    let mut png = Vec::new();
    image::png::PngEncoder::new(&mut Cursor::new(&mut png))
        .encode(&vis, out_w as u32, out_h as u32, image::ColorType::Rgb8)
        .map_err(|e| e.to_string())?;
    if check_cancel(&state, backend_request_id) {
        return Err("Cancelled".into());
    }
    emit_progress(&app, "Listo", 100.0, None);
    // PR-2.5: el lazo MÁS caliente de la app (arrastre de sliders) enviaba
    // un data-URL base64 a resolución completa por IPC en CADA render
    // (decenas de MB por tick en mosaicos 4K, retenidos en el heap del
    // WebView). Archivo temporal + asset protocol como el apilado; nombre
    // único por render (el WebView cachea por URL). Los previews 1:1 se
    // conservan para deshacer/A-B; los reducidos de arrastre son transitorios.
    // Fallback a base64 si el temp falla.
    let preview_tag = if use_roi { "editor_fast" } else { "editor" };
    let preview = save_preview_png_to_temp(&png, preview_tag).unwrap_or_else(|| {
        format!(
            "data:image/png;base64,{}",
            general_purpose::STANDARD.encode(&png)
        )
    });
    prune_editor_previews();
    Ok(preview)
}

#[tauri::command]
async fn analyze_psf(state: State<'_, AppState>) -> Result<PsfResult, String> {
    state.license_manager.check_access()?;
    let original = {
        let s = state.stacked_image.lock().unwrap_or_else(|e| e.into_inner());
        match &*s {
            Some(img) => img.clone(),
            None => return Err("No hay imagen apilada.".into()),
        }
    };

    let len = original.data.len() / 3;
    let mut g_f = vec![0.0f32; len];
    for i in 0..len {
        g_f[i] = original.data[i * 3 + 1] as f32;
    }

    let sigma = auto_detect_sigma(&g_f, original.width, original.height).clamp(0.6, 2.2);
    let stride = (len / 32_768).max(1);
    let mut residuals = Vec::with_capacity((len / stride).max(1));
    if original.width > 2 && original.height > 2 {
        let start = original.width + 1;
        let end = len.saturating_sub(original.width + 1);
        for index in (start..end).step_by(stride) {
            let local = (g_f[index - 1]
                + g_f[index + 1]
                + g_f[index - original.width]
                + g_f[index + original.width])
                * 0.25;
            residuals.push((g_f[index] - local).abs());
        }
    }
    residuals.sort_by(|left, right| left.total_cmp(right));
    let noise_sigma = residuals
        .get(residuals.len().saturating_sub(1) / 2)
        .copied()
        .unwrap_or(0.0)
        * 1.4826;
    let p90 = residuals
        .get(((residuals.len().saturating_sub(1)) as f32 * 0.90).round() as usize)
        .copied()
        .unwrap_or(noise_sigma);
    let confidence = ((p90 / noise_sigma.max(1.0) - 1.4) / 7.0).clamp(0.18, 0.96);
    let base_iterations: f32 = if sigma > 1.8 {
        14.0
    } else if sigma > 1.1 {
        11.0
    } else {
        8.0
    };
    let surface_bonus: f32 = if original.is_surface { 1.0 } else { 0.0 };
    let iterations = (base_iterations * (0.72 + confidence * 0.28) + surface_bonus)
        .round()
        .clamp(6.0, 16.0) as usize;

    Ok(PsfResult {
        sigma: (sigma * 10.0).round() / 10.0,
        iterations,
        confidence,
        noise_sigma: noise_sigma / 65535.0,
        msg: format!(
            "PSF {:.2}px | confianza {:.0}% | ruido {:.4}% | RL {} iteraciones",
            sigma,
            confidence * 100.0,
            noise_sigma / 655.35,
            iterations
        ),
    })
}

#[tauri::command]
async fn save_final_image(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    format_idx: i32,
    u1: f32,
    u2: f32,
    u3: f32,
    u4: f32,
    u5: f32,
    w1: f32,
    w2: f32,
    w3: f32,
    w4: f32,
    w5: f32,
    w6: f32,
    d1: f32,
    d2: f32,
    d3: f32,
    d4: f32,
    d5: f32,
    d6: f32,
    gamma: f32,
    saturation: f32,
    r_x: f32,
    r_y: f32,
    b_x: f32,
    b_y: f32,
    blend: f32,
    deringing_mode: i32,
    deringing_radius: f32,
    deringing_dark: f32,
    deringing_light: f32,
    deringing_mask: bool,
    crisp: f32,
    deconv_iter: usize,
    deconv_sigma: f32,
    vc_iter: usize,
    vc_sigma: f32,
    usm_amount: f32,
    usm_radius: f32,
    lce_amount: f32,
    contrast: f32,
    brightness: f32,
    r_bal: f32,
    b_bal: f32,
    master_denoise: f32, // PHASE 23
    master_denoise_detail: f32,
    master_denoise_chroma: f32,
    use_rgb_sharpening: bool,
    edge_aware_wavelets: Option<bool>, // B: wavelets edge-aware
    psf_from_limb: Option<bool>,       // A: deconv con PSF medida
    edge_aware_strength: Option<f32>,  // B+: intensidad edge-aware (0..100)
    auto_mask: Option<f32>,            // Calidad: sharpening adaptativo por SNR
    adaptive_usm: Option<AdaptiveUsmParams>, // USM adaptativo por luminancia de entrada
    levels_black: Option<f32>,         // Niveles: punto negro (0..1)
    levels_white: Option<f32>,         // Niveles: punto blanco (0..1)
    levels_gamma: Option<f32>,         // Niveles: gamma medios (0.1..5)
    advanced: Option<AdvancedColorParams>,
    // Modo Pureza: "protected" (historico, por defecto) | "balanced" | "pure".
    // Controla la fuerza de las protecciones automaticas (altas luces, croma,
    // frenos de deconvolucion, suelo de denoise, rodillas de USM).
    purity_mode: Option<String>,
) -> Result<String, String> {
    state.license_manager.check_access()?;
    let export_request_id = begin_planetary_user_job(&state);
    let export_token = PlanetaryJobToken::for_app(
        &app,
        export_request_id,
        state.cancel_requested.clone(),
    );
    if format_idx == 1 && !state.license_manager.is_pro() {
        return Err(
            "Guardar en TIFF 16-bit requiere licencia PRO o periodo de prueba activo.".into(),
        );
    }

    let original = {
        let s = state.stacked_image.lock().unwrap_or_else(|e| e.into_inner());
        match &*s {
            Some(img) => img.clone(),
            None => return Err("Sin imagen".into()),
        }
    };
    emit_progress(&app, "Procesando final...", 0.0, None);

    let resolved_post = resolve_post_processing_contract(
        original.is_mono,
        usm_amount,
        usm_radius,
        lce_amount,
        r_bal,
        b_bal,
    );

    // Export: siempre CPU (render final exacto, sin dependencia de GPU).
    let gpu_allowed = false;
    let mut final_u16 = run_processing_pipeline(
        &app,
        &state,
        export_request_id,
        &original,
        original.width,
        original.height,
        [u1, u2, u3, u4, u5],
        [w1, w2, w3, w4, w5, w6],
        [d1, d2, d3, d4, d5, d6],
        gamma,
        saturation,
        r_x,
        r_y,
        b_x,
        b_y,
        deringing_mode,
        deringing_radius,
        deringing_dark,
        deringing_light,
        // PR-1.7 (WYSIWYG): el export respeta el flag del usuario. Antes se
        // forzaba a false y lo guardado NO coincidía con el preview del
        // editor cuando la máscara de deringing estaba activa.
        deringing_mask,
        crisp,
        deconv_iter,
        deconv_sigma,
        vc_iter,
        vc_sigma,
        resolved_post.usm_amount,
        resolved_post.usm_radius,
        adaptive_usm.unwrap_or_default(),
        resolved_post.lce_amount,
        blend,
        contrast,
        brightness,
        resolved_post.r_bal,
        resolved_post.b_bal,
        master_denoise,     // PHASE 23
        master_denoise_detail,
        master_denoise_chroma,
        use_rgb_sharpening, // PHASE 15
        edge_aware_wavelets.unwrap_or(false), // B
        psf_from_limb.unwrap_or(false),        // A
        edge_aware_strength.unwrap_or(50.0),   // B+
        auto_mask.unwrap_or(0.0),              // adaptativo
        gpu_allowed,                           // Velocidad: GPU wavelets (paridad+fallback)
        levels_black.unwrap_or(0.0),           // Niveles
        levels_white.unwrap_or(1.0),
        levels_gamma.unwrap_or(1.0),
        // Exportacion: `original` YA es el master completo → medir aqui es
        // correcto y mantiene el resultado bit-identico al historico.
        None,
        ProtectionProfile::from_mode(purity_mode.as_deref()),
    );

    if final_u16.is_empty() {
        return Err("Error en el procesado (Cancelado)".into());
    }
    if check_cancel(&state, export_request_id) {
        return Err("Error en el procesado (Cancelado)".into());
    }

    if let Some(ref advanced_params) = advanced {
        apply_advanced_postprocess_with(
            &mut final_u16,
            original.width,
            original.height,
            original.is_mono,
            advanced_params,
            ProtectionProfile::from_mode(purity_mode.as_deref()),
        );
    }

    emit_progress(&app, "Guardando...", 97.0, None);
    let (name, label, mut staged) = match format_idx {
        2 => {
            let name = format!("{}_Final.fits", path);
            let staged = StagedPlanetaryArtifact::encode_rgb16_fits(
                PathBuf::from(&name),
                &final_u16,
                original.width,
                original.height,
            )?;
            (name, "FITS 16-bit", staged)
        }
        0 => {
            let name = format!("{}_Final.png", path);
            let staged = StagedPlanetaryArtifact::encode_rgb16_png(
                PathBuf::from(&name),
                &final_u16,
                original.width,
                original.height,
            )?;
            (name, "PNG 16-bit", staged)
        }
        _ => {
            let name = format!("{}_Final_16bit.tiff", path);
            let staged = StagedPlanetaryArtifact::encode_rgb16_tiff(
                PathBuf::from(&name),
                &final_u16,
                original.width,
                original.height,
            )?;
            (name, "TIFF 16-bit", staged)
        }
    };
    planetary_derotation_checkpoint(&export_token, "la codificacion de la exportacion")?;
    with_current_planetary_job(
        &state.planetary_generation_gate,
        &export_token,
        "la publicacion de la exportacion",
        || staged.publish(true),
    )??;
    emit_progress(&app, "Listo", 100.0, None);
    Ok(format!("{} Guardado: {}", label, name))
}

#[tauri::command]
async fn export_mosaic_result(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    source_path: String,
    format_idx: i32,
) -> Result<String, String> {
    state.license_manager.check_access()?;
    let export_request_id = begin_planetary_user_job(&state);
    let export_token = PlanetaryJobToken::for_app(
        &app,
        export_request_id,
        state.cancel_requested.clone(),
    );
    if format_idx == 1 && !state.license_manager.is_pro() {
        return Err(
            "Guardar en TIFF 16-bit requiere licencia PRO o periodo de prueba activo.".into(),
        );
    }

    let original = {
        let s = state.stacked_image.lock().unwrap_or_else(|e| e.into_inner());
        match &*s {
            Some(img) => img.clone(),
            None => return Err("No hay mosaico generado para guardar.".into()),
        }
    };

    if original.data.len() != original.width * original.height * 3 {
        return Err("El mosaico en memoria no tiene dimensiones válidas.".into());
    }

    emit_progress(&app, "Guardando mosaico...", 5.0, None);

    let source = Path::new(&source_path);
    let parent_dir = source
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Mosaic_Result");

    let (out_path, label, mut staged) = if format_idx == 0 {
        let out_path = parent_dir.join(format!("{}_Export_16bit.png", stem));
        let staged = StagedPlanetaryArtifact::encode_rgb16_png(
            out_path.clone(),
            &original.data,
            original.width,
            original.height,
        )?;
        (out_path, "PNG 16-bit", staged)
    } else {
        let out_path = parent_dir.join(format!("{}_Export_16bit.tiff", stem));
        let staged = StagedPlanetaryArtifact::encode_rgb16_tiff(
            out_path.clone(),
            &original.data,
            original.width,
            original.height,
        )?;
        (out_path, "TIFF 16-bit", staged)
    };
    planetary_derotation_checkpoint(&export_token, "la codificacion del mosaico exportado")?;
    with_current_planetary_job(
        &state.planetary_generation_gate,
        &export_token,
        "la publicacion del mosaico exportado",
        || staged.publish(true),
    )??;
    emit_progress(&app, "Listo", 100.0, None);
    Ok(format!(
        "{} guardado: {}",
        label,
        clean_windows_path(out_path)
    ))
}

// --- ADVANCED BLIND STITCHING IMPLEMENTATION ---

// Helper struct for Feature Patches
#[derive(Clone, Debug)]
struct FeaturePoint {
    x: f32, // Akaze uses subpixel precision
    y: f32,
    // angle removed as it was unused
    descriptor: Vec<u8>, // Akaze binary descriptor (usually 61 bytes or similar depending on config)
}

impl FeaturePoint {
    // Hamming Distance for binary descriptors
    fn distance(&self, other: &FeaturePoint) -> u32 {
        let len = self.descriptor.len().min(other.descriptor.len());
        let word_end = len & !7;
        let mut d = 0u32;
        // AKAZE suele entregar 61 bytes: procesar 8 por instrucción reduce
        // drásticamente el hot loop N×M y conserva exactamente el Hamming.
        for offset in (0..word_end).step_by(8) {
            let a = u64::from_ne_bytes(
                self.descriptor[offset..offset + 8]
                    .try_into()
                    .expect("chunk AKAZE de 8 bytes"),
            );
            let b = u64::from_ne_bytes(
                other.descriptor[offset..offset + 8]
                    .try_into()
                    .expect("chunk AKAZE de 8 bytes"),
            );
            d += (a ^ b).count_ones();
        }
        for offset in word_end..len {
            d += (self.descriptor[offset] ^ other.descriptor[offset]).count_ones();
        }
        d
    }
}

struct ImageNode {
    idx: usize,
    width: u32,
    height: u32,
    // PR-1.8: posiciones SUBPÍXEL. El redondeo a entero del registro
    // introducía hasta ±0.5 px de desregistro por costura (micro-seams).
    global_x: f32,
    global_y: f32,
    placed: bool,
    img_gray_stretched: image::GrayImage,
    features: Vec<FeaturePoint>,
    original_path: String,
    // Índice en MosaicTileCache. La imagen 16-bit seleccionada se conserva
    // entre AKAZE y la fusión: los vídeos no vuelven a escanear/decodificar.
    cache_index: usize,
}

#[derive(Clone, Copy, Debug)]
struct MatchEdge {
    target_idx: usize,
    dx: f32,
    dy: f32,
    score: usize,
}

const MOSAIC_ACCUMULATOR_BYTES_PER_PIXEL: u64 = 16; // 4 canales f32
const MOSAIC_OUTPUT_BYTES_PER_PIXEL: u64 = 6; // RGB u16
// Preview RGB16+RGB8 (≤2400²), buffers internos de encoder y allocator.
const MOSAIC_FIXED_SCRATCH_BYTES: u64 = 96 * 1024 * 1024;
const MOSAIC_DISK_RESERVE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug)]
enum MosaicTileStorage {
    Ram(Vec<u16>),
    Spill(PathBuf),
}

#[derive(Debug)]
struct MosaicTileCacheEntry {
    width: u32,
    height: u32,
    storage: MosaicTileStorage,
}

/// Caché 16-bit acotada. Mantiene en RAM lo que cabe en un presupuesto pequeño
/// derivado de la memoria libre y derrama el resto como RGBA16 crudo. El spill
/// evita una segunda decodificación (especialmente cara en MP4/MOV) y solo carga
/// una tesela a la vez durante la fusión.
#[derive(Debug)]
struct MosaicTileCache {
    entries: Vec<MosaicTileCacheEntry>,
    ram_budget: u64,
    ram_used: u64,
    spill_dir: PathBuf,
    max_spilled_tile_bytes: u64,
}

impl MosaicTileCache {
    fn new(available_ram: u64) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        // Como el tamaño final del lienzo aún no se conoce, el caché solo toma
        // 1/8 de la RAM libre (máx. 1.5 GiB). El preflight posterior trabaja con
        // la RAM que realmente queda después de conservar grises/features.
        let ram_budget = (available_ram / 8).min(1536 * 1024 * 1024);
        Self {
            entries: Vec::new(),
            ram_budget,
            ram_used: 0,
            spill_dir: std::env::temp_dir().join(format!(
                "astro-stacker-mosaic-{}-{stamp}",
                std::process::id()
            )),
            max_spilled_tile_bytes: 0,
        }
    }

    fn insert(&mut self, width: u32, height: u32, data: Vec<u16>) -> Result<usize, String> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|v| v.checked_mul(4))
            .ok_or("Dimensiones de tesela RGBA16 fuera de rango")?;
        if data.len() != expected {
            return Err(format!(
                "Tesela RGBA16 inválida: {} muestras, se esperaban {expected}",
                data.len()
            ));
        }
        let bytes = (data.len() as u64)
            .checked_mul(2)
            .ok_or("Tamaño de tesela fuera de rango")?;
        let index = self.entries.len();
        let storage = if self.ram_used.saturating_add(bytes) <= self.ram_budget {
            self.ram_used += bytes;
            MosaicTileStorage::Ram(data)
        } else {
            fs::create_dir_all(&self.spill_dir)
                .map_err(|e| format!("No se pudo crear el caché temporal del mosaico: {e}"))?;
            if let Some(free) = available_disk_bytes(&self.spill_dir) {
                if free < bytes.saturating_add(MOSAIC_DISK_RESERVE_BYTES) {
                    return Err(format!(
                        "Sin espacio para el caché temporal del mosaico: se necesitan {:.1} MB para la siguiente tesela y se preservan 512 MB de seguridad.",
                        bytes as f64 / 1_048_576.0
                    ));
                }
            }
            let path = self.spill_dir.join(format!("tile_{index:05}.rgba16"));
            let file = File::create(&path)
                .map_err(|e| format!("No se pudo crear {}: {e}", path.display()))?;
            let mut writer = BufWriter::new(file);
            writer
                .write_all(bytemuck::cast_slice(&data))
                .and_then(|_| writer.flush())
                .map_err(|e| format!("No se pudo escribir el caché 16-bit: {e}"))?;
            self.max_spilled_tile_bytes = self.max_spilled_tile_bytes.max(bytes);
            MosaicTileStorage::Spill(path)
        };
        self.entries.push(MosaicTileCacheEntry {
            width,
            height,
            storage,
        });
        Ok(index)
    }

    fn load(&self, index: usize) -> Result<Cow<'_, [u16]>, String> {
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| format!("Índice de caché de mosaico inválido: {index}"))?;
        match &entry.storage {
            MosaicTileStorage::Ram(data) => Ok(Cow::Borrowed(data)),
            MosaicTileStorage::Spill(path) => {
                let samples = (entry.width as usize)
                    .checked_mul(entry.height as usize)
                    .and_then(|v| v.checked_mul(4))
                    .ok_or("Dimensiones del caché RGBA16 fuera de rango")?;
                let mut data = try_zeroed_vec::<u16>(samples, "tesela 16-bit temporal")?;
                let file = File::open(path)
                    .map_err(|e| format!("No se pudo abrir el caché {}: {e}", path.display()))?;
                let mut reader = BufReader::new(file);
                reader
                    .read_exact(bytemuck::cast_slice_mut(&mut data))
                    .map_err(|e| format!("Caché 16-bit truncado {}: {e}", path.display()))?;
                Ok(Cow::Owned(data))
            }
        }
    }
}

impl Drop for MosaicTileCache {
    fn drop(&mut self) {
        if self.spill_dir.exists() {
            let _ = fs::remove_dir_all(&self.spill_dir);
        }
    }
}

fn available_disk_bytes(path: &Path) -> Option<u64> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|disk| target.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
        .map(|disk| disk.available_space())
}

fn try_zeroed_vec<T: Default + Clone>(len: usize, label: &str) -> Result<Vec<T>, String> {
    let mut out = Vec::new();
    out.try_reserve_exact(len).map_err(|_| {
        format!(
            "No se pudo reservar memoria para {label} ({:.1} millones de elementos)",
            len as f64 / 1_000_000.0
        )
    })?;
    out.resize(len, T::default());
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MosaicMemoryPlan {
    pixels: u64,
    peak_bytes: u64,
    reserve_bytes: u64,
}

/// Preflight puro del lienzo. `available_ram` se consulta después de crear el
/// caché y los mapas AKAZE, por lo que representa la memoria que queda de verdad.
fn plan_mosaic_canvas_memory(
    width: u32,
    height: u32,
    aggregate_tile_pixels: u64,
    max_spilled_tile_bytes: u64,
    largest_tile_pixels: u64,
    available_ram: u64,
) -> Result<MosaicMemoryPlan, String> {
    let pixels = (width as u64)
        .checked_mul(height as u64)
        .ok_or("Dimensiones del lienzo fuera de rango")?;
    if pixels == 0 {
        return Err("El lienzo de mosaico está vacío".to_string());
    }
    // Un bounding-box con más de 8× el área agregada de las teselas contiene
    // casi todo espacio vacío y suele indicar una correspondencia espuria.
    if aggregate_tile_pixels > 0 && pixels > aggregate_tile_pixels.saturating_mul(8) {
        return Err(format!(
            "Lienzo geométricamente implausible ({width}x{height}): su área supera 8× el área de las teselas. Revisa las coincidencias/solapes."
        ));
    }

    let accum_bytes = pixels
        .checked_mul(MOSAIC_ACCUMULATOR_BYTES_PER_PIXEL)
        .ok_or("Acumulador de mosaico fuera de rango")?;
    let normalize_peak = pixels
        .checked_mul(MOSAIC_ACCUMULATOR_BYTES_PER_PIXEL + MOSAIC_OUTPUT_BYTES_PER_PIXEL)
        .ok_or("Salida de mosaico fuera de rango")?;
    // Durante el blend solo una tesela derramada vuelve a RAM. La mediana de
    // ganancia muestrea 1/16 de sus píxeles como f32 (0.25 B/píxel de tesela).
    let ratio_scratch = largest_tile_pixels / 4;
    let blend_peak = accum_bytes
        .checked_add(max_spilled_tile_bytes)
        .and_then(|v| v.checked_add(ratio_scratch))
        .ok_or("Pico de memoria del mosaico fuera de rango")?;
    let peak_bytes = normalize_peak
        .max(blend_peak)
        .saturating_add(MOSAIC_FIXED_SCRATCH_BYTES);
    // Conserva entre 256 MiB y 1 GiB para SO/WebView/allocator, pero nunca más
    // de la mitad de la RAM que queda en una máquina muy ajustada.
    let reserve_bytes = (available_ram / 5)
        .clamp(256 * 1024 * 1024, 1024 * 1024 * 1024)
        .min(available_ram / 2);
    let usable = available_ram.saturating_sub(reserve_bytes);
    if peak_bytes > usable {
        return Err(format!(
            "RAM insuficiente para mosaico {width}x{height}: pico estimado {:.2} GB, disponibles de forma segura {:.2} GB (reserva del sistema {:.2} GB). Reduce paneles/drizzle, cierra otras aplicaciones o procesa por secciones.",
            peak_bytes as f64 / 1_073_741_824.0,
            usable as f64 / 1_073_741_824.0,
            reserve_bytes as f64 / 1_073_741_824.0
        ));
    }
    Ok(MosaicMemoryPlan {
        pixels,
        peak_bytes,
        reserve_bytes,
    })
}

#[inline]
fn rgba16_at(data: &[u16], width: u32, x: u32, y: u32) -> [u16; 4] {
    let i = (y as usize * width as usize + x as usize) * 4;
    [data[i], data[i + 1], data[i + 2], data[i + 3]]
}

fn normalize_mosaic_accumulators(
    acc_r: &[f32],
    acc_g: &[f32],
    acc_b: &[f32],
    acc_w: &[f32],
) -> Result<Vec<u16>, String> {
    let len = acc_w.len();
    if acc_r.len() != len || acc_g.len() != len || acc_b.len() != len {
        return Err("Buffers de composición de mosaico inconsistentes".to_string());
    }
    let out_len = len
        .checked_mul(3)
        .ok_or("Salida RGB de mosaico fuera de rango")?;
    let mut out = try_zeroed_vec::<u16>(out_len, "salida RGB16 del mosaico")?;
    out.par_chunks_mut(3)
        .enumerate()
        .for_each(|(idx, pixel)| {
            let wsum = acc_w[idx];
            if wsum > 0.0 {
                pixel[0] = (acc_r[idx] / wsum + 0.5).clamp(0.0, 65535.0) as u16;
                pixel[1] = (acc_g[idx] / wsum + 0.5).clamp(0.0, 65535.0) as u16;
                pixel[2] = (acc_b[idx] / wsum + 0.5).clamp(0.0, 65535.0) as u16;
            }
        });
    Ok(out)
}

/// El mosaico siempre se almacena como RGB16, incluso cuando las tres bandas
/// proceden de una captura mono. Comprobar todo el buffer evita clasificar una
/// imagen de color por un único píxel neutro o por el fondo negro del lienzo.
fn mosaic_rgb16_is_mono(rgb: &[u16]) -> bool {
    !rgb.is_empty()
        && rgb.len() % 3 == 0
        && rgb
            .par_chunks_exact(3)
            .all(|pixel| pixel[0] == pixel[1] && pixel[1] == pixel[2])
}

#[cfg(test)]
mod mosaic_resource_tests {
    use super::{
        auto_stretch_luma16_to_gray, mosaic_rgb16_is_mono,
        normalize_mosaic_accumulators, plan_mosaic_canvas_memory, FeaturePoint,
    };

    #[test]
    fn word_hamming_matches_scalar_for_akaze_descriptor() {
        let a: Vec<u8> = (0..61).map(|i| (i * 17 + 3) as u8).collect();
        let b: Vec<u8> = (0..61).map(|i| (i * 29 + 11) as u8).collect();
        let expected: u32 = a
            .iter()
            .zip(&b)
            .map(|(&x, &y)| (x ^ y).count_ones())
            .sum();
        let fa = FeaturePoint {
            x: 0.0,
            y: 0.0,
            descriptor: a,
        };
        let fb = FeaturePoint {
            x: 0.0,
            y: 0.0,
            descriptor: b,
        };
        assert_eq!(fa.distance(&fb), expected);
    }

    #[test]
    fn canvas_budget_uses_real_available_ram() {
        let tiles = 4 * 4096_u64 * 4096;
        let ok = plan_mosaic_canvas_memory(
            8192,
            8192,
            tiles,
            0,
            4096 * 4096,
            4 * 1024 * 1024 * 1024,
        )
        .unwrap();
        assert_eq!(ok.pixels, 8192 * 8192);
        assert!(ok.peak_bytes > ok.pixels * 22);

        let low_ram = plan_mosaic_canvas_memory(
            8192,
            8192,
            tiles,
            0,
            4096 * 4096,
            1024 * 1024 * 1024,
        );
        assert!(low_ram.unwrap_err().contains("RAM insuficiente"));
    }

    #[test]
    fn canvas_budget_rejects_sparse_bad_match() {
        let err = plan_mosaic_canvas_memory(
            30_000,
            30_000,
            2 * 2048_u64 * 2048,
            0,
            2048 * 2048,
            64 * 1024 * 1024 * 1024,
        )
        .unwrap_err();
        assert!(err.contains("geométricamente implausible"));
    }

    #[test]
    fn parallel_composition_is_bit_exact_with_reference() {
        let r = [100.0, 40_000.0, 65_535.0, 10.0];
        let g = [300.0, 20_000.0, 90_000.0, 20.0];
        let b = [500.0, 10_000.0, -25.0, 30.0];
        let w = [2.0, 0.5, 1.0, 0.0];
        let optimized = normalize_mosaic_accumulators(&r, &g, &b, &w).unwrap();
        let mut reference = vec![0u16; 12];
        for i in 0..4 {
            if w[i] > 0.0 {
                reference[i * 3] = (r[i] / w[i] + 0.5).clamp(0.0, 65535.0) as u16;
                reference[i * 3 + 1] =
                    (g[i] / w[i] + 0.5).clamp(0.0, 65535.0) as u16;
                reference[i * 3 + 2] =
                    (b[i] / w[i] + 0.5).clamp(0.0, 65535.0) as u16;
            }
        }
        assert_eq!(optimized, reference);
    }

    #[test]
    fn feature_stretch_preserves_low_16_bit_signal() {
        let mut source = image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_pixel(
            64,
            64,
            image::Luma([128]),
        );
        for y in 16..48 {
            for x in 16..48 {
                source.put_pixel(x, y, image::Luma([768]));
            }
        }
        let stretched = auto_stretch_luma16_to_gray(&source);
        assert!(stretched.get_pixel(32, 32)[0] > stretched.get_pixel(0, 0)[0] + 200);
    }

    #[test]
    fn mosaic_mono_detection_scans_every_pixel() {
        let mut rgb = vec![0u16; 128 * 3];
        for (index, pixel) in rgb.chunks_exact_mut(3).enumerate() {
            let value = index as u16 * 17;
            pixel.copy_from_slice(&[value, value, value]);
        }
        assert!(mosaic_rgb16_is_mono(&rgb));

        // El centro y el fondo siguen siendo neutros; un único píxel de color
        // basta para demostrar que no es una señal mono duplicada.
        rgb[7 * 3 + 1] = rgb[7 * 3 + 1].saturating_add(1);
        assert!(!mosaic_rgb16_is_mono(&rgb));
    }
}

// === AKAZE HELPERS (No explicit helpers needed, using crate directly) ===

/// Un mosaico se construye con masters apilados, nunca con un frame elegido de
/// una captura. Aceptar SER/AVI/MP4 aquí degradaba silenciosamente miles de
/// frames lineales a una sola previsualización RGB8 estirada.
fn load_mosaic_master_image(path_str: &str) -> Result<DynamicImage, String> {
    let path = Path::new(path_str);
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "png" | "tif" | "tiff") {
        return Err(format!(
            "{} no es un master PNG/TIFF. Analiza y apila la captura antes de unirla al mosaico.",
            path_str
        ));
    }
    image::open(path).map_err(|error| format!("Error cargando master {}: {error}", path_str))
}

#[tauri::command]
async fn load_image_thumbnail(path: String) -> Result<AnalysisResult, String> {
    // Check if file exists
    if !std::path::Path::new(&path).exists() {
        return Err(format!("File not found: {}", path));
    }

    // Open image
    let img = image::open(&path).map_err(|e| format!("Failed to open image: {}", e))?;

    // Create thumbnail (resize if too large, e.g. > 800px)
    let (w, h) = (img.width() as usize, img.height() as usize);
    let (new_w, new_h) = if w > 800 {
        let ratio = h as f32 / w as f32;
        (800, (800.0 * ratio) as u32)
    } else {
        (w as u32, h as u32)
    };

    let thumb = img.resize(new_w, new_h, image::imageops::FilterType::Lanczos3);

    // CRITICAL FIX: Convert to 8-bit RGB before encoding
    // This prevents rainbow artifacts when loading 16-bit TIFFs
    // The issue was that write_to() on a 16-bit image encodes 16-bit PNG,
    // but browser/frontend expects 8-bit, causing byte misinterpretation
    let thumb_8bit = DynamicImage::ImageRgb8(thumb.to_rgb8());

    // Encode to PNG
    let mut buf = Vec::new();
    let mut cursor = Cursor::new(&mut buf);
    thumb_8bit
        .write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| format!("Failed to encode thumbnail: {}", e))?;

    let b64 = general_purpose::STANDARD.encode(&buf);

    // Return compatible structure
    Ok(AnalysisResult {
        metadata: VideoMetadata {
            width: w,
            height: h,
            frame_count: 1,
            bpp: 3,
            color_id: 0,
            pattern_name: "RGB".to_string(),
            file_size_mb: 0.0,
            is_color: true,
        },
        stats: VideoStats {
            min_pixel: 0,
            max_pixel: 255,
            avg_brightness: 0.0,
            dynamic_range_pct: 0.0,
            best_score: 0.0,
            worst_score: 0.0,
            avg_quality: 0.0,
            quality_stability: 0.0,
            std_dev: 0.0,
            entropy: 0.0,
        }, // Dummy stats
        quality_graph: vec![],
        preview_base64: b64,
        path: "".to_string(),
        recommended_pct: 0.0,
        ap_points: Vec::new(),
        best_frame_idx: 0,
        execution_plan: None,
        stage_telemetry: Vec::new(),
    })
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct DerotationDiscDto {
    cx: f64,
    cy: f64,
    radius_x: f64,
    radius_y: f64,
    angle_deg: f64,
    phase: f64,
}

impl From<crate::derotation::PlanetDisc> for DerotationDiscDto {
    fn from(disc: crate::derotation::PlanetDisc) -> Self {
        Self {
            cx: disc.cx,
            cy: disc.cy,
            radius_x: disc.radius_x,
            radius_y: disc.radius_y,
            angle_deg: disc.angle_deg,
            phase: disc.phase,
        }
    }
}

impl From<&DerotationDiscDto> for crate::derotation::PlanetDisc {
    fn from(disc: &DerotationDiscDto) -> Self {
        Self {
            cx: disc.cx,
            cy: disc.cy,
            radius_x: disc.radius_x,
            radius_y: disc.radius_y,
            angle_deg: disc.angle_deg,
            phase: disc.phase,
        }
    }
}

#[derive(serde::Serialize)]
struct PlanetaryDerotationPreflight {
    width: usize,
    height: usize,
    file_name: String,
    source_size_bytes: u64,
    preview_base64: String,
    detected_disc: DerotationDiscDto,
    diagnostics: DerotationDiagnostics,
    suggested_planet: String,
    capture_time: String,
    reference_time: String,
    cm1: f64,
    cm2: f64,
    cm3: f64,
    b0_deg: f64,
    north_angle_deg: f64,
    phase_angle_deg: f64,
    apparent_diameter_arcsec: f64,
    distance_au: f64,
    source_kind: String,
}

#[derive(serde::Serialize)]
struct PlanetaryDerotationResult {
    output_path: String,
    preview_base64: String,
    width: usize,
    height: usize,
    file_name: String,
    planet: String,
    cm_system: usize,
    delta_deg: f64,
    detected_disc: DerotationDiscDto,
    diagnostics: DerotationDiagnostics,
    b0_deg: f64,
    north_angle_deg: f64,
}

#[derive(Clone, serde::Serialize)]
struct DerotationDiagnostics {
    confidence: f64,
    classification: String,
    can_apply: bool,
    coverage: f64,
    contrast_ratio: f64,
    edge_margin_px: f64,
    radius_px: f64,
    aspect_ratio: f64,
    expected_aspect_ratio: f64,
    delta_deg: f64,
    time_source: String,
    duration_sec: f64,
    fps: f64,
    b0_deg: f64,
    north_angle_deg: f64,
    phase_angle_deg: f64,
    apparent_diameter_arcsec: f64,
    distance_au: f64,
    warnings: Vec<String>,
}

#[derive(serde::Serialize)]
struct PlanetaryDiscDetectionResult {
    detected_disc: DerotationDiscDto,
    diagnostics: DerotationDiagnostics,
}

#[derive(serde::Serialize)]
struct PlanetaryDerotationSequenceResult {
    output_paths: Vec<String>,
    reference_frame: usize,
    reference_time_jd: f64,
    time_span_sec: f64,
    warnings: Vec<String>,
}

#[derive(serde::Serialize)]
struct PlanetaryDerotationFusionResult {
    output_path: String,
    preview_base64: String,
    width: usize,
    height: usize,
    planet: String,
    cm_system: usize,
    frame_count: usize,
    reference_frame: usize,
    reference_time_jd: f64,
    time_span_sec: f64,
    detected_disc: DerotationDiscDto,
    diagnostics: DerotationDiagnostics,
    b0_deg: f64,
    north_angle_deg: f64,
    weights: Vec<f64>,
    normalization_gains: Vec<[f64; 3]>,
    rejected_pixel_fraction: f64,
    warnings: Vec<String>,
}

fn derot_load_rgb16_image(path: &str) -> Result<(Vec<u16>, usize, usize), String> {
    let img = image::open(path).map_err(|e| format!("No se pudo abrir la imagen: {}", e))?;
    let rgb = img.to_rgb16();
    let (w, h) = rgb.dimensions();
    Ok((rgb.into_raw(), w as usize, h as usize))
}

fn derot_rgb_to_mono(rgb: &[u16]) -> Vec<u16> {
    rgb.chunks_exact(3)
        .map(|px| {
            let r = px[0] as u32;
            let g = px[1] as u32;
            let b = px[2] as u32;
            ((r * 299 + g * 587 + b * 114) / 1000) as u16
        })
        .collect()
}

fn derot_channel_means_inside_disc(
    rgb: &[u16],
    width: usize,
    height: usize,
    disc: &crate::derotation::PlanetDisc,
    radius_limit: f64,
) -> [f64; 3] {
    let rx = disc.radius_x.abs().max(1.0);
    let ry = disc.radius_y.abs().max(1.0);
    let r2_limit = radius_limit * radius_limit;
    let mut sums = [0.0_f64; 3];
    let mut count = 0.0_f64;
    let y_start = (disc.cy - ry * radius_limit).max(0.0) as usize;
    let y_end = ((disc.cy + ry * radius_limit + 1.0) as usize).min(height);
    let x_start = (disc.cx - rx * radius_limit).max(0.0) as usize;
    let x_end = ((disc.cx + rx * radius_limit + 1.0) as usize).min(width);

    for y in y_start..y_end {
        for x in x_start..x_end {
            let nx = (x as f64 - disc.cx) / rx;
            let ny = (y as f64 - disc.cy) / ry;
            if nx * nx + ny * ny > r2_limit {
                continue;
            }
            let idx = (y * width + x) * 3;
            if idx + 2 >= rgb.len() {
                continue;
            }
            sums[0] += rgb[idx] as f64;
            sums[1] += rgb[idx + 1] as f64;
            sums[2] += rgb[idx + 2] as f64;
            count += 1.0;
        }
    }

    if count > 0.0 {
        [sums[0] / count, sums[1] / count, sums[2] / count]
    } else {
        [1.0, 1.0, 1.0]
    }
}

fn derot_photometric_gains(reference_means: [f64; 3], frame_means: [f64; 3]) -> [f64; 3] {
    let mut gains = [1.0_f64; 3];
    for c in 0..3 {
        if frame_means[c].is_finite() && frame_means[c] > 64.0 {
            gains[c] = (reference_means[c] / frame_means[c]).clamp(0.55, 1.85);
        }
    }
    gains
}

fn derot_disc_mask(
    width: usize,
    height: usize,
    disc: &crate::derotation::PlanetDisc,
    radius_limit: f64,
) -> Vec<u8> {
    let rx = disc.radius_x.abs().max(1.0);
    let ry = disc.radius_y.abs().max(1.0);
    let r2_limit = radius_limit * radius_limit;
    let mut mask = vec![0u8; width.saturating_mul(height)];
    let y_start = (disc.cy - ry * radius_limit).max(0.0) as usize;
    let y_end = ((disc.cy + ry * radius_limit + 1.0) as usize).min(height);
    let x_start = (disc.cx - rx * radius_limit).max(0.0) as usize;
    let x_end = ((disc.cx + rx * radius_limit + 1.0) as usize).min(width);

    for y in y_start..y_end {
        for x in x_start..x_end {
            let nx = (x as f64 - disc.cx) / rx;
            let ny = (y as f64 - disc.cy) / ry;
            if nx * nx + ny * ny <= r2_limit {
                mask[y * width + x] = 1;
            }
        }
    }
    mask
}

fn derot_stack_to_rgb16(stack: &StackResult) -> Vec<u16> {
    let pixels = stack.width.saturating_mul(stack.height);
    if stack.data.len() == pixels.saturating_mul(3) {
        stack.data.clone()
    } else {
        let mut rgb = Vec::with_capacity(pixels.saturating_mul(3));
        for v in stack.data.iter().take(pixels) {
            rgb.extend_from_slice(&[*v, *v, *v]);
        }
        rgb
    }
}

fn derot_encode_preview(rgb: &[u16], width: usize, height: usize) -> Result<String, String> {
    if rgb.len() != width.saturating_mul(height).saturating_mul(3) {
        return Err("Buffer RGB16 invalido para preview de derotacion".to_string());
    }
    let vis = to_8bit_preview_visual(rgb);
    let mut png = Vec::new();
    image::png::PngEncoder::new(&mut Cursor::new(&mut png))
        .encode(&vis, width as u32, height as u32, image::ColorType::Rgb8)
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "data:image/png;base64,{}",
        general_purpose::STANDARD.encode(&png)
    ))
}

static DEROTATION_OUTPUT_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

fn derot_write_rgb16_tiff(
    file: File,
    rgb: &[u16],
    width: usize,
    height: usize,
) -> Result<(), String> {
    let expected_samples = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| "Dimensiones TIFF de derotacion fuera de rango".to_string())?;
    if rgb.len() != expected_samples {
        return Err(format!(
            "Buffer RGB16 de derotacion inconsistente: {} muestras para {}x{}",
            rgb.len(),
            width,
            height
        ));
    }
    let byte_len = rgb
        .len()
        .checked_mul(2)
        .ok_or_else(|| "TIFF de derotacion demasiado grande".to_string())?;
    let mut raw_bytes = Vec::new();
    raw_bytes
        .try_reserve_exact(byte_len)
        .map_err(|error| format!("Memoria insuficiente para TIFF de derotacion: {error}"))?;
    for v in rgb {
        raw_bytes.extend_from_slice(&v.to_ne_bytes());
    }

    let mut writer = BufWriter::new(file);
    image::codecs::tiff::TiffEncoder::new(&mut writer)
        .encode(&raw_bytes, width as u32, height as u32, image::ColorType::Rgb16)
        .map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;
    writer.get_ref().sync_all().map_err(|e| e.to_string())
}

// Kept for the pre-existing non-planetary callers included later in the crate.
// Planetary derotation never writes a final path through this helper anymore.
fn derot_save_rgb16_tiff(
    path: &Path,
    rgb: &[u16],
    width: usize,
    height: usize,
) -> Result<(), String> {
    let mut raw_bytes = Vec::with_capacity(rgb.len() * 2);
    for value in rgb {
        raw_bytes.extend_from_slice(&value.to_ne_bytes());
    }
    let file = File::create(path).map_err(|error| error.to_string())?;
    let writer = BufWriter::new(file);
    image::codecs::tiff::TiffEncoder::new(writer)
        .encode(
            &raw_bytes,
            width as u32,
            height as u32,
            image::ColorType::Rgb16,
        )
        .map_err(|error| error.to_string())
}

fn derot_unique_suffix() -> String {
    let sequence = DEROTATION_OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!(
        "{}_{}_p{}_{}",
        chrono::Utc::now().format("%Y%m%d_%H%M%S_%3f"),
        nanos,
        std::process::id(),
        sequence
    )
}

fn derot_bounded_file_component(value: &str, max_bytes: usize) -> String {
    let mut bounded = String::new();
    for ch in value.chars() {
        if bounded.len().saturating_add(ch.len_utf8()) > max_bytes {
            break;
        }
        bounded.push(ch);
    }
    if bounded.is_empty() {
        "derotation".to_string()
    } else {
        bounded
    }
}

fn derot_unique_tiff_path(parent: &Path, prefix: &str) -> PathBuf {
    let prefix = derot_bounded_file_component(prefix, 120);
    loop {
        let candidate = parent.join(format!("{}_{}.tiff", prefix, derot_unique_suffix()));
        if !candidate.exists() {
            return candidate;
        }
    }
}

fn sync_parent_directory(path: &Path) {
    // APFS/File Provider puede dejar `File::open(parent)` bloqueado aun después
    // de que el rename y los fsync de archivo terminaron. Esa barrera adicional
    // no debe mantener el gate generacional ni la interfaz esperando al 98 %.
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(parent) = path.parent() {
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
    }
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let _ = path;
}

/// A TIFF is fully encoded and synced before it becomes visible at its final
/// path. The temporary file lives beside the destination so `rename` cannot
/// cross filesystems. Drop is the rollback path for errors and cancellation.
struct StagedDerotationTiff {
    temporary_path: PathBuf,
    final_path: PathBuf,
    published: bool,
}

impl StagedDerotationTiff {
    fn encode(
        final_path: PathBuf,
        rgb: &[u16],
        width: usize,
        height: usize,
    ) -> Result<Self, String> {
        let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        loop {
            let temporary_path =
                parent.join(format!(".derotation-{}.tmp", derot_unique_suffix()));
            let file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary_path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.to_string()),
            };

            if let Err(error) = derot_write_rgb16_tiff(file, rgb, width, height) {
                let _ = std::fs::remove_file(&temporary_path);
                return Err(error);
            }
            return Ok(Self {
                temporary_path,
                final_path,
                published: false,
            });
        }
    }

    fn publish(&mut self) -> Result<(), String> {
        if self.final_path.exists() {
            return Err(format!(
                "La salida de derotacion ya existe y no se sobrescribira: {}",
                self.final_path.display()
            ));
        }
        std::fs::rename(&self.temporary_path, &self.final_path)
            .map_err(|error| error.to_string())?;
        sync_parent_directory(&self.final_path);
        self.published = true;
        Ok(())
    }
}

/// PNG planetario completamente codificado y sincronizado antes de aparecer
/// bajo su nombre final. Se usa para previews que deben compartir el mismo
/// commit generacional que el máster 16-bit.
struct StagedPlanetaryPng {
    temporary_path: PathBuf,
    final_path: PathBuf,
    published: bool,
}

/// Archivo RGB16/FITS de propósito general: encode+flush+fsync en un sibling
/// oculto, seguido de publicación corta bajo el generation gate.
struct StagedPlanetaryArtifact {
    temporary_path: PathBuf,
    final_path: PathBuf,
    published: bool,
}

impl StagedPlanetaryArtifact {
    fn create_paths(final_path: PathBuf) -> Result<(PathBuf, File), String> {
        let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        loop {
            let temporary_path =
                parent.join(format!(".planetary-artifact-{}.tmp", derot_unique_suffix()));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary_path)
            {
                Ok(file) => return Ok((temporary_path, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    fn encode_rgb16_png(
        final_path: PathBuf,
        rgb: &[u16],
        width: usize,
        height: usize,
    ) -> Result<Self, String> {
        if rgb.len() != width.saturating_mul(height).saturating_mul(3) {
            return Err("Buffer RGB16 invalido para PNG".into());
        }
        let (temporary_path, file) = Self::create_paths(final_path.clone())?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(rgb.len().saturating_mul(2))
            .map_err(|error| format!("Memoria insuficiente para PNG16: {error}"))?;
        for value in rgb {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        let mut writer = BufWriter::new(file);
        if let Err(error) = image::codecs::png::PngEncoder::new(&mut writer).encode(
            &bytes,
            width as u32,
            height as u32,
            image::ColorType::Rgb16,
        ) {
            let _ = std::fs::remove_file(&temporary_path);
            return Err(error.to_string());
        }
        if let Err(error) = writer.flush().and_then(|_| writer.get_ref().sync_all()) {
            let _ = std::fs::remove_file(&temporary_path);
            return Err(error.to_string());
        }
        Ok(Self {
            temporary_path,
            final_path,
            published: false,
        })
    }

    fn encode_rgb16_tiff(
        final_path: PathBuf,
        rgb: &[u16],
        width: usize,
        height: usize,
    ) -> Result<Self, String> {
        let (temporary_path, file) = Self::create_paths(final_path.clone())?;
        if let Err(error) = derot_write_rgb16_tiff(file, rgb, width, height) {
            let _ = std::fs::remove_file(&temporary_path);
            return Err(error);
        }
        Ok(Self {
            temporary_path,
            final_path,
            published: false,
        })
    }

    fn encode_rgb16_fits(
        final_path: PathBuf,
        rgb: &[u16],
        width: usize,
        height: usize,
    ) -> Result<Self, String> {
        let (temporary_path, file) = Self::create_paths(final_path.clone())?;
        drop(file);
        let temporary_string = temporary_path.to_string_lossy().into_owned();
        if let Err(error) = write_rgb16_fits(&temporary_string, rgb, width, height) {
            let _ = std::fs::remove_file(&temporary_path);
            return Err(error);
        }
        File::open(&temporary_path)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())?;
        Ok(Self {
            temporary_path,
            final_path,
            published: false,
        })
    }

    fn publish(&mut self, replace_existing: bool) -> Result<(), String> {
        if replace_existing {
            atomic_replace_analysis_file(&self.temporary_path, &self.final_path)
                .map_err(|error| error.to_string())?;
        } else {
            if self.final_path.exists() {
                return Err(format!(
                    "La salida planetaria ya existe y no se sobrescribira: {}",
                    self.final_path.display()
                ));
            }
            // Los nombres no destructivos incorporan PID+reloj+secuencia y la
            // app es single-instance. rename mantiene compatibilidad con exFAT
            // (habitual en discos de captura), donde hard_link no está soportado.
            std::fs::rename(&self.temporary_path, &self.final_path)
                .map_err(|error| error.to_string())?;
        }
        sync_parent_directory(&self.final_path);
        self.published = true;
        Ok(())
    }
}

impl Drop for StagedPlanetaryArtifact {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.temporary_path);
        }
    }
}

impl StagedPlanetaryPng {
    fn encode_rgb8(
        final_path: PathBuf,
        rgb: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(3))
            .ok_or_else(|| "Preview PNG demasiado grande".to_string())?;
        if rgb.len() != expected {
            return Err("Buffer RGB8 invalido para preview PNG".to_string());
        }
        let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        loop {
            let temporary_path =
                parent.join(format!(".planetary-preview-{}.tmp", derot_unique_suffix()));
            let file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary_path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.to_string()),
            };
            let mut writer = BufWriter::new(file);
            if let Err(error) = image::codecs::png::PngEncoder::new(&mut writer)
                .encode(rgb, width, height, image::ColorType::Rgb8)
            {
                let _ = std::fs::remove_file(&temporary_path);
                return Err(error.to_string());
            }
            if let Err(error) = writer.flush().and_then(|_| writer.get_ref().sync_all()) {
                let _ = std::fs::remove_file(&temporary_path);
                return Err(error.to_string());
            }
            return Ok(Self {
                temporary_path,
                final_path,
                published: false,
            });
        }
    }

    fn publish(&mut self) -> Result<(), String> {
        if self.final_path.exists() {
            return Err(format!(
                "La salida planetaria ya existe y no se sobrescribira: {}",
                self.final_path.display()
            ));
        }
        std::fs::rename(&self.temporary_path, &self.final_path)
            .map_err(|error| error.to_string())?;
        sync_parent_directory(&self.final_path);
        self.published = true;
        Ok(())
    }

}

impl Drop for StagedPlanetaryPng {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.temporary_path);
        }
    }
}

impl Drop for StagedDerotationTiff {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.temporary_path);
        }
    }
}

/// A sequence is staged in a sibling directory and published with one atomic
/// directory rename. This prevents a cancelled animation from exposing a
/// half-written sequence.
struct StagedDerotationSequence {
    temporary_dir: PathBuf,
    final_dir: PathBuf,
    file_names: Vec<String>,
    published: bool,
}

impl StagedDerotationSequence {
    fn create(parent: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        loop {
            let suffix = derot_unique_suffix();
            let final_dir = parent.join(format!("Derotated_Planetary_{}", suffix));
            let temporary_dir = parent.join(format!(".Derotated_Planetary_{}.tmp", suffix));
            if final_dir.exists() {
                continue;
            }
            match std::fs::create_dir(&temporary_dir) {
                Ok(()) => {
                    return Ok(Self {
                        temporary_dir,
                        final_dir,
                        file_names: Vec::new(),
                        published: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    fn stage_rgb16(
        &mut self,
        file_name: String,
        rgb: &[u16],
        width: usize,
        height: usize,
    ) -> Result<(), String> {
        let path = self.temporary_dir.join(&file_name);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| error.to_string())?;
        if let Err(error) = derot_write_rgb16_tiff(file, rgb, width, height) {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        self.file_names.push(file_name);
        Ok(())
    }

    fn output_paths(&self) -> Vec<PathBuf> {
        self.file_names
            .iter()
            .map(|file_name| self.final_dir.join(file_name))
            .collect()
    }

    fn publish(&mut self) -> Result<(), String> {
        if self.final_dir.exists() {
            return Err(format!(
                "La carpeta de derotacion ya existe y no se sobrescribira: {}",
                self.final_dir.display()
            ));
        }
        std::fs::rename(&self.temporary_dir, &self.final_dir)
            .map_err(|error| error.to_string())?;
        sync_parent_directory(&self.final_dir);
        self.published = true;
        Ok(())
    }
}

impl Drop for StagedDerotationSequence {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_dir_all(&self.temporary_dir);
        }
    }
}

fn derot_default_output_path(source_path: &str) -> PathBuf {
    let source = Path::new(source_path);
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Planetary_Derotation");
    derot_unique_tiff_path(parent, &format!("{}_Derotated", stem))
}

fn derot_output_path_from_optional_source(source_path: Option<&str>, fallback_name: &str) -> PathBuf {
    if let Some(path) = source_path.filter(|p| !p.trim().is_empty()) {
        derot_default_output_path(path)
    } else {
        derot_unique_tiff_path(
            &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            &format!("{}_Derotated", fallback_name),
        )
    }
}

fn derot_iso_for_datetime_input(value: &str) -> String {
    let cleaned = value.trim().replace(' ', "T");
    if cleaned.len() >= 19 {
        cleaned[..19].to_string()
    } else if cleaned.len() >= 16 {
        format!("{}:00", &cleaned[..16])
    } else {
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string()
    }
}

fn derot_metadata_with_source(
    path: &str,
    manual_log_path: Option<&str>,
) -> (crate::derotation::CaptureMetadata, String) {
    if let Some(log_path) = manual_log_path.filter(|p| !p.trim().is_empty()) {
        if let Ok(meta) = crate::derotation::parse_capture_log(log_path) {
            return (meta, "manual_log".to_string());
        }
    }
    if let Some(meta) = crate::derotation::auto_parse_log(path) {
        return (meta, "log".to_string());
    }
    if let Ok(meta) = crate::derotation::infer_time_from_file(path) {
        return (meta, "file_modified".to_string());
    }

    (
        {
            let now = chrono::Utc::now();
            let jd = crate::derotation::datetime_to_jd(
                now.format("%Y").to_string().parse().unwrap_or(2026),
                now.format("%m").to_string().parse().unwrap_or(1),
                now.format("%d").to_string().parse().unwrap_or(1),
                now.format("%H").to_string().parse().unwrap_or(0),
                now.format("%M").to_string().parse().unwrap_or(0),
                now.format("%S").to_string().parse::<f64>().unwrap_or(0.0),
            );
            crate::derotation::CaptureMetadata {
                mid_time_jd: jd,
                planet: None,
                duration_sec: 0.0,
                fps: 0.0,
                start_time_iso: now.format("%Y-%m-%dT%H:%M:%S").to_string(),
            }
        },
        "system_clock".to_string(),
    )
}

fn derot_disc_diagnostics(
    mono: &[u16],
    width: usize,
    height: usize,
    disc: &crate::derotation::PlanetDisc,
    planet: &crate::derotation::PlanetaryBody,
    time_source: &str,
    duration_sec: f64,
    fps: f64,
    capture_jd: f64,
    reference_jd: f64,
    cm_system: usize,
    geometry: crate::derotation::ObserverGeometry,
) -> DerotationDiagnostics {
    let rx = disc.radius_x.abs().max(1.0);
    let ry = disc.radius_y.abs().max(1.0);
    let radius_px = rx.min(ry);
    let coverage = (std::f64::consts::PI * rx * ry) / (width.max(1) * height.max(1)) as f64;
    let edge_margin_px = (disc.cx - rx)
        .min(disc.cy - ry)
        .min(width as f64 - (disc.cx + rx))
        .min(height as f64 - (disc.cy + ry));
    let aspect_ratio = ry / rx;
    let expected_aspect_ratio = planet.polar_radius_km / planet.equatorial_radius_km;
    let aspect_error = if expected_aspect_ratio > 0.0 {
        (aspect_ratio / expected_aspect_ratio - 1.0).abs()
    } else {
        1.0
    };

    let step = ((width.max(height) / 900).max(1)).min(8);
    let mut in_sum = 0.0;
    let mut in_count = 0.0;
    let mut out_sum = 0.0;
    let mut out_count = 0.0;
    for y in (0..height).step_by(step) {
        for x in (0..width).step_by(step) {
            let idx = y * width + x;
            if idx >= mono.len() {
                continue;
            }
            let nx = (x as f64 - disc.cx) / rx;
            let ny = (y as f64 - disc.cy) / ry;
            let r2 = nx * nx + ny * ny;
            if r2 <= 0.92 {
                in_sum += mono[idx] as f64;
                in_count += 1.0;
            } else if r2 >= 1.18 {
                out_sum += mono[idx] as f64;
                out_count += 1.0;
            }
        }
    }
    let in_mean = if in_count > 0.0 { in_sum / in_count } else { 0.0 };
    let out_mean = if out_count > 0.0 { out_sum / out_count } else { 0.0 };
    let contrast_ratio = (in_mean + 64.0) / (out_mean + 64.0);

    let radius_score = ((radius_px - 12.0) / 80.0).clamp(0.0, 1.0);
    let coverage_score = if coverage < 0.002 {
        0.0
    } else if coverage < 0.012 {
        (coverage / 0.012).clamp(0.0, 1.0)
    } else if coverage <= 0.65 {
        1.0
    } else {
        ((0.92 - coverage) / 0.27).clamp(0.0, 1.0)
    };
    let contrast_score = ((contrast_ratio - 1.08) / 1.25).clamp(0.0, 1.0);
    let margin_score = ((edge_margin_px + radius_px * 0.20) / (radius_px * 0.20)).clamp(0.0, 1.0);
    let aspect_score = (1.0 - (aspect_error / 0.45)).clamp(0.0, 1.0);
    let confidence = (0.25 * radius_score
        + 0.25 * coverage_score
        + 0.22 * contrast_score
        + 0.16 * margin_score
        + 0.12 * aspect_score)
        .clamp(0.0, 1.0);

    let delta_deg = crate::derotation::rotation_delta_deg(
        planet,
        reference_jd,
        capture_jd,
        cm_system.min(2),
    );
    let mut warnings = Vec::new();
    if time_source != "log" && time_source != "manual_log" {
        warnings.push("No se encontro log de captura; revisa manualmente la hora UTC antes de aplicar.".to_string());
    }
    if confidence < 0.45 {
        warnings.push("La deteccion del disco es moderada o baja; ajusta centro/radio antes de derotar.".to_string());
    }
    if contrast_ratio < 1.18 {
        warnings.push("Contraste bajo entre planeta y fondo; la malla puede estar imprecisa.".to_string());
    }
    if edge_margin_px < 2.0 {
        warnings.push("El disco parece recortado o muy cerca del borde; revisa la geometria manual.".to_string());
    }
    if coverage > 0.72 {
        warnings.push("La imagen cubre gran parte del frame; puede ser superficie lunar/solar, no disco planetario compacto.".to_string());
    }
    if aspect_error > 0.28 {
        warnings.push("La relacion de aspecto detectada no coincide bien con el planeta seleccionado.".to_string());
    }
    if disc.phase < 0.35 {
        warnings.push("Fase iluminada baja: el borde oscuro puede desplazar la deteccion.".to_string());
    }
    if delta_deg.abs() < 0.03 {
        warnings.push("El delta temporal es casi cero; la imagen cambiara muy poco.".to_string());
    } else if delta_deg.abs() > 75.0 {
        warnings.push("Delta de rotacion alto; valida tiempos porque puede generar estiramientos visibles.".to_string());
    }
    if geometry.phase_angle_deg > 25.0 {
        warnings.push("Fase planetaria alta: revisa que el limbo oscuro no desplace la malla.".to_string());
    }

    let can_apply = confidence >= 0.18 && radius_px >= 10.0 && coverage > 0.001;
    let classification = if confidence >= 0.78 {
        "excellent"
    } else if confidence >= 0.58 {
        "good"
    } else if confidence >= 0.35 {
        "review"
    } else {
        "poor"
    }
    .to_string();

    DerotationDiagnostics {
        confidence,
        classification,
        can_apply,
        coverage,
        contrast_ratio,
        edge_margin_px,
        radius_px,
        aspect_ratio,
        expected_aspect_ratio,
        delta_deg,
        time_source: time_source.to_string(),
        duration_sec,
        fps,
        b0_deg: geometry.sub_earth_lat_deg,
        north_angle_deg: geometry.north_pole_angle_deg,
        phase_angle_deg: geometry.phase_angle_deg,
        apparent_diameter_arcsec: geometry.apparent_diameter_arcsec,
        distance_au: geometry.distance_au,
        warnings,
    }
}

fn derot_build_preflight(
    rgb: &[u16],
    width: usize,
    height: usize,
    file_name: String,
    source_size_bytes: u64,
    source_kind: String,
    meta: crate::derotation::CaptureMetadata,
    time_source: String,
) -> Result<PlanetaryDerotationPreflight, String> {
    let mono = derot_rgb_to_mono(rgb);
    let raw_disc = crate::derotation::detect_planet_disc(&mono, width, height);
    let suggested_planet = meta.planet.clone().unwrap_or_else(|| "jupiter".to_string());
    let planet = crate::derotation::get_planet(&suggested_planet)
        .unwrap_or(&crate::derotation::JUPITER);
    let mut disc = crate::derotation::validate_disc_aspect(&raw_disc, planet);
    let (cm1, cm2, cm3) = crate::derotation::calculate_central_meridian(planet, meta.mid_time_jd);
    let geometry = crate::derotation::calculate_observer_geometry(planet, meta.mid_time_jd);
    disc.angle_deg = geometry.north_pole_angle_deg;
    let diagnostics = derot_disc_diagnostics(
        &mono,
        width,
        height,
        &disc,
        planet,
        &time_source,
        meta.duration_sec,
        meta.fps,
        meta.mid_time_jd,
        meta.mid_time_jd,
        1,
        geometry,
    );
    let preview_base64 = derot_encode_preview(rgb, width, height)?;

    Ok(PlanetaryDerotationPreflight {
        width,
        height,
        file_name,
        source_size_bytes,
        preview_base64,
        detected_disc: disc.into(),
        diagnostics,
        suggested_planet,
        capture_time: derot_iso_for_datetime_input(&meta.start_time_iso),
        reference_time: derot_iso_for_datetime_input(&meta.start_time_iso),
        cm1,
        cm2,
        cm3,
        b0_deg: geometry.sub_earth_lat_deg,
        north_angle_deg: geometry.north_pole_angle_deg,
        phase_angle_deg: geometry.phase_angle_deg,
        apparent_diameter_arcsec: geometry.apparent_diameter_arcsec,
        distance_au: geometry.distance_au,
        source_kind,
    })
}

#[tauri::command]
async fn get_planetary_derotation_preflight(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    image_path: String,
    log_path: Option<String>,
) -> Result<PlanetaryDerotationPreflight, String> {
    state.license_manager.check_access()?;
    let path = Path::new(&image_path);
    if !path.exists() {
        return Err("Imagen no encontrada".to_string());
    }

    emit_progress(&app, "Analizando derotacion planetaria...", 8.0, None);
    let (rgb, width, height) = derot_load_rgb16_image(&image_path)?;
    let (meta, time_source) = derot_metadata_with_source(&image_path, log_path.as_deref());
    let preflight = derot_build_preflight(
        &rgb,
        width,
        height,
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("imagen")
            .to_string(),
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        "file".to_string(),
        meta,
        time_source,
    )?;
    emit_progress(&app, "Derotacion lista para ajustar", 100.0, None);
    Ok(preflight)
}

#[tauri::command]
async fn get_current_stacked_derotation_preflight(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    source_path: Option<String>,
    log_path: Option<String>,
) -> Result<PlanetaryDerotationPreflight, String> {
    state.license_manager.check_access()?;
    let stack = {
        let guard = state.stacked_image.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .clone()
            .ok_or_else(|| "No hay una imagen apilada activa para derotar.".to_string())?
    };
    let rgb = derot_stack_to_rgb16(&stack);
    if rgb.len() != stack.width.saturating_mul(stack.height).saturating_mul(3) {
        return Err("La imagen apilada activa no tiene un buffer valido.".to_string());
    }
    let (meta, time_source) = if let Some(path) = source_path.as_deref().filter(|p| !p.trim().is_empty()) {
        derot_metadata_with_source(path, log_path.as_deref())
    } else if let Some(path) = log_path.as_deref().filter(|p| !p.trim().is_empty()) {
        crate::derotation::parse_capture_log(path)
            .map(|meta| (meta, "manual_log".to_string()))
            .unwrap_or_else(|_| {
                let now = chrono::Utc::now();
                let jd = crate::derotation::datetime_to_jd(
                    now.format("%Y").to_string().parse().unwrap_or(2026),
                    now.format("%m").to_string().parse().unwrap_or(1),
                    now.format("%d").to_string().parse().unwrap_or(1),
                    now.format("%H").to_string().parse().unwrap_or(0),
                    now.format("%M").to_string().parse().unwrap_or(0),
                    now.format("%S").to_string().parse::<f64>().unwrap_or(0.0),
                );
                (
                    crate::derotation::CaptureMetadata {
                        mid_time_jd: jd,
                        planet: None,
                        duration_sec: 0.0,
                        fps: 0.0,
                        start_time_iso: now.format("%Y-%m-%dT%H:%M:%S").to_string(),
                    },
                    "system_clock".to_string(),
                )
            })
    } else {
            let now = chrono::Utc::now();
            let jd = crate::derotation::datetime_to_jd(
                now.format("%Y").to_string().parse().unwrap_or(2026),
                now.format("%m").to_string().parse().unwrap_or(1),
                now.format("%d").to_string().parse().unwrap_or(1),
                now.format("%H").to_string().parse().unwrap_or(0),
                now.format("%M").to_string().parse().unwrap_or(0),
                now.format("%S").to_string().parse::<f64>().unwrap_or(0.0),
            );
            (
                crate::derotation::CaptureMetadata {
                    mid_time_jd: jd,
                    planet: None,
                    duration_sec: 0.0,
                    fps: 0.0,
                    start_time_iso: now.format("%Y-%m-%dT%H:%M:%S").to_string(),
                },
                "system_clock".to_string(),
            )
    };

    emit_progress(&app, "Analizando resultado actual para derotacion...", 8.0, None);
    let preflight = derot_build_preflight(
        &rgb,
        stack.width,
        stack.height,
        "Resultado apilado actual".to_string(),
        0,
        "current_stack".to_string(),
        meta,
        time_source,
    )?;
    emit_progress(&app, "Derotacion lista para ajustar", 100.0, None);
    Ok(preflight)
}

#[tauri::command]
async fn detect_planetary_derotation_disc(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    image_path: String,
    planet: String,
    log_path: Option<String>,
) -> Result<PlanetaryDiscDetectionResult, String> {
    state.license_manager.check_access()?;
    emit_progress(&app, "Detectando disco planetario...", 35.0, None);
    let planet_body = crate::derotation::get_planet(&planet).unwrap_or(&crate::derotation::JUPITER);
    let (rgb, width, height) = derot_load_rgb16_image(&image_path)?;
    let mono = derot_rgb_to_mono(&rgb);
    let disc = crate::derotation::detect_planet_disc(&mono, width, height);
    let mut disc = crate::derotation::validate_disc_aspect(&disc, planet_body);
    let (meta, time_source) = derot_metadata_with_source(&image_path, log_path.as_deref());
    let geometry = crate::derotation::calculate_observer_geometry(planet_body, meta.mid_time_jd);
    disc.angle_deg = geometry.north_pole_angle_deg;
    let diagnostics = derot_disc_diagnostics(
        &mono,
        width,
        height,
        &disc,
        planet_body,
        &time_source,
        meta.duration_sec,
        meta.fps,
        meta.mid_time_jd,
        meta.mid_time_jd,
        1,
        geometry,
    );
    emit_progress(&app, "Disco detectado", 100.0, None);
    Ok(PlanetaryDiscDetectionResult {
        detected_disc: disc.into(),
        diagnostics,
    })
}

/// Cooperative checkpoint shared by the derotation commands.  The generation
/// component is essential: a later command may legitimately clear the global
/// cancellation flag, but it must never make an older worker current again.
#[inline]
fn planetary_derotation_checkpoint(
    job_token: &PlanetaryJobToken,
    stage: &str,
) -> Result<(), String> {
    if job_token.is_cancelled() {
        Err(format!(
            "Operacion planetaria cancelada o sustituida durante {stage}"
        ))
    } else {
        Ok(())
    }
}

/// Execute a small ownership transition while the planetary generation cannot
/// change.  Heavy derotation work stays outside this critical section.
fn with_current_planetary_job<T>(
    generation_gate: &Mutex<()>,
    job_token: &PlanetaryJobToken,
    stage: &str,
    action: impl FnOnce() -> T,
) -> Result<T, String> {
    let _generation_guard = generation_gate
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    planetary_derotation_checkpoint(job_token, stage)?;
    Ok(action())
}

/// Commit a derotation result atomically with respect to cancellation and job
/// supersession.  Derived caches belong to the published master, so they are
/// invalidated in the same ownership transition.
fn publish_planetary_derotation_if_current(
    state: &AppState,
    job_token: &PlanetaryJobToken,
    mut staged_tiff: StagedDerotationTiff,
    result: StackResult,
    stage: &str,
) -> Result<PathBuf, String> {
    let final_path = staged_tiff.final_path.clone();
    with_current_planetary_job(
        &state.planetary_generation_gate,
        job_token,
        stage,
        || -> Result<(), String> {
            // The rename and the logical master transition share the same
            // generation gate. A superseded worker can therefore publish
            // neither the TIFF nor `stacked_image`.
            staged_tiff.publish()?;
            *state
                .stacked_image
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(result);
            state
                .deconv_cache
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
            state
                .wavelet_cache
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
            state
                .filter_cache
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
            Ok(())
        },
    )??;
    Ok(final_path)
}

fn publish_planetary_derotation_sequence_if_current(
    state: &AppState,
    job_token: &PlanetaryJobToken,
    mut staged_sequence: StagedDerotationSequence,
    stage: &str,
) -> Result<Vec<PathBuf>, String> {
    let output_paths = staged_sequence.output_paths();
    with_current_planetary_job(
        &state.planetary_generation_gate,
        job_token,
        stage,
        || staged_sequence.publish(),
    )??;
    Ok(output_paths)
}

/// Start a clean session without exposing the old race where generation was
/// advanced under the gate and `cancel_requested=false` was written after the
/// gate had already been released. `while_locked` lets clear_app_memory erase
/// the old published stack in the same transaction.
fn advance_and_rearm_planetary_session(
    generation_gate: &Mutex<()>,
    active_request: &AtomicUsize,
    cancel_requested: &std::sync::atomic::AtomicBool,
    while_locked: impl FnOnce(),
) -> usize {
    let _generation_guard = generation_gate
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let generation = active_request
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    cancel_requested.store(false, Ordering::Release);
    while_locked();
    generation
}

/// Invalidate the current owner and mutate its published slot under one gate,
/// while deliberately preserving the sticky cancellation flag used by batch
/// processing between files.
fn advance_planetary_generation_under_gate(
    generation_gate: &Mutex<()>,
    active_request: &AtomicUsize,
    while_locked: impl FnOnce(),
) -> usize {
    let _generation_guard = generation_gate
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let generation = active_request
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    while_locked();
    generation
}

#[cfg(test)]
mod planetary_publication_tests {
    use super::{
        advance_and_rearm_planetary_session, advance_planetary_generation_under_gate,
        derot_unique_suffix, derot_unique_tiff_path, with_current_planetary_job,
        PlanetaryJobToken, StagedDerotationSequence, StagedDerotationTiff,
    };
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn derotation_test_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zenith-derotation-{}-{}",
            label,
            derot_unique_suffix()
        ));
        std::fs::create_dir_all(&root).expect("crear carpeta temporal");
        root
    }

    #[test]
    fn generation_gate_commits_only_the_current_planetary_token() {
        let gate = Mutex::new(());
        let active = Arc::new(AtomicUsize::new(7));
        let cancelled = Arc::new(AtomicBool::new(false));
        let token = PlanetaryJobToken::for_test(7, active.clone(), cancelled.clone());
        let published = AtomicUsize::new(0);

        with_current_planetary_job(&gate, &token, "prueba", || {
            assert!(gate.try_lock().is_err(), "el commit debe conservar el gate");
            published.store(7, Ordering::Release);
        })
        .expect("el propietario vigente debe publicar");

        active.store(8, Ordering::Release);
        assert!(
            with_current_planetary_job(&gate, &token, "prueba obsoleta", || {
                published.store(99, Ordering::Release);
            })
            .is_err(),
            "una generacion sustituida no debe ejecutar el commit"
        );
        assert_eq!(published.load(Ordering::Acquire), 7);

        active.store(7, Ordering::Release);
        cancelled.store(true, Ordering::Release);
        assert!(
            with_current_planetary_job(&gate, &token, "prueba cancelada", || {
                published.store(100, Ordering::Release);
            })
            .is_err(),
            "una cancelacion explicita tampoco debe ejecutar el commit"
        );
        assert_eq!(published.load(Ordering::Acquire), 7);
    }

    #[test]
    fn session_rearm_advances_generation_and_clears_cancel_under_one_gate() {
        let gate = Mutex::new(());
        let active = Arc::new(AtomicUsize::new(41));
        let cancelled = Arc::new(AtomicBool::new(true));
        let old_token = PlanetaryJobToken::for_test(41, active.clone(), cancelled.clone());
        let reset_ran_under_gate = AtomicBool::new(false);

        let generation = advance_and_rearm_planetary_session(
            &gate,
            active.as_ref(),
            cancelled.as_ref(),
            || reset_ran_under_gate.store(gate.try_lock().is_err(), Ordering::Release),
        );

        assert_eq!(generation, 42);
        assert!(!cancelled.load(Ordering::Acquire));
        assert!(reset_ran_under_gate.load(Ordering::Acquire));
        assert!(old_token.is_cancelled(), "la generacion anterior sigue invalidada");
    }

    #[test]
    fn per_file_clear_advances_under_gate_without_rearming_cancel() {
        let gate = Mutex::new(());
        let active = AtomicUsize::new(12);
        let cancelled = AtomicBool::new(true);
        let clear_ran_under_gate = AtomicBool::new(false);

        let generation = advance_planetary_generation_under_gate(&gate, &active, || {
            clear_ran_under_gate.store(gate.try_lock().is_err(), Ordering::Release);
        });

        assert_eq!(generation, 13);
        assert!(cancelled.load(Ordering::Acquire), "la cancelacion debe seguir sticky");
        assert!(clear_ran_under_gate.load(Ordering::Acquire));
    }

    #[test]
    fn superseded_batch_worker_cannot_resurrect_cleared_anchor() {
        let gate = Mutex::new(());
        let active = Arc::new(AtomicUsize::new(21));
        let cancelled = Arc::new(AtomicBool::new(false));
        let old_token = PlanetaryJobToken::for_test(21, active.clone(), cancelled);
        let anchor = Mutex::new(Some(vec![1u16, 2, 3]));
        let dimensions = Mutex::new((1usize, 1usize));

        advance_planetary_generation_under_gate(&gate, active.as_ref(), || {
            *anchor.lock().unwrap() = None;
            *dimensions.lock().unwrap() = (0, 0);
        });

        let stale_commit = with_current_planetary_job(&gate, &old_token, "anchor obsoleto", || {
            *anchor.lock().unwrap() = Some(vec![9u16, 9, 9]);
            *dimensions.lock().unwrap() = (1, 1);
        });
        assert!(stale_commit.is_err());
        assert!(anchor.lock().unwrap().is_none());
        assert_eq!(*dimensions.lock().unwrap(), (0, 0));
    }

    #[test]
    fn staged_tiff_is_published_only_by_the_current_generation() {
        let root = derotation_test_root("single-success");
        let final_path = derot_unique_tiff_path(&root, "result");
        let mut staged = StagedDerotationTiff::encode(
            final_path.clone(),
            &[1024, 2048, 4096],
            1,
            1,
        )
        .expect("codificar TIFF temporal");
        let temporary_path = staged.temporary_path.clone();
        let gate = Mutex::new(());
        let active = Arc::new(AtomicUsize::new(3));
        let cancelled = Arc::new(AtomicBool::new(false));
        let token = PlanetaryJobToken::for_test(3, active, cancelled);

        with_current_planetary_job(&gate, &token, "publicacion TIFF", || staged.publish())
            .expect("token vigente")
            .expect("rename atomico");

        assert!(final_path.is_file());
        assert!(!temporary_path.exists());
        drop(staged);
        assert!(final_path.is_file(), "Drop no debe borrar una salida publicada");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn superseded_tiff_rolls_back_without_exposing_output() {
        let root = derotation_test_root("single-cancel");
        let final_path = derot_unique_tiff_path(&root, "result");
        let mut staged = StagedDerotationTiff::encode(
            final_path.clone(),
            &[1024, 2048, 4096],
            1,
            1,
        )
        .expect("codificar TIFF temporal");
        let temporary_path = staged.temporary_path.clone();
        let gate = Mutex::new(());
        let active = Arc::new(AtomicUsize::new(8));
        let cancelled = Arc::new(AtomicBool::new(false));
        let token = PlanetaryJobToken::for_test(8, active.clone(), cancelled);
        active.store(9, Ordering::Release);

        assert!(
            with_current_planetary_job(&gate, &token, "publicacion obsoleta", || {
                staged.publish()
            })
            .is_err()
        );
        drop(staged);

        assert!(!temporary_path.exists());
        assert!(!final_path.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn animation_sequence_is_all_or_nothing_on_supersession() {
        let root = derotation_test_root("sequence-cancel");
        let mut staged = StagedDerotationSequence::create(&root)
            .expect("crear staging de secuencia");
        staged
            .stage_rgb16("0001_a.tiff".to_string(), &[1, 2, 3], 1, 1)
            .expect("primer frame");
        staged
            .stage_rgb16("0002_b.tiff".to_string(), &[4, 5, 6], 1, 1)
            .expect("segundo frame");
        let temporary_dir = staged.temporary_dir.clone();
        let final_dir = staged.final_dir.clone();
        let gate = Mutex::new(());
        let active = Arc::new(AtomicUsize::new(21));
        let cancelled = Arc::new(AtomicBool::new(false));
        let token = PlanetaryJobToken::for_test(21, active.clone(), cancelled);
        active.store(22, Ordering::Release);

        assert!(
            with_current_planetary_job(&gate, &token, "publicacion de secuencia", || {
                staged.publish()
            })
            .is_err()
        );
        drop(staged);

        assert!(!temporary_dir.exists());
        assert!(!final_dir.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn animation_sequence_publishes_with_one_directory_rename() {
        let root = derotation_test_root("sequence-success");
        let mut staged = StagedDerotationSequence::create(&root)
            .expect("crear staging de secuencia");
        staged
            .stage_rgb16("0001_a.tiff".to_string(), &[1, 2, 3], 1, 1)
            .expect("primer frame");
        staged
            .stage_rgb16("0002_b.tiff".to_string(), &[4, 5, 6], 1, 1)
            .expect("segundo frame");
        let output_paths = staged.output_paths();
        let temporary_dir = staged.temporary_dir.clone();
        let final_dir = staged.final_dir.clone();
        let gate = Mutex::new(());
        let active = Arc::new(AtomicUsize::new(31));
        let cancelled = Arc::new(AtomicBool::new(false));
        let token = PlanetaryJobToken::for_test(31, active, cancelled);

        with_current_planetary_job(&gate, &token, "publicacion de secuencia", || {
            staged.publish()
        })
        .expect("token vigente")
        .expect("rename de directorio");

        assert!(!temporary_dir.exists());
        assert!(final_dir.is_dir());
        assert!(output_paths.iter().all(|path| path.is_file()));
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tauri::command]
async fn apply_planetary_derotation(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    image_path: String,
    planet: String,
    capture_time: String,
    reference_time: String,
    cm_system: usize,
    limb_strength: f64,
    sub_earth_lat_deg: f64,
    disc_override: Option<DerotationDiscDto>,
) -> Result<PlanetaryDerotationResult, String> {
    state.license_manager.check_access()?;
    let request_id = begin_planetary_user_job(&state);
    let job_token =
        PlanetaryJobToken::for_app(&app, request_id, state.cancel_requested.clone());
    planetary_derotation_checkpoint(&job_token, "el inicio de la derotacion")?;
    let planet_body = crate::derotation::get_planet(&planet)
        .ok_or_else(|| "Planeta no soportado para derotacion".to_string())?;
    let capture_jd = crate::derotation::parse_iso_to_jd(&capture_time)
        .map_err(|e| format!("Tiempo de captura invalido: {}", e))?;
    let reference_jd = crate::derotation::parse_iso_to_jd(&reference_time)
        .map_err(|e| format!("Tiempo de referencia invalido: {}", e))?;

    emit_progress(&app, "Cargando imagen para derotacion...", 5.0, None);
    planetary_derotation_checkpoint(&job_token, "la carga de la imagen")?;
    let (rgb, width, height) = derot_load_rgb16_image(&image_path)?;
    planetary_derotation_checkpoint(&job_token, "la carga de la imagen")?;
    let mono = derot_rgb_to_mono(&rgb);
    let raw_disc = match disc_override.as_ref() {
        Some(disc) => crate::derotation::PlanetDisc::from(disc),
        None => crate::derotation::detect_planet_disc(&mono, width, height),
    };
    let disc = crate::derotation::validate_disc_aspect(&raw_disc, planet_body);
    let mut geometry = crate::derotation::calculate_observer_geometry(planet_body, capture_jd);
    geometry.sub_earth_lat_deg = sub_earth_lat_deg.clamp(-35.0, 35.0);
    geometry.north_pole_angle_deg = disc.angle_deg;
    let delta_deg = crate::derotation::rotation_delta_deg(
        planet_body,
        reference_jd,
        capture_jd,
        cm_system.min(2),
    );
    let diagnostics = derot_disc_diagnostics(
        &mono,
        width,
        height,
        &disc,
        planet_body,
        "manual",
        0.0,
        0.0,
        capture_jd,
        reference_jd,
        cm_system.min(2),
        geometry,
    );
    if !diagnostics.can_apply {
        return Err("La geometria del disco no es suficientemente confiable. Ajusta centro/radio o carga una imagen planetaria con el disco completo visible.".to_string());
    }
    planetary_derotation_checkpoint(&job_token, "el analisis de geometria")?;

    emit_progress(
        &app,
        "Aplicando proyeccion cilindrica y derotacion...",
        35.0,
        Some(format!("Delta {:.3} grados", delta_deg)),
    );

    let planet_for_block = planet.clone();
    let disc_for_block = disc.clone();
    let blocking_token = job_token.clone();
    let derotated = tauri::async_runtime::spawn_blocking(move || {
        planetary_derotation_checkpoint(&blocking_token, "el calculo de derotacion")?;
        let planet_body = crate::derotation::get_planet(&planet_for_block)
            .ok_or_else(|| "Planeta no soportado para derotacion".to_string())?;
        let result = crate::derotation::derotate_single_advanced_cancelable(
            &rgb,
            width,
            height,
            3,
            planet_body,
            capture_jd,
            reference_jd,
            limb_strength.clamp(0.0, 2.0),
            cm_system.min(2),
            sub_earth_lat_deg.clamp(-35.0, 35.0),
            Some(&disc_for_block),
            || blocking_token.is_cancelled(),
        )?;
        planetary_derotation_checkpoint(&blocking_token, "el calculo de derotacion")?;
        Ok::<Vec<u16>, String>(result)
    })
    .await
    .map_err(|e| e.to_string())??;
    planetary_derotation_checkpoint(&job_token, "el calculo de derotacion")?;

    let intended_out_path = derot_default_output_path(&image_path);
    emit_progress(&app, "Guardando TIFF derotado 16-bit...", 82.0, None);
    planetary_derotation_checkpoint(&job_token, "el guardado TIFF")?;
    let staged_tiff =
        StagedDerotationTiff::encode(intended_out_path, &derotated, width, height)?;
    planetary_derotation_checkpoint(&job_token, "el guardado TIFF")?;
    let preview_base64 = derot_encode_preview(&derotated, width, height)?;
    planetary_derotation_checkpoint(&job_token, "la publicacion del resultado")?;
    let out_path = publish_planetary_derotation_if_current(
        &state,
        &job_token,
        staged_tiff,
        StackResult {
            data: derotated,
            width,
            height,
            is_mono: false,
            is_surface: false,
        },
        "la publicacion del resultado",
    )?;
    emit_progress(&app, "Derotacion completada", 100.0, None);
    log_to_front(
        &app,
        "SUCCESS",
        &format!(
            "Derotacion planetaria completada: {} | delta {:.3}°",
            clean_windows_path(out_path.clone()),
            delta_deg
        ),
    );

    Ok(PlanetaryDerotationResult {
        output_path: clean_windows_path(out_path),
        preview_base64,
        width,
        height,
        file_name: Path::new(&image_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("imagen")
            .to_string(),
        planet,
        cm_system: cm_system.min(2),
        delta_deg,
        detected_disc: disc.into(),
        diagnostics,
        b0_deg: sub_earth_lat_deg.clamp(-35.0, 35.0),
        north_angle_deg: raw_disc.angle_deg,
    })
}

#[tauri::command]
async fn apply_current_stacked_planetary_derotation(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    source_path: Option<String>,
    planet: String,
    capture_time: String,
    reference_time: String,
    cm_system: usize,
    limb_strength: f64,
    sub_earth_lat_deg: f64,
    disc_override: Option<DerotationDiscDto>,
) -> Result<PlanetaryDerotationResult, String> {
    state.license_manager.check_access()?;
    let request_id = begin_planetary_user_job(&state);
    let job_token =
        PlanetaryJobToken::for_app(&app, request_id, state.cancel_requested.clone());
    planetary_derotation_checkpoint(&job_token, "el inicio de la derotacion")?;
    let planet_body = crate::derotation::get_planet(&planet)
        .ok_or_else(|| "Planeta no soportado para derotacion".to_string())?;
    let capture_jd = crate::derotation::parse_iso_to_jd(&capture_time)
        .map_err(|e| format!("Tiempo de captura invalido: {}", e))?;
    let reference_jd = crate::derotation::parse_iso_to_jd(&reference_time)
        .map_err(|e| format!("Tiempo de referencia invalido: {}", e))?;

    let stack = {
        let guard = state.stacked_image.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .clone()
            .ok_or_else(|| "No hay una imagen apilada activa para derotar.".to_string())?
    };
    planetary_derotation_checkpoint(&job_token, "la lectura del apilado actual")?;
    let width = stack.width;
    let height = stack.height;
    let rgb = derot_stack_to_rgb16(&stack);
    let mono = derot_rgb_to_mono(&rgb);
    let raw_disc = match disc_override.as_ref() {
        Some(disc) => crate::derotation::PlanetDisc::from(disc),
        None => crate::derotation::detect_planet_disc(&mono, width, height),
    };
    let disc = crate::derotation::validate_disc_aspect(&raw_disc, planet_body);
    let mut geometry = crate::derotation::calculate_observer_geometry(planet_body, capture_jd);
    geometry.sub_earth_lat_deg = sub_earth_lat_deg.clamp(-35.0, 35.0);
    geometry.north_pole_angle_deg = disc.angle_deg;
    let delta_deg = crate::derotation::rotation_delta_deg(
        planet_body,
        reference_jd,
        capture_jd,
        cm_system.min(2),
    );
    let diagnostics = derot_disc_diagnostics(
        &mono,
        width,
        height,
        &disc,
        planet_body,
        "manual",
        0.0,
        0.0,
        capture_jd,
        reference_jd,
        cm_system.min(2),
        geometry,
    );
    if !diagnostics.can_apply {
        return Err("La geometria del disco no es suficientemente confiable. Ajusta centro/radio o carga una imagen planetaria con el disco completo visible.".to_string());
    }
    planetary_derotation_checkpoint(&job_token, "el analisis de geometria")?;

    emit_progress(
        &app,
        "Derotando resultado actual...",
        35.0,
        Some(format!("Delta {:.3} grados", delta_deg)),
    );
    let planet_for_block = planet.clone();
    let disc_for_block = disc.clone();
    let blocking_token = job_token.clone();
    let derotated = tauri::async_runtime::spawn_blocking(move || {
        planetary_derotation_checkpoint(&blocking_token, "el calculo de derotacion")?;
        let planet_body = crate::derotation::get_planet(&planet_for_block)
            .ok_or_else(|| "Planeta no soportado para derotacion".to_string())?;
        let result = crate::derotation::derotate_single_advanced_cancelable(
            &rgb,
            width,
            height,
            3,
            planet_body,
            capture_jd,
            reference_jd,
            limb_strength.clamp(0.0, 2.0),
            cm_system.min(2),
            sub_earth_lat_deg.clamp(-35.0, 35.0),
            Some(&disc_for_block),
            || blocking_token.is_cancelled(),
        )?;
        planetary_derotation_checkpoint(&blocking_token, "el calculo de derotacion")?;
        Ok::<Vec<u16>, String>(result)
    })
    .await
    .map_err(|e| e.to_string())??;
    planetary_derotation_checkpoint(&job_token, "el calculo de derotacion")?;

    let intended_out_path =
        derot_output_path_from_optional_source(source_path.as_deref(), "Current_Stack");
    emit_progress(&app, "Guardando TIFF derotado 16-bit...", 82.0, None);
    planetary_derotation_checkpoint(&job_token, "el guardado TIFF")?;
    let staged_tiff =
        StagedDerotationTiff::encode(intended_out_path, &derotated, width, height)?;
    planetary_derotation_checkpoint(&job_token, "el guardado TIFF")?;
    let preview_base64 = derot_encode_preview(&derotated, width, height)?;
    planetary_derotation_checkpoint(&job_token, "la publicacion del resultado")?;
    let out_path = publish_planetary_derotation_if_current(
        &state,
        &job_token,
        staged_tiff,
        StackResult {
            data: derotated,
            width,
            height,
            is_mono: false,
            is_surface: false,
        },
        "la publicacion del resultado",
    )?;
    emit_progress(&app, "Derotacion completada", 100.0, None);
    log_to_front(
        &app,
        "SUCCESS",
        &format!(
            "Derotacion planetaria del resultado actual completada: {} | delta {:.3}°",
            clean_windows_path(out_path.clone()),
            delta_deg
        ),
    );

    Ok(PlanetaryDerotationResult {
        output_path: clean_windows_path(out_path),
        preview_base64,
        width,
        height,
        file_name: "Resultado apilado actual".to_string(),
        planet,
        cm_system: cm_system.min(2),
        delta_deg,
        detected_disc: disc.into(),
        diagnostics,
        b0_deg: sub_earth_lat_deg.clamp(-35.0, 35.0),
        north_angle_deg: raw_disc.angle_deg,
    })
}

#[tauri::command]
async fn derotate_animation_frames(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    paths: Vec<String>,
    planet: String,
    cm_system: usize,
    limb_strength: f64,
    fallback_interval_sec: Option<f64>,
    sub_earth_lat_deg: Option<f64>,
    north_angle_deg: Option<f64>,
) -> Result<PlanetaryDerotationSequenceResult, String> {
    state.license_manager.check_access()?;
    if paths.is_empty() {
        return Err("No hay frames para derotar".to_string());
    }
    let request_id = begin_planetary_user_job(&state);
    let job_token =
        PlanetaryJobToken::for_app(&app, request_id, state.cancel_requested.clone());
    planetary_derotation_checkpoint(&job_token, "el inicio de la secuencia")?;
    let planet_body = crate::derotation::get_planet(&planet)
        .ok_or_else(|| "Planeta no soportado para derotacion".to_string())?;

    emit_progress(&app, "Preparando derotacion de secuencia...", 5.0, None);
    // Sólo metadata en memoria. Los RGB16 se cargan, derotan, codifican y
    // liberan uno por uno; RAM deja de crecer como N·W·H·6 bytes.
    let mut frames: Vec<(String, usize, usize, f64, String)> = Vec::with_capacity(paths.len());
    for (idx, path) in paths.iter().enumerate() {
        if check_cancel(&state, request_id) {
            return Err("Operacion cancelada por el usuario".to_string());
        }
        let (width, height) = image::image_dimensions(path)
            .map_err(|error| format!("No se pudo leer geometria de {}: {error}", path))?;
        let (meta, time_source) = derot_metadata_with_source(path, None);
        let pct = 5.0 + ((idx as f32 / paths.len() as f32) * 20.0);
        emit_progress(
            &app,
            "Leyendo timestamps de secuencia...",
            pct,
            Some(format!("{} / {}", idx + 1, paths.len())),
        );
        frames.push((
            path.clone(),
            width as usize,
            height as usize,
            meta.mid_time_jd,
            time_source,
        ));
    }
    planetary_derotation_checkpoint(&job_token, "la lectura de la secuencia")?;

    let (width, height) = (frames[0].1, frames[0].2);
    if frames.iter().any(|(_, w, h, _, _)| *w != width || *h != height) {
        return Err("Todos los frames deben tener la misma resolucion para derotacion planetaria".to_string());
    }

    let mut warnings = Vec::new();
    let log_count = frames.iter().filter(|(_, _, _, _, source)| source == "log").count();
    if log_count < frames.len() {
        warnings.push(format!(
            "{} de {} frames no tienen log de captura; valida el intervalo temporal.",
            frames.len() - log_count,
            frames.len()
        ));
    }
    let min_jd = frames
        .iter()
        .map(|(_, _, _, jd, _)| *jd)
        .fold(f64::INFINITY, f64::min);
    let max_jd = frames
        .iter()
        .map(|(_, _, _, jd, _)| *jd)
        .fold(f64::NEG_INFINITY, f64::max);
    let mut time_span_sec = (max_jd - min_jd).abs() * 86400.0;
    let fallback_interval = fallback_interval_sec.unwrap_or(0.0).clamp(0.0, 86400.0);
    if time_span_sec < 1.0 && fallback_interval > 0.0 && frames.len() > 1 {
        let mid = frames.len() / 2;
        let base_jd = frames[mid].3;
        for (idx, frame) in frames.iter_mut().enumerate() {
            let offset = idx as isize - mid as isize;
            frame.3 = base_jd + (offset as f64 * fallback_interval) / 86400.0;
        }
        time_span_sec = fallback_interval * (frames.len().saturating_sub(1)) as f64;
        warnings.push(format!(
            "Se uso intervalo manual de {:.1}s entre frames porque los timestamps no eran utiles.",
            fallback_interval
        ));
    } else if time_span_sec < 1.0 && frames.len() > 1 {
        warnings.push("Los timestamps de la secuencia son casi iguales; la derotacion tendra poco o ningun efecto.".to_string());
    }

    let reference_frame = frames.len() / 2;
    let reference_jd = frames[reference_frame].3;
    let geometry = crate::derotation::calculate_observer_geometry(planet_body, reference_jd);
    let b0 = sub_earth_lat_deg
        .unwrap_or(geometry.sub_earth_lat_deg)
        .clamp(-35.0, 35.0);
    let (reference_rgb, reference_width, reference_height) =
        derot_load_rgb16_image(&frames[0].0)?;
    if reference_width != width || reference_height != height {
        return Err("La geometria del primer frame cambio durante la lectura".into());
    }
    let mono = derot_rgb_to_mono(&reference_rgb);
    drop(reference_rgb);
    let mut disc = crate::derotation::validate_disc_aspect(
        &crate::derotation::detect_planet_disc(&mono, width, height),
        planet_body,
    );
    disc.angle_deg = north_angle_deg.unwrap_or(geometry.north_pole_angle_deg);
    let output_parent = Path::new(&frames[0].0)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let mut staged_sequence = StagedDerotationSequence::create(output_parent)?;

    for (idx, (path, _, _, jd, _)) in frames.into_iter().enumerate() {
        if check_cancel(&state, request_id) {
            return Err("Operacion cancelada por el usuario".to_string());
        }
        planetary_derotation_checkpoint(&job_token, "la derotacion de la secuencia")?;
        let pct = 28.0 + ((idx as f32 / paths.len() as f32) * 66.0);
        emit_progress(
            &app,
            "Derotando frames planetarios...",
            pct,
            Some(format!("{} / {}", idx + 1, paths.len())),
        );

        let (rgb, frame_width, frame_height) = derot_load_rgb16_image(&path)?;
        if frame_width != width || frame_height != height {
            return Err(format!("{} cambio de resolucion durante la secuencia", path));
        }

        let derotated = crate::derotation::derotate_single_advanced_cancelable(
            &rgb,
            width,
            height,
            3,
            planet_body,
            jd,
            reference_jd,
            limb_strength.clamp(0.0, 2.0),
            cm_system.min(2),
            b0,
            Some(&disc),
            || job_token.is_cancelled(),
        )?;
        planetary_derotation_checkpoint(&job_token, "el calculo de un frame")?;
        let stem = Path::new(&path)
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("frame");
        let stem = derot_bounded_file_component(stem, 160);
        let file_name = format!("{:04}_{}_derotated.tiff", idx + 1, stem);
        staged_sequence.stage_rgb16(file_name, &derotated, width, height)?;
        planetary_derotation_checkpoint(&job_token, "el staging de un frame")?;
    }

    planetary_derotation_checkpoint(&job_token, "la publicacion de la secuencia")?;
    let output_paths = publish_planetary_derotation_sequence_if_current(
        &state,
        &job_token,
        staged_sequence,
        "la publicacion de la secuencia",
    )?
    .into_iter()
    .map(clean_windows_path)
    .collect::<Vec<_>>();

    emit_progress(&app, "Secuencia derotada", 100.0, None);
    log_to_front(
        &app,
        "SUCCESS",
        &format!("Derotacion de secuencia completada: {} frames", output_paths.len()),
    );
    Ok(PlanetaryDerotationSequenceResult {
        output_paths,
        reference_frame: reference_frame + 1,
        reference_time_jd: reference_jd,
        time_span_sec,
        warnings,
    })
}

#[tauri::command]
async fn fuse_planetary_derotation_stacks(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    image_paths: Vec<String>,
    planet: String,
    cm_system: usize,
    limb_strength: f64,
    fallback_interval_sec: Option<f64>,
    sub_earth_lat_deg: Option<f64>,
    north_angle_deg: Option<f64>,
    disc_override: Option<DerotationDiscDto>,
) -> Result<PlanetaryDerotationFusionResult, String> {
    state.license_manager.check_access()?;
    if image_paths.len() < 2 {
        return Err("Selecciona al menos 2 stacks planetarios para fusionar.".to_string());
    }
    let request_id = begin_planetary_user_job(&state);
    let job_token =
        PlanetaryJobToken::for_app(&app, request_id, state.cancel_requested.clone());
    planetary_derotation_checkpoint(&job_token, "el inicio de la fusion multi-stack")?;
    let planet_body = crate::derotation::get_planet(&planet)
        .ok_or_else(|| "Planeta no soportado para derotacion".to_string())?;

    emit_progress(&app, "Preparando fusion multi-stack...", 4.0, None);
    let mut frames: Vec<(String, usize, usize, f64, String, DerotationDiagnostics)> =
        Vec::with_capacity(image_paths.len());
    let mut warnings = Vec::new();

    for (idx, path) in image_paths.iter().enumerate() {
        if check_cancel(&state, request_id) {
            return Err("Operacion cancelada por el usuario".to_string());
        }
        let (rgb, width, height) = derot_load_rgb16_image(path)?;
        let mono = derot_rgb_to_mono(&rgb);
        let (meta, time_source) = derot_metadata_with_source(path, None);
        let mut geometry = crate::derotation::calculate_observer_geometry(planet_body, meta.mid_time_jd);
        if let Some(b0) = sub_earth_lat_deg {
            geometry.sub_earth_lat_deg = b0.clamp(-35.0, 35.0);
        }
        let mut disc = match disc_override.as_ref() {
            Some(disc) => crate::derotation::PlanetDisc::from(disc),
            None => crate::derotation::detect_planet_disc(&mono, width, height),
        };
        disc = crate::derotation::validate_disc_aspect(&disc, planet_body);
        disc.angle_deg = north_angle_deg.unwrap_or(geometry.north_pole_angle_deg);
        let diagnostics = derot_disc_diagnostics(
            &mono,
            width,
            height,
            &disc,
            planet_body,
            &time_source,
            meta.duration_sec,
            meta.fps,
            meta.mid_time_jd,
            meta.mid_time_jd,
            cm_system.min(2),
            geometry,
        );
        let pct = 4.0 + ((idx as f32 / image_paths.len() as f32) * 18.0);
        emit_progress(
            &app,
            "Leyendo stacks y logs planetarios...",
            pct,
            Some(format!("{} / {}", idx + 1, image_paths.len())),
        );
        frames.push((
            path.clone(),
            width,
            height,
            meta.mid_time_jd,
            time_source,
            diagnostics,
        ));
    }
    planetary_derotation_checkpoint(&job_token, "la lectura de stacks")?;

    let (width, height) = (frames[0].1, frames[0].2);
    if frames.iter().any(|(_, w, h, _, _, _)| *w != width || *h != height) {
        return Err("Todos los stacks deben tener la misma resolucion para fusion por derotacion.".to_string());
    }

    let log_count = frames
        .iter()
        .filter(|(_, _, _, _, source, _)| source == "log" || source == "manual_log")
        .count();
    if log_count < frames.len() {
        warnings.push(format!(
            "{} de {} stacks no tienen TXT/log de captura; se usara timestamp de archivo o intervalo de respaldo.",
            frames.len() - log_count,
            frames.len()
        ));
    }

    frames.sort_by(|a, b| a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal));
    let mut min_jd = frames
        .iter()
        .map(|(_, _, _, jd, _, _)| *jd)
        .fold(f64::INFINITY, f64::min);
    let mut max_jd = frames
        .iter()
        .map(|(_, _, _, jd, _, _)| *jd)
        .fold(f64::NEG_INFINITY, f64::max);
    let fallback_interval = fallback_interval_sec.unwrap_or(0.0).clamp(0.0, 86400.0);
    if (max_jd - min_jd).abs() * 86400.0 < 1.0 && fallback_interval > 0.0 {
        let mid = frames.len() / 2;
        let base_jd = frames[mid].3;
        for (idx, frame) in frames.iter_mut().enumerate() {
            let offset = idx as isize - mid as isize;
            frame.3 = base_jd + (offset as f64 * fallback_interval) / 86400.0;
        }
        min_jd = frames
            .iter()
            .map(|(_, _, _, jd, _, _)| *jd)
            .fold(f64::INFINITY, f64::min);
        max_jd = frames
            .iter()
            .map(|(_, _, _, jd, _, _)| *jd)
            .fold(f64::NEG_INFINITY, f64::max);
        warnings.push(format!(
            "Se uso intervalo manual de {:.1}s entre stacks porque los timestamps no eran utiles.",
            fallback_interval
        ));
    }

    let reference_frame = frames.len() / 2;
    let reference_jd = frames[reference_frame].3;
    let (reference_rgb, reference_width, reference_height) =
        derot_load_rgb16_image(&frames[reference_frame].0)?;
    if reference_width != width || reference_height != height {
        return Err("El stack de referencia no coincide con la resolucion esperada.".to_string());
    }
    let reference_mono = derot_rgb_to_mono(&reference_rgb);
    let mut reference_disc = match disc_override.as_ref() {
        Some(disc) => crate::derotation::PlanetDisc::from(disc),
        None => crate::derotation::detect_planet_disc(&reference_mono, width, height),
    };
    reference_disc = crate::derotation::validate_disc_aspect(&reference_disc, planet_body);
    let reference_geometry = crate::derotation::calculate_observer_geometry(planet_body, reference_jd);
    let b0 = sub_earth_lat_deg
        .unwrap_or(reference_geometry.sub_earth_lat_deg)
        .clamp(-35.0, 35.0);
    reference_disc.angle_deg = north_angle_deg.unwrap_or(reference_geometry.north_pole_angle_deg);

    let pixel_count = width.saturating_mul(height);
    let reference_derotated = crate::derotation::derotate_single_advanced_cancelable(
        &reference_rgb,
        width,
        height,
        3,
        planet_body,
        reference_jd,
        reference_jd,
        limb_strength.clamp(0.0, 2.0),
        cm_system.min(2),
        b0,
        Some(&reference_disc),
        || job_token.is_cancelled(),
    )?;
    planetary_derotation_checkpoint(&job_token, "la referencia de fusion")?;
    let reference_means =
        derot_channel_means_inside_disc(&reference_derotated, width, height, &reference_disc, 0.72);
    let fusion_mask = derot_disc_mask(width, height, &reference_disc, 1.02);
    let mut accum = vec![0.0f32; pixel_count.saturating_mul(3)];
    let mut accum_weight = vec![0.0f32; pixel_count];
    let mut weights = Vec::with_capacity(frames.len());
    let mut normalization_gains = Vec::with_capacity(frames.len());
    let mut rejected_samples: u64 = 0;
    let mut inspected_samples: u64 = 0;
    let mut final_diag = frames[reference_frame].5.clone();

    for (idx, (path, _, _, jd, source, diagnostics)) in frames.into_iter().enumerate() {
        if check_cancel(&state, request_id) {
            return Err("Operacion cancelada por el usuario".to_string());
        }
        let (rgb, frame_width, frame_height) = derot_load_rgb16_image(&path)?;
        if frame_width != width || frame_height != height {
            return Err(format!(
                "El stack '{}' no coincide con la resolucion de la fusion.",
                Path::new(&path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("stack")
            ));
        }
        let pct = 26.0 + ((idx as f32 / image_paths.len() as f32) * 62.0);
        emit_progress(
            &app,
            "Derotando y fusionando stacks...",
            pct,
            Some(format!("{} / {}", idx + 1, image_paths.len())),
        );

        let quality_weight = (diagnostics.confidence.max(0.15)
            * diagnostics.contrast_ratio.clamp(0.35, 3.0).sqrt()
            * if source == "log" || source == "manual_log" { 1.0 } else { 0.78 })
            .clamp(0.08, 3.0);
        let derotated = if idx == reference_frame {
            reference_derotated.clone()
        } else {
            crate::derotation::derotate_single_advanced_cancelable(
                &rgb,
                width,
                height,
                3,
                planet_body,
                jd,
                reference_jd,
                limb_strength.clamp(0.0, 2.0),
                cm_system.min(2),
                b0,
                Some(&reference_disc),
                || job_token.is_cancelled(),
            )?
        };
        let frame_means =
            derot_channel_means_inside_disc(&derotated, width, height, &reference_disc, 0.72);
        let gains = derot_photometric_gains(reference_means, frame_means);

        for pix in 0..pixel_count {
            if (pix & 0xffff) == 0 {
                planetary_derotation_checkpoint(&job_token, "la fusion de pixeles")?;
            }
            let base = pix * 3;
            let mut pixel_weight = quality_weight as f32;
            if fusion_mask.get(pix).copied().unwrap_or(0) != 0 {
                let mut rejected_channels = 0_u64;
                for c in 0..3 {
                    inspected_samples = inspected_samples.saturating_add(1);
                    let ref_v = reference_derotated[base + c] as f64;
                    let norm_v = derotated[base + c] as f64 * gains[c];
                    let tolerance = (900.0 + ref_v.abs() * 0.18).clamp(900.0, 14000.0);
                    if (norm_v - ref_v).abs() > tolerance {
                        rejected_channels = rejected_channels.saturating_add(1);
                    }
                }
                if rejected_channels > 0 {
                    rejected_samples = rejected_samples.saturating_add(rejected_channels);
                    pixel_weight *= if rejected_channels >= 2 { 0.28 } else { 0.55 };
                }
            }

            accum_weight[pix] += pixel_weight;
            for c in 0..3 {
                let value = (derotated[base + c] as f64 * gains[c]).clamp(0.0, 65535.0) as f32;
                accum[base + c] += value * pixel_weight;
            }
        }
        weights.push(quality_weight);
        normalization_gains.push(gains);
        if idx == reference_frame {
            final_diag = diagnostics;
        }
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Fusion derotacion: {} | peso {:.2} | {}",
                Path::new(&path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("stack"),
                quality_weight,
                source
            ),
        );
    }
    planetary_derotation_checkpoint(&job_token, "la fusion de stacks")?;

    if accum_weight.iter().all(|w| *w <= 0.0) {
        return Err("No se pudo calcular peso valido para fusionar los stacks.".to_string());
    }
    let mut fused = Vec::with_capacity(pixel_count.saturating_mul(3));
    for pix in 0..pixel_count {
        if (pix & 0xffff) == 0 {
            planetary_derotation_checkpoint(&job_token, "la normalizacion de la fusion")?;
        }
        let denom = accum_weight[pix].max(0.0001);
        let base = pix * 3;
        fused.push((accum[base] / denom).round().clamp(0.0, 65535.0) as u16);
        fused.push((accum[base + 1] / denom).round().clamp(0.0, 65535.0) as u16);
        fused.push((accum[base + 2] / denom).round().clamp(0.0, 65535.0) as u16);
    }

    let rejected_pixel_fraction = if inspected_samples > 0 {
        rejected_samples as f64 / inspected_samples as f64
    } else {
        0.0
    };
    if rejected_pixel_fraction > 0.08 {
        warnings.push(format!(
            "La fusion redujo peso en {:.1}% de muestras por variacion local; revisa seeing, enfoque o diferencias de procesado entre stacks.",
            rejected_pixel_fraction * 100.0
        ));
    }

    let parent = Path::new(&image_paths[0])
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let intended_out_path = derot_unique_tiff_path(parent, "Zenith_Derotated_Fusion");
    emit_progress(&app, "Guardando fusion derotada 16-bit...", 92.0, None);
    planetary_derotation_checkpoint(&job_token, "el guardado de la fusion")?;
    let staged_tiff =
        StagedDerotationTiff::encode(intended_out_path, &fused, width, height)?;
    planetary_derotation_checkpoint(&job_token, "el guardado de la fusion")?;
    let preview_base64 = derot_encode_preview(&fused, width, height)?;
    planetary_derotation_checkpoint(&job_token, "la publicacion de la fusion")?;
    let out_path = publish_planetary_derotation_if_current(
        &state,
        &job_token,
        staged_tiff,
        StackResult {
            data: fused,
            width,
            height,
            is_mono: false,
            is_surface: false,
        },
        "la publicacion de la fusion",
    )?;
    let time_span_sec = (max_jd - min_jd).abs() * 86400.0;
    emit_progress(&app, "Fusion derotada completada", 100.0, None);
    log_to_front(
        &app,
        "SUCCESS",
        &format!(
            "Fusion multi-stack derotada: {} stacks | {:.1}s | {}",
            image_paths.len(),
            time_span_sec,
            clean_windows_path(out_path.clone())
        ),
    );

    Ok(PlanetaryDerotationFusionResult {
        output_path: clean_windows_path(out_path),
        preview_base64,
        width,
        height,
        planet,
        cm_system: cm_system.min(2),
        frame_count: image_paths.len(),
        reference_frame: reference_frame + 1,
        reference_time_jd: reference_jd,
        time_span_sec,
        detected_disc: reference_disc.into(),
        diagnostics: final_diag,
        b0_deg: b0,
        north_angle_deg: north_angle_deg.unwrap_or(reference_geometry.north_pole_angle_deg),
        weights,
        normalization_gains,
        rejected_pixel_fraction,
        warnings,
    })
}

/// DEROTACIÓN RGB POR CANAL (cámara mono + rueda de filtros): recibe 3 apilados
/// mono (R, G, B) capturados a tiempos distintos, DEROTA cada uno al tiempo del
/// canal verde (referencia) para eliminar el desfase de rotación entre filtros,
/// y ENSAMBLA el color (R←luma(R derotado), G←luma(G), B←luma(B)). Es el flujo
/// estrella de WinJUPOS para imagers mono. Reutiliza los mismos helpers que la
/// fusión OSC pero SIN promediar: cada canal va a su plano de color.
#[tauri::command]
async fn fuse_planetary_derotation_rgb(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    red_path: String,
    green_path: String,
    blue_path: String,
    planet: String,
    cm_system: usize,
    limb_strength: f64,
    fallback_interval_sec: Option<f64>,
    sub_earth_lat_deg: Option<f64>,
    north_angle_deg: Option<f64>,
    disc_override: Option<DerotationDiscDto>,
) -> Result<PlanetaryDerotationFusionResult, String> {
    state.license_manager.check_access()?;
    let request_id = begin_planetary_user_job(&state);
    let job_token =
        PlanetaryJobToken::for_app(&app, request_id, state.cancel_requested.clone());
    planetary_derotation_checkpoint(&job_token, "el inicio de la derotacion RGB")?;
    let planet_body = crate::derotation::get_planet(&planet)
        .ok_or_else(|| "Planeta no soportado para derotacion".to_string())?;

    let channel_paths = [red_path, green_path, blue_path];
    let channel_names = ["R", "G", "B"];
    let mut warnings: Vec<String> = Vec::new();
    emit_progress(&app, "Preparando derotacion RGB por canal...", 4.0, None);

    // Cargar los 3 canales + su tiempo de captura (log / archivo / reloj).
    let mut loaded: Vec<(Vec<u16>, usize, usize, f64, String)> = Vec::with_capacity(3);
    for (i, path) in channel_paths.iter().enumerate() {
        if check_cancel(&state, request_id) {
            return Err("Operacion cancelada por el usuario".to_string());
        }
        let (rgb, w, h) = derot_load_rgb16_image(path)?;
        let (meta, source) = derot_metadata_with_source(path, None);
        emit_progress(
            &app,
            &format!("Leyendo canal {}...", channel_names[i]),
            4.0 + i as f32 * 6.0,
            None,
        );
        loaded.push((rgb, w, h, meta.mid_time_jd, source));
    }
    planetary_derotation_checkpoint(&job_token, "la lectura de canales RGB")?;
    let (width, height) = (loaded[0].1, loaded[0].2);
    if loaded.iter().any(|(_, w, h, _, _)| *w != width || *h != height) {
        return Err("Los 3 canales (R/G/B) deben tener la misma resolucion.".to_string());
    }

    // Si los timestamps colapsan (sin datos útiles), usar el intervalo de respaldo:
    // R = verde − Δt, B = verde + Δt (orden de captura R→G→B).
    let green_jd = loaded[1].3;
    let span_sec_raw = {
        let mx = loaded.iter().map(|f| f.3).fold(f64::NEG_INFINITY, f64::max);
        let mn = loaded.iter().map(|f| f.3).fold(f64::INFINITY, f64::min);
        (mx - mn).abs() * 86400.0
    };
    let fallback_interval = fallback_interval_sec.unwrap_or(0.0).clamp(0.0, 3600.0);
    if span_sec_raw < 1.0 && fallback_interval > 0.0 {
        loaded[0].3 = green_jd - fallback_interval / 86400.0;
        loaded[2].3 = green_jd + fallback_interval / 86400.0;
        warnings.push(format!(
            "Sin timestamps útiles: se asumió {:.1}s entre canales (R→G→B).",
            fallback_interval
        ));
    }
    let reference_jd = loaded[1].3; // canal verde

    // Disco de referencia (canal verde o override manual) + geometría.
    let green_mono = derot_rgb_to_mono(&loaded[1].0);
    let mut disc = match disc_override.as_ref() {
        Some(d) => crate::derotation::PlanetDisc::from(d),
        None => crate::derotation::detect_planet_disc(&green_mono, width, height),
    };
    disc = crate::derotation::validate_disc_aspect(&disc, planet_body);
    let geometry = crate::derotation::calculate_observer_geometry(planet_body, reference_jd);
    let b0 = sub_earth_lat_deg
        .unwrap_or(geometry.sub_earth_lat_deg)
        .clamp(-35.0, 35.0);
    disc.angle_deg = north_angle_deg.unwrap_or(geometry.north_pole_angle_deg);
    planetary_derotation_checkpoint(&job_token, "la geometria RGB")?;

    let pixel_count = width.saturating_mul(height);
    let mut out = vec![0u16; pixel_count.saturating_mul(3)];
    let mut min_jd = f64::INFINITY;
    let mut max_jd = f64::NEG_INFINITY;

    // Derotar cada canal a reference_jd y colocar su LUMA en el plano de salida.
    for (ch, (rgb, _, _, jd, source)) in loaded.iter().enumerate() {
        if check_cancel(&state, request_id) {
            return Err("Operacion cancelada por el usuario".to_string());
        }
        min_jd = min_jd.min(*jd);
        max_jd = max_jd.max(*jd);
        emit_progress(
            &app,
            &format!("Derotando canal {}...", channel_names[ch]),
            30.0 + ch as f32 * 20.0,
            None,
        );
        let derotated = crate::derotation::derotate_single_advanced_cancelable(
            rgb,
            width,
            height,
            3,
            planet_body,
            *jd,
            reference_jd,
            limb_strength.clamp(0.0, 2.0),
            cm_system.min(2),
            b0,
            Some(&disc),
            || job_token.is_cancelled(),
        )?;
        let mono = derot_rgb_to_mono(&derotated);
        for pix in 0..pixel_count {
            if (pix & 0xffff) == 0 {
                planetary_derotation_checkpoint(&job_token, "el ensamblado RGB")?;
            }
            out[pix * 3 + ch] = mono[pix];
        }
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Derotacion RGB: canal {} desde {} ({})",
                channel_names[ch],
                Path::new(&channel_paths[ch])
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("stack"),
                source
            ),
        );
    }
    planetary_derotation_checkpoint(&job_token, "la derotacion de canales RGB")?;

    let (g_meta, g_source) = derot_metadata_with_source(&channel_paths[1], None);
    let diagnostics = derot_disc_diagnostics(
        &green_mono,
        width,
        height,
        &disc,
        planet_body,
        &g_source,
        g_meta.duration_sec,
        g_meta.fps,
        reference_jd,
        reference_jd,
        cm_system.min(2),
        geometry,
    );

    let parent = Path::new(&channel_paths[1])
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let intended_out_path = derot_unique_tiff_path(parent, "Zenith_Derotated_RGB");
    emit_progress(&app, "Guardando RGB derotado 16-bit...", 92.0, None);
    planetary_derotation_checkpoint(&job_token, "el guardado RGB")?;
    let staged_tiff =
        StagedDerotationTiff::encode(intended_out_path, &out, width, height)?;
    planetary_derotation_checkpoint(&job_token, "el guardado RGB")?;
    let preview_base64 = derot_encode_preview(&out, width, height)?;
    planetary_derotation_checkpoint(&job_token, "la publicacion RGB")?;
    let out_path = publish_planetary_derotation_if_current(
        &state,
        &job_token,
        staged_tiff,
        StackResult {
            data: out,
            width,
            height,
            is_mono: false,
            is_surface: false,
        },
        "la publicacion RGB",
    )?;
    let time_span_sec = (max_jd - min_jd).abs() * 86400.0;
    emit_progress(&app, "Derotacion RGB completada", 100.0, None);
    log_to_front(
        &app,
        "SUCCESS",
        &format!(
            "RGB por canal derotado: 3 canales | {:.1}s | {}",
            time_span_sec,
            clean_windows_path(out_path.clone())
        ),
    );

    Ok(PlanetaryDerotationFusionResult {
        output_path: clean_windows_path(out_path),
        preview_base64,
        width,
        height,
        planet,
        cm_system: cm_system.min(2),
        frame_count: 3,
        reference_frame: 2, // verde
        reference_time_jd: reference_jd,
        time_span_sec,
        detected_disc: disc.into(),
        diagnostics,
        b0_deg: b0,
        north_angle_deg: north_angle_deg.unwrap_or(geometry.north_pole_angle_deg),
        weights: vec![1.0, 1.0, 1.0],
        normalization_gains: vec![[1.0, 1.0, 1.0]; 3],
        rejected_pixel_fraction: 0.0,
        warnings,
    })
}

// Helper: Auto-Stretch 16-bit/8-bit range to 0..255 for feature detection
// This is critical for linear Astro data which often appears "black" in raw 8-bit conversion
fn auto_stretch_gray(img: &image::GrayImage) -> image::GrayImage {
    let total = img.len();
    if total == 0 {
        return img.clone();
    }
    let mut histogram = [0usize; 256];
    for value in img.as_raw() {
        histogram[*value as usize] += 1;
    }
    let percentile = |rank: usize| {
        let mut cumulative = 0usize;
        for (value, count) in histogram.iter().enumerate() {
            cumulative += *count;
            if cumulative > rank {
                return value as u8;
            }
        }
        255
    };
    let min_val = percentile(total.saturating_mul(15) / 1000);
    let max_val = percentile(total.saturating_mul(985) / 1000);

    if max_val <= min_val {
        // Fallback for extremely flat/dark images: use absolute max
        let absolute_max = histogram
            .iter()
            .rposition(|count| *count > 0)
            .unwrap_or(0) as u8;
        if absolute_max > 0 {
            let mut out = image::GrayImage::new(img.width(), img.height());
            let scale = 255.0 / absolute_max as f32;
            for (x, y, p) in img.enumerate_pixels() {
                let val = p[0];
                let new_val = (val as f32 * scale).clamp(0.0, 255.0) as u8;
                out.put_pixel(x, y, image::Luma([new_val]));
            }
            return out;
        }
        return img.clone();
    }

    let mut out = image::GrayImage::new(img.width(), img.height());
    let scale = 255.0 / (max_val as f32 - min_val as f32);

    for (x, y, p) in img.enumerate_pixels() {
        let val = p[0];
        let new_val = ((val as f32 - min_val as f32) * scale).clamp(0.0, 255.0) as u8;
        out.put_pixel(x, y, image::Luma([new_val]));
    }
    out
}

fn auto_stretch_luma16_to_gray(img: &image::ImageBuffer<image::Luma<u16>, Vec<u16>>) -> image::GrayImage {
    let total = img.len();
    if total == 0 {
        return image::GrayImage::new(img.width(), img.height());
    }
    let mut histogram = vec![0u32; 65_536];
    for value in img.as_raw() {
        histogram[*value as usize] += 1;
    }
    let percentile = |rank: usize| {
        let mut cumulative = 0usize;
        for (value, count) in histogram.iter().enumerate() {
            cumulative += *count as usize;
            if cumulative > rank {
                return value as u16;
            }
        }
        65_535
    };
    let min_value = percentile(total.saturating_mul(15) / 1000);
    let mut max_value = percentile(total.saturating_mul(985) / 1000);
    if max_value <= min_value {
        max_value = histogram
            .iter()
            .rposition(|count| *count > 0)
            .unwrap_or(min_value as usize) as u16;
    }
    let span = max_value.saturating_sub(min_value).max(1) as f32;
    image::GrayImage::from_fn(img.width(), img.height(), |x, y| {
        let value = img.get_pixel(x, y)[0];
        let mapped = ((value.saturating_sub(min_value) as f32 / span) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8;
        image::Luma([mapped])
    })
}

#[tauri::command]
async fn stitch_mosaic(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    tiles: Vec<MosaicTileConfig>,
    mode: String,
) -> Result<AnalysisResult, String> {
    let request_id = begin_planetary_user_job(&state);
    let job_token = PlanetaryJobToken::for_app(
        &app,
        request_id,
        state.cancel_requested.clone(),
    );
    if tiles.is_empty() {
        return Err("No hay teselas para unir".to_string());
    }

    log_to_front(
        &app,
        "INFO",
        &format!("Iniciando Mosaico. Modo: Superficie (Forzado/Optimizado)"),
    );
    emit_progress(&app, "Cargando Imagenes...", 5.0, None);

    // 1. Load Images & Detect Features (AKAZE)
    // Akaze works best on full resolution for shape detection.

    // Initialize Akaze: Restore original threshold (0.001) for stability
    let akaze_solver = akaze::Akaze::new(0.001);
    // PR-1.8: DEBE coincidir con la escala real de img_gray (0.75, ver
    // abajo). El 0.5 anterior hacía que el snap del modo guiado buscara con
    // un factor de conversión erróneo (~1.5×) y con rango insuficiente.
    let work_scale = 0.75;

    let mut nodes: Vec<ImageNode> = Vec::with_capacity(tiles.len());
    let mut memory = System::new();
    memory.refresh_memory();
    let mut tile_cache = MosaicTileCache::new(memory.available_memory());

    for (i, t) in tiles.iter().enumerate() {
        if check_cancel(&state, request_id) {
            return Err("OperaciÃ³n cancelada por el usuario".into());
        }
        // Log frontend coords
        log_to_front(
            &app,
            "DEBUG",
            &format!(
                "Tile {}: Frontend Pos {},{} Size {}x{}",
                i, t.x, t.y, t.width, t.height
            ),
        );

        let dyn_img = load_mosaic_master_image(&t.path)
            .map_err(|e| format!("Error cargando {}: {}", t.path, e))?;

        // CRITICAL: Proper 16-bit to 8-bit conversion for AKAZE feature detection
        // Direct to_luma8() loses all contrast in linear astronomical images
        // We need to:
        // 1. Convert to grayscale at full resolution
        // 2. Apply auto_stretch to bring out lunar details (craters, maria)
        // 3. Then pass to AKAZE for feature detection

        let gray_full = dyn_img.to_luma16();
        let gray_stretched = auto_stretch_luma16_to_gray(&gray_full);
        drop(gray_full);
        let img_for_akaze = DynamicImage::ImageLuma8(gray_stretched);

        // Extract Features using Akaze on stretched image
        // Result is (Vec<KeyPoint>, Vec<BitVec>) or similar depending on version.
        // We will infer types and iterate.
        let (kpts, descs) = akaze_solver.extract(&img_for_akaze);

        let mut features = Vec::with_capacity(kpts.len());

        // Akaze 0.7: descs are usually binary (u8 or u64 blocks).
        // specific type might be `akaze::descriptor::Descriptor` which wraps `Vec<u8>`?
        // We iterate zipped.
        for (kpt, desc) in kpts.iter().zip(descs.iter()) {
            // Convert descriptor to Vec<u8> for storage
            // Using simple iteration to be generic safe
            let d_vec: Vec<u8> = desc.iter().cloned().collect();

            features.push(FeaturePoint {
                x: kpt.point.0, // Akaze point is tuple (f32, f32) or Point struct
                y: kpt.point.1,
                // angle removed
                descriptor: d_vec,
            });
        }
        drop(kpts);
        drop(descs);

        // Limit features if too many (sort by response/size?)
        if features.len() > 3000 {
            // Akaze returns sorted by response usually?
            features.truncate(3000);
        }

        // Create gray reference for alignment refinement (snapping)
        // IMPROVED: Use higher resolution (0.75 instead of 0.5) for better matching
        // and use the already-stretched image for SAD optimization
        let small = img_for_akaze.resize(
            (img_for_akaze.width() as f32 * work_scale) as u32,
            (img_for_akaze.height() as f32 * work_scale) as u32,
            image::imageops::FilterType::Lanczos3,
        );
        drop(img_for_akaze);
        let gray_small = small.into_luma8();
        let gray_small_stretched = auto_stretch_gray(&gray_small);
        drop(gray_small);

        // Conservar exactamente el frame elegido y su precisión 16-bit. Para
        // vídeo esto evita volver a escanear/decodificar durante la fusión.
        // La conversión puede necesitar un segundo buffer completo: comprobar
        // RAM antes evita que el allocator aborte el proceso con una tesela
        // patológicamente grande. into_rgba16 reutiliza el buffer si ya es RGBA16.
        if !matches!(&dyn_img, DynamicImage::ImageRgba16(_)) {
            let rgba_bytes = (dyn_img.width() as u64)
                .checked_mul(dyn_img.height() as u64)
                .and_then(|v| v.checked_mul(8))
                .ok_or("Tesela 16-bit fuera de rango")?;
            memory.refresh_memory();
            let free = memory.available_memory();
            let conversion_reserve = (128 * 1024 * 1024).min(free / 2);
            if rgba_bytes > free.saturating_sub(conversion_reserve) {
                return Err(format!(
                    "RAM insuficiente para convertir la tesela {} a RGBA16 ({:.1} MB adicionales).",
                    i + 1,
                    rgba_bytes as f64 / 1_048_576.0
                ));
            }
        }
        let rgba16 = dyn_img.into_rgba16();
        let tile_width = rgba16.width();
        let tile_height = rgba16.height();
        let cache_index = tile_cache.insert(tile_width, tile_height, rgba16.into_raw())?;

        nodes.push(ImageNode {
            idx: i,
            width: tile_width,
            height: tile_height,
            global_x: 0.0,
            global_y: 0.0,
            placed: false,
            img_gray_stretched: gray_small_stretched,
            features,
            original_path: t.path.clone(),
            cache_index,
        });

        emit_progress(
            &app,
            "Analizando (Ayuda AI)...",
            10.0 + (i as f32 / tiles.len() as f32) * 10.0,
            None,
        );
    }

    emit_progress(&app, "Buscando Coincidencias...", 20.0, None);

    // 2. All-to-All Matching & RANSAC
    // Graph Adjacency List
    let mut adjacency: Vec<Vec<MatchEdge>> = vec![Vec::new(); tiles.len()];

    // Parallelize logic if possible? For now, sequential loop over pairs is safer for logic flow
    // O(N^2)
    for i in 0..nodes.len() {
        if check_cancel(&state, request_id) {
            return Err("OperaciÃ³n cancelada por el usuario".into());
        }
        for j in (i + 1)..nodes.len() {
            // Match Node I vs Node J
            // Brute force matching of Descriptors
            // Strategy: For each feature in I, find best match in J.
            // Then filter by consistent translation.

            let feats_i = &nodes[i].features;
            let feats_j = &nodes[j].features;

            // Log feature counts for diagnostics
            log_to_front(
                &app,
                "DEBUG",
                &format!(
                    "Matching T{} ({} features) vs T{} ({} features)",
                    i,
                    feats_i.len(),
                    j,
                    feats_j.len()
                ),
            );

            // REMOVED CONTINUE: If features are empty, we still want to hit the "Guided Fallback" logic below.
            // if feats_i.is_empty() || feats_j.is_empty() {
            //     continue;
            // }

            // ROBUST MATCHING S.O.P. (Standard Operating Procedure):
            // 1. Find Best Match I->J
            // 2. Find Best Match J->I
            // 3. Keep ONLY if they agree (Cross-Check / Reciprocal Match)
            //
            // Forward paralelo: hasta 3000×3000 distancias Hamming por par.
            let forward_matches: Vec<(usize, usize)> = feats_i
                .par_iter()
                .enumerate()
                .filter_map(|(fi_idx, fi)| {
                    // 1. Forward Match I -> J with LOWE'S RATIO and HAMMING DISTANCE
                    let mut best_dist1 = u32::MAX;
                    let mut best_dist2 = u32::MAX;
                    let mut best_j = 0;

                    for (fj_idx, fj) in feats_j.iter().enumerate() {
                        // Use new distance method on FeaturePoint directly
                        let dist = fi.distance(fj);

                        if dist < best_dist1 {
                            best_dist2 = best_dist1;
                            best_dist1 = dist;
                            best_j = fj_idx;
                        } else if dist < best_dist2 {
                            best_dist2 = dist;
                        }
                    }

                    // Lowe's Ratio Test: 0.9 allows more valid matches for lunar surfaces.
                    // Hamming distance < 180 is also more relaxed for noisy data.
                    if best_dist1 < 180 && (best_dist1 as f32) < (best_dist2 as f32 * 0.9) {
                        return Some((fi_idx, best_j));
                    }
                    None
                })
                .collect();

            // Cross-check solo para los J que realmente pasaron Lowe. Antes se
            // recorría todo I de nuevo por candidato; deduplicar J evita trabajo
            // repetido y nunca hace más comparaciones que la versión anterior.
            let mut candidate_js: Vec<usize> =
                forward_matches.iter().map(|&(_, j_idx)| j_idx).collect();
            candidate_js.sort_unstable();
            candidate_js.dedup();
            let reverse_best_i: Vec<usize> = candidate_js
                .par_iter()
                .map(|&j_idx| {
                    let fj = &feats_j[j_idx];
                    let mut best_dist = u32::MAX;
                    let mut best_i = usize::MAX;
                    for (i_idx, fi) in feats_i.iter().enumerate() {
                        let dist = fj.distance(fi);
                        if dist < best_dist {
                            best_dist = dist;
                            best_i = i_idx;
                        }
                    }
                    best_i
                })
                .collect();
            let potential_matches: Vec<_> = forward_matches
                .into_iter()
                .filter_map(|(fi_idx, j_idx)| {
                    let reverse_pos = candidate_js.binary_search(&j_idx).ok()?;
                    (reverse_best_i[reverse_pos] == fi_idx)
                        .then_some((&feats_i[fi_idx], &feats_j[j_idx], fi_idx))
                })
                .collect();

            // RANSAC Translation
            // Find most common (dx, dy) with subpixel precision
            let match_tolerance = 20.0; // Tolerance for RANSAC clustering (pixels)
            let mut best_dx = 0.0f32;
            let mut best_dy = 0.0f32;
            let mut max_inliers = 0;

            let mut shifts = Vec::with_capacity(potential_matches.len());
            for (p1, p2, _) in &potential_matches {
                // Correct direction: Shift J relative to I.
                // dx = p1.x - p2.x (f32)
                let dx = p1.x - p2.x;
                let dy = p1.y - p2.y;
                shifts.push((dx, dy));
            }

            // Consensus O(M²), pero cada candidato se evalúa en paralelo y sin
            // sqrt. collect() conserva el orden: en empates gana el mismo primer
            // candidato que antes, de modo que la salida es determinista.
            let tolerance2 = match_tolerance * match_tolerance;
            let consensus: Vec<(usize, f32, f32)> = shifts
                .par_iter()
                .map(|(ref_dx, ref_dy)| {
                    let mut inliers = 0usize;
                    let mut sx = 0.0f32;
                    let mut sy = 0.0f32;
                    for (dx, dy) in &shifts {
                        let ddx = dx - ref_dx;
                        let ddy = dy - ref_dy;
                        if ddx * ddx + ddy * ddy <= tolerance2 {
                            inliers += 1;
                            sx += dx;
                            sy += dy;
                        }
                    }
                    (inliers, sx, sy)
                })
                .collect();
            for (inliers, sx, sy) in consensus {
                if inliers > max_inliers {
                    max_inliers = inliers;
                    // PR-1.8: MEDIA de los inliers del clúster (subpíxel).
                    // El round() al mejor sample individual metía hasta
                    // ±0.5 px de sesgo por costura.
                    best_dx = sx / inliers.max(1) as f32;
                    best_dy = sy / inliers.max(1) as f32;
                }
            }

            // Log matching results for diagnostics
            log_to_front(
                &app,
                "DEBUG",
                &format!(
                    "T{}<->T{}: {} potential matches, RANSAC inliers: {}",
                    i,
                    j,
                    potential_matches.len(),
                    max_inliers
                ),
            );

            // Calculate Manual/Frontend Alignment expectations FIRST
            let t_i = &tiles[nodes[i].idx];
            let t_j = &tiles[nodes[j].idx];

            // Handle Scale (Frontend Width vs Actual Width)
            let w_i_real = nodes[i].width as f32;
            let w_i_conf = t_i.width as f32;
            let scale_factor = if w_i_conf > 0.0 {
                w_i_real / w_i_conf
            } else {
                1.0
            };

            // manual_dx is vector from I to J in pixels (J.x - I.x)
            let manual_dx_real = (t_j.x - t_i.x) as f32 * scale_factor;
            let manual_dy_real = (t_j.y - t_i.y) as f32 * scale_factor;

            // Decision Logic
            let mut use_ransac = false;
            let mut final_dx = 0.0f32;
            let mut final_dy = 0.0f32;
            let mut final_score = 0;

            if max_inliers > 5 {
                // Minimum 5 inliers for valid match (relaxed for lunar images)
                // RANSAC found a good match. Features are at FULL resolution,
                // and PR-1.8 keeps the consensus SUBPIXEL (mean of inliers).
                let ransac_dx = best_dx;
                let ransac_dy = best_dy;

                log_to_front(
                    &app,
                    "DEBUG",
                    &format!(
                        "T{}<->T{} RANSAC: dx={}, dy={} (from {} inliers)",
                        i, j, ransac_dx, ransac_dy, max_inliers
                    ),
                );

                if mode == "guided" {
                    // VALIDATION CHECK: Is RANSAC consistent with Manual Hint?
                    // Allow generous error (e.g. 1/3 of dimension or 500px) because manual placement is rough.
                    // But if it's completely different (e.g. wrong star field), reject it.
                    let diff_x = (ransac_dx - manual_dx_real).abs();
                    let diff_y = (ransac_dy - manual_dy_real).abs();
                    let threshold = w_i_real * 0.3; // 30% tolerance

                    if diff_x < threshold && diff_y < threshold {
                        use_ransac = true;
                        final_dx = ransac_dx;
                        final_dy = ransac_dy;
                        final_score = max_inliers;
                        log_to_front(
                            &app,
                            "INFO",
                            &format!("Guided Match T{}->T{} CONFIRMED. RANSAC used.", i, j),
                        );
                    } else {
                        log_to_front(&app, "WARN", &format!("Guided Match T{}->T{} REJECTED. RANSAC dist ({},{}) vs Manual ({},{}). Fallback to Snap.", i, j, ransac_dx, ransac_dy, manual_dx_real, manual_dy_real));
                        use_ransac = false;
                    }
                } else {
                    // Blind Mode: Trust RANSAC implicitly
                    use_ransac = true;
                    final_dx = ransac_dx;
                    final_dy = ransac_dy;
                    final_score = max_inliers;
                }
            } else if mode == "guided" {
                // TRUE MANUAL FALLBACK
                // RANSAC failed, but we are in Guided Mode.
                // Trust the manual position IF they are somewhat close in graph topology (adjacent).
                // Logic: Just confirm manual overlap is plausible?
                // Actually, "Guided" means "Use manual position if auto fails".
                // We add the edge with a low score so it's a weak link, but valid.

                final_dx = manual_dx_real;
                final_dy = manual_dy_real;
                final_score = 1; // Weak score
                use_ransac = true;

                log_to_front(
                    &app,
                    "WARN",
                    &format!(
                        "Guided Fallback T{}->T{}: Using Manual Pos ({},{})",
                        i, j, final_dx, final_dy
                    ),
                );
            }

            if use_ransac {
                log_to_front(
                    &app,
                    "INFO",
                    &format!(
                        "Match: T{} -> T{} shift ({}, {}) inliers: {}",
                        i, j, final_dx, final_dy, final_score
                    ),
                );
                adjacency[i].push(MatchEdge {
                    target_idx: j,
                    dx: final_dx,
                    dy: final_dy,
                    score: final_score,
                });
                adjacency[j].push(MatchEdge {
                    target_idx: i,
                    dx: -final_dx,
                    dy: -final_dy,
                    score: final_score,
                });
            } else {
                // FALLBACK: Manual/Frontend Alignment WITH "SNAPPING"
                // Si estamos en modo "guided", intentamos respetar la posicion manual
                // Si estamos en modo NO guided (blind), y RANSAC fallo, NO conectamos (islas realistas).

                if mode != "guided" {
                    continue;
                }

                // GUIDED MODE: Trust user but try to refine (Snap)

                // Convert to Work Scale (Small Gray)
                let guess_dx = (manual_dx_real as f32 * work_scale) as i32;
                let guess_dy = (manual_dy_real as f32 * work_scale) as i32;

                // Try Local Search (Snap)
                let w_small = nodes[i].img_gray_stretched.width() as i32;
                let h_small = nodes[i].img_gray_stretched.height() as i32;

                let mut best_snap_dx = guess_dx;
                let mut best_snap_dy = guess_dy;
                let mut snapped = false;

                // Only snap if overlapping
                if (guess_dx.abs() < w_small) && (guess_dy.abs() < h_small) {
                    let range = 200; // INCREASED: Larger search range (was 80)
                                     // USE STRETCHED IMAGES FOR SAD (Robustness against brightness diffs)
                    let img_i = &nodes[i].img_gray_stretched;
                    let img_j = &nodes[j].img_gray_stretched;
                    let mut best_sad = u64::MAX;

                    for dy in -range..=range {
                        for dx in -range..=range {
                            let try_dx = guess_dx + dx;
                            let try_dy = guess_dy + dy;

                            let start_x_i = 0.max(-try_dx);
                            let start_y_i = 0.max(-try_dy);
                            let end_x_i = w_small.min(w_small - try_dx);
                            let end_y_i = h_small.min(h_small - try_dy);

                            if start_x_i >= end_x_i || start_y_i >= end_y_i {
                                continue;
                            }

                            let center_x = (start_x_i + end_x_i) / 2;
                            let center_y = (start_y_i + end_y_i) / 2;
                            let p_rad = 10;
                            // Bounds Check for Image I (Source)
                            if center_x < p_rad
                                || center_x >= w_small - p_rad
                                || center_y < p_rad
                                || center_y >= h_small - p_rad
                            {
                                continue;
                            }

                            // CRITICAL FIX: Bounds Check for Image J (Target)
                            // We offset center_x/y by try_dx/try_dy to get coords in J
                            // center_j_x = center_x - try_dx, etc.
                            let center_x_j = center_x - try_dx;
                            let center_y_j = center_y - try_dy;

                            let w_small_j = img_j.width() as i32;
                            let h_small_j = img_j.height() as i32;

                            if center_x_j < p_rad
                                || center_x_j >= w_small_j - p_rad
                                || center_y_j < p_rad
                                || center_y_j >= h_small_j - p_rad
                            {
                                continue;
                            }

                            let mut sad = 0u64;
                            for py in -p_rad..=p_rad {
                                for px in -p_rad..=p_rad {
                                    let pix_i = img_i
                                        .get_pixel((center_x + px) as u32, (center_y + py) as u32)
                                        [0] as i32;
                                    // SAFETY: We verified bounds above, but use safe get just in case or standard get
                                    let pix_j = img_j.get_pixel(
                                        (center_x + px - try_dx) as u32,
                                        (center_y + py - try_dy) as u32,
                                    )[0] as i32;
                                    sad += (pix_i - pix_j).abs() as u64;
                                }
                            }
                            if sad < best_sad {
                                best_sad = sad;
                                best_snap_dx = try_dx;
                                best_snap_dy = try_dy;
                            }
                        }
                    }
                    snapped = true;
                }

                let final_dx = if snapped {
                    best_snap_dx as f32 / work_scale
                } else {
                    manual_dx_real
                };
                let final_dy = if snapped {
                    best_snap_dy as f32 / work_scale
                } else {
                    manual_dy_real
                };

                log_to_front(
                    &app,
                    "INFO",
                    &format!("Guided Match: T{}->T{} ({},{})", i, j, final_dx, final_dy),
                );

                adjacency[i].push(MatchEdge {
                    target_idx: j,
                    dx: final_dx,
                    dy: final_dy,
                    score: 5, // Lower score than RANSAC
                });
                adjacency[j].push(MatchEdge {
                    target_idx: i,
                    dx: -final_dx,
                    dy: -final_dy,
                    score: 5,
                });
            }
        }
    }

    // 3. Topology Solving (BFS from Center/Root)
    // Find Root: Node with most connections? Or Tile 0? Tile 0 is usually fine, or max degree.
    let mut root_idx = 0;
    let mut max_deg = 0;
    for (i, edges) in adjacency.iter().enumerate() {
        if edges.len() > max_deg {
            max_deg = edges.len();
            root_idx = i;
        }
    }
    log_to_front(
        &app,
        "INFO",
        &format!("Raiz del Mosaico: T{} ({} conexiones)", root_idx, max_deg),
    );

    // BFS Priority Queue
    let mut queue = std::collections::BinaryHeap::new(); // Use PriorityQueue for Weighted BFS

    // Helper struct for Priority Queue
    #[derive(Eq, PartialEq)]
    struct QueuedNode {
        idx: usize,
        score: u64,
    }
    impl Ord for QueuedNode {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.score
                .cmp(&other.score)
                .then_with(|| self.idx.cmp(&other.idx))
        }
    }
    impl PartialOrd for QueuedNode {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    nodes[root_idx].placed = true;
    nodes[root_idx].global_x = 0.0;
    nodes[root_idx].global_y = 0.0; // Fix: was using uninitialized values if not 0
                                  // Use the work image to confirm root placed (debug)
    log_to_front(
        &app,
        "DEBUG",
        &format!(
            "Raiz colocada. Dim: {}x{}",
            nodes[root_idx].img_gray_stretched.width(),
            nodes[root_idx].img_gray_stretched.height()
        ),
    );

    queue.push(QueuedNode {
        idx: root_idx,
        score: u64::MAX,
    });

    while let Some(qn) = queue.pop() {
        let curr = qn.idx;
        let cx = nodes[curr].global_x;
        let cy = nodes[curr].global_y;

        for edge in &adjacency[curr] {
            let neighbor = edge.target_idx;
            // Use edge.score to prioritize?
            if !nodes[neighbor].placed {
                nodes[neighbor].placed = true;
                nodes[neighbor].global_x = cx + edge.dx;
                nodes[neighbor].global_y = cy + edge.dy;
                // Add to queue with score = edge.score
                // (Algorithmically better to follow strong links first)
                queue.push(QueuedNode {
                    idx: neighbor,
                    score: edge.score as u64,
                });
            }
        }
    }

    // Nunca descartar paneles-isla. Resolver cada componente desconectado con
    // sus propios edges (si los tiene) y empacarlo a la derecha del componente
    // principal. Así un panel de mare liso sigue visible y el usuario puede
    // identificarlo/reubicarlo sin que desaparezca silenciosamente.
    let island_indices: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter_map(|(idx, n)| (!n.placed).then_some(idx))
        .collect();
    let island_names: Vec<String> = island_indices
        .iter()
        .map(|&idx| {
            Path::new(&nodes[idx].original_path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| nodes[idx].original_path.clone())
        })
        .collect();

    if !island_indices.is_empty() {
        let main_min_y = nodes
            .iter()
            .filter(|n| n.placed)
            .map(|n| n.global_y)
            .fold(f32::MAX, f32::min);
        let main_max_x = nodes
            .iter()
            .filter(|n| n.placed)
            .map(|n| n.global_x + n.width as f32)
            .fold(f32::MIN, f32::max);
        let main_max_y = nodes
            .iter()
            .filter(|n| n.placed)
            .map(|n| n.global_y + n.height as f32)
            .fold(f32::MIN, f32::max);
        let island_area: f32 = island_indices
            .iter()
            .map(|&idx| nodes[idx].width as f32 * nodes[idx].height as f32)
            .sum();
        let gap = 96.0f32;
        let target_column_height = (main_max_y - main_min_y)
            .max(island_area.sqrt() * 1.35)
            .max(512.0);
        let mut shelf_x = main_max_x + gap;
        let mut shelf_y = main_min_y;
        let mut column_width = 0.0f32;

        for &seed in &island_indices {
            if nodes[seed].placed {
                continue;
            }
            nodes[seed].placed = true;
            nodes[seed].global_x = 0.0;
            nodes[seed].global_y = 0.0;
            let mut component = vec![seed];
            let mut component_queue = std::collections::BinaryHeap::new();
            component_queue.push(QueuedNode {
                idx: seed,
                score: u64::MAX,
            });
            while let Some(qn) = component_queue.pop() {
                let curr = qn.idx;
                let (cx, cy) = (nodes[curr].global_x, nodes[curr].global_y);
                for edge in &adjacency[curr] {
                    let next = edge.target_idx;
                    if !nodes[next].placed {
                        nodes[next].placed = true;
                        nodes[next].global_x = cx + edge.dx;
                        nodes[next].global_y = cy + edge.dy;
                        component.push(next);
                        component_queue.push(QueuedNode {
                            idx: next,
                            score: edge.score as u64,
                        });
                    }
                }
            }

            let comp_min_x = component
                .iter()
                .map(|&idx| nodes[idx].global_x)
                .fold(f32::MAX, f32::min);
            let comp_min_y = component
                .iter()
                .map(|&idx| nodes[idx].global_y)
                .fold(f32::MAX, f32::min);
            let comp_max_x = component
                .iter()
                .map(|&idx| nodes[idx].global_x + nodes[idx].width as f32)
                .fold(f32::MIN, f32::max);
            let comp_max_y = component
                .iter()
                .map(|&idx| nodes[idx].global_y + nodes[idx].height as f32)
                .fold(f32::MIN, f32::max);
            let comp_w = comp_max_x - comp_min_x;
            let comp_h = comp_max_y - comp_min_y;
            if shelf_y > main_min_y && shelf_y + comp_h > main_min_y + target_column_height {
                shelf_x += column_width + gap;
                shelf_y = main_min_y;
                column_width = 0.0;
            }
            let shift_x = shelf_x - comp_min_x;
            let shift_y = shelf_y - comp_min_y;
            for idx in component {
                nodes[idx].global_x += shift_x;
                nodes[idx].global_y += shift_y;
            }
            shelf_y += comp_h + gap;
            column_width = column_width.max(comp_w);
        }

        log_to_front(
            &app,
            "WARN",
            &format!(
                "{} tesela(s) sin conexión con el componente principal se preservaron como paneles-isla: {}. No se descartó ninguna; aumenta el solape (>20%) para integrarlas.",
                island_names.len(),
                island_names.join(", ")
            ),
        );
        emit_progress(
            &app,
            &format!("Aviso: {} panel(es)-isla preservados", island_names.len()),
            48.0,
            None,
        );
    }

    // 4. Calculate Canvas Bounds & Render
    emit_progress(&app, "Renderizando y Mezclando...", 50.0, None);

    let mut min_x = f32::MAX;
    let mut max_x = f32::MIN;
    let mut min_y = f32::MAX;
    let mut max_y = f32::MIN;

    for n in &nodes {
        if n.global_x < min_x {
            min_x = n.global_x;
        }
        if n.global_y < min_y {
            min_y = n.global_y;
        }

        let r = n.global_x + n.width as f32;
        let b = n.global_y + n.height as f32;
        if r > max_x {
            max_x = r;
        }
        if b > max_y {
            max_y = b;
        }
    }

    let padding = 50.0f32;
    if !min_x.is_finite()
        || !max_x.is_finite()
        || !min_y.is_finite()
        || !max_y.is_finite()
        || max_x <= min_x
        || max_y <= min_y
    {
        return Err("Posiciones de mosaico inválidas después del registro".to_string());
    }
    let cv_w = (max_x - min_x).ceil() as u32 + (padding * 2.0) as u32;
    let cv_h = (max_y - min_y).ceil() as u32 + (padding * 2.0) as u32;

    // Preflight con RAM REAL en este instante, después de AKAZE/cache. Sustituye
    // el cap fijo de 300 MP que podía autorizar ~6.6 GB incluso en equipos sin
    // memoria suficiente. A la vez permite mosaicos grandes en workstations que
    // sí demuestran capacidad, siempre con reserva para SO/WebView.
    memory.refresh_memory();
    let aggregate_tile_pixels = nodes
        .iter()
        .try_fold(0u64, |sum, n| {
            sum.checked_add(n.width as u64 * n.height as u64)
        })
        .ok_or("Área agregada de teselas fuera de rango")?;
    let largest_tile_pixels = nodes
        .iter()
        .map(|n| n.width as u64 * n.height as u64)
        .max()
        .unwrap_or(0);
    let memory_plan = plan_mosaic_canvas_memory(
        cv_w,
        cv_h,
        aggregate_tile_pixels,
        tile_cache.max_spilled_tile_bytes,
        largest_tile_pixels,
        memory.available_memory(),
    )?;
    log_to_front(
        &app,
        "INFO",
        &format!(
            "Preflight mosaico: {:.1} MP, pico RAM {:.2} GB, reserva {:.2} GB, caché 16-bit RAM {:.1} MB",
            memory_plan.pixels as f64 / 1_000_000.0,
            memory_plan.peak_bytes as f64 / 1_073_741_824.0,
            memory_plan.reserve_bytes as f64 / 1_073_741_824.0,
            tile_cache.ram_used as f64 / 1_048_576.0,
        ),
    );
    if cv_w.max(cv_h) > 15000 {
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Mosaico nativo grande detectado: {}x{}. Se renderizara sin reduccion.",
                cv_w, cv_h
            ),
        );
    }

    // 5. Feathering Blending
    // We need: Accumulator Buffer (Float/u32) and Weight Buffer (Float)
    // To handle 16-bit precision, we accumulate in f32.

    // Using simple flat vector for buffers (Width * Height * Channels)
    // This can be huge. 10k x 10k x 3 x 4bytes = 1.2GB. Feasible on modern RAM.

    let len = usize::try_from(memory_plan.pixels)
        .map_err(|_| "El lienzo excede el espacio direccionable de esta plataforma")?;
    let mut acc_r = try_zeroed_vec::<f32>(len, "acumulador rojo del mosaico")?;
    let mut acc_g = try_zeroed_vec::<f32>(len, "acumulador verde del mosaico")?;
    let mut acc_b = try_zeroed_vec::<f32>(len, "acumulador azul del mosaico")?;
    let mut acc_w = try_zeroed_vec::<f32>(len, "pesos del mosaico")?;

    // PR-1.8: FEATHER REAL + IGUALACIÓN DE GANANCIA + SUBPÍXEL.
    // El "blending" anterior era winner-take-all por máxima luminosidad:
    // escalón de brillo abrupto en la costura cuando los paneles difieren en
    // transparencia, y sesgo sistemático a ruido/píxeles calientes (el
    // sample más brillante gana). Ahora: media ponderada por distancia al
    // borde de la tesela (feather), con la exposición de cada panel igualada
    // al lienzo ya acumulado (mediana del ratio en el solape) y colocación
    // subpíxel con muestreo bilineal.
    const FEATHER_PX: f32 = 64.0;
    for (i, n) in nodes.iter().enumerate() {
        if check_cancel(&state, request_id) {
            return Err("Operación cancelada por el usuario".into());
        }
        if !n.placed {
            continue;
        }

        emit_progress(
            &app,
            &format!("Fundiendo tesela {}...", i + 1),
            50.0 + (i as f32 / nodes.len() as f32) * 40.0,
            None,
        );

        // RAM: préstamo sin copia. Spill: una sola lectura secuencial y la Vec
        // se libera al terminar esta iteración. Nunca se reabre/decodifica la
        // fuente original ni se vuelve a seleccionar otro frame del vídeo.
        let rgba_storage = tile_cache.load(n.cache_index)?;
        let rgba = rgba_storage.as_ref();

        let n_w = n.width;
        let n_h = n.height;

        let off_fx = n.global_x - min_x + padding;
        let off_fy = n.global_y - min_y + padding;
        let offset_x = off_fx.floor() as usize;
        let offset_y = off_fy.floor() as usize;
        let frac_x = off_fx - off_fx.floor();
        let frac_y = off_fy - off_fy.floor();

        let crop_margin = 35.0f32;

        // 1) GANANCIA del panel: mediana de lienzo/panel en el solape ya
        // acumulado (el primer panel ancla la exposición de referencia).
        // Corrige la transparencia variable entre capturas — la causa
        // principal del escalón visible en la costura.
        let mut ratios: Vec<f32> = Vec::new();
        for y in (0..n_h).step_by(4) {
            if y % 64 == 0 && job_token.is_cancelled() {
                return Err("Operacion de mosaico cancelada o sustituida".into());
            }
            for x in (0..n_w).step_by(4) {
                let px = rgba16_at(rgba, n_w, x, y);
                if px[3] == 0 {
                    continue;
                }
                let lum_t = px[0] as f32 + px[1] as f32 + px[2] as f32;
                if lum_t < 4500.0 {
                    continue; // solo señal (≈1500 por canal)
                }
                let cv_idx = (offset_y + y as usize) * cv_w as usize + offset_x + x as usize;
                if cv_idx < len && acc_w[cv_idx] > 0.05 {
                    let lum_c = (acc_r[cv_idx] + acc_g[cv_idx] + acc_b[cv_idx]) / acc_w[cv_idx];
                    if lum_c > 4500.0 {
                        ratios.push(lum_c / lum_t);
                    }
                }
            }
        }
        let gain = if ratios.len() >= 200 {
            ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            ratios[ratios.len() / 2].clamp(0.85, 1.18)
        } else {
            1.0
        };
        if (gain - 1.0).abs() > 0.005 {
            log_to_front(
                &app,
                "INFO",
                &format!("Tesela {}: ganancia de exposición x{:.3} (igualada al lienzo)", i + 1, gain),
            );
        }

        // 2) Acumulación feather + bilineal.
        // OPTIMIZED: Parallel execution across rows using Rayon for massive speedup
        use rayon::prelude::*;

        let ptr_r = acc_r.as_mut_ptr() as usize;
        let ptr_g = acc_g.as_mut_ptr() as usize;
        let ptr_b = acc_b.as_mut_ptr() as usize;
        let ptr_w = acc_w.as_mut_ptr() as usize;

        (0..n_h).into_par_iter().for_each(|y| {
            if y % 64 == 0 && job_token.is_cancelled() {
                return;
            }
            let p_r = ptr_r as *mut f32;
            let p_g = ptr_g as *mut f32;
            let p_b = ptr_b as *mut f32;
            let p_w = ptr_w as *mut f32;

            for x in 0..n_w {
                // Muestra del panel en la posición SUBPÍXEL que corresponde
                // al píxel entero del lienzo (backward bilinear).
                let sxf = x as f32 - frac_x;
                let syf = y as f32 - frac_y;
                if sxf < 0.0 || syf < 0.0 {
                    continue;
                }
                let x0 = sxf as u32;
                let y0 = syf as u32;
                if x0 + 1 >= n_w || y0 + 1 >= n_h {
                    continue;
                }
                let p00 = rgba16_at(rgba, n_w, x0, y0);
                let p10 = rgba16_at(rgba, n_w, x0 + 1, y0);
                let p01 = rgba16_at(rgba, n_w, x0, y0 + 1);
                let p11 = rgba16_at(rgba, n_w, x0 + 1, y0 + 1);
                if p00[3] == 0 || p10[3] == 0 || p01[3] == 0 || p11[3] == 0 {
                    continue;
                }
                let fx = sxf - x0 as f32;
                let fy = syf - y0 as f32;
                let w00 = (1.0 - fx) * (1.0 - fy);
                let w10 = fx * (1.0 - fy);
                let w01 = (1.0 - fx) * fy;
                let w11 = fx * fy;
                let r = p00[0] as f32 * w00 + p10[0] as f32 * w10 + p01[0] as f32 * w01 + p11[0] as f32 * w11;
                let g = p00[1] as f32 * w00 + p10[1] as f32 * w10 + p01[1] as f32 * w01 + p11[1] as f32 * w11;
                let b = p00[2] as f32 * w00 + p10[2] as f32 * w10 + p01[2] as f32 * w01 + p11[2] as f32 * w11;

                // 1. EDGE CROP (bordes de sensor sucios) + FEATHER: el peso
                // crece de 0→1 en FEATHER_PX píxeles desde el margen.
                let dx_e = sxf.min(n_w as f32 - 1.0 - sxf);
                let dy_e = syf.min(n_h as f32 - 1.0 - syf);
                let dist = dx_e.min(dy_e);
                if dist < crop_margin {
                    continue;
                }
                let wgt = ((dist - crop_margin) / FEATHER_PX).clamp(0.05, 1.0);

                // 2. BLACK BACKGROUND REJECTION
                // Skip padding and deep space to prevent overlapping solid black on top of craters.
                if r < 1500.0 && g < 1500.0 && b < 1500.0 {
                    continue;
                }

                let cv_idx = (offset_y + y as usize) * cv_w as usize + (offset_x + x as usize);

                if cv_idx < len {
                    unsafe {
                        *p_r.add(cv_idx) += r * gain * wgt;
                        *p_g.add(cv_idx) += g * gain * wgt;
                        *p_b.add(cv_idx) += b * gain * wgt;
                        *p_w.add(cv_idx) += wgt;
                    }
                }
            }
        });
        planetary_derotation_checkpoint(&job_token, "la fusion del mosaico")?;
    }

    // Ya no se necesitan imágenes, grises, descriptores ni spills. Liberarlos
    // antes de reservar RGB16 reduce todavía más el pico real de normalización.
    drop(nodes);
    drop(tile_cache);

    emit_progress(&app, "Finalizando...", 95.0, None);

    // 6. Normalizar bit-exact y soltar 16 B/píxel de acumuladores antes de
    // codificar. Esto reduce el pico posterior de ~34 a 6 B/píxel.
    let out_u16 = normalize_mosaic_accumulators(&acc_r, &acc_g, &acc_b, &acc_w)?;
    drop(acc_r);
    drop(acc_g);
    drop(acc_b);
    drop(acc_w);

    // 7. Save
    // Both artifacts live below one hidden sibling directory.  Only the
    // directory rename makes them visible, so a crash/cancel can never expose
    // just the preview or just the 16-bit master.
    let first_tile_path = Path::new(&tiles[0].path);
    let parent_dir = first_tile_path.parent().unwrap_or(Path::new("."));
    let mut staged_mosaic = StagedPlanetaryDirectory::create(parent_dir, "Mosaic_Result")?;
    let output_base = staged_mosaic
        .final_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Mosaic_Result")
        .to_string();
    let filename_tiff = format!("{output_base}.tiff");
    let filename_preview = format!("{output_base}_NativePreview.png");
    let path_tiff = staged_mosaic.final_path(&filename_tiff);
    let path_preview = staged_mosaic.final_path(&filename_preview);
    let staged_path_tiff = staged_mosaic.temporary_path(&filename_tiff);
    let staged_path_preview = staged_mosaic.temporary_path(&filename_preview);

    // Convert to byte slice for encoder
    let raw_u16 = out_u16.as_slice();
    emit_progress(&app, "Generando PrevisualizaciÃ³n...", 98.0, None);
    // PR-2.6: preview ACOTADO. Un mosaico lunar de 200+ MP generaba un PNG
    // de "preview" a tamaño completo (estirado + codificado + decodificado
    // por el WebView). Se reduce por box-average a ≤2400 px de lado — el
    // TIFF 16-bit completo de al lado sigue siendo el dato real.
    const PREVIEW_MAX_SIDE: u32 = 2400;
    let (preview_bytes, pv_w, pv_h) = if cv_w.max(cv_h) > PREVIEW_MAX_SIDE {
        let factor = (cv_w.max(cv_h) as f32 / PREVIEW_MAX_SIDE as f32).ceil() as usize;
        let dw = (cv_w as usize / factor).max(1);
        let dh = (cv_h as usize / factor).max(1);
        let mut small = try_zeroed_vec::<u16>(dw * dh * 3, "preview del mosaico")?;
        small
            .par_chunks_mut(dw * 3)
            .enumerate()
            .for_each(|(dy, row)| {
                for dx in 0..dw {
                    let (mut sr, mut sg, mut sb, mut n) = (0u64, 0u64, 0u64, 0u64);
                    for oy in 0..factor {
                        let sy = dy * factor + oy;
                        if sy >= cv_h as usize {
                            break;
                        }
                        for ox in 0..factor {
                            let sx = dx * factor + ox;
                            if sx >= cv_w as usize {
                                break;
                            }
                            let idx = (sy * cv_w as usize + sx) * 3;
                            sr += raw_u16[idx] as u64;
                            sg += raw_u16[idx + 1] as u64;
                            sb += raw_u16[idx + 2] as u64;
                            n += 1;
                        }
                    }
                    if n > 0 {
                        row[dx * 3] = (sr / n) as u16;
                        row[dx * 3 + 1] = (sg / n) as u16;
                        row[dx * 3 + 2] = (sb / n) as u16;
                    }
                }
            });
        (to_8bit_preview_visual(&small), dw as u32, dh as u32)
    } else {
        (to_8bit_preview_visual(raw_u16), cv_w, cv_h)
    };

    planetary_derotation_checkpoint(&job_token, "la codificacion del mosaico")?;
    let mut staged_tiff = StagedDerotationTiff::encode(
        staged_path_tiff,
        raw_u16,
        cv_w as usize,
        cv_h as usize,
    )?;

    let mut staged_preview =
        StagedPlanetaryPng::encode_rgb8(staged_path_preview, &preview_bytes, pv_w, pv_h)?;
    // These per-file renames remain inside the hidden transaction directory.
    // If either fails, StagedPlanetaryDirectory::drop deletes the whole tree.
    staged_tiff.publish()?;
    staged_preview.publish()?;
    planetary_derotation_checkpoint(&job_token, "la publicacion del mosaico")?;

    // Determine metadata before moving the master into AppState.
    let is_mono_detected = mosaic_rgb16_is_mono(raw_u16);
    let stack_result = StackResult {
        data: out_u16,
        width: cv_w as usize,
        height: cv_h as usize,
        is_mono: is_mono_detected,
        is_surface: true,
    };

    with_current_planetary_job(
        &state.planetary_generation_gate,
        &job_token,
        "la publicacion atomica del mosaico",
        || -> Result<(), String> {
            staged_mosaic.publish()?;
            *state
                .stacked_image
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(stack_result);
            state.deconv_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
            state.wavelet_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
            state.filter_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
            Ok(())
        },
    )??;

    log_to_front(&app, "INFO", &format!("Mosaico Guardado: {:?}", path_tiff));
    let path_preview_clean =
        clean_windows_path(dunce::canonicalize(&path_preview).unwrap_or(path_preview));
    let path_tiff_clean = clean_windows_path(dunce::canonicalize(&path_tiff).unwrap_or(path_tiff));
    let preview_ref = format!("file_path:{}", path_preview_clean);

    emit_progress(&app, "Listo", 100.0, None);

    Ok(AnalysisResult {
        metadata: VideoMetadata {
            width: cv_w as usize,
            height: cv_h as usize,
            frame_count: tiles.len(),
            bpp: 16,
            color_id: if is_mono_detected { 0 } else { 100 },
            pattern_name: if island_names.is_empty() {
                "Mosaic Blind".to_string()
            } else {
                format!("Mosaic Blind ({} paneles-isla preservados)", island_names.len())
            },
            file_size_mb: 0.0,
            is_color: !is_mono_detected,
        },
        best_frame_idx: 0,
        stats: VideoStats {
            // Dummy stats
            min_pixel: 0,
            max_pixel: 65535,
            avg_brightness: 0.0,
            dynamic_range_pct: 100.0,
            best_score: 100.0,
            worst_score: 100.0,
            avg_quality: 100.0,
            quality_stability: 100.0,
            std_dev: 0.0,
            entropy: 0.0,
        },
        quality_graph: vec![],
        path: path_tiff_clean,
        preview_base64: preview_ref,
        recommended_pct: 100.0,
        ap_points: vec![],
        execution_plan: None,
        stage_telemetry: Vec::new(),
    })
}

// --- COMANDOS DE LICENCIA ---

#[tauri::command]
fn check_license_status(state: State<'_, AppState>) -> AppStatus {
    state.license_manager.get_status()
}

#[tauri::command]
async fn activate_pro_license(
    state: State<'_, AppState>,
    key: String,
    device_name: String,
) -> Result<String, String> {
    state.license_manager.activate_license(&key, &device_name)
}

#[tauri::command]
fn deactivate_license(state: State<'_, AppState>) -> Result<(), String> {
    state.license_manager.deactivate()
}

#[tauri::command]
fn reset_license_state(state: State<'_, AppState>) -> Result<(), String> {
    state.license_manager.reset_license_internal();
    Ok(())
}

// -------------------------------------

// -------------------------------------
// NEW: Explicit White Balance Correction
// Guarantees neutral colors by aligning R/B means to G mean.
// NEW: Professional White Balance Correction (Percentile Based)
// Guarantees neutral highlights (clouds) by aligning R/B 95th percentiles to Green.
fn correct_white_balance(data: &mut [u16], w: usize, h: usize) {
    let len = w * h;
    if len < 100 {
        return;
    }

    // WHITE BALANCE ROBUSTO PARA CUALQUIER TAMAÑO DE OBJETO (pequeño, grande o
    // que llene el cuadro). El umbral de señal ya NO es fijo (2000): se adapta
    // al brillo REAL de la imagen — asi funciona igual con un planeta diminuto
    // sobre negro que con una Luna que ocupa todo el encuadre.
    // 1. Estimar el pico de verde para fijar un umbral relativo.
    let mut g_peak = 0u16;
    {
        let scan = (len / 30000).max(1);
        for i in (0..len).step_by(scan) {
            let g = data[i * 3 + 1];
            if g > g_peak {
                g_peak = g;
            }
        }
    }
    // Umbral = 18% del pico: aisla la SEÑAL del objeto (evita muestrear el
    // fondo como "gris neutro") sin depender del tamaño del objeto.
    let luma_threshold = ((g_peak as f32) * 0.18).clamp(600.0, 40000.0) as u16;

    let step = (len / 20000).max(1);
    let mut sampled_r = Vec::with_capacity(20000);
    let mut sampled_g = Vec::with_capacity(20000);
    let mut sampled_b = Vec::with_capacity(20000);

    for i in (0..len).step_by(step) {
        let off = i * 3;
        let g = data[off + 1];

        // Only sample if there's actual signal
        if g > luma_threshold {
            sampled_r.push(data[off]);
            sampled_g.push(g);
            sampled_b.push(data[off + 2]);
        }
    }

    // Fallback: If the image is extremely dark and we didn't get enough pixels, sample everything.
    if sampled_g.len() < 500 {
        sampled_r.clear();
        sampled_g.clear();
        sampled_b.clear();
        let fallback_step = (len / 2000).max(1);
        for i in (0..len).step_by(fallback_step) {
            let off = i * 3;
            sampled_r.push(data[off]);
            sampled_g.push(data[off + 1]);
            sampled_b.push(data[off + 2]);
        }
    }

    if sampled_g.is_empty() {
        return;
    }

    // Sort to find 95th percentile (highlights)
    sampled_r.sort_unstable();
    sampled_g.sort_unstable();
    sampled_b.sort_unstable();

    // Stability: Instead of just one point, use the average of the top 5% of sampled highlights
    let start_idx = (sampled_g.len() as f32 * 0.95) as usize;
    let end_idx = sampled_g.len();
    let count = (end_idx - start_idx) as f32;

    if count < 1.0 {
        return;
    }

    let mut sum_r = 0.0f32;
    let mut sum_g = 0.0f32;
    let mut sum_b = 0.0f32;

    for i in start_idx..end_idx {
        sum_r += sampled_r[i] as f32;
        sum_g += sampled_g[i] as f32;
        sum_b += sampled_b[i] as f32;
    }

    let p95_r = sum_r / count;
    let p95_g = sum_g / count;
    let p95_b = sum_b / count;

    // Avoid division by zero or black images
    if p95_r < 1.0 || p95_g < 1.0 || p95_b < 1.0 {
        return;
    }

    // 2. Calculate Gains (Target = Green highlights)
    // We boost/cut R and B to match Green's luminosity at the top.
    let gain_r = (p95_g / p95_r).clamp(0.4, 2.5);
    let gain_b = (p95_g / p95_b).clamp(0.4, 2.5);

    // HEADROOM FIX: gains > 1 hard-clipped bright channels (burned highlights
    // in stacked RGB). Renormalize so NO channel gains above 1 — the color
    // ratios are identical, the image just keeps its full highlight detail.
    let max_gain = gain_r.max(gain_b).max(1.0);
    let gain_r = gain_r / max_gain;
    let gain_g = 1.0 / max_gain;
    let gain_b = gain_b / max_gain;

    // 3. Apply Gains
    use rayon::prelude::*;
    data.par_chunks_exact_mut(3).for_each(|pixel| {
        pixel[0] = (pixel[0] as f32 * gain_r + 0.5).clamp(0.0, 65535.0) as u16;
        pixel[1] = (pixel[1] as f32 * gain_g + 0.5).clamp(0.0, 65535.0) as u16;
        pixel[2] = (pixel[2] as f32 * gain_b + 0.5).clamp(0.0, 65535.0) as u16;
    });
}

/// Smooth highlight rolloff in normalised [0,1] space. Identity below the knee,
/// asymptotically compresses everything above it toward 1.0 (no hard clipping).
#[inline]
fn soft_highlight_norm(x: f32) -> f32 {
    let knee = 0.82;
    if x <= knee {
        x
    } else {
        let over = x - knee;
        let head = 1.0 - knee;
        knee + head * (over / (over + head))
    }
}

/// Professional contrast as a smooth S-curve anchored at 0, `pivot` and 1.
/// `k` > 1 increases contrast (steepens around the pivot); `k` < 1 reduces it.
/// The endpoints are preserved exactly, so it never clips shadows or highlights.
#[inline]
fn s_curve_contrast(x: f32, pivot: f32, k: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x < pivot {
        pivot * (x / pivot).powf(k)
    } else {
        1.0 - (1.0 - pivot) * ((1.0 - x) / (1.0 - pivot)).powf(k)
    }
}

fn apply_advanced_color_magic(
    r: &mut f32,
    g: &mut f32,
    b: &mut f32,
    gamma: f32,
    saturation: f32,
    contrast: f32,
    brightness: f32,
    r_bal: f32,
    b_bal: f32,
    contrast_pivot: f32,
    tone_white: f32,
    levels_black: f32,
    levels_white: f32,
    levels_gamma: f32,
) {
    // Professional tone / colour engine working in NORMALISED 16-bit float space.
    // Every adjustment is computed in [0,1] (value / 65535) using smooth, clip-free
    // curves (Lightroom-style) and only converted back to 16-bit at the very end.
    // This is the key difference vs. the old engine, which crushed highlights
    // (gamma normalised against the white point) and clipped hard (linear contrast
    // + additive brightness).
    const N: f32 = 65535.0;
    const INV_N: f32 = 1.0 / 65535.0;

    // --- 1. White Balance (per-channel gain, linear) ------------------------
    // Sliders: -0.5..0.5 (neutral 0.0). Green is the anchor; R/B move relative.
    *r *= 1.0 + r_bal;
    *b *= 1.0 + b_bal;

    let mut rn = (*r * INV_N).max(0.0);
    let mut gn = (*g * INV_N).max(0.0);
    let mut bn = (*b * INV_N).max(0.0);

    // --- 1b. LEVELS (estiramiento por histograma estilo RegiStax) -----------
    // Punto NEGRO / BLANCO de entrada + GAMMA de medios tonos, todo en [0,1].
    // Neutro: black 0, white 1, gamma 1 (identidad). Se aplica ANTES de las
    // curvas de brillo/contraste/gamma para que estas operen sobre el resultado
    // ya estirado (orden "levels → curves" clásico).
    if levels_black > 0.0001
        || (levels_white - 1.0).abs() > 0.0001
        || (levels_gamma - 1.0).abs() > 0.001
    {
        let lb = levels_black.clamp(0.0, 0.98);
        let lw = levels_white.clamp(lb + 0.01, 1.0);
        let inv_span = 1.0 / (lw - lb);
        let inv_lg = 1.0 / levels_gamma.clamp(0.1, 5.0);
        let lv = |v: f32| ((v - lb) * inv_span).clamp(0.0, 1.0).powf(inv_lg);
        rn = lv(rn);
        gn = lv(gn);
        bn = lv(bn);
    }

    // --- 2. Brightness as exposure (multiplicative, hue-preserving) ---------
    // Slider -1..1 -> roughly -1.5..+1.5 stops. Highlights roll off smoothly
    // instead of clipping, exactly how an exposure control behaves in Lightroom.
    if brightness.abs() > 0.0005 {
        let exposure = (brightness * 1.5).exp2();
        rn *= exposure;
        gn *= exposure;
        bn *= exposure;
        if exposure > 1.0 {
            rn = soft_highlight_norm(rn);
            gn = soft_highlight_norm(gn);
            bn = soft_highlight_norm(bn);
        }
    }

    // --- 3. Contrast on luminance, with hue and pivot preserved --------------
    // Applying the S-curve to each RGB channel independently changes colour and
    // can look like a brightness lift. Transforming luminance once and scaling
    // RGB by the same factor makes contrast behave as contrast.
    if (contrast - 1.0).abs() > 0.001 {
        let pivot = (contrast_pivot * INV_N).clamp(0.05, 0.95);
        let k = contrast.clamp(0.1, 3.0);
        let luma = 0.2126 * rn + 0.7152 * gn + 0.0722 * bn;
        let contrasted_luma = s_curve_contrast(luma, pivot, k);
        if luma > 1e-7 {
            let scale = contrasted_luma / luma;
            rn *= scale;
            gn *= scale;
            bn *= scale;
        } else {
            rn = contrasted_luma;
            gn = contrasted_luma;
            bn = contrasted_luma;
        }
    }

    // --- 4. Gamma (midtone power curve over the FULL range) -----------------
    // Normalised against full scale (NOT the white point) so highlights above
    // the estimated white are never crushed. 0->0 and 1->1 stay fixed.
    if (gamma - 1.0).abs() > 0.001 && gamma > 0.0 {
        let inv_gamma = 1.0 / gamma;
        rn = rn.clamp(0.0, 1.0).powf(inv_gamma);
        gn = gn.clamp(0.0, 1.0).powf(inv_gamma);
        bn = bn.clamp(0.0, 1.0).powf(inv_gamma);
    }

    // --- 5. Saturation (luminance-preserving) -------------------------------
    // Neutral 1.0. On mono data r==g==b so lum==r and this is a no-op.
    if (saturation - 1.0).abs() > 0.001 {
        let lum = 0.299 * rn + 0.587 * gn + 0.114 * bn;
        rn = (lum + (rn - lum) * saturation).max(0.0);
        gn = (lum + (gn - lum) * saturation).max(0.0);
        bn = (lum + (bn - lum) * saturation).max(0.0);
    }

    // --- De-normalise. The top end is left to the pipeline soft-clip so any
    // oversaturated channel compresses gracefully instead of hard-clipping.
    let _ = tone_white; // retained in signature; full-range normalisation is used now
    *r = (rn * N).max(0.0);
    *g = (gn * N).max(0.0);
    *b = (bn * N).max(0.0);
}

fn apply_high_pass(
    chan: &Vec<f32>,
    w: usize,
    h: usize,
    sigma: f32,
    amt: f32,
    img_scale: f32,
    protection: ProtectionProfile,
) -> Vec<f32> {
    let blurred = apply_gaussian_blur_safe(chan, w, h, sigma);
    let mut out = vec![0.0; chan.len()];
    for i in 0..chan.len() {
        let hp = chan[i] - blurred[i];
        let mut added = hp * amt;
        // Rodilla: pasado `limit` el exceso se comprime. En Protegido con raiz
        // cuadrada (duplicar el deslizador casi no cambia nada mas alla del
        // codo); en Puro es lineal y el control manda.
        let knee_exp = protection.sharpen_knee_exponent();
        let limit = protection.sharpen_limit(img_scale * 8000.0);
        if added > limit {
            added = limit + (added - limit).powf(knee_exp) * 10.0 * img_scale;
        } else if added < -limit {
            added = -(limit + (-added - limit).powf(knee_exp) * 10.0 * img_scale);
        }
        out[i] = chan[i] + added;
    }
    out
}

fn apply_smart_sharpen_bilateral(
    chan: &Vec<f32>,
    w: usize,
    h: usize,
    radius: f32,
    amt: f32,
    img_scale: f32,
    auto_mask: f32,
    adaptive: &AdaptiveUsmParams,
    input_luminance: Option<&[f32]>,
    protection: ProtectionProfile,
) -> Vec<f32> {
    // FIX: If radius is 0 (default slider pos), use an intelligent default (1.5)
    // allowing "One Slider" operation as requested.
    let effective_radius = if radius < 0.1 { 1.5 } else { radius };
    let knee_exp = protection.sharpen_knee_exponent();

    // SAFE CALL: Use local safe implementation
    let blurred = apply_gaussian_blur_safe(chan, w, h, effective_radius);

    // FIX: Initialize with input to ensure we don't return black if loop fails or logic errors
    let mut out = chan.clone();
    // Bajo este umbral el USM atenua el detalle CUADRATICAMENTE. En Puro pasa a
    // 0, con lo que el detalle fino entra entero (a cambio de amplificar tambien
    // el grano en zonas planas).
    let threshold = protection.usm_fine_threshold(50.0);
    let mask_strength = auto_mask.clamp(0.0, 1.0);
    let confidence = if mask_strength > 0.001 {
        let detail: Vec<f32> = chan
            .iter()
            .zip(blurred.iter())
            .map(|(value, smooth)| value - smooth)
            .collect();
        let noise = estimate_noise_mad(&detail);
        Some(detail_confidence_map(&detail, w, h, noise))
    } else {
        None
    };
    let adaptive_enabled = adaptive.enabled
        && input_luminance.is_some_and(|values| values.len() == chan.len());
    let adaptive_min = adaptive.amount_min.clamp(0.0, 2.0);
    let adaptive_max = adaptive.amount_max.clamp(0.0, 2.0);
    let adaptive_threshold = adaptive.threshold.clamp(0.0, 1.0);
    let adaptive_width = adaptive.transition.clamp(0.005, 1.0);
    let transition_low = adaptive_threshold - adaptive_width * 0.5;
    let transition_high = adaptive_threshold + adaptive_width * 0.5;

    for i in 0..chan.len() {
        let diff = chan[i] - blurred[i];
        let adaptive_scale = if adaptive_enabled {
            let brightness = input_luminance.expect("validated adaptive luminance")[i];
            let t = smoothstep(transition_low, transition_high, brightness);
            adaptive_min + (adaptive_max - adaptive_min) * t
        } else {
            1.0
        };
        let mut added = if threshold <= f32::EPSILON || diff.abs() > threshold {
            // Con `threshold` a 0 (modo Puro) hay que cortocircuitar: la rama de
            // abajo haria 0/0 = NaN cuando `diff` es exactamente cero, y el NaN
            // se propagaria a todo el pixel.
            diff * amt
        } else {
            // Atenuacion cuadratica del detalle fino.
            let factor = (diff.abs() / threshold).powf(2.0);
            diff * amt * factor
        } * adaptive_scale;
        if let Some(ref map) = confidence {
            added *= 1.0 - mask_strength * (1.0 - map[i]);
        }

        let limit = protection.sharpen_limit(img_scale * 8000.0);
        if added > limit {
            added = limit + (added - limit).powf(knee_exp) * 10.0 * img_scale;
        } else if added < -limit {
            added = -(limit + (-added - limit).powf(knee_exp) * 10.0 * img_scale);
        }

        out[i] = chan[i] + added;
    }
    out
}

/// `scale_w`/`scale_h` son las dimensiones que fijan el RADIO del realce local:
/// siempre las del MASTER, aunque `chan` sea un recorte. El sigma se deriva del
/// tamaño de la imagen, asi que medirlo del recorte daria un radio distinto
/// (4K → 30; recorte de 512 → 10.2) y el recuadro mentiria sobre el LCE.
fn apply_clahe_improved(
    chan: &Vec<f32>,
    w: usize,
    h: usize,
    scale_w: usize,
    scale_h: usize,
    amt: f32,
    protection: ProtectionProfile,
) -> Vec<f32> {
    // Robust Local Contrast (LCE) with Limiting
    // FIX: Cap sigma to 30.0 to prevent freezing on large images (convolution explode)
    let dynamic_sigma = (scale_w.max(scale_h) as f32 * 0.02).min(30.0).max(5.0);

    // SAFE CALL: Use local safe implementation
    let blurred = apply_gaussian_blur_safe(chan, w, h, dynamic_sigma);

    let mut out = chan.clone();
    let limit = protection.lce_limit(8000.0);
    let amount_scaled = amt / 100.0;

    if amount_scaled <= 0.001 {
        return chan.clone();
    }

    // Optimization: Parallel iterator if possible, or stick to simple loop
    // rayon is available in this file.
    out.par_iter_mut().enumerate().for_each(|(i, px)| {
        let local_mean = blurred[i];
        let val = chan[i];
        let diff = val - local_mean;
        let boost = diff * amount_scaled;
        let clamped_boost = boost.clamp(-limit, limit);
        *px = val + clamped_boost;
    });
    out
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0).max(1e-6)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn estimate_channel_noise(chan: &[f32], width: usize, height: usize) -> f32 {
    if chan.len() < width * height || width < 3 || height < 3 {
        return 32.0;
    }

    let smooth = box_blur_parallel(chan, width, height, 1);
    let step = (chan.len() / 120_000).max(1);
    let mut residuals: Vec<f32> = chan
        .iter()
        .zip(smooth.iter())
        .step_by(step)
        .map(|(v, s)| (v - s).abs())
        .filter(|v| v.is_finite())
        .collect();

    if residuals.len() < 16 {
        return 32.0;
    }

    residuals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (residuals[residuals.len() / 2] * 1.4826).clamp(8.0, 4096.0)
}

fn apply_luma_preserving_denoise_with(
    chan: &[f32],
    width: usize,
    height: usize,
    amount: f32,
    detail_protect: f32,
    protection: ProtectionProfile,
) -> Vec<f32> {
    // Edge-preserving luminance denoise: a true bilateral filter. It smooths
    // flat/noisy regions hard while leaving edges and fine structure intact.
    // The range kernel auto-scales to the measured noise floor and `detail`
    // tightens it to protect detail; a precomputed LUT keeps the hot loop fast.
    let amount_n = (amount / 100.0).clamp(0.0, 1.0);
    if amount_n <= 0.001 || width < 3 || height < 3 {
        return chan.to_vec();
    }
    let detail_n = (detail_protect / 100.0).clamp(0.0, 1.0);

    // Measured noise sigma drives the range (intensity) kernel automatically.
    let noise = estimate_channel_noise(chan, width, height);

    // Spatial support grows with strength (bilateral radius 1..3).
    let radius: i32 = if amount_n < 0.4 {
        1
    } else if amount_n < 0.78 {
        2
    } else {
        3
    };
    let kdim = (2 * radius + 1) as usize;
    let spatial_sigma = radius as f32 * 0.6 + 0.35;
    let inv_2ss = 1.0 / (2.0 * spatial_sigma * spatial_sigma);

    // Range sigma: how far apart in intensity two pixels can be and still be
    // averaged. Larger -> stronger smoothing; detail_protect shrinks it.
    let range_sigma = (noise * (2.0 + amount_n * 5.0) * (1.3 - detail_n))
        .clamp(noise * 0.6, noise * 14.0)
        .max(10.0);
    let inv_2sr = 1.0 / (2.0 * range_sigma * range_sigma);

    // Precompute the spatial Gaussian weights for the window.
    let mut spatial = vec![0.0f32; kdim * kdim];
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let d2 = (dx * dx + dy * dy) as f32;
            spatial[((dy + radius) * (2 * radius + 1) + (dx + radius)) as usize] =
                (-d2 * inv_2ss).exp();
        }
    }

    // Range-weight LUT keyed by |intensity difference| (no exp() in the hot loop).
    let lut_n = 2048usize;
    let lut_max = (range_sigma * 4.0).max(1.0);
    let mut range_lut = vec![0.0f32; lut_n + 1];
    for i in 0..=lut_n {
        let d = lut_max * i as f32 / lut_n as f32;
        range_lut[i] = (-d * d * inv_2sr).exp();
    }
    let inv_step = lut_n as f32 / lut_max;

    // Global blend so the slider scales the visible strength smoothly.
    // Suelo del 30 %: en Protegido el deslizador NUNCA baja de ese filtrado. Es
    // sobre-suavizado forzado, asi que relajarlo recupera detalle sin introducir
    // ningun artefacto a cambio.
    let blend = protection.denoise_blend(amount_n);

    let w = width as i32;
    let h = height as i32;

    (0..(width * height))
        .into_par_iter()
        .map(|idx| {
            let x = (idx % width) as i32;
            let y = (idx / width) as i32;
            let center = chan[idx];

            let mut wsum = 0.0f32;
            let mut vsum = 0.0f32;
            for dy in -radius..=radius {
                let yy = (y + dy).clamp(0, h - 1);
                let srow = yy * w;
                let sprow = (dy + radius) * (2 * radius + 1);
                for dx in -radius..=radius {
                    let xx = (x + dx).clamp(0, w - 1);
                    let s = chan[(srow + xx) as usize];
                    let sw = spatial[(sprow + (dx + radius)) as usize];
                    let ad = (s - center).abs();
                    let rw = if ad >= lut_max {
                        0.0
                    } else {
                        range_lut[(ad * inv_step) as usize]
                    };
                    let weight = sw * rw;
                    wsum += weight;
                    vsum += weight * s;
                }
            }

            let filtered = if wsum > 1e-6 { vsum / wsum } else { center };
            center + (filtered - center) * blend
        })
        .collect()
}

fn apply_chroma_denoise_planes(
    u: &[f32],
    v: &[f32],
    width: usize,
    height: usize,
    master_amount: f32,
    chroma_amount: f32,
) -> (Vec<f32>, Vec<f32>) {
    let strength = (master_amount / 100.0).clamp(0.0, 1.0) * (chroma_amount / 100.0).clamp(0.0, 1.0);
    if strength <= 0.001 || width < 3 || height < 3 {
        return (u.to_vec(), v.to_vec());
    }

    let radius = if strength < 0.25 { 1 } else if strength < 0.65 { 2 } else { 3 };
    let smooth_u = box_blur_parallel(u, width, height, radius);
    let smooth_v = box_blur_parallel(v, width, height, radius);
    let noise = ((estimate_channel_noise(u, width, height) + estimate_channel_noise(v, width, height)) * 0.5).max(4.0);
    let edge_start = noise * 2.0;
    let edge_end = noise * 7.0;

    let out_u: Vec<f32> = u
        .par_iter()
        .zip(v.par_iter())
        .zip(smooth_u.par_iter().zip(smooth_v.par_iter()))
        .map(|((u0, v0), (us, vs))| {
            let chroma_detail = (u0 - us).abs() + (v0 - vs).abs();
            let flat_weight = 1.0 - smoothstep(edge_start, edge_end, chroma_detail);
            let blend = (strength * (0.25 + flat_weight * 0.65)).clamp(0.0, 0.9);
            u0 + (us - u0) * blend
        })
        .collect();

    let out_v: Vec<f32> = u
        .par_iter()
        .zip(v.par_iter())
        .zip(smooth_u.par_iter().zip(smooth_v.par_iter()))
        .map(|((u0, v0), (us, vs))| {
            let chroma_detail = (u0 - us).abs() + (v0 - vs).abs();
            let flat_weight = 1.0 - smoothstep(edge_start, edge_end, chroma_detail);
            let blend = (strength * (0.25 + flat_weight * 0.65)).clamp(0.0, 0.9);
            v0 + (vs - v0) * blend
        })
        .collect();

    (out_u, out_v)
}

fn apply_master_denoise_channels(
    channels: &mut Vec<Vec<f32>>,
    width: usize,
    height: usize,
    amount: f32,
    detail_protect: f32,
    chroma_amount: f32,
    rgb_mode: bool,
    protection: ProtectionProfile,
) {
    if amount <= 0.001 || channels.is_empty() {
        return;
    }

    if rgb_mode && channels.len() >= 3 {
        let size = width * height;
        let mut y = vec![0.0f32; size];
        let mut u = vec![0.0f32; size];
        let mut v = vec![0.0f32; size];

        for i in 0..size {
            let (cy, cu, cv) = rgb_to_yuv(channels[0][i], channels[1][i], channels[2][i]);
            y[i] = cy;
            u[i] = cu;
            v[i] = cv;
        }

        let y_clean =
            apply_luma_preserving_denoise_with(&y, width, height, amount, detail_protect, protection);
        let (u_clean, v_clean) =
            apply_chroma_denoise_planes(&u, &v, width, height, amount, chroma_amount);

        for i in 0..size {
            let (r, g, b) = yuv_to_rgb(y_clean[i], u_clean[i], v_clean[i]);
            channels[0][i] = r;
            channels[1][i] = g;
            channels[2][i] = b;
        }
    } else {
        channels[0] = apply_luma_preserving_denoise_with(
            &channels[0],
            width,
            height,
            amount,
            detail_protect,
            protection,
        );
    }
}

// *** ADVANCED SCALAR DERINGING ***
// Replaces simple boolean toggle.
// Analyzes difference between Sharpened and Original.
fn apply_advanced_deringing(
    sharp: &mut [f32],
    clean: &[f32],
    _w: usize,
    _h: usize,
    mode: i32,
    radius: f32,
    dark_amt: f32,
    light_amt: f32,
    show_mask: bool,
    channel_idx: usize,
    img_scale: f32, 
) {
    if mode <= 0 { return; }

    // Threshold Scaling - SIGNAL AWARE & ROBUST
    let (eff_radius, eff_dark, eff_light) = if mode == 1 {
        // AUTO MODE (Recommended)
        (1.5, 0.4, 0.05)
    } else {
        // MANUAL MODE 
        // Sliders send values 0-100, so we MUST divide by 100 for proper dampening.
        (
            radius / 8.0, 
            (dark_amt / 100.0).clamp(0.0, 1.0), 
            (light_amt / 100.0).clamp(0.0, 1.0)
        )
    };

    // Use img_scale to ensure threshold is above noise floor
    let threshold = (eff_radius * 200.0 * img_scale).max(50.0);

    sharp
        .par_iter_mut()
        .zip(clean.par_iter())
        .for_each(|(s, o)| {
            let diff = *s - *o;

            if show_mask {
                if diff.abs() > threshold {
                    if diff < 0.0 && eff_dark > 0.0 {
                        // Dark Halo -> PURE RED
                        *s = if channel_idx == 0 { 65535.0 } else { 0.0 };
                    } else if diff > 0.0 && eff_light > 0.0 {
                        // Light Halo -> PURE GREEN
                        *s = if channel_idx == 1 { 65535.0 } else { 0.0 };
                    }
                }
                return;
            }

            // DERINGING LOGIC: Mathematical Soft Clamp
            // Limits the difference directly using the unsharpened reference.
            if diff.abs() > threshold {
                 if diff < 0.0 && eff_dark > 0.0 {
                     // Dark halo overshoot (pixel went too far negative relative to clean ref)
                     let target_s = *o - threshold; // The maximum safe depth
                     *s = *s * (1.0 - eff_dark) + target_s * eff_dark;
                 } else if diff > 0.0 && eff_light > 0.0 {
                     // Light halo overshoot (pixel went too far positive)
                     let target_s = *o + threshold; // The maximum safe height
                     *s = *s * (1.0 - eff_light) + target_s * eff_light;
                 }
            }
        });
}

#[tauri::command]
fn clear_app_memory(state: tauri::State<'_, AppState>) {
    // Inicio explícito de una nueva sesión: este es el único punto del flujo
    // planetario/lote que rearma la cancelación. `clear_stack_memory` no debe
    // tocarla porque se ejecuta entre entradas del mismo lote.
    // Generación, rearme y borrado del resultado forman una sola transición:
    // ningún worker antiguo puede revivir al limpiar el flag, y ningún trabajo
    // nuevo puede publicar entre el cambio de generación y este clear.
    let _ = advance_and_rearm_planetary_session(
        &state.planetary_generation_gate,
        &state.active_req_id,
        state.cancel_requested.as_ref(),
        || {
            state.result_generation.fetch_add(1, Ordering::AcqRel);
            *state
                .stacked_image
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
            *state
                .processed_image
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
            state.deconv_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
            state.wavelet_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
            state.filter_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
            // Mantener el orden de locks que usa el consumidor del lote.
            *state.batch_anchor.lock().unwrap_or_else(|e| e.into_inner()) = None;
            *state.batch_anchor_dims.lock().unwrap_or_else(|e| e.into_inner()) = (0, 0);
        },
    );
}

/// R13: limpieza ligera POR ARCHIVO dentro de un lote. A diferencia de
/// clear_app_memory, NO toca batch_anchor/batch_anchor_dims: borrarlos entre
/// archivos destruia la estabilizacion del timelapse (el anchor nunca
/// sobrevivia mas alla del primer video).
#[tauri::command]
fn clear_stack_memory(state: tauri::State<'_, AppState>) {
    // Invalidación y clear son una sola transición de propietario. La bandera
    // de cancelación NO se rearma: debe permanecer sticky entre entradas del
    // mismo lote.
    let _ = advance_planetary_generation_under_gate(
        &state.planetary_generation_gate,
        &state.active_req_id,
        || {
            state.result_generation.fetch_add(1, Ordering::AcqRel);
            *state
                .stacked_image
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
            *state
                .processed_image
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
            state.deconv_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
            state.wavelet_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
            state.filter_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
        },
    );
    *state.deep_sky_result.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[tauri::command]
async fn zas_stack_video_elite(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
    percent: f32,
    config: EliteConfig,
    category: String,
) -> Result<String, String> {
    // Compatibilidad de una versión: Elite V4 fue retirado de la UI porque su
    // prototipo cargaba el video completo y contenía un warp conceptual. No
    // dejamos el comando registrado apuntando a esa ruta: traduce sus campos al
    // contrato tipado y ejecuta el mismo motor híbrido/paritario que la UI actual.
    let category_key = category.to_ascii_lowercase();
    let is_surface = category_key.contains("surface")
        || category_key.contains("solar")
        || category_key.contains("lunar");
    let response = run_planetary_stack(
        app,
        state,
        PlanetaryStackRequest {
            path,
            percent,
            custom_points: Vec::new(),
            drizzle: 1.0,
            is_surface,
            bayer_override: None,
            ap_size: 48,
            sharpened: config.post_sharpen > 0.0,
            sharpen_intensity: config.post_sharpen.clamp(0.0, 1.0),
            double_pass: true,
            warping_analysis: true,
            anchor_override: None,
            stacking_roi: None,
            normalize_colors: true,
            is_v3: true,
            target_type: category,
            keep_full_frame: Some(false),
            align_rgb: Some(true),
            compute_policy: ComputePolicy::Hybrid,
            decode_policy: DecodePolicy::Auto,
            quality_policy: QualityPolicy::Adaptive,
            profile: PipelineProfile::Custom,
        },
    )
    .await?;
    Ok(response.preview_src)
}

fn main() {
    tauri::Builder::default()
        // El decode-cache mantiene un presupuesto por proceso. Una única
        // instancia convierte ese contrato en presupuesto por máquina y evita
        // que dos ventanas publiquen sobre la misma sesión planetaria.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            let app_data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| PathBuf::from("."));

            // --- MANEJO DE PANICS ---
            // Con panic=unwind el proceso ya no se cierra en seco. Ademas
            // registramos el fallo en un log y avisamos al frontend para mostrar
            // un mensaje (evento "backend_panic") en lugar de desaparecer.
            {
                let handle = app.handle().clone();
                let crash_log = app_data_dir.join("crash_log.txt");
                let default_hook = std::panic::take_hook();
                std::panic::set_hook(Box::new(move |info| {
                    let msg = info.to_string();
                    if let Some(parent) = crash_log.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&crash_log)
                    {
                        use std::io::Write;
                        let _ = writeln!(f, "[{}] {}", chrono::Utc::now().to_rfc3339(), msg);
                    }
                    let _ = handle.emit("backend_panic", msg.clone());
                    default_hook(info);
                }));
            }

            let license_manager = Arc::new(LicenseManager::new(app_data_dir));
            app.manage(AppState {
                stacked_image: Mutex::new(None),
                deep_sky_result: Mutex::new(None),
                processed_image: Mutex::new(None),
                deconv_cache: Mutex::new(Vec::new()),
                wavelet_cache: Mutex::new(Vec::new()),
                filter_cache: Mutex::new(Vec::new()),
                global_stats_cache: Mutex::new(None),
                batch_anchor: Mutex::new(None),
                batch_anchor_dims: Mutex::new((0, 0)),
                planetary_generation_gate: Mutex::new(()),
                active_req_id: AtomicUsize::new(0),
                result_generation: AtomicUsize::new(0),
                cancel_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                job_registry: pipeline::JobRegistry::new(),
                license_manager,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            analyze_video,
            preview_video,
            stack_video,
            apply_wavelets,
            reset_postprocess_state,
            postprocess_histogram,
            sample_postprocess_pixel,
            estimate_postprocess_rgb_alignment,
            analyze_postprocess_artifacts,
            save_final_image,
            export_mosaic_result,
            generate_grid,
            analyze_psf,
            crop_stacked_image,
            scan_directory,
            prepare_batch_output,
            register_batch_output_result,
            export_animation_video,
            process_batch_entry,
            get_ser_conversion_preflight,
            convert_video_to_ser_frontend,
            normalize_batch_brightness,
            realign_animation_frames,
            get_planetary_derotation_preflight,
            get_current_stacked_derotation_preflight,
            detect_planetary_derotation_disc,
            apply_planetary_derotation,
            apply_current_stacked_planetary_derotation,
            derotate_animation_frames,
            fuse_planetary_derotation_stacks,
            fuse_planetary_derotation_rgb,
            get_available_fonts,
            check_ffmpeg_status,
            check_avx2_support,
            get_accel_label,
            get_gpu_info,
            disk_space_info,
            benchmark::get_benchmark_environment,
            benchmark::get_benchmark_dataset_matrix,
            benchmark::begin_benchmark_run,
            benchmark::get_active_benchmark_run,
            benchmark::finish_benchmark_run,
            benchmark::abort_benchmark_run,
            benchmark::clear_pipeline_telemetry,
            benchmark::export_pipeline_telemetry,
            benchmark::generate_benchmark_report,
            benchmark::validate_benchmark_manifest,
            benchmark::compare_linear_masters,
            prepare_deepsky_stack,
            run_deepsky_stack,
            deepsky_cancel_job,
            deepsky_plan_dither,
            prepare_deepsky_session,
            run_deepsky_session,
            stack_deepsky,
            deepsky_probe,
            inspect_deepsky_frames,
            deepsky_scan_classify,
            deepsky_restretch,
            deepsky_frame_preview,
            deepsky_result_view,
            deepsky_export,
            deepsky_export_float32,
            deepsky_histogram,
            deepsky_combine_channels,
            deepsky_split_channels,
            deepsky_dualband_hoo,
            spcc_calibrate,
            check_license_status,
            activate_pro_license,
            deactivate_license,
            reset_license_state,
            cancel_processing,
            analyze_video_v2,
            analyze_planetary,
            set_decode_cache_location,
            stop_analysis,
            activate_license,
            ver_licencia,
            stitch_mosaic,
            load_image_thumbnail,
            generate_smart_ap_grid,     // Smart APs (Integrated)
            stack_video_liquid_warping, // Liquid Warping V2
            run_planetary_stack,
            zas_stack_video_elite,          // Zenith Elite V4
            clear_app_memory,
            clear_stack_memory,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
