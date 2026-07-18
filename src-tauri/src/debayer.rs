// ==========================================
// 4. DEBAYER
// ==========================================

#[inline]
fn unpack_yuv422_pair(p0: u16, p1: u16, swap_uv: bool) -> (f32, f32, f32, f32) {
    if !swap_uv {
        (
            (p0 & 0xFF) as f32,
            ((p0 >> 8) & 0xFF) as f32 - 128.0,
            (p1 & 0xFF) as f32,
            ((p1 >> 8) & 0xFF) as f32 - 128.0,
        )
    } else {
        (
            ((p0 >> 8) & 0xFF) as f32,
            (p0 & 0xFF) as f32 - 128.0,
            ((p1 >> 8) & 0xFF) as f32,
            (p1 & 0xFF) as f32 - 128.0,
        )
    }
}

#[inline]
fn yuv422_green_sample(y: f32, u: f32, v: f32) -> u16 {
    (y - 0.3441 * u - 0.7141 * v).clamp(0.0, 255.0) as u16 * 257
}

/// Canal verde BT.601 canónico para análisis/registro. Comparte exactamente el
/// desempaquetado y la ecuación G con `debayer_into_buffer`, pero evita calcular
/// R/B y evita reservar un RGB de 3× el tamaño por cada worker de análisis.
fn yuv422_to_green_into(
    input: &[u16],
    width: usize,
    height: usize,
    color_id: i32,
    out: &mut Vec<u16>,
) {
    let pixels = width.saturating_mul(height);
    out.clear();
    out.resize(pixels, 0);
    let size = input.len().min(pixels);
    let swap_uv = color_id == 103;
    let mut i = 0;
    while i + 1 < size {
        let p0 = unsafe { *input.get_unchecked(i) };
        let p1 = unsafe { *input.get_unchecked(i + 1) };
        let (y0, u, y1, v) = unpack_yuv422_pair(p0, p1, swap_uv);
        out[i] = yuv422_green_sample(y0, u, v);
        out[i + 1] = yuv422_green_sample(y1, u, v);
        i += 2;
    }
}

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

    if ser::ser_color_is_cmyg(color_id) {
        out.resize(target_size, 0);
        // CMYG no es una permutación de Bayer RGB: necesita una matriz de
        // separación de color específica de la cámara. La apertura normal
        // rechaza estos SER con un error de usuario; esta guarda secundaria
        // evita que un caller interno futuro vuelva a caer al gris genérico.
        eprintln!(
            "ERROR: mosaico CMYG {} ({}) no soportado; frame rechazado",
            color_id,
            ser::ser_pattern_name(color_id)
        );
        return;
    }

    if input.len() == target_size {
        if ser::ser_color_is_direct_bgr(color_id) {
            out.resize(target_size, 0);
            for (dst, src) in out.chunks_exact_mut(3).zip(input.chunks_exact(3)) {
                dst[0] = src[2];
                dst[1] = src[1];
                dst[2] = src[0];
            }
        } else {
            // RGB directo (MOV/MP4 rgb48): una sola pasada. El resize(0)
            // previo costaba otra pasada completa de escritura (117 MB/frame
            // a 20 MP) que copy_from_slice sobrescribía entera.
            out.extend_from_slice(input);
        }
        return;
    }

    // Rutas demosaico/YUV: canvas a cero primero (los bordes sin cobertura y
    // la escritura por puntero de YUY2 exigen el buffer ya dimensionado).
    out.resize(target_size, 0);

    let (rx, ry) = match color_id {
        8 => (0, 0),
        9 => (1, 0),
        10 => (0, 1),
        11 => (1, 1),
        12 | 20 | 102 | 103 => {
            // YUY2 / YUYV / UYVY Handling (Supported IDs: 12, 20, 102, 103)
            // Fix: Banding often occurs due to wrong U/V order or 16-bit misalignment.
            let ptr_out = out.as_mut_ptr();
            // Un frame corrupto no puede autorizar escrituras fuera del
            // canvas declarado aunque traiga muestras extra.
            let size = input.len().min(width.saturating_mul(height));
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

                // Extract YUV components (BT.601 Full Range). El análisis usa
                // estos mismos helpers para que G sea bit-idéntico.
                let (y0, u, y1, v) = unpack_yuv422_pair(p0, p1, swap_uv);

                let r0 = (y0 + 1.402 * v).clamp(0.0, 255.0) as u16 * 257;
                let g0 = yuv422_green_sample(y0, u, v);
                let b0 = (y0 + 1.772 * u).clamp(0.0, 255.0) as u16 * 257;

                let r1 = (y1 + 1.402 * v).clamp(0.0, 255.0) as u16 * 257;
                let g1 = yuv422_green_sample(y1, u, v);
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
            for (i, &val) in input
                .iter()
                .take(width.saturating_mul(height))
                .enumerate()
            {
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
            for (i, &val) in input
                .iter()
                .take(width.saturating_mul(height))
                .enumerate()
            {
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
        debayer_bilinear_rect(
            input,
            width,
            height,
            rx,
            ry,
            out,
            1,
            1,
            width - 1,
            height - 1,
        );
    }
    replicate_border_rgb(out, width, height);
}

/// Extrae el canal verde canónico de un CFA Bayer sin materializar RGB.
///
/// En posiciones G conserva la muestra nativa; en R/B usa exactamente el
/// kernel verde de Malvar-He-Cutler que `debayer_into_buffer`, incluido su
/// fallback bilineal y la política de borde. `origin_x/y` son las coordenadas
/// absolutas del primer píxel del buffer: cambiar la paridad de un ROI cambia
/// también la fase local del CFA.
fn bayer_to_green_into(
    input: &[u16],
    width: usize,
    height: usize,
    color_id: i32,
    origin_x: usize,
    origin_y: usize,
    out: &mut Vec<u16>,
) {
    let pixels = width.saturating_mul(height);
    out.clear();
    if out.capacity() < pixels {
        out.reserve(pixels);
    }
    out.resize(pixels, 0);
    if pixels == 0 || input.len() < pixels {
        return;
    }

    let (base_rx, base_ry) = match color_id {
        8 => (0usize, 0usize),
        9 => (1, 0),
        10 => (0, 1),
        11 => (1, 1),
        _ => {
            debug_assert!(false, "bayer_to_green_into requiere ColorID Bayer");
            return;
        }
    };
    let rx = base_rx ^ (origin_x & 1);
    let ry = base_ry ^ (origin_y & 1);

    // Capturas reales siempre exceden 3×3. Este camino pequeño conserva DC y
    // nunca deja bordes negros ni indexa fuera del frame.
    if width < 3 || height < 3 {
        for y in 0..height {
            for x in 0..width {
                let idx = y * width + x;
                let is_red = (x & 1) == rx && (y & 1) == ry;
                let is_blue = (x & 1) != rx && (y & 1) != ry;
                if !is_red && !is_blue {
                    out[idx] = input[idx];
                    continue;
                }
                let mut sum = 0u32;
                let mut count = 0u32;
                if x > 0 {
                    sum += input[idx - 1] as u32;
                    count += 1;
                }
                if x + 1 < width {
                    sum += input[idx + 1] as u32;
                    count += 1;
                }
                if y > 0 {
                    sum += input[idx - width] as u32;
                    count += 1;
                }
                if y + 1 < height {
                    sum += input[idx + width] as u32;
                    count += 1;
                }
                out[idx] = if count == 0 {
                    input[idx]
                } else {
                    ((sum + count / 2) / count) as u16
                };
            }
        }
        return;
    }

    if width >= 5 && height >= 5 {
        const PARALLEL_MIN_PIXELS: usize = 256 * 256;
        if pixels >= PARALLEL_MIN_PIXELS && rayon::current_num_threads() > 1 {
            out.par_chunks_exact_mut(width)
                .enumerate()
                .skip(2)
                .take(height - 4)
                .for_each(|(y, row)| bayer_green_mhc_row(input, width, y, rx, ry, row));
        } else {
            for y in 2..height - 2 {
                let row = &mut out[y * width..(y + 1) * width];
                bayer_green_mhc_row(input, width, y, rx, ry, row);
            }
        }
        bayer_green_bilinear_rect(input, width, rx, ry, out, 1, 1, width - 1, 2);
        bayer_green_bilinear_rect(
            input,
            width,
            rx,
            ry,
            out,
            1,
            height - 2,
            width - 1,
            height - 1,
        );
        bayer_green_bilinear_rect(input, width, rx, ry, out, 1, 2, 2, height - 2);
        bayer_green_bilinear_rect(
            input,
            width,
            rx,
            ry,
            out,
            width - 2,
            2,
            width - 1,
            height - 2,
        );
    } else {
        bayer_green_bilinear_rect(
            input,
            width,
            rx,
            ry,
            out,
            1,
            1,
            width - 1,
            height - 1,
        );
    }
    replicate_border_mono(out, width, height);
}

#[inline]
fn bayer_green_mhc_row(
    input: &[u16],
    width: usize,
    y: usize,
    rx: usize,
    ry: usize,
    out_row: &mut [u16],
) {
    for x in 2..width - 2 {
        let red_or_blue = ((x & 1) == rx && (y & 1) == ry)
            || ((x & 1) != rx && (y & 1) != ry);
        out_row[x] = if red_or_blue {
            mhc_green_pixel_scalar(input, width, x, y)
        } else {
            input[y * width + x]
        };
    }
}

fn bayer_green_bilinear_rect(
    input: &[u16],
    width: usize,
    rx: usize,
    ry: usize,
    out: &mut [u16],
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) {
    for y in y0..y1 {
        let row = y * width;
        for x in x0..x1 {
            let idx = row + x;
            let red_or_blue = ((x & 1) == rx && (y & 1) == ry)
                || ((x & 1) != rx && (y & 1) != ry);
            out[idx] = if red_or_blue {
                let sum = input[idx - width] as u32
                    + input[idx + width] as u32
                    + input[idx - 1] as u32
                    + input[idx + 1] as u32;
                (sum >> 2) as u16
            } else {
                input[idx]
            };
        }
    }
}

fn replicate_border_mono(out: &mut [u16], width: usize, height: usize) {
    if width < 3 || height < 3 || out.len() < width * height {
        return;
    }
    let (first, rest) = out.split_at_mut(width);
    first.copy_from_slice(&rest[..width]);
    let last_start = (height - 1) * width;
    let (rest, last) = out.split_at_mut(last_start);
    last[..width].copy_from_slice(&rest[last_start - width..]);
    for row in out.chunks_exact_mut(width) {
        row[0] = row[1];
        row[width - 1] = row[width - 2];
    }
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
    debug_assert!(width >= 5 && height >= 5);
    debug_assert!(input.len() >= width * height);
    debug_assert!(out.len() >= width * height * 3);

    // Una fila de salida es independiente de las demás: todos los reads son
    // sobre el CFA inmutable. Rayon elimina el cuello de un único core en
    // capturas lunares/solares grandes, pero evitamos su overhead en ROIs
    // planetarios pequeños. Cada fila despacha además a NEON/AVX2 cuando la
    // arquitectura lo permite; el tail sigue usando la referencia escalar.
    const PARALLEL_MIN_PIXELS: usize = 256 * 256;
    if width.saturating_mul(height) >= PARALLEL_MIN_PIXELS && rayon::current_num_threads() > 1 {
        out.par_chunks_exact_mut(width * 3)
            .enumerate()
            .skip(2)
            .take(height - 4)
            .for_each(|(y, row)| debayer_mhc_row_optimized(input, width, y, rx, ry, row));
    } else {
        for y in 2..height - 2 {
            let row = &mut out[y * width * 3..(y + 1) * width * 3];
            debayer_mhc_row_optimized(input, width, y, rx, ry, row);
        }
    }
}

#[inline(always)]
fn mhc_round_clamp(v: i32) -> u16 {
    ((v + 8) >> 4).clamp(0, 65535) as u16
}

#[inline(always)]
fn mhc_green_pixel_scalar(
    input: &[u16],
    width: usize,
    x: usize,
    y: usize,
) -> u16 {
    let idx = y * width + x;
    unsafe {
        let ptr = input.as_ptr();
        let p = |dx: isize, dy: isize| -> i32 {
            *ptr.add((idx as isize + dy * width as isize + dx) as usize) as i32
        };
        let c = p(0, 0);
        mhc_round_clamp(
            8 * c
                + 4 * (p(0, -1) + p(0, 1) + p(-1, 0) + p(1, 0))
                - 2 * (p(0, -2) + p(0, 2) + p(-2, 0) + p(2, 0)),
        )
    }
}

/// Oráculo escalar entero de un píxel MHC. Las rutas SIMD se prueban contra
/// esta función bit a bit para preservar exactamente saturación y redondeo.
#[inline(always)]
fn debayer_mhc_pixel_scalar(
    input: &[u16],
    width: usize,
    x: usize,
    y: usize,
    rx: usize,
    ry: usize,
) -> [u16; 3] {
    let idx = y * width + x;
    // Todos los callers limitan x/y al interior de radio 2.
    unsafe {
        let ptr = input.as_ptr();
        let p = |dx: isize, dy: isize| -> i32 {
            *ptr.add((idx as isize + dy * width as isize + dx) as usize) as i32
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
        let native = c as u16;
        if red_pixel || blue_pixel {
            let g = mhc_round_clamp(8 * c + 4 * (u + d + l + r) - 2 * (uu + dd + ll + rr));
            let opposite =
                mhc_round_clamp(12 * c + 4 * (ul + ur + dl + dr) - 3 * (uu + dd + ll + rr));
            if red_pixel {
                [native, g, opposite]
            } else {
                [opposite, g, native]
            }
        } else {
            let horizontal =
                mhc_round_clamp(10 * c + 8 * (l + r) - 2 * (ul + ur + dl + dr + ll + rr) + uu + dd);
            let vertical =
                mhc_round_clamp(10 * c + 8 * (u + d) - 2 * (ul + ur + dl + dr + uu + dd) + ll + rr);
            if y % 2 == ry {
                [horizontal, native, vertical]
            } else {
                [vertical, native, horizontal]
            }
        }
    }
}

#[inline]
fn debayer_mhc_row_scalar_from(
    input: &[u16],
    width: usize,
    y: usize,
    rx: usize,
    ry: usize,
    out_row: &mut [u16],
    mut x: usize,
) {
    while x < width - 2 {
        let rgb = debayer_mhc_pixel_scalar(input, width, x, y, rx, ry);
        out_row[x * 3..x * 3 + 3].copy_from_slice(&rgb);
        x += 1;
    }
}

#[inline]
fn debayer_mhc_row_optimized(
    input: &[u16],
    width: usize,
    y: usize,
    rx: usize,
    ry: usize,
    out_row: &mut [u16],
) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        // NEON es parte obligatoria de AArch64.
        return debayer_mhc_row_neon(input, width, y, rx, ry, out_row);
    }

    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        unsafe {
            return debayer_mhc_row_avx2(input, width, y, rx, ry, out_row);
        }
    }

    #[allow(unreachable_code)]
    debayer_mhc_row_scalar_from(input, width, y, rx, ry, out_row, 2);
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn debayer_mhc_row_neon(
    input: &[u16],
    width: usize,
    y: usize,
    rx: usize,
    ry: usize,
    out_row: &mut [u16],
) {
    use std::arch::aarch64::*;

    #[inline(always)]
    unsafe fn load4(ptr: *const u16) -> int32x4_t {
        vreinterpretq_s32_u32(vmovl_u16(vld1_u16(ptr)))
    }
    #[inline(always)]
    unsafe fn rounded_u16(v: int32x4_t) -> uint16x4_t {
        // vqmovun aplica exactamente clamp(0, 65535) después de (+8)>>4.
        vqmovun_s32(vshrq_n_s32(vaddq_s32(v, vdupq_n_s32(8)), 4))
    }

    let base = input.as_ptr();
    let mut x = 2usize;
    while x + 4 <= width - 2 {
        let row = base.add(y * width + x);
        let c = load4(row);
        let l = load4(row.sub(1));
        let r = load4(row.add(1));
        let u = load4(row.sub(width));
        let d = load4(row.add(width));
        let ll = load4(row.sub(2));
        let rr = load4(row.add(2));
        let uu = load4(row.sub(2 * width));
        let dd = load4(row.add(2 * width));
        let ul = load4(row.sub(width + 1));
        let ur = load4(row.sub(width - 1));
        let dl = load4(row.add(width - 1));
        let dr = load4(row.add(width + 1));

        let cross = vaddq_s32(vaddq_s32(u, d), vaddq_s32(l, r));
        let axial2 = vaddq_s32(vaddq_s32(uu, dd), vaddq_s32(ll, rr));
        let diagonals = vaddq_s32(vaddq_s32(ul, ur), vaddq_s32(dl, dr));

        let g = vsubq_s32(
            vaddq_s32(vshlq_n_s32(c, 3), vshlq_n_s32(cross, 2)),
            vshlq_n_s32(axial2, 1),
        );
        let opposite = vsubq_s32(
            vaddq_s32(
                vaddq_s32(vshlq_n_s32(c, 3), vshlq_n_s32(c, 2)),
                vshlq_n_s32(diagonals, 2),
            ),
            vaddq_s32(vshlq_n_s32(axial2, 1), axial2),
        );
        let horizontal_neighbors = vaddq_s32(diagonals, vaddq_s32(ll, rr));
        let vertical_neighbors = vaddq_s32(diagonals, vaddq_s32(uu, dd));
        let horizontal = vaddq_s32(
            vsubq_s32(
                vaddq_s32(
                    vaddq_s32(vshlq_n_s32(c, 3), vshlq_n_s32(c, 1)),
                    vshlq_n_s32(vaddq_s32(l, r), 3),
                ),
                vshlq_n_s32(horizontal_neighbors, 1),
            ),
            vaddq_s32(uu, dd),
        );
        let vertical = vaddq_s32(
            vsubq_s32(
                vaddq_s32(
                    vaddq_s32(vshlq_n_s32(c, 3), vshlq_n_s32(c, 1)),
                    vshlq_n_s32(vaddq_s32(u, d), 3),
                ),
                vshlq_n_s32(vertical_neighbors, 1),
            ),
            vaddq_s32(ll, rr),
        );

        let mut native = [0u16; 4];
        let mut green = [0u16; 4];
        let mut opp = [0u16; 4];
        let mut horiz = [0u16; 4];
        let mut vert = [0u16; 4];
        vst1_u16(native.as_mut_ptr(), vld1_u16(row));
        vst1_u16(green.as_mut_ptr(), rounded_u16(g));
        vst1_u16(opp.as_mut_ptr(), rounded_u16(opposite));
        vst1_u16(horiz.as_mut_ptr(), rounded_u16(horizontal));
        vst1_u16(vert.as_mut_ptr(), rounded_u16(vertical));

        for lane in 0..4 {
            let xx = x + lane;
            let red = (xx % 2 == rx) && (y % 2 == ry);
            let blue = (xx % 2 != rx) && (y % 2 != ry);
            let pixel = if red {
                [native[lane], green[lane], opp[lane]]
            } else if blue {
                [opp[lane], green[lane], native[lane]]
            } else if y % 2 == ry {
                [horiz[lane], native[lane], vert[lane]]
            } else {
                [vert[lane], native[lane], horiz[lane]]
            };
            out_row[xx * 3..xx * 3 + 3].copy_from_slice(&pixel);
        }
        x += 4;
    }
    debayer_mhc_row_scalar_from(input, width, y, rx, ry, out_row, x);
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn debayer_mhc_row_avx2(
    input: &[u16],
    width: usize,
    y: usize,
    rx: usize,
    ry: usize,
    out_row: &mut [u16],
) {
    use std::arch::x86_64::*;

    #[inline(always)]
    unsafe fn load8(ptr: *const u16) -> __m256i {
        _mm256_cvtepu16_epi32(_mm_loadu_si128(ptr as *const __m128i))
    }
    #[inline(always)]
    unsafe fn rounded_i32(v: __m256i) -> __m256i {
        let shifted = _mm256_srai_epi32(_mm256_add_epi32(v, _mm256_set1_epi32(8)), 4);
        _mm256_min_epi32(
            _mm256_max_epi32(shifted, _mm256_setzero_si256()),
            _mm256_set1_epi32(65535),
        )
    }

    let base = input.as_ptr();
    let mut x = 2usize;
    while x + 8 <= width - 2 {
        let row = base.add(y * width + x);
        let c = load8(row);
        let l = load8(row.sub(1));
        let r = load8(row.add(1));
        let u = load8(row.sub(width));
        let d = load8(row.add(width));
        let ll = load8(row.sub(2));
        let rr = load8(row.add(2));
        let uu = load8(row.sub(2 * width));
        let dd = load8(row.add(2 * width));
        let ul = load8(row.sub(width + 1));
        let ur = load8(row.sub(width - 1));
        let dl = load8(row.add(width - 1));
        let dr = load8(row.add(width + 1));

        let cross = _mm256_add_epi32(_mm256_add_epi32(u, d), _mm256_add_epi32(l, r));
        let axial2 = _mm256_add_epi32(_mm256_add_epi32(uu, dd), _mm256_add_epi32(ll, rr));
        let diagonals = _mm256_add_epi32(_mm256_add_epi32(ul, ur), _mm256_add_epi32(dl, dr));
        let g = _mm256_sub_epi32(
            _mm256_add_epi32(_mm256_slli_epi32(c, 3), _mm256_slli_epi32(cross, 2)),
            _mm256_slli_epi32(axial2, 1),
        );
        let opposite = _mm256_sub_epi32(
            _mm256_add_epi32(
                _mm256_add_epi32(_mm256_slli_epi32(c, 3), _mm256_slli_epi32(c, 2)),
                _mm256_slli_epi32(diagonals, 2),
            ),
            _mm256_add_epi32(_mm256_slli_epi32(axial2, 1), axial2),
        );
        let horizontal_neighbors = _mm256_add_epi32(diagonals, _mm256_add_epi32(ll, rr));
        let vertical_neighbors = _mm256_add_epi32(diagonals, _mm256_add_epi32(uu, dd));
        let horizontal = _mm256_add_epi32(
            _mm256_sub_epi32(
                _mm256_add_epi32(
                    _mm256_add_epi32(_mm256_slli_epi32(c, 3), _mm256_slli_epi32(c, 1)),
                    _mm256_slli_epi32(_mm256_add_epi32(l, r), 3),
                ),
                _mm256_slli_epi32(horizontal_neighbors, 1),
            ),
            _mm256_add_epi32(uu, dd),
        );
        let vertical = _mm256_add_epi32(
            _mm256_sub_epi32(
                _mm256_add_epi32(
                    _mm256_add_epi32(_mm256_slli_epi32(c, 3), _mm256_slli_epi32(c, 1)),
                    _mm256_slli_epi32(_mm256_add_epi32(u, d), 3),
                ),
                _mm256_slli_epi32(vertical_neighbors, 1),
            ),
            _mm256_add_epi32(ll, rr),
        );

        let mut native = [0u16; 8];
        let mut green = [0i32; 8];
        let mut opp = [0i32; 8];
        let mut horiz = [0i32; 8];
        let mut vert = [0i32; 8];
        std::ptr::copy_nonoverlapping(row, native.as_mut_ptr(), 8);
        _mm256_storeu_si256(green.as_mut_ptr() as *mut __m256i, rounded_i32(g));
        _mm256_storeu_si256(opp.as_mut_ptr() as *mut __m256i, rounded_i32(opposite));
        _mm256_storeu_si256(horiz.as_mut_ptr() as *mut __m256i, rounded_i32(horizontal));
        _mm256_storeu_si256(vert.as_mut_ptr() as *mut __m256i, rounded_i32(vertical));

        for lane in 0..8 {
            let xx = x + lane;
            let red = (xx % 2 == rx) && (y % 2 == ry);
            let blue = (xx % 2 != rx) && (y % 2 != ry);
            let (g, opposite, horizontal, vertical) = (
                green[lane] as u16,
                opp[lane] as u16,
                horiz[lane] as u16,
                vert[lane] as u16,
            );
            let pixel = if red {
                [native[lane], g, opposite]
            } else if blue {
                [opposite, g, native[lane]]
            } else if y % 2 == ry {
                [horizontal, native[lane], vertical]
            } else {
                [vertical, native[lane], horizontal]
            };
            out_row[xx * 3..xx * 3 + 3].copy_from_slice(&pixel);
        }
        x += 8;
    }
    debayer_mhc_row_scalar_from(input, width, y, rx, ry, out_row, x);
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
    debayer_bilinear_rect(
        input,
        width,
        height,
        rx,
        ry,
        out,
        1,
        height - 2,
        width - 1,
        height - 1,
    );
    debayer_bilinear_rect(input, width, height, rx, ry, out, 1, 2, 2, height - 2);
    debayer_bilinear_rect(
        input,
        width,
        height,
        rx,
        ry,
        out,
        width - 2,
        2,
        width - 1,
        height - 2,
    );
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

    fn deterministic_cfa(width: usize, height: usize, seed: u32) -> Vec<u16> {
        let mut state = seed;
        let mut input = vec![0u16; width * height];
        for value in &mut input {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *value = (state >> 16) as u16;
        }
        // Fuerza saturaciones alta/baja además de la distribución aleatoria.
        for (i, value) in input.iter_mut().take(16).enumerate() {
            *value = if i % 2 == 0 { 0 } else { u16::MAX };
        }
        input
    }

    fn debayer_mhc_scalar_reference(
        input: &[u16],
        width: usize,
        height: usize,
        rx: usize,
        ry: usize,
    ) -> Vec<u16> {
        let mut out = vec![0u16; width * height * 3];
        if width >= 5 && height >= 5 {
            for y in 2..height - 2 {
                let row = &mut out[y * width * 3..(y + 1) * width * 3];
                debayer_mhc_row_scalar_from(input, width, y, rx, ry, row, 2);
            }
            debayer_bilinear_ring(input, width, height, rx, ry, &mut out);
        } else if width >= 3 && height >= 3 {
            debayer_bilinear_rect(
                input,
                width,
                height,
                rx,
                ry,
                &mut out,
                1,
                1,
                width - 1,
                height - 1,
            );
        }
        replicate_border_rgb(&mut out, width, height);
        out
    }

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
            debayer_bilinear_rect(
                &input,
                width,
                height,
                rx,
                ry,
                &mut out,
                1,
                1,
                width - 1,
                height - 1,
            );
            let reference = debayer_bilinear_f32_reference(&input, width, height, rx, ry);
            // Comparar solo el interior 1..dim-1 (la referencia deja el borde a 0).
            for y in 1..height - 1 {
                for x in 1..width - 1 {
                    let o = (y * width + x) * 3;
                    assert_eq!(
                        &out[o..o + 3],
                        &reference[o..o + 3],
                        "bilineal difiere en ({x},{y}) rx={rx} ry={ry}"
                    );
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

    /// Contrato científico del hot path: el despacho SIMD, sus tails y los
    /// bordes deben ser exactamente iguales al oráculo escalar para los cuatro
    /// CFA, también con dimensiones impares y kernels que apenas caben.
    #[test]
    fn debayer_mhc_optimized_is_bit_exact_for_all_bayer_patterns() {
        let shapes = [
            (5usize, 5usize),
            (6, 7),
            (7, 6),
            (17, 19),
            (32, 31),
            (65, 48),
        ];
        for (width, height) in shapes {
            let input = deterministic_cfa(
                width,
                height,
                0xA53C_91E7 ^ width as u32 ^ ((height as u32) << 16),
            );
            for (color_id, rx, ry) in [(8, 0, 0), (9, 1, 0), (10, 0, 1), (11, 1, 1)] {
                let expected = debayer_mhc_scalar_reference(&input, width, height, rx, ry);
                let mut actual = Vec::new();
                debayer_into_buffer(&input, width, height, color_id, &mut actual);
                assert_eq!(
                    actual, expected,
                    "MHC optimizado difiere: {}x{} color_id={}",
                    width, height, color_id
                );
            }
        }
    }

    #[test]
    fn bayer_green_matches_stack_green_for_every_pattern_and_roi_phase() {
        let (width, height) = (37usize, 29usize);
        let input = deterministic_cfa(width, height, 0xB4A7_6E31);
        for color_id in 8..=11 {
            let (base_rx, base_ry) = match color_id {
                8 => (0usize, 0usize),
                9 => (1, 0),
                10 => (0, 1),
                11 => (1, 1),
                _ => unreachable!(),
            };
            for origin_y in 0..=1 {
                for origin_x in 0..=1 {
                    let shifted = match (base_rx ^ origin_x, base_ry ^ origin_y) {
                        (0, 0) => 8,
                        (1, 0) => 9,
                        (0, 1) => 10,
                        (1, 1) => 11,
                        _ => unreachable!(),
                    };
                    let mut expected_rgb = Vec::new();
                    debayer_into_buffer(&input, width, height, shifted, &mut expected_rgb);
                    let expected: Vec<u16> = expected_rgb
                        .chunks_exact(3)
                        .map(|pixel| pixel[1])
                        .collect();
                    let mut actual = Vec::new();
                    bayer_to_green_into(
                        &input,
                        width,
                        height,
                        color_id,
                        origin_x,
                        origin_y,
                        &mut actual,
                    );
                    assert_eq!(
                        actual, expected,
                        "verde no coincide: ColorID={color_id}, origen=({origin_x},{origin_y})"
                    );
                }
            }
        }
    }

    #[test]
    fn bayer_green_preserves_native_green_dc_and_every_border() {
        for &(width, height) in &[(1usize, 1usize), (2, 3), (3, 2), (3, 3), (5, 7), (32, 24)] {
            let input = vec![23_417u16; width * height];
            for color_id in 8..=11 {
                for origin_y in 0..=1 {
                    for origin_x in 0..=1 {
                        let mut green = Vec::new();
                        bayer_to_green_into(
                            &input,
                            width,
                            height,
                            color_id,
                            origin_x,
                            origin_y,
                            &mut green,
                        );
                        assert_eq!(green, input, "DC/borde roto en {width}x{height}");
                    }
                }
            }
        }

        let (width, height) = (17usize, 15usize);
        let input = deterministic_cfa(width, height, 0x71E3_9AC5);
        for color_id in 8..=11 {
            let (rx, ry) = match color_id {
                8 => (0usize, 0usize),
                9 => (1, 0),
                10 => (0, 1),
                11 => (1, 1),
                _ => unreachable!(),
            };
            let mut green = Vec::new();
            bayer_to_green_into(&input, width, height, color_id, 0, 0, &mut green);
            for y in 1..height - 1 {
                for x in 1..width - 1 {
                    let is_red = (x & 1) == rx && (y & 1) == ry;
                    let is_blue = (x & 1) != rx && (y & 1) != ry;
                    if !is_red && !is_blue {
                        assert_eq!(green[y * width + x], input[y * width + x]);
                    }
                }
            }
        }
    }

    /// Fuerza el umbral Rayon con dimensiones impares. La prueba corre dentro
    /// de un pool explícito para no depender del número de threads del runner.
    #[test]
    fn debayer_mhc_parallel_is_bit_exact_on_large_odd_frame() {
        let (width, height) = (259usize, 257usize);
        let input = deterministic_cfa(width, height, 0x19D4_7B2F);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("pool Rayon de prueba");

        for (color_id, rx, ry) in [(8, 0, 0), (9, 1, 0), (10, 0, 1), (11, 1, 1)] {
            let expected = debayer_mhc_scalar_reference(&input, width, height, rx, ry);
            let mut actual = vec![0u16; width * height * 3];
            pool.install(|| {
                debayer_mhc_interior(&input, width, height, rx, ry, &mut actual);
                debayer_bilinear_ring(&input, width, height, rx, ry, &mut actual);
                replicate_border_rgb(&mut actual, width, height);
            });
            assert_eq!(
                actual, expected,
                "MHC Rayon/SIMD difiere para color_id={color_id}"
            );
        }
    }

    /// Microbenchmark manual y no bloqueante para detectar regresiones del hot
    /// path. Ejecutar preferentemente con `cargo test --release ... --ignored`.
    #[test]
    #[ignore = "microbenchmark diagnóstico; ejecutar manualmente en --release"]
    fn debayer_mhc_microbenchmark() {
        use std::hint::black_box;
        use std::time::Instant;

        let (width, height) = (1920usize, 1080usize);
        let input = deterministic_cfa(width, height, 0xC001_D00D);
        let mut scalar = vec![0u16; width * height * 3];
        let mut simd_single = vec![0u16; width * height * 3];
        let mut optimized = vec![0u16; width * height * 3];

        let t0 = Instant::now();
        for _ in 0..3 {
            for y in 2..height - 2 {
                let row = &mut scalar[y * width * 3..(y + 1) * width * 3];
                debayer_mhc_row_scalar_from(&input, width, y, 0, 0, row, 2);
            }
            black_box(&scalar);
        }
        let scalar_elapsed = t0.elapsed();

        let single_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("pool SIMD de benchmark");
        let t_simd = Instant::now();
        single_pool.install(|| {
            for _ in 0..3 {
                debayer_mhc_interior(&input, width, height, 0, 0, &mut simd_single);
                black_box(&simd_single);
            }
        });
        let simd_elapsed = t_simd.elapsed();

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("pool Rayon de benchmark");
        let t1 = Instant::now();
        pool.install(|| {
            for _ in 0..3 {
                debayer_mhc_interior(&input, width, height, 0, 0, &mut optimized);
                black_box(&optimized);
            }
        });
        let optimized_elapsed = t1.elapsed();
        assert_eq!(scalar, simd_single, "SIMD debe ser bit-idéntico");
        assert_eq!(scalar, optimized, "Rayon+SIMD debe ser bit-idéntico");

        let pixels = (width * height * 3) as f64;
        let scalar_mpix = pixels / scalar_elapsed.as_secs_f64() / 1_000_000.0;
        let simd_mpix = pixels / simd_elapsed.as_secs_f64() / 1_000_000.0;
        let optimized_mpix = pixels / optimized_elapsed.as_secs_f64() / 1_000_000.0;
        eprintln!(
            "MHC 1080p×3: scalar={scalar_mpix:.1} MPix/s SIMD-1T={simd_mpix:.1} MPix/s ({:.2}x) SIMD-4T={optimized_mpix:.1} MPix/s ({:.2}x)",
            simd_mpix / scalar_mpix,
            optimized_mpix / scalar_mpix,
        );
    }

    #[test]
    fn cmyg_never_falls_back_to_grayscale() {
        let (width, height) = (8usize, 8usize);
        let input: Vec<u16> = (0..width * height).map(|i| (i as u16) * 977).collect();
        for color_id in 16..=19 {
            let mut out = Vec::new();
            debayer_into_buffer(&input, width, height, color_id, &mut out);
            assert_eq!(out.len(), width * height * 3);
            assert!(
                out.iter().all(|&v| v == 0),
                "CMYG {color_id} no debe producir un gris aparentemente válido"
            );
        }
    }

    /// En un flanco de luminancia neutro, el MHC debe reconstruir el frame
    /// con MENOS error (falso color/zipper) que el bilineal.
    #[test]
    fn debayer_mhc_beats_bilinear_on_edge() {
        let (width, height) = (64usize, 64usize);
        // Verdad: rampa con flanco vertical fuerte en x=32, señal neutra.
        let truth = |x: usize, _y: usize| -> u16 {
            if x < 32 {
                8000
            } else {
                48000
            }
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
        debayer_bilinear_rect(
            &mosaic,
            width,
            height,
            0,
            0,
            &mut bil,
            1,
            1,
            width - 1,
            height - 1,
        );
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
