#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

// ==========================================
// ENHANCEMENT FOR ALIGNMENT
// ==========================================

/// Default high-pass amplification for alignment maps. Planetary targets keep
/// the historical 6×; low-contrast surface texture (solar granulation) uses a
/// gentler amount via `enhance_for_alignment_with_amount` — over-amplification
/// raises the noise floor of the SAD surface and degrades sub-pixel fits.
pub const ALIGN_ENHANCE_AMOUNT_DEFAULT: f32 = 6.0;

pub fn enhance_for_alignment(input: &[u16], width: usize, height: usize) -> Vec<u16> {
    enhance_for_alignment_with_amount(input, width, height, ALIGN_ENHANCE_AMOUNT_DEFAULT)
}

pub fn enhance_for_alignment_with_amount(
    input: &[u16],
    width: usize,
    height: usize,
    amount: f32,
) -> Vec<u16> {
    let len = width * height;
    if len == 0 {
        return Vec::new();
    }
    let mut scratch1 = vec![0u16; len];
    let mut scratch2 = vec![0u16; len];
    let mut out = vec![0u16; len];
    enhance_for_alignment_into_amount(
        input,
        width,
        height,
        &mut scratch1,
        &mut scratch2,
        &mut out,
        amount,
    );
    out
}

pub fn enhance_for_alignment_into(
    input: &[u16],
    width: usize,
    height: usize,
    scratch1: &mut Vec<u16>,
    scratch2: &mut Vec<u16>,
    out: &mut Vec<u16>,
) {
    enhance_for_alignment_into_amount(
        input,
        width,
        height,
        scratch1,
        scratch2,
        out,
        ALIGN_ENHANCE_AMOUNT_DEFAULT,
    );
}

pub fn enhance_for_alignment_into_amount(
    input: &[u16],
    width: usize,
    height: usize,
    scratch1: &mut Vec<u16>,
    scratch2: &mut Vec<u16>,
    out: &mut Vec<u16>,
    amount: f32,
) {
    let len = width * height;
    if len == 0 {
        return;
    }

    if scratch1.len() < len {
        scratch1.resize(len, 0);
    }
    if scratch2.len() < len {
        scratch2.resize(len, 0);
    }
    if out.len() < len {
        out.resize(len, 0);
    }

    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe {
                enhance_for_alignment_avx2(input, width, height, scratch1, scratch2, out, amount);
            }
            return;
        }
    }

    enhance_for_alignment_scalar(input, width, height, scratch1, scratch2, out, amount);
}

/// Median of small samples taken from the FOUR corners of the image.
/// Sampling only the first rows biases the noise floor whenever the target
/// touches the top edge of the frame (common in solar/lunar surface videos).
fn estimate_noise_floor_corners(input: &[u16], width: usize, height: usize) -> i32 {
    let block = 12usize.min(width.max(1)).min(height.max(1));
    let xs = [0usize, width.saturating_sub(block)];
    let ys = [0usize, height.saturating_sub(block)];
    let mut sample = Vec::with_capacity(4 * block * block / 2);
    for &by in &ys {
        for &bx in &xs {
            for y in by..(by + block).min(height) {
                let row = y * width;
                for x in (bx..(bx + block).min(width)).step_by(2) {
                    sample.push(input[row + x]);
                }
            }
        }
    }
    if sample.is_empty() {
        return 0;
    }
    sample.sort_unstable();
    sample[sample.len() / 2] as i32
}

fn enhance_for_alignment_scalar(
    input: &[u16],
    width: usize,
    height: usize,
    scratch1: &mut Vec<u16>,
    scratch2: &mut Vec<u16>,
    out: &mut Vec<u16>,
    amount: f32,
) {
    let len = width * height;

    // 1. Noise Floor Estimation (4-corner median)
    let noise_floor = estimate_noise_floor_corners(input, width, height);

    // 2. Skip Bayer Killer (2x2 Box Blur)
    scratch1.copy_from_slice(input);

    // 3. Fast Box Blur (Radius 1) - Low Pass
    for y in 0..height {
        let row_off = y * width;
        for x in 0..width {
            let mut sum: u32 = 0;
            let start = x.saturating_sub(1);
            let end = (x + 2).min(width);
            let count = end - start;
            for ix in start..end {
                sum += scratch1[row_off + ix] as u32;
            }
            scratch2[row_off + x] = (sum / count as u32) as u16;
        }
    }

    for y in 0..height {
        let y_start = y.saturating_sub(1);
        let y_end = (y + 2).min(height);
        let count = y_end - y_start;
        let row_off = y * width;
        for x in 0..width {
            let mut sum: u32 = 0;
            for iy in y_start..y_end {
                sum += scratch2[iy * width + x] as u32;
            }
            out[row_off + x] = (sum / count as u32) as u16;
        }
    }

    // 4. Noise-Aware High-Pass (Detail Lock)
    // We only amplify details that are significantly above the noise floor.
    let noise_gate = noise_floor + 200;

    for i in 0..len {
        let v = scratch1[i] as i32;
        let b = out[i] as i32; // Blur
        let diff = v - b;

        // Signal-to-Noise Weighting: dampens sharpening in dark/noisy areas
        let snr_weight = if v > noise_gate {
            1.0f32
        } else {
            ((v - noise_floor as i32).max(0) as f32 / 200.0).powi(2)
        };

        let enhanced = v as f32 + (diff as f32 * amount * snr_weight);
        out[i] = enhanced.clamp(0.0, 65535.0) as u16;
    }
}

/// Normalizes a patch to have consistent mean and standard deviation.
/// Compensates for atmospheric scintillation (temporal brightness fluctuations).
pub fn normalize_patch_stats(
    patch: &mut [u16],
    target_mean: f32,
    target_std: f32,
    target_type: &str,
) {
    if patch.is_empty() {
        return;
    }

    let n = patch.len() as f32;
    let mut sum = 0.0f32;
    let mut sum_sq = 0.0f32;

    for &v in patch.iter() {
        let vf = v as f32;
        sum += vf;
        sum_sq += vf * vf;
    }

    let mean = sum / n;
    let var = (sum_sq / n) - (mean * mean);
    let std = var.max(0.1).sqrt();

    let mut gain = target_std / std;

    // --- SMART NOISE SUPPRESSION BY TARGET CATEGORY ---
    // For small planets (black backgrounds), pure statistical normalization
    // can amplify noise to infinite if the patch is mostly black.
    let t_low = target_type.to_lowercase();
    if t_low.contains("pequeño") || t_low.contains("small") {
        // Very strict: Mars, Uranus, Neptune
        gain = gain.clamp(0.2, 1.2);
    } else if t_low.contains("grande") || t_low.contains("large") {
        // Moderate: Jupiter, Saturn
        gain = gain.clamp(0.1, 2.5);
    } else {
        // Surface: Lunar/Solar or Unknown. Higher tolerance for scintillation.
        gain = gain.clamp(0.01, 8.0);
    }

    for v in patch.iter_mut() {
        let vf = *v as f32;
        *v = ((vf - mean) * gain + target_mean).clamp(0.0, 65535.0) as u16;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn enhance_for_alignment_avx2(
    input: &[u16],
    width: usize,
    height: usize,
    scratch1: &mut Vec<u16>,
    scratch2: &mut Vec<u16>,
    out: &mut Vec<u16>,
    amount: f32,
) {
    let len = width * height;

    // 1. Noise Floor Estimation (matching scalar version: 4-corner median)
    let noise_floor = estimate_noise_floor_corners(input, width, height);

    // 2. Copy input to scratch1
    unsafe {
        std::ptr::copy_nonoverlapping(input.as_ptr(), scratch1.as_mut_ptr(), len);
    }

    // 3. Horizontal Blur
    for y in 0..height {
        let row_off = y * width;
        for cx in 0..width {
            let mut sum: u32 = 0;
            let start = cx.saturating_sub(1);
            let end = (cx + 2).min(width);
            let count = end - start;
            for ix in start..end {
                sum += *scratch1.get_unchecked(row_off + ix) as u32;
            }
            *scratch2.get_unchecked_mut(row_off + cx) = (sum / count as u32) as u16;
        }
    }

    // 4. Vertical Blur & Noise-Aware High Pass (matching scalar version)
    let noise_gate = noise_floor + 200;
    for y in 0..height {
        let y_start = y.saturating_sub(1);
        let y_end = (y + 2).min(height);
        let count = y_end - y_start;
        let row_off = y * width;
        for x in 0..width {
            let mut sum: u32 = 0;
            for iy in y_start..y_end {
                sum += *scratch2.get_unchecked(iy * width + x) as u32;
            }
            let blurred = (sum / count as u32) as i32;
            let v = *scratch1.get_unchecked(row_off + x) as i32;
            let diff = v - blurred;

            // Signal-to-Noise Weighting: dampens sharpening in dark/noisy areas
            let snr_weight = if v > noise_gate {
                1.0f32
            } else {
                ((v - noise_floor).max(0) as f32 / 200.0).powi(2)
            };

            let enhanced = v as f32 + (diff as f32 * amount * snr_weight);
            *out.get_unchecked_mut(row_off + x) = enhanced.clamp(0.0, 65535.0) as u16;
        }
    }
}

// ==========================================
// PYRAMIDAL ALIGNMENT (RECT-BASED)
// ==========================================

pub fn downscale_integer(img: &[u16], w: usize, h: usize, factor: usize) -> Vec<u16> {
    if factor == 0 {
        return img.to_vec();
    }
    let new_w = w / factor;
    let new_h = h / factor;
    let mut out = Vec::with_capacity(new_w * new_h);

    for y in 0..new_h {
        let y_off = y * factor * w;
        for x in 0..new_w {
            let x_off = x * factor;
            let mut sum: u32 = 0;
            let area = (factor * factor) as u32;

            for dy in 0..factor {
                let row_idx = y_off + dy * w;
                for dx in 0..factor {
                    let px_idx = row_idx + x_off + dx;
                    if px_idx < img.len() {
                        sum += unsafe { *img.get_unchecked(px_idx) } as u32;
                    }
                }
            }
            out.push((sum / area) as u16);
        }
    }
    out
}

pub fn find_best_match_sad_pyramid(
    ref_full: &[u16],
    tgt_full: &[u16],
    ref_small: &[u16],
    width: usize,
    height: usize,
    small_w: usize,
    small_h: usize,
    roi_x: usize,
    roi_y: usize,
    roi_w: usize,
    roi_h: usize,
    search_range: isize,
    scale_factor: usize,
    fine_search_range: isize,
) -> (f32, f32) {
    let tgt_small = downscale_integer(tgt_full, width, height, scale_factor);
    let factor = scale_factor.max(1) as isize;
    let coarse_range = (search_range / factor).max(2);
    let coarse_roi_x = roi_x / scale_factor;
    let coarse_roi_y = roi_y / scale_factor;
    let coarse_roi_w = roi_w / scale_factor;
    let coarse_roi_h = roi_h / scale_factor;

    let (_best_sad, best_dx, best_dy) = find_best_match_sad_rect(
        ref_small,
        &tgt_small,
        small_w,
        small_h,
        coarse_roi_x,
        coarse_roi_y,
        coarse_roi_w,
        coarse_roi_h,
        0,
        0,
        coarse_range,
    );

    let guess_dx = best_dx * factor;
    let guess_dy = best_dy * factor;

    let (final_dx, final_dy) = find_best_match_sad_subpixel(
        ref_full,
        tgt_full,
        width,
        height,
        roi_x,
        roi_y,
        roi_w,
        roi_h,
        guess_dx,
        guess_dy,
        fine_search_range,
    );

    (final_dx, final_dy)
}

/// Pyramid SAD with an INITIAL COARSE OFFSET (`init_dx/init_dy`, full-res px).
/// The coarse search is centered at the offset instead of zero, so the
/// effective motion range becomes unbounded when a prior estimate exists —
/// e.g. the brightness-centroid delta of a disc on black sky (handheld phone
/// videos, whose frame-to-frame motion far exceeds any fixed search window).
/// With (0, 0) it behaves exactly like `find_best_match_sad_pyramid`.
#[allow(clippy::too_many_arguments)]
pub fn find_best_match_sad_pyramid_offset(
    ref_full: &[u16],
    tgt_full: &[u16],
    ref_small: &[u16],
    width: usize,
    height: usize,
    small_w: usize,
    small_h: usize,
    roi_x: usize,
    roi_y: usize,
    roi_w: usize,
    roi_h: usize,
    search_range: isize,
    scale_factor: usize,
    fine_search_range: isize,
    init_dx: isize,
    init_dy: isize,
) -> (f32, f32) {
    let tgt_small = downscale_integer(tgt_full, width, height, scale_factor);
    let factor = scale_factor.max(1) as isize;
    let coarse_range = (search_range / factor).max(2);

    let (_best_sad, best_dx, best_dy) = find_best_match_sad_rect(
        ref_small,
        &tgt_small,
        small_w,
        small_h,
        roi_x / scale_factor,
        roi_y / scale_factor,
        roi_w / scale_factor,
        roi_h / scale_factor,
        init_dx / factor,
        init_dy / factor,
        coarse_range,
    );

    let (final_dx, final_dy) = find_best_match_sad_subpixel(
        ref_full,
        tgt_full,
        width,
        height,
        roi_x,
        roi_y,
        roi_w,
        roi_h,
        best_dx * factor,
        best_dy * factor,
        fine_search_range,
    );

    (final_dx, final_dy)
}

fn find_best_match_sad_rect(
    ref_data: &[u16],
    tgt_data: &[u16],
    width: usize,
    height: usize,
    roi_x: usize,
    roi_y: usize,
    roi_w: usize,
    roi_h: usize,
    start_dx: isize,
    start_dy: isize,
    search_range: isize,
) -> (u64, isize, isize) {
    // COTA INICIAL: SAD completo del candidato central (start_dx, start_dy).
    // El barrido row-major construía best_sad LENTO desde la esquina y los
    // primeros cientos de candidatos se evaluaban casi enteros. El ganador
    // suele estar en (o junto a) la semilla: evaluarla primero como COTA hace
    // que casi todos los demás aborten en pocas filas. EQUIVALENCIA: la cota
    // acota por arriba al mínimo global M; un candidato abortado tiene
    // SAD > min(best, cota) ≥ M y con comparación estricta '<' jamás gana ni
    // empata — argmin y SAD devueltos bit-idénticos. Coste: el central se
    // evalúa dos veces (despreciable frente al ahorro).
    let abort_bound = if search_range > 0 {
        find_best_match_sad_rect_bounded(
            ref_data, tgt_data, width, height, roi_x, roi_y, roi_w, roi_h, start_dx, start_dy,
            0, u64::MAX,
        )
        .0
    } else {
        u64::MAX
    };
    find_best_match_sad_rect_bounded(
        ref_data, tgt_data, width, height, roi_x, roi_y, roi_w, roi_h, start_dx, start_dy,
        search_range, abort_bound,
    )
}

#[allow(clippy::too_many_arguments)]
fn find_best_match_sad_rect_bounded(
    ref_data: &[u16],
    tgt_data: &[u16],
    width: usize,
    height: usize,
    roi_x: usize,
    roi_y: usize,
    roi_w: usize,
    roi_h: usize,
    start_dx: isize,
    start_dy: isize,
    search_range: isize,
    abort_bound: u64,
) -> (u64, isize, isize) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe {
                return find_best_match_sad_rect_avx2(
                    ref_data,
                    tgt_data,
                    width,
                    height,
                    roi_x,
                    roi_y,
                    roi_w,
                    roi_h,
                    start_dx,
                    start_dy,
                    search_range,
                    abort_bound,
                );
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        unsafe {
            return find_best_match_sad_rect_neon(
                ref_data,
                tgt_data,
                width,
                height,
                roi_x,
                roi_y,
                roi_w,
                roi_h,
                start_dx,
                start_dy,
                search_range,
                abort_bound,
            );
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    find_best_match_sad_internal(
        ref_data,
        tgt_data,
        width,
        height,
        roi_x,
        roi_y,
        roi_w,
        roi_h,
        start_dx,
        start_dy,
        search_range,
        abort_bound,
    )
}

#[allow(dead_code)]
fn find_best_match_sad_internal(
    ref_data: &[u16],
    tgt_data: &[u16],
    width: usize,
    height: usize,
    roi_x: usize,
    roi_y: usize,
    roi_w: usize,
    roi_h: usize,
    start_dx: isize,
    start_dy: isize,
    search_range: isize,
    abort_bound: u64,
) -> (u64, isize, isize) {
    let mut best_sad = u64::MAX;
    let mut best_dx = start_dx;
    let mut best_dy = start_dy;

    let min_dx = start_dx - search_range;
    let max_dx = start_dx + search_range;
    let min_dy = start_dy - search_range;
    let max_dy = start_dy + search_range;

    for dy in min_dy..=max_dy {
        for dx in min_dx..=max_dx {
            let mut current_sad: u64 = 0;
            let mut valid = true;
            for ry in 0..roi_h {
                let ref_y = roi_y + ry;
                let tgt_y = ref_y as isize + dy;
                if tgt_y < 0 || tgt_y >= height as isize {
                    valid = false;
                    break;
                }
                unsafe {
                    let p_ref = ref_data.as_ptr().add(ref_y * width);
                    let p_tgt = tgt_data.as_ptr().add(tgt_y as usize * width);
                    for rx in 0..roi_w {
                        let ref_x = roi_x + rx;
                        let tgt_x = ref_x as isize + dx;
                        if tgt_x < 0 || tgt_x >= width as isize {
                            valid = false;
                            break;
                        }
                        let v_ref = *p_ref.add(ref_x);
                        let v_tgt = *p_tgt.add(tgt_x as usize);
                        let diff = (v_ref as i32 - v_tgt as i32).abs();
                        current_sad += diff as u64;
                    }
                }
                if !valid || current_sad > best_sad.min(abort_bound) {
                    break;
                }
            }
            if valid && current_sad < best_sad {
                best_sad = current_sad;
                best_dx = dx;
                best_dy = dy;
            }
        }
    }
    (best_sad, best_dx, best_dy)
}

fn find_best_match_sad_subpixel(
    ref_data: &[u16],
    tgt_data: &[u16],
    width: usize,
    height: usize,
    roi_x: usize,
    roi_y: usize,
    roi_w: usize,
    roi_h: usize,
    start_dx: isize,
    start_dy: isize,
    search_range: isize,
) -> (f32, f32) {
    let (fsad, fidx, fidy) = find_best_match_sad_rect(
        ref_data,
        tgt_data,
        width,
        height,
        roi_x,
        roi_y,
        roi_w,
        roi_h,
        start_dx,
        start_dy,
        search_range,
    );

    // Apply sub-pixel logic identical to the AP tracking
    let target_idx_x = (roi_x as isize + fidx).max(0) as usize;
    let target_idx_y = (roi_y as isize + fidy).max(0) as usize;

    let (sdx, sdy) = subpixel_refine_sad(
        ref_data,
        tgt_data,
        width,
        roi_x,
        roi_y,
        target_idx_x,
        target_idx_y,
        roi_w.min(roi_h),
        fidx as f32,
        fidy as f32,
        fsad,
    );

    (fidx as f32 + sdx, fidy as f32 + sdy)
}

/// Refinamiento CPU de una semilla entera obtenida por otro motor (por
/// ejemplo el SAD batched de wgpu). Mantiene exactamente la búsqueda SIMD y
/// el ajuste subpíxel de la ruta de referencia, pero evita repetir la búsqueda
/// gruesa en CPU.
#[allow(clippy::too_many_arguments)]
pub fn refine_best_match_sad_offset(
    ref_data: &[u16],
    tgt_data: &[u16],
    width: usize,
    height: usize,
    roi_x: usize,
    roi_y: usize,
    roi_w: usize,
    roi_h: usize,
    guess_dx: isize,
    guess_dy: isize,
    fine_search_range: isize,
) -> (f32, f32) {
    find_best_match_sad_subpixel(
        ref_data,
        tgt_data,
        width,
        height,
        roi_x,
        roi_y,
        roi_w,
        roi_h,
        guess_dx,
        guess_dy,
        fine_search_range,
    )
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn find_best_match_sad_rect_avx2(
    ref_data: &[u16],
    tgt_data: &[u16],
    width: usize,
    height: usize,
    roi_x: usize,
    roi_y: usize,
    roi_w: usize,
    roi_h: usize,
    start_dx: isize,
    start_dy: isize,
    search_range: isize,
    abort_bound: u64,
) -> (u64, isize, isize) {
    let mut best_sad = u64::MAX;
    let mut best_dx = start_dx;
    let mut best_dy = start_dy;

    let min_dx = start_dx - search_range;
    let max_dx = start_dx + search_range;
    let min_dy = start_dy - search_range;
    let max_dy = start_dy + search_range;

    let v_zero = _mm256_setzero_si256();

    for dy in min_dy..=max_dy {
        for dx in min_dx..=max_dx {
            let t_x = roi_x as isize + dx;
            let t_y = roi_y as isize + dy;

            if t_x < 0
                || t_y < 0
                || (t_x + roi_w as isize) > width as isize
                || (t_y + roi_h as isize) > height as isize
            {
                continue;
            }

            let mut running = 0u64;
            let mut aborted = false;

            let row_r_base = roi_y * width + roi_x;
            let row_t_base = (t_y as usize) * width + (t_x as usize);

            let p_ref_start = ref_data.as_ptr().add(row_r_base);
            let p_tgt_start = tgt_data.as_ptr().add(row_t_base);

            for i in 0..roi_h {
                let r_ptr = p_ref_start.add(i * width);
                let t_ptr = p_tgt_start.add(i * width);
                let mut rx = 0;

                let mut vec_acc = _mm256_setzero_si256();
                while rx + 16 <= roi_w {
                    let va = _mm256_loadu_si256(r_ptr.add(rx) as *const _);
                    let vb = _mm256_loadu_si256(t_ptr.add(rx) as *const _);
                    let vmin = _mm256_min_epu16(va, vb);
                    let vmax = _mm256_max_epu16(va, vb);
                    let diff = _mm256_sub_epi16(vmax, vmin);
                    let lo = _mm256_unpacklo_epi16(diff, v_zero);
                    let hi = _mm256_unpackhi_epi16(diff, v_zero);
                    vec_acc = _mm256_add_epi32(vec_acc, lo);
                    vec_acc = _mm256_add_epi32(vec_acc, hi);
                    rx += 16;
                }

                // Reducción por fila (sustituye al flush a 64-bit: las lanes
                // u32 no desbordan dentro de una fila).
                let mut lanes = [0u32; 8];
                _mm256_storeu_si256(lanes.as_mut_ptr() as *mut _, vec_acc);
                let mut row_sad: u64 = lanes.iter().map(|&v| v as u64).sum();

                while rx < roi_w {
                    let val_r = *r_ptr.add(rx) as i32;
                    let val_t = *t_ptr.add(rx) as i32;
                    row_sad += (val_r - val_t).abs() as u64;
                    rx += 1;
                }

                running += row_sad;
                // EARLY-ABORT por fila contra min(best, cota-semilla) — ver
                // nota de equivalencia en find_best_match_sad_rect.
                if running > best_sad.min(abort_bound) {
                    aborted = true;
                    break;
                }
            }

            if !aborted && running < best_sad {
                best_sad = running;
                best_dx = dx;
                best_dy = dy;
            }
        }
    }
    (best_sad, best_dx, best_dy)
}

#[cfg(target_arch = "aarch64")]
unsafe fn find_best_match_sad_rect_neon(
    ref_data: &[u16],
    tgt_data: &[u16],
    width: usize,
    height: usize,
    roi_x: usize,
    roi_y: usize,
    roi_w: usize,
    roi_h: usize,
    start_dx: isize,
    start_dy: isize,
    search_range: isize,
    abort_bound: u64,
) -> (u64, isize, isize) {
    use std::arch::aarch64::*;

    let mut best_sad = u64::MAX;
    let mut best_dx = start_dx;
    let mut best_dy = start_dy;

    let min_dx = start_dx - search_range;
    let max_dx = start_dx + search_range;
    let min_dy = start_dy - search_range;
    let max_dy = start_dy + search_range;

    for dy in min_dy..=max_dy {
        for dx in min_dx..=max_dx {
            let t_x = roi_x as isize + dx;
            let t_y = roi_y as isize + dy;

            if t_x < 0
                || t_y < 0
                || (t_x + roi_w as isize) > width as isize
                || (t_y + roi_h as isize) > height as isize
            {
                continue;
            }

            let mut running = 0u64;
            let mut aborted = false;

            let row_r_base = roi_y * width + roi_x;
            let row_t_base = (t_y as usize) * width + (t_x as usize);

            let p_ref_start = ref_data.as_ptr().add(row_r_base);
            let p_tgt_start = tgt_data.as_ptr().add(row_t_base);

            for i in 0..roi_h {
                let r_ptr = p_ref_start.add(i * width);
                let t_ptr = p_tgt_start.add(i * width);
                let mut rx = 0;

                let mut vec_acc_low = vdupq_n_u32(0);
                let mut vec_acc_high = vdupq_n_u32(0);
                while rx + 8 <= roi_w {
                    let va = vld1q_u16(r_ptr.add(rx));
                    let vb = vld1q_u16(t_ptr.add(rx));
                    let diff = vabdq_u16(va, vb);
                    vec_acc_low = vaddw_u16(vec_acc_low, vget_low_u16(diff));
                    vec_acc_high = vaddw_u16(vec_acc_high, vget_high_u16(diff));
                    rx += 8;
                }
                let mut row_sad =
                    vaddvq_u32(vec_acc_low) as u64 + vaddvq_u32(vec_acc_high) as u64;

                while rx < roi_w {
                    let val_r = *r_ptr.add(rx) as i32;
                    let val_t = *t_ptr.add(rx) as i32;
                    row_sad += (val_r - val_t).abs() as u64;
                    rx += 1;
                }

                running += row_sad;
                // EARLY-ABORT por fila contra min(best, cota-semilla): un
                // candidato cuyo parcial supera esa cota no puede ganar NI
                // empatar (suma no negativa + '<' estricta) — argmin y SAD
                // bit-idénticos a la búsqueda exhaustiva. Poda 60-80%+.
                if running > best_sad.min(abort_bound) {
                    aborted = true;
                    break;
                }
            }

            if !aborted && running < best_sad {
                best_sad = running;
                best_dx = dx;
                best_dy = dy;
            }
        }
    }
    (best_sad, best_dx, best_dy)
}

// ==========================================
// CENTER OF GRAVITY (Planetary Stabilization)
// ==========================================

pub fn calculate_center_of_gravity(
    data: &[u16],
    width: usize,
    height: usize,
    threshold: u16,
) -> Option<(f32, f32)> {
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    let mut total_mass = 0.0;

    for y in 0..height {
        let row_off = y * width;
        for x in 0..width {
            let val = data[row_off + x];
            if val > threshold {
                let v = val as f32;
                sum_x += x as f32 * v;
                sum_y += y as f32 * v;
                total_mass += v;
            }
        }
    }

    if total_mass > 0.0 {
        Some((sum_x / total_mass, sum_y / total_mass))
    } else {
        None
    }
}

// ==========================================
// OUTLIER REJECTION (Robust Multipoint)
// ==========================================

/// Filters AP shifts by rejecting outliers that deviate significantly from the median shift.
/// Returns a boolean mask where true = keep, false = reject.
pub fn filter_ap_shifts(
    shifts: &[(f32, f32, f32)], // (dx, dy, quality)
    tolerance: f32,
) -> Vec<bool> {
    if shifts.is_empty() {
        return Vec::new();
    }

    // 1. Calculate Median Shift (Robust Central Tendency)
    let mut dxs: Vec<f32> = shifts.iter().map(|s| s.0).collect();
    let mut dys: Vec<f32> = shifts.iter().map(|s| s.1).collect();

    // Sort to find median
    // Handle NaNs by treating as valid (shouldn't happen, but safe sort)
    dxs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    dys.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let mid = dxs.len() / 2;
    let median_dx = dxs[mid];
    let median_dy = dys[mid]; // Simple independent median (could use spatial median but this is faster)

    // 2. Filter Outliers
    let mut valid_mask = Vec::with_capacity(shifts.len());
    let tol_sq = tolerance * tolerance;

    for (dx, dy, _) in shifts {
        let diff_x = dx - median_dx;
        let diff_y = dy - median_dy;
        let dist_sq = diff_x * diff_x + diff_y * diff_y;

        if dist_sq <= tol_sq {
            valid_mask.push(true);
        } else {
            valid_mask.push(false);
        }
    }

    valid_mask
}

// ==========================================
// POINT-BASED SAD (Used by Analysis)
// ==========================================

pub fn find_best_match_sad(
    ref_edges: &[u16],
    tgt_edges: &[u16],
    w: usize,
    ax: usize,
    ay: usize,
    fx_est: usize,
    fy_est: usize,
    box_size: usize,
    search_r: i32,
) -> (f32, f32, u64) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe {
                return find_best_match_sad_avx2(
                    ref_edges, tgt_edges, w, ax, ay, fx_est, fy_est, box_size, search_r,
                );
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        unsafe {
            return find_best_match_sad_neon(
                ref_edges, tgt_edges, w, ax, ay, fx_est, fy_est, box_size, search_r,
            );
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    find_best_match_sad_scalar(
        ref_edges, tgt_edges, w, ax, ay, fx_est, fy_est, box_size, search_r,
    )
}

fn subpixel_refine_sad(
    ref_edges: &[u16],
    tgt_edges: &[u16],
    w: usize,
    ax: usize,
    ay: usize,
    fx_est: usize,
    fy_est: usize,
    box_size: usize,
    idx: f32,
    idy: f32,
    best_sad: u64,
) -> (f32, f32) {
    let idx_i = idx as i32;
    let idy_i = idy as i32;

    // ANTI PIXEL-LOCKING: SAD is an L1 cost — near the true minimum its
    // surface is V-shaped (∝|d|), NOT parabolic. Fitting a parabola to a V
    // systematically shrinks the sub-pixel offset toward integer positions
    // ("pixel locking"), adding structured jitter that convolves the stack
    // with a blur kernel. The correct estimator for a V-shaped cost is the
    // EQUIANGULAR (two-line intersection) fit — see fit_axis below.
    //
    // PR-1.5: el fit equiangular solo consume S₋₁/S₀/S₊₁ por eje. La rejilla
    // 5×5 anterior evaluaba 24 SADs de caja completa y descartaba 20 — y si
    // CUALQUIER muestra (incluidas las ±2 y las esquinas, que nadie leía)
    // caía fuera de rango, anulaba TODO el subpíxel del AP: pixel-locking
    // estructural justo en los APs del limbo y del borde del ROI. Ahora se
    // evalúan solo las 4 muestras útiles y cada eje degrada a 0.0 por
    // separado únicamente si SU muestra ±1 no es evaluable.
    let sample = |dx: i32, dy: i32| -> Option<f64> {
        let s = compute_sad_at(
            ref_edges,
            tgt_edges,
            w,
            ax,
            ay,
            fx_est,
            fy_est,
            box_size,
            idx_i + dx,
            idy_i + dy,
        );
        if s > 1e18 as u64 {
            None // Out of bounds
        } else {
            Some(s as f64)
        }
    };
    let s_0 = best_sad as f64;

    // EQUIANGULAR FIT (V-model): the L1/SAD cost near its minimum behaves as
    // S(d) = S_min + a·|d − δ|. The two-line intersection recovers δ without
    // the pixel-locking bias of a parabola fit:
    //   δ = ½·(S₋₁ − S₊₁) / (max(S₋₁, S₊₁) − S₀)
    let fit_axis = |m1: Option<f64>, p1: Option<f64>| -> f32 {
        let (Some(s_m1), Some(s_p1)) = (m1, p1) else {
            return 0.0; // muestra fuera de rango: sin subpíxel en ESTE eje
        };
        let steeper = if s_m1 > s_p1 { s_m1 - s_0 } else { s_p1 - s_0 };
        if steeper <= 1e-9 {
            return 0.0; // flat cost: no sub-pixel information
        }
        let offset = 0.5 * (s_m1 - s_p1) / steeper;
        (offset as f32).clamp(-0.95, 0.95)
    };

    (
        fit_axis(sample(-1, 0), sample(1, 0)),
        fit_axis(sample(0, -1), sample(0, 1)),
    )
}

/// REFINAMIENTO SUB-PIXEL LUCAS-KANADE (Gauss-Newton, inverse compositional).
///
/// El fit equiangular sobre la superficie SAD deja ~0.1-0.2 px de residuo;
/// este pulido de 2-3 iteraciones sobre los MISMOS mapas de bordes converge a
/// ~0.02-0.05 px — la diferencia visible en rilles, festones y granulacion.
/// Esquema inverse-compositional: gradiente y Hessiano se calculan UNA sola
/// vez sobre el master (constantes por AP); cada iteracion solo muestrea el
/// target bilinealmente y acumula el residuo (sin allocations).
///
/// `shift_dx/dy` es el desplazamiento TOTAL master→frame con la convencion
/// del pipeline (Target(x + s) ≈ Master(x)). Devuelve el shift refinado, o
/// None si el patch carece de textura 2-D (aperture problem), el muestreo se
/// sale del frame o la iteracion diverge — el caller conserva entonces el
/// estimado SAD+equiangular. La correccion total esta acotada a ±0.75 px:
/// LK solo PULE el minimo ya encontrado, nunca re-decide el matching.
#[allow(clippy::too_many_arguments)]
pub fn refine_shift_lucas_kanade(
    master: &[u16],
    target: &[u16],
    w: usize,
    h: usize,
    ax: usize,
    ay: usize,
    shift_dx: f32,
    shift_dy: f32,
    box_size: usize,
    iterations: usize,
) -> Option<(f32, f32)> {
    let half = (box_size / 2) as i32;
    let axi = ax as i32;
    let ayi = ay as i32;
    // Margen ±1 para el gradiente central del master.
    if axi - half < 1
        || ayi - half < 1
        || axi + half >= w as i32 - 1
        || ayi + half >= h as i32 - 1
    {
        return None;
    }

    // Hessiano 2×2 del master (una sola vez — inverse compositional).
    let mut h00 = 0.0f64;
    let mut h01 = 0.0f64;
    let mut h11 = 0.0f64;
    for oy in -half..half {
        let y = (ayi + oy) as usize;
        let row = y * w;
        for ox in -half..half {
            let x = (axi + ox) as usize;
            let i = row + x;
            let gx = (master[i + 1] as f64 - master[i - 1] as f64) * 0.5;
            let gy = (master[i + w] as f64 - master[i - w] as f64) * 0.5;
            h00 += gx * gx;
            h01 += gx * gy;
            h11 += gy * gy;
        }
    }
    // Sin energia de gradiente en algun eje (patch plano) o sistema casi
    // singular (borde 1-D puro → aperture problem): dejar el estimado SAD.
    let det = h00 * h11 - h01 * h01;
    if h00 < 1.0 || h11 < 1.0 || det < 1e-6 * h00 * h11 {
        return None;
    }
    let inv00 = h11 / det;
    let inv01 = -h01 / det;
    let inv11 = h00 / det;

    let mut pdx = shift_dx;
    let mut pdy = shift_dy;
    for _ in 0..iterations.max(1) {
        // Bounds del patch desplazado para el muestreo bilineal: si alguna
        // esquina se sale del frame, abortar (conservar estimado SAD).
        let min_x = (axi - half) as f32 + pdx;
        let min_y = (ayi - half) as f32 + pdy;
        let max_x = (axi + half - 1) as f32 + pdx;
        let max_y = (ayi + half - 1) as f32 + pdy;
        if min_x < 0.0 || min_y < 0.0 || max_x >= (w - 1) as f32 || max_y >= (h - 1) as f32 {
            return None;
        }

        let mut b0 = 0.0f64;
        let mut b1 = 0.0f64;
        for oy in -half..half {
            let y = (ayi + oy) as usize;
            let syf = y as f32 + pdy;
            let sy0 = syf as usize; // syf >= 0 garantizado por el bounds check
            let fy = syf - sy0 as f32;
            let row_m = y * w;
            let row_t = sy0 * w;
            for ox in -half..half {
                let x = (axi + ox) as usize;
                let i = row_m + x;
                let gx = (master[i + 1] as f64 - master[i - 1] as f64) * 0.5;
                let gy = (master[i + w] as f64 - master[i - w] as f64) * 0.5;

                let sxf = x as f32 + pdx;
                let sx0 = sxf as usize;
                let fx = sxf - sx0 as f32;
                let t00 = target[row_t + sx0] as f32;
                let t10 = target[row_t + sx0 + 1] as f32;
                let t01 = target[row_t + w + sx0] as f32;
                let t11 = target[row_t + w + sx0 + 1] as f32;
                let tv = t00 * (1.0 - fx) * (1.0 - fy)
                    + t10 * fx * (1.0 - fy)
                    + t01 * (1.0 - fx) * fy
                    + t11 * fx * fy;

                let e = tv as f64 - master[i] as f64;
                b0 += gx * e;
                b1 += gy * e;
            }
        }

        // Gauss-Newton para traslacion pura: p ← p − H⁻¹·b
        let ddx = (inv00 * b0 + inv01 * b1) as f32;
        let ddy = (inv01 * b0 + inv11 * b1) as f32;
        if !ddx.is_finite() || !ddy.is_finite() || ddx.abs() > 1.5 || ddy.abs() > 1.5 {
            return None; // divergencia: el residuo no es un pulido local
        }
        pdx -= ddx;
        pdy -= ddy;
        if ddx.abs() < 0.005 && ddy.abs() < 0.005 {
            break; // convergido
        }
    }

    // LK solo pule: una correccion grande significa salto de cuenca del SAD.
    if (pdx - shift_dx).abs() > 0.75 || (pdy - shift_dy).abs() > 0.75 {
        return None;
    }
    Some((pdx, pdy))
}

fn compute_sad_at(
    ref_edges: &[u16],
    tgt_edges: &[u16],
    w: usize,
    ax: usize,
    ay: usize,
    fx_est: usize,
    fy_est: usize,
    box_size: usize,
    dx: i32,
    dy: i32,
) -> u64 {
    let h = ref_edges.len() / w;
    let half_box = (box_size / 2) as i32;
    let y_start_t = (fy_est as i32) + dy - half_box;
    let x_start_t = (fx_est as i32) + dx - half_box;
    let y_start_r = (ay as i32) - half_box;
    let x_start_r = (ax as i32) - half_box;

    if y_start_t < 0
        || y_start_t + (box_size as i32) > (h as i32)
        || x_start_t < 0
        || x_start_t + (box_size as i32) > (w as i32)
        || y_start_r < 0
        || y_start_r + (box_size as i32) > (h as i32)
        || x_start_r < 0
        || x_start_r + (box_size as i32) > (w as i32)
    {
        return u64::MAX;
    }

    let mut sad = 0u64;
    for iy in 0..box_size {
        let r_off = (y_start_r as usize + iy) * w + x_start_r as usize;
        let t_off = (y_start_t as usize + iy) * w + x_start_t as usize;
        for ix in 0..box_size {
            let diff = (ref_edges[r_off + ix] as i32 - tgt_edges[t_off + ix] as i32).abs();
            sad += diff as u64;
        }
    }
    sad
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn find_best_match_sad_avx2(
    ref_edges: &[u16],
    tgt_edges: &[u16],
    w: usize,
    ax: usize,
    ay: usize,
    fx_est: usize,
    fy_est: usize,
    box_size: usize,
    search_r: i32,
) -> (f32, f32, u64) {
    let h = ref_edges.len() / w;
    let half_box = (box_size / 2) as i32;
    let mut best_sad = u64::MAX;
    let mut best_dx = 0;
    let mut best_dy = 0;

    let v_zero = _mm256_setzero_si256();

    // SEMILLA CENTRAL + EARLY-EXIT: ver find_best_match_sad_scalar (mismo
    // esquema exacto en las 3 variantes; el minimo verdadero nunca se poda).
    for phase in 0..2 {
    for dy in -search_r..=search_r {
        for dx in -search_r..=search_r {
            if (phase == 0) != (dx == 0 && dy == 0) {
                continue;
            }
            let y_start_t = (fy_est as i32) + dy - half_box;
            let x_start_t = (fx_est as i32) + dx - half_box;
            let y_start_r = (ay as i32) - half_box;
            let x_start_r = (ax as i32) - half_box;

            if y_start_t < 0
                || y_start_t + (box_size as i32) > (h as i32)
                || x_start_t < 0
                || x_start_t + (box_size as i32) > (w as i32)
                || y_start_r < 0
                || y_start_r + (box_size as i32) > (h as i32)
                || x_start_r < 0
                || x_start_r + (box_size as i32) > (w as i32)
            {
                continue;
            }

            let mut vec_acc = _mm256_setzero_si256();
            let mut acc_64_lo = _mm256_setzero_si256();
            let mut acc_64_hi = _mm256_setzero_si256();
            let mut scalar_sad = 0u64;
            let mut pruned = false;

            let row_r_base = (y_start_r as usize) * w + (x_start_r as usize);
            let row_t_base = (y_start_t as usize) * w + (x_start_t as usize);

            let p_ref = ref_edges.as_ptr().add(row_r_base);
            let p_tgt = tgt_edges.as_ptr().add(row_t_base);

            // OPT 12: Loop Unrolling & Direct Pointer Arithmetic
            for i in 0..box_size {
                let r_ptr = p_ref.add(i * w);
                let t_ptr = p_tgt.add(i * w);
                let mut rx = 0;

                while rx + 16 <= box_size {
                    let va = _mm256_loadu_si256(r_ptr.add(rx) as *const _);
                    let vb = _mm256_loadu_si256(t_ptr.add(rx) as *const _);

                    // Absolute difference of epu16
                    let vmin = _mm256_min_epu16(va, vb);
                    let vmax = _mm256_max_epu16(va, vb);
                    let diff = _mm256_sub_epi16(vmax, vmin);

                    // Unpack to 32-bit and add to vec_acc
                    let lo = _mm256_unpacklo_epi16(diff, v_zero);
                    let hi = _mm256_unpackhi_epi16(diff, v_zero);
                    vec_acc = _mm256_add_epi32(vec_acc, lo);
                    vec_acc = _mm256_add_epi32(vec_acc, hi);
                    rx += 16;
                }

                while rx < box_size {
                    let val_r = *r_ptr.add(rx) as i32;
                    let val_t = *t_ptr.add(rx) as i32;
                    scalar_sad += (val_r - val_t).abs() as u64;
                    rx += 1;
                }

                // EARLY-EXIT cada 8 filas: reduccion parcial no destructiva —
                // si el parcial ya alcanza la cota, este candidato no puede
                // ganar (terminos no-negativos) y se abandona.
                if (i & 7) == 7 && best_sad != u64::MAX {
                    let mut lanes = [0u32; 8];
                    _mm256_storeu_si256(lanes.as_mut_ptr() as *mut _, vec_acc);
                    let partial: u64 =
                        lanes.iter().map(|&v| v as u64).sum::<u64>() + scalar_sad;
                    if partial >= best_sad {
                        pruned = true;
                        break;
                    }
                }
            }
            if pruned {
                continue;
            }

            // Move periodic flush OUTSIDE the inner `for i` loop block
            // An accumulation of 32-bit SAD for 16x pixels maxes out at 16 * 65535 = 1,048,560.
            // A 32-bit int maxes at 2.14B. So we are COMPLETELY safe accumulating the whole AP box
            // inside 32-bit `vec_acc` without overflowing before doing a single flush at the end!
            let v_lo = _mm256_unpacklo_epi32(vec_acc, v_zero);
            let v_hi = _mm256_unpackhi_epi32(vec_acc, v_zero);
            acc_64_lo = _mm256_add_epi64(acc_64_lo, v_lo);
            acc_64_hi = _mm256_add_epi64(acc_64_hi, v_hi);

            // Final Reduction
            let mut lanes_lo = [0i64; 4];
            let mut lanes_hi = [0i64; 4];
            _mm256_storeu_si256(lanes_lo.as_mut_ptr() as *mut _, acc_64_lo);
            _mm256_storeu_si256(lanes_hi.as_mut_ptr() as *mut _, acc_64_hi);

            let mut simd_sum = 0u64;
            for j in 0..4 {
                simd_sum += lanes_lo[j] as u64 + lanes_hi[j] as u64;
            }

            let total_sad = simd_sum + scalar_sad;

            if total_sad < best_sad {
                best_sad = total_sad;
                best_dx = dx as i32;
                best_dy = dy as i32;
            }
        }
    }
    } // end phase loop
    (best_dx as f32, best_dy as f32, best_sad)
}

#[cfg(target_arch = "aarch64")]
unsafe fn find_best_match_sad_neon(
    ref_edges: &[u16],
    tgt_edges: &[u16],
    w: usize,
    ax: usize,
    ay: usize,
    fx_est: usize,
    fy_est: usize,
    box_size: usize,
    search_r: i32,
) -> (f32, f32, u64) {
    use std::arch::aarch64::*;

    let h = ref_edges.len() / w;
    let half_box = (box_size / 2) as i32;
    let mut best_sad = u64::MAX;
    let mut best_dx = 0;
    let mut best_dy = 0;

    // SEMILLA CENTRAL + EARLY-EXIT: ver find_best_match_sad_scalar (mismo
    // esquema exacto en las 3 variantes; el minimo verdadero nunca se poda).
    for phase in 0..2 {
    for dy in -search_r..=search_r {
        for dx in -search_r..=search_r {
            if (phase == 0) != (dx == 0 && dy == 0) {
                continue;
            }
            let y_start_t = (fy_est as i32) + dy - half_box;
            let x_start_t = (fx_est as i32) + dx - half_box;
            let y_start_r = (ay as i32) - half_box;
            let x_start_r = (ax as i32) - half_box;

            if y_start_t < 0
                || y_start_t + (box_size as i32) > (h as i32)
                || x_start_t < 0
                || x_start_t + (box_size as i32) > (w as i32)
                || y_start_r < 0
                || y_start_r + (box_size as i32) > (h as i32)
                || x_start_r < 0
                || x_start_r + (box_size as i32) > (w as i32)
            {
                continue;
            }

            let mut vec_acc_low = vdupq_n_u32(0);
            let mut vec_acc_high = vdupq_n_u32(0);
            let mut acc_64_low = vdupq_n_u64(0);
            let mut acc_64_high = vdupq_n_u64(0);
            let mut scalar_sad = 0u64;
            let mut pruned = false;

            let row_r_base = (y_start_r as usize) * w + (x_start_r as usize);
            let row_t_base = (y_start_t as usize) * w + (x_start_t as usize);

            let p_ref = ref_edges.as_ptr().add(row_r_base);
            let p_tgt = tgt_edges.as_ptr().add(row_t_base);

            for i in 0..box_size {
                let r_ptr = p_ref.add(i * w);
                let t_ptr = p_tgt.add(i * w);
                let mut rx = 0;

                while rx + 8 <= box_size {
                    let va = vld1q_u16(r_ptr.add(rx));
                    let vb = vld1q_u16(t_ptr.add(rx));
                    let diff = vabdq_u16(va, vb);
                    vec_acc_low = vaddw_u16(vec_acc_low, vget_low_u16(diff));
                    vec_acc_high = vaddw_u16(vec_acc_high, vget_high_u16(diff));
                    rx += 8;
                }

                while rx < box_size {
                    let val_r = *r_ptr.add(rx) as i32;
                    let val_t = *t_ptr.add(rx) as i32;
                    scalar_sad += (val_r - val_t).abs() as u64;
                    rx += 1;
                }

                // EARLY-EXIT cada 8 filas (reduccion parcial no destructiva).
                if (i & 7) == 7 && best_sad != u64::MAX {
                    let partial = vaddvq_u32(vec_acc_low) as u64
                        + vaddvq_u32(vec_acc_high) as u64
                        + scalar_sad;
                    if partial >= best_sad {
                        pruned = true;
                        break;
                    }
                }
            }
            if pruned {
                continue;
            }

            acc_64_low = vaddq_u64(acc_64_low, vmovl_u32(vget_low_u32(vec_acc_low)));
            acc_64_low = vaddq_u64(acc_64_low, vmovl_u32(vget_high_u32(vec_acc_low)));
            acc_64_high = vaddq_u64(acc_64_high, vmovl_u32(vget_low_u32(vec_acc_high)));
            acc_64_high = vaddq_u64(acc_64_high, vmovl_u32(vget_high_u32(vec_acc_high)));

            let mut lanes_low = [0u64; 2];
            let mut lanes_high = [0u64; 2];
            vst1q_u64(lanes_low.as_mut_ptr(), acc_64_low);
            vst1q_u64(lanes_high.as_mut_ptr(), acc_64_high);

            let simd_sum = lanes_low[0] + lanes_low[1] + lanes_high[0] + lanes_high[1];
            let total_sad = simd_sum + scalar_sad;

            if total_sad < best_sad {
                best_sad = total_sad;
                best_dx = dx as i32;
                best_dy = dy as i32;
            }
        }
    }
    } // end phase loop
    (best_dx as f32, best_dy as f32, best_sad)
}

#[allow(dead_code)]
fn find_best_match_sad_scalar(
    ref_edges: &[u16],
    tgt_edges: &[u16],
    w: usize,
    ax: usize,
    ay: usize,
    fx_est: usize,
    fy_est: usize,
    box_size: usize,
    search_r: i32,
) -> (f32, f32, u64) {
    let h = ref_edges.len() / w;
    let half_box = (box_size / 2) as i32;
    let mut best_sad = u64::MAX;
    let mut best_dx = 0;
    let mut best_dy = 0;

    // SEMILLA CENTRAL + EARLY-EXIT (identico en las 3 variantes): la fase 0
    // evalua SOLO (0,0) — el estimado del piramide/analisis, casi siempre a
    // ≤1-2 px del optimo — para arrancar con una cota best_sad fuerte; la
    // fase 1 recorre el resto y cada candidato ABORTA en cuanto su parcial
    // (suma de terminos no-negativos) alcanza la cota. El minimo verdadero
    // nunca se poda: mismo (dx, dy, sad), solo se ahorra el trabajo de los
    // perdedores (~40-60% del coste de la etapa fina).
    for phase in 0..2 {
        for dy in -search_r..=search_r {
            for dx in -search_r..=search_r {
                if (phase == 0) != (dx == 0 && dy == 0) {
                    continue;
                }
                let mut sad = 0u64;
                let y_start_t = (fy_est as i32) + dy - half_box;
                let x_start_t = (fx_est as i32) + dx - half_box;
                let y_start_r = (ay as i32) - half_box;
                let x_start_r = (ax as i32) - half_box;

                if y_start_t < 0 || x_start_t < 0 || y_start_r < 0 || x_start_r < 0 {
                    continue;
                }
                if (y_start_t + box_size as i32) >= h as i32
                    || (x_start_t + box_size as i32) >= w as i32
                {
                    continue;
                }
                if (y_start_r + box_size as i32) >= h as i32
                    || (x_start_r + box_size as i32) >= w as i32
                {
                    continue;
                }

                let mut pruned = false;
                for y in 0..box_size {
                    let t_row = ((y_start_t + y as i32) as usize) * w;
                    let r_row = ((y_start_r + y as i32) as usize) * w;
                    for x in 0..box_size {
                        let t_idx = t_row + (x_start_t + x as i32) as usize;
                        let r_idx = r_row + (x_start_r + x as i32) as usize;

                        if t_idx >= tgt_edges.len() || r_idx >= ref_edges.len() {
                            continue;
                        }

                        let t_val = tgt_edges[t_idx] as i32;
                        let r_val = ref_edges[r_idx] as i32;
                        sad += (t_val - r_val).abs() as u64;
                    }
                    if sad >= best_sad {
                        pruned = true;
                        break;
                    }
                }
                if pruned {
                    continue;
                }

                if sad < best_sad {
                    best_sad = sad;
                    best_dx = dx;
                    best_dy = dy;
                }
            }
        }
    }
    (best_dx as f32, best_dy as f32, best_sad)
}

// ------------------------------------------------------------------
// ADDITIONAL UTILITIES FOR PYRAMIDAL FAST SAD
// ------------------------------------------------------------------

/// Pyramidal (Coarse-to-Fine) SAD search — ~10× faster than brute force.
/// Stage 1: Coarse search on 4× downscaled images with full search_r
/// Stage 2: Fine refinement on full-resolution images with search_r=2 around coarse best
pub fn find_best_match_sad_pyramid_fast(
    ref_edges: &[u16],
    tgt_edges: &[u16],
    w: usize,
    h: usize,
    ax: usize,
    ay: usize,
    fx_est: usize,
    fy_est: usize,
    box_size: usize,
    search_r: i32,
    // Pre-allocated downscaled buffers to avoid per-call allocation
    ref_ds: &[u16],
    tgt_ds: &[u16],
    w_ds: usize,
) -> (f32, f32, u64) {
    if search_r <= 3 || w < 64 || h < 64 || box_size < 8 {
        return find_best_match_sad(
            ref_edges, tgt_edges, w, ax, ay, fx_est, fy_est, box_size, search_r,
        );
    }

    // Fast 4x downscaling
    let mut ref_ds_owned = Vec::new();
    let mut tgt_ds_owned = Vec::new();

    let (ref_ds_ptr, tgt_ds_ptr) = if ref_ds.is_empty() || tgt_ds.is_empty() {
        downscale_4x(ref_edges, w, h, &mut ref_ds_owned);
        downscale_4x(tgt_edges, w, h, &mut tgt_ds_owned);
        (&ref_ds_owned[..], &tgt_ds_owned[..])
    } else {
        (ref_ds, tgt_ds)
    };

    let coarse_search_r = (search_r / 4).max(2);
    let coarse_box = (box_size / 4).max(4);

    let (cdx, cdy, _) = find_best_match_sad(
        ref_ds_ptr,
        tgt_ds_ptr,
        w_ds,
        ax / 4,
        ay / 4,
        fx_est / 4,
        fy_est / 4,
        coarse_box,
        coarse_search_r,
    );

    refine_best_match_sad_from_coarse(
        ref_edges,
        tgt_edges,
        w,
        h,
        ax,
        ay,
        fx_est,
        fy_est,
        box_size,
        cdx,
        cdy,
        4.0,
    )
}

/// Segunda etapa CPU del matcher AP cuando la búsqueda gruesa se ejecutó en
/// GPU. Incluye ventana adaptativa, fallback amplio y el mismo ajuste
/// equiangular subpíxel de la ruta SIMD de referencia.
#[allow(clippy::too_many_arguments)]
pub fn refine_best_match_sad_from_coarse(
    ref_edges: &[u16],
    tgt_edges: &[u16],
    w: usize,
    h: usize,
    ax: usize,
    ay: usize,
    fx_est: usize,
    fy_est: usize,
    box_size: usize,
    coarse_dx: f32,
    coarse_dy: f32,
    coarse_scale: f32,
) -> (f32, f32, u64) {
    let fine_fx = ((fx_est as f32) + coarse_dx * coarse_scale).max(0.0) as usize;
    let fine_fy = ((fy_est as f32) + coarse_dy * coarse_scale).max(0.0) as usize;

    // Clamp to valid range
    let half_box = box_size / 2;
    let fine_fx = fine_fx.max(half_box).min(w.saturating_sub(half_box + 1));
    let fine_fy = fine_fy.max(half_box).min(h.saturating_sub(half_box + 1));

    // Stage 2: Fine refinement with unified router — VENTANA ADAPTATIVA.
    // La etapa fina es el termino DOMINANTE del coste por AP (posiciones ×
    // box² con el box completo, independiente de search_r): ±6 = 169
    // evaluaciones SAD. El coarse a 4× deja un residuo de cuantizacion de
    // ±2 px, asi que ±3 lo cubre en la gran mayoria de frames (49 evals,
    // ~3.4× menos). EXACTITUD PRESERVADA: si el optimo cae EN el borde de la
    // ventana reducida (señal de que el residuo real era mayor), se repite
    // con la ventana completa ±6 — solo pagan el doble los pocos casos que
    // de verdad lo necesitan.
    let (fdx, fdy, fsad) = {
        let (dx3, dy3, sad3) = find_best_match_sad(
            ref_edges, tgt_edges, w, ax, ay, fine_fx, fine_fy, box_size, 3,
        );
        if dx3.abs() >= 2.5 || dy3.abs() >= 2.5 {
            find_best_match_sad(
                ref_edges, tgt_edges, w, ax, ay, fine_fx, fine_fy, box_size, 6,
            )
        } else {
            (dx3, dy3, sad3)
        }
    };

    // Total displacement = coarse offset + fine correction
    let total_dx = (fine_fx as f32 - fx_est as f32) + fdx;
    let total_dy = (fine_fy as f32 - fy_est as f32) + fdy;

    // Sub-pixel refinement (only on final fine pass — 4 scalar SAD evals)
    let (sdx, sdy) = subpixel_refine_sad(
        ref_edges, tgt_edges, w, ax, ay, fine_fx, fine_fy, box_size, fdx, fdy, fsad,
    );

    (total_dx + sdx, total_dy + sdy, fsad)
}

/// Fast 2× downscale of a u16 image using 2×2 box average (analysis half-res)
pub fn downscale_2x_into(src: &[u16], w: usize, h: usize, dst: &mut Vec<u16>) -> (usize, usize) {
    let w_ds = w / 2;
    let h_ds = h / 2;
    let len = w_ds * h_ds;
    if dst.len() != len {
        dst.resize(len, 0);
    }
    for dy in 0..h_ds {
        let sy = dy * 2;
        let row0 = sy * w;
        let row1 = (sy + 1) * w;
        for dx in 0..w_ds {
            let sx = dx * 2;
            let sum = src[row0 + sx] as u32
                + src[row0 + sx + 1] as u32
                + src[row1 + sx] as u32
                + src[row1 + sx + 1] as u32;
            dst[dy * w_ds + dx] = (sum >> 2) as u16;
        }
    }
    (w_ds, h_ds)
}

/// Fast 4× downscale of a u16 image using 4×4 box average
pub fn downscale_4x(src: &[u16], w: usize, h: usize, dst: &mut Vec<u16>) -> (usize, usize) {
    let w_ds = w / 4;
    let h_ds = h / 4;
    let len = w_ds * h_ds;
    if dst.len() < len {
        dst.resize(len, 0);
    }
    for dy in 0..h_ds {
        let sy = dy * 4;
        for dx in 0..w_ds {
            let sx = dx * 4;
            let mut sum = 0u32;
            for ky in 0..4 {
                let row = (sy + ky) * w + sx;
                for kx in 0..4 {
                    sum += src[row + kx] as u32;
                }
            }
            dst[dy * w_ds + dx] = (sum >> 4) as u16; // /16
        }
    }
    (w_ds, h_ds)
}

// ==========================================
// SPATIAL STITCH CHECK (Neighbors)
// ==========================================

/// Filters AP shifts by checking consistency with immediate spatial neighbors.
/// Returns a mask where true = keep, false = reject.
/// Also returns the "safe" fallback vector for rejected points (average of valid neighbors).
pub fn filter_ap_shifts_spatial(
    shifts: &[(f32, f32, f32)], // (dx, dy, quality)
    points: &[crate::smart_grid::ApPoint],
    grid_size: usize,
    tolerance: f32,
) -> (Vec<bool>, Vec<(f32, f32)>) {
    filter_ap_shifts_spatial_with_options(shifts, points, grid_size, tolerance, 1.55, 0.75)
}

/// Spatial AP-shift filter with tunable neighbor radius and rejection strength.
/// The lookup stores multiple APs per hash cell, so it remains valid when
/// surface AP grids intentionally overlap.
pub fn filter_ap_shifts_spatial_with_options(
    shifts: &[(f32, f32, f32)], // (dx, dy, quality)
    points: &[crate::smart_grid::ApPoint],
    grid_size: usize,
    tolerance: f32,
    neighbor_radius_scale: f32,
    rejection_scale: f32,
) -> (Vec<bool>, Vec<(f32, f32)>) {
    if shifts.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let n = shifts.len();
    let mut valid_mask = vec![true; n];
    let mut safe_fallback = vec![(0.0, 0.0); n];

    // Build a spatial lookup that tolerates overlapping AP centers.
    use std::collections::HashMap;
    let cell_size = (grid_size as f32 * 0.75).max(8.0);
    let neighbor_radius = (grid_size as f32 * neighbor_radius_scale).max(cell_size);
    let cell_span = (neighbor_radius / cell_size).ceil() as isize + 1;
    let mut grid_map: HashMap<(isize, isize), Vec<usize>> = HashMap::with_capacity(n);

    for (i, p) in points.iter().enumerate() {
        let gx = (p.x / cell_size).floor() as isize;
        let gy = (p.y / cell_size).floor() as isize;
        grid_map.entry((gx, gy)).or_default().push(i);
    }

    // Calculate Global Median of strictly VALID shifts as absolute fallback
    let mut valid_dxs = Vec::new();
    let mut valid_dys = Vec::new();
    for &(dx, dy, q) in shifts {
        if q > 0.0 {
            valid_dxs.push(dx);
            valid_dys.push(dy);
        }
    }

    let (global_m_dx, global_m_dy) = if !valid_dxs.is_empty() {
        valid_dxs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        valid_dys.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        (
            valid_dxs[valid_dxs.len() / 2],
            valid_dys[valid_dys.len() / 2],
        )
    } else {
        (0.0, 0.0) // Desperate fallback
    };

    for i in 0..n {
        let p = &points[i];
        let (my_dx, my_dy, my_q) = shifts[i];

        let gx = (p.x / cell_size).floor() as isize;
        let gy = (p.y / cell_size).floor() as isize;

        let mut neighbor_sum_dx = 0.0;
        let mut neighbor_sum_dy = 0.0;
        let mut neighbor_sum_w = 0.0;

        // 1. Collect ONLY Valid Neighbors
        for oy in -cell_span..=cell_span {
            for ox in -cell_span..=cell_span {
                if let Some(indices) = grid_map.get(&(gx + ox, gy + oy)) {
                    for &ni in indices {
                        if ni == i {
                            continue;
                        }
                        let np = &points[ni];
                        let px = p.x - np.x;
                        let py = p.y - np.y;
                        let dist_sq = px * px + py * py;
                        if dist_sq > neighbor_radius * neighbor_radius {
                            continue;
                        }
                        let (ndx, ndy, nq) = shifts[ni];
                        if nq > 0.0 {
                            // Favor close, high-confidence neighbors without letting a single
                            // noisy AP dominate the surface warp.
                            let dist = dist_sq.sqrt().max(1.0);
                            let w = (nq.clamp(0.05, 1.0) / dist).max(1e-6);
                            neighbor_sum_dx += ndx * w;
                            neighbor_sum_dy += ndy * w;
                            neighbor_sum_w += w;
                        }
                    }
                }
            }
        }

        let (avg_dx, avg_dy) = if neighbor_sum_w > 0.0 {
            (
                neighbor_sum_dx / neighbor_sum_w,
                neighbor_sum_dy / neighbor_sum_w,
            )
        } else {
            (global_m_dx, global_m_dy) // Fallback to global trend
        };

        if my_q == 0.0 {
            // Already mathematically rejected, force to safe average neighborhood
            valid_mask[i] = false;
            safe_fallback[i] = (avg_dx, avg_dy);
            continue;
        }

        if neighbor_sum_w > 0.0 {
            let diff_x = my_dx - avg_dx;
            let diff_y = my_dy - avg_dy;
            let dist = (diff_x * diff_x + diff_y * diff_y).sqrt();

            if dist > (tolerance * rejection_scale) {
                // Stricter local coherence check
                valid_mask[i] = false;
                safe_fallback[i] = (avg_dx, avg_dy);
            } else {
                safe_fallback[i] = (my_dx, my_dy);
            }
        } else {
            let diff_x = my_dx - global_m_dx;
            let diff_y = my_dy - global_m_dy;
            let dist = (diff_x * diff_x + diff_y * diff_y).sqrt();
            if dist > tolerance * 1.5 {
                // Isolated and wildly out of trend
                valid_mask[i] = false;
                safe_fallback[i] = (global_m_dx, global_m_dy);
            } else {
                safe_fallback[i] = (my_dx, my_dy);
            }
        }
    }

    (valid_mask, safe_fallback)
}

// ==========================================
// SOLAR SURFACE ENHANCEMENT
// ==========================================

/// Specific enhancement for low-contrast solar/lunar surface features.
/// Uses a strong High-Pass filter (Unsharp Mask) to isolate granules/craters.
pub fn enhance_solar_surface(input: &[u16], width: usize, height: usize) -> Vec<u16> {
    let len = width * height;
    if len == 0 {
        return Vec::new();
    }

    let mut out = vec![0u16; len];
    let mut scratch = vec![0u16; len];

    // 1. Heavy Box Blur (Radius 2 or 3) to estimate low-freq background
    // We want to remove the gradient and keep the texture.
    let radius = 2;

    // Horizontal
    for y in 0..height {
        let row_off = y * width;
        for x in 0..width {
            let mut sum: u32 = 0;
            let start = x.saturating_sub(radius);
            let end = (x + radius + 1).min(width);
            let count = end - start;
            for ix in start..end {
                sum += input[row_off + ix] as u32;
            }
            scratch[row_off + x] = (sum / count as u32) as u16;
        }
    }

    // Vertical & Difference
    for x in 0..width {
        for y in 0..height {
            let start = y.saturating_sub(radius);
            let end = (y + radius + 1).min(height);
            let count = end - start;
            let mut sum: u32 = 0;
            for iy in start..end {
                sum += scratch[iy * width + x] as u32;
            }
            let blurred = (sum / count as u32) as i32;
            let original = input[y * width + x] as i32;

            // High Pass = Original - LowPass
            let diff = original - blurred;

            // PHASE 42: Hybrid Surface Enhancement (Luminance + Texture Blend)
            // By keeping the original luminance, we provide a "structural gravity"
            // that prevents the SAD algorithm from wandering into phantom textures.
            let enhanced = original + diff * 14;

            out[y * width + x] = enhanced.clamp(0, 65535) as u16;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PARIDAD del early-abort SIMD: el kernel con poda por fila debe devolver
    /// EXACTAMENTE el mismo (sad, dx, dy) que un barrido exhaustivo sin poda,
    /// en geometrías con cola escalar (roi_w no múltiplo de 8/16), bordes que
    /// descartan candidatos, y empates (imagen constante → gana el PRIMER
    /// candidato del orden de barrido, que la poda debe preservar).
    #[test]
    fn sad_rect_early_abort_is_bit_identical_to_exhaustive_search() {
        fn exhaustive(
            ref_data: &[u16],
            tgt_data: &[u16],
            width: usize,
            height: usize,
            roi_x: usize,
            roi_y: usize,
            roi_w: usize,
            roi_h: usize,
            start_dx: isize,
            start_dy: isize,
            search_range: isize,
        ) -> (u64, isize, isize) {
            let mut best = (u64::MAX, start_dx, start_dy);
            for dy in (start_dy - search_range)..=(start_dy + search_range) {
                for dx in (start_dx - search_range)..=(start_dx + search_range) {
                    let t_x = roi_x as isize + dx;
                    let t_y = roi_y as isize + dy;
                    if t_x < 0
                        || t_y < 0
                        || (t_x + roi_w as isize) > width as isize
                        || (t_y + roi_h as isize) > height as isize
                    {
                        continue;
                    }
                    let mut sad = 0u64;
                    for ry in 0..roi_h {
                        for rx in 0..roi_w {
                            let a = ref_data[(roi_y + ry) * width + roi_x + rx] as i64;
                            let b = tgt_data
                                [(t_y as usize + ry) * width + t_x as usize + rx]
                                as i64;
                            sad += (a - b).unsigned_abs();
                        }
                    }
                    if sad < best.0 {
                        best = (sad, dx, dy);
                    }
                }
            }
            best
        }

        let (w, h) = (96usize, 80usize);
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rand16 = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed & 0xffff) as u16
        };
        let reference: Vec<u16> = (0..w * h).map(|_| rand16()).collect();
        let target: Vec<u16> = (0..w * h).map(|_| rand16()).collect();
        let constant = vec![777u16; w * h];

        let cases: &[(usize, usize, usize, usize, isize, isize, isize)] = &[
            (40, 30, 21, 17, 0, 0, 6),  // cola escalar en ambas rutas SIMD
            (32, 24, 32, 24, 0, 0, 5),  // alineado a 16
            (8, 40, 33, 9, 2, -3, 4),   // arranque descentrado
            (48, 8, 5, 60, 0, 0, 7),    // candidatos descartados por borde
        ];
        for &(rx0, ry0, rw, rh, sdx, sdy, sr) in cases {
            let got = find_best_match_sad_rect(
                &reference, &target, w, h, rx0, ry0, rw, rh, sdx, sdy, sr,
            );
            let want = exhaustive(
                &reference, &target, w, h, rx0, ry0, rw, rh, sdx, sdy, sr,
            );
            assert_eq!(got, want, "caso roi=({rx0},{ry0},{rw},{rh}) sr={sr}");
        }
        // Empates totales: imagen constante — debe ganar el PRIMER candidato.
        let got = find_best_match_sad_rect(&constant, &constant, w, h, 30, 30, 24, 20, 0, 0, 5);
        let want = exhaustive(&constant, &constant, w, h, 30, 30, 24, 20, 0, 0, 5);
        assert_eq!(got, want, "empates: la poda debe preservar el primer candidato");
        assert_eq!(got.0, 0);
        assert_eq!((got.1, got.2), (-5, -5));
    }

    #[test]
    fn test_subpixel_recovers_fractional_shift_without_pixel_locking() {
        // A smooth blob shifted +0.4 px in x via bilinear resampling. The
        // sub-pixel refinement must recover ~0.4, not collapse toward 0
        // (the classic pixel-locking bias of parabola fits on raw L1/SAD).
        let w = 64usize;
        let h = 64usize;
        let blob = |x: f32, y: f32| -> u16 {
            let dx = x - 32.0;
            let dy = y - 32.0;
            (30000.0 * (-(dx * dx + dy * dy) / 50.0).exp()) as u16
        };
        let mut reference = vec![0u16; w * h];
        let mut target = vec![0u16; w * h];
        for y in 0..h {
            for x in 0..w {
                reference[y * w + x] = blob(x as f32, y as f32);
                target[y * w + x] = blob(x as f32 - 0.4, y as f32);
            }
        }

        let (idx, idy, sad) = find_best_match_sad(&reference, &target, w, 32, 32, 32, 32, 16, 4);
        let (sdx, sdy) =
            subpixel_refine_sad(&reference, &target, w, 32, 32, 32, 32, 16, idx, idy, sad);
        let total_dx = idx + sdx;
        let total_dy = idy + sdy;
        assert!(
            (total_dx - 0.4).abs() < 0.15,
            "expected dx≈0.4, got {total_dx}"
        );
        assert!(total_dy.abs() < 0.15, "expected dy≈0.0, got {total_dy}");
    }

    #[test]
    fn test_lucas_kanade_polishes_subpixel_shift() {
        // Textura 2-D suave desplazada (+0.33, −0.21) px via resampleo
        // bilineal, con la convencion del pipeline: Target(x + s) = Master(x).
        // El pulido LK partiendo de (0,0) debe recuperar el shift con error
        // < 0.05 px — mas fino que el ~0.1-0.2 px del fit equiangular.
        let w = 96usize;
        let h = 96usize;
        let pat = |x: f32, y: f32| -> u16 {
            let v = (x * 0.35).sin() * (y * 0.27).cos()
                + (x * 0.11).cos() * (y * 0.17).sin() * 0.7;
            (12000.0 + 6000.0 * v) as u16
        };
        let (tdx, tdy) = (0.33f32, -0.21f32);
        let mut master = vec![0u16; w * h];
        let mut target = vec![0u16; w * h];
        for y in 0..h {
            for x in 0..w {
                master[y * w + x] = pat(x as f32, y as f32);
                // Target(u) = Master(u − s)  ⇔  Target(x + s) = Master(x)
                target[y * w + x] = pat(x as f32 - tdx, y as f32 - tdy);
            }
        }

        let refined =
            refine_shift_lucas_kanade(&master, &target, w, h, 48, 48, 0.0, 0.0, 32, 3)
                .expect("patch con textura 2-D debe refinar");
        assert!(
            (refined.0 - tdx).abs() < 0.05,
            "dx: esperado {tdx}, obtenido {}",
            refined.0
        );
        assert!(
            (refined.1 - tdy).abs() < 0.05,
            "dy: esperado {tdy}, obtenido {}",
            refined.1
        );

        // Patch plano: sin textura no hay sistema (Hessiano singular) — debe
        // devolver None para que el caller conserve el estimado SAD.
        let flat = vec![500u16; w * h];
        assert!(
            refine_shift_lucas_kanade(&flat, &flat, w, h, 48, 48, 0.0, 0.0, 32, 3).is_none()
        );
    }

    #[test]
    fn test_neon_vs_scalar_sad_matching() {
        let mut ref_data = vec![0u16; 1000];
        let mut tgt_data = vec![0u16; 1000];
        for i in 0..ref_data.len() {
            ref_data[i] = (i * 13) as u16;
            tgt_data[i] = (i * 17) as u16;
        }

        let width = 20;
        let height = 20;
        let roi_x = 4;
        let roi_y = 4;
        let roi_w = 12;
        let roi_h = 12;
        let start_dx = 0;
        let start_dy = 0;
        let search_range = 2;

        let (scalar_sad, scalar_dx, scalar_dy) = find_best_match_sad_internal(
            &ref_data,
            &tgt_data,
            width,
            height,
            roi_x,
            roi_y,
            roi_w,
            roi_h,
            start_dx,
            start_dy,
            search_range,
            u64::MAX,
        );

        #[cfg(target_arch = "aarch64")]
        let (neon_sad, neon_dx, neon_dy) = unsafe {
            find_best_match_sad_rect_neon(
                &ref_data,
                &tgt_data,
                width,
                height,
                roi_x,
                roi_y,
                roi_w,
                roi_h,
                start_dx,
                start_dy,
                search_range,
                u64::MAX,
            )
        };
        #[cfg(target_arch = "x86_64")]
        let (neon_sad, neon_dx, neon_dy) = (scalar_sad, scalar_dx, scalar_dy);

        assert_eq!(scalar_sad, neon_sad);
        assert_eq!(scalar_dx, neon_dx);
        assert_eq!(scalar_dy, neon_dy);

        // Test find_best_match_sad
        let box_size = 8;
        let search_r = 2;
        let (scalar_s_dx, scalar_s_dy, scalar_s_sad) =
            find_best_match_sad_scalar(&ref_data, &tgt_data, width, 4, 4, 4, 4, box_size, search_r);

        #[cfg(target_arch = "aarch64")]
        let (neon_s_dx, neon_s_dy, neon_s_sad) = unsafe {
            find_best_match_sad_neon(&ref_data, &tgt_data, width, 4, 4, 4, 4, box_size, search_r)
        };
        #[cfg(target_arch = "x86_64")]
        let (neon_s_dx, neon_s_dy, neon_s_sad) = (scalar_s_dx, scalar_s_dy, scalar_s_sad);

        assert_eq!(scalar_s_sad, neon_s_sad);
        assert_eq!(scalar_s_dx, neon_s_dx);
        assert_eq!(scalar_s_dy, neon_s_dy);
    }
}
