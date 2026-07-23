#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PostprocessHistogram {
    bins: usize,
    red: Vec<u64>,
    green: Vec<u64>,
    blue: Vec<u64>,
    luminance: Vec<u64>,
    minimum: u16,
    maximum: u16,
    median: u16,
    percentile_low: u16,
    percentile_high: u16,
    shadow_clip: f32,
    highlight_clip: f32,
    is_mono: bool,
    source: &'static str,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PostprocessPixelSample {
    x: usize,
    y: usize,
    red: u16,
    green: u16,
    blue: u16,
    luminance: u16,
    red_normalized: f32,
    green_normalized: f32,
    blue_normalized: f32,
    is_mono: bool,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RgbAlignmentEstimate {
    red_x: f32,
    red_y: f32,
    blue_x: f32,
    blue_y: f32,
    confidence: f32,
    applicable: bool,
    reason: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactAnalysis {
    sampled_pixels: usize,
    hot_pixels: usize,
    dead_pixels: usize,
    clipped_shadows: usize,
    clipped_highlights: usize,
    color_fringe_score: f32,
    ringing_score: f32,
    suggested_deringing_mode: i32,
    suggested_deringing_radius: f32,
    suggested_deringing_dark: f32,
    suggested_deringing_light: f32,
    suggested_denoise: f32,
    summary: String,
}

fn postprocess_image_snapshot(
    state: &AppState,
    prefer_processed: bool,
) -> Result<(StackResult, &'static str), String> {
    if prefer_processed {
        if let Some(image) = state.processed_image.lock().unwrap().clone() {
            return Ok((image, "processed"));
        }
    }
    state
        .stacked_image
        .lock()
        .unwrap()
        .clone()
        .map(|image| (image, "source"))
        .ok_or_else(|| "No hay un resultado activo para postprocesar.".to_string())
}

fn validate_rgb_stack(image: &StackResult) -> Result<(), String> {
    let pixels = image
        .width
        .checked_mul(image.height)
        .ok_or_else(|| "Dimensiones de imagen inválidas.".to_string())?;
    if image.data.len() != pixels * 3 {
        return Err(format!(
            "Buffer 16-bit inválido: se esperaban {} muestras RGB y llegaron {}.",
            pixels * 3,
            image.data.len()
        ));
    }
    Ok(())
}

/// Classifies an RGB16 buffer by sampling the whole image instead of trusting a
/// single centre pixel. Mosaic canvases commonly have black or neutral padding
/// at the centre, which previously caused colour results to be marked as mono.
fn rgb16_buffer_is_monochrome(data: &[u16]) -> bool {
    let pixel_count = data.len() / 3;
    if pixel_count == 0 {
        return true;
    }

    const MAX_SAMPLES: usize = 262_144;
    let stride = (pixel_count / MAX_SAMPLES).max(1);
    let mut signal_samples = 0usize;
    let mut chromatic_samples = 0usize;

    for pixel in data.chunks_exact(3).step_by(stride) {
        let minimum = pixel[0].min(pixel[1]).min(pixel[2]);
        let maximum = pixel[0].max(pixel[1]).max(pixel[2]);
        if maximum <= 64 {
            continue;
        }
        signal_samples += 1;
        // Ignore sub-LSB conversion noise, while retaining real low-saturation
        // colour in linear astronomical data.
        let tolerance = ((maximum as u32 / 2048).max(8)) as u16;
        if maximum - minimum > tolerance {
            chromatic_samples += 1;
            if chromatic_samples >= 3 {
                return false;
            }
        }
    }

    !(signal_samples < 128 && chromatic_samples > 0)
}

#[tauri::command]
fn reset_postprocess_state(state: State<'_, AppState>) -> Result<usize, String> {
    state.active_req_id.fetch_add(1, Ordering::SeqCst);
    *state.processed_image.lock().unwrap() = None;
    state.deconv_cache.lock().unwrap().clear();
    state.wavelet_cache.lock().unwrap().clear();
    state.filter_cache.lock().unwrap().clear();
    clear_editor_previews();
    Ok(state.result_generation.fetch_add(1, Ordering::SeqCst) + 1)
}

#[tauri::command]
fn postprocess_histogram(
    state: State<'_, AppState>,
    prefer_processed: Option<bool>,
) -> Result<PostprocessHistogram, String> {
    state.license_manager.check_access()?;
    let (image, source) = postprocess_image_snapshot(&state, prefer_processed.unwrap_or(true))?;
    validate_rgb_stack(&image)?;
    Ok(compute_postprocess_histogram(&image, source))
}

fn compute_postprocess_histogram(
    image: &StackResult,
    source: &'static str,
) -> PostprocessHistogram {
    const BINS: usize = 1024;
    let mut red = vec![0u64; BINS];
    let mut green = vec![0u64; BINS];
    let mut blue = vec![0u64; BINS];
    let mut luminance = vec![0u64; BINS];
    let mut minimum = u16::MAX;
    let mut maximum = u16::MIN;
    let mut clipped_shadows = 0usize;
    let mut clipped_highlights = 0usize;

    for pixel in image.data.chunks_exact(3) {
        let r = pixel[0];
        let g = pixel[1];
        let b = pixel[2];
        let luma =
            ((r as u32 * 2126 + g as u32 * 7152 + b as u32 * 722 + 5000) / 10000).min(65535) as u16;
        red[r as usize * (BINS - 1) / 65535] += 1;
        green[g as usize * (BINS - 1) / 65535] += 1;
        blue[b as usize * (BINS - 1) / 65535] += 1;
        luminance[luma as usize * (BINS - 1) / 65535] += 1;
        minimum = minimum.min(luma);
        maximum = maximum.max(luma);
        if luma <= 16 {
            clipped_shadows += 1;
        }
        if luma >= 65519 {
            clipped_highlights += 1;
        }
    }

    let pixel_count = image.width.saturating_mul(image.height).max(1);
    let percentile_bin = |quantile: f64| -> usize {
        let target = ((pixel_count as f64 * quantile).ceil() as u64).max(1);
        let mut cumulative = 0u64;
        for (index, count) in luminance.iter().enumerate() {
            cumulative += count;
            if cumulative >= target {
                return index;
            }
        }
        BINS - 1
    };
    let bin_to_u16 = |bin: usize| -> u16 {
        ((bin as u32 * 65535 + (BINS as u32 - 1) / 2) / (BINS as u32 - 1)) as u16
    };
    let median_bin = percentile_bin(0.5);
    let percentile_low_bin = percentile_bin(0.001);
    let percentile_high_bin = percentile_bin(0.999);

    PostprocessHistogram {
        bins: BINS,
        red,
        green,
        blue,
        luminance,
        minimum,
        maximum,
        median: bin_to_u16(median_bin).clamp(minimum, maximum),
        percentile_low: bin_to_u16(percentile_low_bin).clamp(minimum, maximum),
        percentile_high: bin_to_u16(percentile_high_bin).clamp(minimum, maximum),
        shadow_clip: clipped_shadows as f32 / pixel_count as f32,
        highlight_clip: clipped_highlights as f32 / pixel_count as f32,
        is_mono: image.is_mono,
        source,
    }
}

#[tauri::command]
fn sample_postprocess_pixel(
    state: State<'_, AppState>,
    x: usize,
    y: usize,
    prefer_processed: Option<bool>,
) -> Result<PostprocessPixelSample, String> {
    state.license_manager.check_access()?;
    let (image, _) = postprocess_image_snapshot(&state, prefer_processed.unwrap_or(true))?;
    validate_rgb_stack(&image)?;
    if x >= image.width || y >= image.height {
        return Err("El cuentagotas quedó fuera de la imagen.".into());
    }
    let index = (y * image.width + x) * 3;
    let red = image.data[index];
    let green = image.data[index + 1];
    let blue = image.data[index + 2];
    let luminance =
        ((red as u32 * 2126 + green as u32 * 7152 + blue as u32 * 722 + 5000) / 10000) as u16;
    Ok(PostprocessPixelSample {
        x,
        y,
        red,
        green,
        blue,
        luminance,
        red_normalized: red as f32 / 65535.0,
        green_normalized: green as f32 / 65535.0,
        blue_normalized: blue as f32 / 65535.0,
        is_mono: image.is_mono,
    })
}

#[tauri::command]
fn estimate_postprocess_rgb_alignment(
    state: State<'_, AppState>,
) -> Result<RgbAlignmentEstimate, String> {
    state.license_manager.check_access()?;
    let (image, _) = postprocess_image_snapshot(&state, false)?;
    validate_rgb_stack(&image)?;
    if image.is_mono {
        return Ok(RgbAlignmentEstimate {
            red_x: 0.0,
            red_y: 0.0,
            blue_x: 0.0,
            blue_y: 0.0,
            confidence: 1.0,
            applicable: false,
            reason: "La alineación RGB no aplica a una fuente monocroma.".into(),
        });
    }
    if image.width < 64 || image.height < 64 {
        return Ok(RgbAlignmentEstimate {
            red_x: 0.0,
            red_y: 0.0,
            blue_x: 0.0,
            blue_y: 0.0,
            confidence: 0.0,
            applicable: false,
            reason: "La imagen es demasiado pequeña para una medición RGB fiable.".into(),
        });
    }
    let mut working: Vec<f32> = image.data.iter().map(|value| *value as f32).collect();
    let (red_x, red_y, blue_x, blue_y) =
        align_stack_rgb_channels(&mut working, image.width, image.height);
    let maximum = red_x
        .abs()
        .max(red_y.abs())
        .max(blue_x.abs())
        .max(blue_y.abs());
    let applicable = maximum >= 0.05;
    let confidence = if applicable {
        (1.0 - (maximum / 12.0)).clamp(0.35, 0.98)
    } else {
        0.75
    };
    // `align_stack_rgb_channels` samples ch(p + measured_shift), while the
    // interactive pipeline's `shift_channel` samples ch(p - slider_shift).
    // Expose the correction in slider coordinates so clicking Auto produces
    // exactly the validated backend alignment.
    Ok(RgbAlignmentEstimate {
        red_x: -red_x,
        red_y: -red_y,
        blue_x: -blue_x,
        blue_y: -blue_y,
        confidence,
        applicable,
        reason: if applicable {
            "Desplazamiento subpíxel medido contra el canal verde.".into()
        } else {
            "Los canales ya están alineados dentro de la tolerancia de 0.05 px.".into()
        },
    })
}

#[tauri::command]
fn analyze_postprocess_artifacts(
    state: State<'_, AppState>,
    prefer_processed: Option<bool>,
) -> Result<ArtifactAnalysis, String> {
    state.license_manager.check_access()?;
    let (image, _) = postprocess_image_snapshot(&state, prefer_processed.unwrap_or(true))?;
    validate_rgb_stack(&image)?;
    Ok(compute_artifact_analysis(&image))
}

fn compute_artifact_analysis(image: &StackResult) -> ArtifactAnalysis {
    let w = image.width;
    let h = image.height;
    let total = w.saturating_mul(h).max(1);
    let stride = (total / 250_000).max(1);
    let mut hot_pixels = 0usize;
    let mut dead_pixels = 0usize;
    let mut clipped_shadows = 0usize;
    let mut clipped_highlights = 0usize;
    let mut fringe_sum = 0.0f64;
    let mut ringing_sum = 0.0f64;
    let mut sampled = 0usize;

    if w > 2 && h > 2 {
        for linear in (w + 1..total.saturating_sub(w + 1)).step_by(stride) {
            let x = linear % w;
            if x == 0 || x + 1 >= w {
                continue;
            }
            let index = linear * 3;
            let r = image.data[index] as f32;
            let g = image.data[index + 1] as f32;
            let b = image.data[index + 2] as f32;
            let center = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let mut neighbours = [0.0f32; 8];
            let mut n = 0usize;
            for oy in -1isize..=1 {
                for ox in -1isize..=1 {
                    if ox == 0 && oy == 0 {
                        continue;
                    }
                    let pos = ((linear as isize + oy * w as isize + ox) as usize) * 3;
                    neighbours[n] = 0.2126 * image.data[pos] as f32
                        + 0.7152 * image.data[pos + 1] as f32
                        + 0.0722 * image.data[pos + 2] as f32;
                    n += 1;
                }
            }
            neighbours.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let local_median = (neighbours[3] + neighbours[4]) * 0.5;
            let local_range = (neighbours[7] - neighbours[0]).max(256.0);
            if center - local_median > 4096.0 && center > local_median * 1.6 {
                hot_pixels += 1;
            }
            if local_median - center > 4096.0 && local_median > center * 1.6 {
                dead_pixels += 1;
            }
            if center <= 16.0 {
                clipped_shadows += 1;
            }
            if center >= 65519.0 {
                clipped_highlights += 1;
            }
            if !image.is_mono {
                fringe_sum += ((r - g).abs() + (b - g).abs()) as f64 / 131070.0;
            }
            let overshoot = (center - local_median).abs() / local_range;
            if overshoot > 1.0 {
                ringing_sum += (overshoot - 1.0).min(4.0) as f64;
            }
            sampled += 1;
        }
    }

    let divisor = sampled.max(1) as f64;
    let ringing_score = (ringing_sum / divisor * 100.0).min(100.0) as f32;
    let color_fringe_score = (fringe_sum / divisor * 100.0).min(100.0) as f32;
    let defect_rate = (hot_pixels + dead_pixels) as f32 / sampled.max(1) as f32;
    let suggested_denoise = (defect_rate * 5000.0).clamp(0.0, 35.0);
    let severity = (ringing_score / 100.0).clamp(0.0, 1.0);
    let suggested_mode = if severity > 0.02 { 2 } else { 1 };
    let summary = if ringing_score > 8.0 {
        "Se detectaron halos u overshoot relevantes; conviene una corrección moderada y revisar en A/B."
    } else if hot_pixels + dead_pixels > sampled / 500 {
        "Predominan defectos puntuales; usa reducción de ruido conservadora antes de aumentar nitidez."
    } else {
        "No se detectaron artefactos severos en la muestra; conserva ajustes suaves."
    };

    ArtifactAnalysis {
        sampled_pixels: sampled,
        hot_pixels,
        dead_pixels,
        clipped_shadows,
        clipped_highlights,
        color_fringe_score,
        ringing_score,
        suggested_deringing_mode: suggested_mode,
        suggested_deringing_radius: (1.0 + severity * 2.0).clamp(1.0, 3.0),
        suggested_deringing_dark: (severity * 0.55).clamp(0.05, 0.55),
        suggested_deringing_light: (severity * 0.4).clamp(0.04, 0.4),
        suggested_denoise,
        summary: summary.into(),
    }
}

#[inline]
fn post_smoothstep(edge0: f32, edge1: f32, value: f32) -> f32 {
    let t = ((value - edge0) / (edge1 - edge0).max(1e-6)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[inline]
fn rgb_hue(r: f32, g: f32, b: f32) -> f32 {
    let maximum = r.max(g).max(b);
    let minimum = r.min(g).min(b);
    let delta = maximum - minimum;
    if delta <= 1e-6 {
        return 0.0;
    }
    let hue = if maximum == r {
        ((g - b) / delta).rem_euclid(6.0)
    } else if maximum == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    hue / 6.0
}

#[inline]
fn interpolate_hue_control(values: &[f32; 8], hue: f32) -> f32 {
    const CENTRES: [f32; 8] = [0.0, 1.0 / 12.0, 1.0 / 6.0, 1.0 / 3.0, 0.5, 2.0 / 3.0, 0.75, 5.0 / 6.0];
    let mut weighted = 0.0f32;
    let mut total = 0.0f32;
    for (index, centre) in CENTRES.iter().enumerate() {
        let distance = (hue - centre).abs().min(1.0 - (hue - centre).abs());
        let weight = (1.0 - distance / (1.0 / 6.0)).max(0.0).powi(2);
        weighted += values[index].clamp(-1.0, 1.0) * weight;
        total += weight;
    }
    if total > 1e-6 { weighted / total } else { 0.0 }
}

#[inline]
fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let maximum = r.max(g).max(b);
    let minimum = r.min(g).min(b);
    let delta = maximum - minimum;
    let lightness = (maximum + minimum) * 0.5;
    let saturation = if delta <= 1e-6 {
        0.0
    } else {
        delta / (1.0 - (2.0 * lightness - 1.0).abs()).max(1e-6)
    };
    (rgb_hue(r, g, b), saturation.clamp(0.0, 1.0), lightness.clamp(0.0, 1.0))
}

#[inline]
fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> (f32, f32, f32) {
    let h = hue.rem_euclid(1.0);
    let s = saturation.clamp(0.0, 1.0);
    let l = lightness.clamp(0.0, 1.0);
    let chroma = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let sector = h * 6.0;
    let x = chroma * (1.0 - (sector.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match sector.floor() as i32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let match_value = l - chroma * 0.5;
    (r1 + match_value, g1 + match_value, b1 + match_value)
}

fn build_local_contrast_delta(
    data: &[u16],
    width: usize,
    height: usize,
    texture: f32,
    clarity: f32,
) -> Option<Vec<f32>> {
    if width == 0 || height == 0 || data.len() != width * height * 3 {
        return None;
    }
    let texture = texture.clamp(-1.0, 1.0);
    let clarity = clarity.clamp(-1.0, 1.0);
    if texture.abs() <= 1e-6 && clarity.abs() <= 1e-6 {
        return None;
    }
    let luma: Vec<f32> = data
        .par_chunks_exact(3)
        .map(|pixel| {
            (0.2126 * pixel[0] as f32 + 0.7152 * pixel[1] as f32 + 0.0722 * pixel[2] as f32)
                / 65535.0
        })
        .collect();
    let fine = if texture.abs() > 1e-6 {
        Some(apply_gaussian_blur(&luma, width, height, 1.15))
    } else {
        None
    };
    let broad = if clarity.abs() > 1e-6 {
        Some(apply_gaussian_blur(&luma, width, height, 4.5))
    } else {
        None
    };
    Some(
        luma.par_iter()
            .enumerate()
            .map(|(index, &value)| {
                let fine_detail = fine.as_ref().map(|blur| value - blur[index]).unwrap_or(0.0);
                let broad_detail = broad.as_ref().map(|blur| value - blur[index]).unwrap_or(0.0);
                let structure = fine_detail.abs() + broad_detail.abs() * 0.55;
                let confidence = post_smoothstep(0.0012, 0.012, structure);
                let signal_gate = post_smoothstep(0.006, 0.065, value)
                    * (1.0 - post_smoothstep(0.88, 0.995, value));
                let positive_gate = if texture > 0.0 || clarity > 0.0 {
                    0.12 + confidence * 0.88
                } else {
                    1.0
                };
                (fine_detail * texture * 1.15 + broad_detail * clarity * 0.82)
                    * signal_gate
                    * positive_gate
            })
            .collect(),
    )
}

fn normalized_solar_curve(points: &[[f32; 2]]) -> Vec<[f32; 2]> {
    let mut normalized: Vec<[f32; 2]> = points
        .iter()
        .filter(|point| point[0].is_finite() && point[1].is_finite())
        .map(|point| [point[0].clamp(0.0, 1.0), point[1].clamp(0.0, 1.0)])
        .collect();
    normalized.sort_by(|left, right| left[0].total_cmp(&right[0]));
    normalized.dedup_by(|left, right| (left[0] - right[0]).abs() < 0.000_1);
    if normalized.first().map(|point| point[0]).unwrap_or(1.0) > 0.000_1 {
        normalized.insert(0, [0.0, 0.0]);
    }
    if normalized.last().map(|point| point[0]).unwrap_or(0.0) < 0.999_9 {
        normalized.push([1.0, 1.0]);
    }
    if normalized.len() < 2 {
        return vec![[0.0, 0.0], [1.0, 1.0]];
    }
    normalized
}

fn solar_curve_tangents(points: &[[f32; 2]]) -> Vec<f32> {
    if points.len() < 2 {
        return vec![1.0; points.len()];
    }
    let intervals: Vec<f32> = points
        .windows(2)
        .map(|pair| (pair[1][0] - pair[0][0]).max(1e-6))
        .collect();
    let secants: Vec<f32> = points
        .windows(2)
        .zip(intervals.iter())
        .map(|(pair, &interval)| (pair[1][1] - pair[0][1]) / interval)
        .collect();
    let mut tangents = vec![0.0; points.len()];
    tangents[0] = secants[0];
    tangents[points.len() - 1] = secants[secants.len() - 1];
    for index in 1..points.len() - 1 {
        let previous = secants[index - 1];
        let next = secants[index];
        tangents[index] = if previous * next <= 0.0 {
            0.0
        } else {
            // Weighted harmonic mean (PCHIP). It preserves a genuinely
            // linear curve, keeps monotone segments monotone and still lets
            // the user create a controlled local maximum or minimum.
            let previous_interval = intervals[index - 1];
            let next_interval = intervals[index];
            let weight_previous = 2.0 * next_interval + previous_interval;
            let weight_next = next_interval + 2.0 * previous_interval;
            (weight_previous + weight_next)
                / (weight_previous / previous + weight_next / next)
        };
    }
    tangents
}

#[inline]
fn evaluate_solar_curve(points: &[[f32; 2]], tangents: &[f32], value: f32) -> f32 {
    let x = value.clamp(0.0, 1.0);
    for (index, pair) in points.windows(2).enumerate() {
        let left = pair[0];
        let right = pair[1];
        if x <= right[0] {
            let span = (right[0] - left[0]).max(1e-6);
            let t = ((x - left[0]) / span).clamp(0.0, 1.0);
            let t2 = t * t;
            let t3 = t2 * t;
            let output = (2.0 * t3 - 3.0 * t2 + 1.0) * left[1]
                + (t3 - 2.0 * t2 + t) * span * tangents[index]
                + (-2.0 * t3 + 3.0 * t2) * right[1]
                + (t3 - t2) * span * tangents[index + 1];
            return output
                .clamp(left[1].min(right[1]), left[1].max(right[1]))
                .clamp(0.0, 1.0);
        }
    }
    points.last().map(|point| point[1]).unwrap_or(x)
}

fn build_solar_filament_delta(
    data: &[u16],
    width: usize,
    height: usize,
    params: &SolarMonoParams,
) -> Option<Vec<f32>> {
    if !params.enabled
        || params.filament_amount <= 1e-6
        || width == 0
        || height == 0
        || data.len() != width * height * 3
    {
        return None;
    }
    let luma: Vec<f32> = data
        .par_chunks_exact(3)
        .map(|pixel| {
            (0.2126 * pixel[0] as f32
                + 0.7152 * pixel[1] as f32
                + 0.0722 * pixel[2] as f32)
                / 65535.0
        })
        .collect();
    let radius = params.filament_radius.clamp(0.55, 4.0);
    let blurred = apply_gaussian_blur(&luma, width, height, radius);
    let details: Vec<f32> = luma
        .par_iter()
        .zip(blurred.par_iter())
        .map(|(&source, &low_pass)| source - low_pass)
        .collect();
    if details
        .par_iter()
        .map(|detail| detail.abs())
        .reduce(|| 0.0, f32::max)
        <= 1e-7
    {
        return None;
    }

    // Robust global noise floor from a deterministic sample of the high-pass
    // residual. It does not invent texture in a flat field and makes the
    // "Protección de ruido" control meaningful for high-resolution stacks.
    let stride = (details.len() / 32_768).max(1);
    let mut absolute_sample: Vec<f32> = details
        .iter()
        .step_by(stride)
        .map(|value| value.abs())
        .collect();
    absolute_sample.sort_by(|left, right| left.total_cmp(right));
    let mad = absolute_sample
        .get(absolute_sample.len().saturating_sub(1) / 2)
        .copied()
        .unwrap_or(0.0);
    let noise_sigma = (mad * 1.4826).max(0.000_35);
    let guard = params.noise_guard.clamp(0.0, 1.0);
    let threshold = noise_sigma * (0.8 + guard * 3.8);
    let upper = threshold * (2.4 + guard * 2.8);
    let amount = params.filament_amount.clamp(0.0, 1.5);

    Some(
        luma.par_iter()
            .zip(details.par_iter())
            .map(|(&value, &detail)| {
                let confidence = post_smoothstep(threshold, upper, detail.abs());
                let signal_gate = post_smoothstep(0.004, 0.055, value)
                    * (1.0 - post_smoothstep(0.9, 0.998, value));
                // Dark H-alpha fibrils benefit from a slightly stronger
                // response, while the confidence gate prevents noise worms.
                let polarity = if detail < 0.0 { 1.16 } else { 1.0 };
                detail * amount * 2.15 * confidence * signal_gate * polarity
            })
            .collect(),
    )
}

#[inline]
fn interpolate_solar_color(
    value: f32,
    shadow: [f32; 3],
    midtone: [f32; 3],
    highlight: [f32; 3],
) -> [f32; 3] {
    let value = value.clamp(0.0, 1.0);
    let (left, right, linear_t) = if value <= 0.5 {
        (shadow, midtone, value * 2.0)
    } else {
        (midtone, highlight, (value - 0.5) * 2.0)
    };
    let t = linear_t * linear_t * (3.0 - 2.0 * linear_t);
    let chroma = [
        left[0] + (right[0] - left[0]) * t,
        left[1] + (right[1] - left[1]) * t,
        left[2] + (right[2] - left[2]) * t,
    ];
    let chroma_luma = (0.2126 * chroma[0] + 0.7152 * chroma[1] + 0.0722 * chroma[2])
        .max(0.015);
    let scale = value / chroma_luma;
    [
        (chroma[0] * scale).clamp(0.0, 1.0),
        (chroma[1] * scale).clamp(0.0, 1.0),
        (chroma[2] * scale).clamp(0.0, 1.0),
    ]
}

fn apply_advanced_postprocess(
    data: &mut [u16],
    width: usize,
    height: usize,
    is_mono: bool,
    params: &AdvancedColorParams,
) {
    let black = params.levels_black.clamp(0.0, 0.98);
    let white = params.levels_white.clamp(black + 0.005, 1.0);
    let mid = params.levels_mid.clamp(0.1, 4.0);
    let exposure = params.exposure.clamp(-4.0, 4.0).exp2();
    let shadows = params.shadows.clamp(-1.0, 1.0);
    let highlights = params.highlights.clamp(-1.0, 1.0);
    let whites = params.whites.clamp(-1.0, 1.0);
    let blacks = params.blacks.clamp(-1.0, 1.0);
    let vibrance = params.vibrance.clamp(-1.0, 1.0);
    let temperature = params.temperature.clamp(-1.0, 1.0);
    let tint = params.tint.clamp(-1.0, 1.0);
    let scnr_green = params.scnr_green.clamp(0.0, 1.0);
    let solar_enabled = is_mono && params.solar.enabled;
    let solar_curve = normalized_solar_curve(&params.solar.curve_points);
    let solar_curve_tangents = solar_curve_tangents(&solar_curve);
    let solar_filament_delta =
        build_solar_filament_delta(data, width, height, &params.solar);
    let hsl_active = params.hsl_hue.iter().any(|value| value.abs() > 1e-6)
        || params.hsl_saturation.iter().any(|value| value.abs() > 1e-6)
        || params.hsl_luminance.iter().any(|value| value.abs() > 1e-6);
    let local_delta = build_local_contrast_delta(data, width, height, params.texture, params.clarity);

    data.par_chunks_exact_mut(3).enumerate().for_each(|(pixel_index, pixel)| {
        let mut r = (pixel[0] as f32 / 65535.0 - black) / (white - black);
        let mut g = (pixel[1] as f32 / 65535.0 - black) / (white - black);
        let mut b = (pixel[2] as f32 / 65535.0 - black) / (white - black);
        r = r.clamp(0.0, 1.0).powf(1.0 / mid) * exposure;
        g = g.clamp(0.0, 1.0).powf(1.0 / mid) * exposure;
        b = b.clamp(0.0, 1.0).powf(1.0 / mid) * exposure;

        let luma = (0.2126 * r + 0.7152 * g + 0.0722 * b).max(0.0);
        let shadow_weight = (1.0 - post_smoothstep(0.05, 0.62, luma)).powi(2);
        let highlight_weight = post_smoothstep(0.38, 0.95, luma).powi(2);
        let black_weight = 1.0 - post_smoothstep(0.0, 0.28, luma);
        let white_weight = post_smoothstep(0.72, 1.0, luma);
        let tone_delta = shadows * shadow_weight * 0.22
            + highlights * highlight_weight * 0.22
            + blacks * black_weight * 0.12
            + whites * white_weight * 0.12;
        let new_luma = (luma
            + tone_delta
            + local_delta
                .as_ref()
                .map(|delta| delta[pixel_index])
                .unwrap_or(0.0)
            + solar_filament_delta
                .as_ref()
                .map(|delta| delta[pixel_index])
                .unwrap_or(0.0))
        .max(0.0);
        if luma > 1e-6 {
            let scale = new_luma / luma;
            r *= scale;
            g *= scale;
            b *= scale;
        } else {
            r = new_luma;
            g = new_luma;
            b = new_luma;
        }

        if !is_mono {
            let warm = temperature * 0.12;
            let magenta = tint * 0.08;
            r *= 1.0 + warm + magenta * 0.5;
            g *= 1.0 - magenta;
            b *= 1.0 - warm + magenta * 0.5;

            let luma_after_wb = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let maximum = r.max(g).max(b);
            let minimum = r.min(g).min(b);
            let chroma = (maximum - minimum).max(0.0);
            let current_saturation = if maximum > 1e-6 {
                chroma / maximum
            } else {
                0.0
            };
            let vibrance_factor = if vibrance >= 0.0 {
                1.0 + vibrance * (1.0 - current_saturation) * 1.25
            } else {
                1.0 + vibrance
            };
            r = luma_after_wb + (r - luma_after_wb) * vibrance_factor;
            g = luma_after_wb + (g - luma_after_wb) * vibrance_factor;
            b = luma_after_wb + (b - luma_after_wb) * vibrance_factor;

            if hsl_active && chroma > 1e-6 {
                let (hue, saturation, lightness) = rgb_to_hsl(r, g, b);
                let hue_adjustment = interpolate_hue_control(&params.hsl_hue, hue) / 12.0;
                let saturation_adjustment = interpolate_hue_control(&params.hsl_saturation, hue);
                let luminance_adjustment = interpolate_hue_control(&params.hsl_luminance, hue);
                let adjusted_saturation = if saturation_adjustment >= 0.0 {
                    saturation + saturation_adjustment * (1.0 - saturation)
                } else {
                    saturation * (1.0 + saturation_adjustment)
                };
                let adjusted_lightness = if luminance_adjustment >= 0.0 {
                    lightness + luminance_adjustment * (1.0 - lightness) * 0.42
                } else {
                    lightness * (1.0 + luminance_adjustment * 0.42)
                };
                (r, g, b) = hsl_to_rgb(
                    hue + hue_adjustment,
                    adjusted_saturation,
                    adjusted_lightness,
                );
            }

            let grade_luma = (0.2126 * r + 0.7152 * g + 0.0722 * b).clamp(0.0, 1.0);
            let grade_weights = [
                (1.0 - post_smoothstep(0.0, 0.55, grade_luma)).powi(2),
                (1.0 - ((grade_luma - 0.5).abs() * 2.0)).clamp(0.0, 1.0),
                post_smoothstep(0.45, 1.0, grade_luma).powi(2),
            ];
            let grade_colors = [
                params.grading_shadows,
                params.grading_midtones,
                params.grading_highlights,
            ];
            for grade_index in 0..3 {
                let amount = params.grading_amounts[grade_index].clamp(0.0, 1.0)
                    * grade_weights[grade_index]
                    * 0.35;
                if amount <= 1e-6 {
                    continue;
                }
                let color = grade_colors[grade_index];
                let color_luma = 0.2126 * color[0] + 0.7152 * color[1] + 0.0722 * color[2];
                r += (color[0] - color_luma) * amount;
                g += (color[1] - color_luma) * amount;
                b += (color[2] - color_luma) * amount;
            }

            if scnr_green > 1e-6 {
                let neutral_green = (r + b) * 0.5;
                let green_excess = (g - neutral_green).max(0.0);
                g -= green_excess * scnr_green;
            }
        } else {
            let mono = (0.2126 * r + 0.7152 * g + 0.0722 * b).clamp(0.0, 1.0);
            if solar_enabled {
                let mut solar_luma =
                    evaluate_solar_curve(&solar_curve, &solar_curve_tangents, mono);
                if params.solar.invert {
                    solar_luma = 1.0 - solar_luma;
                }
                if params.solar.colorize {
                    let mapped = interpolate_solar_color(
                        solar_luma,
                        params.solar.shadow_color,
                        params.solar.midtone_color,
                        params.solar.highlight_color,
                    );
                    let strength = params.solar.color_strength.clamp(0.0, 1.0);
                    let highlight_weight = post_smoothstep(0.72, 0.995, solar_luma)
                        * params.solar.highlight_protect.clamp(0.0, 1.0);
                    let effective_strength = strength * (1.0 - highlight_weight * 0.82);
                    r = solar_luma + (mapped[0] - solar_luma) * effective_strength;
                    g = solar_luma + (mapped[1] - solar_luma) * effective_strength;
                    b = solar_luma + (mapped[2] - solar_luma) * effective_strength;
                } else {
                    r = solar_luma;
                    g = solar_luma;
                    b = solar_luma;
                }
            } else {
                r = mono;
                g = mono;
                b = mono;
            }
        }

        pixel[0] = (r.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16;
        pixel[1] = (g.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16;
        pixel[2] = (b.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16;
    });
}

fn fits_card(keyword: &str, value: &str, comment: &str) -> [u8; 80] {
    let text = if comment.is_empty() {
        format!("{:<8}= {:>20}", keyword, value)
    } else {
        format!("{:<8}= {:>20} / {}", keyword, value, comment)
    };
    let mut card = [b' '; 80];
    let bytes = text.as_bytes();
    let length = bytes.len().min(80);
    card[..length].copy_from_slice(&bytes[..length]);
    card
}

fn fits_history_card(message: &str) -> [u8; 80] {
    let text = format!("HISTORY {}", message);
    let mut card = [b' '; 80];
    let bytes = text.as_bytes();
    let length = bytes.len().min(80);
    card[..length].copy_from_slice(&bytes[..length]);
    card
}

fn build_fits_bytes(image: &StackResult) -> Result<Vec<u8>, String> {
    validate_rgb_stack(image)?;
    let axes = if image.is_mono { 2 } else { 3 };
    let mut header = Vec::<u8>::new();
    for card in [
        fits_card("SIMPLE", "T", "conforms to FITS standard"),
        fits_card("BITPIX", "16", "signed 16-bit storage"),
        fits_card("NAXIS", &axes.to_string(), "number of data axes"),
        fits_card("NAXIS1", &image.width.to_string(), "image width"),
        fits_card("NAXIS2", &image.height.to_string(), "image height"),
    ] {
        header.extend_from_slice(&card);
    }
    if !image.is_mono {
        header.extend_from_slice(&fits_card("NAXIS3", "3", "RGB channel planes"));
    }
    header.extend_from_slice(&fits_card("BSCALE", "1", "physical scaling"));
    header.extend_from_slice(&fits_card("BZERO", "32768", "unsigned 16-bit offset"));
    header.extend_from_slice(&fits_card("EXTEND", "T", "extensions may be present"));
    header.extend_from_slice(&fits_history_card(
        "Zenith Astro Stacker processed derivative; RGB data is debayered.",
    ));
    let mut end = [b' '; 80];
    end[..3].copy_from_slice(b"END");
    header.extend_from_slice(&end);
    let header_padding = (2880 - header.len() % 2880) % 2880;
    header.resize(header.len() + header_padding, b' ');

    let pixels = image.width * image.height;
    let samples = if image.is_mono { pixels } else { pixels * 3 };
    let mut output = Vec::with_capacity(header.len() + samples * 2 + 2880);
    output.extend_from_slice(&header);
    if image.is_mono {
        for pixel in image.data.chunks_exact(3) {
            let stored = (pixel[1] as i32 - 32768) as i16;
            output.extend_from_slice(&stored.to_be_bytes());
        }
    } else {
        for channel in 0..3 {
            for pixel in image.data.chunks_exact(3) {
                let stored = (pixel[channel] as i32 - 32768) as i16;
                output.extend_from_slice(&stored.to_be_bytes());
            }
        }
    }
    let data_padding = (2880 - output.len() % 2880) % 2880;
    output.resize(output.len() + data_padding, 0);
    Ok(output)
}

fn write_fits_u16_atomic(path: &Path, image: &StackResult) -> Result<(), String> {
    let bytes = build_fits_bytes(image)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("result.fits");
    let temporary = parent.join(format!(".{}.{}.tmp", name, std::process::id()));
    let write_result = (|| -> Result<(), String> {
        let mut file = File::create(&temporary).map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        std::fs::rename(&temporary, path).map_err(|error| error.to_string())?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write_result
}

fn processed_export_path(source: &str, suffix: &str) -> PathBuf {
    let source_path = Path::new(source);
    let stem = source_path
        .file_stem()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("Zenith_Result"));
    let mut file_name = stem.to_os_string();
    file_name.push(suffix);
    source_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(file_name)
}

#[cfg(test)]
mod postprocess_io_tests {
    use super::*;

    fn color_image() -> StackResult {
        StackResult {
            data: vec![0, 32768, 65535, 1000, 2000, 3000],
            width: 2,
            height: 1,
            is_mono: false,
            is_surface: false,
        }
    }

    #[test]
    fn fits_is_block_aligned_and_uses_unsigned_16_bit_contract() {
        let bytes = build_fits_bytes(&color_image()).unwrap();
        assert_eq!(bytes.len() % 2880, 0);
        let header = String::from_utf8_lossy(&bytes[..2880]);
        assert!(header.contains("BITPIX  =                   16"));
        assert!(header.contains("BZERO   =                32768"));
        assert!(header.contains("NAXIS3  =                    3"));
        assert!(!header.contains("BAYERPAT"));
        assert_eq!(&bytes[2880..2882], &i16::MIN.to_be_bytes());
    }

    #[test]
    fn processed_export_name_replaces_the_source_extension() {
        assert_eq!(
            processed_export_path("/tmp/Mosaic_Result.tiff", "_Final_16bit.fits"),
            PathBuf::from("/tmp/Mosaic_Result_Final_16bit.fits")
        );
        assert_eq!(
            processed_export_path("planetary-master", "_Final.png"),
            PathBuf::from("planetary-master_Final.png")
        );
    }

    #[test]
    fn histogram_is_computed_from_16_bit_samples() {
        let histogram = compute_postprocess_histogram(&color_image(), "source");
        assert_eq!(histogram.bins, 1024);
        assert_eq!(histogram.red.iter().sum::<u64>(), 2);
        assert_eq!(histogram.luminance.iter().sum::<u64>(), 2);
        assert!(histogram.maximum > histogram.minimum);
        assert!(histogram.percentile_low >= histogram.minimum);
        assert!(histogram.percentile_high <= histogram.maximum);
        assert!(histogram.percentile_high >= histogram.percentile_low);
    }

    #[test]
    fn monochrome_detection_samples_beyond_neutral_canvas_padding() {
        let mut data = vec![0u16; 600 * 3];
        data[15..18].copy_from_slice(&[8000, 16000, 32000]);
        data[1500..1503].copy_from_slice(&[42000, 21000, 9000]);
        assert!(!rgb16_buffer_is_monochrome(&data));
    }

    #[test]
    fn monochrome_detection_accepts_replicated_rgb_signal() {
        let data = vec![0, 0, 0, 1200, 1200, 1200, 64000, 64000, 64000];
        assert!(rgb16_buffer_is_monochrome(&data));
    }

    #[test]
    fn advanced_colour_controls_preserve_monochrome_equality() {
        let mut data = vec![12000, 12000, 12000, 42000, 42000, 42000];
        let mut params = AdvancedColorParams::default();
        params.temperature = 1.0;
        params.tint = -1.0;
        params.hsl_saturation = [1.0; 8];
        params.exposure = 0.25;
        apply_advanced_postprocess(&mut data, 2, 1, true, &params);
        for pixel in data.chunks_exact(3) {
            assert_eq!(pixel[0], pixel[1]);
            assert_eq!(pixel[1], pixel[2]);
        }
    }

    #[test]
    fn solar_mono_curve_can_invert_and_false_colour_without_mutating_the_contract() {
        let mut data = vec![8_000u16, 8_000, 8_000, 48_000, 48_000, 48_000];
        let mut params = AdvancedColorParams::default();
        params.solar.enabled = true;
        params.solar.invert = true;
        params.solar.colorize = true;
        params.solar.color_strength = 1.0;
        params.solar.curve_points = vec![[0.0, 0.0], [0.45, 0.32], [1.0, 1.0]];
        apply_advanced_postprocess(&mut data, 2, 1, true, &params);
        assert!(
            data[0] > data[3],
            "la inversión debe convertir la muestra oscura en la más luminosa"
        );
        assert!(
            data[0] != data[1] || data[1] != data[2],
            "el falso color solar debe producir canales distintos"
        );
    }

    #[test]
    fn solar_filament_recovery_ignores_flat_fields_and_responds_to_structure() {
        let (width, height) = (24usize, 24usize);
        let mut params = AdvancedColorParams::default();
        params.solar.enabled = true;
        params.solar.colorize = false;
        params.solar.filament_amount = 0.8;
        params.solar.filament_radius = 1.0;
        params.solar.noise_guard = 0.7;

        let mut flat = vec![24_000u16; width * height * 3];
        let flat_before = flat.clone();
        apply_advanced_postprocess(&mut flat, width, height, true, &params);
        assert_eq!(flat, flat_before, "un campo plano no debe generar filamentos");

        let mut structured = vec![24_000u16; width * height * 3];
        for y in 5..19 {
            let x = 7 + (y % 3);
            structured[(y * width + x) * 3..(y * width + x) * 3 + 3].fill(15_000);
        }
        let before = structured.clone();
        apply_advanced_postprocess(&mut structured, width, height, true, &params);
        assert_ne!(
            structured, before,
            "una fibrilla coherente por encima del piso robusto debe responder"
        );
        for pixel in structured.chunks_exact(3) {
            assert_eq!(pixel[0], pixel[1]);
            assert_eq!(pixel[1], pixel[2]);
        }
    }

    #[test]
    fn neutral_advanced_recipe_is_identity() {
        let mut data = color_image().data;
        let original = data.clone();
        apply_advanced_postprocess(&mut data, 2, 1, false, &AdvancedColorParams::default());
        assert_eq!(data, original);
    }

    #[test]
    fn local_detail_controls_preserve_mono_and_ignore_a_flat_field() {
        let (width, height) = (16usize, 16usize);
        let mut flat = vec![24000u16; width * height * 3];
        let original = flat.clone();
        let mut params = AdvancedColorParams::default();
        params.texture = 0.8;
        params.clarity = 0.65;
        apply_advanced_postprocess(&mut flat, width, height, true, &params);
        assert_eq!(flat, original, "un campo plano no debe inventar textura");

        let mut structured = vec![12000u16; width * height * 3];
        for y in 5..11 {
            for x in 5..11 {
                let index = (y * width + x) * 3;
                structured[index..index + 3].fill(36000);
            }
        }
        let before = structured.clone();
        apply_advanced_postprocess(&mut structured, width, height, true, &params);
        assert_ne!(structured, before, "la estructura real debe responder a textura/claridad");
        for pixel in structured.chunks_exact(3) {
            assert_eq!(pixel[0], pixel[1]);
            assert_eq!(pixel[1], pixel[2]);
        }
    }

    #[test]
    fn hsl_hue_and_luminance_are_selective_and_scnr_only_reduces_green_excess() {
        let mut red = vec![52000u16, 9000, 7000];
        let mut hsl = AdvancedColorParams::default();
        hsl.hsl_hue[0] = 1.0;
        hsl.hsl_luminance[0] = 0.4;
        apply_advanced_postprocess(&mut red, 1, 1, false, &hsl);
        assert!(red[1] > 9000, "el matiz rojo positivo debe desplazarse hacia naranja");
        assert!(red.iter().copied().max().unwrap() > 52000, "la luminancia selectiva debe elevar el sector rojo");

        let mut green = vec![10000u16, 42000, 12000];
        let mut scnr = AdvancedColorParams::default();
        scnr.scnr_green = 1.0;
        apply_advanced_postprocess(&mut green, 1, 1, false, &scnr);
        assert!(green[1] <= 12000, "SCNR debe limitar el exceso verde a la referencia R/B");
        assert_eq!(green[0], 10000);
        assert_eq!(green[2], 12000);
    }

    #[test]
    fn contrast_preserves_the_luminance_pivot() {
        let pivot = 18000.0f32;
        let (mut r, mut g, mut b) = (pivot, pivot, pivot);
        apply_advanced_color_magic(
            &mut r, &mut g, &mut b, 1.0, 1.0, 1.8, 0.0, 0.0, 0.0, pivot, 65535.0, 0.0, 1.0, 1.0,
        );
        assert!((r - pivot).abs() < 0.5);
        assert!((g - pivot).abs() < 0.5);
        assert!((b - pivot).abs() < 0.5);
    }

    #[test]
    fn contrast_changes_luminance_without_rotating_hue() {
        let (mut r, mut g, mut b) = (9000.0f32, 18000.0f32, 27000.0f32);
        let rg_before = r / g;
        let bg_before = b / g;
        apply_advanced_color_magic(
            &mut r, &mut g, &mut b, 1.0, 1.0, 1.65, 0.0, 0.0, 0.0, 16000.0, 65535.0, 0.0, 1.0, 1.0,
        );
        assert!((r / g - rg_before).abs() < 1e-5);
        assert!((b / g - bg_before).abs() < 1e-5);
    }

    #[test]
    fn gamma_brightness_and_saturation_follow_their_independent_meanings() {
        let apply = |gamma: f32, saturation: f32, brightness: f32| {
            let (mut r, mut g, mut b) = (9000.0f32, 18000.0f32, 27000.0f32);
            apply_advanced_color_magic(
                &mut r,
                &mut g,
                &mut b,
                gamma,
                saturation,
                1.0,
                brightness,
                0.0,
                0.0,
                16000.0,
                65535.0,
                0.0,
                1.0,
                1.0,
            );
            (r, g, b)
        };

        let neutral = apply(1.0, 1.0, 0.0);
        let gamma_lift = apply(1.8, 1.0, 0.0);
        let brighter = apply(1.0, 1.0, 0.4);
        let saturated = apply(1.0, 1.8, 0.0);
        assert!(gamma_lift.1 > neutral.1, "gamma > 1 debe elevar medios tonos");
        assert!(brighter.1 > neutral.1, "brillo debe actuar como exposición positiva");
        assert!(
            saturated.2 - saturated.0 > neutral.2 - neutral.0,
            "saturación debe ampliar la separación cromática"
        );
    }
}
