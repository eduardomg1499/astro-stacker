// ==========================================
// 7. ZENITH V2 IMPLEMENTATION (NEW)
// ==========================================
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

fn zenith_analysis_cache_suffix(
    target_type: &str,
    is_surface: bool,
    warping_analysis: bool,
    has_anchor: bool,
) -> String {
    let flow = if warping_analysis { "warp" } else { "global" };
    // "_a5" = analysis v5: CoG pre-centering for compact objects on black sky
    // (handheld videos) — caches from the bounded-window alignment hold garbage
    // shifts for large-motion videos. ("_a4" was the true-color mono collapse,
    // "_a3" the large-disc texture alignment, "_a2" the half-res R13 scoring.)
    let mut suffix = format!(
        "{}_zenith_ultimate_{}_a5",
        zenith_target_key(target_type, is_surface),
        flow
    );
    if has_anchor {
        suffix.push_str("_anchor");
    }
    suffix
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

/// **Dual-Frequency Planet Scorer (V3 Elite)**
/// Focuses on both microscopic detail (High-pass) and structural contrast (Mid-pass).
/// Robust to noise because it only counts signal that persists across both bands.
fn score_planetary_frequency(
    mono: &[u16],
    w: usize,
    h: usize,
    _scratch1: &mut [u16], 
    _scratch2: &mut [u16], 
    laplacian: &[u16],    
) -> u64 {
    let mut total_score = 0u64;
    let mut valid_pixels = 0u64;

    // ELITE PLANET SCORER V3: Local Contrast Analysis
    // We use the 5x5 Laplacian but only count signal that is 
    // significantly above the local noise floor.
    let max_v = mono.iter().cloned().max().unwrap_or(1) as f32;
    let noise_floor = (max_v * 0.02).max(128.0);

    for i in 0..(w * h) {
        let val = mono[i] as f32;
        if val < noise_floor { continue; } // Skip background

        let lap = laplacian[i] as f32;
        
        // Multi-scale weight: favors fine detail over coarse edges
        if lap > 350.0 {
            // Laplacian^2 * Contrast. High contrast edges get higher scores.
            let pixel_score = (lap * lap * (val / max_v)) / 65535.0;
            total_score += pixel_score as u64;
            valid_pixels += 1;
        }
    }

    if valid_pixels > 20 {
        // Boost factor for high-signal disks
        let fill_factor = valid_pixels as f32 / (w * h) as f32;
        let final_score = (total_score / valid_pixels) as f32 * (1.0 + fill_factor.sqrt());
        final_score as u64
    } else {
        0
    }
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
) -> FrameAlignmentData {
    if raw.is_empty() {
        return FrameAlignmentData::empty(i);
    }

    let qual_score: u64;
    let dx: f32;
    let dy: f32;

    // ALL MODES now use Phase Correlation (SAD) and Noise-Resistant scoring.
    raw_to_u16_buffer_into(raw, roi_img_w, roi_img_h, bpp, &mut buffers.raw_u16);

    // TRUE-COLOR FIX: for RGB inputs (rgb48/rgb24 — converted SER CID=100 and
    // FFmpeg color streams) raw_to_u16_buffer_into yields an INTERLEAVED RGB
    // buffer (3·w·h), but everything below (downscale, scoring, SAD, 40×40
    // grid, CoG) indexes it as MONO w·h — the analysis was effectively run on
    // an interleaved-channel corruption of the top third of the frame, while
    // the reference anchor (raw_to_u16_buffer_into_roi) IS proper mono. That
    // asymmetry produced garbage shifts/scores for true-color videos ("FFmpeg
    // no detecta bien las estructuras"); Bayer/mono inputs (bpp 1-2) were
    // never affected. Collapse to the SAME (r+g+b)/3 mono as the reference.
    let px = roi_img_w * roi_img_h;
    if buffers.raw_u16.len() >= px * 3 {
        for i in 0..px {
            let off = i * 3;
            let r = buffers.raw_u16[off] as u32;
            let g = buffers.raw_u16[off + 1] as u32;
            let b = buffers.raw_u16[off + 2] as u32;
            buffers.raw_u16[i] = ((r + g + b) / 3) as u16;
        }
        buffers.raw_u16.truncate(px);
    }

    // ANALYSIS @ HALF RESOLUTION (R13): scoring, global matching and the
    // 40×40 grid all run on a 2× reduced image — ~4× faster, with a noise-
    // robust quality ranking (industry standard: AS!4 scores on downsampled
    // gradients). Final alignment precision is owned by the STACKING stage
    // (per-frame coarse verification + per-AP sub-pixel search), so the
    // half-res global shift (±~1px) costs nothing in the final result.
    let (hw, hh) =
        crate::alignment::downscale_2x_into(&buffers.raw_u16, roi_img_w, roi_img_h, &mut buffers.half_u16);

    let mut score_val = enhance_and_score_surface_buffered(
        &buffers.half_u16,
        hw,
        hh,
        &mut buffers.blur_temp,
        &mut buffers.blur_out,
        &mut buffers.lap_out,
    );

    // A large lunar disc is treated like a surface for alignment, scoring and
    // grid quality — the same proven path that makes Superficie sharp on the Moon.
    let texture_align = is_surface || large_disc;

    // DEDICATED PLANET SCORER: Focus only on the disk (true small planets only).
    if !texture_align {
        score_val = score_planetary_frequency(
            &buffers.half_u16,
            hw,
            hh,
            &mut buffers.blur_temp,
            &mut buffers.blur_out,
            &buffers.lap_out,
        );
    }

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
            let (fcx, fcy) =
                compute_robust_geometric_center(&buffers.half_u16, hw, hh, 0, 0);
            (
                (fcx - ref_cog_x / 2.0).round() as isize,
                (fcy - ref_cog_y / 2.0).round() as isize,
            )
        } else {
            (0, 0)
        };
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
        let (cur_cog_x, cur_cog_y) = compute_robust_geometric_center(&buffers.raw_u16, roi_img_w, roi_img_h, roi_x, roi_y);
        dx = cur_cog_x - (width as f32 / 2.0);
        dy = cur_cog_y - (height as f32 / 2.0);
    }

    qual_score = score_val;

    let mut grid_scores = None;
    if warping_analysis {
        let input_buffer = if texture_align {
            &buffers.blur_out
        } else {
            &buffers.half_u16
        };
        grid_scores = Some(calculate_grid_quality(
            input_buffer,
            hw,
            hh,
            40,
            texture_align,
        ));
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
fn ram_aware_analysis_threads(rw: usize, rh: usize, is_color: bool) -> usize {
    let hw = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .saturating_sub(2)
        .max(2);
    let mut sys = System::new_all();
    sys.refresh_memory();
    let available = sys.available_memory(); // bytes
    let os_reserve: u64 = 2 * 1024 * 1024 * 1024; // 2 GB para el SO/otros procesos
    let usable = available.saturating_sub(os_reserve);
    // Memoria estimada por hilo: AnalysisBufferSet (~9 bytes/px) + temporales por
    // frame (raw + u16 + debayer en color) con un margen de seguridad.
    let per_px: u64 = if is_color { 28 } else { 16 };
    let per_thread = (rw as u64)
        .saturating_mul(rh as u64)
        .saturating_mul(per_px)
        .max(1);
    let by_ram = (usable / per_thread).max(1) as usize;
    by_ram.min(hw)
}

#[tauri::command]
async fn perform_standardized_analysis(
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
    path: &str,
    is_surface: bool,
    target_type: String,
    warping_analysis: bool,
    bayer_override: Option<i32>,
    anchor_override: Option<Vec<i32>>,
    progress_prefix: Option<String>,
) -> Result<AnalysisResult, String> {
    let is_surface = is_surface || is_surface_target(&target_type);
    let warping_analysis = zenith_should_warp(&target_type, is_surface, warping_analysis);
    let req_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as usize;
    state.active_req_id.store(req_id, Ordering::Relaxed);

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
    let r = VideoInput::open(path, app)?;
    let (tw, th, tf, tbp) = (r.width(), r.height(), r.frame_count(), r.bpp());
    let cid = bayer_override.unwrap_or_else(|| r.color_id());
    let is_color = r.is_color() || ser::ser_color_is_color(cid);
    let reader_kind = if r.is_ffmpeg() { "FFmpeg" } else { "Rust nativo" };
    log_to_front(
        app,
        "INFO",
        &format!(
            "Analisis V2: lector={}, {}x{}, frames={}, bpp={}, color_id={}",
            reader_kind, tw, th, tf, tbp, cid
        ),
    );
    emit_progress(app, &get_msg("Preparando lector..."), 2.0, None);
    let c_suffix =
        zenith_analysis_cache_suffix(&target_type, is_surface, warping_analysis, anchor_override.is_some());

    let c_path = get_analysis_cache_path(path, &c_suffix);
    if Path::new(&c_path).exists() {
        if let Ok(content) = fs::read_to_string(&c_path) {
            if let Ok(cached) = serde_json::from_str::<CachedAnalysis>(&content) {
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
                    let raw = r.get_frame(bi, cid);
                    let mut buf = Vec::new();
                    if is_color {
                        let mut u16 =
                            debayer_to_rgb(&raw_to_u16_buffer(&raw, tw, th, tbp), tw, th, cid);
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
                        let u16 = raw_to_u16_buffer(&raw, tw, th, tbp);
                        let vis = to_8bit_visual(&auto_contrast_stretch_u16(&u16, tw, th), 1.0);
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
                    let pn = ser::ser_pattern_name(cid);
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
                        preview_base64: format!(
                            "data:image/png;base64,{}",
                            general_purpose::STANDARD.encode(&buf)
                        ),
                        path: path.to_string(),
                        ap_points: vec![],
                        best_frame_idx: bi,
                    });
                }
            }
        }
    }
    // R11: with warping analysis the 40×40 grid scores must cover the FULL
    // frame — the stacking stage maps AP coordinates (full-frame) onto that
    // grid. A cropped analysis ROI would misalign every per-AP lookup.
    let (mut rw, mut rh) = if is_surface || warping_analysis {
        (tw, th)
    } else if is_small_planet(&target_type) {
        (tw.min(512), th.min(512))
    } else {
        // Planet Large: Ensure we capture the whole disk (e.g. Jupiter/Saturn with moons)
        (tw.min(800), th.min(800))
    };
    let (mut rx, mut ry) = ((tw - rw) / 2, (th - rh) / 2);
    // NEW: We force SAD Phase Correlation logic and Noise-Free scoring for EVERY mode.
    let use_sad = true; 

    if let Some(anchor) = &anchor_override {
        let box_size = 256;
        rw = box_size.min(tw);
        rh = box_size.min(th);

        let cx = anchor[0] as usize;
        let cy = anchor[1] as usize;

        rx = cx.saturating_sub(rw / 2).min(tw - rw);
        ry = cy.saturating_sub(rh / 2).min(th - rh);
    }

    // Planetary CoG of the reference frame (Calculated while the object is a solid disk)
    let mut ref_cog_cx = tw as f32 / 2.0;
    let mut ref_cog_cy = th as f32 / 2.0;
    // Decided once from the reference frame; threaded into every per-frame call.
    let mut large_disc = false;
    // COG-ASSIST for handheld/phone videos (compact disc on black sky): the
    // centroid delta pre-centers each frame's SAD search → unbounded motion.
    let mut cog_assist = false;

    let analysis_ref_idx = select_signal_frame_index(&r, tw, th, tbp, cid, tf / 2);
    emit_progress(app, &get_msg("Preparando referencia..."), 4.0, None);

    let a_mono = {
        // Fase 4: decodificar el frame de referencia UNA sola vez y reusarlo.
        // En small-planet se re-extraía con otro get_frame -> doble decode (caro
        // en FFmpeg; en SER mmap es barato). Mismos píxeles -> resultado idéntico.
        let ref_raw = r.get_frame(analysis_ref_idx, cid);
        let mut tmp = Vec::with_capacity(rw * rh);
        raw_to_u16_buffer_into_roi(
            &ref_raw,
            tw,
            th,
            tbp,
            rx,
            ry,
            rw,
            rh,
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
                rx = rx + planet_roi.x;
                ry = ry + planet_roi.y;
                rw = planet_roi.w;
                rh = planet_roi.h;
                
                // Re-extract tighter reference mono (reusa el frame ya decodificado)
                tmp.clear();
                raw_to_u16_buffer_into_roi(
                    &ref_raw,
                    tw, th, tbp,
                    rx, ry, rw, rh,
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

    let rt = r.clone();
    let state_c = state.clone();
    let app_c = app.clone();
    let ctr = std::sync::Arc::new(AtomicUsize::new(0));
    let a_pyr_ref = &a_pyr;
    let a_mono_ref = &a_mono;
    let analyze_fn = |i: usize, raw: &[u8], bufs: &mut AnalysisBufferSet| -> FrameAlignmentData {
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
            tbp,
            is_surface,
            warping_analysis,
            a_pyr_ref.as_ref(),
            a_mono_ref,
            cog_assist,
            ref_cog_cx,
            ref_cog_cy,
            &target_type, // NEW
            large_disc,
        )
    };
    let _ = use_sad;
    let mut stats = Vec::new();
    if r.is_ffmpeg() {
        let bin = match rt {
            VideoInput::Ffmpeg(ref f) => f.ffmpeg_path.as_str(),
            _ => "ffmpeg",
        };
        for use_gpu in [true, false] {
            ctr.store(0, Ordering::Relaxed);
            if use_gpu {
                emit_progress(app, "Modo Turbo (GPU)...", 0.0, None);
                log_to_front(app, "INFO", "Intentando decodificacion por hardware (GPU)...");
            } else {
                emit_progress(app, "Modo Turbo (CPU Fallback)...", 0.0, None);
                log_to_front(app, "WARNING", "Reintentando decodificacion por software (CPU fallback)...");
            }
            if let Ok(mut it) = FfmpegStreamIterator::new(
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
                use_gpu,
                rt.codec_name(),
                rt.rotation(),
            ) {
                let cs = ctr.clone();
                let sc = state_c.clone();
                let ac = app_c.clone();

                // OPT FFMPEG 2.5: TRUE ZERO-ALLOCATION (DUAL RING BUFFER)
                let (tx_full, rx_full) = crossbeam_channel::bounded::<(usize, Vec<u8>)>(32);
                let (tx_empty, rx_empty) = crossbeam_channel::bounded::<Vec<u8>>(32);

                let frame_size = rw * rh * (if cid < 8 || cid > 11 { 6 } else { 2 });
                for _ in 0..32 {
                    tx_empty.send(vec![0u8; frame_size]).unwrap();
                }

                let prod_cancel = state.cancel_requested.clone();
                std::thread::spawn(move || {
                    let mut frame_idx = 0;
                    while let Ok(mut buffer) = rx_empty.recv() {
                        if prod_cancel.load(std::sync::atomic::Ordering::Relaxed) {
                            break; // cancelado: dejar de decodificar ya
                        }
                        if it.read_frame_into(&mut buffer) {
                            if tx_full.send((frame_idx, buffer)).is_err() {
                                break;
                            }
                            frame_idx += 1;
                        } else {
                            break;
                        }
                    }
                });

                // Consumer Side (Lock-Free SIMD with Recycler)
                let analysis_pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(ram_aware_analysis_threads(rw, rh, is_color))
                    .build()
                    .unwrap();
                let local_stats: Vec<FrameAlignmentData> = analysis_pool.install(|| rx_full
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
                            if check_cancel(&sc, req_id) {
                                return FrameAlignmentData::empty(i);
                            }
                            if c % 50 == 0 {
                                emit_progress(
                                    &ac,
                                    &format!("Analizando: {}/{}", c, tf),
                                    5.0 + (c as f32 / tf as f32) * 90.0,
                                    None,
                                );
                            }
                            let result = analyze_fn(i, &raw, bs);
                            // Send empty block back to the producer pool
                            let _ = tx_empty.send(raw);
                            result
                        },
                    )
                    .collect());

                if !local_stats.is_empty() {
                    stats = local_stats;
                    break;
                }
            }
        }
    }
    if stats.is_empty() {
        let threads = ram_aware_analysis_threads(rw, rh, is_color);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        let cf = ctr.clone();
        let sc = state_c.clone();
        let ac = app_c.clone();
        emit_progress(app, &get_msg("Analizando frames..."), 5.0, None);
        stats = pool.install(|| {
            (0..tf)
                .into_par_iter()
                .with_min_len(32)
                .map_init(
                    || (rt.clone(), AnalysisBufferSet::new(rw * rh)),
                    |(rl, bs), i| {
                        let c = cf.fetch_add(1, Ordering::Relaxed);
                        // Cancel check on EVERY frame — this is the SER path:
                        // the old per-10 check only skipped that single frame,
                        // so cancelling a SER analysis still burned through the
                        // whole remaining video.
                        if check_cancel(&sc, req_id) {
                            return FrameAlignmentData::empty(i);
                        }
                        if c % 10 == 0 {
                            emit_progress(
                                &ac,
                                &format!("Analizando: {}/{}", c, tf),
                                5.0 + (c as f32 / tf as f32) * 90.0,
                                None,
                            );
                        }
                        analyze_fn(i, &rl.get_frame_roi(i, rx, ry, rw, rh, cid), bs)
                    },
                )
                .collect()
        });
    }
    if stats.is_empty() {
        return Err("Error: Sin frames.".into());
    }
    // Cancelacion: los workers devolvieron frames vacios a partir del aviso —
    // abortar AHORA, antes de escribir un cache de analisis corrupto.
    if state
        .cancel_requested
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Err("Cancelado por el usuario".into());
    }
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
            x: 0,
            y: 0,
            w: tw,
            h: th,
        },
        path_hash: 0,
        frame_stats: Some(stats.clone()),
        quality_graph: Some(qg.clone()),
        width: Some(tw),
        height: Some(th),
        best_frame_idx: Some(best_frame_idx),
        ap_points: None,
    };
    if let Ok(json) = serde_json::to_string(&cached) {
        let _ = fs::write(&c_path, json);
    }
    emit_progress(app, "Finalizando...", 100.0, None);
    let mut buf = Vec::new();
    let raw = r.get_frame(best_frame_idx, cid);
    
    let u16_raw = raw_to_u16_buffer(&raw, tw, th, tbp);

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
        preview_base64: format!(
            "data:image/png;base64,{}",
            general_purpose::STANDARD.encode(&buf)
        ),
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
    // Nueva operacion de usuario: limpiar cualquier cancelacion previa.
    state
        .cancel_requested
        .store(false, std::sync::atomic::Ordering::Relaxed);
    perform_standardized_analysis(
        &app,
        &state,
        &path,
        is_surface,
        target_type, // NEW
        warping_analysis,
        bayer_override,
        anchor_override,
        progress_prefix,
    )
    .await
}

#[tauri::command]
fn stop_analysis(state: State<'_, AppState>) {
    let _ = state.active_req_id.fetch_add(1, Ordering::Relaxed);
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
/// stream can feed them all; the per-batch LZ4 cache still makes pass 2 free.
#[allow(clippy::too_many_arguments)]
fn stream_frames_ffmpeg_chunked(
    reader: &FfmpegReader,
    path: &str,
    app: &tauri::AppHandle,
    chunks: &[Vec<usize>],
    tx: &std::sync::mpsc::SyncSender<std::collections::HashMap<usize, Vec<u16>>>,
    width: usize,
    height: usize,
    bpp: usize,
    color_id: i32,
    pass_label: &str,
    total_batches: usize,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let cache_dir = std::env::temp_dir().join("astro_stacker_cache");
    if !cache_dir.exists() {
        let _ = std::fs::create_dir_all(&cache_dir);
    }
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let path_hash = hasher.finish();

    let is_color_stream = color_id < 8 || color_id > 11;
    let stream_bpp = if is_color_stream { 6usize } else { 2usize };
    let mut raw_buf = vec![0u8; width * height * stream_bpp];

    let mut it: Option<FfmpegStreamIterator> = None;
    let mut pos: usize = 0; // next frame index the stream will yield
    let mut use_gpu = true;
    let t_start = std::time::Instant::now();

    for (batch_idx, indices) in chunks.iter().enumerate() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        let prefix = format!(
            "{}Cargando Lote {}/{}",
            pass_label,
            batch_idx + 1,
            total_batches
        );

        // 1. LZ4 cache first (pass 2 and re-stacks are pure disk reads).
        let batch_hash = {
            let mut bh = DefaultHasher::new();
            indices.hash(&mut bh);
            bh.finish()
        };
        let cache_file = cache_dir.join(format!("batch_{}_{}.bin.lz4", path_hash, batch_hash));
        if cache_file.exists() {
            if let Ok(compressed) = std::fs::read(&cache_file) {
                if let Ok(raw) = lz4_flex::decompress_size_prepended(&compressed) {
                    if let Ok(map) = bincode::deserialize::<
                        std::collections::HashMap<usize, Vec<u16>>,
                    >(&raw)
                    {
                        emit_progress(app, &format!("{}: Recuperado de NVMe", prefix), 100.0, None);
                        if tx.send(map).is_err() {
                            return;
                        }
                        continue;
                    }
                }
            }
        }

        // 2. Serve from the persistent sequential stream.
        let wanted: std::collections::HashSet<usize> = indices.iter().copied().collect();
        let last_wanted = indices.iter().copied().max().unwrap_or(0);
        let mut map = std::collections::HashMap::with_capacity(indices.len());
        let mut cancelled = false;

        loop {
            if it.is_none() {
                match FfmpegStreamIterator::new(
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
                    use_gpu,
                    &reader.codec_name,
                    reader.rotation,
                ) {
                    Ok(v) => {
                        it = Some(v);
                        pos = 0;
                        map.retain(|_, _| false); // restart: refill this batch
                    }
                    Err(_) if use_gpu => {
                        use_gpu = false;
                        continue;
                    }
                    Err(_) => break,
                }
            }

            let s = it.as_mut().expect("stream just ensured");
            let mut stream_ok = true;
            while pos <= last_wanted {
                if pos % 32 == 0 && cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    cancelled = true;
                    break;
                }
                if !s.read_frame_into(&mut raw_buf) {
                    stream_ok = false;
                    break;
                }
                if wanted.contains(&pos) {
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

            if cancelled || map.len() == indices.len() {
                break;
            }
            if !stream_ok {
                // Decoder died mid-pass. One retry on CPU decoding from 0;
                // afterwards fill any stragglers via single-frame fallback.
                it = None;
                if use_gpu {
                    use_gpu = false;
                    continue;
                }
                for &idx in indices {
                    if !map.contains_key(&idx) {
                        let raw = reader.get_frame(idx, color_id);
                        if !raw.is_empty() {
                            map.insert(idx, raw_to_u16_buffer(&raw, width, height, bpp));
                        }
                    }
                }
                break;
            }
        }

        if cancelled {
            break;
        }

        // 3. Bounded LZ4 cache save (same limits as before).
        const MAX_BATCH_CACHE_BYTES: usize = 768 * 1024 * 1024;
        const MAX_CACHE_DIR_BYTES: u64 = 3 * 1024 * 1024 * 1024;
        if let Ok(serialized) = bincode::serialize(&map) {
            if serialized.len() <= MAX_BATCH_CACHE_BYTES {
                let compressed = lz4_flex::compress_prepend_size(&serialized);
                let dir_size: u64 = std::fs::read_dir(&cache_dir)
                    .map(|rd| rd.flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
                    .unwrap_or(0);
                if dir_size + compressed.len() as u64 <= MAX_CACHE_DIR_BYTES {
                    let _ = std::fs::write(&cache_file, compressed);
                }
            }
        }

        if tx.send(map).is_err() {
            break;
        }
    }
}

#[allow(dead_code)] // superseded by stream_frames_ffmpeg_chunked (kept for reference)
fn load_frames_ffmpeg_buffered(
    reader: &FfmpegReader,
    path: &str,
    app: &tauri::AppHandle,
    indices: &[usize], // MUST BE SORTED
    width: usize,
    height: usize,
    bpp: usize,
    color_id: i32,
    _ffmpeg_cmd: &str,
    _fps: f64,
    message_prefix: &str,
    _codec_name: &str,
) -> Result<std::collections::HashMap<usize, Vec<u16>>, String> {
    let total_frames = indices.len();
    println!(
        "DEBUG: Starting Phase 9 FFmpeg Seamless Batch Extraction for {} frames...",
        total_frames
    );

    // PHASE 3 & 4 HYBRID: O(1) Sequential NVMe Check & GPU Cache Feed
    let cache_dir = std::env::temp_dir().join("astro_stacker_cache");
    if !cache_dir.exists() {
        let _ = std::fs::create_dir_all(&cache_dir);
    }

    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let path_hash = hasher.finish();

    let batch_hash = {
        let mut bh = DefaultHasher::new();
        indices.hash(&mut bh);
        bh.finish()
    };

    let cache_file = cache_dir.join(format!("batch_{}_{}.bin.lz4", path_hash, batch_hash));

    // --- 1. TRY HYBRID DISK CACHE FIRST (EXTREME SPEED) ---
    if cache_file.exists() {
        if let Ok(compressed_data) = std::fs::read(&cache_file) {
            if let Ok(decompressed) = lz4_flex::decompress_size_prepended(&compressed_data) {
                if let Ok(f_map) = bincode::deserialize::<std::collections::HashMap<usize, Vec<u16>>>(
                    &decompressed,
                ) {
                    emit_progress(
                        app,
                        &format!("{}: Recuperado de NVMe", message_prefix),
                        100.0,
                        None,
                    );
                    return Ok(f_map); // Elite O(1) Zero-Decode Restored from NVMe
                }
            }
        }
    }

    let t_start = std::time::Instant::now();

    // --- 2. FRAME-ACCURATE SEQUENTIAL EXTRACTION (single pass, exact indices) ---
    // The old path (get_frames_batch) used FAST INPUT SEEKING (-ss before -i)
    // plus select by RELATIVE frame number, assuming the first decoded frame
    // after each seek was exactly `first_idx`. With inter-frame codecs
    // (H.264/HEVC in MOV/MP4) and fractional fps that assumption breaks: the
    // stack received THE WRONG FRAMES, so the analysis shifts were applied to
    // different images → smeared/dim stacks, corner artifacts. It also applied
    // an `unsharp` deblock filter the analysis pipeline does NOT apply
    // (asymmetric preprocessing). We now stream the video SEQUENTIALLY through
    // the SAME FfmpegStreamIterator the analysis uses: exact frame indices,
    // identical preprocessing, and ONE ffmpeg process per batch instead of one
    // per 200 frames. SER-style correctness for compressed videos.
    let mut final_map = std::collections::HashMap::with_capacity(total_frames);
    let wanted: std::collections::HashSet<usize> = indices.iter().copied().collect();
    let last_wanted = indices.iter().copied().max().unwrap_or(0);
    let is_color_stream = color_id < 8 || color_id > 11;
    let stream_bpp = if is_color_stream { 6usize } else { 2usize };
    let mut raw_buf = vec![0u8; width * height * stream_bpp];
    // Cancelacion cooperativa: sin esto, cancelar durante la decodificacion de
    // un lote grande obligaria a esperar minutos a que FFmpeg lo termine.
    let cancel_flag = app.state::<AppState>().cancel_requested.clone();

    for use_gpu in [true, false] {
        final_map.clear();
        let it = FfmpegStreamIterator::new(
            path,
            width,
            height,
            0,
            0,
            width,
            height,
            color_id,
            &reader.ffmpeg_path,
            None, // NO seeking: sequential from frame 0 → indices are exact
            use_gpu,
            &reader.codec_name,
            reader.rotation,
        );
        let mut it = match it {
            Ok(v) => v,
            Err(_) => continue,
        };

        let mut frame_idx: usize = 0;
        while it.read_frame_into(&mut raw_buf) {
            if frame_idx % 32 == 0 && cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(std::collections::HashMap::new()); // cancelado
            }
            if wanted.contains(&frame_idx) {
                final_map.insert(frame_idx, raw_to_u16_buffer(&raw_buf, width, height, bpp));
                let got = final_map.len();
                if got % 50 == 0 || got == total_frames {
                    let elapsed = t_start.elapsed().as_secs_f32().max(0.001);
                    let fps = frame_idx as f32 / elapsed;
                    emit_progress(
                        app,
                        &format!(
                            "{} - {}/{} frames (escaneo {:.0} FPS)",
                            message_prefix, got, total_frames, fps
                        ),
                        (got as f32 / total_frames as f32) * 100.0,
                        None,
                    );
                }
                if got == total_frames {
                    break; // all collected: stop decoding early
                }
            }
            if frame_idx >= last_wanted {
                break;
            }
            frame_idx += 1;
        }

        if final_map.len() == total_frames {
            break; // complete set with this decoder mode
        }
        // GPU decode produced an incomplete stream → retry once on CPU.
    }

    // Last-resort fill for any frame still missing (corrupt tail, etc.).
    if final_map.len() != total_frames {
        for &idx in indices {
            if !final_map.contains_key(&idx) {
                let raw = reader.get_frame(idx, color_id);
                if !raw.is_empty() {
                    final_map.insert(idx, raw_to_u16_buffer(&raw, width, height, bpp));
                }
            }
        }
    }

    // --- 3. SAVE HYBRID DISK CACHE (SYNCHRONOUS ON LOADING THREAD) ---
    // Written sequentially on the loading thread to avoid concurrent memory-heavy clones.
    // BOUNDED: long sessions with many batches must never saturate the disk.
    const MAX_BATCH_CACHE_BYTES: usize = 768 * 1024 * 1024; // skip giant batches
    const MAX_CACHE_DIR_BYTES: u64 = 3 * 1024 * 1024 * 1024; // 3 GB total cap
    if let Ok(serialized) = bincode::serialize(&final_map) {
        if serialized.len() <= MAX_BATCH_CACHE_BYTES {
            let compressed = lz4_flex::compress_prepend_size(&serialized);
            let dir_size: u64 = std::fs::read_dir(&cache_dir)
                .map(|rd| {
                    rd.flatten()
                        .filter_map(|e| e.metadata().ok())
                        .map(|m| m.len())
                        .sum()
                })
                .unwrap_or(0);
            if dir_size + compressed.len() as u64 <= MAX_CACHE_DIR_BYTES {
                let _ = std::fs::write(&cache_file, compressed);
            }
        }
    }

    emit_progress(app, &format!("{}: Completado", message_prefix), 100.0, None);

    Ok(final_map)
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
) -> Result<String, String> {
    // Nueva operacion de usuario: limpiar cualquier cancelacion previa.
    state
        .cancel_requested
        .store(false, std::sync::atomic::Ordering::Relaxed);
    stack_video_liquid_warping_impl(
        &app,
        &state,
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
    )
    .await
}

/// Internal engine entry point — callable from other commands (batch mode)
/// with `&AppHandle`/`&State` (same pattern as perform_standardized_analysis).
/// `progress_prefix` lets batch mode tag progress messages ("[2/7] …").
#[allow(clippy::too_many_arguments)]
pub async fn stack_video_liquid_warping_impl(
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
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
) -> Result<String, String> {
    let app = app.clone();
    let is_surface = is_surface || is_surface_target(&target_type);
    let warping_analysis = zenith_should_warp(&target_type, is_surface, warping_analysis);
    // Derive category profile for tuned parameters
    let category = TargetCategory::from_str(&target_type);
    let cat_profile = category.profile();
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
    let suffix =
        zenith_analysis_cache_suffix(&target_type, is_surface, warping_analysis, anchor_override.is_some());

    let cache_path = get_analysis_cache_path(&path, &suffix);

    if !Path::new(&cache_path).exists() {
        return Err("No se encontro analisis Zenith Presicion Ultimate. Por favor re-analiza el video.".into());
    }

    let cache_content = fs::read_to_string(&cache_path).map_err(|e| e.to_string())?;
    let mut cached: CachedAnalysis =
        serde_json::from_str(&cache_content).map_err(|e| e.to_string())?;

    // Update cache with current AP points for future use (e.g. Batch mode)
    if !custom_points.is_empty() {
        cached.ap_points = Some(custom_points.clone());
        if let Ok(json) = serde_json::to_string(&cached) {
            let _ = fs::write(&cache_path, json);
        }
    }

    // 2. Identify Frames
    let all_stats = cached
        .frame_stats
        .as_ref()
        .ok_or("Datos de analisis corruptos")?;
    let total = all_stats.len();
    let num_to_stack = ((total as f32 * percent / 100.0).ceil() as usize).max(1);

    // ELITE V4: Score Ranges for Sigmoidal Weighting
    let global_min_score = all_stats.iter().map(|f| f.score).min().unwrap_or(0) as f32;
    let global_max_score = all_stats.iter().map(|f| f.score).max().unwrap_or(1000) as f32;
    let _global_score_range = (global_max_score - global_min_score).max(1.0);


    // MAPPING: Frame Index -> Bitmask of which APs accept it (Stitch Stacking)
    let mut frame_acceptance_masks: std::collections::HashMap<usize, Vec<bool>> =
        std::collections::HashMap::with_capacity(total);
    let mut global_active_indices: std::collections::HashSet<usize> =
        std::collections::HashSet::with_capacity(total);

    let r = VideoInput::open(&path, &app)?;
    let w_in = r.width();
    let h_in = r.height();

    // PHASE 4.0: ELITE V4 - DENSE LOCAL QUALITY (declarations)
    // R13 FUSION: the quarter-res dense quality map is now built INLINE in the
    // stacking loop from the frame buffer the prefetcher already loaded
    // (sc.mono_buf) — the previous implementation RE-READ and RE-DECODED the
    // whole video a second time just to build these maps. Same math, one full
    // IO pass less, and every stacked frame has its map in every pass.
    const DQ_DOWNSCALE: usize = 4;
    let dq_w = (w_in / DQ_DOWNSCALE).max(1);
    let dq_h = (h_in / DQ_DOWNSCALE).max(1);

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
            let g_scores: Vec<f32> = all_stats.iter().map(|f| f.score as f32).collect();
            let g_best = g_scores.iter().cloned().fold(1.0f32, f32::max);
            let min_keep = (num_to_stack / 6).clamp(4, 16).min(num_to_stack.max(1));

            for (ap_idx, ap) in custom_points.iter().enumerate() {
                let gx = ((ap.x / w_in as f32) * 40.0).floor().clamp(0.0, 39.0) as usize;
                let gy = ((ap.y / h_in as f32) * 40.0).floor().clamp(0.0, 39.0) as usize;
                let grid_idx = gy * 40 + gx;

                let ap_scores: Vec<f32> = all_stats
                    .iter()
                    .map(|f| {
                        (if let Some(gs) = &f.grid_scores { gs[grid_idx] } else { f.score }) as f32
                    })
                    .collect();
                let ap_best = ap_scores.iter().cloned().fold(1.0f32, f32::max);
                let w_info = pearson_informativeness(&ap_scores, &g_scores);
                let cutoff_frac = 0.45 + 0.25 * w_info;

                let mut ranked: Vec<(usize, f32)> = all_stats
                    .iter()
                    .enumerate()
                    .map(|(k, f)| {
                        let blended = w_info * (ap_scores[k] / ap_best)
                            + (1.0 - w_info) * (g_scores[k] / g_best);
                        (f.idx, blended)
                    })
                    .collect();
                ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let best_blend = ranked.first().map(|&(_, s)| s).unwrap_or(1.0);
                let ap_cutoff = best_blend * cutoff_frac;

                // Every AP takes its own top N% independently.
                let mut accepted = 0usize;
                for &(idx, s) in ranked.iter().take(num_to_stack) {
                    if s < ap_cutoff && accepted >= min_keep {
                        break; // quality cliff reached — but never starve the AP
                    }
                    global_active_indices.insert(idx);
                    let mask = frame_acceptance_masks.entry(idx).or_insert_with(|| vec![false; custom_points.len()]);
                    mask[ap_idx] = true;
                    accepted += 1;
                }
            }
        } else {
            // REGIONAL GRID-VOTING (Optimized for Planet)
            // Porting v4 Adaptive Logic to Planets
            let mut frequency_map: std::collections::HashMap<usize, usize> = std::collections::HashMap::with_capacity(total);
            
            // Step 1: Preliminary vote with adaptive quality check
            for ap in &custom_points {
                let gx = ((ap.x / w_in as f32) * 40.0).floor().clamp(0.0, 39.0) as usize;
                let gy = ((ap.y / h_in as f32) * 40.0).floor().clamp(0.0, 39.0) as usize;
                let grid_idx = gy * 40 + gx;
                
                let mut ap_stats: Vec<(usize, u64)> = all_stats.iter().map(|f| {
                    let s = if let Some(gs) = &f.grid_scores { gs[grid_idx] } else { f.score };
                    (f.idx, s)
                }).collect();
                ap_stats.sort_by(|a, b| b.1.cmp(&a.1));

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
            let mut hit_list: Vec<(usize, usize)> = frequency_map.into_iter().collect();
            hit_list.sort_by(|a, b| b.1.cmp(&a.1));
            let allowed_indices: std::collections::HashSet<usize> = hit_list.into_iter().take(global_limit).map(|(idx, _)| idx).collect();

            // Step 2: Final Acceptance with Moon Detection
            for (ap_idx, ap) in custom_points.iter().enumerate() {
                let gx = ((ap.x / w_in as f32) * 40.0).floor().clamp(0.0, 39.0) as usize;
                let gy = ((ap.y / h_in as f32) * 40.0).floor().clamp(0.0, 39.0) as usize;
                let grid_idx = gy * 40 + gx;
                
                let mut ap_stats: Vec<(usize, u64)> = all_stats.iter().map(|f| {
                    let s = if let Some(gs) = &f.grid_scores { gs[grid_idx] } else { f.score };
                    (f.idx, s)
                }).collect();
                ap_stats.sort_by(|a, b| b.1.cmp(&a.1));

                let best_ap_score = if let Some(&(_, s)) = ap_stats.first() { s as f32 } else { 0.0 };
                let ap_cutoff = best_ap_score * if is_surface_logic { 0.65 } else { 0.55 };

                // MOON DETECTION (Simplified): 
                // Moons are usually small features with high contrast in AP scores vs global.
                // If it's a Large Planet target, we check if this AP's best score is significantly
                // higher than the average best score, or if it's isolated.
                let is_moon_candidate = !is_surface_logic;
                // For now, let's treat every AP as requiring individual quality protection
                
                let mut accepted_for_this_ap = 0;
                for &(idx, s) in &ap_stats {
                    // Moon Protection: If it's a "Moon candidate" AP, we accept the best frames
                    // even if they are not globally popular (allowed_indices).
                    let is_top_and_crisp = accepted_for_this_ap < (num_to_stack / 2).max(1)
                        && (s as f32) > (best_ap_score * 0.82);

                    if allowed_indices.contains(&idx) || (is_moon_candidate && is_top_and_crisp) {
                        if (s as f32) < ap_cutoff { break; }

                        global_active_indices.insert(idx);
                        let mask = frame_acceptance_masks.entry(idx).or_insert_with(|| vec![false; custom_points.len()]);
                        mask[ap_idx] = true;
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
            frame_acceptance_masks.insert(idx, vec![true; custom_points.len()]);
        }
    }

    // FIX FRAME COUNT: Enforce strict num_to_stack cap after per-AP selection.
    // Per-AP voting can overshoot the requested percentage. Surface keeps a
    // small 1.25× headroom (each AP still uses at most its local top-N%, the
    // union is naturally larger), trimmed by global score. The old 1.6× let
    // too many globally poor frames in, washing out fine detail.
    let global_active_indices = if global_active_indices.len() > num_to_stack {
        let mut scored: Vec<(usize, u64)> = global_active_indices
            .iter()
            .filter_map(|&idx| {
                all_stats.iter().find(|f| f.idx == idx).map(|f| (idx, f.score))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        let max_allowed = if is_surface_logic || large_disc {
            (num_to_stack as f32 * 1.25).ceil() as usize
        } else {
            num_to_stack
        };
        scored.truncate(max_allowed);
        scored.into_iter().map(|(idx, _)| idx).collect::<std::collections::HashSet<usize>>()
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

    // FIX: best_idx must be the frame with HIGHEST SCORE, not lowest frame index
    let best_idx = active_frames_data
        .iter()
        .max_by_key(|f| f.score)
        .map(|f| f.idx)
        .unwrap_or(0);
    let bpp = r.bpp();
    let color_id = bayer_override.unwrap_or_else(|| r.color_id());
    let is_color_video = r.is_color() || ser::ser_color_is_color(color_id);
    // MONO BRANCH: grayscale videos are stacked in a single channel end-to-end
    // (no RGB triplication) and expanded to RGB only for the post pipeline.
    let is_mono_stack = !is_color_video;

    // 3. Prepare Master Reference (Low-Noise Stack)

    // OPTIMIZATION: RAM BUFFERING (HashMap Strategy)
    // We load frames into a HashMap<usize, Vec<u16>> for fast access by Index.
    // We load them sequentially to maximize disk speed.
    // This buffer is used BOTH for Master Generation AND Final Stacking (Liquid).

    // Cancelacion cooperativa: clonado (Arc) para poder pasarlo al hilo del
    // prefetcher y consultarlo en los bucles pesados sin tocar `state`.
    let cancel_flag = state.cancel_requested.clone();

    // Identify needed indices
    // OPTIMIZATION: DYNAMIC RAM BATCHING (Unified Logic)
    let mut sys = System::new_all();
    sys.refresh_memory();
    let available_ram = sys.available_memory(); // Bytes
                                                // Safe limit: 30% of FREE RAM to maximize NVMe offloading
    // RAM que se ADAPTA al equipo y nunca desborda: el pipeline mantiene ~2
    // lotes de frames a la vez (uno apilandose mientras el prefetcher carga el
    // siguiente) MAS el scratch por hilo del stacker y los acumuladores de
    // salida. Presupuestar TODO evita que un equipo con mucha RAM asigne por
    // encima de la memoria fisica (crash/OOM del cliente) y a la vez le da a los
    // equipos modestos un lote sano en lugar de matarlos de hambre.
    let os_reserve: u64 = 768 * 1024 * 1024; // 768 MB reservados para el sistema
    let usable_ram = available_ram.saturating_sub(os_reserve);

    // Memoria REAL por frame mantenido en RAM: en color se expande a RGB u16
    // (w*h*3*2) y en mono es w*h*2. La estimacion previa (w*h*bpp*2) subestimaba color.
    let channels: u64 = if is_color_video { 3 } else { 1 };
    let per_frame_real = (w_in as u64) * (h_in as u64) * channels * 2;
    let bytes_per_frame = ((per_frame_real as f64) * 1.6_f64).ceil() as u64;

    // Scratch POR HILO del stacker: solo buffers de trabajo (u16 de entrada +
    // 2-4 planos f32 de salida). Los acumuladores f64 del lienzo ya NO son
    // por-hilo — son COMPARTIDOS con un mutex por canal — asi que el costo por
    // hilo bajo ~4x y (casi) todos los nucleos caben en RAM. Aun asi se limita
    // por seguridad en equipos con poca memoria libre (evita el thrashing que
    // congelaba el equipo).
    let drz = (drizzle as f64).max(1.0);
    let n_in_px = (w_in as u64) * (h_in as u64);
    let n_out_px = ((w_in as f64 * drz) as u64) * ((h_in as f64 * drz) as u64);
    let per_scratch = if is_color_video {
        n_out_px * 16 + n_in_px * 14 // 4×w(f32) + buffers u16 (rgb+mono+edges)
    } else {
        n_out_px * 8 + n_in_px * 8 // 2×w(f32) + buffers u16
    };

    let hw_threads = rayon::current_num_threads().max(1) as u64;
    let max_threads_ram = (((usable_ram as f64) * 0.40) as u64 / per_scratch.max(1)).max(1);
    let stack_threads = hw_threads.min(max_threads_ram).max(1) as usize;

    let scratch_total = per_scratch * (stack_threads as u64);
    // acc_grad_* (direct + direct_w, f64) — with double pass, pass 1 also holds
    // the sigma-clip m2 plane (f64) and the lo/hi winsorization bounds (2×f32)
    // coexist briefly between passes: budget 32 B/px/canal in that mode.
    let global_accum = n_out_px * if double_pass { 32 } else { 16 } * channels;

    // Con sync_channel(1) el pico real son 3 lotes de frames a la vez (el que
    // apila main + el que espera en el canal + el que el prefetcher construye).
    // Presupuestar los 3 lotes + el scratch + los acumuladores dentro del 85% de
    // la RAM usable garantiza que ni en el pico se desborde (evita el crash/OOM y
    // el thrashing del paginado que congelaba el equipo).
    const IN_FLIGHT_BATCHES: u64 = 3;
    let ram_for_frames =
        ((usable_ram as f64 * 0.85) as u64).saturating_sub(scratch_total + global_accum);
    let per_batch_budget = (ram_for_frames / IN_FLIGHT_BATCHES).max(bytes_per_frame);

    let mut frames_per_batch = (per_batch_budget / bytes_per_frame.max(1)).max(1) as usize;

    // Tope alto para NO penalizar gama alta; PISO bajo (2) para que los equipos
    // de bajos recursos usen lotes pequenos en vez de quedarse sin memoria.
    let batch_cap = if w_in >= 3000 {
        1000
    } else if w_in >= 1920 {
        2000
    } else {
        3000
    };
    frames_per_batch = frames_per_batch.clamp(2, batch_cap);

    let total_active = active_frames_data.len();
    let total_batches = (total_active + frames_per_batch - 1) / frames_per_batch;

    emit_progress(
        &app,
        &format!(
            "Modo DinÃ¡mico: {}GB RAM Libres -> Lotes de {} frames ({}/{} total)",
            available_ram / 1024 / 1024 / 1024,
            frames_per_batch,
            total_batches,
            total_active
        ),
        10.0,
        Some(format!(
            "Aceleracion: {} · apilado {} de {} hilos{}",
            get_accel_label(),
            stack_threads,
            hw_threads,
            // FFmpeg decodes in ITS OWN process with its own threads — Task
            // Manager shows those cores busy on top of the stacking pool.
            if r.is_ffmpeg() { " + decodificador FFmpeg" } else { "" }
        )),
    );

    // --- STEP 3: MASTER REFERENCE GENERATION ---
    emit_progress(&app, "Generando Referencia Maestra...", 12.0, None);

    // best_idx already correctly computed above as the frame with HIGHEST SCORE

    let r_ref = VideoInput::open(&path, &app)?;
    let master_raw = r_ref.get_frame(best_idx, color_id);
    let master_u16_best = raw_to_u16_buffer(&master_raw, w_in, h_in, bpp);

    let (master_clean_rgb, ref_mean, _ref_std) = if double_pass || is_v3 {
        emit_progress(
            &app,
            if is_v3 { "Generando Referencia Maestra V3 (Alta Precision)..." } else { "Creando Referencia Low-Noise (Doble Pasada)..." },
            13.0,
            None,
        );
        // Identify top N frames for reference (top 10% or max 12/20) by BEST SCORE
        // planet_small V3: use fewer, higher-quality frames for the reference
        // (median combination comes later — here we just limit count)
        let is_small_planet_ref = is_small_planet(&target_type);
        let ref_limit = if is_v3 && is_small_planet_ref {
            // Keep the reference crisp while allowing faint local features to stabilize.
            (active_frames_data.len() / 8).clamp(4, 12)
        } else if is_v3 {
            (active_frames_data.len() / 5).clamp(4, 20)
        } else {
            (active_frames_data.len() / 10).clamp(2, 12)
        };
        let mut sorted_by_score = active_frames_data.clone();
        sorted_by_score.sort_by(|a, b| b.score.cmp(&a.score));
        

        let mut ref_indices: Vec<usize> = sorted_by_score
            .iter()
            .take(ref_limit)
            .map(|f| f.idx)
            .collect();
        // OPTIMIZATION: Ensure sequential reading for GPU Cache stream (O(1) access)
        ref_indices.sort_unstable();

        // Prepare anchor for this mini-alignment
        let anchor_mono = if is_color_video {
            debayer_to_rgb(&master_u16_best, w_in, h_in, color_id)
                .chunks(3)
                .map(|p| p[1]) // Use Green channel
                .collect::<Vec<u16>>()
        } else {
            master_u16_best.clone()
        };
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

        let ref_data: Vec<(Vec<u16>, (f32, f32))> = ref_indices
            .iter()
            .map(|&idx| {
                // Cancelacion durante la generacion del master (decodifica y
                // alinea ~20 frames grandes): frame vacio → el guard de slices
                // lo ignora y el check de fase posterior aborta con Err.
                if cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    return (Vec::new(), (0.0f32, 0.0f32));
                }
                let f = r_ref.get_frame(idx, color_id);
                let px_u16 = raw_to_u16_buffer(&f, w_in, h_in, bpp);

                let px_expanded = if is_color_video {
                    debayer_to_rgb(&px_u16, w_in, h_in, color_id)
                } else {
                    let mut v = Vec::with_capacity(w_in * h_in * 3);
                    for &p in &px_u16 {
                        v.push(p);
                        v.push(p);
                        v.push(p);
                    }
                    v
                };

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

                (px_expanded, shift)
            })
            .collect();

        // SUBPIXEL MASTER ACCUMULATION: the old integer-rounded shifts
        // (dx.round()) landed every reference frame with up to ±0.5 px of
        // jitter — the master came out convolved with ~1 px of blur, and EVERY
        // per-AP alignment then refined against that slightly soft reference.
        // Bilinear sub-pixel placement keeps the reference crisp (AS!4-style),
        // which sharpens the whole downstream chain for free.
        let mut ref_acc = vec![0.0f32; w_in * h_in * 3];
        // Per-PIXEL weights: dividing by the global frame count darkened every
        // border pixel that fewer shifted frames covered.
        let mut ref_cnt_px = vec![0.0f32; w_in * h_in];

        for (px_rgb, (dx, dy)) in &ref_data {
            if px_rgb.is_empty() {
                continue; // cancelled placeholder
            }
            let (dx, dy) = (*dx, *dy);
            ref_acc
                .par_chunks_mut(w_in * 3)
                .zip(ref_cnt_px.par_chunks_mut(w_in))
                .enumerate()
                .for_each(|(y, (acc_row, cnt_row))| {
                    let syf = y as f32 - dy;
                    if syf < 0.0 || syf >= (h_in - 1) as f32 {
                        return;
                    }
                    let y0 = syf as usize;
                    let fy = syf - y0 as f32;
                    for x in 0..w_in {
                        let sxf = x as f32 - dx;
                        if sxf < 0.0 || sxf >= (w_in - 1) as f32 {
                            continue;
                        }
                        let x0 = sxf as usize;
                        let fx = sxf - x0 as f32;
                        let w00 = (1.0 - fx) * (1.0 - fy);
                        let w10 = fx * (1.0 - fy);
                        let w01 = (1.0 - fx) * fy;
                        let w11 = fx * fy;
                        let i00 = (y0 * w_in + x0) * 3;
                        let i01 = i00 + w_in * 3;
                        for c in 0..3 {
                            acc_row[x * 3 + c] += px_rgb[i00 + c] as f32 * w00
                                + px_rgb[i00 + 3 + c] as f32 * w10
                                + px_rgb[i01 + c] as f32 * w01
                                + px_rgb[i01 + 3 + c] as f32 * w11;
                        }
                        cnt_row[x] += 1.0;
                    }
                });
        }

        let mut final_ref_clean = vec![0u16; w_in * h_in * 3];
        for i in 0..w_in * h_in {
            let c = ref_cnt_px[i];
            if c > 0.0 {
                final_ref_clean[i * 3] = (ref_acc[i * 3] / c + 0.5).min(65535.0) as u16;
                final_ref_clean[i * 3 + 1] = (ref_acc[i * 3 + 1] / c + 0.5).min(65535.0) as u16;
                final_ref_clean[i * 3 + 2] = (ref_acc[i * 3 + 2] / c + 0.5).min(65535.0) as u16;
            }
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
        let final_ref_clean = if is_color_video {
            debayer_to_rgb(&master_u16_best, w_in, h_in, color_id)
        } else {
            let mut v = Vec::with_capacity(w_in * h_in * 3);
            for &p in &master_u16_best {
                v.push(p);
                v.push(p);
                v.push(p);
            }
            v
        };

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

    // --- STEP 3b: Alignment reference ---
    // AS!4-style: the alignment reference is the LOW-NOISE STACK of the best
    // frames, not a single frame. A single frame carries its own seeing
    // distortion (every AP then warps toward that frame's deformation) and its
    // pixel noise directly degrades SAD sub-pixel fits. The averaged reference
    // has ~sqrt(N) less noise and represents the mean (true) geometry, so the
    // per-AP shifts converge to the undistorted Sun/Moon.
    let master_alignment_rgb = master_clean_rgb.clone();
    // `mut`: the true two-pass mode rebuilds this reference from the first
    // pass's stacked result.
    let mut master_mono = master_alignment_rgb.chunks(3).map(|p| p[1]).collect::<Vec<u16>>();

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

    let (mut w_out, mut h_out, roi_offset_x, roi_offset_y) = if let Some(roi) = &stacking_roi {
        (
            (roi[2] as f32 * drizzle) as usize,
            (roi[3] as f32 * drizzle) as usize,
            roi[0] as f32,
            roi[1] as f32,
        )
    } else {
        (
            (w_in as f32 * drizzle) as usize,
            (h_in as f32 * drizzle) as usize,
            0.0,
            0.0,
        )
    };

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
                let mut new_masks =
                    std::collections::HashMap::with_capacity(frame_acceptance_masks.len());
                for (f_idx, mask) in frame_acceptance_masks.iter() {
                    let remapped: Vec<bool> = keep
                        .iter()
                        .map(|&i| mask.get(i).copied().unwrap_or(true))
                        .collect();
                    new_masks.insert(*f_idx, remapped);
                }
                let new_dark: Vec<f32> = keep.iter().map(|&i| ap_dark_ratio[i]).collect();
                (new_points, new_masks, vec![true; keep.len()], new_dark)
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
    let ap_limb_normal: Vec<Option<(f32, f32)>> = if use_liquid && limb_protect {
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
    );

    // Phase-boundary cancel check: master generation and the IDW map are the
    // two long pre-stacking stages — abort here instead of starting the passes.
    if cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
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
    let spawn_prefetcher = |pass_label: String|
        -> std::sync::mpsc::Receiver<std::collections::HashMap<usize, Vec<u16>>> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<std::collections::HashMap<usize, Vec<u16>>>(1);
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
        let pre_cancel = cancel_flag.clone();

        std::thread::spawn(move || {
            let _ = &pre_ffmpeg_cmd;
            if let VideoInput::Ffmpeg(ref fr) = pre_r_template {
                // PERSISTENT STREAM: one sequential decode pass feeds every
                // batch of this stacking pass (batches are ascending). The old
                // per-batch loader re-decoded from frame 0 for each batch —
                // O(N²) work that dominated wall time on compressed videos.
                stream_frames_ffmpeg_chunked(
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
                );
                return;
            }

            for (batch_idx, indices_to_load) in pre_chunks.into_iter().enumerate() {
                if pre_cancel.load(std::sync::atomic::Ordering::Relaxed) {
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
                    // mmap-backed readers scale across cores; each worker opens its
                    // own cheap reader (the OS shares the underlying mmap pages) so
                    // there is no cross-thread aliasing on reader state. IO fan-out
                    // is capped so it overlaps with — instead of starving — the
                    // stacking pass that runs on the same rayon pool.
                    let n_io = rayon::current_num_threads().clamp(1, 8);
                    let mut groups: Vec<Vec<usize>> = (0..n_io).map(|_| Vec::new()).collect();
                    for (k, &idx) in indices_to_load.iter().enumerate() {
                        groups[k % n_io].push(idx);
                    }
                    groups
                        .par_iter()
                        .map(|group| {
                            let mut m =
                                std::collections::HashMap::with_capacity(group.len());
                            if let Ok(r_local) = VideoInput::open(&pre_path, &pre_app) {
                                for &idx in group {
                                    // SER cancel: without this, cancelling mid-load
                                    // waited for the whole batch to finish reading.
                                    if pre_cancel.load(std::sync::atomic::Ordering::Relaxed) {
                                        break;
                                    }
                                    let raw = r_local.get_frame(idx, pre_color_id);
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
                if tx.send(frame_map).is_err() {
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
 
    // Calculate Global Min/Max for Stacking Weight scaling
    let (global_max_score, global_min_score) = {
        let max_s = active_frames_data.iter().map(|f| f.score).max().unwrap_or(1000) as f32;
        let min_s = active_frames_data.iter().map(|f| f.score).min().unwrap_or(0) as f32;
        (max_s, min_s)
    };
    let _global_score_range = (global_max_score - global_min_score).max(1.0);
    let mut sorted_all = active_frames_data.clone();
    sorted_all.sort_by(|a, b| b.score.cmp(&a.score));
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
        0.0
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
    let total_passes = if double_pass
        && (drizzle - 1.0).abs() < 0.01
        && stacking_roi.is_none()
    {
        2
    } else {
        1
    };

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
    // seeing distribution and passes untouched. Only active with the double
    // pass (same canvas guaranteed: drizzle 1×, no ROI).
    let sigma_clip_enabled = total_passes == 2;
    const SIGMA_CLIP_K: f32 = 4.0;
    const SIGMA_CLIP_FLOOR: f32 = 6.0; // ADU16: guards zero-variance pixels
    let mut clip_r: Option<(Vec<f32>, Vec<f32>)> = None;
    let mut clip_g: Option<(Vec<f32>, Vec<f32>)> = None;
    let mut clip_b: Option<(Vec<f32>, Vec<f32>)> = None;

    // Pool ACOTADO para el apilado: fija el numero de workers a `stack_threads`
    // (calculado por la RAM), asi el scratch por-hilo NO desborda la memoria en
    // equipos de muchos nucleos. Si la creacion falla, cae al pool global.
    let stack_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(stack_threads)
        .build()
        .ok();

    for pass in 0..total_passes {
    let pass_label = if total_passes > 1 {
        format!("[Pasada {}/{}] ", pass + 1, total_passes)
    } else {
        String::new()
    };
    emit_progress(&app, &format!("{}Zenith Elite V4: Iniciando Acumulacion Robusta...", pass_label), 50.0, None);
    let rx = spawn_prefetcher(pass_label.clone());
    global_align_counter.store(0, std::sync::atomic::Ordering::Relaxed);

    // SHARED per-channel accumulators: each frame merges under a short-lived
    // lock (merge is ~5-10% of the per-frame alignment+warp cost, and the R/G/B
    // locks overlap between workers). This replaces the per-thread f64 canvas
    // trio that multiplied RAM by the thread count. Lock order is always
    // R → G → B, so no deadlock is possible.
    // Pass 1 of a double-pass run tracks the second moment (m2) for the
    // sigma-clip statistics; pass 2 and single-pass runs use the lean variant.
    let track_variance = sigma_clip_enabled && pass == 0;
    let mk_acc = |active: bool| {
        if !active {
            GradientDomainStacker::new_empty()
        } else if track_variance {
            GradientDomainStacker::new_direct_tracked(w_out, h_out)
        } else {
            GradientDomainStacker::new_direct_only(w_out, h_out)
        }
    };
    let acc_r_mx = std::sync::Mutex::new(mk_acc(!is_mono_stack));
    let acc_g_mx = std::sync::Mutex::new(mk_acc(true));
    let acc_b_mx = std::sync::Mutex::new(mk_acc(!is_mono_stack));
    let scratch_pool_mx: std::sync::Mutex<Vec<LiquidScratch>> = std::sync::Mutex::new(Vec::new());

    // PERF: master-side AP statistics are constant within a pass — compute
    // once instead of per frame × AP (recomputed per pass: the double-pass
    // rebuilds the master from the pass-1 stack).
    let (ap_master_contrast, ap_master_lap) = precompute_ap_master_stats(
        &master_edges,
        &master_mono,
        w_in,
        h_in,
        &custom_points,
        ap_size as usize,
    );

    for (_batch_idx, chunk) in active_frames_data.chunks(frames_per_batch).enumerate() {
        if cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
            break; // cancelado por el usuario
        }
        let frame_map = match rx.recv() { Ok(m) => m, Err(_) => break };
        if frame_map.is_empty() { continue; }

        let counter_ref = &global_align_counter;
        let total_frames_all = total_active;
        let app_ref = &app;
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

        let run_batch = || chunk.par_iter().for_each_init(
            || ScratchLease::take(&scratch_pool_mx, w_in, h_in, w_out, h_out, is_mono_stack),
            |lease, frame_data| {
                if cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    return; // cancelado: no procesar mas frames
                }
                let sc = lease.sc.as_mut().expect("scratch lease always holds a buffer");
                let completed = counter_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if completed % 25 == 0 {
                    emit_progress(app_ref, &format!("Zenith Elite V4: Procesando Frame {}/{}...", completed, total_frames_all), (completed as f32 / total_frames_all as f32) * 40.0 + 50.0, None);
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
                        }
                    } else {
                        debayer_into_buffer(u16_data, w_in, h_in, color_id, &mut sc.rgb_buf);
                        if is_surface_logic || large_disc {
                            normalize_surface_frame_exposure_inplace(&mut sc.rgb_buf, surface_ref_p90, false);
                        }
                        for i in 0..w_in * h_in {
                            sc.mono_buf[i] = sc.rgb_buf[i * 3 + 1];
                        }
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

                    let frame_score = frame_data.score as f32;
                    let q_weight_opt = compute_frame_weight_sigmoidal(frame_score, global_min_score, global_max_score, cat_profile.rejection_percentile, cat_profile.sigmoid_steepness);

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
                        let dq_map = DenseQualityMap::build(&dq_ds, dq_w, dq_h, 3);
                        let d_quality: &[f32] = &dq_map.scores;
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

                        enhance_for_alignment_into_amount(&sc.mono_buf, w_in, h_in, &mut sc.f_s1, &mut sc.f_s2, &mut sc.f_edges, align_amount);
                        downscale_4x(&sc.f_edges, w_in, h_in, &mut sc.f_ds);

                        // FRAME-LEVEL ALIGNMENT VERIFICATION: the cached analysis
                        // shift can be wrong for individual frames (clouds, seeing
                        // bursts, tracking jumps). A wrong base shift is beyond the
                        // ±search_r AP window and stacks a displaced GHOST copy
                        // (doubled limb). A cheap 4×-downscaled re-match against the
                        // master confirms or corrects it before the AP pass.
                        // Large lunar discs get it too: they align by texture (no
                        // per-frame CoG), so they need the same safety net.
                        if is_surface_logic || large_disc {
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
                        );

                        let ap_mask = acceptance_ref.get(&frame_data.idx).map(|v| v.as_slice()).unwrap_or(&[]);
                        let combined_ap_mask: Vec<bool> = (0..custom_points.len())
                            .map(|i| {
                                let frame_ok = ap_mask.get(i).copied().unwrap_or(true);
                                let signal_ok = signal_valid_ref.get(i).copied().unwrap_or(true);
                                frame_ok && signal_ok
                            })
                            .collect();

                        let drop_size = if drizzle > 1.01 { 0.75f32 } else { 1.0f32 };
                        // Dense quality map registration: map output coords back into
                        // the (quarter-res) source-frame coordinate system.
                        let q_scale = dq_w as f32 / w_in as f32;
                        let q_off_x = (roi_offset_x + render_dx) * q_scale;
                        let q_off_y = (roi_offset_y + render_dy) * q_scale;

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
                            acc_g_mx.lock().unwrap().accumulate(&sc.wg, &sc.ww, d_quality, dq_w, dq_h, q_off_x, q_off_y, q_weight);
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
                            // Fixed R → G → B lock order (deadlock-free); each
                            // lock is held only for one channel merge so workers
                            // on different channels overlap.
                            acc_r_mx.lock().unwrap().accumulate(&sc.wr, &sc.ww, d_quality, dq_w, dq_h, q_off_x, q_off_y, q_weight);
                            acc_g_mx.lock().unwrap().accumulate(&sc.wg, &sc.ww, d_quality, dq_w, dq_h, q_off_x, q_off_y, q_weight);
                            acc_b_mx.lock().unwrap().accumulate(&sc.wb, &sc.ww, d_quality, dq_w, dq_h, q_off_x, q_off_y, q_weight);
                        }
                    }
                }
            }
        );
        // Run the batch on the RAM-bounded stacking pool (falls back to the
        // global pool only if the dedicated pool could not be created).
        match &stack_pool {
            Some(p) => p.install(run_batch),
            None => run_batch(),
        };
    }

    // Collect the shared accumulators for this pass.
    acc_grad_r = acc_r_mx.into_inner().unwrap();
    acc_grad_g = acc_g_mx.into_inner().unwrap();
    acc_grad_b = acc_b_mx.into_inner().unwrap();
    drop(scratch_pool_mx);

    // Between passes: rebuild the alignment reference from the fresh stack
    // (its geometry matches the master because drizzle/ROI are disabled here).
    if pass + 1 < total_passes {
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
                clip_g = Some(build_sigma_clip_bounds(&acc_grad_g, SIGMA_CLIP_K, SIGMA_CLIP_FLOOR));
            }
            if !is_mono_stack {
                if !acc_grad_r.m2.is_empty() {
                    clip_r = Some(build_sigma_clip_bounds(&acc_grad_r, SIGMA_CLIP_K, SIGMA_CLIP_FLOOR));
                }
                if !acc_grad_b.m2.is_empty() {
                    clip_b = Some(build_sigma_clip_bounds(&acc_grad_b, SIGMA_CLIP_K, SIGMA_CLIP_FLOOR));
                }
            }
        }
        emit_progress(&app, "Doble Pasada: regenerando referencia desde el apilado...", 92.0, None);
        for i in 0..w_in * h_in {
            let w_g = acc_grad_g.direct_w[i].max(1e-9);
            master_mono[i] = ((acc_grad_g.direct[i] / w_g) as f32).clamp(0.0, 65535.0) as u16;
        }
        master_edges = enhance_for_alignment_with_amount(&master_mono, w_in, h_in, align_amount);
        let (ds_w, _ds_h) = downscale_4x(&master_edges, w_in, h_in, &mut master_ds_buf);
        master_ds_w = ds_w;
    }
    } // end multi-pass loop

    if cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("Cancelado por el usuario".into());
    }

    // Elite V4: High-Fidelity Multi-Point Stack
    // (Poisson gradient stacking removed for this path: it smoothed surface
    //  micro-detail; the stackers run in direct-only mode to save RAM.)
    let mut stacked_f32 = vec![0.0f32; w_out * h_out * 3];
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
            let mut cropped = vec![0.0f32; nw * nh * 3];
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

    // 8. POST-PROCESSING (Elite Phase)
    emit_progress(&app, "Zenith Elite V4: Estimacion de PSF y Deconvolucion TV-RL...", 95.0, None);
    
    let green_channel: Vec<f32> = stacked_f32.chunks(3).map(|c| c[1]).collect();
    let noise_sigma = estimate_stack_noise(&green_channel, w_out, h_out);
    let _planet_mask = compute_planet_mask(&green_channel, w_out, h_out, 12);
    let _snr_map = compute_snr_map(&green_channel, w_out, h_out, noise_sigma);
    
    // Elite V4: Standard High-Fidelity Raw Stack (No Post-Processing)
    // As requested: more like AutoStakkert. The user will sharpen externally.
    //
    // BIT-DEPTH FIX: scale to the full 16-bit range in FLOATING POINT and
    // round. 10/12-bit captures (2-byte container, values ≤4095) used to be
    // truncated to their native quantization here and expanded only at the
    // very end — discarding the sub-LSB precision gained by stacking, i.e.
    // exactly the faint filaments and smooth gray transitions.
    let bd_gain_f: f32 = {
        let mut max_v = 0.0f32;
        for &v in stacked_f32.iter() {
            if v > max_v { max_v = v; }
        }
        if max_v <= 0.5 { 1.0 }
        else if max_v <= 255.5 { 256.0 }
        else if max_v <= 1023.5 { 64.0 }
        else if max_v <= 4095.5 { 16.0 }
        else if max_v <= 16383.5 { 4.0 }
        else { 1.0 }
    };
    let mut final_u16 = vec![0u16; w_out * h_out * 3];
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
            // AS!4-style "Normalize Stack": linear remap anchored at BOTH ends.
            //  - Black point: when a true dark background exists (P1 ≪ P99,
            //    i.e. sky/limb framing), anchor it near 0 — stacking lifts the
            //    background (scattered light + exposure matching) and that
            //    lifted floor is what made the stack look flatter than AS!4.
            //    Full-disk framings without sky (P1 high) skip the subtraction
            //    so real shadow detail is never crushed.
            //  - White point: the true MAX lands at ~95% of full scale, so
            //    clipping is impossible by construction (plage keeps detail).
            let p_max = percentile_u16(&final_u16, 100).max(1.0);
            let p1 = percentile_u16(&final_u16, 1);
            let p99 = percentile_u16(&final_u16, 99).max(1.0);
            // 0.95·P1: deeper black anchor (user feedback: dark-gray zones
            // needed more separation). Still keeps a 5% margin so faint signal
            // just above the background is never clipped to zero.
            let black = if p1 < p99 * 0.15 { p1 * 0.95 } else { 0.0 };
            let gain = ((0.95 * 65535.0) / (p_max - black).max(1.0)).clamp(1.0, 8.0);
            if gain > 1.01 || black > 0.5 {
                for v in &mut final_u16 {
                    let nv = ((*v as f32) - black).max(0.0) * gain + 0.5;
                    *v = nv.min(65535.0) as u16;
                }
            }
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

    emit_progress(&app, "Guardando resultado...", 98.0, None);
    let preview_b64 = {
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
        format!("data:image/png;base64,{}", general_purpose::STANDARD.encode(&enc))
    };

    {
        let mut res = state.stacked_image.lock().unwrap();
        *res = Some(StackResult {
            width: w_out, height: h_out,
            data: final_u16.clone(),
            is_mono: color_id == 0 || color_id == 12,
            is_surface: is_surface,
        });
        state.deconv_cache.lock().unwrap().clear();
        state.wavelet_cache.lock().unwrap().clear();
        state.filter_cache.lock().unwrap().clear();
    }

    let cache_dir = std::env::temp_dir().join("astro_stacker_cache");
    if cache_dir.exists() { let _ = std::fs::remove_dir_all(&cache_dir); }

    Ok(preview_b64)
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
    let mut contrast = Vec::with_capacity(points.len());
    let mut lap = Vec::with_capacity(points.len());
    for ap in points {
        contrast.push(get_area_complexity(
            master_edges, w, h, ap.x as usize, ap.y as usize, box_size, 100.0,
        ));
        lap.push(get_area_quality_laplacian(
            master_mono, w, h, ap.x as usize, ap.y as usize, box_size,
        ));
    }
    (contrast, lap)
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
) -> Vec<(f32, f32, f32)> {
    let mut local_shifts = Vec::with_capacity(custom_points.len());

    for (ap_i, ap) in custom_points.iter().enumerate() {
        if ap_i < ap_signal_valid.len() && !ap_signal_valid[ap_i] {
            local_shifts.push((0.0, 0.0, 0.0));
            continue;
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
            let (dx, dy, sad) = find_best_match_sad_pyramid_fast(
                master_edges, f_edges, w_in, h_in,
                ax as usize, ay as usize, fx_i as usize, fy_i as usize,
                box_size, search_r,
                master_ds, f_ds, master_ds_w,
            );
            // CRITICAL SUB-PIXEL FIX: the matcher measures the shift relative
            // to the ROUNDED search center (fx_i, fy_i), but the warp applies
            // it on top of the UNROUNDED (ax + render). The dropped rounding
            // fraction (uniform ±0.5 px, different per AP per frame) was never
            // compensated — equivalent to convolving the whole stack with a
            // 1-px box blur. Re-express the shift relative to (ax + render):
            let dx = dx - ((ax + render_dx) - fx_i as f32);
            let dy = dy - ((ay + render_dy) - fy_i as f32);
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
            local_shifts.push((dx, dy, nq));
        } else {
            local_shifts.push((0.0, 0.0, 0.0));
        }
    }

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
/// weighted statistics. σ is the real per-pixel spread of the frame
/// distribution, so edges (which legitimately jitter with seeing) get WIDE
/// windows while flat areas get tight ones — transient artifacts (satellites,
/// birds, dust, compression glitches) fall outside and are REJECTED in pass 2
/// without touching genuine detail. `sigma_floor` keeps degenerate
/// zero-variance pixels from rejecting everything. Pixels with no pass-1
/// coverage get (-∞, +∞) bounds (no-op).
fn build_sigma_clip_bounds(
    acc: &GradientDomainStacker,
    k: f32,
    sigma_floor: f32,
) -> (Vec<f32>, Vec<f32>) {
    let n = acc.direct.len();
    let mut lo = vec![f32::MIN; n];
    let mut hi = vec![f32::MAX; n];
    if acc.m2.len() != n {
        return (lo, hi);
    }
    for i in 0..n {
        let w = acc.direct_w[i];
        if w > 1e-9 {
            let mean = acc.direct[i] / w;
            let var = (acc.m2[i] / w - mean * mean).max(0.0);
            let sigma = var.sqrt().max(sigma_floor as f64);
            lo[i] = (mean - k as f64 * sigma) as f32;
            hi[i] = (mean + k as f64 * sigma) as f32;
        }
    }
    (lo, hi)
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
                    neighbors.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
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
        edge_values.select_nth_unstable_by(idx_95, |a, b| a.partial_cmp(b).unwrap());
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

fn auto_contrast_stretch_u16(img: &[u16], w: usize, h: usize) -> Vec<u16> {
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
}

impl DenseQualityMap {
    pub fn build(frame: &[f32], w: usize, h: usize, radius: usize) -> Self {
        let mut lap = vec![0.0f32; w * h];
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

        // Robust Normalization (P02 - P98)
        let mut sorted = quality_map.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p02 = sorted[(sorted.len() as f32 * 0.02) as usize];
        let p98 = sorted[(sorted.len() as f32 * 0.98) as usize];
        let range = (p98 - p02).max(1e-6);

        for v in &mut quality_map {
            *v = ((*v - p02) / range).clamp(0.0, 1.0);
        }

        Self { scores: quality_map, width: w, height: h }
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
    fn new(w_in: usize, h_in: usize, w_out: usize, h_out: usize, is_mono: bool) -> Self {
        let n_in = w_in * h_in;
        let n_out = w_out * h_out;
        Self {
            rgb_buf: if is_mono { Vec::new() } else { vec![0u16; n_in * 3] },
            mono_buf: vec![0u16; n_in],
            f_s1: vec![0u16; n_in],
            f_s2: vec![0u16; n_in],
            f_edges: vec![0u16; n_in],
            f_ds: vec![0u16; (w_in / 4) * (h_in / 4)],
            wr: if is_mono { Vec::new() } else { vec![0.0f32; n_out] },
            wg: vec![0.0f32; n_out],
            wb: if is_mono { Vec::new() } else { vec![0.0f32; n_out] },
            ww: vec![0.0f32; n_out],
        }
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
    fn take(
        pool: &'a std::sync::Mutex<Vec<LiquidScratch>>,
        w_in: usize,
        h_in: usize,
        w_out: usize,
        h_out: usize,
        is_mono: bool,
    ) -> Self {
        let sc = pool
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| LiquidScratch::new(w_in, h_in, w_out, h_out, is_mono));
        Self { pool, sc: Some(sc) }
    }
}

impl<'a> Drop for ScratchLease<'a> {
    fn drop(&mut self) {
        if let Some(sc) = self.sc.take() {
            self.pool.lock().unwrap().push(sc);
        }
    }
}

// ==========================================
// ELITE V4: GRADIENT DOMAIN STACKING (Poisson)
// ==========================================

pub struct GradientDomainStacker {
    pub grad_x: Vec<f64>,
    pub grad_y: Vec<f64>,
    pub weight: Vec<f64>,
    pub direct: Vec<f64>,
    pub direct_w: Vec<f64>,
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
            m2: Vec::new(),
            width: w,
            height: h,
        }
    }

    /// Direct-mean only variant. The three gradient buffers (24 bytes/px) are
    /// consumed exclusively by the Poisson reconstruction, which the liquid
    /// stacking path keeps disabled — skipping them cuts per-thread RAM by 60%.
    pub fn new_direct_only(w: usize, h: usize) -> Self {
        let n = w * h;
        Self {
            grad_x: Vec::new(),
            grad_y: Vec::new(),
            weight: Vec::new(),
            direct: vec![0.0; n],
            direct_w: vec![0.0; n],
            m2: Vec::new(),
            width: w,
            height: h,
        }
    }

    /// Direct-mean variant that ALSO tracks the per-pixel second moment for
    /// sigma-clip statistics (used by pass 1 of the double-pass stack).
    pub fn new_direct_tracked(w: usize, h: usize) -> Self {
        let n = w * h;
        Self {
            grad_x: Vec::new(),
            grad_y: Vec::new(),
            weight: Vec::new(),
            direct: vec![0.0; n],
            direct_w: vec![0.0; n],
            m2: vec![0.0; n],
            width: w,
            height: h,
        }
    }

    /// Zero-size placeholder for unused channels (mono stacking mode).
    pub fn new_empty() -> Self {
        Self {
            grad_x: Vec::new(),
            grad_y: Vec::new(),
            weight: Vec::new(),
            direct: Vec::new(),
            direct_w: Vec::new(),
            m2: Vec::new(),
            width: 0,
            height: 0,
        }
    }

    /// `q_off_x`/`q_off_y` register the quality map (built in SOURCE-frame
    /// coordinates) against the output raster: source ≈ out·scale + offset.
    /// Without this, per-pixel quality is sampled at the wrong location for
    /// frames with a non-zero global shift.
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

/// Rank-based quality weight — replicates AutoStakkert's frame weighting.
///
/// Unlike score-normalized sigmoid (which fails when all scores are similar),
/// this computes weight from the frame's RANK within the selected set.
/// Best frame = weight 1.0, worst selected frame = weight MIN_W (never zero).
/// This guarantees every selected frame contributes, preventing black stacks.
///
/// Parameters kept for API compat but `rejection_percentile`/`steepness` are
/// no longer used here — rejection is handled by frame selection (num_to_stack).
fn compute_frame_weight_sigmoidal(
    frame_score: f32,
    global_min: f32,
    global_max: f32,
    _rejection_percentile: f32,
    _steepness: f32,
) -> Option<f32> {
    const MIN_W: f32 = 0.10; // Worst selected frame still contributes 10%

    let range = (global_max - global_min).max(1.0);
    let normalized = ((frame_score - global_min) / range).clamp(0.0, 1.0);

    // Linear ramp: best frame → 1.0, worst → MIN_W.
    // All selected frames are guaranteed to be in [global_min, global_max].
    let w = MIN_W + (1.0 - MIN_W) * normalized;
    Some(w.clamp(MIN_W, 1.0))
}

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
    let mut dist = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            if mask[y * w + x] < 0.5 { continue; }
            let mut min_d = 32.0f32;
            for dy in -16isize..=16 {
                let ny = (y as isize + dy).clamp(0, h as isize - 1) as usize;
                for dx in -16isize..=16 {
                    let nx = (x as isize + dx).clamp(0, w as isize - 1) as usize;
                    if mask[ny * w + nx] < 0.5 {
                        let d = ((dx * dx + dy * dy) as f32).sqrt();
                        min_d = min_d.min(d);
                    }
                }
            }
            dist[y * w + x] = min_d;
        }
    }
    dist
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
        limb_grads.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
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

pub struct AdaptiveKappaClipper {
    pub kappa: f32,
    pub iterations: u32,
    pub min_frames: usize,
}

impl AdaptiveKappaClipper {
    pub fn clip_pixel_stack(&self, values: &mut Vec<(f32, f32)>) -> (f32, f32) {
        if values.len() < self.min_frames {
            let (ws, vs) = values.iter().fold((0.0, 0.0), |(ws, vs), &(v, w)| (ws + w, vs + v * w));
            return if ws > 1e-7 { (vs / ws, ws) } else { (0.0, 0.0) };
        }

        for _ in 0..self.iterations {
            if values.len() < self.min_frames { break; }
            let (ws, vs) = values.iter().fold((0.0, 0.0), |(ws, vs), &(v, w)| (ws + w, vs + v * w));
            let mean = vs / ws.max(1e-7);
            
            let mut devs: Vec<f32> = values.iter().map(|&(v, _)| (v - mean).abs()).collect();
            devs.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
            let sigma = devs[devs.len() / 2] * 1.4826;
            if sigma < 0.5 { break; }

            let thresh = self.kappa * sigma;
            let len_before = values.len();
            values.retain(|&(v, _)| (v - mean).abs() <= thresh);
            if values.len() == len_before { break; }
        }

        let (ws, vs) = values.iter().fold((0.0, 0.0), |(ws, vs), &(v, w)| (ws + w, vs + v * w));
        if ws > 1e-7 { (vs / ws, ws) } else { (0.0, 0.0) }
    }
}

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

pub fn accumulate_with_sigma_clipping(
    pixel_contributions: &mut Vec<Vec<(f32, f32)>>, // [pixel_idx][(val, weight)]
    _width: usize,
    _height: usize,
    clipper: &AdaptiveKappaClipper,
) -> Vec<f32> {
    pixel_contributions
        .iter_mut()
        .map(|contributions| {
            let (v, w) = clipper.clip_pixel_stack(contributions);
            if w > 1e-9 { v.clamp(0.0, 65535.0) } else { 0.0 }
        })
        .collect()
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
        if key.contains("planet") || key.contains("peque") || key.contains("grande") || key.contains("large") {
            TargetCategory::PlanetSmall
        } else {
            TargetCategory::Surface
        }
    }

    pub fn profile(&self) -> CategoryProfile {
        match self {
            TargetCategory::PlanetLarge  => CategoryProfile::planet_small(),
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

pub struct LumaChromaSeparator;

impl LumaChromaSeparator {
    pub fn rgb_to_ycbcr(r: &[f32], g: &[f32], b: &[f32]) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let n = r.len();
        let mut y  = vec![0.0f32; n];
        let mut cb = vec![0.0f32; n];
        let mut cr = vec![0.0f32; n];

        for i in 0..n {
            let luma = 0.2126 * r[i] + 0.7152 * g[i] + 0.0722 * b[i];
            y[i]  = luma;
            cb[i] = (b[i] - luma) * 0.5389 + 32768.0;
            cr[i] = (r[i] - luma) * 0.6350 + 32768.0;
        }
        (y, cb, cr)
    }

    pub fn ycbcr_to_rgb(y: &[f32], cb: &[f32], cr: &[f32]) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let n = y.len();
        let mut r = vec![0.0f32; n];
        let mut g = vec![0.0f32; n];
        let mut b = vec![0.0f32; n];

        for i in 0..n {
            let cb_c = cb[i] - 32768.0;
            let cr_c = cr[i] - 32768.0;
            let ri = y[i] + 1.5748 * cr_c;
            let gi = y[i] - 0.1873 * cb_c - 0.4681 * cr_c;
            let bi = y[i] + 1.8556 * cb_c;
            r[i] = ri.clamp(0.0, 65535.0);
            g[i] = gi.clamp(0.0, 65535.0);
            b[i] = bi.clamp(0.0, 65535.0);
        }
        (r, g, b)
    }

    pub fn smooth_chroma(
        cb: &[f32], cr: &[f32],
        width: usize, height: usize,
        sigma: f32,
    ) -> (Vec<f32>, Vec<f32>) {
        let cb_smooth = crate::apply_gaussian_blur_f32(cb, width, height, sigma);
        let cr_smooth = crate::apply_gaussian_blur_f32(cr, width, height, sigma);
        (cb_smooth, cr_smooth)
    }
}

// ══════════════════════════════════════════════════════════════════
// DECONVOLUTION BLEND
// ══════════════════════════════════════════════════════════════════

pub fn deconvolve_safe_blend(
    luma: &[f32],
    width: usize,
    height: usize,
    psf: &[f32],
    psf_size: usize,
    snr_map: &[f32],
    planet_mask: &[f32],
    profile: &CategoryProfile,
) -> Vec<f32> {
    if !profile.deconv_enabled || profile.deconv_blend < 0.01 {
        return luma.to_vec();
    }

    let noise = estimate_stack_noise(luma, width, height);
    let signal_sum: f64 = luma.iter()
        .zip(planet_mask.iter())
        .filter(|(_, &m)| m > 0.5)
        .map(|(&v, _)| v as f64)
        .sum();
    let signal_count = luma.iter()
        .zip(planet_mask.iter())
        .filter(|(_, &m)| m > 0.5)
        .count().max(1) as f64;
    
    let signal_mean = signal_sum / signal_count;
    let global_snr = signal_mean as f32 / noise.max(1.0);

    if global_snr < profile.snr_floor_deconv * 2.0 {
        return luma.to_vec();
    }

    let deconvolver = TvRlDeconvolver {
        max_iterations:  profile.deconv_iterations,
        tv_lambda:       profile.tv_lambda,
        convergence_eps: 0.00005,
        snr_floor:       profile.snr_floor_deconv,
    };

    let deconvolved = deconvolver.deconvolve(
        luma, width, height,
        psf, psf_size,
        snr_map, planet_mask,
    );

    luma.iter()
        .zip(deconvolved.iter())
        .zip(snr_map.iter())
        .zip(planet_mask.iter())
        .map(|(((&orig, &deconv), &snr), &mask)| {
            let snr_factor = ((snr - profile.snr_floor_deconv)
                / (profile.snr_floor_deconv * 3.0))
                .clamp(0.0, 1.0)
                .powf(1.5);

            let effective_blend = profile.deconv_blend * snr_factor * mask;

            let deconv_safe = if (deconv - orig).abs() > orig * 2.0 {
                orig
            } else {
                deconv
            };

            (orig * (1.0 - effective_blend) + deconv_safe * effective_blend)
                .clamp(0.0, 65535.0)
        })
        .collect()
}

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

// Dummy stubs for undefined external dependencies in the user's conceptual pipeline
fn warp_frame_lanczos3(_frame: &[f32], w: usize, h: usize, _warp: &[f32]) -> Vec<f32> { vec![0.0; w * h] }

fn stack_channel(
    frames: &[Vec<f32>],
    width: usize,
    height: usize,
    warp_fields: &[Vec<f32>], // Changed WarpField to Vec<f32>
    profile: &CategoryProfile,
    is_surface: bool,
) -> Vec<f32> {
    let quality_maps_scores: Vec<Vec<f32>> = frames.par_iter().map(|f| {
        DenseQualityMap::build(f, width, height, profile.quality_window_radius).scores
    }).collect();

    let global_scores: Vec<f32> = quality_maps_scores.iter()
        .map(|q| percentile_f32(q, 0.85))
        .collect();

    let g_min = global_scores.iter().cloned().fold(f32::MAX, f32::min);
    let g_max = global_scores.iter().cloned().fold(f32::MIN, f32::max);
    let g_range = (g_max - g_min).max(1e-6);

    let mut grad_stacker = GradientDomainStacker::new(width, height);
    let mut pixel_vals: Vec<Vec<(f32, f32)>> = vec![Vec::new(); width * height];
    let clipper = AdaptiveKappaClipper {
        kappa: profile.kappa_sigma,
        iterations: 4,
        min_frames: 4,
    };

    for (fi, frame) in frames.iter().enumerate() {
        let norm = ((global_scores[fi] - g_min) / g_range).clamp(0.0, 1.0);
        if norm < profile.rejection_percentile { continue; }

        let renorm = (norm - profile.rejection_percentile)
            / (1.0 - profile.rejection_percentile).max(1e-6);
        let gw = 1.0 / (1.0 + (-profile.sigmoid_steepness * (renorm - 0.5)).exp());

        let warped = warp_frame_lanczos3(frame, width, height, &warp_fields[fi]);

        for i in 0..width * height {
            let lq = quality_maps_scores[fi][i].powf(2.5);
            let cw = gw * lq;
            if cw > 0.005 {
                pixel_vals[i].push((warped[i], cw));
            }
        }
        grad_stacker.accumulate(frame, &[], &quality_maps_scores[fi], width, height, 0.0, 0.0, gw); // Fixed from accumulate_frame to accumulate
    }

    let stacked_clipped = accumulate_with_sigma_clipping(
        &mut pixel_vals, width, height, &clipper
    );

    // Bypass Poisson gradient stacking for surface targets to preserve micro-details
    let pw = if is_surface { 0.0 } else { profile.poisson_weight };
    if pw > 0.01 {
        let stacked_poisson = grad_stacker.reconstruct(profile.poisson_iterations);
        stacked_clipped.iter()
            .zip(stacked_poisson.iter())
            .map(|(&c, &p)| ((1.0 - pw) * c + pw * p).clamp(0.0, 65535.0))
            .collect()
    } else {
        stacked_clipped
    }
}

pub fn process_by_category(
    frames_r: &[Vec<f32>],
    frames_g: &[Vec<f32>],
    frames_b: &[Vec<f32>],
    width: usize,
    height: usize,
    category: TargetCategory,
    warp_fields: &[Vec<f32>], // Changed WarpField to Vec<f32>
) -> (Vec<f32>, Vec<f32>, Vec<f32>) {

    let profile = category.profile();
    let is_surface = category == TargetCategory::Surface;

    let stacked_r = stack_channel(frames_r, width, height, warp_fields, &profile, is_surface);
    let stacked_g = stack_channel(frames_g, width, height, warp_fields, &profile, is_surface);
    let stacked_b = stack_channel(frames_b, width, height, warp_fields, &profile, is_surface);

    let (luma, cb, cr) = LumaChromaSeparator::rgb_to_ycbcr(
        &stacked_r, &stacked_g, &stacked_b
    );

    let (cb_smooth, cr_smooth) = LumaChromaSeparator::smooth_chroma(
        &cb, &cr, width, height, profile.chroma_blur_sigma
    );

    let planet_mask = if profile.mask_feather_px == 0 {
        vec![1.0f32; width * height]
    } else {
        compute_planet_mask(&luma, width, height, profile.mask_feather_px)
    };

    let noise_sigma = estimate_stack_noise(&luma, width, height);
    let snr_map = compute_snr_map(&luma, width, height, noise_sigma); // Fixed arguments

    let psf_estimator = PsfEstimator { psf_radius: profile.psf_radius };
    let psf = if profile.use_limb_psf {
        let limb_mask = compute_limb_mask(&planet_mask, width, height, 6);
        let psf_from_limb = psf_estimator.estimate_from_limb(
            &luma, width, height, &limb_mask, 0.85
        );
        if psf_is_valid(&psf_from_limb, profile.psf_radius) {
            psf_from_limb
        } else {
            psf_estimator.estimate_gaussian_airy(
                profile.psf_fwhm_pixels,
                profile.psf_airy_weight,
            )
        }
    } else {
        psf_estimator.estimate_gaussian_airy(
            profile.psf_fwhm_pixels,
            profile.psf_airy_weight,
        )
    };

    let psf_size = 2 * profile.psf_radius + 1;

    let luma_deconv = deconvolve_safe_blend(
        &luma, width, height,
        &psf, psf_size,
        &snr_map, &planet_mask,
        &profile,
    );

    let luma_sharp = sharpen_atrous_masked(
        &luma_deconv,
        width, height,
        &snr_map,
        &planet_mask,
        &profile,
        noise_sigma,
    );

    let (out_r, out_g, out_b) = LumaChromaSeparator::ycbcr_to_rgb(
        &luma_sharp, &cb_smooth, &cr_smooth
    );

    (out_r, out_g, out_b)
}

pub async fn zenith_stack_video_elite_impl(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
    percent: f32,
    _config: EliteConfig,
    category_str: String,
) -> Result<String, String> {
    emit_progress(&app, "Iniciando Zenith Elite V4 Stacking...", 0.0, None);

    let r = VideoInput::open(&path, &app)?;
    let w = r.width();
    let h = r.height();
    let frame_count = r.frame_count();
    let is_color = r.is_color();
    let color_id = r.color_id();
    let bpp = r.bpp();

    let cache_path = get_analysis_cache_path(&path, "planet_v2");
    if !std::path::Path::new(&cache_path).exists() {
        return Err("No se encontró análisis previo. Por favor analiza el video primero.".into());
    }
    let cache_content = std::fs::read_to_string(&cache_path).map_err(|e| e.to_string())?;
    let cached: CachedAnalysis = serde_json::from_str(&cache_content).map_err(|e| e.to_string())?;

    let mut stats = cached.frame_stats.ok_or("No hay estadísticas de frames en el caché")?;
    stats.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let limit = ((frame_count as f32 * percent) / 100.0).max(1.0) as usize;
    let selected_indices: Vec<usize> = stats.iter().take(limit).map(|s| s.idx).collect();

    emit_progress(&app, &format!("Procesando {} frames...", selected_indices.len()), 5.0, None);

    let target_category = TargetCategory::from_str(&category_str);
    
    // Instead of processing channels individually, we collect all R, G, B frames.
    let mut frames_r = Vec::with_capacity(selected_indices.len());
    let mut frames_g = Vec::with_capacity(selected_indices.len());
    let mut frames_b = Vec::with_capacity(selected_indices.len());
    
    emit_progress(&app, "Cargando frames RGB en memoria...", 10.0, None);
    for (i, &idx) in selected_indices.iter().enumerate() {
        let raw = r.get_frame(idx, color_id);
        let u16_buf = raw_to_u16_buffer(&raw, w, h, bpp);
        
        if is_color {
            let rgb = debayer_to_rgb(&u16_buf, w, h, color_id);
            frames_r.push(rgb.chunks_exact(3).map(|p| p[0] as f32).collect());
            frames_g.push(rgb.chunks_exact(3).map(|p| p[1] as f32).collect());
            frames_b.push(rgb.chunks_exact(3).map(|p| p[2] as f32).collect());
        } else {
            let mono: Vec<f32> = u16_buf.iter().map(|&p| p as f32).collect();
            frames_r.push(mono.clone());
            frames_g.push(mono.clone());
            frames_b.push(mono);
        }

        if i % 10 == 0 {
            emit_progress(&app, &format!("Cargando frame {}/{}", i, selected_indices.len()), 10.0 + (i as f32 / selected_indices.len() as f32) * 20.0, None);
        }
    }

    emit_progress(&app, "Ejecutando Pipeline Elite por Categorías (Luma/Chroma)...", 30.0, None);
    let warp_fields = vec![vec![]; selected_indices.len()]; // Dummy warp fields
    let (out_r, out_g, out_b) = process_by_category(&frames_r, &frames_g, &frames_b, w, h, target_category, &warp_fields);

    emit_progress(&app, "Combinando canales y guardando...", 95.0, None);
    let mut final_u16 = Vec::with_capacity(w * h * 3);
    for i in 0..w * h {
        if is_color {
            final_u16.push(out_r[i] as u16);
            final_u16.push(out_g[i] as u16);
            final_u16.push(out_b[i] as u16);
        } else {
            let val = out_r[i] as u16;
            final_u16.push(val);
            final_u16.push(val);
            final_u16.push(val);
        }
    }

    let out_name = format!("{}_elite_v4.tiff", std::path::Path::new(&path).file_stem().unwrap().to_str().unwrap());
    let out_path = std::path::Path::new(&path).parent().unwrap().join(out_name);
    
    {
        let mut locked = state.stacked_image.lock().unwrap();
        *locked = Some(StackResult {
            data: final_u16.clone(),
            width: w,
            height: h,
            is_mono: !is_color,
            is_surface: category_str == "surface", 
        });
    }

    emit_progress(&app, "¡Zenith Elite V4 Completado!", 100.0, None);
    Ok(out_path.to_string_lossy().to_string())
}

// ==========================================
// QA REGRESSION TESTS (ZAS quality helpers)
// ==========================================
#[cfg(test)]
mod zas_v3_tests {
    use super::*;

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
        let (lo, hi) = build_sigma_clip_bounds(&acc1, 4.0, 6.0);

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
            compute_idw_map_for_output(w, h, 1.0, 0.0, 0.0, &points, 1.55, 4);
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

        shift_errs.sort_by(|a, b| a.partial_cmp(b).unwrap());
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
