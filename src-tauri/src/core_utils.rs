// ==========================================
// 2. UTILIDADES BASE
// ==========================================

fn clean_windows_path(path: PathBuf) -> String {
    let s = path.to_string_lossy().to_string();
    if cfg!(windows) && s.starts_with("\\\\?\\") {
        s[4..].to_string()
    } else {
        s
    }
}

fn log_to_front(app: &tauri::AppHandle, level: &str, msg: &str) {
    let _ = app.emit(
        "log_event",
        LogMessage {
            level: level.to_string(),
            msg: msg.to_string(),
        },
    );
}

fn emit_progress(app: &tauri::AppHandle, step: &str, pct: f32, details: Option<String>) {
    // SANEO CENTRAL: con totales estimados (MP4/MOV sin nb_frames) el cálculo
    // c/total puede pasarse de 100 o producir NaN/inf; serde serializa NaN
    // como null y el frontend lo lee como 0 → barra congelada en 0. Ningún
    // emisor puede volver a romper la barra desde aquí.
    let pct = if pct.is_finite() {
        pct.clamp(0.0, 100.0)
    } else {
        0.0
    };
    // PR-2.5: THROTTLE temporal (~80 ms). Los bucles calientes se autolimitan
    // por módulo de frames, pero en SER mono muy rápidos la frecuencia real
    // seguía acoplada a los fps (decenas de eventos IPC/seg → jank en el
    // WebView). Se agrupan solo los mensajes que difieren ÚNICAMENTE en sus
    // números ("Frame 12/500" vs "Frame 37/500"): los hitos con texto
    // distinto y los extremos de la barra pasan siempre.
    {
        use std::hash::{Hash, Hasher};
        use std::sync::atomic::{AtomicU64, Ordering};
        static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        static LAST_MS: AtomicU64 = AtomicU64::new(u64::MAX);
        static LAST_KEY: AtomicU64 = AtomicU64::new(0);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for b in step.bytes().filter(|b| !b.is_ascii_digit()) {
            b.hash(&mut hasher);
        }
        let key = hasher.finish();
        let now_ms = EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64;
        if pct > 0.5 && pct < 99.5 && key == LAST_KEY.load(Ordering::Relaxed) {
            let last = LAST_MS.load(Ordering::Relaxed);
            if now_ms.wrapping_sub(last) < 80 {
                return;
            }
        }
        LAST_KEY.store(key, Ordering::Relaxed);
        LAST_MS.store(now_ms, Ordering::Relaxed);
    }
    let _ = app.emit(
        "progress",
        Progress {
            step: step.to_string(),
            pct,
            details,
        },
    );
}

fn emit_pipeline_telemetry(app: &tauri::AppHandle, telemetry: PipelineTelemetry) {
    crate::pipeline::record_pipeline_telemetry(&telemetry);
    let _ = app.emit("pipeline_telemetry", telemetry);
}

/// Telemetria EN VIVO de la FASE DE ANALISIS (mismo evento "stack_telemetry"
/// que el apilado, con phase="analysis" para que la UI etiquete bien).
/// MATICES de aceleracion en el analisis (importante para no confundir):
///  - wgpu procesa por lotes luma/pirámide/Laplaciano/CoG/calidad/SAD grueso;
///    CPU/SIMD conserva refinamiento, validación y decisiones globales.
///  - La DECODIFICACION de videos comprimidos también puede usar GPU hardware
///    (VideoToolbox/NVDEC/D3D11VA via FFmpeg, el "Modo Turbo"). `decode_gpu`:
///    Some(true) = decode HW-GPU activo, Some(false) = decode CPU (fallback),
///    None = lector nativo SER/AVI/FITS (mmap, sin decode).
/// `sys` se refresca bajo lock (barato, cada N frames). align_ms = ms/frame.
#[allow(clippy::too_many_arguments)]
fn emit_analysis_telemetry(
    app: &tauri::AppHandle,
    job_id: &str,
    reader_kind: &str,
    decode_gpu: Option<bool>,
    compute_gpu: bool,
    // Motivo visible cuando el cómputo cayó a CPU ("Auto eligió CPU",
    // "VRAM insuficiente", "la GPU falló en un lote"...): el usuario no debe
    // adivinar por qué la etiqueta dice "scoring CPU".
    cpu_reason: Option<String>,
    uploaded_bytes_per_frame: usize,
    estimated_vram_mb: u64,
    done: usize,
    total: usize,
    start: std::time::Instant,
    threads: usize,
    sys: &std::sync::Mutex<sysinfo::System>,
) {
    let elapsed = start.elapsed().as_secs_f32().max(0.001);
    let d = done.max(1);
    let (ram_mb, cpu_percent, io_read_mb, io_write_mb) = {
        let mut s = sys.lock().unwrap();
        // refresh_all() enumeraba TODOS los procesos del sistema bajo lock en
        // cada emisión (cada 10-50 frames); estos refrescos puntuales producen
        // exactamente los campos consumidos a una fracción del coste.
        s.refresh_memory();
        s.refresh_cpu();
        let pid = sysinfo::get_current_pid().ok();
        if let Some(p) = pid {
            s.refresh_process(p);
        }
        let process = pid.and_then(|p| s.process(p));
        let disk = process.map(|p| p.disk_usage());
        (
            process.map(|p| p.memory() / (1024 * 1024)).unwrap_or_else(|| s.used_memory() / (1024 * 1024)),
            Some(s.global_cpu_info().cpu_usage()),
            disk.map(|d| d.total_read_bytes as f64 / 1_048_576.0).unwrap_or(0.0),
            disk.map(|d| d.total_written_bytes as f64 / 1_048_576.0).unwrap_or(0.0),
        )
    };
    let decode_lbl = match decode_gpu {
        Some(true) => " · decode HW-GPU",
        Some(false) => " · decode CPU",
        None => "",
    };
    let compute_lbl = if compute_gpu {
        " · preprocess GPU + decisiones CPU".to_string()
    } else {
        match &cpu_reason {
            Some(reason) => format!(" · scoring CPU — {reason}"),
            None => " · scoring CPU".to_string(),
        }
    };
    let _ = app.emit(
        "stack_telemetry",
        StackTelemetry {
            phase: "analysis".to_string(),
            // La UI ya antepone "Análisis:", asi que el modo NO lo repite
            // (antes mostraba "Análisis: Analisis · SER..." duplicado).
            mode: format!(
                "{}{}{} {}",
                reader_kind,
                decode_lbl,
                compute_lbl,
                simd_backend_label()
            ),
            decode_gpu,
            compute_gpu,
            frames_done: done,
            frames_total: total,
            fps: d as f32 / elapsed,
            align_ms: (elapsed * 1000.0) / d as f32,
            accum_ms: 0.0,
            upload_mbps: if compute_gpu {
                (done.saturating_mul(uploaded_bytes_per_frame) as f32)
                    / elapsed
                    / 1_048_576.0
            } else {
                0.0
            },
            ram_mb,
            vram_mb: if compute_gpu {
                estimated_vram_mb.min(crate::gpu_stack::gpu_info().vram_budget_mb)
            } else {
                0
            },
            cache_hits: 0,
            threads,
        },
    );
    emit_pipeline_telemetry(
        app,
        PipelineTelemetry {
            job_id: job_id.into(),
            domain: PipelineDomain::Planetary,
            phase: "analysis".into(),
            engine: format!("{}{}{} {}", reader_kind, decode_lbl, compute_lbl, simd_backend_label()),
            progress: done as f32 / total.max(1) as f32 * 100.0,
            eta_seconds: if done > 0 && done < total {
                Some(elapsed / done as f32 * (total - done) as f32)
            } else { None },
            items_done: done,
            items_total: total,
            throughput: Some(d as f32 / elapsed),
            cpu_percent,
            gpu_percent: None,
            ram_mb,
            vram_mb: if compute_gpu {
                estimated_vram_mb.min(crate::gpu_stack::gpu_info().vram_budget_mb)
            } else {
                0
            },
            io_read_mb,
            io_write_mb,
            cache_hits: 0,
            cache_misses: 0,
            fallback_reason: None,
        },
    );
}

/// Escribe un preview PNG a un archivo temporal y devuelve su ruta absoluta
/// (el frontend la carga via asset protocol / convertFileSrc). Transportar el
/// PNG como data-URL base64 por IPC multiplicaba el pico de RAM del WebView.
/// NOMBRE UNICO por llamada: el WebView cachea por URL — reutilizar el mismo
/// nombre mostraria la imagen ANTERIOR. Los previews de sesiones pasadas
/// (>24 h) se purgan en cada escritura. Devuelve None si el temp no es
/// escribible (el caller cae al data-URL clasico).
/// F3: escritor FITS 16-bit para SALIDA planetaria/lunar/solar (WinJUPOS,
/// fotometría, apilado posterior). `rgb` interleaved u16 (w*h*3). Estándar
/// FITS: BITPIX=16 (i16 con BZERO=32768 para el rango sin signo 0..65535),
/// big-endian, datos PLANARES por canal (todo R, luego G, luego B) con
/// NAXIS=3/NAXIS3=3; el bloque de datos se rellena a múltiplo de 2880 bytes.
fn write_rgb16_fits(path: &str, rgb: &[u16], width: usize, height: usize) -> Result<(), String> {
    if rgb.len() != width * height * 3 {
        return Err("Buffer RGB16 inválido para FITS".into());
    }
    let mut header = String::new();
    let mut card = |kw: &str, val: &str| {
        // Cada card ocupa EXACTAMENTE 80 caracteres.
        let line = if val.is_empty() {
            format!("{:<80}", kw)
        } else {
            format!("{:<8}= {:>20}{:<50}", kw, val, "")
        };
        header.push_str(&line[..80]);
    };
    card("SIMPLE", "T");
    card("BITPIX", "16");
    card("NAXIS", "3");
    card("NAXIS1", &width.to_string());
    card("NAXIS2", &height.to_string());
    card("NAXIS3", "3");
    card("BZERO", "32768");
    card("BSCALE", "1");
    card("COMMENT   Zenith Astro Stacker — planetary 16-bit RGB", "");
    card("END", "");
    // Relleno del header a múltiplo de 2880 con espacios.
    while header.len() % 2880 != 0 {
        header.push(' ');
    }

    let n = width * height;
    let mut data = Vec::with_capacity(n * 3 * 2 + 2880);
    // Planar R,G,B; i16 big-endian con offset −32768 (BZERO lo revierte).
    for c in 0..3 {
        for i in 0..n {
            let signed = rgb[i * 3 + c] as i32 - 32768;
            data.extend_from_slice(&(signed as i16).to_be_bytes());
        }
    }
    while data.len() % 2880 != 0 {
        data.push(0);
    }
    let mut out = header.into_bytes();
    out.extend_from_slice(&data);
    std::fs::write(path, out).map_err(|e| e.to_string())
}

fn save_preview_png_to_temp(png_bytes: &[u8], tag: &str) -> Option<String> {
    let dir = std::env::temp_dir().join("astro_stacker_previews");
    std::fs::create_dir_all(&dir).ok()?;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let now = std::time::SystemTime::now();
        for e in rd.flatten() {
            let stale = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .map(|d| d.as_secs() > 24 * 3600)
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let path = dir.join(format!("{}_{}_{}.png", tag, std::process::id(), nanos));
    std::fs::write(&path, png_bytes).ok()?;
    Some(clean_windows_path(path))
}

/// Keeps the full-resolution previews required by undo/A-B for the active
/// session, while bounding disk usage. Fast drag previews are transient; full
/// previews retain a margin above the frontend's 50-entry history limit.
fn prune_editor_previews() {
    let dir = std::env::temp_dir().join("astro_stacker_previews");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut full = Vec::new();
    let mut fast = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if name.starts_with("editor_fast_") {
            fast.push((modified, entry.path()));
        } else if name.starts_with("editor_") {
            full.push((modified, entry.path()));
        }
    }
    fast.sort_by(|left, right| right.0.cmp(&left.0));
    for (_, path) in fast.into_iter().skip(2) {
        let _ = std::fs::remove_file(path);
    }
    full.sort_by(|left, right| right.0.cmp(&left.0));
    for (_, path) in full.into_iter().skip(64) {
        let _ = std::fs::remove_file(path);
    }
}

/// A new stack/mosaic/batch result starts a new atomic editor session, so no
/// preview from the prior result may remain addressable by the new history.
fn clear_editor_previews() {
    let dir = std::env::temp_dir().join("astro_stacker_previews");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in rd.flatten() {
        if entry.file_name().to_string_lossy().starts_with("editor_") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// PR-2.2: deinterleave del canal VERDE (RGB u16 interleaved → mono) con
/// vld3q_u16 en aarch64 (8 píxeles por iteración). El gather escalar con
/// stride 3 corría por frame y por pasada en el bucle de acumulación color
/// (~8.3M iteraciones/frame a 4K, hostil al prefetcher). En x86 se deja el
/// escalar: LLVM lo autovectoriza con shuffles y no hay vld3 equivalente.
fn extract_green_channel_into(rgb: &[u16], out: &mut [u16]) {
    let n = out.len().min(rgb.len() / 3);
    #[cfg(target_arch = "aarch64")]
    {
        unsafe {
            use std::arch::aarch64::*;
            let mut i = 0usize;
            while i + 8 <= n {
                let v = vld3q_u16(rgb.as_ptr().add(i * 3));
                vst1q_u16(out.as_mut_ptr().add(i), v.1);
                i += 8;
            }
            for k in i..n {
                out[k] = rgb[k * 3 + 1];
            }
        }
        return;
    }
    #[cfg(not(target_arch = "aarch64"))]
    for k in 0..n {
        out[k] = rgb[k * 3 + 1];
    }
}

fn load_font_from_path(path: &str) -> Option<Font<'static>> {
    if let Ok(data) = fs::read(path) {
        if let Some(font) = Font::try_from_vec(data) {
            return Some(font);
        }
    }
    None
}

fn get_fallback_font() -> Option<Font<'static>> {
    let paths = [
        "assets/font.ttf",
        "/Library/Fonts/Arial.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Supplemental/Helvetica.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "C:\\Windows\\Fonts\\arial.ttf",
        "C:\\Windows\\Fonts\\segoeui.ttf",
    ];
    for p in paths {
        if let Some(f) = load_font_from_path(p) {
            return Some(f);
        }
    }
    None
}

fn get_font_map() -> Vec<(String, String)> {
    let candidates = [
        ("Arial", "C:\\Windows\\Fonts\\arial.ttf"),
        ("Segoe UI", "C:\\Windows\\Fonts\\segoeui.ttf"),
        ("Arial", "/Library/Fonts/Arial.ttf"),
        ("Arial", "/System/Library/Fonts/Supplemental/Arial.ttf"),
        (
            "Helvetica",
            "/System/Library/Fonts/Supplemental/Helvetica.ttf",
        ),
        ("DejaVu Sans", "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
        (
            "Liberation Sans",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        ),
    ];

    let mut available: Vec<(String, String)> = candidates
        .iter()
        .filter(|(_, path)| Path::new(path).exists())
        .map(|(name, path)| ((*name).to_string(), (*path).to_string()))
        .collect();

    if available.is_empty() {
        available.push(("Default".to_string(), "assets/font.ttf".to_string()));
    }

    available
}

fn resource_binary_names(stem: &str) -> Vec<String> {
    let mut names = Vec::new();

    if cfg!(target_os = "windows") {
        names.push(format!("{}.exe", stem));
        names.push(format!("{}-x86_64-pc-windows-msvc.exe", stem));
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            names.push(format!("{}-aarch64-apple-darwin", stem));
        } else if cfg!(target_arch = "x86_64") {
            names.push(format!("{}-x86_64-apple-darwin", stem));
        }
        names.push(stem.to_string());
    } else {
        names.push(stem.to_string());
    }

    names
}

fn resolve_bundled_binary(app: &tauri::AppHandle, stem: &str) -> Option<String> {
    for name in resource_binary_names(stem) {
        let rel_path = format!("bin/{}", name);
        if let Ok(path) = app
            .path()
            .resolve(&rel_path, tauri::path::BaseDirectory::Resource)
        {
            if path.exists() {
                return Some(clean_windows_path(path));
            }
        }
    }

    None
}

fn resolve_ffmpeg_tool(app: &tauri::AppHandle, stem: &str, env_var: &str) -> String {
    if let Ok(path) = std::env::var(env_var) {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    resolve_bundled_binary(app, stem).unwrap_or_else(|| stem.to_string())
}

fn ffmpeg_install_hint() -> String {
    let ffmpeg_names = resource_binary_names("ffmpeg").join("' o '");
    let ffprobe_names = resource_binary_names("ffprobe").join("' o '");

    format!(
        "Instala FFmpeg en el sistema o coloca '{}' y '{}' en src-tauri/bin para empaquetar.",
        ffmpeg_names, ffprobe_names
    )
}

fn get_ffmpeg_command(app: &tauri::AppHandle) -> String {
    resolve_ffmpeg_tool(app, "ffmpeg", "ZENITH_FFMPEG_PATH")
}

fn get_ffprobe_command(app: &tauri::AppHandle) -> String {
    resolve_ffmpeg_tool(app, "ffprobe", "ZENITH_FFPROBE_PATH")
}

fn apply_saturation_inplace(img: &mut image::DynamicImage, saturation: f32) {
    if let Some(rgb) = img.as_mut_rgb16() {
        for px in rgb.pixels_mut() {
            let r = px[0] as f32;
            let g = px[1] as f32;
            let b = px[2] as f32;
            let l = 0.299 * r + 0.587 * g + 0.114 * b;
            px[0] = (l + (r - l) * saturation).clamp(0.0, 65535.0) as u16;
            px[1] = (l + (g - l) * saturation).clamp(0.0, 65535.0) as u16;
            px[2] = (l + (b - l) * saturation).clamp(0.0, 65535.0) as u16;
        }
    } else if let Some(rgba) = img.as_mut_rgba16() {
        for px in rgba.pixels_mut() {
            let r = px[0] as f32;
            let g = px[1] as f32;
            let b = px[2] as f32;
            let l = 0.299 * r + 0.587 * g + 0.114 * b;
            px[0] = (l + (r - l) * saturation).clamp(0.0, 65535.0) as u16;
            px[1] = (l + (g - l) * saturation).clamp(0.0, 65535.0) as u16;
            px[2] = (l + (b - l) * saturation).clamp(0.0, 65535.0) as u16;
        }
    } else {
        let mut rgba = img.to_rgba8();
        for px in rgba.pixels_mut() {
            let r = px[0] as f32;
            let g = px[1] as f32;
            let b = px[2] as f32;
            let l = 0.299 * r + 0.587 * g + 0.114 * b;
            px[0] = (l + (r - l) * saturation).clamp(0.0, 255.0) as u8;
            px[1] = (l + (g - l) * saturation).clamp(0.0, 255.0) as u8;
            px[2] = (l + (b - l) * saturation).clamp(0.0, 255.0) as u8;
        }
        *img = image::DynamicImage::ImageRgba8(rgba);
    }
}

#[inline(always)]
fn get_pixel_value(data: &[u8], idx: usize, bpp: usize) -> u16 {
    if idx >= data.len() {
        return 0;
    }
    if bpp == 1 {
        unsafe { (*data.get_unchecked(idx) as u16) * 257 }
    } else if bpp == 2 {
        unsafe {
            if idx * 2 + 1 >= data.len() {
                return 0;
            }
            let start = idx * 2;
            let b1 = *data.get_unchecked(start) as u16;
            let b2 = *data.get_unchecked(start + 1) as u16;
            (b2 << 8) | b1
        }
    } else if bpp == 6 {
        unsafe {
            if idx * 6 + 5 >= data.len() {
                return 0;
            }
            let s = idx * 6;
            let r =
                ((*data.get_unchecked(s + 1) as u64) << 8 | *data.get_unchecked(s) as u64) as f32;
            let g = ((*data.get_unchecked(s + 3) as u64) << 8 | *data.get_unchecked(s + 2) as u64)
                as f32;
            let b = ((*data.get_unchecked(s + 5) as u64) << 8 | *data.get_unchecked(s + 4) as u64)
                as f32;
            (0.299 * r + 0.587 * g + 0.114 * b) as u16
        }
    } else {
        unsafe {
            if idx * 3 + 2 >= data.len() {
                return 0;
            }
            let start = idx * 3;
            let r = *data.get_unchecked(start) as u32;
            let g = *data.get_unchecked(start + 1) as u32;
            let b = *data.get_unchecked(start + 2) as u32;
            // Si bpp es 3, asumimos RGB 8-bit -> Escalar a u16
            ((r + g + b) / 3 * 257) as u16
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn raw_to_u16_buffer_avx2(data: &[u8], out_buf: &mut Vec<u16>, bpp: usize, size: usize) {
    use std::arch::x86_64::*;

    // Ensure capacity and set length
    out_buf.clear();
    if out_buf.capacity() < size {
        out_buf.reserve(size);
    }
    out_buf.set_len(size);

    let out_ptr = out_buf.as_mut_ptr();
    let in_ptr = data.as_ptr();

    if bpp == 1 || bpp == 3 {
        // Expansion 8-bit -> 16-bit (x257)
        // 16 pixels per iteration
        let mut i = 0;
        while i + 16 <= size {
            // Load 16 bytes (128-bit)
            let v_u8 = _mm_loadu_si128(in_ptr.add(i) as *const _);
            // Expand to 16 u16s (256-bit)
            let v_u16 = _mm256_cvtepu8_epi16(v_u8);
            // Multiply by 257: (x << 8) | x
            let v_hi = _mm256_slli_epi16(v_u16, 8);
            let v_res = _mm256_or_si256(v_hi, v_u16);
            // Store
            _mm256_storeu_si256(out_ptr.add(i) as *mut _, v_res);
            i += 16;
        }
        // Scalar Tail
        while i < size {
            *out_ptr.add(i) = (*in_ptr.add(i) as u16) * 257;
            i += 1;
        }
    } else if bpp == 2 || bpp == 6 {
        // 16-bit Direct Copy (Little Endian)
        // 16 pixels per iteration (32 bytes)
        let mut i = 0;
        while i + 16 <= size {
            let offset_bytes = i * 2;
            let v = _mm256_loadu_si256(in_ptr.add(offset_bytes) as *const _);
            _mm256_storeu_si256(out_ptr.add(i) as *mut _, v);
            i += 16;
        }
        // Scalar Tail
        while i < size {
            let offset = i * 2;
            let low = *in_ptr.add(offset) as u16;
            let high = *in_ptr.add(offset + 1) as u16;
            *out_ptr.add(i) = (high << 8) | low;
            i += 1;
        }
    } else {
        // Fallback for weird bpp (unlikely)
        for i in 0..size {
            *out_ptr.add(i) = get_pixel_value(data, i, bpp);
        }
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn raw_to_u16_buffer_neon(data: &[u8], out_buf: &mut Vec<u16>, bpp: usize, size: usize) {
    use std::arch::aarch64::*;

    // Ensure capacity and set length
    out_buf.clear();
    if out_buf.capacity() < size {
        out_buf.reserve(size);
    }
    out_buf.set_len(size);

    let out_ptr = out_buf.as_mut_ptr();
    let in_ptr = data.as_ptr();

    if bpp == 1 || bpp == 3 {
        // Expansion 8-bit -> 16-bit (x257)
        // 16 pixels per iteration
        let mut i = 0;
        while i + 16 <= size {
            // Load 16 bytes (128-bit)
            let v_u8 = vld1q_u8(in_ptr.add(i));
            // Expand to 16 u16s (2 x 128-bit = 256-bit total)
            let v_u16_low = vmovl_u8(vget_low_u8(v_u8));
            let v_u16_high = vmovl_u8(vget_high_u8(v_u8));

            // Multiply by 257: (x << 8) | x
            let v_hi_low = vshlq_n_u16(v_u16_low, 8);
            let v_res_low = vorrq_u16(v_hi_low, v_u16_low);

            let v_hi_high = vshlq_n_u16(v_u16_high, 8);
            let v_res_high = vorrq_u16(v_hi_high, v_u16_high);

            // Store
            vst1q_u16(out_ptr.add(i), v_res_low);
            vst1q_u16(out_ptr.add(i + 8), v_res_high);
            i += 16;
        }
        // Scalar Tail
        while i < size {
            *out_ptr.add(i) = (*in_ptr.add(i) as u16) * 257;
            i += 1;
        }
    } else if bpp == 2 || bpp == 6 {
        // 16-bit Direct Copy (Little Endian)
        // 16 pixels per iteration (32 bytes)
        let mut i = 0;
        while i + 16 <= size {
            let offset_bytes = i * 2;
            let v1 = vld1q_u8(in_ptr.add(offset_bytes));
            let v2 = vld1q_u8(in_ptr.add(offset_bytes + 16));
            vst1q_u8(out_ptr.add(i) as *mut u8, v1);
            vst1q_u8(out_ptr.add(i + 8) as *mut u8, v2);
            i += 16;
        }
        // Scalar Tail
        while i < size {
            let offset = i * 2;
            let low = *in_ptr.add(offset) as u16;
            let high = *in_ptr.add(offset + 1) as u16;
            *out_ptr.add(i) = (high << 8) | low;
            i += 1;
        }
    } else {
        // Fallback for weird bpp (unlikely)
        for i in 0..size {
            *out_ptr.add(i) = get_pixel_value(data, i, bpp);
        }
    }
}


fn raw_to_u16_buffer_into(
    data: &[u8],
    width: usize,
    height: usize,
    bpp: usize,
    out_buf: &mut Vec<u16>,
) {
    let is_rgb = bpp == 3 || bpp == 6;
    let size = if is_rgb {
        width * height * 3
    } else {
        width * height
    };

    // GUARD SIMD: los kernels AVX2/NEON leen `size` elementos confiando en los
    // metadatos (width*height), no en data.len(). Un SER/AVI truncado
    // (captura interrumpida) devuelve el ultimo frame corto o vacio (&[]) y la
    // lectura fuera de limites cerraba la app en seco (0xC0000005 / SIGSEGV)
    // sin pasar por el panic hook. Camino frio (solo frames incompletos):
    // convierte lo disponible y rellena el resto con negro.
    let elem_bytes: usize = if bpp == 2 || bpp == 6 { 2 } else { 1 };
    if data.len() < size * elem_bytes {
        out_buf.clear();
        out_buf.reserve(size);
        let avail = (data.len() / elem_bytes).min(size);
        if elem_bytes == 2 {
            for i in 0..avail {
                let s = i * 2;
                out_buf.push((data[s + 1] as u16) << 8 | data[s] as u16);
            }
        } else {
            for &v in data.iter().take(avail) {
                out_buf.push(v as u16 * 257);
            }
        }
        out_buf.resize(size, 0);
        return;
    }

    // AVX2 OPTIMIZATION CHECK
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe {
                raw_to_u16_buffer_avx2(data, out_buf, bpp, size);
            }
            return;
        }
    }

    // NEON OPTIMIZATION CHECK
    #[cfg(target_arch = "aarch64")]
    {
        unsafe {
            raw_to_u16_buffer_neon(data, out_buf, bpp, size);
        }
        return;
    }

    // SCALAR FALLBACK
    #[cfg(not(target_arch = "aarch64"))]
    {
        out_buf.clear();
        if out_buf.capacity() < size {
            out_buf.reserve(size);
        }

        if bpp == 6 {
            // Optimización para RGB 16-bit
            unsafe {
                for i in 0..size {
                    let s = i * 2;
                    if s + 1 < data.len() {
                        out_buf.push(
                            (*data.get_unchecked(s + 1) as u16) << 8 | *data.get_unchecked(s) as u16,
                        );
                    } else {
                        out_buf.push(0);
                    }
                }
            }
        } else if bpp == 3 {
            // RGB 8-bit: Direct mapping [0-255] -> [0-65535]
            for v in data.iter().take(size) {
                out_buf.push((*v as u16) * 257);
            }
        } else if bpp == 2 {
            // Mono/Bayer 16-bit (Little Endian)
            // size = width * height (pixels) -> input bytes = size * 2
            unsafe {
                // AVX2 O(1) Optimization zero-cost mapping
                let expected_bytes = size * 2;
                let copy_bytes = expected_bytes.min(data.len());

                // Fast resize the uninitialized buffer directly
                // Rust will optimize vec.set_len if capacity is sufficient
                let new_len = copy_bytes / 2;
                out_buf.set_len(new_len);

                std::ptr::copy_nonoverlapping(
                    data.as_ptr(),
                    out_buf.as_mut_ptr() as *mut u8,
                    copy_bytes,
                );

                // Pad if data was slightly short (rare but possible with truncation)
                while out_buf.len() < size {
                    out_buf.push(0);
                }
            }
        } else {
            // MONO 8-BIT -> U16
            // AVX2/Neon Optimization here is huge.
            #[cfg(target_arch = "x86_64")]
            {
                if is_x86_feature_detected!("avx2") {
                    unsafe {
                        raw_to_u16_buffer_avx2_mono8(data, out_buf);
                    }
                    return;
                }
            }

            // fallback scalar
            for i in 0..size {
                out_buf.push(get_pixel_value(data, i, bpp));
            }
        }
    }
}

// OPTIMIZATION: AVX2 for 8-bit Mono -> 16-bit Mono (Scaling * 257)
#[cfg(target_arch = "x86_64")]
unsafe fn raw_to_u16_buffer_avx2_mono8(data: &[u8], out_buf: &mut Vec<u16>) {
    use std::arch::x86_64::*;

    let len = data.len();
    out_buf.clear();
    if out_buf.capacity() < len {
        out_buf.reserve(len);
    }
    out_buf.set_len(len);

    let mut i = 0;

    // Process 32 pixels at once (load 256-bit YMM = 32 x u8)
    // Expand to 2 x YMM of u16.
    // Multiply by 257.
    // Store.

    // Note: _mm256_cvtepu8_epi16 expands low 16 u8 to 16 u16.

    while i + 32 <= len {
        let chunk = _mm256_loadu_si256(data.as_ptr().add(i) as *const __m256i);

        // Split into low and high 128-bit lanes extended to 256-bit u16
        let lower_16 = _mm256_cvtepu8_epi16(_mm256_castsi256_si128(chunk));
        let upper_16 = _mm256_cvtepu8_epi16(_mm256_extracti128_si256::<1>(chunk));

        // Multiplier 257 = (x << 8) | x
        // Or just multiply.
        // _mm256_mullo_epi16 keeps low 16 bits. 255 * 257 = 65535 (fits in u16).
        // So mullo is sufficient.
        let multiplier = _mm256_set1_epi16(257);

        let res_low = _mm256_mullo_epi16(lower_16, multiplier);
        let res_high = _mm256_mullo_epi16(upper_16, multiplier);

        // Store to out_buf (ptr is u16)
        _mm256_storeu_si256(out_buf.as_mut_ptr().add(i) as *mut __m256i, res_low);
        _mm256_storeu_si256(out_buf.as_mut_ptr().add(i + 16) as *mut __m256i, res_high);

        i += 32;
    }

    // Scalar tail
    for j in i..len {
        unsafe { *out_buf.as_mut_ptr().add(j) = (*data.get_unchecked(j) as u16) * 257 };
    }
}
// [calculate_noise_floor moved to smart_grid.rs]

fn raw_to_u16_buffer(data: &[u8], width: usize, height: usize, bpp: usize) -> Vec<u16> {
    let is_rgb = bpp == 3 || bpp == 6;
    let size = if is_rgb {
        width * height * 3
    } else {
        width * height
    };
    let mut buf = Vec::with_capacity(size);
    raw_to_u16_buffer_into(data, width, height, bpp, &mut buf);
    buf
}

fn estimate_raw_frame_signal(data: &[u8], width: usize, height: usize, bpp: usize) -> (u16, f32) {
    if data.is_empty() || width == 0 || height == 0 {
        return (0, 0.0);
    }

    let pixel_count = width.saturating_mul(height);
    if pixel_count == 0 {
        return (0, 0.0);
    }

    let max_samples = 65_536usize;
    let step = (pixel_count / max_samples).max(1);
    let mut max_v = 0u16;
    let mut sum = 0.0f32;
    let mut samples = 0usize;

    for idx in (0..pixel_count).step_by(step) {
        let v = get_pixel_value(data, idx, bpp);
        max_v = max_v.max(v);
        sum += v as f32;
        samples += 1;
    }

    let avg = if samples > 0 {
        sum / samples as f32
    } else {
        0.0
    };
    (max_v, avg)
}

fn select_signal_frame_index(
    reader: &VideoInput,
    width: usize,
    height: usize,
    bpp: usize,
    cid: i32,
    preferred_idx: usize,
) -> usize {
    let total = reader.frame_count();
    if total <= 1 {
        return 0;
    }

    // Native readers are cheap random access. FFmpeg random seeks are expensive,
    // pero devolver el preferido A CIEGAS era peligroso: si el total es una
    // estimación alta o el seek cae en EOF, la referencia queda NEGRA y todas
    // las puntuaciones del análisis salen 0. Se valida la señal del preferido
    // y sólo si está vacío se sondean unos pocos candidatos baratos.
    if reader.is_ffmpeg() {
        let preferred = preferred_idx.min(total - 1);
        let raw = reader.get_frame(preferred, cid);
        let (max_v, avg) = estimate_raw_frame_signal(&raw, width, height, bpp);
        if max_v as f32 + avg * 8.0 > 512.0 {
            return preferred;
        }
        let mut best_idx = preferred;
        let mut best_score = max_v as f32 + avg * 8.0;
        for idx in [total / 4, total / 10, (total * 3) / 4, 0] {
            let idx = idx.min(total - 1);
            if idx == preferred {
                continue;
            }
            let raw = reader.get_frame(idx, cid);
            let (max_v, avg) = estimate_raw_frame_signal(&raw, width, height, bpp);
            let score = max_v as f32 + avg * 8.0;
            if score > best_score {
                best_score = score;
                best_idx = idx;
            }
            // Con señal clara no hace falta seguir sondeando (cada probe es un
            // seek+spawn de FFmpeg).
            if best_score > 512.0 {
                break;
            }
        }
        return best_idx;
    }

    let mut candidates = vec![
        preferred_idx.min(total - 1),
        0,
        1.min(total - 1),
        2.min(total - 1),
        (total / 20).min(total - 1),
        (total / 10).min(total - 1),
        (total / 4).min(total - 1),
        (total / 2).min(total - 1),
        ((total * 3) / 4).min(total - 1),
    ];
    candidates.sort_unstable();
    candidates.dedup();

    let mut best_idx = preferred_idx.min(total - 1);
    let mut best_score = 0.0f32;
    let mut preferred_score = None;

    for idx in candidates {
        let raw = reader.get_frame(idx, cid);
        let (max_v, avg) = estimate_raw_frame_signal(&raw, width, height, bpp);
        let score = max_v as f32 + avg * 8.0;
        if idx == preferred_idx.min(total - 1) {
            preferred_score = Some(score);
        }
        if score > best_score {
            best_score = score;
            best_idx = idx;
        }
    }

    // Preserve the requested frame when it has comparable signal; otherwise
    // recover from black/dark startup frames.
    if let Some(score) = preferred_score {
        if score > 512.0 && score >= best_score * 0.75 {
            return preferred_idx.min(total - 1);
        }
    }

    best_idx
}

fn suggest_target_from_frame(input: &[u16]) -> String {
    if input.is_empty() {
        return "surface".to_string();
    }

    let max_v = input.iter().copied().max().unwrap_or(0);
    if max_v == 0 {
        return "surface".to_string();
    }

    let threshold = (max_v / 8).max(512);
    let signal_pixels = input.iter().filter(|&&v| v > threshold).count();
    let coverage = signal_pixels as f32 / input.len() as f32;

    if coverage < 0.12 {
        "planet_small".to_string()
    } else {
        "surface".to_string()
    }
}

// NUEVO: Helper para extraer ROI de buffer RAW (para optimizaciÃ³n extrema V2)
// Convierte siempre a MONO u16 (para alineaciÃ³n/calidad)
fn raw_to_u16_buffer_into_roi(
    data: &[u8],
    width: usize,
    _height: usize,
    bpp: usize,
    roi_x: usize,
    roi_y: usize,
    roi_w: usize,
    roi_h: usize,
    out_buf: &mut Vec<u16>,
) {
    let size = roi_w * roi_h; // Target MONO size

    out_buf.clear();
    if out_buf.capacity() < size {
        out_buf.reserve(size);
    }

    // Validate ROI
    if roi_x + roi_w > width {
        // Graceful fallback or panic? Graceful.
        out_buf.resize(size, 0);
        return;
    }

    if bpp == 6 {
        // GUARD: mismo caso que raw_to_u16_buffer_into — un frame truncado o
        // vacio (SER/AVI interrumpido) hacia que los get_unchecked de abajo
        // leyeran fuera de limites (cierre en seco). Tambien cubre un ROI que
        // exceda la altura real. El fallback negro es el mismo que el del ROI
        // horizontal invalido de arriba.
        if data.len() < (roi_y + roi_h) * width * 6 {
            out_buf.resize(size, 0);
            return;
        }
        // RGB 16-bit Optimized -> MONO (Average)
        // Access pattern: y from roi_y to roi_y + roi_h
        unsafe {
            let row_stride = width * 6; // 6 bytes per pixel
            for y in 0..roi_h {
                let row_start = (roi_y + y) * row_stride;

                for x in 0..roi_w {
                    // Simplified:
                    let abs_offset = row_start + (roi_x + x) * 6;

                    // R (Little Endian)
                    let r = (*data.get_unchecked(abs_offset + 1) as u32) << 8
                        | *data.get_unchecked(abs_offset) as u32;
                    // G
                    let g = (*data.get_unchecked(abs_offset + 3) as u32) << 8
                        | *data.get_unchecked(abs_offset + 2) as u32;
                    // B
                    let b = (*data.get_unchecked(abs_offset + 5) as u32) << 8
                        | *data.get_unchecked(abs_offset + 4) as u32;

                    // Mono Average
                    let mono = (r + g + b) / 3;
                    out_buf.push(mono as u16);
                }
            }
        }
    } else {
        // Generic Loop for Mono 8/16 or RGB 8
        // Relies on get_pixel_value which handles layout
        for y in 0..roi_h {
            let abs_y = roi_y + y;
            for x in 0..roi_w {
                let abs_x = roi_x + x;
                let pixel_idx = abs_y * width + abs_x;
                out_buf.push(get_pixel_value(data, pixel_idx, bpp));
            }
        }
    }
}

fn auto_color_balance(buffer: &mut [u16], width: usize, height: usize) {
    let mut sum_r = 0.0;
    let mut sum_g = 0.0;
    let mut sum_b = 0.0;
    let mut count = 0.0;
    let mut max_val = 0;
    for i in (0..buffer.len()).step_by(100) {
        unsafe {
            if i + 1 < buffer.len() {
                let v = *buffer.get_unchecked(i + 1);
                if v > max_val {
                    max_val = v;
                }
            }
        }
    }
    let threshold = (max_val as f32 * 0.15) as u16;
    let start_y = height / 4;
    let end_y = height * 3 / 4;
    let start_x = width / 4;
    let end_x = width * 3 / 4;
    let step = 8;
    for y in (start_y..end_y).step_by(step) {
        for x in (start_x..end_x).step_by(step) {
            let idx = (y * width + x) * 3;
            if idx + 2 < buffer.len() {
                unsafe {
                    let r = *buffer.get_unchecked(idx);
                    let g = *buffer.get_unchecked(idx + 1);
                    let b = *buffer.get_unchecked(idx + 2);
                    if g > threshold && g < 64000 {
                        sum_r += r as f32;
                        sum_g += g as f32;
                        sum_b += b as f32;
                        count += 1.0;
                    }
                }
            }
        }
    }
    if count < 50.0 {
        return;
    }
    let avg_r = sum_r / count;
    let avg_g = sum_g / count;
    let avg_b = sum_b / count;
    let gain_r = if avg_r > 1.0 { avg_g / avg_r } else { 1.0 };
    let gain_b = if avg_b > 1.0 { avg_g / avg_b } else { 1.0 };

    // HEADROOM FIX: never gain a channel above 1 (it clipped/burned bright
    // areas in the RGB preview); renormalize all three to preserve the ratio.
    let max_gain = gain_r.max(gain_b).max(1.0);
    let gain_r = gain_r / max_gain;
    let gain_g = 1.0 / max_gain;
    let gain_b = gain_b / max_gain;

    buffer.par_chunks_exact_mut(3).for_each(|pixel| {
        pixel[0] = (pixel[0] as f32 * gain_r + 0.5).min(65535.0) as u16;
        pixel[1] = (pixel[1] as f32 * gain_g + 0.5).min(65535.0) as u16;
        pixel[2] = (pixel[2] as f32 * gain_b + 0.5).min(65535.0) as u16;
    });
}

fn auto_detect_sigma(data: &[f32], width: usize, _height: usize) -> f32 {
    let mut max_val = 0.0;
    let mut max_idx = 0;
    for (i, &v) in data.iter().enumerate().step_by(2) {
        if v > max_val {
            max_val = v;
            max_idx = i;
        }
    }
    if max_val < 5000.0 {
        return 1.0;
    }

    let cx = max_idx % width;
    let cy = max_idx / width;
    let half_max = max_val / 2.0;
    let mut radius = 1.0;

    for x in cx..width {
        let idx = cy * width + x;
        if data[idx] < half_max {
            radius = (x - cx) as f32;
            break;
        }
    }
    let sigma = radius / 1.177;
    sigma.clamp(0.6, 2.5)
}



/// POST-STACK CHROMA NOISE REDUCTION (Mejora 5)
/// Smooths only the Cb/Cr chroma channels in YCbCr space while preserving
/// all luminance (Y) detail. This eliminates frame-to-frame chromatic
/// fluctuation noise that causes the "gummy/chicloso" look in bright areas.
///
/// `radius` controls the blur kernel size (1=3x3, 2=5x5). Use 1 for planets
/// (minimal chroma smoothing to preserve color edges), 2 for surface/solar
/// (more aggressive chroma denoising).
fn smooth_chroma_inplace(data: &mut [u16], width: usize, height: usize, radius: usize) {
    if width < 3 || height < 3 || radius == 0 {
        return;
    }

    let n_pixels = width * height;
    if data.len() < n_pixels * 3 {
        return;
    }

    // 1. Convert RGB → YCbCr (f32 for precision)
    let mut y_chan = vec![0.0f32; n_pixels];
    let mut cb_chan = vec![0.0f32; n_pixels];
    let mut cr_chan = vec![0.0f32; n_pixels];

    for i in 0..n_pixels {
        let r = data[i * 3] as f32;
        let g = data[i * 3 + 1] as f32;
        let b = data[i * 3 + 2] as f32;
        // ITU-R BT.601 conversion (standard for astro imaging)
        y_chan[i] = 0.299 * r + 0.587 * g + 0.114 * b;
        cb_chan[i] = -0.169 * r - 0.331 * g + 0.500 * b;
        cr_chan[i] = 0.500 * r - 0.419 * g - 0.081 * b;
    }

    // 2. Box blur Cb and Cr channels only (Y stays untouched = detail preserved)
    let mut cb_smooth = vec![0.0f32; n_pixels];
    let mut cr_smooth = vec![0.0f32; n_pixels];
    let r = radius as isize;

    for y in 0..height {
        for x in 0..width {
            let mut sum_cb = 0.0f32;
            let mut sum_cr = 0.0f32;
            let mut count = 0.0f32;

            for ky in -r..=r {
                let py = y as isize + ky;
                if py < 0 || py >= height as isize { continue; }
                let row = py as usize * width;
                for kx in -r..=r {
                    let px = x as isize + kx;
                    if px < 0 || px >= width as isize { continue; }
                    let idx = row + px as usize;
                    sum_cb += cb_chan[idx];
                    sum_cr += cr_chan[idx];
                    count += 1.0;
                }
            }

            let out_idx = y * width + x;
            if count > 0.0 {
                cb_smooth[out_idx] = sum_cb / count;
                cr_smooth[out_idx] = sum_cr / count;
            } else {
                cb_smooth[out_idx] = cb_chan[out_idx];
                cr_smooth[out_idx] = cr_chan[out_idx];
            }
        }
    }

    // 3. Convert YCbCr -> RGB (using original Y + smoothed Cb/Cr)
    for i in 0..n_pixels {
        let y = y_chan[i];
        let cb = cb_smooth[i];
        let cr = cr_smooth[i];
        let r = y + 1.402 * cr;
        let g = y - 0.344 * cb - 0.714 * cr;
        let b = y + 1.772 * cb;
        data[i * 3] = r.clamp(0.0, 65535.0) as u16;
        data[i * 3 + 1] = g.clamp(0.0, 65535.0) as u16;
        data[i * 3 + 2] = b.clamp(0.0, 65535.0) as u16;
    }
}

fn surface_luma_percentile_rgb(data: &[u16], percentile: usize) -> f32 {
    if data.len() < 3 {
        return 0.0;
    }

    let pixels = data.len() / 3;
    let step = (pixels / 250_000).max(1);
    let mut sample = Vec::with_capacity((pixels / step).max(1));

    for i in (0..pixels).step_by(step) {
        let off = i * 3;
        let luma = 0.299 * data[off] as f32
            + 0.587 * data[off + 1] as f32
            + 0.114 * data[off + 2] as f32;
        sample.push(luma);
    }

    if sample.is_empty() {
        return 0.0;
    }
    sample.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = (sample.len() * percentile / 100).min(sample.len() - 1);
    sample[idx]
}

/// Percentile over a single-channel u16 buffer (sampled).
fn surface_percentile_mono(data: &[u16], percentile: usize) -> f32 {
    if data.is_empty() {
        return 0.0;
    }
    let step = (data.len() / 250_000).max(1);
    let mut sample: Vec<u16> = data.iter().step_by(step).copied().collect();
    if sample.is_empty() {
        return 0.0;
    }
    sample.sort_unstable();
    let idx = (sample.len() * percentile / 100).min(sample.len() - 1);
    sample[idx] as f32
}

/// Mono counterpart of `normalize_surface_frame_exposure_inplace`: works on a
/// single-channel buffer (the mono stacking branch never builds RGB frames).
fn normalize_surface_frame_exposure_mono_inplace(data: &mut [u16], target_p90: f32) {
    if target_p90 < 1.0 || data.is_empty() {
        return;
    }
    let current_p90 = surface_percentile_mono(data, 90);
    if current_p90 < 1.0 {
        return;
    }
    let gain = (target_p90 / current_p90).clamp(0.70, 1.45);
    if (gain - 1.0).abs() < 0.003 {
        return;
    }
    for v in data.iter_mut() {
        *v = (*v as f32 * gain + 0.5).clamp(0.0, 65535.0) as u16;
    }
}

/// Percentil de luma SOLO sobre los píxeles del DISCO (por encima de un
/// suelo del 2 % del p99.9 muestreado): en un planeta pequeño el p90 global
/// cae en el cielo negro y no mide la exposición del objeto. Devuelve 0.0
/// si no hay suficientes píxeles con señal (sin disco → sin normalizar).
fn planetary_disc_luma_percentile_rgb(data: &[u16], percentile: usize) -> f32 {
    if data.len() < 3 {
        return 0.0;
    }
    let pixels = data.len() / 3;
    let step = (pixels / 250_000).max(1);
    let mut sample = Vec::with_capacity((pixels / step).max(1));
    for i in (0..pixels).step_by(step) {
        let off = i * 3;
        let luma = 0.299 * data[off] as f32
            + 0.587 * data[off + 1] as f32
            + 0.114 * data[off + 2] as f32;
        sample.push(luma);
    }
    planetary_disc_percentile_from_samples(sample, percentile)
}

/// Variante mono de `planetary_disc_luma_percentile_rgb`.
fn planetary_disc_percentile_mono(data: &[u16], percentile: usize) -> f32 {
    if data.is_empty() {
        return 0.0;
    }
    let step = (data.len() / 250_000).max(1);
    let sample: Vec<f32> = data.iter().step_by(step).map(|&v| v as f32).collect();
    planetary_disc_percentile_from_samples(sample, percentile)
}

fn planetary_disc_percentile_from_samples(mut sample: Vec<f32>, percentile: usize) -> f32 {
    if sample.is_empty() {
        return 0.0;
    }
    sample.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p999 = sample[(sample.len() * 999 / 1000).min(sample.len() - 1)];
    let floor = (p999 * 0.02).max(64.0);
    // sample está ordenado: los píxeles de disco son el sufijo >= floor.
    let first = sample.partition_point(|&v| v < floor);
    let disc = &sample[first..];
    if disc.len() < 64 {
        return 0.0;
    }
    disc[(disc.len() * percentile / 100).min(disc.len() - 1)]
}

/// PR-1.4: normalización de exposición per-frame para PLANETAS. Igual que
/// la de superficie pero midiendo el percentil solo en el disco. Sin esto,
/// con transparencia variable (nubes finas, extinción) los frames más
/// brillantes dominaban la media ponderada sesgando fotometría y contraste.
fn normalize_planetary_frame_exposure_mono_inplace(data: &mut [u16], target_p90: f32) {
    if target_p90 < 1.0 || data.is_empty() {
        return;
    }
    let current_p90 = planetary_disc_percentile_mono(data, 90);
    if current_p90 < 1.0 {
        return;
    }
    let gain = (target_p90 / current_p90).clamp(0.70, 1.45);
    if (gain - 1.0).abs() < 0.003 {
        return;
    }
    for v in data.iter_mut() {
        *v = (*v as f32 * gain + 0.5).clamp(0.0, 65535.0) as u16;
    }
}

/// Variante RGB de `normalize_planetary_frame_exposure_mono_inplace`.
fn normalize_planetary_frame_exposure_rgb_inplace(data: &mut [u16], target_p90: f32) {
    if target_p90 < 1.0 || data.len() < 3 {
        return;
    }
    let current_p90 = planetary_disc_luma_percentile_rgb(data, 90);
    if current_p90 < 1.0 {
        return;
    }
    let gain = (target_p90 / current_p90).clamp(0.78, 1.30);
    if (gain - 1.0).abs() < 0.003 {
        return;
    }
    for v in data.iter_mut() {
        *v = (*v as f32 * gain + 0.5).clamp(0.0, 65535.0) as u16;
    }
}

fn normalize_surface_frame_exposure_inplace(data: &mut [u16], target_p90: f32, is_mono: bool) {
    if target_p90 < 1.0 || data.len() < 3 {
        return;
    }

    let current_p90 = surface_luma_percentile_rgb(data, 90);
    if current_p90 < 1.0 {
        return;
    }

    let (lo, hi) = if is_mono { (0.70, 1.45) } else { (0.78, 1.30) };
    let gain = (target_p90 / current_p90).clamp(lo, hi);
    if (gain - 1.0).abs() < 0.003 {
        return;
    }

    for v in data.iter_mut() {
        *v = (*v as f32 * gain + 0.5).clamp(0.0, 65535.0) as u16;
    }
}
