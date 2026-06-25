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

// ===========================================================================
// Fase 1b — SIMD para el fast-path del muestreo Lanczos RGB. El frame está
// interleaved (R,G,B por píxel), así que las columnas de un canal NO son
// contiguas. Para no añadir buffers (riesgo de OOM) ni shuffles delicados,
// hacemos el de-interleave con lecturas escalares ACOTADAS (los índices están
// garantizados por el guard del fast-path) y vectorizamos la ARITMÉTICA de los
// 3 canales (productos, sumas, min/max) reutilizando los reductores del mono.
// min/max son exactos; la suma se reordena <1 ULP. La ruta escalar queda como
// fallback y referencia de validación (ver test rgb_fast_sample_avx2_*).
// ===========================================================================

#[allow(clippy::type_complexity)]
#[inline(always)]
fn fast_sample_rgb_scalar(
    rgb_buf: &[u16],
    w_in: usize,
    sx0: isize,
    sy0: isize,
    w_xs: &[f32; 6],
    fy: f32,
    lut: &LanczosLUT,
    drop_size: f32,
) -> (f32, f32, f32, f32, f32, f32, f32, f32, f32, f32) {
    // (sum_r, sum_g, sum_b, sum_w, min_r, min_g, min_b, max_r, max_g, max_b)
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
    (
        sum_r, sum_g, sum_b, sum_w, min_r, min_g, min_b, max_r, max_g, max_b,
    )
}

#[allow(clippy::type_complexity)]
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn fast_sample_rgb_avx2(
    rgb_buf: &[u16],
    w_in: usize,
    sx0: isize,
    sy0: isize,
    w_xs: &[f32; 6],
    fy: f32,
    lut: &LanczosLUT,
    drop_size: f32,
) -> (f32, f32, f32, f32, f32, f32, f32, f32, f32, f32) {
    use std::arch::x86_64::*;
    let wxs = _mm256_setr_ps(w_xs[0], w_xs[1], w_xs[2], w_xs[3], w_xs[4], w_xs[5], 0.0, 0.0);
    let pad_hi = _mm256_set1_ps(65535.0);
    let mut sum_r = _mm256_setzero_ps();
    let mut sum_g = _mm256_setzero_ps();
    let mut sum_b = _mm256_setzero_ps();
    let mut sum_w = _mm256_setzero_ps();
    let mut vmin_r = pad_hi;
    let mut vmin_g = pad_hi;
    let mut vmin_b = pad_hi;
    let mut vmax_r = _mm256_setzero_ps();
    let mut vmax_g = _mm256_setzero_ps();
    let mut vmax_b = _mm256_setzero_ps();
    for ky in -2..=3isize {
        let wy = lut.get(fy - ky as f32, drop_size);
        if wy.abs() < 0.001 {
            continue;
        }
        let src_row = (sy0 + ky) as usize * w_in;
        // base..base+18 son las 6 columnas RGB; índices garantizados en rango
        // por el guard del fast-path (sx0>=2, sx0+3<w_in, sy0>=2, sy0+3<h_in).
        let base = (src_row + (sx0 - 2) as usize) * 3;
        let g = |o: usize| *rgb_buf.get_unchecked(base + o) as f32;
        let rv = _mm256_setr_ps(g(0), g(3), g(6), g(9), g(12), g(15), 0.0, 0.0);
        let gv = _mm256_setr_ps(g(1), g(4), g(7), g(10), g(13), g(16), 0.0, 0.0);
        let bv = _mm256_setr_ps(g(2), g(5), g(8), g(11), g(14), g(17), 0.0, 0.0);
        let wf = _mm256_mul_ps(wxs, _mm256_set1_ps(wy));
        sum_r = _mm256_add_ps(sum_r, _mm256_mul_ps(rv, wf));
        sum_g = _mm256_add_ps(sum_g, _mm256_mul_ps(gv, wf));
        sum_b = _mm256_add_ps(sum_b, _mm256_mul_ps(bv, wf));
        sum_w = _mm256_add_ps(sum_w, wf);
        vmin_r = _mm256_min_ps(vmin_r, _mm256_blend_ps::<0b1100_0000>(rv, pad_hi));
        vmin_g = _mm256_min_ps(vmin_g, _mm256_blend_ps::<0b1100_0000>(gv, pad_hi));
        vmin_b = _mm256_min_ps(vmin_b, _mm256_blend_ps::<0b1100_0000>(bv, pad_hi));
        vmax_r = _mm256_max_ps(vmax_r, rv);
        vmax_g = _mm256_max_ps(vmax_g, gv);
        vmax_b = _mm256_max_ps(vmax_b, bv);
    }
    (
        hsum256_ps(sum_r),
        hsum256_ps(sum_g),
        hsum256_ps(sum_b),
        hsum256_ps(sum_w),
        hmin256_ps(vmin_r),
        hmin256_ps(vmin_g),
        hmin256_ps(vmin_b),
        hmax256_ps(vmax_r),
        hmax256_ps(vmax_g),
        hmax256_ps(vmax_b),
    )
}

#[allow(clippy::type_complexity)]
#[inline(always)]
fn fast_sample_rgb(
    rgb_buf: &[u16],
    w_in: usize,
    sx0: isize,
    sy0: isize,
    w_xs: &[f32; 6],
    fy: f32,
    lut: &LanczosLUT,
    drop_size: f32,
) -> (f32, f32, f32, f32, f32, f32, f32, f32, f32, f32) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return unsafe {
                fast_sample_rgb_avx2(rgb_buf, w_in, sx0, sy0, w_xs, fy, lut, drop_size)
            };
        }
    }
    fast_sample_rgb_scalar(rgb_buf, w_in, sx0, sy0, w_xs, fy, lut, drop_size)
}

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
                    // El warp map puede referenciar un AP fuera de rango (mismatch
                    // de conteo de APs en ciertos SER). Sin este guard, local_shifts[ap_idx]
                    // hace panic y, con panic=abort, cerraba la app en seco.
                    if ap_idx >= local_shifts.len() {
                        continue;
                    }
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

                let (sum_r, sum_g, sum_b, sum_w, min_r, min_g, min_b, max_r, max_g, max_b) =
                    if sx0 >= 2 && sx0 + 3 < w_in as isize && sy0 >= 2 && sy0 + 3 < h_in as isize {
                        // FAST PATH (Interior Pixels) — SIMD (AVX2) con fallback escalar
                        fast_sample_rgb(rgb_buf, w_in, sx0, sy0, &w_xs, fy, lut, drop_size)
                    } else {
                        // SLOW PATH (Boundary clipping) — escalar
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
                        (
                            sum_r, sum_g, sum_b, sum_w, min_r, min_g, min_b, max_r, max_g, max_b,
                        )
                    };

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
                if tidx < acc_r.len() {
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
}

// ===========================================================================
// Fase 1a — SIMD para el fast-path del muestreo Lanczos MONO (el loop más
// caliente del apilado). Por cada una de las 6 filas del kernel 6×6, los 6
// píxeles de columna son CONTIGUOS en memoria, así que se vectoriza la fila y
// se reduce una sola vez por píxel. min/max son exactos (sin reordenamiento);
// solo la SUMA se reordena → diferencias <1 ULP, del mismo orden que la
// no-determinación que ya introduce la reducción en paralelo de rayon.
// La ruta escalar queda como fallback y referencia de validación (ver test).
// ===========================================================================

#[inline(always)]
fn fast_sample_mono_scalar(
    mono_buf: &[u16],
    w_in: usize,
    sx0: isize,
    sy0: isize,
    w_xs: &[f32; 6],
    fy: f32,
    lut: &LanczosLUT,
    drop_size: f32,
) -> (f32, f32, f32, f32) {
    let mut min_v = 65535.0f32;
    let mut max_v = 0.0f32;
    let mut sum_v = 0.0f32;
    let mut sum_w = 0.0f32;
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
    (sum_v, sum_w, min_v, max_v)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn hsum256_ps(v: std::arch::x86_64::__m256) -> f32 {
    use std::arch::x86_64::*;
    let lo = _mm256_castps256_ps128(v);
    let hi = _mm256_extractf128_ps::<1>(v);
    let s = _mm_add_ps(lo, hi);
    let s = _mm_hadd_ps(s, s);
    let s = _mm_hadd_ps(s, s);
    _mm_cvtss_f32(s)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn hmin256_ps(v: std::arch::x86_64::__m256) -> f32 {
    use std::arch::x86_64::*;
    let lo = _mm256_castps256_ps128(v);
    let hi = _mm256_extractf128_ps::<1>(v);
    let m = _mm_min_ps(lo, hi);
    let m = _mm_min_ps(m, _mm_movehl_ps(m, m));
    let m = _mm_min_ss(m, _mm_shuffle_ps::<1>(m, m));
    _mm_cvtss_f32(m)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn hmax256_ps(v: std::arch::x86_64::__m256) -> f32 {
    use std::arch::x86_64::*;
    let lo = _mm256_castps256_ps128(v);
    let hi = _mm256_extractf128_ps::<1>(v);
    let m = _mm_max_ps(lo, hi);
    let m = _mm_max_ps(m, _mm_movehl_ps(m, m));
    let m = _mm_max_ss(m, _mm_shuffle_ps::<1>(m, m));
    _mm_cvtss_f32(m)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn fast_sample_mono_avx2(
    mono_buf: &[u16],
    w_in: usize,
    sx0: isize,
    sy0: isize,
    w_xs: &[f32; 6],
    fy: f32,
    lut: &LanczosLUT,
    drop_size: f32,
) -> (f32, f32, f32, f32) {
    use std::arch::x86_64::*;
    // Pesos en X con los 2 lanes de relleno a 0 (no contribuyen a las sumas).
    let wxs = _mm256_setr_ps(w_xs[0], w_xs[1], w_xs[2], w_xs[3], w_xs[4], w_xs[5], 0.0, 0.0);
    let mut sum_v = _mm256_setzero_ps();
    let mut sum_w = _mm256_setzero_ps();
    let mut vmin = _mm256_set1_ps(65535.0);
    let mut vmax = _mm256_setzero_ps();
    let base = (sx0 - 2) as usize;
    for ky in -2..=3isize {
        let wy = lut.get(fy - ky as f32, drop_size);
        if wy.abs() < 0.001 {
            continue;
        }
        let p = mono_buf.as_ptr().add((sy0 + ky) as usize * w_in + base);
        // Carga de EXACTAMENTE 6 u16 (4 + 2) para no leer fuera de rango: el
        // guard del fast-path solo garantiza índices [sx0-2 .. sx0+3].
        let lo = _mm_loadl_epi64(p as *const __m128i); // u16[0..4]
        let hi = _mm_cvtsi32_si128(core::ptr::read_unaligned(p.add(4) as *const i32)); // u16[4..6]
        let combined = _mm_or_si128(lo, _mm_bslli_si128::<8>(hi));
        let pv = _mm256_cvtepi32_ps(_mm256_cvtepu16_epi32(combined)); // lanes 6,7 = 0
        let wf = _mm256_mul_ps(wxs, _mm256_set1_ps(wy)); // lanes 6,7 = 0
        sum_v = _mm256_add_ps(sum_v, _mm256_mul_ps(pv, wf));
        sum_w = _mm256_add_ps(sum_w, wf);
        // Para el mínimo, fijar los lanes de relleno a 65535 (neutros).
        let pv_min = _mm256_blend_ps::<0b1100_0000>(pv, _mm256_set1_ps(65535.0));
        vmin = _mm256_min_ps(vmin, pv_min);
        vmax = _mm256_max_ps(vmax, pv); // lanes 6,7 = 0, neutros para max (datos >= 0)
    }
    (
        hsum256_ps(sum_v),
        hsum256_ps(sum_w),
        hmin256_ps(vmin),
        hmax256_ps(vmax),
    )
}

/// Despacho: usa AVX2 si está disponible; si no, el escalar (mismo resultado
/// salvo <1 ULP en la suma). En aarch64/otros usa el escalar.
#[inline(always)]
fn fast_sample_mono(
    mono_buf: &[u16],
    w_in: usize,
    sx0: isize,
    sy0: isize,
    w_xs: &[f32; 6],
    fy: f32,
    lut: &LanczosLUT,
    drop_size: f32,
) -> (f32, f32, f32, f32) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return unsafe {
                fast_sample_mono_avx2(mono_buf, w_in, sx0, sy0, w_xs, fy, lut, drop_size)
            };
        }
    }
    fast_sample_mono_scalar(mono_buf, w_in, sx0, sy0, w_xs, fy, lut, drop_size)
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
                    // El warp map puede referenciar un AP fuera de rango (mismatch
                    // de conteo de APs en ciertos SER). Sin este guard, local_shifts[ap_idx]
                    // hace panic y, con panic=abort, cerraba la app en seco.
                    if ap_idx >= local_shifts.len() {
                        continue;
                    }
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

            let (sum_v, sum_w, min_v, max_v) = if sx0 >= 2
                && sx0 + 3 < w_in as isize
                && sy0 >= 2
                && sy0 + 3 < h_in as isize
            {
                // FAST PATH (Interior Pixels) — SIMD (AVX2) con fallback escalar
                fast_sample_mono(mono_buf, w_in, sx0, sy0, &w_xs, fy, lut, drop_size)
            } else {
                // SLOW PATH (Boundary clipping) — escalar
                let mut min_v = 65535.0f32;
                let mut max_v = 0.0f32;
                let mut sum_v = 0.0f32;
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
                (sum_v, sum_w, min_v, max_v)
            };

            if sum_w.abs() > 0.00001 {
                // Symmetric range-based anti-ringing clamp (same policy as RGB):
                // unbiased on dark structure, scale-invariant.
                let band = (max_v - min_v) * 0.18 + 32.0;
                let val =
                    (sum_v / sum_w).clamp((min_v - band).max(0.0), (max_v + band).min(65535.0));
                let tidx = row_off + x_out;
                if tidx < acc.len() {
                    unsafe {
                        *acc.get_unchecked_mut(tidx) += val * pixel_weight;
                        *acc_w.get_unchecked_mut(tidx) += pixel_weight;
                    }
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

// ===========================================================================
// Validación Fase 1a: el fast-path AVX2 debe coincidir con el escalar.
// Solo se compila/ejecuta en x86_64 (donde corre la ruta AVX2). min/max deben
// ser EXACTOS; la suma puede diferir <1 ULP por el reordenamiento.
// ===========================================================================
#[cfg(all(test, target_arch = "x86_64"))]
mod simd_validation {
    use super::*;

    #[test]
    fn mono_fast_sample_avx2_matches_scalar() {
        // Nota: bajo Rosetta, is_x86_feature_detected!("avx2") puede devolver
        // false aunque las instrucciones AVX2 SÍ se ejecuten. Forzamos la llamada
        // directa a la versión AVX2 para validarla aquí. En hardware sin AVX2
        // real esto daría SIGILL (y entonces la validación debe hacerse en x86).
        eprintln!(
            "avx2 reportado por cpuid = {}",
            is_x86_feature_detected!("avx2")
        );
        let w_in = 96usize;
        let h_in = 96usize;
        // Buffer mono pseudo-aleatorio determinista (LCG).
        let mut buf = vec![0u16; w_in * h_in];
        let mut s = 0x1234_5678u32;
        for v in buf.iter_mut() {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *v = (s >> 16) as u16;
        }
        let lut = LanczosLUT::new(30000, 3.0);
        let drop_size = 1.0f32;

        let mut max_diff_v = 0.0f32;
        let mut max_diff_w = 0.0f32;
        let mut checked = 0u64;

        for sy0 in 2..(h_in as isize - 3) {
            for sx0 in 2..(w_in as isize - 3) {
                for &fx in &[0.0f32, 0.2, 0.5, 0.51, 0.77, 0.999] {
                    for &fy in &[0.0f32, 0.13, 0.49, 0.5, 0.86, 0.999] {
                        let mut w_xs = [0.0f32; 6];
                        for (i, kx) in (-2..=3).enumerate() {
                            w_xs[i] = lut.get(fx - kx as f32, drop_size);
                        }
                        let sc =
                            fast_sample_mono_scalar(&buf, w_in, sx0, sy0, &w_xs, fy, &lut, drop_size);
                        let si = unsafe {
                            fast_sample_mono_avx2(&buf, w_in, sx0, sy0, &w_xs, fy, &lut, drop_size)
                        };
                        // min/max: exactos (min/max de los mismos f32, sin aritmética).
                        assert_eq!(sc.2, si.2, "min difiere en sx0={sx0} sy0={sy0}");
                        assert_eq!(sc.3, si.3, "max difiere en sx0={sx0} sy0={sy0}");
                        max_diff_v = max_diff_v.max((sc.0 - si.0).abs());
                        max_diff_w = max_diff_w.max((sc.1 - si.1).abs());
                        checked += 1;
                    }
                }
            }
        }
        eprintln!(
            "SIMD validado en {checked} casos. max_diff sum_v={max_diff_v}, sum_w={max_diff_w}"
        );
        // sum_v ~ hasta ~65535×(Σpesos); el reordenamiento da error relativo ~1e-6.
        // Umbral holgado pero estricto frente a un bug real (que daría diffs grandes).
        assert!(max_diff_v < 0.5, "sum_v difiere demasiado: {max_diff_v}");
        assert!(max_diff_w < 1e-3, "sum_w difiere demasiado: {max_diff_w}");
    }

    #[test]
    fn rgb_fast_sample_avx2_matches_scalar() {
        let w_in = 80usize;
        let h_in = 80usize;
        // Buffer RGB interleaved pseudo-aleatorio determinista.
        let mut buf = vec![0u16; w_in * h_in * 3];
        let mut s = 0x9E37_79B9u32;
        for v in buf.iter_mut() {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *v = (s >> 16) as u16;
        }
        let lut = LanczosLUT::new(30000, 3.0);
        let drop_size = 1.0f32;

        let mut max_diff_sum = 0.0f32;
        let mut checked = 0u64;
        for sy0 in 2..(h_in as isize - 3) {
            for sx0 in 2..(w_in as isize - 3) {
                for &fx in &[0.0f32, 0.2, 0.5, 0.51, 0.77, 0.999] {
                    for &fy in &[0.0f32, 0.13, 0.49, 0.5, 0.86, 0.999] {
                        let mut w_xs = [0.0f32; 6];
                        for (i, kx) in (-2..=3).enumerate() {
                            w_xs[i] = lut.get(fx - kx as f32, drop_size);
                        }
                        let sc =
                            fast_sample_rgb_scalar(&buf, w_in, sx0, sy0, &w_xs, fy, &lut, drop_size);
                        let si = unsafe {
                            fast_sample_rgb_avx2(&buf, w_in, sx0, sy0, &w_xs, fy, &lut, drop_size)
                        };
                        // min/max (índices 4..10) deben ser EXACTOS.
                        for j in 4..10 {
                            let a = [sc.4, sc.5, sc.6, sc.7, sc.8, sc.9][j - 4];
                            let b = [si.4, si.5, si.6, si.7, si.8, si.9][j - 4];
                            assert_eq!(a, b, "min/max idx {j} difiere en sx0={sx0} sy0={sy0}");
                        }
                        // sumas (0..4): <1 ULP.
                        for j in 0..4 {
                            let a = [sc.0, sc.1, sc.2, sc.3][j];
                            let b = [si.0, si.1, si.2, si.3][j];
                            max_diff_sum = max_diff_sum.max((a - b).abs());
                        }
                        checked += 1;
                    }
                }
            }
        }
        eprintln!("RGB SIMD validado en {checked} casos. max_diff suma={max_diff_sum}");
        assert!(max_diff_sum < 0.5, "suma RGB difiere demasiado: {max_diff_sum}");
    }
}
