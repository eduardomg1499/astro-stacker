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
                ds.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
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
            for cell in 0..cells {
                if let (Some(vi), Some(vj)) = (samples[i][cell], samples[j][cell]) {
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
        rms_residual,
    })
}

/// Rejilla de muestras de fondo de un frame para el grafo: percentil 15 por
/// celda (mismo estimador robusto que el modelo BG), `None` en celdas con
/// menos de 8 muestras válidas o completamente sin datos (valor exacto 0.0 de
/// los bordes de warp).
pub(crate) fn background_cell_samples(
    data: &[f32],
    w: usize,
    h: usize,
    ch: usize,
    channel: usize,
    gw: usize,
    gh: usize,
) -> Vec<Option<f64>> {
    let mut out = vec![None; gw * gh];
    if w == 0 || h == 0 || channel >= ch {
        return out;
    }
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
                    let v = data[(y * w + x) * ch + channel];
                    if v.is_finite() && v != 0.0 {
                        cell.push(v);
                    }
                    x += sx;
                }
                y += sy;
            }
            if cell.len() < 8 {
                continue;
            }
            cell.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            out[gy * gw + gx] = Some(cell[cell.len() * 3 / 20] as f64);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 2. Modelo BG de sesión (no destructivo, exportable y validable)
// ---------------------------------------------------------------------------

/// Modelo polinómico de fondo por canal. La corrección que la resta clásica
/// aplicaría es `g(x,y) − level` (conserva la mediana del frame).
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
        out.par_chunks_mut(w * ch)
            .enumerate()
            .for_each(|(y, row)| {
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
pub(crate) fn fit_background_model(
    data: &[f32],
    w: usize,
    h: usize,
    ch: usize,
) -> Option<BgModel> {
    const GRID: usize = 32;
    const DEG: usize = 2;
    let nterms = (DEG + 1) * (DEG + 2) / 2;
    if w < GRID * 3 || h < GRID * 3 {
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
                        cell.push(data[(y * w + x) * ch + c]);
                        x += sx;
                    }
                    y += sy;
                }
                if cell.len() < 8 {
                    continue;
                }
                cell.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
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
                let b = crate::ds_poly_basis(xs[i], ys[i], DEG);
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
                    let f: f64 = crate::ds_poly_basis(xs[i], ys[i], DEG)
                        .iter()
                        .zip(&coef)
                        .map(|(b, cc)| b * cc)
                        .sum();
                    res.push(vs[i] - f);
                }
            }
            let mut ares: Vec<f64> = res.iter().map(|r| r.abs()).collect();
            ares.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let sigma = ares[ares.len() / 2] * 1.4826 + 1e-6;
            for i in 0..vs.len() {
                if keep[i] {
                    let f: f64 = crate::ds_poly_basis(xs[i], ys[i], DEG)
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
                crate::ds_poly_basis(xs[i], ys[i], DEG)
                    .iter()
                    .zip(&coef)
                    .map(|(b, cc)| b * cc)
                    .sum()
            })
            .collect();
        fitted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        level[c] = fitted[fitted.len() / 2];
        coeffs[c] = coef;
    }
    if coeffs.iter().all(|c| c.is_empty()) {
        return None;
    }
    Some(BgModel {
        degree: DEG,
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
    fn gate_f2_sampling_advisor_classifies_synthetic_scenarios() {
        assert_eq!(
            advise_sampling(1.2),
            (SamplingClass::Undersampled, "2x")
        );
        assert_eq!(
            advise_sampling(1.7),
            (SamplingClass::Undersampled, "1.5x")
        );
        assert_eq!(advise_sampling(2.5), (SamplingClass::WellSampled, "1x"));
        assert_eq!(
            advise_sampling(4.2),
            (SamplingClass::Oversampled, "0.75x")
        );
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
        let sessions: [(f64, (f64, f64)); 3] =
            [(200.0, (0.3, 0.0)), (260.0, (-0.2, 0.25)), (230.0, (0.0, -0.35))];
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
