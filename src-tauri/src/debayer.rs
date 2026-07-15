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
            // PR-1.6: con size==0 (frame vacío/truncado) `size - 1` hacía
            // underflow de usize y el get_unchecked leía fuera de límites.
            if size < 2 {
                return;
            }
            let swap_uv = color_id == 103;

            let mut i = 0;
            while i + 1 < size {
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

    // PR-1.6: demosaico MALVAR-HE-CUTLER 5×5 (gradiente-corregido, entero)
    // como ruta por defecto para Bayer. El bilineal producía zipper/falso
    // color en el limbo planetario y bordes lunares de alto contraste — el
    // tipo exacto de artefacto que este producto quiere eliminar. MHC corrige
    // la interpolación con el gradiente del canal nativo (coeficientes
    // enteros ×16, redondeo al más cercano); el anillo de 1 px donde el
    // kernel 5×5 no cabe usa bilineal, y el borde exterior se RELLENA
    // REPLICANDO el vecino interior (antes quedaba un marco NEGRO de 1 px
    // que contaminaba warp, drizzle y el verde de alineación).
    if width >= 5 && height >= 5 {
        debayer_mhc_interior(input, width, height, rx, ry, out);
        debayer_bilinear_ring(input, width, height, rx, ry, out);
    } else if width >= 3 && height >= 3 {
        debayer_bilinear_rect(input, width, height, rx, ry, out, 1, 1, width - 1, height - 1);
    }
    replicate_border_rgb(out, width, height);
}

/// Interior MHC (y,x ∈ [2, dim−2)): interpolación gradiente-corregida de
/// Malvar-He-Cutler con aritmética entera i32 (kernels ×16, +8 y >>4 para
/// redondear). Preserva DC exactamente (suma de coeficientes = 16).
fn debayer_mhc_interior(
    input: &[u16],
    width: usize,
    height: usize,
    rx: usize,
    ry: usize,
    out: &mut [u16],
) {
    let ptr_in = input.as_ptr();
    let ptr_out = out.as_mut_ptr();
    for y in 2..height - 2 {
        for x in 2..width - 2 {
            let idx = y * width + x;
            let o = idx * 3;
            unsafe {
                // Vecindario 5×5 en unidades i32.
                let p = |dx: isize, dy: isize| -> i32 {
                    *ptr_in.add((idx as isize + dy * width as isize + dx) as usize) as i32
                };
                let c = p(0, 0);
                let l = p(-1, 0);
                let r = p(1, 0);
                let u = p(0, -1);
                let d = p(0, 1);
                let ll = p(-2, 0);
                let rr = p(2, 0);
                let uu = p(0, -2);
                let dd = p(0, 2);
                let ul = p(-1, -1);
                let ur = p(1, -1);
                let dl = p(-1, 1);
                let dr = p(1, 1);

                let red_pixel = (x % 2 == rx) && (y % 2 == ry);
                let blue_pixel = (x % 2 != rx) && (y % 2 != ry);

                let clamp16 = |v: i32| -> u16 { ((v + 8) >> 4).clamp(0, 65535) as u16 };

                let (cr, cg, cb) = if red_pixel || blue_pixel {
                    // G en posición R/B (kernel ×16): 8C + 4(U+D+L+R) − 2(UU+DD+LL+RR)
                    let g = clamp16(8 * c + 4 * (u + d + l + r) - 2 * (uu + dd + ll + rr));
                    // Canal opuesto (R en B o B en R), diagonales (×16):
                    // 12C + 4(diagonales) − 3(UU+DD+LL+RR)
                    let opp = clamp16(12 * c + 4 * (ul + ur + dl + dr) - 3 * (uu + dd + ll + rr));
                    let native = c.clamp(0, 65535) as u16;
                    if red_pixel {
                        (native, g, opp)
                    } else {
                        (opp, g, native)
                    }
                } else {
                    // Píxel G. La fila determina dónde viven R y B:
                    // en la fila de R (y%2==ry) los vecinos horizontales son R;
                    // en la fila de B, los horizontales son B.
                    // Canal en el eje HORIZONTAL del G (kernel ×16):
                    // 10C + 8(L+R) − 2(UL+UR+DL+DR+LL+RR) + (UU+DD)
                    let horiz = clamp16(
                        10 * c + 8 * (l + r) - 2 * (ul + ur + dl + dr + ll + rr) + (uu + dd),
                    );
                    // Canal en el eje VERTICAL del G (transpuesto):
                    let vert = clamp16(
                        10 * c + 8 * (u + d) - 2 * (ul + ur + dl + dr + uu + dd) + (ll + rr),
                    );
                    let native = c.clamp(0, 65535) as u16;
                    if y % 2 == ry {
                        (horiz, native, vert)
                    } else {
                        (vert, native, horiz)
                    }
                };
                *ptr_out.add(o) = cr;
                *ptr_out.add(o + 1) = cg;
                *ptr_out.add(o + 2) = cb;
            }
        }
    }
}

/// Anillo de 1 px alrededor del interior MHC, interpolado con el bilineal
/// entero clásico (el kernel 5×5 no cabe ahí).
fn debayer_bilinear_ring(
    input: &[u16],
    width: usize,
    height: usize,
    rx: usize,
    ry: usize,
    out: &mut [u16],
) {
    // Filas y==1 y y==height−2 completas; columnas x==1 y x==width−2 del resto.
    debayer_bilinear_rect(input, width, height, rx, ry, out, 1, 1, width - 1, 2);
    debayer_bilinear_rect(input, width, height, rx, ry, out, 1, height - 2, width - 1, height - 1);
    debayer_bilinear_rect(input, width, height, rx, ry, out, 1, 2, 2, height - 2);
    debayer_bilinear_rect(input, width, height, rx, ry, out, width - 2, 2, width - 1, height - 2);
}

/// Bilineal entero (bit-idéntico a la referencia f32 histórica) sobre el
/// rectángulo [x0, x1) × [y0, y1), que debe estar dentro de 1..dim−1.
fn debayer_bilinear_rect(
    input: &[u16],
    width: usize,
    _height: usize,
    rx: usize,
    ry: usize,
    out: &mut [u16],
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) {
    let ptr_in = input.as_ptr();
    let ptr_out = out.as_mut_ptr();
    for y in y0..y1 {
        let row_offset = y * width;
        let prev_row = (y - 1) * width;
        let next_row = (y + 1) * width;
        for x in x0..x1 {
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

/// Rellena el marco exterior de 1 px replicando el vecino interior más
/// cercano. El marco NEGRO anterior entraba en el warp/drizzle y en el canal
/// verde de alineación, oscureciendo el borde del stack.
fn replicate_border_rgb(out: &mut [u16], width: usize, height: usize) {
    if width < 3 || height < 3 || out.len() < width * height * 3 {
        return;
    }
    let row_bytes = width * 3;
    // Fila 0 ← fila 1; fila h−1 ← fila h−2.
    let (first, rest) = out.split_at_mut(row_bytes);
    first.copy_from_slice(&rest[..row_bytes]);
    let last_start = (height - 1) * row_bytes;
    let (rest, last) = out.split_at_mut(last_start);
    last[..row_bytes].copy_from_slice(&rest[last_start - row_bytes..]);
    // Columnas 0 y w−1 por fila.
    for y in 0..height {
        let row = y * row_bytes;
        for cch in 0..3 {
            out[row + cch] = out[row + 3 + cch];
            out[row + (width - 1) * 3 + cch] = out[row + (width - 2) * 3 + cch];
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
        // El BILINEAL entero (hoy anillo/fallback del MHC) sigue siendo
        // bit-idéntico a la referencia f32 histórica.
        for &(color_id, rx, ry) in &[(8, 0, 0), (9, 1, 0), (10, 0, 1), (11, 1, 1)] {
            let _ = color_id;
            let mut out = vec![0u16; width * height * 3];
            debayer_bilinear_rect(&input, width, height, rx, ry, &mut out, 1, 1, width - 1, height - 1);
            let reference = debayer_bilinear_f32_reference(&input, width, height, rx, ry);
            // Comparar solo el interior 1..dim-1 (la referencia deja el borde a 0).
            for y in 1..height - 1 {
                for x in 1..width - 1 {
                    let o = (y * width + x) * 3;
                    assert_eq!(&out[o..o + 3], &reference[o..o + 3], "bilineal difiere en ({x},{y}) rx={rx} ry={ry}");
                }
            }
        }
    }

    /// El MHC preserva DC: una imagen Bayer CONSTANTE debe demosaicarse al
    /// mismo valor en los tres canales, en TODO el frame (incluidos anillo
    /// bilineal y borde replicado — sin marco negro).
    #[test]
    fn debayer_mhc_preserves_dc_and_borders() {
        let (width, height) = (32usize, 24usize);
        let input = vec![9137u16; width * height];
        for color_id in 8..=11 {
            let mut out = Vec::new();
            debayer_into_buffer(&input, width, height, color_id, &mut out);
            assert_eq!(out.len(), width * height * 3);
            for (i, &v) in out.iter().enumerate() {
                assert_eq!(v, 9137, "DC roto en componente {i} (color_id {color_id})");
            }
        }
    }

    /// En un flanco de luminancia neutro, el MHC debe reconstruir el frame
    /// con MENOS error (falso color/zipper) que el bilineal.
    #[test]
    fn debayer_mhc_beats_bilinear_on_edge() {
        let (width, height) = (64usize, 64usize);
        // Verdad: rampa con flanco vertical fuerte en x=32, señal neutra.
        let truth = |x: usize, _y: usize| -> u16 {
            if x < 32 { 8000 } else { 48000 }
        };
        let mut mosaic = vec![0u16; width * height];
        for y in 0..height {
            for x in 0..width {
                mosaic[y * width + x] = truth(x, y); // neutro: R=G=B ⇒ CFA = luminancia
            }
        }
        let mae_of = |rgb: &[u16]| -> f64 {
            let mut err = 0.0f64;
            let mut n = 0.0f64;
            for y in 4..height - 4 {
                for x in 28..36 {
                    let t = truth(x, y) as f64;
                    let o = (y * width + x) * 3;
                    for cch in 0..3 {
                        err += (rgb[o + cch] as f64 - t).abs();
                        n += 1.0;
                    }
                }
            }
            err / n
        };
        let mut mhc = Vec::new();
        debayer_into_buffer(&mosaic, width, height, 8, &mut mhc);
        let mut bil = vec![0u16; width * height * 3];
        debayer_bilinear_rect(&mosaic, width, height, 0, 0, &mut bil, 1, 1, width - 1, height - 1);
        let (e_mhc, e_bil) = (mae_of(&mhc), mae_of(&bil));
        assert!(
            e_mhc <= e_bil,
            "MHC debe reconstruir el flanco al menos tan bien como bilineal: MHC={e_mhc:.1} bilineal={e_bil:.1}"
        );
    }
}

fn debayer_to_rgb(input: &[u16], width: usize, height: usize, color_id: i32) -> Vec<u16> {
    let mut out = Vec::with_capacity(width * height * 3);
    debayer_into_buffer(input, width, height, color_id, &mut out);
    out
}

fn to_8bit_visual(input: &[u16], brightness: f32) -> Vec<u8> {
    // PR-1.7: dithering determinista al bajar 16→8 bits. La truncación
    // (`as u8`) producía banding visible en los gradientes suaves del limbo
    // lunar/planetario — y esta es la ÚNICA vía por la que la imagen llega
    // al editor. Un umbral fraccional por-píxel (hash del índice, sin estado
    // ni patrón repetitivo visible) reparte el error de cuantización.
    let mut out = Vec::with_capacity(input.len());
    for (i, &v) in input.iter().enumerate() {
        let scaled = (v as f32 * brightness / 65535.0).min(1.0) * 255.0;
        let t = ((i as u32).wrapping_mul(0x9E37_79B1) >> 24) as f32 / 256.0;
        out.push((scaled + t).min(255.0) as u8);
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

