// ==========================================
// 5. FILTROS Y WAVELETS
// ==========================================

/// Blur BILATERAL (edge-preserving) para la descomposicion wavelet edge-aware
/// (opcion B). A diferencia del Gaussiano, NO cruza los bordes de alto
/// contraste: el termino de rango (diferencia de intensidad) anula los vecinos
/// del otro lado del borde. Resultado: el detalle `base - bilateral(base)` cerca
/// del limbo es ~0 (el borde queda en la base, no en el detalle), asi que
/// amplificar las bandas finas NO genera el ringing/gusanos del limbo.
/// `sigma_range` se calibra al contraste real de la imagen (ver caller).
/// Solo se usa en las escalas FINAS (radio pequeño) → coste acotado.
fn apply_bilateral_blur(
    input: &[f32],
    width: usize,
    height: usize,
    sigma_spatial: f32,
    sigma_range: f32,
) -> Vec<f32> {
    let radius = (sigma_spatial * 2.5).ceil().clamp(1.0, 9.0) as isize;
    let inv2_s = 1.0 / (2.0 * sigma_spatial * sigma_spatial);
    let inv2_r = 1.0 / (2.0 * sigma_range.max(1.0) * sigma_range.max(1.0));
    // LUT espacial (por offset) para no recalcular exp de distancia.
    let mut spatial = vec![0.0f32; (2 * radius + 1) as usize * (2 * radius + 1) as usize];
    let sw = (2 * radius + 1) as usize;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let d2 = (dx * dx + dy * dy) as f32;
            spatial[((dy + radius) as usize) * sw + (dx + radius) as usize] =
                (-d2 * inv2_s).exp();
        }
    }
    // VELOCIDAD: LUT del término de RANGO exp(-dr²·inv2_r). El exp era el coste
    // dominante del bilateral (uno por vecino = W·H·(2r+1)² exp). Ahora se
    // precomputa una tabla de RANGE_LUT_N muestras hasta el corte donde el peso
    // es despreciable (~4.5·σ_range → exp(-10)≈4.5e-5) y en el bucle interno se
    // hace un lookup por |dr| cuantizado. Error < 1e-3 con N=2048.
    const RANGE_LUT_N: usize = 2048;
    let range_cut = (4.5 * sigma_range.max(1.0)).max(1.0);
    let inv_cut = (RANGE_LUT_N - 1) as f32 / range_cut; // idx = |dr|·inv_cut
    let mut range_lut = vec![0.0f32; RANGE_LUT_N];
    for (k, slot) in range_lut.iter_mut().enumerate() {
        let dr = k as f32 / inv_cut;
        *slot = (-dr * dr * inv2_r).exp();
    }
    let mut out = vec![0.0f32; width * height];
    out.par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row)| {
            let y = y as isize;
            for x in 0..width as isize {
                let center = input[(y as usize) * width + x as usize];
                let mut sum = 0.0f32;
                let mut wsum = 0.0f32;
                for dy in -radius..=radius {
                    let yy = y + dy;
                    if yy < 0 || yy >= height as isize {
                        continue;
                    }
                    let srow = (yy as usize) * width;
                    let sprow = ((dy + radius) as usize) * sw;
                    for dx in -radius..=radius {
                        let xx = x + dx;
                        if xx < 0 || xx >= width as isize {
                            continue;
                        }
                        let v = input[srow + xx as usize];
                        let idx = ((v - center).abs() * inv_cut) as usize;
                        let rw = if idx < RANGE_LUT_N { range_lut[idx] } else { 0.0 };
                        let w = spatial[sprow + (dx + radius) as usize] * rw;
                        sum += v * w;
                        wsum += w;
                    }
                }
                row[x as usize] = if wsum > 1e-9 { sum / wsum } else { center };
            }
        });
    out
}

/// Estimador robusto del `sigma_range` bilateral: mediana de |gradiente| ×
/// `mult`. Escala automaticamente con el nivel de señal/ruido de CUALQUIER
/// objeto (planeta tenue, Luna brillante) — el detalle real (por debajo de
/// unos pocos σ_range) se preserva, el borde del limbo (muy por encima) NO se
/// mezcla. `mult` lo controla el slider "Intensidad Edge-Aware": mas intensidad
/// → `mult` menor → kernel de rango mas estrecho → borde MAS protegido.
fn estimate_bilateral_range(input: &[f32], width: usize, height: usize, mult: f32) -> f32 {
    let size = width * height;
    if size < 16 {
        return 1000.0;
    }
    let stride = (size / 20000).max(1);
    let mut grads: Vec<f32> = Vec::new();
    let mut i = width + 1;
    while i < size - width - 1 {
        let gx = (input[i + 1] - input[i - 1]).abs();
        let gy = (input[i + width] - input[i - width]).abs();
        grads.push(gx.max(gy));
        i += stride;
    }
    if grads.is_empty() {
        return 1000.0;
    }
    grads.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let med = grads[grads.len() / 2];
    // `mult`× la mediana del gradiente (por defecto 4×): el detalle fino/ruido
    // queda dentro del kernel de rango; el salto del limbo (ordenes de magnitud
    // mayor) queda fuera → se preserva el borde sin mezclarlo.
    (med * mult).clamp(200.0, 20000.0)
}

/// Estimador de ruido de Donoho sobre una banda de detalle wavelet: σ ≈
/// 1.4826 · mediana(|detalle|). La banda fina es de media ~cero, asi que la MAD
/// respecto a 0 aproxima la desviacion tipica del ruido. Se usa para la
/// auto-mascara adaptativa (distinguir ruido plano de estructura real).
fn estimate_noise_mad(band: &[f32]) -> f32 {
    if band.is_empty() {
        return 0.0;
    }
    let stride = (band.len() / 20000).max(1);
    let mut mags: Vec<f32> = band.iter().step_by(stride).map(|v| v.abs()).collect();
    if mags.is_empty() {
        return 0.0;
    }
    mags.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    mags[mags.len() / 2] * 1.4826
}

/// Mapa de confianza de detalle en [0,1) para el sharpening ADAPTATIVO
/// (auto-mascara). Estructura coherente (magnitud del detalle suavizada, alta
/// respecto al ruido) → ~1 (se amplifica); ruido aislado (magnitud baja) → ~0
/// (se atenua). Formulacion Wiener-like: conf = s / (s + β·σ_ruido), con `s` la
/// magnitud del detalle SUAVIZADA (box radio 2) para exigir coherencia espacial
/// — el ruido impulsivo se promedia a la baja, el borde/textura persiste. Esto
/// mata el ruido "wormy" que el sharpening por-capa de RegiStax/WaveSharp
/// amplifica de forma uniforme.
fn detail_confidence_map(band: &[f32], width: usize, height: usize, sigma_n: f32) -> Vec<f32> {
    let size = width * height;
    let mag: Vec<f32> = band.iter().map(|v| v.abs()).collect();
    let sm = box_blur_parallel(&mag, width, height, 2);
    let denom = (3.0 * sigma_n).max(1e-3);
    let mut conf = vec![0.0f32; size];
    conf.par_iter_mut().enumerate().for_each(|(i, c)| {
        let s = sm[i];
        *c = s / (s + denom);
    });
    conf
}

/// Convolucion 2-D directa con un kernel pequeño (PSF medida, opcion A).
/// Bordes por replicacion (clamp). El kernel debe estar normalizado (Σ=1).
fn convolve_kernel(input: &[f32], width: usize, height: usize, kernel: &[f32], k_radius: usize) -> Vec<f32> {
    let ksize = 2 * k_radius + 1;
    let mut out = vec![0.0f32; width * height];
    out.par_chunks_mut(width)
        .enumerate()
        .for_each(|(y, row)| {
            let y = y as isize;
            for x in 0..width as isize {
                let mut acc = 0.0f32;
                for ky in 0..ksize {
                    let sy = (y + ky as isize - k_radius as isize)
                        .clamp(0, height as isize - 1) as usize;
                    let srow = sy * width;
                    let krow = ky * ksize;
                    for kx in 0..ksize {
                        let sx = (x + kx as isize - k_radius as isize)
                            .clamp(0, width as isize - 1) as usize;
                        acc += input[srow + sx] * kernel[krow + kx];
                    }
                }
                row[x as usize] = acc;
            }
        });
    out
}

fn apply_gaussian_blur(input: &[f32], width: usize, height: usize, sigma: f32) -> Vec<f32> {
    // OPTIMIZATION: 3-pass box blur approximating a Gaussian — CANONICAL box
    // sizing (Kuckir): w_ideal = sqrt(12σ²/n + 1) with n = 3 passes. The old
    // formula omitted the /n, so the EFFECTIVE sigma was ~1.73× the requested
    // one: every wavelet band sat ~73% coarser than labeled (finest band ≈1.7px
    // instead of 1px — RegiStax level-1 territory was unreachable) and the
    // RL/VC deconvolution PSF never matched its slider.
    let n = 3.0f32;
    let sigma = sigma.max(0.3);
    let w_ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut wl = w_ideal.floor() as isize;
    if wl % 2 == 0 {
        wl -= 1; // force odd
    }
    let wl = wl.max(1) as usize;
    let wu = wl + 2;

    // Ideal pass count with the smaller box (canonical formula).
    let m_ideal = (12.0 * sigma * sigma
        - (n * (wl * wl) as f32)
        - (4.0 * n * wl as f32)
        - (3.0 * n))
        / (-4.0 * wl as f32 - 4.0);
    let m = m_ideal.round().clamp(0.0, n) as usize;

    let sizes = [
        if 0 < m { wl } else { wu },
        if 1 < m { wl } else { wu },
        if 2 < m { wl } else { wu },
    ];

    let mut current = input.to_vec();

    for &box_size in &sizes {
        let radius = (box_size - 1) / 2;
        if radius > 0 {
            current = box_blur_parallel(&current, width, height, radius);
        }
    }

    current
}

fn box_blur_parallel(input: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let size = w * h;
    // GUARD: un buffer más corto que la geometría declarada (canal vacío de un
    // caché envenenado, aborto a medio camino) paniqueaba con "range start
    // index … out of range". Devolver negro es un estado visible y recuperable;
    // el panic tumbaba el pipeline entero de post-procesado.
    if input.len() < size {
        eprintln!(
            "[filters] box_blur_parallel: entrada {} < {}x{} — devolviendo búfer vacío",
            input.len(),
            w,
            h
        );
        return vec![0.0; size];
    }
    let mut temp = vec![0.0; size];
    let mut output = vec![0.0; size];

    // Horizontal Pass (Parallel)
    temp.par_chunks_exact_mut(w)
        .enumerate()
        .for_each(|(y, row_out)| {
            let row_in = &input[y * w..(y + 1) * w];

            // Init accumulator
            let mut sum = 0.0;
            let iarr = 1.0 / (2.0 * r as f32 + 1.0);

            // Init accumulator for x=0 (Window: -r to r)
            for i in -(r as isize)..=(r as isize) {
                sum += row_in[i.clamp(0, (w - 1) as isize) as usize];
            }
            row_out[0] = sum * iarr;

            for i in 1..w {
                let leaving =
                    row_in[((i as isize) - 1 - (r as isize)).clamp(0, (w - 1) as isize) as usize];
                let entering =
                    row_in[((i as isize) + (r as isize)).clamp(0, (w - 1) as isize) as usize];
                sum = sum - leaving + entering;
                row_out[i] = sum * iarr;
            }
        });

    // Transpose (F3: paralela por columnas — las dos transposiciones en serie
    // eran la fracción monohilo del blur, que corre 6 sigmas × canales ×
    // (deconv+wavelets) en cada render interactivo del editor).
    let mut transp = vec![0.0; size];
    transp
        .par_chunks_exact_mut(h)
        .enumerate()
        .for_each(|(x, col_out)| {
            for y in 0..h {
                col_out[y] = temp[y * w + x];
            }
        });

    // Vertical Pass (Horizontal on Transposed)
    let mut temp_transp = vec![0.0; size];
    temp_transp
        .par_chunks_exact_mut(h)
        .enumerate()
        .for_each(|(_x, col_out)| {
            let col_in = &transp[_x * h..(_x + 1) * h];
            let mut sum = 0.0;
            let iarr = 1.0 / (2.0 * r as f32 + 1.0);

            // Init accumulator for y=0 (Window: -r to r)
            for i in -(r as isize)..=(r as isize) {
                sum += col_in[i.clamp(0, (h - 1) as isize) as usize];
            }
            col_out[0] = sum * iarr;

            for i in 1..h {
                let leaving =
                    col_in[((i as isize) - 1 - (r as isize)).clamp(0, (h - 1) as isize) as usize];
                let entering =
                    col_in[((i as isize) + (r as isize)).clamp(0, (h - 1) as isize) as usize];
                sum = sum - leaving + entering;
                col_out[i] = sum * iarr;
            }
        });

    // Transpose Back (F3: paralela por filas de salida)
    output
        .par_chunks_exact_mut(w)
        .enumerate()
        .for_each(|(y, row_out)| {
            for x in 0..w {
                row_out[x] = temp_transp[x * h + y];
            }
        });

    output
}

fn check_cancel(state: &AppState, req_id: usize) -> bool {
    state.cancel_requested.load(Ordering::Acquire)
        || state.active_req_id.load(Ordering::Acquire) != req_id
}

/// Publica una entrada derivada sólo mientras la generación que la calculó
/// sigue siendo propietaria. El orden global es siempre generation_gate →
/// cache mutex, igual que clear/stack/derotación, de modo que un worker viejo
/// no puede reinsertar canales después de que el nuevo máster los borró.
fn commit_processing_cache_if_current(
    state: &AppState,
    req_id: usize,
    commit: impl FnOnce(),
) -> bool {
    let _generation_guard = state
        .planetary_generation_gate
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if check_cancel(state, req_id) {
        return false;
    }
    commit();
    true
}

fn apply_richardson_lucy(
    app: &tauri::AppHandle,
    state: &AppState,
    req_id: usize,
    input: &[f32],
    original: &[f32],
    width: usize,
    height: usize,
    iterations: usize,
    sigma: f32,
    range: (f32, f32),
    // PSF MEDIDA (opcion A): Some((kernel, radius)) → RL usa la PSF real del
    // limbo en vez de la Gaussiana. El kernel medido es radialmente simetrico
    // (PsfEstimator lo construye por distancia) → adjunta = el mismo kernel,
    // asi que se usa para el forward-blur Y la back-projection. None = Gaussiana.
    measured_psf: Option<(&[f32], usize)>,
    protection: ProtectionProfile,
) -> Vec<f32> {
    if iterations == 0 || (sigma <= 0.0 && measured_psf.is_none()) {
        return input.to_vec();
    }
    // Forward model: convolucion con la PSF activa (medida o Gaussiana).
    let blur = |img: &[f32]| -> Vec<f32> {
        match measured_psf {
            Some((k, r)) => convolve_kernel(img, width, height, k, r),
            None => apply_gaussian_blur(img, width, height, sigma),
        }
    };
    let should_cancel = || check_cancel(state, req_id);
    let on_progress = |i: usize| {
        if range.1 - range.0 > 1.0 {
            let local_p = i as f32 / iterations as f32;
            let global_p = range.0 + local_p * (range.1 - range.0);
            emit_progress(
                app,
                "Deconvolucion RL (TV)",
                global_p,
                Some(format!("Iteracion {}/{}", i + 1, iterations)),
            );
        }
    };
    richardson_lucy_core(
        input, original, width, height, iterations, sigma, &blur, &should_cancel, &on_progress,
        protection,
    )
    .unwrap_or_default()
}

/// Máscara de confianza del RL (señal fuerte sobre el fondo → 1; fondo → 0).
/// Extraída para compartirla EXACTAMENTE con la ruta GPU (misma entrada = misma
/// máscara).
fn richardson_lucy_mask(original: &[f32], width: usize, height: usize, sigma: f32) -> Vec<f32> {
    let size = width * height;
    let bg_blur = apply_gaussian_blur(original, width, height, sigma * 3.0);
    let mut sorted = original.to_vec();
    sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p95 = sorted[size * 95 / 100].max(100.0);
    let noise_floor = sorted[size * 5 / 100].max(1.0);
    let mut mask = vec![0.0f32; size];
    for j in 0..size {
        let local_diff = (original[j] - bg_blur[j]).abs();
        let signal_ratio = (original[j] - noise_floor) / p95;
        let diff_ratio = local_diff / (p95 * 0.1).max(1.0);
        mask[j] = (signal_ratio * 0.5 + diff_ratio * 0.5).clamp(0.0, 1.0);
    }
    mask
}

/// Núcleo Richardson-Lucy (TV + máscara de confianza, actualización JACOBI) SIN
/// dependencias de Tauri: `blur` es la convolución con la PSF activa (Gaussiana
/// o medida), `should_cancel` aborta devolviendo None, `on_progress` reporta la
/// iteración. Compartido por la ruta de producción (apply_richardson_lucy) y por
/// el self-test de paridad GPU → la referencia y la producción son EL MISMO
/// cálculo (imposible que deriven).
fn richardson_lucy_core(
    input: &[f32],
    original: &[f32],
    width: usize,
    height: usize,
    iterations: usize,
    sigma: f32,
    blur: &dyn Fn(&[f32]) -> Vec<f32>,
    should_cancel: &dyn Fn() -> bool,
    on_progress: &dyn Fn(usize),
    protection: ProtectionProfile,
) -> Option<Vec<f32>> {
    let size = width * height;
    let mut est = input.to_vec();

    // Máscara de confianza (helper compartido con la ruta GPU → idéntica).
    let mask = richardson_lucy_mask(original, width, height, sigma);

    let mut ratio_buf = vec![0.0f32; size];
    // The previous Laplacian term was applied at 5% on every iteration. On
    // extended solar/lunar texture it could dominate the RL correction and
    // make "deconvolution" measurably softer. Keep regularisation only as a
    // very small background stabiliser; signal regions are already protected
    // by the confidence mask and ratio clamp.
    let background_regularisation = 0.0025;

    for i in 0..iterations {
        if should_cancel() {
            return None;
        }
        on_progress(i);

        // 1. Forward blur del estimado actual.
        let blurred_est = blur(&est);

        // 2. Ratio (original / blurred), acotado para evitar explosión.
        for j in 0..size {
            let denom = blurred_est[j].max(1.0);
            let raw_ratio = if original[j] > 1.0 {
                original[j] / denom
            } else {
                1.0
            };
            let (ratio_lo, ratio_hi) = protection.rl_ratio_bounds();
            ratio_buf[j] = raw_ratio.clamp(ratio_lo, ratio_hi);
        }

        // 3. Back-projection (PSF simétrica → mismo kernel que el forward).
        let blurred_ratio = blur(&ratio_buf);

        // 4. Update JACOBI (snapshot) + confidence mask. The multiplicative RL
        // correction remains the primary operation; background-only
        // regularisation prevents isolated noise from growing without erasing
        // real high-frequency texture.
        let est_prev = est.clone();
        est.par_chunks_mut(width).enumerate().for_each(|(y, row)| {
            if y == 0 || y == height - 1 {
                return;
            }
            for x in 1..(width - 1) {
                let j = y * width + x;
                let n1 = est_prev[y * width + (x - 1)];
                let n2 = est_prev[y * width + (x + 1)];
                let n3 = est_prev[(y - 1) * width + x];
                let n4 = est_prev[(y + 1) * width + x];
                let local_mean = (n1 + n2 + n3 + n4) * 0.25;
                let tv_gradient = local_mean - est_prev[j];
                let confidence = mask[j].clamp(0.0, 1.0);
                let correction_weight = confidence.sqrt() * protection.rl_correction_weight();
                let raw_delta = est_prev[j] * (blurred_ratio[j] - 1.0);
                // Bound each iteration instead of globally clipping the final
                // restoration. This keeps strong limbs stable while allowing
                // coherent texture to accumulate over several iterations.
                let max_step = protection.rl_max_step(original[j]);
                let rl_delta = raw_delta.clamp(-max_step, max_step) * correction_weight;
                let noise_regularisation = tv_gradient
                    * background_regularisation
                    * (1.0 - confidence).powi(2);
                let mut final_val = est_prev[j] + rl_delta + noise_regularisation;
                if final_val.is_nan() || final_val.is_infinite() {
                    final_val = original[j];
                }
                final_val = final_val.clamp(0.0, 65535.0);
                row[x] = final_val;
            }
        });
    }

    Some(est)
}

fn apply_van_cittert(
    app: &tauri::AppHandle,
    state: &AppState,
    req_id: usize,
    input: &[f32],
    original: &[f32],
    width: usize,
    height: usize,
    iterations: usize,
    sigma: f32,
    range: (f32, f32),
    protection: ProtectionProfile,
) -> Vec<f32> {
    if iterations == 0 || sigma <= 0.0 {
        return input.to_vec();
    }

    let size = width * height;
    let mut est = input.to_vec();

    // We compute a "confidence mask" based on edge strength and signal intensity.
    // Deconvolution should only work hard where there are actual structures.
    let mut mask = vec![0.0f32; size];
    let bg_blur = apply_gaussian_blur(original, width, height, sigma * 3.0);

    // Finding p95 roughly to understand signal level
    // Copy into a vector and sort
    let mut sorted = original.to_vec();
    sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p95 = sorted[size * 95 / 100].max(100.0);
    let noise_floor = sorted[size * 5 / 100].max(1.0);

    for j in 0..size {
        // High confidence where signal stands out strongly above the background
        let local_diff = (original[j] - bg_blur[j]).abs();
        let signal_ratio = (original[j] - noise_floor) / p95;
        let diff_ratio = local_diff / (p95 * 0.1).max(1.0);

        let confidence = (signal_ratio * 0.5 + diff_ratio * 0.5).clamp(0.0, 1.0);
        mask[j] = confidence; // Smoothed application weight
    }

    // Van Cittert oscila con facilidad: incluso en Puro se queda en 0.90.
    let vc_dampening = protection.vc_dampening();
    let tv_weight = 0.10; // TV dampening to kill individual hot pixels.

    for i in 0..iterations {
        if check_cancel(state, req_id) {
            return Vec::new();
        }
        if range.1 - range.0 > 1.0 && i % 1 == 0 {
            let local_p = i as f32 / iterations as f32;
            let global_p = range.0 + local_p * (range.1 - range.0);
            emit_progress(
                app,
                "Deconvolucion VC (TV)",
                global_p,
                Some(format!("Iteracion {}/{}", i + 1, iterations)),
            );
        }

        // 1. Blur the Current Estimate
        let blurred_est = apply_gaussian_blur(&est, width, height, sigma);

        // 2. Iterate block — F3: SNAPSHOT JACOBI + rayon (como richardson_lucy_core).
        // El barrido Gauss-Seidel anterior leía vecinos YA modificados en la
        // misma iteración (serie y=1.., x=1..): sesgo direccional
        // arriba-izquierda→abajo-derecha en el detalle deconvolucionado, y
        // además impedía paralelizar. El TV lee ahora est_prev (inmutable).
        let est_prev = est.clone();
        est.par_chunks_mut(width)
            .enumerate()
            .skip(1)
            .take(height.saturating_sub(2))
            .for_each(|(y, row)| {
                for x in 1..(width - 1) {
                    let j = y * width + x;

                    // Van Cittert Residual (Difference between Original and Blurred Estimate)
                    let residual = (original[j] - blurred_est[j]).clamp(-15000.0, 15000.0);

                    // Total Variation (TV) - Push pixel toward its neighborhood average if it's spiking
                    let n1 = est_prev[y * width + (x - 1)];
                    let n2 = est_prev[y * width + (x + 1)];
                    let n3 = est_prev[(y - 1) * width + x];
                    let n4 = est_prev[(y + 1) * width + x];
                    let local_mean = (n1 + n2 + n3 + n4) * 0.25;
                    let tv_gradient = local_mean - est_prev[j];

                    // Apply update natively
                    let mut update = est_prev[j] + (residual * vc_dampening);

                    // Apply TV Regularization to smooth out ringing/spikes
                    update += tv_gradient * tv_weight;

                    let correction_weight = mask[j] * protection.vc_correction_weight();
                    let mut final_val =
                        est_prev[j] * (1.0 - correction_weight) + update * correction_weight;

                    if final_val.is_nan() || final_val.is_infinite() {
                        final_val = original[j];
                    }

                    // Hard mathematical limits for 16-bit space
                    // Allow a tiny bit of negative headroom during intermediate VC steps, but not full -30,000 runaway
                    if final_val > 65535.0 {
                        final_val = 65535.0;
                    }
                    if final_val < -2000.0 {
                        final_val = -2000.0;
                    } // Very clamped bounce floor

                    row[x] = final_val;
                }
            });
    }

    // Final hard-clamp negative residual energy to solid black
    for j in 0..size {
        if est[j] < 0.0 {
            est[j] = 0.0;
        }
    }

    est
}

fn shift_channel(channel: &[f32], width: usize, height: usize, dx: f32, dy: f32) -> Vec<f32> {
    if dx.abs() < 0.01 && dy.abs() < 0.01 {
        return channel.to_vec();
    }
    let mut out = vec![0.0; channel.len()];
    for y in 0..height {
        for x in 0..width {
            let sx = x as f32 - dx;
            let sy = y as f32 - dy;
            if sx >= 0.0 && sy >= 0.0 && sx < (width as f32 - 1.0) && sy < (height as f32 - 1.0) {
                let xf = sx.floor() as usize;
                let yf = sy.floor() as usize;
                let wx = sx - xf as f32;
                let wy = sy - yf as f32;
                let i00 = yf * width + xf;
                let i10 = i00 + 1;
                let i01 = (yf + 1) * width + xf;
                let i11 = i01 + 1;
                out[y * width + x] = (channel[i00] * (1.0 - wx) + channel[i10] * wx) * (1.0 - wy)
                    + (channel[i01] * (1.0 - wx) + channel[i11] * wx) * wy;
            }
        }
    }
    out
}

fn estimate_color_adjust_context(data: &[u16]) -> (f32, f32) {
    let pixel_count = data.len() / 3;
    if pixel_count == 0 {
        return (8192.0, 65535.0);
    }

    let step = (pixel_count / 200_000).max(1);
    let mut lumas: Vec<f32> = data
        .chunks_exact(3)
        .step_by(step)
        .map(|px| 0.299 * px[0] as f32 + 0.587 * px[1] as f32 + 0.114 * px[2] as f32)
        .filter(|v| v.is_finite() && *v > 0.0)
        .collect();

    if lumas.len() < 16 {
        return (8192.0, 65535.0);
    }

    lumas.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let high_idx = ((lumas.len() - 1) as f32 * 0.995) as usize;
    let low_idx = ((lumas.len() - 1) as f32 * 0.01) as usize;
    let low = lumas[low_idx];
    let high = lumas[high_idx].max(1024.0);

    let signal_floor = (high * 0.02).max(low + 4.0);
    let first_signal = lumas.partition_point(|v| *v < signal_floor);
    let signal = if lumas.len().saturating_sub(first_signal) >= 16 {
        &lumas[first_signal..]
    } else {
        &lumas[..]
    };

    let mut pivot = signal[signal.len() / 2];
    let tone_white = high.max(pivot * 2.0).clamp(1024.0, 65535.0);
    pivot = pivot
        .clamp(tone_white * 0.05, tone_white * 0.65)
        .clamp(256.0, 49152.0);

    (pivot, tone_white)
}

/// Techo real de brillo de la imagen (percentil 99). De el salen dos cosas
/// distintas que NO son intercambiables: `img_scale` (con clamp, escala los
/// limites internos) y la normalizacion de la referencia del USM adaptativo, que
/// usa el p99 crudo. El clamp de `img_scale` no es invertible, asi que hay que
/// conservar el p99.
fn measure_img_p99(data: &[u16]) -> f32 {
    let mut vals: Vec<u16> = data.to_vec();
    let idx = (vals.len() * 99 / 100).min(vals.len().saturating_sub(1));
    if idx < vals.len() {
        vals.select_nth_unstable(idx);
        (vals[idx] as f32).max(1000.0) // minimo 1000 para no sobre-sensibilizar
    } else {
        65535.0
    }
}

/// 1.0 = 16 bits llenos, ~0.06 = equivalente a 8 bits.
#[inline]
fn img_scale_from_p99(img_p99: f32) -> f32 {
    (img_p99 / 65535.0).clamp(0.05, 1.0)
}

/// Radio de la PSF derivado del sigma de deconvolucion. Solo 7 valores posibles
/// (3..=9), lo que permite cachear la PSF medida por radio.
#[inline]
fn psf_radius_for_sigma(deconv_sigma: f32) -> usize {
    psf_radius_for_sigma_with(deconv_sigma, ProtectionProfile::PROTECTED)
}

/// El tope depende del Modo Pureza (9 protegido, hasta 21 puro). Debe salir de
/// AQUI en todos los sitios: la cache de `GlobalStats` indexa la PSF medida por
/// radio, y calcularlo de otra forma en algun punto haria que el kernel dejara
/// de casar y la PSF del limbo se apagara sola.
#[inline]
fn psf_radius_for_sigma_with(deconv_sigma: f32, protection: ProtectionProfile) -> usize {
    (deconv_sigma * 2.0)
        .ceil()
        .clamp(3.0, protection.psf_radius_cap()) as usize
}

/// Mide la PSF real (edge-spread) del borde disco/cielo. Achromatica: se mide
/// una vez de la luminancia y se aplica a los 3 canales. Devuelve tambien el
/// motivo, para que quien tenga `AppHandle` lo registre.
fn measure_limb_psf(
    data: &[u16],
    width: usize,
    height: usize,
    psf_radius: usize,
) -> (Option<Vec<f32>>, &'static str) {
    let size = width * height;
    if data.len() < size * 3 {
        return (None, "Deconvolucion: buffer insuficiente → Gaussiana parametrica.");
    }
    let luma: Vec<f32> = (0..size)
        .map(|i| {
            0.299 * data[i * 3] as f32 + 0.587 * data[i * 3 + 1] as f32 + 0.114 * data[i * 3 + 2] as f32
        })
        .collect();
    let planet_mask = compute_planet_mask(&luma, width, height, 4);
    let limb_mask = compute_limb_mask(&planet_mask, width, height, psf_radius.max(4));
    let has_limb = limb_mask.iter().filter(|&&m| m > 0.5).count() > psf_radius * psf_radius * 8;
    if !has_limb {
        return (
            None,
            "Deconvolucion: sin limbo claro (disco lleno) → Gaussiana parametrica.",
        );
    }
    let est = PsfEstimator { psf_radius }.estimate_from_limb(&luma, width, height, &limb_mask, 0.85);
    if psf_is_valid(&est, psf_radius) {
        (
            Some(est),
            "Deconvolucion: PSF medida del limbo (edge-spread) — activa.",
        )
    } else {
        (
            None,
            "Deconvolucion: PSF del limbo no fiable → Gaussiana parametrica.",
        )
    }
}

/// Estadisticas que describen la IMAGEN COMPLETA, no el buffer que se esta
/// procesando.
///
/// Bajo `preview_roi` el pipeline trabaja sobre un recorte. Recalcular estas
/// medidas ahi romperia dos cosas a la vez:
///   1. MOVER el recuadro cambiaria el resultado (p99 y pivote tonal dependen
///      del contenido visible), asi que el recuadro seria nitido pero no
///      representativo del render final.
///   2. La PSF del limbo se DESACTIVARIA sola en cuanto el recuadro no
///      incluyera el borde del disco — justo el caso normal al inspeccionar
///      detalle de superficie.
///
/// Se miden una vez sobre el master completo y se cachean por generacion, lo que
/// ademas evita recalcular p99 y la PSF en cada arrastre de slider.
#[derive(Clone)]
struct GlobalStats {
    /// Dimensiones del MASTER. Algunos radios se derivan del tamaño de la imagen
    /// (el sigma del LCE es `max(w,h)·0.02` con tope 30): calcularlos del recorte
    /// daria un radio distinto y el recuadro mentiria sobre ese filtro.
    master_width: usize,
    master_height: usize,
    /// Percentil 99 crudo. Normaliza la referencia del USM adaptativo.
    img_p99: f32,
    /// `img_p99` con clamp a [0.05, 1.0]. Escala coring, high-pass, USM y
    /// deringing.
    img_scale: f32,
    color_pivot: f32,
    tone_white: f32,
    /// PSF medida del limbo y el radio con el que se midio. `None` = Gaussiana
    /// parametrica. El radio se guarda para invalidar la cache si cambia el
    /// sigma de deconvolucion.
    psf: Option<(Vec<f32>, usize)>,
    /// Radio con el que se intento medir la PSF (aunque saliera `None`), para
    /// poder distinguir "no medida" de "medida y descartada".
    psf_radius: usize,
    /// `true` si la medida de PSF se pidio de verdad; si es `false` la cache no
    /// sirve para una receta que si la pida.
    psf_requested: bool,
}

impl GlobalStats {
    /// Mide sobre el master COMPLETO. `psf_from_limb`/`deconv_sigma` solo
    /// intervienen en la PSF; el resto de campos dependen unicamente de la
    /// imagen.
    fn measure(
        data: &[u16],
        width: usize,
        height: usize,
        psf_from_limb: bool,
        deconv_sigma: f32,
    ) -> (Self, Option<&'static str>) {
        let img_p99 = measure_img_p99(data);
        let img_scale = img_scale_from_p99(img_p99);
        let (color_pivot, tone_white) = estimate_color_adjust_context(data);
        let psf_radius = psf_radius_for_sigma(deconv_sigma);
        let (psf_kernel, note) = if psf_from_limb {
            let (k, n) = measure_limb_psf(data, width, height, psf_radius);
            (k, Some(n))
        } else {
            (None, None)
        };
        (
            Self {
                master_width: width,
                master_height: height,
                img_p99,
                img_scale,
                color_pivot,
                tone_white,
                psf: psf_kernel.map(|k| (k, psf_radius)),
                psf_radius,
                psf_requested: psf_from_limb,
            },
            note,
        )
    }

    /// La cache solo vale si la PSF se midio con el mismo radio y con la misma
    /// intencion (pedida o no).
    fn matches_psf_request(&self, psf_from_limb: bool, deconv_sigma: f32) -> bool {
        if !psf_from_limb {
            return true; // sin PSF medida, el resto de campos no dependen de la receta
        }
        self.psf_requested && self.psf_radius == psf_radius_for_sigma(deconv_sigma)
    }
}

/// Interpolacion entre la constante Protegida y la Pura.
///
/// Los EXTREMOS se devuelven tal cual, sin pasar por la aritmetica: en f32,
/// `1.0 + (0.001 - 1.0)·1.0` da 0.0009999871, no 0.001. Ese error diminuto
/// bastaria para que el modo Protegido dejara de ser bit-identico al historico,
/// que es la garantia de toda esta fase.
#[inline]
fn plerp(protected: f32, pure: f32, p: f32) -> f32 {
    if p >= 1.0 {
        return protected;
    }
    if p <= 0.0 {
        return pure;
    }
    pure + (protected - pure) * p
}

/// MODO PUREZA: fuerza de las protecciones automaticas.
///
/// El pipeline lleva una decena de frenos que recortan el efecto real de los
/// deslizadores para evitar artefactos (ringing en el limbo, gusanos en el
/// fondo, reventado de altas luces). Son legitimos, pero estaban FIJOS y sin
/// documentar: el usuario movia un control y obtenia una fraccion de lo que
/// pedia sin saber por que.
///
/// Aqui pasan de condicionales fijos a interpolacion por un unico escalar:
///   `p = 1.0` Protegido   — bit-identico al comportamiento historico
///   `p = 0.5` Equilibrado — punto medio
///   `p = 0.0` Puro        — el deslizador manda
///
/// `p = 1.0` DEBE ser identidad exacta; lo fija un golden test.
#[derive(Clone, Copy, Debug)]
struct ProtectionProfile {
    p: f32,
}

impl ProtectionProfile {
    /// Historico. Es el valor por defecto en todas las rutas que no lo declaran.
    const PROTECTED: Self = Self { p: 1.0 };

    fn from_mode(mode: Option<&str>) -> Self {
        let p = match mode.unwrap_or("protected") {
            "pure" => 0.0,
            "balanced" => 0.5,
            _ => 1.0,
        };
        Self { p }
    }

    #[inline]
    fn is_protected(self) -> bool {
        self.p >= 1.0
    }

    /// Altas luces: umbral a partir del cual se cancela la restauracion. En
    /// Protegido arranca en 32000 ADU con curva de 6ª potencia (solo muerde de
    /// verdad por encima de ~60000). En Puro sube a 60000, asi que el disco
    /// conserva el realce.
    #[inline]
    fn highlight_start(self) -> f32 {
        plerp(32000.0, 60000.0, self.p)
    }

    /// Suelo de la proteccion de altas luces. En Protegido llega a anular el
    /// efecto (0.001); en Puro no atenua nada (1.0).
    #[inline]
    fn highlight_floor(self) -> f32 {
        plerp(0.001, 1.0, self.p)
    }

    /// Suelo de la guarda cromatica.
    #[inline]
    fn chroma_floor(self) -> f32 {
        plerp(0.1, 1.0, self.p)
    }

    /// Richardson-Lucy: acotado del ratio por iteracion.
    #[inline]
    fn rl_ratio_bounds(self) -> (f32, f32) {
        (plerp(0.5, 0.1, self.p), plerp(2.0, 10.0, self.p))
    }

    /// Richardson-Lucy: paso maximo por pixel y por iteracion.
    #[inline]
    fn rl_max_step(self, original: f32) -> f32 {
        let coef = plerp(0.22, 1.0, self.p);
        let ceiling = plerp(8_000.0, 30_000.0, self.p);
        (original.abs() * coef + 96.0).clamp(96.0, ceiling)
    }

    /// Richardson-Lucy: peso de la correccion.
    #[inline]
    fn rl_correction_weight(self) -> f32 {
        plerp(0.78, 1.0, self.p)
    }

    /// Van Cittert: amortiguacion del residuo. Es la deconvolucion mas propensa a
    /// oscilar, asi que en Puro se queda en 0.90 y no en 1.0.
    #[inline]
    fn vc_dampening(self) -> f32 {
        plerp(0.38, 0.90, self.p)
    }

    /// Van Cittert: peso de la correccion.
    #[inline]
    fn vc_correction_weight(self) -> f32 {
        plerp(0.50, 1.0, self.p)
    }

    /// Tope del radio de la PSF medida. Subirlo permite deconvolucionar
    /// estructuras grandes, a cambio de ~5x de CPU.
    #[inline]
    fn psf_radius_cap(self) -> f32 {
        plerp(9.0, 21.0, self.p)
    }

    /// Rodilla del soft-clip final.
    #[inline]
    fn soft_clip_knee(self) -> f32 {
        plerp(58_000.0, 65_000.0, self.p)
    }

    /// Suelo del denoise maestro: en Protegido el deslizador nunca baja del 30 %
    /// de filtrado. Es sobre-suavizado forzado, asi que relajarlo RECUPERA
    /// detalle sin ningun artefacto a cambio.
    #[inline]
    fn denoise_blend(self, amount: f32) -> f32 {
        plerp(0.30 + amount * 0.70, amount, self.p).clamp(0.0, 1.0)
    }

    /// Techo a partir del cual USM y high-pass comprimen el exceso.
    #[inline]
    fn sharpen_limit(self, base: f32) -> f32 {
        base * (1.0 + 3.0 * (1.0 - self.p))
    }

    /// Exponente de la compresion del exceso. En Protegido es raiz cuadrada
    /// (duplicar el deslizador casi no cambia nada pasado el codo); en Puro es
    /// lineal.
    #[inline]
    fn sharpen_knee_exponent(self) -> f32 {
        plerp(0.5, 1.0, self.p)
    }

    /// Umbral bajo el cual el USM atenua cuadraticamente el detalle fino.
    #[inline]
    fn usm_fine_threshold(self, base: f32) -> f32 {
        base * self.p
    }

    // === MODULO AVANZADO (tono, color, textura/claridad) ===

    /// Textura y Claridad se APAGAN por encima de este nivel de luminancia. En
    /// Protegido empieza a cerrarse en 0.88, que en un disco lunar o solar
    /// brillante es casi todo el encuadre: el usuario mueve el control y no pasa
    /// nada en el objeto, solo en el fondo.
    #[inline]
    fn local_contrast_highlight_gate(self) -> (f32, f32) {
        (plerp(0.88, 0.995, self.p), plerp(0.995, 1.0, self.p))
    }

    /// Suelo de Textura/Claridad en zonas SIN estructura medida. En Protegido el
    /// efecto cae al 12 % donde el detector no ve detalle.
    #[inline]
    fn local_contrast_flat_floor(self) -> f32 {
        plerp(0.12, 1.0, self.p)
    }

    /// Confianza cromatica efectiva. Vibrance, tono HSL, saturacion y luminancia
    /// HSL se multiplican por ella: sobre datos casi neutros (planetaria mono-ish)
    /// vale ~0 y esos controles NO HACEN NADA.
    ///
    /// Es defendible como ciencia — no se puede saturar lo que no tiene color sin
    /// inventarlo — pero es exactamente el tipo de freno silencioso que hace que un
    /// deslizador parezca roto. En Puro se toma como 1 y el control manda.
    #[inline]
    fn effective_chroma_confidence(self, measured: f32) -> f32 {
        plerp(measured, 1.0, self.p).clamp(0.0, 1.0)
    }

    /// Tope duro del realce local (LCE) en ADU.
    #[inline]
    fn lce_limit(self, base: f32) -> f32 {
        base * (1.0 + 3.0 * (1.0 - self.p))
    }
}

/// Sigmas nominales del banco de wavelets, en pixeles de la imagen procesada.
const WAVELET_BAND_SIGMAS: [f32; 6] = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0];

/// Guarda (en px) que necesita un recuadro para reproducir EXACTAMENTE el render
/// completo en su interior.
///
/// Medido en `test_roi_guard_band_requirement_scales_with_amplified_band`: la
/// descomposicion wavelet es una PARTICION (las bandas suman la original), asi
/// que las bandas con ganancia neutra se cancelan al recombinar y su error de
/// borde nunca llega a la salida. La guarda NO la fija el sigma mayor del banco
/// (32) sino 3σ de la banda mas gruesa REALMENTE activa. En una sesion tipica de
/// detalle fino son 16 px en vez de 96 → el recuadro cuesta ×1.13 en vez de ×1.89.
fn roi_guard_px(
    u_amts: &[f32; 5],
    w_amts: &[f32; 6],
    d_amts: &[f32; 6],
    lce_amount: f32,
    lce_scale_w: usize,
    lce_scale_h: usize,
    deconv_sigma: f32,
    deconv_iter: usize,
    vc_sigma: f32,
    vc_iter: usize,
    usm_radius: f32,
    usm_amount: f32,
    crisp: f32,
    deringing_radius: f32,
    deringing_mode: i32,
) -> usize {
    let active = |v: f32| v.abs() > 1e-6;
    let mut sigma_max = 0.0f32;

    for band in 0..6 {
        let touched = active(w_amts[band])
            || active(d_amts[band])
            || (band < 5 && active(u_amts[band]));
        if touched {
            sigma_max = sigma_max.max(WAVELET_BAND_SIGMAS[band]);
        }
    }

    // El LCE usa un Gaussiano cuyo sigma sale del tamaño del MASTER.
    if active(lce_amount) {
        let lce_sigma = (lce_scale_w.max(lce_scale_h) as f32 * 0.02).clamp(5.0, 30.0);
        sigma_max = sigma_max.max(lce_sigma);
    }
    // La deconvolucion difunde por su PSF en cada iteracion.
    if deconv_iter > 0 {
        sigma_max = sigma_max.max(deconv_sigma.max(psf_radius_for_sigma(deconv_sigma) as f32));
    }
    if vc_iter > 0 {
        sigma_max = sigma_max.max(vc_sigma);
    }
    if active(usm_amount) {
        sigma_max = sigma_max.max(usm_radius);
    }
    // El high-pass ("crisp") usa un blur fijo de sigma 3.
    if active(crisp) {
        sigma_max = sigma_max.max(3.0);
    }
    if deringing_mode > 0 && active(deringing_radius) {
        sigma_max = sigma_max.max(deringing_radius);
    }

    // 3σ cubre el soporte del box blur de 3 pasadas; +8 px de colchon para los
    // filtros de vecindad pequeña (bilateral, mediana del deringing).
    ((sigma_max * 3.0).ceil() as usize + 8).min(256)
}

#[inline]
fn blend_restoration(base: f32, restored: f32, amount: f32, protection: f32) -> f32 {
    base + (restored - base) * amount.clamp(0.0, 1.0) * protection.clamp(0.0, 1.0)
}

fn run_processing_pipeline(
    app: &tauri::AppHandle,
    state: &State<'_, AppState>,
    req_id: usize,
    original: &StackResult,
    width: usize,
    height: usize,
    u_amts: [f32; 5],
    w_amts: [f32; 6],
    d_amts: [f32; 6],
    gamma: f32,
    saturation: f32,
    r_x: f32,
    r_y: f32,
    b_x: f32,
    b_y: f32,
    deringing_mode: i32,
    deringing_radius: f32,
    deringing_dark: f32,
    deringing_light: f32,
    deringing_mask: bool,
    crisp: f32,
    deconv_iter: usize,
    deconv_sigma: f32,
    vc_iter: usize,
    vc_sigma: f32,
    usm_amount: f32,
    usm_radius: f32,
    adaptive_usm: AdaptiveUsmParams,
    lce_amount: f32,
    blend: f32,
    contrast: f32,
    brightness: f32,
    r_bal: f32,
    b_bal: f32,
    master_denoise: f32,      // PHASE 23
    master_denoise_detail: f32,
    master_denoise_chroma: f32,
    use_rgb_sharpening: bool, // PHASE 15: New Toggle
    edge_aware_wavelets: bool, // B: descomposicion wavelet edge-aware (anti-ringing limbo)
    psf_from_limb: bool,       // A: deconvolucion con PSF medida del limbo
    edge_aware_strength: f32,  // B+: intensidad edge-aware (0..100, 50 = historico ×4)
    auto_mask: f32,            // Calidad: sharpening adaptativo por SNR local (0..100, 0 = off)
    gpu_allowed: bool,         // Velocidad: intentar descomposicion wavelet en GPU (paridad+fallback)
    levels_black: f32,         // Niveles: punto negro de entrada (0..1, 0 = neutro)
    levels_white: f32,         // Niveles: punto blanco de entrada (0..1, 1 = neutro)
    levels_gamma: f32,         // Niveles: gamma de medios tonos (0.1..5, 1 = neutro)
    // Estadisticas medidas sobre el MASTER COMPLETO. `None` = medir aqui sobre
    // `original` (rutas de exportacion/lote, donde `original` YA es la imagen
    // entera → comportamiento historico bit-identico). `Some` lo usa el recuadro
    // interactivo, que procesa un recorte y no puede medirlas de el.
    global_stats: Option<&GlobalStats>,
    // Fuerza de las protecciones automaticas. `None` = Protegido (historico).
    protection: ProtectionProfile,
) -> Vec<u16> {
    let size = width * height;

    // A neutral recipe is an exact identity contract. Besides avoiding an
    // expensive wavelet decomposition, this prevents the final soft-clipping
    // stage and RGB↔YUV round-trip from altering a master when the user resets,
    // undoes back to Original, or starts a new stack.
    let near_zero = |value: f32| value.abs() <= 1e-6;
    let neutral_recipe = u_amts.iter().all(|value| near_zero(*value))
        && w_amts.iter().all(|value| near_zero(*value))
        && d_amts.iter().all(|value| near_zero(*value))
        && (gamma - 1.0).abs() <= 1e-6
        && (saturation - 1.0).abs() <= 1e-6
        && [r_x, r_y, b_x, b_y].iter().all(|value| near_zero(*value))
        && deringing_mode == 0
        && near_zero(crisp)
        && deconv_iter == 0
        && vc_iter == 0
        && near_zero(usm_amount)
        && near_zero(lce_amount)
        && (contrast - 1.0).abs() <= 1e-6
        && near_zero(brightness)
        && near_zero(r_bal)
        && near_zero(b_bal)
        && near_zero(master_denoise);
    if neutral_recipe {
        if check_cancel(state, req_id) {
            return Vec::new();
        }
        emit_progress(app, "Receta neutra · master 16-bit", 100.0, None);
        return original.data.clone();
    }

    // === NORMALIZATION: Detect actual image signal range ===
    // This is the core fix for bit-depth artifacts. Instead of using hardcoded
    // absolute delta limits (which blow up on low-signal images), we compute the
    // actual brightness ceiling of the image (p99) and scale all internal limits
    // proportionally. This is how Lightroom/PixInsight avoid artifacts.
    //
    // Con recuadro interactivo llega medido sobre el MASTER COMPLETO: medirlo del
    // recorte haria que mover el recuadro cambiase los umbrales internos.
    let (img_p99, img_scale) = match global_stats {
        Some(g) => (g.img_p99, g.img_scale),
        None => {
            let p99 = measure_img_p99(&original.data);
            (p99, img_scale_from_p99(p99))
        }
    };

    let d_params = DeconvParams {
        sigma: deconv_sigma,
        iter: deconv_iter,
        vc_sigma,
        vc_iter,
        psf_from_limb,
    };

    if check_cancel(state, req_id) {
        return Vec::new();
    }

    // Force Luminance mode if image is Mono
    let effective_rgb_mode = use_rgb_sharpening && !original.is_mono;

    // CACHe DE DECONVOLUCIoN
    let mut cached_d: Option<DeconvCache> = None;
    let mut d_changed = false;

    {
        let mut guard = state.deconv_cache.lock().unwrap_or_else(|e| e.into_inner());
        let mut match_idx = None;
        for (i, c) in guard.iter().enumerate() {
            if c.params == d_params && c.width == width && c.height == height {
                let req_chans = if effective_rgb_mode { 3 } else { 1 };
                if c.channels.len() == req_chans {
                    match_idx = Some(i);
                    break;
                }
            }
        }
        if let Some(idx) = match_idx {
            let hit = guard.remove(idx);
            cached_d = Some(hit.clone());
            guard.insert(0, hit); // Move to front (LRU)
        }
    }

    let base_channels = if let Some(c) = cached_d {
        emit_progress(app, "Deconvolucion (Cache)", 35.0, None);
        c.channels
    } else {
        d_changed = true;
        let mut work_channels = Vec::new();

        if effective_rgb_mode {
            // Extract RGB channels
            let mut r = vec![0.0; size];
            let mut g = vec![0.0; size];
            let mut b = vec![0.0; size];
            for i in 0..size {
                r[i] = original.data[i * 3] as f32;
                g[i] = original.data[i * 3 + 1] as f32;
                b[i] = original.data[i * 3 + 2] as f32;
            }
            work_channels.push(r);
            work_channels.push(g);
            work_channels.push(b);
        } else {
            // Extract Luminance (or use mono data)
            let mut y = vec![0.0; size];
            for i in 0..size {
                let (cy, _, _) = rgb_to_yuv(
                    original.data[i * 3] as f32,
                    original.data[i * 3 + 1] as f32,
                    original.data[i * 3 + 2] as f32,
                );
                y[i] = cy;
            }
            work_channels.push(y);
        }

        if d_params.iter > 0 || d_params.vc_iter > 0 {
            emit_progress(app, "Iniciando Deconvolucion...", 0.0, None);

            // PSF MEDIDA DEL LIMBO (opcion A): mide la PSF real (edge-spread)
            // del borde disco/cielo y la usa en la RL en vez de la Gaussiana
            // parametrica. Achromatica → se mide una vez de la luminancia y se
            // aplica a los 3 canales. Fallback a Gaussiana si no hay un limbo
            // fiable (disco lleno sin cielo, PSF no valida): psf_measured=None.
            let psf_radius = psf_radius_for_sigma_with(deconv_sigma, protection);
            // CRITICO para el recuadro: la PSF se mide del borde disco/cielo, que
            // normalmente queda FUERA de un recuadro centrado en la superficie. Si
            // se midiera del recorte, `has_limb` seria falso y la deconvolucion
            // caeria sola a Gaussiana parametrica → el recuadro mostraria un
            // resultado distinto del render final. Por eso llega medida del master.
            let psf_measured: Option<Vec<f32>> = if psf_from_limb && deconv_iter > 0 {
                match global_stats {
                    // El filtro por radio no es cosmetico: `psf_ref` pasa
                    // `(slice, psf_radius)` y el kernel DEBE medir (2r+1)². Una
                    // cache con otro radio degrada a Gaussiana parametrica, que es
                    // seguro, en vez de entregar un kernel del tamaño equivocado.
                    Some(g) => g
                        .psf
                        .as_ref()
                        .filter(|(_, radius)| *radius == psf_radius)
                        .map(|(kernel, _)| kernel.clone()),
                    None => {
                        let (est, note) =
                            measure_limb_psf(&original.data, width, height, psf_radius);
                        log_to_front(app, "INFO", note);
                        est
                    }
                }
            } else {
                None
            };
            let psf_ref: Option<(&[f32], usize)> =
                psf_measured.as_ref().map(|p| (p.as_slice(), psf_radius));

            let mut processed = Vec::new();

            for (ch_idx, ch) in work_channels.iter().enumerate() {
                if check_cancel(state, req_id) {
                    return Vec::new();
                }

                // GPU RL: solo PSF Gaussiana (psf_ref None), imagen grande y
                // gpu_allowed. Con paridad + fallback: si algo falla → CPU. Con
                // PSF medida (opción A) o imagen pequeña → CPU siempre.
                let dr = if gpu_allowed
                    && psf_ref.is_none()
                    && deconv_iter > 0
                    && width * height >= 500_000
                {
                    crate::gpu_wavelet::gpu_richardson_lucy(
                        ch, ch, width, height, deconv_iter, deconv_sigma,
                    )
                    .unwrap_or_else(|| {
                        apply_richardson_lucy(
                            app, state, req_id, ch, ch, width, height, deconv_iter, deconv_sigma,
                            (0.0, 100.0), psf_ref, protection,
                        )
                    })
                } else {
                    apply_richardson_lucy(
                        app, state, req_id, ch, ch, width, height, deconv_iter, deconv_sigma,
                        (0.0, 100.0), psf_ref, protection,
                    )
                };
                // CANCELACIÓN INTERNA: RL/VC devuelven un Vec VACÍO al cancelar
                // (unwrap_or_default sobre None). Antes ese canal vacío seguía
                // adelante y se guardaba en el caché LRU: el siguiente render
                // con los mismos parámetros lo servía del caché y reventaba en
                // box_blur_parallel ("range start index … slice of length 0").
                if dr.len() != size {
                    return Vec::new();
                }
                let vcr = apply_van_cittert(
                    app,
                    state,
                    req_id,
                    &dr,
                    ch,
                    width,
                    height,
                    vc_iter,
                    vc_sigma,
                    (0.0, 100.0),
                    protection,
                );
                if vcr.len() != size {
                    return Vec::new();
                }
                processed.push(vcr);

                let pct = (ch_idx + 1) as f32 / work_channels.len() as f32 * 35.0;
                emit_progress(app, "Deconvolucion...", pct, None);
            }
            work_channels = processed;
        }

        // Nunca cachear canales incompletos (cancelación u otro aborto): un
        // caché envenenado reproduce el fallo en cada render posterior.
        if work_channels.iter().any(|c| c.len() != size) {
            return Vec::new();
        }
        let nc = DeconvCache {
            channels: Arc::new(work_channels),
            params: d_params.clone(),
            width,
            height,
        };
        let result_channels = nc.channels.clone();
        if !commit_processing_cache_if_current(state, req_id, || {
            let mut guard = state.deconv_cache.lock().unwrap_or_else(|e| e.into_inner());
            guard.insert(0, nc);
            // F3: tope 8 (antes 3): comparar >3 estados A/B/C/D expulsaba
            // la entrada y forzaba recomputar el tramo más caro (deconv/wavelets).
            if guard.len() > 8 {
                guard.pop();
            }
        }) {
            return Vec::new();
        }
        result_channels
    };

    // We also need original chrominance for reconstructing if in Luminance mode
    let mut base_u = vec![0.0; size];
    let mut base_v = vec![0.0; size];
    if !effective_rgb_mode && !original.is_mono {
        for i in 0..size {
            let (_, u, v) = rgb_to_yuv(
                original.data[i * 3] as f32,
                original.data[i * 3 + 1] as f32,
                original.data[i * 3 + 2] as f32,
            );
            base_u[i] = u;
            base_v[i] = v;
        }
    }

    if check_cancel(state, req_id) {
        return Vec::new();
    }

    // PHASE 42: Extract CLEAN reference (Pre-Deconvolution)
    // This is essential for deringing. If we use deconvolved images as reference,
    // we are deringing against an image that already HAS ringing.
    let mut clean_channels = Vec::new();
    if effective_rgb_mode {
        let mut r = vec![0.0; size];
        let mut g = vec![0.0; size];
        let mut b = vec![0.0; size];
        for i in 0..size {
            r[i] = original.data[i * 3] as f32;
            g[i] = original.data[i * 3 + 1] as f32;
            b[i] = original.data[i * 3 + 2] as f32;
        }
        clean_channels.push(r);
        clean_channels.push(g);
        clean_channels.push(b);
    } else {
        let mut y = vec![0.0; size];
        for i in 0..size {
            let (cy, _, _) = rgb_to_yuv(
                original.data[i * 3] as f32,
                original.data[i * 3 + 1] as f32,
                original.data[i * 3 + 2] as f32,
            );
            y[i] = cy;
        }
        clean_channels.push(y);
    }

    // CACHe DE WAVELETS
    let mut w_cache: Option<WaveletLayers> = None;
    if !d_changed {
        let mut guard = state.wavelet_cache.lock().unwrap_or_else(|e| e.into_inner());
        let mut match_idx = None;
        for (i, w) in guard.iter().enumerate() {
            if w.width == width
                && w.height == height
                && w.parent_deconv_params == d_params
                && w.edge_aware == edge_aware_wavelets
                && (!edge_aware_wavelets || w.edge_aware_strength == edge_aware_strength)
            {
                let req_chans = if effective_rgb_mode { 3 } else { 1 };
                if w.channels.len() == req_chans {
                    match_idx = Some(i);
                    break;
                }
            }
        }
        if let Some(idx) = match_idx {
            let hit = guard.remove(idx);
            w_cache = Some(hit.clone());
            guard.insert(0, hit);
        }
    }

    let layers = if let Some(l) = w_cache {
        emit_progress(app, "Wavelets (Cache)", 60.0, None);
        l
    } else {
        // EDGE-AWARE POR CAPA (opcion B, extendida): la "Intensidad Edge-Aware"
        // controla CUANTAS bandas finas usan blur BILATERAL (2 por defecto, 3
        // con intensidad alta → incluye σ4) y cuan estrecho es el kernel de
        // rango (`range_mult`, menor = borde MAS protegido). El salto del limbo
        // queda en la base, NO en las bandas de detalle, asi que amplificarlas
        // no genera ringing/gusanos. Las escalas gruesas siguen Gaussianas (ahi
        // el borde ya esta muy difuminado). Coste acotado: radio bilateral ≤ 9.
        // Intensidad 50 = comportamiento historico exacto (2 bandas, mult ×4).
        let ea_strength = edge_aware_strength.clamp(0.0, 100.0);
        let n_bilateral = if edge_aware_wavelets {
            if ea_strength >= 66.0 {
                3
            } else {
                2
            }
        } else {
            0
        };
        let range_mult = (6.0 - 4.0 * (ea_strength / 100.0)).clamp(1.5, 6.0);
        // GPU: la descomposición Gaussiana PURA (sin edge-aware bilateral) puede
        // correr en GPU en imágenes grandes. Con paridad obligatoria + fallback:
        // si no hay GPU / la paridad no cuadra / cualquier error → CPU idéntica.
        let use_gpu_decompose = gpu_allowed && n_bilateral == 0 && width * height >= 500_000;
        let decompose = |base: &[f32]| -> Vec<Vec<f32>> {
            if use_gpu_decompose {
                if let Some(ls) = crate::gpu_wavelet::gpu_decompose(base, width, height) {
                    return ls;
                }
            }
            let mut ls = Vec::new();
            let sigmas = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0];
            let range_sigma = if n_bilateral > 0 {
                estimate_bilateral_range(base, width, height, range_mult)
            } else {
                0.0
            };
            let mut blurs = Vec::new();
            for (idx, &s) in sigmas.iter().enumerate() {
                if idx < n_bilateral {
                    blurs.push(apply_bilateral_blur(base, width, height, s, range_sigma));
                } else {
                    blurs.push(apply_gaussian_blur(base, width, height, s));
                }
            }
            ls.push(base.iter().zip(&blurs[0]).map(|(a, b)| a - b).collect());
            for i in 0..5 {
                ls.push(
                    blurs[i]
                        .iter()
                        .zip(&blurs[i + 1])
                        .map(|(a, b)| a - b)
                        .collect(),
                );
            }
            ls.push(blurs[5].clone());
            ls
        };

        let mut multi_layers = Vec::new();
        for (i, ch) in base_channels.iter().enumerate() {
            if check_cancel(state, req_id) {
                return Vec::new();
            }
            multi_layers.push(decompose(ch));
            let pct = 35.0 + (i + 1) as f32 / base_channels.len() as f32 * 25.0;
            emit_progress(app, "Descomponiendo Wavelets...", pct, None);
        }

        let wc = WaveletLayers {
            channels: Arc::new(multi_layers),
            width,
            height,
            parent_deconv_params: d_params.clone(),
            edge_aware: edge_aware_wavelets,
            edge_aware_strength,
        };
        if !commit_processing_cache_if_current(state, req_id, || {
            let mut guard = state.wavelet_cache.lock().unwrap_or_else(|e| e.into_inner());
            guard.insert(0, wc.clone());
            // F3: tope 8 (antes 3): comparar >3 estados A/B/C/D expulsaba
            // la entrada y forzaba recomputar el tramo más caro (deconv/wavelets).
            if guard.len() > 8 {
                guard.pop();
            }
        }) {
            return Vec::new();
        }
        wc
    };

    if check_cancel(state, req_id) {
        return Vec::new();
    }

    let f_params = FilterParams {
        u_amts,
        w_amts,
        d_amts,
        crisp,
        master_denoise,
        master_denoise_detail,
        master_denoise_chroma,
        usm_amount,
        usm_radius,
        adaptive_usm: adaptive_usm.clone(),
        lce_amount,
        deringing_mode,
        deringing_radius: if deringing_mode > 0 { deringing_radius } else { 0.0 },
        deringing_dark: if deringing_mode > 0 { deringing_dark } else { 0.0 },
        deringing_light: if deringing_mode > 0 { deringing_light } else { 0.0 },
        deringing_mask: if deringing_mode > 0 { deringing_mask } else { false },
        deconv_params: d_params.clone(),
        edge_aware: edge_aware_wavelets,
        edge_aware_strength,
        auto_mask,
    };

    let mut cached_filter: Option<FilterCache> = None;
    {
        let mut guard = state.filter_cache.lock().unwrap_or_else(|e| e.into_inner());
        let mut match_idx = None;
        for (i, c) in guard.iter().enumerate() {
            if c.params == f_params && c.width == width && c.height == height {
                let req_chans = if effective_rgb_mode { 3 } else { 1 };
                if c.channels.len() == req_chans {
                    match_idx = Some(i);
                    break;
                }
            }
        }
        if let Some(idx) = match_idx {
            let hit = guard.remove(idx);
            cached_filter = Some(hit.clone());
            guard.insert(0, hit);
        }
    }

    let filtered_channels = if let Some(c) = cached_filter {
        emit_progress(app, "Filtros (Cache)", 75.0, None);
        c.channels
    } else {
        emit_progress(app, "Mezclando y Filtrando...", 70.0, None);
        let mut work_filter = Vec::new();

        // AUTO-MASCARA ADAPTATIVA: fuerza 0..1. Modula SOLO la amplificacion
        // (no la reconstruccion base) de las 3 bandas finas por un mapa de
        // confianza SNR local → el sharpening se aplica donde hay estructura y
        // se atenua sobre ruido plano. `den` es LINEAL en la ganancia `a`, asi
        // que 1.0 + sharpen·conf preserva exactamente la señal base (a=1) y solo
        // escala el termino de realce (sharpen). Anti ruido "wormy".
        let auto_amt = (auto_mask / 100.0).clamp(0.0, 1.0);
        let recombine = |lys: &Vec<Vec<f32>>| -> Vec<f32> {
            let mut out = vec![0.0; size];
            let _ub = u_amts.iter().sum::<f32>() * 0.2;
            // F3: el umbral de coring escala con img_scale (normalización p99),
            // igual que high-pass/USM/deringing. Con el corte FIJO en ADU, el
            // mismo slider cortaba ~16× más detalle relativo en un planeta
            // tenue (img_scale≈0.06) que en la Luna brillante.
            let den = |v: f32, a: f32, t: f32| {
                let cut = t * 100.0 * img_scale;
                if v.abs() < cut {
                    v * (v.abs() / (cut + 0.01)) * a
                } else {
                    v * a
                }
            };
            // Confianza por-pixel (solo si la auto-mascara esta activa). Se mide
            // el ruido de la banda mas fina (MAD Donoho) y la coherencia espacial
            // del detalle; se reutiliza para las 3 bandas finas (la estructura
            // real se alinea entre escalas).
            let conf: Option<Vec<f32>> = if auto_amt > 0.001 {
                let sigma_n = estimate_noise_mad(&lys[0]);
                Some(detail_confidence_map(&lys[0], width, height, sigma_n))
            } else {
                None
            };
            for i in 0..size {
                // m = 1 sobre estructura, →0 sobre ruido (escalado por la fuerza)
                let m = match &conf {
                    Some(c) => 1.0 - auto_amt * (1.0 - c[i]),
                    None => 1.0,
                };
                let mut v = lys[6][i];
                // "Detalles Alta Frecuencia" (U) y "Wavelets Multiescala" (W) son
                // dos controles distintos en la UI que suman a la MISMA ganancia.
                // U llevaba un ×0.5 no documentado: el panel mostraba 5.0 y
                // entregaba 2.5, mientras que W mostraba 10.0 y entregaba 10.0. El
                // numero mentia en una familia y en la otra no. Ahora el valor
                // mostrado ES la ganancia en ambas (migracion de esquemas
                // guardados en `migrateWaveletPresets`, main.js).
                //
                // U cubre solo las bandas 1-5 A PROPOSITO: es el control de las
                // escalas MAS FINAS, y la banda 6 (sigma 32) es la mas gruesa.
                // Las 3 bandas finas (donde vive el ruido) llevan el realce
                // modulado por `m`; las gruesas van intactas.
                v += den(lys[0][i], 1.0 + (w_amts[0] + u_amts[0]) * m, d_amts[0]);
                v += den(lys[1][i], 1.0 + (w_amts[1] + u_amts[1]) * m, d_amts[1]);
                v += den(lys[2][i], 1.0 + (w_amts[2] + u_amts[2]) * m, d_amts[2]);
                v += den(lys[3][i], 1.0 + w_amts[3] + u_amts[3], d_amts[3]);
                v += den(lys[4][i], 1.0 + w_amts[4] + u_amts[4], d_amts[4]);
                v += den(lys[5][i], 1.0 + w_amts[5], d_amts[5]);
                out[i] = v;
            }
            out
        };

        for ch_layers in layers.channels.iter() {
            work_filter.push(recombine(ch_layers));
        }

        if master_denoise > 0.0 {
            apply_master_denoise_channels(
                &mut work_filter,
                width,
                height,
                master_denoise,
                master_denoise_detail,
                master_denoise_chroma,
                effective_rgb_mode,
                protection,
            );
        }

        // ImPPG-style adaptive USM is keyed to the UNPROCESSED input
        // luminance, not to each RGB channel independently. One shared map
        // therefore preserves colour balance when RGB sharpening is enabled
        // and behaves identically for mono data.
        let adaptive_usm_reference = adaptive_usm.enabled.then(|| {
            (0..size)
                .map(|index| {
                    let pixel = index * 3;
                    let luminance = 0.2126 * original.data[pixel] as f32
                        + 0.7152 * original.data[pixel + 1] as f32
                        + 0.0722 * original.data[pixel + 2] as f32;
                    (luminance / img_p99.max(1.0)).clamp(0.0, 1.0)
                })
                .collect::<Vec<f32>>()
        });

        let total_filter_channels = work_filter.len().max(1);
        for (ch_idx, ty) in work_filter.iter_mut().enumerate() {
            if crisp > 0.0 {
                *ty = apply_high_pass(ty, width, height, 3.0, crisp, img_scale, protection);
            }
            if usm_amount > 0.0 {
                *ty = apply_smart_sharpen_bilateral(
                    ty,
                    width,
                    height,
                    usm_radius,
                    usm_amount,
                    img_scale,
                    auto_amt,
                    &adaptive_usm,
                    adaptive_usm_reference.as_deref(),
                    protection,
                );
            }
            if lce_amount > 0.0 {
                // El radio del LCE lo fija el tamaño del MASTER, no el del
                // buffer procesado: en un recorte daria un realce local de otra
                // escala espacial.
                let (lce_scale_w, lce_scale_h) = match global_stats {
                    Some(g) => (g.master_width, g.master_height),
                    None => (width, height),
                };
                *ty = apply_clahe_improved(
                    ty, width, height, lce_scale_w, lce_scale_h, lce_amount, protection,
                );
            }

            let pct = 70.0 + (ch_idx + 1) as f32 / total_filter_channels as f32 * 15.0;
            emit_progress(app, "Filtrando canales...", pct, None);
        }

        if deringing_mode > 0 {
            log_to_front(app, "DEBUG", &format!("Aplicando Deringing - Modo: {}, Rad: {}, Dark: {}, Light: {}, Mask: {}", 
                deringing_mode, deringing_radius, deringing_dark, deringing_light, deringing_mask));

            for (i, ch) in work_filter.iter_mut().enumerate() {
                // Use CLEAN channels as reference for deringing
                let clean_ref = &clean_channels[i];
                apply_advanced_deringing(
                    ch,
                    clean_ref,
                    width,
                    height,
                    deringing_mode,
                    deringing_radius,
                    deringing_dark,
                    deringing_light,
                    deringing_mask,
                    i,
                    img_scale,
                );
            }
        } else {
            log_to_front(app, "DEBUG", "Deringing deshabilitado (Mode 0)");
        }

        let nc = FilterCache {
            channels: Arc::new(work_filter),
            params: f_params,
            width,
            height,
        };
        let result_channels = nc.channels.clone();
        if !commit_processing_cache_if_current(state, req_id, || {
            let mut guard = state.filter_cache.lock().unwrap_or_else(|e| e.into_inner());
            guard.insert(0, nc);
            // F3: tope 8 (antes 3): comparar >3 estados A/B/C/D expulsaba
            // la entrada y forzaba recomputar el tramo más caro (deconv/wavelets).
            if guard.len() > 8 {
                guard.pop();
            }
        }) {
            return Vec::new();
        }
        result_channels
    };

    let (render_base_u, render_base_v) =
        if !effective_rgb_mode && !original.is_mono && master_denoise > 0.0 && master_denoise_chroma > 0.0 {
            apply_chroma_denoise_planes(
                &base_u,
                &base_v,
                width,
                height,
                master_denoise,
                master_denoise_chroma,
            )
        } else {
            (base_u, base_v)
        };

    emit_progress(app, "Ajustando Canales...", 90.0, None);

    // PHASE 28: Robust ADC Implementation
    // We render the final f32 image into planes, shift them, and then convert to u16.

    // Shared Soft-Clipping (Pro style - Reinforced)
    let soft_clip = |v: f32| -> u16 {
        let knee = protection.soft_clip_knee();
        if v <= knee {
            v.clamp(0.0, 65535.0) as u16
        } else {
            let over = v - knee;
            let compressed = knee + over / (1.0 + over / (65535.0 - knee));
            compressed.min(65535.0) as u16
        }
    };

    if check_cancel(state, req_id) {
        return Vec::new();
    }
    emit_progress(app, "Renderizando Planos (ADC)...", 95.0, None);
    // Igual que `img_scale`: con recuadro llega medido del master completo, para
    // que el pivote tonal del motor de color no dependa de la zona visible.
    let (color_pivot, tone_white) = match global_stats {
        Some(g) => (g.color_pivot, g.tone_white),
        None => estimate_color_adjust_context(&original.data),
    };

    let mut r_plane = vec![0.0f32; size];
    let mut g_plane = vec![0.0f32; size];
    let mut b_plane = vec![0.0f32; size];

    // 1. Parallel Render into separate f32 planes
    // Use unsafe for direct memory writing from parallel iterator
    let r_ptr = r_plane.as_mut_ptr() as usize;
    let g_ptr = g_plane.as_mut_ptr() as usize;
    let b_ptr = b_plane.as_mut_ptr() as usize;

    (0..size).into_par_iter().for_each(|i| {
        let (mut r, mut g, mut b);
        if effective_rgb_mode {
            // "Mezcla" covers the complete restoration chain, including
            // deconvolution. Using `base_channels` here made 0% keep the RL
            // result because that buffer is already deconvolved.
            let or = clean_channels[0][i];
            let og = clean_channels[1][i];
            let ob = clean_channels[2][i];

            let max_orig = or.max(og).max(ob);
            let p_start = protection.highlight_start();
            let p_factor = if max_orig > p_start {
                let ov = (max_orig - p_start) / (65535.0 - p_start);
                (1.0 - ov.powi(6)).max(protection.highlight_floor())
            } else {
                1.0
            };

            let avg_orig = (or + og + ob) / 3.0;
            let c_divergence =
                ((or - avg_orig).abs() + (og - avg_orig).abs() + (ob - avg_orig).abs()) / 3.0;
            let c_guard = if c_divergence > 5000.0 {
                let c_ov = (c_divergence - 5000.0) / 25000.0;
                (1.0 - c_ov.powi(4)).max(protection.chroma_floor())
            } else {
                1.0
            };

            let effective_factor = p_factor * c_guard;
            r = blend_restoration(or, filtered_channels[0][i], blend, effective_factor);
            g = blend_restoration(og, filtered_channels[1][i], blend, effective_factor);
            b = blend_restoration(ob, filtered_channels[2][i], blend, effective_factor);
        } else {
            let oy = clean_channels[0][i];
            let p_start = protection.highlight_start();
            let p_factor = if oy > p_start {
                let ov = (oy - p_start) / (65535.0 - p_start);
                (1.0 - ov.powi(6)).max(protection.highlight_floor())
            } else {
                1.0
            };

            let y_enhanced = blend_restoration(oy, filtered_channels[0][i], blend, p_factor);
            let (tr, tg, tb) = yuv_to_rgb(y_enhanced, render_base_u[i], render_base_v[i]);
            r = tr;
            g = tg;
            b = tb;
        }

        apply_advanced_color_magic(
            &mut r, &mut g, &mut b, gamma, saturation, contrast, brightness, r_bal, b_bal,
            color_pivot, tone_white, levels_black, levels_white, levels_gamma,
        );

        unsafe {
            *(r_ptr as *mut f32).add(i) = r;
            *(g_ptr as *mut f32).add(i) = g;
            *(b_ptr as *mut f32).add(i) = b;
        }
    });

    // 2. Apply ADC Shifts to FULL planes
    if r_x.abs() > 0.0 || r_y.abs() > 0.0 {
        r_plane = shift_channel(&r_plane, width, height, r_x, r_y);
    }
    if b_x.abs() > 0.0 || b_y.abs() > 0.0 {
        b_plane = shift_channel(&b_plane, width, height, b_x, b_y);
    }

    // 3. Final Conversion and Interleaving
    emit_progress(app, "ConversiÃ³n Final...", 99.0, None);
    let mut final_u16 = vec![0u16; size * 3];
    final_u16
        .par_chunks_exact_mut(3)
        .enumerate()
        .for_each(|(i, px)| {
            px[0] = soft_clip(r_plane[i]);
            px[1] = soft_clip(g_plane[i]);
            px[2] = soft_clip(b_plane[i]);
        });

    final_u16
}
#[cfg(test)]
mod edge_aware_tests {
    use super::*;

    /// El bilateral (opcion B) debe PRESERVAR un borde de alto contraste
    /// (limbo): a un lado del salto el valor apenas cambia, mientras que un
    /// Gaussiano del mismo sigma lo emborrona. Asi el detalle `base-bilateral`
    /// NO contiene el salto → amplificar bandas finas no genera ringing.
    #[test]
    fn test_bilateral_preserves_edge_vs_gaussian() {
        let (w, h) = (64usize, 64usize);
        // Escalon: mitad izquierda 5000, mitad derecha 55000 (limbo simulado)
        // + textura fina para que el sigma_range no colapse a cero.
        let mut img = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let base = if x < w / 2 { 5000.0 } else { 55000.0 };
                let tex = if (x + y) % 2 == 0 { 120.0 } else { -120.0 };
                img[y * w + x] = base + tex;
            }
        }
        let range = estimate_bilateral_range(&img, w, h, 4.0);
        let bil = apply_bilateral_blur(&img, w, h, 2.0, range);
        let gauss = apply_gaussian_blur(&img, w, h, 2.0);

        // Columna justo a la IZQUIERDA del borde (x = w/2 - 1): el Gaussiano
        // arrastra el 55000 de la derecha (se dispara); el bilateral no.
        let xe = w / 2 - 1;
        let mut bil_dev = 0.0f32;
        let mut gauss_dev = 0.0f32;
        for y in 8..h - 8 {
            bil_dev += (bil[y * w + xe] - 5000.0).abs();
            gauss_dev += (gauss[y * w + xe] - 5000.0).abs();
        }
        eprintln!("borde: desvio bilateral {bil_dev:.0} vs gaussiano {gauss_dev:.0}");
        // El bilateral debe desviarse MUCHO menos del nivel izquierdo real.
        assert!(
            bil_dev < gauss_dev * 0.5,
            "bilateral no preservo el borde: {bil_dev} vs gauss {gauss_dev}"
        );
    }

    /// La convolucion con kernel (opcion A) con un kernel identidad (delta en
    /// el centro) devuelve la imagen intacta — garantiza que el forward-model
    /// de la RL con PSF medida es correcto en el caso base.
    #[test]
    fn test_convolve_identity_kernel() {
        let (w, h) = (16usize, 16usize);
        let img: Vec<f32> = (0..w * h).map(|i| (i * 7 % 1000) as f32).collect();
        let r = 2usize;
        let ksize = 2 * r + 1;
        let mut kernel = vec![0.0f32; ksize * ksize];
        kernel[r * ksize + r] = 1.0; // delta central
        let out = convolve_kernel(&img, w, h, &kernel, r);
        for i in 0..w * h {
            assert!((out[i] - img[i]).abs() < 1e-3, "identidad rota en {i}");
        }
    }

    /// El estimador de ruido MAD (Donoho) sobre una banda con |v| constante = A
    /// debe recuperar σ ≈ 1.4826·A (mediana |v| = A).
    #[test]
    fn test_estimate_noise_mad_recovers_sigma() {
        let a = 100.0f32;
        let band: Vec<f32> = (0..4096).map(|i| if i % 2 == 0 { a } else { -a }).collect();
        let s = estimate_noise_mad(&band);
        assert!(
            (s - 1.4826 * a).abs() < 1.0,
            "sigma_mad {s} != esperado {}",
            1.4826 * a
        );
    }

    /// La AUTO-MASCARA debe distinguir estructura de ruido: una linea coherente
    /// de alta magnitud recibe confianza ALTA (se amplifica) mientras el ruido
    /// plano de baja magnitud recibe confianza BAJA (se atenua). Esto es lo que
    /// evita el ruido "wormy" del sharpening uniforme de RegiStax/WaveSharp.
    #[test]
    fn test_auto_mask_confidence_structure_vs_noise() {
        let (w, h) = (64usize, 64usize);
        // Banda de detalle sintetica: mitad izquierda = ruido plano ±30 (baja
        // magnitud, sin coherencia); mitad derecha = linea coherente de 800 en
        // x=3w/4, cero alrededor.
        let mut band = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let idx = y * w + x;
                band[idx] = if x < w / 2 {
                    if (x + y) % 2 == 0 { 30.0 } else { -30.0 }
                } else if x == 3 * w / 4 {
                    800.0
                } else {
                    0.0
                };
            }
        }
        let sigma_n = estimate_noise_mad(&band);
        let conf = detail_confidence_map(&band, w, h, sigma_n);

        let mut noise_conf = 0.0f32;
        let mut nn = 0.0f32;
        for y in 8..h - 8 {
            for x in 4..(w / 2 - 4) {
                noise_conf += conf[y * w + x];
                nn += 1.0;
            }
        }
        noise_conf /= nn;

        let mut line_conf = 0.0f32;
        let mut ln = 0.0f32;
        for y in 8..h - 8 {
            line_conf += conf[y * w + 3 * w / 4];
            ln += 1.0;
        }
        line_conf /= ln;

        eprintln!("auto-mask conf ruido {noise_conf:.3} vs linea {line_conf:.3}");
        assert!(
            line_conf > noise_conf * 2.0,
            "auto-mask no distingue estructura ({line_conf}) de ruido ({noise_conf})"
        );
    }

    /// La LUT de rango del bilateral (optimización de velocidad) debe reproducir
    /// el bilateral con exp DIRECTO dentro de una tolerancia estrecha — así el
    /// speedup no cambia el resultado visible.
    #[test]
    fn test_bilateral_range_lut_accuracy() {
        let (w, h) = (32usize, 32usize);
        let mut img = vec![0.0f32; w * h];
        for i in 0..w * h {
            img[i] = 500.0 + ((i * 37) % 900) as f32; // textura pseudo-aleatoria
        }
        let sigma_spatial = 2.0f32;
        let sigma_range = 300.0f32;
        let out = apply_bilateral_blur(&img, w, h, sigma_spatial, sigma_range);

        // Referencia directa (mismo kernel espacial, exp de rango sin LUT), solo
        // en píxeles interiores donde el kernel completo cabe (sin bordes).
        let radius = (sigma_spatial * 2.5).ceil().clamp(1.0, 9.0) as isize;
        let inv2_s = 1.0 / (2.0 * sigma_spatial * sigma_spatial);
        let inv2_r = 1.0 / (2.0 * sigma_range * sigma_range);
        let mut maxrel = 0.0f32;
        for y in (radius as usize)..(h - radius as usize) {
            for x in (radius as usize)..(w - radius as usize) {
                let center = img[y * w + x];
                let (mut sum, mut wsum) = (0.0f32, 0.0f32);
                for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        let v = img[((y as isize + dy) as usize) * w + (x as isize + dx) as usize];
                        let ws = (-((dx * dx + dy * dy) as f32) * inv2_s).exp();
                        let dr = v - center;
                        let ww = ws * (-dr * dr * inv2_r).exp();
                        sum += v * ww;
                        wsum += ww;
                    }
                }
                let refv = sum / wsum;
                let rel = (out[y * w + x] - refv).abs() / refv.abs().max(1.0);
                maxrel = maxrel.max(rel);
            }
        }
        eprintln!("bilateral LUT vs exp directo: maxrel {maxrel:.6}");
        assert!(maxrel < 2e-3, "LUT de rango imprecisa: {maxrel}");
    }

    #[test]
    fn test_richardson_lucy_increases_blurred_peak_without_instability() {
        let (width, height) = (48usize, 48usize);
        let mut truth = vec![1800.0f32; width * height];
        let centre = height / 2 * width + width / 2;
        truth[centre] = 52000.0;
        let observed = apply_gaussian_blur(&truth, width, height, 1.35);
        let blur = |image: &[f32]| apply_gaussian_blur(image, width, height, 1.35);
        let restored = richardson_lucy_core(
            &observed,
            &observed,
            width,
            height,
            8,
            1.35,
            &blur,
            &|| false,
            &|_| {},
            ProtectionProfile::PROTECTED,
        )
        .expect("RL no debe cancelarse");
        assert!(restored.iter().all(|value| value.is_finite() && *value >= 0.0 && *value <= 65535.0));
        assert!(restored[centre] > observed[centre] * 1.03, "RL debe recuperar contraste del pico");
    }

    #[test]
    fn test_richardson_lucy_restores_extended_texture_instead_of_softening_it() {
        let (width, height) = (96usize, 80usize);
        let mut truth = vec![0.0f32; width * height];
        for y in 0..height {
            for x in 0..width {
                let xf = x as f32;
                let yf = y as f32;
                let fibrils = (xf * 0.53 + (yf * 0.17).sin() * 2.4).sin() * 2_300.0;
                let cells = (xf * 0.19).cos() * (yf * 0.23).sin() * 1_450.0;
                let filament = if (x + (y / 5)) % 29 <= 1 { -4_800.0 } else { 0.0 };
                truth[y * width + x] = (31_000.0 + fibrils + cells + filament)
                    .clamp(2_000.0, 62_000.0);
            }
        }
        let sigma = 1.25;
        let observed = apply_gaussian_blur(&truth, width, height, sigma);
        let blur = |image: &[f32]| apply_gaussian_blur(image, width, height, sigma);
        let restored = richardson_lucy_core(
            &observed,
            &observed,
            width,
            height,
            12,
            sigma,
            &blur,
            &|| false,
            &|_| {},
            ProtectionProfile::PROTECTED,
        )
        .expect("RL no debe cancelarse");

        let mse = |image: &[f32]| {
            image
                .iter()
                .zip(truth.iter())
                .map(|(value, target)| (value - target).powi(2))
                .sum::<f32>()
                / image.len() as f32
        };
        let acutance = |image: &[f32]| {
            let mut total = 0.0f32;
            let mut count = 0usize;
            for y in 2..height - 2 {
                for x in 2..width - 2 {
                    let index = y * width + x;
                    total += (image[index + 1] - image[index - 1]).abs()
                        + (image[index + width] - image[index - width]).abs();
                    count += 2;
                }
            }
            total / count as f32
        };

        let observed_mse = mse(&observed);
        let restored_mse = mse(&restored);
        let observed_acutance = acutance(&observed);
        let restored_acutance = acutance(&restored);
        assert!(
            restored_mse < observed_mse * 0.94,
            "RL debe acercar la textura a la señal: MSE {restored_mse} vs {observed_mse}"
        );
        assert!(
            restored_acutance > observed_acutance * 1.05,
            "RL no puede suavizar la textura: acutancia {restored_acutance} vs {observed_acutance}"
        );
    }

    #[test]
    fn test_smart_sharpen_auto_mask_suppresses_flat_noise_more_than_structure() {
        let (width, height) = (64usize, 64usize);
        let mut image = vec![12000.0f32; width * height];
        for y in 0..height {
            for x in 0..width {
                let index = y * width + x;
                if x < width / 2 {
                    image[index] += if (x * 17 + y * 13) % 2 == 0 { 90.0 } else { -90.0 };
                } else {
                    image[index] = 36000.0;
                }
            }
        }
        let uniform = apply_smart_sharpen_bilateral(
            &image,
            width,
            height,
            1.2,
            1.0,
            1.0,
            0.0,
            &AdaptiveUsmParams::default(),
            None,
            ProtectionProfile::PROTECTED,
        );
        let protected = apply_smart_sharpen_bilateral(
            &image,
            width,
            height,
            1.2,
            1.0,
            1.0,
            1.0,
            &AdaptiveUsmParams::default(),
            None,
            ProtectionProfile::PROTECTED,
        );
        let flat_change = |result: &[f32]| -> f32 {
            let mut total = 0.0;
            let mut samples = 0usize;
            for y in 8..height - 8 {
                for x in 8..width / 2 - 8 {
                    let index = y * width + x;
                    total += (result[index] - image[index]).abs();
                    samples += 1;
                }
            }
            total / samples.max(1) as f32
        };
        let uniform_noise = flat_change(&uniform);
        let protected_noise = flat_change(&protected);
        assert!(protected_noise < uniform_noise * 0.7, "la auto-máscara debe atenuar ruido plano: {protected_noise} vs {uniform_noise}");

        let edge_index = height / 2 * width + width / 2 - 1;
        let uniform_edge = (uniform[edge_index] - image[edge_index]).abs();
        let protected_edge = (protected[edge_index] - image[edge_index]).abs();
        assert!(protected_edge > uniform_edge * 0.35, "la estructura coherente no debe desaparecer");
    }

    #[test]
    fn test_high_pass_changes_structure_but_preserves_a_flat_field() {
        let (width, height) = (48usize, 48usize);
        let flat = vec![18000.0f32; width * height];
        let flat_result =
            apply_high_pass(&flat, width, height, 3.0, 1.5, 1.0, ProtectionProfile::PROTECTED);
        assert!(
            flat_result
                .iter()
                .zip(flat.iter())
                .all(|(result, source)| (result - source).abs() < 1e-3),
            "High Pass no debe inventar detalle sobre un campo plano"
        );

        let mut structured = flat;
        for y in 18..30 {
            for x in 18..30 {
                structured[y * width + x] = 42000.0;
            }
        }
        let result =
            apply_high_pass(&structured, width, height, 3.0, 1.5, 1.0, ProtectionProfile::PROTECTED);
        let mean_delta = result
            .iter()
            .zip(structured.iter())
            .map(|(after, before)| (after - before).abs())
            .sum::<f32>()
            / result.len() as f32;
        assert!(mean_delta > 25.0, "High Pass debe cambiar estructura real: {mean_delta}");
    }

    #[test]
    fn test_adaptive_usm_protects_dark_regions_and_keeps_bright_detail() {
        let (width, height) = (64usize, 48usize);
        let mut image = vec![0.0f32; width * height];
        let mut luminance = vec![0.0f32; width * height];
        for y in 0..height {
            for x in 0..width {
                let index = y * width + x;
                let bright = x >= width / 2;
                let base = if bright { 42000.0 } else { 5000.0 };
                let texture = if (x + y) % 2 == 0 { 500.0 } else { -500.0 };
                image[index] = base + texture;
                luminance[index] = if bright { 0.82 } else { 0.08 };
            }
        }
        let adaptive = AdaptiveUsmParams {
            enabled: true,
            amount_min: 0.05,
            amount_max: 1.0,
            threshold: 0.45,
            transition: 0.2,
        };
        let result = apply_smart_sharpen_bilateral(
            &image,
            width,
            height,
            1.2,
            1.4,
            1.0,
            0.0,
            &adaptive,
            Some(&luminance),
            ProtectionProfile::PROTECTED,
        );
        let region_delta = |left: usize, right: usize| -> f32 {
            let mut total = 0.0;
            let mut count = 0usize;
            for y in 6..height - 6 {
                for x in left..right {
                    let index = y * width + x;
                    total += (result[index] - image[index]).abs();
                    count += 1;
                }
            }
            total / count.max(1) as f32
        };
        let dark_delta = region_delta(6, width / 2 - 6);
        let bright_delta = region_delta(width / 2 + 6, width - 6);
        assert!(
            bright_delta > dark_delta * 8.0,
            "USM adaptativo debe proteger señal oscura: oscuro {dark_delta}, brillante {bright_delta}"
        );
    }

    #[test]
    fn test_restoration_blend_has_clear_endpoints_and_clamps() {
        assert_eq!(blend_restoration(1200.0, 4200.0, 0.0, 1.0), 1200.0);
        assert_eq!(blend_restoration(1200.0, 4200.0, 1.0, 1.0), 4200.0);
        assert_eq!(blend_restoration(1200.0, 4200.0, 2.0, 1.0), 4200.0);
        assert_eq!(blend_restoration(1200.0, 4200.0, 1.0, 0.5), 2700.0);
    }

    #[test]
    fn test_rgb_shift_moves_a_signal_in_the_requested_direction() {
        let (width, height) = (9usize, 7usize);
        let mut channel = vec![0.0f32; width * height];
        channel[3 * width + 4] = 50000.0;
        let shifted = shift_channel(&channel, width, height, 1.0, -1.0);
        let peak = shifted
            .iter()
            .enumerate()
            .max_by(|left, right| left.1.partial_cmp(right.1).unwrap())
            .map(|(index, _)| index)
            .unwrap();
        assert_eq!((peak % width, peak / width), (5, 2));
    }
}

/// PARIDAD PREVIEW↔RENDER FINAL
///
/// El preview interactivo procesa la imagen reducida a 1/N (`preview_downscale`
/// en `apply_wavelets`) pero pasa los MISMOS sigmas al pipeline, y los sigmas de
/// `decompose` estan en PIXELES ABSOLUTOS (`[1,2,4,8,16,32]`). A 1/4, la banda
/// que el slider "U1/W1" amplifica cubre 4 px reales: el preview y el render
/// final amplifican contenido espacial DISTINTO, no solo con distinta fuerza.
///
/// Estos tests miden esa divergencia con correlacion cruzada normalizada sobre
/// el incremento que introduce el slider (`realzado - original`). La correlacion
/// responde exactamente a la pregunta del usuario: "lo que veo mientras arrastro,
/// ¿es lo mismo que voy a obtener?". Es invariante a la escala de amplitud, asi
/// que no confunde "mas debil" con "otra cosa".
#[cfg(test)]
mod preview_parity_tests {
    use super::*;

    /// Misma tolerancia que el gate de paridad GPU↔CPU
    /// (`gpu_wavelet::WAVELET_PARITY_TOL`): mismo algoritmo → debe ser ~0.
    const PARITY_TOL: f32 = 0.5;

    /// Escena determinista: disco brillante con limbo, textura FINA de periodo
    /// 2 px y textura MEDIA de periodo ~50 px. Reproduce lo esencial de un master
    /// lunar/planetario y separa a proposito las dos escalas espaciales, que es
    /// donde se ve el fallo: el promediado 4x4 destruye la fina y deja la media.
    fn parity_scene(w: usize, h: usize) -> Vec<f32> {
        let cx = w as f32 / 2.0;
        let cy = h as f32 / 2.0;
        let radius = w.min(h) as f32 * 0.42;
        let mut img = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let r = (dx * dx + dy * dy).sqrt();
                if r >= radius {
                    img[y * w + x] = 900.0;
                    continue;
                }
                let t = (1.0 - (r / radius) * (r / radius)).max(0.0);
                let disc = 18000.0 + 26000.0 * t.sqrt();
                let fine = 900.0 * (((x + y) % 2) as f32 * 2.0 - 1.0);
                let mid = 1400.0 * (x as f32 / 8.0).sin() * (y as f32 / 8.0).sin();
                img[y * w + x] = disc + fine + mid;
            }
        }
        img
    }

    /// Replica exacta de la closure `decompose` de `run_processing_pipeline` en su
    /// forma Gaussiana pura (sin edge-aware). Cada blur parte de `base`, no en
    /// cascada — igual que produccion.
    fn parity_decompose(base: &[f32], w: usize, h: usize) -> Vec<Vec<f32>> {
        let sigmas = [1.0f32, 2.0, 4.0, 8.0, 16.0, 32.0];
        let blurs: Vec<Vec<f32>> = sigmas
            .iter()
            .map(|&s| apply_gaussian_blur(base, w, h, s))
            .collect();
        let mut ls: Vec<Vec<f32>> = Vec::new();
        ls.push(base.iter().zip(&blurs[0]).map(|(a, b)| a - b).collect());
        for i in 0..5 {
            ls.push(
                blurs[i]
                    .iter()
                    .zip(&blurs[i + 1])
                    .map(|(a, b)| a - b)
                    .collect(),
            );
        }
        ls.push(blurs[5].clone());
        ls
    }

    /// Recombina amplificando SOLO `band`, replicando `recombine` con auto-mascara
    /// y coring neutros (m = 1, cut = 0) para aislar el efecto de la escala.
    fn parity_amplify(layers: &[Vec<f32>], band: usize, gain: f32) -> Vec<f32> {
        (0..layers[0].len())
            .map(|i| {
                let mut v = layers[6][i];
                for (k, layer) in layers.iter().take(6).enumerate() {
                    v += layer[i] * if k == band { 1.0 + gain } else { 1.0 };
                }
                v
            })
            .collect()
    }

    /// Promedio de bloque, misma semantica que `downsample_rgb_u16`
    /// (`commands_core.rs`) sobre un solo canal.
    fn box_downsample(src: &[f32], w: usize, h: usize, factor: usize) -> (Vec<f32>, usize, usize) {
        let (sw, sh) = (w / factor, h / factor);
        let n = (factor * factor) as f32;
        let mut out = vec![0.0f32; sw * sh];
        for ty in 0..sh {
            for tx in 0..sw {
                let mut acc = 0.0f32;
                for dy in 0..factor {
                    for dx in 0..factor {
                        acc += src[(ty * factor + dy) * w + tx * factor + dx];
                    }
                }
                out[ty * sw + tx] = acc / n;
            }
        }
        (out, sw, sh)
    }

    /// Nearest-neighbour, misma semantica que `upscale_rgb8_nearest`.
    fn nearest_upscale(src: &[f32], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; dw * dh];
        for y in 0..dh {
            let sy = (y * sh / dh).min(sh - 1);
            for x in 0..dw {
                let sx = (x * sw / dw).min(sw - 1);
                out[y * dw + x] = src[sy * sw + sx];
            }
        }
        out
    }

    /// Correlacion cruzada normalizada de dos incrementos sobre una region.
    /// 1.0 = el preview muestra exactamente el mismo contenido que el final;
    /// 0.0 = contenido espacial no relacionado.
    fn region_correlation(
        a: &[f32],
        b: &[f32],
        w: usize,
        x0: usize,
        y0: usize,
        rw: usize,
        rh: usize,
    ) -> f32 {
        let (mut sa, mut sb) = (0.0f64, 0.0f64);
        for y in y0..y0 + rh {
            for x in x0..x0 + rw {
                sa += a[y * w + x] as f64;
                sb += b[y * w + x] as f64;
            }
        }
        let n = (rw * rh) as f64;
        let (ma, mb) = (sa / n, sb / n);
        let (mut num, mut da, mut db) = (0.0f64, 0.0f64, 0.0f64);
        for y in y0..y0 + rh {
            for x in x0..x0 + rw {
                let va = a[y * w + x] as f64 - ma;
                let vb = b[y * w + x] as f64 - mb;
                num += va * vb;
                da += va * va;
                db += vb * vb;
            }
        }
        if da <= f64::EPSILON || db <= f64::EPSILON {
            return 0.0;
        }
        (num / (da.sqrt() * db.sqrt())) as f32
    }

    fn region_rms(v: &[f32], w: usize, x0: usize, y0: usize, rw: usize, rh: usize) -> f32 {
        let mut acc = 0.0f64;
        for y in y0..y0 + rh {
            for x in x0..x0 + rw {
                acc += (v[y * w + x] as f64).powi(2);
            }
        }
        (acc / (rw * rh) as f64).sqrt() as f32
    }

    /// DOCUMENTA EL FALLO ACTUAL. El preview a 1/4 y el render 1:1 amplifican
    /// contenido espacial NO RELACIONADO cuando el usuario mueve la banda fina:
    /// la correlacion entre ambos incrementos es practicamente nula.
    ///
    /// Este test fija el comportamiento roto con un numero para poder demostrar
    /// la mejora. La Fase 1 lo sustituye por la version de recuadro 1:1, que debe
    /// correlacionar ~1.0.
    #[test]
    fn test_downscaled_preview_amplifies_different_spatial_content() {
        let (w, h) = (512usize, 512usize);
        let scene = parity_scene(w, h);
        let gain = 3.0f32;

        // Ruta A: render final 1:1.
        let full_layers = parity_decompose(&scene, w, h);
        let full_sharp = parity_amplify(&full_layers, 0, gain);
        let full_delta: Vec<f32> = full_sharp
            .iter()
            .zip(&scene)
            .map(|(a, b)| a - b)
            .collect();

        // Ruta B: preview a 1/4 (promedio de bloque, mismos sigmas, nearest de vuelta).
        let factor = 4usize;
        let (small, sw, sh) = box_downsample(&scene, w, h, factor);
        let small_layers = parity_decompose(&small, sw, sh);
        let small_sharp = parity_amplify(&small_layers, 0, gain);
        let small_delta: Vec<f32> = small_sharp.iter().zip(&small).map(|(a, b)| a - b).collect();
        let preview_delta = nearest_upscale(&small_delta, sw, sh, w, h);

        // Region central, dentro del disco y lejos del borde de la imagen.
        let (x0, y0, rw, rh) = (160usize, 160usize, 192usize, 192usize);
        let corr = region_correlation(&full_delta, &preview_delta, w, x0, y0, rw, rh);
        let rms_full = region_rms(&full_delta, w, x0, y0, rw, rh);
        let rms_preview = region_rms(&preview_delta, w, x0, y0, rw, rh);

        eprintln!(
            "preview 1/4 vs final 1:1 -> correlacion {corr:.4} | RMS final {rms_full:.0} ADU, RMS preview {rms_preview:.0} ADU"
        );

        assert!(
            corr.abs() < 0.25,
            "el preview a 1/4 deberia estar decorrelacionado del final (fallo conocido); correlacion medida {corr:.4}"
        );
    }

    /// LA GARANTIA WYSIWYG. Un recuadro procesado a resolucion NATIVA reproduce
    /// el render completo en la region comun: misma banda, mismo contenido, misma
    /// amplitud. Es la prueba de que la direccion del arreglo (recuadro 1:1 en vez
    /// de preview reducido) es correcta.
    ///
    /// El margen de guarda absorbe las colas de las Gaussianas; con sigma maximo
    /// 32 se necesita ~3σ. Este test fija cuanta guarda hace falta de verdad.
    #[test]
    fn test_roi_crop_at_native_resolution_matches_full_render() {
        let (w, h) = (512usize, 512usize);
        let scene = parity_scene(w, h);
        let gain = 3.0f32;

        let full_layers = parity_decompose(&scene, w, h);
        let full_sharp = parity_amplify(&full_layers, 0, gain);
        let full_delta: Vec<f32> = full_sharp
            .iter()
            .zip(&scene)
            .map(|(a, b)| a - b)
            .collect();

        // Recuadro visible de 192x192 en (160,160) con 128 px de guarda a cada lado.
        let (vis_x, vis_y, vis_w, vis_h) = (160usize, 160usize, 192usize, 192usize);
        let guard = 128usize;
        let (cx, cy) = (vis_x - guard, vis_y - guard);
        let (cw, ch) = (vis_w + 2 * guard, vis_h + 2 * guard);
        let mut crop = vec![0.0f32; cw * ch];
        for y in 0..ch {
            for x in 0..cw {
                crop[y * cw + x] = scene[(cy + y) * w + cx + x];
            }
        }

        let crop_layers = parity_decompose(&crop, cw, ch);
        let crop_sharp = parity_amplify(&crop_layers, 0, gain);
        let crop_delta: Vec<f32> = crop_sharp.iter().zip(&crop).map(|(a, b)| a - b).collect();

        // Comparar solo la zona visible (la guarda se descarta tras procesar).
        let mut num = 0.0f64;
        let mut den = 0.0f64;
        let mut worst = 0.0f32;
        for y in 0..vis_h {
            for x in 0..vis_w {
                let a = full_delta[(vis_y + y) * w + vis_x + x];
                let b = crop_delta[(guard + y) * cw + guard + x];
                num += ((a - b) as f64).powi(2);
                den += (a as f64).powi(2);
                worst = worst.max((a - b).abs());
            }
        }
        let rmse = (num / (vis_w * vis_h) as f64).sqrt() as f32;
        let rms_ref = (den / (vis_w * vis_h) as f64).sqrt() as f32;
        let corr = {
            let mut cropped_full = vec![0.0f32; w * h];
            for y in 0..vis_h {
                for x in 0..vis_w {
                    cropped_full[(vis_y + y) * w + vis_x + x] =
                        crop_delta[(guard + y) * cw + guard + x];
                }
            }
            region_correlation(&full_delta, &cropped_full, w, vis_x, vis_y, vis_w, vis_h)
        };

        eprintln!(
            "recuadro 1:1 (guarda {guard} px) vs final -> correlacion {corr:.6} | RMSE {rmse:.3} ADU sobre señal {rms_ref:.0} ADU | peor pixel {worst:.3} ADU"
        );

        assert!(
            corr > 0.999,
            "el recuadro 1:1 debe reproducir el contenido del render completo; correlacion {corr:.6}"
        );
        assert!(
            rmse <= PARITY_TOL,
            "el recuadro 1:1 debe coincidir con el render completo dentro de {PARITY_TOL} ADU; RMSE {rmse:.3}"
        );
    }

    /// U y W SUMAN A LA MISMA GANANCIA, así que el mismo número debe producir el
    /// mismo efecto en las dos familias.
    ///
    /// Antes no era así: U llevaba un `×0.5` no documentado, de modo que el panel
    /// "Detalles Alta Frecuencia" mostraba 5.0 y entregaba 2.5, mientras que
    /// "Wavelets Multiescala" mostraba 10.0 y entregaba 10.0. El número mentía en
    /// una familia y en la otra no.
    /// Réplica EXACTA de la recombinación de `run_processing_pipeline` con las dos
    /// familias por separado, sin coring ni auto-máscara (para aislar la ganancia).
    fn parity_recombine(layers: &[Vec<f32>], u_amts: &[f32; 5], w_amts: &[f32; 6]) -> Vec<f32> {
        (0..layers[0].len())
            .map(|i| {
                let mut v = layers[6][i];
                v += layers[0][i] * (1.0 + w_amts[0] + u_amts[0]);
                v += layers[1][i] * (1.0 + w_amts[1] + u_amts[1]);
                v += layers[2][i] * (1.0 + w_amts[2] + u_amts[2]);
                v += layers[3][i] * (1.0 + w_amts[3] + u_amts[3]);
                v += layers[4][i] * (1.0 + w_amts[4] + u_amts[4]);
                v += layers[5][i] * (1.0 + w_amts[5]);
                v
            })
            .collect()
    }

    #[test]
    fn test_u_and_w_families_deliver_the_same_gain_for_the_same_number() {
        let (w, h) = (256usize, 256usize);
        let layers = parity_decompose(&parity_scene(w, h), w, h);
        let value = 3.0f32;

        // El mismo número en U y en W debe producir el MISMO resultado en las
        // cinco bandas que aceptan ambas familias.
        for band in 0..5usize {
            let mut u = [0.0f32; 5];
            let mut w_amts = [0.0f32; 6];
            u[band] = value;
            w_amts[band] = value;

            let via_u = parity_recombine(&layers, &u, &[0.0; 6]);
            let via_w = parity_recombine(&layers, &[0.0; 5], &w_amts);
            let worst = via_u
                .iter()
                .zip(&via_w)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(
                worst <= 1e-3,
                "banda {band}: U={value} y W={value} deben pesar igual, desviación {worst}"
            );
        }

        // Y deben SUMARSE, no competir: U=1.5 + W=1.5 == W=3.0.
        let mitad = value * 0.5;
        let mut u_half = [0.0f32; 5];
        let mut w_half = [0.0f32; 6];
        let mut w_full = [0.0f32; 6];
        u_half[0] = mitad;
        w_half[0] = mitad;
        w_full[0] = value;
        let sumadas = parity_recombine(&layers, &u_half, &w_half);
        let solo_w = parity_recombine(&layers, &[0.0; 5], &w_full);
        let worst = sumadas
            .iter()
            .zip(&solo_w)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst <= 1e-3, "U y W deben sumarse, desviación {worst}");

        // La banda 6 NO acepta U, y es DELIBERADO: U es el control de "las escalas
        // más finas" y sigma 32 es la más gruesa del banco. Añadir U6 contradiría
        // lo que el panel dice que hace.
        assert_eq!(WAVELET_BAND_SIGMAS[5], 32.0);
        // Saturar las cinco bandas U no debe alterar la banda 6: sólo W la toca.
        let mut w_solo_b6 = [0.0f32; 6];
        w_solo_b6[5] = value;
        let con_u = parity_recombine(&layers, &[9.0; 5], &w_solo_b6);
        let sin_u = parity_recombine(&layers, &[9.0; 5], &[0.0; 6]);
        let banda6 = con_u
            .iter()
            .zip(&sin_u)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(banda6 > 1.0, "W6 debe seguir teniendo efecto propio: {banda6}");
    }

    /// GOLDEN TEST DEL MODO PUREZA: `p = 1.0` (Protegido) debe reproducir
    /// EXACTAMENTE las constantes historicas. Es la red de seguridad de toda la
    /// fase: mientras esto pase, activar la funcion no cambia el aspecto de nada
    /// que ya existiera.
    #[test]
    fn test_protection_profile_protected_matches_historic_constants() {
        let p = ProtectionProfile::PROTECTED;
        assert!(p.is_protected());
        assert_eq!(p.highlight_start(), 32000.0);
        assert_eq!(p.highlight_floor(), 0.001);
        assert_eq!(p.chroma_floor(), 0.1);
        assert_eq!(p.rl_ratio_bounds(), (0.5, 2.0));
        assert_eq!(p.rl_correction_weight(), 0.78);
        assert_eq!(p.vc_dampening(), 0.38);
        assert_eq!(p.vc_correction_weight(), 0.50);
        assert_eq!(p.psf_radius_cap(), 9.0);
        assert_eq!(p.soft_clip_knee(), 58_000.0);
        assert_eq!(p.usm_fine_threshold(50.0), 50.0);
        assert_eq!(p.sharpen_knee_exponent(), 0.5);
        assert_eq!(p.sharpen_limit(8000.0), 8000.0);

        // `max_step` historico: `(|v|·0.22 + 96).clamp(96, 8000)`.
        for v in [0.0f32, 1000.0, 30000.0, 65535.0] {
            assert_eq!(p.rl_max_step(v), (v.abs() * 0.22 + 96.0).clamp(96.0, 8_000.0));
        }
        // Suelo del denoise historico: `0.30 + a·0.70`.
        for a in [0.0f32, 0.25, 0.5, 1.0] {
            assert_eq!(p.denoise_blend(a), (0.30 + a * 0.70).clamp(0.0, 1.0));
        }
        // El radio de PSF sigue acotado a [3, 9] con el perfil por defecto.
        assert_eq!(psf_radius_for_sigma(0.0), 3);
        assert_eq!(psf_radius_for_sigma(100.0), 9);

        // Modulo avanzado: constantes historicas intactas.
        assert_eq!(p.local_contrast_highlight_gate(), (0.88, 0.995));
        assert_eq!(p.local_contrast_flat_floor(), 0.12);
        assert_eq!(p.lce_limit(8000.0), 8000.0);
        // La confianza cromatica medida pasa TAL CUAL en Protegido.
        for measured in [0.0f32, 0.13, 0.5, 1.0] {
            assert_eq!(p.effective_chroma_confidence(measured), measured);
        }
    }

    /// Las tres etiquetas de la UI se traducen al escalar esperado, y cualquier
    /// valor desconocido cae en Protegido — un ajuste corrupto nunca debe
    /// desactivar protecciones sin querer.
    #[test]
    fn test_protection_profile_mode_parsing_defaults_to_protected() {
        assert_eq!(ProtectionProfile::from_mode(Some("protected")).p, 1.0);
        assert_eq!(ProtectionProfile::from_mode(Some("balanced")).p, 0.5);
        assert_eq!(ProtectionProfile::from_mode(Some("pure")).p, 0.0);
        assert_eq!(ProtectionProfile::from_mode(None).p, 1.0);
        assert_eq!(ProtectionProfile::from_mode(Some("")).p, 1.0);
        assert_eq!(ProtectionProfile::from_mode(Some("PURE")).p, 1.0);
        assert_eq!(ProtectionProfile::from_mode(Some("basura")).p, 1.0);
    }

    /// Bajar la proteccion nunca debe RESTRINGIR mas. Cada ley debe moverse de
    /// forma monotona hacia "el deslizador manda".
    #[test]
    fn test_protection_profile_relaxes_monotonically() {
        let modes = [
            ProtectionProfile::from_mode(Some("protected")),
            ProtectionProfile::from_mode(Some("balanced")),
            ProtectionProfile::from_mode(Some("pure")),
        ];
        for pair in modes.windows(2) {
            let (strict, loose) = (pair[0], pair[1]);
            // Altas luces: empieza a proteger MAS TARDE y atenua MENOS.
            assert!(loose.highlight_start() > strict.highlight_start());
            assert!(loose.highlight_floor() > strict.highlight_floor());
            assert!(loose.chroma_floor() > strict.chroma_floor());
            // Deconvolucion: mas recorrido por iteracion.
            let (slo, shi) = strict.rl_ratio_bounds();
            let (llo, lhi) = loose.rl_ratio_bounds();
            assert!(llo < slo && lhi > shi);
            assert!(loose.rl_max_step(30000.0) > strict.rl_max_step(30000.0));
            assert!(loose.rl_correction_weight() > strict.rl_correction_weight());
            assert!(loose.vc_dampening() > strict.vc_dampening());
            assert!(loose.vc_correction_weight() > strict.vc_correction_weight());
            // Topes y rodillas: mas margen.
            assert!(loose.psf_radius_cap() > strict.psf_radius_cap());
            assert!(loose.soft_clip_knee() > strict.soft_clip_knee());
            assert!(loose.sharpen_limit(8000.0) > strict.sharpen_limit(8000.0));
            assert!(loose.sharpen_knee_exponent() > strict.sharpen_knee_exponent());
            assert!(loose.usm_fine_threshold(50.0) < strict.usm_fine_threshold(50.0));
            // Denoise: el suelo forzado baja (a 0 el deslizador llega a 0 de verdad).
            assert!(loose.denoise_blend(0.0) < strict.denoise_blend(0.0));
            // Modulo avanzado: Textura/Claridad se apagan mas tarde y el suelo en
            // zona plana sube.
            assert!(loose.local_contrast_highlight_gate().0 > strict.local_contrast_highlight_gate().0);
            assert!(loose.local_contrast_flat_floor() > strict.local_contrast_flat_floor());
            assert!(loose.lce_limit(8000.0) > strict.lce_limit(8000.0));
            // Color: la puerta de confianza cromatica se abre.
            assert!(
                loose.effective_chroma_confidence(0.0) > strict.effective_chroma_confidence(0.0),
                "sobre datos neutros el color debe responder mas al relajar la proteccion"
            );
        }

        // En Puro el deslizador de denoise cubre TODO el rango, sin suelo.
        let pure = ProtectionProfile::from_mode(Some("pure"));
        assert_eq!(pure.denoise_blend(0.0), 0.0);
        assert_eq!(pure.denoise_blend(1.0), 1.0);
        // Van Cittert oscila con facilidad: ni en Puro se suelta del todo.
        assert!(pure.vc_dampening() < 1.0);

        // En Puro, Textura/Claridad ya no se apagan sobre el objeto brillante ni
        // se atenuan en zonas planas.
        assert_eq!(pure.local_contrast_flat_floor(), 1.0);
        assert!(pure.local_contrast_highlight_gate().0 >= 0.99);
        // Y el color responde aunque el dato sea casi neutro.
        assert_eq!(pure.effective_chroma_confidence(0.0), 1.0);
        assert_eq!(pure.effective_chroma_confidence(0.4), 1.0);
    }

    /// Escena para el sharpening pre-apilado: superficie lisa y BRILLANTE (bien
    /// por encima de la puerta SNR) con textura fina de amplitud `amplitude`.
    ///
    /// La amplitud es el parametro clave. El fallo historico no tocaba el detalle
    /// marcado — vivia en el detalle DEBIL, de magnitud comparable al umbral de
    /// coring (60-144 ADU): justo el detalle de superficie que se busca en Luna y
    /// planetas. Las esquinas se dejan planas porque de ahi sale la estimacion de
    /// ruido (MAD sobre los bordes del encuadre).
    fn prestack_scene(w: usize, h: usize, amplitude: f32) -> Vec<u16> {
        let mut out = vec![0u16; w * h * 3];
        let margin = 16usize;
        for y in 0..h {
            for x in 0..w {
                let flat_corner = (x < margin || x >= w - margin) && (y < margin || y >= h - margin);
                let value = if flat_corner {
                    900.0
                } else {
                    30000.0 + amplitude * (((x + y) % 2) as f32 * 2.0 - 1.0)
                };
                let c = value.clamp(0.0, 65535.0) as u16;
                let i = (y * w + x) * 3;
                out[i] = c;
                out[i + 1] = c;
                out[i + 2] = c;
            }
        }
        out
    }

    fn prestack_detail_energy(base: &[u16], w: usize, h: usize, intensity: f32) -> f64 {
        let mut buf = base.to_vec();
        apply_autostakkert_sharpening(&mut buf, w, h, intensity, "luna");
        let mut acc = 0.0f64;
        let m = 48usize;
        for y in m..h - m {
            for x in m..w - m {
                let i = (y * w + x) * 3;
                let d = buf[i] as f64 - base[i] as f64;
                acc += d * d;
            }
        }
        (acc / ((h - 2 * m) * (w - 2 * m)) as f64).sqrt()
    }

    /// EL SLIDER DE SHARPENING PRE-APILADO DEBE SER MONOTONO, TAMBIEN EN DETALLE
    /// DEBIL.
    ///
    /// Antes no lo era. `base_threshold = σ·(2.5 + intensity)` con suelo
    /// `120·√intensity` hacia que subir la intensidad subiera tambien el umbral, y
    /// el coring DURO `(|c|−t)·signo(c)` borraba de golpe lo que quedaba debajo.
    /// Un coeficiente de 100 ADU: a intensidad 0.25 el umbral de banda 1 era 72 y
    /// pasaba; a intensidad 1.00 era 144 y se BORRABA. Subir el deslizador
    /// eliminaba el detalle que decia realzar.
    #[test]
    fn test_prestack_sharpening_is_monotonic_even_for_faint_detail() {
        let (w, h) = (256usize, 256usize);
        let steps = [0.25f32, 0.50, 0.75, 1.00];

        // Barrido de amplitudes alrededor del umbral historico (60..144 ADU).
        for amplitude in [60.0f32, 100.0, 160.0, 400.0] {
            let base = prestack_scene(w, h, amplitude);
            let energies: Vec<f64> = steps
                .iter()
                .map(|&s| prestack_detail_energy(&base, w, h, s))
                .collect();
            eprintln!(
                "detalle ±{amplitude:>5.0} ADU -> {:8.2} {:8.2} {:8.2} {:8.2}",
                energies[0], energies[1], energies[2], energies[3]
            );
            for (idx, pair) in energies.windows(2).enumerate() {
                assert!(
                    pair[1] > pair[0],
                    "con detalle de ±{amplitude} ADU, subir de {} a {} REDUJO el realce ({:.2} -> {:.2})",
                    steps[idx],
                    steps[idx + 1],
                    pair[0],
                    pair[1]
                );
            }
        }
    }

    /// El umbral de coring describe el RUIDO, no la intencion del usuario: dos
    /// intensidades deben cribar igual, y lo unico que cambia es cuanto se
    /// amplifica lo que pasa la criba. Con el umbral desacoplado, duplicar la
    /// intensidad duplica el efecto — sea cual sea la amplitud del detalle.
    #[test]
    fn test_prestack_response_is_linear_across_detail_amplitudes() {
        let (w, h) = (256usize, 256usize);
        for amplitude in [60.0f32, 100.0, 160.0, 400.0] {
            let base = prestack_scene(w, h, amplitude);
            let low = prestack_detail_energy(&base, w, h, 0.25);
            let high = prestack_detail_energy(&base, w, h, 0.50);
            let ratio = high / low.max(1e-9);
            eprintln!("detalle ±{amplitude:>5.0} ADU -> relacion 0.50/0.25 = {ratio:.3}");
            assert!(
                (1.85..=2.15).contains(&ratio),
                "con detalle de ±{amplitude} ADU el umbral sigue acoplado a la intensidad: relacion {ratio:.3}"
            );
        }
    }

    /// LA MÁSCARA SNR NO DEBE TENER ESCALÓN.
    ///
    /// Invariante permanente, no la prueba de un arreglo: la forma anterior
    /// (`if orig_l < signal_threshold { 0.0 } else { rampa lineal }`) YA era
    /// continua — valía 0 justo en el umbral. Este test pasa con ambas, y ése es
    /// su propósito: impedir que una futura optimización meta un corte de verdad
    /// en la frontera fondo→objeto, que se vería como un borde alrededor del disco.
    ///
    /// Se comprueba sobre una rampa de luminancia: el incremento que introduce el
    /// sharpening debe crecer de forma acotada, sin saltos bruscos entre columnas
    /// contiguas.
    #[test]
    fn test_prestack_snr_mask_has_no_discontinuity() {
        let (w, h) = (192usize, 120usize);
        // El estimador de ruido muestrea las CUATRO ESQUINAS del encuadre, así que
        // el fondo tiene que ser uniforme ahí o `median_bg` y σ salen disparatados.
        // Por eso la rampa vive sólo en una banda central de filas: las esquinas
        // quedan en fondo plano y la estimación es la de un caso real.
        let band = h / 3..2 * h / 3;
        let mut base = vec![0u16; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let ramp = if band.contains(&y) {
                    (x as f32 / w as f32) * 9000.0
                } else {
                    0.0
                };
                let tex = 250.0 * (((x + y) % 2) as f32 * 2.0 - 1.0);
                let c = (800.0 + ramp + tex).clamp(0.0, 65535.0) as u16;
                let i = (y * w + x) * 3;
                base[i] = c;
                base[i + 1] = c;
                base[i + 2] = c;
            }
        }

        let mut sharp = base.clone();
        apply_autostakkert_sharpening(&mut sharp, w, h, 1.0, "luna");

        // Perfil del incremento por columna DENTRO de la banda, promediado en
        // vertical para cancelar la textura y dejar sólo el peso de la máscara.
        let column_delta: Vec<f32> = (0..w)
            .map(|x| {
                let mut acc = 0.0f64;
                let mut n = 0usize;
                for y in band.clone() {
                    let i = (y * w + x) * 3;
                    acc += (sharp[i] as f64 - base[i] as f64).abs();
                    n += 1;
                }
                (acc / n.max(1) as f64) as f32
            })
            .collect();

        let peak = column_delta.iter().cloned().fold(0.0f32, f32::max);
        assert!(peak > 1.0, "la escena debe producir realce medible: {peak}");

        // Mayor salto entre columnas contiguas, relativo al pico.
        let (mut worst_jump, mut worst_at) = (0.0f32, 0usize);
        for x in 1..w {
            let jump = (column_delta[x] - column_delta[x - 1]).abs();
            if jump > worst_jump {
                worst_jump = jump;
                worst_at = x;
            }
        }
        let relative = worst_jump / peak;
        eprintln!(
            "perfil SNR: pico {peak:.1} ADU · mayor salto {worst_jump:.1} ADU en x={worst_at} ({:.1}% del pico)",
            relative * 100.0
        );

        // Con el corte duro, el salto en la frontera era del orden del propio
        // pico. Un smoothstep lo reparte en varias columnas.
        assert!(
            relative < 0.25,
            "la máscara SNR sigue metiendo un escalón: {:.1}% del pico en x={worst_at}",
            relative * 100.0
        );
    }

    /// Convierte la escena f32 a un buffer RGB16 entrelazado, como `StackResult`.
    fn scene_to_rgb16(scene: &[f32]) -> Vec<u16> {
        let mut out = vec![0u16; scene.len() * 3];
        for (i, &v) in scene.iter().enumerate() {
            let c = v.clamp(0.0, 65535.0) as u16;
            out[i * 3] = c;
            out[i * 3 + 1] = c;
            out[i * 3 + 2] = c;
        }
        out
    }

    fn crop_rgb16(
        src: &[u16],
        w: usize,
        x0: usize,
        y0: usize,
        cw: usize,
        ch: usize,
    ) -> Vec<u16> {
        let mut out = vec![0u16; cw * ch * 3];
        for y in 0..ch {
            for x in 0..cw {
                let s = ((y0 + y) * w + x0 + x) * 3;
                let d = (y * cw + x) * 3;
                out[d..d + 3].copy_from_slice(&src[s..s + 3]);
            }
        }
        out
    }

    /// LA RAZON DE SER DE `GlobalStats`. Las estadisticas que gobiernan umbrales
    /// internos y el motor de color dependen del CONTENIDO del buffer. Medirlas de
    /// un recorte hace que MOVER el recuadro cambie el resultado: seria nitido
    /// pero no representativo del render final.
    ///
    /// Este test demuestra la divergencia (recortes distintos → estadisticas
    /// distintas) y que `GlobalStats` medido del master la elimina.
    #[test]
    fn test_global_stats_are_invariant_to_roi_position() {
        let (w, h) = (512usize, 512usize);
        let scene = parity_scene(w, h);
        let rgb = scene_to_rgb16(&scene);

        let (master, _) = GlobalStats::measure(&rgb, w, h, false, 0.0);

        // Dos recuadros muy distintos: centro del disco (brillante y con textura)
        // y esquina (casi todo cielo).
        let centre = crop_rgb16(&rgb, w, 192, 192, 128, 128);
        let corner = crop_rgb16(&rgb, w, 8, 8, 128, 128);
        let (centre_stats, _) = GlobalStats::measure(&centre, 128, 128, false, 0.0);
        let (corner_stats, _) = GlobalStats::measure(&corner, 128, 128, false, 0.0);

        eprintln!(
            "master  -> img_scale {:.4}, pivote {:.0}, blanco {:.0}",
            master.img_scale, master.color_pivot, master.tone_white
        );
        eprintln!(
            "centro  -> img_scale {:.4}, pivote {:.0}, blanco {:.0}",
            centre_stats.img_scale, centre_stats.color_pivot, centre_stats.tone_white
        );
        eprintln!(
            "esquina -> img_scale {:.4}, pivote {:.0}, blanco {:.0}",
            corner_stats.img_scale, corner_stats.color_pivot, corner_stats.tone_white
        );

        // Sin `GlobalStats`, dos recuadros darian umbrales incompatibles.
        assert!(
            (centre_stats.color_pivot - corner_stats.color_pivot).abs() > 1000.0,
            "el test no esta ejerciendo la divergencia: pivotes {:.0} vs {:.0}",
            centre_stats.color_pivot,
            corner_stats.color_pivot
        );

        // Con `GlobalStats` del master, el valor que ve el pipeline es UNO solo,
        // independientemente de donde este el recuadro.
        for (label, stats) in [("centro", &centre_stats), ("esquina", &corner_stats)] {
            let _ = stats;
            assert_eq!(
                master.img_scale,
                GlobalStats::measure(&rgb, w, h, false, 0.0).0.img_scale,
                "las estadisticas del master deben ser deterministas ({label})"
            );
        }
        assert_eq!(master.img_p99, measure_img_p99(&rgb));
        assert_eq!(master.img_scale, img_scale_from_p99(master.img_p99));
    }

    /// `None` (exportacion/lote) debe medir exactamente lo mismo que
    /// `GlobalStats::measure` sobre el mismo buffer completo: la ruta de
    /// exportacion sigue siendo bit-identica al comportamiento historico.
    #[test]
    fn test_global_stats_match_inline_measurement_on_full_image() {
        let (w, h) = (256usize, 256usize);
        let scene = parity_scene(w, h);
        let rgb = scene_to_rgb16(&scene);

        let (stats, _) = GlobalStats::measure(&rgb, w, h, false, 0.0);
        let inline_p99 = measure_img_p99(&rgb);
        let (inline_pivot, inline_white) = estimate_color_adjust_context(&rgb);

        assert_eq!(stats.img_p99, inline_p99);
        assert_eq!(stats.img_scale, img_scale_from_p99(inline_p99));
        assert_eq!(stats.color_pivot, inline_pivot);
        assert_eq!(stats.tone_white, inline_white);
    }

    /// La cache se invalida si cambia el radio de PSF. Un kernel medido con otro
    /// radio no solo daria otro resultado: `psf_ref` asume que mide (2r+1)².
    #[test]
    fn test_global_stats_psf_cache_key_tracks_radius_and_intent() {
        let (w, h) = (128usize, 128usize);
        let rgb = scene_to_rgb16(&parity_scene(w, h));

        // Sin PSF pedida, la cache sirve para cualquier receta que tampoco la pida.
        let (no_psf, _) = GlobalStats::measure(&rgb, w, h, false, 0.0);
        assert!(no_psf.matches_psf_request(false, 0.0));
        assert!(no_psf.matches_psf_request(false, 3.0));
        // ...pero NO para una que si la pida.
        assert!(!no_psf.matches_psf_request(true, 1.5));

        // Medida con sigma 1.5 → radio 3. Solo vale para ese radio.
        let (with_psf, _) = GlobalStats::measure(&rgb, w, h, true, 1.5);
        assert_eq!(with_psf.psf_radius, psf_radius_for_sigma(1.5));
        assert!(with_psf.matches_psf_request(true, 1.5));
        assert!(
            !with_psf.matches_psf_request(true, 4.0),
            "sigma 4.0 da radio {} ≠ {}",
            psf_radius_for_sigma(4.0),
            with_psf.psf_radius
        );

        // El radio esta acotado a [3, 9] pase lo que pase con el sigma.
        assert_eq!(psf_radius_for_sigma(0.0), 3);
        assert_eq!(psf_radius_for_sigma(100.0), 9);
    }

    /// `roi_guard_px` debe pedir AL MENOS la guarda que el banco necesita de
    /// verdad. Este test cierra el lazo: para cada banda, calcula la guarda con la
    /// funcion de produccion y comprueba que con ella el recuadro sale exacto.
    #[test]
    fn test_roi_guard_px_covers_the_measured_requirement() {
        let (w, h) = (512usize, 512usize);
        let scene = parity_scene(w, h);
        let gain = 3.0f32;
        let (vis_x, vis_y, vis_w, vis_h) = (192usize, 192usize, 128usize, 128usize);

        for band in 0..6usize {
            let mut w_amts = [0.0f32; 6];
            w_amts[band] = gain;
            let guard = roi_guard_px(
                &[0.0; 5],
                &w_amts,
                &[0.0; 6],
                0.0,
                w,
                h,
                0.0,
                0,
                0.0,
                0,
                0.0,
                0.0,
                0.0,
                0.0,
                0,
            );

            let full_layers = parity_decompose(&scene, w, h);
            let full_sharp = parity_amplify(&full_layers, band, gain);

            assert!(
                vis_x >= guard && vis_y >= guard && vis_x + vis_w + guard <= w,
                "guarda {guard} no cabe en la escena de prueba para la banda {band}"
            );
            let (cx, cy) = (vis_x - guard, vis_y - guard);
            let (cw, ch) = (vis_w + 2 * guard, vis_h + 2 * guard);
            let mut crop = vec![0.0f32; cw * ch];
            for y in 0..ch {
                for x in 0..cw {
                    crop[y * cw + x] = scene[(cy + y) * w + cx + x];
                }
            }
            let crop_layers = parity_decompose(&crop, cw, ch);
            let crop_sharp = parity_amplify(&crop_layers, band, gain);

            let mut worst = 0.0f32;
            for y in 0..vis_h {
                for x in 0..vis_w {
                    let a = full_sharp[(vis_y + y) * w + vis_x + x];
                    let b = crop_sharp[(guard + y) * cw + guard + x];
                    worst = worst.max((a - b).abs());
                }
            }
            eprintln!(
                "banda {band} (sigma {:>4.0}) -> roi_guard_px pide {guard:>3} px, peor desviacion {worst:.3} ADU",
                WAVELET_BAND_SIGMAS[band]
            );
            assert!(
                worst <= PARITY_TOL,
                "la guarda de produccion ({guard} px) no basta para la banda {band}: {worst:.3} ADU"
            );
        }
    }

    /// La guarda debe ESCALAR con la receta, no ser el caso peor siempre: es la
    /// diferencia entre pagar ×1.13 y ×1.89 en pixeles procesados.
    #[test]
    fn test_roi_guard_px_scales_with_the_active_recipe() {
        let zero_u = [0.0f32; 5];
        let zero_w = [0.0f32; 6];
        let zero_d = [0.0f32; 6];
        let no_extras = |u: &[f32; 5], w: &[f32; 6], d: &[f32; 6]| {
            roi_guard_px(u, w, d, 0.0, 4096, 4096, 0.0, 0, 0.0, 0, 0.0, 0.0, 0.0, 0.0, 0)
        };

        // Receta neutra: guarda minima (solo el colchon).
        let neutral = no_extras(&zero_u, &zero_w, &zero_d);
        // Detalle fino (banda 1): barata.
        let fine = no_extras(&zero_u, &[3.0, 0.0, 0.0, 0.0, 0.0, 0.0], &zero_d);
        // Banda gruesa (banda 6): cara.
        let coarse = no_extras(&zero_u, &[0.0, 0.0, 0.0, 0.0, 0.0, 3.0], &zero_d);

        eprintln!("guarda neutra {neutral} px | detalle fino {fine} px | banda gruesa {coarse} px");
        assert!(fine < coarse, "la guarda debe escalar con la banda activa");
        assert!(neutral <= fine);
        assert!(coarse >= 96, "banda sigma 32 necesita 3σ = 96 px");

        // Las bandas de denoise (`d`) tambien cuentan: operan sobre la misma banda.
        let denoise_only = no_extras(&zero_u, &zero_w, &[0.0, 0.0, 0.0, 0.0, 0.0, 2.0]);
        assert_eq!(denoise_only, coarse, "`d6` activa la misma banda que `w6`");

        // El LCE deriva su sigma del MASTER: en 4K son 30 px → 90 de guarda.
        let lce_4k = roi_guard_px(
            &zero_u, &zero_w, &zero_d, 50.0, 4096, 4096, 0.0, 0, 0.0, 0, 0.0, 0.0, 0.0, 0.0, 0,
        );
        let lce_small = roi_guard_px(
            &zero_u, &zero_w, &zero_d, 50.0, 640, 480, 0.0, 0, 0.0, 0, 0.0, 0.0, 0.0, 0.0, 0,
        );
        eprintln!("guarda LCE: master 4K {lce_4k} px | master 640x480 {lce_small} px");
        assert!(
            lce_4k > lce_small,
            "el radio del LCE crece con el tamaño del master, y la guarda con el"
        );

        // La deconvolucion tambien difunde.
        let deconv = roi_guard_px(
            &zero_u, &zero_w, &zero_d, 0.0, 4096, 4096, 3.0, 12, 0.0, 0, 0.0, 0.0, 0.0, 0.0, 0,
        );
        assert!(deconv > neutral, "la deconvolucion activa debe ampliar la guarda");
        // ...pero solo si de verdad se ejecuta (0 iteraciones = no difunde).
        let deconv_off = roi_guard_px(
            &zero_u, &zero_w, &zero_d, 0.0, 4096, 4096, 3.0, 0, 0.0, 0, 0.0, 0.0, 0.0, 0.0, 0,
        );
        assert_eq!(deconv_off, neutral);
    }

    /// Mide la GUARDA MINIMA que necesita el recuadro, POR BANDA. Es un parametro
    /// real de implementacion (`preview_roi` procesa recuadro+guarda y descarta la
    /// guarda): cada pixel de guarda cuesta CPU, y quedarse corto mete un halo en
    /// el borde del recuadro.
    ///
    /// Resultado no obvio: la descomposicion es una PARTICION (las bandas suman la
    /// original), asi que las bandas con ganancia 1.0 se cancelan al recombinar y
    /// su error de borde no llega a la salida. La guarda no la fija el sigma mayor
    /// del banco, sino el sigma de la banda que el usuario esta amplificando.
    /// Por eso hay que dimensionar por el CASO PEOR (banda mas gruesa activa), que
    /// es justo lo que mide este test.
    #[test]
    fn test_roi_guard_band_requirement_scales_with_amplified_band() {
        let (w, h) = (512usize, 512usize);
        let scene = parity_scene(w, h);
        let gain = 3.0f32;
        let (vis_x, vis_y, vis_w, vis_h) = (192usize, 192usize, 128usize, 128usize);
        // Sigma nominal de cada banda del banco [1,2,4,8,16,32].
        let band_sigma = [1.0f32, 2.0, 4.0, 8.0, 16.0, 32.0];
        let mut worst_required_guard = 0usize;

        for band in 0..6usize {
            let full_layers = parity_decompose(&scene, w, h);
            let full_sharp = parity_amplify(&full_layers, band, gain);
            let full_delta: Vec<f32> = full_sharp
                .iter()
                .zip(&scene)
                .map(|(a, b)| a - b)
                .collect();

            let mut minimum_exact_guard = None;
            for guard in [0usize, 8, 16, 32, 64, 96, 128, 160] {
                if vis_x < guard || vis_y < guard {
                    continue;
                }
                let (cx, cy) = (vis_x - guard, vis_y - guard);
                let (cw, ch) = (vis_w + 2 * guard, vis_h + 2 * guard);
                if cx + cw > w || cy + ch > h {
                    continue;
                }
                let mut crop = vec![0.0f32; cw * ch];
                for y in 0..ch {
                    for x in 0..cw {
                        crop[y * cw + x] = scene[(cy + y) * w + cx + x];
                    }
                }
                let crop_layers = parity_decompose(&crop, cw, ch);
                let crop_sharp = parity_amplify(&crop_layers, band, gain);

                let mut worst = 0.0f32;
                for y in 0..vis_h {
                    for x in 0..vis_w {
                        let a = full_delta[(vis_y + y) * w + vis_x + x];
                        let b = crop_sharp[(guard + y) * cw + guard + x]
                            - crop[(guard + y) * cw + guard + x];
                        worst = worst.max((a - b).abs());
                    }
                }
                if worst <= PARITY_TOL && minimum_exact_guard.is_none() {
                    minimum_exact_guard = Some(guard);
                    break;
                }
            }

            match minimum_exact_guard {
                Some(g) => {
                    eprintln!(
                        "banda {band} (sigma {:>4.0}) -> guarda minima exacta {g:>3} px",
                        band_sigma[band]
                    );
                    worst_required_guard = worst_required_guard.max(g);
                }
                None => panic!(
                    "banda {band} (sigma {}) no alcanzo la tolerancia con ninguna guarda probada",
                    band_sigma[band]
                ),
            }
        }

        eprintln!(
            "GUARDA A DIMENSIONAR (caso peor, banda mas gruesa activa): {worst_required_guard} px"
        );
        assert!(
            worst_required_guard <= 128,
            "la guarda del caso peor ({worst_required_guard} px) excede lo asumido en el plan (128 px)"
        );
    }
}
