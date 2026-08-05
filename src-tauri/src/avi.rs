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
    /// Los DIB con altura positiva se almacenan bottom-up. El lector siempre
    /// entrega filas en orden visual (arriba -> abajo).
    pub top_down: bool,
    row_stride: usize,
    compact_row_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct RiffChunk {
    id: [u8; 4],
    data_start: usize,
    data_end: usize,
    next: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct MainHeader {
    width: usize,
    height: usize,
    frame_count: usize,
    fps: Option<f64>,
}

#[derive(Debug, Clone)]
struct BitmapHeader {
    width: usize,
    height: usize,
    top_down: bool,
    bit_count: usize,
    compression: [u8; 4],
    size_image: usize,
    identity_gray_palette: bool,
}

#[derive(Debug, Clone)]
struct VideoStream {
    index: usize,
    handler: [u8; 4],
    scale: u32,
    rate: u32,
    frame_count: usize,
    bitmap: BitmapHeader,
}

#[derive(Debug, Clone, Copy)]
enum RawLayout {
    Bgr24,
    Mono8,
    Mono16,
    Bayer { color_id: i32, sample_bits: usize },
}

fn is_avi(m: &[u8]) -> bool {
    m.len() >= 12 && &m[0..4] == b"RIFF" && &m[8..12] == b"AVI "
}

fn fourcc(bytes: &[u8]) -> [u8; 4] {
    let mut tag = [0u8; 4];
    tag.copy_from_slice(&bytes[..4]);
    tag
}

fn fourcc_display(tag: [u8; 4]) -> String {
    if tag == [0, 0, 0, 0] {
        return "BI_RGB(0)".to_string();
    }
    tag.iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        })
        .collect()
}

fn parse_chunk(m: &[u8], cursor: usize, container_end: usize) -> Result<RiffChunk, String> {
    let header_end = cursor
        .checked_add(8)
        .ok_or_else(|| "Overflow al leer un chunk AVI".to_string())?;
    if header_end > container_end || header_end > m.len() {
        return Err("Chunk AVI truncado: falta cabecera de 8 bytes".into());
    }

    let id = fourcc(&m[cursor..cursor + 4]);
    let size = LittleEndian::read_u32(&m[cursor + 4..cursor + 8]) as usize;
    let data_start = cursor + 8;
    let data_end = data_start
        .checked_add(size)
        .ok_or_else(|| "Overflow en el tamano declarado de un chunk AVI".to_string())?;
    if data_end > container_end || data_end > m.len() {
        return Err(format!(
            "Chunk AVI {} truncado: declara {} bytes fuera de su contenedor",
            fourcc_display(id),
            size
        ));
    }

    let next = data_end
        .checked_add(size & 1)
        .ok_or_else(|| "Overflow al alinear un chunk AVI".to_string())?;
    if next > container_end || next > m.len() {
        return Err(format!(
            "Chunk AVI {} truncado: falta su byte de alineacion",
            fourcc_display(id)
        ));
    }

    Ok(RiffChunk {
        id,
        data_start,
        data_end,
        next,
    })
}

fn list_type(m: &[u8], chunk: RiffChunk) -> Result<[u8; 4], String> {
    if chunk.data_end.saturating_sub(chunk.data_start) < 4 {
        return Err("LIST AVI invalida: no contiene tipo de lista".into());
    }
    Ok(fourcc(&m[chunk.data_start..chunk.data_start + 4]))
}

fn parse_main_header(data: &[u8]) -> Result<MainHeader, String> {
    if data.len() < 40 {
        return Err("Cabecera avih truncada (se requieren al menos 40 bytes)".into());
    }
    let microseconds = LittleEndian::read_u32(&data[0..4]);
    let width = LittleEndian::read_u32(&data[32..36]) as usize;
    let height = LittleEndian::read_u32(&data[36..40]) as usize;
    if width == 0 || height == 0 {
        return Err("Cabecera avih con dimensiones nulas".into());
    }
    Ok(MainHeader {
        width,
        height,
        frame_count: LittleEndian::read_u32(&data[16..20]) as usize,
        fps: (microseconds > 0).then(|| 1_000_000.0 / microseconds as f64),
    })
}

fn parse_bitmap_header(data: &[u8]) -> Result<BitmapHeader, String> {
    if data.len() < 40 {
        return Err("Cabecera strf/BITMAPINFOHEADER truncada".into());
    }
    let header_size = LittleEndian::read_u32(&data[0..4]) as usize;
    if header_size < 40 || header_size > data.len() {
        return Err(format!(
            "BITMAPINFOHEADER invalida: biSize={} y strf={} bytes",
            header_size,
            data.len()
        ));
    }

    let width_i32 = LittleEndian::read_i32(&data[4..8]);
    let height_i32 = LittleEndian::read_i32(&data[8..12]);
    if width_i32 <= 0 || height_i32 == 0 || height_i32 == i32::MIN {
        return Err(format!(
            "BITMAPINFOHEADER con dimensiones no soportadas: {}x{}",
            width_i32, height_i32
        ));
    }
    let planes = LittleEndian::read_u16(&data[12..14]);
    if planes != 1 {
        return Err(format!(
            "AVI raw invalido: biPlanes={} (se requiere exactamente 1)",
            planes
        ));
    }

    let bit_count = LittleEndian::read_u16(&data[14..16]) as usize;
    let compression = fourcc(&data[16..20]);
    let size_image = LittleEndian::read_u32(&data[20..24]) as usize;
    let colors_used = LittleEndian::read_u32(&data[32..36]) as usize;

    // BI_RGB de 8 bits describe indices de paleta, no luminancia. Sólo es
    // seguro reinterpretarlo como MONO cuando la paleta está presente y es
    // exactamente la rampa de grises identidad 0..255.
    let identity_gray_palette = if bit_count == 8
        && (compression == [0, 0, 0, 0] || compression == *b"DIB " || compression == *b"RGB ")
    {
        let palette_len = if colors_used == 0 { 256 } else { colors_used };
        if palette_len != 256 {
            false
        } else if let Some(palette_bytes) = palette_len.checked_mul(4) {
            if header_size
                .checked_add(palette_bytes)
                .is_some_and(|end| end <= data.len())
            {
                (0..256).all(|i| {
                    let p = header_size + i * 4;
                    data[p] == i as u8 && data[p + 1] == i as u8 && data[p + 2] == i as u8
                })
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };

    Ok(BitmapHeader {
        width: width_i32 as usize,
        height: height_i32.unsigned_abs() as usize,
        top_down: height_i32 < 0,
        bit_count,
        compression,
        size_image,
        identity_gray_palette,
    })
}

fn parse_stream_list(
    m: &[u8],
    start: usize,
    end: usize,
    index: usize,
) -> Result<Option<VideoStream>, String> {
    let mut cursor = start;
    let mut stream_header: Option<&[u8]> = None;
    let mut bitmap_header: Option<&[u8]> = None;

    while cursor < end {
        let chunk = parse_chunk(m, cursor, end)?;
        match &chunk.id {
            b"strh" => {
                if stream_header
                    .replace(&m[chunk.data_start..chunk.data_end])
                    .is_some()
                {
                    return Err(format!("Stream AVI {} contiene mas de un strh", index));
                }
            }
            b"strf" => {
                if bitmap_header
                    .replace(&m[chunk.data_start..chunk.data_end])
                    .is_some()
                {
                    return Err(format!("Stream AVI {} contiene mas de un strf", index));
                }
            }
            b"indx" | b"ix00" | b"ix01" => {
                return Err(
                    "AVI OpenDML/indice extendido no se procesa en el lector raw nativo; use FFmpeg"
                        .into(),
                );
            }
            _ => {}
        }
        cursor = chunk.next;
    }

    let header = stream_header.ok_or_else(|| format!("Stream AVI {} sin strh", index))?;
    if header.len() < 40 {
        return Err(format!("strh truncado en el stream AVI {}", index));
    }
    if &header[0..4] != b"vids" {
        return Ok(None);
    }
    if header.len() < 56 {
        return Err(format!(
            "AVISTREAMHEADER de video truncada en el stream {}",
            index
        ));
    }
    let bitmap = parse_bitmap_header(
        bitmap_header.ok_or_else(|| format!("Stream de video AVI {} sin strf", index))?,
    )?;

    Ok(Some(VideoStream {
        index,
        handler: fourcc(&header[4..8]),
        scale: LittleEndian::read_u32(&header[20..24]),
        rate: LittleEndian::read_u32(&header[24..28]),
        frame_count: LittleEndian::read_u32(&header[32..36]) as usize,
        bitmap,
    }))
}

fn parse_hdrl(m: &[u8], start: usize, end: usize) -> Result<(MainHeader, VideoStream), String> {
    let mut cursor = start;
    let mut main_header = None;
    let mut video_stream = None;
    let mut stream_index = 0usize;

    while cursor < end {
        let chunk = parse_chunk(m, cursor, end)?;
        if &chunk.id == b"avih" {
            if main_header
                .replace(parse_main_header(&m[chunk.data_start..chunk.data_end])?)
                .is_some()
            {
                return Err("AVI contiene mas de una cabecera avih".into());
            }
        } else if &chunk.id == b"LIST" {
            let kind = list_type(m, chunk)?;
            if &kind == b"strl" {
                let stream =
                    parse_stream_list(m, chunk.data_start + 4, chunk.data_end, stream_index)?;
                if let Some(stream) = stream {
                    if video_stream.replace(stream).is_some() {
                        return Err(
                            "AVI con multiples streams de video: delegue la seleccion a FFmpeg"
                                .into(),
                        );
                    }
                }
                stream_index = stream_index
                    .checked_add(1)
                    .ok_or_else(|| "Demasiados streams en el AVI".to_string())?;
            } else if &kind == b"odml" {
                return Err(
                    "AVI OpenDML (LIST odml) no se procesa en el lector raw nativo; use FFmpeg"
                        .into(),
                );
            }
        } else if &chunk.id == b"dmlh" || &chunk.id == b"indx" {
            return Err("AVI OpenDML no se procesa en el lector raw nativo; use FFmpeg".into());
        }
        cursor = chunk.next;
    }

    Ok((
        main_header.ok_or_else(|| "AVI sin cabecera principal avih".to_string())?,
        video_stream.ok_or_else(|| "AVI sin un stream de video vids".to_string())?,
    ))
}

fn is_raw_wrapper(tag: [u8; 4]) -> bool {
    tag == [0, 0, 0, 0] || tag == *b"DIB " || tag == *b"RGB " || tag == *b"raw "
}

fn bayer_color_id(tag: [u8; 4]) -> Option<i32> {
    match &tag {
        b"RGGB" => Some(8),
        b"GRBG" => Some(9),
        b"GBRG" => Some(10),
        b"BGGR" => Some(11),
        _ => None,
    }
}

fn is_mono8_tag(tag: [u8; 4]) -> bool {
    matches!(&tag, b"Y800" | b"GREY" | b"Y8  ")
}

fn is_mono16_tag(tag: [u8; 4]) -> bool {
    // Y16B es deliberadamente excluido: el pipeline nativo consume u16 LE.
    matches!(&tag, b"Y16 " | b"Y16L")
}

fn explicit_tag_pair(
    handler: [u8; 4],
    compression: [u8; 4],
    predicate: impl Fn([u8; 4]) -> bool,
) -> Option<[u8; 4]> {
    let h = predicate(handler).then_some(handler);
    let c = predicate(compression).then_some(compression);
    match (h, c) {
        (Some(a), Some(b)) if a == b => Some(a),
        (Some(a), None) if is_raw_wrapper(compression) => Some(a),
        (None, Some(b)) if is_raw_wrapper(handler) => Some(b),
        _ => None,
    }
}

fn classify_raw_layout(stream: &VideoStream) -> Result<RawLayout, String> {
    let b = &stream.bitmap;

    if let Some(tag) = explicit_tag_pair(stream.handler, b.compression, |tag| {
        bayer_color_id(tag).is_some()
    }) {
        if !matches!(b.bit_count, 8 | 16) {
            return Err(format!(
                "AVI Bayer {} con biBitCount={}; solo se admiten 8 o 16 bits raw",
                fourcc_display(tag),
                b.bit_count
            ));
        }
        return Ok(RawLayout::Bayer {
            color_id: bayer_color_id(tag).expect("tag Bayer ya validado"),
            sample_bits: b.bit_count,
        });
    }

    if explicit_tag_pair(stream.handler, b.compression, is_mono8_tag).is_some() {
        if b.bit_count != 8 {
            return Err(format!(
                "FourCC MONO8 con biBitCount={}; se requieren 8 bits",
                b.bit_count
            ));
        }
        return Ok(RawLayout::Mono8);
    }
    if explicit_tag_pair(stream.handler, b.compression, is_mono16_tag).is_some() {
        if b.bit_count != 16 {
            return Err(format!(
                "FourCC MONO16 con biBitCount={}; se requieren 16 bits",
                b.bit_count
            ));
        }
        return Ok(RawLayout::Mono16);
    }

    if b.bit_count == 8
        && b.identity_gray_palette
        && is_raw_wrapper(stream.handler)
        && is_raw_wrapper(b.compression)
    {
        return Ok(RawLayout::Mono8);
    }

    if b.bit_count == 24 && is_raw_wrapper(stream.handler) && is_raw_wrapper(b.compression) {
        return Ok(RawLayout::Bgr24);
    }

    Err(format!(
        "Codec/layout AVI no soportado por el lector raw nativo: handler={}, biCompression={}, biBitCount={}. Un codec comprimido o ambiguo debe decodificarse con FFmpeg",
        fourcc_display(stream.handler),
        fourcc_display(b.compression),
        b.bit_count
    ))
}

fn stream_chunk_id(id: [u8; 4]) -> Option<(usize, [u8; 2])> {
    if !id[0].is_ascii_digit() || !id[1].is_ascii_digit() {
        return None;
    }
    Some((
        ((id[0] - b'0') as usize) * 10 + (id[1] - b'0') as usize,
        [id[2], id[3]],
    ))
}

fn collect_raw_frames(
    m: &[u8],
    start: usize,
    end: usize,
    stream_index: usize,
) -> Result<(Vec<usize>, Vec<usize>), String> {
    if stream_index > 99 {
        return Err("El stream de video AVI excede el indice 99; use FFmpeg".into());
    }
    let mut offsets = Vec::new();
    let mut sizes = Vec::new();
    let mut cursor = start;

    while cursor < end {
        let chunk = parse_chunk(m, cursor, end)?;
        if &chunk.id == b"LIST" {
            let kind = list_type(m, chunk)?;
            if &kind == b"rec " {
                return Err(
                    "AVI con LIST rec no se procesa en el lector raw nativo; use FFmpeg".into(),
                );
            }
            return Err(format!(
                "LIST {} anidada dentro de movi no soportada; use FFmpeg",
                fourcc_display(kind)
            ));
        }
        if &chunk.id == b"RIFF"
            || &chunk.id == b"indx"
            || (chunk.id[0] == b'i' && chunk.id[1] == b'x')
        {
            return Err(
                "AVI OpenDML/AVIX no se procesa en el lector raw nativo; use FFmpeg".into(),
            );
        }

        if let Some((index, suffix)) = stream_chunk_id(chunk.id) {
            if index == stream_index {
                match &suffix {
                    b"db" => {
                        let size = chunk.data_end - chunk.data_start;
                        if size == 0 {
                            return Err("Frame AVI raw vacio dentro de movi".into());
                        }
                        offsets.push(chunk.data_start);
                        sizes.push(size);
                    }
                    b"dc" => {
                        return Err(
                            "El stream AVI usa chunks ##dc (video comprimido); el lector nativo solo acepta ##db raw. Use FFmpeg"
                                .into(),
                        );
                    }
                    b"pc" => {
                        return Err(
                            "El stream AVI cambia la paleta durante movi; use FFmpeg para preservar el color"
                                .into(),
                        );
                    }
                    _ => {}
                }
            }
        }
        cursor = chunk.next;
    }

    if offsets.is_empty() {
        return Err("No se encontraron frames ##db raw del stream de video dentro de movi".into());
    }
    Ok((offsets, sizes))
}

impl AviReader {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let mmap = unsafe { Mmap::map(&file).map_err(|e| e.to_string())? };

        if !is_avi(&mmap) {
            return Err("No es un AVI RIFF clasico valido (RIFF/AVI)".into());
        }

        let riff_size = LittleEndian::read_u32(&mmap[4..8]) as usize;
        if riff_size < 4 {
            return Err("Cabecera RIFF AVI con tamano invalido".into());
        }
        let riff_end = 8usize
            .checked_add(riff_size)
            .ok_or_else(|| "Overflow en el tamano RIFF AVI".to_string())?;
        if riff_end > mmap.len() {
            return Err(format!(
                "AVI truncado: RIFF declara {} bytes y el archivo contiene {}",
                riff_end,
                mmap.len()
            ));
        }
        let physical_end = riff_end
            .checked_add(riff_size & 1)
            .ok_or_else(|| "Overflow al alinear RIFF AVI".to_string())?;
        if physical_end > mmap.len() {
            return Err("AVI truncado: falta byte de alineacion RIFF".into());
        }
        if physical_end != mmap.len() {
            let trailing = &mmap[physical_end..];
            if trailing.len() >= 12 && &trailing[0..4] == b"RIFF" && &trailing[8..12] == b"AVIX" {
                return Err(
                    "AVI OpenDML con segmento RIFF AVIX: delegue la decodificacion a FFmpeg".into(),
                );
            }
            return Err(format!(
                "AVI RIFF contiene {} bytes adicionales no declarados; se rechaza para no interpretar payload ambiguo",
                mmap.len() - physical_end
            ));
        }

        let mut cursor = 12usize;
        let mut hdrl = None;
        let mut movi = None;
        while cursor < riff_end {
            let chunk = parse_chunk(&mmap, cursor, riff_end)?;
            if &chunk.id == b"LIST" {
                let kind = list_type(&mmap, chunk)?;
                match &kind {
                    b"hdrl" => {
                        if hdrl
                            .replace((chunk.data_start + 4, chunk.data_end))
                            .is_some()
                        {
                            return Err("AVI contiene mas de una LIST hdrl".into());
                        }
                    }
                    b"movi" => {
                        if movi
                            .replace((chunk.data_start + 4, chunk.data_end))
                            .is_some()
                        {
                            return Err("AVI contiene mas de una LIST movi; use FFmpeg".into());
                        }
                    }
                    b"odml" => {
                        return Err(
                            "AVI OpenDML (LIST odml) no se procesa nativamente; use FFmpeg".into(),
                        );
                    }
                    _ => {}
                }
            } else if &chunk.id == b"RIFF" || &chunk.id == b"indx" {
                return Err(
                    "AVI OpenDML/AVIX no se procesa en el lector raw nativo; use FFmpeg".into(),
                );
            }
            cursor = chunk.next;
        }

        let (hdrl_start, hdrl_end) = hdrl.ok_or_else(|| "AVI sin LIST hdrl".to_string())?;
        let (movi_start, movi_end) = movi.ok_or_else(|| "AVI sin LIST movi".to_string())?;
        let (main, stream) = parse_hdrl(&mmap, hdrl_start, hdrl_end)?;
        let layout = classify_raw_layout(&stream)?;

        if main.width != stream.bitmap.width || main.height != stream.bitmap.height {
            return Err(format!(
                "Dimensiones AVI inconsistentes: avih={}x{}, strf={}x{}",
                main.width, main.height, stream.bitmap.width, stream.bitmap.height
            ));
        }

        let (frame_offsets, frame_sizes) =
            collect_raw_frames(&mmap, movi_start, movi_end, stream.index)?;
        let actual_frames = frame_offsets.len();
        if main.frame_count != 0 && main.frame_count != actual_frames {
            return Err(format!(
                "Conteo AVI inconsistente: avih declara {} frames y movi contiene {} raw",
                main.frame_count, actual_frames
            ));
        }
        if stream.frame_count != 0 && stream.frame_count != actual_frames {
            return Err(format!(
                "Conteo AVI inconsistente: strh declara {} frames y movi contiene {} raw",
                stream.frame_count, actual_frames
            ));
        }

        let (bytes_per_pixel, sample_bits, color_id) = match layout {
            RawLayout::Bgr24 => (3usize, 8usize, 101),
            RawLayout::Mono8 => (1, 8, 0),
            RawLayout::Mono16 => (2, 16, 0),
            RawLayout::Bayer {
                color_id,
                sample_bits,
            } => ((sample_bits / 8), sample_bits, color_id),
        };
        let compact_row_bytes = stream
            .bitmap
            .width
            .checked_mul(bytes_per_pixel)
            .ok_or_else(|| "Dimensiones AVI exceden el espacio direccionable".to_string())?;
        let dib_stride = compact_row_bytes
            .checked_add(3)
            .map(|n| n & !3)
            .ok_or_else(|| "Overflow al calcular stride AVI".to_string())?;
        let compact_size = compact_row_bytes
            .checked_mul(stream.bitmap.height)
            .ok_or_else(|| "Overflow al calcular tamano de frame AVI".to_string())?;
        let padded_size = dib_stride
            .checked_mul(stream.bitmap.height)
            .ok_or_else(|| "Overflow al calcular tamano DIB AVI".to_string())?;

        let first_size = frame_sizes[0];
        if frame_sizes.iter().any(|&size| size != first_size) {
            return Err("Los chunks de video AVI raw tienen tamanos distintos; use FFmpeg".into());
        }
        let row_stride = match layout {
            RawLayout::Bgr24 => {
                if first_size != padded_size {
                    return Err(format!(
                        "Frame BGR24 AVI con tamano {}: se esperaban {} bytes (stride DIB de 4 bytes)",
                        first_size, padded_size
                    ));
                }
                dib_stride
            }
            _ if first_size == compact_size => compact_row_bytes,
            _ if first_size == padded_size => dib_stride,
            _ => {
                return Err(format!(
                    "Frame AVI raw con tamano {} incompatible con layout compacto {} y DIB {}",
                    first_size, compact_size, padded_size
                ));
            }
        };
        if stream.bitmap.size_image != 0 && stream.bitmap.size_image != first_size {
            return Err(format!(
                "biSizeImage={} no coincide con los chunks raw de {} bytes",
                stream.bitmap.size_image, first_size
            ));
        }

        // Todos los rangos se validan una vez al abrir; get_frame no necesita
        // reinterpretar ni recortar payloads potencialmente comprimidos.
        for (&offset, &size) in frame_offsets.iter().zip(&frame_sizes) {
            if offset.checked_add(size).is_none_or(|end| end > mmap.len()) {
                return Err("Frame AVI raw apunta fuera del archivo".into());
            }
        }

        let fps = if stream.scale > 0 && stream.rate > 0 {
            stream.rate as f64 / stream.scale as f64
        } else {
            main.fps.unwrap_or(30.0)
        };
        if !fps.is_finite() || fps <= 0.0 {
            return Err("AVI con tasa de cuadros invalida".into());
        }

        Ok(AviReader {
            mmap: Arc::new(mmap),
            info: AviInfo {
                width: stream.bitmap.width,
                height: stream.bitmap.height,
                frame_count: actual_frames,
                bytes_per_pixel,
                sample_bits,
                color_id,
                fps,
            },
            frame_offsets,
            frame_sizes,
            top_down: stream.bitmap.top_down,
            row_stride,
            compact_row_bytes,
        })
    }

    pub fn get_frame(&self, index: usize, _cid: i32) -> std::borrow::Cow<'_, [u8]> {
        if index >= self.frame_offsets.len() {
            return std::borrow::Cow::Borrowed(&[]);
        }

        let start = self.frame_offsets[index];
        let chunk_size = self.frame_sizes[index];
        let expected = match self.row_stride.checked_mul(self.info.height) {
            Some(expected) => expected,
            None => return std::borrow::Cow::Borrowed(&[]),
        };
        if chunk_size != expected
            || start
                .checked_add(expected)
                .is_none_or(|end| end > self.mmap.len())
        {
            return std::borrow::Cow::Borrowed(&[]);
        }

        let src = &self.mmap[start..start + expected];
        if self.top_down && self.row_stride == self.compact_row_bytes {
            return std::borrow::Cow::Borrowed(src);
        }

        let output_size = match self.compact_row_bytes.checked_mul(self.info.height) {
            Some(size) => size,
            None => return std::borrow::Cow::Borrowed(&[]),
        };
        let mut out = vec![0u8; output_size];
        for y in 0..self.info.height {
            let src_y = if self.top_down {
                y
            } else {
                self.info.height - 1 - y
            };
            let src_start = src_y * self.row_stride;
            let dst_start = y * self.compact_row_bytes;
            out[dst_start..dst_start + self.compact_row_bytes]
                .copy_from_slice(&src[src_start..src_start + self.compact_row_bytes]);
        }
        std::borrow::Cow::Owned(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(1);

    fn push_chunk(out: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(id);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        if data.len() & 1 != 0 {
            out.push(0);
        }
    }

    fn list(kind: &[u8; 4], children: &[u8]) -> Vec<u8> {
        let mut data = Vec::with_capacity(4 + children.len());
        data.extend_from_slice(kind);
        data.extend_from_slice(children);
        let mut out = Vec::new();
        push_chunk(&mut out, b"LIST", &data);
        out
    }

    fn make_avi(
        width: u32,
        height: i32,
        bit_count: u16,
        handler: [u8; 4],
        compression: [u8; 4],
        frame_chunk: [u8; 4],
        frame: &[u8],
        identity_gray_palette: bool,
        wrap_in_rec: bool,
    ) -> Vec<u8> {
        let mut avih = vec![0u8; 56];
        avih[0..4].copy_from_slice(&33_333u32.to_le_bytes());
        avih[16..20].copy_from_slice(&1u32.to_le_bytes());
        avih[24..28].copy_from_slice(&1u32.to_le_bytes());
        avih[32..36].copy_from_slice(&width.to_le_bytes());
        avih[36..40].copy_from_slice(&height.unsigned_abs().to_le_bytes());

        let mut strh = vec![0u8; 56];
        strh[0..4].copy_from_slice(b"vids");
        strh[4..8].copy_from_slice(&handler);
        strh[20..24].copy_from_slice(&1u32.to_le_bytes());
        strh[24..28].copy_from_slice(&30u32.to_le_bytes());
        strh[32..36].copy_from_slice(&1u32.to_le_bytes());

        let mut strf = vec![0u8; 40];
        strf[0..4].copy_from_slice(&40u32.to_le_bytes());
        strf[4..8].copy_from_slice(&(width as i32).to_le_bytes());
        strf[8..12].copy_from_slice(&height.to_le_bytes());
        strf[12..14].copy_from_slice(&1u16.to_le_bytes());
        strf[14..16].copy_from_slice(&bit_count.to_le_bytes());
        strf[16..20].copy_from_slice(&compression);
        strf[20..24].copy_from_slice(&(frame.len() as u32).to_le_bytes());
        if identity_gray_palette {
            assert_eq!(bit_count, 8, "la paleta de prueba requiere AVI de 8 bits");
            strf[32..36].copy_from_slice(&256u32.to_le_bytes());
            for value in 0..=255u8 {
                strf.extend_from_slice(&[value, value, value, 0]);
            }
        }

        let mut stream_children = Vec::new();
        push_chunk(&mut stream_children, b"strh", &strh);
        push_chunk(&mut stream_children, b"strf", &strf);
        let stream_list = list(b"strl", &stream_children);

        let mut hdrl_children = Vec::new();
        push_chunk(&mut hdrl_children, b"avih", &avih);
        hdrl_children.extend_from_slice(&stream_list);
        let hdrl = list(b"hdrl", &hdrl_children);

        let mut frame_children = Vec::new();
        push_chunk(&mut frame_children, &frame_chunk, frame);
        let movi_children = if wrap_in_rec {
            list(b"rec ", &frame_children)
        } else {
            frame_children
        };
        let movi = list(b"movi", &movi_children);

        let mut riff_payload = Vec::new();
        riff_payload.extend_from_slice(b"AVI ");
        riff_payload.extend_from_slice(&hdrl);
        riff_payload.extend_from_slice(&movi);
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(riff_payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&riff_payload);
        if riff_payload.len() & 1 != 0 {
            out.push(0);
        }
        out
    }

    fn write_test_file(tag: &str, bytes: &[u8]) -> PathBuf {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("zas-avi-{}-{}-{}.avi", tag, std::process::id(), id));
        fs::write(&path, bytes).expect("write AVI test fixture");
        path
    }

    fn open_bytes(tag: &str, bytes: &[u8]) -> Result<AviReader, String> {
        let path = write_test_file(tag, bytes);
        let result = AviReader::new(&path);
        let _ = fs::remove_file(path);
        result
    }

    #[test]
    fn accepts_verified_raw_bgr24_and_removes_stride_bottom_up() {
        // 2x2 BGR24: 6 bytes utiles + 2 de padding por fila. El payload DIB
        // almacena primero la fila inferior.
        let bottom = [10, 11, 12, 13, 14, 15, 0, 0];
        let top = [20, 21, 22, 23, 24, 25, 0, 0];
        let frame: Vec<u8> = bottom.into_iter().chain(top).collect();
        let avi = make_avi(
            2,
            2,
            24,
            *b"DIB ",
            [0, 0, 0, 0],
            *b"00db",
            &frame,
            false,
            false,
        );
        let reader = open_bytes("bgr24", &avi).expect("raw BGR24 debe abrir");

        assert_eq!(reader.info.width, 2);
        assert_eq!(reader.info.height, 2);
        assert_eq!(reader.info.bytes_per_pixel, 3);
        assert_eq!(reader.info.sample_bits, 8);
        assert_eq!(reader.info.color_id, 101);
        assert_eq!(
            reader.get_frame(0, 101).as_ref(),
            &[20, 21, 22, 23, 24, 25, 10, 11, 12, 13, 14, 15]
        );
    }

    #[test]
    fn accepts_verified_raw_bayer16() {
        let frame = [1, 0, 2, 0, 3, 0, 4, 0];
        let avi = make_avi(
            2, -2, 16, *b"RGGB", *b"RGGB", *b"00db", &frame, false, false,
        );
        let reader = open_bytes("bayer16", &avi).expect("Bayer16 raw debe abrir");

        assert_eq!(reader.info.bytes_per_pixel, 2);
        assert_eq!(reader.info.sample_bits, 16);
        assert_eq!(reader.info.color_id, 8);
        assert_eq!(reader.get_frame(0, 8).as_ref(), frame);
    }

    #[test]
    fn accepts_verified_identity_gray_palette_as_manual_cfa_candidate() {
        // SharpCap puede envolver RAW8 en un DIB de 8 bits cuya paleta es una
        // rampa gris identidad. Los índices siguen siendo el mosaico original;
        // clasificarlos como RGB impediría que el usuario indique RGGB.
        let frame = [12, 34, 56, 78, 90, 123, 167, 201];
        let avi = make_avi(
            4,
            -2,
            8,
            *b"raw ",
            [0, 0, 0, 0],
            *b"00db",
            &frame,
            true,
            false,
        );
        let reader = open_bytes("gray-palette", &avi).expect("DIB gris RAW8 debe abrir");

        assert_eq!(reader.info.bytes_per_pixel, 1);
        assert_eq!(reader.info.sample_bits, 8);
        assert_eq!(reader.info.color_id, 0);
        assert_eq!(reader.get_frame(0, 8).as_ref(), frame);
    }

    #[test]
    fn rejects_compressed_or_unknown_fourcc_instead_of_treating_it_as_pixels() {
        for codec in [*b"H264", *b"MJPG", *b"XVID", *b"ZZZZ"] {
            let avi = make_avi(2, 2, 24, codec, codec, *b"00dc", &[0u8; 16], false, false);
            let error = open_bytes("compressed", &avi).expect_err("codec debe rechazarse");
            assert!(
                error.contains("no soportado") || error.contains("comprimido"),
                "error inesperado para {}: {}",
                fourcc_display(codec),
                error
            );
        }
    }

    #[test]
    fn rejects_dc_chunks_even_when_headers_claim_raw() {
        let avi = make_avi(
            2,
            2,
            24,
            *b"DIB ",
            [0, 0, 0, 0],
            *b"00dc",
            &[0u8; 16],
            false,
            false,
        );
        let error = open_bytes("dc", &avi).expect_err("##dc nunca debe pasar como raw");
        assert!(
            error.contains("##dc") && error.contains("FFmpeg"),
            "{error}"
        );
    }

    #[test]
    fn rejects_bad_raw_frame_size_instead_of_truncating() {
        let avi = make_avi(
            2,
            2,
            24,
            *b"DIB ",
            [0, 0, 0, 0],
            *b"00db",
            &[0u8; 12],
            false,
            false,
        );
        let error = open_bytes("bad-size", &avi).expect_err("stride invalido debe rechazarse");
        assert!(
            error.contains("tamano") || error.contains("stride"),
            "{error}"
        );
    }

    #[test]
    fn rejects_list_rec_and_open_dml_avix() {
        let rec = make_avi(
            2,
            2,
            24,
            *b"DIB ",
            [0, 0, 0, 0],
            *b"00db",
            &[0u8; 16],
            false,
            true,
        );
        let error = open_bytes("rec", &rec).expect_err("LIST rec debe delegarse");
        assert!(
            error.contains("LIST rec") && error.contains("FFmpeg"),
            "{error}"
        );

        let mut avix = make_avi(
            2,
            2,
            24,
            *b"DIB ",
            [0, 0, 0, 0],
            *b"00db",
            &[0u8; 16],
            false,
            false,
        );
        avix.extend_from_slice(b"RIFF\x04\x00\x00\x00AVIX");
        let error = open_bytes("avix", &avix).expect_err("AVIX debe delegarse");
        assert!(
            error.contains("AVIX") && error.contains("FFmpeg"),
            "{error}"
        );
    }
}
