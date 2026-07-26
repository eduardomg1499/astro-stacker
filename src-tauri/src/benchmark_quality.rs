//! Métricas de calidad y artefactos para el arnés A/B PLANETARIO (F0).
//!
//! Complementa a `benchmark.rs` (que ya cubre RMSE/MAE, linealidad
//! scale/offset, FWHM estelar y paridad CPU/GPU) con las métricas que
//! detectan los artefactos específicos del apilado planetario/lunar/solar:
//!
//! - **Nitidez del limbo**: distancia de subida 10–90 % del perfil radial
//!   (ESF) por sectores angulares — el proxy directo de "más nítido que
//!   AutoStakkert" en el borde del disco.
//! - **Ringing del limbo**: overshoot/undershoot respecto a las mesetas
//!   interior/exterior del perfil radial, como fracción del salto del limbo.
//! - **Seams entre APs**: energía de gradiente sobre las fronteras de las
//!   celdas de Alignment Points frente al interior (ratio ≈ 1 ⇒ sin
//!   costuras).
//! - **Ocupación de rango**: percentil 99.99 frente a la profundidad
//!   declarada de la fuente (detecta el bug de bd_gain: imagen 4× oscura).
//! - **PSNR/SSIM**: determinismo entre runs y comparación contra el stack
//!   de referencia (AS!4) con la misma geometría.
//! - **Spearman**: correlación de rankings de calidad (contra la verdad
//!   del simulador o contra el listado de AS!4).
//! - **Escalón de mosaico**: salto de brillo al cruzar la frontera entre
//!   paneles.
//!
//! Todas las funciones son puras (slices de entrada, structs Serialize de
//! salida) para poder usarse igual desde tests, desde el runner A/B con
//! ficheros reales y, más adelante, desde comandos tauri.

#![allow(dead_code)]

use serde::Serialize;

/// Versión de las métricas: se escribe en cada informe para que dos
/// baselines solo se comparen si midieron lo mismo.
pub const QUALITY_METRICS_VERSION: &str = "pq1";

// ---------------------------------------------------------------------------
// Utilidades básicas
// ---------------------------------------------------------------------------

/// Luma lineal simple (media de canales) en f64. `channels` ∈ {1, 3}.
fn to_luma_f64(img: &[u16], channels: usize) -> Vec<f64> {
    match channels {
        1 => img.iter().map(|&v| v as f64).collect(),
        3 => img
            .chunks_exact(3)
            .map(|px| (px[0] as f64 + px[1] as f64 + px[2] as f64) / 3.0)
            .collect(),
        _ => panic!("channels debe ser 1 o 3"),
    }
}

/// Muestreo bilineal de una imagen mono f64. None fuera de rango.
fn sample_bilinear(img: &[f64], w: usize, h: usize, x: f64, y: f64) -> Option<f64> {
    if x < 0.0 || y < 0.0 {
        return None;
    }
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    if x0 + 1 >= w || y0 + 1 >= h {
        return None;
    }
    let fx = x - x0 as f64;
    let fy = y - y0 as f64;
    let p00 = img[y0 * w + x0];
    let p10 = img[y0 * w + x0 + 1];
    let p01 = img[(y0 + 1) * w + x0];
    let p11 = img[(y0 + 1) * w + x0 + 1];
    Some(
        p00 * (1.0 - fx) * (1.0 - fy)
            + p10 * fx * (1.0 - fy)
            + p01 * (1.0 - fx) * fy
            + p11 * fx * fy,
    )
}

fn median_f64(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

/// Percentil por histograma de 65536 bins (exacto para u16).
fn percentile_u16(img: &[u16], p: f64) -> u16 {
    if img.is_empty() {
        return 0;
    }
    let mut hist = vec![0u64; 65536];
    for &v in img {
        hist[v as usize] += 1;
    }
    let target = (img.len() as f64 * p).floor().min(img.len() as f64 - 1.0) as u64;
    let mut seen = 0u64;
    for (v, &count) in hist.iter().enumerate() {
        seen += count;
        if seen > target {
            return v as u16;
        }
    }
    65535
}

// ---------------------------------------------------------------------------
// Geometría del disco
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize)]
pub struct DiscGeometry {
    pub cx: f64,
    pub cy: f64,
    pub radius_px: f64,
    pub threshold_adu: f64,
}

/// Estima centro y radio del disco por umbral medio entre las mesetas de
/// fondo y disco (p5/p99), centroide y área. None si no hay disco plausible.
pub fn estimate_disc_geometry(
    img: &[u16],
    w: usize,
    h: usize,
    channels: usize,
) -> Option<DiscGeometry> {
    let luma = to_luma_f64(img, channels);
    let luma_u16: Vec<u16> = luma
        .iter()
        .map(|&v| v.round().clamp(0.0, 65535.0) as u16)
        .collect();
    let lo = percentile_u16(&luma_u16, 0.05) as f64;
    let hi = percentile_u16(&luma_u16, 0.99) as f64;
    if hi - lo < 500.0 {
        return None; // sin contraste disco/fondo suficiente
    }
    // Umbral bajo (25 % del salto): captura el disco COMPLETO aunque la
    // textura (bandas, óvalos) o el oscurecimiento de limbo acerquen zonas
    // del disco al fondo — un umbral al 50 % sesga el centroide hacia las
    // zonas brillantes.
    let threshold = lo + 0.25 * (hi - lo);
    let (mut sx, mut sy, mut n) = (0.0f64, 0.0f64, 0.0f64);
    for y in 0..h {
        for x in 0..w {
            if luma[y * w + x] >= threshold {
                sx += x as f64;
                sy += y as f64;
                n += 1.0;
            }
        }
    }
    if n < 200.0 {
        return None;
    }
    Some(DiscGeometry {
        cx: sx / n,
        cy: sy / n,
        radius_px: (n / std::f64::consts::PI).sqrt(),
        threshold_adu: threshold,
    })
}

// ---------------------------------------------------------------------------
// Perfil radial del limbo: nitidez (ESF 10–90 %) y ringing
// ---------------------------------------------------------------------------

const PROFILE_STEP_PX: f64 = 0.25;

/// Perfil radial mono desde 0.70·r hasta 1.30·r. None si el rayo sale de la
/// imagen.
fn radial_profile(
    luma: &[f64],
    w: usize,
    h: usize,
    geo: &DiscGeometry,
    angle_rad: f64,
) -> Option<Vec<f64>> {
    let r0 = geo.radius_px * 0.70;
    let r1 = geo.radius_px * 1.30;
    let steps = ((r1 - r0) / PROFILE_STEP_PX).ceil() as usize;
    let (dx, dy) = (angle_rad.cos(), angle_rad.sin());
    let mut out = Vec::with_capacity(steps + 1);
    for s in 0..=steps {
        let r = r0 + s as f64 * PROFILE_STEP_PX;
        let v = sample_bilinear(luma, w, h, geo.cx + dx * r, geo.cy + dy * r)?;
        out.push(v);
    }
    Some(out)
}

/// Mesetas interior/exterior y validación de contraste de un perfil.
/// Devuelve (inner, outer, step) o None si el sector no tiene salto útil
/// (p.ej. terminador lunar).
fn profile_plateaus(profile: &[f64]) -> Option<(f64, f64, f64)> {
    let n = profile.len();
    if n < 20 {
        return None;
    }
    // Interior: [0.70r, 0.85r] = primer cuarto del perfil. Exterior:
    // [1.15r, 1.30r] = último cuarto.
    let mut inner: Vec<f64> = profile[..n / 4].to_vec();
    let mut outer: Vec<f64> = profile[3 * n / 4..].to_vec();
    let inner_med = median_f64(&mut inner);
    let outer_med = median_f64(&mut outer);
    let step = inner_med - outer_med;
    if step < 0.08 * inner_med.max(1.0) || step <= 0.0 {
        return None;
    }
    Some((inner_med, outer_med, step))
}

/// FWHM (en índices fraccionarios de perfil) del pico de la LSF (derivada
/// del perfil, en valor de descenso). Medir el ancho de la LSF en vez de la
/// subida 10–90 % del perfil aísla el flanco del seeing/óptica del
/// oscurecimiento de limbo, que es una rampa lenta y ensancharía la ESF.
/// Devuelve (idx_pico, fwhm_en_samples) o None si no hay flanco.
fn lsf_fwhm(t: &[f64]) -> Option<(usize, f64)> {
    let n = t.len();
    if n < 8 {
        return None;
    }
    // LSF = -dt/di por diferencias centradas (positiva en el descenso).
    let mut lsf = vec![0.0f64; n];
    for i in 1..n - 1 {
        lsf[i] = (t[i - 1] - t[i + 1]) * 0.5;
    }
    let (mut peak_i, mut peak_v) = (0usize, 0.0f64);
    for i in 1..n - 1 {
        if lsf[i] > peak_v {
            peak_v = lsf[i];
            peak_i = i;
        }
    }
    if peak_v <= 0.0 || peak_i == 0 {
        return None;
    }
    let half = peak_v * 0.5;
    // Cruce por la mitad a la izquierda del pico (interpolado).
    let mut left = None;
    for i in (1..=peak_i).rev() {
        if lsf[i - 1] <= half && lsf[i] >= half {
            let denom = lsf[i] - lsf[i - 1];
            let frac = if denom.abs() > 1e-12 {
                (lsf[i] - half) / denom
            } else {
                0.0
            };
            left = Some(i as f64 - frac.clamp(0.0, 1.0));
            break;
        }
    }
    // Cruce a la derecha.
    let mut right = None;
    for i in peak_i..n - 1 {
        if lsf[i] >= half && lsf[i + 1] <= half {
            let denom = lsf[i] - lsf[i + 1];
            let frac = if denom.abs() > 1e-12 {
                (lsf[i] - half) / denom
            } else {
                0.0
            };
            right = Some(i as f64 + frac.clamp(0.0, 1.0));
            break;
        }
    }
    match (left, right) {
        (Some(l), Some(r)) if r > l => Some((peak_i, r - l)),
        _ => None,
    }
}

/// Máxima "remontada" tras un descenso dentro de un segmento: cuánto vuelve
/// a subir t después de haber bajado (no-monotonicidad). Un flanco limpio es
/// monótono (≈0); un lóbulo de ringing remonta. En fracción del salto del
/// limbo (t está normalizado).
fn max_bounce(segment: &[f64]) -> f64 {
    let mut run_min = f64::MAX;
    let mut bounce = 0.0f64;
    for &v in segment {
        run_min = run_min.min(v);
        bounce = bounce.max(v - run_min);
    }
    bounce
}

#[derive(Clone, Debug, Serialize)]
pub struct LimbSharpness {
    /// FWHM de la LSF del limbo (px), mediana entre sectores. Para un flanco
    /// gaussiano puro de sigma σ ≈ 2.355·σ.
    pub lsf_fwhm_px_median: f64,
    /// Percentil 90 entre sectores (peor zona del limbo).
    pub lsf_fwhm_px_p90: f64,
    pub sectors_measured: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct LimbRinging {
    /// Máximo overshoot exterior entre sectores, como fracción del salto.
    pub overshoot_frac_max: f64,
    /// Máximo undershoot interior cerca del flanco, fracción del salto.
    pub undershoot_frac_max: f64,
    pub sectors_measured: usize,
}

/// Nitidez y ringing del limbo en un solo barrido de sectores.
pub fn limb_metrics(
    img: &[u16],
    w: usize,
    h: usize,
    channels: usize,
    geo: &DiscGeometry,
    n_sectors: usize,
) -> (Option<LimbSharpness>, Option<LimbRinging>) {
    let luma = to_luma_f64(img, channels);
    let mut fwhms = Vec::new();
    let mut overshoot_max = 0.0f64;
    let mut undershoot_max = 0.0f64;
    let mut ring_sectors = 0usize;
    for s in 0..n_sectors {
        let angle = s as f64 / n_sectors as f64 * std::f64::consts::TAU;
        let Some(profile) = radial_profile(&luma, w, h, geo, angle) else {
            continue;
        };
        let Some((_inner, outer, step)) = profile_plateaus(&profile) else {
            continue;
        };
        let t: Vec<f64> = profile.iter().map(|&v| (v - outer) / step).collect();
        let Some((peak_i, fwhm_samples)) = lsf_fwhm(&t) else {
            continue;
        };
        fwhms.push(fwhm_samples * PROFILE_STEP_PX);
        // Ringing = no-monotonicidad del perfil a cada lado del flanco:
        // hacia fuera, cuánto REMONTA t tras el descenso (lóbulo positivo
        // exterior); hacia dentro (recorrido interior→flanco), ídem para el
        // lóbulo negativo pegado al limbo. Ambos en fracción del salto.
        let out_seg = &t[peak_i.min(t.len())..];
        overshoot_max = overshoot_max.max(max_bounce(out_seg));
        // Franja interior de 4 px acabando en el flanco, en sentido
        // meseta→flanco (t desciende): un dip que RECUPERA antes del
        // descenso final es el lóbulo negativo del ringing. La franja corta
        // evita confundir la textura del disco (bandas) con ringing.
        let inner_band = (4.0 / PROFILE_STEP_PX) as usize;
        let in_start = peak_i.saturating_sub(inner_band);
        undershoot_max = undershoot_max.max(max_bounce(&t[in_start..peak_i]));
        ring_sectors += 1;
    }
    let sharpness = if fwhms.len() >= 8 {
        let mut sorted = fwhms.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Some(LimbSharpness {
            lsf_fwhm_px_median: sorted[sorted.len() / 2],
            lsf_fwhm_px_p90: sorted[(sorted.len() * 9 / 10).min(sorted.len() - 1)],
            sectors_measured: fwhms.len(),
        })
    } else {
        None
    };
    let ringing = if ring_sectors >= 8 {
        Some(LimbRinging {
            overshoot_frac_max: overshoot_max,
            undershoot_frac_max: undershoot_max,
            sectors_measured: ring_sectors,
        })
    } else {
        None
    };
    (sharpness, ringing)
}

// ---------------------------------------------------------------------------
// Seams entre celdas de Alignment Points
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct ApSeamMetric {
    pub boundary_mean_grad: f64,
    pub interior_mean_grad: f64,
    /// ≈1.0 sin costuras; >1 indica energía de gradiente extra en las
    /// fronteras de las celdas de AP.
    pub seam_ratio: f64,
    pub boundary_lines: usize,
}

/// Compara la energía de gradiente sobre las fronteras de celdas de AP
/// (midlines entre coordenadas adyacentes de AP) frente al interior.
pub fn ap_seam_metric(
    img: &[u16],
    w: usize,
    h: usize,
    channels: usize,
    ap_xs: &[f32],
    ap_ys: &[f32],
) -> Option<ApSeamMetric> {
    if ap_xs.len() < 2 || ap_ys.len() < 2 || w < 8 || h < 8 {
        return None;
    }
    let luma = to_luma_f64(img, channels);
    let midlines = |coords: &[f32], limit: usize| -> Vec<usize> {
        let mut unique: Vec<i64> = coords.iter().map(|&c| c.round() as i64).collect();
        unique.sort_unstable();
        unique.dedup();
        unique
            .windows(2)
            .filter(|pair| pair[1] - pair[0] > 4)
            .map(|pair| ((pair[0] + pair[1]) / 2) as usize)
            .filter(|&m| m >= 2 && m + 2 < limit)
            .collect()
    };
    let bx = midlines(ap_xs, w);
    let by = midlines(ap_ys, h);
    if bx.is_empty() && by.is_empty() {
        return None;
    }
    // Cada frontera se compara contra bandas de REFERENCIA paralelas a
    // 4–6 px de distancia, sobre las MISMAS filas/columnas: así el disco se
    // compara con disco y el cielo con cielo, y el ratio no se sesga porque
    // una frontera cruce zonas con más estructura que la media global.
    let grad_x_at =
        |x: usize, y: usize| -> f64 { ((luma[y * w + x + 1] - luma[y * w + x - 1]) * 0.5).abs() };
    let grad_y_at = |x: usize, y: usize| -> f64 {
        ((luma[(y + 1) * w + x] - luma[(y - 1) * w + x]) * 0.5).abs()
    };
    let (mut b_sum, mut b_n, mut r_sum, mut r_n) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut ratios: Vec<f64> = Vec::new();
    for &xb in &bx {
        if xb < 7 || xb + 7 >= w {
            continue;
        }
        let (mut b, mut bn, mut r, mut rn) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for y in 1..h - 1 {
            for dx in -1i64..=1 {
                b += grad_x_at((xb as i64 + dx) as usize, y);
                bn += 1.0;
            }
            for &dx in &[-6i64, -5, -4, 4, 5, 6] {
                r += grad_x_at((xb as i64 + dx) as usize, y);
                rn += 1.0;
            }
        }
        if bn > 32.0 && rn > 32.0 && r / rn > 1e-9 {
            ratios.push((b / bn) / (r / rn));
            b_sum += b;
            b_n += bn;
            r_sum += r;
            r_n += rn;
        }
    }
    for &yb in &by {
        if yb < 7 || yb + 7 >= h {
            continue;
        }
        let (mut b, mut bn, mut r, mut rn) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for x in 1..w - 1 {
            for dy in -1i64..=1 {
                b += grad_y_at(x, (yb as i64 + dy) as usize);
                bn += 1.0;
            }
            for &dy in &[-6i64, -5, -4, 4, 5, 6] {
                r += grad_y_at(x, (yb as i64 + dy) as usize);
                rn += 1.0;
            }
        }
        if bn > 32.0 && rn > 32.0 && r / rn > 1e-9 {
            ratios.push((b / bn) / (r / rn));
            b_sum += b;
            b_n += bn;
            r_sum += r;
            r_n += rn;
        }
    }
    if ratios.is_empty() {
        return None;
    }
    // La PEOR frontera manda: una única costura visible ya es un artefacto.
    let worst = ratios.iter().cloned().fold(f64::MIN, f64::max);
    Some(ApSeamMetric {
        boundary_mean_grad: b_sum / b_n.max(1.0),
        interior_mean_grad: r_sum / r_n.max(1.0),
        seam_ratio: worst,
        boundary_lines: ratios.len(),
    })
}

// ---------------------------------------------------------------------------
// Ocupación de rango (detector del bug bd_gain)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct RangeOccupancy {
    pub p9999_adu: u16,
    pub max_adu: u16,
    pub declared_max_adu: u16,
    /// p99.99 / máximo declarado. Un stack bien expandido de fuente 8-bit
    /// debería rondar ≥0.5; ~0.25 delata una expansión de bits fallida.
    pub occupancy_frac: f64,
}

pub fn range_occupancy(img: &[u16], declared_max_adu: u16) -> RangeOccupancy {
    let p9999 = percentile_u16(img, 0.9999);
    let max = img.iter().copied().max().unwrap_or(0);
    RangeOccupancy {
        p9999_adu: p9999,
        max_adu: max,
        declared_max_adu,
        occupancy_frac: p9999 as f64 / declared_max_adu.max(1) as f64,
    }
}

// ---------------------------------------------------------------------------
// PSNR / SSIM
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PsnrSsim {
    /// dB sobre rango 65535; 99.0 significa "idéntico o mejor que 99 dB".
    pub psnr_db: f64,
    pub ssim: f64,
}

pub fn psnr_ssim(a: &[u16], b: &[u16], w: usize, h: usize) -> Option<PsnrSsim> {
    if a.len() != b.len() || a.len() != w * h || a.is_empty() {
        return None;
    }
    let mut se = 0.0f64;
    for i in 0..a.len() {
        let d = a[i] as f64 - b[i] as f64;
        se += d * d;
    }
    let mse = se / a.len() as f64;
    let psnr = if mse <= f64::EPSILON {
        99.0
    } else {
        (20.0 * (65535.0f64).log10() - 10.0 * mse.log10()).min(99.0)
    };
    // SSIM por bloques 8×8 (constantes estándar, L = 65535).
    let l = 65535.0f64;
    let c1 = (0.01 * l) * (0.01 * l);
    let c2 = (0.03 * l) * (0.03 * l);
    let block = 8usize;
    let (mut ssim_sum, mut blocks) = (0.0f64, 0.0f64);
    let mut by = 0;
    while by + block <= h {
        let mut bx = 0;
        while bx + block <= w {
            let (mut ma, mut mb) = (0.0f64, 0.0f64);
            for y in by..by + block {
                for x in bx..bx + block {
                    ma += a[y * w + x] as f64;
                    mb += b[y * w + x] as f64;
                }
            }
            let n = (block * block) as f64;
            ma /= n;
            mb /= n;
            let (mut va, mut vb, mut cov) = (0.0f64, 0.0f64, 0.0f64);
            for y in by..by + block {
                for x in bx..bx + block {
                    let da = a[y * w + x] as f64 - ma;
                    let db = b[y * w + x] as f64 - mb;
                    va += da * da;
                    vb += db * db;
                    cov += da * db;
                }
            }
            va /= n - 1.0;
            vb /= n - 1.0;
            cov /= n - 1.0;
            ssim_sum += ((2.0 * ma * mb + c1) * (2.0 * cov + c2))
                / ((ma * ma + mb * mb + c1) * (va + vb + c2));
            blocks += 1.0;
            bx += block;
        }
        by += block;
    }
    if blocks < 1.0 {
        return None;
    }
    Some(PsnrSsim {
        psnr_db: psnr,
        ssim: ssim_sum / blocks,
    })
}

// ---------------------------------------------------------------------------
// Spearman (rankings de calidad)
// ---------------------------------------------------------------------------

fn ranks_with_ties(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| values[a].partial_cmp(&values[b]).unwrap());
    let mut ranks = vec![0.0f64; n];
    let mut i = 0usize;
    while i < n {
        let mut j = i;
        while j + 1 < n && values[idx[j + 1]] == values[idx[i]] {
            j += 1;
        }
        let avg = (i + j) as f64 / 2.0 + 1.0;
        for k in i..=j {
            ranks[idx[k]] = avg;
        }
        i = j + 1;
    }
    ranks
}

/// Correlación de Spearman entre dos secuencias emparejadas. [-1, 1];
/// 0 si n < 2 o varianza nula.
pub fn spearman(a: &[f64], b: &[f64]) -> f64 {
    if a.len() != b.len() || a.len() < 2 {
        return 0.0;
    }
    let ra = ranks_with_ties(a);
    let rb = ranks_with_ties(b);
    let n = a.len() as f64;
    let mean = (n + 1.0) / 2.0;
    let (mut num, mut da, mut db) = (0.0f64, 0.0f64, 0.0f64);
    for i in 0..a.len() {
        let xa = ra[i] - mean;
        let xb = rb[i] - mean;
        num += xa * xb;
        da += xa * xa;
        db += xb * xb;
    }
    if da <= 0.0 || db <= 0.0 {
        return 0.0;
    }
    num / (da.sqrt() * db.sqrt())
}

// ---------------------------------------------------------------------------
// Escalón de costura de mosaico
// ---------------------------------------------------------------------------

/// Salto de brillo (ADU, mediana entre filas con señal) al cruzar una
/// frontera vertical de mosaico en x = boundary_x, comparando bandas de
/// `band` px a cada lado.
pub fn mosaic_seam_step(
    img: &[u16],
    w: usize,
    h: usize,
    channels: usize,
    boundary_x: usize,
    band: usize,
) -> Option<f64> {
    if boundary_x < band || boundary_x + band >= w || band == 0 {
        return None;
    }
    let luma = to_luma_f64(img, channels);
    let mut steps = Vec::new();
    for y in 0..h {
        let (mut left, mut right) = (0.0f64, 0.0f64);
        for k in 0..band {
            left += luma[y * w + boundary_x - 1 - k];
            right += luma[y * w + boundary_x + k];
        }
        left /= band as f64;
        right /= band as f64;
        // Solo filas con señal en ambos lados (evita fondo de cielo).
        if left > 1_000.0 && right > 1_000.0 {
            steps.push((left - right).abs());
        }
    }
    if steps.len() < 8 {
        return None;
    }
    Some(median_f64(&mut steps))
}

// ---------------------------------------------------------------------------
// Informe agregado
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct PlanetaryQualityReport {
    pub metrics_version: String,
    pub source: String,
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub disc: Option<DiscGeometry>,
    pub limb_sharpness: Option<LimbSharpness>,
    pub limb_ringing: Option<LimbRinging>,
    pub range: RangeOccupancy,
    pub vs_reference: Option<PsnrSsim>,
    pub notes: Vec<String>,
}

/// Informe de calidad de una imagen apilada. `reference` debe compartir
/// geometría exacta (mismo crop) para PSNR/SSIM; si no, se anota y se omite.
pub fn quality_report(
    source: &str,
    img: &[u16],
    w: usize,
    h: usize,
    channels: usize,
    declared_max_adu: u16,
    reference: Option<(&[u16], usize, usize)>,
) -> PlanetaryQualityReport {
    let mut notes = Vec::new();
    let disc = estimate_disc_geometry(img, w, h, channels);
    let (limb_sharpness, limb_ringing) = match &disc {
        Some(geo) => limb_metrics(img, w, h, channels, geo, 48),
        None => {
            notes.push("sin disco detectable: métricas de limbo omitidas (¿superficie?)".into());
            (None, None)
        }
    };
    let luma_u16: Vec<u16> = to_luma_f64(img, channels)
        .iter()
        .map(|&v| v.round().clamp(0.0, 65535.0) as u16)
        .collect();
    let range = range_occupancy(&luma_u16, declared_max_adu);
    let vs_reference = match reference {
        Some((r, rw, rh)) if rw == w && rh == h => {
            let r_luma: Vec<u16> = to_luma_f64(r, channels)
                .iter()
                .map(|&v| v.round().clamp(0.0, 65535.0) as u16)
                .collect();
            psnr_ssim(&luma_u16, &r_luma, w, h)
        }
        Some((_, rw, rh)) => {
            notes.push(format!(
                "referencia con geometría distinta ({rw}x{rh} vs {w}x{h}): PSNR/SSIM omitidos; \
                 exporta ambos stacks con el mismo encuadre o usa compare_linear_masters"
            ));
            None
        }
        None => None,
    };
    PlanetaryQualityReport {
        metrics_version: QUALITY_METRICS_VERSION.to_string(),
        source: source.to_string(),
        width: w,
        height: h,
        channels,
        disc,
        limb_sharpness,
        limb_ringing,
        range,
        vs_reference,
        notes,
    }
}

// ---------------------------------------------------------------------------
// Tests: validación de métricas contra verdad conocida + evidencia F0
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planetary_sim::{make_frame_mono16, make_gain_panels, FrameSpec, PlanetScene};

    #[test]
    fn q_spearman_basics() {
        assert!((spearman(&[1.0, 2.0, 3.0, 4.0], &[10.0, 20.0, 30.0, 40.0]) - 1.0).abs() < 1e-12);
        assert!((spearman(&[1.0, 2.0, 3.0, 4.0], &[40.0, 30.0, 20.0, 10.0]) + 1.0).abs() < 1e-12);
        assert_eq!(spearman(&[1.0], &[2.0]), 0.0);
        assert_eq!(spearman(&[5.0, 5.0, 5.0], &[1.0, 2.0, 3.0]), 0.0);
    }

    #[test]
    fn q_psnr_ssim_identity_and_noise() {
        let scene = PlanetScene::jupiter_like(96, 96);
        let a = make_frame_mono16(
            &scene,
            &FrameSpec {
                dx: 0.0,
                dy: 0.0,
                blur_sigma: 0.5,
                noise_adu: 0.0,
                seed: 1,
            },
        );
        let same = psnr_ssim(&a, &a, 96, 96).unwrap();
        assert_eq!(same.psnr_db, 99.0);
        assert!((same.ssim - 1.0).abs() < 1e-9);
        let b = make_frame_mono16(
            &scene,
            &FrameSpec {
                dx: 0.0,
                dy: 0.0,
                blur_sigma: 0.5,
                noise_adu: 400.0,
                seed: 2,
            },
        );
        let diff = psnr_ssim(&a, &b, 96, 96).unwrap();
        assert!(diff.psnr_db < 99.0 && diff.ssim < 0.9999);
    }

    #[test]
    fn q_disc_geometry_matches_scene() {
        let scene = PlanetScene::jupiter_like(160, 160);
        let img = make_frame_mono16(
            &scene,
            &FrameSpec {
                dx: 3.0,
                dy: -2.0,
                blur_sigma: 0.6,
                noise_adu: 80.0,
                seed: 4,
            },
        );
        let geo = estimate_disc_geometry(&img, 160, 160, 1).expect("disco detectable");
        assert!((geo.cx - (scene.cx + 3.0)).abs() < 1.5, "cx: {}", geo.cx);
        assert!((geo.cy - (scene.cy - 2.0)).abs() < 1.5, "cy: {}", geo.cy);
        assert!((geo.radius_px - scene.radius_px).abs() < scene.radius_px * 0.05);
    }

    #[test]
    fn q_limb_sharpness_orders_blur() {
        let scene = PlanetScene::jupiter_like(192, 192);
        let fwhm_of = |sigma: f64| -> f64 {
            let img = make_frame_mono16(
                &scene,
                &FrameSpec {
                    dx: 0.0,
                    dy: 0.0,
                    blur_sigma: sigma,
                    noise_adu: 0.0,
                    seed: 1,
                },
            );
            let geo = estimate_disc_geometry(&img, 192, 192, 1).unwrap();
            limb_metrics(&img, 192, 192, 1, &geo, 48)
                .0
                .expect("nitidez medible")
                .lsf_fwhm_px_median
        };
        let sharp = fwhm_of(0.4);
        let soft = fwhm_of(2.5);
        assert!(
            soft > sharp * 1.6,
            "el blur debe ensanchar la LSF del limbo: sharp={sharp:.2}px soft={soft:.2}px"
        );
    }

    /// Valida el detector de ringing con un perfil construido a mano:
    /// disco suave (sin overshoot) vs disco con lóbulo de ringing añadido
    /// justo fuera del limbo.
    #[test]
    fn q_limb_ringing_detects_overshoot() {
        let (w, h) = (160usize, 160usize);
        let (cx, cy, r) = (80.0f64, 80.0f64, 50.0f64);
        let render = |ringing: f64| -> Vec<u16> {
            let mut img = vec![0u16; w * h];
            for y in 0..h {
                for x in 0..w {
                    let d = ((x as f64 - cx).powi(2) + (y as f64 - cy).powi(2)).sqrt();
                    // Flanco suave de ~2px con tanh.
                    let edge = 0.5 * (1.0 - ((d - r) / 1.2).tanh());
                    let mut v = 400.0 + 40_000.0 * edge;
                    // Lóbulo de overshoot centrado 4px fuera del limbo.
                    v += ringing * 40_000.0 * (-((d - (r + 4.0)) / 1.0).powi(2)).exp();
                    img[y * w + x] = v.clamp(0.0, 65535.0) as u16;
                }
            }
            img
        };
        let clean = render(0.0);
        let ringy = render(0.18);
        let geo_c = estimate_disc_geometry(&clean, w, h, 1).unwrap();
        let geo_r = estimate_disc_geometry(&ringy, w, h, 1).unwrap();
        let ring_clean = limb_metrics(&clean, w, h, 1, &geo_c, 48).1.unwrap();
        let ring_ringy = limb_metrics(&ringy, w, h, 1, &geo_r, 48).1.unwrap();
        assert!(
            ring_clean.overshoot_frac_max < 0.06,
            "disco limpio no debe reportar ringing: {}",
            ring_clean.overshoot_frac_max
        );
        assert!(
            ring_ringy.overshoot_frac_max > 0.12,
            "el lóbulo inyectado debe detectarse: {}",
            ring_ringy.overshoot_frac_max
        );
    }

    #[test]
    fn q_ap_seam_metric_flags_injected_seams() {
        let scene = PlanetScene::jupiter_like(192, 192);
        let img = make_frame_mono16(
            &scene,
            &FrameSpec {
                dx: 0.0,
                dy: 0.0,
                blur_sigma: 0.8,
                noise_adu: 60.0,
                seed: 9,
            },
        );
        // Rejilla de APs 5×5 con paso 32 px empezando en 32.
        let mut xs = Vec::new();
        let mut ys = Vec::new();
        for k in 0..5 {
            xs.push((32 + k * 32) as f32);
            ys.push((32 + k * 32) as f32);
        }
        let clean = ap_seam_metric(&img, 192, 192, 1, &xs, &ys).unwrap();
        assert!(
            clean.seam_ratio < 1.35,
            "imagen sin costuras no debe disparar el ratio: {}",
            clean.seam_ratio
        );
        // Inyectar escalones de brillo en las midlines verticales (48, 80…).
        let mut seamy = img.clone();
        for y in 0..192usize {
            for k in 0..4usize {
                let bx = 48 + k * 32;
                for x in bx..192.min(bx + 16) {
                    let i = y * 192 + x;
                    seamy[i] = (seamy[i] as f32 * 1.08) as u16;
                }
            }
        }
        let with_seams = ap_seam_metric(&seamy, 192, 192, 1, &xs, &ys).unwrap();
        assert!(
            with_seams.seam_ratio > clean.seam_ratio * 1.5,
            "los escalones inyectados deben subir el ratio: {} vs {}",
            with_seams.seam_ratio,
            clean.seam_ratio
        );
    }

    #[test]
    fn q_range_occupancy_detects_wrong_bit_gain() {
        // Fuente 8-bit bien expandida a 16-bit: p99.99 cerca de 65535.
        let good: Vec<u16> = (0..65536usize).map(|i| ((i % 256) * 256) as u16).collect();
        let occ_good = range_occupancy(&good, 65535);
        assert!(occ_good.occupancy_frac > 0.9);
        // La misma fuente con ganancia 4× menor (el bug bd_gain).
        let dark: Vec<u16> = good.iter().map(|&v| v / 4).collect();
        let occ_dark = range_occupancy(&dark, 65535);
        assert!(occ_dark.occupancy_frac < 0.3, "{}", occ_dark.occupancy_frac);
    }

    #[test]
    fn q_mosaic_seam_step_measures_known_gain_delta() {
        let scene = PlanetScene::jupiter_like(200, 120);
        let (a, b, _overlap) = make_gain_panels(&scene, 120, 120, 80, 1.0, 1.12, 0.0, 5);
        // Composición winner-take-all simplificada: A a la izquierda de la
        // frontera x=100 del lienzo, B a la derecha (B empieza en x=80).
        let (cw, ch) = (200usize, 120usize);
        let mut canvas = vec![0u16; cw * ch];
        for y in 0..ch {
            for x in 0..cw {
                canvas[y * cw + x] = if x < 100 {
                    if x < 120 {
                        a[y * 120 + x]
                    } else {
                        0
                    }
                } else if x >= 80 {
                    b[y * 120 + (x - 80)]
                } else {
                    0
                };
            }
        }
        let step = mosaic_seam_step(&canvas, cw, ch, 1, 100, 6).expect("frontera medible");
        // El salto esperado es ~12 % del nivel local del disco en x=100.
        let mut locals = Vec::new();
        for y in 40..80usize {
            locals.push(a[y * 120 + 100] as f64);
        }
        let local = median_f64(&mut locals);
        let expected = local * 0.12;
        assert!(
            (step - expected).abs() < expected * 0.35,
            "escalón medido {step:.0} ADU vs esperado ~{expected:.0} ADU"
        );
    }

    /// GATE PR-1.1 (antes: evidencia F0): el scorer v2 de PRODUCCIÓN debe
    /// ordenar los frames como la verdad conocida del simulador. El scorer
    /// v1 (autonormalizado) medía Spearman = −1.0 en este mismo escenario
    /// (ranking invertido; baseline commiteado en
    /// benchmarks/baselines/planetary-f0-scorer-baseline.json).
    ///
    /// Con ZAS_F0_BASELINE_OUT=<ruta.json> escribe el informe de baseline.
    #[test]
    fn f0_baseline_production_scorer_vs_ground_truth() {
        let scene = PlanetScene::jupiter_like(256, 256);
        // Sigmas conocidos en orden barajado determinista (verdad: menor
        // sigma = mejor frame).
        let sigmas = [
            1.75, 0.25, 3.0, 0.0, 2.25, 1.0, 2.75, 0.5, 1.5, 3.25, 0.75, 2.0, 1.25, 2.5,
        ];
        let mut v2_scores = Vec::new();
        for (i, &sigma) in sigmas.iter().enumerate() {
            let frame = make_frame_mono16(
                &scene,
                &FrameSpec {
                    dx: 0.0,
                    dy: 0.0,
                    blur_sigma: sigma,
                    noise_adu: 80.0,
                    seed: 100 + i as u64,
                },
            );
            // Secuencia EXACTA de producción (process_analysis_frame, camino
            // CPU): downscale 2x -> enhance_and_lap_raw (lap CRUDO) ->
            // score_frame_quality_v2 -> (normalize_lap_for_sad, irrelevante
            // para el score).
            let mut half = Vec::new();
            let (hw, hh) = crate::alignment::downscale_2x_into(&frame, 256, 256, &mut half);
            let mut blur_temp = Vec::new();
            let mut blur_out = Vec::new();
            let mut lap_out = Vec::new();
            let _legacy = crate::enhance_and_lap_raw(
                &half,
                hw,
                hh,
                &mut blur_temp,
                &mut blur_out,
                &mut lap_out,
            );
            let mut quarter: Vec<u16> = Vec::new();
            let score = crate::score_frame_quality_v2(&blur_out, &lap_out, hw, hh, &mut quarter);
            v2_scores.push(score as f64);
        }
        let truth: Vec<f64> = sigmas.iter().map(|&s| -s).collect();
        let rho_v2 = spearman(&v2_scores, &truth);
        println!(
            "PR-1.1 scorer v2: spearman={rho_v2:.3} (verdad: sigma menor = mejor; \
             v1 medía -1.0 en este escenario)"
        );
        if let Ok(out) = std::env::var("ZAS_F0_BASELINE_OUT") {
            let report = serde_json::json!({
                "metrics_version": QUALITY_METRICS_VERSION,
                "scenario": "sintetico jupiter_like 256x256, 14 frames, sigmas 0.0..3.25, ruido 80 ADU",
                "spearman_scorer_v2": rho_v2,
                "spearman_scorer_v1_baseline": -1.0,
                "gate_pr11": 0.95,
            });
            std::fs::write(&out, serde_json::to_string_pretty(&report).unwrap())
                .expect("escribir baseline");
            println!("baseline escrito en {out}");
        }
        assert!(
            rho_v2 >= 0.95,
            "GATE PR-1.1: el scorer v2 debe correlacionar >= 0.95 con la verdad (medido {rho_v2:.3})"
        );
    }

    /// Runner A/B con ficheros reales (ignorado en CI): compara un stack
    /// candidato contra una referencia (p.ej. AutoStakkert!4) con la misma
    /// geometría y escribe el informe JSON.
    ///
    /// ZAS_AB_CANDIDATE=<png/tiff> [ZAS_AB_REFERENCE=<png/tiff>]
    /// [ZAS_AB_OUT=<json>] cargo test --release f0_ab_compare -- --ignored --nocapture
    #[test]
    #[ignore = "runner manual con ficheros reales (ZAS_AB_CANDIDATE)"]
    fn f0_ab_compare() {
        let cand_path = std::env::var("ZAS_AB_CANDIDATE").expect("define ZAS_AB_CANDIDATE");
        let load = |p: &str| -> (Vec<u16>, usize, usize) {
            let img = image::open(p).unwrap_or_else(|e| panic!("no puedo abrir {p}: {e}"));
            let rgba = img.to_rgba16();
            let (w, h) = (rgba.width() as usize, rgba.height() as usize);
            let mut rgb = Vec::with_capacity(w * h * 3);
            for px in rgba.pixels() {
                rgb.extend_from_slice(&[px[0], px[1], px[2]]);
            }
            (rgb, w, h)
        };
        let (cand, w, h) = load(&cand_path);
        let reference = std::env::var("ZAS_AB_REFERENCE").ok().map(|p| load(&p));
        let report = quality_report(
            &cand_path,
            &cand,
            w,
            h,
            3,
            65535,
            reference
                .as_ref()
                .map(|(r, rw, rh)| (r.as_slice(), *rw, *rh)),
        );
        let json = serde_json::to_string_pretty(&report).unwrap();
        println!("{json}");
        if let Ok(out) = std::env::var("ZAS_AB_OUT") {
            std::fs::write(&out, &json).expect("escribir informe A/B");
            println!("informe escrito en {out}");
        }
    }
}
