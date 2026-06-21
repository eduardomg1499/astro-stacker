$file = "g:\SOFTWARE by EMG\astro-stacker PROYECTO\astro-stacker\src-tauri\src\commands_core.rs"
$content = [System.IO.File]::ReadAllText($file)
$lines = $content -split "`n"
Write-Host "Total lines before: $($lines.Count)"

# The corruption is at lines 3977-3982 (0-indexed: 3976-3981)
# Line 3977 should be: "if is_surface {" then the complexity check, else, pts.push, closing braces, 
# then emit_progress, Ok(pts), }, then the new sharpening function, then the gaussian blur helper.

# Build the replacement block
$replacement = @"
                if is_surface {
                    let complexity = get_area_complexity(&u16s, w, h, x, y, ap_size, bri);
                    if complexity > min_complexity {
                        pts.push((x as f32, y as f32, ap_size as f32));
                    }
                } else {
                    pts.push((x as f32, y as f32, ap_size as f32));
                }
            }
        }
    }

    emit_progress(
        &app,
        &format!("Malla generada: {} puntos", pts.len()),
        100.0,
        None,
    );

    Ok(pts)
}

/// PHASE 12: AutoStakkert!-Style Sharpening (Noise-Free)
/// Key principles from AS!:
/// 1. LUMINANCE-ONLY: Sharpen only the L channel -> zero chromatic noise.
/// 2. CONSERVATIVE AMPLIFICATION: 1-6x per band, not 35-55x.
/// 3. ADAPTIVE SOFT CORING on ALL bands: only amplify detail > local noise floor.
/// 4. LOCAL SNR MASKING: suppress sharpening in background/sky pixels.
fn apply_autostakkert_sharpening(buffer: &mut [u16], width: usize, height: usize, intensity: f32, target_type: &str) {
    let npix = width * height;
    if npix == 0 || buffer.len() < npix * 3 { return; }

    let target = target_type.to_lowercase();
    let is_large_planet = target.contains("grande") || target.contains("large");
    let is_small_planet = target.contains("peque") || target.contains("small");

    // Conservative wavelet amplification scales (like AS!)
    let (s1_amp, s2_amp, s3_amp, s4_amp, s5_amp) = if is_large_planet {
        (2.0 * intensity, 3.5 * intensity, 2.0 * intensity, 1.0 * intensity, 0.3 * intensity)
    } else if is_small_planet {
        (3.5 * intensity, 2.5 * intensity, 1.0 * intensity, 0.4 * intensity, 0.1 * intensity)
    } else {
        (3.0 * intensity, 2.8 * intensity, 1.5 * intensity, 0.8 * intensity, 0.2 * intensity)
    };

    // 1. EXTRACT LUMINANCE
    let mut lum = vec![0.0f32; npix];
    for i in 0..npix {
        let r = buffer[i * 3] as f32;
        let g = buffer[i * 3 + 1] as f32;
        let b = buffer[i * 3 + 2] as f32;
        lum[i] = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    }

    // 2. ESTIMATE NOISE from background corners (MAD estimator)
    let mut noise_samples = Vec::with_capacity(200);
    let margin = 8.min(width / 4).min(height / 4);
    for y in 0..margin {
        for x in 0..margin { noise_samples.push(lum[y * width + x]); }
        for x in (width - margin)..width { noise_samples.push(lum[y * width + x]); }
    }
    for y in (height - margin)..height {
        for x in 0..margin { noise_samples.push(lum[y * width + x]); }
        for x in (width - margin)..width { noise_samples.push(lum[y * width + x]); }
    }
    noise_samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_bg = noise_samples.get(noise_samples.len() / 2).cloned().unwrap_or(500.0);
    let mut abs_devs: Vec<f32> = noise_samples.iter().map(|v| (v - median_bg).abs()).collect();
    abs_devs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let noise_sigma = abs_devs.get(abs_devs.len() / 2).cloned().unwrap_or(100.0) * 1.4826;
    let base_threshold = (noise_sigma * 3.0).max(80.0);
    let signal_threshold = median_bg + noise_sigma * 2.0;

    // 3. WAVELET DECOMPOSITION (luminance only)
    let b1_blur = apply_gaussian_blur_f32(&lum, width, height, 1.0);
    let b2_blur = apply_gaussian_blur_f32(&b1_blur, width, height, 2.0);
    let b3_blur = apply_gaussian_blur_f32(&b2_blur, width, height, 4.0);
    let b4_blur = apply_gaussian_blur_f32(&b3_blur, width, height, 8.0);
    let b5_blur = apply_gaussian_blur_f32(&b4_blur, width, height, 16.0);

    let band1: Vec<f32> = lum.iter().zip(b1_blur.iter()).map(|(a, b)| a - b).collect();
    let band2: Vec<f32> = b1_blur.iter().zip(b2_blur.iter()).map(|(a, b)| a - b).collect();
    let band3: Vec<f32> = b2_blur.iter().zip(b3_blur.iter()).map(|(a, b)| a - b).collect();
    let band4: Vec<f32> = b3_blur.iter().zip(b4_blur.iter()).map(|(a, b)| a - b).collect();
    let band5: Vec<f32> = b4_blur.iter().zip(b5_blur.iter()).map(|(a, b)| a - b).collect();

    // 4. SHARPEN LUMINANCE with adaptive soft coring + SNR masking
    let mut sharp_lum = vec![0.0f32; npix];
    for i in 0..npix {
        let orig_l = lum[i];

        // SNR mask: don't sharpen background
        let snr_weight = if orig_l < signal_threshold {
            0.0
        } else {
            ((orig_l - signal_threshold) / (noise_sigma * 5.0 + 1.0)).clamp(0.0, 1.0)
        };

        if snr_weight < 0.01 {
            sharp_lum[i] = orig_l;
            continue;
        }

        // Soft coring: only amplify coefficients above noise floor
        let core = |coeff: f32, thresh: f32| -> f32 {
            let ac = coeff.abs();
            if ac < thresh { 0.0 } else { (ac - thresh) * coeff.signum() }
        };

        let d1 = core(band1[i], base_threshold * 1.2) * s1_amp;
        let d2 = core(band2[i], base_threshold * 0.8) * s2_amp;
        let d3 = core(band3[i], base_threshold * 0.5) * s3_amp;
        let d4 = core(band4[i], base_threshold * 0.3) * s4_amp;
        let d5 = core(band5[i], base_threshold * 0.2) * s5_amp;

        let total = (d1 + d2 + d3 + d4 + d5) * snr_weight;
        sharp_lum[i] = (orig_l + total).clamp(0.0, 65535.0);
    }

    // 5. APPLY back to RGB preserving chrominance (zero chromatic noise)
    for i in 0..npix {
        let orig_l = lum[i];
        let new_l = sharp_lum[i];
        if orig_l < 1.0 {
            let v = new_l.clamp(0.0, 65535.0) as u16;
            buffer[i * 3] = v;
            buffer[i * 3 + 1] = v;
            buffer[i * 3 + 2] = v;
        } else {
            let ratio = new_l / orig_l;
            buffer[i * 3]     = (buffer[i * 3] as f32 * ratio).clamp(0.0, 65535.0) as u16;
            buffer[i * 3 + 1] = (buffer[i * 3 + 1] as f32 * ratio).clamp(0.0, 65535.0) as u16;
            buffer[i * 3 + 2] = (buffer[i * 3 + 2] as f32 * ratio).clamp(0.0, 65535.0) as u16;
        }
    }
}

// Low-overhead Gaussian helper for sharpening (using recursive-like approximation or same as exists)
fn apply_gaussian_blur_f32(data: &[f32], w: usize, h: usize, sigma: f32) -> Vec<f32> {
    // If sigma is large, we use a larger kernel approximation or same fixed tap for speed.
    // For 2.5 and 6.0, we can use the existing 5-tap helper multiple times or a box-blur chain.
    let passes = if sigma > 3.0 { 3 } else { 1 };
    let mut current = data.to_vec();
    for _ in 0..passes {
        current = apply_gaussian_blur_safe(&current, w, h, 1.5); // Reusing existing 5-tap helper
    }
    current
}
"@

# Find the line number to replace: line 3977 (0-indexed 3976) starts with "if is_surface {"
# and the corruption goes through line 3982 (the closing brace of gaussian blur)
# We need to replace lines 3977-3982 (1-indexed) with the replacement block

$newLines = @()
for ($i = 0; $i -lt $lines.Count; $i++) {
    $lineNum = $i + 1
    if ($lineNum -eq 3977) {
        # Insert replacement block
        $repLines = $replacement -split "`n"
        foreach ($rl in $repLines) {
            $newLines += $rl
        }
    } elseif ($lineNum -ge 3978 -and $lineNum -le 3982) {
        # Skip corrupted lines
        continue
    } else {
        $newLines += $lines[$i]
    }
}

$newContent = $newLines -join "`n"
[System.IO.File]::WriteAllText($file, $newContent)
Write-Host "Done. New total lines: $($newLines.Count)"
