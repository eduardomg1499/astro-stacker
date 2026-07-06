use crate::AppState;
// use rayon::prelude::*; // Unused
use tauri::State;

fn default_ap_size() -> usize {
    48
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct ApPoint {
    pub x: f32,
    pub y: f32,
    #[serde(default = "default_ap_size")]
    pub size: usize,
}

#[tauri::command]
pub async fn cmd_generate_smart_ap_grid(
    state: State<'_, AppState>,
    grid_size: usize,
    threshold: f32, // 0.0 - 1.0 (Variance threshold)
    mode: String,   // "planetary" or "surface"
) -> Result<Vec<ApPoint>, String> {
    // 1. Get Master Reference (Stacked Image)
    let stacked_img = {
        let guard = state.stacked_image.lock().unwrap();
        if let Some(res) = &*guard {
            // Clone data to work without lock
            (res.data.clone(), res.width, res.height)
        } else {
            return Err("No stacked image available for Smart APs".into());
        }
    };

    let (data, width, height) = stacked_img;
    let points = generate_smart_grid_internal(&data, width, height, grid_size, threshold, &mode);
    Ok(points)
}

/// Public function to generate AP grid from any image buffer
use crate::integral_image::IntegralImage;

// NUEVO: Noise Floor Calculator (Moved from main.rs)
pub fn calculate_noise_floor(data: &[u16]) -> u16 {
    // 1. Strided sampling to build histogram (speed)
    let stride = 17; // Prime stride
                     // Using Vec to be safe from stack overflow
    let mut histogram = vec![0u32; 65536];

    let len = data.len();
    let mut i = 0;
    while i < len {
        histogram[data[i] as usize] += 1;
        i += stride;
    }

    // 2. Find Peak (Mode)
    let mut max_count = 0;
    let mut peak_idx = 0;

    // Search mostly in the lower range (0-10000 approx) where background exists.
    for (val, &c) in histogram.iter().enumerate().take(32000) {
        if c > max_count {
            max_count = c;
            peak_idx = val;
        }
    }

    // 3. Heuristic: Noise Floor ~ Peak + 3 * Sigma (FWTM)
    // Let's estimate width of the peak
    let mut upper_bound = peak_idx;
    for k in peak_idx..65535 {
        if histogram[k] < max_count / 10 {
            upper_bound = k;
            break;
        }
    }

    // Safety margin
    let width = upper_bound.saturating_sub(peak_idx);
    (peak_idx + width * 2).min(65535) as u16
}

/// Public function to generate AP grid from any image buffer
/// Uses Integral Image for fast variance calculation.
pub fn generate_smart_grid_internal(
    data: &[u16],
    width: usize,
    height: usize,
    grid_size: usize,
    threshold: f32,
    mode: &str,
) -> Vec<ApPoint> {
    // 1. Handle Multi-channel input (RGB -> Mono) for Analysis
    // If data.len() > width * height, we assume it's interleaved (RGB or similar).
    // We assume Green channel is at offset 1 (RGB) or we just average?
    // For performance, let's extract Green if it looks like RGB.

    let is_rgb = data.len() >= width * height * 3;
    let (mono_ptr, _mono_vec_guard): (*const u16, Option<Vec<u16>>) = if is_rgb {
        let count = width * height;
        let mut mono = Vec::with_capacity(count);
        unsafe {
            mono.set_len(count);
            for i in 0..count {
                // Determine stride. Assume 3 for RGB.
                // Improve robustness: check if len is exactly w*h*3.
                // Just use simple heuristic: index * 3 + 1
                let src_idx = i * 3 + 1;
                if src_idx < data.len() {
                    *mono.get_unchecked_mut(i) = *data.get_unchecked(src_idx);
                } else {
                    *mono.get_unchecked_mut(i) = 0;
                }
            }
        }
        let ptr = mono.as_ptr();
        (ptr, Some(mono))
    } else {
        // Already Mono
        (data.as_ptr(), None)
    };

    // Safety: u16 data cast to u8 slice for IntegralImage
    let sat_slice =
        unsafe { std::slice::from_raw_parts(mono_ptr as *const u8, width * height * 2) };

    // 2. Integral Image (O(1) lookups)
    let sat = IntegralImage::new(sat_slice, width, height, 16);

    // 3. Noise Floor
    // Only calculate if needed (Surface mode uses it?)
    // Actually main.rs code used it for both?
    // Planetary mode uses it to ignore background.
    // Surface mode uses logic to find high contrast areas.

    // We can use the mono data for noise floor too (it expects &[u16]).
    let mono_slice = unsafe { std::slice::from_raw_parts(mono_ptr, width * height) };
    let noise_floor = calculate_noise_floor(mono_slice);

    // Use conservative threshold scaling
    // Now that input is always u16 (0-65535), max variance is huge.
    // 65535^2 approx 4e9.
    let max_possible_var = 1_000_000_000.0;
    let abs_threshold = (threshold as f64).powi(2) * max_possible_var * 0.05;

    let is_planet_mode = mode.contains("planet");
    let min_brightness = if is_planet_mode {
        noise_floor as f64 + 12.0
    } else {
        noise_floor as f64 + (grid_size as f64 * 0.1).max(50.0)
    };
    let use_noise_floor_check = noise_floor > 0;

    // --- BIG PLANET DETECTION ---
    // If the object covers a significant portion of the frame, we should
    // treat it like a surface to ensure full AP coverage (no gaps).
    let is_large_object = if is_planet_mode {
        let illuminated = mono_slice
            .iter()
            .filter(|&&v| v > noise_floor + 200)
            .count();
        (illuminated as f64 / (width * height) as f64) > 0.25
    } else {
        false
    };

    // Disk-level estimate (P90) so surface grids do not place APs on pure sky:
    // sky APs corrupt the local warp field at the limb (dots/ghost artifacts).
    let disk_p90: f64 = {
        let mut sample: Vec<u16> = mono_slice.iter().step_by(31).copied().collect();
        if sample.is_empty() {
            0.0
        } else {
            sample.sort_unstable();
            let idx = (sample.len() * 90 / 100).min(sample.len() - 1);
            sample[idx] as f64
        }
    };
    let surface_min_mean = (noise_floor as f64 + 30.0).max(disk_p90 * 0.12);

    let mut points = Vec::new();
    let ap_size = grid_size.max(1);
    let surface_like = !is_planet_mode || is_large_object;
    let step = if surface_like {
        // Surface stacks need overlapping APs so the local warp can follow seeing
        // cells instead of interpolating between a sparse checkerboard of anchors.
        let base = ((ap_size as f32 * 0.67).round() as usize).clamp(8, ap_size);
        // FRAME-AREA CAP: at the base overlap step a 20-Mpx lunar disc spawns
        // 40k+ candidate APs (~3x the density validated on solar surfaces) and
        // multiplies the stacking cost with no quality gain. Scale the step so
        // the candidate count stays near the proven density (~14k for the
        // frame). A 2792x2174 solar capture yields cap==21 == base: the tuned
        // solar behaviour is preserved exactly.
        let cap = (((width * height) as f32 / 14_000.0).sqrt().ceil() as usize).min(ap_size * 3);
        base.max(cap)
    } else {
        ap_size
    };
    let half_step = ap_size / 2;

    // Rows
    // Start at half_step to cover edges (e.g. centered at 48 if step is 96) for better "Stitch" coverage.
    let start_offset = half_step.max(16);
    for y in (start_offset..height.saturating_sub(start_offset)).step_by(step) {
        // Cols
        for x in (start_offset..width.saturating_sub(start_offset)).step_by(step) {
            let start_x = x.saturating_sub(half_step);
            let start_y = y.saturating_sub(half_step);
            let bw = ap_size.min(width - start_x);
            let bh = ap_size.min(height - start_y);

            let (variance, pool_mean) = sat.get_stats(start_x, start_y, bw, bh);

            // Min Brightness Check (Dynamic)
            if use_noise_floor_check {
                if pool_mean < min_brightness {
                    continue;
                }
            } else {
                if pool_mean < 100.0 {
                    continue;
                }
            }

            // Mode-specific checks
            let mut valid = false;

            if is_planet_mode && !is_large_object {
                // Planetary: keep normal texture APs and faint moon/transit APs.
                let texture_signal = variance > abs_threshold * 0.35;
                let faint_local_signal =
                    pool_mean > noise_floor as f64 + 18.0 && variance > abs_threshold * 0.08;
                if texture_signal || faint_local_signal {
                    valid = true;
                }
            } else {
                // Surface OR Big Planet: FORCE DENSE GRID over the DISK only.
                // AutoStakkert fills the target for Moon/Sun/Big Venus, but never
                // anchors APs on background sky (they destabilize the limb warp).
                if pool_mean > surface_min_mean {
                    valid = true;
                }
            }

            if valid {
                points.push(ApPoint {
                    x: x as f32,
                    y: y as f32,
                    size: ap_size,
                });
            }
        }
    }

    if is_planet_mode {
        let cx = (width / 2) as f32;
        let cy = (height / 2) as f32;
        let min_dist = (grid_size as f32 * 0.75).max(12.0);
        let has_center = points.iter().any(|p| {
            let dx = p.x - cx;
            let dy = p.y - cy;
            (dx * dx + dy * dy).sqrt() < min_dist
        });
        if !has_center {
            points.push(ApPoint {
                x: cx,
                y: cy,
                size: ap_size,
            });
        }
    }

    // Always add at least one center point if none found
    if points.is_empty() {
        points.push(ApPoint {
            x: (width / 2) as f32,
            y: (height / 2) as f32,
            size: ap_size,
        });
    }

    points
}
