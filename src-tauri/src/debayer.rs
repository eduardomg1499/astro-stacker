// ==========================================
// 4. DEBAYER
// ==========================================

fn debayer_into_buffer(
    input: &[u16],
    width: usize,
    height: usize,
    color_id: i32,
    out: &mut Vec<u16>,
) {
    out.clear();
    let target_size = width * height * 3;
    if out.capacity() < target_size {
        out.reserve(target_size);
    }
    // SAFE FIX: Initialize with zeros to avoid garbage in borders
    out.resize(target_size, 0);

    if input.len() == target_size {
        if ser::ser_color_is_direct_bgr(color_id) {
            for (dst, src) in out.chunks_exact_mut(3).zip(input.chunks_exact(3)) {
                dst[0] = src[2];
                dst[1] = src[1];
                dst[2] = src[0];
            }
        } else {
            out.copy_from_slice(input);
        }
        return;
    }

    let (rx, ry) = match color_id {
        8 => (0, 0),
        9 => (1, 0),
        10 => (0, 1),
        11 => (1, 1),
        12 | 20 | 102 | 103 => {
            // YUY2 / YUYV / UYVY Handling (Supported IDs: 12, 20, 102, 103)
            // Fix: Banding often occurs due to wrong U/V order or 16-bit misalignment.
            let ptr_out = out.as_mut_ptr();
            let size = input.len();
            let swap_uv = color_id == 103;

            let mut i = 0;
            while i < size - 1 {
                let p0 = unsafe { *input.get_unchecked(i) };
                let p1 = unsafe { *input.get_unchecked(i + 1) };

                // Extract YUV components (BT.601 Full Range)
                let (y0, u, y1, v) = if !swap_uv {
                    (
                        (p0 & 0xFF) as f32,
                        ((p0 >> 8) & 0xFF) as f32 - 128.0,
                        (p1 & 0xFF) as f32,
                        ((p1 >> 8) & 0xFF) as f32 - 128.0,
                    )
                } else {
                    // Try alternate packing if standard causes banding
                    (
                        ((p0 >> 8) & 0xFF) as f32,
                        (p0 & 0xFF) as f32 - 128.0,
                        ((p1 >> 8) & 0xFF) as f32,
                        (p1 & 0xFF) as f32 - 128.0,
                    )
                };

                let r0 = (y0 + 1.402 * v).clamp(0.0, 255.0) as u16 * 257;
                let g0 = (y0 - 0.3441 * u - 0.7141 * v).clamp(0.0, 255.0) as u16 * 257;
                let b0 = (y0 + 1.772 * u).clamp(0.0, 255.0) as u16 * 257;

                let r1 = (y1 + 1.402 * v).clamp(0.0, 255.0) as u16 * 257;
                let g1 = (y1 - 0.3441 * u - 0.7141 * v).clamp(0.0, 255.0) as u16 * 257;
                let b1 = (y1 + 1.772 * u).clamp(0.0, 255.0) as u16 * 257;

                unsafe {
                    let dest = ptr_out.add(i * 3);
                    *dest = r0;
                    *dest.add(1) = g0;
                    *dest.add(2) = b0;
                    let dest2 = ptr_out.add((i + 1) * 3);
                    *dest2 = r1;
                    *dest2.add(1) = g1;
                    *dest2.add(2) = b1;
                }
                i += 2;
            }
            return;
        }
        105 | 200 => {
            // MJPG / Reserved Color (Experimental)
            // If MJPG in SER, frames are compressed or high-bitdepth YUV.
            // Safest fallback to avoid bands is Grayscale Y until a full jpeg decoder is in place.
            let ptr_out = out.as_mut_ptr();
            for (i, &val) in input.iter().enumerate() {
                let y = (val & 0xFF) as u16 * 257;
                unsafe {
                    *ptr_out.add(i * 3) = y;
                    *ptr_out.add(i * 3 + 1) = y;
                    *ptr_out.add(i * 3 + 2) = y;
                }
            }
            return;
        }
        _ => {
            // Default: Grayscale Copy
            let ptr_out = out.as_mut_ptr();
            for (i, &val) in input.iter().enumerate() {
                unsafe {
                    *ptr_out.add(i * 3) = val;
                    *ptr_out.add(i * 3 + 1) = val;
                    *ptr_out.add(i * 3 + 2) = val;
                }
            }
            return;
        }
    };

    // OPTIMIZACION (bit-identica): aritmetica ENTERA en vez de f32.
    // `((a+b+c+d) as f32 * 0.25) as u16` == `(a+b+c+d) >> 2`, y `((a+b) as f32
    // * 0.5) as u16` == `(a+b) >> 1` para sumas de u16 no negativas (el truncado
    // a u16 es el floor de la division exacta). Mismo flujo de control que la
    // version f32 previa -> resultado IDENTICO, pero sin conversiones f32 y con
    // mejor auto-vectorizacion. Validado en el test `debayer_integer_matches_f32`.
    let ptr_in = input.as_ptr();
    let ptr_out = out.as_mut_ptr();
    for y in 1..height - 1 {
        let row_offset = y * width;
        let prev_row = (y - 1) * width;
        let next_row = (y + 1) * width;
        for x in 1..width - 1 {
            let idx = row_offset + x;
            let o = idx * 3;
            unsafe {
                let v = *ptr_in.add(idx) as u32;
                let red_pixel = (x % 2 == rx) && (y % 2 == ry);
                let blue_pixel = (x % 2 != rx) && (y % 2 != ry);
                let u = *ptr_in.add(prev_row + x) as u32;
                let d = *ptr_in.add(next_row + x) as u32;
                let l = *ptr_in.add(row_offset + x - 1) as u32;
                let r = *ptr_in.add(row_offset + x + 1) as u32;

                let (cr, cg, cb) = if red_pixel {
                    let c1 = *ptr_in.add(prev_row + x - 1) as u32;
                    let c2 = *ptr_in.add(prev_row + x + 1) as u32;
                    let c3 = *ptr_in.add(next_row + x - 1) as u32;
                    let c4 = *ptr_in.add(next_row + x + 1) as u32;
                    (v, (u + d + l + r) >> 2, (c1 + c2 + c3 + c4) >> 2)
                } else if blue_pixel {
                    let c1 = *ptr_in.add(prev_row + x - 1) as u32;
                    let c2 = *ptr_in.add(prev_row + x + 1) as u32;
                    let c3 = *ptr_in.add(next_row + x - 1) as u32;
                    let c4 = *ptr_in.add(next_row + x + 1) as u32;
                    ((c1 + c2 + c3 + c4) >> 2, (u + d + l + r) >> 2, v)
                } else if y % 2 == ry {
                    ((l + r) >> 1, v, (u + d) >> 1)
                } else {
                    ((u + d) >> 1, v, (l + r) >> 1)
                };
                *ptr_out.add(o) = cr as u16;
                *ptr_out.add(o + 1) = cg as u16;
                *ptr_out.add(o + 2) = cb as u16;
            }
        }
    }
}

#[cfg(test)]
mod debayer_validation {
    use super::*;

    /// Referencia f32 (logica original) para validar que la version entera es
    /// bit-identica.
    fn debayer_bilinear_f32_reference(
        input: &[u16],
        width: usize,
        height: usize,
        rx: usize,
        ry: usize,
    ) -> Vec<u16> {
        let mut out = vec![0u16; width * height * 3];
        for y in 1..height - 1 {
            let row_offset = y * width;
            let prev_row = (y - 1) * width;
            let next_row = (y + 1) * width;
            for x in 1..width - 1 {
                let idx = row_offset + x;
                let o = idx * 3;
                let v = input[idx] as f32;
                let red_pixel = (x % 2 == rx) && (y % 2 == ry);
                let blue_pixel = (x % 2 != rx) && (y % 2 != ry);
                let u = input[prev_row + x] as f32;
                let d = input[next_row + x] as f32;
                let l = input[row_offset + x - 1] as f32;
                let r = input[row_offset + x + 1] as f32;
                let (cr, cg, cb) = if red_pixel {
                    let c1 = input[prev_row + x - 1] as f32;
                    let c2 = input[prev_row + x + 1] as f32;
                    let c3 = input[next_row + x - 1] as f32;
                    let c4 = input[next_row + x + 1] as f32;
                    (v, (u + d + l + r) * 0.25, (c1 + c2 + c3 + c4) * 0.25)
                } else if blue_pixel {
                    let c1 = input[prev_row + x - 1] as f32;
                    let c2 = input[prev_row + x + 1] as f32;
                    let c3 = input[next_row + x - 1] as f32;
                    let c4 = input[next_row + x + 1] as f32;
                    ((c1 + c2 + c3 + c4) * 0.25, (u + d + l + r) * 0.25, v)
                } else if y % 2 == ry {
                    ((l + r) * 0.5, v, (u + d) * 0.5)
                } else {
                    ((u + d) * 0.5, v, (l + r) * 0.5)
                };
                out[o] = cr as u16;
                out[o + 1] = cg as u16;
                out[o + 2] = cb as u16;
            }
        }
        out
    }

    #[test]
    fn debayer_integer_matches_f32() {
        let width = 64usize;
        let height = 48usize;
        // Imagen Bayer sintetica determinista (cubre todo el rango u16).
        let mut input = vec![0u16; width * height];
        let mut s = 0xC0FF_EE12u32;
        for v in input.iter_mut() {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *v = (s >> 16) as u16;
        }
        // Probar los 4 patrones Bayer (color_id 8..11 -> (rx,ry)).
        for &(color_id, rx, ry) in &[(8, 0, 0), (9, 1, 0), (10, 0, 1), (11, 1, 1)] {
            let mut out = Vec::new();
            debayer_into_buffer(&input, width, height, color_id, &mut out);
            let reference = debayer_bilinear_f32_reference(&input, width, height, rx, ry);
            assert_eq!(
                out, reference,
                "debayer entero difiere del f32 para color_id={color_id}"
            );
        }
    }
}

fn debayer_to_rgb(input: &[u16], width: usize, height: usize, color_id: i32) -> Vec<u16> {
    let mut out = Vec::with_capacity(width * height * 3);
    debayer_into_buffer(input, width, height, color_id, &mut out);
    out
}

fn to_8bit_visual(input: &[u16], brightness: f32) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    for &v in input {
        out.push(((v as f32 * brightness / 65535.0).min(1.0) * 255.0) as u8);
    }
    out
}

fn to_8bit_preview_visual(input: &[u16]) -> Vec<u8> {
    // CONSERVATIVE LINEAR PREVIEW (parity with the mono path):
    // high end anchored at the TRUE MAX (burning is impossible by
    // construction) and low end at the 0.05% percentile; strictly linear.
    // The previous p99.5 clip + sqrt gamma lift over-exposed every RGB view.
    if input.is_empty() {
        return Vec::new();
    }

    let max_samples = 200_000usize;
    let step = (input.len() / max_samples).max(1);
    let mut sample: Vec<u16> = input
        .iter()
        .step_by(step)
        .copied()
        .filter(|&v| v > 0)
        .collect();

    if sample.len() < 16 {
        return to_8bit_visual(input, 1.0);
    }
    sample.sort_unstable();

    let low_idx = ((sample.len() - 1) as f32 * 0.0005) as usize;
    let low = sample[low_idx];
    let high = sample[sample.len() - 1]; // true max of the sample
    let range = high.saturating_sub(low);
    if range < 256 {
        return to_8bit_visual(input, 1.0);
    }

    let scale = 255.0 / range as f32;
    let mut out = Vec::with_capacity(input.len());
    for &v in input {
        out.push((v.saturating_sub(low) as f32 * scale).clamp(0.0, 255.0) as u8);
    }
    out
}

fn encode_rgb16_png_data_url(input: &[u16], width: usize, height: usize) -> Result<String, String> {
    if input.len() != width * height * 3 {
        return Err(format!(
            "Buffer RGB16 invalido: {} valores para {}x{}",
            input.len(),
            width,
            height
        ));
    }

    let mut raw_bytes_be = Vec::with_capacity(input.len() * 2);
    for &v in input {
        raw_bytes_be.extend_from_slice(&v.to_be_bytes());
    }

    let mut png = Vec::new();
    image::png::PngEncoder::new(&mut std::io::Cursor::new(&mut png))
        .encode(
            &raw_bytes_be,
            width as u32,
            height as u32,
            image::ColorType::Rgb16,
        )
        .map_err(|e| e.to_string())?;

    use base64::Engine as _;
    Ok(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&png)
    ))
}

fn encode_rgb16_preview_png_data_url(
    input: &[u16],
    width: usize,
    height: usize,
) -> Result<String, String> {
    encode_rgb16_png_data_url(input, width, height)
}

fn encode_mono16_preview_png_data_url(
    input: &[u16],
    width: usize,
    height: usize,
) -> Result<String, String> {
    if input.len() != width * height {
        return Err(format!(
            "Buffer mono16 invalido: {} valores para {}x{}",
            input.len(),
            width,
            height
        ));
    }

    let mut rgb = Vec::with_capacity(width * height * 3);
    for &v in input {
        rgb.push(v);
        rgb.push(v);
        rgb.push(v);
    }
    encode_rgb16_png_data_url(&rgb, width, height)
}

fn encode_dynamic_image_preview_png_data_url(img: &image::DynamicImage) -> Result<String, String> {
    let rgb16 = img.to_rgb16();
    encode_rgb16_png_data_url(
        rgb16.as_raw(),
        rgb16.width() as usize,
        rgb16.height() as usize,
    )
}

