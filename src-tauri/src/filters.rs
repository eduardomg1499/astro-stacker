// ==========================================
// 5. FILTROS Y WAVELETS
// ==========================================

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

    // Transpose
    let mut transp = vec![0.0; size];
    for y in 0..h {
        for x in 0..w {
            transp[x * h + y] = temp[y * w + x];
        }
    }

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

    // Transpose Back
    for x in 0..w {
        for y in 0..h {
            output[y * w + x] = temp_transp[x * h + y];
        }
    }

    output
}

fn check_cancel(state: &AppState, req_id: usize) -> bool {
    state.active_req_id.load(Ordering::Relaxed) != req_id
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
) -> Vec<f32> {
    if iterations == 0 || sigma <= 0.0 {
        return input.to_vec();
    }

    let size = width * height;
    let mut est = input.to_vec();

    // We compute a "confidence mask" based on edge strength and signal intensity.
    // Deconvolution should only work hard where there are actual structures (planetary disk, rings).
    // It should NOT work hard in the pure dark background.
    let mut mask = vec![0.0f32; size];
    let bg_blur = apply_gaussian_blur(original, width, height, sigma * 3.0);

    // Finding p95 roughly to understand signal level
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

    let mut ratio_buf = vec![0.0f32; size];

    for i in 0..iterations {
        if check_cancel(state, req_id) {
            return Vec::new();
        }
        if range.1 - range.0 > 1.0 && i % 1 == 0 {
            let local_p = i as f32 / iterations as f32;
            let global_p = range.0 + local_p * (range.1 - range.0);
            emit_progress(
                app,
                "Deconvolucion RL (TV)",
                global_p,
                Some(format!("Iteracion {}/{}", i + 1, iterations)),
            );
        }

        // 1. Blur the Current Estimate
        let blurred_est = apply_gaussian_blur(&est, width, height, sigma);

        // 2. Compute Ratio (Original / Blurred_Estimate)
        for j in 0..size {
            let denom = blurred_est[j].max(1.0);
            let raw_ratio = if original[j] > 1.0 {
                original[j] / denom
            } else {
                1.0
            };
            // Strict per-iteration bounds to prevent explosion
            ratio_buf[j] = raw_ratio.clamp(0.5, 2.0);
        }

        // 3. Back-Project Ratio (Blur the Ratio)
        let blurred_ratio = apply_gaussian_blur(&ratio_buf, width, height, sigma);

        // 4. Update the Estimate (With Total Variation Regularization & Masking)
        let tv_weight = 0.05; // TV dampening to kill individual hot pixels

        for y in 1..(height - 1) {
            for x in 1..(width - 1) {
                let j = y * width + x;

                // Total Variation (TV) - Push pixel toward its neighborhood average if it's spiking
                let n1 = est[y * width + (x - 1)];
                let n2 = est[y * width + (x + 1)];
                let n3 = est[(y - 1) * width + x];
                let n4 = est[(y + 1) * width + x];
                let local_mean = (n1 + n2 + n3 + n4) * 0.25;
                let tv_gradient = local_mean - est[j];

                // Apply update natively
                let mut update = est[j] * blurred_ratio[j];

                // Apply TV Regularization to smooth out ringing/spikes
                update += tv_gradient * tv_weight;

                // Blend the correction conservatively. Even high-confidence pixels should not
                // receive the full RL update in one pass because that creates halos quickly.
                let correction_weight = mask[j] * 0.58;
                let mut final_val = est[j] * (1.0 - correction_weight) + update * correction_weight;

                if final_val.is_nan() || final_val.is_infinite() {
                    final_val = original[j];
                }

                // Hard mathematical limits for 16-bit space
                if final_val > 65535.0 {
                    final_val = 65535.0;
                }
                if final_val < 0.0 {
                    final_val = 0.0;
                }

                est[j] = final_val;
            }
        }
    }

    est
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

        // 2. Iterate block
        for y in 1..(height - 1) {
            for x in 1..(width - 1) {
                let j = y * width + x;

                // Van Cittert Residual (Difference between Original and Blurred Estimate)
                let residual = (original[j] - blurred_est[j]).clamp(-15000.0, 15000.0);

                // Total Variation (TV) - Push pixel toward its neighborhood average if it's spiking
                let n1 = est[y * width + (x - 1)];
                let n2 = est[y * width + (x + 1)];
                let n3 = est[(y - 1) * width + x];
                let n4 = est[(y + 1) * width + x];
                let local_mean = (n1 + n2 + n3 + n4) * 0.25;
                let tv_gradient = local_mean - est[j];

                // Apply update natively
                let mut update = est[j] + (residual * vc_dampening);

                // Apply TV Regularization to smooth out ringing/spikes
                update += tv_gradient * tv_weight;

                let correction_weight = mask[j] * 0.50;
                let mut final_val = est[j] * (1.0 - correction_weight) + update * correction_weight;

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

                est[j] = final_val;
            }
        }
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
        let mut guard = state.deconv_cache.lock().unwrap();
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
            let mut processed = Vec::new();

            for (ch_idx, ch) in work_channels.iter().enumerate() {
                if check_cancel(state, req_id) {
                    return Vec::new();
                }

                let dr = apply_richardson_lucy(
                    app,
                    state,
                    req_id,
                    ch,
                    ch,
                    width,
                    height,
                    deconv_iter,
                    deconv_sigma,
                    (0.0, 100.0),
                );
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
                processed.push(vcr);

                let pct = (ch_idx + 1) as f32 / work_channels.len() as f32 * 35.0;
                emit_progress(app, "Deconvolucion...", pct, None);
            }
            work_channels = processed;
        }

        let nc = DeconvCache {
            channels: work_channels.clone(),
            params: d_params.clone(),
            width,
            height,
        };
        {
            let mut guard = state.deconv_cache.lock().unwrap();
            guard.insert(0, nc);
            if guard.len() > 3 {
                guard.pop();
            }
        }
        work_channels
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
        let mut guard = state.wavelet_cache.lock().unwrap();
        let mut match_idx = None;
        for (i, w) in guard.iter().enumerate() {
            if w.width == width && w.height == height && w.parent_deconv_params == d_params {
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
        let decompose = |base: &[f32]| -> Vec<Vec<f32>> {
            let mut ls = Vec::new();
            let sigmas = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0];
            let mut blurs = Vec::new();
            for &s in &sigmas {
                blurs.push(apply_gaussian_blur(base, width, height, s));
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
            channels: multi_layers,
            width,
            height,
            parent_deconv_params: d_params.clone(),
        };
        {
            let mut guard = state.wavelet_cache.lock().unwrap();
            guard.insert(0, wc.clone());
            if guard.len() > 3 {
                guard.pop();
            }
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
    };

    let mut cached_filter: Option<FilterCache> = None;
    {
        let mut guard = state.filter_cache.lock().unwrap();
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

        let recombine = |lys: &Vec<Vec<f32>>| -> Vec<f32> {
            let mut out = vec![0.0; size];
            let _ub = u_amts.iter().sum::<f32>() * 0.2;
            let den = |v: f32, a: f32, t: f32| {
                if v.abs() < t * 100.0 {
                    v * (v.abs() / (t * 100.0 + 0.01)) * a
                } else {
                    v * a
                }
            };
            for i in 0..size {
                let mut v = lys[6][i];
                // Distribute high-frequency U levels properly across the first fine wavelet layers
                v += den(lys[0][i], 1.0 + w_amts[0] + u_amts[0] * 0.5, d_amts[0]);
                v += den(lys[1][i], 1.0 + w_amts[1] + u_amts[1] * 0.5, d_amts[1]);
                v += den(lys[2][i], 1.0 + w_amts[2] + u_amts[2] * 0.5, d_amts[2]);
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
                    ty, width, height, usm_radius, usm_amount, img_scale,
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
            channels: work_filter.clone(),
            params: f_params,
            width,
            height,
        };
        {
            let mut guard = state.filter_cache.lock().unwrap();
            guard.insert(0, nc);
            if guard.len() > 3 {
                guard.pop();
            }
        }
        work_filter
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
            color_pivot, tone_white,
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
