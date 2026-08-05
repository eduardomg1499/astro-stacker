// ==========================================\n// 8. SMART AP GENERATOR (Integrated)\n// ==========================================\n
#[derive(serde::Serialize)]
struct SerProfileEstimate {
    profile: String,
    is_color: bool,
    color_id: i32,
    color_label: String,
    bit_depth: i32,
    ffmpeg_pix_fmt: String,
    bytes_per_frame: u64,
    estimated_size_bytes: u64,
    recommended: bool,
    supported: bool,
    reason: Option<String>,
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
    source_color_id: i32,
    source_color_pattern: String,
    source_is_float: bool,
    pix_fmt: String,
    conversion_policy: String,
    estimates: Vec<SerProfileEstimate>,
}

#[derive(Clone, Debug, Default)]
struct VideoPixelProbe {
    width: Option<usize>,
    height: Option<usize>,
    sample_bits: Option<usize>,
    pix_fmt: String,
    is_float: bool,
}

#[derive(Clone, Debug)]
struct SerSourceFormat {
    width: usize,
    height: usize,
    frame_count: usize,
    color_id: i32,
    sample_bits: usize,
    pix_fmt: String,
    is_float: bool,
}

fn pix_fmt_float_bits(pix_fmt: &str, sample_fmt: &str) -> Option<usize> {
    let value = format!("{} {}", pix_fmt, sample_fmt).to_ascii_lowercase();
    if value.contains("f64") || value.contains("dbl") {
        Some(64)
    } else if value.contains("f32") || value.contains("flt") || value.contains("float") {
        Some(32)
    } else if value.contains("f16") {
        Some(16)
    } else {
        None
    }
}

fn infer_integer_sample_bits(pix_fmt: &str) -> Option<usize> {
    let format = pix_fmt.trim().to_ascii_lowercase();
    if format.is_empty() || format == "unknown" {
        return None;
    }
    if format == "rgb48le"
        || format == "rgb48be"
        || format == "bgr48le"
        || format == "bgr48be"
        || format.contains("rgba64")
        || format.contains("bgra64")
    {
        return Some(16);
    }
    for bits in [32usize, 16, 14, 12, 10, 9] {
        if format.contains(&bits.to_string()) {
            return Some(bits);
        }
    }
    Some(8)
}

fn resolve_probe_sample_bits(
    pix_fmt: &str,
    sample_fmt: &str,
    raw_bits: Option<usize>,
) -> (Option<usize>, bool) {
    if let Some(float_bits) = pix_fmt_float_bits(pix_fmt, sample_fmt) {
        return (Some(float_bits), true);
    }
    let inferred = infer_integer_sample_bits(pix_fmt);
    // Para contenedores de 16 bits (p.ej. bayer_rggb16le) el campo
    // bits_per_raw_sample puede declarar que la cámara sólo utilizó 9/10/12/14
    // bits. En formatos nominales explícitos (gray10, p010, bayer8), el pix_fmt
    // es autoritativo y evita confundir bits de almacenamiento con bits/sample.
    let resolved = if inferred == Some(16) {
        raw_bits
            .filter(|&bits| (9..=16).contains(&bits))
            .or(inferred)
    } else {
        inferred.or(raw_bits)
    };
    (resolved, false)
}

fn probe_video_pixels(
    app: &tauri::AppHandle,
    input_path: &str,
    cancel: Option<&FfmpegCancelCheck>,
) -> Result<VideoPixelProbe, String> {
    let ffprobe_path = get_ffprobe_command(app);
    let output = {
        let mut cmd = std::process::Command::new(&ffprobe_path);
        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x08000000);
        cmd.args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,pix_fmt,bits_per_raw_sample,sample_fmt",
            "-of",
            "json",
            input_path,
        ]);
        run_command_supervised(
            cmd,
            FFPROBE_METADATA_TIMEOUT,
            cancel,
            "FFprobe del conversor SER",
        )
    }
    .map_err(|error| format!("No se pudo ejecutar FFprobe para el conversor SER: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "FFprobe no pudo inspeccionar el formato de píxel: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Respuesta FFprobe inválida: {error}"))?;
    let stream = json["streams"]
        .as_array()
        .and_then(|streams| streams.first())
        .ok_or_else(|| "FFprobe no devolvió un stream de video".to_string())?;
    let pix_fmt = stream["pix_fmt"]
        .as_str()
        .unwrap_or("unknown")
        .trim()
        .to_ascii_lowercase();
    let sample_fmt = stream["sample_fmt"].as_str().unwrap_or("");
    let raw_bits = stream["bits_per_raw_sample"]
        .as_str()
        .and_then(|value| value.parse::<usize>().ok())
        .or_else(|| {
            stream["bits_per_raw_sample"]
                .as_u64()
                .map(|value| value as usize)
        })
        .filter(|&bits| bits > 0);
    let (sample_bits, is_float) = resolve_probe_sample_bits(&pix_fmt, sample_fmt, raw_bits);

    Ok(VideoPixelProbe {
        width: stream["width"]
            .as_u64()
            .map(|value| value as usize)
            .filter(|&v| v > 0),
        height: stream["height"]
            .as_u64()
            .map(|value| value as usize)
            .filter(|&v| v > 0),
        sample_bits,
        pix_fmt,
        is_float,
    })
}

fn bayer_color_id_from_pix_fmt(pix_fmt: &str) -> Option<i32> {
    let format = pix_fmt.to_ascii_lowercase();
    if format.starts_with("bayer_rggb") {
        Some(8)
    } else if format.starts_with("bayer_grbg") {
        Some(9)
    } else if format.starts_with("bayer_gbrg") {
        Some(10)
    } else if format.starts_with("bayer_bggr") {
        Some(11)
    } else {
        None
    }
}

fn pix_fmt_is_single_plane(pix_fmt: &str) -> bool {
    pix_fmt.starts_with("gray") || pix_fmt.starts_with("bayer_")
}

fn resolve_converter_color_id(
    reported_color_id: i32,
    reported_is_color: bool,
    pix_fmt: &str,
) -> Result<i32, String> {
    if let Some(probed_bayer) = bayer_color_id_from_pix_fmt(pix_fmt) {
        return Ok(probed_bayer);
    }
    if (8..=11).contains(&reported_color_id) {
        if pix_fmt_is_single_plane(pix_fmt) || pix_fmt == "unknown" {
            return Ok(reported_color_id);
        }
        return Err(format!(
            "Metadatos CFA contradictorios: ColorID {} pero FFmpeg entrega {}. No se reetiquetará RGB/YUV como Bayer",
            reported_color_id, pix_fmt
        ));
    }
    if pix_fmt_is_single_plane(pix_fmt) {
        return Ok(0);
    }
    if pix_fmt != "unknown" {
        return Ok(100);
    }
    Ok(if reported_color_id == 0 && !reported_is_color {
        0
    } else {
        100
    })
}

fn source_format_from_input(
    input: &VideoInput,
    probe: VideoPixelProbe,
) -> Result<SerSourceFormat, String> {
    let color_id = resolve_converter_color_id(input.color_id(), input.is_color(), &probe.pix_fmt)?;
    let sample_bits = probe
        .sample_bits
        .unwrap_or_else(|| input.sample_bits())
        .max(1);
    if !probe.is_float && sample_bits > 16 {
        return Err(format!(
            "Profundidad entera de {sample_bits} bits no representable en SER"
        ));
    }
    Ok(SerSourceFormat {
        // SER conserva el raster codificado. No aplica DAR, resize ni
        // autorrotación: esas operaciones desplazan la fase CFA o interpolan.
        width: probe.width.unwrap_or_else(|| input.width()),
        height: probe.height.unwrap_or_else(|| input.height()),
        frame_count: input.frame_count(),
        color_id,
        sample_bits,
        pix_fmt: probe.pix_fmt,
        is_float: probe.is_float,
    })
}

fn exact_single_plane_pix_fmt(source: &SerSourceFormat) -> Option<String> {
    let format = source.pix_fmt.as_str();
    let allowed = matches!(
        format,
        "gray"
            | "gray8"
            | "gray9le"
            | "gray9be"
            | "gray10le"
            | "gray10be"
            | "gray12le"
            | "gray12be"
            | "gray14le"
            | "gray14be"
            | "gray16le"
            | "gray16be"
            | "bayer_rggb8"
            | "bayer_grbg8"
            | "bayer_gbrg8"
            | "bayer_bggr8"
            | "bayer_rggb16le"
            | "bayer_rggb16be"
            | "bayer_grbg16le"
            | "bayer_grbg16be"
            | "bayer_gbrg16le"
            | "bayer_gbrg16be"
            | "bayer_bggr16le"
            | "bayer_bggr16be"
    );
    allowed.then(|| source.pix_fmt.clone())
}

fn plan_ser_profile(
    requested_profile: &str,
    source: &SerSourceFormat,
) -> Result<converter::SerConversionSpec, String> {
    if source.is_float {
        return Err(
            "La fuente usa muestras float. SER sólo admite enteros de hasta 16 bits; se rechaza la cuantización implícita. Conserva la secuencia como FITS float o conviértela explícitamente con rango definido."
                .to_string(),
        );
    }

    let profile = requested_profile.trim().to_ascii_lowercase();
    let is_bayer = (8..=11).contains(&source.color_id);
    let pattern = crate::ser::ser_pattern_name(source.color_id);
    if is_bayer {
        let exact_format = exact_single_plane_pix_fmt(source).ok_or_else(|| {
            format!(
                "El CFA {pattern} llega como {}, formato que FFmpeg no puede publicar bit-exacto en SER",
                source.pix_fmt
            )
        })?;
        let storage_is_8bit =
            exact_format == "gray" || exact_format == "gray8" || exact_format.ends_with('8');
        match profile.as_str() {
            "autodepth" | "auto_depth" | "auto" => converter::SerConversionSpec::new(
                source.color_id,
                source.sample_bits,
                exact_format,
                format!("CFA {pattern} nativo {}-bit", source.sample_bits),
            ),
            "color8" if storage_is_8bit && source.sample_bits <= 8 => {
                converter::SerConversionSpec::new(
                    source.color_id,
                    source.sample_bits,
                    exact_format,
                    format!("CFA {pattern} nativo {}-bit", source.sample_bits),
                )
            }
            "color8" => Err(format!(
                "Reducir el CFA {pattern} de {} a 8 bits no es bit-exacto y FFmpeg lo demosaica; usa Alta profundidad",
                source.sample_bits
            )),
            _ => Err(format!(
                "El perfil mono destruiría la matriz CFA {pattern}; usa Alta profundidad para conservar los fotositos"
            )),
        }
    } else if source.color_id == 0 {
        match profile.as_str() {
            "autodepth" | "auto_depth" | "auto" => {
                if let Some(exact_format) = exact_single_plane_pix_fmt(source) {
                    if let Ok(spec) = converter::SerConversionSpec::new(
                        0,
                        source.sample_bits,
                        exact_format,
                        format!("Mono nativo {}-bit", source.sample_bits),
                    ) {
                        return Ok(spec);
                    }
                }
                if source.sample_bits <= 8 {
                    converter::SerConversionSpec::new(0, 8, "gray", "Mono 8-bit")
                } else {
                    converter::SerConversionSpec::new(0, 16, "gray16le", "Mono 16-bit")
                }
            }
            "mono16" => converter::SerConversionSpec::new(0, 16, "gray16le", "Mono 16-bit"),
            "mono8" | "color8" | _ => converter::SerConversionSpec::new(0, 8, "gray", "Mono 8-bit"),
        }
    } else {
        match profile.as_str() {
            "mono16" => converter::SerConversionSpec::new(0, 16, "gray16le", "Mono 16-bit"),
            "mono8" => converter::SerConversionSpec::new(0, 8, "gray", "Mono 8-bit"),
            "color8" => converter::SerConversionSpec::new(100, 8, "rgb24", "Color RGB 8-bit"),
            "autodepth" | "auto_depth" | "auto" if source.sample_bits <= 8 => {
                converter::SerConversionSpec::new(100, 8, "rgb24", "Color RGB 8-bit")
            }
            "autodepth" | "auto_depth" | "auto" => converter::SerConversionSpec::new(
                100,
                16,
                "rgb48le",
                format!("Color RGB 16-bit (fuente {}-bit)", source.sample_bits),
            ),
            _ => converter::SerConversionSpec::new(0, 8, "gray", "Mono 8-bit"),
        }
    }
}

fn estimate_ser_profile(profile: &str, source: &SerSourceFormat) -> SerProfileEstimate {
    match plan_ser_profile(profile, source) {
        Ok(spec) => {
            let bytes_per_frame = (source.width as u64)
                .saturating_mul(source.height as u64)
                .saturating_mul(
                    crate::ser::ser_bytes_per_pixel(spec.color_id, spec.pixel_depth) as u64,
                );
            SerProfileEstimate {
                profile: profile.to_string(),
                is_color: spec.is_color(),
                color_id: spec.color_id,
                color_label: crate::ser::ser_pattern_name(spec.color_id).to_string(),
                bit_depth: spec.pixel_depth as i32,
                ffmpeg_pix_fmt: spec.ffmpeg_pix_fmt,
                bytes_per_frame,
                estimated_size_bytes: 178u64
                    .saturating_add(bytes_per_frame.saturating_mul(source.frame_count as u64)),
                recommended: profile.eq_ignore_ascii_case("autoDepth"),
                supported: true,
                reason: None,
            }
        }
        Err(reason) => SerProfileEstimate {
            profile: profile.to_string(),
            is_color: crate::ser::ser_color_is_color(source.color_id),
            color_id: source.color_id,
            color_label: crate::ser::ser_pattern_name(source.color_id).to_string(),
            bit_depth: source.sample_bits.min(i32::MAX as usize) as i32,
            ffmpeg_pix_fmt: source.pix_fmt.clone(),
            bytes_per_frame: 0,
            estimated_size_bytes: 0,
            recommended: false,
            supported: false,
            reason: Some(reason),
        },
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
    let source = source_format_from_input(&input, probe_video_pixels(&app, &input_path, None)?)?;
    let source_is_color = crate::ser::ser_color_is_color(source.color_id);

    Ok(SerConversionPreflight {
        input_path: input_path.clone(),
        file_name: p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("video")
            .to_string(),
        source_size_bytes: std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
        width: source.width,
        height: source.height,
        frame_count: source.frame_count,
        source_is_color,
        source_bit_depth: source.sample_bits.min(i32::MAX as usize) as i32,
        source_color_id: source.color_id,
        source_color_pattern: crate::ser::ser_pattern_name(source.color_id).to_string(),
        source_is_float: source.is_float,
        pix_fmt: source.pix_fmt.clone(),
        conversion_policy: if source.is_float {
            "Float no se cuantiza implícitamente: conservar como FITS float".to_string()
        } else if (8..=11).contains(&source.color_id) {
            "CFA se conserva bit-exacto, sin demosaic, resize ni autorrotación".to_string()
        } else {
            "SER conserva el raster codificado; Auto mantiene la máxima profundidad representable"
                .to_string()
        },
        estimates: vec![
            estimate_ser_profile("mono8", &source),
            estimate_ser_profile("color8", &source),
            estimate_ser_profile("autoDepth", &source),
        ],
    })
}

#[tauri::command]
async fn convert_video_to_ser_frontend(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
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

    // La generación nace antes de cualquier FFprobe/apertura potencialmente
    // lenta. Así Cancelar y una operación posterior invalidan también esta
    // fase, no sólo el pipe de conversión que se crea después.
    let request_id = begin_planetary_user_job(&state);
    let token = PlanetaryJobToken::for_app(&app, request_id, state.cancel_requested.clone());
    let probe_cancel: FfmpegCancelCheck = {
        let token = token.clone();
        std::sync::Arc::new(move || token.is_cancelled())
    };

    // 1. Open Video to get properties
    let input = VideoInput::open_cancelable(&input_path, &app, probe_cancel.clone())
        .map_err(|e| e.to_string())?;

    let source = source_format_from_input(
        &input,
        probe_video_pixels(&app, &input_path, Some(&probe_cancel))?,
    )?;

    // 2. Get FFmpeg Path (Reusing helper from FfmpegReader)
    let ffmpeg_path = get_ffmpeg_command(&app);

    let requested_profile = profile
        .unwrap_or_else(|| "autoDepth".to_string())
        .trim()
        .to_lowercase();
    let conversion_spec = plan_ser_profile(&requested_profile, &source)?;

    // 3. Run Conversion (Blocking to avoid freezing async runtime)
    let app_handle = app.clone();
    let p_clone = input_path.clone();
    let o_clone = out_path.clone();

    let result = tauri::async_runtime::spawn_blocking(move || {
        converter::convert_video_to_ser(
            &p_clone,
            &o_clone,
            source.width,
            source.height,
            source.frame_count,
            conversion_spec,
            &ffmpeg_path,
            &app_handle,
            token,
        )
    })
    .await
    .map_err(|e| e.to_string())??;

    Ok(result)
}

#[cfg(test)]
mod ser_conversion_policy_tests {
    use super::*;

    fn source(color_id: i32, sample_bits: usize, pix_fmt: &str) -> SerSourceFormat {
        SerSourceFormat {
            width: 640,
            height: 480,
            frame_count: 12,
            color_id,
            sample_bits,
            pix_fmt: pix_fmt.to_string(),
            is_float: false,
        }
    }

    #[test]
    fn pixel_probe_policy_detects_nine_bit_and_float_formats() {
        assert_eq!(infer_integer_sample_bits("gray9le"), Some(9));
        assert_eq!(infer_integer_sample_bits("yuv420p10le"), Some(10));
        assert_eq!(infer_integer_sample_bits("rgb48le"), Some(16));
        assert_eq!(pix_fmt_float_bits("gbrpf32le", ""), Some(32));
        assert_eq!(pix_fmt_float_bits("grayf32le", "flt"), Some(32));
        assert_eq!(
            resolve_probe_sample_bits("gray9le", "", Some(16)),
            (Some(9), false)
        );
        assert_eq!(
            resolve_probe_sample_bits("bayer_rggb16le", "", Some(9)),
            (Some(9), false)
        );
        assert_eq!(
            resolve_probe_sample_bits("yuv420p", "", Some(10)),
            (Some(8), false)
        );
        assert_eq!(
            resolve_probe_sample_bits("gbrpf32le", "", Some(16)),
            (Some(32), true)
        );
    }

    #[test]
    fn pix_fmt_authoritatively_resolves_every_bayer_pattern() {
        for (format, expected) in [
            ("bayer_rggb8", 8),
            ("bayer_grbg16le", 9),
            ("bayer_gbrg8", 10),
            ("bayer_bggr16be", 11),
        ] {
            assert_eq!(
                resolve_converter_color_id(100, true, format).unwrap(),
                expected
            );
        }
        assert!(resolve_converter_color_id(8, false, "yuv420p").is_err());
    }

    #[test]
    fn autodepth_preserves_cfa_nine_bit_contract_without_demosaic() {
        let spec = plan_ser_profile("autoDepth", &source(8, 9, "bayer_rggb16le")).unwrap();
        assert_eq!(spec.color_id, 8);
        assert_eq!(spec.pixel_depth, 9);
        assert_eq!(spec.ffmpeg_pix_fmt, "bayer_rggb16le");
        assert!(plan_ser_profile("mono8", &source(8, 9, "bayer_rggb16le")).is_err());
        assert!(plan_ser_profile("color8", &source(8, 9, "bayer_rggb16le")).is_err());
    }

    #[test]
    fn autodepth_promotes_processed_ten_bit_color_to_truthful_rgb16() {
        let spec = plan_ser_profile("autoDepth", &source(100, 10, "yuv420p10le")).unwrap();
        assert_eq!(spec.color_id, 100);
        assert_eq!(spec.pixel_depth, 16);
        assert_eq!(spec.ffmpeg_pix_fmt, "rgb48le");
    }

    #[test]
    fn float_source_requires_explicit_fits_policy() {
        let mut float_source = source(100, 32, "gbrpf32le");
        float_source.is_float = true;
        let error = plan_ser_profile("autoDepth", &float_source).unwrap_err();
        assert!(error.contains("FITS float"));
    }
}

// ==========================================
// 8. SMART AP GENERATOR (Integrated)
// ==========================================

#[derive(Clone, Debug, PartialEq, Eq)]
struct SmartApReferenceKey {
    source_key: u64,
    frame_idx: usize,
    width: usize,
    height: usize,
    color_id: i32,
}

#[derive(Clone)]
struct SmartApReferenceCache {
    key: SmartApReferenceKey,
    mono: Arc<Vec<u16>>,
}

/// Cache de un solo frame de referencia. Cambiar tamano/umbral de AP no debe
/// volver a abrir FFprobe ni atravesar otra vez un GOP largo desde el frame 0.
/// Se conserva luma u16 (2 bytes/pixel), no RGB (6 bytes/pixel).
static SMART_AP_REFERENCE_CACHE: std::sync::OnceLock<Mutex<Option<SmartApReferenceCache>>> =
    std::sync::OnceLock::new();
const SMART_AP_DISK_CACHE_BUDGET: u64 = 1024 * 1024 * 1024;

fn smart_ap_disk_cache_dir() -> PathBuf {
    std::env::temp_dir().join("astro_stacker_smart_ap_cache")
}

fn smart_ap_reference_disk_path(key: &SmartApReferenceKey) -> PathBuf {
    smart_ap_disk_cache_dir().join(format!(
        "smartap_{:016x}_{}_{}_{}_{}.lz4",
        key.source_key, key.frame_idx, key.width, key.height, key.color_id
    ))
}

fn prune_smart_ap_disk_cache(dir: &Path, max_bytes: u64) {
    let mut entries: Vec<(std::time::SystemTime, u64, PathBuf)> = std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let metadata = entry.metadata().ok()?;
            let name = path.file_name()?.to_str()?;
            if metadata.is_file()
                && path.extension().and_then(|value| value.to_str()) == Some("lz4")
                && name.starts_with("smartap_")
            {
                Some((metadata.modified().ok()?, metadata.len(), path))
            } else {
                None
            }
        })
        .collect();
    let mut total: u64 = entries.iter().map(|entry| entry.1).sum();
    if total <= max_bytes {
        return;
    }
    entries.sort_unstable_by_key(|entry| entry.0);
    for (_, bytes, path) in entries {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(bytes);
        }
    }
}

fn publish_smart_ap_reference_disk(key: &SmartApReferenceKey, mono: &[u16]) {
    // Separado del caché RGB48 todo-o-nada: podar una referencia mono de
    // ~37 MiB nunca debe abrir un hueco en un conjunto FFmpeg completo de
    // varios GiB, ni un snapshot de disco=0 debe borrar ambos cachés.
    let cache_dir = smart_ap_disk_cache_dir();
    let _ = std::fs::create_dir_all(&cache_dir);
    let path = smart_ap_reference_disk_path(key);
    if path.exists() {
        if read_cached_frame(&path, mono.len()).is_some() {
            return;
        }
        let _ = std::fs::remove_file(&path);
    }
    if let Some(compressed) = encode_cached_frame(mono) {
        let bytes = compressed.len() as u64;
        if bytes <= SMART_AP_DISK_CACHE_BUDGET {
            prune_smart_ap_disk_cache(
                &cache_dir,
                SMART_AP_DISK_CACHE_BUDGET.saturating_sub(bytes),
            );
            let _ = write_cache_bytes_atomically(&path, &compressed);
        }
    }
}

/// Recupera el mejor frame mono ANTES de abrir VideoInput/FFprobe. El nombre
/// contiene geometría y ColorID y el payload conserva magic+CRC+longitud, por
/// lo que un archivo viejo/corrupto sólo se ignora. Esto hace que un hit del
/// análisis persistente pueda generar la malla sin volver a recorrer el MOV.
fn load_smart_ap_reference_disk(
    source_key: u64,
    frame_idx: usize,
) -> Option<(SmartApReferenceKey, Vec<u16>)> {
    let cache_dir = smart_ap_disk_cache_dir();
    let prefix = format!("smartap_{source_key:016x}_{frame_idx}_");
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&cache_dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if !name.starts_with(&prefix) || path.extension()?.to_str()? != "lz4" {
                return None;
            }
            Some((entry.metadata().ok()?.modified().ok()?, path))
        })
        .collect();
    candidates.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in candidates {
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(tail) = stem.strip_prefix(&prefix) else {
            continue;
        };
        let mut fields = tail.split('_');
        let Some(width) = fields.next().and_then(|value| value.parse::<usize>().ok()) else {
            continue;
        };
        let Some(height) = fields.next().and_then(|value| value.parse::<usize>().ok()) else {
            continue;
        };
        let Some(color_id) = fields.next().and_then(|value| value.parse::<i32>().ok()) else {
            continue;
        };
        if fields.next().is_some() || width == 0 || height == 0 {
            continue;
        }
        let Some(pixels) = width.checked_mul(height) else {
            continue;
        };
        if let Some(mono) = read_cached_frame(&path, pixels) {
            return Some((
                SmartApReferenceKey {
                    source_key,
                    frame_idx,
                    width,
                    height,
                    color_id,
                },
                mono,
            ));
        }
    }
    None
}

fn smart_ap_source_key(path: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    if let Ok(metadata) = std::fs::metadata(path) {
        metadata.len().hash(&mut hasher);
        metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
            .hash(&mut hasher);
    }
    hasher.finish()
}

/// Reproduce exactamente la semantica historica del Smart Grid: RGB usa el
/// canal verde, mono/CFA usa su unico plano y 8-bit se expande con x257.
/// Hacerlo directamente evita el Vec RGB u16 intermedio de 6 bytes/pixel.
fn smart_ap_mono_from_raw(raw: &[u8], pixels: usize, bpp: usize) -> Result<Vec<u16>, String> {
    let required = pixels
        .checked_mul(bpp)
        .ok_or_else(|| "Overflow al dimensionar el frame de Smart AP".to_string())?;
    if raw.len() < required {
        return Err(format!(
            "Frame de referencia truncado para Smart AP: {} bytes; se esperaban {required}",
            raw.len()
        ));
    }

    let mono = match bpp {
        6 => raw[..required]
            .par_chunks_exact(6)
            .map(|pixel| u16::from_le_bytes([pixel[2], pixel[3]]))
            .collect(),
        3 => raw[..required]
            .par_chunks_exact(3)
            .map(|pixel| pixel[1] as u16 * 257)
            .collect(),
        2 => raw[..required]
            .par_chunks_exact(2)
            .map(|sample| u16::from_le_bytes([sample[0], sample[1]]))
            .collect(),
        1 => raw[..required]
            .par_iter()
            .map(|&sample| sample as u16 * 257)
            .collect(),
        _ => {
            return Err(format!(
                "Formato de {bpp} bytes/pixel no soportado por Smart AP"
            ))
        }
    };
    Ok(mono)
}

fn smart_ap_mono_from_cached_u16(
    cached: Vec<u16>,
    pixels: usize,
    is_color: bool,
) -> Option<Vec<u16>> {
    if is_color {
        if cached.len() != pixels.checked_mul(3)? {
            return None;
        }
        Some(cached.par_chunks_exact(3).map(|pixel| pixel[1]).collect())
    } else if cached.len() == pixels {
        Some(cached)
    } else {
        None
    }
}

/// Publica el best frame que el analisis YA tiene en RAM. `u16_data` puede ser
/// mono/CFA (w*h) o RGB interleaved (3*w*h); el cache bounded conserva un unico
/// plano mono u16. La siguiente generacion de malla no abre FFprobe ni decodifica.
fn cache_smart_grid_frame(
    path: &str,
    frame_idx: usize,
    width: usize,
    height: usize,
    color_id: i32,
    u16_data: &[u16],
) -> Result<(), String> {
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| "Overflow en la geometria del cache Smart AP".to_string())?;
    if pixels == 0 {
        return Err("No se puede cachear un frame Smart AP vacio".into());
    }
    let mono = if u16_data.len() == pixels {
        u16_data.to_vec()
    } else if u16_data.len() == pixels.saturating_mul(3) {
        u16_data.par_chunks_exact(3).map(|pixel| pixel[1]).collect()
    } else {
        return Err(format!(
            "Frame Smart AP incompatible: {} samples para {width}x{height} (ColorID {color_id})",
            u16_data.len()
        ));
    };
    let key = SmartApReferenceKey {
        source_key: smart_ap_source_key(path),
        frame_idx,
        width,
        height,
        color_id,
    };
    publish_smart_ap_reference_disk(&key, &mono);
    let entry = SmartApReferenceCache {
        key,
        mono: Arc::new(mono),
    };
    *SMART_AP_REFERENCE_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(entry);
    Ok(())
}

/// El analisis FFmpeg ya publico los frames decodificados cuando el presupuesto
/// NVMe lo permitio. Consultar ese cache antes de `get_frame` elimina una pasada
/// completa hasta `best_frame_idx` sin cambiar un solo sample.
fn smart_ap_try_shared_decode_cache(
    reader: &FfmpegReader,
    path: &str,
    width: usize,
    height: usize,
    bpp: usize,
    color_id: i32,
    frame_idx: usize,
) -> Option<Vec<u16>> {
    let pixels = width.checked_mul(height)?;
    let expected_len = pixels.checked_mul(if ffmpeg_stream_is_color(color_id) {
        3
    } else {
        1
    })?;
    let cache_dir = std::env::temp_dir().join("astro_stacker_cache");

    // No repetir aqui el micro-benchmark GPU/CPU (hasta 40 s si el usuario
    // llega desde un cache de analisis de otra sesion). La clave ya codifica la
    // ruta; probar el conjunto finito de backends son simples `stat()` y se usa
    // la entrada mas reciente, que corresponde a la ultima pasada valida.
    const ROUTES: [Option<&str>; 8] = [
        None,
        Some("videotoolbox"),
        Some("d3d11va"),
        Some("dxva2"),
        Some("cuda"),
        Some("qsv"),
        Some("vaapi"),
        Some("vulkan"),
    ];
    let cache_path = ROUTES
        .into_iter()
        .filter_map(|backend| {
            let route = ffmpeg_decode_route_label(&reader.ffmpeg_path, backend);
            let key = ffmpeg_decode_cache_key(
                path,
                width,
                height,
                bpp,
                color_id,
                reader.rotation,
                &reader.codec_name,
                &route,
            );
            let candidate = decode_cache_frame_path(&cache_dir, key, frame_idx);
            let modified = std::fs::metadata(&candidate).ok()?.modified().ok()?;
            Some((modified, candidate))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)?;
    read_cached_frame(&cache_path, expected_len).and_then(|cached| {
        smart_ap_mono_from_cached_u16(cached, pixels, ffmpeg_stream_is_color(color_id))
    })
}

#[derive(Clone, Copy)]
struct SmartApCandidate {
    x: usize,
    y: usize,
    start_x: usize,
    start_y: usize,
    width: usize,
    height: usize,
}

/// Evaluador nativo exacto sin SAT gigante. La malla tiene como maximo ~14k
/// candidatos y cada AP lee su parche original; incluso con el solape 0.67x
/// esto recorre ~2.2 veces el raster, pero evita asignar/escribir `sum+sq_sum`
/// (16 bytes/pixel, 320 MB para 20 MP). Rayon conserva el orden del iterador
/// indexado, por lo que la salida es determinista y mantiene la cobertura.
fn generate_smart_grid_exact_from_mono(
    mono: &[u16],
    width: usize,
    height: usize,
    grid_size: usize,
    threshold: f32,
    mode: &str,
) -> Result<Vec<ApPoint>, String> {
    let pixel_count = width
        .checked_mul(height)
        .ok_or_else(|| "Overflow en la geometria de Smart AP".to_string())?;
    if width == 0 || height == 0 || mono.len() != pixel_count {
        return Err(format!(
            "Raster Smart AP invalido: {} samples para {width}x{height}",
            mono.len()
        ));
    }

    let noise_floor = smart_grid::calculate_noise_floor(mono);
    let max_possible_var = 1_000_000_000.0;
    let abs_threshold = (threshold as f64).powi(2) * max_possible_var * 0.05;
    let is_planet_mode = mode.contains("planet");
    let min_brightness = if is_planet_mode {
        noise_floor as f64 + 12.0
    } else {
        noise_floor as f64 + (grid_size as f64 * 0.1).max(50.0)
    };
    let use_noise_floor_check = noise_floor > 0;

    let is_large_object = if is_planet_mode {
        let illuminated_limit = noise_floor.saturating_add(200);
        let illuminated = mono
            .par_iter()
            .filter(|&&sample| sample > illuminated_limit)
            .count();
        (illuminated as f64 / pixel_count as f64) > 0.25
    } else {
        false
    };

    let disk_p90 = {
        let mut sample: Vec<u16> = mono.iter().step_by(31).copied().collect();
        if sample.is_empty() {
            0.0
        } else {
            let index = (sample.len() * 90 / 100).min(sample.len() - 1);
            // Mismo estadistico de orden que sort_unstable()[index], en O(n).
            let (_, value, _) = sample.select_nth_unstable(index);
            *value as f64
        }
    };
    let surface_min_mean = (noise_floor as f64 + 30.0).max(disk_p90 * 0.12);
    let ap_size = grid_size.max(1);
    let surface_like = !is_planet_mode || is_large_object;
    let step = if surface_like {
        let base = ((ap_size as f32 * 0.67).round() as usize).clamp(8, ap_size);
        let cap = ((pixel_count as f32 / 14_000.0).sqrt().ceil() as usize).min(ap_size * 3);
        base.max(cap)
    } else {
        ap_size
    };
    let half_step = ap_size / 2;
    let start_offset = half_step.max(16);
    let mut candidates = Vec::new();
    for y in (start_offset..height.saturating_sub(start_offset)).step_by(step) {
        for x in (start_offset..width.saturating_sub(start_offset)).step_by(step) {
            let start_x = x.saturating_sub(half_step);
            let start_y = y.saturating_sub(half_step);
            candidates.push(SmartApCandidate {
                x,
                y,
                start_x,
                start_y,
                width: ap_size.min(width - start_x),
                height: ap_size.min(height - start_y),
            });
        }
    }

    let accepted: Vec<bool> = candidates
        .par_iter()
        .map(|candidate| {
            let mut sum = 0u64;
            let mut square_sum = 0u64;
            for row in candidate.start_y..candidate.start_y + candidate.height {
                let row_start = row * width + candidate.start_x;
                for &sample in &mono[row_start..row_start + candidate.width] {
                    let value = sample as u64;
                    sum += value;
                    if is_planet_mode && !is_large_object {
                        square_sum += value * value;
                    }
                }
            }
            let count = (candidate.width * candidate.height) as f64;
            let pool_mean = sum as f64 / count;
            if (use_noise_floor_check && pool_mean < min_brightness)
                || (!use_noise_floor_check && pool_mean < 100.0)
            {
                return false;
            }

            if is_planet_mode && !is_large_object {
                let variance = (square_sum as f64 / count - pool_mean * pool_mean).max(0.0);
                variance > abs_threshold * 0.35
                    || (pool_mean > noise_floor as f64 + 18.0 && variance > abs_threshold * 0.08)
            } else {
                pool_mean > surface_min_mean
            }
        })
        .collect();

    let mut points: Vec<ApPoint> = candidates
        .iter()
        .zip(accepted)
        .filter_map(|(candidate, valid)| {
            valid.then_some(ApPoint {
                x: candidate.x as f32,
                y: candidate.y as f32,
                size: ap_size,
            })
        })
        .collect();

    if is_planet_mode {
        let cx = (width / 2) as f32;
        let cy = (height / 2) as f32;
        let min_dist = (grid_size as f32 * 0.75).max(12.0);
        let has_center = points.iter().any(|point| {
            let dx = point.x - cx;
            let dy = point.y - cy;
            (dx * dx + dy * dy).sqrt() < min_dist
        });
        if !has_center {
            points.push(ApPoint {
                x: cx,
                y: cy,
                size: ap_size,
            });
        }
    }
    if points.is_empty() {
        points.push(ApPoint {
            x: (width / 2) as f32,
            y: (height / 2) as f32,
            size: ap_size,
        });
    }
    Ok(points)
}

fn load_exact_smart_ap_reference(
    app: &tauri::AppHandle,
    path: &str,
    frame_idx: usize,
) -> Result<(Arc<Vec<u16>>, usize, usize, &'static str), String> {
    let source_key = smart_ap_source_key(path);
    let cache = SMART_AP_REFERENCE_CACHE.get_or_init(|| Mutex::new(None));
    if let Some(hit) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .filter(|hit| hit.key.source_key == source_key && hit.key.frame_idx == frame_idx)
        .cloned()
    {
        return Ok((
            hit.mono,
            hit.key.width,
            hit.key.height,
            "RAM u16 del analisis",
        ));
    }

    if let Some((key, mono)) = load_smart_ap_reference_disk(source_key, frame_idx) {
        let mono = Arc::new(mono);
        *cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(SmartApReferenceCache {
            key: key.clone(),
            mono: mono.clone(),
        });
        return Ok((mono, key.width, key.height, "cache NVMe Smart AP"));
    }

    let reader = VideoInput::open(path, app)?;
    let width = reader.width();
    let height = reader.height();
    let bpp = reader.bpp();
    let color_id = reader.color_id();
    let bounded_idx = frame_idx.min(reader.frame_count().saturating_sub(1));
    let exact_key = SmartApReferenceKey {
        source_key,
        frame_idx: bounded_idx,
        width,
        height,
        color_id,
    };
    if let Some(hit) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .filter(|hit| hit.key == exact_key)
        .cloned()
    {
        return Ok((hit.mono, width, height, "RAM u16 del analisis"));
    }
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| "Overflow en la geometria del frame Smart AP".to_string())?;

    let (mono, source) = match &reader {
        VideoInput::Ffmpeg(ffmpeg) => {
            if let Some(mono) = smart_ap_try_shared_decode_cache(
                ffmpeg,
                path,
                width,
                height,
                bpp,
                color_id,
                bounded_idx,
            ) {
                (mono, "cache NVMe")
            } else {
                let raw = reader.get_frame(bounded_idx, color_id);
                if raw.is_empty() {
                    return Err("No se pudo leer el frame de referencia".into());
                }
                (smart_ap_mono_from_raw(&raw, pixels, bpp)?, "decode")
            }
        }
        _ => {
            let raw = reader.get_frame(bounded_idx, color_id);
            if raw.is_empty() {
                return Err("No se pudo leer el frame de referencia".into());
            }
            (smart_ap_mono_from_raw(&raw, pixels, bpp)?, "lector nativo")
        }
    };
    let mono = Arc::new(mono);
    publish_smart_ap_reference_disk(&exact_key, &mono);
    *cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(SmartApReferenceCache {
        key: exact_key,
        mono: mono.clone(),
    });
    Ok((mono, width, height, source))
}

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
    let app_for_work = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let started = std::time::Instant::now();
        let frame_idx = ref_frame_idx.unwrap_or(0);
        let grid_mode = mode.unwrap_or_else(|| "surface".to_string());
        let (mono, width, height, source) =
            load_exact_smart_ap_reference(&app_for_work, &path, frame_idx)?;
        let points = generate_smart_grid_exact_from_mono(
            &mono, width, height, grid_size, threshold, &grid_mode,
        )?;
        log_to_front(
            &app_for_work,
            "INFO",
            &format!(
                "Smart AP: {} puntos desde {} en {:.1} ms ({}x{}).",
                points.len(),
                source,
                started.elapsed().as_secs_f64() * 1000.0,
                width,
                height
            ),
        );
        Ok(points)
    })
    .await
    .map_err(|error| format!("El worker Smart AP fallo: {error}"))?
}
// 8. SMART AP GENERATOR (Integrated)
// ==========================================

#[cfg(test)]
mod smart_ap_perf_tests {
    use super::*;

    #[test]
    fn smart_ap_reference_survives_ram_cache_reset_without_reopening_source() {
        let safe_thread_name = std::thread::current()
            .name()
            .unwrap_or("test")
            .replace(':', "_");
        let source = std::env::temp_dir().join(format!(
            "zas_smart_ap_source_{}_{}.mov",
            std::process::id(),
            safe_thread_name
        ));
        std::fs::write(&source, b"fingerprint-only fixture").unwrap();
        let path = source.to_string_lossy();
        let mono: Vec<u16> = (0..64 * 48).map(|index| (index * 17) as u16).collect();
        cache_smart_grid_frame(&path, 37, 64, 48, 0, &mono).unwrap();
        *SMART_AP_REFERENCE_CACHE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        let source_key = smart_ap_source_key(&path);
        let (key, restored) = load_smart_ap_reference_disk(source_key, 37).unwrap();
        assert_eq!((key.width, key.height, key.color_id), (64, 48, 0));
        assert_eq!(restored, mono);
        let _ = std::fs::remove_file(smart_ap_reference_disk_path(&key));
        let _ = std::fs::remove_file(source);
    }

    fn deterministic_frame(width: usize, height: usize) -> Vec<u16> {
        let mut state = 0x9E37_79B9u32;
        (0..width * height)
            .map(|index| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let x = index % width;
                let y = index / width;
                let disc = if (x as isize - width as isize / 2).pow(2)
                    + (y as isize - height as isize / 2).pow(2)
                    < (width.min(height) as isize / 3).pow(2)
                {
                    24_000u16.saturating_add(((x * 37 + y * 19) % 20_000) as u16)
                } else {
                    600
                };
                disc.saturating_add(((state >> 25) & 0x7f) as u16)
            })
            .collect()
    }

    fn assert_grid_parity(width: usize, height: usize, size: usize, threshold: f32, mode: &str) {
        let frame = deterministic_frame(width, height);
        let legacy =
            smart_grid::generate_smart_grid_internal(&frame, width, height, size, threshold, mode);
        let optimized =
            generate_smart_grid_exact_from_mono(&frame, width, height, size, threshold, mode)
                .unwrap();
        assert_eq!(optimized, legacy);
    }

    #[test]
    fn exact_patch_grid_matches_integral_grid_for_surface_and_planet() {
        assert_grid_parity(319, 241, 32, 0.18, "surface");
        assert_grid_parity(319, 241, 48, 0.32, "surface");
        assert_grid_parity(257, 193, 32, 0.18, "planet");
        assert_grid_parity(257, 193, 48, 0.32, "planet");
    }

    #[test]
    fn raw_rgb_green_extraction_matches_legacy_conversion() {
        let width = 47;
        let height = 31;
        let pixels = width * height;
        let mut raw = Vec::with_capacity(pixels * 6);
        for index in 0..pixels {
            for value in [index as u16, (index * 13) as u16, (index * 29) as u16] {
                raw.extend_from_slice(&value.to_le_bytes());
            }
        }
        let legacy_rgb = raw_to_u16_buffer(&raw, width, height, 6);
        let legacy_green: Vec<u16> = legacy_rgb.chunks_exact(3).map(|pixel| pixel[1]).collect();
        assert_eq!(
            smart_ap_mono_from_raw(&raw, pixels, 6).unwrap(),
            legacy_green
        );
    }

    #[test]
    fn reference_cache_key_changes_when_source_changes() {
        let path = std::env::temp_dir().join(format!(
            "smart-ap-key-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, [1u8, 2, 3]).unwrap();
        let first = smart_ap_source_key(path.to_str().unwrap());
        std::fs::write(&path, [1u8, 2, 3, 4]).unwrap();
        let second = smart_ap_source_key(path.to_str().unwrap());
        let _ = std::fs::remove_file(path);
        assert_ne!(first, second);
    }

    #[test]
    #[ignore = "microbenchmark fisico; ejecutar con --release --ignored"]
    fn benchmark_exact_patch_grid_4k_against_integral_grid() {
        let width = 3840;
        let height = 2160;
        let frame = deterministic_frame(width, height);
        let legacy_started = std::time::Instant::now();
        let legacy =
            smart_grid::generate_smart_grid_internal(&frame, width, height, 48, 0.2, "surface");
        let legacy_elapsed = legacy_started.elapsed();
        let optimized_started = std::time::Instant::now();
        let optimized =
            generate_smart_grid_exact_from_mono(&frame, width, height, 48, 0.2, "surface").unwrap();
        let optimized_elapsed = optimized_started.elapsed();
        assert_eq!(optimized, legacy);
        eprintln!(
            "SMART_AP_4K legacy_ms={:.3} optimized_ms={:.3} speedup={:.2}x legacy_sat_mib={:.1}",
            legacy_elapsed.as_secs_f64() * 1000.0,
            optimized_elapsed.as_secs_f64() * 1000.0,
            legacy_elapsed.as_secs_f64() / optimized_elapsed.as_secs_f64().max(1e-9),
            (width * height * 16) as f64 / (1024.0 * 1024.0),
        );
    }
}
