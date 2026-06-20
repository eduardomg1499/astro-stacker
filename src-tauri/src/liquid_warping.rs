use crate::smart_grid::ApPoint;
use rayon::prelude::*;
use std::sync::OnceLock;

/// Pre-computes the Warp Map for IDW (Inverse Distance Weighting).
/// Returns (warp_indices, warp_weights).
pub fn compute_idw_map(
    width: usize,
    height: usize,
    custom_points: &[ApPoint],
) -> (Vec<u16>, Vec<f32>) {
    compute_idw_map_with_power(width, height, custom_points, 1.5)
}

/// Pre-computes the Warp Map for IDW with configurable power.
/// `idw_power` controls the interpolation sharpness:
///   - 1.5 (d^3): default, smooth transitions between APs
///   - 2.0 (d^4): sharper cells, better local correction for planets
///   - 1.25 (d^2.5): softer blend for surface/lunar targets
pub fn compute_idw_map_with_power(
    width: usize,
    height: usize,
    custom_points: &[ApPoint],
    idw_power: f32,
) -> (Vec<u16>, Vec<f32>) {
    compute_idw_map_for_output(width, height, 1.0, 0.0, 0.0, custom_points, idw_power, 8)
}

/// Pre-computes an IDW warp map for the actual output raster.
/// Output pixels are mapped back into reference/input coordinates before AP
/// distances are evaluated, which keeps liquid warping correct with ROI/drizzle.
///
/// `top_k`: number of nearest APs blended per pixel (≤8). Smaller K makes the
/// warp field MORE LOCAL (follows seeing cells more tightly → sharper surface
/// texture); larger K smooths it (safer for sparse planetary grids).
pub fn compute_idw_map_for_output(
    width: usize,
    height: usize,
    drizzle: f32,
    roi_offset_x: f32,
    roi_offset_y: f32,
    custom_points: &[ApPoint],
    idw_power: f32,
    top_k: usize,
) -> (Vec<u16>, Vec<f32>) {
    // --- OPT 1+B: PRE-COMPUTE TOP-K IDW WARP MAP ---
    // Instead of ALL n_points weights per pixel, store only the K nearest APs.
    // For p≥3 IDW, distant AP weights are negligible (<0.001), so Top-K loses no quality.
    // Memory: pixels × K × 6 bytes ≈ 50MB for 2MP image (vs 1.6GB for 200 full APs).
    const WARP_K: usize = 8;
    let n_points = custom_points.len();

    if n_points == 0 {
        return (Vec::new(), Vec::new());
    }

    let effective_k = top_k.clamp(1, WARP_K).min(n_points); // Handle cases with fewer than K APs

    // Flat arrays: warp_indices[pixel × K + k] = AP index, warp_weights[pixel × K + k] = normalized weight
    let total_pixels = width * height;
    let mut indices = vec![0u16; total_pixels * effective_k];
    let mut weights = vec![0.0f32; total_pixels * effective_k];

    // PARALLEL IDW: Compute indices and weights using all cores (AVX2 auto-vectorization friendly)
    // We chunk the flat arrays to allow parallel mutation.
    let chunk_size = width; // Process one row per task (good balance)

    indices
        .par_chunks_mut(chunk_size * effective_k)
        .zip(weights.par_chunks_mut(chunk_size * effective_k))
        .enumerate()
        .for_each(|(chunk_idx, (idx_chunk, weight_chunk))| {
            // Each chunk corresponds to 'chunk_size' pixels (e.g. one row)

            let start_pixel = chunk_idx * chunk_size;

            // Iterate pixels in this chunk
            for i in 0..chunk_size {
                let pixel_idx = start_pixel + i;
                if pixel_idx >= total_pixels {
                    break;
                }

                let y = pixel_idx / width;
                let x = pixel_idx % width;

                let inv_drizzle = 1.0 / drizzle.max(0.0001);
                let px = (x as f32 * inv_drizzle) + roi_offset_x;
                let py = (y as f32 * inv_drizzle) + roi_offset_y;

                // Base offset within the CHUNK
                let base_in_chunk = i * effective_k;

                // --- O(1) Top-K INSERTION SORT ---
                // We maintain a fixed array of the K closest points. No heap allocations per pixel.
                let mut top_k = [(f32::MAX, 0u16); WARP_K];

                for (kp_i, kp) in custom_points.iter().enumerate() {
                    let dx = px - kp.x;
                    let dy = py - kp.y;
                    let dist_sq = dx * dx + dy * dy;

                    // If this point is closer than our FURTHEST point in the Top K list...
                    if dist_sq < top_k[effective_k - 1].0 {
                        // Find where to insert it (O(K), which is tiny, max 8)
                        let mut insert_idx = effective_k - 1;
                        while insert_idx > 0 && dist_sq < top_k[insert_idx - 1].0 {
                            top_k[insert_idx] = top_k[insert_idx - 1];
                            insert_idx -= 1;
                        }
                        top_k[insert_idx] = (dist_sq, kp_i as u16);
                    }
                }

                // 3. Compute Weights
                let mut total_w = 0.0f32;
                for k in 0..effective_k {
                    let d2 = top_k[k].0.max(0.1);
                    // IDW con p=3 (d^2.powf(1.5) = d^3): balance entre rigidez local y
                    // transiciones suaves entre APs. p=10 anterior creaba celdas Voronoi
                    // extremadamente duras que causaban manchas elípticas visibles.
                    // p=3 produce campos de warping suaves compatibles con AutoStakkert.
                    let w = 1.0 / (d2.powf(idw_power));

                    // Write to output buffers
                    idx_chunk[base_in_chunk + k] = top_k[k].1;
                    weight_chunk[base_in_chunk + k] = w;
                    total_w += w;
                }

                // 4. Normalize
                if total_w > 0.0 {
                    let inv = 1.0 / total_w;
                    for k in 0..effective_k {
                        weight_chunk[base_in_chunk + k] *= inv;
                    }
                }
            }
        });

    (indices, weights)
}

/// Accumulates a single frame into the stack using Liquid Warping (Multipoint).
/// Uses the pre-computed IDW map and local shifts.
/// Optimized with AVX2-friendly loop structure.
#[allow(clippy::too_many_arguments)] // Wrapper function, many args needed
/// PHASE 11: Lanczos-3 Kernel
/// Standard of excellence for preserving astronomical edges without blur.
struct LanczosLUT {
    table: Vec<f32>,
    scale: f32,
    max_val: f32,
}

impl LanczosLUT {
    fn new(resolution: usize, max_val: f32) -> Self {
        let mut table = Vec::with_capacity(resolution);
        let scale = (resolution as f32 - 1.0) / max_val;

        for i in 0..resolution {
            let x = (i as f32) / scale;
            table.push(Self::calc_lanczos_3(x));
        }

        Self {
            table,
            scale,
            max_val,
        }
    }

    #[inline(always)]
    fn calc_lanczos_3(x: f32) -> f32 {
        let ax = x.abs();
        if ax < 0.0001 {
            return 1.0;
        }
        if ax >= 3.0 {
            return 0.0;
        }
        let pin_x = ax * std::f32::consts::PI;
        let sinc_x = pin_x.sin() / pin_x;
        let pin_x_3 = pin_x / 3.0;
        let sinc_x_3 = pin_x_3.sin() / pin_x_3;
        sinc_x * sinc_x_3
    }

    #[inline(always)]
    fn get(&self, x: f32, drop_size: f32) -> f32 {
        let ax = (x / drop_size).abs();
        if ax >= self.max_val {
            return 0.0;
        }
        let idx = (ax * self.scale) as usize;
        *self.table.get(idx).unwrap_or(&0.0)
    }
}

static LANCZOS_LUT: OnceLock<LanczosLUT> = OnceLock::new();

pub fn accumulate_frame_liquid(
    acc_r: &mut [f32],
    acc_g: &mut [f32],
    acc_b: &mut [f32],
    acc_w: &mut [f32],
    rgb_buf: &[u16],
    w_in: usize,
    h_in: usize,
    w_out: usize,
    h_out: usize,
    drizzle: f32,
    roi_offset_x: f32,
    roi_offset_y: f32,
    global_dx: f32,
    global_dy: f32,
    local_shifts: &[(f32, f32, f32)], // (dx, dy, quality)
    warp_indices: &[u16],
    warp_weights: &[f32],
    ap_acceptance: &[bool],
    ap_weights: &[f32],
    _custom_points: &[ApPoint],
    global_fallback_weight: f32,
    drop_size: f32,
) {
    let use_warp_map = !warp_weights.is_empty();
    let effective_k = if use_warp_map {
        warp_indices.len() / (w_out * h_out)
    } else {
        0
    };

    let lut = LANCZOS_LUT.get_or_init(|| LanczosLUT::new(30000, 3.0));
    let inv_drizzle = 1.0 / drizzle;

    for y_out in 0..h_out {
        let row_off = y_out * w_out;
        for x_out in 0..w_out {
            let ref_x = (x_out as f32 * inv_drizzle) + roi_offset_x;
            let ref_y = (y_out as f32 * inv_drizzle) + roi_offset_y;

            let pixel_weight;
            let mut sx_in = ref_x + global_dx;
            let mut sy_in = ref_y + global_dy;

            if use_warp_map {
                let p_idx = (y_out * w_out + x_out) * effective_k;
                let mut dx_sum = 0.0f32;
                let mut dy_sum = 0.0f32;
                let mut total_w = 0.0f32;

                for k in 0..effective_k {
                    let ap_idx = warp_indices[p_idx + k] as usize;
                    let w_idw = warp_weights[p_idx + k];

                    if ap_idx < ap_acceptance.len() && !ap_acceptance[ap_idx] {
                        continue;
                    }

                    let ap_w = if ap_idx < ap_weights.len() {
                        ap_weights[ap_idx]
                    } else {
                        1.0
                    };
                    let combined_w = w_idw * ap_w;

                    let (dx, dy, q) = local_shifts[ap_idx];
                    // Quality-weighted: APs with poor alignment (limb/background) have less
                    // influence on the warp field. This prevents turbulent/wavy edges on
                    // planetary disks where limb APs align poorly due to low contrast.
                    // q == 0 means NO measurement (out of bounds / invalid): its fake
                    // (0,0) shift must not drag the warp toward "no correction".
                    if q <= 0.0 {
                        continue;
                    }
                    let q_factor = q.clamp(0.05, 1.0);
                    let final_w = combined_w * q_factor;
                    dx_sum += dx * final_w;
                    dy_sum += dy * final_w;
                    total_w += final_w;
                }

                if total_w > 0.001 {
                    sx_in += dx_sum / total_w;
                    sy_in += dy_sum / total_w;
                    pixel_weight = total_w;
                } else if global_fallback_weight > 0.001 {
                    // Fallback to global alignment if all local APs are rejected
                    sx_in = ref_x + global_dx;
                    sy_in = ref_y + global_dy;
                    pixel_weight = global_fallback_weight;
                } else {
                    continue;
                }
            } else {
                pixel_weight = if ap_weights.is_empty() {
                    1.0
                } else {
                    ap_weights[0]
                };
            }

            if pixel_weight < 0.001 {
                continue;
            }

            // 2. Prepare pixel sampling closure
            let sample_pixel = |sx_in: f32, sy_in: f32| -> Option<(f32, f32, f32)> {
                let sx_floor = sx_in.floor();
                let sy_floor = sy_in.floor();
                let fx = sx_in - sx_floor;
                let fy = sy_in - sy_floor;
                let sx0 = sx_floor as isize;
                let sy0 = sy_floor as isize;

                let mut w_xs = [0.0f32; 6];
                for (i, kx) in (-2..=3).enumerate() {
                    w_xs[i] = lut.get(fx - kx as f32, drop_size);
                }

                let mut min_r = 65535.0f32;
                let mut min_g = 65535.0f32;
                let mut min_b = 65535.0f32;
                let mut max_r = 0.0f32;
                let mut max_g = 0.0f32;
                let mut max_b = 0.0f32;
                let mut sum_r = 0.0f32;
                let mut sum_g = 0.0f32;
                let mut sum_b = 0.0f32;
                let mut sum_w = 0.0f32;

                if sx0 >= 2 && sx0 + 3 < w_in as isize && sy0 >= 2 && sy0 + 3 < h_in as isize {
                    // FAST PATH (Interior Pixels)
                    for ky in -2..=3 {
                        let wy = lut.get(fy - ky as f32, drop_size);
                        if wy.abs() < 0.001 {
                            continue;
                        }
                        let src_row = (sy0 + ky) as usize * w_in;
                        for (i, kx) in (-2..=3).enumerate() {
                            let w_final = w_xs[i] * wy;
                            let off = (src_row + (sx0 + kx) as usize) * 3;
                            let pr = rgb_buf[off] as f32;
                            let pg = rgb_buf[off + 1] as f32;
                            let pb = rgb_buf[off + 2] as f32;
                            if pr < min_r {
                                min_r = pr;
                            }
                            if pg < min_g {
                                min_g = pg;
                            }
                            if pb < min_b {
                                min_b = pb;
                            }
                            if pr > max_r {
                                max_r = pr;
                            }
                            if pg > max_g {
                                max_g = pg;
                            }
                            if pb > max_b {
                                max_b = pb;
                            }
                            sum_r += pr * w_final;
                            sum_g += pg * w_final;
                            sum_b += pb * w_final;
                            sum_w += w_final;
                        }
                    }
                } else {
                    // SLOW PATH (Boundary clipping)
                    for ky in -2..=3 {
                        let py = sy0 + ky;
                        if py < 0 || py >= h_in as isize {
                            continue;
                        }
                        let wy = lut.get(fy - ky as f32, drop_size);
                        if wy.abs() < 0.001 {
                            continue;
                        }
                        let src_row = py as usize * w_in;
                        for (i, kx) in (-2..=3).enumerate() {
                            let px = sx0 + kx;
                            if px < 0 || px >= w_in as isize {
                                continue;
                            }
                            let w_final = w_xs[i] * wy;
                            let off = (src_row + px as usize) * 3;
                            let pr = rgb_buf[off] as f32;
                            let pg = rgb_buf[off + 1] as f32;
                            let pb = rgb_buf[off + 2] as f32;
                            if pr < min_r {
                                min_r = pr;
                            }
                            if pg < min_g {
                                min_g = pg;
                            }
                            if pb < min_b {
                                min_b = pb;
                            }
                            if pr > max_r {
                                max_r = pr;
                            }
                            if pg > max_g {
                                max_g = pg;
                            }
                            if pb > max_b {
                                max_b = pb;
                            }
                            sum_r += pr * w_final;
                            sum_g += pg * w_final;
                            sum_b += pb * w_final;
                            sum_w += w_final;
                        }
                    }
                }

                // SYMMETRIC RANGE-BASED ANTI-RINGING CLAMP:
                // The old lower-only clamp (min·factor) brightened dark
                // micro-structure systematically (weak filaments, dark lanes)
                // and its strength scaled with absolute brightness instead of
                // local contrast. Allow a fixed fraction of the LOCAL RANGE as
                // under/overshoot on BOTH sides — scale-invariant and unbiased.
                if sum_w.abs() > 0.00001 {
                    let band_r = (max_r - min_r) * 0.18 + 32.0;
                    let band_g = (max_g - min_g) * 0.18 + 32.0;
                    let band_b = (max_b - min_b) * 0.18 + 32.0;
                    Some((
                        (sum_r / sum_w)
                            .clamp((min_r - band_r).max(0.0), (max_r + band_r).min(65535.0)),
                        (sum_g / sum_w)
                            .clamp((min_g - band_g).max(0.0), (max_g + band_g).min(65535.0)),
                        (sum_b / sum_w)
                            .clamp((min_b - band_b).max(0.0), (max_b + band_b).min(65535.0)),
                    ))
                } else {
                    None
                }
            };

            if let Some((pr, pg, pb)) = sample_pixel(sx_in, sy_in) {
                let tidx = row_off + x_out;
                unsafe {
                    *acc_r.get_unchecked_mut(tidx) += pr * pixel_weight;
                    *acc_g.get_unchecked_mut(tidx) += pg * pixel_weight;
                    *acc_b.get_unchecked_mut(tidx) += pb * pixel_weight;
                    *acc_w.get_unchecked_mut(tidx) += pixel_weight;
                }
            }
        }
    }
}

/// Mono variant of `accumulate_frame_liquid`: identical warp/IDW logic but
/// samples a single channel. Avoids the historical mono→RGB triplication
/// (3× RAM and 3× sampling cost for grayscale cameras).
#[allow(clippy::too_many_arguments)]
pub fn accumulate_frame_liquid_mono(
    acc: &mut [f32],
    acc_w: &mut [f32],
    mono_buf: &[u16],
    w_in: usize,
    h_in: usize,
    w_out: usize,
    h_out: usize,
    drizzle: f32,
    roi_offset_x: f32,
    roi_offset_y: f32,
    global_dx: f32,
    global_dy: f32,
    local_shifts: &[(f32, f32, f32)], // (dx, dy, quality)
    warp_indices: &[u16],
    warp_weights: &[f32],
    ap_acceptance: &[bool],
    ap_weights: &[f32],
    global_fallback_weight: f32,
    drop_size: f32,
) {
    let use_warp_map = !warp_weights.is_empty();
    let effective_k = if use_warp_map {
        warp_indices.len() / (w_out * h_out)
    } else {
        0
    };

    let lut = LANCZOS_LUT.get_or_init(|| LanczosLUT::new(30000, 3.0));
    let inv_drizzle = 1.0 / drizzle;

    for y_out in 0..h_out {
        let row_off = y_out * w_out;
        for x_out in 0..w_out {
            let ref_x = (x_out as f32 * inv_drizzle) + roi_offset_x;
            let ref_y = (y_out as f32 * inv_drizzle) + roi_offset_y;

            let pixel_weight;
            let mut sx_in = ref_x + global_dx;
            let mut sy_in = ref_y + global_dy;

            if use_warp_map {
                let p_idx = (row_off + x_out) * effective_k;
                let mut dx_sum = 0.0f32;
                let mut dy_sum = 0.0f32;
                let mut total_w = 0.0f32;

                for k in 0..effective_k {
                    let ap_idx = warp_indices[p_idx + k] as usize;
                    let w_idw = warp_weights[p_idx + k];

                    if ap_idx < ap_acceptance.len() && !ap_acceptance[ap_idx] {
                        continue;
                    }

                    let ap_w = if ap_idx < ap_weights.len() {
                        ap_weights[ap_idx]
                    } else {
                        1.0
                    };
                    let combined_w = w_idw * ap_w;

                    let (dx, dy, q) = local_shifts[ap_idx];
                    // q == 0 means NO measurement (out of bounds / invalid): its
                    // fake (0,0) shift must not drag the warp toward "no correction".
                    if q <= 0.0 {
                        continue;
                    }
                    let q_factor = q.clamp(0.05, 1.0);
                    let final_w = combined_w * q_factor;
                    dx_sum += dx * final_w;
                    dy_sum += dy * final_w;
                    total_w += final_w;
                }

                if total_w > 0.001 {
                    sx_in += dx_sum / total_w;
                    sy_in += dy_sum / total_w;
                    pixel_weight = total_w;
                } else if global_fallback_weight > 0.001 {
                    sx_in = ref_x + global_dx;
                    sy_in = ref_y + global_dy;
                    pixel_weight = global_fallback_weight;
                } else {
                    continue;
                }
            } else {
                pixel_weight = if ap_weights.is_empty() {
                    1.0
                } else {
                    ap_weights[0]
                };
            }

            if pixel_weight < 0.001 {
                continue;
            }

            // Lanczos-3 sampling (single channel)
            let sx_floor = sx_in.floor();
            let sy_floor = sy_in.floor();
            let fx = sx_in - sx_floor;
            let fy = sy_in - sy_floor;
            let sx0 = sx_floor as isize;
            let sy0 = sy_floor as isize;

            let mut w_xs = [0.0f32; 6];
            for (i, kx) in (-2..=3).enumerate() {
                w_xs[i] = lut.get(fx - kx as f32, drop_size);
            }

            let mut min_v = 65535.0f32;
            let mut max_v = 0.0f32;
            let mut sum_v = 0.0f32;
            let mut sum_w = 0.0f32;

            if sx0 >= 2 && sx0 + 3 < w_in as isize && sy0 >= 2 && sy0 + 3 < h_in as isize {
                // FAST PATH (Interior Pixels)
                for ky in -2..=3 {
                    let wy = lut.get(fy - ky as f32, drop_size);
                    if wy.abs() < 0.001 {
                        continue;
                    }
                    let src_row = (sy0 + ky) as usize * w_in;
                    for (i, kx) in (-2..=3).enumerate() {
                        let w_final = w_xs[i] * wy;
                        let pv = mono_buf[src_row + (sx0 + kx) as usize] as f32;
                        if pv < min_v {
                            min_v = pv;
                        }
                        if pv > max_v {
                            max_v = pv;
                        }
                        sum_v += pv * w_final;
                        sum_w += w_final;
                    }
                }
            } else {
                // SLOW PATH (Boundary clipping)
                for ky in -2..=3 {
                    let py = sy0 + ky;
                    if py < 0 || py >= h_in as isize {
                        continue;
                    }
                    let wy = lut.get(fy - ky as f32, drop_size);
                    if wy.abs() < 0.001 {
                        continue;
                    }
                    let src_row = py as usize * w_in;
                    for (i, kx) in (-2..=3).enumerate() {
                        let px = sx0 + kx;
                        if px < 0 || px >= w_in as isize {
                            continue;
                        }
                        let w_final = w_xs[i] * wy;
                        let pv = mono_buf[src_row + px as usize] as f32;
                        if pv < min_v {
                            min_v = pv;
                        }
                        if pv > max_v {
                            max_v = pv;
                        }
                        sum_v += pv * w_final;
                        sum_w += w_final;
                    }
                }
            }

            if sum_w.abs() > 0.00001 {
                // Symmetric range-based anti-ringing clamp (same policy as RGB):
                // unbiased on dark structure, scale-invariant.
                let band = (max_v - min_v) * 0.18 + 32.0;
                let val =
                    (sum_v / sum_w).clamp((min_v - band).max(0.0), (max_v + band).min(65535.0));
                let tidx = row_off + x_out;
                unsafe {
                    *acc.get_unchecked_mut(tidx) += val * pixel_weight;
                    *acc_w.get_unchecked_mut(tidx) += pixel_weight;
                }
            }
        }
    }
}

/// Accumulates a single frame into the stack using Global (Rigid) alignment.
/// This is used when Multipoint (Liquid Warping) is disabled for the batch.
pub fn accumulate_frame_global_rgb(
    acc_r: &mut [f32],
    acc_g: &mut [f32],
    acc_b: &mut [f32],
    acc_w: &mut [f32],
    rgb_buf: &[u16],
    w_in: usize,
    h_in: usize,
    w_out: usize,
    h_out: usize,
    drizzle: f32,
    dx: f32,
    dy: f32,
    base_weight: f32,
    _sigma_scale: f32, // NEW: 1.0=normal, >1.0=more permissive (for surface/lunar)
    drop_size: f32,
) {
    if base_weight <= 0.0 {
        return;
    }

    let lut = LANCZOS_LUT.get_or_init(|| LanczosLUT::new(30000, 3.0));
    let inv_drizzle = 1.0 / drizzle;

    // BACKWARD WARPING: Iterate over OUTPUT pixels
    for y_out in 0..h_out {
        let row_off = y_out * w_out;

        for x_out in 0..w_out {
            // Find EXACT corresponding source coordinate in input image
            let center_offset = 0.5 * (inv_drizzle - 1.0);
            let sx_in = (x_out as f32 * inv_drizzle) + dx + center_offset;
            let sy_in = (y_out as f32 * inv_drizzle) + dy + center_offset;

            // Nearest integer coordinate
            let sx_floor = sx_in.floor();
            let sy_floor = sy_in.floor();
            let fx = sx_in - sx_floor;
            let fy = sy_in - sy_floor;
            let sx0 = sx_floor as isize;
            let sy0 = sy_floor as isize;

            // We sample a 6x6 pixel window around (sx0, sy0) in the input image. (Lanczos-3)
            let mut w_xs = [0.0f32; 6];
            for (i, kx) in (-2..=3).enumerate() {
                w_xs[i] = lut.get(fx - kx as f32, drop_size);
            }

            // If the window is perfectly inside the input image boundaries, use fast path
            if sx0 >= 2 && sx0 + 3 < w_in as isize && sy0 >= 2 && sy0 + 3 < h_in as isize {
                let mut min_r = 65535.0f32;
                let mut min_g = 65535.0f32;
                let mut min_b = 65535.0f32;
                let mut max_r = 0.0f32;
                let mut max_g = 0.0f32;
                let mut max_b = 0.0f32;
                let mut sum_r = 0.0f32;
                let mut sum_g = 0.0f32;
                let mut sum_b = 0.0f32;
                let mut sum_w = 0.0f32;

                for ky in -2..=3 {
                    let wy = lut.get(fy - ky as f32, drop_size);
                    if wy.abs() < 0.001 {
                        continue;
                    }
                    let py = (sy0 + ky) as usize;
                    let src_row = py * w_in;

                    for (i, kx) in (-2..=3).enumerate() {
                        let px = (sx0 + kx) as usize;
                        let w_final = w_xs[i] * wy;

                        let off = (src_row + px) * 3;
                        let pr = rgb_buf[off] as f32;
                        let pg = rgb_buf[off + 1] as f32;
                        let pb = rgb_buf[off + 2] as f32;

                        if pr < min_r {
                            min_r = pr;
                        }
                        if pg < min_g {
                            min_g = pg;
                        }
                        if pb < min_b {
                            min_b = pb;
                        }
                        if pr > max_r {
                            max_r = pr;
                        }
                        if pg > max_g {
                            max_g = pg;
                        }
                        if pb > max_b {
                            max_b = pb;
                        }

                        sum_r += pr * w_final;
                        sum_g += pg * w_final;
                        sum_b += pb * w_final;
                        sum_w += w_final;
                    }
                }

                // ADAPTIVE ANTI-RINGING: Relax clamp in high-contrast textured areas
                if sum_w.abs() > 0.00001 {
                    let tidx = row_off + x_out;
                    let inv_sum = 1.0 / sum_w;
                    let range_r = max_r - min_r;
                    let range_g = max_g - min_g;
                    let range_b = max_b - min_b;
                    let clamp_r = if range_r > 3000.0 {
                        0.80
                    } else if range_r > 1000.0 {
                        0.88
                    } else {
                        0.95
                    };
                    let clamp_g = if range_g > 3000.0 {
                        0.80
                    } else if range_g > 1000.0 {
                        0.88
                    } else {
                        0.95
                    };
                    let clamp_b = if range_b > 3000.0 {
                        0.80
                    } else if range_b > 1000.0 {
                        0.88
                    } else {
                        0.95
                    };
                    let px_r = (sum_r * inv_sum).clamp(min_r * clamp_r, 65535.0);
                    let px_g = (sum_g * inv_sum).clamp(min_g * clamp_g, 65535.0);
                    let px_b = (sum_b * inv_sum).clamp(min_b * clamp_b, 65535.0);
                    let eff_w = base_weight;
                    unsafe {
                        *acc_r.get_unchecked_mut(tidx) += px_r * eff_w;
                        *acc_g.get_unchecked_mut(tidx) += px_g * eff_w;
                        *acc_b.get_unchecked_mut(tidx) += px_b * eff_w;
                        *acc_w.get_unchecked_mut(tidx) += eff_w;
                    }
                }
            } else {
                // SLOW PATH (Edges clipping)
                let mut sum_r = 0.0f32;
                let mut sum_g = 0.0f32;
                let mut sum_b = 0.0f32;
                let mut sum_w = 0.0f32;

                for ky in -2..=3 {
                    let py = sy0 + ky;
                    if py < 0 || py >= h_in as isize {
                        continue;
                    }

                    let wy = lut.get(fy - ky as f32, drop_size);
                    if wy.abs() < 0.001 {
                        continue;
                    }
                    let src_row = py as usize * w_in;

                    for (i, kx) in (-2..=3).enumerate() {
                        let px = sx0 + kx;
                        if px < 0 || px >= w_in as isize {
                            continue;
                        }

                        let w_final = w_xs[i] * wy;
                        let off = (src_row + px as usize) * 3;

                        sum_r += rgb_buf[off] as f32 * w_final;
                        sum_g += rgb_buf[off + 1] as f32 * w_final;
                        sum_b += rgb_buf[off + 2] as f32 * w_final;
                        sum_w += w_final;
                    }
                }

                if sum_w.abs() > 0.00001 {
                    let tidx = row_off + x_out;
                    let inv_sum = 1.0 / sum_w;
                    let px_r = (sum_r * inv_sum).clamp(0.0, 65535.0);
                    let px_g = (sum_g * inv_sum).clamp(0.0, 65535.0);
                    let px_b = (sum_b * inv_sum).clamp(0.0, 65535.0);
                    let eff_w = base_weight;
                    unsafe {
                        *acc_r.get_unchecked_mut(tidx) += px_r * eff_w;
                        *acc_g.get_unchecked_mut(tidx) += px_g * eff_w;
                        *acc_b.get_unchecked_mut(tidx) += px_b * eff_w;
                        *acc_w.get_unchecked_mut(tidx) += eff_w;
                    }
                }
            }
        }
    }
}

// ============================================================
// SMALL PLANET V3: Rigid Bilinear Accumulator
// ============================================================
/// Accumulates a single frame using ONLY a rigid global shift + 2×2 bilinear sampling.
///
/// Designed exclusively for `planet_small + V3` where:
///   - The planetary disk is small (50–200px) relative to the frame
///   - IDW warping produces ghosting because AP vectors differ slightly
///     across a tiny disk, splitting the disk between sub-pixel positions
///   - Lanczos-3 negative lobes soften the limb of a small bright disk
///
/// Bilinear advantages for small planets:
///   - ZERO negative lobes → limb stays crisp, no brightness bleeding
///   - Only 2×2 support → much less cross-pixel contamination
///   - Deterministic output: same pixels land at same output coordinates
///   - No IDW: single global (dx,dy) vector, zero ghosting
///
/// `quality_weight`: frame quality weight in [0, 1]. For lucky imaging,
/// low-quality frames get a lower weight so they contribute less to the stack.
pub fn accumulate_frame_rigid_lanczos(
    acc_r: &mut [f32],
    acc_g: &mut [f32],
    acc_b: &mut [f32],
    acc_w: &mut [f32],
    rgb_buf: &[u16],
    w_in: usize,
    h_in: usize,
    w_out: usize,
    h_out: usize,
    drizzle: f32,
    roi_offset_x: f32,
    roi_offset_y: f32,
    dx: f32, // global rigid X shift (render_dx)
    dy: f32, // global rigid Y shift (render_dy)
    quality_weight: f32,
    drop_size: f32,
) {
    if quality_weight <= 0.0 {
        return;
    }

    let lut = LANCZOS_LUT.get_or_init(|| LanczosLUT::new(30000, 3.0));
    let inv_drizzle = 1.0 / drizzle;
    let center_offset = 0.5 * (inv_drizzle - 1.0);

    for y_out in 0..h_out {
        let row_off = y_out * w_out;

        for x_out in 0..w_out {
            let sx = (x_out as f32 * inv_drizzle) + roi_offset_x + dx + center_offset;
            let sy = (y_out as f32 * inv_drizzle) + roi_offset_y + dy + center_offset;

            let sx_floor = sx.floor();
            let sy_floor = sy.floor();
            let fx = sx - sx_floor;
            let fy = sy - sy_floor;
            let sx0 = sx_floor as isize;
            let sy0 = sy_floor as isize;

            let mut w_xs = [0.0f32; 6];
            for (i, kx) in (-2..=3).enumerate() {
                w_xs[i] = lut.get(fx - kx as f32, drop_size);
            }

            if sx0 >= 2 && sx0 + 3 < w_in as isize && sy0 >= 2 && sy0 + 3 < h_in as isize {
                let mut sum_r = 0.0f32;
                let mut sum_g = 0.0f32;
                let mut sum_b = 0.0f32;
                let mut sum_w = 0.0f32;
                let mut min_r = 65535.0f32;
                let mut min_g = 65535.0f32;
                let mut min_b = 65535.0f32;
                let mut max_r = 0.0f32;
                let mut max_g = 0.0f32;
                let mut max_b = 0.0f32;

                for ky in -2..=3 {
                    let wy = lut.get(fy - ky as f32, drop_size);
                    if wy.abs() < 0.001 {
                        continue;
                    }
                    let py = (sy0 + ky) as usize;
                    let src_row = py * w_in;

                    for (i, kx) in (-2..=3).enumerate() {
                        let px = (sx0 + kx) as usize;
                        let w_final = w_xs[i] * wy;

                        let off = (src_row + px) * 3;
                        let pr = rgb_buf[off] as f32;
                        let pg = rgb_buf[off + 1] as f32;
                        let pb = rgb_buf[off + 2] as f32;

                        if pr < min_r {
                            min_r = pr;
                        }
                        if pg < min_g {
                            min_g = pg;
                        }
                        if pb < min_b {
                            min_b = pb;
                        }
                        if pr > max_r {
                            max_r = pr;
                        }
                        if pg > max_g {
                            max_g = pg;
                        }
                        if pb > max_b {
                            max_b = pb;
                        }

                        sum_r += pr * w_final;
                        sum_g += pg * w_final;
                        sum_b += pb * w_final;
                        sum_w += w_final;
                    }
                }

                if sum_w.abs() > 0.00001 {
                    let tidx = row_off + x_out;
                    let inv_sum = 1.0 / sum_w;
                    // ADAPTIVE ANTI-RINGING: preserve micro-contrast in high-contrast areas
                    let range_r = max_r - min_r;
                    let range_g = max_g - min_g;
                    let range_b = max_b - min_b;
                    let clamp_r = if range_r > 3000.0 {
                        0.80
                    } else if range_r > 1000.0 {
                        0.88
                    } else {
                        0.95
                    };
                    let clamp_g = if range_g > 3000.0 {
                        0.80
                    } else if range_g > 1000.0 {
                        0.88
                    } else {
                        0.95
                    };
                    let clamp_b = if range_b > 3000.0 {
                        0.80
                    } else if range_b > 1000.0 {
                        0.88
                    } else {
                        0.95
                    };
                    let px_r = (sum_r * inv_sum).clamp(min_r * clamp_r, 65535.0);
                    let px_g = (sum_g * inv_sum).clamp(min_g * clamp_g, 65535.0);
                    let px_b = (sum_b * inv_sum).clamp(min_b * clamp_b, 65535.0);

                    acc_r[tidx] += px_r * quality_weight;
                    acc_g[tidx] += px_g * quality_weight;
                    acc_b[tidx] += px_b * quality_weight;
                    acc_w[tidx] += quality_weight;
                }
            }
        }
    }
}
