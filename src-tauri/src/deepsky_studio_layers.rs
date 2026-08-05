//! Restauración y capas estelares nativas del Deep Sky Studio.
//!
//! Este módulo no reutiliza el postprocesado planetario: trabaja siempre en
//! float32, conserva valores firmados/NaN y no publica una VAR inventada para
//! salidas modeladas. La separación es aditiva y determinista:
//! `base = object + stars + residual`. El `DeepSkyResult` conserva la revisión
//! que alimentó la separación, por lo que el workspace no duplica otro frame
//! float32 completo sólo para devolver el combinado inicial.

#![allow(dead_code)]

use rayon::prelude::*;

use crate::{PostStackLayerTarget, PostStackPsfModel};

#[derive(Clone, Debug)]
pub(crate) struct StarLayerWorkspace {
    pub object: Vec<f32>,
    pub stars: Vec<f32>,
    pub residual: Vec<f32>,
    pub mask: Vec<f32>,
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub psf: PostStackPsfModel,
    pub star_fraction: f32,
    pub reconstruction_error: f32,
    pub object_modified: bool,
    pub stars_modified: bool,
    pub object_nonlinear: bool,
    pub stars_nonlinear: bool,
    pub recombined: bool,
    pub weights: (f32, f32, f32),
}

impl StarLayerWorkspace {
    pub(crate) fn target_mut(
        &mut self,
        target: PostStackLayerTarget,
    ) -> Result<&mut Vec<f32>, String> {
        match target {
            PostStackLayerTarget::Object => {
                self.object_modified = true;
                Ok(&mut self.object)
            }
            PostStackLayerTarget::Stars => {
                self.stars_modified = true;
                Ok(&mut self.stars)
            }
            PostStackLayerTarget::Combined => {
                Err("Selecciona Objeto o Estrellas para editar una rama separada".into())
            }
        }
    }

    pub(crate) fn compose(&self) -> Vec<f32> {
        let (object_weight, star_weight, residual_weight) = self.weights;
        self.object
            .par_iter()
            .zip(self.stars.par_iter())
            .zip(self.residual.par_iter())
            .map(|((object, stars), residual)| {
                if object.is_finite() && stars.is_finite() && residual.is_finite() {
                    object * object_weight + stars * star_weight + residual * residual_weight
                } else if !object.is_finite() {
                    *object
                } else if !stars.is_finite() {
                    *stars
                } else {
                    *residual
                }
            })
            .collect()
    }

    pub(crate) fn exact_restore(&self) -> bool {
        !self.object_modified && !self.stars_modified && self.weights == (1.0, 1.0, 1.0)
    }
}

fn finite_luma(data: &[f32], channels: usize) -> Vec<f32> {
    let channels = channels.max(1);
    data.par_chunks(channels)
        .map(|pixel| {
            let value = if channels >= 3 {
                0.2126 * pixel[0] + 0.7152 * pixel[1] + 0.0722 * pixel[2]
            } else {
                pixel[0]
            };
            if value.is_finite() {
                value
            } else {
                0.0
            }
        })
        .collect()
}

fn sampled_median_noise(data: &[f32]) -> (f32, f32, f32) {
    let step = (data.len() / 400_000).max(1);
    let mut sample = data
        .iter()
        .step_by(step)
        .copied()
        .filter(|value| value.is_finite())
        .collect::<Vec<_>>();
    if sample.is_empty() {
        return (0.0, 1.0, 1.0);
    }
    sample.sort_by(|a, b| a.total_cmp(b));
    let median = sample[sample.len() / 2];
    let mut deviations = sample
        .iter()
        .map(|value| (value - median).abs())
        .collect::<Vec<_>>();
    deviations.sort_by(|a, b| a.total_cmp(b));
    let noise = (deviations[deviations.len() / 2] * 1.4826).max(1e-6);
    let white =
        sample[((sample.len() - 1) as f32 * 0.9995).round() as usize].max(median + noise * 8.0);
    (median, noise, white)
}

pub(crate) fn estimate_poststack_psf(
    data: &[f32],
    width: usize,
    height: usize,
    channels: usize,
) -> PostStackPsfModel {
    if width < 17
        || height < 17
        || channels == 0
        || data.len() != width.saturating_mul(height).saturating_mul(channels)
    {
        return PostStackPsfModel::default();
    }
    let luma = finite_luma(data, channels);
    let stars = crate::ds_detect_stars(&luma, width, height, 160);
    if let Some((field, report)) =
        crate::deepsky_psf::fit_frame_psf(&luma, width, height, &stars, 0.2)
    {
        let psf = field.base;
        let census = report.stars_used as f32;
        let confidence =
            (census / 40.0).clamp(0.0, 1.0) * (1.0 - report.holdout_fwhm_bias.abs().min(0.75));
        return PostStackPsfModel {
            fwhm_x: psf.fwhm_x,
            fwhm_y: psf.fwhm_y,
            theta: psf.theta,
            beta: psf.beta,
            stars_used: report.stars_used,
            confidence,
            measured: report.stars_used >= 6,
        };
    }
    let fwhm = crate::ds_frame_fwhm_proxy(&luma, width, height, &stars);
    if fwhm.is_finite() && fwhm > 0.5 {
        PostStackPsfModel {
            fwhm_x: fwhm,
            fwhm_y: fwhm,
            stars_used: stars.len(),
            confidence: (stars.len() as f32 / 30.0).clamp(0.0, 0.72),
            measured: stars.len() >= 6,
            ..PostStackPsfModel::default()
        }
    } else {
        PostStackPsfModel::default()
    }
}

/// Separador PSF/multiescala de reserva completamente local. No se presenta
/// como red neuronal: detecta estructura compacta compatible con la PSF,
/// conserva señal extendida y construye un residual firmado sin clipping.
pub(crate) fn separate_stars_native(
    data: &[f32],
    width: usize,
    height: usize,
    channels: usize,
    sensitivity: f32,
    scale: f32,
    halo_protection: f32,
    faint_star_protection: f32,
    psf_override: Option<PostStackPsfModel>,
) -> Result<StarLayerWorkspace, String> {
    let expected = width.saturating_mul(height).saturating_mul(channels);
    if channels == 0 || data.len() != expected || width < 8 || height < 8 {
        return Err("La separación estelar no coincide con la geometría activa".into());
    }
    let psf = psf_override.unwrap_or_else(|| estimate_poststack_psf(data, width, height, channels));
    let fwhm = (0.5 * (psf.fwhm_x + psf.fwhm_y)).clamp(1.2, 12.0) * scale.clamp(0.65, 1.8);
    let sigma = (fwhm / 2.354_820_1).clamp(0.55, 5.5);
    let luma = finite_luma(data, channels);
    let (background, noise, white) = sampled_median_noise(&luma);
    let core = crate::apply_gaussian_blur(&luma, width, height, (sigma * 0.55).max(0.45));
    let support = crate::apply_gaussian_blur(
        &luma,
        width,
        height,
        sigma * (1.8 + halo_protection.clamp(0.0, 1.0) * 1.4),
    );
    let sensitivity = sensitivity.clamp(0.0, 1.0);
    let threshold = noise * (7.5 - 5.2 * sensitivity);
    let faint_guard = faint_star_protection.clamp(0.0, 1.0);
    let mut mask = vec![0.0f32; width * height];
    mask.par_iter_mut().enumerate().for_each(|(index, value)| {
        let compact = (core[index] - support[index]).max(0.0);
        let signal = (luma[index] - background).max(0.0);
        let significance = ((compact - threshold) / (noise * 2.0).max(1e-6)).clamp(-8.0, 8.0);
        let logistic = 1.0 / (1.0 + (-significance).exp());
        let compactness = (compact / (signal + noise * 2.0)).clamp(0.0, 1.0);
        let saturation_guard = 1.0
            - ((luma[index] - white * 0.72) / (white * 0.28).max(1e-6)).clamp(0.0, 1.0)
                * halo_protection.clamp(0.0, 1.0)
                * 0.25;
        *value = (logistic * compactness.powf(0.45 + faint_guard * 0.45) * saturation_guard)
            .clamp(0.0, 1.0);
    });
    // Expansión PSF suave: incluye alas sin convertir nebulosa extensa en
    // estrellas. La máscara dilatada sigue limitada por señal positiva local.
    let expanded = crate::apply_gaussian_blur(&mask, width, height, sigma.max(0.7));
    mask.par_iter_mut().enumerate().for_each(|(index, value)| {
        let signal_gate =
            ((luma[index] - background - noise) / (noise * 8.0).max(1e-6)).clamp(0.0, 1.0);
        *value = value
            .max(expanded[index] * (0.48 + 0.42 * halo_protection.clamp(0.0, 1.0)))
            .min(signal_gate.max(*value))
            .clamp(0.0, 1.0);
    });

    let mut stars = vec![0.0f32; expected];
    for channel in 0..channels {
        let plane = data
            .iter()
            .skip(channel)
            .step_by(channels)
            .map(|value| {
                if value.is_finite() {
                    *value
                } else {
                    background
                }
            })
            .collect::<Vec<_>>();
        let broad = crate::apply_gaussian_blur(
            &plane,
            width,
            height,
            sigma * (1.9 + halo_protection.clamp(0.0, 1.0)),
        );
        stars
            .par_chunks_mut(channels)
            .enumerate()
            .for_each(|(pixel, output)| {
                let source = data[pixel * channels + channel];
                output[channel] = if source.is_finite() {
                    (source - broad[pixel]).max(0.0) * mask[pixel]
                } else {
                    0.0
                };
            });
    }
    let object = data
        .par_iter()
        .zip(stars.par_iter())
        .map(|(source, star)| {
            if source.is_finite() {
                source - star
            } else {
                *source
            }
        })
        .collect::<Vec<_>>();
    // Conserva también el residuo de redondeo de la descomposición float32.
    // De este modo las ramas siguen siendo un contrato aditivo verificable
    // incluso cuando una estrella brillante está varios órdenes de magnitud
    // por encima del fondo firmado.
    let residual = data
        .par_iter()
        .zip(object.par_iter())
        .zip(stars.par_iter())
        .map(|((source, object), star)| {
            if source.is_finite() {
                *source - (*object + *star)
            } else {
                0.0
            }
        })
        .collect::<Vec<_>>();
    let stellar_flux = stars
        .par_iter()
        .filter(|value| value.is_finite())
        .map(|value| value.abs() as f64)
        .sum::<f64>();
    let total_flux = data
        .par_iter()
        .filter(|value| value.is_finite())
        .map(|value| value.abs() as f64)
        .sum::<f64>()
        .max(1e-12);
    let reconstruction_error = data
        .par_iter()
        .zip(object.par_iter())
        .zip(stars.par_iter())
        .zip(residual.par_iter())
        .filter_map(|(((source, object), star), residual)| {
            source
                .is_finite()
                .then_some((source - (object + star + residual)).abs() / source.abs().max(1.0))
        })
        .reduce(|| 0.0f32, f32::max);
    Ok(StarLayerWorkspace {
        object,
        stars,
        residual,
        mask,
        width,
        height,
        channels,
        psf,
        star_fraction: (stellar_flux / total_flux).clamp(0.0, 1.0) as f32,
        reconstruction_error,
        object_modified: false,
        stars_modified: false,
        object_nonlinear: false,
        stars_nonlinear: false,
        recombined: false,
        weights: (1.0, 1.0, 1.0),
    })
}

/// Núcleo gaussiano 1-D normalizado (Σ=1) truncado a 3σ. La normalización
/// explícita tras el truncado garantiza que cada pasada separable conserva el
/// flujo total en el interior (los bordes usan replicación, igual que el resto
/// de blurs del pipeline).
fn gaussian_kernel_1d(sigma: f32) -> (Vec<f32>, usize) {
    let sigma = sigma.max(0.3);
    // 3σ cubre el 99.73% del flujo de una gaussiana; más allá el kernel
    // aporta menos que el error de redondeo float32 del acumulador.
    let radius = (sigma * 3.0).ceil().max(1.0) as usize;
    let denom = 2.0 * sigma * sigma;
    let mut kernel = Vec::with_capacity(2 * radius + 1);
    for tap in -(radius as isize)..=(radius as isize) {
        let distance2 = (tap * tap) as f32;
        kernel.push((-distance2 / denom).exp());
    }
    let sum: f32 = kernel.iter().sum();
    if sum > 0.0 {
        kernel.iter_mut().for_each(|weight| *weight /= sum);
    }
    (kernel, radius)
}

/// Blur gaussiano anisotrópico separable EXACTO alineado con los ejes de
/// imagen: cada eje convoluciona con su gaussiana 1-D real truncada a 3σ.
///
/// NO es el modelo directo de la RL (ver `anisotropic_box_gaussian_blur`):
/// una gaussiana ancha real tiene MTF ≈ e^(−σ²ω²/2), prácticamente cero en
/// alta frecuencia, y la RL amortiguada del módulo (límite aditivo por píxel,
/// máximo 32 iteraciones) converge por frecuencia a un ritmo ∝ MTF — con
/// σ≳2 el eje mayor no se recupera dentro del presupuesto de iteraciones.
/// Se conserva como kernel de síntesis/referencia: los tests generan campos
/// elongados con ESTA convolución exacta y los invierten con el modelo box,
/// evitando el "inverse crime" (generar e invertir con el mismo operador).
fn anisotropic_gaussian_blur(
    input: &[f32],
    width: usize,
    height: usize,
    sigma_x: f32,
    sigma_y: f32,
) -> Vec<f32> {
    let size = width * height;
    // GUARD homólogo a box_blur_parallel: un búfer corto devuelve negro
    // visible en lugar de un panic que tumbe el post-procesado entero.
    if input.len() < size || width == 0 || height == 0 {
        eprintln!(
            "[studio_layers] anisotropic_gaussian_blur: entrada {} < {}x{} — devolviendo búfer vacío",
            input.len(),
            width,
            height
        );
        return vec![0.0; size];
    }
    let (kernel_x, radius_x) = gaussian_kernel_1d(sigma_x);
    let (kernel_y, radius_y) = gaussian_kernel_1d(sigma_y);

    // Pasada horizontal (σ_x): paralela por filas, bordes por replicación.
    let mut horizontal = vec![0.0f32; size];
    horizontal
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row_out)| {
            let row_in = &input[y * width..(y + 1) * width];
            for x in 0..width {
                let mut acc = 0.0f32;
                for (tap, weight) in kernel_x.iter().enumerate() {
                    let sx = (x as isize + tap as isize - radius_x as isize)
                        .clamp(0, width as isize - 1) as usize;
                    acc += row_in[sx] * weight;
                }
                row_out[x] = acc;
            }
        });

    // Pasada vertical (σ_y): acumulamos fila fuente completa por tap para que
    // el acceso sea contiguo (vectorizable) en lugar de saltar por columnas.
    let mut output = vec![0.0f32; size];
    output
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row_out)| {
            for (tap, weight) in kernel_y.iter().enumerate() {
                let sy = (y as isize + tap as isize - radius_y as isize)
                    .clamp(0, height as isize - 1) as usize;
                let source_row = &horizontal[sy * width..(sy + 1) * width];
                if tap == 0 {
                    for x in 0..width {
                        row_out[x] = source_row[x] * weight;
                    }
                } else {
                    for x in 0..width {
                        row_out[x] += source_row[x] * weight;
                    }
                }
            }
        });
    output
}

/// Tamaños de caja canónicos (Kuckir) para aproximar una gaussiana 1-D con
/// n=3 pasadas: w_ideal = sqrt(12σ²/n + 1). Misma aritmética exacta que
/// `apply_gaussian_blur` en filters.rs (que es 2-D isotrópico y no se puede
/// parametrizar por eje) para que la ruta circular y la elíptica compartan
/// numérica por eje: con σx == σy ambas familias producen el mismo kernel.
fn kuckir_box_radii(sigma: f32) -> [usize; 3] {
    let n = 3.0f32;
    let sigma = sigma.max(0.3);
    let w_ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut wl = w_ideal.floor() as isize;
    if wl % 2 == 0 {
        wl -= 1; // fuerza impar: una caja par no tiene centro y desplaza fase
    }
    let wl = wl.max(1) as usize;
    let wu = wl + 2;
    let m_ideal =
        (12.0 * sigma * sigma - (n * (wl * wl) as f32) - (4.0 * n * wl as f32) - (3.0 * n))
            / (-4.0 * wl as f32 - 4.0);
    let m = m_ideal.round().clamp(0.0, n) as usize;
    let size = |pass: usize| if pass < m { wl } else { wu };
    [(size(0) - 1) / 2, (size(1) - 1) / 2, (size(2) - 1) / 2]
}

/// Una pasada de caja 1-D horizontal (suma corrida, bordes por replicación).
/// Numérica idéntica a la pasada horizontal de `box_blur_parallel`: mismo
/// clamp de índices y misma normalización 1/(2r+1).
fn box_pass_rows(data: &mut Vec<f32>, width: usize, radius: usize) {
    if radius == 0 || width == 0 {
        return;
    }
    let mut output = vec![0.0f32; data.len()];
    output
        .par_chunks_mut(width)
        .zip(data.par_chunks(width))
        .for_each(|(row_out, row_in)| {
            let iarr = 1.0 / (2.0 * radius as f32 + 1.0);
            let mut sum = 0.0f32;
            for tap in -(radius as isize)..=(radius as isize) {
                sum += row_in[tap.clamp(0, width as isize - 1) as usize];
            }
            row_out[0] = sum * iarr;
            for x in 1..width {
                let leaving = row_in
                    [(x as isize - 1 - radius as isize).clamp(0, width as isize - 1) as usize];
                let entering =
                    row_in[(x as isize + radius as isize).clamp(0, width as isize - 1) as usize];
                sum = sum - leaving + entering;
                row_out[x] = sum * iarr;
            }
        });
    *data = output;
}

/// Traspone una imagen w×h a h×w (paralela por columnas de salida).
fn transpose_plane(data: &[f32], width: usize, height: usize) -> Vec<f32> {
    let mut output = vec![0.0f32; data.len()];
    output
        .par_chunks_mut(height)
        .enumerate()
        .for_each(|(x, col_out)| {
            for y in 0..height {
                col_out[y] = data[y * width + x];
            }
        });
    output
}

/// Aproximación box de 3 pasadas de una gaussiana ANISOTRÓPICA separable:
/// cajas dimensionadas por eje (Kuckir) — σx gobierna las pasadas
/// horizontales y σy las verticales.
///
/// Es el modelo directo (y su adjunto: cajas simétricas ⇒ operador
/// autoadjunto) de la RL cuando la PSF medida es elíptica. Se usa la familia
/// box y no la gaussiana exacta por consistencia de convergencia con la ruta
/// circular (`apply_gaussian_blur`, misma familia): el soporte compacto de
/// las cajas conserva MTF útil en alta frecuencia y la RL amortiguada
/// converge en el mismo presupuesto de iteraciones en ambas rutas. La PSF
/// real es Moffat elíptica, de modo que ambas familias son aproximaciones
/// del mismo orden; lo que C6 exige preservar es la ANISOTROPÍA medida, y
/// aquí cada eje recibe exactamente su anchura.
fn anisotropic_box_gaussian_blur(
    input: &[f32],
    width: usize,
    height: usize,
    sigma_x: f32,
    sigma_y: f32,
) -> Vec<f32> {
    let size = width * height;
    // GUARD homólogo a box_blur_parallel: un búfer corto devuelve negro
    // visible en lugar de un panic que tumbe el post-procesado entero.
    if input.len() < size || width == 0 || height == 0 {
        eprintln!(
            "[studio_layers] anisotropic_box_gaussian_blur: entrada {} < {}x{} — devolviendo búfer vacío",
            input.len(),
            width,
            height
        );
        return vec![0.0; size];
    }
    let mut current = input[..size].to_vec();
    for radius in kuckir_box_radii(sigma_x) {
        box_pass_rows(&mut current, width, radius);
    }
    // Pasadas verticales como horizontales sobre la traspuesta: mismo truco
    // que box_blur_parallel para que la suma corrida recorra memoria contigua.
    let mut transposed = transpose_plane(&current, width, height);
    for radius in kuckir_box_radii(sigma_y) {
        box_pass_rows(&mut transposed, height, radius);
    }
    transpose_plane(&transposed, height, width)
}

/// Muestra bilineal con borde replicado. Para un desplazamiento constante,
/// la pareja `+offset/-offset` produce un kernel discreto simétrico; por ello
/// cada pasada direccional sigue siendo autoadjunta en el interior y puede
/// usarse tanto como modelo directo como adjunto de Richardson-Lucy.
#[inline]
fn bilinear_sample_replicated(input: &[f32], width: usize, height: usize, x: f32, y: f32) -> f32 {
    let x = x.clamp(0.0, width.saturating_sub(1) as f32);
    let y = y.clamp(0.0, height.saturating_sub(1) as f32);
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let tx = x - x0 as f32;
    let ty = y - y0 as f32;
    let top = input[y0 * width + x0] * (1.0 - tx) + input[y0 * width + x1] * tx;
    let bottom = input[y1 * width + x0] * (1.0 - tx) + input[y1 * width + x1] * tx;
    top * (1.0 - ty) + bottom * ty
}

/// Una pasada binomial `[1, 2, 1] / 4` a lo largo de un vector subpíxel.
/// La varianza de la pasada es `0.5 * |offset|²`. Tres pasadas con
/// `|offset| = sigma * sqrt(2/3)` aproximan una gaussiana con la varianza
/// solicitada, sin construir un kernel 2-D O(r²) ni perder la orientación.
fn directional_binomial_pass(
    input: &[f32],
    output: &mut [f32],
    width: usize,
    height: usize,
    offset_x: f32,
    offset_y: f32,
) {
    output
        .par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, value) in row.iter_mut().enumerate() {
                let x = x as f32;
                let y = y as f32;
                let before =
                    bilinear_sample_replicated(input, width, height, x - offset_x, y - offset_y);
                let after =
                    bilinear_sample_replicated(input, width, height, x + offset_x, y + offset_y);
                *value =
                    before * 0.25 + input[y as usize * width + x as usize] * 0.5 + after * 0.25;
            }
        });
}

/// Gaussiana elíptica ROTADA de coste O(N), aproximada mediante tres pasadas
/// binomiales sobre cada eje principal. A diferencia de proyectar la elipse a
/// X/Y, este operador conserva la covarianza completa:
///
/// `Σ = R(theta) · diag(sigma_x², sigma_y²) · R(theta)ᵀ`
///
/// incluido `Σxy = (sigma_x²-sigma_y²) sin(theta) cos(theta)`. El número de
/// pasadas es fijo (seis), de modo que una PSF ancha no convierte una revisión
/// de 60 MP en una convolución cuadrática por píxel.
fn rotated_binomial_gaussian_blur(
    input: &[f32],
    width: usize,
    height: usize,
    sigma_x: f32,
    sigma_y: f32,
    theta: f32,
) -> Vec<f32> {
    let size = width.saturating_mul(height);
    if input.len() < size || width == 0 || height == 0 {
        eprintln!(
            "[studio_layers] rotated_binomial_gaussian_blur: entrada {} < {}x{} — devolviendo búfer vacío",
            input.len(),
            width,
            height
        );
        return vec![0.0; size];
    }
    let (sin_t, cos_t) = theta.sin_cos();
    let pass_scale = (2.0f32 / 3.0).sqrt();
    let axes = [
        (sigma_x * pass_scale * cos_t, sigma_x * pass_scale * sin_t),
        (-sigma_y * pass_scale * sin_t, sigma_y * pass_scale * cos_t),
    ];
    let mut current = input[..size].to_vec();
    let mut scratch = vec![0.0f32; size];
    for (offset_x, offset_y) in axes {
        for _ in 0..3 {
            directional_binomial_pass(&current, &mut scratch, width, height, offset_x, offset_y);
            std::mem::swap(&mut current, &mut scratch);
        }
    }
    current
}

/// RL regularizada para cielo profundo. Usa la anchura PSF medida, pedestal
/// reversible para datos firmados, límite de actualización guiado por ruido y
/// renormalización de flujo. No clipea a 0..65535.
pub(crate) fn deconvolve_linear(
    data: &mut Vec<f32>,
    width: usize,
    height: usize,
    channels: usize,
    psf: &PostStackPsfModel,
    iterations: usize,
    regularization: f32,
    deringing: f32,
    flux_conservation: bool,
) -> Result<(), String> {
    if channels == 0 || data.len() != width.saturating_mul(height).saturating_mul(channels) {
        return Err("La restauración PSF no coincide con la geometría activa".into());
    }
    let iterations = iterations.clamp(1, 32);
    // C6: la PSF medida trae fwhm_x/fwhm_y/theta (Moffat elíptica ajustada por
    // deepsky_psf). Antes todo se colapsaba a UNA sigma circular promedio, así
    // que la elongación medida nunca se corregía y el desajuste del modelo
    // directo inyectaba ringing a lo largo del eje mayor. La ruta rotada usa
    // ahora la covarianza completa, incluido el término cruzado.
    let sigma_x_raw = (psf.fwhm_x.max(0.6) / 2.354_820_1).clamp(0.3, 6.0);
    let sigma_y_raw = (psf.fwhm_y.max(0.6) / 2.354_820_1).clamp(0.3, 6.0);
    // Elipticidad sobre las sigmas medidas (no las proyectadas): si la PSF es
    // casi circular conservamos la ruta box-blur actual — misma numérica que
    // siempre, sin cambiar resultados en el caso común (compatibilidad).
    let ellipticity = (sigma_x_raw - sigma_y_raw).abs() / sigma_x_raw.max(sigma_y_raw).max(1e-6);
    let (sin_t, cos_t) = psf.theta.sin_cos();
    let cross_covariance = (sigma_x_raw * sigma_x_raw - sigma_y_raw * sigma_y_raw) * sin_t * cos_t;
    let elliptical = ellipticity >= 0.05;
    let rotated = elliptical
        && cross_covariance.abs()
            > (sigma_x_raw * sigma_x_raw + sigma_y_raw * sigma_y_raw) * 1.0e-4;
    let sigma = ((psf.fwhm_x.max(0.6) + psf.fwhm_y.max(0.6)) * 0.5 / 2.354_820_1).clamp(0.3, 6.0);
    // Modelo directo de la RL. La ruta alineada usa cajas separables y la
    // rotada, binomiales direccionales subpíxel; ambas construyen kernels
    // centrados y simétricos, por lo que el mismo operador sirve como adjunto
    // en el paso de corrección.
    let psf_blur = |image: &[f32]| -> Vec<f32> {
        if rotated {
            rotated_binomial_gaussian_blur(
                image,
                width,
                height,
                sigma_x_raw,
                sigma_y_raw,
                psf.theta,
            )
        } else if elliptical {
            // theta=0/90°: el término cruzado es nulo y la ruta de cajas
            // separables es exacta respecto a la orientación y más rápida.
            if cos_t.abs() >= sin_t.abs() {
                anisotropic_box_gaussian_blur(image, width, height, sigma_x_raw, sigma_y_raw)
            } else {
                anisotropic_box_gaussian_blur(image, width, height, sigma_y_raw, sigma_x_raw)
            }
        } else {
            crate::apply_gaussian_blur(image, width, height, sigma)
        }
    };
    let regularization = regularization.clamp(0.0, 0.35);
    let deringing = deringing.clamp(0.0, 1.0);
    for channel in 0..channels {
        let original = data
            .iter()
            .skip(channel)
            .step_by(channels)
            .copied()
            .collect::<Vec<_>>();
        let finite = original
            .iter()
            .copied()
            .filter(|value| value.is_finite())
            .collect::<Vec<_>>();
        if finite.is_empty() {
            continue;
        }
        let (background, noise, white) = sampled_median_noise(&finite);
        let minimum = finite.iter().copied().reduce(f32::min).unwrap_or(0.0);
        let pedestal = (-minimum + noise * 4.0).max(noise * 2.0).max(1e-6);
        let input = original
            .iter()
            .map(|value| {
                if value.is_finite() {
                    (*value + pedestal).max(1e-8)
                } else {
                    (background + pedestal).max(1e-8)
                }
            })
            .collect::<Vec<_>>();
        let input_flux = original
            .iter()
            .filter(|value| value.is_finite())
            .map(|value| *value as f64)
            .sum::<f64>();
        let mut estimate = input.clone();
        for iteration in 0..iterations {
            let forward = psf_blur(&estimate);
            let ratio = input
                .par_iter()
                .zip(forward.par_iter())
                .map(|(observed, predicted)| (observed / predicted.max(1e-8)).clamp(0.15, 6.0))
                .collect::<Vec<_>>();
            let correction = psf_blur(&ratio);
            let previous = estimate.clone();
            estimate
                .par_iter_mut()
                .enumerate()
                .for_each(|(index, value)| {
                    let x = index % width;
                    let y = index / width;
                    let raw = previous[index] * correction[index];
                    let left = previous[y * width + x.saturating_sub(1)];
                    let right = previous[y * width + (x + 1).min(width - 1)];
                    let up = previous[y.saturating_sub(1) * width + x];
                    let down = previous[(y + 1).min(height - 1) * width + x];
                    let smooth = (left + right + up + down) * 0.25;
                    let regularized = raw * (1.0 - regularization) + smooth * regularization;
                    let source = input[index];
                    let local_limit =
                        noise * (5.0 - deringing * 2.5) + source.abs() * (0.28 - deringing * 0.12);
                    let original_value = if original[index].is_finite() {
                        original[index]
                    } else {
                        background
                    };
                    let highlight = ((original_value - white * 0.82) / (white * 0.18).max(1e-6))
                        .clamp(0.0, 1.0);
                    let limit = local_limit * (1.0 - highlight * deringing * 0.75);
                    *value = (previous[index]
                        + (regularized - previous[index]).clamp(-limit, limit))
                    .max(1e-8);
                });
            if iteration >= 3 {
                let change = estimate
                    .par_iter()
                    .zip(previous.par_iter())
                    .map(|(next, before)| ((next - before) / before.max(1e-6)).abs() as f64)
                    .sum::<f64>()
                    / estimate.len().max(1) as f64;
                if change < 2e-5 {
                    break;
                }
            }
        }
        for (pixel, value) in estimate.iter().enumerate() {
            if original[pixel].is_finite() {
                data[pixel * channels + channel] = *value - pedestal;
            } else {
                data[pixel * channels + channel] = original[pixel];
            }
        }
        if flux_conservation {
            let output_flux = data
                .iter()
                .skip(channel)
                .step_by(channels)
                .filter(|value| value.is_finite())
                .map(|value| *value as f64)
                .sum::<f64>();
            let finite_count = data
                .iter()
                .skip(channel)
                .step_by(channels)
                .filter(|value| value.is_finite())
                .count();
            if input_flux.is_finite() && output_flux.is_finite() && finite_count > 0 {
                // Devuelve cualquier pequeña deriva al soporte de señal que
                // la originó. Un offset DC uniforme puede levantar todo el
                // fondo cuando existe una estrella muy brillante, y una
                // escala global por canal puede cambiar el color.
                let delta = input_flux - output_flux;
                let support_flux = original
                    .iter()
                    .filter(|value| value.is_finite())
                    .map(|value| (*value - background).max(0.0) as f64)
                    .sum::<f64>();
                if delta.is_finite() && support_flux > 1e-12 {
                    data.par_chunks_mut(channels)
                        .enumerate()
                        .for_each(|(index, pixel)| {
                            if pixel[channel].is_finite() && original[index].is_finite() {
                                let support =
                                    (original[index] - background).max(0.0) as f64 / support_flux;
                                pixel[channel] += (delta * support) as f32;
                            }
                        });
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn adjust_star_layer(
    data: &mut Vec<f32>,
    width: usize,
    height: usize,
    channels: usize,
    reduction: f32,
    saturation: f32,
    halo_suppression: f32,
) {
    if channels == 0 || data.len() != width.saturating_mul(height).saturating_mul(channels) {
        return;
    }
    let reduction = reduction.clamp(0.0, 0.85);
    let saturation = saturation.clamp(-1.0, 1.5);
    let halo = halo_suppression.clamp(0.0, 1.0);
    let source = data.clone();
    let mut broad_channels = Vec::with_capacity(channels);
    for channel in 0..channels {
        // C7a: el box blur de suma corrida de filters.rs propaga UN no-finito
        // a toda su fila, luego a su columna y de ahí a casi toda la capa.
        // Mismo patrón que separate_stars_native: se blurrea una copia
        // saneada (no-finitos → 0) junto con una máscara de validez, y el
        // cociente blur(plano)/blur(máscara) es la convolución normalizada:
        // los píxeles inválidos no aportan flujo NI arrastran el promedio a
        // la baja — no se fabrica ningún dato. Los no-finitos originales se
        // restauran intactos en la salida (rama `!value.is_finite()` abajo).
        let mut has_invalid = false;
        let plane = source
            .iter()
            .skip(channel)
            .step_by(channels)
            .map(|value| {
                if value.is_finite() {
                    *value
                } else {
                    has_invalid = true;
                    0.0
                }
            })
            .collect::<Vec<_>>();
        let mut broad = crate::apply_gaussian_blur(&plane, width, height, 2.2);
        if has_invalid {
            let validity = source
                .iter()
                .skip(channel)
                .step_by(channels)
                .map(|value| if value.is_finite() { 1.0f32 } else { 0.0 })
                .collect::<Vec<_>>();
            let coverage = crate::apply_gaussian_blur(&validity, width, height, 2.2);
            broad
                .par_iter_mut()
                .zip(coverage.par_iter())
                .for_each(|(blurred, weight)| {
                    // Con cobertura ~0 (vecindario entero inválido) no hay
                    // información local: 0 es el halo neutro, y el píxel
                    // central conservará su no-finito de todos modos.
                    *blurred = if *weight > 1e-4 {
                        *blurred / *weight
                    } else {
                        0.0
                    };
                });
        }
        broad_channels.push(broad);
    }
    data.par_chunks_mut(channels)
        .enumerate()
        .for_each(|(pixel, output)| {
            // Reducción por canal: núcleo compacto atenuado + halo atenuado.
            // Se factoriza en un closure porque la luma pivote (C7b) necesita
            // los valores YA reducidos de los tres primeros canales.
            let reduce = |channel: usize| -> f32 {
                let value = source[pixel * channels + channel];
                let broad = broad_channels[channel][pixel];
                let core = value - broad;
                core * (1.0 - reduction * 0.55) + broad * (1.0 - reduction) * (1.0 - halo * 0.65)
            };
            // C7b: el pivote de saturación debe ser la luma del píxel
            // REDUCIDO. Pivotar sobre la luma original re-amplificaba el
            // chroma contra un pivote obsoleto: subir saturación deshacía
            // parcialmente la reducción y re-abrillantaba las estrellas.
            // Con el pivote correcto la luma de salida es exactamente la
            // luma reducida (Σ pesos Rec.709 = 1), así que la saturación
            // sólo mueve chroma, nunca brillo.
            let saturable = channels >= 3
                && (0..3).all(|channel| source[pixel * channels + channel].is_finite());
            let reduced_luma = if saturable {
                0.2126 * reduce(0) + 0.7152 * reduce(1) + 0.0722 * reduce(2)
            } else {
                0.0
            };
            for channel in 0..channels {
                let value = source[pixel * channels + channel];
                if !value.is_finite() {
                    output[channel] = value;
                    continue;
                }
                let reduced = reduce(channel);
                output[channel] = if saturable && channel < 3 {
                    reduced_luma + (reduced - reduced_luma) * (1.0 + saturation)
                } else {
                    // Sin trío RGB finito no hay pivote honesto: se aplica la
                    // reducción sin tocar el chroma en lugar de inventar una
                    // luma con canales rotos.
                    reduced
                };
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_star_field(width: usize, height: usize) -> Vec<f32> {
        let mut image = vec![-2.0f32; width * height];
        for (cx, cy, amplitude, sigma) in [
            (18.5f32, 20.0f32, 1200.0f32, 1.4f32),
            (42.0, 35.5, 850.0, 1.9),
            (27.0, 47.0, 500.0, 1.1),
        ] {
            for y in 0..height {
                for x in 0..width {
                    let r2 = (x as f32 - cx).powi(2) + (y as f32 - cy).powi(2);
                    image[y * width + x] += amplitude * (-r2 / (2.0 * sigma * sigma)).exp();
                }
            }
        }
        image
    }

    #[test]
    fn separation_is_additive_and_preserves_signed_background() {
        let (width, height) = (64, 64);
        let source = synthetic_star_field(width, height);
        let layers =
            separate_stars_native(&source, width, height, 1, 0.6, 1.0, 0.7, 0.6, None).unwrap();
        assert_eq!(
            layers.compose(),
            source,
            "sin edición debe devolver la fuente exacta"
        );
        assert!(layers.object.iter().any(|value| *value < 0.0));
        assert!(layers.stars.iter().any(|value| *value > 0.0));
        assert!(layers.reconstruction_error <= f32::EPSILON * 16.0);
    }

    #[test]
    fn branch_edit_changes_only_the_selected_layer() {
        let (width, height) = (64, 64);
        let source = synthetic_star_field(width, height);
        let mut layers =
            separate_stars_native(&source, width, height, 1, 0.6, 1.0, 0.7, 0.6, None).unwrap();
        let stars_before = layers.stars.clone();
        layers.object[0] += 3.0;
        layers.object_modified = true;
        assert_eq!(layers.stars, stars_before);
        assert_ne!(layers.compose(), source);
    }

    #[test]
    fn recomposition_never_hides_an_invalid_branch_pixel() {
        let (width, height) = (64, 64);
        let source = synthetic_star_field(width, height);
        let mut layers =
            separate_stars_native(&source, width, height, 1, 0.6, 1.0, 0.7, 0.6, None).unwrap();
        layers.object_modified = true;
        layers.stars[7] = f32::NAN;
        layers.residual[11] = f32::NAN;
        let combined = layers.compose();
        assert!(combined[7].is_nan());
        assert!(combined[11].is_nan());
    }

    #[test]
    fn deconvolution_keeps_nan_and_conserves_flux_without_u16_clipping() {
        let (width, height) = (48, 48);
        let mut source = synthetic_star_field(width, height);
        source[0] = f32::NAN;
        source[24 * width + 24] = 120_000.0;
        let input_flux = source
            .iter()
            .filter(|value| value.is_finite())
            .map(|value| *value as f64)
            .sum::<f64>();
        deconvolve_linear(
            &mut source,
            width,
            height,
            1,
            &PostStackPsfModel {
                fwhm_x: 3.2,
                fwhm_y: 3.0,
                measured: true,
                ..PostStackPsfModel::default()
            },
            5,
            0.06,
            0.7,
            true,
        )
        .unwrap();
        let output_flux = source
            .iter()
            .filter(|value| value.is_finite())
            .map(|value| *value as f64)
            .sum::<f64>();
        assert!(source[0].is_nan());
        assert!(source.iter().any(|value| value.is_finite() && *value < 0.0));
        assert!(
            source
                .iter()
                .any(|value| value.is_finite() && *value > u16::MAX as f32),
            "el motor float32 no puede recortar altas luces al rango u16"
        );
        assert!((input_flux - output_flux).abs() <= input_flux.abs().max(1.0) * 2e-4);
    }

    /// Ratio de ejes por momentos segundos ponderados por el flujo sobre
    /// fondo dentro de una ventana centrada. ratio = sqrt(myy/mxx): >1 indica
    /// elongación a lo largo de y. El epsilon simétrico evita el 0/0 si la
    /// estrella colapsara a un único píxel (caso degenerado → ratio 1).
    fn axis_ratio(
        data: &[f32],
        width: usize,
        cx: usize,
        cy: usize,
        window: usize,
        background: f32,
    ) -> f32 {
        let mut mxx = 0.0f64;
        let mut myy = 0.0f64;
        let mut total = 0.0f64;
        for y in (cy - window)..=(cy + window) {
            for x in (cx - window)..=(cx + window) {
                let value = data[y * width + x];
                if !value.is_finite() {
                    continue;
                }
                let weight = (value - background).max(0.0) as f64;
                let dx = x as f64 - cx as f64;
                let dy = y as f64 - cy as f64;
                total += weight;
                mxx += weight * dx * dx;
                myy += weight * dy * dy;
            }
        }
        if total <= 0.0 {
            return 1.0;
        }
        (((myy / total) + 1e-6) / ((mxx / total) + 1e-6)).sqrt() as f32
    }

    /// Anchura a media altura (FWHM) del perfil que pasa por (cx,cy) a lo
    /// largo de x (`horizontal`) o de y, con interpolación lineal subpíxel en
    /// el primer cruce por debajo de la media altura a cada lado del pico.
    /// A diferencia de los momentos segundos, es ROBUSTA al ringing: los
    /// lóbulos secundarios de un kernel desajustado viven muy por debajo de
    /// la media altura y no desplazan el primer cruce.
    fn half_max_width(
        data: &[f32],
        width: usize,
        cx: usize,
        cy: usize,
        horizontal: bool,
        background: f32,
    ) -> f32 {
        let sample = |offset: isize| -> f32 {
            let (x, y) = if horizontal {
                ((cx as isize + offset) as usize, cy)
            } else {
                (cx, (cy as isize + offset) as usize)
            };
            (data[y * width + x] - background).max(0.0)
        };
        let peak = sample(0);
        if peak <= 0.0 {
            return 0.0;
        }
        let half = peak * 0.5;
        let mut width_total = 0.0f32;
        for direction in [-1isize, 1] {
            let mut previous = peak;
            for step in 1..=24isize {
                let value = sample(direction * step);
                if value < half {
                    // Cruce entre step-1 y step: interpolación lineal.
                    let fraction = (previous - half) / (previous - value).max(1e-9);
                    width_total += (step - 1) as f32 + fraction;
                    break;
                }
                previous = value;
            }
        }
        width_total
    }

    /// Ratio de ejes por FWHM: anchura a media altura vertical / horizontal
    /// del perfil que pasa por el pico. >1 indica elongación a lo largo de y.
    fn fwhm_axis_ratio(data: &[f32], width: usize, cx: usize, cy: usize, background: f32) -> f32 {
        let fw_x = half_max_width(data, width, cx, cy, true, background);
        let fw_y = half_max_width(data, width, cx, cy, false, background);
        if fw_x <= 0.0 {
            return 1.0;
        }
        fw_y / fw_x
    }

    fn covariance_about(
        data: &[f32],
        width: usize,
        height: usize,
        cx: f32,
        cy: f32,
    ) -> (f32, f32, f32) {
        let mut weight = 0.0f64;
        let mut xx = 0.0f64;
        let mut yy = 0.0f64;
        let mut xy = 0.0f64;
        for y in 0..height {
            for x in 0..width {
                let value = data[y * width + x].max(0.0) as f64;
                let dx = x as f64 - cx as f64;
                let dy = y as f64 - cy as f64;
                weight += value;
                xx += value * dx * dx;
                yy += value * dy * dy;
                xy += value * dx * dy;
            }
        }
        (
            (xx / weight.max(1e-12)) as f32,
            (yy / weight.max(1e-12)) as f32,
            (xy / weight.max(1e-12)) as f32,
        )
    }

    #[test]
    fn rotated_psf_forward_model_preserves_cross_term_and_orientation() {
        let (width, height) = (97usize, 97usize);
        let (cx, cy) = (48usize, 48usize);
        let mut impulse = vec![0.0f32; width * height];
        impulse[cy * width + cx] = 1.0;
        let theta = std::f32::consts::FRAC_PI_4;
        let rotated = rotated_binomial_gaussian_blur(&impulse, width, height, 2.6, 0.9, theta);
        let (mxx, myy, mxy) = covariance_about(&rotated, width, height, cx as f32, cy as f32);
        let measured_theta = 0.5 * (2.0 * mxy).atan2(mxx - myy);
        assert!(
            mxy > 1.5,
            "una PSF rotada 45° debe conservar covarianza cruzada positiva (mxy={mxy})"
        );
        assert!(
            (measured_theta - theta).abs() < 0.08,
            "la orientación del kernel debe seguir theta (medida={measured_theta}, esperada={theta})"
        );
        assert!(
            (mxx - myy).abs() < 0.2,
            "a 45° ambas proyecciones marginales deben coincidir (mxx={mxx}, myy={myy})"
        );

        // La aproximación anterior sólo proyectaba los dos ejes y, a 45°,
        // degeneraba en una PSF circular sin término cruzado.
        let projected_sigma = ((2.6f32.powi(2) + 0.9f32.powi(2)) * 0.5).sqrt();
        let projected = anisotropic_box_gaussian_blur(
            &impulse,
            width,
            height,
            projected_sigma,
            projected_sigma,
        );
        let (_, _, projected_xy) =
            covariance_about(&projected, width, height, cx as f32, cy as f32);
        assert!(
            projected_xy.abs() < 1.0e-4,
            "proyectar sobre X/Y pierde la orientación y debe demostrar el fallo previo"
        );
    }

    #[test]
    fn deconvolution_with_elliptical_psf_recovers_axis_ratio() {
        // C6: delta convuelta con PSF elongada (sx=1.0, sy=2.2, theta=0). La
        // RL con el kernel elíptico medido debe devolver una estrella con
        // ratio de ejes más cercano a 1 que la ruta circular antigua (que
        // colapsaba a sigma promedio 1.6: sobre-deconvoluciona x y deja y a
        // medias, empeorando la elongación medida).
        //
        // MÉTRICA: el ratio se mide por FWHM y no por momentos segundos. Los
        // momentos con peso (v−fondo)⁺ PREMIAN el defecto que C6 corrige: el
        // kernel circular desajustado inyecta ringing a lo largo del eje
        // menor (lóbulos positivos lejos del núcleo) que engorda mxx por
        // encima incluso del valor observado, y ese engorde artificial acerca
        // sqrt(myy/mxx) a 1 sin corregir nada. La FWHM ignora los lóbulos
        // (viven bajo la media altura) y mide la elongación real del núcleo.
        //
        // La verdad se sintetiza con la gaussiana anisotrópica EXACTA y se
        // invierte con el modelo box: generar e invertir con el mismo
        // operador sería un "inverse crime" que ocultaría el desajuste de
        // familia presente en producción (PSF real Moffat vs modelo box).
        let (width, height) = (96usize, 96usize);
        let (cx, cy) = (48usize, 48usize);
        let background = 5.0f32;
        let mut truth = vec![background; width * height];
        truth[cy * width + cx] += 4000.0;
        let observed = anisotropic_gaussian_blur(&truth, width, height, 1.0, 2.2);
        let ratio_observed = fwhm_axis_ratio(&observed, width, cx, cy, background);
        assert!(
            ratio_observed > 1.8,
            "el campo sintético debe estar claramente elongado (ratio={ratio_observed})"
        );

        let fwhm_factor = 2.354_820_1f32;
        let psf_elliptical = PostStackPsfModel {
            fwhm_x: 1.0 * fwhm_factor,
            fwhm_y: 2.2 * fwhm_factor,
            theta: 0.0,
            measured: true,
            ..PostStackPsfModel::default()
        };
        // Réplica exacta del comportamiento antiguo: ambos ejes al promedio
        // (1.0+2.2)/2 = 1.6 — con fwhm_x == fwhm_y la ruta circular se activa.
        let psf_circular = PostStackPsfModel {
            fwhm_x: 1.6 * fwhm_factor,
            fwhm_y: 1.6 * fwhm_factor,
            theta: 0.0,
            measured: true,
            ..PostStackPsfModel::default()
        };
        let input_flux = observed.iter().map(|value| *value as f64).sum::<f64>();

        let iterations = 32;
        let mut restored_elliptical = observed.clone();
        deconvolve_linear(
            &mut restored_elliptical,
            width,
            height,
            1,
            &psf_elliptical,
            iterations,
            0.02,
            0.2,
            true,
        )
        .unwrap();
        let mut restored_circular = observed.clone();
        deconvolve_linear(
            &mut restored_circular,
            width,
            height,
            1,
            &psf_circular,
            iterations,
            0.02,
            0.2,
            true,
        )
        .unwrap();

        let ratio_elliptical = fwhm_axis_ratio(&restored_elliptical, width, cx, cy, background);
        let ratio_circular = fwhm_axis_ratio(&restored_circular, width, cx, cy, background);
        assert!(
            (ratio_elliptical - 1.0).abs() < (ratio_circular - 1.0).abs(),
            "el kernel elíptico debe acercar el ratio a 1 más que el circular \
             (elíptico={ratio_elliptical}, circular={ratio_circular}, observado={ratio_observed})"
        );
        // Y debe mejorar de verdad respecto al observado: corregir elongación,
        // no solo "perder por menos" que la ruta circular.
        assert!(
            ratio_elliptical < ratio_observed - 0.15,
            "la elongación debe reducirse (elíptico={ratio_elliptical}, observado={ratio_observed})"
        );
        // Corregir el eje mayor no puede costar ensanchar el menor: la FWHM
        // en x de la restauración elíptica no supera la observada.
        let fw_x_restored = half_max_width(&restored_elliptical, width, cx, cy, true, background);
        let fw_x_observed = half_max_width(&observed, width, cx, cy, true, background);
        assert!(
            fw_x_restored <= fw_x_observed + 0.05,
            "el eje menor no debe ensancharse (restaurado={fw_x_restored}, observado={fw_x_observed})"
        );
        // La conservación de flujo sigue intacta en la ruta elíptica.
        let output_flux = restored_elliptical
            .iter()
            .filter(|value| value.is_finite())
            .map(|value| *value as f64)
            .sum::<f64>();
        assert!(
            (input_flux - output_flux).abs() <= input_flux.abs().max(1.0) * 2e-4,
            "flujo entrada={input_flux} salida={output_flux}"
        );
    }

    #[test]
    fn anisotropic_box_blur_is_flux_conserving_and_collapses_to_isotropic_family() {
        // El modelo directo elíptico de la RL debe (a) conservar flujo — cajas
        // normalizadas 1/(2r+1) con bordes por replicación — y (b) colapsar a
        // la MISMA familia que apply_gaussian_blur cuando σx == σy: las seis
        // pasadas 1-D conmutan con las tres pasadas 2-D de box_blur_parallel
        // (operador separable), solo difiere el orden de redondeo float32.
        let (width, height) = (64usize, 64usize);
        let mut field = vec![2.0f32; width * height];
        field[32 * width + 32] += 1000.0;
        field[20 * width + 40] += 300.0;
        let input_flux = field.iter().map(|value| *value as f64).sum::<f64>();
        let blurred = anisotropic_box_gaussian_blur(&field, width, height, 1.0, 2.2);
        let output_flux = blurred.iter().map(|value| *value as f64).sum::<f64>();
        assert!((input_flux - output_flux).abs() <= input_flux.abs() * 1e-4);
        // Elongación del núcleo difundido a lo largo de y (σy > σx).
        let fw_x = half_max_width(&blurred, width, 32, 32, true, 2.0);
        let fw_y = half_max_width(&blurred, width, 32, 32, false, 2.0);
        assert!(
            fw_y > fw_x * 1.5,
            "la delta difundida debe quedar elongada en y (fw_x={fw_x}, fw_y={fw_y})"
        );
        let isotropic_family = crate::apply_gaussian_blur(&field, width, height, 1.6);
        let circular = anisotropic_box_gaussian_blur(&field, width, height, 1.6, 1.6);
        for (ours, reference) in circular.iter().zip(isotropic_family.iter()) {
            assert!(
                (ours - reference).abs() <= reference.abs() * 1e-4 + 1e-3,
                "con σx==σy ambas rutas deben coincidir ({ours} vs {reference})"
            );
        }
    }

    #[test]
    fn anisotropic_blur_conserves_flux_and_matches_axes() {
        // El kernel 1-D está normalizado (Σ=1) y los bordes replican, así que
        // el flujo total de un campo interior se conserva; y la elongación
        // resultante de una delta debe seguir sigma_y/sigma_x.
        let (width, height) = (64usize, 64usize);
        let mut field = vec![2.0f32; width * height];
        field[32 * width + 32] += 1000.0;
        let input_flux = field.iter().map(|value| *value as f64).sum::<f64>();
        let blurred = anisotropic_gaussian_blur(&field, width, height, 1.0, 2.2);
        let output_flux = blurred.iter().map(|value| *value as f64).sum::<f64>();
        assert!((input_flux - output_flux).abs() <= input_flux.abs() * 1e-4);
        let ratio = axis_ratio(&blurred, width, 32, 32, 10, 2.0);
        assert!(
            (ratio - 2.2).abs() < 0.25,
            "la delta difundida debe medir ratio≈2.2 (medido {ratio})"
        );
    }

    #[test]
    fn star_layer_blur_is_not_poisoned_by_a_nan() {
        // C7a: un único no-finito en la capa no puede envenenar el halo
        // blurreado de sus vecinos. La salida conserva el NaN EXACTAMENTE en
        // su píxel/canal y todo lo demás sigue finito.
        let (width, height) = (48usize, 48usize);
        let mono = synthetic_star_field(width, height);
        let mut data = Vec::with_capacity(width * height * 3);
        for value in &mono {
            data.push(*value);
            data.push(*value * 0.8);
            data.push(*value * 0.6);
        }
        let poisoned = (10 * width + 10) * 3 + 1;
        data[poisoned] = f32::NAN;
        adjust_star_layer(&mut data, width, height, 3, 0.4, 0.3, 0.2);
        assert!(data[poisoned].is_nan(), "el no-finito original se restaura");
        for (index, value) in data.iter().enumerate() {
            if index != poisoned {
                assert!(
                    value.is_finite(),
                    "el índice {index} debe seguir finito: el NaN no puede propagarse"
                );
            }
        }
    }

    #[test]
    fn saturation_pivots_on_reduced_luma_and_does_not_rebrighten() {
        // C7b: con el pivote sobre la luma REDUCIDA, la saturación mueve solo
        // chroma: la luma de salida es idéntica al camino sin saturación
        // (Σ pesos Rec.709 = 1). El pivote antiguo (luma original) alteraba el
        // brillo: con saturación negativa re-abrillantaba la estrella hacia su
        // luma pre-reducción, deshaciendo la reducción pedida.
        let (width, height) = (32usize, 32usize);
        let mut field = vec![0.0f32; width * height * 3];
        for y in 0..height {
            for x in 0..width {
                let r2 = (x as f32 - 16.0).powi(2) + (y as f32 - 16.0).powi(2);
                field[(y * width + x) * 3] = 800.0 * (-r2 / (2.0 * 1.5 * 1.5)).exp();
            }
        }
        let luma_of =
            |pixel: &[f32]| -> f32 { 0.2126 * pixel[0] + 0.7152 * pixel[1] + 0.0722 * pixel[2] };
        let peak = (16 * width + 16) * 3;

        let mut no_saturation = field.clone();
        adjust_star_layer(&mut no_saturation, width, height, 3, 0.5, 0.0, 0.0);
        let luma_reference = luma_of(&no_saturation[peak..peak + 3]);
        assert!(luma_reference > 0.0);

        for saturation in [0.5f32, -0.5f32] {
            let mut saturated = field.clone();
            adjust_star_layer(&mut saturated, width, height, 3, 0.5, saturation, 0.0);
            let luma_saturated = luma_of(&saturated[peak..peak + 3]);
            // No re-abrillanta: el brillo nunca supera el camino sin saturación.
            assert!(
                luma_saturated <= luma_reference + luma_reference.abs() * 1e-4 + 1e-4,
                "saturación {saturation}: luma {luma_saturated} > referencia {luma_reference}"
            );
            // Y es exactamente el mismo brillo: la saturación solo mueve chroma.
            assert!(
                (luma_saturated - luma_reference).abs() <= luma_reference.abs() * 1e-3 + 1e-3,
                "saturación {saturation}: luma {luma_saturated} != referencia {luma_reference}"
            );
            // El tono se conserva: el rojo sigue dominando y G==B (estrella
            // de color puro con planos G y B idénticos).
            assert!(saturated[peak] > saturated[peak + 1]);
            assert!(saturated[peak] > saturated[peak + 2]);
            assert!((saturated[peak + 1] - saturated[peak + 2]).abs() <= 1e-3);
        }
    }

    #[test]
    fn deconvolution_flux_guard_is_stable_for_zero_sum_signed_data() {
        let (width, height) = (32, 32);
        let mut source = (0..width * height)
            .map(|index| {
                let phase = index as f32 * 0.173;
                phase.sin() * 4.0
            })
            .collect::<Vec<_>>();
        let mean = source.iter().map(|value| *value as f64).sum::<f64>() / source.len() as f64;
        source.iter_mut().for_each(|value| *value -= mean as f32);
        let original = source.clone();
        deconvolve_linear(
            &mut source,
            width,
            height,
            1,
            &PostStackPsfModel {
                fwhm_x: 2.8,
                fwhm_y: 2.8,
                measured: true,
                ..PostStackPsfModel::default()
            },
            4,
            0.08,
            0.8,
            true,
        )
        .unwrap();
        let input_flux = original.iter().map(|value| *value as f64).sum::<f64>();
        let output_flux = source.iter().map(|value| *value as f64).sum::<f64>();
        assert!(source.iter().all(|value| value.is_finite()));
        assert!(source.iter().all(|value| value.abs() < 100.0));
        assert!((input_flux - output_flux).abs() <= 2e-3);
    }
}
