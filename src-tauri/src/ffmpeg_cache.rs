// =============================================================
// ffmpeg_cache.rs — Pre-decoded RAM Frame Cache for Compressed Video
//
// Solves the critical performance bottleneck of FfmpegReader::get_frame()
// which spawns a new ffmpeg subprocess for EVERY frame access.
//
// Strategy:
//   1. Detect best GPU hardware decoder (NVDEC, QSV, VideoToolbox, VAAPI)
//   2. Spawn a SINGLE ffmpeg process that streams ALL frames via pipe
//   3. Store decoded frames in a Vec<Vec<u8>> indexed by frame number
//   4. Provide instant random access from RAM (like mmap for SER/AVI)
//
// This module does NOT affect SER or AVI native readers.
// =============================================================

use std::io::Read;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::Emitter;

/// Cached result of GPU hardware acceleration probe.
/// Detected once per session, reused across all cache operations.
#[derive(Clone, Debug)]
pub struct GpuAccelInfo {
    /// The `-hwaccel` value to pass to ffmpeg, or None for CPU-only
    pub hwaccel: Option<String>,
    /// Human-readable description for UI
    pub description: String,
}

/// Pre-decoded frame cache. Loads all frames from a compressed video file
/// into RAM using a single ffmpeg streaming process.
#[derive(Clone, Debug)]
pub struct FfmpegFrameCache {
    /// Raw frame data indexed by frame number
    pub frames: Vec<Vec<u8>>,
    pub width: usize,
    pub height: usize,
    pub bpp: usize, // bytes per pixel (2 for gray16le, 6 for rgb48le)
    pub color_id: i32,
    pub frame_count: usize,
    pub fps: f64,
    pub is_color: bool,
    pub codec_name: String,
}

impl FfmpegFrameCache {
    /// Get a specific frame by index — instant O(1) from RAM.
    pub fn get_frame(&self, index: usize) -> &[u8] {
        if index < self.frames.len() {
            &self.frames[index]
        } else {
            &[]
        }
    }

    /// Total number of actually loaded frames
    pub fn loaded_count(&self) -> usize {
        self.frames.len()
    }
}

/// Detect the best available hardware accelerator by probing ffmpeg.
///
/// Strategy:
///   1. Query `ffmpeg -hwaccels` to get available options
///   2. Try platform-specific accelerators in priority order
///   3. Fall back to CPU if nothing works
///
/// This function is cheap (~100ms) and should be called once per session.
pub fn detect_gpu_accel(ffmpeg_path: &str) -> GpuAccelInfo {
    // Query available hwaccels
    let available = query_available_hwaccels(ffmpeg_path);

    // Platform-specific priority order
    #[cfg(target_os = "windows")]
    let candidates = vec![
        ("cuda", "NVIDIA NVDEC (CUDA)"),
        ("d3d11va", "DirectX 11 Video Acceleration"),
        ("qsv", "Intel Quick Sync Video"),
        ("auto", "FFmpeg Auto Select"),
    ];

    #[cfg(target_os = "macos")]
    let candidates = vec![
        ("videotoolbox", "Apple VideoToolbox"),
        ("auto", "FFmpeg Auto Select"),
    ];

    #[cfg(target_os = "linux")]
    let candidates = vec![
        ("cuda", "NVIDIA NVDEC (CUDA)"),
        ("vaapi", "Video Acceleration API"),
        ("vdpau", "VDPAU (NVIDIA Legacy)"),
        ("auto", "FFmpeg Auto Select"),
    ];

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    let candidates = vec![("auto", "FFmpeg Auto Select")];

    for (accel, desc) in &candidates {
        if *accel == "auto" {
            // Auto is always available as a fallback before CPU-only
            return GpuAccelInfo {
                hwaccel: Some(accel.to_string()),
                description: desc.to_string(),
            };
        }
        if available.contains(&accel.to_string()) {
            return GpuAccelInfo {
                hwaccel: Some(accel.to_string()),
                description: desc.to_string(),
            };
        }
    }

    // Pure CPU fallback
    GpuAccelInfo {
        hwaccel: None,
        description: "CPU Only (No GPU Acceleration)".to_string(),
    }
}

/// Query ffmpeg for available hardware accelerators
fn query_available_hwaccels(ffmpeg_path: &str) -> Vec<String> {
    let mut cmd = Command::new(ffmpeg_path);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    let output = cmd
        .args(&["-hide_banner", "-hwaccels"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            text.lines()
                .skip(1) // Skip "Hardware acceleration methods:" header
                .map(|l| l.trim().to_lowercase())
                .filter(|l| !l.is_empty())
                .collect()
        }
        Err(_) => Vec::new(),
    }
}

/// Pre-load ALL frames from a compressed video into RAM.
///
/// Uses a single ffmpeg process with optional GPU acceleration.
/// Frames are stored as raw pixel data (gray16le or rgb48le) in a Vec.
///
/// # Arguments
/// * `path` - Path to the video file
/// * `width`, `height` - Decoded frame dimensions
/// * `bpp` - Bytes per pixel (2 for mono, 6 for color)
/// * `color_id` - SER-compatible color ID
/// * `is_color` - Whether the video is color (affects pixel format)
/// * `fps` - Frame rate for metadata
/// * `codec_name` - Source codec name (for deblock filter decision)
/// * `ffmpeg_path` - Path to ffmpeg binary
/// * `gpu_info` - Pre-detected GPU acceleration info
/// * `app` - Tauri AppHandle for progress reporting
/// * `cancel_flag` - Atomic flag to check for user cancellation
/// * `max_frames` - Optional limit on frames to load (for RAM safety)
///
/// Returns `FfmpegFrameCache` with all frames loaded.
pub fn preload_all_frames(
    path: &str,
    width: usize,
    height: usize,
    bpp: usize,
    color_id: i32,
    is_color: bool,
    fps: f64,
    codec_name: &str,
    ffmpeg_path: &str,
    gpu_info: &GpuAccelInfo,
    app: &tauri::AppHandle,
    cancel_flag: Option<&AtomicBool>,
    max_frames: Option<usize>,
) -> Result<FfmpegFrameCache, String> {
    let frame_size = width * height * bpp;
    if frame_size == 0 {
        return Err("Frame size is 0 — invalid dimensions".to_string());
    }

    let p_fmt = if is_color { "rgb48le" } else { "gray16le" };

    // Build ffmpeg command
    let mut args: Vec<String> = Vec::new();

    // Probe limits for containers with metadata at end (MOV/MP4)
    args.extend_from_slice(&[
        "-analyzeduration".into(),
        "100M".into(),
        "-probesize".into(),
        "100M".into(),
    ]);
    args.extend_from_slice(&["-hide_banner".into(), "-nostdin".into(), "-y".into()]);

    // GPU hardware acceleration
    if let Some(ref accel) = gpu_info.hwaccel {
        args.extend_from_slice(&["-hwaccel".into(), accel.clone()]);
    }

    // Use all CPU cores for decoding
    args.extend_from_slice(&["-threads".into(), "0".into()]);

    // Input file
    args.extend_from_slice(&["-i".into(), path.to_string()]);

    // Build filter chain
    let mut filters = Vec::new();

    // Soft deblock for lossy codecs (reduces macro-block artifacts)
    let needs_deblock = matches!(
        codec_name,
        "h264" | "hevc" | "h265" | "mpeg4" | "mpeg2video"
    );
    if needs_deblock {
        filters.push("unsharp=3:3:-0.3:3:3:-0.3".to_string());
    }

    // Scale (standardize resolution — uses nearest neighbor for speed)
    filters.push(format!("scale={}:{}:flags=neighbor", width, height));

    // Force output pixel format
    filters.push(format!("format={}", p_fmt));

    let filter_str = filters.join(",");

    // Output arguments
    args.extend_from_slice(&[
        "-an".into(),
        "-sn".into(),
        "-fps_mode".into(),
        "passthrough".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        p_fmt.into(),
        "-vf".into(),
        filter_str,
        "pipe:1".into(),
    ]);

    eprintln!(
        "DEBUG: FfmpegFrameCache preload args: {:?}",
        args.iter().map(|a| a.as_str()).collect::<Vec<_>>()
    );

    // Spawn ffmpeg process
    let mut cmd = Command::new(ffmpeg_path);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000); // NO_WINDOW

    let mut child = cmd
        .args(args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg for cache: {}", e))?;

    let stdout = child
        .stdout
        .take()
        .ok_or("Failed to capture ffmpeg stdout")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("Failed to capture ffmpeg stderr")?;

    // Drain stderr in a background thread to prevent deadlock
    std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stderr);
        for line in reader.lines() {
            if let Ok(l) = line {
                eprintln!("[FFMPEG CACHE]: {}", l);
            }
        }
    });

    // Read frames from pipe with large buffer
    let mut reader = std::io::BufReader::with_capacity(4 * frame_size + 65536, stdout);

    // Pre-allocate frame storage
    let estimated_frames = max_frames.unwrap_or(50000); // Conservative pre-alloc
    let mut frames: Vec<Vec<u8>> = Vec::with_capacity(estimated_frames.min(50000));

    // Buffer recycling pool
    let pool_size = 32;
    let mut recycle_pool: Vec<Vec<u8>> = (0..pool_size).map(|_| vec![0u8; frame_size]).collect();

    let mut loaded = 0usize;
    let frame_limit = max_frames.unwrap_or(usize::MAX);

    loop {
        if loaded >= frame_limit {
            break;
        }

        // Check cancellation
        if let Some(flag) = cancel_flag {
            if flag.load(Ordering::Relaxed) {
                eprintln!(
                    "DEBUG: FfmpegFrameCache preload cancelled at frame {}",
                    loaded
                );
                break;
            }
        }

        // Get a buffer (recycle if possible, otherwise allocate)
        let mut buffer = recycle_pool.pop().unwrap_or_else(|| vec![0u8; frame_size]);
        if buffer.len() != frame_size {
            buffer.resize(frame_size, 0);
        }

        // Read exactly one frame
        match reader.read_exact(&mut buffer) {
            Ok(_) => {
                frames.push(buffer);
                loaded += 1;

                // Progress reporting every 50 frames
                if loaded % 50 == 0 {
                    let gpu_tag = if gpu_info.hwaccel.is_some() {
                        " 🎮 GPU"
                    } else {
                        ""
                    };
                    let avx_tag = if cfg!(target_arch = "x86_64") {
                        #[cfg(target_arch = "x86_64")]
                        {
                            if is_x86_feature_detected!("avx2") {
                                " AVX2"
                            } else {
                                ""
                            }
                        }
                        #[cfg(not(target_arch = "x86_64"))]
                        {
                            ""
                        }
                    } else {
                        ""
                    };

                    let _ = app.emit(
                        "stacking_progress",
                        serde_json::json!({
                            "step": format!("Pre-cargando frames{}{}: {}/{}...",
                                gpu_tag, avx_tag, loaded,
                                if frame_limit < usize::MAX { frame_limit.to_string() } else { "?".to_string() }
                            ),
                            "pct": if frame_limit < usize::MAX {
                                (loaded as f32 / frame_limit as f32) * 100.0
                            } else {
                                -1.0 // Indeterminate
                            },
                        }),
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Normal end of stream
                break;
            }
            Err(e) => {
                eprintln!(
                    "DEBUG: FfmpegFrameCache read error at frame {}: {}",
                    loaded, e
                );
                break;
            }
        }
    }

    // Clean up ffmpeg process
    let _ = child.kill();
    let _ = child.wait();

    if frames.is_empty() {
        return Err("No frames could be decoded from the video".to_string());
    }

    eprintln!(
        "DEBUG: FfmpegFrameCache loaded {} frames ({}x{} @ {} bpp, {:.1} MB total)",
        frames.len(),
        width,
        height,
        bpp,
        (frames.len() * frame_size) as f64 / (1024.0 * 1024.0)
    );

    Ok(FfmpegFrameCache {
        frame_count: frames.len(),
        frames,
        width,
        height,
        bpp,
        color_id,
        fps,
        is_color,
        codec_name: codec_name.to_string(),
    })
}

/// Pre-load only SPECIFIC frames (by sorted index list) from a compressed video.
///
/// This is an optimized variant that uses `-ss` seek + sequential stream reading.
/// Only stores frames whose indices are in `target_indices`.
/// This is ideal for stacking where we only need the top-N% quality frames.
///
/// # Arguments
/// * `target_indices` - MUST be sorted in ascending order
pub fn preload_selected_frames(
    path: &str,
    target_indices: &[usize],
    width: usize,
    height: usize,
    bpp: usize,
    _color_id: i32,
    is_color: bool,
    fps: f64,
    codec_name: &str,
    ffmpeg_path: &str,
    gpu_info: &GpuAccelInfo,
    app: &tauri::AppHandle,
    cancel_flag: Option<&AtomicBool>,
    message_prefix: &str,
) -> Result<std::collections::HashMap<usize, Vec<u8>>, String> {
    if target_indices.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let frame_size = width * height * bpp;
    if frame_size == 0 {
        return Err("Frame size is 0 — invalid dimensions".to_string());
    }

    let p_fmt = if is_color { "rgb48le" } else { "gray16le" };
    let first_idx = target_indices[0];
    let last_idx = *target_indices.last().unwrap();

    // Calculate seek position for efficiency
    let start_time = if first_idx > 0 {
        Some(format!("{:.4}", first_idx as f64 / fps))
    } else {
        None
    };

    // Build ffmpeg command
    let mut args: Vec<String> = Vec::new();
    args.extend_from_slice(&[
        "-analyzeduration".into(),
        "100M".into(),
        "-probesize".into(),
        "100M".into(),
    ]);
    args.extend_from_slice(&["-hide_banner".into(), "-nostdin".into(), "-y".into()]);

    // GPU acceleration
    if let Some(ref accel) = gpu_info.hwaccel {
        args.extend_from_slice(&["-hwaccel".into(), accel.clone()]);
    }

    // Seek to start (fast input seek)
    if let Some(ref ss) = start_time {
        args.extend_from_slice(&["-ss".into(), ss.clone()]);
    }

    // Threading
    args.extend_from_slice(&["-threads".into(), "0".into()]);

    // Input
    args.extend_from_slice(&["-i".into(), path.to_string()]);

    // Filter chain
    let mut filters = Vec::new();
    let needs_deblock = matches!(
        codec_name,
        "h264" | "hevc" | "h265" | "mpeg4" | "mpeg2video"
    );
    if needs_deblock {
        filters.push("unsharp=3:3:-0.3:3:3:-0.3".to_string());
    }
    filters.push(format!("scale={}:{}:flags=neighbor", width, height));
    filters.push(format!("format={}", p_fmt));
    let filter_str = filters.join(",");

    args.extend_from_slice(&[
        "-an".into(),
        "-sn".into(),
        "-fps_mode".into(),
        "passthrough".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        p_fmt.into(),
        "-vf".into(),
        filter_str,
        "pipe:1".into(),
    ]);

    // Spawn ffmpeg
    let mut cmd = Command::new(ffmpeg_path);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x08000000);

    let mut child = cmd
        .args(args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn ffmpeg for selective cache: {}", e))?;

    let stdout = child.stdout.take().ok_or("Failed to capture stdout")?;
    let stderr = child.stderr.take().ok_or("Failed to capture stderr")?;

    // Drain stderr
    std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stderr);
        for line in reader.lines() {
            if let Ok(l) = line {
                eprintln!("[FFMPEG SEL_CACHE]: {}", l);
            }
        }
    });

    let mut reader = std::io::BufReader::with_capacity(4 * frame_size + 65536, stdout);
    let mut result = std::collections::HashMap::with_capacity(target_indices.len());

    // Track which indices we still need
    let mut idx_iter = target_indices.iter().peekable();
    let mut current_frame = first_idx; // Start counting from seek position
    let total_target = target_indices.len();
    let mut collected = 0usize;

    // Buffer for reading (recycled)
    let mut buffer = vec![0u8; frame_size];

    loop {
        // Check cancellation
        if let Some(flag) = cancel_flag {
            if flag.load(Ordering::Relaxed) {
                break;
            }
        }

        // Nothing more to find
        if idx_iter.peek().is_none() {
            break;
        }

        // Read one frame
        match reader.read_exact(&mut buffer) {
            Ok(_) => {
                // Skip duplicates and advance past the current frame
                while let Some(&&target) = idx_iter.peek() {
                    if target == current_frame {
                        // Found a needed frame — store it
                        result.insert(target, buffer.clone());
                        collected += 1;
                        idx_iter.next();

                        // Progress
                        if collected % 10 == 0 || collected == total_target {
                            let pct = (collected as f32 / total_target as f32) * 100.0;
                            let gpu_tag = if gpu_info.hwaccel.is_some() {
                                " 🎮"
                            } else {
                                ""
                            };
                            let _ = app.emit(
                                "stacking_progress",
                                serde_json::json!({
                                    "step": format!("{}{}: Cargando {}/{}...",
                                        message_prefix, gpu_tag, collected, total_target),
                                    "pct": pct,
                                }),
                            );
                        }
                    } else if target < current_frame {
                        // Skipped index (shouldn't happen with sorted input)
                        idx_iter.next();
                    } else {
                        break; // Target is ahead, keep reading
                    }
                }

                current_frame += 1;

                // Safety: stop if we've gone well past the last needed index
                if current_frame > last_idx + 10 {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(e) => {
                eprintln!(
                    "DEBUG: preload_selected_frames read error at frame {}: {}",
                    current_frame, e
                );
                break;
            }
        }
    }

    // Cleanup
    let _ = child.kill();
    let _ = child.wait();

    eprintln!(
        "DEBUG: preload_selected_frames collected {}/{} frames ({:.1} MB)",
        result.len(),
        total_target,
        (result.len() * frame_size) as f64 / (1024.0 * 1024.0)
    );

    Ok(result)
}

/// Estimate RAM usage for caching N frames of given dimensions.
/// Returns bytes.
pub fn estimate_cache_ram(width: usize, height: usize, bpp: usize, frame_count: usize) -> u64 {
    (width as u64) * (height as u64) * (bpp as u64) * (frame_count as u64)
}

/// Check if enough RAM is available to cache the given number of frames.
/// Uses 80% of free RAM as the safe limit.
pub fn can_fit_in_ram(
    width: usize,
    height: usize,
    bpp: usize,
    frame_count: usize,
) -> (bool, u64, u64) {
    let needed = estimate_cache_ram(width, height, bpp, frame_count);
    let mut sys = sysinfo::System::new_all();
    sys.refresh_memory();
    let available = sys.available_memory();
    let safe_limit = (available as f64 * 0.80) as u64;
    (needed <= safe_limit, needed, safe_limit)
}
