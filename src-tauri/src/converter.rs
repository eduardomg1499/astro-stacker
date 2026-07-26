use byteorder::{LittleEndian, WriteBytesExt};
use crossbeam_channel::{bounded, RecvTimeoutError, SendTimeoutError};
use serde::Serialize;

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{Emitter, Manager};

const SER_HEADER_LEN: usize = 178;
const SER_FIXED_HEADER_BYTES: usize = 42;
const SER_FRAME_COUNT_OFFSET: u64 = 38;
const FRAME_QUEUE_MEMORY_BUDGET: usize = 128 * 1024 * 1024;
const MAX_FRAME_QUEUE_DEPTH: usize = 16;
const MAX_FFMPEG_DIAGNOSTIC_BYTES: usize = 1024 * 1024;
const CHANNEL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const FFMPEG_EXIT_TIMEOUT: Duration = Duration::from_secs(30);
const FFMPEG_REAP_TIMEOUT: Duration = Duration::from_secs(5);
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerConversionSpec {
    pub color_id: i32,
    pub pixel_depth: usize,
    pub ffmpeg_pix_fmt: String,
    pub pixel_little_endian: bool,
    pub profile: String,
}

impl SerConversionSpec {
    pub fn new(
        color_id: i32,
        pixel_depth: usize,
        ffmpeg_pix_fmt: impl Into<String>,
        profile: impl Into<String>,
    ) -> Result<Self, String> {
        if !matches!(color_id, 0 | 8..=11 | 100) {
            return Err(format!(
                "ColorID SER no soportado por el conversor: {color_id}"
            ));
        }
        if !(1..=16).contains(&pixel_depth) {
            return Err(format!(
                "SER sólo admite muestras enteras de 1 a 16 bits; se solicitó {pixel_depth}"
            ));
        }

        let ffmpeg_pix_fmt = ffmpeg_pix_fmt.into().trim().to_ascii_lowercase();
        let storage_bpp = ffmpeg_storage_bytes_per_pixel(&ffmpeg_pix_fmt).ok_or_else(|| {
            format!("Formato FFmpeg no permitido para publicación SER: {ffmpeg_pix_fmt}")
        })?;
        let ser_bpp = crate::ser::ser_bytes_per_pixel(color_id, pixel_depth);
        if storage_bpp != ser_bpp {
            return Err(format!(
                "Contrato SER/FFmpeg inconsistente: ColorID {color_id}, {pixel_depth} bits requieren {ser_bpp} B/pixel, pero {ffmpeg_pix_fmt} entrega {storage_bpp}"
            ));
        }

        let is_rgb =
            ffmpeg_pix_fmt == "rgb24" || ffmpeg_pix_fmt == "rgb48le" || ffmpeg_pix_fmt == "rgb48be";
        if (color_id == 100) != is_rgb {
            return Err(format!(
                "ColorID {color_id} no corresponde al formato {ffmpeg_pix_fmt}"
            ));
        }
        if let Some(format_color_id) = bayer_color_id_from_pix_fmt(&ffmpeg_pix_fmt) {
            if format_color_id != color_id {
                return Err(format!(
                    "Patrón CFA inconsistente: ColorID {color_id}, pix_fmt {ffmpeg_pix_fmt}"
                ));
            }
        }
        if (8..=11).contains(&color_id)
            && !(ffmpeg_pix_fmt.starts_with("bayer_") || ffmpeg_pix_fmt.starts_with("gray"))
        {
            return Err(format!(
                "Un CFA Bayer debe permanecer en un solo plano; {ffmpeg_pix_fmt} no es válido"
            ));
        }

        let pixel_little_endian = pixel_depth <= 8 || !ffmpeg_pix_fmt.ends_with("be");
        Ok(Self {
            color_id,
            pixel_depth,
            ffmpeg_pix_fmt,
            pixel_little_endian,
            profile: profile.into(),
        })
    }

    #[inline]
    pub(crate) fn is_color(&self) -> bool {
        crate::ser::ser_color_is_color(self.color_id)
    }
}

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

fn bayer_color_id_from_pix_fmt(pix_fmt: &str) -> Option<i32> {
    let format = pix_fmt.to_ascii_lowercase();
    if format.starts_with("bayer_rggb") {
        Some(8)
    } else if format.starts_with("bayer_grbg") {
        Some(9)
    } else if format.starts_with("bayer_gbrg") {
        Some(10)
    } else if format.starts_with("bayer_bggr") {
        Some(11)
    } else {
        None
    }
}

fn ffmpeg_storage_bytes_per_pixel(pix_fmt: &str) -> Option<usize> {
    match pix_fmt {
        "gray" | "gray8" | "bayer_rggb8" | "bayer_grbg8" | "bayer_gbrg8" | "bayer_bggr8" => Some(1),
        "gray9le" | "gray9be" | "gray10le" | "gray10be" | "gray12le" | "gray12be" | "gray14le"
        | "gray14be" | "gray16le" | "gray16be" | "bayer_rggb16le" | "bayer_rggb16be"
        | "bayer_grbg16le" | "bayer_grbg16be" | "bayer_gbrg16le" | "bayer_gbrg16be"
        | "bayer_bggr16le" | "bayer_bggr16be" => Some(2),
        "rgb24" => Some(3),
        "rgb48le" | "rgb48be" => Some(6),
        _ => None,
    }
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

fn checked_frame_size(
    width: usize,
    height: usize,
    spec: &SerConversionSpec,
) -> Result<usize, String> {
    width
        .checked_mul(height)
        .and_then(|pixels| {
            pixels.checked_mul(crate::ser::ser_bytes_per_pixel(
                spec.color_id,
                spec.pixel_depth,
            ))
        })
        .filter(|&bytes| bytes > 0)
        .ok_or_else(|| "Dimensiones o tamaño de frame SER fuera de rango".to_string())
}

fn adaptive_queue_depth(frame_size: usize, logical_cpus: usize) -> usize {
    let memory_limited = (FRAME_QUEUE_MEMORY_BUDGET / frame_size.max(1)).max(1);
    let cpu_target = logical_cpus
        .max(1)
        .saturating_mul(2)
        .clamp(2, MAX_FRAME_QUEUE_DEPTH);
    memory_limited
        .min(cpu_target)
        .clamp(1, MAX_FRAME_QUEUE_DEPTH)
}

fn allocate_frame_buffer(frame_size: usize) -> Result<Vec<u8>, String> {
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(frame_size).map_err(|error| {
        format!(
            "Memoria insuficiente para un frame SER de {:.1} MB: {error}",
            frame_size as f64 / (1024.0 * 1024.0)
        )
    })?;
    buffer.resize(frame_size, 0);
    Ok(buffer)
}

fn write_ser_header<W: Write>(
    writer: &mut W,
    width: usize,
    height: usize,
    frame_count: usize,
    spec: &SerConversionSpec,
) -> Result<(), String> {
    let width = i32::try_from(width).map_err(|_| "Ancho SER fuera de rango".to_string())?;
    let height = i32::try_from(height).map_err(|_| "Alto SER fuera de rango".to_string())?;
    let frame_count = i32::try_from(frame_count)
        .map_err(|_| "Conteo de frames SER fuera de rango".to_string())?;

    writer
        .write_all(b"LUCAM-RECORDER")
        .map_err(|error| error.to_string())?;
    writer
        .write_i32::<LittleEndian>(0)
        .map_err(|error| error.to_string())?;
    writer
        .write_i32::<LittleEndian>(spec.color_id)
        .map_err(|error| error.to_string())?;
    // Convención SER interoperable: 0=payload LE, 1=payload BE.
    writer
        .write_i32::<LittleEndian>(i32::from(!spec.pixel_little_endian))
        .map_err(|error| error.to_string())?;
    writer
        .write_i32::<LittleEndian>(width)
        .map_err(|error| error.to_string())?;
    writer
        .write_i32::<LittleEndian>(height)
        .map_err(|error| error.to_string())?;
    writer
        .write_i32::<LittleEndian>(spec.pixel_depth as i32)
        .map_err(|error| error.to_string())?;
    writer
        .write_i32::<LittleEndian>(frame_count)
        .map_err(|error| error.to_string())?;
    writer
        .write_all(&[0u8; SER_HEADER_LEN - SER_FIXED_HEADER_BYTES])
        .map_err(|error| error.to_string())
}

fn patch_ser_frame_count(path: &Path, frame_count: usize) -> Result<(), String> {
    let frame_count =
        i32::try_from(frame_count).map_err(|_| "Conteo final SER fuera de rango".to_string())?;
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| format!("No se pudo reabrir el SER temporal: {error}"))?;
    file.seek(SeekFrom::Start(SER_FRAME_COUNT_OFFSET))
        .map_err(|error| error.to_string())?;
    file.write_i32::<LittleEndian>(frame_count)
        .map_err(|error| error.to_string())?;
    file.flush().map_err(|error| error.to_string())?;
    file.sync_all()
        .map_err(|error| format!("No se pudo sincronizar el encabezado SER: {error}"))
}

struct StagedSerOutput {
    staged_path: PathBuf,
    final_path: PathBuf,
    committed: bool,
}

impl StagedSerOutput {
    fn create(final_path: &Path) -> Result<(Self, File), String> {
        let parent = final_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            return Err(format!(
                "La carpeta de destino no existe: {}",
                parent.display()
            ));
        }
        if final_path.is_dir() {
            return Err("La ruta SER de destino es una carpeta".to_string());
        }

        let base = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("converted.ser");
        let epoch_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();

        for _ in 0..128 {
            let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let staged_path = parent.join(format!(
                ".{base}.astro-ser-{}-{epoch_nanos}-{sequence}.tmp",
                std::process::id()
            ));
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&staged_path)
            {
                Ok(file) => {
                    return Ok((
                        Self {
                            staged_path,
                            final_path: final_path.to_path_buf(),
                            committed: false,
                        },
                        file,
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "No se pudo crear el SER temporal junto al destino: {error}"
                    ))
                }
            }
        }
        Err("No se pudo reservar un nombre temporal único para el SER".to_string())
    }

    fn commit(&mut self) -> Result<(), String> {
        replace_staged_file(&self.staged_path, &self.final_path)?;
        self.committed = true;
        // El archivo ya fue fsync antes del rename. Algunos NAS/FUSE no
        // permiten fsync del directorio: después de publicar no podemos
        // informar falsamente que "no existe" ni deshacer un replace. En FS
        // locales esta llamada añade durabilidad de la entrada de directorio.
        if let Err(error) = sync_parent_directory(&self.final_path) {
            eprintln!("WARNING: {error}");
        }
        Ok(())
    }
}

impl Drop for StagedSerOutput {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.staged_path);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn replace_staged_file(staged_path: &Path, final_path: &Path) -> Result<(), String> {
    std::fs::rename(staged_path, final_path)
        .map_err(|error| format!("No se pudo publicar el SER atómicamente: {error}"))
}

#[cfg(target_os = "windows")]
fn replace_staged_file(staged_path: &Path, final_path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    let staged: Vec<u16> = staged_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let final_name: Vec<u16> = final_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            staged.as_ptr(),
            final_name.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(format!(
            "No se pudo publicar el SER atómicamente: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
fn sync_parent_directory(final_path: &Path) -> Result<(), String> {
    let parent = final_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("No se pudo sincronizar la carpeta del SER: {error}"))
}

#[cfg(target_os = "windows")]
fn sync_parent_directory(_final_path: &Path) -> Result<(), String> {
    // MoveFileExW(WRITE_THROUGH) ya fuerza la publicación durable.
    Ok(())
}

fn read_one_frame<R, F>(
    reader: &mut R,
    buffer: &mut [u8],
    mut is_cancelled: F,
) -> Result<bool, String>
where
    R: Read,
    F: FnMut() -> bool,
{
    let mut offset = 0usize;
    while offset < buffer.len() {
        if is_cancelled() {
            return Err("Conversión SER cancelada por el usuario".to_string());
        }
        match reader.read(&mut buffer[offset..]) {
            Ok(0) if offset == 0 => return Ok(false),
            Ok(0) => {
                return Err(format!(
                "FFmpeg terminó con un frame parcial ({offset}/{} bytes); el SER no se publicará",
                buffer.len()
            ))
            }
            Ok(read) => offset += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("Error leyendo frames de FFmpeg: {error}")),
        }
    }
    Ok(true)
}

fn ffmpeg_error_excerpt(log: &Arc<Mutex<Vec<u8>>>) -> String {
    let bytes = log.lock().unwrap_or_else(|error| error.into_inner());
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ")
}

fn terminate_unshared_ffmpeg(child: &mut std::process::Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let _ = child.kill();
    let started = std::time::Instant::now();
    while started.elapsed() < FFMPEG_REAP_TIMEOUT {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(CHANNEL_POLL_INTERVAL),
        }
    }
}

/// Wait without ever holding the child mutex across a blocking `wait()`.  The
/// cancellation watchdog can therefore acquire the process and kill it at any
/// point.  EOF from rawvideo should be followed by a quick FFmpeg exit; a hard
/// deadline handles broken codecs/devices without freezing the converter UI.
fn wait_for_ffmpeg_exit(
    child: &Arc<Mutex<std::process::Child>>,
    token: &crate::PlanetaryJobToken,
) -> Result<std::process::ExitStatus, String> {
    let started = std::time::Instant::now();
    let mut killed_at: Option<std::time::Instant> = None;
    let mut timeout_error = None;
    loop {
        if token.is_cancelled() && killed_at.is_none() {
            let mut process = child.lock().unwrap_or_else(|error| error.into_inner());
            let _ = process.kill();
            killed_at = Some(std::time::Instant::now());
        }
        if started.elapsed() >= FFMPEG_EXIT_TIMEOUT && killed_at.is_none() {
            let mut process = child.lock().unwrap_or_else(|error| error.into_inner());
            let _ = process.kill();
            killed_at = Some(std::time::Instant::now());
            timeout_error = Some(format!(
                "FFmpeg no finalizó en {} s después de cerrar su salida; se terminó el proceso",
                FFMPEG_EXIT_TIMEOUT.as_secs()
            ));
        }

        let status = {
            let mut process = child.lock().unwrap_or_else(|error| error.into_inner());
            process
                .try_wait()
                .map_err(|error| format!("No se pudo consultar FFmpeg: {error}"))?
        };
        if let Some(status) = status {
            if let Some(error) = timeout_error {
                return Err(error);
            }
            return Ok(status);
        }
        if killed_at
            .map(|instant| instant.elapsed() >= FFMPEG_REAP_TIMEOUT)
            .unwrap_or(false)
        {
            return Err(format!(
                "FFmpeg no pudo recolectarse {} s después de terminarlo",
                FFMPEG_REAP_TIMEOUT.as_secs()
            ));
        }
        thread::sleep(CHANNEL_POLL_INTERVAL);
    }
}

pub fn convert_video_to_ser(
    input_path: &str,
    output_path: &str,
    width: usize,
    height: usize,
    declared_frame_count: usize,
    spec: SerConversionSpec,
    ffmpeg_path: &str,
    app_handle: &tauri::AppHandle,
    token: crate::PlanetaryJobToken,
) -> Result<ConversionResult, String> {
    let start_time = std::time::Instant::now();

    if declared_frame_count == 0 {
        return Err("No se detectaron frames válidos para convertir".to_string());
    }
    if token.is_cancelled() {
        return Err("Conversión SER cancelada por el usuario".to_string());
    }

    let input_frame_size = checked_frame_size(width, height, &spec)?;
    let bytes_per_frame = input_frame_size as u64;
    let estimated_size_bytes = (SER_HEADER_LEN as u64)
        .saturating_add(bytes_per_frame.saturating_mul(declared_frame_count as u64));
    let queue_depth = adaptive_queue_depth(input_frame_size, num_cpus::get());
    let writer_buffer_capacity = input_frame_size.clamp(1024 * 1024, 8 * 1024 * 1024);

    emit_conversion_progress(
        app_handle,
        0.0,
        0,
        declared_frame_count,
        format!(
            "Preparando SER {}x{} | {} | cola {} ({:.1} MB máx.) | estimado {:.1} MB",
            width,
            height,
            spec.profile,
            queue_depth,
            (queue_depth.saturating_mul(input_frame_size)) as f64 / (1024.0 * 1024.0),
            estimated_size_bytes as f64 / (1024.0 * 1024.0)
        ),
        estimated_size_bytes,
        SER_HEADER_LEN as u64,
    );

    let (mut staged_output, file) = StagedSerOutput::create(Path::new(output_path))?;
    let mut writer = BufWriter::with_capacity(writer_buffer_capacity, file);
    write_ser_header(&mut writer, width, height, declared_frame_count, &spec)?;

    let (tx, rx) = bounded::<Vec<u8>>(queue_depth);
    let (recycle_tx, recycle_rx) = bounded::<Vec<u8>>(queue_depth);
    for _ in 0..queue_depth {
        recycle_tx
            .try_send(allocate_frame_buffer(input_frame_size)?)
            .map_err(|_| "No se pudo inicializar la cola acotada de conversión".to_string())?;
    }

    let ffmpeg_threads = if num_cpus::get() <= 4 {
        num_cpus::get().saturating_sub(1).max(1)
    } else {
        num_cpus::get().saturating_sub(2).max(4)
    };
    let ffmpeg_threads_arg = ffmpeg_threads.to_string();
    let mut cmd = Command::new(ffmpeg_path);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);
    cmd.args([
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-analyzeduration",
        "2147483647",
        "-probesize",
        "2147483647",
        "-threads",
        &ffmpeg_threads_arg,
        "-noautorotate",
        "-i",
        input_path,
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-fps_mode",
        "passthrough",
        "-c:v",
        "rawvideo",
        "-pix_fmt",
        &spec.ffmpeg_pix_fmt,
        "-f",
        "rawvideo",
        "pipe:1",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|error| format!("No se pudo iniciar FFmpeg: {error}"))?;
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_unshared_ffmpeg(&mut child);
            return Err("FFmpeg no expuso su salida de frames".to_string());
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_unshared_ffmpeg(&mut child);
            return Err("FFmpeg no expuso su canal de diagnóstico".to_string());
        }
    };

    let recycle_tx_writer = recycle_tx.clone();
    let writer_handle = thread::spawn(move || -> Result<usize, String> {
        let mut frames_written = 0usize;
        for data in rx {
            writer
                .write_all(&data)
                .map_err(|error| format!("Error escribiendo el SER temporal: {error}"))?;
            frames_written = frames_written.saturating_add(1);
            let _ = recycle_tx_writer.try_send(data);
        }
        writer
            .flush()
            .map_err(|error| format!("No se pudo vaciar el SER temporal: {error}"))?;
        writer
            .get_ref()
            .sync_all()
            .map_err(|error| format!("No se pudo sincronizar el SER temporal: {error}"))?;
        Ok(frames_written)
    });
    drop(recycle_tx);

    let stderr_log = Arc::new(Mutex::new(Vec::with_capacity(64 * 1024)));
    let stderr_log_writer = stderr_log.clone();
    let stderr_handle = thread::spawn(move || {
        let mut stderr = std::io::BufReader::new(stderr);
        let mut chunk = [0u8; 8192];
        loop {
            let Ok(read) = stderr.read(&mut chunk) else {
                break;
            };
            if read == 0 {
                break;
            }
            let mut log = stderr_log_writer
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let available = MAX_FFMPEG_DIAGNOSTIC_BYTES.saturating_sub(log.len());
            log.extend_from_slice(&chunk[..read.min(available)]);
        }
    });

    let child = Arc::new(Mutex::new(child));
    let watchdog_done = Arc::new(AtomicBool::new(false));
    let watchdog_child = child.clone();
    let watchdog_done_worker = watchdog_done.clone();
    let watchdog_token = token.clone();
    let watchdog_handle = thread::spawn(move || {
        while !watchdog_done_worker.load(Ordering::Acquire) {
            if watchdog_token.is_cancelled() {
                if let Ok(mut process) = watchdog_child.lock() {
                    let _ = process.kill();
                }
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
    });

    let mut processed_frames = 0usize;
    let mut last_progress_emit = std::time::Instant::now();
    let stream_result = loop {
        if token.is_cancelled() {
            break Err("Conversión SER cancelada por el usuario".to_string());
        }

        let input_buffer = loop {
            match recycle_rx.recv_timeout(CHANNEL_POLL_INTERVAL) {
                Ok(buffer) => break Some(buffer),
                Err(RecvTimeoutError::Timeout) if token.is_cancelled() => break None,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break None,
            }
        };
        let Some(mut input_buffer) = input_buffer else {
            break if token.is_cancelled() {
                Err("Conversión SER cancelada por el usuario".to_string())
            } else {
                Err("El escritor SER terminó antes que el decodificador".to_string())
            };
        };

        match read_one_frame(&mut stdout, &mut input_buffer, || token.is_cancelled()) {
            Ok(true) => {}
            Ok(false) => break Ok(()),
            Err(error) => break Err(error),
        }

        let mut sent = false;
        loop {
            match tx.send_timeout(input_buffer, CHANNEL_POLL_INTERVAL) {
                Ok(()) => {
                    sent = true;
                    break;
                }
                Err(SendTimeoutError::Timeout(returned)) => {
                    input_buffer = returned;
                    if token.is_cancelled() {
                        break;
                    }
                }
                Err(SendTimeoutError::Disconnected(_)) => {
                    break;
                }
            }
        }
        if token.is_cancelled() {
            break Err("Conversión SER cancelada por el usuario".to_string());
        }
        if !sent {
            break Err("El escritor SER falló durante la conversión".to_string());
        }

        processed_frames = processed_frames.saturating_add(1);
        if processed_frames > i32::MAX as usize {
            break Err("El video excede el máximo de frames representable por SER".to_string());
        }
        if last_progress_emit.elapsed() >= Duration::from_millis(100)
            || processed_frames == declared_frame_count
        {
            let pct =
                ((processed_frames as f64 / declared_frame_count as f64) * 100.0).clamp(0.0, 99.9);
            emit_conversion_progress(
                app_handle,
                pct,
                processed_frames,
                declared_frame_count,
                format!(
                    "Convirtiendo frame {} / {}",
                    processed_frames, declared_frame_count
                ),
                estimated_size_bytes,
                (SER_HEADER_LEN as u64)
                    .saturating_add(bytes_per_frame.saturating_mul(processed_frames as u64)),
            );
            last_progress_emit = std::time::Instant::now();
        }
    };

    if stream_result.is_err() || token.is_cancelled() {
        if let Ok(mut process) = child.lock() {
            let _ = process.kill();
        }
    }
    drop(stdout);
    drop(tx);

    // Keep the cancellation watchdog alive throughout process termination.
    // `wait_for_ffmpeg_exit` uses short try_wait locks, so the watchdog never
    // gets trapped behind a mutex held by a blocking Child::wait.
    let process_status = wait_for_ffmpeg_exit(&child, &token);
    watchdog_done.store(true, Ordering::Release);
    let _ = watchdog_handle.join();
    let writer_result = writer_handle
        .join()
        .map_err(|_| "El hilo escritor SER terminó inesperadamente".to_string())?;
    if process_status.is_ok() {
        let _ = stderr_handle.join();
    } else {
        // Avoid turning an already diagnosed, unreapable child into a second
        // indefinite wait on its stderr pipe. The detached drainer owns no
        // application data and exits when the OS closes that pipe.
        drop(stderr_handle);
    }

    if token.is_cancelled() {
        return Err("Conversión SER cancelada por el usuario".to_string());
    }
    let written_frames = writer_result?;
    stream_result?;
    let status = process_status?;
    if !status.success() {
        let diagnostic = ffmpeg_error_excerpt(&stderr_log);
        return Err(if diagnostic.is_empty() {
            format!(
                "FFmpeg terminó con error; se descartó el SER temporal ({written_frames} frames)"
            )
        } else {
            format!("FFmpeg terminó con error; se descartó el SER temporal: {diagnostic}")
        });
    }
    if written_frames == 0 {
        return Err("FFmpeg no produjo ningún frame completo; no se creó un SER".to_string());
    }

    patch_ser_frame_count(&staged_output.staged_path, written_frames)?;
    let size_bytes = std::fs::metadata(&staged_output.staged_path)
        .map_err(|error| format!("No se pudo validar el SER temporal: {error}"))?
        .len();

    // La validación generacional y el rename comparten el mismo gate: ningún
    // trabajo anterior puede publicar después de cancelar o iniciar otro job.
    {
        let state = app_handle.state::<crate::AppState>();
        let _generation_guard = state
            .planetary_generation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if token.is_cancelled() {
            return Err("Conversión SER cancelada antes de publicar".to_string());
        }
        staged_output.commit()?;
    }

    let duration_sec = start_time.elapsed().as_secs_f64();
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
        width,
        height,
        is_color: spec.is_color(),
        bit_depth: spec.pixel_depth as i32,
        color_id: spec.color_id,
        bytes_per_frame,
        estimated_size_bytes,
        ffmpeg_pix_fmt: spec.ffmpeg_pix_fmt,
        profile: spec.profile,
    })
}

#[cfg(test)]
mod converter_tests {
    use super::*;
    use byteorder::{ByteOrder, LittleEndian};
    use std::io::Cursor;

    fn unique_test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "astro-stacker-{name}-{}-{}",
            std::process::id(),
            STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn ser_header_preserves_every_bayer_color_id_and_nine_bit_depth() {
        let formats = [
            (8, "bayer_rggb16le"),
            (9, "bayer_grbg16le"),
            (10, "bayer_gbrg16le"),
            (11, "bayer_bggr16le"),
        ];
        for (color_id, pix_fmt) in formats {
            let spec = SerConversionSpec::new(color_id, 9, pix_fmt, "CFA 9-bit").unwrap();
            let mut header = Vec::new();
            write_ser_header(&mut header, 640, 480, 17, &spec).unwrap();
            assert_eq!(header.len(), SER_HEADER_LEN);
            assert_eq!(LittleEndian::read_i32(&header[18..22]), color_id);
            assert_eq!(LittleEndian::read_i32(&header[22..26]), 0);
            assert_eq!(LittleEndian::read_i32(&header[34..38]), 9);
            assert_eq!(LittleEndian::read_i32(&header[38..42]), 17);
        }
    }

    #[test]
    fn conversion_spec_rejects_cfa_pattern_or_storage_mismatch() {
        assert!(SerConversionSpec::new(8, 16, "bayer_bggr16le", "bad").is_err());
        assert!(SerConversionSpec::new(8, 8, "bayer_rggb16le", "bad").is_err());
        assert!(SerConversionSpec::new(100, 16, "gray16le", "bad").is_err());
        assert!(SerConversionSpec::new(0, 16, "rgb48le", "bad").is_err());
    }

    #[test]
    fn adaptive_queue_is_bounded_by_bytes_and_depth() {
        for frame_size in [
            1,
            1024,
            8 * 1024 * 1024,
            96 * 1024 * 1024,
            256 * 1024 * 1024,
        ] {
            let depth = adaptive_queue_depth(frame_size, 32);
            assert!((1..=MAX_FRAME_QUEUE_DEPTH).contains(&depth));
            if frame_size <= FRAME_QUEUE_MEMORY_BUDGET {
                assert!(depth.saturating_mul(frame_size) <= FRAME_QUEUE_MEMORY_BUDGET);
            } else {
                assert_eq!(depth, 1);
            }
        }
    }

    #[test]
    fn partial_ffmpeg_frame_is_rejected_not_published_as_eof() {
        let mut reader = Cursor::new(vec![1u8, 2, 3]);
        let mut frame = [0u8; 4];
        let error = read_one_frame(&mut reader, &mut frame, || false).unwrap_err();
        assert!(error.contains("frame parcial"));

        let mut empty = Cursor::new(Vec::<u8>::new());
        assert!(!read_one_frame(&mut empty, &mut frame, || false).unwrap());

        let mut complete = Cursor::new(vec![1u8, 2, 3, 4]);
        let error = read_one_frame(&mut complete, &mut frame, || true).unwrap_err();
        assert!(error.contains("cancelada"));
    }

    #[test]
    fn staged_output_replaces_only_on_commit_and_cleans_on_drop() {
        let final_path = unique_test_path("atomic.ser");
        std::fs::write(&final_path, b"old").unwrap();
        let staged_path;
        {
            let (mut staged, mut file) = StagedSerOutput::create(&final_path).unwrap();
            staged_path = staged.staged_path.clone();
            file.write_all(b"new").unwrap();
            file.flush().unwrap();
            file.sync_all().unwrap();
            drop(file);
            assert_eq!(std::fs::read(&final_path).unwrap(), b"old");
            staged.commit().unwrap();
        }
        assert_eq!(std::fs::read(&final_path).unwrap(), b"new");
        assert!(!staged_path.exists());
        let _ = std::fs::remove_file(&final_path);

        let abandoned_final = unique_test_path("abandoned.ser");
        let abandoned_stage;
        {
            let (staged, _file) = StagedSerOutput::create(&abandoned_final).unwrap();
            abandoned_stage = staged.staged_path.clone();
            assert!(abandoned_stage.exists());
        }
        assert!(!abandoned_stage.exists());
        assert!(!abandoned_final.exists());
    }

    #[cfg(unix)]
    #[test]
    fn supervised_ffmpeg_wait_honors_cancellation_without_blocking() {
        let child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let child = Arc::new(Mutex::new(child));
        let token = crate::PlanetaryJobToken::for_test(
            31,
            Arc::new(std::sync::atomic::AtomicUsize::new(31)),
            Arc::new(AtomicBool::new(true)),
        );
        let started = std::time::Instant::now();
        let status = wait_for_ffmpeg_exit(&child, &token).unwrap();
        assert!(!status.success());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "la espera supervisada no reacciono a la cancelacion"
        );
    }
}
