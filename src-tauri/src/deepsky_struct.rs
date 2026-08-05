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
/// Filas de salida por tile. El halo vertical es 2·step a cada lado; a nivel
/// máximo (step=64) el scratch ocupa 512 filas, no una imagen completa.
const STRUCT_TILE_ROWS: usize = 256;
/// Pico de la ruta productiva level-by-level, expresado como planos f32:
/// STRUCT + current/smooth para SCI/A/B (7) + un vector BH de |z| f32 (1). El scratch
/// tiled se suma aparte. Antes eran 31 planos equivalentes por conservar tres
/// pirámides completas y cuatro vectores BH simultáneos.
const BUILD_PEAK_F32_PLANES: u64 = 8;
/// STRUCT no puede apropiarse de más de 4 GiB aunque el host tenga mucha RAM:
/// NebulaFusion mantiene simultáneamente SCI/VAR/pesos y buffers de warp.
const STRUCT_HARD_BUDGET_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Si el SO no informa memoria disponible, usar un límite conservador pero no
/// cero. Cero convertía incluso un fixture de 0.1 MiB en un falso fallback.
const STRUCT_UNKNOWN_MEMORY_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

#[cfg(test)]
thread_local! {
    static TEST_MEMORY_BUDGET: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) struct TestMemoryBudgetGuard(Option<u64>);

#[cfg(test)]
impl Drop for TestMemoryBudgetGuard {
    fn drop(&mut self) {
        TEST_MEMORY_BUDGET.with(|budget| budget.set(self.0));
    }
}

#[cfg(test)]
pub(crate) fn override_test_memory_budget(bytes: u64) -> TestMemoryBudgetGuard {
    let previous = TEST_MEMORY_BUDGET.with(|budget| budget.replace(Some(bytes)));
    TestMemoryBudgetGuard(previous)
}

fn allocation_error(label: &str, error: std::collections::TryReserveError) -> String {
    format!("STRUCT omitido: no se pudo reservar {label}: {error}")
}

fn try_with_capacity<T>(len: usize, label: &str) -> Result<Vec<T>, String> {
    let mut out = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|error| allocation_error(label, error))?;
    Ok(out)
}

fn try_zeroed_f32(len: usize, label: &str) -> Result<Vec<f32>, String> {
    let mut out = try_with_capacity(len, label)?;
    out.resize(len, 0.0);
    Ok(out)
}

fn try_clone_f32(src: &[f32], label: &str) -> Result<Vec<f32>, String> {
    let mut out = try_with_capacity(src.len(), label)?;
    out.extend_from_slice(src);
    Ok(out)
}

fn checked_pixels(w: usize, h: usize) -> Result<usize, String> {
    w.checked_mul(h)
        .filter(|&npx| npx > 0)
        .ok_or_else(|| "STRUCT omitido: geometría vacía o fuera de rango".to_string())
}

fn current_memory_budget() -> u64 {
    #[cfg(test)]
    if let Some(budget) = TEST_MEMORY_BUDGET.with(|value| value.get()) {
        return budget;
    }

    let mut sys = sysinfo::System::new_all();
    sys.refresh_memory();
    let available = sys.available_memory();
    if available == 0 {
        STRUCT_UNKNOWN_MEMORY_BUDGET_BYTES
    } else {
        (available.saturating_mul(35) / 100).min(STRUCT_HARD_BUDGET_BYTES)
    }
}

fn checked_tile_scratch_bytes(w: usize, h: usize) -> Result<u64, String> {
    let levels = recommended_levels(w, h);
    let max_step = 1usize
        .checked_shl(levels.saturating_sub(1) as u32)
        .ok_or_else(|| "STRUCT omitido: nivel starlet fuera de rango".to_string())?;
    let halo_rows = max_step
        .checked_mul(4)
        .ok_or_else(|| "STRUCT omitido: halo starlet fuera de rango".to_string())?;
    let scratch_rows = h
        .min(STRUCT_TILE_ROWS)
        .checked_add(halo_rows)
        .ok_or_else(|| "STRUCT omitido: halo starlet fuera de rango".to_string())?;
    let scratch_rows = u64::try_from(scratch_rows)
        .map_err(|_| "STRUCT omitido: scratch starlet no representable".to_string())?;
    u64::try_from(w)
        .ok()
        .and_then(|width| width.checked_mul(scratch_rows))
        .and_then(|samples| samples.checked_mul(std::mem::size_of::<f32>() as u64))
        .ok_or_else(|| "STRUCT omitido: estimación de scratch fuera de rango".to_string())
}

fn checked_build_bytes(w: usize, h: usize) -> Result<u64, String> {
    let npx = checked_pixels(w, h)?;
    let planes = u64::try_from(npx)
        .map_err(|_| "STRUCT omitido: geometría no representable".to_string())?
        .checked_mul(std::mem::size_of::<f32>() as u64)
        .and_then(|bytes| bytes.checked_mul(BUILD_PEAK_F32_PLANES))
        .ok_or_else(|| "STRUCT omitido: estimación de memoria fuera de rango".to_string())?;
    planes
        .checked_add(checked_tile_scratch_bytes(w, h)?)
        .ok_or_else(|| "STRUCT omitido: estimación de memoria fuera de rango".to_string())
}

/// Preflight de la etapa starlet aislada. Los tres planos de entrada son
/// prestados y no forman parte de esta cifra.
pub(crate) fn validate_build_memory_budget(w: usize, h: usize) -> Result<u64, String> {
    let required = checked_build_bytes(w, h)?;
    let budget = current_memory_budget();
    if required > budget {
        return Err(format!(
            "STRUCT omitido por presupuesto de memoria: requiere ~{:.1} MiB, presupuesto {:.1} MiB",
            required as f64 / (1024.0 * 1024.0),
            budget as f64 / (1024.0 * 1024.0)
        ));
    }
    Ok(required)
}

/// Preflight del modo FullWithStruct completo. Incluye las cuatro sumas/pesos
/// split-half f64 y los tres lumas que permanecen vivos durante el starlet.
pub(crate) fn validate_pipeline_memory_budget(
    w: usize,
    h: usize,
    ch: usize,
) -> Result<u64, String> {
    let npx = checked_pixels(w, h)?;
    if ch == 0 {
        return Err("STRUCT omitido: número de canales cero".into());
    }
    let npx64 =
        u64::try_from(npx).map_err(|_| "STRUCT omitido: geometría no representable".to_string())?;
    let split_lumas = npx64
        .checked_mul(2)
        .and_then(|v| v.checked_mul(std::mem::size_of::<f32>() as u64))
        .ok_or_else(|| "STRUCT omitido: estimación de lumas fuera de rango".to_string())?;
    let split_peak = npx64
        .checked_mul(ch as u64)
        .and_then(|v| v.checked_mul(4 * std::mem::size_of::<f64>() as u64))
        .and_then(|v| v.checked_add(split_lumas))
        .ok_or_else(|| "STRUCT omitido: estimación split-half fuera de rango".to_string())?;
    let build_inputs = npx64
        .checked_mul(3)
        .and_then(|v| v.checked_mul(std::mem::size_of::<f32>() as u64))
        .ok_or_else(|| "STRUCT omitido: estimación de entradas fuera de rango".to_string())?;
    let build_peak = checked_build_bytes(w, h)?
        .checked_add(build_inputs)
        .ok_or_else(|| "STRUCT omitido: estimación starlet fuera de rango".to_string())?;
    let required = split_peak.max(build_peak);
    let budget = current_memory_budget();
    if required > budget {
        return Err(format!(
            "STRUCT omitido por presupuesto de memoria: requiere ~{:.1} MiB, presupuesto {:.1} MiB",
            required as f64 / (1024.0 * 1024.0),
            budget as f64 / (1024.0 * 1024.0)
        ));
    }
    Ok(required)
}

#[derive(Debug)]
pub(crate) struct StructOutput {
    /// coarse + detalles ACEPTADOS (mismas unidades que la entrada).
    pub struct_map: Vec<f32>,
    /// entrada − struct_map (detalles rechazados).
    pub residual: Vec<f32>,
    /// Por nivel: (aceptados, total).
    pub accepted_per_level: Vec<(usize, usize)>,
    pub levels: usize,
}

#[inline]
fn mirror_index(index: isize, len: usize) -> usize {
    if len <= 1 {
        return 0;
    }
    let period = 2 * (len as isize - 1);
    let folded = index.rem_euclid(period);
    if folded < len as isize {
        folded as usize
    } else {
        (period - folded) as usize
    }
}

/// Convolución à trous separable con tiles de filas y halo `2·step`.
/// Sólo el plano vertical de salida es completo; el intermedio horizontal es
/// un scratch acotado a `(tile_rows + 4·step) × width` y se reutiliza por tile.
fn atrous_smooth_tiled(
    src: &[f32],
    w: usize,
    h: usize,
    step: usize,
    tile_rows: usize,
) -> Result<Vec<f32>, String> {
    let npx = checked_pixels(w, h)?;
    if src.len() != npx || step == 0 || tile_rows == 0 {
        return Err(format!(
            "STRUCT omitido: plano/step/tile starlet inválido (len={}, geometría {w}×{h}, step={step}, tile={tile_rows})",
            src.len(),
        ));
    }
    let mut out = try_zeroed_f32(npx, "buffer vertical starlet")?;
    let rows_per_tile = tile_rows.min(h);
    let halo_rows = step
        .checked_mul(4)
        .ok_or_else(|| "STRUCT omitido: halo starlet fuera de rango".to_string())?;
    let max_scratch_rows = rows_per_tile
        .checked_add(halo_rows)
        .ok_or_else(|| "STRUCT omitido: halo starlet fuera de rango".to_string())?;
    let scratch_len = max_scratch_rows
        .checked_mul(w)
        .ok_or_else(|| "STRUCT omitido: scratch starlet fuera de rango".to_string())?;
    let mut horizontal = try_zeroed_f32(scratch_len, "tile+halo horizontal starlet")?;

    for y0 in (0..h).step_by(rows_per_tile) {
        let y1 = (y0 + rows_per_tile).min(h);
        let output_rows = y1 - y0;
        let scratch_rows = output_rows
            .checked_add(halo_rows)
            .ok_or_else(|| "STRUCT omitido: halo starlet fuera de rango".to_string())?;
        let logical_y0 = y0 as isize - 2 * step as isize;

        // Horizontal sobre tile+halo. Las filas reflejadas se recalculan sólo
        // en los bordes/tile-overlap; no existe un intermedio de imagen completa.
        for local_y in 0..scratch_rows {
            let source_y = mirror_index(logical_y0 + local_y as isize, h);
            let source_row = source_y * w;
            let scratch_row = local_y * w;
            for x in 0..w {
                let mut acc = 0.0f32;
                for (tap, &kernel) in B3.iter().enumerate() {
                    let dx = (tap as isize - 2) * step as isize;
                    acc += kernel * src[source_row + mirror_index(x as isize + dx, w)];
                }
                horizontal[scratch_row + x] = acc;
            }
        }

        // Vertical sólo para las filas interiores del tile.
        for local_y in 0..output_rows {
            let output_y = y0 + local_y;
            let centre = local_y + 2 * step;
            for x in 0..w {
                let mut acc = 0.0f32;
                for (tap, &kernel) in B3.iter().enumerate() {
                    let scratch_y = (centre as isize + (tap as isize - 2) * step as isize) as usize;
                    acc += kernel * horizontal[scratch_y * w + x];
                }
                out[output_y * w + x] = acc;
            }
        }
    }
    Ok(out)
}

fn atrous_smooth(src: &[f32], w: usize, h: usize, step: usize) -> Result<Vec<f32>, String> {
    atrous_smooth_tiled(src, w, h, step, STRUCT_TILE_ROWS)
}

/// Descomposición starlet B3 à trous canónica: d_j = c_j − c_{j+1}.
/// Devuelve (detalles a resolución completa, coarse).
pub(crate) fn try_starlet_decompose(
    data: &[f32],
    w: usize,
    h: usize,
    levels: usize,
) -> Result<(Vec<Vec<f32>>, Vec<f32>), String> {
    let npx = checked_pixels(w, h)?;
    if data.len() != npx {
        return Err(format!(
            "STRUCT omitido: plano de longitud {} para geometría {w}×{h}",
            data.len()
        ));
    }
    let mut details = try_with_capacity(levels, "índice de niveles starlet")?;
    let mut current = try_clone_f32(data, "plano inicial starlet")?;
    for j in 0..levels {
        let step = 1usize
            .checked_shl(j as u32)
            .ok_or_else(|| "STRUCT omitido: nivel starlet fuera de rango".to_string())?;
        let smooth = atrous_smooth(&current, w, h, step)?;
        // Reutilizar `current` como detalle evita reservar un plano adicional
        // justo en el nivel de mayor pico (cuando ya viven j detalles).
        for p in 0..npx {
            current[p] -= smooth[p];
        }
        details.push(current);
        current = smooth;
    }
    Ok((details, current))
}

/// Wrapper conservado para consumidores de diagnóstico ya existentes. La
/// ruta productiva FullWithStruct usa `build_struct` level-by-level; este API
/// sólo materializa la pirámide cuando un consumidor la solicita expresamente.
pub(crate) fn starlet_decompose(
    data: &[f32],
    w: usize,
    h: usize,
    levels: usize,
) -> (Vec<Vec<f32>>, Vec<f32>) {
    try_starlet_decompose(data, w, h, levels).unwrap_or_else(|error| panic!("{error}"))
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
                (d.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / (d.len() as f64 - 1.0))
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
fn bh_fdr_pcut(pvals: &mut [f64], q: f64) -> f64 {
    if pvals.is_empty() {
        return 0.0;
    }
    pvals.sort_by(|a, b| a.total_cmp(b));
    let m = pvals.len() as f64;
    let mut cut = 0.0f64;
    for (i, &p) in pvals.iter().enumerate() {
        if p <= q * (i as f64 + 1.0) / m {
            cut = p;
        }
    }
    cut
}

/// Mismo gate BH-FDR, almacenando |z| f32 en vez de p-valores f64. Como
/// p(|z|) es estrictamente monótono, ordenar |z| descendente es equivalente a
/// ordenar p ascendente. Devuelve el menor |z| aceptado; infinito significa
/// que ningún z finito pasó. Esto evita recalcular erfc en la pasada de
/// aceptación y reduce el scratch BH de dos planos f32 equivalentes a uno.
fn bh_fdr_zcut(z_abs: &mut [f32], q: f64) -> f32 {
    if z_abs.is_empty() {
        return f32::INFINITY;
    }
    z_abs.sort_by(|a, b| b.total_cmp(a));
    let m = z_abs.len() as f64;
    let mut cut = f32::INFINITY;
    for (i, &z) in z_abs.iter().enumerate() {
        if z.is_finite() && p_two_sided(z) <= q * (i as f64 + 1.0) / m {
            cut = z;
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
) -> Result<StructOutput, String> {
    let npx = checked_pixels(w, h)?;
    for (name, plane) in [("SCI", full), ("mitad A", half_a), ("mitad B", half_b)] {
        if plane.len() != npx {
            return Err(format!(
                "STRUCT omitido: {name} tiene {} muestras; se esperaban {npx}",
                plane.len()
            ));
        }
    }
    validate_build_memory_budget(w, h)?;
    let levels = recommended_levels(w, h);
    let norms = norm_b3(levels);
    // Tres estados corrientes, no tres pirámides. En cada nivel sólo coexisten
    // current/smooth de SCI/A/B; al terminar el nivel, smooth pasa a ser el
    // current siguiente y los detalles dejan de existir.
    let mut current_full = try_clone_f32(full, "estado starlet SCI")?;
    let mut current_a = try_clone_f32(half_a, "estado starlet mitad A")?;
    let mut current_b = try_clone_f32(half_b, "estado starlet mitad B")?;
    let mut struct_map = try_zeroed_f32(npx, "mapa STRUCT")?;
    let mut accepted_per_level = try_with_capacity(levels, "telemetría STRUCT")?;
    let sa = sigma_a.max(1e-6);
    let sb = sigma_b.max(1e-6);
    for j in 0..levels {
        let step = 1usize
            .checked_shl(j as u32)
            .ok_or_else(|| "STRUCT omitido: nivel starlet fuera de rango".to_string())?;
        let smooth_full = atrous_smooth(&current_full, w, h, step)?;
        let smooth_a = atrous_smooth(&current_a, w, h, step)?;
        let smooth_b = atrous_smooth(&current_b, w, h, step)?;
        let nj = norms[j].max(1e-6);

        // Un único vector |z| f32 reutilizado. La monotonía p↔|z| conserva BH
        // exactamente y evita tanto cuatro vectores f64 como una segunda erfc.
        let mut z_values = try_with_capacity(npx, "z-scores STRUCT")?;
        for p in 0..npx {
            let detail_a = current_a[p] - smooth_a[p];
            let z = detail_a / (sa * nj);
            z_values.push(if z.is_finite() { z.abs() } else { 0.0 });
        }
        let cut_a = bh_fdr_zcut(&mut z_values, q as f64);
        z_values.clear();
        for p in 0..npx {
            let detail_b = current_b[p] - smooth_b[p];
            let z = detail_b / (sb * nj);
            z_values.push(if z.is_finite() { z.abs() } else { 0.0 });
        }
        let cut_b = bh_fdr_zcut(&mut z_values, q as f64);
        drop(z_values);

        let mut accepted = 0usize;
        for p in 0..npx {
            let detail_a = current_a[p] - smooth_a[p];
            let detail_b = current_b[p] - smooth_b[p];
            let za = detail_a / (sa * nj);
            let zb = detail_b / (sb * nj);
            let ok = za.is_finite()
                && zb.is_finite()
                && za.abs() >= cut_a
                && zb.abs() >= cut_b
                && (za.signum() == zb.signum())
                && za.abs().min(zb.abs()) >= z_min;
            if ok {
                struct_map[p] += current_full[p] - smooth_full[p];
                accepted += 1;
            }
        }
        accepted_per_level.push((accepted, npx));
        current_full = smooth_full;
        current_a = smooth_a;
        current_b = smooth_b;
    }
    // El coarse pasa sin umbral, como en el contrato original. Se suma al
    // acumulador de detalles aceptados una sola vez al cerrar la pirámide.
    for p in 0..npx {
        struct_map[p] += current_full[p];
    }
    let mut residual = try_with_capacity(npx, "residual STRUCT")?;
    residual.extend(full.iter().zip(&struct_map).map(|(f, s)| f - s));
    Ok(StructOutput {
        struct_map,
        residual,
        accepted_per_level,
        levels,
    })
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

    fn atrous_reference(src: &[f32], w: usize, h: usize, step: usize) -> Vec<f32> {
        let mut horizontal = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let mut acc = 0.0f32;
                for (tap, &kernel) in B3.iter().enumerate() {
                    let dx = (tap as isize - 2) * step as isize;
                    acc += kernel * src[y * w + mirror_index(x as isize + dx, w)];
                }
                horizontal[y * w + x] = acc;
            }
        }
        let mut out = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let mut acc = 0.0f32;
                for (tap, &kernel) in B3.iter().enumerate() {
                    let dy = (tap as isize - 2) * step as isize;
                    acc += kernel * horizontal[mirror_index(y as isize + dy, h) * w + x];
                }
                out[y * w + x] = acc;
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn build_struct_pyramid_reference(
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
        let (d_full, coarse) = try_starlet_decompose(full, w, h, levels).unwrap();
        let (d_a, _) = try_starlet_decompose(half_a, w, h, levels).unwrap();
        let (d_b, _) = try_starlet_decompose(half_b, w, h, levels).unwrap();
        let mut struct_map = coarse;
        let mut accepted_per_level = Vec::with_capacity(levels);
        let sa = sigma_a.max(1e-6);
        let sb = sigma_b.max(1e-6);
        for j in 0..levels {
            let nj = norms[j].max(1e-6);
            let mut p_a: Vec<f64> = d_a[j]
                .iter()
                .map(|&detail| p_two_sided(detail / (sa * nj)))
                .collect();
            let mut p_b: Vec<f64> = d_b[j]
                .iter()
                .map(|&detail| p_two_sided(detail / (sb * nj)))
                .collect();
            let cut_a = bh_fdr_pcut(&mut p_a, q as f64);
            let cut_b = bh_fdr_pcut(&mut p_b, q as f64);
            let mut accepted = 0usize;
            for p in 0..npx {
                let za = d_a[j][p] / (sa * nj);
                let zb = d_b[j][p] / (sb * nj);
                if p_two_sided(za) <= cut_a
                    && p_two_sided(zb) <= cut_b
                    && za.signum() == zb.signum()
                    && za.abs().min(zb.abs()) >= z_min
                {
                    struct_map[p] += d_full[j][p];
                    accepted += 1;
                }
            }
            accepted_per_level.push((accepted, npx));
        }
        let residual = full
            .iter()
            .zip(&struct_map)
            .map(|(science, structure)| science - structure)
            .collect();
        StructOutput {
            struct_map,
            residual,
            accepted_per_level,
            levels,
        }
    }

    #[test]
    fn tiled_atrous_matches_full_plane_reference_at_edges_and_tile_seams() {
        let (w, h) = (37usize, 29usize);
        let data: Vec<f32> = (0..w * h)
            .map(|index| {
                let x = (index % w) as f32;
                let y = (index / w) as f32;
                100.0 + 3.0 * (0.31 * x).sin() - 2.0 * (0.23 * y).cos()
            })
            .collect();
        for step in [1usize, 2, 4, 8] {
            let tiled = atrous_smooth_tiled(&data, w, h, step, 5).unwrap();
            let reference = atrous_reference(&data, w, h, step);
            assert_eq!(tiled.len(), reference.len());
            for (index, (&actual, &expected)) in tiled.iter().zip(&reference).enumerate() {
                assert_eq!(
                    actual.to_bits(),
                    expected.to_bits(),
                    "step={step}, pixel={index}, actual={actual}, expected={expected}"
                );
            }
        }
    }

    #[test]
    fn z_threshold_bh_is_equivalent_to_sorted_p_values() {
        let z = [0.0f32, 0.5, 1.0, 1.7, 2.1, 2.8, 3.5, 4.2, 5.0, 5.0];
        for q in [0.001f64, 0.01, 0.05, 0.2] {
            let mut p_values: Vec<f64> = z.iter().map(|&value| p_two_sided(value)).collect();
            let p_cut = bh_fdr_pcut(&mut p_values, q);
            let mut z_values = z.map(f32::abs);
            let z_cut = bh_fdr_zcut(&mut z_values, q);
            for &value in &z {
                assert_eq!(
                    p_two_sided(value) <= p_cut,
                    value.abs() >= z_cut,
                    "q={q}, z={value}, p_cut={p_cut}, z_cut={z_cut}"
                );
            }
        }
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
    fn level_streaming_struct_matches_legacy_pyramid_contract() {
        let (w, h) = (64usize, 48usize);
        let npx = w * h;
        let mut half_a = lcg_normal(0xA11CE, npx);
        let mut half_b = lcg_normal(0xB0B, npx);
        for y in 14..34 {
            for x in 10..54 {
                let dy = y as f32 - 24.0;
                let signal = 7.0 * (-0.5 * dy * dy / 5.0).exp();
                half_a[y * w + x] += signal;
                half_b[y * w + x] += signal;
            }
        }
        let full: Vec<f32> = half_a
            .iter()
            .zip(&half_b)
            .map(|(a, b)| 0.5 * (a + b))
            .collect();
        let expected =
            build_struct_pyramid_reference(&full, &half_a, &half_b, w, h, 1.0, 1.0, 0.01, 2.5);
        let actual = build_struct(&full, &half_a, &half_b, w, h, 1.0, 1.0, 0.01, 2.5).unwrap();
        assert_eq!(actual.levels, expected.levels);
        assert_eq!(actual.accepted_per_level, expected.accepted_per_level);
        for p in 0..npx {
            let tolerance = 2e-6 * expected.struct_map[p].abs().max(1.0);
            assert!(
                (actual.struct_map[p] - expected.struct_map[p]).abs() <= tolerance,
                "STRUCT cambió en pixel {p}: {} vs {}",
                actual.struct_map[p],
                expected.struct_map[p]
            );
            assert!(
                (actual.residual[p] - expected.residual[p]).abs() <= tolerance,
                "residual cambió en pixel {p}: {} vs {}",
                actual.residual[p],
                expected.residual[p]
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
        let out =
            build_struct(&full, &na, &nb, w, h, 1.0, 1.0, 0.01, 2.5).expect("STRUCT campo vacío");
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
        let out =
            build_struct(&full, &na, &nb, w, h, sigma, sigma, 0.01, 2.5).expect("STRUCT filamento");
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

    #[test]
    fn struct_budget_rejects_before_large_allocations() {
        let _budget = override_test_memory_budget(64 * 1024 * 1024);
        let error = validate_pipeline_memory_budget(6_248, 4_176, 3).unwrap_err();
        assert!(error.contains("presupuesto de memoria"));
        assert!(error.contains("requiere"));
    }

    #[test]
    fn streaming_peak_estimate_reduces_sixty_mp_struct_from_31_to_eight_planes() {
        let (w, h) = (10_000usize, 6_000usize); // 60 MP
        let npx = (w * h) as u64;
        let old_pyramid_bytes = npx * std::mem::size_of::<f32>() as u64 * 31;
        let streaming_bytes = checked_build_bytes(w, h).unwrap();
        assert!(
            streaming_bytes * 100 <= old_pyramid_bytes * 31,
            "pico nuevo {:.1} MiB, anterior {:.1} MiB",
            streaming_bytes as f64 / 1_048_576.0,
            old_pyramid_bytes as f64 / 1_048_576.0
        );
        assert!(streaming_bytes < 2_200 * 1_048_576);
        {
            let _budget = override_test_memory_budget(2_200 * 1_048_576);
            assert_eq!(validate_build_memory_budget(w, h).unwrap(), streaming_bytes);
        }
        // Limitación cuantificada: las sumas split-half RGB de NebulaFusion
        // siguen dominando el preflight completo aunque el build STRUCT ya quepa.
        {
            let _budget = override_test_memory_budget(4 * 1024 * 1024 * 1024);
            assert!(validate_pipeline_memory_budget(w, h, 3).is_err());
        }
    }

    #[test]
    fn build_struct_rejects_invalid_geometry_without_allocating() {
        let error =
            build_struct(&[1.0; 3], &[1.0; 3], &[1.0; 3], 2, 2, 1.0, 1.0, 0.01, 2.5).unwrap_err();
        assert!(error.contains("tiene 3 muestras"));
    }
}
