use byteorder::{ByteOrder, LittleEndian};
use memmap2::Mmap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct SerInfo {
    pub width: usize,
    pub height: usize,
    pub frame_count: usize,
    pub bytes_per_pixel: usize,
    pub sample_bits: usize,
    pub color_id: i32,
    /// Orden de los samples del payload (el header SER siempre es LE).
    /// Para PixelDepth <= 8 se normaliza a `true` porque no aplica.
    pub is_little_endian: bool,
}

#[derive(Debug, Clone)]
pub struct SerReader {
    pub mmap: Arc<Mmap>,
    pub info: SerInfo,
    pub header_shift: usize, // Offset real al inicio de frames
}

#[derive(Debug, Clone, Copy)]
struct ParsedSerHeader {
    width: usize,
    height: usize,
    pixel_depth: usize,
    frame_count: usize,
    score: i32,
    is_legacy: bool,
    pixel_little_endian: bool,
}

fn read_i32_le(mmap: &[u8], offset: usize) -> i32 {
    LittleEndian::read_i32(&mmap[offset..offset + 4])
}

/// Detecta el trailer opcional de timestamps cuando FrameCount=0. No basta
/// con probar divisibilidad: para ciertos tamaños N·(frame_size+8) también es
/// divisible por frame_size y el trailer parecería un frame extra. Validar la
/// serie temporal resuelve esa ambigüedad sin inspeccionar/alterar píxeles.
fn timestamp_trailer_frame_count(
    mmap: &[u8],
    header_offset: usize,
    frame_size: usize,
) -> Option<usize> {
    if frame_size == 0 {
        return None;
    }
    let remaining = mmap.len().checked_sub(header_offset)?;
    let record_size = frame_size.checked_add(8)?;
    if remaining == 0 || remaining % record_size != 0 {
        return None;
    }
    let frame_count = remaining / record_size;
    if frame_count == 0 {
        return None;
    }
    let trailer_start = header_offset.checked_add(frame_count.checked_mul(frame_size)?)?;
    let timestamps = mmap.get(trailer_start..)?;
    if timestamps.len() != frame_count.checked_mul(8)? {
        return None;
    }

    // Si los tamaños no son ambiguos, la ecuación exacta N·(frame+8) basta y
    // tolera cámaras que escriben timestamps cero/defectuosos. Sólo exigimos
    // semántica temporal cuando el mismo archivo también podría ser N frames
    // puros, que es donde se perdería o inventaría un frame.
    if remaining % frame_size != 0 {
        return Some(frame_count);
    }

    let first = LittleEndian::read_u64(timestamps.get(0..8)?);
    if first == 0 {
        return None;
    }
    if frame_count == 1 {
        return Some(1);
    }

    // Los timestamps SER son ticks de 100 ns. Un salto >24 h dentro de una
    // captura planetaria es inválido y, sobre todo, diferencia una serie real
    // de los últimos píxeles de un frame con gradiente que casualmente sean
    // monótonos al agruparlos como u64.
    const MAX_TIMESTAMP_STEP_TICKS: u64 = 24 * 60 * 60 * 10_000_000;
    let mut previous = first;
    let mut positive_steps = 0usize;
    for chunk in timestamps[8..].chunks_exact(8) {
        let current = LittleEndian::read_u64(chunk);
        if current == 0 || current < previous {
            return None;
        }
        let delta = current - previous;
        if delta > MAX_TIMESTAMP_STEP_TICKS {
            return None;
        }
        positive_steps += usize::from(delta > 0);
        previous = current;
    }
    (positive_steps > 0).then_some(frame_count)
}

pub fn ser_sample_bytes_from_depth(pixel_depth: usize) -> usize {
    match pixel_depth {
        0..=8 => 1,
        9..=16 => 2,
        17..=24 => 3,
        _ => 4,
    }
}

pub fn ser_color_is_direct_rgb(color_id: i32) -> bool {
    matches!(color_id, 14 | 100)
}

pub fn ser_color_is_direct_bgr(color_id: i32) -> bool {
    matches!(color_id, 101)
}

pub fn ser_color_is_yuv422(color_id: i32) -> bool {
    matches!(color_id, 12 | 20 | 102 | 103)
}

pub fn ser_color_is_bayer(color_id: i32) -> bool {
    (8..=11).contains(&color_id)
}

/// Los ColorID 16..=19 son mosaicos CMYG, no Bayer RGB. El motor no debe
/// enviarlos al demosaico RGGB: hacerlo produce color falso o, peor, una
/// conversión silenciosa a gris.
pub fn ser_color_is_cmyg(color_id: i32) -> bool {
    (16..=19).contains(&color_id)
}

pub fn ser_color_is_color(color_id: i32) -> bool {
    ser_color_is_bayer(color_id)
        || ser_color_is_cmyg(color_id)
        || ser_color_is_direct_rgb(color_id)
        || ser_color_is_direct_bgr(color_id)
        || ser_color_is_yuv422(color_id)
}

pub fn ser_pattern_name(color_id: i32) -> &'static str {
    match color_id {
        0 => "MONO",
        8 => "RGGB",
        9 => "GRBG",
        10 => "GBRG",
        11 => "BGGR",
        16 => "CYYM",
        17 => "YCMY",
        18 => "YMCY",
        19 => "MYYC",
        12 | 20 | 102 | 103 => "YUY2/YUV",
        14 | 100 => "RGB",
        101 => "BGR",
        _ => "RAW/OTHER",
    }
}

pub fn ser_bytes_per_pixel(color_id: i32, pixel_depth: usize) -> usize {
    let sample_bytes = ser_sample_bytes_from_depth(pixel_depth);
    if ser_color_is_direct_rgb(color_id) || ser_color_is_direct_bgr(color_id) {
        sample_bytes * 3
    } else if ser_color_is_yuv422(color_id) {
        sample_bytes * 2
    } else {
        sample_bytes
    }
}

fn parse_header_candidate(
    mmap_len: usize,
    mmap: &[u8],
    field_base: usize,
    preferred: bool,
    color_id: i32,
    pixel_little_endian: bool,
) -> Option<ParsedSerHeader> {
    // SER v3 fija TODOS los enteros del encabezado en little-endian. El
    // campo LittleEndian del offset 22 describe únicamente el orden de los
    // samples de imagen cuando PixelDepth > 8.
    let width_i = read_i32_le(mmap, field_base);
    let height_i = read_i32_le(mmap, field_base + 4);
    let pixel_depth_i = read_i32_le(mmap, field_base + 8);
    let frame_count_i = read_i32_le(mmap, field_base + 12);

    if width_i <= 0 || height_i <= 0 || pixel_depth_i <= 0 || frame_count_i < 0 {
        return None;
    }

    let width = width_i as usize;
    let height = height_i as usize;
    let pixel_depth = pixel_depth_i as usize;
    let frame_count = frame_count_i as usize;

    if width < 16 || height < 16 || width > 100_000 || height > 100_000 {
        return None;
    }
    // El formato SER publicado define profundidades de 1 a 16 bits por
    // sample. Las rutas de conversión internas también producen u16.
    if !(1..=16).contains(&pixel_depth) {
        return None;
    }

    let bpp = ser_bytes_per_pixel(color_id, pixel_depth);
    let frame_size = width.checked_mul(height)?.checked_mul(bpp)?;
    let header_offset = if field_base == 22 { 174 } else { 178 };
    if frame_size == 0 || header_offset >= mmap_len || mmap_len - header_offset < frame_size {
        return None;
    }

    let frames_by_size = (mmap_len - header_offset) / frame_size;
    let mut score = if preferred { 50 } else { 0 };
    if frame_count > 0 {
        score += 5;
        if frames_by_size >= frame_count {
            score += 25;
        }
        if frames_by_size == frame_count {
            score += 25;
        }
    }
    if (mmap_len - header_offset) % frame_size == 0 {
        score += 15;
    }

    Some(ParsedSerHeader {
        width,
        height,
        pixel_depth,
        frame_count,
        score,
        is_legacy: field_base == 22,
        // El indicador carece de significado para pixels de un byte.
        pixel_little_endian: pixel_depth <= 8 || pixel_little_endian,
    })
}

impl SerReader {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let mmap = unsafe { Mmap::map(&file).map_err(|e| e.to_string())? };

        if mmap.len() < 178 {
            return Err("Archivo muy pequeno".into());
        }

        let color_id = read_i32_le(&mmap, 18);
        if ser_color_is_cmyg(color_id) {
            return Err(format!(
                "SER CMYG {} ({}) no soportado: se requiere una matriz de conversión CMYG explícita; no se convertirá silenciosamente a gris",
                color_id,
                ser_pattern_name(color_id)
            ));
        }

        let endian_flag = read_i32_le(&mmap, 22);
        // La especificación original llamó a este campo LittleEndian, pero el
        // ecosistema real (Siril, GoQat y grabadores históricos) consolidó la
        // convención inversa: 0=payload LE, 1=payload BE. Nuestros writers ya
        // emiten 0 + u16 LE; leer con la semántica literal corrompía cada SER
        // generado por la propia aplicación.
        let pixel_little_endian = endian_flag == 0;
        let standard =
            parse_header_candidate(mmap.len(), &mmap, 26, true, color_id, pixel_little_endian);
        // Compatibilidad conservadora con el layout antiguo de 174 bytes,
        // que no tenía indicador separado: sus samples se asumían LE. Sus
        // campos numéricos siguen leyéndose LE.
        let legacy = parse_header_candidate(mmap.len(), &mmap, 22, false, color_id, true);

        let parsed = [standard, legacy]
            .into_iter()
            .flatten()
            .max_by_key(|h| h.score)
            .ok_or_else(|| {
                "Header SER invalido: dimensiones/profundidad no plausibles".to_string()
            })?;

        let width = parsed.width;
        let height = parsed.height;
        let pixel_depth = parsed.pixel_depth;
        let mut frame_count = parsed.frame_count;
        let is_little_endian = parsed.pixel_little_endian;

        if ser_color_is_yuv422(color_id) && pixel_depth > 8 {
            return Err(format!(
                "SER YUV422 >8-bit no soportado en preflight ({} bits, ColorID {}): se requiere validar packing, rango y matriz antes de convertir; el trabajo no se iniciará",
                pixel_depth, color_id
            ));
        }

        if width == 0 || height == 0 {
            return Err(
                "Error fatal: Dimensiones 0x0. Intenta volver a grabar sin ROI variable.".into(),
            );
        }

        // YUV422 comparte una muestra cromática por cada pareja horizontal.
        // Aceptar un ancho impar dejaría el último píxel sin pareja y haría
        // ambiguo el stride del frame; se rechaza antes de decodificarlo.
        if ser_color_is_yuv422(color_id) && width % 2 != 0 {
            return Err(format!(
                "SER YUV422 inválido: el ancho debe ser par, recibido {width}"
            ));
        }

        let bytes_per_pixel = ser_bytes_per_pixel(color_id, pixel_depth);

        let frame_size = width
            .checked_mul(height)
            .and_then(|v| v.checked_mul(bytes_per_pixel))
            .ok_or_else(|| "Overflow calculando frame_size".to_string())?;

        let mut header_offset = if parsed.is_legacy { 174usize } else { 178usize };
        let mut real_frames = if mmap.len() > header_offset {
            (mmap.len() - header_offset) / frame_size
        } else {
            0
        };

        // Cuando FrameCount=0, algunos grabadores dejan que el tamaño del
        // archivo sea la única fuente de verdad y aun así anexan el trailer
        // estándar de timestamps (8 bytes por frame). La validación temporal
        // también cubre el caso ambiguo en que ambos tamaños son divisibles.
        if frame_count == 0 && mmap.len() > header_offset {
            if let Some(timestamped_frames) =
                timestamp_trailer_frame_count(&mmap, header_offset, frame_size)
            {
                real_frames = timestamped_frames;
            }
        }

        if frame_count > 0 && real_frames >= frame_count {
            real_frames = frame_count;
        } else if real_frames == 0 {
            let candidates: [usize; 9] = [
                header_offset,
                header_offset + 2,
                header_offset + 4,
                header_offset + 6,
                256,
                512,
                1024,
                2048,
                4096,
            ];
            for &off in &candidates {
                if off >= mmap.len() {
                    continue;
                }
                let remain = mmap.len() - off;
                if remain < frame_size {
                    continue;
                }
                let frames = remain / frame_size;
                if frames > 0 {
                    header_offset = off;
                    real_frames = frames;
                    break;
                }
            }

            if real_frames == 0 {
                return Err("Archivo SER sin frames legibles (frame_size no encaja)".into());
            }
        }

        if frame_count == 0 {
            frame_count = real_frames;
        } else {
            frame_count = frame_count.min(real_frames);
        }

        Ok(SerReader {
            mmap: Arc::new(mmap),
            info: SerInfo {
                width,
                height,
                frame_count,
                bytes_per_pixel,
                sample_bits: pixel_depth,
                color_id,
                is_little_endian,
            },
            header_shift: header_offset,
        })
    }

    pub fn get_frame<'a>(&'a self, index: usize, _cid: i32) -> std::borrow::Cow<'a, [u8]> {
        if index >= self.info.frame_count {
            return std::borrow::Cow::Borrowed(&[]);
        }
        let frame_size = self.info.width * self.info.height * self.info.bytes_per_pixel;
        let start = self.header_shift + (index * frame_size);

        if start + frame_size > self.mmap.len() {
            return std::borrow::Cow::Borrowed(&[]);
        }

        let slice = &self.mmap[start..start + frame_size];
        if !self.info.is_little_endian && self.info.bytes_per_pixel >= 2 {
            // Swap bytes for 16-bit pixel data
            let mut swapped = slice.to_vec();
            for chunk in swapped.chunks_exact_mut(2) {
                chunk.swap(0, 1);
            }
            std::borrow::Cow::Owned(swapped)
        } else {
            std::borrow::Cow::Borrowed(slice)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standard_ser(
        width: usize,
        height: usize,
        depth: usize,
        color_id: i32,
        little_endian_flag: i32,
        frame_count: usize,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut data = Vec::with_capacity(178 + payload.len());
        data.extend_from_slice(b"LUCAM-RECORDER");
        let put_i32 = |buf: &mut Vec<u8>, v: i32| buf.extend_from_slice(&v.to_le_bytes());
        put_i32(&mut data, 0); // LuID
        put_i32(&mut data, color_id);
        put_i32(&mut data, little_endian_flag);
        put_i32(&mut data, width as i32);
        put_i32(&mut data, height as i32);
        put_i32(&mut data, depth as i32);
        put_i32(&mut data, frame_count as i32);
        data.resize(178, 0);
        data.extend_from_slice(payload);
        data
    }

    fn temp_ser(tag: &str, data: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("zas_ser_{}_{}.ser", tag, std::process::id()));
        std::fs::write(&path, data).unwrap();
        path
    }

    /// "SER DORADO" sintetico: fija DOS contratos que protegen a los usuarios.
    /// (1) Contrato del lector: un .ser estandar (header 178 bytes, LE,
    ///     mono16) se parsea con dimensiones/frames/profundidad exactos y los
    ///     pixeles llegan intactos hasta despues de raw_to_u16_buffer.
    /// (2) Contrato de SEGURIDAD con archivos truncados (captura interrumpida,
    ///     el caso real mas comun de crash): el frame incompleto se descarta
    ///     del conteo, get_frame fuera de rango devuelve slice VACIO, y
    ///     raw_to_u16_buffer convierte vacios/cortos en buffers rellenos de
    ///     negro DEL TAMANO CORRECTO — la guarda que evita la lectura fuera
    ///     de limites (0xC0000005/SIGSEGV) en los kernels AVX2/NEON.
    /// Si un cambio futuro rompe cualquiera de los dos, este test lo detiene.
    #[test]
    fn test_golden_synthetic_ser_and_truncation_safety() {
        let (w, h, frames) = (32usize, 24usize, 5usize);
        let frame_px = w * h;
        let frame_bytes = frame_px * 2; // mono16

        // --- Construir el .ser en memoria (header estandar de 178 bytes) ---
        let mut data = Vec::with_capacity(178 + frames * frame_bytes);
        data.extend_from_slice(b"LUCAM-RECORDER"); // FileID (14 bytes)
        let put_i32 = |buf: &mut Vec<u8>, v: i32| buf.extend_from_slice(&v.to_le_bytes());
        put_i32(&mut data, 0); // LuID
        put_i32(&mut data, 0); // ColorID = MONO (offset 18)
        put_i32(&mut data, 0); // Convención interoperable: 0 = pixels LE
        put_i32(&mut data, w as i32); // Width (offset 26)
        put_i32(&mut data, h as i32); // Height
        put_i32(&mut data, 16); // PixelDepth
        put_i32(&mut data, frames as i32); // FrameCount
        data.resize(162, 0); // Observer + Instrument + Telescope (40×3)
        data.resize(178, 0); // DateTime + DateTimeUTC (8+8)
        assert_eq!(data.len(), 178);
        // Frames identificables: pixel = f·1000 + indice_lineal.
        for f in 0..frames {
            for i in 0..frame_px {
                let v = (f * 1000 + i) as u16;
                data.extend_from_slice(&v.to_le_bytes());
            }
        }

        let dir = std::env::temp_dir();
        let p_full = dir.join(format!("zas_golden_{}.ser", std::process::id()));
        let p_trunc = dir.join(format!("zas_golden_trunc_{}.ser", std::process::id()));
        std::fs::write(&p_full, &data).unwrap();
        // Truncado a mitad del ultimo frame (100 bytes menos).
        std::fs::write(&p_trunc, &data[..data.len() - 100]).unwrap();

        // --- (1) Contrato del lector: archivo integro ---
        let r = SerReader::new(&p_full).expect("SER sintetico valido");
        assert_eq!(r.info.width, w);
        assert_eq!(r.info.height, h);
        assert_eq!(r.info.frame_count, frames);
        assert_eq!(r.info.bytes_per_pixel, 2);
        assert_eq!(r.info.sample_bits, 16);
        assert_eq!(r.info.color_id, 0);
        assert!(r.info.is_little_endian);
        let f2 = r.get_frame(2, 0);
        assert_eq!(f2.len(), frame_bytes);
        let u16s = crate::raw_to_u16_buffer(&f2, w, h, 2);
        assert_eq!(u16s.len(), frame_px);
        assert_eq!(u16s[3 * w + 5], (2 * 1000 + 3 * w + 5) as u16);

        // --- (2) Contrato de seguridad: archivo truncado ---
        let rt = SerReader::new(&p_trunc).expect("SER truncado sigue siendo legible");
        assert_eq!(
            rt.info.frame_count,
            frames - 1,
            "el frame incompleto del final debe descartarse del conteo"
        );
        let last_ok = rt.get_frame(frames - 2, 0);
        assert_eq!(last_ok.len(), frame_bytes);
        // Fuera de rango → slice VACIO (contrato de get_frame)...
        let beyond = rt.get_frame(frames - 1, 0);
        assert!(beyond.is_empty());
        // ...y la conversion produce NEGRO del tamano correcto, jamas OOB.
        let safe = crate::raw_to_u16_buffer(&beyond, w, h, 2);
        assert_eq!(safe.len(), frame_px);
        assert!(safe.iter().all(|&v| v == 0));
        // Frame corto NO-vacio: convierte lo disponible y rellena con negro.
        let partial = crate::raw_to_u16_buffer(&f2[..100], w, h, 2);
        assert_eq!(partial.len(), frame_px);
        assert_eq!(partial[10], (2 * 1000 + 10) as u16);
        assert!(partial[50..].iter().all(|&v| v == 0));

        let _ = std::fs::remove_file(&p_full);
        let _ = std::fs::remove_file(&p_trunc);
    }

    #[test]
    fn header_is_le_and_pixel_flag_controls_16bit_endianness() {
        let (w, h) = (16usize, 16usize);
        let expected: Vec<u16> = (0..w * h)
            .map(|i| (i as u16).wrapping_mul(251).wrapping_add(17))
            .collect();

        for &(tag, flag, big_endian_pixels) in &[("le16", 0, false), ("be16", 1, true)] {
            let mut payload = Vec::with_capacity(expected.len() * 2);
            for &sample in &expected {
                let bytes = if big_endian_pixels {
                    sample.to_be_bytes()
                } else {
                    sample.to_le_bytes()
                };
                payload.extend_from_slice(&bytes);
            }
            let path = temp_ser(tag, &standard_ser(w, h, 16, 0, flag, 1, &payload));
            let reader = SerReader::new(&path).expect("SER16 válido");

            // Las dimensiones se leen LE en ambos archivos; sólo cambia el
            // orden de los samples indicado en offset 22.
            assert_eq!((reader.info.width, reader.info.height), (w, h));
            assert_eq!(reader.info.is_little_endian, !big_endian_pixels);
            let canonical = reader.get_frame(0, 0);
            let decoded = crate::raw_to_u16_buffer(&canonical, w, h, 2);
            assert_eq!(decoded, expected, "falló normalización {tag}");
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn native_10_12_14bit_samples_and_all_bayer_ids_are_exact() {
        let (w, h) = (16usize, 16usize);
        for bits in [10usize, 12, 14] {
            let max_value = (1u16 << bits) - 1;
            let expected: Vec<u16> = (0..w * h)
                .map(|i| ((i as u16).wrapping_mul(73).wrapping_add(19)) & max_value)
                .collect();
            for color_id in [0, 8, 9, 10, 11] {
                for &(endian_tag, flag, big_endian) in &[("le", 0, false), ("be", 1, true)] {
                    let mut payload = Vec::with_capacity(expected.len() * 2);
                    for &sample in &expected {
                        let bytes = if big_endian {
                            sample.to_be_bytes()
                        } else {
                            sample.to_le_bytes()
                        };
                        payload.extend_from_slice(&bytes);
                    }
                    let path = temp_ser(
                        &format!("{bits}b_cid{color_id}_{endian_tag}"),
                        &standard_ser(w, h, bits, color_id, flag, 1, &payload),
                    );
                    let reader = SerReader::new(&path).expect("SER nativo válido");
                    assert_eq!(reader.info.sample_bits, bits);
                    assert_eq!(reader.info.color_id, color_id);
                    let canonical = reader.get_frame(0, color_id);
                    let decoded = crate::raw_to_u16_buffer(&canonical, w, h, 2);
                    assert_eq!(decoded, expected, "{bits}b CID={color_id} {endian_tag}");
                    let gain = crate::planetary_quality::native_sample_to_u16_gain(bits);
                    assert_eq!((max_value as f32 * gain + 0.5) as u16, u16::MAX);
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }

    #[test]
    fn native_rgb_and_bgr_eight_and_sixteen_bit_layouts_are_exact() {
        let (w, h) = (16usize, 16usize);
        for color_id in [100, 101] {
            for bits in [8usize, 16] {
                let endian_cases: &[(i32, bool)] = if bits == 8 {
                    &[(0, false)]
                } else {
                    &[(0, false), (1, true)]
                };
                for &(flag, big_endian) in endian_cases {
                    let source: Vec<u16> = (0..w * h * 3)
                        .map(|index| {
                            if bits == 8 {
                                ((index * 53 + 7) & 0xff) as u16
                            } else {
                                (index as u16).wrapping_mul(977).wrapping_add(31)
                            }
                        })
                        .collect();
                    let mut payload = Vec::with_capacity(source.len() * if bits == 8 { 1 } else { 2 });
                    for &sample in &source {
                        if bits == 8 {
                            payload.push(sample as u8);
                        } else if big_endian {
                            payload.extend_from_slice(&sample.to_be_bytes());
                        } else {
                            payload.extend_from_slice(&sample.to_le_bytes());
                        }
                    }
                    let path = temp_ser(
                        &format!("cid{color_id}_{bits}b_flag{flag}"),
                        &standard_ser(w, h, bits, color_id, flag, 1, &payload),
                    );
                    let reader = SerReader::new(&path).expect("SER RGB/BGR válido");
                    assert_eq!(reader.info.bytes_per_pixel, if bits == 8 { 3 } else { 6 });
                    let canonical = reader.get_frame(0, color_id);
                    let decoded = crate::raw_to_u16_buffer(
                        &canonical,
                        w,
                        h,
                        reader.info.bytes_per_pixel,
                    );
                    let expected: Vec<u16> = if bits == 8 {
                        source.iter().map(|sample| sample * 257).collect()
                    } else {
                        source
                    };
                    assert_eq!(decoded, expected, "CID={color_id}, bits={bits}, flag={flag}");
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }

    #[test]
    fn eight_bit_pixels_ignore_endian_flag() {
        let (w, h) = (16usize, 16usize);
        let payload: Vec<u8> = (0..w * h).map(|i| (i * 73 + 11) as u8).collect();
        for flag in [0, 1] {
            let path = temp_ser(
                &format!("flag{flag}_8bit"),
                &standard_ser(w, h, 8, 0, flag, 1, &payload),
            );
            let reader = SerReader::new(&path).expect("SER8 válido");

            assert!(
                reader.info.is_little_endian,
                "el flag no aplica a samples u8"
            );
            assert_eq!(reader.get_frame(0, 0).as_ref(), payload.as_slice());
            let decoded = crate::raw_to_u16_buffer(&payload, w, h, 1);
            assert_eq!(decoded[97], payload[97] as u16 * 257);
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn yuv422_rejects_odd_width_instead_of_synthesizing_a_last_pixel() {
        let (w, h) = (17usize, 16usize);
        let payload = vec![0u8; w * h * 2];
        let path = temp_ser(
            "yuv422_odd_width",
            &standard_ser(w, h, 8, 12, 0, 1, &payload),
        );

        let error = SerReader::new(&path).expect_err("YUV422 requiere parejas horizontales");
        assert!(error.contains("ancho debe ser par"), "{error}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn yuv422_over_eight_bits_fails_explicit_preflight() {
        let (w, h) = (16usize, 16usize);
        let payload = vec![0u8; w * h * 4];
        let path = temp_ser(
            "yuv422_12bit",
            &standard_ser(w, h, 12, 12, 0, 1, &payload),
        );
        let error = SerReader::new(&path).expect_err("YUV422 >8-bit no es elegible");
        assert!(error.contains("YUV422 >8-bit"), "{error}");
        assert!(error.contains("preflight"), "{error}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn timestamp_trailer_is_not_mistaken_for_extra_frames() {
        let (w, h, frames) = (16usize, 16usize, 3usize);
        let mut payload = Vec::with_capacity(w * h * frames * 2 + frames * 8);
        for frame in 0..frames {
            for pixel in 0..w * h {
                payload.extend_from_slice(&((frame * 1000 + pixel) as u16).to_le_bytes());
            }
        }
        // Trailer opcional SER: un timestamp u64 por frame. Debe quedar fuera
        // del stride de imagen y jamás convertirse en un cuarto frame.
        for frame in 0..frames {
            payload.extend_from_slice(&(10_000_000u64 + frame as u64).to_le_bytes());
        }
        let path = temp_ser(
            "timestamps",
            &standard_ser(w, h, 16, 0, 0, frames, &payload),
        );
        let reader = SerReader::new(&path).expect("SER con trailer de timestamps");
        assert_eq!(reader.info.frame_count, frames);
        let last = reader.get_frame(frames - 1, 0);
        let decoded = crate::raw_to_u16_buffer(&last, w, h, 2);
        assert_eq!(decoded[17], ((frames - 1) * 1000 + 17) as u16);
        assert!(reader.get_frame(frames, 0).is_empty());
        let _ = std::fs::remove_file(path);

        let unknown_count_path = temp_ser(
            "timestamps_unknown_count",
            &standard_ser(w, h, 16, 0, 0, 0, &payload),
        );
        let unknown =
            SerReader::new(&unknown_count_path).expect("SER con timestamps y FrameCount=0");
        assert_eq!(unknown.info.frame_count, frames);
        assert!(unknown.get_frame(frames, 0).is_empty());
        let _ = std::fs::remove_file(unknown_count_path);
    }

    #[test]
    fn unknown_count_resolves_timestamp_trailer_even_when_both_sizes_divide() {
        let (w, h, frames) = (16usize, 16usize, 64usize);
        let frame_size = w * h * 2;
        let mut timestamped = Vec::with_capacity(frames * (frame_size + 8));
        for frame in 0..frames {
            for pixel in 0..w * h {
                timestamped.extend_from_slice(&((frame * 257 + pixel) as u16).to_le_bytes());
            }
        }
        for frame in 0..frames {
            timestamped.extend_from_slice(&(10_000_000u64 + frame as u64 * 333_333).to_le_bytes());
        }
        assert_eq!(
            timestamped.len() % frame_size,
            0,
            "fixture debe reproducir la ambigüedad: 64·(512+8)=65·512"
        );
        let path = temp_ser(
            "timestamps_ambiguous",
            &standard_ser(w, h, 16, 0, 0, 0, &timestamped),
        );
        let reader = SerReader::new(&path).expect("SER ambiguo con trailer válido");
        assert_eq!(reader.info.frame_count, frames);
        assert!(reader.get_frame(frames, 0).is_empty());
        let _ = std::fs::remove_file(path);

        // El caso dual protege contra falsos positivos: 65 frames reales sin
        // trailer tienen exactamente el mismo tamaño. Sus últimos píxeles no
        // deben convertirse en 64 timestamps ni perder el frame final.
        let pure_frames = 65usize;
        let mut pure = Vec::with_capacity(pure_frames * frame_size);
        for frame in 0..pure_frames {
            for pixel in 0..w * h {
                pure.extend_from_slice(&((frame * 257 + pixel) as u16).to_le_bytes());
            }
        }
        let pure_path = temp_ser(
            "timestamps_ambiguous_no_trailer",
            &standard_ser(w, h, 16, 0, 0, 0, &pure),
        );
        let pure_reader = SerReader::new(&pure_path).expect("65 frames sin trailer");
        assert_eq!(pure_reader.info.frame_count, pure_frames);
        let last = pure_reader.get_frame(pure_frames - 1, 0);
        assert_eq!(last.len(), frame_size);
        let _ = std::fs::remove_file(pure_path);
    }

    #[test]
    fn cmyg_is_rejected_explicitly() {
        let (w, h) = (16usize, 16usize);
        let payload = vec![0u8; w * h];
        let path = temp_ser("cmyg", &standard_ser(w, h, 8, 16, 0, 1, &payload));
        let err = SerReader::new(&path).expect_err("CMYG no puede caer a gris/Bayer");
        assert!(
            err.contains("CMYG") && err.contains("no soportado"),
            "{err}"
        );
        let _ = std::fs::remove_file(path);
    }
}
