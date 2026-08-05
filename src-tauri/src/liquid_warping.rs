use crate::smart_grid::ApPoint;
use rayon::prelude::*;
use std::sync::OnceLock;

/// Pre-computes the Warp Map for IDW (Inverse Distance Weighting).
/// Returns (warp_indices, warp_weights).
pub fn compute_idw_map(
    width: usize,
    height: usize,
    custom_points: &[ApPoint],
) -> Result<(Vec<u16>, Vec<f32>), String> {
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
) -> Result<(Vec<u16>, Vec<f32>), String> {
    compute_idw_map_for_output(width, height, 1.0, 0.0, 0.0, custom_points, idw_power, 8)
}

fn try_idw_filled_vec<T: Clone>(len: usize, value: T, label: &str) -> Result<Vec<T>, String> {
    let bytes = len
        .checked_mul(std::mem::size_of::<T>())
        .ok_or_else(|| format!("La reserva de {label} excede el espacio direccionable"))?;
    let mut values = Vec::new();
    values.try_reserve_exact(len).map_err(|error| {
        format!(
            "No se pudieron reservar {bytes} bytes para {label}: {error}. Reduce drizzle/ROI o libera RAM."
        )
    })?;
    values.resize(len, value);
    Ok(values)
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

#[inline]
fn idw_neighbor_before(candidate: (f32, u16), current: (f32, u16)) -> bool {
    candidate.0 < current.0 || (candidate.0 == current.0 && candidate.1 < current.1)
}

impl ApSpatialGrid {
    fn build(points: &[ApPoint]) -> Result<Self, String> {
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
        let cell_count = gw
            .checked_mul(gh)
            .ok_or("La rejilla espacial IDW excede el espacio direccionable")?;
        let mut cells = try_idw_filled_vec(cell_count, Vec::new(), "rejilla espacial IDW")?;
        for (i, p) in points.iter().enumerate() {
            let cx = (((p.x - min_x) / cell) as usize).min(gw - 1);
            let cy = (((p.y - min_y) / cell) as usize).min(gh - 1);
            cells[cy * gw + cx]
                .try_reserve(1)
                .map_err(|error| format!("No se pudo ampliar una celda IDW: {error}"))?;
            cells[cy * gw + cx].push(i as u16);
        }
        Ok(Self {
            cell,
            gw,
            gh,
            min_x,
            min_y,
            cells,
        })
    }

    /// Llena `top_k[..k]` con los k APs mas cercanos a (px, py), ordenados por
    /// distancia — MISMO resultado que el barrido lineal (insertion sort
    /// identico), solo que visitando celdas por anillos crecientes.
    #[inline]
    fn top_k_into(&self, px: f32, py: f32, k: usize, top_k: &mut [(f32, u16)], points: &[ApPoint]) {
        for slot in top_k.iter_mut() {
            *slot = (f32::MAX, 0u16);
        }
        let cx = (((px - self.min_x) / self.cell).floor() as isize).clamp(0, self.gw as isize - 1);
        let cy = (((py - self.min_y) / self.cell).floor() as isize).clamp(0, self.gh as isize - 1);
        let max_ring = (self.gw.max(self.gh)) as isize;

        let visit = |gx: isize, gy: isize, top_k: &mut [(f32, u16)]| {
            for &pi in &self.cells[gy as usize * self.gw + gx as usize] {
                let p = &points[pi as usize];
                let dx = px - p.x;
                let dy = py - p.y;
                let d2 = dx * dx + dy * dy;
                let candidate = (d2, pi);
                if idw_neighbor_before(candidate, top_k[k - 1]) {
                    let mut ins = k - 1;
                    while ins > 0 && idw_neighbor_before(candidate, top_k[ins - 1]) {
                        top_k[ins] = top_k[ins - 1];
                        ins -= 1;
                    }
                    top_k[ins] = candidate;
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
                // Con igualdad todavía puede existir en el siguiente anillo un
                // AP a la misma distancia pero con índice menor. Continuar en
                // ese borde mantiene el mismo desempate que el barrido lineal.
                if top_k[k - 1].0 < ring_min * ring_min {
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
) -> Result<(Vec<u16>, Vec<f32>), String> {
    // --- OPT 1+B: PRE-COMPUTE TOP-K IDW WARP MAP ---
    // Instead of ALL n_points weights per pixel, store only the K nearest APs.
    // For p≥3 IDW, distant AP weights are negligible (<0.001), so Top-K loses no quality.
    // Memory: pixels × K × 6 bytes ≈ 50MB for 2MP image (vs 1.6GB for 200 full APs).
    const WARP_K: usize = 8;
    let n_points = custom_points.len();

    if n_points == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    if width == 0 || height == 0 {
        return Err("No se puede construir un mapa IDW con geometría vacía".into());
    }
    if n_points > u16::MAX as usize {
        return Err(format!(
            "El mapa IDW admite como máximo {} puntos de alineación; recibió {n_points}",
            u16::MAX
        ));
    }

    let effective_k = top_k.clamp(1, WARP_K).min(n_points); // Handle cases with fewer than K APs

    // Rejilla espacial solo para mallas densas: por debajo de ~96 APs el
    // barrido lineal es mas barato que el overhead de anillos por pixel.
    let grid = if n_points >= 96 {
        Some(ApSpatialGrid::build(custom_points)?)
    } else {
        None
    };

    // Flat arrays: warp_indices[pixel × K + k] = AP index, warp_weights[pixel × K + k] = normalized weight
    let total_pixels = width
        .checked_mul(height)
        .ok_or("El mapa IDW excede el espacio direccionable")?;
    let map_len = total_pixels
        .checked_mul(effective_k)
        .ok_or("Los vecinos del mapa IDW exceden el espacio direccionable")?;
    let mut indices = try_idw_filled_vec(map_len, 0u16, "índices del mapa IDW")?;
    let mut weights = try_idw_filled_vec(map_len, 0.0f32, "pesos del mapa IDW")?;

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
                        let candidate = (dist_sq, kp_i as u16);

                        // If this point is closer than our FURTHEST point in the Top K list...
                        if idw_neighbor_before(candidate, top_k[effective_k - 1]) {
                            // Find where to insert it (O(K), which is tiny, max 8)
                            let mut insert_idx = effective_k - 1;
                            while insert_idx > 0
                                && idw_neighbor_before(candidate, top_k[insert_idx - 1])
                            {
                                top_k[insert_idx] = top_k[insert_idx - 1];
                                insert_idx -= 1;
                            }
                            top_k[insert_idx] = candidate;
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

    Ok((indices, weights))
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

// El plan planetario limita la concurrencia de FRAMES por la RAM del scratch
// (~600 MiB/frame en una captura RGB 3312x5888), pero el render de un frame no
// necesita duplicar ese scratch: cada fila de salida es completamente
// independiente. Un pool propio evita heredar el pool acotado de frames (que
// puede tener sólo 1-2 workers) y ocupa los núcleos ociosos durante el hot path
// Lanczos. Es persistente para no crear hilos en cada frame y se comparte entre
// las dos llamadas que puedan llegar simultáneamente desde el pool exterior.
//
// Paridad: no existe reducción entre filas ni dos filas escriben el mismo
// píxel. Cada píxel ejecuta exactamente la misma secuencia escalar/SIMD que en
// el bucle secuencial; únicamente cambia qué worker posee la fila.
const PARALLEL_WARP_MIN_PIXELS: usize = 1_000_000;
static CPU_WARP_POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();

fn for_each_warp_row<F>(width: usize, height: usize, process: F)
where
    F: Fn(usize) + Send + Sync,
{
    let pixels = width.saturating_mul(height);
    // Override de diagnóstico/benchmark; no cambia el valor por defecto.
    let hardware_threads = std::env::var("ZAS_CPU_WARP_THREADS")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(1)
        })
        .clamp(1, 32);
    if pixels < PARALLEL_WARP_MIN_PIXELS || hardware_threads <= 1 {
        for y in 0..height {
            process(y);
        }
        return;
    }

    // Si el plan por fases ya pudo conceder al pool exterior al menos la
    // mitad de los núcleos, reutilizar ese pool evita crear 7+10 workers en
    // una máquina de 10 cores. Rayon resuelve el paralelismo anidado por
    // work-stealing: los workers que terminaron su frame ayudan con filas de
    // los otros. El pool dedicado se reserva para el caso problemático real
    // (1-2 frames concurrentes por un plan de RAM conservador).
    if rayon::current_thread_index().is_some()
        && rayon::current_num_threads() >= hardware_threads.div_ceil(2)
    {
        (0..height).into_par_iter().for_each(process);
        return;
    }

    let pool = CPU_WARP_POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(hardware_threads)
            .thread_name(|index| format!("zas-liquid-warp-{index}"))
            .build()
            .ok()
    });
    if let Some(pool) = pool {
        pool.install(|| (0..height).into_par_iter().for_each(process));
    } else {
        for y in 0..height {
            process(y);
        }
    }
}

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
    let wxs = _mm256_setr_ps(
        w_xs[0], w_xs[1], w_xs[2], w_xs[3], w_xs[4], w_xs[5], 0.0, 0.0,
    );
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

/// PR-2.2: espejo NEON del sampler RGB AVX2. En Apple Silicon el fallback
/// CPU del warp (canvas pequeño, GPU caída, ZAS_FORCE_CPU) corría 100 %
/// ESCALAR — la etapa por-frame más cara del pipeline sin vectorizar en la
/// plataforma objetivo de macOS. Misma disciplina que AVX2: mul+add (sin
/// FMA, que cambiaría el redondeo), lanes de relleno a 0 para las sumas y a
/// 65535 para el mínimo; min/max exactos, sumas <1 ULP vs escalar.
#[allow(clippy::type_complexity)]
#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn fast_sample_rgb_neon(
    rgb_buf: &[u16],
    w_in: usize,
    sx0: isize,
    sy0: isize,
    w_xs: &[f32; 6],
    fy: f32,
    lut: &LanczosLUT,
    drop_size: f32,
) -> (f32, f32, f32, f32, f32, f32, f32, f32, f32, f32) {
    use std::arch::aarch64::*;
    let wxs_lo = vld1q_f32(w_xs.as_ptr());
    let wxs_hi = vld1q_f32([w_xs[4], w_xs[5], 0.0f32, 0.0].as_ptr());
    let pad_hi = vdupq_n_f32(65535.0);
    let mut sum_r_lo = vdupq_n_f32(0.0);
    let mut sum_r_hi = vdupq_n_f32(0.0);
    let mut sum_g_lo = vdupq_n_f32(0.0);
    let mut sum_g_hi = vdupq_n_f32(0.0);
    let mut sum_b_lo = vdupq_n_f32(0.0);
    let mut sum_b_hi = vdupq_n_f32(0.0);
    let mut sum_w_lo = vdupq_n_f32(0.0);
    let mut sum_w_hi = vdupq_n_f32(0.0);
    let mut vmin_r = pad_hi;
    let mut vmin_g = pad_hi;
    let mut vmin_b = pad_hi;
    let mut vmax_r = vdupq_n_f32(0.0);
    let mut vmax_g = vdupq_n_f32(0.0);
    let mut vmax_b = vdupq_n_f32(0.0);
    for ky in -2..=3isize {
        let wy = lut.get(fy - ky as f32, drop_size);
        if wy.abs() < 0.001 {
            continue;
        }
        let src_row = (sy0 + ky) as usize * w_in;
        // base..base+18 son las 6 columnas RGB; índices garantizados en rango
        // por el guard del fast-path (sx0>=2, sx0+3<w_in, sy0>=2, sy0+3<h_in).
        //
        // Los primeros cuatro píxeles se cargan directamente desde el RGB
        // intercalado con LD3. La versión anterior hacía 12 gathers escalares,
        // construía tres arrays f32 temporales y después volvía a cargarlos en
        // NEON por CADA fila del kernel Lanczos. En un frame 3312x5888 eso se
        // repite cientos de millones de veces. LD3 entrega exactamente las
        // mismas lanes R/G/B; sólo elimina el de-interleave escalar y conserva
        // intactos el orden de productos y reducciones (paridad numérica).
        let base = (src_row + (sx0 - 2) as usize) * 3;
        let g = |o: usize| *rgb_buf.get_unchecked(base + o) as f32;
        let rgb4 = vld3_u16(rgb_buf.as_ptr().add(base));
        let rv_lo = vcvtq_f32_u32(vmovl_u16(rgb4.0));
        let gv_lo = vcvtq_f32_u32(vmovl_u16(rgb4.1));
        let bv_lo = vcvtq_f32_u32(vmovl_u16(rgb4.2));
        let r4 = g(12);
        let r5 = g(15);
        let g4 = g(13);
        let g5 = g(16);
        let b4 = g(14);
        let b5 = g(17);
        let rv_hi = vld1q_f32([r4, r5, 0.0f32, 0.0].as_ptr());
        let gv_hi = vld1q_f32([g4, g5, 0.0f32, 0.0].as_ptr());
        let bv_hi = vld1q_f32([b4, b5, 0.0f32, 0.0].as_ptr());
        let wyv = vdupq_n_f32(wy);
        let wf_lo = vmulq_f32(wxs_lo, wyv);
        let wf_hi = vmulq_f32(wxs_hi, wyv);
        sum_r_lo = vaddq_f32(sum_r_lo, vmulq_f32(rv_lo, wf_lo));
        sum_r_hi = vaddq_f32(sum_r_hi, vmulq_f32(rv_hi, wf_hi));
        sum_g_lo = vaddq_f32(sum_g_lo, vmulq_f32(gv_lo, wf_lo));
        sum_g_hi = vaddq_f32(sum_g_hi, vmulq_f32(gv_hi, wf_hi));
        sum_b_lo = vaddq_f32(sum_b_lo, vmulq_f32(bv_lo, wf_lo));
        sum_b_hi = vaddq_f32(sum_b_hi, vmulq_f32(bv_hi, wf_hi));
        sum_w_lo = vaddq_f32(sum_w_lo, wf_lo);
        sum_w_hi = vaddq_f32(sum_w_hi, wf_hi);
        // Mínimo: lanes de relleno neutralizados a 65535.
        vmin_r = vminq_f32(vmin_r, rv_lo);
        vmin_r = vminq_f32(vmin_r, vld1q_f32([r4, r5, 65535.0f32, 65535.0].as_ptr()));
        vmin_g = vminq_f32(vmin_g, gv_lo);
        vmin_g = vminq_f32(vmin_g, vld1q_f32([g4, g5, 65535.0f32, 65535.0].as_ptr()));
        vmin_b = vminq_f32(vmin_b, bv_lo);
        vmin_b = vminq_f32(vmin_b, vld1q_f32([b4, b5, 65535.0f32, 65535.0].as_ptr()));
        vmax_r = vmaxq_f32(vmax_r, rv_lo);
        vmax_r = vmaxq_f32(vmax_r, rv_hi);
        vmax_g = vmaxq_f32(vmax_g, gv_lo);
        vmax_g = vmaxq_f32(vmax_g, gv_hi);
        vmax_b = vmaxq_f32(vmax_b, bv_lo);
        vmax_b = vmaxq_f32(vmax_b, bv_hi);
    }
    (
        vaddvq_f32(sum_r_lo) + vaddvq_f32(sum_r_hi),
        vaddvq_f32(sum_g_lo) + vaddvq_f32(sum_g_hi),
        vaddvq_f32(sum_b_lo) + vaddvq_f32(sum_b_hi),
        vaddvq_f32(sum_w_lo) + vaddvq_f32(sum_w_hi),
        vminvq_f32(vmin_r),
        vminvq_f32(vmin_g),
        vminvq_f32(vmin_b),
        vmaxvq_f32(vmax_r),
        vmaxvq_f32(vmax_g),
        vmaxvq_f32(vmax_b),
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
    #[cfg(target_arch = "aarch64")]
    {
        // NEON es baseline en aarch64: sin detección en runtime.
        return unsafe { fast_sample_rgb_neon(rgb_buf, w_in, sx0, sy0, w_xs, fy, lut, drop_size) };
    }
    #[cfg(not(target_arch = "aarch64"))]
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

    // Punteros enteros: `for_each_warp_row` reparte filas disjuntas. Las Vec
    // pertenecen al scratch del frame y no se redimensionan durante el render.
    let acc_r_ptr = acc_r.as_mut_ptr() as usize;
    let acc_g_ptr = acc_g.as_mut_ptr() as usize;
    let acc_b_ptr = acc_b.as_mut_ptr() as usize;
    let acc_w_ptr = acc_w.as_mut_ptr() as usize;
    let acc_len = acc_r
        .len()
        .min(acc_g.len())
        .min(acc_b.len())
        .min(acc_w.len());
    for_each_warp_row(w_out, h_out, |y_out| {
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
                    if tidx < acc_len {
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
                            *(acc_r_ptr as *mut f32).add(tidx) += dr * inv * wq;
                            *(acc_g_ptr as *mut f32).add(tidx) += dg * inv * wq;
                            *(acc_b_ptr as *mut f32).add(tidx) += db * inv * wq;
                            *(acc_w_ptr as *mut f32).add(tidx) += wq;
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
                if tidx < acc_len {
                    unsafe {
                        *(acc_r_ptr as *mut f32).add(tidx) += pr * pixel_weight;
                        *(acc_g_ptr as *mut f32).add(tidx) += pg * pixel_weight;
                        *(acc_b_ptr as *mut f32).add(tidx) += pb * pixel_weight;
                        *(acc_w_ptr as *mut f32).add(tidx) += pixel_weight;
                    }
                }
            }
        }
    });
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
    let wxs = _mm256_setr_ps(
        w_xs[0], w_xs[1], w_xs[2], w_xs[3], w_xs[4], w_xs[5], 0.0, 0.0,
    );
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

/// PR-2.2: espejo NEON del sampler mono AVX2 (ver fast_sample_rgb_neon).
/// El mono es el caso más común en planetaria/solar: era la pérdida escalar
/// más grave del fallback CPU en Apple Silicon.
#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn fast_sample_mono_neon(
    mono_buf: &[u16],
    w_in: usize,
    sx0: isize,
    sy0: isize,
    w_xs: &[f32; 6],
    fy: f32,
    lut: &LanczosLUT,
    drop_size: f32,
) -> (f32, f32, f32, f32) {
    use std::arch::aarch64::*;
    let wxs_lo = vld1q_f32(w_xs.as_ptr());
    let wxs_hi = vld1q_f32([w_xs[4], w_xs[5], 0.0f32, 0.0].as_ptr());
    let mut sum_v_lo = vdupq_n_f32(0.0);
    let mut sum_v_hi = vdupq_n_f32(0.0);
    let mut sum_w_lo = vdupq_n_f32(0.0);
    let mut sum_w_hi = vdupq_n_f32(0.0);
    let mut vmin = vdupq_n_f32(65535.0);
    let mut vmax = vdupq_n_f32(0.0);
    let base = (sx0 - 2) as usize;
    for ky in -2..=3isize {
        let wy = lut.get(fy - ky as f32, drop_size);
        if wy.abs() < 0.001 {
            continue;
        }
        let p = mono_buf.as_ptr().add((sy0 + ky) as usize * w_in + base);
        // Carga de EXACTAMENTE 6 u16 (4 vectorizados + 2 escalares): el guard
        // del fast-path solo garantiza índices [sx0-2 .. sx0+3].
        let pv_lo = vcvtq_f32_u32(vmovl_u16(vld1_u16(p)));
        let p4 = *p.add(4) as f32;
        let p5 = *p.add(5) as f32;
        let pv_hi = vld1q_f32([p4, p5, 0.0f32, 0.0].as_ptr());
        let wyv = vdupq_n_f32(wy);
        let wf_lo = vmulq_f32(wxs_lo, wyv);
        let wf_hi = vmulq_f32(wxs_hi, wyv);
        sum_v_lo = vaddq_f32(sum_v_lo, vmulq_f32(pv_lo, wf_lo));
        sum_v_hi = vaddq_f32(sum_v_hi, vmulq_f32(pv_hi, wf_hi));
        sum_w_lo = vaddq_f32(sum_w_lo, wf_lo);
        sum_w_hi = vaddq_f32(sum_w_hi, wf_hi);
        vmin = vminq_f32(vmin, pv_lo);
        vmin = vminq_f32(vmin, vld1q_f32([p4, p5, 65535.0f32, 65535.0].as_ptr()));
        vmax = vmaxq_f32(vmax, pv_lo);
        vmax = vmaxq_f32(vmax, pv_hi); // lanes de relleno = 0, neutros (datos >= 0)
    }
    (
        vaddvq_f32(sum_v_lo) + vaddvq_f32(sum_v_hi),
        vaddvq_f32(sum_w_lo) + vaddvq_f32(sum_w_hi),
        vminvq_f32(vmin),
        vmaxvq_f32(vmax),
    )
}

/// Despacho: AVX2 en x86_64, NEON en aarch64 (baseline, sin detección),
/// escalar como red de seguridad (mismo resultado salvo <1 ULP en la suma).
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
    #[cfg(target_arch = "aarch64")]
    {
        return unsafe {
            fast_sample_mono_neon(mono_buf, w_in, sx0, sy0, w_xs, fy, lut, drop_size)
        };
    }
    #[cfg(not(target_arch = "aarch64"))]
    fast_sample_mono_scalar(mono_buf, w_in, sx0, sy0, w_xs, fy, lut, drop_size)
}

/// Bounds of the non-negative 2x2 reconstruction footprint around a mono
/// sample. Lanczos-3 remains the detail-preserving interpolator, but limiting
/// its result to these immediate neighbours makes it monotonicity-preserving:
/// distant negative lobes cannot create a second bright/dark contour around a
/// high-contrast limb. Coordinates outside the frame use edge replication,
/// matching the only physically available sample at that boundary.
#[inline(always)]
fn mono_linear_support_bounds(
    mono_buf: &[u16],
    w_in: usize,
    h_in: usize,
    sx0: isize,
    sy0: isize,
) -> (f32, f32) {
    debug_assert!(w_in > 0 && h_in > 0 && mono_buf.len() >= w_in * h_in);
    let last_x = w_in.saturating_sub(1) as isize;
    let last_y = h_in.saturating_sub(1) as isize;
    let x0 = sx0.clamp(0, last_x) as usize;
    let x1 = (sx0 + 1).clamp(0, last_x) as usize;
    let y0 = sy0.clamp(0, last_y) as usize;
    let y1 = (sy0 + 1).clamp(0, last_y) as usize;
    let values = [
        mono_buf[y0 * w_in + x0],
        mono_buf[y0 * w_in + x1],
        mono_buf[y1 * w_in + x0],
        mono_buf[y1 * w_in + x1],
    ];
    let mut min_v = u16::MAX;
    let mut max_v = u16::MIN;
    for value in values {
        min_v = min_v.min(value);
        max_v = max_v.max(value);
    }
    (min_v as f32, max_v as f32)
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

    let acc_ptr = acc.as_mut_ptr() as usize;
    let acc_w_ptr = acc_w.as_mut_ptr() as usize;
    let acc_len = acc.len().min(acc_w.len());
    for_each_warp_row(w_out, h_out, |y_out| {
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
                    if tidx < acc_len {
                        let cov = (dw / drz_full_cov).min(1.0);
                        let wq = pixel_weight * cov;
                        unsafe {
                            *(acc_ptr as *mut f32).add(tidx) += (dv / dw) * wq;
                            *(acc_w_ptr as *mut f32).add(tidx) += wq;
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

            let (sum_v, sum_w, _wide_min_v, _wide_max_v) =
                if sx0 >= 2 && sx0 + 3 < w_in as isize && sy0 >= 2 && sy0 + 3 < h_in as isize {
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
                // MONO EDGE-SAFE LANCZOS: the former 6x6 ±18% allowance let
                // negative Lanczos lobes become displaced contours at a solar
                // limb. Preserve the Lanczos estimate whenever it lies inside
                // the immediate 2x2 source support, otherwise clip only the
                // invented extremum. RGB keeps its historical policy above.
                let (support_min, support_max) =
                    mono_linear_support_bounds(mono_buf, w_in, h_in, sx0, sy0);
                let val = (sum_v / sum_w).clamp(support_min, support_max);
                let tidx = row_off + x_out;
                if tidx < acc_len {
                    unsafe {
                        *(acc_ptr as *mut f32).add(tidx) += val * pixel_weight;
                        *(acc_w_ptr as *mut f32).add(tidx) += pixel_weight;
                    }
                }
            }
        }
    });
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
                        let sc = fast_sample_mono_scalar(
                            &buf, w_in, sx0, sy0, &w_xs, fy, &lut, drop_size,
                        );
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
                        let sc = fast_sample_rgb_scalar(
                            &buf, w_in, sx0, sy0, &w_xs, fy, &lut, drop_size,
                        );
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
        assert!(
            max_diff_sum < 0.5,
            "suma RGB difiere demasiado: {max_diff_sum}"
        );
    }
}

// ===========================================================================
// Validación del DRIZZLE VERDADERO (kernel drop). Independiente de la
// arquitectura (el kernel drop es escalar puro).
// ===========================================================================
#[cfg(test)]
mod drizzle_tests {
    use super::*;

    #[test]
    fn parallel_warp_scheduler_visits_every_row_exactly_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let height = 1024usize;
        let visits: Vec<AtomicUsize> = (0..height).map(|_| AtomicUsize::new(0)).collect();
        // 1024² supera deliberadamente PARALLEL_WARP_MIN_PIXELS.
        for_each_warp_row(1024, height, |y| {
            visits[y].fetch_add(1, Ordering::Relaxed);
        });
        assert!(visits
            .iter()
            .all(|count| count.load(Ordering::Relaxed) == 1));
    }

    #[test]
    fn idw_rejects_overflow_before_allocating_large_planes() {
        let points = [ApPoint {
            x: 0.0,
            y: 0.0,
            size: 16,
        }];
        let error =
            compute_idw_map_for_output(usize::MAX, 2, 1.0, 0.0, 0.0, &points, 1.5, 1).unwrap_err();
        assert!(error.contains("espacio direccionable"), "{error}");
    }

    #[test]
    fn idw_rejects_empty_output_geometry() {
        let points = [ApPoint {
            x: 0.0,
            y: 0.0,
            size: 16,
        }];
        let error = compute_idw_map_for_output(0, 8, 1.0, 0.0, 0.0, &points, 1.5, 1).unwrap_err();
        assert!(error.contains("geometría vacía"), "{error}");
    }

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
        let grid = ApSpatialGrid::build(&points).unwrap();

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
                let candidate = (d2, i as u16);
                if idw_neighbor_before(candidate, want[k - 1]) {
                    let mut ins = k - 1;
                    while ins > 0 && idw_neighbor_before(candidate, want[ins - 1]) {
                        want[ins] = want[ins - 1];
                        ins -= 1;
                    }
                    want[ins] = candidate;
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

    #[test]
    fn idw_spatial_grid_ties_match_linear_index_order() {
        // >96 activa la rejilla. Los primeros cuatro AP son perfectamente
        // simétricos respecto al query; los restantes están lejos y fuerzan
        // un recorrido de varias celdas en un orden distinto al de sus índices.
        let mut points = vec![
            ApPoint {
                x: 90.0,
                y: 100.0,
                size: 32,
            },
            ApPoint {
                x: 110.0,
                y: 100.0,
                size: 32,
            },
            ApPoint {
                x: 100.0,
                y: 90.0,
                size: 32,
            },
            ApPoint {
                x: 100.0,
                y: 110.0,
                size: 32,
            },
        ];
        points.extend((0..100).map(|index| ApPoint {
            x: 300.0 + (index % 10) as f32 * 20.0,
            y: 300.0 + (index / 10) as f32 * 20.0,
            size: 32,
        }));
        let grid = ApSpatialGrid::build(&points).unwrap();
        let mut got = [(f32::MAX, 0u16); 8];
        grid.top_k_into(100.0, 100.0, 4, &mut got, &points);
        assert_eq!(
            got[..4].iter().map(|entry| entry.1).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
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
            &mut acc,
            &mut acc_w,
            &mono,
            w_in,
            h_in,
            w_out,
            h_out,
            drizzle,
            0.0,
            0.0,
            0.0,
            0.0,
            &[],
            &[],
            &[],
            &[],
            &[],
            1.0,
            0.75,
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

    /// Un limbo solar mono es, localmente, un escalón de alto contraste. Al
    /// corregir un desplazamiento subpíxel, el remuestreo no debe inventar
    /// lóbulos oscuros/brillantes fuera del soporte bilineal inmediato: esos
    /// lóbulos se ven como contornos separados del disco al acumular frames.
    #[test]
    fn mono_lanczos_fractional_limb_is_monotone_and_bounded() {
        let (w, h) = (64usize, 32usize);
        let sky = 1_000u16;
        let disc = 60_000u16;
        let edge_x = 32usize;
        let mut mono = vec![sky; w * h];
        for row in mono.chunks_exact_mut(w) {
            row[edge_x..].fill(disc);
        }

        let mut acc = vec![0.0f32; w * h];
        let mut acc_w = vec![0.0f32; w * h];
        accumulate_frame_liquid_mono(
            &mut acc,
            &mut acc_w,
            &mono,
            w,
            h,
            w,
            h,
            1.0,
            0.0,
            0.0,
            0.37,
            0.0,
            &[],
            &[],
            &[],
            &[],
            &[],
            1.0,
            1.0,
        );

        let y = h / 2;
        let profile: Vec<f32> = (edge_x - 5..=edge_x + 4)
            .map(|x| {
                let index = y * w + x;
                assert!(acc_w[index] > 0.0, "pixel sin cobertura en x={x}");
                acc[index] / acc_w[index]
            })
            .collect();

        for (offset, &value) in profile.iter().enumerate() {
            let x = edge_x - 5 + offset;
            assert!(
                value >= sky as f32 - 0.5 && value <= disc as f32 + 0.5,
                "ringing fuera del rango fuente en x={x}: {value}; perfil={profile:?}"
            );
        }
        for pair in profile.windows(2) {
            assert!(
                pair[1] + 0.5 >= pair[0],
                "contorno no monótono alrededor del limbo: {profile:?}"
            );
        }
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
            (0.0, 0.0),
            (1.0 / 3.0, 0.0),
            (2.0 / 3.0, 0.0),
            (0.0, 1.0 / 3.0),
            (1.0 / 3.0, 1.0 / 3.0),
            (2.0 / 3.0, 1.0 / 3.0),
            (0.0, 2.0 / 3.0),
            (1.0 / 3.0, 2.0 / 3.0),
            (2.0 / 3.0, 2.0 / 3.0),
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
                &mut acc,
                &mut acc_w,
                &frame,
                w_in,
                h_in,
                w_out,
                h_out,
                drizzle,
                0.0,
                0.0,
                dx,
                dy,
                &[],
                &[],
                &[],
                &[],
                &[],
                1.0,
                0.75,
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

// ===========================================================================
// PR-2.2: el fast-path NEON debe coincidir con el escalar (mismo contrato que
// la validación AVX2 de arriba): min/max EXACTOS; sumas <1 ULP por el
// reordenamiento. Solo se compila/ejecuta en aarch64.
// ===========================================================================
#[cfg(all(test, target_arch = "aarch64"))]
mod simd_validation_neon {
    use super::*;

    #[test]
    fn mono_fast_sample_neon_matches_scalar() {
        let w_in = 96usize;
        let h_in = 96usize;
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
                        let sc = fast_sample_mono_scalar(
                            &buf, w_in, sx0, sy0, &w_xs, fy, &lut, drop_size,
                        );
                        let si = unsafe {
                            fast_sample_mono_neon(&buf, w_in, sx0, sy0, &w_xs, fy, &lut, drop_size)
                        };
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
            "NEON mono validado en {checked} casos. max_diff sum_v={max_diff_v}, sum_w={max_diff_w}"
        );
        assert!(max_diff_v < 0.5, "sum_v difiere demasiado: {max_diff_v}");
        assert!(max_diff_w < 1e-3, "sum_w difiere demasiado: {max_diff_w}");
    }

    #[test]
    fn rgb_fast_sample_neon_matches_scalar() {
        let w_in = 80usize;
        let h_in = 80usize;
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
                        let sc = fast_sample_rgb_scalar(
                            &buf, w_in, sx0, sy0, &w_xs, fy, &lut, drop_size,
                        );
                        let si = unsafe {
                            fast_sample_rgb_neon(&buf, w_in, sx0, sy0, &w_xs, fy, &lut, drop_size)
                        };
                        let sc_mm = [sc.4, sc.5, sc.6, sc.7, sc.8, sc.9];
                        let si_mm = [si.4, si.5, si.6, si.7, si.8, si.9];
                        for j in 0..6 {
                            assert_eq!(
                                sc_mm[j], si_mm[j],
                                "min/max idx {j} difiere en sx0={sx0} sy0={sy0}"
                            );
                        }
                        let sc_s = [sc.0, sc.1, sc.2, sc.3];
                        let si_s = [si.0, si.1, si.2, si.3];
                        for j in 0..4 {
                            max_diff_sum = max_diff_sum.max((sc_s[j] - si_s[j]).abs());
                        }
                        checked += 1;
                    }
                }
            }
        }
        eprintln!("NEON RGB validado en {checked} casos. max_diff={max_diff_sum}");
        assert!(
            max_diff_sum < 0.5,
            "sumas difieren demasiado: {max_diff_sum}"
        );
    }

    /// Benchmark manual reproducible del caso reportado (19.5 Mpx RGB).
    /// Ejecutar dos procesos separados para comparar el mismo binario:
    /// `ZAS_CPU_WARP_THREADS=1 cargo test --release benchmark_surface_rgb_3312x5888 -- --ignored --nocapture`
    /// y luego sin el override. Se ignora en CI por su pico aproximado de
    /// 430 MiB; no incluye debayer/alineación, sólo el hot path warp Lanczos.
    #[test]
    #[ignore = "benchmark manual de 19.5 Mpx / ~430 MiB"]
    fn benchmark_surface_rgb_3312x5888() {
        let (w, h) = (3312usize, 5888usize);
        let n = w * h;
        let mut rgb = vec![0u16; n * 3];
        for (i, pixel) in rgb.chunks_exact_mut(3).enumerate() {
            let base = ((i as u64 * 1103 + (i / w) as u64 * 7919) & 0xffff) as u16;
            pixel[0] = base;
            pixel[1] = base.wrapping_add(733);
            pixel[2] = base.wrapping_add(1901);
        }
        let mut r = vec![0.0f32; n];
        let mut g = vec![0.0f32; n];
        let mut b = vec![0.0f32; n];
        let mut weights = vec![0.0f32; n];
        let started = std::time::Instant::now();
        accumulate_frame_liquid(
            &mut r,
            &mut g,
            &mut b,
            &mut weights,
            &rgb,
            w,
            h,
            w,
            h,
            1.0,
            0.0,
            0.0,
            0.0,
            0.0,
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            1.0,
            1.0,
        );
        let elapsed = started.elapsed().as_secs_f64();
        let checksum = r[n / 3] as f64 + g[n / 2] as f64 + b[n * 2 / 3] as f64;
        eprintln!(
            "warp RGB 3312x5888: {:.3}s, {:.2} Mpx/s, {:.2} fps, threads={}, checksum={checksum:.1}",
            elapsed,
            n as f64 / elapsed / 1.0e6,
            1.0 / elapsed,
            std::env::var("ZAS_CPU_WARP_THREADS").unwrap_or_else(|_| "auto".into()),
        );
        assert!(checksum.is_finite() && checksum > 0.0);
    }
}
