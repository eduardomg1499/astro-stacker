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

// NUEVO: FunciÃ³n unificada para generar ruta de cachÃ© de anÃ¡lisis
fn get_analysis_cache_path(video_path: &str, mode_suffix: &str) -> String {
    // Normalizar path para evitar mismatches por ", /, o mayÃºsculas
    let p = Path::new(video_path);
    // Usamos el path limpio, pero conservamos la integridad del nombre base
    let clean = clean_windows_path(p.to_path_buf());
    format!("{}_{}.analysis_v2", clean, mode_suffix)
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
            return Err(format!("Error: No se encontro FFprobe. {}", ffmpeg_install_hint()));
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
        let probe: serde_json::Value = serde_json::from_str(&json_str)
            .map_err(|e| {
                let stderr = String::from_utf8_lossy(&output.stderr);
                format!(
                    "Error al parsear JSON de FFprobe: {}. {}",
                    e,
                    stderr.trim()
                )
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
                // Fallback to empty
                buffer.fill(0);
                *cache_guard = None; // Kill dead stream
            }
        }

        buffer
    }

    pub fn get_frames_batch(
        &self,
        indices: &[usize], // MUST BE SORTED
        current_color_id: i32,
    ) -> Result<std::collections::HashMap<usize, Vec<u8>>, String> {
        if indices.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let p_fmt = if self.is_color && (current_color_id < 8 || current_color_id > 11) {
            "rgb48le"
        } else {
            "gray16le"
        };

        let mut filters = Vec::new();
        let needs_deblock = matches!(
            self.codec_name.as_str(),
            "h264" | "hevc" | "h265" | "mpeg4" | "mpeg2video"
        );
        if needs_deblock {
            filters.push("unsharp=3:3:-0.3:3:3:-0.3".to_string());
        }
        filters.push(format!(
            "scale={}:{}:flags=neighbor",
            self.width, self.height
        ));
        filters.push(format!("format={}", p_fmt));

        let first_idx = indices[0];
        let mut args = Vec::new();
        let mut seek_target = None;
        if first_idx > 0 && self.fps > 0.0 {
            let time_secs = first_idx as f64 / self.fps;
            seek_target = Some(format!("{:.4}", time_secs));
        }

        let mut select_clauses = Vec::with_capacity(indices.len());
        for &idx in indices {
            let n_val = idx - first_idx;
            select_clauses.push(format!("eq(n\\,{})", n_val));
        }
        let select_expr = select_clauses.join("+");

        filters.push(format!("select='{}'", select_expr));
        let filter = filters.join(",");

        if let Some(ref ss) = seek_target {
            args.extend_from_slice(&["-ss", ss]);
        }
        args.extend_from_slice(&["-hwaccel", "auto"]);
        args.extend_from_slice(&["-i", &self.path]);

        let vframes = indices.len().to_string();
        args.extend_from_slice(&[
            "-threads",
            "0",
            "-fps_mode",
            "passthrough",
            "-vframes",
            &vframes,
            "-f",
            "rawvideo",
            "-pix_fmt",
            p_fmt,
            "-vf",
            &filter,
            "pipe:1",
        ]);

        let perform_ffmpeg = |ffmpeg_args: &[&str]| -> Vec<u8> {
            let mut cmd = Command::new(&self.ffmpeg_path);
            #[cfg(target_os = "windows")]
            cmd.creation_flags(0x08000000);
            cmd.args(ffmpeg_args)
                .output()
                .map(|o| o.stdout)
                .unwrap_or_default()
        };

        let mut data = perform_ffmpeg(&args);
        let expected_frame_size = self.width * self.height * self.bytes_per_pixel;

        if data.is_empty() {
            let mut fallback_args = Vec::new();
            if let Some(ref ss) = seek_target {
                fallback_args.extend_from_slice(&["-ss", ss]);
            }
            let fallback_filter = if self.rotation != 0 {
                let transpose_filter = match self.rotation {
                    90 => "transpose=1",
                    180 => "transpose=2,transpose=2",
                    270 => "transpose=2",
                    _ => "",
                };
                format!(
                    "{},{},{},select='{}'",
                    transpose_filter,
                    format!("scale={}:{}:flags=neighbor", self.width, self.height),
                    format!("format={}", p_fmt),
                    select_expr
                )
            } else {
                format!(
                    "scale={}:{}:flags=neighbor,format={},select='{}'",
                    self.width, self.height, p_fmt, select_expr
                )
            };

            fallback_args.extend_from_slice(&[
                "-i",
                &self.path,
                "-threads",
                "0",
                "-fps_mode",
                "passthrough",
                "-vframes",
                &vframes,
                "-f",
                "rawvideo",
                "-pix_fmt",
                p_fmt,
                "-vf",
                &fallback_filter,
                "pipe:1",
            ]);
            data = perform_ffmpeg(&fallback_args);
        }

        if data.is_empty() {
            return Err("FFmpeg devolviÃ³ 0 bytes en modo Batch".to_string());
        }

        let mut map = std::collections::HashMap::with_capacity(indices.len());
        for (i, &idx) in indices.iter().enumerate() {
            let start = i * expected_frame_size;
            let end = start + expected_frame_size;
            if end <= data.len() {
                map.insert(idx, data[start..end].to_vec());
            } else {
                break;
            }
        }

        Ok(map)
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
        let p_fmt = if self.is_color && (current_color_id < 8 || current_color_id > 11) {
            "rgb48le"
        } else {
            "gray16le"
        };

        let mut filters = Vec::new();
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
        let is_color = color_id < 8 || color_id > 11;
        let p_fmt = if is_color { "rgb48le" } else { "gray16le" };
        let bpp = if is_color { 6 } else { 2 };
        let frame_size_bytes = roi_w * roi_h * bpp;

        // Construct Filter Chain
        let mut filters = Vec::new();
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
        let filter_str = filters.join(",");

        let mut args = Vec::new();
        // Robust Probe Limits for MOV (MOOV at end)
        args.extend_from_slice(&["-analyzeduration", "100M", "-probesize", "100M"]);
        args.extend_from_slice(&["-hide_banner", "-nostdin", "-y"]);

        // 4. Rotation (NEW: Added Rotation Parameter)
        let transpose_filter = match rotation {
            90 => "transpose=1",
            180 => "transpose=2,transpose=2",
            270 => "transpose=2",
            _ => "",
        };

        if !transpose_filter.is_empty() {
            filters.push(transpose_filter.to_string());
        }

        // GPU Hardware Acceleration (Auto)
        if use_gpu {
            args.extend_from_slice(&["-hwaccel", "auto"]);
        }

        // Seek Input (Fast Seek)
        if let Some(ref ss) = start_time {
            args.extend_from_slice(&["-ss", ss]);
        }

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

        match self.reader.read_exact(buffer) {
            Ok(_) => true,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::UnexpectedEof {
                    eprintln!("DEBUG: FfmpegStream Read Failed (Msg): {}", e);
                }
                false
            }
        }
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
            VideoInput::Avi(r) => Cow::Borrowed(r.get_frame(idx, cid)),
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
                    return Cow::Borrowed(r.get_frame(idx, cid));
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
    deconv_cache: Mutex<Vec<DeconvCache>>,
    wavelet_cache: Mutex<Vec<WaveletLayers>>,
    filter_cache: Mutex<Vec<FilterCache>>,
    batch_anchor: Mutex<Option<Vec<u16>>>,
    batch_anchor_dims: Mutex<(usize, usize)>,
    active_req_id: AtomicUsize,
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
