import os

file_path = r"g:\SOFTWARE by EMG\astro-stacker PROYECTO\astro-stacker\src-tauri\src\commands_v2_v3.rs"

with open(file_path, "r", encoding="utf-8") as f:
    lines = f.readlines()

# 1. Update Weighting Logic (approx lines 2029-2106)
# Search for the block
start_accum = -1
end_accum = -1
for i, line in enumerate(lines):
    if "if is_small_planet_v3_local {" in line and 2000 < i < 2100:
        start_accum = i
    if "sigma_scale," in line and 2080 < i < 2150:
        end_accum = i + 2
        break

if start_accum != -1 and end_accum != -1:
    new_accum = """                        if is_small_planet_v3_local {
                            // ── PLANET SMALL V3: Rigid Lanczos ──────────────────
                            let frame_score = frame_data.score as f32;
                            let rejection_threshold = 0.25; 

                            let normalized = if global_score_range > 0.0 {
                                ((frame_score - global_min_score) / global_score_range).clamp(0.0, 1.0)
                            } else {
                                1.0
                            };

                            if normalized < rejection_threshold {
                                return acc; // Discard poor frames
                            }

                            let q_weight = normalized.powi(3); // Power law for detail

                            accumulate_frame_rigid_lanczos(
                                t_r, t_g, t_b, t_w,
                                rgb_buf, w_in, h_in, w_out, h_out,
                                drizzle, roi_offset_x, roi_offset_y,
                                render_dx, render_dy,
                                q_weight,
                            );
                        } else {
                            // ── SURFACE / PLANET LARGE: Liquid Warping ──
                            let frame_score = frame_data.score as f32;
                            let rejection_threshold = if is_surface_logic { 0.0 } else { 0.25 }; 

                            let normalized = if global_score_range > 0.0 {
                                ((frame_score - global_min_score) / global_score_range).clamp(0.0, 1.0)
                            } else {
                                1.0
                            };

                            if normalized < rejection_threshold {
                                return acc;
                            }

                            let q_weight = if !is_surface_logic {
                                normalized.powi(3)
                            } else {
                                1.0 // Surface remains linear/constant weight
                            };

                            accumulate_frame_liquid(
                                t_r, t_g, t_b, t_w,
                                rgb_buf, w_in, h_in, w_out, h_out,
                                drizzle, roi_offset_x, roi_offset_y,
                                render_dx, render_dy,
                                &local_shifts, &warp_indices, &warp_weights,
                                ap_mask,
                                q_weight,
                                &custom_points,
                                sigma_scale,
                            );
                        }
"""
    lines[start_accum:end_accum] = [new_accum]

# 2. Update Sharpening Logic (approx lines 2602-2732)
start_sharp = -1
end_sharp = -1
for i, line in enumerate(lines):
    if "fn apply_planetary_multiscale_sharpening" in line:
        start_sharp = i
    if "out" in line and i > 2700 and "Vec::with_capacity" not in line and "push" not in line and "{" not in line:
        # Looking for the final 'out' or closing brace
        pass

# Actually, let's just search for the function signature and the next '}' that closes it
brace_count = 0
found_start = False
for i in range(start_sharp, len(lines)):
    line = lines[i]
    if "{" in line:
        if not found_start: found_start = True
        brace_count += line.count("{")
    if "}" in line:
        brace_count -= line.count("}")
    
    if found_start and brace_count == 0:
        end_sharp = i + 1
        break

if start_sharp != -1 and end_sharp != -1:
    new_sharp = """fn apply_planetary_multiscale_sharpening(img: &[u16], w: usize, h: usize, target_type: &str) -> Vec<u16> {
    let n = w * h;
    let t_low = target_type.to_lowercase();
    let is_small = t_low.contains("peque") || t_low.contains("small");

    // Split interleaved RGB into planar channels
    let mut r_ch = vec![0.0f32; n];
    let mut g_ch = vec![0.0f32; n];
    let mut b_ch = vec![0.0f32; n];
    for i in 0..n {
        r_ch[i] = img[i * 3]     as f32;
        g_ch[i] = img[i * 3 + 1] as f32;
        b_ch[i] = img[i * 3 + 2] as f32;
    }

    // PHASE 1: RICHARDSON-LUCY DECONVOLUTION
    let rl_sigma = if is_small { 1.15 } else { 1.45 };
    let rl_iter = 20;

    let r_rl = apply_richardson_lucy(&r_ch, w, h, rl_sigma, rl_iter);
    let g_rl = apply_richardson_lucy(&g_ch, w, h, rl_sigma, rl_iter);
    let b_rl = apply_richardson_lucy(&b_ch, w, h, rl_sigma, rl_iter);

    // PHASE 2: COSMETIC MULTI-SCALE SHARPENING
    let b_m_hi = apply_gaussian_blur_safe(&g_rl, w, h, 3.5);
    let b_c_hi = apply_gaussian_blur_safe(&g_rl, w, h, 8.0);
    let (w_m, w_c) = (0.75f32, 0.35f32); 

    let apply_ch = |ch: &[f32]| -> Vec<f32> {
        ch.iter().enumerate().map(|(i, &v)| {
            let m_detail = v - b_m_hi[i];
            let c_detail = b_m_hi[i] - b_c_hi[i];
            let detail = m_detail * w_m + c_detail * w_c;
            (v + detail).clamp(0.0, 65535.0)
        }).collect()
    };

    let r_fin = apply_ch(&r_rl);
    let g_fin = apply_ch(&g_rl);
    let b_fin = apply_ch(&b_rl);

    let mut out = Vec::with_capacity(img.len());
    for i in 0..n {
        out.push(r_fin[i] as u16);
        out.push(g_fin[i] as u16);
        out.push(b_fin[i] as u16);
    }
    out
}
"""
    lines[start_sharp:end_sharp] = [new_sharp]

with open(file_path, "w", encoding="utf-8") as f:
    f.writelines(lines)

print("Patch applied successfully.")
