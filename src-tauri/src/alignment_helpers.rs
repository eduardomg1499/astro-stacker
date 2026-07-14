// ==========================================
// 3. ALINEACION
// ==========================================

// ------------------------------------------------------------------
// Legacy alignment code moved to alignment.rs
// ------------------------------------------------------------------

// MAIN ENTRY POINT: Coarse-to-Fine Alignment
// MAIN ENTRY POINT: Coarse-to-Fine Alignment

// INTERNAL: Brute Force SAD (Discrete Integer Return)
// Used for the Coarse Step

// ORIGINAL FUNCTION RENAMED TO SUBPIXEL (Fine Stage)
// Returns float subpixel coordinates (dx, dy)

// NEW: AVX2 Optimized Rectangular SAD

// ORIGINAL FUNCTION RENAMED TO SUBPIXEL (Fine Stage)
// Returns float subpixel coordinates (dx, dy)

fn find_planet_roi(data: &[u8], width: usize, height: usize, bpp: usize) -> Rect {
    let step = 8;
    let mut min_x = width;
    let mut max_x = 0;
    let mut min_y = height;
    let mut max_y = 0;
    let mut max_val = 0;
    for i in (0..data.len()).step_by(step * bpp) {
        let val = if bpp == 1 {
            data[i] as u16
        } else {
            get_pixel_value(data, i / bpp, bpp)
        };
        if val > max_val {
            max_val = val;
        }
    }
    let threshold = max_val / 8;
    if threshold < 50 {
        return Rect {
            x: 0,
            y: 0,
            w: width,
            h: height,
        };
    }
    for y in (0..height).step_by(step) {
        let row = y * width;
        for x in (0..width).step_by(step) {
            let val = get_pixel_value(data, row + x, bpp);
            if val > threshold {
                if x < min_x {
                    min_x = x;
                }
                if x > max_x {
                    max_x = x;
                }
                if y < min_y {
                    min_y = y;
                }
                if y > max_y {
                    max_y = y;
                }
            }
        }
    }
    let padding = 48;
    let x0 = min_x.saturating_sub(padding);
    let y0 = min_y.saturating_sub(padding);
    let x1 = (max_x + padding).min(width);
    let y1 = (max_y + padding).min(height);
    if x1 <= x0 || y1 <= y0 {
        return Rect {
            x: 0,
            y: 0,
            w: width,
            h: height,
        };
    }
    Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    }
}

fn find_planet_roi_from_u16(data: &[u16], width: usize, height: usize) -> Rect {
    let step = 4;
    let mut min_x = width;
    let mut max_x = 0;
    let mut min_y = height;
    let mut max_y = 0;
    let mut max_val = 0;
    for i in (0..data.len()).step_by(step) {
        let val = data[i];
        if val > max_val {
            max_val = val;
        }
    }
    let threshold = max_val / 6; // slightly lower threshold for better capture
    if threshold < 100 {
        return Rect { x: 0, y: 0, w: width, h: height };
    }
    for y in (0..height).step_by(step) {
        let row = y * width;
        for x in (0..width).step_by(step) {
            let val = data[row + x];
            if val > threshold {
                if x < min_x { min_x = x; }
                if x > max_x { max_x = x; }
                if y < min_y { min_y = y; }
                if y > max_y { max_y = y; }
            }
        }
    }
    let padding = 32;
    let x0 = min_x.saturating_sub(padding);
    let y0 = min_y.saturating_sub(padding);
    let x1 = (max_x + padding).min(width);
    let y1 = (max_y + padding).min(height);
    if x1 <= x0 || y1 <= y0 {
        return Rect { x: 0, y: 0, w: width, h: height };
    }
    Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    }
}

fn calculate_quality_metric(
    data: &[u8],
    width: usize,
    _height: usize,
    bpp: usize,
    roi: &Rect,
) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe {
                return calculate_quality_metric_avx2(data, width, roi, bpp);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        unsafe {
            return calculate_quality_metric_neon(data, width, roi, bpp);
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    calculate_quality_metric_scalar(data, width, roi, bpp)
}

fn calculate_quality_metric_scalar(data: &[u8], width: usize, roi: &Rect, bpp: usize) -> u64 {
    let mut score: u64 = 0;
    let step = 2;
    let end_y = roi.y + roi.h - 1;
    let end_x = roi.x + roi.w - 1;

    // Noise Gate: Adaptive to bit depth. For 8-bit data, max Laplacian is ~1020,
    // so sq max is ~1,040,400. A gate of 4M would reject EVERYTHING.
    // For 16-bit, max Laplacian is ~262140, sq max is ~68B.
    // Gate = (max_pixel_value * 0.02)^2 â€” targets gradients above 2% of full range.
    let max_val = if bpp > 8 { 65535.0f64 } else { 255.0f64 };
    let gate_threshold = max_val * 0.015; // 1.5% of dynamic range
    let noise_gate_sq: u64 = (gate_threshold * gate_threshold * 16.0) as u64; // * 16 because Laplacian has coeff 4

    for y in (roi.y + 2..end_y.saturating_sub(2)).step_by(step) {
        let row_idx = y * width;
        let row_up = (y - 2) * width;
        let row_down = (y + 2) * width;

        for x in (roi.x + 2..end_x.saturating_sub(2)).step_by(step) {
            let idx = row_idx + x;
            let v = get_pixel_value(data, idx, bpp) as i64;
            let v_left = get_pixel_value(data, idx - 2, bpp) as i64;
            let v_right = get_pixel_value(data, idx + 2, bpp) as i64;
            let v_up = get_pixel_value(data, row_up + x, bpp) as i64;
            let v_down = get_pixel_value(data, row_down + x, bpp) as i64;

            // Wide-Kernel Laplacian (Phase 9): Better for filaments
            let lap = (4 * v) - (v_left + v_right + v_up + v_down);
            let sq = (lap * lap) as u64;
            if sq > noise_gate_sq {
                score += sq;
            }
        }
    }
    score
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn calculate_quality_metric_avx2(data: &[u8], width: usize, roi: &Rect, bpp: usize) -> u64 {
    // Only support 8-bit or 16-bit (bpp 8 or 16)
    // If not standard, fallback to scalar? Actually existing get_pixel_value handles packing?
    // Wait, get_pixel_value returns u16.
    // If bpp=8, raw is u8. If bpp=16, raw is u16 (LE/BE?).
    // The scalar code uses `get_pixel_value`.
    // For AVX2 we need direct access.
    // Let's implement for standard raw buffer (u8/u16).

    // Assume data is raw buffer.
    // Width is in PIXELS.
    let byte_width = width * (if bpp > 8 { 2 } else { 1 });
    let step = 2;

    // Safety check: if bpp is weird, fallback (though usually 8 or 16)
    if bpp != 8 && bpp != 16 {
        return calculate_quality_metric_scalar(data, width, roi, bpp);
    }

    let is_16bit = bpp == 16;
    let mut total_score: u64 = 0;

    // Adaptive noise gate (same formula as scalar)
    let max_val = if is_16bit { 65535.0f64 } else { 255.0f64 };
    let gate_threshold = max_val * 0.015;
    let noise_gate_sq: i64 = (gate_threshold * gate_threshold * 16.0) as i64;

    let y_start = roi.y + 2;
    let y_end = (roi.y + roi.h).saturating_sub(2);
    let x_start = roi.x + 2;
    let x_end = (roi.x + roi.w).saturating_sub(2);

    for y in (y_start..y_end).step_by(step) {
        let mut x = x_start;
        // Pointers to rows
        let row_c = data.as_ptr().add(y * byte_width);
        let row_u = data.as_ptr().add((y - 2) * byte_width);
        let row_d = data.as_ptr().add((y + 2) * byte_width);

        // Process 16 pixels at a time (if 16-bit) or 32 (if 8-bit)?
        // Laplacian Needs: Center, Left-2, Right-2, Up-2, Down-2.
        // Stride is 2. So we are skipping every other pixel.
        // If we load continuous block, half are useful.

        let mut row_acc = _mm256_setzero_si256(); // Accumulator for this row

        // Unroll loop
        // We need to load enough data to extract the STRIDED pixels.
        // If we process 8 strided pixels:
        // We need X, X+2, X+4, ... X+14.
        // That spans 16 pixels of width.
        // 16 pixels * 2 bytes = 32 bytes (1 YMM Load).

        while x + 16 <= x_end {
            let offset_c = x * (if is_16bit { 2 } else { 1 });
            let offset_u = offset_c;
            let offset_d = offset_c;

            // Values are u16 always for calc.
            // Load 32 bytes (16 pixels u16, or 32 pixels u8)

            if is_16bit {
                // LOAD 16 Pixels (32 bytes)
                // We only want even indices: 0, 2, 4... (Stride 2)
                // Center
                let vc_raw = _mm256_loadu_si256(row_c.add(offset_c) as *const _); // 16 pixels
                                                                                  // Up
                let vu_raw = _mm256_loadu_si256(row_u.add(offset_u) as *const _);
                // Down
                let vd_raw = _mm256_loadu_si256(row_d.add(offset_d) as *const _);
                // Left-2 (Offset - 4 bytes)
                let vl_raw = _mm256_loadu_si256(row_c.add(offset_c - 4) as *const _);
                // Right-2 (Offset + 4 bytes)
                let vr_raw = _mm256_loadu_si256(row_c.add(offset_c + 4) as *const _);

                // We have 16 pixels in register: [0, 1, 2, 3 ... 15]
                // We want [0, 2, 4, 6, 8, 10, 12, 14] -> 8 pixels.
                // Shuffle/Pack not easy for stride 2 directly for i16?
                // Actually, we can just mask or use specialized blend.
                // Or: Do calculating for ALL 16, and mask out result?
                // Wait, if step is 2, we ONLY sum the strided ones.
                // Calculating all is 2x work but avoids complex shuffling.
                // BUT, Left-2 for pixel 1 is pixel -1 (which might be invalid?).
                // Our loop starts at x+2, so x-2 is valid.
                // But if we load at 'x', pixel 1 needs pixel -1.
                // Pixel 0 needs pixel -2.
                // We loaded at x.
                // vl_raw is loaded at x-2.

                // Let's implement strict Stride-2 logic efficiently.
                // Shuffle to extract even lanes?
                // _mm256_permute4x64_epi64? No.
                // Simple: Masking.
                // But efficient usage would be packing 0,2,4... into dense register.

                // Let's TRY processing ALL 16 and masking out odds?
                // No, Stride 2 means we skip pixels entirely in the loop `step_by(2)`.
                // So efficiently we care about 0, 2, 4...
                // If we compute all, we waste 50%.

                // BETTER: Gather/Pack 8 "Even" pixels into one register.
                // For u16:
                // Lo: indices 0, 2, 4, 6
                // Hi: indices 8, 10, 12, 14

                // _mm256_packs_epi32? No.
                // There isn't a single instruction to pack evens from u16.
                // Manual shuffle is fast enough.

                // Let's rely on calculating all 8 at once using extracts.
                // Or just do a simplified valid loading.

                // Let's unpack to i32 for math (overflow protection for laplacian).
                // 4 * v can exceed u16? No, 4*65535 fits in i32.
                // Difference can be negative.

                // STRATEGY:
                // 1. Load Center, Up, Down, Left, Right vectors. (All 16 pixels).
                // 2. Compute Laplacian for ALL 16.
                // 3. Square.
                // 4. MASK out odd pixels. (Set to 0).
                // 5. Accumulate.

                // CONVERSION TO i32:
                // 16 pixels -> Two registers of i32 (8 + 8).

                let v_c_lo = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(vc_raw)); // First 8
                let v_c_hi = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(vc_raw, 1)); // Next 8

                let v_u_lo = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(vu_raw));
                let v_u_hi = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(vu_raw, 1));

                let v_d_lo = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(vd_raw));
                let v_d_hi = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(vd_raw, 1));

                let v_l_lo = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(vl_raw));
                let v_l_hi = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(vl_raw, 1));

                let v_r_lo = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(vr_raw));
                let v_r_hi = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(vr_raw, 1));

                // CALC LO (Pixels 0..7)
                // Lap = 4*C - (U+D+L+R)
                let sum_neigh_lo = _mm256_add_epi32(
                    _mm256_add_epi32(v_u_lo, v_d_lo),
                    _mm256_add_epi32(v_l_lo, v_r_lo),
                );
                let v_4c_lo = _mm256_slli_epi32(v_c_lo, 2); // C * 4
                let lap_lo = _mm256_sub_epi32(v_4c_lo, sum_neigh_lo);

                // CALC HI (Pixels 8..15)
                let sum_neigh_hi = _mm256_add_epi32(
                    _mm256_add_epi32(v_u_hi, v_d_hi),
                    _mm256_add_epi32(v_l_hi, v_r_hi),
                );
                let v_4c_hi = _mm256_slli_epi32(v_c_hi, 2);
                let lap_hi = _mm256_sub_epi32(v_4c_hi, sum_neigh_hi);
                // Wait. Laplacian can be ~ 4*65535.
                // (4*65535)^2 = 6.8e10. Exceeds i32 (2e9).
                // PROBLEM: Squared result overflows i32.
                // Need 64-bit for Square? Yes.

                // OPTIMIZACION:
                // Usually differences are small (edges).
                // But "Noise Gate" logic implies we only care about big ones.
                // If diff > sqrt(2^31), it overflows. Sqrt(2e9) ~ 46000.
                // Difference CAN differ by 65535.

                // Since this is generic quality metric, maybe we can assume pixels don't max out massively?
                // Or validly extend to 64 bit?
                // AVX2 64-bit mul? _mm256_mul_epu32 (Low 32->64).

                // Let's handle LO register (8 pixels) -> Split into TWO 64-bit registers (4+4).
                // 0, 1, 2, 3 -> Q0 (Even indices of i32 vec)
                // But we only care about EVEN PIXELS (0, 2, 4, 6).
                // We don't care about 1, 3, 5, 7.
                // Lucky! _mm256_mul_epu32 multipliers operands [0], [2], [4], [6] automatically!
                // It treats data as 4 x 64bit, multiplying the lower 32 bits of each.
                // The lower 32 bits of 64-bit lane 0 is Pixel 0.
                // The lower 32 bits of 64-bit lane 1 is Pixel 2.
                // EXACTLY WHAT WE NEED FOR STRIDE 2!

                // So if we used _mm256_cvtepu16_epi32, we have:
                // Lane 0: P0
                // Lane 1: P1
                // Lane 2: P2...
                // _mm256_mul_epu32(a, a) will separate:
                // Res0 = P0 * P0
                // Res1 = P2 * P2
                // Res2 = P4 * P4
                // Res3 = P6 * P6
                // IT AUTOMATICALLY SKIPS ODDS (P1, P3...)!
                // THIS IS PERFECT FOR step_by(2).

                let sq_64_lo = _mm256_mul_epu32(lap_lo, lap_lo); // P0, P2, P4, P6
                let sq_64_hi = _mm256_mul_epu32(lap_hi, lap_hi); // P8, P10, P12, P14

                // Noise Gate Comparison (in 64 bit?)
                // Comparison in 64-bit AVX2 is ... hard. _mm256_cmpgt_epi64 (AVX512 only usually, AVX2 is limited)
                // ACtually AVX2 has _mm256_cmpgt_epi64.
                let v_gate_64 = _mm256_set1_epi64x(noise_gate_sq);
                let mask_lo = _mm256_cmpgt_epi64(sq_64_lo, v_gate_64);
                let mask_hi = _mm256_cmpgt_epi64(sq_64_hi, v_gate_64);

                // Mask and Add
                // Blend/And
                let val_lo = _mm256_and_si256(sq_64_lo, mask_lo);
                let val_hi = _mm256_and_si256(sq_64_hi, mask_hi);

                row_acc = _mm256_add_epi64(row_acc, val_lo);
                row_acc = _mm256_add_epi64(row_acc, val_hi);
            } else {
                // 8-BIT Version (Similar logic)
                // ... (Implementation for u8 if needed, but usually bpp=8 means mono u8)
                // Assuming u8 input.
                // Load 16 bytes -> extend to i32?
                // Same logic.

                // Implementing correctly for 8-bit is important.

                // OK, implement:
                // Load offsets +2/-2 ...
                // Similar expansion to i32.
                let vu_raw = _mm256_cvtepu8_epi16(_mm_loadu_si128(row_u.add(x) as *const _));
                let vd_raw = _mm256_cvtepu8_epi16(_mm_loadu_si128(row_d.add(x) as *const _));
                let vl_raw = _mm256_cvtepu8_epi16(_mm_loadu_si128(row_c.add(x - 2) as *const _));
                let vr_raw = _mm256_cvtepu8_epi16(_mm_loadu_si128(row_c.add(x + 2) as *const _));
                let vc_raw = _mm256_cvtepu8_epi16(_mm_loadu_si128(row_c.add(x) as *const _));

                // Now they are u16. Same pipeline as 16-bit input but values are smaller.
                let v_c_lo = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(vc_raw));
                let v_c_hi = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(vc_raw, 1));

                let v_u_lo = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(vu_raw));
                let v_u_hi = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(vu_raw, 1));

                let v_d_lo = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(vd_raw));
                let v_d_hi = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(vd_raw, 1));

                let v_l_lo = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(vl_raw));
                let v_l_hi = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(vl_raw, 1));

                let v_r_lo = _mm256_cvtepu16_epi32(_mm256_castsi256_si128(vr_raw));
                let v_r_hi = _mm256_cvtepu16_epi32(_mm256_extracti128_si256(vr_raw, 1));

                let sum_neigh_lo = _mm256_add_epi32(
                    _mm256_add_epi32(v_u_lo, v_d_lo),
                    _mm256_add_epi32(v_l_lo, v_r_lo),
                );
                let v_4c_lo = _mm256_slli_epi32(v_c_lo, 2);
                let lap_lo = _mm256_sub_epi32(v_4c_lo, sum_neigh_lo);

                let sum_neigh_hi = _mm256_add_epi32(
                    _mm256_add_epi32(v_u_hi, v_d_hi),
                    _mm256_add_epi32(v_l_hi, v_r_hi),
                );
                let v_4c_hi = _mm256_slli_epi32(v_c_hi, 2);
                let lap_hi = _mm256_sub_epi32(v_4c_hi, sum_neigh_hi);

                let sq_64_lo = _mm256_mul_epu32(lap_lo, lap_lo);
                let sq_64_hi = _mm256_mul_epu32(lap_hi, lap_hi);

                let v_gate_64 = _mm256_set1_epi64x(noise_gate_sq);
                let mask_lo = _mm256_cmpgt_epi64(sq_64_lo, v_gate_64);
                let mask_hi = _mm256_cmpgt_epi64(sq_64_hi, v_gate_64);

                let val_lo = _mm256_and_si256(sq_64_lo, mask_lo);
                let val_hi = _mm256_and_si256(sq_64_hi, mask_hi);

                row_acc = _mm256_add_epi64(row_acc, val_lo);
                row_acc = _mm256_add_epi64(row_acc, val_hi);
            }

            x += 16;
        }

        // Reduce ROW Accumulator (4x u64)
        // [A, B, C, D] -> Sum
        let v1 = _mm256_castsi256_si128(row_acc);
        let v2 = _mm256_extracti128_si256(row_acc, 1);
        let v_sum = _mm_add_epi64(v1, v2); // [A+C, B+D]
                                           // HADD not available for epi64 in AVX2?
                                           // _mm_extract_epi64 is fine.
        let low = _mm_cvtsi128_si64(v_sum);
        let high = _mm_extract_epi64(v_sum, 1);
        total_score += (low as u64) + (high as u64);

        // TAIL LOOP (Scalar)
        while x < x_end {
            // (Same as scalar implementation)
            // FIX: Color-Aware Gradient (Bayer Killer for Focus)
            // Compare Pixel(x) vs Pixel(x-2) to stay on the SAME color channel (e.g. Red vs Red).
            let idx = y * width + x;
            let row_up = (y - 2) * width;
            let row_down = (y + 2) * width;

            let v = get_pixel_value(data, idx, bpp) as i64;
            let v_left = get_pixel_value(data, idx - 2, bpp) as i64;
            let v_right = get_pixel_value(data, idx + 2, bpp) as i64;
            let v_up = get_pixel_value(data, row_up + x, bpp) as i64;
            let v_down = get_pixel_value(data, row_down + x, bpp) as i64;

            let lap = (4 * v) - (v_left + v_right + v_up + v_down);
            let sq = (lap * lap) as u64;
            if sq > noise_gate_sq as u64 {
                total_score += sq;
            }
            x += step;
        }
    }

    total_score
}

fn calculate_quality_metric_from_buffer(data: &[u16], width: usize, height: usize) -> u64 {
    let mut score: u64 = 0;
    // Noise Gate optimized for u16 (0-65535)
    // 2000^2 = 4M (Asegura que el ruido del "Seeing" no simule nitidez)
    let noise_gate_sq: u64 = 4_000_000;
    let step = 2; // Speed optimization (Check every 2nd pixel)

    // Margins: 2 pixels to be safe with step
    for y in (2..height.saturating_sub(2)).step_by(step) {
        let row = y * width;
        let row_up = (y - 1) * width;
        let row_down = (y + 1) * width;

        for x in (2..width.saturating_sub(2)).step_by(step) {
            let idx = row + x;

            let v = data[idx] as i64;
            let v_left = data[idx - 1] as i64;
            let v_right = data[idx + 1] as i64;
            let v_up = data[row_up + x] as i64;
            let v_down = data[row_down + x] as i64;

            // Standard Laplacian (Star-kernel)
            let lap = (4 * v) - (v_left + v_right + v_up + v_down);
            let sq = (lap * lap) as u64;

            if sq > noise_gate_sq {
                score += sq;
            }
        }
    }
    score
}

fn calculate_quality_metric_u16_roi(data: &[u16], width: usize, height: usize, roi: &Rect) -> u64 {
    let mut score: u64 = 0;
    let noise_gate_sq: u64 = 4_000_000;
    let step = 2;

    let start_y = roi.y.max(2);
    let end_y = (roi.y + roi.h).min(height.saturating_sub(2));
    let start_x = roi.x.max(2);
    let end_x = (roi.x + roi.w).min(width.saturating_sub(2));

    if start_y >= end_y || start_x >= end_x {
        return 0;
    }

    for y in (start_y..end_y).step_by(step) {
        let row = y * width;
        let row_up = (y - 1) * width;
        let row_down = (y + 1) * width;

        for x in (start_x..end_x).step_by(step) {
            let idx = row + x;
            let v = data[idx] as i64;
            let v_left = data[idx - 1] as i64;
            let v_right = data[idx + 1] as i64;
            let v_up = data[row_up + x] as i64;
            let v_down = data[row_down + x] as i64;

            let lap = (4 * v) - (v_left + v_right + v_up + v_down);
            let sq = (lap * lap) as u64;
            if sq > noise_gate_sq {
                score += sq;
            }
        }
    }
    score
}

fn calculate_entropy_u16_roi(data: &[u16], width: usize, height: usize, roi: &Rect) -> u64 {
    // Spatial Entropy: Measures local variation / information density
    // For 16-bit, we downsample intensity to 8-bit for the histogram to avoid sparse bins
    let mut hist = [0u32; 256];
    let mut total_pixels = 0;

    let start_y = roi.y;
    let end_y = (roi.y + roi.h).min(height);
    let start_x = roi.x;
    let end_x = (roi.x + roi.w).min(width);

    for y in start_y..end_y {
        let row = y * width;
        for x in start_x..end_x {
            let val = (data[row + x] >> 8) as usize; // 8-bit quantization
            hist[val] += 1;
            total_pixels += 1;
        }
    }

    if total_pixels == 0 {
        return 0;
    }

    let mut entropy = 0.0f64;
    let total_f = total_pixels as f64;
    for &count in &hist {
        if count > 0 {
            let p = count as f64 / total_f;
            entropy -= p * p.log2();
        }
    }

    // Scale entropy (0-8 bits) to match Laplacian magnitude order (~10^7)
    (entropy * 10_000_000.0) as u64
}

fn calculate_grid_quality(
    mono: &[u16],
    width: usize,
    height: usize,
    grid_size: usize,
    is_surface: bool,
) -> Vec<u64> {
    if is_surface {
        let mut grid_scores = vec![0u64; grid_size * grid_size];
        // Consistent with enhance_and_score_surface_buffered's noise gate: lap > 100 -> sq > 10,000
        let noise_gate_sq: u64 = 10_000;
        let step = if width > 1200 || height > 1200 { 4 } else { 2 };

        let start_y = 2;
        let end_y = height.saturating_sub(2);
        let start_x = 2;
        let end_x = width.saturating_sub(2);

        for y in (start_y..end_y).step_by(step) {
            let row = y * width;
            let row_up = (y - 1) * width;
            let row_down = (y + 1) * width;

            for x in (start_x..end_x).step_by(step) {
                let idx = row + x;

                // 3x3 Laplacian: 8 * Center - sum of 8 neighbors
                let c = mono[idx] as i32;
                let n_sum = mono[idx - 1] as i32
                    + mono[idx + 1] as i32
                    + mono[row_up + x] as i32
                    + mono[row_down + x] as i32
                    + mono[row_up + x - 1] as i32
                    + mono[row_up + x + 1] as i32
                    + mono[row_down + x - 1] as i32
                    + mono[row_down + x + 1] as i32;

                let lap = (c * 8 - n_sum).abs();
                let sq = (lap as u64) * (lap as u64);

                if sq > noise_gate_sq {
                    // Partición entera determinista: evita que CPU y GPU
                    // asignen un píxel de frontera a celdas distintas por el
                    // redondeo de `width/grid` en f32.
                    let gx = (x.saturating_mul(grid_size) / width.max(1)).min(grid_size - 1);
                    let gy = (y.saturating_mul(grid_size) / height.max(1)).min(grid_size - 1);
                    let grid_idx = gy * grid_size + gx;
                    grid_scores[grid_idx] += sq;
                }
            }
        }
        grid_scores
    } else {
        let mut grid_scores = Vec::with_capacity(grid_size * grid_size);
        let tile_w = width / grid_size;
        let tile_h = height / grid_size;

        for gy in 0..grid_size {
            for gx in 0..grid_size {
                let x0 = gx * tile_w.max(1);
                let y0 = gy * tile_h.max(1);
                let roi = Rect {
                    x: x0,
                    y: y0,
                    w: tile_w.max(1),
                    h: tile_h.max(1),
                };

                let lap = calculate_quality_metric_u16_roi(mono, width, height, &roi);
                grid_scores.push(lap);
            }
        }
        grid_scores
    }
}

fn calculate_stabilized_center(
    data: &[u16],
    width: usize,
    height: usize,
    is_surface: bool,
) -> (usize, usize) {
    // FIX: Si estamos en modo Surface, devolvemos el centro geometrico para evitar saltos
    // por cambios de brillo en la superficie (atmosfera).
    if is_surface {
        return (width / 2, height / 2);
    }

    let mut max_val = 0;
    for i in (0..data.len()).step_by(3) {
        // Fix: Check all channels for max intensity, not just Green
        let r = data[i];
        let g = data[i + 1];
        let b = data[i + 2];
        let v = r.max(g).max(b);
        if v > max_val {
            max_val = v;
        }
    }
    // Modificado: Usar Max(R,G,B) para mejor deteccion en cualquier color y umbral mas bajo (10%)
    let threshold = (max_val as f32 * 0.10) as u16;
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    let mut total_mass = 0.0;
    for y in 0..height {
        let row_off = y * width;
        for x in 0..width {
            let idx = (row_off + x) * 3;
            // Usar maximo de canales para mayor sensibilidad
            let r = data[idx] as f32;
            let g = data[idx + 1] as f32;
            let b = data[idx + 2] as f32;
            let val = r.max(g).max(b);

            if val > threshold as f32 {
                sum_x += x as f32 * val;
                sum_y += y as f32 * val;
                total_mass += val;
            }
        }
    }
    // FIX: Si es imagen muy grande (>2000px) y no tiene un pico claro, asumimos Surface/Solar y devolvemos centro
    // Esto evita que 'calculate_stabilized_center' salte erraticamente en superficies uniformes
    if total_mass == 0.0 {
        return (width / 2, height / 2);
    }
    // Si la masa esta muy distribuida (Superficie Solar), el centro de brillo es el centro geometrico aprox
    // Comprobacion simple: si el centro calculado esta muy lejos del centro geometrico (> 30% del ancho), desconfiamos
    let cx_raw = sum_x / total_mass;
    let cy_raw = sum_y / total_mass;

    // Safety clamp relax: Allow up to 85% deviation (near edge) to support sudden movements
    // Before: 0.6 (30% from center). Now: 0.85 (42.5% from center, i.e. 7.5% from edge)
    let center_x = width as f32 / 2.0;
    let center_y = height as f32 / 2.0;
    if (cx_raw - center_x).abs() > center_x * 0.85 || (cy_raw - center_y).abs() > center_y * 0.85 {
        return (width / 2, height / 2);
    }

    let cx = cx_raw.round() as usize;
    let cy = cy_raw.round() as usize;
    (cx.clamp(0, width - 1), cy.clamp(0, height - 1))
}

#[allow(dead_code)]
fn auto_center_crop_batch(
    data: &[u16],
    width: usize,
    height: usize,
    output_size: usize,
) -> (Vec<u16>, usize, usize) {
    let (cx, cy) = calculate_stabilized_center(data, width, height, false);
    let mut out = vec![0u16; output_size * output_size * 3];
    let half_out = output_size as isize / 2;
    let offset_x = half_out - cx as isize;
    let offset_y = half_out - cy as isize;
    for y in 0..height {
        let dst_y = y as isize + offset_y;
        if dst_y >= 0 && dst_y < output_size as isize {
            let row_src = y * width;
            let row_dst = (dst_y as usize) * output_size;
            for x in 0..width {
                let dst_x = x as isize + offset_x;
                if dst_x >= 0 && dst_x < output_size as isize {
                    let idx_src = (row_src + x) * 3;
                    let idx_dst = (row_dst + dst_x as usize) * 3;
                    unsafe {
                        let r = *data.get_unchecked(idx_src);
                        let g = *data.get_unchecked(idx_src + 1);
                        let b = *data.get_unchecked(idx_src + 2);
                        *out.get_unchecked_mut(idx_dst) = r;
                        *out.get_unchecked_mut(idx_dst + 1) = g;
                        *out.get_unchecked_mut(idx_dst + 2) = b;
                    }
                }
            }
        }
    }
    (out, output_size, output_size)
}

fn get_area_brightness(
    data: &[u8],
    width: usize,
    _height: usize,
    bpp: usize,
    cx: usize,
    cy: usize,
    size: usize,
) -> f32 {
    let h = size / 2;
    let sx = cx.saturating_sub(h);
    let sy = cy.saturating_sub(h);
    let ex = (cx + h).min(width);
    let ey = (cy + h).min(width);
    let mut s = 0.0;
    let mut c = 0.0;
    for y in (sy..ey).step_by(4) {
        if y * width + ex >= data.len() {
            continue;
        }
        for x in (sx..ex).step_by(4) {
            s += get_pixel_value(data, y * width + x, bpp) as f32;
            c += 1.0;
        }
    }
    if c == 0.0 {
        0.0
    } else {
        s / c
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn calculate_quality_metric_neon(data: &[u8], width: usize, roi: &Rect, bpp: usize) -> u64 {
    use std::arch::aarch64::*;

    // Only support 8-bit or 16-bit (bpp 8 or 16)
    if bpp != 8 && bpp != 16 {
        return calculate_quality_metric_scalar(data, width, roi, bpp);
    }

    let is_16bit = bpp == 16;
    let byte_width = width * (if is_16bit { 2 } else { 1 });
    let step = 2;

    let mut total_score: u64 = 0;

    // Adaptive noise gate (same formula as scalar)
    let max_val = if is_16bit { 65535.0f64 } else { 255.0f64 };
    let gate_threshold = max_val * 0.015;
    let noise_gate_sq = (gate_threshold * gate_threshold * 16.0) as i64;

    let y_start = roi.y + 2;
    let y_end = (roi.y + roi.h).saturating_sub(2);
    let x_start = roi.x + 2;
    let x_end = (roi.x + roi.w).saturating_sub(2);

    let v_gate_64 = vdupq_n_s64(noise_gate_sq);

    for y in (y_start..y_end).step_by(step) {
        let mut x = x_start;
        let row_c = data.as_ptr().add(y * byte_width);
        let row_u = data.as_ptr().add((y - 2) * byte_width);
        let row_d = data.as_ptr().add((y + 2) * byte_width);

        let mut row_acc = vdupq_n_u64(0);

        while x + 16 <= x_end {
            let offset_c = x * (if is_16bit { 2 } else { 1 });
            let offset_u = offset_c;
            let offset_d = offset_c;

            // Load and unzip even elements to get 8 even pixels of uint16x8_t
            let vc_even: uint16x8_t;
            let vu_even: uint16x8_t;
            let vd_even: uint16x8_t;
            let vl_even: uint16x8_t;
            let vr_even: uint16x8_t;

            if is_16bit {
                let vc1 = vld1q_u16(row_c.add(offset_c) as *const u16);
                let vc2 = vld1q_u16(row_c.add(offset_c + 16) as *const u16);
                vc_even = vuzp1q_u16(vc1, vc2);

                let vu1 = vld1q_u16(row_u.add(offset_u) as *const u16);
                let vu2 = vld1q_u16(row_u.add(offset_u + 16) as *const u16);
                vu_even = vuzp1q_u16(vu1, vu2);

                let vd1 = vld1q_u16(row_d.add(offset_d) as *const u16);
                let vd2 = vld1q_u16(row_d.add(offset_d + 16) as *const u16);
                vd_even = vuzp1q_u16(vd1, vd2);

                let vl1 = vld1q_u16(row_c.add(offset_c - 4) as *const u16);
                let vl2 = vld1q_u16(row_c.add(offset_c + 12) as *const u16);
                vl_even = vuzp1q_u16(vl1, vl2);

                let vr1 = vld1q_u16(row_c.add(offset_c + 4) as *const u16);
                let vr2 = vld1q_u16(row_c.add(offset_c + 20) as *const u16);
                vr_even = vuzp1q_u16(vr1, vr2);
            } else {
                let vc1 = vld1q_u8(row_c.add(offset_c));
                let vc_even_u8 = vget_low_u8(vuzp1q_u8(vc1, vc1));
                vc_even = vmovl_u8(vc_even_u8);

                let vu1 = vld1q_u8(row_u.add(offset_u));
                let vu_even_u8 = vget_low_u8(vuzp1q_u8(vu1, vu1));
                vu_even = vmovl_u8(vu_even_u8);

                let vd1 = vld1q_u8(row_d.add(offset_d));
                let vd_even_u8 = vget_low_u8(vuzp1q_u8(vd1, vd1));
                vd_even = vmovl_u8(vd_even_u8);

                let vl1 = vld1q_u8(row_c.add(offset_c - 2));
                let vl_even_u8 = vget_low_u8(vuzp1q_u8(vl1, vl1));
                vl_even = vmovl_u8(vl_even_u8);

                let vr1 = vld1q_u8(row_c.add(offset_c + 2));
                let vr_even_u8 = vget_low_u8(vuzp1q_u8(vr1, vr1));
                vr_even = vmovl_u8(vr_even_u8);
            }

            // Convert to signed 16-bit to widen to signed 32-bit safely
            let vc_s16 = vreinterpretq_s16_u16(vc_even);
            let vu_s16 = vreinterpretq_s16_u16(vu_even);
            let vd_s16 = vreinterpretq_s16_u16(vd_even);
            let vl_s16 = vreinterpretq_s16_u16(vl_even);
            let vr_s16 = vreinterpretq_s16_u16(vr_even);

            // Widen to signed 32-bit registers (Low/High halves)
            let vc_lo = vmovl_s16(vget_low_s16(vc_s16));
            let vc_hi = vmovl_s16(vget_high_s16(vc_s16));

            let vu_lo = vmovl_s16(vget_low_s16(vu_s16));
            let vu_hi = vmovl_s16(vget_high_s16(vu_s16));

            let vd_lo = vmovl_s16(vget_low_s16(vd_s16));
            let vd_hi = vmovl_s16(vget_high_s16(vd_s16));

            let vl_lo = vmovl_s16(vget_low_s16(vl_s16));
            let vl_hi = vmovl_s16(vget_high_s16(vl_s16));

            let vr_lo = vmovl_s16(vget_low_s16(vr_s16));
            let vr_hi = vmovl_s16(vget_high_s16(vr_s16));

            // Laplacian: 4 * C - (U + D + L + R)
            // Low half
            let sum_ud_lo = vaddq_s32(vu_lo, vd_lo);
            let sum_lr_lo = vaddq_s32(vl_lo, vr_lo);
            let sum_neigh_lo = vaddq_s32(sum_ud_lo, sum_lr_lo);
            let v_4c_lo = vshlq_n_s32(vc_lo, 2);
            let lap_lo = vsubq_s32(v_4c_lo, sum_neigh_lo);

            // High half
            let sum_ud_hi = vaddq_s32(vu_hi, vd_hi);
            let sum_lr_hi = vaddq_s32(vl_hi, vr_hi);
            let sum_neigh_hi = vaddq_s32(sum_ud_hi, sum_lr_hi);
            let v_4c_hi = vshlq_n_s32(vc_hi, 2);
            let lap_hi = vsubq_s32(v_4c_hi, sum_neigh_hi);

            // Square: lap^2 (widened to 64-bit to prevent overflow)
            // Low half -> 2 x 64-bit registers
            let sq0 = vmull_s32(vget_low_s32(lap_lo), vget_low_s32(lap_lo));
            let sq1 = vmull_s32(vget_high_s32(lap_lo), vget_high_s32(lap_lo));

            // High half -> 2 x 64-bit registers
            let sq2 = vmull_s32(vget_low_s32(lap_hi), vget_low_s32(lap_hi));
            let sq3 = vmull_s32(vget_high_s32(lap_hi), vget_high_s32(lap_hi));

            // Noise gate threshold check
            let mask0 = vcgtq_s64(sq0, v_gate_64);
            let mask1 = vcgtq_s64(sq1, v_gate_64);
            let mask2 = vcgtq_s64(sq2, v_gate_64);
            let mask3 = vcgtq_s64(sq3, v_gate_64);

            let val0 = vandq_u64(vreinterpretq_u64_s64(sq0), mask0);
            let val1 = vandq_u64(vreinterpretq_u64_s64(sq1), mask1);
            let val2 = vandq_u64(vreinterpretq_u64_s64(sq2), mask2);
            let val3 = vandq_u64(vreinterpretq_u64_s64(sq3), mask3);

            row_acc = vaddq_u64(row_acc, val0);
            row_acc = vaddq_u64(row_acc, val1);
            row_acc = vaddq_u64(row_acc, val2);
            row_acc = vaddq_u64(row_acc, val3);

            x += 16;
        }

        // Reduce Row Accumulator (2x u64)
        let mut lanes = [0u64; 2];
        vst1q_u64(lanes.as_mut_ptr(), row_acc);
        total_score += lanes[0] + lanes[1];

        // Tail Loop (Scalar)
        while x < x_end {
            let idx = y * width + x;
            let v = get_pixel_value(data, idx, bpp) as i64;
            let v_left = get_pixel_value(data, idx - 2, bpp) as i64;
            let v_right = get_pixel_value(data, idx + 2, bpp) as i64;
            let v_up = get_pixel_value(data, (y - 2) * width + x, bpp) as i64;
            let v_down = get_pixel_value(data, (y + 2) * width + x, bpp) as i64;

            let lap = (4 * v) - (v_left + v_right + v_up + v_down);
            let sq = (lap * lap) as u64;
            if sq > noise_gate_sq as u64 {
                total_score += sq;
            }
            x += step;
        }
    }

    total_score
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_neon_vs_scalar_quality_metric() {
        let mut data = vec![0u8; 1000];
        for i in 0..data.len() {
            data[i] = (i * 17) as u8;
        }

        let width = 20;
        let roi = Rect { x: 4, y: 4, w: 12, h: 12 };

        // Test 8-bit
        let scalar_score_8 = calculate_quality_metric_scalar(&data, width, &roi, 8);
        #[cfg(target_arch = "aarch64")]
        let neon_score_8 = unsafe { calculate_quality_metric_neon(&data, width, &roi, 8) };
        #[cfg(target_arch = "x86_64")]
        let neon_score_8 = scalar_score_8; // Dummy for non-neon target check
        assert_eq!(scalar_score_8, neon_score_8);

        // Test 16-bit
        let scalar_score_16 = calculate_quality_metric_scalar(&data, width, &roi, 16);
        #[cfg(target_arch = "aarch64")]
        let neon_score_16 = unsafe { calculate_quality_metric_neon(&data, width, &roi, 16) };
        #[cfg(target_arch = "x86_64")]
        let neon_score_16 = scalar_score_16; // Dummy for non-neon target check
        assert_eq!(scalar_score_16, neon_score_16);
    }

    #[test]
    fn test_calculate_grid_quality() {
        // Create dummy image data: 120 x 120 pixels, u16 format
        let width = 120;
        let height = 120;
        let mut mono = vec![0u16; width * height];

        // Put a simulated grid/spot of sharp detail in the middle
        for y in 40..80 {
            for x in 40..80 {
                mono[y * width + x] = if (x + y) % 2 == 0 { 65000 } else { 100 };
            }
        }

        // 1. Run planetary/fallback path
        let scores_planetary = calculate_grid_quality(&mono, width, height, 4, false);
        assert_eq!(scores_planetary.len(), 16);

        // 2. Run optimized surface path
        let scores_surface = calculate_grid_quality(&mono, width, height, 4, true);
        assert_eq!(scores_surface.len(), 16);

        // Verify that the highest scores are indeed in the middle tiles (indices 5, 6, 9, 10 for a 4x4 grid)
        let max_planetary_idx = scores_planetary.iter().enumerate().max_by_key(|(_, &s)| s).unwrap().0;
        let max_surface_idx = scores_surface.iter().enumerate().max_by_key(|(_, &s)| s).unwrap().0;

        assert!(max_planetary_idx == 5 || max_planetary_idx == 6 || max_planetary_idx == 9 || max_planetary_idx == 10);
        assert!(max_surface_idx == 5 || max_surface_idx == 6 || max_surface_idx == 9 || max_surface_idx == 10);
    }
}

include!("debayer.rs");
