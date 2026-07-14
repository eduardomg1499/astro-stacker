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
    // 1. Invalida el req_id activo → los workers del analisis y el pipeline de
    //    wavelets que consultan check_cancel() dejan de trabajar de inmediato.
    state.active_req_id.store(0, Ordering::Relaxed);
    // 2. Flag cooperativo → los bucles de apilado, el prefetcher y el decoder
    //    FFmpeg abortan limpio y el comando devuelve Err("Cancelado...").
    state
        .cancel_requested
        .store(true, std::sync::atomic::Ordering::Relaxed);
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

#[tauri::command]
async fn scan_directory(
    _app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    recursive: bool,
) -> Result<Vec<String>, String> {
    state.license_manager.check_access()?;

    let mut files = Vec::new();
    let root = Path::new(&path);
    if !root.exists() {
        return Err("La carpeta no existe".into());
    }

    fn visit_dirs(dir: &Path, files: &mut Vec<String>, recursive: bool) -> std::io::Result<()> {
        if dir.is_dir() {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() && recursive {
                    visit_dirs(&path, files, recursive)?;
                } else if let Some(ext) = path.extension() {
                    let ext_str = ext.to_string_lossy().to_lowercase();
                    if ext_str == "ser" || ext_str == "avi" {
                        files.push(clean_windows_path(path));
                    }
                }
            }
        }
        Ok(())
    }

    visit_dirs(root, &mut files, recursive).map_err(|e| e.to_string())?;
    files.sort();
    Ok(files)
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
    emit_progress(&app, "Iniciando exportacion...", 0.0, None);

    // 0. Preparar Paths
    let p = Path::new(&folder);
    if !p.exists() {
        return Err("Carpeta no existe".into());
    }

    // 1. Validar FFmpeg si es video
    let is_video = format == "mp4" || format == "avi";
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
        let mut rev = files_to_process.clone();
        rev.reverse();
        // Remove first and last to avoid duplication at turning points
        if rev.len() > 2 {
            rev.remove(0);
            rev.remove(rev.len() - 1);
            files_to_process.extend(rev);
        }
    }

    let out_filename = format!("export_{}.{}", chrono::Utc::now().timestamp(), format);
    let out_file = p.join(&out_filename);

    // 3. Preparar Encoder (Deferred for Video)
    let mut gif_encoder: Option<image::codecs::gif::GifEncoder<BufWriter<File>>> = None;
    let mut ffmpeg_child: Option<std::process::Child> = None;
    let fps = 1000.0 / (delay_ms.max(20) as f64);

    // Canonical format to ensure FFmpeg stream homogeneity
    let mut canonical_w = 0u32;
    let mut canonical_h = 0u32;
    let mut canonical_depth_16 = false;

    if !is_video {
        // GIF Init (can be done early)
        let f = File::create(&out_file).map_err(|e| e.to_string())?;
        let writer = BufWriter::new(f);
        let mut enc = image::codecs::gif::GifEncoder::new(writer);
        enc.set_repeat(image::codecs::gif::Repeat::Infinite)
            .map_err(|e| e.to_string())?;
        gif_encoder = Some(enc);
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
        emit_progress(
            &app,
            "Procesando frame",
            (i as f32 / files_to_process.len() as f32) * 100.0,
            None,
        );

        // A. Cargar Imagen y NormalizaciÃ³n Inicial
        let mut img = image::open(fpath).map_err(|e| e.to_string())?;
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
            for p in rgb.pixels_mut() {
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
            for p in rgb.pixels_mut() {
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
            args.push(clean_windows_path(out_file.clone()));

            let child = {
                let mut cmd = Command::new(&ffmpeg_cmd);
                #[cfg(target_os = "windows")]
                cmd.creation_flags(0x08000000);
                cmd.args(&args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            };

            match child {
                Ok(child) => ffmpeg_child = Some(child),
                Err(e) => return Err(format!("Error iniciando FFmpeg: {}", e)),
            }
        }

        // H. Output (Pipe or GIF)
        if is_video {
            if let Some(child) = &mut ffmpeg_child {
                if let Some(stdin) = &mut child.stdin {
                    // Send RAW pixels to FFmpeg
                    if canonical_depth_16 {
                        // rgb48be (16-bit Big-Endian RGB)
                        let rgb = img.to_rgb16();
                        let raw = rgb.as_raw();
                        let mut be_bytes = Vec::with_capacity(raw.len() * 2);
                        for &v in raw {
                            be_bytes.extend_from_slice(&v.to_be_bytes());
                        }
                        let _ = stdin.write_all(&be_bytes);
                    } else {
                        // rgb24 (8-bit RGB)
                        let rgb = img.to_rgb8();
                        let _ = stdin.write_all(rgb.as_raw());
                    }
                }
            }
        } else {
            if let Some(enc) = &mut gif_encoder {
                let frame = Frame::from_parts(
                    img.to_rgba8(),
                    0,
                    0,
                    Delay::from_numer_denom_ms(delay_ms as u32, 1),
                );
                let _ = enc.encode_frame(frame);
            }
        }
    } // End Loop

    // Finalize Video
    if is_video {
        if let Some(mut child) = ffmpeg_child {
            drop(child.stdin.take());
            let output = child.wait_with_output().map_err(|e| e.to_string())?;
            if !output.status.success() {
                let err = String::from_utf8_lossy(&output.stderr);
                return Err(format!("FFmpeg Error: {}", err));
            }
        }
    }

    emit_progress(&app, "ExportaciÃ³n Finalizada", 100.0, None);
    Ok(clean_windows_path(out_file))
}

#[tauri::command]
async fn normalize_batch_brightness(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> Result<Vec<String>, String> {
    state.license_manager.check_access()?;
    emit_progress(&app, "Cargando imÃ¡genes...", 0.0, None);

    let images: Vec<(String, DynamicImage)> = paths
        .par_iter()
        .map(|p| {
            let img = image::open(p).ok();
            (p.clone(), img)
        })
        .filter_map(|(p, img)| img.map(|i| (p, i)))
        .collect();

    if images.is_empty() {
        return Err("Error al cargar imÃ¡genes".into());
    }

    // PHASE 39: Robust 16-bit Peak-based Normalization
    // We calculate the average of the brightest pixels (top 0.1%) to ignore background noise
    // and focus on the object brightness itself.
    let brightness_values: Vec<f32> = images
        .iter()
        .map(|(_, dynamic_img)| {
            // Convert to 16-bit internally for analysis if needed, or use directly
            let pixels: Vec<f32> = if let Some(rgb16) = dynamic_img.as_rgb16() {
                rgb16
                    .pixels()
                    .map(|p| 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32)
                    .collect()
            } else {
                dynamic_img
                    .to_rgb8()
                    .pixels()
                    .map(|p| {
                        (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) * 256.0
                    })
                    .collect()
            };

            if pixels.is_empty() {
                return 0.0;
            }

            let mut sorted = pixels;
            sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));

            // Mean of top 0.1% (minimum 10 pixels, max 1000 pixels for speed)
            let top_count = ((sorted.len() as f32 * 0.001) as usize).clamp(10, 1000);
            let peak_sum: f32 = sorted.iter().take(top_count).sum();
            peak_sum / top_count as f32
        })
        .collect();

    let target_peak = brightness_values.iter().sum::<f32>() / brightness_values.len() as f32;
    emit_progress(&app, "Ajustando exposiciÃ³n (16-bit)...", 50.0, None);

    let results: Vec<String> = images
        .into_par_iter()
        .zip(brightness_values)
        .map(|((path, mut dynamic_img), current_peak)| {
            if current_peak > 10.0 {
                let gain = target_peak / current_peak;
                if (gain - 1.0).abs() > 0.005 {
                    // Apply gain in 16-bit
                    if let Some(rgb16) = dynamic_img.as_mut_rgb16() {
                        for p in rgb16.pixels_mut() {
                            p[0] = (p[0] as f32 * gain).clamp(0.0, 65535.0) as u16;
                            p[1] = (p[1] as f32 * gain).clamp(0.0, 65535.0) as u16;
                            p[2] = (p[2] as f32 * gain).clamp(0.0, 65535.0) as u16;
                        }
                    } else {
                        // Fallback 8-bit
                        let mut rgb8 = dynamic_img.to_rgb8();
                        for p in rgb8.pixels_mut() {
                            p[0] = (p[0] as f32 * gain).clamp(0.0, 255.0) as u8;
                            p[1] = (p[1] as f32 * gain).clamp(0.0, 255.0) as u8;
                            p[2] = (p[2] as f32 * gain).clamp(0.0, 255.0) as u8;
                        }
                        dynamic_img = DynamicImage::ImageRgb8(rgb8);
                    }
                    // Overwrite original preserving depth
                    let _ = dynamic_img.save(&path);
                }
            }

            // ASSET PROTOCOL: el archivo ya quedo sobrescrito en disco — la UI
            // recarga la MISMA ruta via convertFileSrc (con cache-busting).
            // Devolver base64 por IPC re-pinaba todos los frames en el heap
            // del WebView (el mismo problema de RAM que se elimino del batch).
            path
        })
        .collect();

    emit_progress(&app, "NormalizaciÃ³n completa", 100.0, None);
    Ok(results)
}

#[tauri::command]
async fn crop_animation_frames(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    paths: Vec<String>,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<String>, String> {
    state.license_manager.check_access()?;
    emit_progress(&app, "Recortando animacion...", 0.0, None);
    let results: Result<Vec<String>, String> = paths
        .par_iter()
        .enumerate()
        .map(|(_, p)| {
            let path = Path::new(p);
            let mut img = image::open(path).map_err(|e| e.to_string())?;
            let cropped = img.crop(x, y, w, h);
            cropped.save(path).map_err(|e| e.to_string())?;
            let mut buf = Vec::new();
            DynamicImage::ImageRgba8(cropped.to_rgba8())
                .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "data:image/png;base64,{}",
                general_purpose::STANDARD.encode(&buf)
            ))
        })
        .collect();
    emit_progress(&app, "Recorte finalizado", 100.0, None);
    results
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
    let img0 = image::open(p0).map_err(|e| e.to_string())?.to_rgb8();
    let w = img0.width() as usize;
    let h = img0.height() as usize;

    // Convertir anchor a mono u16 para SAD
    // Convertir anchor a mono u16 para SAD
    let anchor_mono = {
        let mut m = Vec::with_capacity(w * h);
        for p in img0.pixels() {
            let val = (p[0] as u32 + p[1] as u32 + p[2] as u32) / 3;
            m.push(val as u16 * 256);
        }
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
    let stab_x = roi_x.min(w - roi_w);
    let stab_y = roi_y.min(h - roi_h);

    emit_progress(&app, "Analizando movimientos...", 10.0, None);

    // 2. Procesar todos los frames
    let results: Result<Vec<String>, String> = paths
        .par_iter()
        .enumerate()
        .map(|(i, p)| {
            if i == 0 {
                // Primer frame = anchor: no se toca en disco, devolver su ruta.
                return Ok(p.clone());
            }

            let path = Path::new(p);
            let img = image::open(path).map_err(|e| e.to_string())?;

            if img.width() as usize != w || img.height() as usize != h {
                return Err("Dimensiones inconsistentes detectadas en los frames de la animaciÃ³n. AsegÃºrate de que todas las imÃ¡genes tengan el mismo tamaÃ±o o recÃ³rtalas.".to_string());
            }

            // Convert to mono for alignment analysis only (doesn't change original img)
            let current_mono = {
                let w_u32 = img.width();
                let h_u32 = img.height();
                let mut m = Vec::with_capacity((w_u32 * h_u32) as usize);

                if let Some(rgb16) = img.as_rgb16() {
                    for p in rgb16.pixels() {
                        let luma = (p[0] as u32 + p[1] as u32 + p[2] as u32) / 3;
                        m.push(luma as u16);
                    }
                } else {
                    for p in img.to_rgb8().pixels() {
                        let luma = (p[0] as u32 + p[1] as u32 + p[2] as u32) / 3;
                        m.push(luma as u16 * 256);
                    }
                }

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

            // PHASE 39: 16-bit aware Bilinear Shift
            let mut shifted_16 = image::ImageBuffer::new(w as u32, h as u32);
            let mut shifted_8 = image::ImageBuffer::new(w as u32, h as u32);
            let is_16bit = img.as_rgb16().is_some();

            for y_out in 0..h as u32 {
                for x_out in 0..w as u32 {
                    let src_x = x_out as f32 + dx;
                    let src_y = y_out as f32 + dy;

                    if src_x >= 0.0
                        && src_x < (w as f32 - 1.0)
                        && src_y >= 0.0
                        && src_y < (h as f32 - 1.0)
                    {
                        let x0 = src_x.floor() as u32;
                        let y0 = src_y.floor() as u32;
                        let x1 = x0 + 1;
                        let y1 = y0 + 1;
                        let wx = src_x - x0 as f32;
                        let wy = src_y - y0 as f32;

                        if is_16bit {
                            let rgb16 = img.as_rgb16().unwrap();
                            let p00 = rgb16.get_pixel(x0, y0);
                            let p10 = rgb16.get_pixel(x1, y0);
                            let p01 = rgb16.get_pixel(x0, y1);
                            let p11 = rgb16.get_pixel(x1, y1);

                            let mut new_p = image::Rgb([0u16, 0u16, 0u16]);
                            for c in 0..3 {
                                let v00 = p00[c] as f32;
                                let v10 = p10[c] as f32;
                                let v01 = p01[c] as f32;
                                let v11 = p11[c] as f32;
                                let val = (v00 * (1.0 - wx) + v10 * wx) * (1.0 - wy)
                                    + (v01 * (1.0 - wx) + v11 * wx) * wy;
                                new_p[c] = val as u16;
                            }
                            shifted_16.put_pixel(x_out, y_out, new_p);
                        } else {
                            let rgb8 = img.to_rgb8();
                            let p00 = rgb8.get_pixel(x0, y0);
                            let p10 = rgb8.get_pixel(x1, y0);
                            let p01 = rgb8.get_pixel(x0, y1);
                            let p11 = rgb8.get_pixel(x1, y1);

                            let mut new_p = image::Rgb([0u8, 0u8, 0u8]);
                            for c in 0..3 {
                                let v00 = p00[c] as f32;
                                let v10 = p10[c] as f32;
                                let v01 = p01[c] as f32;
                                let v11 = p11[c] as f32;
                                let val = (v00 * (1.0 - wx) + v10 * wx) * (1.0 - wy)
                                    + (v01 * (1.0 - wx) + v11 * wx) * wy;
                                new_p[c] = val as u8;
                            }
                            shifted_8.put_pixel(x_out, y_out, new_p);
                        }
                    }
                }
            }

            // Save and Return base64
            let final_img = if is_16bit {
                DynamicImage::ImageRgb16(shifted_16)
            } else {
                DynamicImage::ImageRgb8(shifted_8)
            };

            final_img.save(path).map_err(|e| e.to_string())?;

            // ASSET PROTOCOL: el frame realineado ya esta sobrescrito en disco;
            // la UI lo recarga por ruta (cache-busting en el frontend). El
            // base64 por IPC pinaba todos los frames en el heap del WebView.
            Ok(p.clone())
        })
        .collect();

    emit_progress(&app, "Alineacion completada", 100.0, None);
    results
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

#[tauri::command]
async fn process_batch_entry(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    file_path: String,
    output_folder: String,
    stack_pct: f32,
    drizzle: f32,
    _align_mode: String,
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
    _usm_amount: f32,
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
    edge_aware_wavelets: Option<bool>, // B: wavelets edge-aware
    psf_from_limb: Option<bool>,       // A: deconv con PSF medida
    edge_aware_strength: Option<f32>,  // B+: intensidad edge-aware (0..100)
    auto_mask: Option<f32>,            // Calidad: sharpening adaptativo por SNR
    levels_black: Option<f32>,         // Niveles: punto negro (0..1)
    levels_white: Option<f32>,         // Niveles: punto blanco (0..1)
    levels_gamma: Option<f32>,         // Niveles: gamma medios (0.1..5)
) -> Result<BatchEntryResult, String> {
    state.license_manager.check_access()?;
    // Nueva entrada del lote: limpiar cancelaciones previas. Si el usuario
    // cancela a mitad de este archivo, el analisis/apilado devuelve
    // Err("Cancelado") y el frontend corta el bucle del lote.
    state
        .cancel_requested
        .store(false, std::sync::atomic::Ordering::Relaxed);
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
    let is_v3 = is_v3.unwrap_or_else(|| _align_mode.contains("v3") || batch_mode.contains("v3"));
    // Removed: let _ = (deconv_iter, deconv_sigma, vc_iter, vc_sigma);
    // Now using these parameters to apply deconvolution in batch mode
    if drizzle > 1.0 && !state.license_manager.is_pro() {
        return Err("Drizzle > 1.0 requiere licencia PRO.".into());
    }

    {
        state.deconv_cache.lock().unwrap().clear();
    }
    {
        state.wavelet_cache.lock().unwrap().clear();
        state.filter_cache.lock().unwrap().clear();
    }
    state.active_req_id.store(0, Ordering::Relaxed);

    let path_obj = Path::new(&file_path);
    let fname = path_obj.file_stem().unwrap().to_string_lossy();
    let save_path = Path::new(&output_folder).join(format!("{}.png", fname));

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
    let cid = bayer_override.unwrap_or_else(|| r.color_id());

    let ref_idx = select_signal_frame_index(&r, w, h, bpp, cid, total / 2);

    // --- CACHE & SCORE LOGIC ---
    let suffix =
        zenith_analysis_cache_suffix(&target_type, is_surface_batch, warping_analysis, anchor_override.is_some());
    let cache_path = get_analysis_cache_path(&file_path, &suffix);
    let source_fingerprint = planetary_source_fingerprint(&file_path)?;
    let mut cached_opt: Option<CachedAnalysis> = if Path::new(&cache_path).exists() {
        load_cached_analysis(&cache_path).filter(|cached| {
            cached.path_hash == source_fingerprint
                && cached.width == Some(w)
                && cached.height == Some(h)
                // Igual que el análisis interactivo: el total puede ser una
                // estimación en vídeos comprimidos — el fingerprint manda.
                && cached.frame_stats.as_ref().is_some_and(|stats| {
                    !stats.is_empty()
                        && stats.iter().all(|item| item.idx < total.max(stats.len()))
                })
        })
    } else {
        None
    };

    if cached_opt.is_none() {
        let _ = perform_standardized_analysis(
            &app,
            &state,
            &file_path,
            is_surface_batch,
            target_type.clone(), // NEW
            warping_analysis,
            bayer_override,
            anchor_override.clone(),
            progress_prefix,
            // El lote respeta el mismo selector GPU de Ajustes que el flujo
            // individual (antes forzaba Auto e ignoraba la elección).
            ComputePolicy::from_legacy(gpu_mode.as_deref()),
        )
        .await?;
        cached_opt = load_cached_analysis(&cache_path);
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

    // 2. El motor borra el cache temporal de frames al terminar: el lector
    //    local debe cerrarse antes y no debe usarse despues.
    drop(r);

    let ap_size_px = ap_grid_size.unwrap_or(32).max(8);
    let _engine_preview = stack_video_liquid_warping_impl(
        &app,
        &state,
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
        gpu_mode.clone(), // GPU compute: mismo select de Ajustes que el flujo individual
    )
    .await?;

    // 3. Recoger el resultado del motor (y liberar el slot compartido).
    let engine_stack = state
        .stacked_image
        .lock()
        .unwrap()
        .take()
        .ok_or("El motor Zenith no produjo resultado")?;
    let stacked_data = engine_stack.data;
    let out_w = engine_stack.width;
    let out_h = engine_stack.height;

    // El analisis in-band pudo mover active_req_id; el pipeline de filtros
    // del batch compara contra 0 — re-sincronizar para no auto-cancelarse.
    state.active_req_id.store(0, Ordering::Relaxed);

    // CONSISTENCIA CON EL FIX DE DISCO GRANDE (single-file): una Luna / fase
    // lunar grande se estabiliza por TEXTURA (SAD contra anchor), igual que
    // Superficie. El re-centrado CoG planetario haria que el disco "baile" entre
    // frames del timelapse al moverse el centroide con la fase. Los planetas
    // pequenos (blob diminuto) conservan intacto su re-centrado CoG.
    let large_disc_batch = if is_surface_batch {
        false
    } else {
        let mono: Vec<u16> = stacked_data
            .chunks_exact(3)
            .map(|p| ((p[0] as u32 + p[1] as u32 + p[2] as u32) / 3) as u16)
            .collect();
        is_large_lunar_disc(&mono, out_w, out_h)
    };

    // --- RECORTE Y ESTABILIZACION SOLAR (Logica Mantenida) ---
    let (centered_data, cw, ch) = if is_surface_batch || large_disc_batch {
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

        // Estabilizacion Solar usando el Anchor
        let mut anchor_guard = state.batch_anchor.lock().unwrap();
        let mut dims_guard = state.batch_anchor_dims.lock().unwrap();

        if anchor_guard.is_none() {
            *anchor_guard = Some(solar_out.clone());
            *dims_guard = (safe_w, safe_h);
            (solar_out, safe_w, safe_h)
        } else {
            let (aw, ah) = *dims_guard;
            // R13: el auto-crop del motor Zenith puede variar las dimensiones
            // unos pixeles entre archivos. Ajustamos por recorte/padding
            // centrado a las dims del anchor en vez del reset silencioso
            // anterior (que rompia la estabilizacion del timelapse).
            let solar_out = if aw == safe_w && ah == safe_h {
                solar_out
            } else {
                center_crop_or_pad_rgb(&solar_out, safe_w, safe_h, aw, ah)
            };
            let (safe_w, safe_h) = (aw, ah);
            {
                // Alineamos este resultado final contra el anchor usando el mismo SAD

                // --- BLOCK 1: Calculo de Desplazamiento (Immutable Borrow) ---
                let (dx, dy) = {
                    let anchor = anchor_guard.as_ref().unwrap();
                    let to_mono = |d: &[u16]| -> Vec<u16> {
                        let mut m = Vec::with_capacity(safe_w * safe_h);
                        for i in 0..(d.len() / 3) {
                            let off = i * 3;
                            // Usamos promedio simple para velocidad
                            m.push(
                                ((d[off] as u32 + d[off + 1] as u32 + d[off + 2] as u32) / 3)
                                    as u16,
                            );
                        }
                        m
                    };
                    let anchor_mono = to_mono(anchor);
                    let current_mono = to_mono(&solar_out);

                    // PYRAMID FOR STABILIZATION
                    let anchor_pyramid = downscale_integer(&anchor_mono, safe_w, safe_h, 4);

                    // Usamos un ROI central grande (85%) para capturar detalles solares
                    let roi_w = (safe_w as f32 * 0.85) as usize;
                    let roi_h = (safe_h as f32 * 0.85) as usize;
                    let stab_x = safe_w.saturating_sub(roi_w) / 2;
                    let stab_y = safe_h.saturating_sub(roi_h) / 2;

                    find_best_match_sad_pyramid(
                        &anchor_mono,
                        &current_mono,
                        &anchor_pyramid,
                        safe_w,
                        safe_h,
                        safe_w / 4,
                        safe_h / 4,
                        stab_x,
                        stab_y,
                        roi_w,
                        roi_h,
                        120,
                        4, // Scale Factor (Standard 4x)
                        4, // Fine Range (Standard 4px)
                    )
                };

                // --- BLOCK 2: Aplicar Shift y Actualizar Anchor (Mutable Borrow) ---
                let mut shifted = vec![0u16; solar_out.len()];
                for y in 0..safe_h {
                    for x in 0..safe_w {
                        let sx = x as f32 + dx;
                        let sy = y as f32 + dy;
                        if sx >= 0.0
                            && sx < (safe_w - 1) as f32
                            && sy >= 0.0
                            && sy < (safe_h - 1) as f32
                        {
                            let x0 = sx.floor() as usize;
                            let x1 = x0 + 1;
                            let y0 = sy.floor() as usize;
                            let y1 = y0 + 1;
                            let wx = sx - x0 as f32;
                            let wy = sy - y0 as f32;
                            let idx_dst = (y * safe_w + x) * 3;
                            for c in 0..3 {
                                let v00 = solar_out[(y0 * safe_w + x0) * 3 + c] as f32;
                                let v10 = solar_out[(y0 * safe_w + x1) * 3 + c] as f32;
                                let v01 = solar_out[(y1 * safe_w + x0) * 3 + c] as f32;
                                let v11 = solar_out[(y1 * safe_w + x1) * 3 + c] as f32;
                                shifted[idx_dst + c] = ((v00 * (1.0 - wx) + v10 * wx) * (1.0 - wy)
                                    + (v01 * (1.0 - wx) + v11 * wx) * wy)
                                    as u16;
                            }
                        }
                    }
                }

                if is_surface_batch || large_disc_batch {
                    let anchor_mut = anchor_guard.as_mut().unwrap();
                    // FIX: Reducir factor de actualizacion (Drift) del 5% al 0.5% para super-estabilidad
                    // Esto evita que el ancla "persiga" la turbulencia
                    for i in 0..anchor_mut.len() {
                        let old = anchor_mut[i] as f32;
                        let new = shifted[i] as f32;
                        anchor_mut[i] = (old * 0.995 + new * 0.005) as u16;
                    }
                }

                (shifted, safe_w, safe_h)
            }
        }
    } else {
        // R13: RE-CENTRADO CoG PLANETARIO (timelapse estable)
        // Cada stack se desplaza para que el centro de gravedad del disco
        // caiga en el centro del lienzo — posicion identica entre archivos
        // del lote. Dimensiones compartidas via batch_anchor_dims para que
        // todos los PNG salgan del mismo tamano aunque el auto-crop varie.
        let (tw, th) = {
            let mut dims_guard = state.batch_anchor_dims.lock().unwrap();
            if dims_guard.0 == 0 || dims_guard.1 == 0 {
                *dims_guard = (out_w, out_h);
            }
            *dims_guard
        };

        let base = if tw == out_w && th == out_h {
            stacked_data.clone()
        } else {
            center_crop_or_pad_rgb(&stacked_data, out_w, out_h, tw, th)
        };

        let mono: Vec<u16> = base
            .chunks_exact(3)
            .map(|p| ((p[0] as u32 + p[1] as u32 + p[2] as u32) / 3) as u16)
            .collect();
        let peak = mono.iter().copied().max().unwrap_or(2000) as f32;
        let cog_threshold = (peak * 0.15).max(512.0) as u16;
        let out = match crate::alignment::calculate_center_of_gravity(&mono, tw, th, cog_threshold)
        {
            Some((cx, cy)) => {
                // Muestreo: src = dst + (dx, dy) — misma convencion que surface
                let dx = cx - tw as f32 / 2.0;
                let dy = cy - th as f32 / 2.0;
                let mut shifted = vec![0u16; base.len()];
                for y in 0..th {
                    for x in 0..tw {
                        let sx = x as f32 + dx;
                        let sy = y as f32 + dy;
                        if sx >= 0.0
                            && sx < (tw - 1) as f32
                            && sy >= 0.0
                            && sy < (th - 1) as f32
                        {
                            let x0 = sx.floor() as usize;
                            let x1 = x0 + 1;
                            let y0 = sy.floor() as usize;
                            let y1 = y0 + 1;
                            let wx = sx - x0 as f32;
                            let wy = sy - y0 as f32;
                            let idx_dst = (y * tw + x) * 3;
                            for c in 0..3 {
                                let v00 = base[(y0 * tw + x0) * 3 + c] as f32;
                                let v10 = base[(y0 * tw + x1) * 3 + c] as f32;
                                let v01 = base[(y1 * tw + x0) * 3 + c] as f32;
                                let v11 = base[(y1 * tw + x1) * 3 + c] as f32;
                                shifted[idx_dst + c] = ((v00 * (1.0 - wx) + v10 * wx)
                                    * (1.0 - wy)
                                    + (v01 * (1.0 - wx) + v11 * wx) * wy)
                                    as u16;
                            }
                        }
                    }
                }
                shifted
            }
            None => base,
        };
        (out, tw, th)
    };

    // --- R13: la salida del motor Zenith YA incluye rechazo de outliers
    // (planetas), balance de blancos sin clipping, normalizacion de rango y
    // sharpening opcional. Los antiguos bloques "parity" duplicaban WB y
    // sharpening (doble aplicacion) y dependian del master legacy: eliminados.
    let pre_processed = centered_data;
    let (safe_w, safe_h) = (cw, ch);
    let usm_amount = if is_surface_batch { 0.0 } else { _usm_amount.max(0.0) };


    let temp_res = StackResult {
        data: pre_processed, // NOW using the enhanced base
        width: safe_w,
        height: safe_h,
        is_mono: cid == 0 || cid == 12,
        is_surface: is_surface_batch,
    };

    // Apply deconvolution parameters from reference image configuration
    let (use_d_iter, use_d_sigma, use_v_iter, use_v_sigma) =
        (deconv_iter, deconv_sigma, vc_iter, vc_sigma);

    // PHASE 36 FIX: Improved detection for Surface/Solar (including v2 variations)
    // "Unbeatable" preset: Sharpen 0.5 + LCE 15.0
    // Logic matching stack_video_liquid_warping
    let (usm_amount, usm_radius, lce_amount) =
        if is_surface_batch {
            let (u_amt, u_rad) = if usm_amount <= 0.01 {
                (0.5, 1.5)
            } else {
                (usm_amount, usm_radius)
            };
            let l_amt = if lce_amount <= 0.01 { 15.0 } else { lce_amount };
            (u_amt, u_rad, l_amt)
        } else {
            (usm_amount, usm_radius, lce_amount)
        };

    // FIX: Force Neutral WB for Mono Purity
    let is_mono = cid == 0 || cid == 12;
    let (r_bal, b_bal) = if is_mono { (0.0, 0.0) } else { (r_bal, b_bal) };

    // Batch: la descomposición GPU interactiva no aplica aquí (el apilado ya usa
    // su propio motor GPU de acumulación); el post se re-aplica en CPU.
    let gpu_allowed = false;
    let processed = run_processing_pipeline(
        &app,
        &state,
        0,
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
        usm_amount,
        usm_radius,
        lce_amount,
        blend,
        contrast,
        brightness,
        r_bal,
        b_bal,
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
    );

    // --- FINAL EXPORT: High Quality 16-bit PNG (Matches save_final_image) ---
    // Usamos el codificador de 16 bits para preservar todo el procesado del pipeline.
    let mut raw_bytes_be = Vec::with_capacity(processed.len() * 2);
    for v in &processed {
        raw_bytes_be.extend_from_slice(&v.to_be_bytes()); // PNG standard (Big Endian)
    }

    let f = File::create(&save_path).map_err(|e| e.to_string())?;
    let ref_writer = BufWriter::new(f);
    let encoder = image::codecs::png::PngEncoder::new(ref_writer);

    encoder
        .encode(
            &raw_bytes_be,
            safe_w as u32,
            safe_h as u32,
            image::ColorType::Rgb16,
        )
        .map_err(|e| e.to_string())?;

    // ASSET PROTOCOL: la UI (batch y mosaico) carga `path` con convertFileSrc,
    // asi que ya no se genera el preview base64 por entrada. Antes cada archivo
    // del lote pagaba un PNG 8-bit + base64 (CPU) y megabytes de IPC, y el
    // frontend RETENIA todos esos strings en RAM para el reproductor — en lotes
    // grandes empujaba el heap del WebView a >1 GB (crash del renderer).
    Ok(BatchEntryResult {
        path: clean_windows_path(dunce::canonicalize(&save_path).unwrap_or(save_path)),
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
    pub raw_u16: Vec<u16>,   // Reuse for raw_to_u16 conversion (full res)
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
fn enhance_and_score_surface_buffered(
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

    // 3. Normalization (Needed for SAD Alignment consistency)
    let mut min_val = 65535;
    let mut max_val = 0;

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

    emit_progress(&app, "Recortando...", 0.0, None);

    let (new_data, new_w, new_h) = {
        let mut guard = state.stacked_image.lock().unwrap();
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

        *guard = Some(StackResult {
            data: cropped.clone(),
            width: w,
            height: h,
            is_mono: img.is_mono,
            is_surface: img.is_surface,
        });
        (cropped, w, h)
    };

    {
        state.deconv_cache.lock().unwrap().clear();
        state.wavelet_cache.lock().unwrap().clear();
        state.filter_cache.lock().unwrap().clear();
    }

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
    let cid = bayer_override.unwrap_or_else(|| reader.color_id());

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
    let is_color = reader.is_color() || ser::ser_color_is_color(cid);
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
        is_color: reader.is_color(),
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
    let cid = bayer_override.unwrap_or_else(|| reader.color_id());

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
    // GENERIC REQ_ID (Timestamp based)
    let req_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as usize;
    state.active_req_id.store(req_id, Ordering::Relaxed);

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
                let final_is_color = reader.is_color() || ser::ser_color_is_color(cid);

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

    let final_is_color = reader.is_color() || ser::ser_color_is_color(cid);

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

    // 1. EXTRACT LUMINANCE
    let mut lum = vec![0.0f32; npix];
    for i in 0..npix {
        let r = buffer[i * 3] as f32;
        let g = buffer[i * 3 + 1] as f32;
        let b = buffer[i * 3 + 2] as f32;
        lum[i] = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    }

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
    // Dynamic soft coring: as intensity increases, we also increase the noise threshold
    // to prevent amplification of tiny artifacts at high sharpening levels.
    let base_threshold = (noise_sigma * (2.5 + intensity)).max(120.0 * intensity.sqrt());
    let signal_threshold = median_bg + noise_sigma * 2.0;

    // 3. WAVELET DECOMPOSITION (luminance only)
    let b1_blur = apply_gaussian_blur_f32(&lum, width, height, 1.0);
    let b2_blur = apply_gaussian_blur_f32(&b1_blur, width, height, 2.0);
    let b3_blur = apply_gaussian_blur_f32(&b2_blur, width, height, 4.0);
    let b4_blur = apply_gaussian_blur_f32(&b3_blur, width, height, 8.0);
    let b5_blur = apply_gaussian_blur_f32(&b4_blur, width, height, 16.0);

    let band1: Vec<f32> = lum.iter().zip(b1_blur.iter()).map(|(a, b)| a - b).collect();
    let band2: Vec<f32> = b1_blur.iter().zip(b2_blur.iter()).map(|(a, b)| a - b).collect();
    let band3: Vec<f32> = b2_blur.iter().zip(b3_blur.iter()).map(|(a, b)| a - b).collect();
    let band4: Vec<f32> = b3_blur.iter().zip(b4_blur.iter()).map(|(a, b)| a - b).collect();
    let band5: Vec<f32> = b4_blur.iter().zip(b5_blur.iter()).map(|(a, b)| a - b).collect();

    // 4. SHARPEN LUMINANCE with adaptive soft coring + SNR masking
    let mut sharp_lum = vec![0.0f32; npix];
    for i in 0..npix {
        let orig_l = lum[i];

        // SNR mask: don't sharpen background
        let snr_weight = if orig_l < signal_threshold {
            0.0
        } else {
            ((orig_l - signal_threshold) / (noise_sigma * 5.0 + 1.0)).clamp(0.0, 1.0)
        };

        if snr_weight < 0.01 {
            sharp_lum[i] = orig_l;
            continue;
        }

        // Soft coring: only amplify coefficients above noise floor
        let core = |coeff: f32, thresh: f32| -> f32 {
            let ac = coeff.abs();
            if ac < thresh { 0.0 } else { (ac - thresh) * coeff.signum() }
        };

        let d1 = core(band1[i], base_threshold * 1.2) * s1_amp;
        let d2 = core(band2[i], base_threshold * 0.8) * s2_amp;
        let d3 = core(band3[i], base_threshold * 0.5) * s3_amp;
        let d4 = core(band4[i], base_threshold * 0.3) * s4_amp;
        let d5 = core(band5[i], base_threshold * 0.2) * s5_amp;

        let total = (d1 + d2 + d3 + d4 + d5) * snr_weight;
        sharp_lum[i] = (orig_l + total).clamp(0.0, 65535.0);
    }

    // 5. APPLY back to RGB preserving chrominance (zero chromatic noise)
    for i in 0..npix {
        let orig_l = lum[i];
        let new_l = sharp_lum[i];
        if orig_l < 1.0 {
            let v = new_l.clamp(0.0, 65535.0) as u16;
            buffer[i * 3] = v;
            buffer[i * 3 + 1] = v;
            buffer[i * 3 + 2] = v;
        } else {
            let ratio = new_l / orig_l;
            buffer[i * 3]     = (buffer[i * 3] as f32 * ratio).clamp(0.0, 65535.0) as u16;
            buffer[i * 3 + 1] = (buffer[i * 3 + 1] as f32 * ratio).clamp(0.0, 65535.0) as u16;
            buffer[i * 3 + 2] = (buffer[i * 3 + 2] as f32 * ratio).clamp(0.0, 65535.0) as u16;
        }
    }
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

    let mut tmp = vec![0.0f32; w * h];
    let mut out = vec![0.0f32; w * h];

    // Horizontal pass
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            let mut acc = 0.0f32;
            for (k, &kv) in kernel.iter().enumerate() {
                let xx = (x as isize + k as isize - radius as isize).clamp(0, w as isize - 1) as usize;
                acc += data[row + xx] * kv;
            }
            tmp[row + x] = acc;
        }
    }
    // Vertical pass
    for x in 0..w {
        for y in 0..h {
            let mut acc = 0.0f32;
            for (k, &kv) in kernel.iter().enumerate() {
                let yy = (y as isize + k as isize - radius as isize).clamp(0, h as isize - 1) as usize;
                acc += tmp[yy * w + x] * kv;
            }
            out[y * w + x] = acc;
        }
    }
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
        None, // align_rgb: el legacy usa el default (activado, con gates de seguridad)
        gpu_mode,
    )
    .await;
}

/// Downsample de un buffer RGB16 entrelazado por un factor entero (2/4) con
/// promedio de bloque (area). Para el preview rapido en vivo: procesar a 1/N de
/// resolucion abarata la deconvolucion/wavelets ~N². Devuelve (data, w', h').
fn downsample_rgb_u16(data: &[u16], w: usize, h: usize, factor: usize) -> (Vec<u16>, usize, usize) {
    let sw = w / factor;
    let sh = h / factor;
    let mut out = vec![0u16; sw * sh * 3];
    let n = (factor * factor) as u32;
    out.par_chunks_exact_mut(sw * 3)
        .enumerate()
        .for_each(|(ty, row)| {
            for tx in 0..sw {
                let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
                for dy in 0..factor {
                    let sy = ty * factor + dy;
                    let base = (sy * w + tx * factor) * 3;
                    for dx in 0..factor {
                        let p = base + dx * 3;
                        r += data[p] as u32;
                        g += data[p + 1] as u32;
                        b += data[p + 2] as u32;
                    }
                }
                let o = tx * 3;
                row[o] = (r / n) as u16;
                row[o + 1] = (g / n) as u16;
                row[o + 2] = (b / n) as u16;
            }
        });
    (out, sw, sh)
}

/// Re-escala (nearest) un buffer RGB8 al tamaño destino. Se usa para devolver el
/// preview downscaled a dimensiones COMPLETAS → el pan/zoom del visor no salta
/// entre el preview rapido (arrastre) y el render final (al soltar).
fn upscale_rgb8_nearest(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut out = vec![0u8; dw * dh * 3];
    out.par_chunks_exact_mut(dw * 3)
        .enumerate()
        .for_each(|(y, row)| {
            let sy = (y * sh / dh).min(sh.saturating_sub(1));
            for x in 0..dw {
                let sx = (x * sw / dw).min(sw.saturating_sub(1));
                let sp = (sy * sw + sx) * 3;
                let o = x * 3;
                row[o] = src[sp];
                row[o + 1] = src[sp + 1];
                row[o + 2] = src[sp + 2];
            }
        });
    out
}

#[tauri::command]
async fn apply_wavelets(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    req_id: usize,
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
    preview_downscale: Option<u32>,    // Interactividad: 2/4 = preview rapido en arrastre
    gpu_mode: Option<String>,          // Velocidad: "auto"|"gpu"|"cpu" (descomposicion GPU)
    levels_black: Option<f32>,         // Niveles: punto negro (0..1)
    levels_white: Option<f32>,         // Niveles: punto blanco (0..1)
    levels_gamma: Option<f32>,         // Niveles: gamma medios (0.1..5)
) -> Result<String, String> {
    state.license_manager.check_access()?;
    state.active_req_id.store(req_id, Ordering::Relaxed);
    let original = {
        let s = state.stacked_image.lock().unwrap();
        match &*s {
            Some(img) => img.clone(),
            None => return Err("Sin imagen".into()),
        }
    };

    // FIX: Apply Smart Defaults for Single Stacking View if unset (Unbeatable Surface)
    let (usm_amount, usm_radius, lce_amount) = if original.is_mono && original.is_surface {
        let (u_amt, u_rad) = if usm_amount <= 0.01 {
            (0.5, 1.5)
        } else {
            (usm_amount, usm_radius)
        };
        let l_amt = if lce_amount <= 0.01 { 15.0 } else { lce_amount };
        (u_amt, u_rad, l_amt)
    } else {
        (usm_amount, usm_radius, lce_amount)
    };

    // FIX: Force Neutral WB for Mono Purity. 0.0 is neutral.
    let (r_bal, b_bal) = if original.is_mono {
        (0.0, 0.0)
    } else {
        (r_bal, b_bal)
    };

    // PREVIEW EN VIVO (interactividad): durante el arrastre de sliders el front
    // pide un downscale (2/4). Procesamos a 1/N de resolucion (deconv/wavelets
    // ~N² mas rapidos) y luego re-escalamos el resultado a dimensiones COMPLETAS
    // (abajo) para que el visor (pan/zoom) no salte. Stats globales/limbo/norma-
    // lizacion siguen coherentes → el render final al soltar (downscale=1) es exacto.
    let ds = preview_downscale.unwrap_or(1).max(1) as usize;
    let use_ds = ds > 1 && original.width >= 256 * ds && original.height >= 256 * ds;
    let ds_holder;
    let (proc_ref, pw, ph): (&StackResult, usize, usize) = if use_ds {
        let (small, sw, sh) =
            downsample_rgb_u16(&original.data, original.width, original.height, ds);
        ds_holder = StackResult {
            data: small,
            width: sw,
            height: sh,
            is_mono: original.is_mono,
            is_surface: original.is_surface,
        };
        (&ds_holder, sw, sh)
    } else {
        (&original, original.width, original.height)
    };

    // GPU wavelets: permitido salvo modo "cpu"; run_processing_pipeline aún exige
    // tamaño mínimo + paridad, y cae a CPU ante cualquier problema.
    let gpu_allowed = gpu_mode.as_deref().map(|m| m != "cpu").unwrap_or(true)
        && crate::gpu_stack::gpu_runtime().is_some();

    let final_u16 = run_processing_pipeline(
        &app,
        &state,
        req_id,
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
        usm_amount,
        usm_radius,
        lce_amount,
        blend,
        contrast,
        brightness,
        r_bal,
        b_bal,
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
    );

    if final_u16.is_empty() {
        return Err("Cancelled".into());
    }

    emit_progress(&app, "Generando vista...", 97.0, None);
    let vis_small = to_8bit_visual(&final_u16, 1.0);
    // Re-escalar el preview downscaled a dimensiones completas (visor estable).
    let vis = if use_ds {
        upscale_rgb8_nearest(&vis_small, pw, ph, original.width, original.height)
    } else {
        vis_small
    };
    let mut png = Vec::new();
    image::png::PngEncoder::new(&mut Cursor::new(&mut png))
        .encode(
            &vis,
            original.width as u32,
            original.height as u32,
            image::ColorType::Rgb8,
        )
        .map_err(|e| e.to_string())?;
    emit_progress(&app, "Listo", 100.0, None);
    Ok(format!(
        "data:image/png;base64,{}",
        general_purpose::STANDARD.encode(&png)
    ))
}

#[tauri::command]
async fn analyze_psf(state: State<'_, AppState>) -> Result<PsfResult, String> {
    state.license_manager.check_access()?;
    let original = {
        let s = state.stacked_image.lock().unwrap();
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
    let iterations = if sigma > 1.8 {
        3
    } else if sigma > 1.1 {
        2
    } else {
        1
    };

    Ok(PsfResult {
        sigma: (sigma * 10.0).round() / 10.0,
        iterations,
        msg: format!("Sigma Calc: {:.2}px | Iteraciones conservadoras: {}", sigma, iterations),
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
    _deringing_mask: bool,
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
    levels_black: Option<f32>,         // Niveles: punto negro (0..1)
    levels_white: Option<f32>,         // Niveles: punto blanco (0..1)
    levels_gamma: Option<f32>,         // Niveles: gamma medios (0.1..5)
) -> Result<String, String> {
    state.license_manager.check_access()?;
    state.active_req_id.store(0, Ordering::Relaxed);
    if format_idx == 1 && !state.license_manager.is_pro() {
        return Err(
            "Guardar en TIFF 16-bit requiere licencia PRO o periodo de prueba activo.".into(),
        );
    }

    let original = {
        let s = state.stacked_image.lock().unwrap();
        match &*s {
            Some(img) => img.clone(),
            None => return Err("Sin imagen".into()),
        }
    };
    emit_progress(&app, "Procesando final...", 0.0, None);

    // Export: siempre CPU (render final exacto, sin dependencia de GPU).
    let gpu_allowed = false;
    let final_u16 = run_processing_pipeline(
        &app,
        &state,
        0,
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
        false, // deringing_mask (Force OFF for export)
        crisp,
        deconv_iter,
        deconv_sigma,
        vc_iter,
        vc_sigma,
        usm_amount,
        usm_radius,
        lce_amount,
        blend,
        contrast,
        brightness,
        r_bal,
        b_bal,
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
    );

    if final_u16.is_empty() {
        return Err("Error en el procesado (Cancelado)".into());
    }

    emit_progress(&app, "Guardando...", 97.0, None);

    if format_idx == 0 {
        // PNG 16-BIT EXPORT (Requested: "tal cual" 16-bit preservation)
        let name = format!("{}_Final.png", path);
        // PNG standard requires Big Endian for 16-bit
        let mut raw_bytes_be = Vec::with_capacity(final_u16.len() * 2);
        for v in &final_u16 {
            raw_bytes_be.extend_from_slice(&v.to_be_bytes());
        }

        let f = File::create(&name).map_err(|e| e.to_string())?;
        let ref_writer = BufWriter::new(f);
        let encoder = image::codecs::png::PngEncoder::new(ref_writer);

        encoder
            .encode(
                &raw_bytes_be,
                original.width as u32,
                original.height as u32,
                image::ColorType::Rgb16,
            )
            .map_err(|e| e.to_string())?;

        emit_progress(&app, "Listo", 100.0, None);
        return Ok(format!("PNG 16-bit Guardado: {}", name));
    } else {
        let name = format!("{}_Final_16bit.tiff", path);
        let f = File::create(&name).map_err(|e| e.to_string())?;
        let ref_writer = BufWriter::new(f);
        let encoder = image::codecs::tiff::TiffEncoder::new(ref_writer);
        let mut raw_bytes = Vec::with_capacity(final_u16.len() * 2);
        for v in &final_u16 {
            raw_bytes.extend_from_slice(&v.to_ne_bytes());
        }
        encoder
            .encode(
                &raw_bytes,
                original.width as u32,
                original.height as u32,
                image::ColorType::Rgb16,
            )
            .map_err(|e| e.to_string())?;
        emit_progress(&app, "Listo", 100.0, None);
        return Ok(format!("TIFF 16-bit Guardado: {}", name));
    }
}

#[tauri::command]
async fn export_mosaic_result(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    source_path: String,
    format_idx: i32,
) -> Result<String, String> {
    state.license_manager.check_access()?;
    if format_idx == 1 && !state.license_manager.is_pro() {
        return Err(
            "Guardar en TIFF 16-bit requiere licencia PRO o periodo de prueba activo.".into(),
        );
    }

    let original = {
        let s = state.stacked_image.lock().unwrap();
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

    if format_idx == 0 {
        let out_path = parent_dir.join(format!("{}_Export_16bit.png", stem));
        let mut raw_bytes_be = Vec::with_capacity(original.data.len() * 2);
        for v in &original.data {
            raw_bytes_be.extend_from_slice(&v.to_be_bytes());
        }

        let f = File::create(&out_path).map_err(|e| e.to_string())?;
        let ref_writer = BufWriter::new(f);
        let encoder = image::codecs::png::PngEncoder::new(ref_writer);
        encoder
            .encode(
                &raw_bytes_be,
                original.width as u32,
                original.height as u32,
                image::ColorType::Rgb16,
            )
            .map_err(|e| e.to_string())?;

        emit_progress(&app, "Listo", 100.0, None);
        Ok(format!(
            "PNG 16-bit guardado: {}",
            clean_windows_path(out_path)
        ))
    } else {
        let out_path = parent_dir.join(format!("{}_Export_16bit.tiff", stem));
        let mut raw_bytes = Vec::with_capacity(original.data.len() * 2);
        for v in &original.data {
            raw_bytes.extend_from_slice(&v.to_ne_bytes());
        }

        let f = File::create(&out_path).map_err(|e| e.to_string())?;
        let ref_writer = BufWriter::new(f);
        let encoder = image::codecs::tiff::TiffEncoder::new(ref_writer);
        encoder
            .encode(
                &raw_bytes,
                original.width as u32,
                original.height as u32,
                image::ColorType::Rgb16,
            )
            .map_err(|e| e.to_string())?;

        emit_progress(&app, "Listo", 100.0, None);
        Ok(format!(
            "TIFF 16-bit guardado: {}",
            clean_windows_path(out_path)
        ))
    }
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
        let mut d = 0;
        // Assume same length
        for (b1, b2) in self.descriptor.iter().zip(other.descriptor.iter()) {
            d += (b1 ^ b2).count_ones();
        }
        d
    }
}

struct ImageNode {
    idx: usize,
    width: u32,
    height: u32,
    global_x: i32,
    global_y: i32,
    placed: bool,
    img_gray: image::GrayImage,
    img_gray_stretched: image::GrayImage,
    features: Vec<FeaturePoint>,
    original_path: String, // Store path to reload in 16-bit for final fusion
}

#[derive(Clone, Copy, Debug)]
struct MatchEdge {
    target_idx: usize,
    dx: i32,
    dy: i32,
    score: usize,
}

// === AKAZE HELPERS (No explicit helpers needed, using crate directly) ===

// NUEVO: Helper robusto para cargar imagen o frame de video
// NUEVO: Helper robusto para cargar y SELECCIONAR el mejor frame (logica Surface Mode)
fn load_image_or_video_frame(
    path_str: &str,
    app: &tauri::AppHandle,
) -> Result<DynamicImage, String> {
    let path = Path::new(path_str);

    // 1. Intentar imagen directa
    if let Ok(img) = image::open(path) {
        return Ok(img);
    }

    // 2. Intentar como Video con Seleccion Inteligente (Surface Mode Logic)
    match VideoInput::open(path_str, app) {
        Ok(reader) => {
            let total = reader.frame_count();
            if total == 0 {
                return Err(format!("El video {} no tiene frames", path_str));
            }

            let w = reader.width();
            let h = reader.height();
            let bpp = reader.bpp();
            let cid = reader.color_id();

            // SURFACE MODE LOGIC: Safe ROI for quality check (Center 50%)
            // Avoids edge artifacts or black borders affecting score
            let safe_roi = Rect {
                x: w / 4,
                y: h / 4,
                w: w / 2,
                h: h / 2,
            };

            // Scan Strategy: Check up to 20 frames distributed across the video
            // to find the sharpest/best one without reading the whole file.
            let step = (total / 20).max(1);
            let mut best_score = 0;
            let mut best_idx = 0;

            // First pass: Rapid scan
            for i in (0..total).step_by(step) {
                let raw = reader.get_frame(i, cid);
                if raw.is_empty() {
                    continue;
                }

                // Calculate score using Surface logic
                let score = calculate_quality_metric(&raw, w, h, bpp, &safe_roi);

                if score > best_score {
                    best_score = score;
                    best_idx = i;
                }
            }

            // Fallback: If score is 0 (broken video?), take middle frame
            if best_score == 0 {
                best_idx = total / 2;
            }

            // Load the Winner Frame
            let raw = reader.get_frame(best_idx, cid);
            let u16s = raw_to_u16_buffer(&raw, w, h, bpp);
            let mut rgb = debayer_to_rgb(&u16s, w, h, cid);

            if cid >= 8 && cid <= 11 {
                auto_color_balance(&mut rgb, w, h);
            }

            let vis = to_8bit_visual(&rgb, 1.0);

            match image::RgbImage::from_raw(w as u32, h as u32, vis) {
                Some(buf) => Ok(DynamicImage::ImageRgb8(buf)),
                None => Err(format!("Error buffer visual: {}", path_str)),
            }
        }
        Err(e) => Err(format!("Error cargando {}: {}", path_str, e)),
    }
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

fn derot_save_rgb16_tiff(path: &Path, rgb: &[u16], width: usize, height: usize) -> Result<(), String> {
    let mut raw_bytes = Vec::with_capacity(rgb.len() * 2);
    for v in rgb {
        raw_bytes.extend_from_slice(&v.to_ne_bytes());
    }

    let file = File::create(path).map_err(|e| e.to_string())?;
    let writer = BufWriter::new(file);
    image::codecs::tiff::TiffEncoder::new(writer)
        .encode(&raw_bytes, width as u32, height as u32, image::ColorType::Rgb16)
        .map_err(|e| e.to_string())
}

fn derot_default_output_path(source_path: &str) -> PathBuf {
    let source = Path::new(source_path);
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Planetary_Derotation");
    let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    parent.join(format!("{}_Derotated_{}.tiff", stem, stamp))
}

fn derot_output_path_from_optional_source(source_path: Option<&str>, fallback_name: &str) -> PathBuf {
    if let Some(path) = source_path.filter(|p| !p.trim().is_empty()) {
        derot_default_output_path(path)
    } else {
        let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(format!("{}_Derotated_{}.tiff", fallback_name, stamp))
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
        let guard = state.stacked_image.lock().unwrap();
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
    let planet_body = crate::derotation::get_planet(&planet)
        .ok_or_else(|| "Planeta no soportado para derotacion".to_string())?;
    let capture_jd = crate::derotation::parse_iso_to_jd(&capture_time)
        .map_err(|e| format!("Tiempo de captura invalido: {}", e))?;
    let reference_jd = crate::derotation::parse_iso_to_jd(&reference_time)
        .map_err(|e| format!("Tiempo de referencia invalido: {}", e))?;

    emit_progress(&app, "Cargando imagen para derotacion...", 5.0, None);
    let (rgb, width, height) = derot_load_rgb16_image(&image_path)?;
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

    emit_progress(
        &app,
        "Aplicando proyeccion cilindrica y derotacion...",
        35.0,
        Some(format!("Delta {:.3} grados", delta_deg)),
    );

    let planet_for_block = planet.clone();
    let disc_for_block = disc.clone();
    let derotated = tauri::async_runtime::spawn_blocking(move || {
        let planet_body = crate::derotation::get_planet(&planet_for_block)
            .ok_or_else(|| "Planeta no soportado para derotacion".to_string())?;
        Ok::<Vec<u16>, String>(crate::derotation::derotate_single_advanced(
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
        ))
    })
    .await
    .map_err(|e| e.to_string())??;

    let out_path = derot_default_output_path(&image_path);
    emit_progress(&app, "Guardando TIFF derotado 16-bit...", 82.0, None);
    derot_save_rgb16_tiff(&out_path, &derotated, width, height)?;

    {
        let mut stacked = state.stacked_image.lock().unwrap();
        *stacked = Some(StackResult {
            data: derotated.clone(),
            width,
            height,
            is_mono: false,
            is_surface: false,
        });
    }
    state.deconv_cache.lock().unwrap().clear();
    state.wavelet_cache.lock().unwrap().clear();
    state.filter_cache.lock().unwrap().clear();

    let preview_base64 = derot_encode_preview(&derotated, width, height)?;
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
    let planet_body = crate::derotation::get_planet(&planet)
        .ok_or_else(|| "Planeta no soportado para derotacion".to_string())?;
    let capture_jd = crate::derotation::parse_iso_to_jd(&capture_time)
        .map_err(|e| format!("Tiempo de captura invalido: {}", e))?;
    let reference_jd = crate::derotation::parse_iso_to_jd(&reference_time)
        .map_err(|e| format!("Tiempo de referencia invalido: {}", e))?;

    let stack = {
        let guard = state.stacked_image.lock().unwrap();
        guard
            .clone()
            .ok_or_else(|| "No hay una imagen apilada activa para derotar.".to_string())?
    };
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

    emit_progress(
        &app,
        "Derotando resultado actual...",
        35.0,
        Some(format!("Delta {:.3} grados", delta_deg)),
    );
    let planet_for_block = planet.clone();
    let disc_for_block = disc.clone();
    let derotated = tauri::async_runtime::spawn_blocking(move || {
        let planet_body = crate::derotation::get_planet(&planet_for_block)
            .ok_or_else(|| "Planeta no soportado para derotacion".to_string())?;
        Ok::<Vec<u16>, String>(crate::derotation::derotate_single_advanced(
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
        ))
    })
    .await
    .map_err(|e| e.to_string())??;

    let out_path = derot_output_path_from_optional_source(source_path.as_deref(), "Current_Stack");
    emit_progress(&app, "Guardando TIFF derotado 16-bit...", 82.0, None);
    derot_save_rgb16_tiff(&out_path, &derotated, width, height)?;

    {
        let mut stacked = state.stacked_image.lock().unwrap();
        *stacked = Some(StackResult {
            data: derotated.clone(),
            width,
            height,
            is_mono: false,
            is_surface: false,
        });
    }
    state.deconv_cache.lock().unwrap().clear();
    state.wavelet_cache.lock().unwrap().clear();
    state.filter_cache.lock().unwrap().clear();

    let preview_base64 = derot_encode_preview(&derotated, width, height)?;
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
    state.active_req_id.store(1, Ordering::Relaxed);
    let planet_body = crate::derotation::get_planet(&planet)
        .ok_or_else(|| "Planeta no soportado para derotacion".to_string())?;

    emit_progress(&app, "Preparando derotacion de secuencia...", 5.0, None);
    let mut frames: Vec<(String, Vec<u16>, usize, usize, f64, String)> = Vec::with_capacity(paths.len());
    for (idx, path) in paths.iter().enumerate() {
        if state.active_req_id.load(Ordering::Relaxed) == 0 {
            return Err("Operacion cancelada por el usuario".to_string());
        }
        let (rgb, width, height) = derot_load_rgb16_image(path)?;
        let (meta, time_source) = derot_metadata_with_source(path, None);
        let pct = 5.0 + ((idx as f32 / paths.len() as f32) * 20.0);
        emit_progress(
            &app,
            "Leyendo timestamps de secuencia...",
            pct,
            Some(format!("{} / {}", idx + 1, paths.len())),
        );
        frames.push((path.clone(), rgb, width, height, meta.mid_time_jd, time_source));
    }

    let (width, height) = (frames[0].2, frames[0].3);
    if frames.iter().any(|(_, _, w, h, _, _)| *w != width || *h != height) {
        return Err("Todos los frames deben tener la misma resolucion para derotacion planetaria".to_string());
    }

    let mut warnings = Vec::new();
    let log_count = frames.iter().filter(|(_, _, _, _, _, source)| source == "log").count();
    if log_count < frames.len() {
        warnings.push(format!(
            "{} de {} frames no tienen log de captura; valida el intervalo temporal.",
            frames.len() - log_count,
            frames.len()
        ));
    }
    let min_jd = frames
        .iter()
        .map(|(_, _, _, _, jd, _)| *jd)
        .fold(f64::INFINITY, f64::min);
    let max_jd = frames
        .iter()
        .map(|(_, _, _, _, jd, _)| *jd)
        .fold(f64::NEG_INFINITY, f64::max);
    let mut time_span_sec = (max_jd - min_jd).abs() * 86400.0;
    let fallback_interval = fallback_interval_sec.unwrap_or(0.0).clamp(0.0, 86400.0);
    if time_span_sec < 1.0 && fallback_interval > 0.0 && frames.len() > 1 {
        let mid = frames.len() / 2;
        let base_jd = frames[mid].4;
        for (idx, frame) in frames.iter_mut().enumerate() {
            let offset = idx as isize - mid as isize;
            frame.4 = base_jd + (offset as f64 * fallback_interval) / 86400.0;
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
    let reference_jd = frames[reference_frame].4;
    let geometry = crate::derotation::calculate_observer_geometry(planet_body, reference_jd);
    let b0 = sub_earth_lat_deg
        .unwrap_or(geometry.sub_earth_lat_deg)
        .clamp(-35.0, 35.0);
    let mono = derot_rgb_to_mono(&frames[0].1);
    let mut disc = crate::derotation::validate_disc_aspect(
        &crate::derotation::detect_planet_disc(&mono, width, height),
        planet_body,
    );
    disc.angle_deg = north_angle_deg.unwrap_or(geometry.north_pole_angle_deg);
    let first_parent = Path::new(&frames[0].0)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("Derotated_Planetary");
    std::fs::create_dir_all(&first_parent).map_err(|e| e.to_string())?;

    let mut output_paths = Vec::with_capacity(frames.len());
    for (idx, (path, rgb, _, _, jd, _)) in frames.into_iter().enumerate() {
        if state.active_req_id.load(Ordering::Relaxed) == 0 {
            return Err("Operacion cancelada por el usuario".to_string());
        }
        let pct = 28.0 + ((idx as f32 / paths.len() as f32) * 66.0);
        emit_progress(
            &app,
            "Derotando frames planetarios...",
            pct,
            Some(format!("{} / {}", idx + 1, paths.len())),
        );

        let derotated = crate::derotation::derotate_single_advanced(
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
        );
        let stem = Path::new(&path)
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("frame");
        let out_path = first_parent.join(format!("{:04}_{}_derotated.tiff", idx + 1, stem));
        derot_save_rgb16_tiff(&out_path, &derotated, width, height)?;
        output_paths.push(clean_windows_path(out_path));
    }

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
    state.active_req_id.store(1, Ordering::Relaxed);
    let planet_body = crate::derotation::get_planet(&planet)
        .ok_or_else(|| "Planeta no soportado para derotacion".to_string())?;

    emit_progress(&app, "Preparando fusion multi-stack...", 4.0, None);
    let mut frames: Vec<(String, usize, usize, f64, String, DerotationDiagnostics)> =
        Vec::with_capacity(image_paths.len());
    let mut warnings = Vec::new();

    for (idx, path) in image_paths.iter().enumerate() {
        if state.active_req_id.load(Ordering::Relaxed) == 0 {
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
    let reference_derotated = crate::derotation::derotate_single_advanced(
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
    );
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
        if state.active_req_id.load(Ordering::Relaxed) == 0 {
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
            crate::derotation::derotate_single_advanced(
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
            )
        };
        let frame_means =
            derot_channel_means_inside_disc(&derotated, width, height, &reference_disc, 0.72);
        let gains = derot_photometric_gains(reference_means, frame_means);

        for pix in 0..pixel_count {
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

    if accum_weight.iter().all(|w| *w <= 0.0) {
        return Err("No se pudo calcular peso valido para fusionar los stacks.".to_string());
    }
    let mut fused = Vec::with_capacity(pixel_count.saturating_mul(3));
    for pix in 0..pixel_count {
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
    let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let out_path = parent.join(format!("Zenith_Derotated_Fusion_{}.tiff", stamp));
    emit_progress(&app, "Guardando fusion derotada 16-bit...", 92.0, None);
    derot_save_rgb16_tiff(&out_path, &fused, width, height)?;

    {
        let mut stacked = state.stacked_image.lock().unwrap();
        *stacked = Some(StackResult {
            data: fused.clone(),
            width,
            height,
            is_mono: false,
            is_surface: false,
        });
    }
    state.deconv_cache.lock().unwrap().clear();
    state.wavelet_cache.lock().unwrap().clear();
    state.filter_cache.lock().unwrap().clear();

    let preview_base64 = derot_encode_preview(&fused, width, height)?;
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
    state.active_req_id.store(1, Ordering::Relaxed);
    let planet_body = crate::derotation::get_planet(&planet)
        .ok_or_else(|| "Planeta no soportado para derotacion".to_string())?;

    let channel_paths = [red_path, green_path, blue_path];
    let channel_names = ["R", "G", "B"];
    let mut warnings: Vec<String> = Vec::new();
    emit_progress(&app, "Preparando derotacion RGB por canal...", 4.0, None);

    // Cargar los 3 canales + su tiempo de captura (log / archivo / reloj).
    let mut loaded: Vec<(Vec<u16>, usize, usize, f64, String)> = Vec::with_capacity(3);
    for (i, path) in channel_paths.iter().enumerate() {
        if state.active_req_id.load(Ordering::Relaxed) == 0 {
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

    let pixel_count = width.saturating_mul(height);
    let mut out = vec![0u16; pixel_count.saturating_mul(3)];
    let mut min_jd = f64::INFINITY;
    let mut max_jd = f64::NEG_INFINITY;

    // Derotar cada canal a reference_jd y colocar su LUMA en el plano de salida.
    for (ch, (rgb, _, _, jd, source)) in loaded.iter().enumerate() {
        if state.active_req_id.load(Ordering::Relaxed) == 0 {
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
        let derotated = crate::derotation::derotate_single_advanced(
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
        );
        let mono = derot_rgb_to_mono(&derotated);
        for pix in 0..pixel_count {
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
    let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let out_path = parent.join(format!("Zenith_Derotated_RGB_{}.tiff", stamp));
    emit_progress(&app, "Guardando RGB derotado 16-bit...", 92.0, None);
    derot_save_rgb16_tiff(&out_path, &out, width, height)?;

    {
        let mut stacked = state.stacked_image.lock().unwrap();
        *stacked = Some(StackResult {
            data: out.clone(),
            width,
            height,
            is_mono: false,
            is_surface: false,
        });
    }
    state.deconv_cache.lock().unwrap().clear();
    state.wavelet_cache.lock().unwrap().clear();
    state.filter_cache.lock().unwrap().clear();

    let preview_base64 = derot_encode_preview(&out, width, height)?;
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
    // IMPROVED ALGORITHM: Percentile-based stretch
    // This is more robust for lunar images with varying amounts of black space

    let mut pixels: Vec<u8> = img.pixels().map(|p| p[0]).collect();
    pixels.sort_unstable();

    let total = pixels.len();
    if total == 0 {
        return img.clone();
    }

    // Find the 1.5th percentile (ignore noise/background) and 98.5th percentile
    // This is safer for lunar images with varying amounts of black space.
    let idx_min = (total as f32 * 0.015) as usize;
    let idx_max = (total as f32 * 0.985) as usize;

    let min_val = pixels[idx_min.min(total - 1)];
    let max_val = pixels[idx_max.min(total - 1)];

    if max_val <= min_val {
        // Fallback for extremely flat/dark images: use absolute max
        let absolute_max = pixels[total - 1];
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

#[tauri::command]
async fn stitch_mosaic(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    tiles: Vec<MosaicTileConfig>,
    mode: String, // Ignored, always "blind" now
) -> Result<AnalysisResult, String> {
    state.active_req_id.store(1, Ordering::Relaxed);
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
    let work_scale = 0.5;

    let mut nodes: Vec<ImageNode> = Vec::with_capacity(tiles.len());

    for (i, t) in tiles.iter().enumerate() {
        if state.active_req_id.load(Ordering::Relaxed) == 0 {
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

        let dyn_img = load_image_or_video_frame(&t.path, &app)
            .map_err(|e| format!("Error cargando {}: {}", t.path, e))?;

        // CRITICAL: Proper 16-bit to 8-bit conversion for AKAZE feature detection
        // Direct to_luma8() loses all contrast in linear astronomical images
        // We need to:
        // 1. Convert to grayscale at full resolution
        // 2. Apply auto_stretch to bring out lunar details (craters, maria)
        // 3. Then pass to AKAZE for feature detection

        let gray_full = dyn_img.to_luma8();
        let gray_stretched = auto_stretch_gray(&gray_full);
        let img_for_akaze = DynamicImage::ImageLuma8(gray_stretched.clone());

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

        // Limit features if too many (sort by response/size?)
        if features.len() > 3000 {
            // Akaze returns sorted by response usually?
            features.truncate(3000);
        }

        // Create gray reference for alignment refinement (snapping)
        // IMPROVED: Use higher resolution (0.75 instead of 0.5) for better matching
        // and use the already-stretched image for SAD optimization
        let work_scale = 0.75;
        let small = img_for_akaze.resize(
            (img_for_akaze.width() as f32 * work_scale) as u32,
            (img_for_akaze.height() as f32 * work_scale) as u32,
            image::imageops::FilterType::Lanczos3,
        );
        let gray_small = small.to_luma8();
        let gray_small_stretched = auto_stretch_gray(&gray_small);

        nodes.push(ImageNode {
            idx: i,
            width: dyn_img.width(),
            height: dyn_img.height(),
            global_x: 0,
            global_y: 0,
            placed: false,
            img_gray: gray_small,
            img_gray_stretched: gray_small_stretched,
            features,
            original_path: t.path.clone(), // Store for 16-bit reload during fusion
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
        if state.active_req_id.load(Ordering::Relaxed) == 0 {
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

            let mut potential_matches = Vec::new();

            // ROBUST MATCHING S.O.P. (Standard Operating Procedure):
            // 1. Find Best Match I->J
            // 2. Find Best Match J->I
            // 3. Keep ONLY if they agree (Cross-Check / Reciprocal Match)

            for (fi_idx, fi) in feats_i.iter().enumerate() {
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
                    // 2. Backward Match J -> I (Cross Check for Safety)
                    let fj_candidate = &feats_j[best_j];
                    let mut best_back_dist = u32::MAX;
                    let mut best_i_back = 0;

                    for (fk_idx, fk) in feats_i.iter().enumerate() {
                        let dist = fj_candidate.distance(fk);
                        if dist < best_back_dist {
                            best_back_dist = dist;
                            best_i_back = fk_idx;
                        }
                    }

                    // 3. Consistency Check (Mutual Best Match)
                    if best_i_back == fi_idx {
                        // IT'S A MATCH! Mutual agreement.
                        potential_matches.push((fi, fj_candidate, fi_idx));
                    }
                }
            }

            // RANSAC Translation
            // Find most common (dx, dy) with subpixel precision
            let match_tolerance = 20.0; // Tolerance for RANSAC clustering (pixels)
            let mut best_dx = 0;
            let mut best_dy = 0;
            let mut max_inliers = 0;

            let mut shifts = Vec::with_capacity(potential_matches.len());
            for (p1, p2, _) in &potential_matches {
                // Correct direction: Shift J relative to I.
                // dx = p1.x - p2.x (f32)
                let dx = p1.x - p2.x;
                let dy = p1.y - p2.y;
                shifts.push((dx, dy));
            }

            // Consensus: Find cluster of shifts using Euclidean distance
            for k in 0..shifts.len() {
                let (ref_dx, ref_dy) = shifts[k]; // f32
                let mut inliers = 0;

                // Optimized inner loop
                for (dx, dy) in &shifts {
                    // Check Euclidean distance
                    let dist = ((dx - ref_dx).powi(2) + (dy - ref_dy).powi(2)).sqrt();
                    if dist <= match_tolerance {
                        inliers += 1;
                    }
                }

                if inliers > max_inliers {
                    max_inliers = inliers;
                    best_dx = ref_dx.round() as i32; // Fix type mismatch
                    best_dy = ref_dy.round() as i32;
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
            let manual_dx_real = ((t_j.x - t_i.x) as f32 * scale_factor) as i32;
            let manual_dy_real = ((t_j.y - t_i.y) as f32 * scale_factor) as i32;

            // Decision Logic
            let mut use_ransac = false;
            let mut final_dx = 0;
            let mut final_dy = 0;
            let mut final_score = 0;

            if max_inliers > 5 {
                // Minimum 5 inliers for valid match (relaxed for lunar images)
                // RANSAC found a good match.
                // CRITICAL: Features are now extracted at FULL resolution (not scaled)
                // and NO WORK SCALE division because work_scale is 1.0 for features.
                // UNLESS we use 0.5 work_scale.
                let ransac_dx = (best_dx as f32 / 1.0) as i32;
                let ransac_dy = (best_dy as f32 / 1.0) as i32;

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
                    let threshold = (w_i_real * 0.3) as i32; // 30% tolerance

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
                let w_small = nodes[i].img_gray.width() as i32;
                let h_small = nodes[i].img_gray.height() as i32;

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
                    (best_snap_dx as f32 / work_scale) as i32
                } else {
                    manual_dx_real
                };
                let final_dy = if snapped {
                    (best_snap_dy as f32 / work_scale) as i32
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

    if max_deg == 0 && tiles.len() > 1 {
        return Err("No se encontraron coincidencias suficientes. Intenta aumentar el solapamiento o brillo.".to_string());
    }

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
            self.score.cmp(&other.score)
        }
    }
    impl PartialOrd for QueuedNode {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    nodes[root_idx].placed = true;
    nodes[root_idx].global_x = 0;
    nodes[root_idx].global_y = 0; // Fix: was using uninitialized values if not 0
                                  // Use img_gray to confirm root placed (debug)
    log_to_front(
        &app,
        "DEBUG",
        &format!(
            "Raiz colocada. Dim: {}x{}",
            nodes[root_idx].img_gray.width(),
            nodes[root_idx].img_gray.height()
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

    // Check orphan nodes? logic simple for now: ignore or place at 0,0 (will overlap root)
    // Ideally we warn.
    let unplaced = nodes.iter().filter(|n| !n.placed).count();
    if unplaced > 0 {
        log_to_front(
            &app,
            "WARN",
            &format!("{} teselas no pudieron conectarse (islas).", unplaced),
        );
    }

    // 4. Calculate Canvas Bounds & Render
    emit_progress(&app, "Renderizando y Mezclando...", 50.0, None);

    let mut min_x = i32::MAX;
    let mut max_x = i32::MIN;
    let mut min_y = i32::MAX;
    let mut max_y = i32::MIN;

    for n in &nodes {
        if !n.placed {
            continue;
        } // Or render unplaced overlap root? skip.
        if n.global_x < min_x {
            min_x = n.global_x;
        }
        if n.global_y < min_y {
            min_y = n.global_y;
        }

        let r = n.global_x + n.width as i32;
        let b = n.global_y + n.height as i32;
        if r > max_x {
            max_x = r;
        }
        if b > max_y {
            max_y = b;
        }
    }

    let padding = 50;
    let cv_w = (max_x - min_x) as u32 + (padding * 2) as u32;
    let cv_h = (max_y - min_y) as u32 + (padding * 2) as u32;

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

    // We process sequentially to save RAM or just allocate?
    // Let's Allocate.
    let len = (cv_w as usize) * (cv_h as usize);
    let mut acc_r = vec![0.0f32; len];
    let mut acc_g = vec![0.0f32; len];
    let mut acc_b = vec![0.0f32; len];
    let mut acc_w = vec![0.0f32; len];

    for (i, n) in nodes.iter().enumerate() {
        if !n.placed {
            continue;
        }

        emit_progress(
            &app,
            &format!("Fundiendo tesela {}...", i + 1),
            50.0 + (i as f32 / nodes.len() as f32) * 40.0,
            None,
        );

        // OPTIMIZED: Load from stored path to get full 16-bit image for fusion
        // This avoids reloading from tiles array and ensures we use original quality
        let dyn_img =
            load_image_or_video_frame(&n.original_path, &app).map_err(|e| e.to_string())?;
        let rgba = dyn_img.to_rgba16(); // Force 16-bit load for quality preservation

        let n_w = n.width;
        let n_h = n.height;

        let offset_x = (n.global_x - min_x + padding as i32) as usize;
        let offset_y = (n.global_y - min_y + padding as i32) as usize;

        let crop_margin = 35.0f32;

        // OPTIMIZED: Parallel execution across rows using Rayon for massive speedup
        use rayon::prelude::*;

        let ptr_r = acc_r.as_mut_ptr() as usize;
        let ptr_g = acc_g.as_mut_ptr() as usize;
        let ptr_b = acc_b.as_mut_ptr() as usize;
        let ptr_w = acc_w.as_mut_ptr() as usize;

        (0..n_h).into_par_iter().for_each(|y| {
            let p_r = ptr_r as *mut f32;
            let p_g = ptr_g as *mut f32;
            let p_b = ptr_b as *mut f32;
            let p_w = ptr_w as *mut f32;

            for x in 0..n_w {
                let px = rgba.get_pixel(x, y);
                if px[3] == 0 {
                    continue;
                }

                // 1. EDGE CROP (Requested by user to avoid bad sensor edges)
                // Discard pixels near the boundaries where cameras often leave artifacts or straight cuts.
                let dx = x.min(n_w - 1 - x) as f32;
                let dy = y.min(n_h - 1 - y) as f32;
                let dist = dx.min(dy);

                if dist < crop_margin {
                    continue;
                }

                // 2. BLACK BACKGROUND REJECTION
                // Skip padding and deep space to prevent overlapping solid black on top of craters.
                if px[0] < 1500 && px[1] < 1500 && px[2] < 1500 {
                    continue;
                }

                // 3. MAXIMUM LUMINOSITY BLENDING (NO AVERAGING = NO BLURRING)
                // Keep the conservative behavior that preserves the sharpest
                // existing sample instead of synthesizing seam pixels.
                let lum_f32 = px[0] as f32 + px[1] as f32 + px[2] as f32;

                let cv_idx = (offset_y + y as usize) * cv_w as usize + (offset_x + x as usize);

                if cv_idx < len {
                    unsafe {
                        // acc_w tracks the current maximum luminosity at this pixel.
                        let current_lum = *p_w.add(cv_idx);
                        if lum_f32 > current_lum {
                            *p_r.add(cv_idx) = px[0] as f32;
                            *p_g.add(cv_idx) = px[1] as f32;
                            *p_b.add(cv_idx) = px[2] as f32;
                            *p_w.add(cv_idx) = lum_f32;
                        }
                    }
                }
            }
        });
    }

    emit_progress(&app, "Finalizando...", 95.0, None);

    // 6. Normalize & Output
    // 16-bit output buffer
    let mut out_u16: Vec<u16> = vec![0; len * 3]; // RGB
    
    // OPTIMIZED: Parallel normalization. The luminosity winner pass already chose
    // the final 16-bit sample for each pixel, so no averaging division is needed.
    out_u16.par_chunks_mut(3).enumerate().for_each(|(idx, pixel)| {
        let lum = acc_w[idx];
        if lum > 0.0 {
            pixel[0] = acc_r[idx] as u16;
            pixel[1] = acc_g[idx] as u16;
            pixel[2] = acc_b[idx] as u16;
        }
    });

    // Optimization: Directly write to Rgb16 Image buffer
    // Replaces: let mut result_img = image::ImageBuffer::<image::Rgb<u16>, Vec<u16>>::new(cv_w, cv_h);
    let result_img: image::ImageBuffer<image::Rgb<u16>, Vec<u16>> =
        image::ImageBuffer::from_raw(cv_w, cv_h, out_u16).ok_or("Failed to create image buffer")?;

    // 7. Save
    // Create Filename
    let first_tile_path = Path::new(&tiles[0].path);
    let parent_dir = first_tile_path.parent().unwrap_or(Path::new("."));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();

    // Save Pro Result (TIFF 16-bit)
    let filename_tiff = format!("Mosaic_Result_{}.tiff", now.as_secs());
    let path_tiff = parent_dir.join(&filename_tiff);

    let f = File::create(&path_tiff).map_err(|e| e.to_string())?;
    let ref_writer = BufWriter::new(f);
    let encoder = image::codecs::tiff::TiffEncoder::new(ref_writer);

    // Convert to byte slice for encoder
    // Image buffer provides as_raw which is &[u16] ordered R,G,B
    let raw_u16 = result_img.as_raw();
    emit_progress(&app, "Generando PrevisualizaciÃ³n...", 98.0, None);
    let preview_bytes = to_8bit_preview_visual(raw_u16);

    let mut raw_bytes = Vec::with_capacity(raw_u16.len() * 2);
    for s in raw_u16 {
        raw_bytes.extend_from_slice(&s.to_ne_bytes());
    }

    encoder
        .encode(&raw_bytes, cv_w, cv_h, image::ColorType::Rgb16)
        .map_err(|e| e.to_string())?;

    log_to_front(&app, "INFO", &format!("Mosaico Guardado: {:?}", path_tiff));

    let filename_preview = format!("Mosaic_Result_{}_NativePreview.png", now.as_secs());
    let path_preview = parent_dir.join(&filename_preview);
    let f_preview = File::create(&path_preview).map_err(|e| e.to_string())?;
    let preview_writer = BufWriter::new(f_preview);
    let preview_encoder = image::codecs::png::PngEncoder::new(preview_writer);
    preview_encoder
        .encode(&preview_bytes, cv_w, cv_h, image::ColorType::Rgb8)
        .map_err(|e| e.to_string())?;
    let path_preview_clean =
        clean_windows_path(dunce::canonicalize(&path_preview).unwrap_or(path_preview));
    let path_tiff_clean = clean_windows_path(dunce::canonicalize(&path_tiff).unwrap_or(path_tiff));
    let preview_ref = format!("file_path:{}", path_preview_clean);

    // Update Global State StackResult (for Wavelets)
    let is_mono_detected = if raw_u16.len() > 3 {
        let mid = raw_u16.len() / 2;
        let mid = mid - (mid % 3); // Align R
        raw_u16[mid] == raw_u16[mid + 1] && raw_u16[mid + 1] == raw_u16[mid + 2]
    } else {
        false
    };

    {
        let mut locked = state.stacked_image.lock().unwrap();
        *locked = Some(StackResult {
            data: raw_u16.to_vec(),
            width: cv_w as usize,
            height: cv_h as usize,
            is_mono: is_mono_detected,
            is_surface: true,
        });
    }

    emit_progress(&app, "Listo", 100.0, None);

    Ok(AnalysisResult {
        metadata: VideoMetadata {
            width: cv_w as usize,
            height: cv_h as usize,
            frame_count: tiles.len(),
            bpp: 16,
            color_id: 0,
            pattern_name: "Mosaic Blind".to_string(),
            file_size_mb: 0.0,
            is_color: true,
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

    // --- 3. Contrast as an S-curve anchored at the image midtone ------------
    // Neutral 1.0. Pivots on the actual signal midtone (great for astro frames
    // that are mostly dark) and preserves both endpoints -> no hard clipping.
    if (contrast - 1.0).abs() > 0.001 {
        let pivot = (contrast_pivot * INV_N).clamp(0.05, 0.95);
        let k = contrast.clamp(0.1, 3.0);
        rn = s_curve_contrast(rn, pivot, k);
        gn = s_curve_contrast(gn, pivot, k);
        bn = s_curve_contrast(bn, pivot, k);
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
) -> Vec<f32> {
    let blurred = apply_gaussian_blur_safe(chan, w, h, sigma);
    let mut out = vec![0.0; chan.len()];
    for i in 0..chan.len() {
        let hp = chan[i] - blurred[i];
        let mut added = hp * amt;
        let limit = img_scale * 8000.0;
        if added > limit {
            added = limit + (added - limit).powf(0.5) * 10.0 * img_scale;
        } else if added < -limit {
            added = -(limit + (-added - limit).powf(0.5) * 10.0 * img_scale);
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
) -> Vec<f32> {
    // FIX: If radius is 0 (default slider pos), use an intelligent default (1.5)
    // allowing "One Slider" operation as requested.
    let effective_radius = if radius < 0.1 { 1.5 } else { radius };

    // SAFE CALL: Use local safe implementation
    let blurred = apply_gaussian_blur_safe(chan, w, h, effective_radius);

    // FIX: Initialize with input to ensure we don't return black if loop fails or logic errors
    let mut out = chan.clone();
    let threshold = 50.0;

    for i in 0..chan.len() {
        let diff = chan[i] - blurred[i];
        let mut added = if diff.abs() > threshold {
            diff * amt
        } else {
            let factor = (diff.abs() / threshold).powf(2.0);
            diff * amt * factor
        };

        let limit = img_scale * 8000.0;
        if added > limit {
            added = limit + (added - limit).powf(0.5) * 10.0 * img_scale;
        } else if added < -limit {
            added = -(limit + (-added - limit).powf(0.5) * 10.0 * img_scale);
        }

        out[i] = chan[i] + added;
    }
    out
}

fn apply_clahe_improved(chan: &Vec<f32>, w: usize, h: usize, amt: f32) -> Vec<f32> {
    // Robust Local Contrast (LCE) with Limiting
    // FIX: Cap sigma to 30.0 to prevent freezing on large images (convolution explode)
    let dynamic_sigma = (w.max(h) as f32 * 0.02).min(30.0).max(5.0);

    // SAFE CALL: Use local safe implementation
    let blurred = apply_gaussian_blur_safe(chan, w, h, dynamic_sigma);

    let mut out = chan.clone();
    let limit = 8000.0;
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

fn apply_luma_preserving_denoise(
    chan: &[f32],
    width: usize,
    height: usize,
    amount: f32,
    detail_protect: f32,
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
    let blend = (0.30 + amount_n * 0.70).clamp(0.0, 1.0);

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

        let y_clean = apply_luma_preserving_denoise(&y, width, height, amount, detail_protect);
        let (u_clean, v_clean) =
            apply_chroma_denoise_planes(&u, &v, width, height, amount, chroma_amount);

        for i in 0..size {
            let (r, g, b) = yuv_to_rgb(y_clean[i], u_clean[i], v_clean[i]);
            channels[0][i] = r;
            channels[1][i] = g;
            channels[2][i] = b;
        }
    } else {
        channels[0] = apply_luma_preserving_denoise(
            &channels[0],
            width,
            height,
            amount,
            detail_protect,
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
    *state.stacked_image.lock().unwrap() = None;
    state.deconv_cache.lock().unwrap().clear();
    state.wavelet_cache.lock().unwrap().clear();
    state.filter_cache.lock().unwrap().clear();
    *state.batch_anchor.lock().unwrap() = None;
    *state.batch_anchor_dims.lock().unwrap() = (0, 0);
    state.active_req_id.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// R13: limpieza ligera POR ARCHIVO dentro de un lote. A diferencia de
/// clear_app_memory, NO toca batch_anchor/batch_anchor_dims: borrarlos entre
/// archivos destruia la estabilizacion del timelapse (el anchor nunca
/// sobrevivia mas alla del primer video).
#[tauri::command]
fn clear_stack_memory(state: tauri::State<'_, AppState>) {
    *state.stacked_image.lock().unwrap() = None;
    *state.deep_sky_result.lock().unwrap() = None;
    state.deconv_cache.lock().unwrap().clear();
    state.wavelet_cache.lock().unwrap().clear();
    state.filter_cache.lock().unwrap().clear();
    state.active_req_id.store(0, std::sync::atomic::Ordering::Relaxed);
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
    run_planetary_stack(
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
            profile: PipelineProfile::Custom,
        },
    )
    .await
}

fn main() {
    tauri::Builder::default()
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
                deconv_cache: Mutex::new(Vec::new()),
                wavelet_cache: Mutex::new(Vec::new()),
                filter_cache: Mutex::new(Vec::new()),
                batch_anchor: Mutex::new(None),
                batch_anchor_dims: Mutex::new((0, 0)),
                active_req_id: AtomicUsize::new(0),
                cancel_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                license_manager,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            analyze_video,
            preview_video,
            stack_video,
            apply_wavelets,
            save_final_image,
            export_mosaic_result,
            generate_grid,
            analyze_psf,
            crop_stacked_image,
            scan_directory,
            create_gif_animation,
            export_animation_video,
            process_batch_entry,
            get_ser_conversion_preflight,
            convert_video_to_ser_frontend,
            normalize_batch_brightness,
            crop_animation_frames,
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
