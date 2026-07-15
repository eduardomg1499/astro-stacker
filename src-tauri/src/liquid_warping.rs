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
/// Rejilla espacial de APs para el Top-K EXACTO en O(vecindario) por pixel.
/// Con mallas densas (24px + umbral bajo → ~6000 APs) el barrido lineal del
/// builder IDW costaba n_px × n_APs ≈ 2×10^10 distancias por apilado —
/// decenas de segundos SOLO en construir el warp map. La busqueda por anillos
/// Chebyshev crecientes es exacta: se detiene cuando el K-esimo mejor esta
/// garantizado mas cerca que cualquier celda sin visitar (cota (r−1)·celda).
struct ApSpatialGrid {
    cell: f32,
    gw: usize,
    gh: usize,
    min_x: f32,
    min_y: f32,
    cells: Vec<Vec<u16>>,
}

impl ApSpatialGrid {
    fn build(points: &[ApPoint]) -> Self {
        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_y = f32::MIN;
        for p in points {
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
            max_x = max_x.max(p.x);
            max_y = max_y.max(p.y);
        }
        let span_x = (max_x - min_x).max(1.0);
        let span_y = (max_y - min_y).max(1.0);
        // ~2 puntos por celda de media (celda = espaciado tipico × 1.4).
        let cell = ((span_x * span_y / points.len().max(1) as f32).sqrt() * 1.4).max(4.0);
        let gw = ((span_x / cell).ceil() as usize + 1).max(1);
        let gh = ((span_y / cell).ceil() as usize + 1).max(1);
        let mut cells = vec![Vec::new(); gw * gh];
        for (i, p) in points.iter().enumerate() {
            let cx = (((p.x - min_x) / cell) as usize).min(gw - 1);
            let cy = (((p.y - min_y) / cell) as usize).min(gh - 1);
            cells[cy * gw + cx].push(i as u16);
        }
        Self { cell, gw, gh, min_x, min_y, cells }
    }

    /// Llena `top_k[..k]` con los k APs mas cercanos a (px, py), ordenados por
    /// distancia — MISMO resultado que el barrido lineal (insertion sort
    /// identico), solo que visitando celdas por anillos crecientes.
    #[inline]
    fn top_k_into(
        &self,
        px: f32,
        py: f32,
        k: usize,
        top_k: &mut [(f32, u16)],
        points: &[ApPoint],
    ) {
        for slot in top_k.iter_mut() {
            *slot = (f32::MAX, 0u16);
        }
        let cx = (((px - self.min_x) / self.cell).floor() as isize)
            .clamp(0, self.gw as isize - 1);
        let cy = (((py - self.min_y) / self.cell).floor() as isize)
            .clamp(0, self.gh as isize - 1);
        let max_ring = (self.gw.max(self.gh)) as isize;

        let visit = |gx: isize, gy: isize, top_k: &mut [(f32, u16)]| {
            for &pi in &self.cells[gy as usize * self.gw + gx as usize] {
                let p = &points[pi as usize];
                let dx = px - p.x;
                let dy = py - p.y;
                let d2 = dx * dx + dy * dy;
                if d2 < top_k[k - 1].0 {
                    let mut ins = k - 1;
                    while ins > 0 && d2 < top_k[ins - 1].0 {
                        top_k[ins] = top_k[ins - 1];
                        ins -= 1;
                    }
                    top_k[ins] = (d2, pi);
                }
            }
        };

        for r in 0..=max_ring {
            // Parada EXACTA: toda celda del anillo r esta a ≥ (r−1)·cell del
            // punto (que vive dentro de su propia celda) — si ya tenemos k
            // vecinos y el peor esta mas cerca que esa cota, no hay nada
            // mejor en anillos posteriores.
            if top_k[k - 1].0 < f32::MAX {
                let ring_min = ((r - 1).max(0) as f32) * self.cell;
                if top_k[k - 1].0 <= ring_min * ring_min {
                    return;
                }
            }
            let (x0, x1) = (cx - r, cx + r);
            let (y0, y1) = (cy - r, cy + r);
            for gy in y0.max(0)..=y1.min(self.gh as isize - 1) {
                if gy == y0 || gy == y1 {
                    // fila superior/inferior del anillo: completa
                    for gx in x0.max(0)..=x1.min(self.gw as isize - 1) {
                        visit(gx, gy, top_k);
                    }
                } else {
                    // filas intermedias: solo las columnas del borde
                    if x0 >= 0 {
                        visit(x0, gy, top_k);
                    }
                    if x1 != x0 && x1 < self.gw as isize {
                        visit(x1, gy, top_k);
                    }
                }
            }
        }
    }
}

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

    // Rejilla espacial solo para mallas densas: por debajo de ~96 APs el
    // barrido lineal es mas barato que el overhead de anillos por pixel.
    let grid = if n_points >= 96 {
        Some(ApSpatialGrid::build(custom_points))
    } else {
        None
    };

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

                if let Some(g) = grid.as_ref() {
                    // Malla densa: Top-K exacto visitando solo celdas vecinas.
                    g.top_k_into(px, py, effective_k, &mut top_k, custom_points);
                } else {
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
// DRIZZLE VERDADERO (kernel "drop" de solape de areas, estilo AS!4/HST).
//
// Con drizzle > 1x el muestreo anterior era un Lanczos-3 ESTRECHADO
// (drop_size 0.75 → soporte ±2.25 px) sobre el grid fino: una interpolacion
// excelente, pero correlaciona vecinos y limita la resolucion recuperable.
// El drizzle real deposita el flujo de cada pixel fuente con una huella
// reducida (pixfrac) y deja que el jitter sub-pixel del seeing rellene el
// grid fino frame a frame — de ahi sale la resolucion extra con stacks
// grandes. La UI no cambia: drop_size juega el rol de pixfrac (0.75).
//
// Formulacion INVERSE-MAPPING, equivalente al deposito forward clasico
// cuando el warp es localmente constante (el campo IDW varia en escalas de
// decenas de px, muy por encima del soporte del kernel): el peso de un pixel
// fuente sobre el pixel de salida es el AREA DE SOLAPE entre el drop del
// fuente (lado pixfrac, centrado en su coordenada entera) y la huella del
// pixel de salida proyectada a coordenadas fuente (lado 1/escala, centrada
// en sx_in). Separable en X e Y → producto de dos solapes 1-D trapezoidales:
//     overlap(d) = clamp((h1 + h2) − |d|, 0, 2·min(h1, h2))
// con h1 = 1/(2·escala), h2 = pixfrac/2. Soporte total h1+h2 < 1 px → bastan
// los vecinos ±1 por eje. Kernel NO-NEGATIVO: cero ringing (no necesita el
// clamp anti-ringing del Lanczos). Con pixfrac 0.75, h1+h2 > 0.5 siempre →
// ningun pixel de salida queda sin cobertura dentro del frame.
//
// Nota de fidelidad: la ponderacion CRUZADA entre frames por cobertura (el
// peso overlap del drizzle clasico) no se propaga al acumulador global (que
// pondera por calidad q² del frame) — refinamiento posible si hiciera falta.
// ===========================================================================
#[inline(always)]
fn drizzle_overlap_1d(d: f32, h1: f32, h2: f32) -> f32 {
    ((h1 + h2) - d.abs()).clamp(0.0, 2.0 * h1.min(h2))
}

/// Muestreo drop mono en (sx_in, sy_in). Devuelve (suma_ponderada, suma_pesos);
/// sum_w == 0 → sin cobertura (fuera del frame), el caller no escribe.
#[inline(always)]
fn drizzle_sample_mono(
    mono_buf: &[u16],
    w_in: usize,
    h_in: usize,
    sx_in: f32,
    sy_in: f32,
    h1: f32,
    h2: f32,
) -> (f32, f32) {
    let x0 = sx_in.round() as isize;
    let y0 = sy_in.round() as isize;
    let mut sum_v = 0.0f32;
    let mut sum_w = 0.0f32;
    for ky in -1isize..=1 {
        let py = y0 + ky;
        if py < 0 || py >= h_in as isize {
            continue;
        }
        let wy = drizzle_overlap_1d(py as f32 - sy_in, h1, h2);
        if wy <= 0.0 {
            continue;
        }
        let row = py as usize * w_in;
        for kx in -1isize..=1 {
            let px = x0 + kx;
            if px < 0 || px >= w_in as isize {
                continue;
            }
            let wx = drizzle_overlap_1d(px as f32 - sx_in, h1, h2);
            if wx <= 0.0 {
                continue;
            }
            let w = wx * wy;
            sum_v += mono_buf[row + px as usize] as f32 * w;
            sum_w += w;
        }
    }
    (sum_v, sum_w)
}

/// Muestreo drop RGB interleaved. Devuelve (r, g, b, suma_pesos).
#[inline(always)]
fn drizzle_sample_rgb(
    rgb_buf: &[u16],
    w_in: usize,
    h_in: usize,
    sx_in: f32,
    sy_in: f32,
    h1: f32,
    h2: f32,
) -> (f32, f32, f32, f32) {
    let x0 = sx_in.round() as isize;
    let y0 = sy_in.round() as isize;
    let mut sum_r = 0.0f32;
    let mut sum_g = 0.0f32;
    let mut sum_b = 0.0f32;
    let mut sum_w = 0.0f32;
    for ky in -1isize..=1 {
        let py = y0 + ky;
        if py < 0 || py >= h_in as isize {
            continue;
        }
        let wy = drizzle_overlap_1d(py as f32 - sy_in, h1, h2);
        if wy <= 0.0 {
            continue;
        }
        let row = py as usize * w_in;
        for kx in -1isize..=1 {
            let px = x0 + kx;
            if px < 0 || px >= w_in as isize {
                continue;
            }
            let wx = drizzle_overlap_1d(px as f32 - sx_in, h1, h2);
            if wx <= 0.0 {
                continue;
            }
            let w = wx * wy;
            let off = (row + px as usize) * 3;
            sum_r += rgb_buf[off] as f32 * w;
            sum_g += rgb_buf[off + 1] as f32 * w;
            sum_b += rgb_buf[off + 2] as f32 * w;
            sum_w += w;
        }
    }
    (sum_r, sum_g, sum_b, sum_w)
}

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

    // DRIZZLE VERDADERO: con escala >1x se muestrea con el kernel drop de
    // solape de areas (ver drizzle_sample_*) en vez del Lanczos estrechado.
    let true_drizzle = drizzle > 1.01;
    let drz_h1 = 0.5 * inv_drizzle; // media huella del pixel de salida (coords fuente)
    let drz_h2 = 0.5 * drop_size.clamp(0.3, 1.0); // pixfrac/2
    // Cobertura maxima del kernel (solape pleno en ambos ejes): normaliza cov a (0,1].
    let drz_full_cov = {
        let m = 2.0 * drz_h1.min(drz_h2);
        (m * m).max(1e-9)
    };

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

            // DRIZZLE VERDADERO: kernel drop no-negativo (sin ringing → sin
            // clamp) con soporte < 1 px. La normalizacion sum/sum_w conserva
            // la media local; sum_w == 0 solo fuera del frame (no se escribe).
            if true_drizzle {
                let (dr, dg, db, dw) =
                    drizzle_sample_rgb(rgb_buf, w_in, h_in, sx_in, sy_in, drz_h1, drz_h2);
                if dw > 1e-6 {
                    let tidx = row_off + x_out;
                    if tidx < acc_r.len() {
                        // PONDERACION POR COBERTURA (drizzle clasico): un frame
                        // cuyo drop apenas roza este pixel de salida aporta un
                        // estimado dominado por UN solo pixel fuente (mas
                        // ruidoso) — cov ∈ (0,1] viaja en el plano de pesos y
                        // el acumulador global lo usa para ponderar ENTRE
                        // frames (weight_by_coverage). El VALOR del frame no
                        // cambia: (v·pw·cov)/(pw·cov) = v.
                        let cov = (dw / drz_full_cov).min(1.0);
                        let wq = pixel_weight * cov;
                        let inv = 1.0 / dw;
                        unsafe {
                            *acc_r.get_unchecked_mut(tidx) += dr * inv * wq;
                            *acc_g.get_unchecked_mut(tidx) += dg * inv * wq;
                            *acc_b.get_unchecked_mut(tidx) += db * inv * wq;
                            *acc_w.get_unchecked_mut(tidx) += wq;
                        }
                    }
                }
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

    // DRIZZLE VERDADERO: con escala >1x se muestrea con el kernel drop de
    // solape de areas (ver drizzle_sample_*) en vez del Lanczos estrechado.
    let true_drizzle = drizzle > 1.01;
    let drz_h1 = 0.5 * inv_drizzle; // media huella del pixel de salida (coords fuente)
    let drz_h2 = 0.5 * drop_size.clamp(0.3, 1.0); // pixfrac/2
    // Cobertura maxima del kernel (solape pleno en ambos ejes): normaliza cov a (0,1].
    let drz_full_cov = {
        let m = 2.0 * drz_h1.min(drz_h2);
        (m * m).max(1e-9)
    };

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

            // DRIZZLE VERDADERO: kernel drop no-negativo (sin ringing → sin
            // clamp) con soporte < 1 px; ver drizzle_sample_mono. cov pondera
            // la cobertura del drop entre frames (ver la variante RGB).
            if true_drizzle {
                let (dv, dw) =
                    drizzle_sample_mono(mono_buf, w_in, h_in, sx_in, sy_in, drz_h1, drz_h2);
                if dw > 1e-6 {
                    let tidx = row_off + x_out;
                    if tidx < acc.len() {
                        let cov = (dw / drz_full_cov).min(1.0);
                        let wq = pixel_weight * cov;
                        unsafe {
                            *acc.get_unchecked_mut(tidx) += (dv / dw) * wq;
                            *acc_w.get_unchecked_mut(tidx) += wq;
                        }
                    }
                }
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
                    // PR-1.5: SYMMETRIC RANGE-BASED ANTI-RINGING CLAMP — misma
                    // política que las rutas líquidas (ver accumulate_frame_liquid).
                    // El clamp antiguo (min·factor, sin tope superior) dejaba
                    // pasar el overshoot de los lóbulos negativos del Lanczos en
                    // las altas luces (halo/ringing en el limbo brillante — esta
                    // ruta es justo la de planeta pequeño/global) y aclaraba
                    // sistemáticamente la microestructura oscura.
                    let band_r = (max_r - min_r) * 0.18 + 32.0;
                    let band_g = (max_g - min_g) * 0.18 + 32.0;
                    let band_b = (max_b - min_b) * 0.18 + 32.0;
                    let px_r = (sum_r * inv_sum)
                        .clamp((min_r - band_r).max(0.0), (max_r + band_r).min(65535.0));
                    let px_g = (sum_g * inv_sum)
                        .clamp((min_g - band_g).max(0.0), (max_g + band_g).min(65535.0));
                    let px_b = (sum_b * inv_sum)
                        .clamp((min_b - band_b).max(0.0), (max_b + band_b).min(65535.0));
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
                    // PR-1.5: SYMMETRIC RANGE-BASED ANTI-RINGING CLAMP — misma
                    // política que las rutas líquidas (ver accumulate_frame_liquid).
                    // El clamp antiguo (min·factor, sin tope superior) dejaba
                    // pasar el overshoot de los lóbulos negativos del Lanczos en
                    // las altas luces (halo/ringing en el limbo brillante — esta
                    // ruta es justo la de planeta pequeño/global) y aclaraba
                    // sistemáticamente la microestructura oscura.
                    let band_r = (max_r - min_r) * 0.18 + 32.0;
                    let band_g = (max_g - min_g) * 0.18 + 32.0;
                    let band_b = (max_b - min_b) * 0.18 + 32.0;
                    let px_r = (sum_r * inv_sum)
                        .clamp((min_r - band_r).max(0.0), (max_r + band_r).min(65535.0));
                    let px_g = (sum_g * inv_sum)
                        .clamp((min_g - band_g).max(0.0), (max_g + band_g).min(65535.0));
                    let px_b = (sum_b * inv_sum)
                        .clamp((min_b - band_b).max(0.0), (max_b + band_b).min(65535.0));

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

// ===========================================================================
// Validación del DRIZZLE VERDADERO (kernel drop). Independiente de la
// arquitectura (el kernel drop es escalar puro).
// ===========================================================================
#[cfg(test)]
mod drizzle_tests {
    use super::*;

    /// La rejilla espacial del builder IDW debe dar EXACTAMENTE el mismo
    /// Top-K que el barrido lineal (misma insertion sort, distinto orden de
    /// visita). Puntos y queries pseudo-aleatorios deterministas (LCG),
    /// incluyendo queries FUERA del bounding box de los APs (borde del canvas
    /// con drizzle/ROI), y ambos valores de K usados en produccion (4 y 8).
    #[test]
    fn test_idw_spatial_grid_matches_brute_force() {
        let mut seed = 0x1234_5678u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / 16_777_216.0
        };
        let n = 700usize; // > umbral 96 → produccion usaria la rejilla
        let points: Vec<ApPoint> = (0..n)
            .map(|_| ApPoint {
                x: rnd() * 640.0,
                y: rnd() * 480.0,
                size: 32,
            })
            .collect();
        let grid = ApSpatialGrid::build(&points);

        for q in 0..500 {
            let px = rnd() * 700.0 - 30.0;
            let py = rnd() * 540.0 - 30.0;
            let k = if q % 2 == 0 { 4usize } else { 8 };

            let mut got = [(f32::MAX, 0u16); 8];
            grid.top_k_into(px, py, k, &mut got, &points);

            let mut want = [(f32::MAX, 0u16); 8];
            for (i, p) in points.iter().enumerate() {
                let dx = px - p.x;
                let dy = py - p.y;
                let d2 = dx * dx + dy * dy;
                if d2 < want[k - 1].0 {
                    let mut ins = k - 1;
                    while ins > 0 && d2 < want[ins - 1].0 {
                        want[ins] = want[ins - 1];
                        ins -= 1;
                    }
                    want[ins] = (d2, i as u16);
                }
            }
            for j in 0..k {
                assert_eq!(
                    got[j].1, want[j].1,
                    "indice distinto en j={j} (query {px:.1},{py:.1} k={k})"
                );
                assert!(
                    (got[j].0 - want[j].0).abs() < 1e-3,
                    "distancia distinta en j={j}"
                );
            }
        }
    }

    /// Campo constante → cada pixel de salida cubierto debe devolver EXACTAMENTE
    /// el valor del campo (la normalización sum/sum_w conserva la media local).
    #[test]
    fn test_true_drizzle_conserves_constant_field() {
        let (w_in, h_in) = (32usize, 32usize);
        let drizzle = 2.0f32;
        let (w_out, h_out) = (64usize, 64usize);
        let mono = vec![1000u16; w_in * h_in];
        let mut acc = vec![0.0f32; w_out * h_out];
        let mut acc_w = vec![0.0f32; w_out * h_out];

        accumulate_frame_liquid_mono(
            &mut acc, &mut acc_w, &mono, w_in, h_in, w_out, h_out, drizzle,
            0.0, 0.0, 0.0, 0.0, &[], &[], &[], &[], &[], 1.0, 0.75,
        );

        let mut covered = 0usize;
        for i in 0..w_out * h_out {
            if acc_w[i] > 1e-6 {
                let v = acc[i] / acc_w[i];
                assert!(
                    (v - 1000.0).abs() < 0.01,
                    "pixel {} devolvio {} (esperado 1000)",
                    i,
                    v
                );
                covered += 1;
            }
        }
        // Con pixfrac 0.75 el soporte h1+h2 > 0.5 garantiza cobertura total
        // dentro del frame (solo el borde extremo puede quedar fuera).
        assert!(
            covered > (w_out - 2) * (h_out - 2),
            "cobertura insuficiente: {} pixeles",
            covered
        );
    }

    /// Recuperación de detalle sub-pixel: una sinusoide con periodo 1.4 px
    /// FUENTE (por encima del Nyquist de la fuente — aliased frame a frame,
    /// resoluble en el grid 2x) capturada en 9 frames con dither sub-pixel
    /// uniforme. El stack drizzle (kernel drop) debe reconstruir el patron
    /// claramente mejor que el promedio de upsampleos bilineales de los
    /// mismos frames — esa diferencia ES la resolucion que recupera drizzle.
    #[test]
    fn test_true_drizzle_recovers_subpixel_detail() {
        let (w_in, h_in) = (48usize, 48usize);
        let drizzle = 2.0f32;
        let (w_out, h_out) = (96usize, 96usize);

        // Patron continuo en coordenadas FUENTE.
        let f = |x: f32, y: f32| -> f32 {
            8000.0
                + 3000.0 * (x * std::f32::consts::TAU / 1.4).sin()
                + 3000.0 * (y * std::f32::consts::TAU / 1.6).cos()
        };

        // Frame k: pixel (i,j) = promedio de caja 1x1 (supersampleo 4x4) del
        // patron desplazado -d_k (convencion del acumulador: la fuente se
        // muestrea en sx = ref + global_dx, y source(sx) = ref(sx - d_k)).
        let offsets: [(f32, f32); 9] = [
            (0.0, 0.0), (1.0 / 3.0, 0.0), (2.0 / 3.0, 0.0),
            (0.0, 1.0 / 3.0), (1.0 / 3.0, 1.0 / 3.0), (2.0 / 3.0, 1.0 / 3.0),
            (0.0, 2.0 / 3.0), (1.0 / 3.0, 2.0 / 3.0), (2.0 / 3.0, 2.0 / 3.0),
        ];
        let render_frame = |dx: f32, dy: f32| -> Vec<u16> {
            let mut buf = vec![0u16; w_in * h_in];
            for j in 0..h_in {
                for i in 0..w_in {
                    let mut s = 0.0f32;
                    for oy in 0..4 {
                        for ox in 0..4 {
                            let sx = i as f32 - dx - 0.375 + ox as f32 * 0.25;
                            let sy = j as f32 - dy - 0.375 + oy as f32 * 0.25;
                            s += f(sx, sy);
                        }
                    }
                    buf[j * w_in + i] = (s / 16.0).clamp(0.0, 65535.0) as u16;
                }
            }
            buf
        };

        // --- Stack drizzle (kernel drop) ---
        let mut acc = vec![0.0f32; w_out * h_out];
        let mut acc_w = vec![0.0f32; w_out * h_out];
        // --- Referencia: promedio de upsampleos bilineales de los mismos frames ---
        let mut bil = vec![0.0f32; w_out * h_out];
        let mut bil_n = vec![0.0f32; w_out * h_out];

        for &(dx, dy) in &offsets {
            let frame = render_frame(dx, dy);
            accumulate_frame_liquid_mono(
                &mut acc, &mut acc_w, &frame, w_in, h_in, w_out, h_out, drizzle,
                0.0, 0.0, dx, dy, &[], &[], &[], &[], &[], 1.0, 0.75,
            );
            for yo in 0..h_out {
                for xo in 0..w_out {
                    let sx = xo as f32 / drizzle + dx;
                    let sy = yo as f32 / drizzle + dy;
                    let x0 = sx.floor() as isize;
                    let y0 = sy.floor() as isize;
                    if x0 < 0 || y0 < 0 || x0 + 1 >= w_in as isize || y0 + 1 >= h_in as isize {
                        continue;
                    }
                    let fx = sx - x0 as f32;
                    let fy = sy - y0 as f32;
                    let (x0, y0) = (x0 as usize, y0 as usize);
                    let v = frame[y0 * w_in + x0] as f32 * (1.0 - fx) * (1.0 - fy)
                        + frame[y0 * w_in + x0 + 1] as f32 * fx * (1.0 - fy)
                        + frame[(y0 + 1) * w_in + x0] as f32 * (1.0 - fx) * fy
                        + frame[(y0 + 1) * w_in + x0 + 1] as f32 * fx * fy;
                    bil[yo * w_out + xo] += v;
                    bil_n[yo * w_out + xo] += 1.0;
                }
            }
        }

        // RMSE contra el patron FILTRADO POR LA APERTURA del pixel (caja 1x1):
        // ninguna reconstruccion puede deshacer el prefiltro fisico del sensor,
        // asi que la referencia justa es la sinusoide con su amplitud atenuada
        // por sinc(π·w/T) — lo mejor alcanzable desde estas muestras. Contra el
        // patron puntual la perdida de apertura (compartida) diluia el ratio.
        let sinc = |t: f32| if t.abs() < 1e-6 { 1.0 } else { t.sin() / t };
        let ax = sinc(std::f32::consts::PI / 1.4);
        let ay = sinc(std::f32::consts::PI / 1.6);
        let f_ap = |x: f32, y: f32| -> f32 {
            8000.0
                + 3000.0 * ax * (x * std::f32::consts::TAU / 1.4).sin()
                + 3000.0 * ay * (y * std::f32::consts::TAU / 1.6).cos()
        };
        let margin = 6usize;
        let mut se_drz = 0.0f64;
        let mut se_bil = 0.0f64;
        let mut n = 0.0f64;
        for yo in margin..h_out - margin {
            for xo in margin..w_out - margin {
                let i = yo * w_out + xo;
                if acc_w[i] <= 1e-6 || bil_n[i] <= 0.0 {
                    continue;
                }
                let gt = f_ap(xo as f32 / drizzle, yo as f32 / drizzle) as f64;
                let d = (acc[i] / acc_w[i]) as f64 - gt;
                let b = (bil[i] / bil_n[i]) as f64 - gt;
                se_drz += d * d;
                se_bil += b * b;
                n += 1.0;
            }
        }
        let rmse_drz = (se_drz / n).sqrt();
        let rmse_bil = (se_bil / n).sqrt();
        eprintln!("drizzle RMSE={rmse_drz:.1} vs bilineal RMSE={rmse_bil:.1} (n={n})");
        assert!(
            rmse_drz < rmse_bil * 0.8,
            "el drizzle drop ({rmse_drz:.1}) debe superar claramente al upsample bilineal ({rmse_bil:.1})"
        );
    }
}
