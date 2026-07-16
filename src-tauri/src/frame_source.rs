use serde::Serialize;

/// Metadatos invariantes que acompañan cada lote. Conservan la geometría y la
/// interpretación del origen en vez de inferirlas nuevamente en cada etapa.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameDescriptor {
    pub source_kind: String,
    pub width: usize,
    pub height: usize,
    pub frame_count: usize,
    pub bytes_per_pixel: usize,
    pub sample_bits: usize,
    pub color_id: i32,
    pub bayer: Option<String>,
    pub little_endian: bool,
    pub rotation_degrees: i32,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameRoi {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

/// Lote con índices absolutos. Nunca se renumeran después de decode/crop, por
/// lo que análisis, selección y apilado comparten exactamente el mismo frame.
pub struct FrameBatch {
    pub descriptor: FrameDescriptor,
    pub roi: Option<FrameRoi>,
    pub indices: Vec<usize>,
    pub frames: Vec<Vec<u8>>,
}

pub trait FrameSource: Send + Sync {
    fn descriptor(&self) -> FrameDescriptor;
    fn read_batch(&self, indices: &[usize], roi: Option<FrameRoi>) -> Result<FrameBatch, String>;
}

#[derive(Clone)]
pub struct UnifiedFrameSource {
    input: crate::VideoInput,
    color_id: i32,
}

impl UnifiedFrameSource {
    pub(crate) fn from_input(input: crate::VideoInput, color_id: i32) -> Self {
        Self { input, color_id }
    }
}

impl FrameSource for UnifiedFrameSource {
    fn descriptor(&self) -> FrameDescriptor {
        let source_kind = match &self.input {
            crate::VideoInput::Ser(_) => "SER",
            crate::VideoInput::Avi(_) => "AVI",
            crate::VideoInput::Ffmpeg(_) => "FFmpeg",
            crate::VideoInput::Fits(_) => "FITS sequence",
        };
        let little_endian = match &self.input {
            crate::VideoInput::Ser(r) => r.info.is_little_endian,
            _ => true,
        };
        FrameDescriptor {
            source_kind: source_kind.into(),
            width: self.input.width(),
            height: self.input.height(),
            frame_count: self.input.frame_count(),
            bytes_per_pixel: self.input.bpp(),
            sample_bits: self.input.sample_bits(),
            color_id: self.color_id,
            bayer: crate::ser::ser_color_is_bayer(self.color_id)
                .then(|| crate::ser::ser_pattern_name(self.color_id).to_string()),
            little_endian,
            rotation_degrees: self.input.rotation(),
        }
    }

    fn read_batch(&self, indices: &[usize], roi: Option<FrameRoi>) -> Result<FrameBatch, String> {
        let desc = self.descriptor();
        let roi = roi.map(|r| FrameRoi {
            x: r.x.min(desc.width),
            y: r.y.min(desc.height),
            width: r.width.min(desc.width.saturating_sub(r.x.min(desc.width))),
            height: r
                .height
                .min(desc.height.saturating_sub(r.y.min(desc.height))),
        });
        if roi.is_some_and(|r| r.width == 0 || r.height == 0) {
            return Err("FrameBatch con ROI vacía".into());
        }
        for &index in indices {
            if index >= desc.frame_count {
                return Err(format!("Índice de frame fuera de rango: {index}"));
            }
        }
        let frames = match &self.input {
            // Un índice de frame es una posición de decodificación, no un
            // timestamp. Para FFmpeg recorremos desde cero una sola vez: usar
            // `-ss index/fps` no es exacto con GOP largos o video VFR.
            crate::VideoInput::Ffmpeg(reader) => {
                read_ffmpeg_batch_exact(reader, indices, roi, self.color_id)?
            }
            _ => {
                let mut frames = Vec::with_capacity(indices.len());
                for &index in indices {
                    let bytes = match roi {
                        Some(r) => self
                            .input
                            .get_frame_roi(index, r.x, r.y, r.width, r.height, self.color_id)
                            .into_owned(),
                        None => self.input.get_frame(index, self.color_id).into_owned(),
                    };
                    if bytes.is_empty() {
                        return Err(format!("El origen devolvió vacío el frame {index}"));
                    }
                    frames.push(bytes);
                }
                frames
            }
        };
        Ok(FrameBatch {
            descriptor: desc,
            roi,
            indices: indices.to_vec(),
            frames,
        })
    }
}

fn read_ffmpeg_batch_exact(
    reader: &crate::FfmpegReader,
    indices: &[usize],
    roi: Option<FrameRoi>,
    color_id: i32,
) -> Result<Vec<Vec<u8>>, String> {
    if indices.is_empty() {
        return Ok(Vec::new());
    }
    let region = roi.unwrap_or(FrameRoi {
        x: 0,
        y: 0,
        width: reader.width,
        height: reader.height,
    });
    let bytes_per_pixel = if crate::ffmpeg_stream_is_color(color_id) {
        6
    } else {
        2
    };
    let frame_size = region
        .width
        .saturating_mul(region.height)
        .saturating_mul(bytes_per_pixel);
    if frame_size == 0 {
        return Err("FrameSource FFmpeg con geometría vacía".into());
    }

    // La referencia robusta pide normalmente 12–20 posiciones dispersas. El
    // camino anterior recorría desde cero y, aunque sólo conservaba esas
    // posiciones, FFmpeg convertía/reescalaba/enviaba TODOS los frames previos
    // como gray16/rgb48. En 4K eso significa cientos de GiB por el pipe. `n`
    // del filtro select es la posición absoluta de decode, así que conserva la
    // exactitud con GOP/B-frames/VFR y sólo materializa los cuadros solicitados.
    let mut selected_indices = indices.to_vec();
    selected_indices.sort_unstable();
    selected_indices.dedup();
    if crate::ffmpeg_exact_frame_select_filter(&selected_indices).is_ok() {
        let mut stream = crate::FfmpegStreamIterator::new_selected(
            &reader.path,
            reader.width,
            reader.height,
            region.x,
            region.y,
            region.width,
            region.height,
            color_id,
            &reader.ffmpeg_path,
            None,
            &reader.codec_name,
            reader.rotation,
            &selected_indices,
        )?;
        let mut decoded = std::collections::HashMap::with_capacity(selected_indices.len());
        let mut buffer = vec![0u8; frame_size];
        for &index in &selected_indices {
            if !stream.read_frame_into(&mut buffer) {
                return Err(format!(
                    "FFmpeg terminó antes de materializar el frame exacto {index}"
                ));
            }
            decoded.insert(index, buffer.clone());
        }
        // Mover cada frame al resultado, no clonarlo. En RGB48 3312x5888 una
        // copia son ~111.6 MiB; el `get().cloned()` anterior duplicaba el top-N
        // completo justo en el pico de la referencia robusta (hasta 2.23 GiB
        // extra para 20 frames). Sólo duplicamos si el caller pidió de forma
        // explícita el mismo índice más de una vez.
        let mut remaining_uses = std::collections::HashMap::new();
        for &index in indices {
            *remaining_uses.entry(index).or_insert(0usize) += 1;
        }
        let mut ordered = Vec::with_capacity(indices.len());
        for &index in indices {
            let uses = remaining_uses
                .get_mut(&index)
                .ok_or_else(|| format!("FFmpeg perdió el contador del frame {index}"))?;
            let frame = if *uses == 1 {
                decoded
                    .remove(&index)
                    .ok_or_else(|| format!("FFmpeg no entregó el frame exacto {index}"))?
            } else {
                decoded
                    .get(&index)
                    .cloned()
                    .ok_or_else(|| format!("FFmpeg no entregó el frame exacto {index}"))?
            };
            *uses -= 1;
            ordered.push(frame);
        }
        return Ok(ordered);
    }

    // Esta ruta es el fallback contractual de FrameSource. La selección
    // hardware/CPU cronometrada vive en los coordinadores de análisis/apilado;
    // aquí CPU evita etiquetar `-hwaccel auto` como aceleración confirmada.
    let mut stream = crate::FfmpegStreamIterator::new(
        &reader.path,
        reader.width,
        reader.height,
        region.x,
        region.y,
        region.width,
        region.height,
        color_id,
        &reader.ffmpeg_path,
        None,
        None,
        &reader.codec_name,
        reader.rotation,
    )?;
    collect_exact_frames(indices, frame_size, |buffer| stream.read_frame_into(buffer))
}

fn collect_exact_frames(
    indices: &[usize],
    frame_size: usize,
    mut read_next: impl FnMut(&mut [u8]) -> bool,
) -> Result<Vec<Vec<u8>>, String> {
    if indices.is_empty() {
        return Ok(Vec::new());
    }
    let last = *indices.iter().max().unwrap_or(&0);
    let wanted: std::collections::HashSet<usize> = indices.iter().copied().collect();
    let mut decoded = std::collections::HashMap::<usize, Vec<u8>>::new();
    let mut buffer = vec![0u8; frame_size];
    for index in 0..=last {
        if !read_next(&mut buffer) {
            return Err(format!(
                "FFmpeg terminó antes del frame solicitado {last}; último frame completo: {}",
                index.saturating_sub(1)
            ));
        }
        if wanted.contains(&index) {
            decoded.insert(index, buffer.clone());
        }
    }
    indices
        .iter()
        .map(|index| {
            decoded
                .get(index)
                .cloned()
                .ok_or_else(|| format!("FFmpeg no entregó el frame exacto {index}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{collect_exact_frames, FrameRoi, FrameSource, UnifiedFrameSource};

    /// Genera un MP4 sintético con ffmpeg del sistema. Devuelve None si no hay
    /// ffmpeg disponible (el test se considera no aplicable). `tag` diferencia
    /// las fixtures de tests que corren en paralelo en el mismo proceso.
    fn synthetic_mp4(tag: &str, frames: usize, fragmented: bool) -> Option<std::path::PathBuf> {
        let path = std::env::temp_dir().join(format!(
            "zas_ffmpeg_fixture_{tag}_{}_{}_{}.mp4",
            std::process::id(),
            frames,
            fragmented
        ));
        let duration = frames as f64 / 30.0;
        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"]);
        cmd.arg(format!("testsrc2=size=64x48:rate=30:duration={duration}"));
        if fragmented {
            // Sin nb_frames en el contenedor: el caso que obliga al conteo por
            // paquetes (fragmented MP4 de drones/móviles, dumps de stream).
            cmd.args(["-movflags", "frag_keyframe+empty_moov"]);
        }
        cmd.args(["-c:v", "libx264", "-pix_fmt", "yuv420p"]);
        cmd.arg(&path);
        let ok = cmd.status().map(|s| s.success()).unwrap_or(false);
        ok.then_some(path)
    }

    fn ffmpeg_reader_for(path: &std::path::Path, frames: usize, fps: f64) -> crate::FfmpegReader {
        crate::FfmpegReader {
            path: path.to_string_lossy().into_owned(),
            width: 64,
            height: 48,
            frame_count: frames,
            frame_count_exact: true,
            bytes_per_pixel: 2,
            sample_bits: 8,
            color_id: 0,
            ffmpeg_path: "ffmpeg".into(),
            fps,
            is_color: false,
            rotation: 0,
            codec_name: "h264".into(),
            stream_cache: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[test]
    #[ignore = "requiere ffmpeg/ffprobe del sistema"]
    fn ffmpeg_batch_decodes_exact_indices_from_real_mp4() {
        let Some(path) = synthetic_mp4("batch", 40, false) else {
            eprintln!("ffmpeg no disponible; test omitido");
            return;
        };
        let reader = ffmpeg_reader_for(&path, 40, 30.0);
        let source = UnifiedFrameSource::from_input(crate::VideoInput::Ffmpeg(reader), 0);
        let batch = source.read_batch(&[39, 0, 17], None).unwrap();
        assert_eq!(batch.indices, vec![39, 0, 17]);
        assert!(batch.frames.iter().all(|f| f.len() == 64 * 48 * 2));
        assert!(
            batch.frames.iter().all(|f| f.iter().any(|&b| b != 0)),
            "ningún frame decodificado puede ser negro absoluto"
        );
        assert_ne!(
            batch.frames[0], batch.frames[1],
            "testsrc2 anima: el frame 39 debe diferir del 0"
        );
        let roi = source
            .read_batch(
                &[5],
                Some(FrameRoi { x: 8, y: 4, width: 32, height: 24 }),
            )
            .unwrap();
        assert_eq!(roi.frames[0].len(), 32 * 24 * 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    #[ignore = "requiere ffmpeg/ffprobe del sistema"]
    fn ffmpeg_analysis_green_is_bit_exact_to_rgb48_green_channel() {
        let Some(path) = synthetic_mp4("green-plane", 8, false) else {
            eprintln!("ffmpeg no disponible; test omitido");
            return;
        };
        let mut rgb = crate::FfmpegStreamIterator::new(
            &path.to_string_lossy(),
            64,
            48,
            0,
            0,
            64,
            48,
            100,
            "ffmpeg",
            None,
            None,
            "h264",
            0,
        )
        .unwrap();
        let never_cancel: crate::FfmpegCancelCheck = std::sync::Arc::new(|| false);
        let mut green = crate::FfmpegStreamIterator::new_cancelable_analysis_green(
            &path.to_string_lossy(),
            64,
            48,
            0,
            0,
            64,
            48,
            100,
            "ffmpeg",
            None,
            None,
            "h264",
            0,
            never_cancel,
        )
        .unwrap();
        let mut rgb_frame = vec![0u8; 64 * 48 * 6];
        let mut green_frame = vec![0u8; 64 * 48 * 2];
        assert!(rgb.read_frame_into(&mut rgb_frame));
        assert!(green.read_frame_into(&mut green_frame));
        for (pixel, actual_green) in rgb_frame
            .chunks_exact(6)
            .zip(green_frame.chunks_exact(2))
        {
            assert_eq!(actual_green, &pixel[2..4]);
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    #[ignore = "requiere ffmpeg/ffprobe del sistema"]
    fn ffmpeg_cancelable_select_handles_more_than_256_exact_rgb_frames() {
        let Some(path) = synthetic_mp4("selected-stack", 2056, false) else {
            eprintln!("ffmpeg no disponible; test omitido");
            return;
        };
        let selected: Vec<usize> = (0..2056).step_by(2).collect();
        assert_eq!(selected.len(), 1028);
        let never_cancel: crate::FfmpegCancelCheck = std::sync::Arc::new(|| false);
        let mut stream = crate::FfmpegStreamIterator::new_cancelable_selected(
            &path.to_string_lossy(),
            64,
            48,
            0,
            0,
            64,
            48,
            100,
            "ffmpeg",
            None,
            "h264",
            0,
            &selected,
            never_cancel,
        )
        .unwrap();
        // Contrato de exactitud: comparar los 1028 outputs contra un decode
        // secuencial del mismo H.264 (incluye B-frames), no sólo comprobar que
        // FFmpeg produjo la cantidad esperada.
        let mut sequential = crate::FfmpegStreamIterator::new(
            &path.to_string_lossy(),
            64,
            48,
            0,
            0,
            64,
            48,
            100,
            "ffmpeg",
            None,
            None,
            "h264",
            0,
        )
        .unwrap();
        let mut selected_frame = vec![0u8; 64 * 48 * 6];
        let mut sequential_frame = vec![0u8; 64 * 48 * 6];
        let mut sequential_index = 0usize;
        let mut first = Vec::new();
        let mut last = Vec::new();
        for (position, &expected_index) in selected.iter().enumerate() {
            while sequential_index <= expected_index {
                assert!(
                    sequential.read_frame_into(&mut sequential_frame),
                    "secuencial terminó en {sequential_index}"
                );
                sequential_index += 1;
            }
            assert!(
                stream.read_frame_into(&mut selected_frame),
                "salida #{position} (índice {expected_index})"
            );
            assert_eq!(
                selected_frame, sequential_frame,
                "select reasignó o alteró el frame absoluto {expected_index}"
            );
            if position == 0 {
                first.clone_from(&selected_frame);
            }
            if position + 1 == selected.len() {
                last.clone_from(&selected_frame);
            }
        }
        assert!(first.iter().any(|&value| value != 0));
        assert_ne!(first, last, "testsrc2 debe cambiar entre frames 0 y 2054");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    #[ignore = "requiere ffmpeg/ffprobe del sistema"]
    fn fragmented_mp4_needs_and_gets_packet_count() {
        let Some(path) = synthetic_mp4("fragmented", 40, true) else {
            eprintln!("ffmpeg no disponible; test omitido");
            return;
        };
        // Premisa del fallback: el contenedor fragmentado NO declara nb_frames.
        let probe = std::process::Command::new("ffprobe")
            .args([
                "-v", "error", "-select_streams", "v:0", "-show_entries",
                "stream=nb_frames", "-of", "csv=p=0",
            ])
            .arg(&path)
            .output()
            .unwrap();
        let nb_frames = String::from_utf8_lossy(&probe.stdout).trim().to_string();
        assert!(
            nb_frames.is_empty() || nb_frames == "N/A",
            "la fixture debía carecer de nb_frames y tiene '{nb_frames}'"
        );
        // El MISMO comando que usa FfmpegReader::new como fallback.
        let counted = std::process::Command::new("ffprobe")
            .args([
                "-v", "error", "-select_streams", "v:0", "-count_packets",
                "-show_entries", "stream=nb_read_packets", "-of", "csv=p=0",
            ])
            .arg(&path)
            .output()
            .unwrap();
        let n: usize = String::from_utf8_lossy(&counted.stdout).trim().parse().unwrap();
        assert_eq!(n, 40, "el conteo por paquetes debe dar los frames exactos");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    #[ignore = "requiere ffmpeg/ffprobe del sistema"]
    fn ffmpeg_get_frame_is_exact_even_with_wrong_fps_metadata() {
        let Some(path) = synthetic_mp4("ladder", 40, false) else {
            eprintln!("ffmpeg no disponible; test omitido");
            return;
        };
        // fps deliberadamente FALSO (10 en vez de 30): el seek por timestamp
        // estimado del frame 35 (3.5 s) cae más allá del final real (~1.33 s).
        // Antes: el seek temporal podía devolver negro o un frame cercano.
        // Ahora el índice se recorre desde cero y debe coincidir byte por byte
        // con el contrato exacto de UnifiedFrameSource.
        let reader = ffmpeg_reader_for(&path, 40, 10.0);
        let exact_source = UnifiedFrameSource::from_input(
            crate::VideoInput::Ffmpeg(reader.clone()),
            0,
        );
        let expected = exact_source.read_batch(&[35], None).unwrap().frames.remove(0);
        let frame = reader.get_frame(35, 0);
        assert_eq!(frame.len(), 64 * 48 * 2);
        assert_eq!(frame, expected, "get_frame debe preservar el índice absoluto");
        let _ = std::fs::remove_file(path);
    }

    fn synthetic_ser(width: usize, height: usize, frames: usize) -> std::path::PathBuf {
        let mut data = Vec::new();
        data.extend_from_slice(b"LUCAM-RECORDER");
        let put_i32 = |buf: &mut Vec<u8>, value: i32| {
            buf.extend_from_slice(&value.to_le_bytes());
        };
        put_i32(&mut data, 0); // LuID
        put_i32(&mut data, 0); // mono
        put_i32(&mut data, 0); // little endian
        put_i32(&mut data, width as i32);
        put_i32(&mut data, height as i32);
        put_i32(&mut data, 16);
        put_i32(&mut data, frames as i32);
        data.resize(178, 0);
        for frame in 0..frames {
            for pixel in 0..width * height {
                data.extend_from_slice(&((frame * 1000 + pixel) as u16).to_le_bytes());
            }
        }
        let path = std::env::temp_dir().join(format!(
            "zas_frame_source_{}_{}.ser",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&path, data).unwrap();
        path
    }

    #[test]
    fn exact_frame_collection_preserves_absolute_indices_and_request_order() {
        let mut next = 0u8;
        let frames = collect_exact_frames(&[4, 1, 4, 0], 3, |buffer| {
            buffer.fill(next);
            next = next.saturating_add(1);
            true
        })
        .unwrap();
        assert_eq!(frames, vec![vec![4; 3], vec![1; 3], vec![4; 3], vec![0; 3]]);
        assert_eq!(next, 5, "sólo debe decodificar hasta el mayor índice");
    }

    #[test]
    fn exact_frame_collection_rejects_a_truncated_stream() {
        let mut available = 2usize;
        let error = collect_exact_frames(&[0, 3], 4, |buffer| {
            if available == 0 {
                return false;
            }
            available -= 1;
            buffer.fill(7);
            true
        })
        .unwrap_err();
        assert!(error.contains("antes del frame solicitado 3"));
    }

    #[test]
    fn ser_batch_preserves_indices_roi_and_sixteen_bit_samples() {
        let path = synthetic_ser(64, 48, 6);
        let reader = crate::ser::SerReader::new(&path).unwrap();
        let source = UnifiedFrameSource::from_input(crate::VideoInput::Ser(reader), 0);
        let batch = source
            .read_batch(
                &[5, 0, 3],
                Some(FrameRoi {
                    x: 7,
                    y: 5,
                    width: 21,
                    height: 17,
                }),
            )
            .unwrap();
        assert_eq!(batch.indices, vec![5, 0, 3]);
        assert_eq!(batch.frames.len(), 3);
        assert!(batch.frames.iter().all(|frame| frame.len() == 21 * 17 * 2));
        let first = u16::from_le_bytes([batch.frames[0][0], batch.frames[0][1]]);
        assert_eq!(first, (5000 + 5 * 64 + 7) as u16);
        assert_eq!(batch.descriptor.sample_bits, 16);
        assert_eq!(batch.descriptor.source_kind, "SER");
        let _ = std::fs::remove_file(path);
    }
}
