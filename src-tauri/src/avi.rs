use byteorder::{ByteOrder, LittleEndian};
use memmap2::Mmap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct AviInfo {
    pub width: usize,
    pub height: usize,
    pub frame_count: usize,
    pub bytes_per_pixel: usize,
    pub sample_bits: usize,
    pub color_id: i32,
    pub fps: f64,
}

#[derive(Debug, Clone)]
pub struct AviReader {
    pub mmap: Arc<Mmap>,
    pub info: AviInfo,
    pub frame_offsets: Vec<usize>,
    pub frame_sizes: Vec<usize>,
    /// F3: DIB top-down (height negativa en el header). Los DIB clásicos son
    /// bottom-up y el lector voltea las filas al servir RGB24.
    pub top_down: bool,
}

fn is_avi(m: &[u8]) -> bool {
    m.len() >= 12 && &m[0..4] == b"RIFF" && &m[8..12] == b"AVI "
}

fn clamp_read_i32(m: &[u8], off: usize) -> Option<i32> {
    if off + 4 <= m.len() {
        Some(LittleEndian::read_i32(&m[off..off + 4]))
    } else {
        None
    }
}

fn fourcc_to_color_id(fourcc: &[u8; 4]) -> i32 {
    match fourcc {
        b"RGGB" => 8,
        b"GRBG" => 9,
        b"GBRG" => 10,
        b"BGGR" => 11,
        // Algunos encoders usan otros tags, fallback a MONO seguro
        _ => 0,
    }
}

impl AviReader {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let mmap = unsafe { Mmap::map(&file).map_err(|e| e.to_string())? };

        if mmap.len() < 100 {
            return Err("Archivo AVI invalido o muy pequeno".into());
        }
        if !is_avi(&mmap) {
            return Err("No es un archivo AVI valido (Falta cabecera RIFF/AVI)".into());
        }

        let mut width = 0usize;
        let mut height = 0usize;
        let mut frames_reported = 0usize;
        let mut fps = 30.0;

        // Datos BITMAPINFOHEADER (strf)
        let mut bit_count: usize = 8;
        let mut compression = [0u8; 4];
        let mut top_down = false;

        let limit = 512 * 1024usize.min(mmap.len()); // busca mas que 10k

        // 1) Buscar avih
        {
            let scan = limit.min(mmap.len()).saturating_sub(8);
            let mut i = 0usize;
            while i < scan {
                if &mmap[i..i + 4] == b"avih" {
                    let h_start = i + 8;
                    if h_start + 40 <= mmap.len() {
                        let microsec = LittleEndian::read_u32(&mmap[h_start..h_start + 4]);
                        if microsec > 0 {
                            fps = 1_000_000.0 / microsec as f64;
                        }

                        frames_reported =
                            LittleEndian::read_u32(&mmap[h_start + 16..h_start + 20]) as usize;
                        width = LittleEndian::read_u32(&mmap[h_start + 32..h_start + 36]) as usize;
                        height = LittleEndian::read_u32(&mmap[h_start + 36..h_start + 40]) as usize;
                    }
                    break;
                }
                i += 1;
            }
        }

        // 2) Buscar strf (BITMAPINFOHEADER)
        {
            let scan = limit.min(mmap.len()).saturating_sub(8);
            let mut i = 0usize;
            while i < scan {
                if &mmap[i..i + 4] == b"strf" {
                    let f_start = i + 8;
                    // BITMAPINFOHEADER minimo 40 bytes
                    if f_start + 40 <= mmap.len() {
                        // biWidth/biHeight dentro del header tambien (mas confiable a veces)
                        if let Some(w) = clamp_read_i32(&mmap, f_start + 4) {
                            if w > 0 {
                                width = w as usize;
                            }
                        }
                        if let Some(h) = clamp_read_i32(&mmap, f_start + 8) {
                            if h < 0 {
                                top_down = true;
                                height = (-h) as usize;
                            } else if h > 0 {
                                height = h as usize;
                            }
                        }

                        // biBitCount offset 14 (u16)
                        if f_start + 16 <= mmap.len() {
                            bit_count =
                                LittleEndian::read_u16(&mmap[f_start + 14..f_start + 16]) as usize;
                        }

                        // biCompression offset 16
                        compression.copy_from_slice(&mmap[f_start + 16..f_start + 20]);
                    }
                    break;
                }
                i += 1;
            }
        }

        if width == 0 || height == 0 {
            return Err("No se pudieron leer las dimensiones del AVI".into());
        }

        // F3: RGB24 sin comprimir (biCompression=0/DIB/RGB con 24 bpp). Antes
        // caía a bpp=2 + MONO y los bytes BGR se interpretaban como mono16 →
        // imagen basura en el fallback nativo (FFmpeg lo tapaba casi siempre).
        let is_raw_rgb24 = bit_count == 24
            && (compression == [0, 0, 0, 0]
                || &compression == b"DIB "
                || &compression == b"RGB "
                || &compression == b"raw ");
        // bytes_per_pixel para el resto del pipeline:
        // rawvideo bayer 8 bits -> 1; bayer 16 -> 2; RGB24 -> 3
        let bytes_per_pixel = if is_raw_rgb24 {
            3
        } else if bit_count > 8 {
            2
        } else {
            1
        };

        let color_id = if is_raw_rgb24 {
            101 // BGR directo (orden de bytes DIB), ya soportado por el pipeline SER
        } else {
            fourcc_to_color_id(&compression)
        };

        // 3) Buscar movi (LIST movi o texto movi)
        let mut movi_start = 0usize;
        {
            let mut i = 12usize;
            while i + 4 <= mmap.len() {
                if &mmap[i..i + 4] == b"movi" {
                    movi_start = i + 4;
                    break;
                }
                i += 1;
            }
        }

        if movi_start == 0 {
            return Err("No se encontraron datos de video (lista movi)".into());
        }

        // 4) Parsear chunks 00db/00dc
        let mut frame_offsets: Vec<usize> = Vec::with_capacity(frames_reported.max(1024));
        let mut frame_sizes: Vec<usize> = Vec::with_capacity(frames_reported.max(1024));

        let mut cursor = movi_start;
        while cursor + 8 <= mmap.len() {
            let chunk_id = &mmap[cursor..cursor + 4];
            let chunk_size = LittleEndian::read_u32(&mmap[cursor + 4..cursor + 8]) as usize;

            let data_start = cursor + 8;
            let data_end = data_start.saturating_add(chunk_size);

            // Avance word-aligned
            let total_skip = 8 + chunk_size + (chunk_size & 1);

            // Validacion basica para evitar loops
            if total_skip == 0 {
                break;
            }

            // 00db/00dc video
            if chunk_id.len() == 4
                && chunk_id[0] == b'0'
                && chunk_id[1] == b'0'
                && chunk_id[2] == b'd'
                && (chunk_id[3] == b'b' || chunk_id[3] == b'c')
            {
                if data_end <= mmap.len() {
                    frame_offsets.push(data_start);
                    frame_sizes.push(chunk_size);
                }
            }

            cursor = cursor.saturating_add(total_skip);
        }

        if frame_offsets.is_empty() {
            return Err("No se encontraron frames de video en movi".into());
        }

        Ok(AviReader {
            mmap: Arc::new(mmap),
            info: AviInfo {
                width,
                height,
                frame_count: frame_offsets.len(),
            bytes_per_pixel,
            sample_bits: bit_count,
                color_id,
                fps,
            },
            frame_offsets,
            frame_sizes,
            top_down,
        })
    }

    pub fn get_frame(&self, index: usize, _cid: i32) -> std::borrow::Cow<'_, [u8]> {
        if index >= self.frame_offsets.len() {
            return std::borrow::Cow::Borrowed(&[]);
        }

        let start = self.frame_offsets[index];
        let chunk_size = self.frame_sizes[index];

        // F3: RGB24 (DIB): filas alineadas a 4 bytes y orden bottom-up salvo
        // top_down. Se sirve compactado (sin padding) y con las filas en
        // orden natural (arriba→abajo).
        if self.info.color_id == 101 && self.info.bytes_per_pixel == 3 {
            let (w, h) = (self.info.width, self.info.height);
            let row_bytes = w * 3;
            let stride = (row_bytes + 3) & !3;
            if start + stride * h > self.mmap.len() || chunk_size < stride * h {
                return std::borrow::Cow::Borrowed(&[]);
            }
            let src = &self.mmap[start..start + stride * h];
            let mut out = vec![0u8; row_bytes * h];
            for y in 0..h {
                let src_y = if self.top_down { y } else { h - 1 - y };
                out[y * row_bytes..(y + 1) * row_bytes]
                    .copy_from_slice(&src[src_y * stride..src_y * stride + row_bytes]);
            }
            return std::borrow::Cow::Owned(out);
        }

        // Esperado para RAW bayer: width*height*bpp
        let expected = self.info.width * self.info.height * self.info.bytes_per_pixel;

        // Si el chunk trae padding/stride, toma el minimo seguro
        let read_size = expected.min(chunk_size);

        if start + read_size > self.mmap.len() {
            return std::borrow::Cow::Borrowed(&[]);
        }

        std::borrow::Cow::Borrowed(&self.mmap[start..start + read_size])
    }
}
