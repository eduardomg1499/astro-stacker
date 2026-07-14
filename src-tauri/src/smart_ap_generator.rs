// ==========================================\n// 8. SMART AP GENERATOR (Integrated)\n// ==========================================\n
#[derive(serde::Serialize)]
struct SerProfileEstimate {
    profile: String,
    is_color: bool,
    bit_depth: i32,
    bytes_per_frame: u64,
    estimated_size_bytes: u64,
    recommended: bool,
}

#[derive(serde::Serialize)]
struct SerConversionPreflight {
    input_path: String,
    file_name: String,
    source_size_bytes: u64,
    width: usize,
    height: usize,
    frame_count: usize,
    source_is_color: bool,
    source_bit_depth: i32,
    pix_fmt: String,
    estimates: Vec<SerProfileEstimate>,
}

fn detect_video_bit_depth(app: &tauri::AppHandle, input_path: &str) -> (bool, String) {
    let mut is_8bit = true;
    let mut pix_fmt = String::from("unknown");
    let ffprobe_path = get_ffprobe_command(app);

    let probe_output = {
        let mut cmd = std::process::Command::new(&ffprobe_path);
        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x08000000);
        cmd.args(&[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=pix_fmt",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            input_path,
        ])
        .output()
    };

    if let Ok(output) = probe_output {
        let text = String::from_utf8_lossy(&output.stdout)
            .trim()
            .to_lowercase();
        if !text.is_empty() {
            pix_fmt = text.clone();
        }
        if text.contains("10") || text.contains("12") || text.contains("14") || text.contains("16")
        {
            is_8bit = false;
        }
    }

    (is_8bit, pix_fmt)
}

fn ser_estimate_for_profile(
    profile: &str,
    width: usize,
    height: usize,
    frame_count: usize,
    source_is_color: bool,
    source_is_8bit: bool,
) -> SerProfileEstimate {
    let (is_color, is_8bit, recommended) = match profile {
        "color8" => (source_is_color, true, false),
        "autoDepth" => (source_is_color, source_is_8bit, false),
        _ => (false, true, true),
    };
    let bytes_per_pixel = match (is_color, is_8bit) {
        (true, true) => 3u64,
        (true, false) => 6u64,
        (false, true) => 1u64,
        (false, false) => 2u64,
    };
    let bytes_per_frame = (width as u64)
        .saturating_mul(height as u64)
        .saturating_mul(bytes_per_pixel);
    let estimated_size_bytes =
        178u64.saturating_add(bytes_per_frame.saturating_mul(frame_count as u64));

    SerProfileEstimate {
        profile: profile.to_string(),
        is_color,
        bit_depth: if is_8bit { 8 } else { 16 },
        bytes_per_frame,
        estimated_size_bytes,
        recommended,
    }
}

fn default_ser_output_path(input_path: &Path) -> Result<PathBuf, String> {
    let mut out_path_buf = input_path.with_extension("ser");
    if out_path_buf.exists() {
        let parent = input_path.parent().unwrap_or_else(|| Path::new(""));
        let stem = input_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("converted");
        let mut unique_path = None;
        for idx in 1..1000 {
            let candidate = parent.join(format!("{}_converted_{}.ser", stem, idx));
            if !candidate.exists() {
                unique_path = Some(candidate);
                break;
            }
        }
        out_path_buf =
            unique_path.ok_or_else(|| "No se pudo generar un nombre SER unico.".to_string())?;
    }
    Ok(out_path_buf)
}

#[tauri::command]
async fn get_ser_conversion_preflight(
    app: tauri::AppHandle,
    input_path: String,
) -> Result<SerConversionPreflight, String> {
    let p = Path::new(&input_path);
    if !p.exists() {
        return Err("Archivo no encontrado".to_string());
    }

    let input = VideoInput::open(&input_path, &app).map_err(|e| e.to_string())?;
    let width = input.width();
    let height = input.height();
    let frame_count = input.frame_count();
    let source_is_color = input.is_color();
    let (source_is_8bit, pix_fmt) = detect_video_bit_depth(&app, &input_path);

    Ok(SerConversionPreflight {
        input_path: input_path.clone(),
        file_name: p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("video")
            .to_string(),
        source_size_bytes: std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
        width,
        height,
        frame_count,
        source_is_color,
        source_bit_depth: if source_is_8bit { 8 } else { 16 },
        pix_fmt,
        estimates: vec![
            ser_estimate_for_profile("mono8", width, height, frame_count, source_is_color, source_is_8bit),
            ser_estimate_for_profile("color8", width, height, frame_count, source_is_color, source_is_8bit),
            ser_estimate_for_profile("autoDepth", width, height, frame_count, source_is_color, source_is_8bit),
        ],
    })
}

#[tauri::command]
async fn convert_video_to_ser_frontend(
    app: tauri::AppHandle,
    input_path: String,
    profile: Option<String>,
    output_path: Option<String>,
) -> Result<converter::ConversionResult, String> {
    let p = Path::new(&input_path);
    if !p.exists() {
        return Err("Archivo no encontrado".to_string());
    }

    let out_path_buf = if let Some(path) = output_path {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            default_ser_output_path(p)?
        } else {
            let mut explicit = PathBuf::from(trimmed);
            if explicit
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase()
                != "ser"
            {
                explicit.set_extension("ser");
            }
            if let Some(parent) = explicit.parent() {
                if !parent.as_os_str().is_empty() && !parent.exists() {
                    return Err("La carpeta de destino no existe".to_string());
                }
            }
            explicit
        }
    } else {
        default_ser_output_path(p)?
    };
    let out_path = out_path_buf.to_string_lossy().to_string();

    // 1. Open Video to get properties
    let input = VideoInput::open(&input_path, &app).map_err(|e| e.to_string())?;

    let width = input.width();
    let height = input.height();
    let frame_count = input.frame_count();
    let source_is_color = input.is_color();

    // 2. Get FFmpeg Path (Reusing helper from FfmpegReader)
    let ffmpeg_path = get_ffmpeg_command(&app);

    let (is_8bit, pix_fmt) = detect_video_bit_depth(&app, &input_path);
    if is_8bit {
        println!("DEBUG: [convert_video_to_ser_frontend] Detected 8-bit source ({}). Optimization active.", pix_fmt);
    } else {
        println!("DEBUG: [convert_video_to_ser_frontend] Detected >8-bit source ({}). Using 16-bit output.", pix_fmt);
    }

    let requested_profile = profile
        .unwrap_or_else(|| "mono8".to_string())
        .trim()
        .to_lowercase();

    let (is_color, is_8bit) = match requested_profile.as_str() {
        "color8" => (source_is_color, true),
        "autodepth" | "auto_depth" | "auto" => (source_is_color, is_8bit),
        "mono16" => (false, false),
        _ => (false, true),
    };

    // 3. Run Conversion (Blocking to avoid freezing async runtime)
    let app_handle = app.clone();
    let p_clone = input_path.clone();
    let o_clone = out_path.clone();

    let result = tauri::async_runtime::spawn_blocking(move || {
        converter::convert_video_to_ser(
            &p_clone,
            &o_clone,
            width,
            height,
            frame_count,
            is_color,
            is_8bit, // Pasar flag de 8 bits a converter
            &ffmpeg_path,
            &app_handle,
        )
    })
    .await
    .map_err(|e| e.to_string())??;

    Ok(result)
}

// ==========================================
// 8. SMART AP GENERATOR (Integrated)
// ==========================================
#[tauri::command]
async fn generate_smart_ap_grid(
    app: tauri::AppHandle,
    _state: State<'_, AppState>,
    path: String,
    grid_size: usize,
    threshold: f32, // 0.0 - 1.0 (Variance threshold)
    ref_frame_idx: Option<usize>,
    mode: Option<String>,
) -> Result<Vec<ApPoint>, String> {
    // 1. Open Video & Get Master Reference (Index 0 or Best)
    let r = VideoInput::open(&path, &app)?;
    let w = r.width();
    let h = r.height();
    let bpp = r.bpp();
    let cid = r.color_id();

    let frame_idx = ref_frame_idx.unwrap_or(0).min(r.frame_count().saturating_sub(1));
    let raw = r.get_frame(frame_idx, cid);
    if raw.is_empty() {
        return Err("No se pudo leer el frame de referencia".into());
    }

    // 2. Convert to Full 16-bit Buffer (Optimized with AVX2)
    // This gives us uniform access to pixel values (0-65535) regardless of input depth.
    let u16_data = raw_to_u16_buffer(&raw, w, h, bpp);

    // 3. Calculate Noise Floor (Dynamic Background Detection)
    // Used to ignore dark background areas (space).
    // Delegate to shared logic in smart_grid.rs
    // Mode defaults to "surface" as this command is generic.
    // Ideally frontend should pass mode.
    let grid_mode = mode.unwrap_or_else(|| "surface".to_string());
    let points =
        smart_grid::generate_smart_grid_internal(&u16_data, w, h, grid_size, threshold, &grid_mode);

    Ok(points)
}
// 8. SMART AP GENERATOR (Integrated)
// ==========================================
