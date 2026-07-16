//! F7: STRUCT — starlet B3 à trous + validación split-half con control FDR.
//!
//! STRUCT es una reconstrucción de EVIDENCIA multiescala (plan técnico §6.7):
//! solo contiene las estructuras que REAPARECEN en dos mitades independientes
//! del dataset. Nunca sustituye al máster SCI. Aceptación de cada coeficiente
//! de detalle: BH-FDR (q) por nivel en AMBAS mitades + mismo signo +
//! min(|z_A|,|z_B|) ≥ z_min. El coarse pasa SIN umbral (V1 del plan). El
//! refit PCG de amplitudes queda POSPUESTO: los umbrales sesgan levemente a
//! la baja las amplitudes aceptadas (documentado en la receta como
//! structRefit=false).

#![allow(dead_code)]

use std::sync::OnceLock;

/// Kernel B3-spline 1D del starlet canónico.
const B3: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];
/// Niveles máximos (escala 2^7 = 128 px).
const MAX_LEVELS: usize = 7;

pub(crate) struct StructOutput {
    /// coarse + detalles ACEPTADOS (mismas unidades que la entrada).
    pub struct_map: Vec<f32>,
    /// entrada − struct_map (detalles rechazados).
    pub residual: Vec<f32>,
    /// Por nivel: (aceptados, total).
    pub accepted_per_level: Vec<(usize, usize)>,
    pub levels: usize,
}

/// Convolución à trous separable con paso `step` y bordes por reflexión.
fn atrous_smooth(src: &[f32], w: usize, h: usize, step: usize) -> Vec<f32> {
    let mirror = |i: isize, n: usize| -> usize {
        let n = n as isize;
        let mut i = i;
        if i < 0 {
            i = -i;
        }
        if i >= n {
            i = 2 * n - 2 - i;
        }
        i.clamp(0, n - 1) as usize
    };
    // Horizontal.
    let mut tmp = vec![0.0f32; w * h];
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            let mut acc = 0.0f32;
            for (t, &k) in B3.iter().enumerate() {
                let dx = (t as isize - 2) * step as isize;
                acc += k * src[row + mirror(x as isize + dx, w)];
            }
            tmp[row + x] = acc;
        }
    }
    // Vertical.
    let mut out = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0f32;
            for (t, &k) in B3.iter().enumerate() {
                let dy = (t as isize - 2) * step as isize;
                acc += k * tmp[mirror(y as isize + dy, h) * w + x];
            }
            out[y * w + x] = acc;
        }
    }
    out
}

/// Descomposición starlet B3 à trous canónica: d_j = c_j − c_{j+1}.
/// Devuelve (detalles a resolución completa, coarse).
pub(crate) fn starlet_decompose(
    data: &[f32],
    w: usize,
    h: usize,
    levels: usize,
) -> (Vec<Vec<f32>>, Vec<f32>) {
    let mut details = Vec::with_capacity(levels);
    let mut current = data.to_vec();
    for j in 0..levels {
        let smooth = atrous_smooth(&current, w, h, 1 << j);
        let d: Vec<f32> = current
            .iter()
            .zip(&smooth)
            .map(|(c, s)| c - s)
            .collect();
        details.push(d);
        current = smooth;
    }
    (details, current)
}

/// Niveles recomendados para una imagen: min(7, log2(min(w,h)) − 2).
pub(crate) fn recommended_levels(w: usize, h: usize) -> usize {
    let m = w.min(h).max(8);
    let log2 = (usize::BITS - 1 - m.leading_zeros()) as usize;
    log2.saturating_sub(2).clamp(1, MAX_LEVELS)
}

/// Constantes de propagación de ruido blanco N(0,1) por nivel del starlet B3
/// (σ del detalle de cada nivel). Medidas UNA vez numéricamente sobre ruido
/// determinista (LCG + Box-Muller) — nivel 0 ≈ 0.889, paridad con el factor
/// de `ds_mrs_noise`/PixInsight.
pub(crate) fn norm_b3(levels: usize) -> &'static [f32] {
    static NORMS: OnceLock<Vec<f32>> = OnceLock::new();
    let v = NORMS.get_or_init(|| {
        let (w, h) = (256, 256);
        let mut state = 0x9E3779B97F4A7C15u64;
        let mut uniform = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 11) as f64 / (1u64 << 53) as f64).clamp(1e-12, 1.0 - 1e-12)
        };
        let mut noise = vec![0.0f32; w * h];
        let mut i = 0;
        while i + 1 < w * h {
            let (u1, u2) = (uniform(), uniform());
            let r = (-2.0 * u1.ln()).sqrt();
            noise[i] = (r * (2.0 * std::f64::consts::PI * u2).cos()) as f32;
            noise[i + 1] = (r * (2.0 * std::f64::consts::PI * u2).sin()) as f32;
            i += 2;
        }
        let (details, _) = starlet_decompose(&noise, w, h, MAX_LEVELS);
        details
            .iter()
            .map(|d| {
                let mean = d.iter().map(|&v| v as f64).sum::<f64>() / d.len() as f64;
                (d.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>()
                    / (d.len() as f64 - 1.0))
                    .sqrt() as f32
            })
            .collect()
    });
    &v[..levels.min(v.len())]
}

/// erfc por aproximación Abramowitz–Stegun 7.1.26 (error < 1.5e-7).
fn erfc(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let y = t
        * (0.254829592
            + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429))))
        * (-x * x).exp();
    if sign < 0.0 {
        2.0 - y
    } else {
        y
    }
}

/// p-valor bilateral de un z-score gaussiano.
fn p_two_sided(z: f32) -> f64 {
    erfc((z.abs() as f64) / std::f64::consts::SQRT_2)
}

/// Umbral BH-FDR: mayor p_(i) ≤ q·i/m entre los p-valores ordenados.
/// Devuelve el corte de |z| equivalente (aceptar si p ≤ p_corte).
fn bh_fdr_pcut(pvals: &mut Vec<f64>, q: f64) -> f64 {
    if pvals.is_empty() {
        return 0.0;
    }
    pvals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let m = pvals.len() as f64;
    let mut cut = 0.0f64;
    for (i, &p) in pvals.iter().enumerate() {
        if p <= q * (i as f64 + 1.0) / m {
            cut = p;
        }
    }
    cut
}

/// Construye STRUCT validado por mitades (ver doc del módulo).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_struct(
    full: &[f32],
    half_a: &[f32],
    half_b: &[f32],
    w: usize,
    h: usize,
    sigma_a: f32,
    sigma_b: f32,
    q: f32,
    z_min: f32,
) -> StructOutput {
    let npx = w * h;
    let levels = recommended_levels(w, h);
    let norms = norm_b3(levels);
    let (d_full, coarse) = starlet_decompose(full, w, h, levels);
    let (d_a, _) = starlet_decompose(half_a, w, h, levels);
    let (d_b, _) = starlet_decompose(half_b, w, h, levels);

    let mut struct_map = coarse.clone();
    let mut accepted_per_level = Vec::with_capacity(levels);
    let sa = sigma_a.max(1e-6);
    let sb = sigma_b.max(1e-6);
    for j in 0..levels {
        let nj = norms[j].max(1e-6);
        // z por mitad y p-valores para el corte BH del nivel.
        let mut p_a: Vec<f64> = Vec::with_capacity(npx);
        let mut p_b: Vec<f64> = Vec::with_capacity(npx);
        for p in 0..npx {
            p_a.push(p_two_sided(d_a[j][p] / (sa * nj)));
            p_b.push(p_two_sided(d_b[j][p] / (sb * nj)));
        }
        let cut_a = bh_fdr_pcut(&mut p_a.clone(), q as f64);
        let cut_b = bh_fdr_pcut(&mut p_b.clone(), q as f64);
        let mut accepted = 0usize;
        for p in 0..npx {
            let za = d_a[j][p] / (sa * nj);
            let zb = d_b[j][p] / (sb * nj);
            let ok = p_a[p] <= cut_a
                && p_b[p] <= cut_b
                && (za.signum() == zb.signum())
                && za.abs().min(zb.abs()) >= z_min;
            if ok {
                struct_map[p] += d_full[j][p];
                accepted += 1;
            }
        }
        accepted_per_level.push((accepted, npx));
    }
    let residual: Vec<f32> = full
        .iter()
        .zip(&struct_map)
        .map(|(f, s)| f - s)
        .collect();
    StructOutput {
        struct_map,
        residual,
        accepted_per_level,
        levels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg_normal(seed: u64, n: usize) -> Vec<f32> {
        let mut state = seed;
        let mut uniform = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 11) as f64 / (1u64 << 53) as f64).clamp(1e-12, 1.0 - 1e-12)
        };
        let mut out = vec![0.0f32; n];
        let mut i = 0;
        while i + 1 < n {
            let (u1, u2) = (uniform(), uniform());
            let r = (-2.0 * u1.ln()).sqrt();
            out[i] = (r * (2.0 * std::f64::consts::PI * u2).cos()) as f32;
            out[i + 1] = (r * (2.0 * std::f64::consts::PI * u2).sin()) as f32;
            i += 2;
        }
        out
    }

    #[test]
    fn test_starlet_perfect_reconstruction() {
        let (w, h) = (64, 64);
        let data: Vec<f32> = (0..w * h)
            .map(|i| 100.0 + ((i % w) as f32).sin() * 20.0 + ((i / w) as f32) * 0.5)
            .collect();
        let (details, coarse) = starlet_decompose(&data, w, h, 4);
        for p in 0..w * h {
            let sum: f32 = details.iter().map(|d| d[p]).sum::<f32>() + coarse[p];
            assert!(
                (sum - data[p]).abs() <= 1e-3 * data[p].abs().max(1.0),
                "reconstrucción px {p}: {sum} vs {}",
                data[p]
            );
        }
    }

    #[test]
    fn test_norm_b3_constants() {
        let norms = norm_b3(MAX_LEVELS);
        // Nivel 0 ≈ 0.889 (paridad con ds_mrs_noise/PixInsight, ±5%).
        assert!(
            (norms[0] - 0.889).abs() < 0.045,
            "norma nivel 0 = {}",
            norms[0]
        );
        // Decrecientes con la escala.
        for j in 1..norms.len() {
            assert!(norms[j] < norms[j - 1]);
        }
    }

    /// Gate F7: campo vacío — fracción de detalles aceptados << q. La doble
    /// FDR + signo + z_min hace la tasa real órdenes de magnitud menor que q;
    /// margen 3q por la correlación espacial del starlet.
    #[test]
    fn gate_f7_empty_field_false_positives() {
        let (w, h) = (128, 128);
        let npx = w * h;
        // Mitades: ruido independiente σ=1 sobre fondo 0; full = media.
        let na = lcg_normal(1111, npx);
        let nb = lcg_normal(2222, npx);
        let full: Vec<f32> = na.iter().zip(&nb).map(|(a, b)| (a + b) * 0.5).collect();
        let out = build_struct(&full, &na, &nb, w, h, 1.0, 1.0, 0.01, 2.5);
        let accepted: usize = out.accepted_per_level.iter().map(|&(a, _)| a).sum();
        let total: usize = out.accepted_per_level.iter().map(|&(_, t)| t).sum();
        let rate = accepted as f64 / total.max(1) as f64;
        assert!(
            rate < 3.0 * 0.01,
            "falsos positivos {rate:.4} >= 3q en campo vacío"
        );
    }

    /// Gate F7: un filamento presente en AMBAS mitades se acepta; un blob
    /// presente SOLO en la mitad A se rechaza; el residual no contiene el
    /// filamento.
    #[test]
    fn gate_f7_filament_recovered_and_ab_only_rejected() {
        let (w, h) = (128, 128);
        let npx = w * h;
        let sigma = 1.0f32;
        let mut na = lcg_normal(3333, npx);
        let mut nb = lcg_normal(4444, npx);
        // Filamento horizontal (fila 64, x 20..108) con perfil gaussiano
        // vertical FWHM 6 px y amplitud 8σ — en AMBAS mitades.
        let mut skeleton = Vec::new();
        let s_v = 6.0f32 / 2.3548;
        for x in 20..108 {
            skeleton.push(64 * w + x);
            for y in 56..72 {
                let dy = y as f32 - 64.0;
                let v = 8.0 * sigma * (-0.5 * dy * dy / (s_v * s_v)).exp();
                na[y * w + x] += v;
                nb[y * w + x] += v;
            }
        }
        // Blob 2D FWHM 6 px, 8σ — SOLO en la mitad A.
        let mut blob_px = Vec::new();
        for y in 90..106 {
            for x in 90..106 {
                let (dx, dy) = (x as f32 - 98.0, y as f32 - 98.0);
                let v = 8.0 * sigma * (-0.5 * (dx * dx + dy * dy) / (s_v * s_v)).exp();
                na[y * w + x] += v;
                if v > 2.0 * sigma {
                    blob_px.push(y * w + x);
                }
            }
        }
        let full: Vec<f32> = na.iter().zip(&nb).map(|(a, b)| (a + b) * 0.5).collect();
        let out = build_struct(&full, &na, &nb, w, h, sigma, sigma, 0.01, 2.5);
        // Filamento: >70% del esqueleto con detalle aceptado.
        let detail_at = |p: usize| (out.struct_map[p] - full[p]).abs() < f32::INFINITY; // placeholder
        let _ = detail_at;
        let coarse_free: Vec<f32> = {
            // detalle aceptado = struct_map − coarse; coarse = full − Σd_full.
            let (d_full, coarse) = starlet_decompose(&full, w, h, out.levels);
            let _ = d_full;
            out.struct_map
                .iter()
                .zip(&coarse)
                .map(|(s, c)| s - c)
                .collect()
        };
        let fil_hits = skeleton
            .iter()
            .filter(|&&p| coarse_free[p].abs() > 0.5 * sigma)
            .count();
        let fil_rate = fil_hits as f64 / skeleton.len() as f64;
        assert!(fil_rate > 0.7, "filamento recuperado solo {fil_rate:.2}");
        // Blob solo-A: <10% aceptado.
        let blob_hits = blob_px
            .iter()
            .filter(|&&p| coarse_free[p].abs() > 0.5 * sigma)
            .count();
        let blob_rate = blob_hits as f64 / blob_px.len().max(1) as f64;
        assert!(blob_rate < 0.10, "blob solo-A aceptado {blob_rate:.2}");
        // Residual sin el filamento: |media sobre esqueleto| < 1σ global.
        let res_mean: f64 = skeleton
            .iter()
            .map(|&p| out.residual[p] as f64)
            .sum::<f64>()
            / skeleton.len() as f64;
        let res_global_sd = {
            let m = out.residual.iter().map(|&v| v as f64).sum::<f64>() / npx as f64;
            (out.residual
                .iter()
                .map(|&v| (v as f64 - m).powi(2))
                .sum::<f64>()
                / npx as f64)
                .sqrt()
        };
        assert!(
            res_mean.abs() < res_global_sd,
            "el residual retiene el filamento: media {res_mean:.2} vs σ {res_global_sd:.2}"
        );
    }
}
