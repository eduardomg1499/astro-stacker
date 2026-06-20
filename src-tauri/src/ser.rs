use byteorder::{BigEndian, ByteOrder, LittleEndian};
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
    pub color_id: i32,
    pub is_little_endian: bool, // <--- NUEVO
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
    is_little_endian: bool,
}

fn read_i32_le(mmap: &[u8], offset: usize) -> i32 {
    LittleEndian::read_i32(&mmap[offset..offset + 4])
}

fn read_i32(mmap: &[u8], offset: usize, is_little_endian: bool) -> i32 {
    if is_little_endian {
        LittleEndian::read_i32(&mmap[offset..offset + 4])
    } else {
        BigEndian::read_i32(&mmap[offset..offset + 4])
    }
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
    (8..=11).contains(&color_id) || (16..=19).contains(&color_id)
}

pub fn ser_color_is_color(color_id: i32) -> bool {
    ser_color_is_bayer(color_id)
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
    is_little_endian: bool,
) -> Option<ParsedSerHeader> {
    let width_i = read_i32(mmap, field_base, is_little_endian);
    let height_i = read_i32(mmap, field_base + 4, is_little_endian);
    let pixel_depth_i = read_i32(mmap, field_base + 8, is_little_endian);
    let frame_count_i = read_i32(mmap, field_base + 12, is_little_endian);

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
    if !(1..=32).contains(&pixel_depth) {
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
        is_little_endian,
    })
}

impl SerReader {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let mmap = unsafe { Mmap::map(&file).map_err(|e| e.to_string())? };

        if mmap.len() < 178 {
            return Err("Archivo muy pequeno".into());
        }

        let color_id_le = read_i32_le(&mmap, 18);
        let color_id_be = BigEndian::read_i32(&mmap[18..22]);

        let standard_le = parse_header_candidate(mmap.len(), &mmap, 26, true, color_id_le, true);
        let legacy_le = parse_header_candidate(mmap.len(), &mmap, 22, false, color_id_le, true);
        let standard_be = parse_header_candidate(mmap.len(), &mmap, 26, true, color_id_be, false);
        let legacy_be = parse_header_candidate(mmap.len(), &mmap, 22, false, color_id_be, false);

        let parsed = [standard_le, legacy_le, standard_be, legacy_be]
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
        let is_little_endian = parsed.is_little_endian;
        let color_id = if is_little_endian {
            color_id_le
        } else {
            color_id_be
        };

        if width == 0 || height == 0 {
            return Err(
                "Error fatal: Dimensiones 0x0. Intenta volver a grabar sin ROI variable.".into(),
            );
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
                color_id,
                is_little_endian,
            },
            header_shift: header_offset,
        })
    }

    pub fn get_frame<'a>(&'a self, index: usize, _cid: i32) -> std::borrow::Cow<'a, [u8]> {
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
