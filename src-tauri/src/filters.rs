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
) -> Option<Vec<f32>> {
    let size = width * height;
    let mut est = input.to_vec();

    // Máscara de confianza (helper compartido con la ruta GPU → idéntica).
    let mask = richardson_lucy_mask(original, width, height, sigma);

    let mut ratio_buf = vec![0.0f32; size];
    let tv_weight = 0.05;

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
            ratio_buf[j] = raw_ratio.clamp(0.5, 2.0);
        }

        // 3. Back-projection (PSF simétrica → mismo kernel que el forward).
        let blurred_ratio = blur(&ratio_buf);

        // 4. Update JACOBI (snapshot) + TV + máscara. Bordes intactos. Paralelo.
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
                let mut update = est_prev[j] * blurred_ratio[j];
                update += tv_gradient * tv_weight;
                let correction_weight = mask[j] * 0.58;
                let mut final_val =
                    est_prev[j] * (1.0 - correction_weight) + update * correction_weight;
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

    let vc_dampening = 0.38; // Van Cittert overshoots easily; keep residual updates gentle.
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

                    let correction_weight = mask[j] * 0.50;
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
    let img_p99 = {
        let mut vals: Vec<u16> = original.data.iter().copied().collect();
        let idx = (vals.len() * 99 / 100).min(vals.len().saturating_sub(1));
        if idx < vals.len() {
            vals.select_nth_unstable(idx);
            (vals[idx] as f32).max(1000.0) // minimum of 1000 to avoid over-sensitivity
        } else {
            65535.0
        }
    };
    // Scale factor: 1.0 when image is full 16-bit, ~0.06 when image is 8-bit equiv.
    let img_scale = (img_p99 / 65535.0).clamp(0.05, 1.0);

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
            let psf_radius = (deconv_sigma * 2.0).ceil().clamp(3.0, 9.0) as usize;
            let psf_measured: Option<Vec<f32>> = if psf_from_limb && deconv_iter > 0 {
                let luma: Vec<f32> = (0..size)
                    .map(|i| {
                        0.299 * original.data[i * 3] as f32
                            + 0.587 * original.data[i * 3 + 1] as f32
                            + 0.114 * original.data[i * 3 + 2] as f32
                    })
                    .collect();
                let planet_mask = compute_planet_mask(&luma, width, height, 4);
                let limb_mask = compute_limb_mask(&planet_mask, width, height, psf_radius.max(4));
                let has_limb = limb_mask.iter().filter(|&&m| m > 0.5).count() > psf_radius * psf_radius * 8;
                if has_limb {
                    let est = PsfEstimator { psf_radius }
                        .estimate_from_limb(&luma, width, height, &limb_mask, 0.85);
                    if psf_is_valid(&est, psf_radius) {
                        log_to_front(app, "INFO", "Deconvolucion: PSF medida del limbo (edge-spread) — activa.");
                        Some(est)
                    } else {
                        log_to_front(app, "INFO", "Deconvolucion: PSF del limbo no fiable → Gaussiana parametrica.");
                        None
                    }
                } else {
                    log_to_front(app, "INFO", "Deconvolucion: sin limbo claro (disco lleno) → Gaussiana parametrica.");
                    None
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
                            (0.0, 100.0), psf_ref,
                        )
                    })
                } else {
                    apply_richardson_lucy(
                        app, state, req_id, ch, ch, width, height, deconv_iter, deconv_sigma,
                        (0.0, 100.0), psf_ref,
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
                // Distribute high-frequency U levels properly across the first fine wavelet layers.
                // Las 3 bandas finas (donde vive el ruido) llevan el realce
                // modulado por `m`; las gruesas van intactas.
                v += den(lys[0][i], 1.0 + (w_amts[0] + u_amts[0] * 0.5) * m, d_amts[0]);
                v += den(lys[1][i], 1.0 + (w_amts[1] + u_amts[1] * 0.5) * m, d_amts[1]);
                v += den(lys[2][i], 1.0 + (w_amts[2] + u_amts[2] * 0.5) * m, d_amts[2]);
                v += den(lys[3][i], 1.0 + w_amts[3] + u_amts[3] * 0.5, d_amts[3]);
                v += den(lys[4][i], 1.0 + w_amts[4] + u_amts[4] * 0.5, d_amts[4]);
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
            );
        }

        let total_filter_channels = work_filter.len().max(1);
        for (ch_idx, ty) in work_filter.iter_mut().enumerate() {
            if crisp > 0.0 {
                *ty = apply_high_pass(ty, width, height, 3.0, crisp, img_scale);
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
                );
            }
            if lce_amount > 0.0 {
                *ty = apply_clahe_improved(ty, width, height, lce_amount);
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
        let knee = 58000.0;
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
    let (color_pivot, tone_white) = estimate_color_adjust_context(&original.data);

    let mut r_plane = vec![0.0f32; size];
    let g_plane = vec![0.0f32; size];
    let mut b_plane = vec![0.0f32; size];

    // 1. Parallel Render into separate f32 planes
    // Use unsafe for direct memory writing from parallel iterator
    let r_ptr = r_plane.as_ptr() as usize;
    let g_ptr = g_plane.as_ptr() as usize;
    let b_ptr = b_plane.as_ptr() as usize;

    (0..size).into_par_iter().for_each(|i| {
        let (mut r, mut g, mut b);
        if effective_rgb_mode {
            let or = base_channels[0][i];
            let og = base_channels[1][i];
            let ob = base_channels[2][i];

            let max_orig = or.max(og).max(ob);
            let p_start = 32000.0;
            let p_factor = if max_orig > p_start {
                let ov = (max_orig - p_start) / (65535.0 - p_start);
                (1.0 - ov.powi(6)).max(0.001)
            } else {
                1.0
            };

            let avg_orig = (or + og + ob) / 3.0;
            let c_divergence =
                ((or - avg_orig).abs() + (og - avg_orig).abs() + (ob - avg_orig).abs()) / 3.0;
            let c_guard = if c_divergence > 5000.0 {
                let c_ov = (c_divergence - 5000.0) / 25000.0;
                (1.0 - c_ov.powi(4)).max(0.1)
            } else {
                1.0
            };

            let effective_factor = p_factor * c_guard;
            r = or + (filtered_channels[0][i] - or) * blend * effective_factor;
            g = og + (filtered_channels[1][i] - og) * blend * effective_factor;
            b = ob + (filtered_channels[2][i] - ob) * blend * effective_factor;
        } else {
            let oy = base_channels[0][i];
            let p_start = 32000.0;
            let p_factor = if oy > p_start {
                let ov = (oy - p_start) / (65535.0 - p_start);
                (1.0 - ov.powi(6)).max(0.001)
            } else {
                1.0
            };

            let y_enhanced = oy + (filtered_channels[0][i] - oy) * blend * p_factor;
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
        )
        .expect("RL no debe cancelarse");
        assert!(restored.iter().all(|value| value.is_finite() && *value >= 0.0 && *value <= 65535.0));
        assert!(restored[centre] > observed[centre] * 1.03, "RL debe recuperar contraste del pico");
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
        let uniform = apply_smart_sharpen_bilateral(&image, width, height, 1.2, 1.0, 1.0, 0.0);
        let protected = apply_smart_sharpen_bilateral(&image, width, height, 1.2, 1.0, 1.0, 1.0);
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
}
