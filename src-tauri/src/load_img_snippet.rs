#[tauri::command]
async fn load_image_thumbnail(path: String) -> Result<VideoPreview, String> {
    // Check if file exists
    if !std::path::Path::new(&path).exists() {
        return Err(format!("File not found: {}", path));
    }

    // Open image
    let img = image::open(&path).map_err(|e| format!("Failed to open image: {}", e))?;

    // Create thumbnail (resize if too large, e.g. > 800px)
    // Maintain aspect ratio
    let (w, h) = img.dimensions();
    let (new_w, new_h) = if w > 800 {
        let ratio = h as f32 / w as f32;
        (800, (800.0 * ratio) as u32)
    } else {
        (w, h)
    };

    let thumb = img.resize(new_w, new_h, image::imageops::FilterType::Lanczos3);

    // Encode to PNG
    let mut buf = Vec::new();
    let mut cursor = Cursor::new(&mut buf);
    thumb
        .write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| format!("Failed to encode thumbnail: {}", e))?;

    let b64 = general_purpose::STANDARD.encode(&buf);

    Ok(VideoPreview {
        preview_base64: b64,
        metadata: VideoMetadata {
            width: w, // Return ORIGINAL dimensions for layout
            height: h,
            frame_count: 1,
            fps: 0.0,
        },
    })
}
