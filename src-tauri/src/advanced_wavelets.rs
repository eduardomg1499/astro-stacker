// ==========================================
// 4.5 LIQUID WRAPING & ADVANCED WAVELETS
// ==========================================

// Helper to prevent noise amplification
fn rgb_to_yuv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let y = 0.299 * r + 0.587 * g + 0.114 * b;
    let u = -0.14713 * r - 0.28886 * g + 0.436 * b;
    let v = 0.615 * r - 0.51499 * g - 0.10001 * b;
    (y, u, v)
}

fn yuv_to_rgb(y: f32, u: f32, v: f32) -> (f32, f32, f32) {
    let r = y + 1.13983 * v;
    let g = y - 0.39465 * u - 0.58060 * v;
    let b = y + 2.03211 * u;
    (r, g, b)
}


include!("filters.rs");

