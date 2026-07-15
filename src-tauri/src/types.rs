// ==========================================
// 1. ESTRUCTURAS DE DATOS
// ==========================================

#[derive(Clone, Debug)]
pub struct FfmpegReader {
    pub path: String,
    pub width: usize,
    pub height: usize,
    pub frame_count: usize,
    pub bytes_per_pixel: usize,
    pub sample_bits: usize,
    pub color_id: i32,
    pub ffmpeg_path: String,
    pub fps: f64,
    pub is_color: bool,
    pub rotation: i32,
    pub codec_name: String,
    // THE NEW PERSISTENT STREAM CACHE
    pub stream_cache: Arc<Mutex<Option<(usize, FfmpegStreamIterator)>>>,
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

/// Fingerprint estable del origen planetario. Incluye versión algorítmica,
/// ruta canónica, tamaño/mtime y muestras de contenido; para una secuencia FITS
/// en carpeta incluye cada entrada. Evita reutilizar desplazamientos cuando el
/// usuario sobrescribe un SER/MP4 conservando nombre y geometría.
fn planetary_source_fingerprint(path: &str) -> Result<u64, String> {
    use std::hash::{Hash, Hasher};
    use std::io::{Read, Seek, SeekFrom};

    const ALGORITHM_VERSION: &str = "planetary-analysis-hybrid-v2-a6";
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
fn parse_cached_analysis(raw: &[u8]) -> Option<CachedAnalysis> {
    // SNIFF antes de intentar LZ4: decompress_size_prepended lee los primeros
    // 4 bytes como TAMANO a asignar — en un JSON legado ('{"sc...') eso es una
    // peticion de ~1.67 GB que se asigna y descarta en cada lectura (hipo en
    // macOS; en un Windows justo de RAM puede abortar el proceso). Un cache
    // JSON siempre empieza con '{': ir directo a serde_json en ese caso.
    if raw.first() == Some(&b'{') {
        return serde_json::from_slice::<CachedAnalysis>(raw).ok();
    }
    if let Ok(bin) = lz4_flex::decompress_size_prepended(raw) {
        if let Ok(c) = bincode::deserialize::<CachedAnalysis>(&bin) {
            return Some(c);
        }
    }
    serde_json::from_slice::<CachedAnalysis>(raw).ok()
}

fn load_cached_analysis(cache_path: &str) -> Option<CachedAnalysis> {
    parse_cached_analysis(&fs::read(cache_path).ok()?)
}

fn save_cached_analysis(cache_path: &str, cached: &CachedAnalysis) {
    if let Ok(bin) = bincode::serialize(cached) {
        // Escritura silenciosa a proposito: el cache vive junto al video y en
        // medios de solo lectura (SD, red) el write falla — no es un error.
        let _ = fs::write(cache_path, lz4_flex::compress_prepend_size(&bin));
    }
    prune_stale_analysis_caches(cache_path);
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
    let Some(cache_name) = current.file_name().map(|s| s.to_string_lossy().into_owned()) else {
        return;
    };
    let Some(cur_ver) = version_tag(&cache_name) else { return };
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
            // Check ColorID at bytes 18-21 using both LE and BE.
            // A valid ColorID must be between 0 and 255 (e.g., 0 for MONO, 8-11 for Bayer, 100 for RGB).
            let cid_le = i32::from_le_bytes([buf[18], buf[19], buf[20], buf[21]]);
            let cid_be = i32::from_be_bytes([buf[18], buf[19], buf[20], buf[21]]);
            if cid_le >= 0 && cid_le <= 255 {
                return cid_le;
            } else if cid_be >= 0 && cid_be <= 255 {
                return cid_be;
            }
            return cid_le;
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

impl FfmpegReader {
    pub fn new(path: &str, app: &tauri::AppHandle) -> Result<Self, String> {
        let ffprobe_path = get_ffprobe_command(app);
        let ffmpeg_path = get_ffmpeg_command(app);

        // Verificamos si los binarios existen para dar errores claros
        if ffprobe_path == "ffprobe"
            && !std::process::Command::new("ffprobe")
                .arg("-version")
                .status()
                .is_ok()
        {
            return Err(format!(
                "Error: No se encontro FFprobe. {}",
                ffmpeg_install_hint()
            ));
        }

        // Usar ffprobe para obtener metadatos precisos.
        // Aumentamos analyzeduration y probesize para archivos grandes (ej: AVI de 20GB)
        let output = {
            let mut cmd = Command::new(&ffprobe_path);
            #[cfg(target_os = "windows")]
            cmd.creation_flags(0x08000000);
            cmd.args(&[
                "-v",
                "error",
                "-analyzeduration",
                "2147483647",
                "-probesize",
                "2147483647",
                "-show_format",
                "-show_streams",
                "-of",
                "json",
                path,
            ])
            .output()
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

        let mut width = stream["width"].as_u64().unwrap_or(0) as usize;
        let mut height = stream["height"].as_u64().unwrap_or(0) as usize;
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
            clean.parse::<f64>().ok().map(|f| f.round() as i32)
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
        let sar = if sar_str == "0:1" || sar_str == "1:0" || sar_str == "0:0" {
            1.0
        } else {
            let parts: Vec<&str> = sar_str.split(':').collect();
            if parts.len() == 2 {
                let num = parts[0].parse::<f64>().unwrap_or(1.0);
                let den = parts[1].parse::<f64>().unwrap_or(1.0);
                if den != 0.0 {
                    num / den
                } else {
                    1.0
                }
            } else {
                1.0
            }
        };

        // Get DAR from metadata if available
        let mut target_dar: Option<f64> = if let Some(dar) = stream["display_aspect_ratio"].as_str()
        {
            let parts: Vec<&str> = dar.split(':').collect();
            if parts.len() == 2 {
                let num = parts[0].parse::<f64>().unwrap_or(1.0);
                let den = parts[1].parse::<f64>().unwrap_or(1.0);
                if den != 0.0 {
                    Some(num / den)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        log_to_front(
            app,
            "INFO",
            &format!(
                "DEBUG RATIO PRE-ROT: Orig={}x{}, SAR={:.3}, MetaDAR={:?}",
                width, height, sar, target_dar
            ),
        );

        // 5. Apply Rotation Swap
        let rot_abs = rotation.abs();
        if rot_abs == 90 || rot_abs == 270 {
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
                width = (height as f64 * final_target_dar).round() as usize;
                if width % 2 != 0 {
                    width += 1;
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
        };

        let fps = parse_fps_str(stream["avg_frame_rate"].as_str().unwrap_or("0"))
            .or_else(|| parse_fps_str(stream["r_frame_rate"].as_str().unwrap_or("0")))
            .unwrap_or(25.0);

        let (sc_frame_count, sc_fps) = parse_sharpcap_metadata(path);
        let fps = sc_fps.unwrap_or(fps);

        let duration = stream["duration"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| {
                format["duration"]
                    .as_str()
                    .and_then(|s| s.parse::<f64>().ok())
            })
            .unwrap_or(0.0);

        let mut frame_count = stream["nb_frames"]
            .as_str()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);

        // REFUERZO: Si es WMV/AVI o nb_frames es sospechoso, confiar en duraciÃ³n
        let is_asf = format_name.contains("asf") || format_name.contains("wmv");
        if duration > 0.0 {
            let expected = (duration * fps).round() as usize;
            if frame_count == 0
                || is_asf
                || (frame_count as f64 - expected as f64).abs() > (expected as f64 * 0.2)
            {
                frame_count = expected;
            }
        }
        frame_count = sc_frame_count.unwrap_or(frame_count);

        // CONTEO EXACTO POR PAQUETES cuando el contenedor no declara nb_frames
        // ni duración (fragmented MP4/MOV, dumps de stream) o es ASF/WMV con
        // metadatos poco fiables. Un total inventado (antes quedaba en 1 por el
        // max(1)) rompía el % de progreso y podía mandar el frame de referencia
        // del análisis más allá del último frame real → referencia NEGRA y
        // todas las puntuaciones en 0. Leer el índice del contenedor no
        // decodifica nada y para MP4/MOV es casi instantáneo.
        if sc_frame_count.is_none() && (frame_count == 0 || is_asf) {
            let mut cmd = Command::new(&ffprobe_path);
            #[cfg(target_os = "windows")]
            cmd.creation_flags(0x08000000);
            let counted = cmd
                .args([
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
                ])
                .output()
                .ok()
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
                        "FFprobe: conteo exacto de frames por paquetes = {} (el contenedor no declaraba nb_frames fiable).",
                        n
                    ),
                );
                frame_count = n;
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

        if width == 0 || height == 0 {
            return Err(format!(
                "Dimensiones invÃ¡lidas: {}x{}. Archivo corrupto?",
                width, height
            ));
        }

        Ok(FfmpegReader {
            path: path.to_string(),
            width,
            height,
            frame_count: frame_count.max(1),
            bytes_per_pixel: if is_color { 6 } else { 2 },
            sample_bits,
            color_id,
            ffmpeg_path: ffmpeg_path.to_string(), // Ensure owned string
            fps,
            is_color,
            rotation,
            codec_name,
            stream_cache: Arc::new(Mutex::new(None)),
        })
    }

    pub fn get_frame(&self, index: usize, current_color_id: i32) -> Vec<u8> {
        // STREAMING CACHE LOGIC
        let expected = self.width * self.height * self.bytes_per_pixel;
        let mut buffer = vec![0u8; expected];

        let mut cache_guard = self.stream_cache.lock().unwrap();

        let mut needs_restart = true;
        let mut frames_to_skip = 0;

        if let Some((ref mut current_idx, ref mut _stream)) = *cache_guard {
            if index == *current_idx {
                needs_restart = false;
                frames_to_skip = 0;
            } else if index > *current_idx && (index - *current_idx) < 100 {
                // If it's a small jump forward, just read and discard the in-between frames (Fast-Forward)
                frames_to_skip = index - *current_idx;
                needs_restart = false;
            } else {
                // Jump backwards or huge jump forward -> Restart pipe
                needs_restart = true;
            }
        }

        if needs_restart {
            // Kill old stream implicitly by overriding it
            let start_time = if index > 0 && self.fps > 0.0 {
                Some(format!("{:.4}", index as f64 / self.fps))
            } else {
                None
            };

            // FfmpegStreamIterator natively uses crop/scale. Since FfmpegReader handles scaling/cropping later optionally,
            // for pure get_frame we just ask for full scale, no crop.
            match FfmpegStreamIterator::new(
                &self.path,
                self.width,
                self.height,
                0,           // x
                0,           // y
                self.width,  // w
                self.height, // h
                current_color_id,
                &self.ffmpeg_path,
                start_time,
                true, // use GPU
                &self.codec_name,
                self.rotation,
            ) {
                Ok(new_stream) => {
                    *cache_guard = Some((index, new_stream));
                    frames_to_skip = 0; // The stream starts EXACTLY at `index` due to `-ss` before `-i`
                }
                Err(e) => {
                    eprintln!("DEBUG: Failed to restart stream: {}", e);
                    return vec![0u8; expected];
                }
            }
        }

        // Now we definitely have a stream
        if let Some((ref mut current_idx, ref mut stream)) = *cache_guard {
            // Fast-Forward if needed
            if frames_to_skip > 0 {
                stream.skip_frames(frames_to_skip);
            }

            // Read target frame
            if stream.read_frame_into(&mut buffer) {
                *current_idx = index + 1; // It consumed the frame, so next frame is index + 1
            } else {
                eprintln!("DEBUG: Stream EOF or Read Failed at index {}", index);
                *cache_guard = None; // Kill dead stream
            }
        }

        if cache_guard.is_none() {
            // ESCALERA DE REINTENTOS tras EOF/lectura fallida: el `-ss index/fps`
            // usa un fps ESTIMADO — con VFR o metadatos imprecisos puede caer en
            // (o tras) el final real y este método devolvía un frame NEGRO en
            // silencio; el máster de apilado y los previews heredaban ese vacío.
            //  A) buscar ~2 s antes y avanzar hasta el objetivo (frame exacto si
            //     la estimación era razonable);
            //  B) mismo seek sin avanzar (frame real ~2 s antes del objetivo);
            //  C) frame 0 (siempre existe en un vídeo legible).
            // Un frame cercano REAL siempre es mejor referencia/preview que
            // oscuridad absoluta. Decode CPU: máxima compatibilidad.
            let target_seconds = if self.fps > 0.0 && index > 0 {
                index as f64 / self.fps
            } else {
                0.0
            };
            let near_seek = (target_seconds - 2.0).max(0.0);
            let near_delta = if self.fps > 0.0 {
                index.saturating_sub((near_seek * self.fps).round() as usize)
            } else {
                index
            };
            let attempts: [(f64, usize); 3] = [(near_seek, near_delta), (near_seek, 0), (0.0, 0)];
            for (attempt, &(seek_s, skip)) in attempts.iter().enumerate() {
                let stream = FfmpegStreamIterator::new(
                    &self.path,
                    self.width,
                    self.height,
                    0,
                    0,
                    self.width,
                    self.height,
                    current_color_id,
                    &self.ffmpeg_path,
                    (seek_s > 0.0).then(|| format!("{:.4}", seek_s)),
                    false,
                    &self.codec_name,
                    self.rotation,
                );
                let Ok(mut retry_stream) = stream else {
                    continue;
                };
                if skip > 0 {
                    retry_stream.skip_frames(skip);
                }
                if retry_stream.read_frame_into(&mut buffer) {
                    // El stream queda posicionado tras el frame entregado; sólo
                    // el intento exacto conserva la numeración para reuso.
                    if attempt == 0 {
                        *cache_guard = Some((index + 1, retry_stream));
                    }
                    return buffer;
                }
            }
            eprintln!(
                "DEBUG: Retry EOF definitivo en frame {} (seek {:.2}s)",
                index, near_seek
            );
            buffer.fill(0);
        }

        buffer
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
        let p_fmt = if ffmpeg_stream_is_color(current_color_id) {
            "rgb48le"
        } else {
            "gray16le"
        };

        let mut filters = Vec::new();
        if let Some(rotation_filter) = ffmpeg_rotation_filter(self.rotation) {
            filters.push(rotation_filter.to_string());
        }
        filters.push(format!(
            "scale={}:{}:flags=spline+accurate_rnd+full_chroma_int+full_chroma_inp,setsar=1/1",
            self.width, self.height
        ));
        filters.push(format!("crop={}:{}:{}:{}", roi_w, roi_h, roi_x, roi_y));
        filters.push(format!("format={}", p_fmt));
        let mut args = Vec::new();

        let mut seek_target = None;
        if index > 0 && self.fps > 0.0 {
            let time_secs = index as f64 / self.fps;
            seek_target = Some(format!("{:.4}", time_secs));
        }

        if seek_target.is_some() {
            filters.push(format!("select='eq(n\\,0)'"));
        } else {
            filters.push(format!("select='eq(n\\,{})'", index));
        }
        let filter = filters.join(",");

        if let Some(ref ss) = seek_target {
            args.extend_from_slice(&["-ss", ss]);
        }

        args.extend_from_slice(&["-hwaccel", "auto"]);
        args.push("-noautorotate");
        args.extend_from_slice(&["-i", &self.path]);

        args.extend_from_slice(&[
            "-threads",
            "0",
            "-fps_mode",
            "passthrough",
            "-vframes",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            p_fmt,
            "-vf",
            &filter,
            "pipe:1",
        ]);

        let output = {
            let mut cmd = Command::new(&self.ffmpeg_path);
            #[cfg(target_os = "windows")]
            cmd.creation_flags(0x08000000);
            cmd.args(&args).output()
        };

        match output {
            Ok(o) => {
                let mut data = o.stdout;
                let expected = roi_w * roi_h * self.bytes_per_pixel;
                if data.len() > expected {
                    data.truncate(expected);
                } else if data.len() < expected && !data.is_empty() {
                    data.resize(expected, 0);
                }
                data
            }
            Err(_) => Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct FfmpegStreamIterator {
    reader: BufReader<std::process::ChildStdout>,
    _process: std::process::Child,
    frame_size_bytes: usize,
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
        use_gpu: bool,
        _codec_name: &str,
        rotation: i32,
    ) -> Result<Self, String> {
        let is_color = ffmpeg_stream_is_color(color_id);
        let p_fmt = if is_color { "rgb48le" } else { "gray16le" };
        let bpp = if is_color { 6 } else { 2 };
        let frame_size_bytes = roi_w * roi_h * bpp;

        // Construct Filter Chain. Rotation must happen before resize/crop: the
        // public FfmpegReader dimensions already describe the displayed
        // orientation (width/height are swapped for 90/270 degrees).
        let mut filters = Vec::new();
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
        // 3. Format
        filters.push(format!("format={}", p_fmt));
        let mut args = Vec::new();
        // Robust Probe Limits for MOV (MOOV at end)
        args.extend_from_slice(&["-analyzeduration", "100M", "-probesize", "100M"]);
        args.extend_from_slice(&["-hide_banner", "-nostdin", "-y"]);

        let filter_str = filters.join(",");

        // GPU Hardware Acceleration (Auto)
        if use_gpu {
            args.extend_from_slice(&["-hwaccel", "auto"]);
        }

        // Seek Input (Fast Seek)
        if let Some(ref ss) = start_time {
            args.extend_from_slice(&["-ss", ss]);
        }

        // La orientación se aplica mediante el filtro anterior. Desactivar la
        // autorrotación implícita evita doble giro y mantiene CPU/GPU idénticos.
        args.push("-noautorotate");

        // INTELLIGENT CPU MANAGEMENT (Modest PCs)
        // Detect logical cores and reserve 1-2 cores to prevent freezing.
        let num_cpus = num_cpus::get(); // Use the standard `num_cpus` crate already in use for rayon
        let ffmpeg_threads = if num_cpus <= 4 {
            (num_cpus - 1).max(1) // Keep at least 1 core free for OS on weak PCs
        } else {
            (num_cpus - 2).max(4) // Keep 2 cores free for OS on powerful PCs
        };
        let thread_str = ffmpeg_threads.to_string();
        args.extend_from_slice(&["-threads", &thread_str]);
        // Los filtros (scale neighbor / crop / format) van por defecto en UN
        // solo hilo — en 4K rgb48le la conversión era el cuello del productor.
        // El troceado por slices es determinista: bytes idénticos.
        args.extend_from_slice(&["-filter_threads", &thread_str]);

        args.extend_from_slice(&["-i", path]);
        args.extend_from_slice(&[
            "-an",
            "-sn",
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            p_fmt,
            "-vf",
            &filter_str,
            "pipe:1",
        ]);

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

        let stdout = child.stdout.take().ok_or("Failed to open ffmpeg stdout")?;
        let stderr = child.stderr.take().ok_or("Failed to open ffmpeg stderr")?;

        // SPAWN THREAD TO DRAIN STDERR (PREVENT DEADLOCK)
        std::thread::spawn(move || {
            eprintln!("DEBUG: FfmpegStreamIterator Stderr Thread Started");
            use std::io::BufRead;
            let reader = std::io::BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(l) = line {
                    // Filter verbose generic "frame=" lines if too spammy, or just print everything
                    eprintln!("[FFMPEG STREAM]: {}", l);
                }
            }
        });

        Ok(Self {
            reader: BufReader::with_capacity(frame_size_bytes + 65536, stdout),
            _process: child,
            frame_size_bytes,
        })
    }
}

impl FfmpegStreamIterator {
    // OPT FFMPEG 1: ZERO-ALLOCATION FRAME READING
    // We completely disabled `impl Iterator` because it FORCES the return of a new `Vec<u8>` every frame,
    // which triggers an extreme DDOS of Windows RAM (~25MB per frame alloc/dealloc = 25GB per 1000 frames).
    // Now we write strictly into a pre-allocated fixed memory block without ever allocating new RAM.
    pub fn read_frame_into(&mut self, buffer: &mut [u8]) -> bool {
        if buffer.len() != self.frame_size_bytes {
            return false;
        }

        read_exact_ffmpeg_frame(&mut self.reader, buffer)
    }

    // OPT FFMPEG 3: FAST-FORWARD DRAIN (Sequential Skipping)
    // Instantly dumps N frames straight from the pipe into the void without decoding or housing them.
    pub fn skip_frames(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let bytes_to_dump = count as u64 * self.frame_size_bytes as u64;
        let mut sink = std::io::sink();

        // Take exact amount and drain it directly
        let mut take = (&mut self.reader).take(bytes_to_dump);
        let _ = std::io::copy(&mut take, &mut sink);
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
        let _ = self._process.kill();
        let _ = self._process.wait();
    }
}

#[derive(Clone, Debug)]
enum VideoInput {
    Ser(SerReader),
    Avi(AviReader),
    Ffmpeg(FfmpegReader),
    Fits(FitsSequenceReader),
}

impl VideoInput {
    fn open(path: &str, app: &tauri::AppHandle) -> Result<Self, String> {
        println!("DEBUG: [VideoInput::open] Opening: {}", path);
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

            if ser_color_is_native_decodable(cid) {
                match SerReader::new(path) {
                    Ok(r) => {
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

        // Intentar FFmpeg para el resto (o si SER nativo falla)
        match FfmpegReader::new(path, app) {
            Ok(r) => {
                println!("DEBUG: [VideoInput] Using FFmpeg Reader for: {}", path);
                return Ok(VideoInput::Ffmpeg(r));
            }
            Err(e) => {
                // Fallback AVI nativo (Solo para archivos .avi reales)
                if ext == "avi" {
                    if let Ok(r) = AviReader::new(path) {
                        return Ok(VideoInput::Avi(r));
                    }
                }
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
    fn color_id(&self) -> i32 {
        match self {
            VideoInput::Ser(r) => r.info.color_id,
            VideoInput::Avi(r) => r.info.color_id,
            VideoInput::Ffmpeg(r) => r.color_id,
            VideoInput::Fits(r) => r.color_id,
        }
    }
    fn fps(&self) -> f64 {
        match self {
            VideoInput::Ser(_r) => 30.0,
            VideoInput::Avi(r) => r.info.fps,
            VideoInput::Ffmpeg(r) => r.fps,
            VideoInput::Fits(r) => r.fps,
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
            VideoInput::Ser(r) => r.get_frame(idx, cid),
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
                if roi_x == 0 && roi_y == 0 && roi_w == r.info.width && roi_h == r.info.height {
                    return r.get_frame(idx, cid);
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
                Cow::Owned(out)
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
    channels: Vec<Vec<f32>>, // [0]=Y or [0,1,2]=RGB
    params: DeconvParams,
    width: usize,
    height: usize,
}

#[derive(Clone)]
struct WaveletLayers {
    channels: Vec<Vec<Vec<f32>>>, // [Channel][Layer][Pixel]
    width: usize,
    height: usize,
    parent_deconv_params: DeconvParams,
    // B: descomposicion edge-aware activa → invalida el cache de wavelets.
    edge_aware: bool,
    // B+: intensidad edge-aware (nº bandas bilaterales + estrechez del rango) →
    // tambien cambia la descomposicion, invalida el cache cuando edge_aware=on.
    edge_aware_strength: f32,
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
}

#[derive(Clone)]
struct FilterCache {
    channels: Vec<Vec<f32>>, // [0]=Y or [0,1,2]=RGB
    params: FilterParams,
    width: usize,
    height: usize,
}

struct AppState {
    stacked_image: Mutex<Option<StackResult>>,
    /// Máster lineal float32 y mapas científicos de cielo profundo. Se mantiene
    /// separado del resultado planetario u16 para no perder headroom.
    deep_sky_result: Mutex<Option<DeepSkyLinearResult>>,
    deconv_cache: Mutex<Vec<DeconvCache>>,
    wavelet_cache: Mutex<Vec<WaveletLayers>>,
    filter_cache: Mutex<Vec<FilterCache>>,
    batch_anchor: Mutex<Option<Vec<u16>>>,
    batch_anchor_dims: Mutex<(usize, usize)>,
    active_req_id: AtomicUsize,
    // Cancelacion cooperativa de analisis/apilado. Arc para poder clonarlo a
    // los hilos productores (decoder FFmpeg, prefetcher) que sobreviven al
    // scope del comando. Se resetea al INICIAR una operacion de usuario, y lo
    // consultan los bucles pesados para abortar limpio con Err("Cancelado").
    cancel_requested: Arc<std::sync::atomic::AtomicBool>,
    license_manager: Arc<LicenseManager>,
}

#[derive(Clone)]
struct StackResult {
    data: Vec<u16>,
    width: usize,
    height: usize,
    is_mono: bool,
    is_surface: bool, // Support for V2 surface handling
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
    preview_base64: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Copy)]
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
        assert!(fs::metadata(&other_mode_a7).is_ok(), "otro modo con la versión vigente permanece");
        assert!(fs::metadata(&other_video).is_ok(), "otro vídeo no se toca");
        assert!(fs::metadata(&stale_a6).is_err(), "la versión vieja del mismo vídeo se borra");
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod f3_fits_output_tests {
    /// F3: el FITS 16-bit de salida debe releerse con las MISMAS dimensiones
    /// y valores por el propio lector de secuencias FITS (que aplica
    /// BZERO/BSCALE), cerrando el round-trip write→read.
    #[test]
    fn rgb16_fits_roundtrips_through_reader() {
        let (w, h) = (5usize, 4usize);
        let rgb: Vec<u16> = (0..w * h * 3).map(|i| ((i * 4099 + 7) % 65536) as u16).collect();
        let dir = std::env::temp_dir().join(format!("zas_fits_out_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stack.fits");
        crate::write_rgb16_fits(path.to_str().unwrap(), &rgb, w, h).unwrap();

        let reader = crate::fits_sequence::FitsSequenceReader::new(path.to_str().unwrap())
            .expect("FITS de salida legible por el lector de secuencias");
        assert_eq!(reader.width, w);
        assert_eq!(reader.height, h);
        // El lector devuelve el PRIMER plano (canal R) como mono16 LE.
        let frame = reader.get_frame(0);
        assert!(!frame.is_empty(), "el frame no debe venir vacío");
        let r0 = u16::from_le_bytes([frame[0], frame[1]]);
        assert_eq!(r0, rgb[0], "el primer píxel R debe sobrevivir el round-trip FITS");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
