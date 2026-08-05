//! Fondo y contaminación lumínica (F2 del plan NebulaFusion/EIDR).
//!
//! Tres piezas, todas NO destructivas (el máster clásico no cambia):
//!
//! 1. **Grafo de fondo entre frames**: resuelve offsets (y opcionalmente
//!    planos de primer orden) relativos `b_i` minimizando
//!    `Σ ω_ij·[(b_i − b_j) − d_ij]²` con gauge `Σ b_i = 0`, para que ninguna
//!    referencia única imprima su ruido/gradiente al resto (§4.3 del plan
//!    técnico). Los `d_ij` se miden de forma robusta (mediana) sobre celdas
//!    de fondo comunes en el espacio de referencia.
//! 2. **Modelo BG de sesión**: ajuste polinómico de grado ≤2 del fondo del
//!    máster con las MISMAS muestras y rechazo robusto que
//!    `ds_extract_background_gradient`, pero devolviendo el MODELO en vez de
//!    restarlo — para exportar `*_BG.fits` reversible, para la vista de
//!    diagnóstico y para la validación split-half.
//! 3. **Asesor de muestreo**: clasifica el muestreo (FWHM mediana en px) en
//!    submuestreado / bien muestreado / sobremuestreado y recomienda escala
//!    de salida (2x/1.5x/1x/0.75x/0.5x). Cubre el hueco de sobremuestreo del
//!    documento técnico.

#![allow(dead_code)] // API consumida progresivamente por F3 (NF-Lite)

// ---------------------------------------------------------------------------
// 1. Grafo de fondo entre frames
// ---------------------------------------------------------------------------

/// Mínimo de celdas comunes para aceptar una arista entre dos frames.
const MIN_COMMON_CELLS: usize = 12;
/// Límites explícitos del API productivo. Mantienen acotadas la matriz densa
/// del grafo (3·frames en primer orden) y las rejillas de salida.
const MAX_LOCAL_NORM_FRAMES: usize = 512;
const MAX_LOCAL_NORM_CHANNELS: usize = 4;
const MAX_LOCAL_NORM_GRID_CELLS: usize = 4_096;
const MAX_LOCAL_NORM_FIELD_SAMPLES: usize =
    MAX_LOCAL_NORM_FRAMES * MAX_LOCAL_NORM_CHANNELS * MAX_LOCAL_NORM_GRID_CELLS;
const MAX_NOISE_DIFFERENCES: usize = 131_072;

#[derive(Debug)]
pub(crate) struct BackgroundGraphSolution {
    /// Offset aditivo por frame (gauge Σ=0 POR COMPONENTE conexa). Restar
    /// `offsets[i]` del frame i iguala los fondos sin elegir referencia.
    pub offsets: Vec<f64>,
    /// Extensión de primer orden por frame: `[c0, cx, cy]` en coordenadas de
    /// celda normalizadas (0..1). `None` si se resolvió solo el offset.
    pub planes: Option<Vec<[f64; 3]>>,
    /// Aristas aceptadas (pares con solape suficiente).
    pub edges: usize,
    /// Componentes conexas del grafo. Con más de una (sesiones/paneles sin
    /// solape) los offsets ENTRE componentes son indeterminables por
    /// definición: cada componente queda con media 0 y el caller debe avisar.
    pub components: usize,
    /// Identificador determinista 0..components por frame. Permite conservar
    /// el gauge separado cuando dos paneles/sesiones no tienen solape.
    pub component_ids: Vec<usize>,
    /// RMS de los residuales de celda tras aplicar la solución — la
    /// "costura" restante entre frames, en las unidades de las muestras.
    pub rms_residual: f64,
}

/// Resuelve el grafo de fondo. `samples[i]` es la rejilla de fondo del frame
/// `i` medida en el ESPACIO DE REFERENCIA: `gw*gh` celdas, `None` donde el
/// frame no cubre la celda o la celda está enmascarada por objeto. Con
/// `first_order` se resuelve además un plano por frame (útil cuando la
/// contaminación lumínica rota o deriva entre sesiones).
pub(crate) fn solve_background_graph(
    samples: &[Vec<Option<f64>>],
    gw: usize,
    gh: usize,
    first_order: bool,
) -> Option<BackgroundGraphSolution> {
    let n = samples.len();
    let cells = gw * gh;
    if n < 2 || cells == 0 || samples.iter().any(|s| s.len() != cells) {
        return None;
    }

    // Diferencias por arista. Para el modo constante basta la mediana de las
    // diferencias; para primer orden se ajusta un plano por arista y se
    // acumulan las ecuaciones normales por bloques 3×3.
    let unknowns_per_frame = if first_order { 3 } else { 1 };
    let dim = n * unknowns_per_frame;
    let mut a = vec![vec![0.0f64; dim + 1]; dim];
    let mut edges = 0usize;
    let mut total_weight = 0.0f64;
    // Union-find para detectar componentes conexas del grafo de solapes.
    let mut parent: Vec<usize> = (0..n).collect();
    fn uf_find(parent: &mut Vec<usize>, mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }

    let cell_xy = |cell: usize| -> (f64, f64) {
        let cx = (cell % gw) as f64 + 0.5;
        let cy = (cell / gw) as f64 + 0.5;
        (cx / gw as f64, cy / gh as f64)
    };

    for i in 0..n {
        for j in (i + 1)..n {
            // Celdas comunes.
            let mut diffs: Vec<(f64, f64, f64)> = Vec::new(); // (xn, yn, si−sj)
            for cell in 0..cells {
                if let (Some(vi), Some(vj)) = (samples[i][cell], samples[j][cell]) {
                    if !vi.is_finite() || !vj.is_finite() {
                        continue;
                    }
                    let (xn, yn) = cell_xy(cell);
                    diffs.push((xn, yn, vi - vj));
                }
            }
            if diffs.len() < MIN_COMMON_CELLS {
                continue;
            }
            edges += 1;
            let (ri, rj) = (uf_find(&mut parent, i), uf_find(&mut parent, j));
            if ri != rj {
                parent[ri] = rj;
            }
            let w = diffs.len() as f64;
            total_weight += w;

            if first_order {
                // Gram 3×3 de la base [1, x, y] y proyección de la diferencia.
                let mut g = [[0.0f64; 3]; 3];
                let mut r = [0.0f64; 3];
                for &(xn, yn, d) in &diffs {
                    let phi = [1.0, xn, yn];
                    for u in 0..3 {
                        for v in 0..3 {
                            g[u][v] += phi[u] * phi[v];
                        }
                        r[u] += phi[u] * d;
                    }
                }
                let (bi, bj) = (i * 3, j * 3);
                for u in 0..3 {
                    for v in 0..3 {
                        a[bi + u][bi + v] += g[u][v];
                        a[bj + u][bj + v] += g[u][v];
                        a[bi + u][bj + v] -= g[u][v];
                        a[bj + u][bi + v] -= g[u][v];
                    }
                    a[bi + u][dim] += r[u];
                    a[bj + u][dim] -= r[u];
                }
            } else {
                // Mediana robusta de las diferencias (satélites/estrellas en
                // celdas mal enmascaradas no arrastran la arista).
                let mut ds: Vec<f64> = diffs.iter().map(|&(_, _, d)| d).collect();
                ds.sort_by(|x, y| x.total_cmp(y));
                let dij = ds[ds.len() / 2];
                a[i][i] += w;
                a[j][j] += w;
                a[i][j] -= w;
                a[j][i] -= w;
                a[i][dim] += w * dij;
                a[j][dim] -= w * dij;
            }
        }
    }
    if edges == 0 {
        return None;
    }

    // Gauge Σ b = 0 POR COMPONENTE CONEXA: una penalización global (λ·J sobre
    // todos los frames) dejaría el sistema singular con grafo desconectado
    // (dos sesiones/paneles sin solape: el offset ENTRE componentes es
    // indeterminable). Con λ·J por componente cada una queda con media 0 y el
    // lado derecho antisimétrico garantiza 1ᵀc = 0 dentro de cada componente.
    let roots: Vec<usize> = (0..n).map(|i| uf_find(&mut parent, i)).collect();
    let mut component_roots: Vec<usize> = roots.clone();
    component_roots.sort_unstable();
    component_roots.dedup();
    let components = component_roots.len();
    let component_ids: Vec<usize> = roots
        .iter()
        .map(|root| {
            component_roots
                .binary_search(root)
                .expect("la raíz union-find debe existir")
        })
        .collect();
    let lambda = (total_weight / n as f64).max(1e-9);
    for comp in 0..unknowns_per_frame {
        for fi in 0..n {
            for fj in 0..n {
                if roots[fi] != roots[fj] {
                    continue;
                }
                a[fi * unknowns_per_frame + comp][fj * unknowns_per_frame + comp] += lambda;
            }
        }
    }

    let solution = crate::ds_solve_linear_n(&mut a, dim)?;

    let (offsets, planes) = if first_order {
        let planes: Vec<[f64; 3]> = (0..n)
            .map(|i| [solution[i * 3], solution[i * 3 + 1], solution[i * 3 + 2]])
            .collect();
        // El offset equivalente en el centro del campo (x=y=0.5).
        let offsets = planes
            .iter()
            .map(|p| p[0] + 0.5 * p[1] + 0.5 * p[2])
            .collect();
        (offsets, Some(planes))
    } else {
        (solution.clone(), None)
    };

    // Costura residual: diferencias de celda tras aplicar la corrección.
    let mut sq_sum = 0.0f64;
    let mut count = 0usize;
    let eval = |frame: usize, xn: f64, yn: f64| -> f64 {
        match &planes {
            Some(p) => p[frame][0] + p[frame][1] * xn + p[frame][2] * yn,
            None => offsets[frame],
        }
    };
    for i in 0..n {
        for j in (i + 1)..n {
            let common = (0..cells)
                .filter(|&cell| {
                    matches!(
                        (samples[i][cell], samples[j][cell]),
                        (Some(vi), Some(vj)) if vi.is_finite() && vj.is_finite()
                    )
                })
                .count();
            if common < MIN_COMMON_CELLS {
                continue;
            }
            for cell in 0..cells {
                if let (Some(vi), Some(vj)) = (samples[i][cell], samples[j][cell]) {
                    if !vi.is_finite() || !vj.is_finite() {
                        continue;
                    }
                    let (xn, yn) = cell_xy(cell);
                    let res = (vi - eval(i, xn, yn)) - (vj - eval(j, xn, yn));
                    sq_sum += res * res;
                    count += 1;
                }
            }
        }
    }
    let rms_residual = if count > 0 {
        (sq_sum / count as f64).sqrt()
    } else {
        0.0
    };

    Some(BackgroundGraphSolution {
        offsets,
        planes,
        edges,
        components,
        component_ids,
        rms_residual,
    })
}

/// Frame lineal ya registrado en el canvas común. `coverage` tiene un byte por
/// píxel (cero = sin cobertura); no se infiere cobertura desde SCI porque cero
/// y los valores negativos son muestras calibradas perfectamente válidas.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RegisteredBackgroundFrame<'a> {
    pub data: &'a [f32],
    pub coverage: Option<&'a [u8]>,
}

/// Configuración acotada de normalización local. `first_order=true` produce
/// campos suaves capaces de igualar gradientes lineales rotados entre sesiones;
/// `false` conserva el mismo contrato con un offset constante por frame/canal.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SymmetricLocalNormalizationConfig {
    pub grid_width: usize,
    pub grid_height: usize,
    pub first_order: bool,
}

impl Default for SymmetricLocalNormalizationConfig {
    fn default() -> Self {
        Self {
            grid_width: 24,
            grid_height: 24,
            first_order: true,
        }
    }
}

/// QA por canal. El RMS se mide sólo sobre aristas aceptadas del grafo; nunca
/// compara componentes cuyo nivel absoluto es indeterminable.
#[derive(Clone, Debug)]
pub(crate) struct LocalNormalizationChannelMetrics {
    pub edges: usize,
    pub components: usize,
    pub component_ids: Vec<usize>,
    pub rms_seam: f64,
    /// Estimación MAD de ruido de alta frecuencia en las entradas registradas.
    pub noise_sigma: f64,
    /// `rms_seam/noise_sigma`; el gate científico objetivo es <0.2.
    pub seam_sigma_ratio: f64,
}

/// Campos aditivos simétricos. El layout es frame-major/canal-major:
/// `((frame*channels + channel)*grid_height + gy)*grid_width + gx`.
/// Aplicación: `SCI_normalizada = SCI_escalada + campo_aditivo`; el factor
/// fotométrico, si existe, debe aplicarse antes. Ningún frame es referencia.
#[derive(Debug)]
pub(crate) struct SymmetricLocalNormalization {
    pub frame_count: usize,
    pub channels: usize,
    pub grid_width: usize,
    pub grid_height: usize,
    pub additive_fields: Vec<f32>,
    pub channel_metrics: Vec<LocalNormalizationChannelMetrics>,
}

impl SymmetricLocalNormalization {
    pub(crate) fn field(&self, frame: usize, channel: usize) -> Option<&[f32]> {
        if frame >= self.frame_count || channel >= self.channels {
            return None;
        }
        let cells = self.grid_width.checked_mul(self.grid_height)?;
        let start = frame
            .checked_mul(self.channels)?
            .checked_add(channel)?
            .checked_mul(cells)?;
        self.additive_fields.get(start..start + cells)
    }

    /// Muestreo bilineal compatible con las rejillas de integración actuales.
    pub(crate) fn sample_additive(
        &self,
        frame: usize,
        channel: usize,
        u: f32,
        v: f32,
    ) -> Option<f32> {
        let grid = self.field(frame, channel)?;
        let gw = self.grid_width;
        let gh = self.grid_height;
        let fx = u.clamp(0.0, 1.0) * (gw as f32 - 1.0);
        let fy = v.clamp(0.0, 1.0) * (gh as f32 - 1.0);
        let x0 = (fx.floor() as usize).min(gw - 1);
        let y0 = (fy.floor() as usize).min(gh - 1);
        let x1 = (x0 + 1).min(gw - 1);
        let y1 = (y0 + 1).min(gh - 1);
        let tx = fx - x0 as f32;
        let ty = fy - y0 as f32;
        let top = grid[y0 * gw + x0] * (1.0 - tx) + grid[y0 * gw + x1] * tx;
        let bottom = grid[y1 * gw + x0] * (1.0 - tx) + grid[y1 * gw + x1] * tx;
        Some(top * (1.0 - ty) + bottom * ty)
    }
}

fn validate_local_normalization_inputs(
    frames: &[RegisteredBackgroundFrame<'_>],
    w: usize,
    h: usize,
    ch: usize,
    config: SymmetricLocalNormalizationConfig,
) -> Result<(usize, usize), String> {
    if frames.len() < 2 || frames.len() > MAX_LOCAL_NORM_FRAMES {
        return Err(format!(
            "normalización local requiere 2..={MAX_LOCAL_NORM_FRAMES} frames; recibió {}",
            frames.len()
        ));
    }
    if ch == 0 || ch > MAX_LOCAL_NORM_CHANNELS {
        return Err(format!(
            "normalización local admite 1..={MAX_LOCAL_NORM_CHANNELS} canales; recibió {ch}"
        ));
    }
    let pixels = w
        .checked_mul(h)
        .filter(|&count| count > 0)
        .ok_or_else(|| "geometría de normalización local vacía o fuera de rango".to_string())?;
    let samples_per_frame = pixels
        .checked_mul(ch)
        .ok_or_else(|| "geometría multicanal fuera de rango".to_string())?;
    let cells = config
        .grid_width
        .checked_mul(config.grid_height)
        .filter(|&count| count >= MIN_COMMON_CELLS && count <= MAX_LOCAL_NORM_GRID_CELLS)
        .ok_or_else(|| {
            format!(
                "rejilla local debe contener {MIN_COMMON_CELLS}..={MAX_LOCAL_NORM_GRID_CELLS} celdas"
            )
        })?;
    if config.grid_width < 2
        || config.grid_height < 2
        || config.grid_width > w
        || config.grid_height > h
    {
        return Err(format!(
            "rejilla {}x{} incompatible con canvas {w}x{h}",
            config.grid_width, config.grid_height
        ));
    }
    let output_samples = frames
        .len()
        .checked_mul(ch)
        .and_then(|count| count.checked_mul(cells))
        .filter(|&count| count <= MAX_LOCAL_NORM_FIELD_SAMPLES)
        .ok_or_else(|| "campos de normalización local exceden el límite acotado".to_string())?;
    for (index, frame) in frames.iter().enumerate() {
        if frame.data.len() != samples_per_frame {
            return Err(format!(
                "frame registrado {index}: {} muestras, esperadas {samples_per_frame}",
                frame.data.len()
            ));
        }
        if let Some(coverage) = frame.coverage {
            if coverage.len() != pixels {
                return Err(format!(
                    "cobertura del frame {index}: {} muestras, esperadas {pixels}",
                    coverage.len()
                ));
            }
        }
    }
    Ok((cells, output_samples))
}

/// Percentil 15 por celda usando exclusivamente cobertura/DQ y finitud como
/// criterios de elegibilidad. Conserva negativos y ceros calibrados.
fn background_cell_samples_with_coverage(
    data: &[f32],
    coverage: Option<&[u8]>,
    w: usize,
    h: usize,
    ch: usize,
    channel: usize,
    gw: usize,
    gh: usize,
) -> Result<Vec<Option<f64>>, String> {
    let pixels = w
        .checked_mul(h)
        .filter(|&count| count > 0)
        .ok_or_else(|| "geometría de muestras de fondo inválida".to_string())?;
    let expected = pixels
        .checked_mul(ch)
        .ok_or_else(|| "geometría multicanal de fondo fuera de rango".to_string())?;
    let cells = gw
        .checked_mul(gh)
        .filter(|&count| count > 0 && count <= MAX_LOCAL_NORM_GRID_CELLS)
        .ok_or_else(|| "rejilla de fondo vacía o excesiva".to_string())?;
    if channel >= ch || data.len() != expected {
        return Err("canal o longitud de frame inválidos para muestras de fondo".to_string());
    }
    if coverage.is_some_and(|mask| mask.len() != pixels) {
        return Err("longitud de cobertura inválida para muestras de fondo".to_string());
    }

    let mut out = Vec::new();
    out.try_reserve_exact(cells)
        .map_err(|error| format!("sin memoria para rejilla de fondo: {error}"))?;
    out.resize(cells, None);
    for gy in 0..gh {
        for gx in 0..gw {
            let x0 = gx * w / gw;
            let x1 = (((gx + 1) * w) / gw).min(w);
            let y0 = gy * h / gh;
            let y1 = (((gy + 1) * h) / gh).min(h);
            let mut cell: Vec<f32> = Vec::with_capacity(256);
            let sx = ((x1 - x0) / 16).max(1);
            let sy = ((y1 - y0) / 16).max(1);
            let mut y = y0;
            while y < y1 {
                let mut x = x0;
                while x < x1 {
                    let pixel = y * w + x;
                    let covered = coverage.map(|mask| mask[pixel] != 0).unwrap_or(true);
                    let value = data[pixel * ch + channel];
                    if covered && value.is_finite() {
                        cell.push(value);
                    }
                    x += sx;
                }
                y += sy;
            }
            if cell.len() < 8 {
                continue;
            }
            cell.sort_by(|a, b| a.total_cmp(b));
            out[gy * gw + gx] = Some(cell[cell.len() * 3 / 20] as f64);
        }
    }
    Ok(out)
}

fn estimate_registered_noise_sigma(
    frames: &[RegisteredBackgroundFrame<'_>],
    w: usize,
    h: usize,
    ch: usize,
    channel: usize,
) -> f64 {
    let possible = frames
        .len()
        .saturating_mul(h)
        .saturating_mul(w.saturating_sub(1));
    let stride = possible.div_ceil(MAX_NOISE_DIFFERENCES).max(1);
    let mut differences = Vec::with_capacity(possible.min(MAX_NOISE_DIFFERENCES));
    let mut ordinal = 0usize;
    for frame in frames {
        for y in 0..h {
            for x in 1..w {
                let take = ordinal % stride == 0;
                ordinal = ordinal.saturating_add(1);
                if !take {
                    continue;
                }
                let p0 = y * w + x - 1;
                let p1 = p0 + 1;
                let covered = frame
                    .coverage
                    .map(|mask| mask[p0] != 0 && mask[p1] != 0)
                    .unwrap_or(true);
                let a = frame.data[p0 * ch + channel];
                let b = frame.data[p1 * ch + channel];
                if covered && a.is_finite() && b.is_finite() {
                    differences.push((b as f64 - a as f64) * std::f64::consts::FRAC_1_SQRT_2);
                }
            }
        }
    }
    if differences.len() < 16 {
        return f64::NAN;
    }
    differences.sort_by(|a, b| a.total_cmp(b));
    let center = differences[differences.len() / 2];
    for value in &mut differences {
        *value = (*value - center).abs();
    }
    differences.sort_by(|a, b| a.total_cmp(b));
    differences[differences.len() / 2] * 1.4826
}

/// Normalización local simétrica multiframe por canal. Construye una rejilla
/// robusta por frame/canal, resuelve todas las diferencias mediante el grafo
/// de solapes y renderiza el negativo de la solución gauge-cero. Por ello la
/// referencia efectiva es el centro robusto del conjunto/componente, nunca la
/// toma 0 ni la toma de mayor peso.
pub(crate) fn solve_symmetric_local_normalization(
    frames: &[RegisteredBackgroundFrame<'_>],
    w: usize,
    h: usize,
    ch: usize,
    config: SymmetricLocalNormalizationConfig,
) -> Result<SymmetricLocalNormalization, String> {
    let (cells, output_samples) = validate_local_normalization_inputs(frames, w, h, ch, config)?;
    let mut additive_fields = Vec::new();
    additive_fields
        .try_reserve_exact(output_samples)
        .map_err(|error| format!("sin memoria para campos de normalización local: {error}"))?;
    additive_fields.resize(output_samples, 0.0f32);
    let mut channel_metrics = Vec::with_capacity(ch);

    for channel in 0..ch {
        let mut samples = Vec::with_capacity(frames.len());
        for frame in frames {
            samples.push(background_cell_samples_with_coverage(
                frame.data,
                frame.coverage,
                w,
                h,
                ch,
                channel,
                config.grid_width,
                config.grid_height,
            )?);
        }
        let graph = solve_background_graph(
            &samples,
            config.grid_width,
            config.grid_height,
            config.first_order,
        )
        .ok_or_else(|| {
            format!(
                "canal {channel}: no hay {} celdas comunes para formar el grafo local",
                MIN_COMMON_CELLS
            )
        })?;

        for frame in 0..frames.len() {
            let base = (frame * ch + channel) * cells;
            for gy in 0..config.grid_height {
                let yn = gy as f64 / (config.grid_height - 1) as f64;
                for gx in 0..config.grid_width {
                    let xn = gx as f64 / (config.grid_width - 1) as f64;
                    let solved_background = match &graph.planes {
                        Some(planes) => {
                            planes[frame][0] + planes[frame][1] * xn + planes[frame][2] * yn
                        }
                        None => graph.offsets[frame],
                    };
                    additive_fields[base + gy * config.grid_width + gx] = -solved_background as f32;
                }
            }
        }

        let noise_sigma = estimate_registered_noise_sigma(frames, w, h, ch, channel);
        let seam_sigma_ratio = if noise_sigma.is_finite() && noise_sigma > 0.0 {
            graph.rms_residual / noise_sigma
        } else if graph.rms_residual == 0.0 {
            0.0
        } else {
            f64::INFINITY
        };
        channel_metrics.push(LocalNormalizationChannelMetrics {
            edges: graph.edges,
            components: graph.components,
            component_ids: graph.component_ids,
            rms_seam: graph.rms_residual,
            noise_sigma,
            seam_sigma_ratio,
        });
    }

    Ok(SymmetricLocalNormalization {
        frame_count: frames.len(),
        channels: ch,
        grid_width: config.grid_width,
        grid_height: config.grid_height,
        additive_fields,
        channel_metrics,
    })
}

/// Rejilla de muestras de fondo de un frame completamente cubierto: percentil
/// 15 por celda. Todos los valores finitos, incluidos negativos y cero, son
/// científicamente válidos. Para un warp con bordes use el API superior con
/// una máscara de cobertura explícita.
pub(crate) fn background_cell_samples(
    data: &[f32],
    w: usize,
    h: usize,
    ch: usize,
    channel: usize,
    gw: usize,
    gh: usize,
) -> Vec<Option<f64>> {
    background_cell_samples_with_coverage(data, None, w, h, ch, channel, gw, gh).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 2. Modelo BG de sesión (no destructivo, exportable y validable)
// ---------------------------------------------------------------------------

/// Modelo polinómico de fondo por canal. La corrección que la resta clásica
/// aplicaría es `g(x,y) − level` (conserva la mediana del frame).
#[derive(Clone)]
pub(crate) struct BgModel {
    pub degree: usize,
    /// Coeficientes por canal en la base `ds_poly_basis` (coords x/w, y/h).
    pub coeffs: Vec<Vec<f64>>,
    /// Nivel mediano preservado por canal.
    pub level: Vec<f64>,
    pub w: usize,
    pub h: usize,
    pub ch: usize,
}

impl BgModel {
    /// Corrección por píxel/canal (`g − level`), mismo layout que el máster.
    /// Es exactamente el plano que `ds_extract_background_gradient` restaría;
    /// máster_original = máster_corregido + este plano (reversible).
    /// Paralelizado por filas y sin asignaciones por píxel: la evaluación
    /// grado 2 usa la MISMA base/orden que `ds_poly_basis` ([1, x, x², y, xy,
    /// y²]) escrita en línea; otros grados caen a la ruta genérica.
    pub(crate) fn render_correction(&self) -> Vec<f32> {
        use rayon::prelude::*;
        let (w, h, ch) = (self.w, self.h, self.ch);
        let mut out = vec![0.0f32; w * h * ch];
        out.par_chunks_mut(w * ch).enumerate().for_each(|(y, row)| {
            let yn = y as f64 / h as f64;
            for c in 0..ch {
                if self.coeffs[c].is_empty() {
                    continue;
                }
                let k = &self.coeffs[c];
                let level = self.level[c];
                if self.degree == 2 && k.len() == 6 {
                    for x in 0..w {
                        let xn = x as f64 / w as f64;
                        let g = k[0]
                            + k[1] * xn
                            + k[2] * xn * xn
                            + k[3] * yn
                            + k[4] * xn * yn
                            + k[5] * yn * yn;
                        row[x * ch + c] = (g - level) as f32;
                    }
                } else {
                    for x in 0..w {
                        let xn = x as f64 / w as f64;
                        let g: f64 = crate::ds_poly_basis(xn, yn, self.degree)
                            .iter()
                            .zip(k)
                            .map(|(b, cc)| b * cc)
                            .sum();
                        row[x * ch + c] = (g - level) as f32;
                    }
                }
            }
        });
        out
    }

    pub(crate) fn eval(&self, c: usize, xn: f64, yn: f64) -> f64 {
        crate::ds_poly_basis(xn, yn, self.degree)
            .iter()
            .zip(&self.coeffs[c])
            .map(|(b, cc)| b * cc)
            .sum()
    }
}

/// Ajusta el modelo de fondo del máster SIN modificarlo. Mismo muestreo y
/// rechazo robusto que `ds_extract_background_gradient` (rejilla 32×32,
/// percentil 15 por celda, 3 pasadas descartando > ajuste + 2.5σ): un canal
/// sin muestras suficientes queda con `coeffs` vacío.
pub(crate) fn fit_background_model(data: &[f32], w: usize, h: usize, ch: usize) -> Option<BgModel> {
    fit_background_model_masked(data, w, h, ch, None)
}

/// Variante con máscara de exclusión por píxel. `true` significa que la
/// muestra pertenece a nebulosa/galaxia/objeto y no puede influir en el
/// modelo. La máscara solo afecta al muestreo; nunca altera el máster.
pub(crate) fn fit_background_model_masked(
    data: &[f32],
    w: usize,
    h: usize,
    ch: usize,
    excluded: Option<&[bool]>,
) -> Option<BgModel> {
    fit_background_model_masked_degree(data, w, h, ch, excluded, 2)
}

/// Igual que `fit_background_model_masked`, pero conserva en la receta el
/// grado elegido por el usuario experto. El rango 1..=4 evita modelos de alto
/// orden mal condicionados sobre campos astronómicos con poco fondo real.
pub(crate) fn fit_background_model_masked_degree(
    data: &[f32],
    w: usize,
    h: usize,
    ch: usize,
    excluded: Option<&[bool]>,
    degree: usize,
) -> Option<BgModel> {
    const GRID: usize = 32;
    let degree = degree.clamp(1, 4);
    let nterms = (degree + 1) * (degree + 2) / 2;
    if w < GRID * 3
        || h < GRID * 3
        || data.len() < w.saturating_mul(h).saturating_mul(ch)
        || excluded.is_some_and(|mask| mask.len() != w.saturating_mul(h))
    {
        return None;
    }
    let mut coeffs = vec![Vec::new(); ch];
    let mut level = vec![0.0f64; ch];
    for c in 0..ch {
        let mut xs: Vec<f64> = Vec::new();
        let mut ys: Vec<f64> = Vec::new();
        let mut vs: Vec<f64> = Vec::new();
        for gy in 0..GRID {
            for gx in 0..GRID {
                let x0 = gx * w / GRID;
                let x1 = ((gx + 1) * w / GRID).min(w);
                let y0 = gy * h / GRID;
                let y1 = ((gy + 1) * h / GRID).min(h);
                let mut cell: Vec<f32> = Vec::with_capacity(256);
                let sx = ((x1 - x0) / 16).max(1);
                let sy = ((y1 - y0) / 16).max(1);
                let mut y = y0;
                while y < y1 {
                    let mut x = x0;
                    while x < x1 {
                        let pixel = y * w + x;
                        if !excluded.is_some_and(|mask| mask[pixel]) {
                            let value = data[pixel * ch + c];
                            if value.is_finite() {
                                cell.push(value);
                            }
                        }
                        x += sx;
                    }
                    y += sy;
                }
                if cell.len() < 8 {
                    continue;
                }
                cell.sort_by(|a, b| a.total_cmp(b));
                xs.push((x0 + x1) as f64 / 2.0 / w as f64);
                ys.push((y0 + y1) as f64 / 2.0 / h as f64);
                vs.push(cell[cell.len() * 3 / 20] as f64);
            }
        }
        if vs.len() < nterms * 2 {
            continue;
        }
        let mut keep = vec![true; vs.len()];
        let mut coef: Vec<f64> = Vec::new();
        for pass in 0..3 {
            let mut mat: Vec<Vec<f64>> = vec![vec![0.0; nterms + 1]; nterms];
            let mut count = 0;
            for i in 0..vs.len() {
                if !keep[i] {
                    continue;
                }
                count += 1;
                let b = crate::ds_poly_basis(xs[i], ys[i], degree);
                for r in 0..nterms {
                    for cc in 0..nterms {
                        mat[r][cc] += b[r] * b[cc];
                    }
                    mat[r][nterms] += b[r] * vs[i];
                }
            }
            if count < nterms * 2 {
                break;
            }
            coef = match crate::ds_solve_linear_n(&mut mat, nterms) {
                Some(v) => v,
                None => break,
            };
            if pass == 2 {
                break;
            }
            let mut res: Vec<f64> = Vec::new();
            for i in 0..vs.len() {
                if keep[i] {
                    let f: f64 = crate::ds_poly_basis(xs[i], ys[i], degree)
                        .iter()
                        .zip(&coef)
                        .map(|(b, cc)| b * cc)
                        .sum();
                    res.push(vs[i] - f);
                }
            }
            let mut ares: Vec<f64> = res.iter().map(|r| r.abs()).collect();
            ares.sort_by(|a, b| a.total_cmp(b));
            let sigma = ares[ares.len() / 2] * 1.4826 + 1e-6;
            for i in 0..vs.len() {
                if keep[i] {
                    let f: f64 = crate::ds_poly_basis(xs[i], ys[i], degree)
                        .iter()
                        .zip(&coef)
                        .map(|(b, cc)| b * cc)
                        .sum();
                    if vs[i] - f > 2.5 * sigma {
                        keep[i] = false;
                    }
                }
            }
        }
        if coef.is_empty() {
            continue;
        }
        let mut fitted: Vec<f64> = (0..vs.len())
            .filter(|&i| keep[i])
            .map(|i| {
                crate::ds_poly_basis(xs[i], ys[i], degree)
                    .iter()
                    .zip(&coef)
                    .map(|(b, cc)| b * cc)
                    .sum()
            })
            .collect();
        fitted.sort_by(|a, b| a.total_cmp(b));
        level[c] = fitted[fitted.len() / 2];
        coeffs[c] = coef;
    }
    if coeffs.iter().all(|c| c.is_empty()) {
        return None;
    }
    Some(BgModel {
        degree,
        coeffs,
        level,
        w,
        h,
        ch,
    })
}

fn bg_weighted_robust_fit(
    xs: &[f64],
    ys: &[f64],
    values: &[f64],
    weights: &[f64],
    degree: usize,
) -> Option<Vec<f64>> {
    let nterms = (degree + 1) * (degree + 2) / 2;
    if xs.len() != ys.len()
        || xs.len() != values.len()
        || xs.len() != weights.len()
        || xs.len() < nterms * 2
    {
        return None;
    }
    let mut robust = vec![1.0f64; values.len()];
    let mut coeffs = Vec::new();
    for _ in 0..6 {
        let mut matrix = vec![vec![0.0f64; nterms + 1]; nterms];
        let mut effective = 0usize;
        for index in 0..values.len() {
            let weight = weights[index] * robust[index];
            if !weight.is_finite() || weight <= 1e-9 {
                continue;
            }
            effective += 1;
            let basis = crate::ds_poly_basis(xs[index], ys[index], degree);
            for row in 0..nterms {
                for column in 0..nterms {
                    matrix[row][column] += weight * basis[row] * basis[column];
                }
                matrix[row][nterms] += weight * basis[row] * values[index];
            }
        }
        if effective < nterms * 2 {
            return None;
        }
        // Regularización numérica mínima, relativa a la traza: estabiliza
        // coordenadas válidas pero casi colineales sin ocultar una matriz
        // realmente singular.
        let trace = (0..nterms)
            .map(|index| matrix[index][index].abs())
            .sum::<f64>()
            .max(1.0);
        for index in 0..nterms {
            matrix[index][index] += trace * 1e-12;
        }
        coeffs = crate::ds_solve_linear_n(&mut matrix, nterms)?;

        let residuals = (0..values.len())
            .map(|index| {
                let estimate = crate::ds_poly_basis(xs[index], ys[index], degree)
                    .iter()
                    .zip(&coeffs)
                    .map(|(basis, coefficient)| basis * coefficient)
                    .sum::<f64>();
                values[index] - estimate
            })
            .collect::<Vec<_>>();
        let mut ordered = residuals.clone();
        ordered.sort_by(|a, b| a.total_cmp(b));
        let center = ordered[ordered.len() / 2];
        let mut absolute = residuals
            .iter()
            .map(|residual| (residual - center).abs())
            .collect::<Vec<_>>();
        absolute.sort_by(|a, b| a.total_cmp(b));
        let sigma = (absolute[absolute.len() / 2] * 1.4826).max(1e-9);
        let huber = 1.5 * sigma;
        robust
            .iter_mut()
            .zip(residuals.iter())
            .for_each(|(weight, residual)| {
                let distance = (residual - center).abs();
                *weight = if distance <= huber {
                    1.0
                } else {
                    (huber / distance).clamp(0.0, 1.0)
                };
            });
    }
    (!coeffs.is_empty()).then_some(coeffs)
}

/// Ajuste DBE-like desde muestras circulares explícitas. Cada muestra aporta
/// el percentil 20 de sus píxeles válidos por canal; después un IRLS Huber
/// ponderado rechaza estrellas o estructura residual sin convertir las
/// muestras en simples rectángulos de exclusión.
pub(crate) fn fit_background_model_samples(
    data: &[f32],
    w: usize,
    h: usize,
    ch: usize,
    samples: &[crate::pipeline::PostStackBackgroundSample],
    degree: usize,
    excluded: Option<&[bool]>,
) -> Result<BgModel, String> {
    let degree = degree.clamp(1, 4);
    let pixels = w.saturating_mul(h);
    if w < 16
        || h < 16
        || ch == 0
        || data.len() < pixels.saturating_mul(ch)
        || excluded.is_some_and(|mask| mask.len() != pixels)
    {
        return Err("La geometría o la máscara DQ no coincide con el máster".into());
    }
    let enabled = samples
        .iter()
        .filter(|sample| sample.enabled)
        .collect::<Vec<_>>();
    let nterms = (degree + 1) * (degree + 2) / 2;
    if enabled.len() < nterms * 2 {
        return Err(format!(
            "El grado {degree} requiere al menos {} muestras de fondo habilitadas",
            nterms * 2
        ));
    }
    for (index, sample) in enabled.iter().enumerate() {
        if !sample.x.is_finite()
            || !sample.y.is_finite()
            || !sample.radius.is_finite()
            || !sample.weight.is_finite()
            || !(0.0..=1.0).contains(&sample.x)
            || !(0.0..=1.0).contains(&sample.y)
            || !(0.002..=0.25).contains(&sample.radius)
            || !(0.01..=100.0).contains(&sample.weight)
        {
            return Err(format!(
                "La muestra de fondo habilitada {} tiene posición, radio o peso inválido",
                index + 1
            ));
        }
    }
    let (min_x, max_x) = enabled.iter().fold((1.0f32, 0.0f32), |(lo, hi), sample| {
        (lo.min(sample.x), hi.max(sample.x))
    });
    let (min_y, max_y) = enabled.iter().fold((1.0f32, 0.0f32), |(lo, hi), sample| {
        (lo.min(sample.y), hi.max(sample.y))
    });
    if max_x - min_x < 0.25 || max_y - min_y < 0.25 {
        return Err(
            "Las muestras de fondo no cubren el campo: distribúyelas en ancho y alto".into(),
        );
    }

    let mut xs = Vec::with_capacity(enabled.len());
    let mut ys = Vec::with_capacity(enabled.len());
    let mut values = vec![Vec::<f64>::with_capacity(enabled.len()); ch];
    let mut weights = Vec::with_capacity(enabled.len());
    for sample in enabled {
        let center_x = (sample.x * (w.saturating_sub(1)) as f32).round() as isize;
        let center_y = (sample.y * (h.saturating_sub(1)) as f32).round() as isize;
        let radius = (sample.radius * w.min(h) as f32).round().max(2.0) as isize;
        let x0 = (center_x - radius).max(0) as usize;
        let x1 = (center_x + radius + 1).min(w as isize) as usize;
        let y0 = (center_y - radius).max(0) as usize;
        let y1 = (center_y + radius + 1).min(h as isize) as usize;
        let stride = ((radius as usize * 2 + 1) / 48).max(1);
        let mut cells = vec![Vec::<f32>::new(); ch];
        for y in (y0..y1).step_by(stride) {
            for x in (x0..x1).step_by(stride) {
                let dx = x as isize - center_x;
                let dy = y as isize - center_y;
                if dx * dx + dy * dy > radius * radius {
                    continue;
                }
                let pixel = y * w + x;
                if excluded.is_some_and(|mask| mask[pixel]) {
                    continue;
                }
                for channel in 0..ch {
                    let value = data[pixel * ch + channel];
                    if value.is_finite() {
                        cells[channel].push(value);
                    }
                }
            }
        }
        let valid = cells.iter().map(Vec::len).min().unwrap_or(0);
        if valid < 8 {
            continue;
        }
        xs.push(sample.x as f64);
        ys.push(sample.y as f64);
        weights.push(sample.weight as f64 * (valid as f64).sqrt());
        for channel in 0..ch {
            cells[channel].sort_by(|a, b| a.total_cmp(b));
            values[channel].push(cells[channel][cells[channel].len() / 5] as f64);
        }
    }
    if xs.len() < nterms * 2 {
        return Err(format!(
            "Sólo {} muestras conservan suficientes píxeles válidos; se requieren {}",
            xs.len(),
            nterms * 2
        ));
    }

    let mut coeffs = Vec::with_capacity(ch);
    let mut level = Vec::with_capacity(ch);
    for channel_values in &values {
        let coefficients = bg_weighted_robust_fit(&xs, &ys, channel_values, &weights, degree)
            .ok_or_else(|| {
                "Las muestras producen un modelo singular; redistribúyelas por el campo".to_string()
            })?;
        let mut fitted = xs
            .iter()
            .zip(ys.iter())
            .map(|(x, y)| {
                crate::ds_poly_basis(*x, *y, degree)
                    .iter()
                    .zip(&coefficients)
                    .map(|(basis, coefficient)| basis * coefficient)
                    .sum::<f64>()
            })
            .collect::<Vec<_>>();
        fitted.sort_by(|a, b| a.total_cmp(b));
        level.push(fitted[fitted.len() / 2]);
        coeffs.push(coefficients);
    }
    Ok(BgModel {
        degree,
        coeffs,
        level,
        w,
        h,
        ch,
    })
}

/// Validación split-half: RMS de la diferencia entre las superficies de dos
/// modelos ajustados sobre mitades independientes, evaluada en una rejilla
/// 32×32 y promediada entre canales con modelo. El caller compara contra su
/// σ de fondo (aceptar si `rms < 0.2·σ`, gate del plan).
pub(crate) fn bg_split_half_rms(a: &BgModel, b: &BgModel) -> f64 {
    const GRID: usize = 32;
    let mut sq = 0.0f64;
    let mut count = 0usize;
    for c in 0..a.ch.min(b.ch) {
        if a.coeffs[c].is_empty() || b.coeffs[c].is_empty() {
            continue;
        }
        for gy in 0..GRID {
            for gx in 0..GRID {
                let xn = (gx as f64 + 0.5) / GRID as f64;
                let yn = (gy as f64 + 0.5) / GRID as f64;
                let da = a.eval(c, xn, yn) - a.level[c];
                let db = b.eval(c, xn, yn) - b.level[c];
                sq += (da - db) * (da - db);
                count += 1;
            }
        }
    }
    if count == 0 {
        return f64::INFINITY;
    }
    (sq / count as f64).sqrt()
}

// ---------------------------------------------------------------------------
// 3. Asesor de muestreo
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SamplingClass {
    /// FWHM < 2 px: hay información subpíxel — candidato a EIDR 1.5x/2x.
    Undersampled,
    /// FWHM 2–3.5 px: muestreo correcto, integrar a resolución nativa.
    WellSampled,
    /// FWHM > 3.5 px: sobremuestreado — el super-binning PSF-matched gana SNR
    /// por píxel sin perder detalle real.
    Oversampled,
}

impl SamplingClass {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            SamplingClass::Undersampled => "undersampled",
            SamplingClass::WellSampled => "well_sampled",
            SamplingClass::Oversampled => "oversampled",
        }
    }
}

/// Clasifica el muestreo y recomienda escala de salida. Umbrales del plan
/// (F2): <2 px submuestreado (2x si <1.5, si no 1.5x); 2–3.5 px nativo;
/// >3.5 px sobremuestreado (0.75x hasta 5 px, 0.5x por encima).
pub(crate) fn advise_sampling(fwhm_median_px: f64) -> (SamplingClass, &'static str) {
    if !fwhm_median_px.is_finite() || fwhm_median_px <= 0.0 {
        return (SamplingClass::WellSampled, "1x");
    }
    if fwhm_median_px < 2.0 {
        if fwhm_median_px < 1.5 {
            (SamplingClass::Undersampled, "2x")
        } else {
            (SamplingClass::Undersampled, "1.5x")
        }
    } else if fwhm_median_px <= 3.5 {
        (SamplingClass::WellSampled, "1x")
    } else if fwhm_median_px <= 5.0 {
        (SamplingClass::Oversampled, "0.75x")
    } else {
        (SamplingClass::Oversampled, "0.5x")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_graph_recovers_known_offsets_with_zero_gauge() {
        // 4 frames sobre una rejilla 6×6 completamente solapada, offsets
        // verdaderos [-3, 1, 2, 0] → esperados re-centrados a Σ=0.
        let (gw, gh) = (6, 6);
        let truth = [-3.0f64, 1.0, 2.0, 0.0];
        let base: Vec<f64> = (0..gw * gh).map(|c| 100.0 + (c % gw) as f64).collect();
        let samples: Vec<Vec<Option<f64>>> = truth
            .iter()
            .map(|off| base.iter().map(|v| Some(v + off)).collect())
            .collect();
        let sol = solve_background_graph(&samples, gw, gh, false).unwrap();
        let mean_truth: f64 = truth.iter().sum::<f64>() / truth.len() as f64;
        for (i, t) in truth.iter().enumerate() {
            assert!(
                (sol.offsets[i] - (t - mean_truth)).abs() < 1e-9,
                "offset {i}: {} vs {}",
                sol.offsets[i],
                t - mean_truth
            );
        }
        assert!(sol.offsets.iter().sum::<f64>().abs() < 1e-9);
        assert!(sol.rms_residual < 1e-9);
        assert_eq!(sol.edges, 6);
    }

    #[test]
    fn test_graph_first_order_recovers_frame_planes() {
        // 3 frames con planos distintos: d(x,y) = c0 + cx·x + cy·y.
        let (gw, gh) = (8, 8);
        let truth: [[f64; 3]; 3] = [[5.0, 2.0, -1.0], [-2.0, 0.0, 3.0], [0.0, -2.0, -2.0]];
        let samples: Vec<Vec<Option<f64>>> = truth
            .iter()
            .map(|p| {
                (0..gw * gh)
                    .map(|cell| {
                        let xn = ((cell % gw) as f64 + 0.5) / gw as f64;
                        let yn = ((cell / gw) as f64 + 0.5) / gh as f64;
                        Some(200.0 + 10.0 * xn + p[0] + p[1] * xn + p[2] * yn)
                    })
                    .collect()
            })
            .collect();
        let sol = solve_background_graph(&samples, gw, gh, true).unwrap();
        let planes = sol.planes.as_ref().unwrap();
        // Los planos se recuperan salvo el gauge común (media por componente).
        for comp in 0..3 {
            let mean: f64 = truth.iter().map(|p| p[comp]).sum::<f64>() / 3.0;
            for (i, t) in truth.iter().enumerate() {
                assert!(
                    (planes[i][comp] - (t[comp] - mean)).abs() < 1e-6,
                    "frame {i} comp {comp}: {} vs {}",
                    planes[i][comp],
                    t[comp] - mean
                );
            }
        }
        assert!(sol.rms_residual < 1e-9);
    }

    #[test]
    fn test_graph_handles_partial_overlap_and_masked_cells() {
        // Frame 0 cubre la mitad izquierda, frame 2 la derecha; frame 1 todo:
        // la cadena 0↔1↔2 conecta el grafo aunque 0 y 2 no compartan celdas.
        let (gw, gh) = (8, 4);
        let cells = gw * gh;
        let mk = |off: f64, pred: &dyn Fn(usize) -> bool| -> Vec<Option<f64>> {
            (0..cells)
                .map(|c| if pred(c) { Some(50.0 + off) } else { None })
                .collect()
        };
        let s0 = mk(4.0, &|c| c % gw < 5);
        let s1 = mk(-1.0, &|_| true);
        let s2 = mk(-3.0, &|c| c % gw >= 3);
        let sol = solve_background_graph(&[s0, s1, s2], gw, gh, false).unwrap();
        let truth = [4.0f64, -1.0, -3.0];
        let mean: f64 = truth.iter().sum::<f64>() / 3.0;
        for (i, t) in truth.iter().enumerate() {
            assert!((sol.offsets[i] - (t - mean)).abs() < 1e-9);
        }
    }

    #[test]
    fn test_graph_disconnected_components_get_per_component_gauge() {
        // Dos grupos SIN celdas comunes (paneles de mosaico): el offset entre
        // grupos es indeterminable — cada componente queda con media 0 y
        // components=2, sin singularidad ni valores absurdos.
        let (gw, gh) = (8, 4);
        let cells = gw * gh;
        let mk = |off: f64, pred: &dyn Fn(usize) -> bool| -> Vec<Option<f64>> {
            (0..cells)
                .map(|c| if pred(c) { Some(80.0 + off) } else { None })
                .collect()
        };
        // Grupo A: mitad izquierda; grupo B: mitad derecha (sin solape).
        let s0 = mk(6.0, &|c| c % gw < 4);
        let s1 = mk(2.0, &|c| c % gw < 4);
        let s2 = mk(-5.0, &|c| c % gw >= 4);
        let s3 = mk(-1.0, &|c| c % gw >= 4);
        let sol = solve_background_graph(&[s0, s1, s2, s3], gw, gh, false).unwrap();
        assert_eq!(sol.components, 2);
        assert_eq!(sol.edges, 2);
        // Dentro de cada componente: diferencias exactas y media 0.
        assert!((sol.offsets[0] - sol.offsets[1] - 4.0).abs() < 1e-9);
        assert!((sol.offsets[2] - sol.offsets[3] + 4.0).abs() < 1e-9);
        assert!((sol.offsets[0] + sol.offsets[1]).abs() < 1e-9);
        assert!((sol.offsets[2] + sol.offsets[3]).abs() < 1e-9);
        assert!(sol.offsets.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn local_samples_preserve_negative_and_zero_and_respect_coverage() {
        let (w, h, gw, gh) = (16usize, 16usize, 2usize, 2usize);
        let mut data = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                data[y * w + x] = match (x >= 8, y >= 8) {
                    (false, false) => -17.5,
                    (true, false) => 0.0,
                    (false, true) => f32::NAN,
                    (true, true) => 9.0,
                };
            }
        }
        let all_covered = vec![1u8; w * h];
        let samples =
            background_cell_samples_with_coverage(&data, Some(&all_covered), w, h, 1, 0, gw, gh)
                .unwrap();
        assert_eq!(samples, vec![Some(-17.5), Some(0.0), None, Some(9.0)]);

        let mut masked = all_covered;
        for y in 0..8 {
            for x in 8..16 {
                masked[y * w + x] = 0;
            }
        }
        let samples =
            background_cell_samples_with_coverage(&data, Some(&masked), w, h, 1, 0, gw, gh)
                .unwrap();
        assert_eq!(samples[0], Some(-17.5));
        assert_eq!(
            samples[1], None,
            "la máscara, y no el valor SCI=0, debe declarar no-cobertura"
        );
    }

    #[test]
    fn symmetric_local_normalization_is_rgb_per_channel_and_gradient_aware() {
        let (w, h, ch) = (64usize, 48usize, 3usize);
        let config = SymmetricLocalNormalizationConfig {
            grid_width: 8,
            grid_height: 6,
            first_order: true,
        };
        let truth: [[[f64; 3]; 3]; 4] = [
            [[-8.0, 3.0, -2.0], [22.0, -5.0, 1.0], [90.0, 2.0, 7.0]],
            [[4.0, -1.0, 5.0], [-12.0, 4.0, -6.0], [20.0, -8.0, 3.0]],
            [[11.0, 6.0, 1.0], [7.0, 2.0, 9.0], [-40.0, 3.0, -5.0]],
            [[-3.0, -8.0, -4.0], [31.0, -1.0, 2.0], [60.0, 5.0, -1.0]],
        ];
        let base = [-120.0f64, 200.0, 900.0];
        let mut owned = Vec::new();
        for frame_planes in &truth {
            let mut data = vec![0.0f32; w * h * ch];
            for y in 0..h {
                for x in 0..w {
                    let gx = x * config.grid_width / w;
                    let gy = y * config.grid_height / h;
                    let xn = (gx as f64 + 0.5) / config.grid_width as f64;
                    let yn = (gy as f64 + 0.5) / config.grid_height as f64;
                    for channel in 0..ch {
                        let common = base[channel] + (channel as f64 + 1.0) * (4.0 * xn - 3.0 * yn);
                        let p = frame_planes[channel];
                        data[(y * w + x) * ch + channel] =
                            (common + p[0] + p[1] * xn + p[2] * yn) as f32;
                    }
                }
            }
            owned.push(data);
        }
        let inputs: Vec<RegisteredBackgroundFrame<'_>> = owned
            .iter()
            .map(|data| RegisteredBackgroundFrame {
                data,
                coverage: None,
            })
            .collect();
        let solution = solve_symmetric_local_normalization(&inputs, w, h, ch, config).unwrap();
        assert_eq!(solution.frame_count, 4);
        assert_eq!(solution.channels, 3);
        for metric in &solution.channel_metrics {
            assert_eq!(metric.edges, 6);
            assert_eq!(metric.components, 1);
            assert_eq!(metric.component_ids, vec![0, 0, 0, 0]);
            assert!(metric.rms_seam < 5e-4, "seam RGB = {}", metric.rms_seam);
        }

        // En cada canal/celda, todos los frames llegan al mismo centro robusto
        // y los campos suman cero: no existe una toma de referencia oculta.
        for channel in 0..ch {
            for gy in 0..config.grid_height {
                for gx in 0..config.grid_width {
                    let xn = (gx as f32 + 0.5) / config.grid_width as f32;
                    let yn = (gy as f32 + 0.5) / config.grid_height as f32;
                    let x = gx * w / config.grid_width;
                    let y = gy * h / config.grid_height;
                    let mut corrected = Vec::new();
                    let mut field_sum = 0.0f64;
                    for frame in 0..owned.len() {
                        let field = solution.sample_additive(frame, channel, xn, yn).unwrap();
                        field_sum += field as f64;
                        corrected.push(owned[frame][(y * w + x) * ch + channel] + field);
                    }
                    let span = corrected.iter().copied().fold(f32::NEG_INFINITY, f32::max)
                        - corrected.iter().copied().fold(f32::INFINITY, f32::min);
                    assert!(span < 1e-3, "canal {channel}, celda {gx},{gy}: span={span}");
                    assert!(field_sum.abs() < 2e-4, "gauge no simétrico: {field_sum}");
                }
            }
        }
        assert!(owned[0].iter().step_by(ch).all(|value| *value < 0.0));
        assert_ne!(solution.field(0, 0).unwrap(), solution.field(0, 1).unwrap());
    }

    #[test]
    fn symmetric_local_normalization_keeps_disconnected_component_gauges() {
        let (w, h, ch) = (64usize, 32usize, 1usize);
        let config = SymmetricLocalNormalizationConfig {
            grid_width: 8,
            grid_height: 4,
            first_order: true,
        };
        let offsets = [6.0f32, 2.0, -5.0, -1.0];
        let mut owned = Vec::new();
        let mut masks = Vec::new();
        for (frame, &offset) in offsets.iter().enumerate() {
            let left = frame < 2;
            let mut data = vec![f32::NAN; w * h];
            let mut coverage = vec![0u8; w * h];
            for y in 0..h {
                for x in 0..w {
                    if (x < w / 2) == left {
                        data[y * w + x] = 80.0 + offset;
                        coverage[y * w + x] = 1;
                    }
                }
            }
            owned.push(data);
            masks.push(coverage);
        }
        let inputs: Vec<RegisteredBackgroundFrame<'_>> = owned
            .iter()
            .zip(&masks)
            .map(|(data, coverage)| RegisteredBackgroundFrame {
                data,
                coverage: Some(coverage),
            })
            .collect();
        let solution = solve_symmetric_local_normalization(&inputs, w, h, ch, config).unwrap();
        let metric = &solution.channel_metrics[0];
        assert_eq!(metric.components, 2);
        assert_eq!(metric.edges, 2);
        assert_eq!(metric.component_ids[0], metric.component_ids[1]);
        assert_eq!(metric.component_ids[2], metric.component_ids[3]);
        assert_ne!(metric.component_ids[0], metric.component_ids[2]);
        for pair in [[0usize, 1usize], [2usize, 3usize]] {
            let f0 = solution.sample_additive(pair[0], 0, 0.5, 0.5).unwrap();
            let f1 = solution.sample_additive(pair[1], 0, 0.5, 0.5).unwrap();
            assert!((f0 + f1).abs() < 1e-5, "gauge de componente no nulo");
            let corrected0 = 80.0 + offsets[pair[0]] + f0;
            let corrected1 = 80.0 + offsets[pair[1]] + f1;
            assert!((corrected0 - corrected1).abs() < 1e-5);
        }
    }

    #[test]
    fn gate_symmetric_local_normalization_seam_below_02_sigma() {
        let (w, h, ch) = (128usize, 96usize, 1usize);
        let sigma = 10.0f64;
        let config = SymmetricLocalNormalizationConfig {
            grid_width: 8,
            grid_height: 6,
            first_order: true,
        };
        let planes = [
            [40.0f64, 18.0, -12.0],
            [-25.0, -9.0, 16.0],
            [5.0, 4.0, 3.0],
            [18.0, -14.0, -6.0],
            [-11.0, 7.0, -15.0],
            [2.0, -6.0, 11.0],
        ];
        let mut state = 0xD1CE_BA5E_CAFE_F00Du64;
        let mut normal = || {
            let mut sum = 0.0f64;
            for _ in 0..12 {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                sum += (state >> 11) as f64 / (1u64 << 53) as f64;
            }
            sum - 6.0
        };
        let mut owned = Vec::new();
        for plane in planes {
            let mut data = vec![0.0f32; w * h];
            for y in 0..h {
                let yn = (y as f64 + 0.5) / h as f64;
                for x in 0..w {
                    let xn = (x as f64 + 0.5) / w as f64;
                    let common = -35.0 + 12.0 * xn - 8.0 * yn;
                    data[y * w + x] =
                        (common + plane[0] + plane[1] * xn + plane[2] * yn + sigma * normal())
                            as f32;
                }
            }
            owned.push(data);
        }
        let inputs: Vec<RegisteredBackgroundFrame<'_>> = owned
            .iter()
            .map(|data| RegisteredBackgroundFrame {
                data,
                coverage: None,
            })
            .collect();
        let solution = solve_symmetric_local_normalization(&inputs, w, h, ch, config).unwrap();
        let metric = &solution.channel_metrics[0];
        assert!(
            metric.rms_seam < 0.2 * sigma,
            "seam {:.3} >= 0.2 sigma ({:.3})",
            metric.rms_seam,
            0.2 * sigma
        );
        assert!(
            metric.seam_sigma_ratio < 0.2,
            "ratio seam/sigma estimada = {:.3}, sigma estimada = {:.3}",
            metric.seam_sigma_ratio,
            metric.noise_sigma
        );
    }

    #[test]
    fn gate_f2_sampling_advisor_classifies_synthetic_scenarios() {
        assert_eq!(advise_sampling(1.2), (SamplingClass::Undersampled, "2x"));
        assert_eq!(advise_sampling(1.7), (SamplingClass::Undersampled, "1.5x"));
        assert_eq!(advise_sampling(2.5), (SamplingClass::WellSampled, "1x"));
        assert_eq!(advise_sampling(4.2), (SamplingClass::Oversampled, "0.75x"));
        assert_eq!(advise_sampling(6.5), (SamplingClass::Oversampled, "0.5x"));
        // Degenerado: sin medida fiable, no recomendar nada distinto de 1x.
        assert_eq!(advise_sampling(f64::NAN).1, "1x");
    }

    /// Gate F2: multi-sesión simulada con gradientes distintos por sesión —
    /// tras resolver el grafo de primer orden, la costura residual entre
    /// frames debe quedar por debajo de 0.2·σ del fondo.
    #[test]
    fn gate_f2_background_graph_seam_below_02_sigma() {
        let (w, h) = (96, 96);
        let sensor = crate::deepsky_sim::SimSensor {
            gain_e_per_adu: 1.0,
            read_noise_e: 3.0,
            bias_adu: 0.0,
            dark_adu_per_s: 0.0,
            full_well_adu: 65535.0,
            hot_pixels: Vec::new(),
            bayer: None,
            vignette: None,
        };
        // Tres "sesiones" con fondo y gradiente distintos (luna/LP cambiante).
        let sessions: [(f64, (f64, f64)); 3] = [
            (200.0, (0.3, 0.0)),
            (260.0, (-0.2, 0.25)),
            (230.0, (0.0, -0.35)),
        ];
        let mut samples: Vec<Vec<Option<f64>>> = Vec::new();
        let (gw, gh) = (12, 12);
        let mut sigma_bg = 0.0f64;
        let mut frames_count = 0usize;
        for (s, (bg, grad)) in sessions.iter().enumerate() {
            for k in 0..4 {
                let scene = crate::deepsky_sim::SimScene {
                    width: w,
                    height: h,
                    background_adu: *bg,
                    gradient_adu_per_px: *grad,
                    color: [1.0, 1.0, 1.0],
                    stars: Vec::new(),
                };
                let exp = crate::deepsky_sim::SimExposure {
                    exposure_s: 60.0,
                    dx: 0.0,
                    dy: 0.0,
                    seed: (s * 100 + k) as u64 + 1,
                };
                let (frame, truevar) = crate::deepsky_sim::render_light(&scene, &sensor, &exp);
                sigma_bg += truevar.iter().sum::<f64>() / truevar.len() as f64;
                frames_count += 1;
                samples.push(background_cell_samples(&frame, w, h, 1, 0, gw, gh));
            }
        }
        let sigma = (sigma_bg / frames_count as f64).sqrt();
        let sol = solve_background_graph(&samples, gw, gh, true).unwrap();
        // La costura del gate es el error SISTEMÁTICO de la corrección, no el
        // ruido de muestreo de las celdas (el percentil 15 de ~64 muestras
        // lleva ~0.2σ de ruido propio que se cancela al promediar píxeles).
        // Se mide contra la verdad sin ruido del simulador: tras corregir,
        // los fondos ideales de cada par de frames deben coincidir <0.2σ.
        let planes = sol.planes.as_ref().unwrap();
        let mut ideal_cells: Vec<Vec<Option<f64>>> = Vec::new();
        for (bg, grad) in sessions.iter() {
            let scene = crate::deepsky_sim::SimScene {
                width: w,
                height: h,
                background_adu: *bg,
                gradient_adu_per_px: *grad,
                color: [1.0, 1.0, 1.0],
                stars: Vec::new(),
            };
            let exp = crate::deepsky_sim::SimExposure {
                exposure_s: 60.0,
                dx: 0.0,
                dy: 0.0,
                seed: 1,
            };
            let ideal: Vec<f32> = crate::deepsky_sim::render_ideal(&scene, &sensor, &exp)
                .iter()
                .map(|&v| v as f32)
                .collect();
            let cells = background_cell_samples(&ideal, w, h, 1, 0, gw, gh);
            for _ in 0..4 {
                ideal_cells.push(cells.clone());
            }
        }
        let n = samples.len();
        let mut sq = 0.0f64;
        let mut cnt = 0usize;
        for i in 0..n {
            for j in (i + 1)..n {
                for cell in 0..gw * gh {
                    if let (Some(ti), Some(tj)) = (ideal_cells[i][cell], ideal_cells[j][cell]) {
                        let xn = ((cell % gw) as f64 + 0.5) / gw as f64;
                        let yn = ((cell / gw) as f64 + 0.5) / gh as f64;
                        let ci = planes[i][0] + planes[i][1] * xn + planes[i][2] * yn;
                        let cj = planes[j][0] + planes[j][1] * xn + planes[j][2] * yn;
                        let seam = (ti - ci) - (tj - cj);
                        sq += seam * seam;
                        cnt += 1;
                    }
                }
            }
        }
        let seam_rms = (sq / cnt as f64).sqrt();
        assert!(
            seam_rms < 0.2 * sigma,
            "costura sistemática {seam_rms:.3} ADU >= 0.2·σ ({:.3} ADU)",
            0.2 * sigma
        );
        assert_eq!(sol.offsets.len(), 12);
    }

    /// Gate F2: el modelo BG de sesión concuerda entre mitades independientes
    /// y NO absorbe una nebulosa extensa (blob gaussiano grande). El blob
    /// ocupa ~10% del área: el rechazo robusto de outliers positivos deja
    /// celdas de fondo limpias suficientes. Cuando la nebulosa LLENA el campo
    /// el modelo flexible debe quedar desactivado (regla §4.3 del plan; la
    /// máscara de objeto llega con las máscaras de F3).
    #[test]
    fn gate_f2_bg_model_split_half_agrees_and_ignores_nebula() {
        let (w, h) = (192, 192);
        let sensor = crate::deepsky_sim::SimSensor {
            gain_e_per_adu: 1.0,
            read_noise_e: 3.0,
            bias_adu: 0.0,
            dark_adu_per_s: 0.0,
            full_well_adu: 65535.0,
            hot_pixels: Vec::new(),
            bayer: None,
            vignette: None,
        };
        let scene = crate::deepsky_sim::SimScene {
            width: w,
            height: h,
            background_adu: 180.0,
            gradient_adu_per_px: (0.4, -0.25),
            color: [1.0, 1.0, 1.0],
            stars: vec![crate::deepsky_sim::SimStar {
                // "Nebulosa": blob gaussiano de 36 px de FWHM y pico ~64 ADU.
                x: 96.0,
                y: 96.0,
                flux_adu: 94_000.0,
                fwhm_px: 36.0,
                moffat_beta: None,
            }],
        };
        let n = 16usize;
        let mut half_a = vec![0.0f64; w * h];
        let mut half_b = vec![0.0f64; w * h];
        for k in 0..n {
            let exp = crate::deepsky_sim::SimExposure {
                exposure_s: 60.0,
                dx: 0.0,
                dy: 0.0,
                seed: 40_000 + k as u64,
            };
            let (frame, _) = crate::deepsky_sim::render_light(&scene, &sensor, &exp);
            let dst = if k % 2 == 0 { &mut half_a } else { &mut half_b };
            for (d, v) in dst.iter_mut().zip(&frame) {
                *d += *v as f64 / (n as f64 / 2.0);
            }
        }
        let a32: Vec<f32> = half_a.iter().map(|&v| v as f32).collect();
        let b32: Vec<f32> = half_b.iter().map(|&v| v as f32).collect();
        let ma = fit_background_model(&a32, w, h, 1).expect("modelo A");
        let mb = fit_background_model(&b32, w, h, 1).expect("modelo B");
        // σ del fondo de cada mitad ≈ sqrt(σ²_frame / (n/2)).
        let sigma_half = ((180.0f64 + 9.0) / (n as f64 / 2.0)).sqrt();
        let rms = bg_split_half_rms(&ma, &mb);
        assert!(
            rms < 0.2 * sigma_half.max(1.0),
            "split-half rms {rms:.3} ADU >= {:.3}",
            0.2 * sigma_half.max(1.0)
        );
        // El modelo no debe seguir a la nebulosa: su corrección en el centro
        // del blob no puede superar el 12% del pico del blob (~64 ADU); un
        // ajuste flexible que absorbiera el objeto lo superaría de largo.
        let peak = 94_000.0 / (2.0 * std::f64::consts::PI * (36.0 / 2.3548f64).powi(2));
        let center_correction = ma.eval(0, 0.5, 0.5) - ma.level[0];
        assert!(
            center_correction.abs() < 0.12 * peak,
            "el BG absorbe la nebulosa: corrección {center_correction:.2} ADU vs pico {peak:.2}"
        );
    }
}
