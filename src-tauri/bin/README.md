# FFmpeg runtime binaries

Place platform-specific FFmpeg binaries here only when you need packaged builds to carry a private runtime.

- Windows: `ffmpeg.exe` and `ffprobe.exe`
- macOS Apple Silicon: `ffmpeg-aarch64-apple-darwin` and `ffprobe-aarch64-apple-darwin`
- macOS Intel: `ffmpeg-x86_64-apple-darwin` and `ffprobe-x86_64-apple-darwin`
- macOS/Linux generic fallback: `ffmpeg` and `ffprobe`

These binaries are intentionally ignored by Git. During development, Zenith Astro Stacker will use `ffmpeg` and `ffprobe` from the system `PATH` unless `ZENITH_FFMPEG_PATH` or `ZENITH_FFPROBE_PATH` are set.
