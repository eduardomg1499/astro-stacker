// ==========================================
// 1. ESTRUCTURAS DE DATOS
// ==========================================

#[derive(Clone, Debug)]
pub struct FfmpegReader {
    pub path: String,
    pub width: usize,
    pub height: usize,
    pub frame_count: usize,
    /// True only when the container/log supplied an authoritative count (or
    /// ffprobe counted packets). Duration×fps is explicitly an estimate.
    pub frame_count_exact: bool,
    pub bytes_per_pixel: usize,
    pub sample_bits: usize,
    pub color_id: i32,
    pub ffmpeg_path: String,
    pub fps: f64,
    pub is_color: bool,
    pub rotation: i32,
    pub codec_name: String,
    pub pixel_format: Option<String>,
    pub color_range: Option<String>,
    pub color_matrix: Option<String>,
    // THE NEW PERSISTENT STREAM CACHE
    pub stream_cache: Arc<Mutex<Option<(usize, FfmpegStreamIterator)>>>,
}

static PLANETARY_FFMPEG_THREAD_BUDGET: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub fn set_planetary_ffmpeg_thread_budget(threads: usize) {
    PLANETARY_FFMPEG_THREAD_BUDGET.store(
        threads.max(1),
        std::sync::atomic::Ordering::Release,
    );
}

#[tauri::command]
#[allow(dead_code)]
fn check_avx2_support() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return true;
        }
    }
    false
}

/// Best SIMD backend actually enabled at RUNTIME for the hot loops (SAD match,
/// Lanczos accumulation, alignment enhance). x86 is probed per-CPU; aarch64
/// (Apple Silicon) always has NEON as a baseline. This is what powers the
/// acceleration label — so a Mac shows "NEON" instead of a bogus "SIMD OFF".
fn simd_backend_label() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            "AVX2"
        } else if is_x86_feature_detected!("avx") {
            "AVX"
        } else if is_x86_feature_detected!("sse4.1") {
            "SSE4.1"
        } else {
            "Escalar"
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        "NEON"
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        "Escalar"
    }
}

fn os_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "SO"
    }
}

/// e.g. "AVX2 · Windows · GPU DX12 (RTX 3060)" or "NEON · macOS · GPU Metal
/// (Apple M2)". Shown on the progress screen so the user can confirm hardware
/// acceleration (SIMD + GPU compute) is engaged on whatever OS/GPU they run.
#[tauri::command]
fn get_accel_label() -> String {
    format!(
        "{} · {}{}",
        simd_backend_label(),
        os_label(),
        crate::gpu_stack::accel_suffix()
    )
}

/// Deteccion de GPU compute para la UI: nombre/backend/VRAM presupuestada y
/// estado del self-test de paridad numerica GPU-vs-CPU.
#[tauri::command]
fn get_gpu_info() -> crate::gpu_stack::GpuInfo {
    crate::gpu_stack::gpu_info()
}

// NUEVO: FunciÃ³n unificada para generar ruta de cachÃ© de anÃ¡lisis
fn get_analysis_cache_path(video_path: &str, mode_suffix: &str) -> String {
    // Normalizar path para evitar mismatches por ", /, o mayÃºsculas
    let p = Path::new(video_path);
    // Usamos el path limpio, pero conservamos la integridad del nombre base
    let clean = clean_windows_path(p.to_path_buf());
    format!("{}_{}.analysis_v2", clean, mode_suffix)
}

/// Caché alterno para fuentes de solo lectura (SD bloqueada, NAS, medios
/// ópticos). El nombre no expone la ruta del usuario y es estable entre
/// análisis/apilado y reinicios de la aplicación.
fn get_analysis_cache_fallback_path(primary_cache_path: &str) -> String {
    use sha2::{Digest as _, Sha256};

    let digest = Sha256::digest(primary_cache_path.as_bytes());
    std::env::temp_dir()
        .join("astro_stacker_analysis_cache")
        .join(format!("{}.analysis_v2", hex::encode(digest)))
        .to_string_lossy()
        .into_owned()
}

fn analysis_cache_candidates(primary_cache_path: &str) -> [String; 2] {
    [
        primary_cache_path.to_owned(),
        get_analysis_cache_fallback_path(primary_cache_path),
    ]
}

/// Fingerprint estable del origen planetario. Incluye versión algorítmica,
/// ruta canónica, tamaño/mtime y muestras de contenido; para una secuencia FITS
/// en carpeta incluye cada entrada. Evita reutilizar desplazamientos cuando el
/// usuario sobrescribe un SER/MP4 conservando nombre y geometría.
fn planetary_source_fingerprint(path: &str) -> Result<u64, String> {
    use std::hash::{Hash, Hasher};
    use std::io::{Read, Seek, SeekFrom};

    const ALGORITHM_VERSION: &str = "planetary-analysis-hybrid-v2-a11";
    const SAMPLE_BYTES: usize = 64 * 1024;
    let source = Path::new(path);
    let metadata = std::fs::metadata(source)
        .map_err(|error| format!("No se pudo identificar el origen '{path}': {error}"))?;
    let canonical = source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    ALGORITHM_VERSION.hash(&mut hasher);
    clean_windows_path(canonical).hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .hash(&mut hasher);

    if metadata.is_file() {
        let mut file = File::open(source)
            .map_err(|error| format!("No se pudo muestrear el origen '{path}': {error}"))?;
        let mut sample = vec![0u8; SAMPLE_BYTES.min(metadata.len() as usize)];
        let first = file
            .read(&mut sample)
            .map_err(|error| format!("No se pudo leer el origen '{path}': {error}"))?;
        sample[..first].hash(&mut hasher);
        if metadata.len() > SAMPLE_BYTES as u64 {
            file.seek(SeekFrom::End(-(SAMPLE_BYTES as i64)))
                .map_err(|error| format!("No se pudo muestrear el final de '{path}': {error}"))?;
            sample.resize(SAMPLE_BYTES, 0);
            let last = file
                .read(&mut sample)
                .map_err(|error| format!("No se pudo leer el final de '{path}': {error}"))?;
            sample[..last].hash(&mut hasher);
        }
    } else if metadata.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(source)
            .map_err(|error| format!("No se pudo enumerar la secuencia '{path}': {error}"))?
            .flatten()
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        entries.len().hash(&mut hasher);
        for entry in entries {
            entry.file_name().hash(&mut hasher);
            if let Ok(meta) = std::fs::metadata(&entry) {
                meta.len().hash(&mut hasher);
                meta.modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos())
                    .hash(&mut hasher);
            }
        }
    }
    Ok(hasher.finish())
}

// CACHE DE ANALISIS V2 EN BINCODE+LZ4. Antes era JSON: con grid_scores de
// 40×40 u64 por frame (warping activo), un video de 10k frames producia un
// .analysis_v2 de >100 MB que se re-parseaba con serde_json AL INICIO DE CADA
// APILADO (segundos de espera + pico de RAM 2-3× el archivo) y se reescribia
// completo en cada clic. bincode+LZ4 es 10-50× mas rapido y ~5× mas compacto.
// COMPATIBILIDAD: parse intenta el formato nuevo y cae a JSON legado, asi los
// caches ya escritos junto a los videos siguen siendo validos sin re-analizar.
// NOTA bincode: sin nombres de campo — si CachedAnalysis cambia de forma,
// bumpear el sufijo del cache (zenith_analysis_cache_suffix) para invalidar.
const ANALYSIS_CACHE_PAYLOAD_MAGIC: &[u8; 8] = b"ZACv10\0\0";
const ANALYSIS_CACHE_HEADER_BYTES: usize = 8 + 8 + 8 + 4;
// El caché es una optimización, no una razón válida para arriesgar un OOM. Una
// captura cuya tabla supere 1 GiB se vuelve a analizar; el siguiente salto de
// escala debe usar un formato segmentado/mmap en lugar de ampliar esta cota.
const ANALYSIS_CACHE_MAX_DECOMPRESSED_BYTES: usize = 1024 * 1024 * 1024;
const ANALYSIS_CACHE_MAX_FILE_BYTES: usize = ANALYSIS_CACHE_MAX_DECOMPRESSED_BYTES
    + ANALYSIS_CACHE_MAX_DECOMPRESSED_BYTES / 255
    + 64 * 1024;

fn deserialize_cached_analysis_bounded(bin: &[u8]) -> Option<CachedAnalysis> {
    use bincode::Options as _;

    if bin.len() > ANALYSIS_CACHE_MAX_DECOMPRESSED_BYTES {
        return None;
    }
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .with_limit(ANALYSIS_CACHE_MAX_DECOMPRESSED_BYTES as u64)
        .deserialize::<CachedAnalysis>(bin)
        .ok()
}

fn encode_cached_analysis(cached: &CachedAnalysis) -> Result<Vec<u8>, String> {
    let bin = bincode::serialize(cached)
        .map_err(|error| format!("No se pudo serializar el análisis: {error}"))?;
    if bin.len() > ANALYSIS_CACHE_MAX_DECOMPRESSED_BYTES {
        return Err(format!(
            "El análisis serializado supera el límite seguro de {} MiB",
            ANALYSIS_CACHE_MAX_DECOMPRESSED_BYTES / (1024 * 1024)
        ));
    }
    let compressed = lz4_flex::compress_prepend_size(&bin);
    let mut envelope = Vec::with_capacity(ANALYSIS_CACHE_HEADER_BYTES + compressed.len());
    envelope.extend_from_slice(ANALYSIS_CACHE_PAYLOAD_MAGIC);
    envelope.extend_from_slice(&(bin.len() as u64).to_le_bytes());
    envelope.extend_from_slice(&(compressed.len() as u64).to_le_bytes());
    envelope.extend_from_slice(&crc32fast::hash(&bin).to_le_bytes());
    envelope.extend_from_slice(&compressed);
    if envelope.len() > ANALYSIS_CACHE_MAX_FILE_BYTES {
        return Err("El caché comprimido supera el límite seguro de archivo".into());
    }
    Ok(envelope)
}

fn parse_cached_analysis(raw: &[u8]) -> Option<CachedAnalysis> {
    // SNIFF antes de intentar LZ4: decompress_size_prepended lee los primeros
    // 4 bytes como TAMANO a asignar — en un JSON legado ('{"sc...') eso es una
    // peticion de ~1.67 GB que se asigna y descarta en cada lectura (hipo en
    // macOS; en un Windows justo de RAM puede abortar el proceso). Un cache
    // JSON siempre empieza con '{': ir directo a serde_json en ese caso.
    if raw.len() > ANALYSIS_CACHE_MAX_FILE_BYTES {
        return None;
    }
    if raw.first() == Some(&b'{') {
        if raw.len() > ANALYSIS_CACHE_MAX_DECOMPRESSED_BYTES {
            return None;
        }
        return serde_json::from_slice::<CachedAnalysis>(raw).ok();
    }

    // Formato actual: el tamaño declarado se valida ANTES de pedir memoria y
    // el CRC cubre los bytes bincode ya descomprimidos. Un bit-flip que siga
    // siendo LZ4/bincode válido nunca puede alterar scores o shifts en silencio.
    if raw.starts_with(ANALYSIS_CACHE_PAYLOAD_MAGIC) {
        if raw.len() < ANALYSIS_CACHE_HEADER_BYTES {
            return None;
        }
        let declared = u64::from_le_bytes(raw[8..16].try_into().ok()?) as usize;
        let compressed_len = u64::from_le_bytes(raw[16..24].try_into().ok()?) as usize;
        let expected_crc = u32::from_le_bytes(raw[24..28].try_into().ok()?);
        if declared > ANALYSIS_CACHE_MAX_DECOMPRESSED_BYTES
            || compressed_len != raw.len().saturating_sub(ANALYSIS_CACHE_HEADER_BYTES)
        {
            return None;
        }
        let compressed = &raw[ANALYSIS_CACHE_HEADER_BYTES..];
        if compressed.len() < 4
            || u32::from_le_bytes(compressed[..4].try_into().ok()?) as usize != declared
        {
            return None;
        }
        let bin = lz4_flex::decompress_size_prepended(compressed).ok()?;
        if bin.len() != declared || crc32fast::hash(&bin) != expected_crc {
            return None;
        }
        return deserialize_cached_analysis_bounded(&bin);
    }

    // Compatibilidad con el LZ4 legado, pero con el mismo límite previo a la
    // asignación. Se reescribirá con envelope en la siguiente publicación.
    if raw.len() >= 4 {
        let declared = u32::from_le_bytes(raw[..4].try_into().ok()?) as usize;
        if declared <= ANALYSIS_CACHE_MAX_DECOMPRESSED_BYTES {
            if let Ok(bin) = lz4_flex::decompress_size_prepended(raw) {
                if bin.len() == declared {
                    if let Some(cached) = deserialize_cached_analysis_bounded(&bin) {
                        return Some(cached);
                    }
                }
            }
        }
    }
    serde_json::from_slice::<CachedAnalysis>(raw).ok()
}

fn load_cached_analysis(cache_path: &str) -> Option<CachedAnalysis> {
    recover_interrupted_analysis_cache(cache_path);
    let metadata = fs::metadata(cache_path).ok()?;
    if !metadata.is_file() || metadata.len() > ANALYSIS_CACHE_MAX_FILE_BYTES as u64 {
        return None;
    }
    parse_cached_analysis(&fs::read(cache_path).ok()?)
}

/// Recupera backups creados por versiones anteriores, cuyo replace hacía
/// target→previous→target. El escritor actual usa replace atómico, pero esta
/// reparación conserva capturas existentes tras un cierre en aquella ventana.
fn recover_interrupted_analysis_cache(cache_path: &str) {
    let target = Path::new(cache_path);
    if target.exists() {
        return;
    }
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let Some(name) = target.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let prefix = format!(".{name}.");
    let mut backups: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(parent)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let candidate = path.file_name()?.to_str()?;
            if !candidate.starts_with(&prefix) || !candidate.ends_with(".previous") {
                return None;
            }
            Some((entry.metadata().ok()?.modified().ok()?, path))
        })
        .collect();
    backups.sort_by_key(|(modified, _)| *modified);
    if let Some((_, newest)) = backups.pop() {
        let _ = fs::rename(newest, target);
    }
}

static ANALYSIS_CACHE_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Serializa fuera del lock de generación y escribe un temporal exclusivo.
/// El nombre PID+secuencia evita que dos análisis solapados compartan bytes.
fn stage_cached_analysis(cache_path: &str, cached: &CachedAnalysis) -> Result<PathBuf, String> {
    use std::io::Write as _;
    use std::sync::atomic::Ordering as AtomicOrdering;

    let compressed = encode_cached_analysis(cached)?;
    let target = Path::new(cache_path);
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("analysis_v2");
    let sequence = ANALYSIS_CACHE_TEMP_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
    let temporary = parent.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| {
            format!(
                "No se pudo crear el caché temporal '{}': {error}",
                temporary.display()
            )
        })?;
    if let Err(error) = file.write_all(&compressed).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "No se pudo completar el caché temporal '{}': {error}",
            temporary.display()
        ));
    }
    Ok(temporary)
}

#[cfg(target_os = "windows")]
fn atomic_replace_analysis_file(temporary: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;
    let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: ambas cadenas están terminadas en NUL y permanecen vivas durante
    // la llamada; MoveFileExW no conserva los punteros.
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
fn atomic_replace_analysis_file(temporary: &Path, target: &Path) -> std::io::Result<()> {
    // POSIX rename reemplaza el destino como una sola operación de namespace.
    fs::rename(temporary, target)
}

/// Publica mediante replace atómico; nunca existe una ventana en la que el
/// destino válido haya sido retirado y el nuevo aún no esté instalado.
fn commit_staged_analysis(cache_path: &str, temporary: &Path) -> Result<(), String> {
    let target = Path::new(cache_path);
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    if let Err(error) = atomic_replace_analysis_file(temporary, target) {
        let _ = fs::remove_file(temporary);
        return Err(format!(
            "No se pudo publicar el caché '{}': {error}",
            target.display()
        ));
    }
    // El archivo temporal ya fue fsync; sincronizar el directorio completa la
    // durabilidad del rename en Unix. Algunos filesystems no lo permiten: el
    // replace sigue siendo atómico y ese sync queda best-effort.
    #[cfg(not(target_os = "windows"))]
    let _ = File::open(parent).and_then(|directory| directory.sync_all());
    Ok(())
}

/// F3: al guardar un caché de análisis, borra los HERMANOS del MISMO vídeo
/// con TAG DE VERSIÓN de métrica distinto (p.ej. los "_a6" huérfanos tras
/// el bump a "_a7"): cada bump dejaba cientos de MB de grid_scores muertos
/// junto a los vídeos del usuario. Los cachés de OTROS MODOS con la versión
/// vigente (warp/global, anchor, otras categorías) se conservan; solo se
/// tocan archivos nuestros (mismo prefijo de vídeo + extensión propia).
fn prune_stale_analysis_caches(current_cache_path: &str) {
    fn version_tag(name: &str) -> Option<String> {
        let stem = name.strip_suffix(".analysis_v2")?;
        let i = stem.rfind("_a")?;
        let digits: String = stem[i + 2..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if digits.is_empty() {
            None
        } else {
            Some(format!("_a{digits}"))
        }
    }
    let current = Path::new(current_cache_path);
    let Some(dir) = current.parent() else { return };
    let Some(cache_name) = current
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
    else {
        return;
    };
    let Some(cur_ver) = version_tag(&cache_name) else {
        return;
    };
    // Prefijo estable "video.ext_": el sufijo de modo va tras la extensión
    // de vídeo conocida (cache = "{video}_{sufijo}.analysis_v2").
    let Some(video_prefix) = cache_name.strip_suffix(".analysis_v2").and_then(|stem| {
        ["ser_", "avi_", "mp4_", "mov_", "mkv_", "fits_", "fit_"]
            .iter()
            .filter_map(|ext| stem.to_ascii_lowercase().rfind(ext).map(|i| i + ext.len()))
            .max()
            .map(|end| stem[..end].to_ascii_lowercase())
    }) else {
        return;
    };
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name == cache_name || !name.ends_with(".analysis_v2") {
            continue;
        }
        if !name.to_ascii_lowercase().starts_with(&video_prefix) {
            continue;
        }
        // Solo versiones DISTINTAS a la vigente (y con tag reconocible).
        if version_tag(&name).is_some_and(|v| v != cur_ver) {
            let _ = fs::remove_file(e.path());
        }
    }
}

// NUEVO: Helper para detectar ColorID en SER sin cargar todo el archivo
fn peek_ser_color_id(path: &str) -> i32 {
    use std::io::Read;
    if let Ok(mut f) = File::open(path) {
        let mut buf = [0u8; 26];
        if f.read_exact(&mut buf).is_ok() {
            // SER v3: los enteros del header son siempre little-endian. El
            // indicador del offset 22 sólo afecta los samples >8 bit.
            return i32::from_le_bytes([buf[18], buf[19], buf[20], buf[21]]);
        }
    }
    100 // Default to RGB if error
}

fn ser_color_is_native_decodable(color_id: i32) -> bool {
    color_id == 0
        || ser::ser_color_is_bayer(color_id)
        || ser::ser_color_is_direct_rgb(color_id)
        || ser::ser_color_is_direct_bgr(color_id)
        || ser::ser_color_is_yuv422(color_id)
}

// NUEVO: Helper para parsear archivos de log de SharpCap (.txt)
fn parse_sharpcap_metadata(video_path: &str) -> (Option<usize>, Option<f64>) {
    let mut txt_path = std::path::PathBuf::from(video_path);
    txt_path.set_extension("txt");

    if !txt_path.exists() {
        return (None, None);
    }

    let content = std::fs::read_to_string(txt_path).unwrap_or_default();
    let mut frame_count = None;
    let mut actual_fps = None;

    for line in content.lines() {
        if line.starts_with("FrameCount=") {
            if let Ok(c) = line.replace("FrameCount=", "").parse::<usize>() {
                frame_count = Some(c);
            }
        } else if line.starts_with("ActualFrameRate=") {
            let fps_str = line
                .replace("ActualFrameRate=", "")
                .replace("fps", "")
                .trim()
                .to_string();
            if let Ok(f) = fps_str.parse::<f64>() {
                actual_fps = Some(f);
            }
        }
    }
    (frame_count, actual_fps)
}

type FfmpegCancelCheck = Arc<dyn Fn() -> bool + Send + Sync + 'static>;

// Un MP4/MOV planetario normal sólo tiene que identificar un stream de vídeo.
// Los límites anteriores (2 GiB / ~35 min de media analizada, con deadlines de
// 45 s) convertían una apertura problemática en una espera de uno o dos
// minutos antes de empezar. Estos límites siguen siendo holgados para headers
// grandes, pero fallan pronto y dejan que el decode secuencial confirme el EOF.
const FFPROBE_METADATA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);
const FFPROBE_PACKET_COUNT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const FFMPEG_PROBE_SIZE: &str = "16M";
const FFMPEG_ANALYZE_DURATION: &str = "8M";
const FFMPEG_TOOL_IDENTITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const FFMPEG_STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const FFMPEG_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// `-progress` avanza con los frames de SALIDA. Un filtro `select` disperso no
/// emite nada mientras decodifica el hueco anterior al siguiente seleccionado,
/// por lo que el timeout normal de 20 s puede matar un HEVC/AV1 válido. Sólo
/// los streams selectos reciben esta tolerancia adaptativa; la cancelación del
/// usuario sigue comprobándose cada 25 ms por el watchdog.
fn ffmpeg_selected_idle_timeout(indices: &[usize], frame_size_bytes: usize) -> std::time::Duration {
    // PR-05: el ritmo conservador de decode escala con la GEOMETRÍA. El valor
    // fijo de 4 fps era correcto a 1080p pero mataba decodes HEVC 20 MP sanos
    // en equipos lentos (<4 fps reales) al toparse el techo antiguo de 120 s.
    const BASELINE_PIXELS: usize = 2_073_600; // 1080p
    const BASELINE_MILLI_FPS: usize = 4_000; // 4 fps a 1080p
    const MIN_MILLI_FPS: usize = 500; // 0.5 fps en frames gigantes (20 MP HEVC)
    const STARTUP_MARGIN_SECS: usize = 5;
    const MAX_SELECTED_IDLE_SECS: usize = 600;

    // El pipe selecto de análisis es G16 (2 B/px); con RGB48 esto sobreestima
    // los píxeles ×3 y sólo ALARGA el margen (dirección segura, techo 600 s).
    let pixels = (frame_size_bytes / 2).max(1);
    let milli_fps = BASELINE_MILLI_FPS
        .saturating_mul(BASELINE_PIXELS)
        .checked_div(pixels)
        .unwrap_or(BASELINE_MILLI_FPS)
        .clamp(MIN_MILLI_FPS, BASELINE_MILLI_FPS);

    let first_gap = indices
        .first()
        .copied()
        .unwrap_or(0)
        .saturating_add(1);
    let max_gap = indices
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0]))
        .fold(first_gap, usize::max);
    let estimated_secs = max_gap
        .saturating_mul(1000)
        .div_ceil(milli_fps)
        .saturating_add(STARTUP_MARGIN_SECS)
        .clamp(
            FFMPEG_STREAM_IDLE_TIMEOUT.as_secs() as usize,
            MAX_SELECTED_IDLE_SECS,
        );
    std::time::Duration::from_secs(estimated_secs as u64)
}

fn ffmpeg_effective_idle_timeout(
    selected_indices: Option<&[usize]>,
    frame_size_bytes: usize,
    normal_timeout: std::time::Duration,
) -> std::time::Duration {
    selected_indices
        .map(|indices| ffmpeg_selected_idle_timeout(indices, frame_size_bytes))
        .unwrap_or(normal_timeout)
}
const MAX_PLANETARY_FRAME_DIMENSION: usize = 262_144;
const MAX_SUPERVISED_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

fn spawn_bounded_output_reader<R: std::io::Read + Send + 'static>(
    mut reader: R,
    limit: usize,
) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
        let mut chunk = [0u8; 8192];
        let mut truncated = false;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    let remaining = limit.saturating_sub(bytes.len());
                    bytes.extend_from_slice(&chunk[..read.min(remaining)]);
                    truncated |= read > remaining;
                }
                Err(_) => break,
            }
        }
        (bytes, truncated)
    })
}

fn kill_and_reap_child(child: &mut std::process::Child) -> Result<(), String> {
    match child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("no se pudo consultar el proceso hijo: {error}"));
        }
    }
    let kill_result = child.kill();
    let wait_result = child.wait();
    match (kill_result, wait_result) {
        (_, Ok(_)) => Ok(()),
        (Err(kill_error), Err(wait_error)) => Err(format!(
            "no se pudo terminar ({kill_error}) ni cosechar ({wait_error}) el proceso hijo"
        )),
        (Ok(()), Err(wait_error)) => {
            Err(format!("se terminó el proceso, pero no se pudo cosechar: {wait_error}"))
        }
    }
}

/// Ejecuta una herramienta FFmpeg sin `Command::output`: stdout/stderr se
/// drenan en paralelo y el supervisor impone deadline/cancelación. Toda salida
/// (éxito, error, timeout o cancelación) cosecha el hijo antes de retornar.
fn run_command_supervised(
    mut command: Command,
    timeout: std::time::Duration,
    cancel: Option<&FfmpegCancelCheck>,
    label: &str,
) -> Result<std::process::Output, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("No se pudo iniciar {label}: {error}"))?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = kill_and_reap_child(&mut child);
            return Err(format!("{label} no expuso stdout"));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(stdout);
            let _ = kill_and_reap_child(&mut child);
            return Err(format!("{label} no expuso stderr"));
        }
    };
    let stdout_thread = spawn_bounded_output_reader(stdout, MAX_SUPERVISED_OUTPUT_BYTES);
    let stderr_thread = spawn_bounded_output_reader(stderr, MAX_SUPERVISED_OUTPUT_BYTES);
    let started = std::time::Instant::now();

    let status = loop {
        if cancel.is_some_and(|check| check()) {
            let reap = kill_and_reap_child(&mut child);
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return match reap {
                Ok(()) => Err(format!("{label} cancelado o sustituido por otro trabajo")),
                Err(error) => Err(format!("{label} cancelado; además {error}")),
            };
        }
        if started.elapsed() >= timeout {
            let reap = kill_and_reap_child(&mut child);
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            return match reap {
                Ok(()) => Err(format!(
                    "{label} excedió el límite estricto de {:.1} s y fue terminado",
                    timeout.as_secs_f32()
                )),
                Err(error) => Err(format!(
                    "{label} excedió {:.1} s; además {error}",
                    timeout.as_secs_f32()
                )),
            };
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(error) => {
                let reap = kill_and_reap_child(&mut child);
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(match reap {
                    Ok(()) => format!("No se pudo supervisar {label}: {error}"),
                    Err(reap_error) => {
                        format!("No se pudo supervisar {label}: {error}; además {reap_error}")
                    }
                });
            }
        }
    };

    // `try_wait(Some)` ya cosechó al hijo. Cerrar ambos pipes hace que los
    // drainers terminen; unirlos impide threads huérfanos en cada apertura.
    let (stdout, stdout_truncated) = stdout_thread
        .join()
        .map_err(|_| format!("El drainer de stdout de {label} entró en pánico"))?;
    let (mut stderr, stderr_truncated) = stderr_thread
        .join()
        .map_err(|_| format!("El drainer de stderr de {label} entró en pánico"))?;
    if stdout_truncated {
        return Err(format!(
            "{label} devolvió más de {} MiB en stdout; se rechaza metadata descontrolada",
            MAX_SUPERVISED_OUTPUT_BYTES / (1024 * 1024)
        ));
    }
    if stderr_truncated {
        stderr.extend_from_slice(b"\n[diagnostico truncado por Zenith]");
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn available_planetary_frame_budget_bytes() -> usize {
    let mut system = System::new();
    system.refresh_memory();
    let available = usize::try_from(system.available_memory()).unwrap_or(usize::MAX);
    let total = usize::try_from(system.total_memory()).unwrap_or(usize::MAX);
    if available == 0 {
        return 512 * 1024 * 1024;
    }
    // El pipe, el ring y los scratch de análisis coexisten: un solo frame no
    // puede consumir más de 1/4 de la RAM disponible ni más de 4 GiB. PERO en
    // macOS la "disponible" instantánea puede caer a cientos de MB con las
    // cachés del sistema llenas (se reclaman bajo presión) — eso NO es un OOM
    // real, y abortaba un frame 20MP RGB48 legítimo (111.6 MiB) en una
    // máquina de 24 GB (baseline 2026-07-17). Suelo: 1/32 de la RAM TOTAL,
    // que el OS siempre puede liberar para una asignación puntual.
    let floor = total / 32;
    (available / 4)
        .max(floor)
        .min(4usize.saturating_mul(1024 * 1024 * 1024))
}

fn validate_planetary_frame_geometry_with_budget(
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    budget: usize,
    context: &str,
) -> Result<usize, String> {
    if width == 0 || height == 0 || bytes_per_pixel == 0 {
        return Err(format!(
            "{context}: geometría o formato vacío ({width}x{height}, {bytes_per_pixel} B/px)"
        ));
    }
    if width > MAX_PLANETARY_FRAME_DIMENSION || height > MAX_PLANETARY_FRAME_DIMENSION {
        return Err(format!(
            "{context}: dimensión {width}x{height} excede el límite planetario de {MAX_PLANETARY_FRAME_DIMENSION} px por eje"
        ));
    }
    let pixels = width.checked_mul(height).ok_or_else(|| {
        format!("{context}: overflow al calcular {width}×{height} píxeles")
    })?;
    let bytes = pixels.checked_mul(bytes_per_pixel).ok_or_else(|| {
        format!(
            "{context}: overflow al calcular {width}×{height}×{bytes_per_pixel} bytes"
        )
    })?;
    if bytes > budget {
        return Err(format!(
            "{context}: un frame requiere {:.1} MiB, por encima del presupuesto adaptativo de {:.1} MiB según la RAM libre",
            bytes as f64 / (1024.0 * 1024.0),
            budget as f64 / (1024.0 * 1024.0)
        ));
    }
    Ok(bytes)
}

fn validate_planetary_frame_geometry(
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    context: &str,
) -> Result<usize, String> {
    validate_planetary_frame_geometry_with_budget(
        width,
        height,
        bytes_per_pixel,
        available_planetary_frame_budget_bytes(),
        context,
    )
}

fn parse_positive_ratio(value: &str, label: &str) -> Result<Option<f64>, String> {
    if value.is_empty()
        || value.eq_ignore_ascii_case("unknown")
        || value.eq_ignore_ascii_case("n/a")
        || matches!(value, "0:0" | "0:1" | "1:0")
    {
        return Ok(None);
    }
    let mut parts = value.split(':');
    let Some(numerator) = parts.next() else {
        return Ok(None);
    };
    let Some(denominator) = parts.next() else {
        return Err(format!("FFprobe devolvió {label} inválido: {value}"));
    };
    if parts.next().is_some() {
        return Err(format!("FFprobe devolvió {label} inválido: {value}"));
    }
    let numerator = numerator
        .parse::<f64>()
        .map_err(|_| format!("FFprobe devolvió {label} inválido: {value}"))?;
    let denominator = denominator
        .parse::<f64>()
        .map_err(|_| format!("FFprobe devolvió {label} inválido: {value}"))?;
    if !numerator.is_finite()
        || !denominator.is_finite()
        || numerator <= 0.0
        || denominator <= 0.0
    {
        return Err(format!("FFprobe devolvió {label} no positivo/finito: {value}"));
    }
    let ratio = numerator / denominator;
    if !ratio.is_finite() || !(0.0001..=10_000.0).contains(&ratio) {
        return Err(format!("FFprobe devolvió {label} fuera de rango: {value}"));
    }
    Ok(Some(ratio))
}

impl FfmpegReader {
    pub fn new(path: &str, app: &tauri::AppHandle) -> Result<Self, String> {
        Self::new_with_cancel(path, app, None)
    }

    fn new_with_cancel(
        path: &str,
        app: &tauri::AppHandle,
        cancel: Option<FfmpegCancelCheck>,
    ) -> Result<Self, String> {
        let ffprobe_path = get_ffprobe_command(app);
        let ffmpeg_path = get_ffmpeg_command(app);

        // Obtener sólo metadata; el tamaño total del archivo no exige escanear
        // 2 GiB. En MP4/MOV local, el demuxer busca el átomo `moov` directamente
        // aunque esté al final. El análisis posterior sigue decodificando por
        // posición absoluta y confirma el EOF, por lo que reducir el probe no
        // sacrifica exactitud de índices.
        let output = {
            let mut cmd = Command::new(&ffprobe_path);
            #[cfg(target_os = "windows")]
            cmd.creation_flags(0x08000000);
            cmd.args([
                "-v",
                "error",
                "-analyzeduration",
                FFMPEG_ANALYZE_DURATION,
                "-probesize",
                FFMPEG_PROBE_SIZE,
                "-show_format",
                "-show_streams",
                "-of",
                "json",
                path,
            ]);
            run_command_supervised(
                cmd,
                FFPROBE_METADATA_TIMEOUT,
                cancel.as_ref(),
                "FFprobe de metadata",
            )
        }
        .map_err(|e| {
            format!(
                "Error al ejecutar FFprobe: {}. {}",
                e,
                ffmpeg_install_hint()
            )
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "FFprobe no pudo leer el video. {} {}",
                stderr.trim(),
                ffmpeg_install_hint()
            ));
        }

        let json_str = String::from_utf8_lossy(&output.stdout);
        let probe: serde_json::Value = serde_json::from_str(&json_str).map_err(|e| {
            let stderr = String::from_utf8_lossy(&output.stderr);
            format!("Error al parsear JSON de FFprobe: {}. {}", e, stderr.trim())
        })?;

        // Find the first VIDEO stream
        let mut stream_idx = 0;
        let mut found_video = false;

        if let Some(streams) = probe["streams"].as_array() {
            for (i, s) in streams.iter().enumerate() {
                let c_type = s["codec_type"].as_str().unwrap_or("unknown");
                log_to_front(
                    app,
                    "INFO",
                    &format!("DEBUG PROBE STREAM {}: type={}", i, c_type),
                );

                if c_type.to_lowercase() == "video" {
                    stream_idx = i;
                    found_video = true;
                    break;
                }
            }
        }

        if !found_video {
            // Fallback: Use 0 if streams exist but none marked "video" (weird edge case)
            let stream_count = probe["streams"].as_array().map(|s| s.len()).unwrap_or(0);
            if stream_count > 0 {
                log_to_front(
                    app,
                    "WARN",
                    "No se encontro 'codec_type=video', usando stream 0 por defecto.",
                );
                stream_idx = 0;
            } else {
                return Err(
                    "No se detecto stream de video (codec_type=video) en JSON de FFprobe. El contenedor MOV puede estar incompleto o requiere una version mas reciente de FFmpeg/FFprobe.".into(),
                );
            }
        }

        let stream = &probe["streams"][stream_idx];
        let format = &probe["format"];
        let format_name = format["format_name"].as_str().unwrap_or("unknown");
        let dar = stream["display_aspect_ratio"].as_str().unwrap_or("unknown");

        log_to_front(
            app,
            "INFO",
            &format!("DEBUG STREAM: Format={}, DAR={}", format_name, dar),
        );

        let mut width = usize::try_from(stream["width"].as_u64().unwrap_or(0))
            .map_err(|_| "FFprobe devolvió un ancho que no cabe en esta plataforma".to_string())?;
        let mut height = usize::try_from(stream["height"].as_u64().unwrap_or(0))
            .map_err(|_| "FFprobe devolvió un alto que no cabe en esta plataforma".to_string())?;
        if width == 0 || height == 0 {
            return Err(format!(
                "FFprobe devolvió dimensiones inválidas: {width}x{height}. Archivo corrupto o stream incompleto"
            ));
        }
        // Valida el tamaño codificado antes de usarlo en divisiones/DAR. El
        // formato final (mono16/rgb48) se valida de nuevo al conocer el CFA.
        validate_planetary_frame_geometry(width, height, 2, "Metadata FFmpeg")?;
        let pix_fmt = stream["pix_fmt"].as_str().unwrap_or("unknown");
        let sample_bits = stream["bits_per_raw_sample"]
            .as_str()
            .and_then(|v| v.parse::<usize>().ok())
            .or_else(|| stream["bits_per_raw_sample"].as_u64().map(|v| v as usize))
            .filter(|&v| v > 0)
            .unwrap_or_else(|| {
                [16usize, 14, 12, 10, 9]
                    .into_iter()
                    .find(|bits| pix_fmt.contains(&bits.to_string()))
                    .unwrap_or(8)
            });
        let codec_tag = stream["codec_tag_string"].as_str().unwrap_or("");
        let codec_name = stream["codec_name"]
            .as_str()
            .unwrap_or("unknown")
            .to_lowercase();
        let metadata_value = |key: &str| {
            stream[key]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("unknown"))
                .map(str::to_string)
        };
        let pixel_format = metadata_value("pix_fmt");
        let color_range = metadata_value("color_range");
        // FFprobe denomina `color_space` a la matriz (bt709, bt2020nc...).
        let color_matrix = metadata_value("color_space");

        // DetecciÃ³n de rotaciÃ³n ULTRA-ROBUSTA (v4)
        let mut rotation = 0;

        let clean_rotation = |val: &serde_json::Value| -> Option<i32> {
            let s = if let Some(sv) = val.as_str() {
                sv.to_string()
            } else {
                val.to_string()
            };
            let clean: String = s
                .chars()
                .filter(|c| c.is_digit(10) || *c == '-' || *c == '.')
                .collect();
            clean.parse::<f64>().ok().and_then(|value| {
                if value.is_finite()
                    && value >= i32::MIN as f64
                    && value <= i32::MAX as f64
                {
                    Some(value.round() as i32)
                } else {
                    None
                }
            })
        };

        // 1. Check Stream Tags Exhaustively
        if let Some(tags) = stream["tags"].as_object() {
            for (k, v) in tags {
                let key_low = k.to_lowercase();
                if key_low.contains("rotat") || key_low.contains("orient") {
                    if let Some(r) = clean_rotation(v) {
                        rotation = r;
                        break;
                    }
                }
            }
        }

        // 2. Check side_data_list (Display Matrix)
        if rotation == 0 {
            if let Some(side_data) = stream["side_data_list"].as_array() {
                for item in side_data {
                    if let Some(r) = item["rotation"].as_i64() {
                        rotation = r as i32;
                        break;
                    } else if let Some(r_f) = item["rotation"].as_f64() {
                        rotation = r_f as i32;
                        break;
                    }
                }
            }
        }

        // 3. Check Container/Format Tags
        if rotation == 0 {
            if let Some(tags) = probe["format"]["tags"].as_object() {
                for (k, v) in tags {
                    let key_low = k.to_lowercase();
                    if key_low.contains("rotat") || key_low.contains("orient") {
                        if let Some(r) = clean_rotation(v) {
                            rotation = r;
                            break;
                        }
                    }
                }
            }
        }

        // 3.5 HEURISTIC: Force 90 if video is high-res landscape but metadata is from mobile
        if rotation == 0 && width == 1920 && height == 1080 {
            let format_tags = probe["format"]["tags"].as_object();
            let is_mobile = format_tags
                .map(|t| {
                    t.keys().any(|k| {
                        let k_low = k.to_lowercase();
                        k_low.contains("android")
                            || k_low.contains("huawei")
                            || k_low.contains("xiaomi")
                    })
                })
                .unwrap_or(false);

            if is_mobile {
                log_to_front(app, "WARN", "DEBUG: Video 1920x1080 de mÃ³vil detectado sin rotaciÃ³n. Si se ve estirado, podrÃ­a faltar el tag.");
            }
        }

        // Calculamos el Target DAR (Display Aspect Ratio) de forma robusta:
        // 4. Sample Aspect Ratio (SAR) & Display Aspect Ratio (DAR) Check
        // PAR (SAR) is the shape of individual pixels. Correct AR = (W/H) * SAR.
        let sar_str = stream["sample_aspect_ratio"].as_str().unwrap_or("1:1");
        let sar = parse_positive_ratio(sar_str, "SAR")?.unwrap_or(1.0);

        // Get DAR from metadata if available
        let mut target_dar = stream["display_aspect_ratio"]
            .as_str()
            .map(|value| parse_positive_ratio(value, "DAR"))
            .transpose()?
            .flatten();

        log_to_front(
            app,
            "INFO",
            &format!(
                "DEBUG RATIO PRE-ROT: Orig={}x{}, SAR={:.3}, MetaDAR={:?}",
                width, height, sar, target_dar
            ),
        );

        // 5. Apply Rotation Swap
        rotation = rotation.rem_euclid(360);
        if rotation == 90 || rotation == 270 {
            std::mem::swap(&mut width, &mut height);

            // If we have a DAR from metadata, it usually describes the ORIGINAL frame.
            // If the video is now portrait, the effective DAR is 1/DAR.
            if let Some(td) = target_dar {
                // Heuristic: If DAR is already < 1.0, it might already be portrait-adjusted (rare in ffprobe)
                if td > 1.0 {
                    target_dar = Some(1.0 / td);
                }
            }

            log_to_front(
                app,
                "INFO",
                &format!(
                    "DEBUG RATIO POST-ROT: Swapped={}x{}, NewTargetDAR={:?}",
                    width, height, target_dar
                ),
            );
        }

        // 6. Final Effective DAR Decision
        // CRITICAL: Only apply dimension correction if we have EXPLICIT DAR from metadata.
        // Do NOT try to "fix" videos that don't have aspect ratio issues.
        // If target_dar is None (no metadata), we trust the dimensions as-is.
        let should_adjust = target_dar.is_some();
        let final_target_dar = target_dar.unwrap_or_else(|| {
            // No metadata DAR = trust current dimensions
            width as f64 / height as f64
        });

        // 7. FIX ASPECT RATIO (Stretching Correction) - ONLY if we have metadata
        if should_adjust {
            let current_pixel_ar = width as f64 / height as f64;
            if (final_target_dar - current_pixel_ar).abs() > 0.01 {
                log_to_front(
                    app,
                    "WARN",
                    &format!(
                        "DEBUG: AJUSTE FINAL. Pixels={:.3}, MetaDAR={:.3}. Forzando Resize.",
                        current_pixel_ar, final_target_dar
                    ),
                );
                let adjusted_width = height as f64 * final_target_dar;
                if !adjusted_width.is_finite()
                    || adjusted_width < 1.0
                    || adjusted_width > usize::MAX as f64
                {
                    return Err(format!(
                        "DAR inválido: produciría un ancho no representable ({adjusted_width})"
                    ));
                }
                width = adjusted_width.round() as usize;
                if width % 2 != 0 {
                    width = width
                        .checked_add(1)
                        .ok_or_else(|| "Overflow al redondear el ancho por DAR".to_string())?;
                }
            }
        }

        log_to_front(
            app,
            "SUCCESS",
            &format!(
                "DEBUG RATIO FINAL: RenderSize={}x{}, DAR={:.3}",
                width, height, final_target_dar
            ),
        );

        let parse_fps_str = |s: &str| -> Option<f64> {
            let f_parts: Vec<&str> = s.split('/').collect();
            if f_parts.len() == 2 {
                let num = f_parts[0].parse::<f64>().ok()?;
                let den = f_parts[1].parse::<f64>().ok()?;
                if den == 0.0 {
                    None
                } else {
                    Some(num / den)
                }
            } else {
                s.parse::<f64>().ok()
            }
            .filter(|value| value.is_finite() && *value > 0.0 && *value <= 1_000_000.0)
        };

        let fps = parse_fps_str(stream["avg_frame_rate"].as_str().unwrap_or("0"))
            .or_else(|| parse_fps_str(stream["r_frame_rate"].as_str().unwrap_or("0")))
            .unwrap_or(25.0);

        let (sc_frame_count, sc_fps) = parse_sharpcap_metadata(path);
        let fps = sc_fps
            .filter(|value| value.is_finite() && *value > 0.0 && *value <= 1_000_000.0)
            .unwrap_or(fps);

        let duration = stream["duration"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| {
                format["duration"]
                    .as_str()
                    .and_then(|s| s.parse::<f64>().ok())
            })
            .filter(|value| value.is_finite() && *value >= 0.0)
            .unwrap_or(0.0);

        let mut frame_count = stream["nb_frames"]
            .as_str()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let mut frame_count_exact = frame_count > 0;

        // REFUERZO: Si es WMV/AVI o nb_frames es sospechoso, confiar en duraciÃ³n
        let is_asf = format_name.contains("asf") || format_name.contains("wmv");
        if duration > 0.0 {
            let expected_f64 = (duration * fps).round();
            if !expected_f64.is_finite() || expected_f64 > usize::MAX as f64 {
                return Err("FFprobe devolvió duración×FPS fuera de rango".into());
            }
            let expected = expected_f64 as usize;
            if frame_count == 0
                || is_asf
                || (frame_count as f64 - expected as f64).abs() > (expected as f64 * 0.2)
            {
                frame_count = expected;
                frame_count_exact = false;
            }
        }
        if let Some(count) = sc_frame_count {
            frame_count = count;
            frame_count_exact = true;
        }

        // ESTIMACIÓN POR PAQUETES sólo cuando el contenedor no permite ni
        // nb_frames ni duración. Para ASF/WMV con duración ya conservamos
        // duration×fps como estimación no autoritativa: recorrer el archivo
        // entero aquí duplicaba el decode antes del análisis. Un total
        // inventado (antes quedaba en 1 por el
        // max(1)) rompía el % de progreso y podía mandar el frame de referencia
        // del análisis más allá del último frame real → referencia NEGRA y
        // todas las puntuaciones en 0. Leer el índice del contenedor no
        // decodifica nada y para MP4/MOV es casi instantáneo.
        if sc_frame_count.is_none() && frame_count == 0 {
            let mut cmd = Command::new(&ffprobe_path);
            #[cfg(target_os = "windows")]
            cmd.creation_flags(0x08000000);
            cmd.args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-count_packets",
                    "-show_entries",
                    "stream=nb_read_packets",
                    "-of",
                    "csv=p=0",
                    path,
                ]);
            let counted_output = run_command_supervised(
                cmd,
                FFPROBE_PACKET_COUNT_TIMEOUT,
                cancel.as_ref(),
                "FFprobe de conteo de paquetes",
            );
            let counted = match counted_output {
                Ok(output) => Some(output),
                Err(error) if cancel.as_ref().is_some_and(|check| check()) => {
                    return Err(error);
                }
                Err(error) => {
                    log_to_front(app, "WARNING", &error);
                    None
                }
            }
                .filter(|out| out.status.success())
                .and_then(|out| {
                    String::from_utf8_lossy(&out.stdout)
                        .trim()
                        .parse::<usize>()
                        .ok()
                })
                .filter(|&n| n > 0);
            if let Some(n) = counted {
                log_to_front(
                    app,
                    "INFO",
                    &format!(
                        "FFprobe: estimación por paquetes = {} (el contenedor no declaraba nb_frames fiable; el análisis confirmará el EOF decodificado).",
                        n
                    ),
                );
                frame_count = n;
                // Un paquete suele contener un frame, pero el contenedor puede
                // agrupar varios o incluir paquetes sin imagen. Marcarlo como
                // exacto hacía rechazar un decode perfectamente limpio de VFR/
                // B-frames por una desigualdad de metadatos.
                frame_count_exact = false;
            }
        }

        // DETECCION DE COLOR vs MONO/BAYER
        let mut is_color = true;
        let mut color_id = 0; // Default Mono/Bayer

        if pix_fmt.starts_with("gray") {
            is_color = false;
        }

        // Detectar Bayer por tags (SharpCap o AVI FourCC)
        let mut bayer_str = codec_tag.to_uppercase();
        if let Some(tags) = stream["tags"].as_object() {
            if let Some(b) = tags.get("Bayer Pattern").and_then(|v| v.as_str()) {
                bayer_str = b.to_uppercase();
            }
        }
        match bayer_str.as_str() {
            "RGGB" => {
                color_id = 8;
                is_color = false;
            }
            "GRBG" => {
                color_id = 9;
                is_color = false;
            }
            "GBRG" => {
                color_id = 10;
                is_color = false;
            }
            "BGGR" => {
                color_id = 11;
                is_color = false;
            }
            _ => {}
        }

        if is_color {
            color_id = 100; // RGB
        }

        let bytes_per_pixel = if is_color { 6 } else { 2 };
        validate_planetary_frame_geometry(
            width,
            height,
            bytes_per_pixel,
            "Frame decodificado FFmpeg",
        )?;

        Ok(FfmpegReader {
            path: path.to_string(),
            width,
            height,
            frame_count: frame_count.max(1),
            frame_count_exact,
            bytes_per_pixel,
            sample_bits,
            color_id,
            ffmpeg_path: ffmpeg_path.to_string(), // Ensure owned string
            fps,
            is_color,
            rotation,
            codec_name,
            pixel_format,
            color_range,
            color_matrix,
            stream_cache: Arc::new(Mutex::new(None)),
        })
    }

    pub fn get_frame(&self, index: usize, current_color_id: i32) -> Vec<u8> {
        // Un índice es una POSICIÓN de decode, no `index / fps`. En VFR y GOP
        // largos un seek por timestamp puede entregar otro frame aunque FFmpeg
        // termine correctamente. El stream persistente avanza desde cero.
        let stream_bpp = if ffmpeg_stream_is_color(current_color_id) {
            6
        } else {
            2
        };
        let expected = self.width * self.height * stream_bpp;
        let mut buffer = vec![0u8; expected];
        let mut cache_guard = self.stream_cache.lock().unwrap();

        if let Some((ref mut next_idx, ref mut stream)) = *cache_guard {
            if index >= *next_idx {
                let mut ok = true;
                while *next_idx <= index {
                    if !stream.read_frame_into(&mut buffer) {
                        ok = false;
                        break;
                    }
                    *next_idx += 1;
                }
                if ok {
                    return buffer;
                }
            }
            // Salto atrás o EOF: destruir y reiniciar exactamente desde cero.
            *cache_guard = None;
        }

        // Esta ruta puntual no dispone del probe cronometrado/verificado del
        // coordinador. CPU evita aceptar `-hwaccel auto` como si fuera GPU.
        for _ in [()] {
            let Ok(mut stream) = FfmpegStreamIterator::new(
                &self.path,
                self.width,
                self.height,
                0,
                0,
                self.width,
                self.height,
                current_color_id,
                &self.ffmpeg_path,
                None,
                None,
                &self.codec_name,
                self.rotation,
            ) else {
                continue;
            };
            let mut ok = true;
            for _ in 0..=index {
                if !stream.read_frame_into(&mut buffer) {
                    ok = false;
                    break;
                }
            }
            if ok {
                *cache_guard = Some((index + 1, stream));
                return buffer;
            }
        }
        eprintln!("FFmpeg no pudo entregar el frame exacto {index}");
        Vec::new()
    }

    pub fn get_frame_roi(
        &self,
        index: usize,
        roi_x: usize,
        roi_y: usize,
        roi_w: usize,
        roi_h: usize,
        current_color_id: i32,
    ) -> Vec<u8> {
        let stream_bpp = if ffmpeg_stream_is_color(current_color_id) {
            6
        } else {
            2
        };
        let mut frame = vec![0u8; roi_w.saturating_mul(roi_h).saturating_mul(stream_bpp)];
        for _ in [()] {
            let Ok(mut stream) = FfmpegStreamIterator::new(
                &self.path,
                self.width,
                self.height,
                roi_x,
                roi_y,
                roi_w,
                roi_h,
                current_color_id,
                &self.ffmpeg_path,
                None,
                None,
                &self.codec_name,
                self.rotation,
            ) else {
                continue;
            };
            let mut ok = true;
            for _ in 0..=index {
                if !stream.read_frame_into(&mut frame) {
                    ok = false;
                    break;
                }
            }
            if ok {
                return frame;
            }
        }
        eprintln!("FFmpeg no pudo entregar el ROI del frame exacto {index}");
        Vec::new()
    }
}

pub struct FfmpegStreamIterator {
    reader: BufReader<std::process::ChildStdout>,
    process: Arc<Mutex<std::process::Child>>,
    frame_size_bytes: usize,
    expected_hardware_backend: Option<String>,
    stderr_log: Arc<Mutex<Vec<u8>>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
    cancel_check: Option<FfmpegCancelCheck>,
    watchdog_stop: Arc<std::sync::atomic::AtomicBool>,
    last_activity_ms: Arc<std::sync::atomic::AtomicU64>,
    watchdog_started: std::time::Instant,
    terminal_error: Arc<Mutex<Option<String>>>,
    watchdog_thread: Option<std::thread::JoinHandle<()>>,
    /// PR-12: filtergraph en fichero (-filter_script:v) para selecciones
    /// grandes; se borra en Drop.
    filter_script_path: Option<std::path::PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FfmpegPipeOutput {
    /// RGB/BGR/YUV interpretado sale como RGB48; mono/CFA como gray16.
    Native16,
    /// Para análisis de luminancia: en una fuente de color entrega exactamente
    /// el canal G que habría ocupado los bytes centrales de RGB48, sin mover R/B
    /// por el pipe. Mono y CFA permanecen gray16 sin reinterpretación.
    AnalysisGreen16,
}

impl std::fmt::Debug for FfmpegStreamIterator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FfmpegStreamIterator")
            .field("frame_size_bytes", &self.frame_size_bytes)
            .field(
                "expected_hardware_backend",
                &self.expected_hardware_backend,
            )
            .field("has_cancel_check", &self.cancel_check.is_some())
            .field("terminal_error", &self.terminal_error)
            .finish_non_exhaustive()
    }
}

fn elapsed_millis_u64(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn record_ffmpeg_terminal_error(terminal_error: &Arc<Mutex<Option<String>>>, reason: String) {
    let mut error = terminal_error
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if error.is_none() {
        *error = Some(reason);
    }
}

fn spawn_ffmpeg_stream_watchdog(
    process: Arc<Mutex<std::process::Child>>,
    cancel_check: Option<FfmpegCancelCheck>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    last_activity_ms: Arc<std::sync::atomic::AtomicU64>,
    started: std::time::Instant,
    idle_timeout: std::time::Duration,
    terminal_error: Arc<Mutex<Option<String>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let reason = if cancel_check.as_ref().is_some_and(|check| check()) {
            Some("Decode FFmpeg cancelado o sustituido por otro trabajo".to_string())
        } else {
            let now_ms = elapsed_millis_u64(started);
            let last_ms = last_activity_ms.load(Ordering::Acquire);
            let idle_ms = now_ms.saturating_sub(last_ms);
            (idle_ms >= u64::try_from(idle_timeout.as_millis()).unwrap_or(u64::MAX)).then(|| {
                format!(
                    "Decode FFmpeg sin producir bytes durante {:.1} s; el watchdog lo terminó",
                    idle_timeout.as_secs_f32()
                )
            })
        };
        if let Some(reason) = reason {
            record_ffmpeg_terminal_error(&terminal_error, reason);
            let mut child = process
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    })
}

/// FFprobe puede devolver 90, -270 o incluso múltiplos de 360 para la misma
/// orientación. Normalizamos y construimos un filtro explícito para que todas
/// las rutas (stream, batch y ROI) decodifiquen la misma geometría. Los callers
/// añaden `-noautorotate`; así FFmpeg no aplica además su autorrotación oculta.
fn ffmpeg_rotation_filter(rotation: i32) -> Option<&'static str> {
    match rotation.rem_euclid(360) {
        // `side_data_list.rotation` follows FFmpeg's display-matrix
        // convention: positive angles are counter-clockwise.
        90 => Some("transpose=2"),
        180 => Some("transpose=2,transpose=2"),
        270 => Some("transpose=1"),
        _ => None,
    }
}

/// Formato que Zenith solicita al pipe FFmpeg. Mono (0) y todos los mosaicos
/// CFA deben seguir siendo un plano `gray16le`; sólo RGB/BGR/YUV ya
/// interpretados se convierten a `rgb48le`.
fn ffmpeg_stream_is_color(color_id: i32) -> bool {
    crate::ser::ser_color_is_direct_rgb(color_id)
        || crate::ser::ser_color_is_direct_bgr(color_id)
        || crate::ser::ser_color_is_yuv422(color_id)
}

/// Filtro exacto por posición de decode. `n` es el contador de frames de
/// entrada del filtro, por lo que no depende de FPS, timestamps, GOP ni VFR.
/// Se limita la cantidad para mantener acotada la línea de comando y el coste
/// de evaluar la expresión; los lotes de referencia usan normalmente 12–20.
/// Umbral de línea de comandos: CreateProcess limita la línea completa a
/// ~32 KiB en Windows; por encima el MISMO filtergraph va a un fichero
/// temporal con `-filter_script:v` (PR-12) en vez de `-vf` inline.
const MAX_INLINE_FILTER_BYTES: usize = 24 * 1024;
static FILTER_SCRIPT_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn ffmpeg_exact_frame_select_filter(indices: &[usize]) -> Result<String, String> {
    // PR-12: el límite ya NO es la línea de comandos (las expresiones grandes
    // van por filter_script), sino el tamaño del AST del evaluador de FFmpeg.
    // 16384 términos ≈ 200 KiB de script con profundidad O(log N) — muy por
    // debajo de cualquier límite práctico. El límite anterior de 2048 hacía
    // caer selecciones grandes al pipe secuencial que convierte TODOS los
    // frames a RGB48 (mucho peor).
    const MAX_SELECTED_FRAMES: usize = 16_384;
    if indices.is_empty() {
        return Err("La selección FFmpeg exacta está vacía".into());
    }
    if indices.len() > MAX_SELECTED_FRAMES {
        return Err(format!(
            "La selección FFmpeg exacta supera {MAX_SELECTED_FRAMES} frames"
        ));
    }
    if indices.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err("Los índices FFmpeg seleccionados deben ser únicos y ascendentes".into());
    }
    // Una suma plana de cientos de terminos (`a+b+c+...`) forma un AST de
    // profundidad lineal en el evaluador de expresiones de FFmpeg. En builds
    // reales fallaba ya con 260 cuadros con "Cannot allocate memory", aunque
    // la linea de comando cupiera de sobra. Reducir por pares produce el mismo
    // OR numerico exacto (cada eq vale 0 o 1) con profundidad O(log N).
    let mut terms: Vec<String> = indices
        .iter()
        // La coma pertenece a eq(), no separa filtros: FFmpeg exige
        // escaparla incluso cuando Command evita el shell.
        .map(|index| format!("eq(n\\,{index})"))
        .collect();
    while terms.len() > 1 {
        let mut balanced = Vec::with_capacity(terms.len().div_ceil(2));
        let mut iter = terms.into_iter();
        while let Some(left) = iter.next() {
            if let Some(right) = iter.next() {
                balanced.push(format!("({left}+{right})"));
            } else {
                balanced.push(left);
            }
        }
        terms = balanced;
    }
    Ok(format!("select={}", terms.pop().expect("indices no vacios")))
}

fn read_exact_ffmpeg_frame<R: std::io::Read>(reader: &mut R, buffer: &mut [u8]) -> bool {
    match reader.read_exact(buffer) {
        Ok(_) => true,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::UnexpectedEof {
                eprintln!("DEBUG: FfmpegStream Read Failed (Msg): {}", e);
            }
            // Un frame parcial nunca se entrega: el caller conserva el último
            // índice completo y descarta limpiamente una cola FFmpeg truncada.
            false
        }
    }
}

impl FfmpegStreamIterator {
    pub fn new(
        path: &str,
        full_width: usize,
        full_height: usize,
        roi_x: usize,
        roi_y: usize,
        roi_w: usize,
        roi_h: usize,
        color_id: i32,
        ffmpeg_path: &str,
        start_time: Option<String>,
        hardware_backend: Option<&str>,
        _codec_name: &str,
        rotation: i32,
    ) -> Result<Self, String> {
        Self::new_internal(
            path,
            full_width,
            full_height,
            roi_x,
            roi_y,
            roi_w,
            roi_h,
            color_id,
            ffmpeg_path,
            start_time,
            hardware_backend,
            _codec_name,
            rotation,
            None,
            FfmpegPipeOutput::Native16,
            None,
            FFMPEG_STREAM_IDLE_TIMEOUT,
        )
    }

    /// Decodifica por posición absoluta pero sólo materializa los cuadros
    /// solicitados. El codec todavía recorre el GOP/stream desde cero (exacto
    /// para VFR/B-frames), mientras `select` evita convertir, escalar y enviar
    /// por el pipe todos los frames descartados.
    #[allow(clippy::too_many_arguments)]
    pub fn new_selected(
        path: &str,
        full_width: usize,
        full_height: usize,
        roi_x: usize,
        roi_y: usize,
        roi_w: usize,
        roi_h: usize,
        color_id: i32,
        ffmpeg_path: &str,
        hardware_backend: Option<&str>,
        codec_name: &str,
        rotation: i32,
        selected_indices: &[usize],
    ) -> Result<Self, String> {
        Self::new_internal(
            path,
            full_width,
            full_height,
            roi_x,
            roi_y,
            roi_w,
            roi_h,
            color_id,
            ffmpeg_path,
            None,
            hardware_backend,
            codec_name,
            rotation,
            Some(selected_indices),
            FfmpegPipeOutput::Native16,
            None,
            FFMPEG_STREAM_IDLE_TIMEOUT,
        )
    }

    pub fn new_cancelable(
        path: &str,
        full_width: usize,
        full_height: usize,
        roi_x: usize,
        roi_y: usize,
        roi_w: usize,
        roi_h: usize,
        color_id: i32,
        ffmpeg_path: &str,
        start_time: Option<String>,
        hardware_backend: Option<&str>,
        codec_name: &str,
        rotation: i32,
        cancel_check: FfmpegCancelCheck,
    ) -> Result<Self, String> {
        Self::new_internal(
            path,
            full_width,
            full_height,
            roi_x,
            roi_y,
            roi_w,
            roi_h,
            color_id,
            ffmpeg_path,
            start_time,
            hardware_backend,
            codec_name,
            rotation,
            None,
            FfmpegPipeOutput::Native16,
            Some(cancel_check),
            FFMPEG_STREAM_IDLE_TIMEOUT,
        )
    }

    /// Stream exacto y cancelable que decodifica el GOP/archivo en orden, pero
    /// solo convierte, escala y publica los indices solicitados. Es la ruta de
    /// apilado MOV/MP4: conserva exactitud por posicion de decode y reduce el
    /// trafico RGB48 proporcionalmente al porcentaje seleccionado.
    #[allow(clippy::too_many_arguments)]
    pub fn new_cancelable_selected(
        path: &str,
        full_width: usize,
        full_height: usize,
        roi_x: usize,
        roi_y: usize,
        roi_w: usize,
        roi_h: usize,
        color_id: i32,
        ffmpeg_path: &str,
        hardware_backend: Option<&str>,
        codec_name: &str,
        rotation: i32,
        selected_indices: &[usize],
        cancel_check: FfmpegCancelCheck,
    ) -> Result<Self, String> {
        Self::new_internal(
            path,
            full_width,
            full_height,
            roi_x,
            roi_y,
            roi_w,
            roi_h,
            color_id,
            ffmpeg_path,
            None,
            hardware_backend,
            codec_name,
            rotation,
            Some(selected_indices),
            FfmpegPipeOutput::Native16,
            Some(cancel_check),
            FFMPEG_STREAM_IDLE_TIMEOUT,
        )
    }

    /// Variante para el análisis planetario. En vídeo RGB/YUV convierte a
    /// RGB48 dentro de FFmpeg y extrae G antes del pipe: 2 B/px en lugar de
    /// 6 B/px, con el mismo valor de verde que la ruta RGB48. Bayer/mono no se
    /// debayerizan aquí y conservan su plano gray16.
    #[allow(clippy::too_many_arguments)]
    pub fn new_cancelable_analysis_green(
        path: &str,
        full_width: usize,
        full_height: usize,
        roi_x: usize,
        roi_y: usize,
        roi_w: usize,
        roi_h: usize,
        color_id: i32,
        ffmpeg_path: &str,
        start_time: Option<String>,
        hardware_backend: Option<&str>,
        codec_name: &str,
        rotation: i32,
        cancel_check: FfmpegCancelCheck,
    ) -> Result<Self, String> {
        Self::new_internal(
            path,
            full_width,
            full_height,
            roi_x,
            roi_y,
            roi_w,
            roi_h,
            color_id,
            ffmpeg_path,
            start_time,
            hardware_backend,
            codec_name,
            rotation,
            None,
            FfmpegPipeOutput::AnalysisGreen16,
            Some(cancel_check),
            FFMPEG_STREAM_IDLE_TIMEOUT,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_internal(
        path: &str,
        full_width: usize,
        full_height: usize,
        roi_x: usize,
        roi_y: usize,
        roi_w: usize,
        roi_h: usize,
        color_id: i32,
        ffmpeg_path: &str,
        start_time: Option<String>,
        hardware_backend: Option<&str>,
        _codec_name: &str,
        rotation: i32,
        selected_indices: Option<&[usize]>,
        output: FfmpegPipeOutput,
        cancel_check: Option<FfmpegCancelCheck>,
        idle_timeout: std::time::Duration,
    ) -> Result<Self, String> {
        let is_color = ffmpeg_stream_is_color(color_id);
        let analysis_green = output == FfmpegPipeOutput::AnalysisGreen16 && is_color;
        let p_fmt = if is_color && !analysis_green {
            "rgb48le"
        } else {
            "gray16le"
        };
        let bpp = if is_color && !analysis_green { 6 } else { 2 };
        let roi_end_x = roi_x
            .checked_add(roi_w)
            .ok_or_else(|| "ROI FFmpeg desborda el eje X".to_string())?;
        let roi_end_y = roi_y
            .checked_add(roi_h)
            .ok_or_else(|| "ROI FFmpeg desborda el eje Y".to_string())?;
        if roi_end_x > full_width || roi_end_y > full_height {
            return Err(format!(
                "ROI FFmpeg fuera del frame: ({roi_x},{roi_y}) {roi_w}x{roi_h} sobre {full_width}x{full_height}"
            ));
        }
        validate_planetary_frame_geometry(
            full_width,
            full_height,
            bpp,
            "Frame completo del filtro FFmpeg",
        )?;
        let frame_size_bytes = validate_planetary_frame_geometry(
            roi_w,
            roi_h,
            bpp,
            "ROI de salida FFmpeg",
        )?;
        if cancel_check.as_ref().is_some_and(|check| check()) {
            return Err("Decode FFmpeg cancelado antes de iniciar".into());
        }

        // Construct Filter Chain. Rotation must happen before resize/crop: the
        // public FfmpegReader dimensions already describe the displayed
        // orientation (width/height are swapped for 90/270 degrees).
        let mut filters = Vec::new();
        if let Some(indices) = selected_indices {
            filters.push(ffmpeg_exact_frame_select_filter(indices)?);
        }
        if let Some(rotation_filter) = ffmpeg_rotation_filter(rotation) {
            filters.push(rotation_filter.to_string());
        }
        // (Removed unsharp deblocking as it destroys CPU performance during extraction)
        // 1. Scale with NEAREST NEIGHBOR (Fastest & Safest)
        filters.push(format!(
            "scale={}:{}:flags=neighbor",
            full_width, full_height
        ));
        // 2. Crop ROI
        filters.push(format!("crop={}:{}:{}:{}", roi_w, roi_h, roi_x, roi_y));
        // 3. Formato. `format=rgb48le,extractplanes=g` obliga a que el valor G
        // sea el mismo de la ruta RGB48 normal y sólo después elimina R/B.
        if analysis_green {
            filters.push("format=rgb48le".into());
            filters.push("extractplanes=g".into());
            filters.push("format=gray16le".into());
        } else {
            filters.push(format!("format={}", p_fmt));
        }
        let mut args = Vec::new();
        // El demuxer MP4/MOV puede buscar `moov` al final en archivos locales;
        // 100M/100M retrasaba cada nuevo proceso sin mejorar la exactitud.
        args.extend_from_slice(&[
            "-analyzeduration",
            FFMPEG_ANALYZE_DURATION,
            "-probesize",
            FFMPEG_PROBE_SIZE,
        ]);
        args.extend_from_slice(&["-hide_banner", "-nostdin", "-y"]);

        let filter_str = filters.join(",");
        // PR-12: por encima del umbral de línea de comandos el MISMO
        // filtergraph se escribe a un fichero temporal y se pasa con
        // -filter_script:v (bit-exacto por construcción; imprescindible para
        // selecciones exactas de >2048 frames, sobre todo en Windows).
        let filter_script_path = if filter_str.len() > MAX_INLINE_FILTER_BYTES {
            let path = std::env::temp_dir().join(format!(
                "zas-filter-{}-{}.txt",
                std::process::id(),
                FILTER_SCRIPT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, &filter_str)
                .map_err(|e| format!("No se pudo escribir el filter_script FFmpeg: {e}"))?;
            Some(path)
        } else {
            None
        };

        // La ruta GPU ya fue medida por el probe y debe ser explícita. Usar
        // `auto` aquí permitiría que FFmpeg cambiara silenciosamente de
        // backend (o a software) y publicara esos frames bajo una clave GPU.
        let expected_hardware_backend = hardware_backend
            .map(|backend| backend.trim().to_ascii_lowercase())
            .filter(|backend| !backend.is_empty());
        if let Some(backend) = expected_hardware_backend.as_deref() {
            args.extend_from_slice(&["-loglevel", "verbose", "-hwaccel", backend]);
        } else {
            args.extend_from_slice(&["-loglevel", "error"]);
        }
        // El progreso periódico mantiene vivo el supervisor aunque `select`
        // descarte miles de cuadros antes del primer frame solicitado. Viaja
        // por stderr, que ya se drena y acota; no contamina rawvideo/stdout.
        args.extend_from_slice(&["-progress", "pipe:2"]);

        // Seek Input (Fast Seek)
        if let Some(ref ss) = start_time {
            args.extend_from_slice(&["-ss", ss]);
        }

        // La orientación se aplica mediante el filtro anterior. Desactivar la
        // autorrotación implícita evita doble giro y mantiene CPU/GPU idénticos.
        args.push("-noautorotate");

        // PR-13 revisado con trazas 2026-07-17: presupuesto de hilos por RUTA.
        // - Decode HW (VideoToolbox/D3D11VA/QSV): el códec corre en silicio
        //   dedicado; 2 hilos bastan para demux/colas.
        // - Resto: cpus-2, TAMBIÉN en pasadas de apilado. El cap anterior de
        //   cpus/3 ("ceder núcleos al pool durante el solape") resultó
        //   contraproducente medido: con MOV 20MP en frío el suministro caía a
        //   ~5 fps y el pool pasaba 352 s (38% de la pared) ocioso en
        //   decode_wait. El backpressure del pipe + sync_channel ya cede
        //   núcleos solo: cuando el cómputo manda, FFmpeg se bloquea al
        //   escribir y sus hilos duermen.
        let num_cpus = num_cpus::get(); // Use the standard `num_cpus` crate already in use for rayon
        let configured_budget = PLANETARY_FFMPEG_THREAD_BUDGET
            .load(std::sync::atomic::Ordering::Acquire);
        let cpu_route_threads = if configured_budget > 0 {
            configured_budget.min(num_cpus).max(1)
        } else if num_cpus <= 4 {
            num_cpus.saturating_sub(1).max(1) // Keep at least 1 core free for OS on weak PCs
        } else {
            (num_cpus - 2).max(4) // Keep 2 cores free for OS on powerful PCs
        };
        let ffmpeg_threads = if expected_hardware_backend.is_some() {
            2.min(cpu_route_threads).max(1)
        } else {
            cpu_route_threads
        };
        let thread_str = ffmpeg_threads.to_string();
        args.extend_from_slice(&["-threads", &thread_str]);
        // Los filtros (scale neighbor / crop / format) van por defecto en UN
        // solo hilo — en 4K rgb48le la conversión swscale es el coste
        // dominante del productor (117 MB/frame a 20MP). El troceado por
        // slices es determinista: bytes idénticos. OJO: NO heredar el "2" de
        // la ruta HW — el decoder va en silicio pero la conversión es CPU
        // pura y con 2 hilos estrangulaba el suministro igual que en SW.
        let filter_threads_str = cpu_route_threads.to_string();
        args.extend_from_slice(&["-filter_threads", &filter_threads_str]);

        args.extend_from_slice(&["-i", path, "-map", "0:v:0"]);
        args.extend_from_slice(&[
            "-an",
            "-sn",
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            p_fmt,
        ]);
        let filter_script_arg: String;
        if let Some(script) = &filter_script_path {
            filter_script_arg = script.to_string_lossy().into_owned();
            args.extend_from_slice(&["-filter_script:v", &filter_script_arg]);
        } else {
            args.extend_from_slice(&["-vf", &filter_str]);
        }
        args.push("pipe:1");

        eprintln!("DEBUG: FfmpegStreamIterator Args: {:?}", args);
        eprintln!("DEBUG: Frame Size Bytes: {}", frame_size_bytes);
        eprintln!("DEBUG: Using ffmpeg path: {}", ffmpeg_path);

        // Attempt to find ffmpeg
        // let ffmpeg_cmd = if Path::new("./ffmpeg.exe").exists() {
        //     "./ffmpeg.exe"
        // } else {
        //     "ffmpeg"
        // };
        let ffmpeg_cmd = ffmpeg_path;

        let mut cmd = Command::new(ffmpeg_cmd);
        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x08000000); // NO_WINDOW

        let mut child = cmd
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn ffmpeg stream: {}", e))?;

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = kill_and_reap_child(&mut child);
                return Err("Failed to open ffmpeg stdout".into());
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                drop(stdout);
                let _ = kill_and_reap_child(&mut child);
                return Err("Failed to open ffmpeg stderr".into());
            }
        };

        // Drenar stderr evita deadlock. Conservamos sólo una ventana acotada
        // para verificar el backend real; nunca acumulamos el log completo de
        // una captura larga.
        const MAX_FFMPEG_DIAGNOSTIC_BYTES: usize = 2 * 1024 * 1024;
        let stderr_log = Arc::new(Mutex::new(Vec::with_capacity(64 * 1024)));
        let stderr_log_writer = stderr_log.clone();
        let last_activity_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let watchdog_started = std::time::Instant::now();
        let progress_activity = last_activity_ms.clone();
        let stderr_thread = std::thread::spawn(move || {
            use std::io::Read;
            let mut reader = std::io::BufReader::new(stderr);
            let mut chunk = [0u8; 8192];
            loop {
                let Ok(read) = reader.read(&mut chunk) else { break };
                if read == 0 {
                    break;
                }
                progress_activity.store(
                    elapsed_millis_u64(watchdog_started),
                    Ordering::Release,
                );
                let mut log = stderr_log_writer
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let remaining = MAX_FFMPEG_DIAGNOSTIC_BYTES.saturating_sub(log.len());
                log.extend_from_slice(&chunk[..read.min(remaining)]);
            }
        });

        // Un BufReader del tamaño de un frame duplicaba cientos de MiB en 8K.
        // 4 MiB mantienen el pipe caliente y el frame vive sólo en el ring.
        let reader_capacity = frame_size_bytes
            .min(4 * 1024 * 1024)
            .checked_add(64 * 1024)
            .ok_or_else(|| {
                let _ = kill_and_reap_child(&mut child);
                "Overflow al dimensionar el buffer FFmpeg".to_string()
            })?;
        let process = Arc::new(Mutex::new(child));
        let watchdog_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let terminal_error = Arc::new(Mutex::new(None));
        let effective_idle_timeout =
            ffmpeg_effective_idle_timeout(selected_indices, frame_size_bytes, idle_timeout);
        let watchdog_thread = spawn_ffmpeg_stream_watchdog(
            process.clone(),
            cancel_check.clone(),
            watchdog_stop.clone(),
            last_activity_ms.clone(),
            watchdog_started,
            effective_idle_timeout,
            terminal_error.clone(),
        );
        Ok(Self {
            reader: BufReader::with_capacity(reader_capacity, stdout),
            process,
            frame_size_bytes,
            expected_hardware_backend,
            stderr_log,
            stderr_thread: Some(stderr_thread),
            cancel_check,
            watchdog_stop,
            last_activity_ms,
            watchdog_started,
            terminal_error,
            watchdog_thread: Some(watchdog_thread),
            filter_script_path,
        })
    }
}

impl FfmpegStreamIterator {
    fn terminal_reason(&self) -> Option<String> {
        self.terminal_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn stop_watchdog(&mut self) {
        self.watchdog_stop.store(true, Ordering::Release);
        if let Some(thread) = self.watchdog_thread.take() {
            let _ = thread.join();
        }
    }

    fn terminate_and_reap(&mut self, reason: String) -> Result<(), String> {
        record_ffmpeg_terminal_error(&self.terminal_error, reason);
        self.watchdog_stop.store(true, Ordering::Release);
        let result = {
            let mut child = self
                .process
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            kill_and_reap_child(&mut child)
        };
        self.stop_watchdog();
        result
    }

    fn mark_activity(&self) {
        self.last_activity_ms.store(
            elapsed_millis_u64(self.watchdog_started),
            Ordering::Release,
        );
    }

    // OPT FFMPEG 1: ZERO-ALLOCATION FRAME READING
    // We completely disabled `impl Iterator` because it FORCES the return of a new `Vec<u8>` every frame,
    // which triggers an extreme DDOS of Windows RAM (~25MB per frame alloc/dealloc = 25GB per 1000 frames).
    // Now we write strictly into a pre-allocated fixed memory block without ever allocating new RAM.
    pub fn read_frame_into(&mut self, buffer: &mut [u8]) -> bool {
        if buffer.len() != self.frame_size_bytes {
            let _ = self.terminate_and_reap(format!(
                "Buffer FFmpeg incorrecto: {} bytes; se esperaban {}",
                buffer.len(),
                self.frame_size_bytes
            ));
            return false;
        }
        if self.cancel_check.as_ref().is_some_and(|check| check()) {
            let _ = self.terminate_and_reap(
                "Decode FFmpeg cancelado o sustituido antes de leer el frame".into(),
            );
            return false;
        }

        let mut offset = 0usize;
        while offset < buffer.len() {
            match self.reader.read(&mut buffer[offset..]) {
                Ok(0) if offset == 0 => return false,
                Ok(0) => {
                    let _ = self.terminate_and_reap(format!(
                        "FFmpeg entregó un frame truncado: {offset}/{} bytes",
                        buffer.len()
                    ));
                    return false;
                }
                Ok(read) => {
                    offset += read;
                    self.mark_activity();
                    if self.cancel_check.as_ref().is_some_and(|check| check()) {
                        let _ = self.terminate_and_reap(
                            "Decode FFmpeg cancelado o sustituido durante la lectura".into(),
                        );
                        return false;
                    }
                }
                Err(error) => {
                    let reason = self.terminal_reason().unwrap_or_else(|| {
                        format!("Fallo al leer el pipe FFmpeg: {error}")
                    });
                    let _ = self.terminate_and_reap(reason);
                    return false;
                }
            }
        }
        true
    }

    /// Called after stdout reaches an exact frame boundary. A non-zero FFmpeg
    /// exit distinguishes corrupt/truncated input from a legitimate clean EOF;
    /// callers must not accept the decoded prefix in that case.
    pub fn finish_status(&mut self) -> Result<(), String> {
        let started = std::time::Instant::now();
        let status = loop {
            if let Some(reason) = self.terminal_reason() {
                let reap = self.terminate_and_reap(reason.clone());
                if let Some(thread) = self.stderr_thread.take() {
                    let _ = thread.join();
                }
                return match reap {
                    Ok(()) => Err(reason),
                    Err(error) => Err(format!("{reason}; además {error}")),
                };
            }
            if self.cancel_check.as_ref().is_some_and(|check| check()) {
                let reason = "FFmpeg cancelado o sustituido mientras terminaba".to_string();
                let reap = self.terminate_and_reap(reason.clone());
                if let Some(thread) = self.stderr_thread.take() {
                    let _ = thread.join();
                }
                return match reap {
                    Ok(()) => Err(reason),
                    Err(error) => Err(format!("{reason}; además {error}")),
                };
            }
            if started.elapsed() >= FFMPEG_EXIT_TIMEOUT {
                let reason = format!(
                    "FFmpeg no terminó {:.1} s después del EOF; fue terminado",
                    FFMPEG_EXIT_TIMEOUT.as_secs_f32()
                );
                let reap = self.terminate_and_reap(reason.clone());
                if let Some(thread) = self.stderr_thread.take() {
                    let _ = thread.join();
                }
                return match reap {
                    Ok(()) => Err(reason),
                    Err(error) => Err(format!("{reason}; además {error}")),
                };
            }
            let polled = {
                let mut child = self
                    .process
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                child.try_wait()
            };
            match polled {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(error) => {
                    let reason = format!("no se pudo esperar a FFmpeg: {error}");
                    let reap = self.terminate_and_reap(reason.clone());
                    if let Some(thread) = self.stderr_thread.take() {
                        let _ = thread.join();
                    }
                    return match reap {
                        Ok(()) => Err(reason),
                        Err(reap_error) => Err(format!("{reason}; además {reap_error}")),
                    };
                }
            }
        };
        self.stop_watchdog();
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
        if !status.success() {
            return Err(format!("FFmpeg terminó con estado {status}"));
        }
        self.validate_hardware_route()
    }

    /// Verifica que la ruta que está a punto de alimentar el caché coincide
    /// con el backend elegido por el probe. Puede llamarse antes de EOF: la
    /// inicialización del decoder se registra antes de entregar el primer
    /// frame. Si FFmpeg hizo fallback, el intento se descarta y CPU reinicia.
    pub fn validate_hardware_route(&self) -> Result<(), String> {
        let Some(expected) = self.expected_hardware_backend.as_deref() else {
            return Ok(());
        };
        let mut observed = None;
        // El drainer corre en paralelo; tras el primer frame el mensaje de
        // inicialización ya existe en el pipe, pero puede faltarle un quantum
        // para copiarlo al buffer compartido.
        for _ in 0..10 {
            if self.cancel_check.as_ref().is_some_and(|check| check()) {
                return Err("FFmpeg cancelado durante la validación del backend".into());
            }
            {
                let log = self
                    .stderr_log
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                observed = confirmed_ffmpeg_hardware_backend(
                    &String::from_utf8_lossy(&log),
                    true,
                );
            }
            if observed.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if observed.as_deref() != Some(expected) {
            return Err(format!(
                "FFmpeg no confirmó el backend {expected}; observado: {}",
                observed.as_deref().unwrap_or("software/indeterminado")
            ));
        }
        Ok(())
    }

    // OPT FFMPEG 3: FAST-FORWARD DRAIN (Sequential Skipping)
    // Instantly dumps N frames straight from the pipe into the void without decoding or housing them.
    pub fn skip_frames(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let Some(mut remaining) = count.checked_mul(self.frame_size_bytes) else {
            let _ = self.terminate_and_reap("Overflow al saltar frames FFmpeg".into());
            return;
        };
        let mut scratch = [0u8; 64 * 1024];
        while remaining > 0 {
            if self.cancel_check.as_ref().is_some_and(|check| check()) {
                let _ = self.terminate_and_reap(
                    "Decode FFmpeg cancelado o sustituido al saltar frames".into(),
                );
                return;
            }
            let requested = remaining.min(scratch.len());
            match self.reader.read(&mut scratch[..requested]) {
                Ok(0) => return,
                Ok(read) => {
                    remaining -= read;
                    self.mark_activity();
                }
                Err(error) => {
                    let reason = self.terminal_reason().unwrap_or_else(|| {
                        format!("Fallo al drenar frames FFmpeg: {error}")
                    });
                    let _ = self.terminate_and_reap(reason);
                    return;
                }
            }
        }
    }
}

impl Drop for FfmpegStreamIterator {
    // Al reemplazar/descartar un stream (salto atras en get_frame, reintento
    // GPU->CPU, fin de pasada) el hijo ffmpeg solo moria cuando su siguiente
    // write chocaba con el pipe cerrado, y NUNCA se cosechaba (wait): en
    // macOS quedaban entradas zombie acumulandose durante toda la sesion.
    // kill lo termina de inmediato y wait libera la entrada del proceso.
    // Si el proceso ya salio (EOF normal), kill falla y se ignora.
    fn drop(&mut self) {
        self.watchdog_stop.store(true, Ordering::Release);
        {
            let mut child = self
                .process
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = kill_and_reap_child(&mut child);
        }
        self.stop_watchdog();
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
        if let Some(script) = self.filter_script_path.take() {
            let _ = std::fs::remove_file(script);
        }
    }
}

#[derive(Clone, Debug)]
enum VideoInput {
    Ser(SerReader),
    Avi(AviReader),
    Ffmpeg(FfmpegReader),
    Fits(FitsSequenceReader),
}

/// Lleva samples SER de 9–15 bits LSB-aligned al dominio de trabajo ADU16 en
/// la frontera de entrada. Así scorer, CoG, detección lunar, APs y sigma-clip
/// ven la misma escala independientemente de la cámara; hacerlo sólo al final
/// producía decisiones distintas para la misma escena de 10 y 16 bits.
fn normalize_ser_samples_to_adu16<'a>(
    frame: Cow<'a, [u8]>,
    sample_bits: usize,
) -> Cow<'a, [u8]> {
    if !(9..16).contains(&sample_bits) || frame.is_empty() {
        return frame;
    }
    let max_native = (1u32 << sample_bits) - 1;
    let mut normalized = frame.into_owned();
    for sample in normalized.chunks_exact_mut(2) {
        let native = u16::from_le_bytes([sample[0], sample[1]]) as u32;
        let scaled = (native.min(max_native) * 65_535 + max_native / 2) / max_native;
        sample.copy_from_slice(&(scaled as u16).to_le_bytes());
    }
    Cow::Owned(normalized)
}

impl VideoInput {
    fn open(path: &str, app: &tauri::AppHandle) -> Result<Self, String> {
        Self::open_with_cancel(path, app, None)
    }

    fn open_cancelable(
        path: &str,
        app: &tauri::AppHandle,
        cancel: FfmpegCancelCheck,
    ) -> Result<Self, String> {
        Self::open_with_cancel(path, app, Some(cancel))
    }

    fn open_with_cancel(
        path: &str,
        app: &tauri::AppHandle,
        cancel: Option<FfmpegCancelCheck>,
    ) -> Result<Self, String> {
        println!("DEBUG: [VideoInput::open] Opening: {}", path);
        if cancel.as_ref().is_some_and(|check| check()) {
            return Err("Apertura de video cancelada o sustituida por otro trabajo".into());
        }
        let p = Path::new(path);
        if !p.exists() {
            return Err(format!("El archivo no existe: {}", path));
        }
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();

        // 1. Check if FITS Sequence (Folder or single FITS file marking the sequence)
        if p.is_dir() || ext == "fits" || ext == "fit" {
            if let Ok(r) = FitsSequenceReader::new(path) {
                println!(
                    "DEBUG: [VideoInput] Using FITS Sequence Reader for: {}",
                    path
                );
                return Ok(VideoInput::Fits(r));
            }
        }

        // Priorizar lector nativo SER para formatos no comprimidos.
        // FFmpeg no decodifica de forma confiable SER RGB/YUV directo; el lector nativo si.
        if ext == "ser" {
            let cid = peek_ser_color_id(path);

            if ser::ser_color_is_cmyg(cid) {
                return Err(format!(
                    "SER CMYG {} ({}) no soportado: este patrón requiere conversión CMYG explícita y no se degradará silenciosamente a gris",
                    cid,
                    ser::ser_pattern_name(cid)
                ));
            }

            if ser_color_is_native_decodable(cid) {
                match SerReader::new(path) {
                    Ok(r) => {
                        if ser::ser_color_is_yuv422(cid) && r.info.sample_bits > 8 {
                            return Err(format!(
                                "SER YUV422 de {} bits no soportado por el conversor nativo; conviértelo explícitamente a RGB48/mono16 para evitar interpretar canales incorrectamente",
                                r.info.sample_bits
                            ));
                        }
                        println!(
                            "DEBUG: [VideoInput] Using Native SER Reader for: {} (CID={})",
                            path, cid
                        );
                        return Ok(VideoInput::Ser(r));
                    }
                    Err(err) => {
                        log_to_front(
                            app,
                            "WARNING",
                            &format!(
                                "SER CID={} compatible, pero el lector nativo fallo: {}. Probando FFmpeg...",
                                cid, err
                            ),
                        );
                    }
                }
            } else {
                log_to_front(
                    app,
                    "INFO",
                    &format!(
                        "SER comprimido/no soportado nativamente (CID={}). Delegando decodificacion a FFmpeg...",
                        cid
                    ),
                );
            }
        }

        // Un AVI RAW8 suele transportar el mosaico CFA como Y800/MONO8 o
        // como DIB de 8 bits con paleta gris. FFmpeg puede exponer ese último
        // caso como `pal8` y convertirlo a RGB, lo que produce ColorID 100 y
        // hace imposible aplicar después el patrón Bayer manual. El lector
        // nativo es deliberadamente estricto: sólo acepta layouts raw
        // verificables y rechaza codecs comprimidos, ambiguos y OpenDML para
        // que éstos continúen por FFmpeg sin cambiar su flujo.
        if ext == "avi" {
            if cancel.as_ref().is_some_and(|check| check()) {
                return Err("Apertura AVI cancelada o sustituida por otro trabajo".into());
            }
            match AviReader::new(path) {
                Ok(r) => {
                    println!(
                        "DEBUG: [VideoInput] Using Native AVI Reader for: {} (CID={})",
                        path, r.info.color_id
                    );
                    return Ok(VideoInput::Avi(r));
                }
                Err(err) => {
                    log_to_front(
                        app,
                        "INFO",
                        &format!(
                            "AVI no es raw clásico verificable ({}). Delegando decodificación a FFmpeg...",
                            err
                        ),
                    );
                }
            }
        }

        // Intentar FFmpeg para el resto (o si SER/AVI nativo falla)
        if cancel.as_ref().is_some_and(|check| check()) {
            return Err("Apertura FFmpeg cancelada o sustituida por otro trabajo".into());
        }
        match FfmpegReader::new_with_cancel(path, app, cancel) {
            Ok(r) => {
                println!("DEBUG: [VideoInput] Using FFmpeg Reader for: {}", path);
                return Ok(VideoInput::Ffmpeg(r));
            }
            Err(e) => {
                let hint = ffmpeg_install_hint();
                if e.contains("Instala FFmpeg") || e.contains(&hint) {
                    return Err(format!("Error al abrir con FFmpeg: {}", e));
                }
                return Err(format!("Error al abrir con FFmpeg: {}\n\n{}", e, hint));
            }
        }
    }
    fn width(&self) -> usize {
        match self {
            VideoInput::Ser(r) => r.info.width,
            VideoInput::Avi(r) => r.info.width,
            VideoInput::Ffmpeg(r) => r.width,
            VideoInput::Fits(r) => r.width,
        }
    }
    fn height(&self) -> usize {
        match self {
            VideoInput::Ser(r) => r.info.height,
            VideoInput::Avi(r) => r.info.height,
            VideoInput::Ffmpeg(r) => r.height,
            VideoInput::Fits(r) => r.height,
        }
    }
    fn frame_count(&self) -> usize {
        match self {
            VideoInput::Ser(r) => r.info.frame_count,
            VideoInput::Avi(r) => r.info.frame_count,
            VideoInput::Ffmpeg(r) => r.frame_count,
            VideoInput::Fits(r) => r.frame_count,
        }
    }
    /// SER/AVI/FITS enumeran frames físicos. En contenedores FFmpeg el total
    /// sólo es autoritativo cuando proviene de nb_frames/metadata explícita;
    /// duración×fps y conteo de paquetes son estimaciones.
    fn frame_count_is_exact(&self) -> bool {
        match self {
            VideoInput::Ffmpeg(r) => r.frame_count_exact,
            VideoInput::Ser(_) | VideoInput::Avi(_) | VideoInput::Fits(_) => true,
        }
    }
    fn bpp(&self) -> usize {
        match self {
            VideoInput::Ser(r) => r.info.bytes_per_pixel,
            VideoInput::Avi(r) => r.info.bytes_per_pixel,
            VideoInput::Ffmpeg(r) => r.bytes_per_pixel,
            VideoInput::Fits(r) => r.bytes_per_pixel,
        }
    }
    fn sample_bits(&self) -> usize {
        match self {
            VideoInput::Ser(r) => r.info.sample_bits,
            VideoInput::Avi(r) => r.info.sample_bits,
            VideoInput::Ffmpeg(r) => r.sample_bits,
            VideoInput::Fits(r) => r.sample_bits,
        }
    }
    /// La frontera VideoInput ya normaliza SER 9–15 bit y FFmpeg/FITS entregan
    /// u16 canónico. Los orígenes de 8 bit se expanden en raw_to_u16_buffer.
    fn native_sample_to_u16_gain(&self) -> f32 {
        match self {
            VideoInput::Ser(_) => 1.0,
            VideoInput::Avi(r) => {
                crate::planetary_quality::native_sample_to_u16_gain(r.info.sample_bits)
            }
            VideoInput::Ffmpeg(_) | VideoInput::Fits(_) => 1.0,
        }
    }
    fn color_id(&self) -> i32 {
        match self {
            VideoInput::Ser(r) => r.info.color_id,
            VideoInput::Avi(r) => r.info.color_id,
            VideoInput::Ffmpeg(r) => r.color_id,
            VideoInput::Fits(r) => r.color_id,
        }
    }
    /// Resuelve la reinterpretación manual de CFA sin permitir que una opción
    /// de UI convierta silenciosamente RGB/YUV ya decodificado en un mosaico.
    ///
    /// SER, AVI raw y FITS de un solo plano sí pueden venir con BAYERPAT/CID
    /// ausente o erróneo. En FFmpeg sólo aceptamos cambiar un CFA que el propio
    /// probe ya identificó como CFA: un MP4/MOV RGB no conserva el mosaico de
    /// sensor y volver a etiquetarlo como Bayer destruye crominancia y detalle.
    fn resolve_bayer_override(&self, requested: Option<i32>) -> Result<i32, String> {
        let source = self.color_id();
        let Some(requested) = requested else {
            return Ok(source);
        };
        if !matches!(requested, 0 | 8..=11) {
            return Err(format!(
                "Override Bayer inválido ({requested}); sólo se aceptan MONO=0 y RGGB/GRBG/GBRG/BGGR=8..11"
            ));
        }
        if requested == source {
            return Ok(source);
        }

        let source_is_single_plane = source == 0 || ser::ser_color_is_bayer(source);
        let can_reinterpret_raw_plane = match self {
            VideoInput::Ser(_) | VideoInput::Avi(_) => source_is_single_plane,
            VideoInput::Fits(r) => !r.is_color && source_is_single_plane,
            // FFmpeg puede conservar un CFA raw/lossless reconocido, pero un
            // stream mono genérico también puede ser vídeo ya procesado.
            VideoInput::Ffmpeg(_) => ser::ser_color_is_bayer(source),
        };
        if !can_reinterpret_raw_plane {
            return Err(format!(
                "No se puede aplicar un patrón Bayer manual a esta fuente (ColorID {source}). El flujo ya es RGB/YUV o no conserva un CFA verificable; usa Auto/Original para evitar color falso."
            ));
        }
        Ok(requested)
    }

    /// Color efectivo después de una reinterpretación CFA validada. No usa el
    /// `is_color` original cuando el usuario convirtió explícitamente Bayer a
    /// MONO, ni mantiene MONO cuando corrigió un CFA raw mal etiquetado.
    fn is_color_for(&self, resolved_color_id: i32) -> bool {
        if resolved_color_id != self.color_id() {
            ser::ser_color_is_color(resolved_color_id)
        } else {
            self.is_color() || ser::ser_color_is_color(resolved_color_id)
        }
    }
    fn rotation(&self) -> i32 {
        match self {
            VideoInput::Ffmpeg(r) => r.rotation,
            _ => 0, // SER, AVI, FITS don't currently expose rotation natively in our readers
        }
    }
    fn get_frame<'a>(&'a self, idx: usize, cid: i32) -> Cow<'a, [u8]> {
        match self {
            VideoInput::Ser(r) => {
                normalize_ser_samples_to_adu16(r.get_frame(idx, cid), r.info.sample_bits)
            }
            VideoInput::Avi(r) => r.get_frame(idx, cid),
            VideoInput::Fits(r) => Cow::Owned(r.get_frame(idx)),
            VideoInput::Ffmpeg(r) => {
                // Warn on console if using Ffmpeg for high-speed analysis
                // println!("Using FfmpegReader (Slow Process Spawn) for frame {}", idx);
                Cow::Owned(r.get_frame(idx, cid))
            }
        }
    }

    // Optimized ROI getter
    // For Ffmpeg: Uses crop filter (saves IO/Mem)
    // For SER/AVI: Fallback to full frame + crop (in memory but safe due to Mmap)
    // Ideally update SerReader to be smarter too.
    fn get_frame_roi(
        &self,
        idx: usize,
        roi_x: usize,
        roi_y: usize,
        roi_w: usize,
        roi_h: usize,
        cid: i32,
    ) -> Cow<'_, [u8]> {
        match self {
            VideoInput::Ffmpeg(r) => {
                Cow::Owned(r.get_frame_roi(idx, roi_x, roi_y, roi_w, roi_h, cid))
            }
            VideoInput::Ser(r) => {
                if idx >= r.info.frame_count {
                    return Cow::Owned(Vec::new());
                }
                if roi_x == 0 && roi_y == 0 && roi_w == r.info.width && roi_h == r.info.height {
                    return normalize_ser_samples_to_adu16(
                        r.get_frame(idx, cid),
                        r.info.sample_bits,
                    );
                }
                let frame_size = r.info.width * r.info.height * r.info.bytes_per_pixel;
                let start = r.header_shift + (idx * frame_size);
                if start + frame_size > r.mmap.len() {
                    return Cow::Owned(Vec::new());
                }
                let full = &r.mmap[start..start + frame_size];

                let bpp = r.info.bytes_per_pixel;
                let w = r.info.width;
                let stride = w * bpp;
                let roi_stride = roi_w * bpp;
                let mut out = Vec::with_capacity(roi_w * roi_h * bpp);

                // Copy row by row directly from memory map slice
                let start_offset = roi_y * stride + roi_x * bpp;
                for y in 0..roi_h {
                    let row_start = start_offset + y * stride;
                    if row_start + roi_stride <= full.len() {
                        out.extend_from_slice(&full[row_start..row_start + roi_stride]);
                    } else {
                        out.extend(std::iter::repeat(0).take(roi_stride));
                    }
                }

                // Swap bytes ONLY on the extracted ROI data
                if !r.info.is_little_endian && bpp >= 2 {
                    for chunk in out.chunks_exact_mut(2) {
                        chunk.swap(0, 1);
                    }
                }
                normalize_ser_samples_to_adu16(Cow::Owned(out), r.info.sample_bits)
            }
            VideoInput::Avi(r) => {
                if roi_x == 0 && roi_y == 0 && roi_w == r.info.width && roi_h == r.info.height {
                    return r.get_frame(idx, cid);
                }
                // Similar fallback for AVI
                let full = r.get_frame(idx, cid);
                if full.is_empty() {
                    return Cow::Owned(Vec::new());
                }
                let bpp = r.info.bytes_per_pixel;
                let w = r.info.width;
                let stride = w * bpp;
                let roi_stride = roi_w * bpp;
                let mut out = Vec::with_capacity(roi_w * roi_h * bpp);

                let start_offset = roi_y * stride + roi_x * bpp;
                for y in 0..roi_h {
                    let row_start = start_offset + y * stride;
                    if row_start + roi_stride <= full.len() {
                        out.extend_from_slice(&full[row_start..row_start + roi_stride]);
                    } else {
                        out.extend(std::iter::repeat(0).take(roi_stride));
                    }
                }
                Cow::Owned(out)
            }
            VideoInput::Fits(r) => {
                if roi_x == 0 && roi_y == 0 && roi_w == r.width && roi_h == r.height {
                    return Cow::Owned(r.get_frame(idx));
                }
                let full = r.get_frame(idx);
                if full.is_empty() {
                    return Cow::Owned(Vec::new());
                }
                let bpp = r.bytes_per_pixel;
                let w = r.width;
                let stride = w * bpp;
                let roi_stride = roi_w * bpp;
                let mut out = Vec::with_capacity(roi_w * roi_h * bpp);

                let start_offset = roi_y * stride + roi_x * bpp;
                for y in 0..roi_h {
                    let row_start = start_offset + y * stride;
                    if row_start + roi_stride <= full.len() {
                        out.extend_from_slice(&full[row_start..row_start + roi_stride]);
                    } else {
                        out.extend(std::iter::repeat(0).take(roi_stride));
                    }
                }
                Cow::Owned(out)
            }
        }
    }
    fn is_ffmpeg(&self) -> bool {
        matches!(self, VideoInput::Ffmpeg(_))
    }
    /// Etiqueta corta del formato para la telemetria (visible en la UI).
    fn reader_kind_label(&self) -> &'static str {
        match self {
            VideoInput::Ser(_) => "SER",
            VideoInput::Avi(_) => "AVI",
            VideoInput::Fits(_) => "FITS",
            VideoInput::Ffmpeg(_) => "FFmpeg",
        }
    }
    fn codec_name(&self) -> &str {
        match self {
            VideoInput::Ffmpeg(r) => &r.codec_name,
            _ => "rawvideo",
        }
    }
    fn is_color(&self) -> bool {
        match self {
            VideoInput::Ffmpeg(r) => r.is_color,
            VideoInput::Ser(r) => ser::ser_color_is_color(r.info.color_id),
            VideoInput::Avi(r) => ser::ser_color_is_color(r.info.color_id),
            VideoInput::Fits(r) => r.is_color,
        }
    }
}

#[derive(Clone, PartialEq)]
struct DeconvParams {
    sigma: f32,
    iter: usize,
    vc_sigma: f32,
    vc_iter: usize,
    // A: PSF medida del limbo activa → invalida el cache de deconv al togglear.
    psf_from_limb: bool,
}

#[derive(Clone)]
struct DeconvCache {
    channels: Arc<Vec<Vec<f32>>>, // [0]=Y or [0,1,2]=RGB; hits clone sólo el Arc
    params: DeconvParams,
    width: usize,
    height: usize,
}

#[derive(Clone)]
struct WaveletLayers {
    channels: Arc<Vec<Vec<Vec<f32>>>>, // [Channel][Layer][Pixel]
    width: usize,
    height: usize,
    parent_deconv_params: DeconvParams,
    // B: descomposicion edge-aware activa → invalida el cache de wavelets.
    edge_aware: bool,
    // B+: intensidad edge-aware (nº bandas bilaterales + estrechez del rango) →
    // tambien cambia la descomposicion, invalida el cache cuando edge_aware=on.
    edge_aware_strength: f32,
}

/// Brightness-aware unsharp-mask contract. The base USM amount remains the
/// master strength; these values modulate that strength from dark to bright
/// regions using the unprocessed luminance as the reference. This mirrors the
/// useful ImPPG workflow without changing the neutral recipe when disabled.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct AdaptiveUsmParams {
    enabled: bool,
    /// Multiplier applied below the threshold, expressed as 0..2.
    amount_min: f32,
    /// Multiplier applied above the transition, expressed as 0..2.
    amount_max: f32,
    /// Normalised input luminance at the centre of the transition.
    threshold: f32,
    /// Width of the smooth transition in normalised luminance.
    transition: f32,
}

impl Default for AdaptiveUsmParams {
    fn default() -> Self {
        Self {
            enabled: false,
            amount_min: 0.15,
            amount_max: 1.0,
            threshold: 0.12,
            transition: 0.18,
        }
    }
}

#[derive(Clone, PartialEq)]
struct FilterParams {
    u_amts: [f32; 5],
    w_amts: [f32; 6],
    d_amts: [f32; 6],
    crisp: f32,
    master_denoise: f32, // New Intelligent Master Denoise
    master_denoise_detail: f32,
    master_denoise_chroma: f32,
    usm_amount: f32,
    usm_radius: f32,
    lce_amount: f32,
    deringing_mode: i32,
    deringing_radius: f32,
    deringing_dark: f32,
    deringing_light: f32,
    deringing_mask: bool,
    deconv_params: DeconvParams,
    // La descomposicion wavelet (edge-aware + intensidad) y la auto-mascara
    // adaptativa cambian la recombinacion → deben formar parte de la clave del
    // filter_cache o un toggle solo (sin mover otro slider) devolveria un
    // resultado obsoleto de la cache.
    edge_aware: bool,
    edge_aware_strength: f32,
    auto_mask: f32,
    adaptive_usm: AdaptiveUsmParams,
}

#[derive(Clone)]
struct FilterCache {
    channels: Arc<Vec<Vec<f32>>>, // [0]=Y or [0,1,2]=RGB
    params: FilterParams,
    width: usize,
    height: usize,
}

/// Producto científico inactivo respaldado por disco. Mantener cuatro
/// `DeepSkyResult` completos (SCI + VAR + NEFF + DQ + diagnósticos) en RAM
/// multiplica el pico de forma explosiva, especialmente con una rama Drizzle
/// 2× y otra EIDR. El producto activo sigue íntegro en memoria; los demás se
/// cargan bajo demanda sin recalcular.
#[derive(Debug)]
struct DeepSkyProductCacheEntry {
    path: PathBuf,
    stored_bytes: u64,
    /// Los volcados de intercambio entre productos son efímeros y se borran
    /// al salir del mapa en memoria. Un checkpoint ya publicado, en cambio,
    /// debe sobrevivir a cancelación, OOM recuperable y reinicio de la app para
    /// que el siguiente intento compatible no repita la integración.
    remove_on_drop: bool,
}

impl DeepSkyProductCacheEntry {
    fn transient(path: PathBuf, stored_bytes: u64) -> Self {
        Self {
            path,
            stored_bytes,
            remove_on_drop: true,
        }
    }

    fn durable(path: PathBuf, stored_bytes: u64) -> Self {
        Self {
            path,
            stored_bytes,
            remove_on_drop: false,
        }
    }
}

impl Drop for DeepSkyProductCacheEntry {
    fn drop(&mut self) {
        if !self.remove_on_drop {
            return;
        }
        let _ = std::fs::remove_file(&self.path);
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

struct AppState {
    stacked_image: Mutex<Option<StackResult>>,
    /// Máster lineal float32 y mapas científicos de cielo profundo. Se mantiene
    /// separado del resultado planetario u16 para no perder headroom.
    deep_sky_result: Mutex<Option<DeepSkyLinearResult>>,
    /// Productos v5 no activos. El producto visible vive en
    /// `deep_sky_result`/`stacked_image`; los demás se conservan en un caché
    /// temporal lossless para que Classic Drizzle, NF, STRUCT y EIDR no
    /// permanezcan simultáneamente en RAM.
    deep_sky_products:
        Mutex<std::collections::BTreeMap<String, DeepSkyProductCacheEntry>>,
    deep_sky_active_product: Mutex<Option<String>>,
    /// Last rendered 16-bit result for histogram, sampling and export parity.
    /// It is always derived again from `stacked_image`, never used as the base
    /// of the next edit, so adjustments cannot accumulate destructively.
    processed_image: Mutex<Option<StackResult>>,
    deconv_cache: Mutex<Vec<DeconvCache>>,
    wavelet_cache: Mutex<Vec<WaveletLayers>>,
    filter_cache: Mutex<Vec<FilterCache>>,
    /// Estadisticas del MASTER COMPLETO (p99, pivote tonal, PSF del limbo) con la
    /// `result_generation` que las valida. El recuadro interactivo procesa un
    /// recorte y no puede medirlas de el; ademas evita recalcular p99 y la PSF en
    /// cada arrastre de slider.
    global_stats_cache: Mutex<Option<(usize, GlobalStats)>>,
    batch_anchor: Mutex<Option<Vec<u16>>>,
    batch_anchor_dims: Mutex<(usize, usize)>,
    /// Serializa cambios de generación con la publicación/consumo del
    /// resultado compartido. El trabajo pesado sigue concurrente; sólo la
    /// transición atómica de propietario queda protegida.
    planetary_generation_gate: Mutex<()>,
    active_req_id: AtomicUsize,
    /// Monotonic identity for the result currently open in post-processing.
    /// Late async requests from a previous stack are rejected.
    result_generation: AtomicUsize,
    // Cancelacion cooperativa de analisis/apilado. Arc para poder clonarlo a
    // los hilos productores (decoder FFmpeg, prefetcher) que sobreviven al
    // scope del comando. Se resetea al INICIAR una operacion de usuario, y lo
    // consultan los bucles pesados para abortar limpio con Err("Cancelado").
    cancel_requested: Arc<std::sync::atomic::AtomicBool>,
    // F8: cancelación POR TRABAJO (NF-Full/EIDR). El cancel global de arriba
    // sigue funcionando; el registro permite cortar un stack concreto sin
    // tumbar los demás y limpia su flag al terminar (JobGuard).
    job_registry: pipeline::JobRegistry,
    license_manager: Arc<LicenseManager>,
}

#[derive(Clone)]
pub struct StackResult {
    pub data: Vec<u16>,
    pub width: usize,
    pub height: usize,
    pub is_mono: bool,
    pub is_surface: bool, // Support for V2 surface handling
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SolarMonoParams {
    /// Activates the solar-only derivative. The stacked master remains mono.
    enabled: bool,
    /// Inverts the luminance after the editable tone curve.
    invert: bool,
    /// Maps the processed mono luminance to the three-stop false-colour gradient.
    colorize: bool,
    /// Normalised input/output control points. Endpoints are restored if absent.
    curve_points: Vec<[f32; 2]>,
    shadow_color: [f32; 3],
    midtone_color: [f32; 3],
    highlight_color: [f32; 3],
    color_strength: f32,
    /// Preserves luminance in the bright solar disk instead of protecting
    /// only the false-colour saturation.
    highlight_protect: f32,
    /// Rolls the brightest solar values into protected 16-bit headroom instead
    /// of allowing a curve or palette to flatten them at pure white.
    highlight_compression: f32,
    /// Keeps the estimated sky/background near its measured black floor.
    background_protect: f32,
    /// Recovers coherent low-signal structures immediately outside the disk.
    prominence_amount: f32,
    /// Edge-confident local contrast dedicated to fine solar filaments.
    filament_amount: f32,
    filament_radius: f32,
    noise_guard: f32,
}

impl Default for SolarMonoParams {
    fn default() -> Self {
        Self {
            enabled: false,
            invert: false,
            colorize: true,
            curve_points: vec![[0.0, 0.0], [1.0, 1.0]],
            shadow_color: [0.06, 0.0, 0.0],
            midtone_color: [0.72, 0.2, 0.0],
            highlight_color: [1.0, 0.94, 0.35],
            color_strength: 0.9,
            highlight_protect: 0.65,
            highlight_compression: 0.62,
            background_protect: 0.72,
            prominence_amount: 0.0,
            filament_amount: 0.0,
            filament_radius: 1.15,
            noise_guard: 0.65,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct AdvancedColorParams {
    /// Input/output levels in normalised linear 16-bit space.
    levels_black: f32,
    levels_mid: f32,
    levels_white: f32,
    /// Optional free luminance curve shared by mono and colour output.
    /// A two-point diagonal is an exact identity.
    tone_curve_points: Vec<[f32; 2]>,
    /// Exposure is expressed in stops; the remaining tone controls use -1..1.
    exposure: f32,
    shadows: f32,
    highlights: f32,
    whites: f32,
    blacks: f32,
    vibrance: f32,
    temperature: f32,
    tint: f32,
    /// Edge-aware local contrast controls. Texture targets finer structure;
    /// clarity targets wider structure. Both use -1..1.
    texture: f32,
    clarity: f32,
    /// Selective chromatic noise reduction for excess green, 0..1.
    scnr_green: f32,
    /// Red, orange, yellow, green, aqua, blue, purple and magenta.
    hsl_hue: [f32; 8],
    hsl_saturation: [f32; 8],
    hsl_luminance: [f32; 8],
    grading_shadows: [f32; 3],
    grading_midtones: [f32; 3],
    grading_highlights: [f32; 3],
    grading_amounts: [f32; 3],
    /// Exclusive derivative for monochrome solar data.
    solar: SolarMonoParams,
}

impl Default for AdvancedColorParams {
    fn default() -> Self {
        Self {
            levels_black: 0.0,
            levels_mid: 1.0,
            levels_white: 1.0,
            tone_curve_points: vec![[0.0, 0.0], [1.0, 1.0]],
            exposure: 0.0,
            shadows: 0.0,
            highlights: 0.0,
            whites: 0.0,
            blacks: 0.0,
            vibrance: 0.0,
            temperature: 0.0,
            tint: 0.0,
            texture: 0.0,
            clarity: 0.0,
            scnr_green: 0.0,
            hsl_hue: [0.0; 8],
            hsl_saturation: [0.0; 8],
            hsl_luminance: [0.0; 8],
            grading_shadows: [1.0, 1.0, 1.0],
            grading_midtones: [1.0, 1.0, 1.0],
            grading_highlights: [1.0, 1.0, 1.0],
            grading_amounts: [0.0; 3],
            solar: SolarMonoParams::default(),
        }
    }
}

const ANALYSIS_CACHE_SCHEMA_VERSION: u32 = 11;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct AnalysisCacheContract {
    schema_version: u32,
    resolved_color_id: i32,
    target_type: String,
    is_surface: bool,
    warping_analysis: bool,
    anchor_override: Option<Vec<i32>>,
    requested_roi: Rect,
    resolved_roi: Rect,
    width: usize,
    height: usize,
    declared_frame_count: usize,
    frame_count_exact: bool,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct CachedAnalysis {
    scores: Vec<(usize, u64)>,
    roi: Rect,
    path_hash: u64,
    // V2 Fields (Optional for backward compatibility)
    frame_stats: Option<Vec<FrameAlignmentData>>,
    quality_graph: Option<Vec<f32>>,
    width: Option<usize>,
    height: Option<usize>,
    best_frame_idx: Option<usize>,
    ap_points: Option<Vec<smart_grid::ApPoint>>, // NEW
    contract: Option<AnalysisCacheContract>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct FrameAlignmentData {
    frame_idx: usize,
    global_shift: (f32, f32),
    local_shifts: Vec<((f32, f32), (f32, f32))>,
    // V2 Fields
    idx: usize, // Alias for frame_idx in V2 logic
    x_shift: f32,
    y_shift: f32,
    score: u64,
    added: bool,
    grid_scores: Option<Vec<u64>>, // NEW: 10x10 grid for regional quality (MAP IDW)
}

impl FrameAlignmentData {
    fn empty(idx: usize) -> Self {
        Self {
            frame_idx: idx,
            global_shift: (0.0, 0.0),
            local_shifts: Vec::new(),
            idx,
            x_shift: 0.0,
            y_shift: 0.0,
            score: 0,
            added: false,
            grid_scores: None,
        }
    }
}

#[derive(Clone, serde::Serialize)]
struct VideoStats {
    min_pixel: u16,
    max_pixel: u16,
    avg_brightness: f32,
    dynamic_range_pct: f32,
    // Quality Statistics
    best_score: f64,
    worst_score: f64,
    avg_quality: f64,
    quality_stability: f64, // (avg / best) * 100
    std_dev: f64,
    entropy: f64,
}
#[derive(Clone, serde::Serialize)]
struct VideoMetadata {
    width: usize,
    height: usize,
    frame_count: usize,
    bpp: usize,
    color_id: i32,
    pattern_name: String,
    file_size_mb: f64,
    is_color: bool,
}
#[derive(Clone, serde::Serialize)]
struct AnalysisResult {
    metadata: VideoMetadata,
    stats: VideoStats,
    quality_graph: Vec<(usize, f64)>,
    preview_base64: String,
    path: String,
    recommended_pct: f32,
    ap_points: Vec<(f32, f32, f32)>,
    best_frame_idx: usize,
    #[serde(rename = "executionPlan", skip_serializing_if = "Option::is_none")]
    execution_plan: Option<crate::pipeline::PlanetaryExecutionPlan>,
    #[serde(
        rename = "stageTelemetry",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    stage_telemetry: Vec<crate::pipeline::StageTelemetry>,
}
#[derive(Clone, serde::Serialize)]
struct LogMessage {
    level: String,
    msg: String,
}
/// Telemetria EN VIVO del apilado (evento "stack_telemetry", cada ~25
/// frames): modo de acumulacion (GPU/CPU), rendimiento y recursos — la
/// validacion visible que pide el usuario de que la GPU esta trabajando.
#[derive(Clone, serde::Serialize)]
struct StackTelemetry {
    /// "analysis" | "stacking" — la UI ajusta las etiquetas de la rejilla.
    phase: String,
    mode: String,
    /// Decodificación de video por hardware. `None` para SER/AVI/FITS nativo.
    decode_gpu: Option<bool>,
    /// Kernel wgpu efectivo para preprocesado/acumulación, no mera detección.
    compute_gpu: bool,
    frames_done: usize,
    frames_total: usize,
    fps: f32,
    align_ms: f32,
    accum_ms: f32,
    upload_mbps: f32,
    ram_mb: u64,
    vram_mb: u64,
    cache_hits: usize,
    threads: usize,
}

#[derive(Clone, serde::Serialize)]
struct Progress {
    step: String,
    pct: f32,
    details: Option<String>,
}

#[derive(serde::Serialize)]
struct PsfResult {
    sigma: f32,
    iterations: usize,
    confidence: f32,
    noise_sigma: f32,
    msg: String,
}
#[derive(serde::Serialize)]
struct PreviewResult {
    width: usize,
    height: usize,
    preview_base64: String,
    filename: String,
    frame_count: usize,
    is_color: bool,
    suggested_target: String,
}

#[derive(serde::Serialize)]
struct BatchEntryResult {
    path: String,
    master_path: String,
    preview_base64: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Copy)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

#[derive(serde::Deserialize)]
struct MosaicTileConfig {
    #[allow(dead_code)]
    path: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    #[allow(dead_code)]
    rotation: f32,
}

#[cfg(test)]
mod frame_stream_tests {
    fn sleeping_command() -> std::process::Command {
        #[cfg(unix)]
        {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "sleep 5"]);
            command
        }
        #[cfg(windows)]
        {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "ping 127.0.0.1 -n 6 >NUL"]);
            command
        }
    }

    fn fits_input(is_color: bool, color_id: i32, bytes_per_pixel: usize) -> super::VideoInput {
        super::VideoInput::Fits(crate::fits_sequence::FitsSequenceReader {
            folder_path: String::new(),
            files: Vec::new(),
            width: 8,
            height: 8,
            bytes_per_pixel,
            sample_bits: 16,
            frame_count: 1,
            color_id,
            is_color,
            fps: 1.0,
        })
    }

    #[test]
    fn bayer_override_only_reinterprets_verified_single_plane_sources() {
        let mono = fits_input(false, 0, 2);
        assert_eq!(mono.resolve_bayer_override(Some(8)).unwrap(), 8);
        assert!(mono.is_color_for(8));

        let rgb = fits_input(true, 100, 6);
        let err = rgb.resolve_bayer_override(Some(8)).unwrap_err();
        assert!(err.contains("RGB/YUV") || err.contains("CFA verificable"));
        assert_eq!(rgb.resolve_bayer_override(None).unwrap(), 100);
        assert!(rgb.is_color_for(100));

        assert!(mono.resolve_bayer_override(Some(12)).is_err());
    }

    #[test]
    fn native_ser_samples_enter_the_pipeline_as_adu16() {
        for bits in [9usize, 10, 12, 14, 15] {
            let max_native = (1u16 << bits) - 1;
            let native = [0u16, 1, max_native / 2, max_native];
            let mut bytes = Vec::new();
            for value in native {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            let normalized = super::normalize_ser_samples_to_adu16(
                std::borrow::Cow::Owned(bytes),
                bits,
            );
            let values: Vec<u16> = normalized
                .chunks_exact(2)
                .map(|sample| u16::from_le_bytes([sample[0], sample[1]]))
                .collect();
            assert_eq!(values[0], 0);
            assert!(values[1] > 0);
            assert!(values[1] < values[2]);
            assert!(values[2] < values[3]);
            assert_eq!(values[3], u16::MAX, "falló expansión de {bits} bits");
        }

        let original = vec![0x34, 0x12];
        assert_eq!(
            super::normalize_ser_samples_to_adu16(
                std::borrow::Cow::Owned(original.clone()),
                16,
            )
            .as_ref(),
            original
        );
    }

    #[test]
    fn ffmpeg_rotation_is_normalized_and_explicit() {
        assert_eq!(super::ffmpeg_rotation_filter(0), None);
        assert_eq!(super::ffmpeg_rotation_filter(360), None);
        assert_eq!(super::ffmpeg_rotation_filter(90), Some("transpose=2"));
        assert_eq!(super::ffmpeg_rotation_filter(-270), Some("transpose=2"));
        assert_eq!(
            super::ffmpeg_rotation_filter(180),
            Some("transpose=2,transpose=2")
        );
        assert_eq!(
            super::ffmpeg_rotation_filter(-180),
            Some("transpose=2,transpose=2")
        );
        assert_eq!(super::ffmpeg_rotation_filter(270), Some("transpose=1"));
        assert_eq!(super::ffmpeg_rotation_filter(-90), Some("transpose=1"));
    }

    #[test]
    fn ffmpeg_pipe_keeps_mono_and_cfa_single_channel() {
        for color_id in [0, 8, 9, 10, 11, 16, 17, 18, 19] {
            assert!(
                !super::ffmpeg_stream_is_color(color_id),
                "color_id={color_id}"
            );
        }
        for color_id in [12, 14, 20, 100, 101, 102, 103] {
            assert!(
                super::ffmpeg_stream_is_color(color_id),
                "color_id={color_id}"
            );
        }
    }

    #[test]
    fn ffmpeg_select_filter_is_exact_bounded_and_shell_independent() {
        assert_eq!(
            super::ffmpeg_exact_frame_select_filter(&[0, 17, 39]).unwrap(),
            r"select=((eq(n\,0)+eq(n\,17))+eq(n\,39))"
        );
        assert!(super::ffmpeg_exact_frame_select_filter(&[]).is_err());
        assert!(super::ffmpeg_exact_frame_select_filter(&[7, 7]).is_err());
        assert!(super::ffmpeg_exact_frame_select_filter(&[9, 3]).is_err());
        assert!(super::ffmpeg_exact_frame_select_filter(&(0..1028).collect::<Vec<_>>()).is_ok());
        // PR-12: >2048 índices ya es válido (la expresión larga viaja por
        // -filter_script:v, no por la línea de comandos).
        let big = super::ffmpeg_exact_frame_select_filter(&(0..3000).collect::<Vec<_>>())
            .expect("3000 índices deben ser válidos");
        assert!(
            big.len() > super::MAX_INLINE_FILTER_BYTES,
            "3000 términos deben superar el umbral inline y forzar filter_script"
        );
        assert!(
            super::ffmpeg_exact_frame_select_filter(&(0..16_385).collect::<Vec<_>>()).is_err(),
            "por encima del tope del AST se rechaza (fallback secuencial)"
        );
        let balanced =
            super::ffmpeg_exact_frame_select_filter(&(0..520).step_by(2).collect::<Vec<_>>())
                .unwrap();
        assert!(balanced.starts_with("select=("));
        assert!(balanced.contains("eq(n\\,518)"));
        assert!(
            super::ffmpeg_exact_frame_select_filter(&(0..2049).collect::<Vec<_>>()).is_ok(),
            "2049 índices dejaron de ser un acantilado (PR-12)"
        );
    }

    #[test]
    fn ffmpeg_selected_timeout_grows_only_for_sparse_select_gaps() {
        use std::time::Duration;

        // 1080p G16: 2 B/px (el ritmo base histórico de 4 fps se conserva).
        const FHD: usize = 2_073_600 * 2;
        // 20 MP G16 (3312×5888): el ritmo conservador baja a 0.5 fps.
        const HUGE: usize = 19_501_056 * 2;

        assert_eq!(
            super::ffmpeg_effective_idle_timeout(None, FHD, Duration::from_secs(7)),
            Duration::from_secs(7),
            "un stream normal nunca hereda el máximo adaptativo de select"
        );
        assert_eq!(
            super::ffmpeg_selected_idle_timeout(&[0, 1, 2], FHD),
            Duration::from_secs(20)
        );
        assert_eq!(
            super::ffmpeg_selected_idle_timeout(&[100], FHD),
            Duration::from_secs(31)
        );
        assert_eq!(
            super::ffmpeg_selected_idle_timeout(&[0, 400], FHD),
            Duration::from_secs(105)
        );
        // Techo nuevo 600 s (antes 120 s: mataba HEVC 20 MP sanos y lentos).
        assert_eq!(
            super::ffmpeg_selected_idle_timeout(&[10_000], FHD),
            Duration::from_secs(600)
        );
        // 20 MP: mismo hueco de 400 frames → margen mucho mayor (0.5 fps),
        // acotado por el techo.
        assert_eq!(
            super::ffmpeg_selected_idle_timeout(&[0, 400], HUGE),
            Duration::from_secs(600)
        );
        // Hueco moderado a 20 MP: 100 frames / 0.5 fps + 5 s = 205 s.
        assert_eq!(
            super::ffmpeg_selected_idle_timeout(&[0, 100], HUGE),
            Duration::from_secs(205)
        );
    }

    #[test]
    fn ffmpeg_truncated_pipe_never_emits_a_partial_frame() {
        let frame_size = 64usize;
        let mut bytes = vec![7u8; frame_size + frame_size / 2];
        for (i, v) in bytes.iter_mut().enumerate() {
            *v = (i & 0xff) as u8;
        }
        let mut reader = std::io::Cursor::new(bytes);
        let mut frame = vec![0u8; frame_size];
        assert!(super::read_exact_ffmpeg_frame(&mut reader, &mut frame));
        assert_eq!(frame[17], 17);
        assert!(!super::read_exact_ffmpeg_frame(&mut reader, &mut frame));
        assert_eq!(reader.position(), (frame_size + frame_size / 2) as u64);
    }

    #[test]
    fn ffprobe_supervisor_enforces_deadline_and_reaps_fake_process() {
        let started = std::time::Instant::now();
        let error = super::run_command_supervised(
            sleeping_command(),
            std::time::Duration::from_millis(120),
            None,
            "FFprobe falso",
        )
        .unwrap_err();
        assert!(error.contains("límite estricto"), "{error}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "el supervisor no respetó el deadline"
        );
    }

    #[test]
    fn ffprobe_supervisor_observes_generation_cancellation() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let check: super::FfmpegCancelCheck = {
            let cancelled = cancelled.clone();
            std::sync::Arc::new(move || {
                cancelled.load(std::sync::atomic::Ordering::Acquire)
            })
        };
        let error = super::run_command_supervised(
            sleeping_command(),
            std::time::Duration::from_secs(5),
            Some(&check),
            "FFprobe falso",
        )
        .unwrap_err();
        assert!(error.contains("cancelado"), "{error}");
    }

    #[test]
    fn ffmpeg_watchdog_kills_idle_child_and_caller_reaps_it() {
        let child = sleeping_command().spawn().unwrap();
        let process = std::sync::Arc::new(std::sync::Mutex::new(child));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let activity = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let terminal = std::sync::Arc::new(std::sync::Mutex::new(None));
        let started = std::time::Instant::now();
        let watchdog = super::spawn_ffmpeg_stream_watchdog(
            process.clone(),
            None,
            stop,
            activity,
            started,
            std::time::Duration::from_millis(120),
            terminal.clone(),
        );
        watchdog.join().unwrap();
        {
            let mut child = process.lock().unwrap();
            super::kill_and_reap_child(&mut child).unwrap();
        }
        let reason = terminal.lock().unwrap().clone().unwrap();
        assert!(reason.contains("sin producir bytes"), "{reason}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn ffmpeg_geometry_rejects_overflow_and_ram_exhaustion() {
        let budget_error = super::validate_planetary_frame_geometry_with_budget(
            4096,
            4096,
            6,
            8 * 1024 * 1024,
            "prueba",
        )
        .unwrap_err();
        assert!(budget_error.contains("presupuesto adaptativo"), "{budget_error}");

        let overflow_error = super::validate_planetary_frame_geometry_with_budget(
            super::MAX_PLANETARY_FRAME_DIMENSION,
            super::MAX_PLANETARY_FRAME_DIMENSION,
            usize::MAX,
            usize::MAX,
            "prueba",
        )
        .unwrap_err();
        assert!(overflow_error.contains("overflow"), "{overflow_error}");

        assert!(super::parse_positive_ratio("16:9", "DAR").is_ok());
        assert!(super::parse_positive_ratio("NaN:1", "DAR").is_err());
        assert!(super::parse_positive_ratio("1:0.0", "DAR").is_err());
    }
}

#[cfg(test)]
mod f3_cache_prune_tests {
    use super::prune_stale_analysis_caches;
    use std::fs;

    /// F3: al guardar el caché _a7 de un vídeo, los hermanos _a6 del MISMO
    /// vídeo se borran, pero se conservan los _a7 de otros modos y los cachés
    /// de OTROS vídeos.
    #[test]
    fn prune_removes_only_stale_version_siblings() {
        let dir = std::env::temp_dir().join(format!("zas_prune_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let base = dir.join("jupiter.ser");
        let mk = |suffix: &str| {
            let p = format!("{}_{}.analysis_v2", base.to_string_lossy(), suffix);
            fs::write(&p, b"x").unwrap();
            p
        };
        let current = mk("planet_zenith_ultimate_warp_a7");
        let stale_a6 = mk("planet_zenith_ultimate_warp_a6");
        let other_mode_a7 = mk("surface_zenith_ultimate_global_a7");
        // Otro vídeo distinto no debe tocarse.
        let other_video = format!(
            "{}_planet_zenith_ultimate_warp_a6.analysis_v2",
            dir.join("saturn.ser").to_string_lossy()
        );
        fs::write(&other_video, b"x").unwrap();

        prune_stale_analysis_caches(&current);

        assert!(fs::metadata(&current).is_ok(), "el caché vigente permanece");
        assert!(
            fs::metadata(&other_mode_a7).is_ok(),
            "otro modo con la versión vigente permanece"
        );
        assert!(fs::metadata(&other_video).is_ok(), "otro vídeo no se toca");
        assert!(
            fs::metadata(&stale_a6).is_err(),
            "la versión vieja del mismo vídeo se borra"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod f3_fits_output_tests {
    fn write_mono16_fits(path: &std::path::Path, mono: &[u16], width: usize, height: usize) {
        let mut header = String::new();
        let mut card = |kw: &str, val: &str| {
            let line = if val.is_empty() {
                format!("{:<80}", kw)
            } else {
                format!("{:<8}= {:>20}{:<50}", kw, val, "")
            };
            header.push_str(&line[..80]);
        };
        card("SIMPLE", "T");
        card("BITPIX", "16");
        card("NAXIS", "2");
        card("NAXIS1", &width.to_string());
        card("NAXIS2", &height.to_string());
        card("BZERO", "32768");
        card("BSCALE", "1");
        card("END", "");
        while header.len() % 2880 != 0 {
            header.push(' ');
        }
        let mut bytes = header.into_bytes();
        for &value in mono {
            bytes.extend_from_slice(&((value as i32 - 32768) as i16).to_be_bytes());
        }
        while bytes.len() % 2880 != 0 {
            bytes.push(0);
        }
        std::fs::write(path, bytes).unwrap();
    }

    /// F3: el FITS 16-bit de salida debe releerse con las MISMAS dimensiones
    /// y valores por el propio lector de secuencias FITS (que aplica
    /// BZERO/BSCALE), cerrando el round-trip write→read.
    #[test]
    fn rgb16_fits_roundtrips_through_reader() {
        let (w, h) = (5usize, 4usize);
        let rgb: Vec<u16> = (0..w * h * 3)
            .map(|i| ((i * 4099 + 7) % 65536) as u16)
            .collect();
        let dir = std::env::temp_dir().join(format!("zas_fits_out_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stack.fits");
        crate::write_rgb16_fits(path.to_str().unwrap(), &rgb, w, h).unwrap();

        let reader = crate::fits_sequence::FitsSequenceReader::new(path.to_str().unwrap())
            .expect("FITS de salida legible por el lector de secuencias");
        assert_eq!(reader.width, w);
        assert_eq!(reader.height, h);
        assert!(reader.is_color);
        assert_eq!(
            reader.color_id, 100,
            "FITS RGB debe usar layout RGB directo"
        );
        assert_eq!(reader.bytes_per_pixel, 6);
        let frame = reader.get_frame(0);
        assert_eq!(frame.len(), w * h * 6);
        let decoded = crate::raw_to_u16_buffer(&frame, w, h, reader.bytes_per_pixel);
        assert_eq!(
            decoded, rgb,
            "todos los canales RGB deben sobrevivir planar FITS → interleaved pipeline"
        );

        // El demosaico/canonicalizador debe reconocer el layout ya-RGB y no
        // interpretarlo como mosaico Bayer (el antiguo ColorID=8 lo hacía).
        let canonical = crate::debayer_to_rgb(&decoded, w, h, reader.color_id);
        assert_eq!(canonical, rgb);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mono16_fits_uses_mono_layout_not_yuv_id() {
        let (w, h) = (7usize, 3usize);
        let mono: Vec<u16> = (0..w * h)
            .map(|i| ((i * 8191 + 3) & 0xffff) as u16)
            .collect();
        let dir = std::env::temp_dir().join(format!("zas_fits_mono_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mono.fits");
        write_mono16_fits(&path, &mono, w, h);

        let reader = crate::fits_sequence::FitsSequenceReader::new(path.to_str().unwrap())
            .expect("FITS mono16 legible");
        assert!(!reader.is_color);
        assert_eq!(
            reader.color_id, 0,
            "mono16 no debe sobrecargar ColorID YUV=12"
        );
        assert_eq!(reader.bytes_per_pixel, 2);
        let frame = reader.get_frame(0);
        assert_eq!(crate::raw_to_u16_buffer(&frame, w, h, 2), mono);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
