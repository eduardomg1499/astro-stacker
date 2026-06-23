use byteorder::{LittleEndian, WriteBytesExt};

use std::fs::File;
use std::io::{BufWriter, Write};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::sync_channel;
use std::thread;
use tauri::Emitter;

use serde::Serialize;

#[derive(Serialize)]
pub struct ConversionResult {
    pub path: String,
    pub frame_count: usize,
    pub size_bytes: u64,
    pub duration_sec: f64,
    pub width: usize,
    pub height: usize,
    pub is_color: bool,
    pub bit_depth: i32,
    pub color_id: i32,
    pub bytes_per_frame: u64,
    pub estimated_size_bytes: u64,
    pub ffmpeg_pix_fmt: String,
    pub profile: String,
}

fn emit_conversion_progress(
    app_handle: &tauri::AppHandle,
    progress: f64,
    frame_current: usize,
    frame_total: usize,
    message: String,
    estimated_size_bytes: u64,
    written_bytes: u64,
) {
    let payload = serde_json::json!({
        "progress": progress,
        "percent": progress,
        "frameCurrent": frame_current,
        "frameTotal": frame_total,
        "message": message,
        "estimatedSizeBytes": estimated_size_bytes,
        "writtenBytes": written_bytes,
    });

    let _ = app_handle.emit("conversion_progress", payload.clone());
    let _ = app_handle.emit("conversion-progress", payload);
}

pub fn convert_video_to_ser(
    input_path: &str,
    output_path: &str,
    width: usize,
    height: usize,
    frame_count: usize,
    is_color: bool,
    is_8bit: bool,
    ffmpeg_path: &str,
    app_handle: &tauri::AppHandle,
) -> Result<ConversionResult, String> {
    const SER_HEADER_LEN: usize = 178;
    const SER_FIXED_HEADER_BYTES: usize = 42;

    let start_time = std::time::Instant::now();

    if frame_count == 0 {
        return Err("No se detectaron frames validos para convertir.".to_string());
    }

    let ser_depth = if is_8bit { 8 } else { 16 };
    // SER ColorID: 0=Mono, 100=RGB. The reader still accepts legacy RGB ID 14.
    let ser_color_id = if is_color { 100 } else { 0 };
    let profile = if is_color {
        if is_8bit {
            "Color RGB 8-bit"
        } else {
            "Color RGB 16-bit"
        }
    } else if is_8bit {
        "Mono 8-bit"
    } else {
        "Mono 16-bit"
    }
    .to_string();

    // FFmpeg Input Specs — 16-bit output preserves dynamic range from 10/12-bit codecs, 8-bit saves space
    let ffmpeg_pix_fmt = if is_8bit {
        if is_color {
            "rgb24"
        } else {
            "gray"
        }
    } else {
        if is_color {
            "rgb48le"
        } else {
            "gray16le"
        }
    };

    let input_bpp = if is_8bit {
        if is_color {
            3
        } else {
            1
        }
    } else {
        if is_color {
            6
        } else {
            2
        }
    }; // bytes per pixel

    // Determine Output Dimensions (Full)
    let (out_width, out_height) = (width, height);
    let input_frame_size = out_width * out_height * input_bpp;
    let bytes_per_frame = input_frame_size as u64;
    let estimated_size_bytes =
        (SER_HEADER_LEN as u64).saturating_add(bytes_per_frame.saturating_mul(frame_count as u64));

    emit_conversion_progress(
        app_handle,
        0.0,
        0,
        frame_count,
        format!(
            "Preparando SER {}x{} | {} | estimado {:.1} MB",
            out_width,
            out_height,
            profile,
            estimated_size_bytes as f64 / (1024.0 * 1024.0)
        ),
        estimated_size_bytes,
        SER_HEADER_LEN as u64,
    );

    // 1. Setup Output File (Main Thread writes Header, Writer Thread writes Body)
    // Create File and Writer
    let file = File::create(output_path).map_err(|e| e.to_string())?;
    // Increase buffer to 32MB for smoother large sequential writes (SSD optimization)
    let mut writer = BufWriter::with_capacity(32 * 1024 * 1024, file);

    // Initial Header Write
    writer
        .write_all(b"LUCAM-RECORDER")
        .map_err(|e| e.to_string())?;
    writer
        .write_i32::<LittleEndian>(0)
        .map_err(|e| e.to_string())?;
    writer
        .write_i32::<LittleEndian>(ser_color_id)
        .map_err(|e| e.to_string())?;
    writer
        .write_i32::<LittleEndian>(0)
        .map_err(|e| e.to_string())?;
    writer
        .write_i32::<LittleEndian>(out_width as i32)
        .map_err(|e| e.to_string())?;
    writer
        .write_i32::<LittleEndian>(out_height as i32)
        .map_err(|e| e.to_string())?;
    writer
        .write_i32::<LittleEndian>(ser_depth)
        .map_err(|e| e.to_string())?;
    writer
        .write_i32::<LittleEndian>(frame_count as i32)
        .map_err(|e| e.to_string())?;
    writer
        .write_all(&[0u8; SER_HEADER_LEN - SER_FIXED_HEADER_BYTES])
        .map_err(|e| e.to_string())?;

    // Channel for Data Frames (Double Buffering)
    // Increase Queue Depth to keep CPUs busy if Disk/FFmpeg fluctuates
    let queue_depth = 32;
    let (tx, rx) = sync_channel::<Vec<u8>>(queue_depth);
    let (recycle_tx, recycle_rx) = sync_channel::<Vec<u8>>(queue_depth);

    // Input buffer size (from FFmpeg)
    // Pre-allocate reusable buffers to avoid cloning every large frame.
    for _ in 0..queue_depth {
        let _ = recycle_tx.send(vec![0u8; input_frame_size]);
    }

    // Spawn Writer Thread
    let recycle_tx_writer = recycle_tx.clone();
    let writer_handle = thread::spawn(move || -> Result<usize, String> {
        let mut frames_written = 0;
        for data in rx {
            writer.write_all(&data).map_err(|e| e.to_string())?;
            frames_written += 1;
            let _ = recycle_tx_writer.try_send(data);
        }
        writer.flush().map_err(|e| e.to_string())?;
        Ok(frames_written)
    });

    // 2. Spawn FFmpeg
    let mut args = Vec::new();
    args.extend_from_slice(&["-hide_banner", "-nostdin", "-y"]);
    args.extend_from_slice(&["-threads", "0"]); // Use all cores for decoding
    args.extend_from_slice(&["-benchmark"]);
    args.extend_from_slice(&["-analyzeduration", "2147483647", "-probesize", "2147483647"]);
    // Reduce latency to fill buffers faster
    // args.extend_from_slice(&["-tune", "zerolatency"]); // Optional, might help start faster

    args.extend_from_slice(&["-i", input_path]);

    args.extend_from_slice(&[
        "-fps_mode",
        "passthrough",
        "-f",
        "rawvideo",
        "-pix_fmt",
        ffmpeg_pix_fmt,
        "-bufsize",
        "100M", // Large buffer for FFmpeg output
        "pipe:1",
    ]);

    let mut cmd = Command::new(ffmpeg_path);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    let mut child = cmd
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn FFmpeg: {}", e))?;

    let mut stdout = child.stdout.take().ok_or("Failed to open stdout")?;
    let stderr = child.stderr.take().ok_or("Failed to open stderr")?;

    // Drain Stderr
    thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stderr);
        for _ in reader.lines() {}
    });

    // 3. Read loop (Main Thread)
    let mut processed_frames = 0;

    loop {
        // Get an INPUT buffer
        let mut input_buffer = recycle_rx
            .recv()
            .unwrap_or_else(|_| vec![0u8; input_frame_size]);
        if input_buffer.len() != input_frame_size {
            input_buffer.resize(input_frame_size, 0);
        }

        match std::io::Read::read_exact(&mut stdout, &mut input_buffer) {
            Ok(_) => {
                if tx.send(input_buffer).is_err() {
                    break;
                }

                processed_frames += 1;

                if processed_frames % 5 == 0 || processed_frames == frame_count {
                    let pct = (processed_frames as f64 / frame_count as f64) * 100.0;
                    emit_conversion_progress(
                        app_handle,
                        pct,
                        processed_frames,
                        frame_count,
                        format!("Convirtiendo frame {} / {}", processed_frames, frame_count),
                        estimated_size_bytes,
                        (SER_HEADER_LEN as u64).saturating_add(
                            bytes_per_frame.saturating_mul(processed_frames as u64),
                        ),
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                let _ = recycle_tx.send(input_buffer);
                break;
            }
            Err(e) => {
                // Recycle
                let _ = recycle_tx.send(input_buffer);
                return Err(format!("Error reading from FFmpeg: {}", e));
            }
        }
    }

    drop(tx);
    let written_frames = writer_handle
        .join()
        .map_err(|_| "Writer thread panicked".to_string())??;

    // 4. Update Header if needed
    if written_frames != frame_count {
        let mut f = File::options()
            .write(true)
            .open(output_path)
            .map_err(|e| e.to_string())?;
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(38))
            .map_err(|e| e.to_string())?;
        f.write_i32::<LittleEndian>(written_frames as i32)
            .map_err(|e| e.to_string())?;
    }

    let ffmpeg_status = child
        .wait()
        .map_err(|e| format!("Error esperando FFmpeg: {}", e))?;
    if !ffmpeg_status.success() {
        return Err(format!(
            "FFmpeg termino con error durante la conversion SER. Frames escritos: {} / {}",
            written_frames, frame_count
        ));
    }

    let duration_sec = start_time.elapsed().as_secs_f64();
    let size_bytes = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    emit_conversion_progress(
        app_handle,
        100.0,
        written_frames,
        written_frames,
        "Conversión SER completada".to_string(),
        estimated_size_bytes,
        size_bytes,
    );

    Ok(ConversionResult {
        path: output_path.to_string(),
        frame_count: written_frames,
        size_bytes,
        duration_sec,
        width: out_width,
        height: out_height,
        is_color,
        bit_depth: ser_depth,
        color_id: ser_color_id,
        bytes_per_frame,
        estimated_size_bytes,
        ffmpeg_pix_fmt: ffmpeg_pix_fmt.to_string(),
        profile,
    })
}
