//! Motor `wgpu` de integración de cielo profundo.
//!
//! La CPU conserva lectura FITS, metadatos, estrellas, PSF y RANSAC. La GPU
//! recibe bandas del lienzo y hace warp + normalización + momentos ponderados.
//! Cada píxel de salida pertenece a un único invocation (gather, sin atomics),
//! y las bandas permiten trabajar con imágenes mayores que el límite de un
//! storage buffer. La fuente se recorta al bbox inverso de cada banda para no
//! subir un RGB completo cuando sólo se necesitan unas filas.

use std::sync::atomic::{AtomicU8, Ordering};

const PARITY_PENDING: u8 = 0;
const PARITY_OK: u8 = 1;
const PARITY_FAILED: u8 = 2;
static DS_PARITY: AtomicU8 = AtomicU8::new(PARITY_PENDING);
static DS_ADVANCED_WARP_PARITY: AtomicU8 = AtomicU8::new(PARITY_PENDING);
static DS_TILED_PARITY: AtomicU8 = AtomicU8::new(PARITY_PENDING);
static DS_PIXEL_PARITY: AtomicU8 = AtomicU8::new(PARITY_PENDING);

// Preprocesado de píxel previo al ajuste PSF. La CPU conserva las estadísticas
// robustas, centroides y selección; GPU ejecuta la corrección cosmética, el
// debayer float32 y el mapa denso de máximos estelares en bandas con halo.
const PIXEL_PREPROCESS_WGSL: &str = r#"
struct PixelParams {
    w: u32, tile_h: u32, full_h: u32, channels: u32,
    global_y0: u32, halo: u32, rx: u32, ry: u32,
    grid_w: u32, grid_h: u32, same_color_step: u32, _pad0: u32,
    med: vec4<f32>, noise: vec4<f32>,
}
@group(0) @binding(0) var<uniform> P: PixelParams;
@group(0) @binding(1) var<storage, read> input_px: array<f32>;
@group(0) @binding(2) var<storage, read> aux_bg: array<f32>;
@group(0) @binding(3) var<storage, read> aux_noise: array<f32>;
@group(0) @binding(4) var<storage, read_write> output_px: array<f32>;

fn grid_sample_bg(x: u32, gy: u32) -> f32 {
    let gw = P.grid_w;
    let gh = P.grid_h;
    let fx = clamp((f32(x) / f32(P.w)) * f32(max(gw, 1u) - 1u), 0.0, f32(max(gw, 1u) - 1u));
    let fy = clamp((f32(gy) / f32(P.full_h)) * f32(max(gh, 1u) - 1u), 0.0, f32(max(gh, 1u) - 1u));
    let x0 = min(u32(floor(fx)), gw - 1u); let y0 = min(u32(floor(fy)), gh - 1u);
    let x1 = min(x0 + 1u, gw - 1u); let y1 = min(y0 + 1u, gh - 1u);
    let tx = fx - f32(x0); let ty = fy - f32(y0);
    let top = aux_bg[y0 * gw + x0] * (1.0 - tx) + aux_bg[y0 * gw + x1] * tx;
    let bot = aux_bg[y1 * gw + x0] * (1.0 - tx) + aux_bg[y1 * gw + x1] * tx;
    return top * (1.0 - ty) + bot * ty;
}

fn grid_sample_noise(x: u32, gy: u32) -> f32 {
    let gw = P.grid_w;
    let gh = P.grid_h;
    let fx = clamp((f32(x) / f32(P.w)) * f32(max(gw, 1u) - 1u), 0.0, f32(max(gw, 1u) - 1u));
    let fy = clamp((f32(gy) / f32(P.full_h)) * f32(max(gh, 1u) - 1u), 0.0, f32(max(gh, 1u) - 1u));
    let x0 = min(u32(floor(fx)), gw - 1u); let y0 = min(u32(floor(fy)), gh - 1u);
    let x1 = min(x0 + 1u, gw - 1u); let y1 = min(y0 + 1u, gh - 1u);
    let tx = fx - f32(x0); let ty = fy - f32(y0);
    let top = aux_noise[y0 * gw + x0] * (1.0 - tx) + aux_noise[y0 * gw + x1] * tx;
    let bot = aux_noise[y1 * gw + x0] * (1.0 - tx) + aux_noise[y1 * gw + x1] * tx;
    return top * (1.0 - ty) + bot * ty;
}

@compute @workgroup_size(256)
fn cosmetic(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let count = P.w * P.tile_h * P.channels;
    if (i >= count) { return; }
    output_px[i] = input_px[i];
    let c = i % P.channels;
    let pixel = i / P.channels;
    let x = pixel % P.w;
    let y = pixel / P.w;
    let gy = P.global_y0 + y;
    let d = P.same_color_step;
    if (x < d || x + d >= P.w || gy < d || gy + d >= P.full_h || y < d || y + d >= P.tile_h) { return; }
    var nb: array<f32, 8>;
    let row = P.w * P.channels;
    let dx = d * P.channels;
    let dy = d * row;
    nb[0] = input_px[i - dy - dx]; nb[1] = input_px[i - dy]; nb[2] = input_px[i - dy + dx];
    nb[3] = input_px[i - dx];      nb[4] = input_px[i + dx];
    nb[5] = input_px[i + dy - dx]; nb[6] = input_px[i + dy]; nb[7] = input_px[i + dy + dx];
    for (var a = 1u; a < 8u; a = a + 1u) {
        let key = nb[a];
        var b = a;
        loop {
            if (b == 0u || nb[b - 1u] <= key) { break; }
            nb[b] = nb[b - 1u];
            b = b - 1u;
        }
        nb[b] = key;
    }
    let m8 = 0.5 * (nb[3] + nb[4]);
    let v = input_px[i];
    let noise = P.noise[c];
    let med = P.med[c];
    if ((v > m8 + 6.0 * noise && v > m8 * 1.5)
        || (v < m8 - 6.0 * noise && v < med - 3.0 * noise)) {
        output_px[i] = m8;
    }
}

@compute @workgroup_size(256)
fn debayer(@builtin(global_invocation_id) gid: vec3<u32>) {
    let pixel = gid.x;
    let count = P.w * P.tile_h;
    if (pixel >= count) { return; }
    let x = pixel % P.w;
    let y = pixel / P.w;
    let gy = P.global_y0 + y;
    let o = pixel * 3u;
    output_px[o] = 0.0; output_px[o + 1u] = 0.0; output_px[o + 2u] = 0.0;
    if (x == 0u || x + 1u >= P.w || gy == 0u || gy + 1u >= P.full_h || y == 0u || y + 1u >= P.tile_h) { return; }
    let i = pixel;
    let v = input_px[i];
    let u = input_px[i - P.w]; let d = input_px[i + P.w];
    let l = input_px[i - 1u]; let r = input_px[i + 1u];
    let diag = 0.25 * (input_px[i - P.w - 1u] + input_px[i - P.w + 1u]
        + input_px[i + P.w - 1u] + input_px[i + P.w + 1u]);
    let red = (x & 1u) == P.rx && (gy & 1u) == P.ry;
    let blue = (x & 1u) != P.rx && (gy & 1u) != P.ry;
    if (red) {
        output_px[o] = v; output_px[o + 1u] = 0.25 * (u + d + l + r); output_px[o + 2u] = diag;
    } else if (blue) {
        output_px[o] = diag; output_px[o + 1u] = 0.25 * (u + d + l + r); output_px[o + 2u] = v;
    } else if ((gy & 1u) == P.ry) {
        output_px[o] = 0.5 * (l + r); output_px[o + 1u] = v; output_px[o + 2u] = 0.5 * (u + d);
    } else {
        output_px[o] = 0.5 * (u + d); output_px[o + 1u] = v; output_px[o + 2u] = 0.5 * (l + r);
    }
}

@compute @workgroup_size(256)
fn star_map(@builtin(global_invocation_id) gid: vec3<u32>) {
    let pixel = gid.x;
    let count = P.w * P.tile_h;
    if (pixel >= count) { return; }
    output_px[pixel] = 0.0;
    let x = pixel % P.w;
    let y = pixel / P.w;
    let gy = P.global_y0 + y;
    if (x < 4u || x + 4u >= P.w || gy < 4u || gy + 4u >= P.full_h || y == 0u || y + 1u >= P.tile_h) { return; }
    let v = input_px[pixel];
    let bg = grid_sample_bg(x, gy);
    let noise = grid_sample_noise(x, gy);
    if (v <= bg + 5.0 * noise) { return; }
    let threshold = bg + 2.5 * noise;
    var is_max = true;
    var extended = 0u;
    for (var dy = -1i; dy <= 1i; dy = dy + 1i) {
        for (var dx = -1i; dx <= 1i; dx = dx + 1i) {
            if (dx == 0i && dy == 0i) { continue; }
            let ni = u32(i32(pixel) + dy * i32(P.w) + dx);
            let nv = input_px[ni];
            if (nv > v) { is_max = false; }
            if (nv > threshold) { extended = extended + 1u; }
        }
    }
    if (is_max && extended >= 4u) { output_px[pixel] = v - bg; }
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PixelParams {
    w: u32,
    tile_h: u32,
    full_h: u32,
    channels: u32,
    global_y0: u32,
    halo: u32,
    rx: u32,
    ry: u32,
    grid_w: u32,
    grid_h: u32,
    same_color_step: u32,
    pad0: u32,
    med: [f32; 4],
    noise: [f32; 4],
}

struct PixelPipelines {
    cosmetic: wgpu::ComputePipeline,
    debayer: wgpu::ComputePipeline,
    star_map: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

static PIXEL_PIPELINES: std::sync::OnceLock<PixelPipelines> = std::sync::OnceLock::new();

fn pixel_pipelines(rt: &'static crate::gpu_stack::GpuRuntime) -> &'static PixelPipelines {
    PIXEL_PIPELINES.get_or_init(|| {
        let module = rt.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("zas-deepsky-pixel-preprocess-wgsl"),
            source: wgpu::ShaderSource::Wgsl(PIXEL_PREPROCESS_WGSL.into()),
        });
        let storage = |binding, read_only| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = rt.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zas-deepsky-pixel-preprocess-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                storage(1, true), storage(2, true), storage(3, true), storage(4, false),
            ],
        });
        let pl = rt.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("zas-deepsky-pixel-preprocess-pipeline-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let make = |entry| rt.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(entry),
            layout: Some(&pl),
            module: &module,
            entry_point: Some(entry),
            compilation_options: Default::default(),
            cache: None,
        });
        PixelPipelines {
            cosmetic: make("cosmetic"),
            debayer: make("debayer"),
            star_map: make("star_map"),
            layout,
        }
    })
}

// Rechazo por-píxel para las franjas que ya contienen todos los samples
// registrados. A diferencia del integrador streaming, este kernel puede
// ordenar cada pila y ejecutar Winsorized/linear-fit reales. Un invocation
// posee un pixel-canal; no hay atomics ni carreras entre canales.
const TILED_REJECT_WGSL: &str = r#"
struct RejectParams {
    pixels: u32, channels: u32, frames: u32, method: u32,
    k_low: f32, k_high: f32, sigma_floor: f32, _p1: f32,
}
@group(0) @binding(0) var<uniform> P: RejectParams;
@group(0) @binding(1) var<storage, read_write> values: array<f32>;
@group(0) @binding(2) var<storage, read> frame_weights: array<f32>;
@group(0) @binding(3) var<storage, read_write> work_weights: array<f32>;
@group(0) @binding(4) var<storage, read_write> output: array<f32>;
@group(0) @binding(5) var<storage, read_write> coverage: array<f32>;
@group(0) @binding(6) var<storage, read_write> present: array<f32>;
@group(0) @binding(7) var<storage, read_write> rejected_low: array<f32>;
@group(0) @binding(8) var<storage, read_write> rejected_high: array<f32>;

fn median_at(base: u32, n: u32) -> f32 {
    let m = n / 2u;
    if ((n & 1u) != 0u) { return values[base + m]; }
    return 0.5 * (values[base + m - 1u] + values[base + m]);
}

@compute @workgroup_size(128)
fn reject_tiled(@builtin(global_invocation_id) gid: vec3<u32>) {
    let lane = gid.x;
    let total = P.pixels * P.channels;
    if (lane >= total) { return; }
    let pixel = lane / P.channels;
    let channel = lane % P.channels;
    let base = lane * P.frames;
    var n = 0u;
    var original_weight = 0.0;
    for (var k = 0u; k < P.frames; k = k + 1u) {
        let v = values[base + k];
        // Las posiciones sin cobertura llegan como NaN. Comprobar el exponente
        // evita que un backend con fast-math optimice `v == v` como verdadero.
        let finite = (bitcast<u32>(v) & 0x7f800000u) != 0x7f800000u;
        if (finite) {
            let w = frame_weights[k];
            values[base + n] = v;
            work_weights[base + n] = w;
            original_weight = original_weight + w;
            n = n + 1u;
        }
    }
    if (channel == 0u) { present[pixel] = original_weight; }
    if (n == 0u) {
        output[lane] = 0.0;
        if (channel == 0u) {
            coverage[pixel] = 0.0;
            rejected_low[pixel] = 0.0;
            rejected_high[pixel] = 0.0;
        }
        return;
    }
    // Insertion sort estable de (valor,peso); N astronómico suele ser 10–300
    // y cada pixel-canal se procesa independientemente.
    for (var i = 1u; i < n; i = i + 1u) {
        let key = values[base + i];
        let key_w = work_weights[base + i];
        var j = i;
        loop {
            if (j == 0u || values[base + j - 1u] <= key) { break; }
            values[base + j] = values[base + j - 1u];
            work_weights[base + j] = work_weights[base + j - 1u];
            j = j - 1u;
        }
        values[base + j] = key;
        work_weights[base + j] = key_w;
    }

    var kept = n;
    var rej_lo = 0.0;
    var rej_hi = 0.0;
    if (n > 2u && P.method == 1u) {
        // Iterative Winsorized sigma, mismos 5 ciclos/bias que CPU.
        for (var it = 0u; it < 5u; it = it + 1u) {
            if (kept < 3u) { break; }
            let med = median_at(base, kept);
            var mean = 0.0;
            for (var i = 0u; i < kept; i = i + 1u) { mean = mean + values[base + i]; }
            mean = mean / f32(kept);
            var variance = 0.0;
            for (var i = 0u; i < kept; i = i + 1u) {
                let d = values[base + i] - mean;
                variance = variance + d * d;
            }
            let sd = max(sqrt(variance / f32(kept)), 0.000001);
            let wlo = med - 1.5 * sd;
            let whi = med + 1.5 * sd;
            var win_mean = 0.0;
            for (var i = 0u; i < kept; i = i + 1u) {
                win_mean = win_mean + clamp(values[base + i], wlo, whi);
            }
            win_mean = win_mean / f32(kept);
            var win_var = 0.0;
            for (var i = 0u; i < kept; i = i + 1u) {
                let d = clamp(values[base + i], wlo, whi) - win_mean;
                win_var = win_var + d * d;
            }
            let spread = max(sqrt(win_var / f32(kept)) * 1.134, P.sigma_floor);
            let lo = med - P.k_low * spread;
            let hi = med + P.k_high * spread;
            var dst = 0u;
            for (var i = 0u; i < kept; i = i + 1u) {
                let v = values[base + i];
                let wt = work_weights[base + i];
                if (v >= lo && v <= hi) {
                    values[base + dst] = v;
                    work_weights[base + dst] = wt;
                    dst = dst + 1u;
                } else if (v < lo) {
                    rej_lo = rej_lo + wt;
                } else {
                    rej_hi = rej_hi + wt;
                }
            }
            if (dst == kept) { break; }
            kept = dst;
            if (kept < 3u) { break; }
        }
    } else if (n > 2u && P.method == 2u) {
        // Iterative linear-fit sobre rango ordenado.
        for (var it = 0u; it < 4u; it = it + 1u) {
            if (kept < 3u) { break; }
            var sx = 0.0; var sy = 0.0; var sxx = 0.0; var sxy = 0.0;
            for (var i = 0u; i < kept; i = i + 1u) {
                let x = f32(i) / f32(kept - 1u);
                let y = values[base + i];
                sx = sx + x; sy = sy + y; sxx = sxx + x*x; sxy = sxy + x*y;
            }
            let den = f32(kept) * sxx - sx * sx;
            var a = 0.0; var b = sy / f32(kept);
            if (abs(den) > 0.000000001) {
                a = (f32(kept) * sxy - sx * sy) / den;
                b = (sy * sxx - sx * sxy) / den;
            }
            var mean_r = 0.0;
            for (var i = 0u; i < kept; i = i + 1u) {
                mean_r = mean_r + values[base + i] - (a * (f32(i) / f32(kept - 1u)) + b);
            }
            mean_r = mean_r / f32(kept);
            var rv = 0.0;
            for (var i = 0u; i < kept; i = i + 1u) {
                let r = values[base + i] - (a * (f32(i) / f32(kept - 1u)) + b);
                let d = r - mean_r;
                rv = rv + d*d;
            }
            let sd = max(sqrt(rv / f32(kept)), P.sigma_floor);
            let lo = -P.k_low * sd;
            let hi = P.k_high * sd;
            var dst = 0u;
            for (var i = 0u; i < kept; i = i + 1u) {
                let v = values[base + i];
                let wt = work_weights[base + i];
                let r = v - (a * (f32(i) / f32(kept - 1u)) + b);
                if (r >= lo && r <= hi) {
                    values[base + dst] = v;
                    work_weights[base + dst] = wt;
                    dst = dst + 1u;
                } else if (r < lo) {
                    rej_lo = rej_lo + wt;
                } else {
                    rej_hi = rej_hi + wt;
                }
            }
            if (dst == kept) { break; }
            kept = dst;
            if (kept < 3u) { break; }
        }
    }

    var sum = 0.0;
    var sum_w = 0.0;
    for (var i = 0u; i < kept; i = i + 1u) {
        let wt = work_weights[base + i];
        sum = sum + values[base + i] * wt;
        sum_w = sum_w + wt;
    }
    output[lane] = select(0.0, sum / sum_w, sum_w > 0.0);
    if (channel == 0u) {
        coverage[pixel] = sum_w;
        rejected_low[pixel] = rej_lo;
        rejected_high[pixel] = rej_hi;
    }
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct RejectParams {
    pixels: u32,
    channels: u32,
    frames: u32,
    method: u32,
    k_low: f32,
    k_high: f32,
    p0: f32,
    p1: f32,
}

struct RejectPipeline {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

static REJECT_PIPELINE: std::sync::OnceLock<RejectPipeline> = std::sync::OnceLock::new();

fn reject_pipeline(rt: &'static crate::gpu_stack::GpuRuntime) -> &'static RejectPipeline {
    REJECT_PIPELINE.get_or_init(|| {
        let module = rt.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("zas-deepsky-tiled-reject-wgsl"),
            source: wgpu::ShaderSource::Wgsl(TILED_REJECT_WGSL.into()),
        });
        let storage = |binding, read_only| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = rt.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zas-deepsky-tiled-reject-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                storage(1, false), storage(2, true), storage(3, false),
                storage(4, false), storage(5, false), storage(6, false),
                storage(7, false), storage(8, false),
            ],
        });
        let pl = rt.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("zas-deepsky-tiled-reject-pipeline-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = rt.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("zas-deepsky-tiled-reject-pipeline"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("reject_tiled"),
            compilation_options: Default::default(),
            cache: None,
        });
        RejectPipeline { pipeline, layout }
    })
}

pub struct TiledRejectOutput {
    pub data: Vec<f32>,
    pub present: Vec<f32>,
    pub coverage: Vec<f32>,
    pub rejected_low: Vec<f32>,
    pub rejected_high: Vec<f32>,
    pub vram_bytes: u64,
}

pub fn reject_tiled_pass(
    stack: &[f32],
    frame_weights: &[f32],
    pixels: usize,
    channels: usize,
    method: &str,
    k_low: f32,
    k_high: f32,
    sigma_floor: f32,
) -> Result<TiledRejectOutput, String> {
    let method_id = match method {
        "winsorized" => 1,
        "linearfit" => 2,
        _ => return Err(format!("Método GPU tiled no soportado: {method}")),
    };
    let frames = frame_weights.len();
    let expected = pixels.saturating_mul(channels).saturating_mul(frames);
    if frames < 3 || stack.len() != expected {
        return Err("Stack GPU tiled con geometría inválida".into());
    }
    let rt = crate::gpu_stack::gpu_runtime().ok_or("No hay runtime wgpu")?;
    let stack_bytes = (stack.len() * 4) as u64;
    let px_bytes = (pixels * 4) as u64;
    let out_bytes = (pixels * channels * 4) as u64;
    let frame_bytes = (frames * 4) as u64;
    let needed = stack_bytes * 2 + px_bytes * 4 + out_bytes + frame_bytes + 4096;
    if stack_bytes > rt.max_binding
        || px_bytes > rt.max_binding
        || out_bytes > rt.max_binding
        || needed > rt.vram_budget
    {
        return Err(format!(
            "GPU tiled requiere {:.1} MB VRAM/binding; presupuesto {:.1} MB",
            needed as f64 / 1_048_576.0,
            rt.vram_budget as f64 / 1_048_576.0,
        ));
    }
    let params = rt.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("zas-deepsky-tiled-reject-params"),
        size: std::mem::size_of::<RejectParams>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mk = |label: &'static str, size: u64, readback: bool| rt.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(4),
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | if readback { wgpu::BufferUsages::COPY_SRC } else { wgpu::BufferUsages::empty() },
        mapped_at_creation: false,
    });
    let values = mk("zas-deepsky-tiled-values", stack_bytes, false);
    let frame_w = mk("zas-deepsky-tiled-frame-weights", frame_bytes, false);
    let work_w = mk("zas-deepsky-tiled-work-weights", stack_bytes, false);
    let output = mk("zas-deepsky-tiled-output", out_bytes, true);
    let coverage = mk("zas-deepsky-tiled-coverage", px_bytes, true);
    let present = mk("zas-deepsky-tiled-present", px_bytes, true);
    let low = mk("zas-deepsky-tiled-reject-low", px_bytes, true);
    let high = mk("zas-deepsky-tiled-reject-high", px_bytes, true);
    rt.queue.write_buffer(&values, 0, bytemuck::cast_slice(stack));
    rt.queue.write_buffer(&frame_w, 0, bytemuck::cast_slice(frame_weights));
    rt.queue.write_buffer(&params, 0, bytemuck::bytes_of(&RejectParams {
        pixels: pixels as u32,
        channels: channels as u32,
        frames: frames as u32,
        method: method_id,
        k_low,
        k_high,
        p0: sigma_floor,
        p1: 0.0,
    }));
    let pp = reject_pipeline(rt);
    let bind = rt.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("zas-deepsky-tiled-reject-bind"),
        layout: &pp.layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: values.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: frame_w.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 3, resource: work_w.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 4, resource: output.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 5, resource: coverage.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 6, resource: present.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 7, resource: low.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 8, resource: high.as_entire_binding() },
        ],
    });
    let mut enc = rt.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("zas-deepsky-tiled-reject-encoder"),
    });
    {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("zas-deepsky-tiled-reject-pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pp.pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(((pixels * channels) as u32).div_ceil(128), 1, 1);
    }
    rt.queue.submit(Some(enc.finish()));
    let data = read_f32(rt, &output, pixels * channels)?;
    let coverage_v = read_f32(rt, &coverage, pixels)?;
    let present_v = read_f32(rt, &present, pixels)?;
    let low_v = read_f32(rt, &low, pixels)?;
    let high_v = read_f32(rt, &high, pixels)?;
    if crate::gpu_stack::take_gpu_error() {
        return Err("Device loss/OOM durante rechazo GPU tiled".into());
    }
    Ok(TiledRejectOutput {
        data,
        present: present_v,
        coverage: coverage_v,
        rejected_low: low_v,
        rejected_high: high_v,
        vram_bytes: needed,
    })
}

const DS_WGSL: &str = r#"
struct Params {
    src_w: u32, src_h: u32, out_w: u32, tile_rows: u32,
    tile_y0: u32, channels: u32, flags: u32, ln_g: u32,
    full_w: u32, full_h: u32, src_x0: u32, src_y0: u32,
    out_h: u32, band_y0: u32, band_y1: u32, _p2: u32,
    m0: f32, m1: f32, m2: f32, m3: f32,
    m4: f32, m5: f32, m6: f32, m7: f32,
    m8: f32, inv_scale: f32, norm_mul: f32, norm_add: f32,
    frame_weight: f32, norm_cx: f32, norm_cy: f32, norm_scale: f32,
    q0: f32, q1: f32, q2: f32, q3: f32,
    q4: f32, q5: f32, q6: f32, q7: f32,
    q8: f32, q9: f32, q10: f32, q11: f32,
    // Per-channel normalization for channels 1 and 2 (channel 0 reuses
    // norm_mul/norm_add above): v'[c] = raw·mul[c] + add[c].
    norm_mul1: f32, norm_add1: f32, norm_mul2: f32, norm_add2: f32,
}

fn norm_mul_c(c: u32) -> f32 {
    if (c == 1u) { return P.norm_mul1; }
    if (c == 2u) { return P.norm_mul2; }
    return P.norm_mul;
}
fn norm_add_c(c: u32) -> f32 {
    if (c == 1u) { return P.norm_add1; }
    if (c == 2u) { return P.norm_add2; }
    return P.norm_add;
}
// flags: 1=Lanczos3, 2=bounds, 4=local normalization, 8=track M2,
//        16=transformación cuadrática de distorsión local
@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> frame: array<f32>;
@group(0) @binding(2) var<storage, read> local_grid: array<f32>;
@group(0) @binding(3) var<storage, read> lower: array<f32>;
@group(0) @binding(4) var<storage, read> upper: array<f32>;
@group(0) @binding(5) var<storage, read_write> mean: array<f32>;
@group(0) @binding(6) var<storage, read_write> moment2: array<f32>;
@group(0) @binding(7) var<storage, read_write> weight: array<f32>;
@group(0) @binding(8) var<storage, read> l3: array<f32>;
@group(0) @binding(9) var<storage, read_write> rejected_low: array<f32>;
@group(0) @binding(10) var<storage, read_write> rejected_high: array<f32>;

fn px(x: u32, y: u32, c: u32) -> f32 {
    return frame[(y * P.src_w + x) * P.channels + c];
}

fn grid_value(u0: f32, v0: f32) -> f32 {
    if ((P.flags & 4u) == 0u || P.ln_g == 0u) { return 0.0; }
    let u = clamp(u0, 0.0, 1.0) * f32(P.ln_g - 1u);
    let v = clamp(v0, 0.0, 1.0) * f32(P.ln_g - 1u);
    let x0 = min(u32(floor(u)), P.ln_g - 1u);
    let y0 = min(u32(floor(v)), P.ln_g - 1u);
    let x1 = min(x0 + 1u, P.ln_g - 1u);
    let y1 = min(y0 + 1u, P.ln_g - 1u);
    let fx = u - f32(x0);
    let fy = v - f32(y0);
    let a = local_grid[y0 * P.ln_g + x0];
    let b = local_grid[y0 * P.ln_g + x1];
    let c = local_grid[y1 * P.ln_g + x0];
    let d = local_grid[y1 * P.ln_g + x1];
    return mix(mix(a, b, fx), mix(c, d, fx), fy);
}

fn lut(x: f32) -> f32 {
    let ax = abs(x);
    if (ax >= 3.0) { return 0.0; }
    return l3[min(u32(ax * 1024.0), 3072u)];
}

fn bilinear(sx: f32, sy: f32, c: u32) -> f32 {
    let lx = sx - f32(P.src_x0);
    let ly = sy - f32(P.src_y0);
    let x0 = u32(floor(lx));
    let y0 = u32(floor(ly));
    let fx = lx - f32(x0);
    let fy = ly - f32(y0);
    let v00 = px(x0, y0, c);
    let v10 = px(x0 + 1u, y0, c);
    let v01 = px(x0, y0 + 1u, c);
    let v11 = px(x0 + 1u, y0 + 1u, c);
    return v00 * (1.0 - fx) * (1.0 - fy)
         + v10 * fx * (1.0 - fy)
         + v01 * (1.0 - fx) * fy
         + v11 * fx * fy;
}

// Misma LUT, orden de sumas y clamp simétrico que ds_sample_lanczos3.
fn lanczos3(sx: f32, sy: f32, c: u32) -> f32 {
    let gx0 = u32(floor(sx));
    let gy0 = u32(floor(sy));
    let lx0 = gx0 - P.src_x0;
    let ly0 = gy0 - P.src_y0;
    let fx = sx - f32(gx0);
    let fy = sy - f32(gy0);
    var wx: array<f32, 6>;
    var wy: array<f32, 6>;
    var swx = 0.0;
    var swy = 0.0;
    for (var k = 0u; k < 6u; k = k + 1u) {
        let off = f32(k) - 2.0;
        wx[k] = lut(off - fx);
        wy[k] = lut(off - fy);
        swx = swx + wx[k];
        swy = swy + wy[k];
    }
    var acc = 0.0;
    for (var j = 0u; j < 6u; j = j + 1u) {
        var ax = 0.0;
        for (var i = 0u; i < 6u; i = i + 1u) {
            ax = ax + wx[i] * px(lx0 + i - 2u, ly0 + j - 2u, c);
        }
        acc = acc + wy[j] * ax;
    }
    let bx = gx0 - P.src_x0;
    let by = gy0 - P.src_y0;
    let p00 = px(bx, by, c);
    let p10 = px(bx + 1u, by, c);
    let p01 = px(bx, by + 1u, c);
    let p11 = px(bx + 1u, by + 1u, c);
    let lo4 = min(min(p00, p10), min(p01, p11));
    let hi4 = max(max(p00, p10), max(p01, p11));
    return clamp(acc / max(swx * swy, 0.000001), lo4, hi4);
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    // Banded dispatch: gid.y is relative to band_y0; band_y1 bounds it so the
    // div_ceil(8) padding of a sub-band never re-processes the next band's rows.
    // With one band (band_y0=0, band_y1=tile_rows) this is identical to before.
    let ly = P.band_y0 + gid.y;
    if (gid.x >= P.out_w || ly >= P.band_y1) { return; }
    let gy = P.tile_y0 + ly;
    let rx = f32(gid.x) * P.inv_scale;
    let ry = f32(gy) * P.inv_scale;
    var sx = 0.0;
    var sy = 0.0;
    if ((P.flags & 16u) != 0u) {
        // Newton sobre el polinomio target→reference, misma semilla y
        // Jacobiano que la referencia CPU.
        let ad = P.q0 * P.q7 - P.q1 * P.q6;
        if (abs(ad) < 0.000000000001) { return; }
        let du = rx - P.q2;
        let dv = ry - P.q8;
        var xn = (P.q7 * du - P.q1 * dv) / ad;
        var yn = (-P.q6 * du + P.q0 * dv) / ad;
        for (var it = 0u; it < 7u; it = it + 1u) {
            let fu = P.q0*xn + P.q1*yn + P.q2 + P.q3*xn*xn + P.q4*xn*yn + P.q5*yn*yn - rx;
            let fv = P.q6*xn + P.q7*yn + P.q8 + P.q9*xn*xn + P.q10*xn*yn + P.q11*yn*yn - ry;
            let j00 = P.q0 + 2.0*P.q3*xn + P.q4*yn;
            let j01 = P.q1 + P.q4*xn + 2.0*P.q5*yn;
            let j10 = P.q6 + 2.0*P.q9*xn + P.q10*yn;
            let j11 = P.q7 + P.q10*xn + 2.0*P.q11*yn;
            let jd = j00*j11 - j01*j10;
            if (abs(jd) < 0.00000000000001) { return; }
            let ddx = (j11*fu - j01*fv) / jd;
            let ddy = (-j10*fu + j00*fv) / jd;
            xn = xn - ddx;
            yn = yn - ddy;
        }
        sx = xn * P.norm_scale + P.norm_cx;
        sy = yn * P.norm_scale + P.norm_cy;
    } else {
        let hd = P.m6 * rx + P.m7 * ry + P.m8;
        if (abs(hd) < 0.000000000001) { return; }
        sx = (P.m0 * rx + P.m1 * ry + P.m2) / hd;
        sy = (P.m3 * rx + P.m4 * ry + P.m5) / hd;
    }
    if (sx < 0.0 || sy < 0.0 || sx >= f32(P.full_w - 1u) || sy >= f32(P.full_h - 1u)) { return; }
    let lsx = sx - f32(P.src_x0);
    let lsy = sy - f32(P.src_y0);
    if (lsx < 0.0 || lsy < 0.0 || lsx >= f32(P.src_w - 1u) || lsy >= f32(P.src_h - 1u)) { return; }

    let lanczos_ok = (P.flags & 1u) != 0u
        && sx >= 3.0 && sy >= 3.0
        && sx < f32(P.full_w - 4u) && sy < f32(P.full_h - 4u)
        && lsx >= 3.0 && lsy >= 3.0
        && lsx < f32(P.src_w - 4u) && lsy < f32(P.src_h - 4u);
    let loc = grid_value(f32(gid.x) / f32(P.out_w), f32(gy) / f32(P.out_h));
    let pix = ly * P.out_w + gid.x;
    var vals: array<f32, 3>;
    for (var c = 0u; c < P.channels; c = c + 1u) {
        let raw = select(bilinear(sx, sy, c), lanczos3(sx, sy, c), lanczos_ok);
        vals[c] = raw * norm_mul_c(c) + norm_add_c(c) + loc;
    }
    if (P.frame_weight <= 0.0) { return; }
    // PER-CHANNEL rejection + weighted Welford. A channel outside its κσ window is
    // rejected on its own, so one hot channel no longer discards the good data in
    // the others (no colour fringes on clipped cosmic rays/satellite trails). The
    // weight is now per channel (weight[oi]); for mono this is identical to the
    // old per-pixel path, so parity is preserved.
    var any_low = false;
    var any_high = false;
    for (var c = 0u; c < P.channels; c = c + 1u) {
        let oi = pix * P.channels + c;
        var accept_c = true;
        if ((P.flags & 2u) != 0u) {
            if (vals[c] < lower[oi]) { accept_c = false; any_low = true; }
            else if (vals[c] > upper[oi]) { accept_c = false; any_high = true; }
        }
        if (accept_c) {
            let old_w = weight[oi];
            let new_w = old_w + P.frame_weight;
            let old_mean = mean[oi];
            let delta = vals[c] - old_mean;
            let new_mean = old_mean + delta * (P.frame_weight / new_w);
            mean[oi] = new_mean;
            if ((P.flags & 8u) != 0u) {
                moment2[oi] = moment2[oi] + P.frame_weight * delta * (vals[c] - new_mean);
            }
            weight[oi] = new_w;
        }
    }
    // Per-pixel rejection maps (QA): tag the pixel if ANY channel was clipped.
    if (any_low && any_high) {
        rejected_low[pix] = rejected_low[pix] + 0.5 * P.frame_weight;
        rejected_high[pix] = rejected_high[pix] + 0.5 * P.frame_weight;
    } else if (any_low) {
        rejected_low[pix] = rejected_low[pix] + P.frame_weight;
    } else if (any_high) {
        rejected_high[pix] = rejected_high[pix] + P.frame_weight;
    }
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    src_w: u32,
    src_h: u32,
    out_w: u32,
    tile_rows: u32,
    tile_y0: u32,
    channels: u32,
    flags: u32,
    ln_g: u32,
    full_w: u32,
    full_h: u32,
    src_x0: u32,
    src_y0: u32,
    out_h: u32,
    p0: u32,
    p1: u32,
    p2: u32,
    m0: f32,
    m1: f32,
    m2: f32,
    m3: f32,
    m4: f32,
    m5: f32,
    m6: f32,
    m7: f32,
    m8: f32,
    inv_scale: f32,
    norm_mul: f32,
    norm_add: f32,
    frame_weight: f32,
    norm_cx: f32,
    norm_cy: f32,
    norm_scale: f32,
    q0: f32,
    q1: f32,
    q2: f32,
    q3: f32,
    q4: f32,
    q5: f32,
    q6: f32,
    q7: f32,
    q8: f32,
    q9: f32,
    q10: f32,
    q11: f32,
    // Per-channel normalization for channels 1 and 2 (channel 0 = norm_mul/add).
    norm_mul1: f32,
    norm_add1: f32,
    norm_mul2: f32,
    norm_add2: f32,
}

struct Pipelines {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    lut: wgpu::Buffer,
}

static PIPELINES: std::sync::OnceLock<Pipelines> = std::sync::OnceLock::new();

const CALIBRATE_WGSL: &str = r#"
struct CalParams { n: u32, flags: u32, _p0: u32, _p1: u32, dark_scale: f32, _p2: f32, _p3: f32, _p4: f32 }
@group(0) @binding(0) var<uniform> P: CalParams;
@group(0) @binding(1) var<storage, read_write> light: array<f32>;
@group(0) @binding(2) var<storage, read> bias: array<f32>;
@group(0) @binding(3) var<storage, read> dark: array<f32>;
@group(0) @binding(4) var<storage, read> flat: array<f32>;
@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= P.n) { return; }
    var v = light[i];
    if ((P.flags & 1u) != 0u) { v = v - bias[i]; }
    if ((P.flags & 2u) != 0u) { v = v - P.dark_scale * dark[i]; }
    if ((P.flags & 4u) != 0u) { v = v / max(flat[i], 0.05); }
    light[i] = v;
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CalParams {
    n: u32,
    flags: u32,
    p0: u32,
    p1: u32,
    dark_scale: f32,
    p2: f32,
    p3: f32,
    p4: f32,
}

struct CalPipelines {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

static CAL_PIPELINES: std::sync::OnceLock<CalPipelines> = std::sync::OnceLock::new();

fn calibration_pipelines(rt: &'static crate::gpu_stack::GpuRuntime) -> &'static CalPipelines {
    CAL_PIPELINES.get_or_init(|| {
        let shader = rt.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("zas-deepsky-calibrate-wgsl"),
            source: wgpu::ShaderSource::Wgsl(CALIBRATE_WGSL.into()),
        });
        let storage = |binding, read_only| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = rt.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zas-deepsky-calibrate-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                storage(1, false), storage(2, true), storage(3, true), storage(4, true),
            ],
        });
        let pipe_layout = rt.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("zas-deepsky-calibrate-pipeline-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = rt.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("zas-deepsky-calibrate-pipeline"),
            layout: Some(&pipe_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        CalPipelines { pipeline, layout }
    })
}

fn pipelines(rt: &'static crate::gpu_stack::GpuRuntime) -> &'static Pipelines {
    PIPELINES.get_or_init(|| {
        let shader = rt.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("zas-deepsky-integrate-wgsl"),
            source: wgpu::ShaderSource::Wgsl(DS_WGSL.into()),
        });
        let storage = |binding, read_only| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = rt.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zas-deepsky-integrate-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                storage(1, true), storage(2, true), storage(3, true), storage(4, true),
                storage(5, false), storage(6, false), storage(7, false), storage(8, true),
                storage(9, false), storage(10, false),
            ],
        });
        let pipe_layout = rt.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("zas-deepsky-integrate-pipeline-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = rt.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("zas-deepsky-integrate-pipeline"),
            layout: Some(&pipe_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let lut_values: Vec<f32> = (0..=3072)
            .map(|i| {
                let x = i as f32 / 1024.0;
                if x < 1e-4 { 1.0 } else if x >= 3.0 { 0.0 } else {
                    let pix = std::f32::consts::PI * x;
                    3.0 * (pix.sin() * (pix / 3.0).sin()) / (pix * pix)
                }
            })
            .collect();
        let lut = rt.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zas-deepsky-lanczos3-lut"),
            size: (lut_values.len() * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        rt.queue.write_buffer(&lut, 0, bytemuck::cast_slice(&lut_values));
        Pipelines { pipeline, layout, lut }
    })
}

#[derive(Clone, Copy, Debug)]
pub struct WarpTransform {
    /// Homografía reference→target ya invertida en CPU.
    pub inverse_h: [f32; 9],
    /// Polinomio target→reference para Newton cuando `local_distortion`.
    pub poly: [f32; 12],
    pub norm: [f32; 3],
    pub local_distortion: bool,
}

impl WarpTransform {
    pub fn from_similarity(t: (f32, f32, f32, f32)) -> Self {
        let det = (t.0 * t.0 + t.1 * t.1).max(1e-12);
        let ia = t.0 / det;
        let ib = -t.1 / det;
        Self {
            inverse_h: [
                ia, -ib, -ia * t.2 + ib * t.3,
                ib, ia, -ib * t.2 - ia * t.3,
                0.0, 0.0, 1.0,
            ],
            poly: [0.0; 12],
            norm: [0.0, 0.0, 1.0],
            local_distortion: false,
        }
    }

    fn inverse(self, u: f32, v: f32) -> Option<(f32, f32)> {
        if self.local_distortion {
            let q = self.poly;
            let det = q[0] * q[7] - q[1] * q[6];
            if det.abs() < 1e-12 { return None; }
            let du = u - q[2];
            let dv = v - q[8];
            let mut xn = (q[7] * du - q[1] * dv) / det;
            let mut yn = (-q[6] * du + q[0] * dv) / det;
            for _ in 0..7 {
                let fu = q[0]*xn + q[1]*yn + q[2] + q[3]*xn*xn + q[4]*xn*yn + q[5]*yn*yn - u;
                let fv = q[6]*xn + q[7]*yn + q[8] + q[9]*xn*xn + q[10]*xn*yn + q[11]*yn*yn - v;
                let j00 = q[0] + 2.0*q[3]*xn + q[4]*yn;
                let j01 = q[1] + q[4]*xn + 2.0*q[5]*yn;
                let j10 = q[6] + 2.0*q[9]*xn + q[10]*yn;
                let j11 = q[7] + q[10]*xn + 2.0*q[11]*yn;
                let jd = j00*j11 - j01*j10;
                if jd.abs() < 1e-14 { return None; }
                let dx = (j11*fu - j01*fv) / jd;
                let dy = (-j10*fu + j00*fv) / jd;
                xn -= dx;
                yn -= dy;
            }
            return Some((xn * self.norm[2] + self.norm[0], yn * self.norm[2] + self.norm[1]));
        }
        let h = self.inverse_h;
        let d = h[6] * u + h[7] * v + h[8];
        if d.abs() < 1e-12 { return None; }
        Some(((h[0] * u + h[1] * v + h[2]) / d, (h[3] * u + h[4] * v + h[5]) / d))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FrameMeta {
    pub index: usize,
    pub transform: WarpTransform,
    pub weight: f32,
    // Per-channel normalization (mul[3], add[3]); mono uses index 0.
    pub norm: ([f32; 3], [f32; 3]),
}

pub struct IntegrateConfig {
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub scale: f32,
    pub lanczos: bool,
    pub local_grid_size: usize,
    pub track_m2: bool,
}

pub struct GpuPassResult {
    pub mean: Vec<f32>,
    pub moment2: Vec<f32>,
    pub weight: Vec<f32>,
    pub rejected_low: Vec<f32>,
    pub rejected_high: Vec<f32>,
    pub peak_vram_bytes: u64,
    pub tiles: usize,
}

#[derive(Clone, Copy)]
struct Crop { x: usize, y: usize, w: usize, h: usize }

fn crop_for(cfg: &IntegrateConfig, meta: FrameMeta, y0: usize, y1: usize) -> Crop {
    let inv_scale = 1.0 / cfg.scale.max(1.0);
    let mut minx = f32::MAX;
    let mut maxx = f32::MIN;
    let mut miny = f32::MAX;
    let mut maxy = f32::MIN;
    for &(ox, oy) in &[
        (0.0, y0 as f32),
        ((cfg.width - 1) as f32, y0 as f32),
        (0.0, (y1 - 1) as f32),
        ((cfg.width - 1) as f32, (y1 - 1) as f32),
    ] {
        let Some((sx, sy)) = meta.transform.inverse(ox * inv_scale, oy * inv_scale) else { continue; };
        minx = minx.min(sx);
        maxx = maxx.max(sx);
        miny = miny.min(sy);
        maxy = maxy.max(sy);
    }
    let margin = if cfg.lanczos { 4.0 } else { 1.0 };
    let x0 = (minx.floor() - margin).max(0.0) as usize;
    let y0s = (miny.floor() - margin).max(0.0) as usize;
    let x1 = (maxx.ceil() + margin).min((cfg.width - 1) as f32) as usize;
    let y1s = (maxy.ceil() + margin).min((cfg.height - 1) as f32) as usize;
    Crop {
        x: x0,
        y: y0s,
        w: x1.saturating_sub(x0).saturating_add(1).max(2),
        h: y1s.saturating_sub(y0s).saturating_add(1).max(2),
    }
}

fn copy_crop(full: &[f32], cfg: &IntegrateConfig, crop: Crop) -> Vec<f32> {
    let mut out = vec![0.0f32; crop.w * crop.h * cfg.channels];
    let row_len = crop.w * cfg.channels;
    for y in 0..crop.h {
        let src = ((crop.y + y) * cfg.width + crop.x) * cfg.channels;
        let dst = y * row_len;
        out[dst..dst + row_len].copy_from_slice(&full[src..src + row_len]);
    }
    out
}

fn read_f32(rt: &crate::gpu_stack::GpuRuntime, buffer: &wgpu::Buffer, count: usize) -> Result<Vec<f32>, String> {
    let bytes = (count * 4) as u64;
    let staging = rt.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("zas-deepsky-readback"),
        size: bytes.max(4),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = rt.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("zas-deepsky-readback-encoder"),
    });
    enc.copy_buffer_to_buffer(buffer, 0, &staging, 0, bytes);
    rt.queue.submit(Some(enc.finish()));
    let slice = staging.slice(0..bytes);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| { let _ = tx.send(r); });
    crate::gpu_stack::wait_for_readback(&rt.device, &rx, "readback deepsky")?;
    let out = {
        let mapped = slice.get_mapped_range();
        bytemuck::cast_slice::<u8, f32>(&mapped).to_vec()
    };
    staging.unmap();
    Ok(out)
}

#[derive(Clone, Copy)]
enum PixelKernel { Cosmetic, Debayer, StarMap }

#[allow(clippy::too_many_arguments)]
fn run_pixel_tiles(
    input: &[f32],
    w: usize,
    h: usize,
    input_channels: usize,
    output_channels: usize,
    halo: usize,
    mut params: PixelParams,
    aux_bg: &[f32],
    aux_noise: &[f32],
    kernel: PixelKernel,
) -> Result<Vec<f32>, String> {
    if w == 0 || h == 0 || input.len() < w * h * input_channels {
        return Err("Preprocesado GPU: geometría de entrada inválida".into());
    }
    let rt = crate::gpu_stack::gpu_runtime().ok_or("No hay runtime wgpu")?;
    let row_in = w.saturating_mul(input_channels).saturating_mul(4).max(4);
    let row_out = w.saturating_mul(output_channels).saturating_mul(4).max(4);
    let by_input = (rt.max_binding as usize / row_in).max(1);
    let by_output = (rt.max_binding as usize / row_out).max(1);
    // input + output + staging readback; deja margen para auxiliares/runtime.
    let by_budget = (rt.vram_budget as usize / row_in.saturating_add(row_out * 2)).max(1);
    let max_rows = h.min(by_input).min(by_output).min(by_budget);
    if max_rows <= halo * 2 {
        return Err(format!(
            "Preprocesado GPU no puede reservar una banda con halo: {} filas disponibles",
            max_rows
        ));
    }
    let interior_rows = (max_rows - halo * 2).max(1);
    let input_bytes = (max_rows * row_in) as u64;
    let output_bytes = (max_rows * row_out) as u64;
    let aux_bg_bytes = (aux_bg.len().max(1) * 4) as u64;
    let aux_noise_bytes = (aux_noise.len().max(1) * 4) as u64;
    if aux_bg_bytes > rt.max_binding || aux_noise_bytes > rt.max_binding {
        return Err("Grilla auxiliar excede el límite de binding GPU".into());
    }
    let pp = pixel_pipelines(rt);
    let uniform = rt.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("zas-deepsky-pixel-params"),
        size: std::mem::size_of::<PixelParams>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let make = |label: &str, size: u64, read_only: bool| rt.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(4),
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | if read_only { wgpu::BufferUsages::empty() } else { wgpu::BufferUsages::COPY_SRC },
        mapped_at_creation: false,
    });
    let input_buf = make("zas-deepsky-pixel-input", input_bytes, true);
    let output_buf = make("zas-deepsky-pixel-output", output_bytes, false);
    let bg_buf = make("zas-deepsky-pixel-bg", aux_bg_bytes, true);
    let noise_buf = make("zas-deepsky-pixel-noise", aux_noise_bytes, true);
    let zero = [0.0f32];
    rt.queue.write_buffer(&bg_buf, 0, bytemuck::cast_slice(if aux_bg.is_empty() { &zero } else { aux_bg }));
    rt.queue.write_buffer(&noise_buf, 0, bytemuck::cast_slice(if aux_noise.is_empty() { &zero } else { aux_noise }));
    let bind = rt.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("zas-deepsky-pixel-bind"),
        layout: &pp.layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: input_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: bg_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 3, resource: noise_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 4, resource: output_buf.as_entire_binding() },
        ],
    });
    let pipeline = match kernel {
        PixelKernel::Cosmetic => &pp.cosmetic,
        PixelKernel::Debayer => &pp.debayer,
        PixelKernel::StarMap => &pp.star_map,
    };
    let mut output = vec![0.0f32; w * h * output_channels];
    for out_y0 in (0..h).step_by(interior_rows) {
        let out_y1 = (out_y0 + interior_rows).min(h);
        let src_y0 = out_y0.saturating_sub(halo);
        let src_y1 = (out_y1 + halo).min(h);
        let tile_h = src_y1 - src_y0;
        let input_start = src_y0 * w * input_channels;
        let input_end = src_y1 * w * input_channels;
        rt.queue.write_buffer(&input_buf, 0, bytemuck::cast_slice(&input[input_start..input_end]));
        params.w = w as u32;
        params.tile_h = tile_h as u32;
        params.full_h = h as u32;
        params.channels = input_channels as u32;
        params.global_y0 = src_y0 as u32;
        params.halo = halo as u32;
        rt.queue.write_buffer(&uniform, 0, bytemuck::bytes_of(&params));
        let invocations = match kernel {
            PixelKernel::Cosmetic => tile_h * w * input_channels,
            PixelKernel::Debayer | PixelKernel::StarMap => tile_h * w,
        };
        let mut enc = rt.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("zas-deepsky-pixel-encoder"),
        });
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("zas-deepsky-pixel-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups((invocations as u32).div_ceil(256), 1, 1);
        }
        rt.queue.submit(Some(enc.finish()));
        let tile = read_f32(rt, &output_buf, tile_h * w * output_channels)?;
        let local_y0 = out_y0 - src_y0;
        for y in out_y0..out_y1 {
            let src = (local_y0 + y - out_y0) * w * output_channels;
            let dst = y * w * output_channels;
            let len = w * output_channels;
            output[dst..dst + len].copy_from_slice(&tile[src..src + len]);
        }
        if crate::gpu_stack::take_gpu_error() {
            return Err("Device loss/OOM durante preprocesado de cielo profundo".into());
        }
    }
    Ok(output)
}

pub fn cosmetic_hot_pixels(
    data: &mut [f32],
    w: usize,
    h: usize,
    channels: usize,
    bayer: bool,
    medians: &[f32],
    noises: &[f32],
) -> Result<u64, String> {
    if !matches!(channels, 1 | 3) || medians.len() < channels || noises.len() < channels {
        return Err("Parámetros cosméticos GPU inválidos".into());
    }
    let mut med = [0.0f32; 4];
    let mut noise = [1.0f32; 4];
    med[..channels].copy_from_slice(&medians[..channels]);
    noise[..channels].copy_from_slice(&noises[..channels]);
    let params = PixelParams {
        w: 0, tile_h: 0, full_h: 0, channels: channels as u32,
        global_y0: 0, halo: 0, rx: 0, ry: 0,
        grid_w: 1, grid_h: 1, same_color_step: if bayer { 2 } else { 1 }, pad0: 0,
        med, noise,
    };
    let out = run_pixel_tiles(
        data, w, h, channels, channels, if bayer { 2 } else { 1 }, params,
        &[], &[], PixelKernel::Cosmetic,
    )?;
    data.copy_from_slice(&out);
    Ok((data.len() * 12) as u64)
}

pub fn debayer_float32(data: &[f32], w: usize, h: usize, cid: i32) -> Result<Vec<f32>, String> {
    let (rx, ry) = match cid {
        8 => (0, 0), 9 => (1, 0), 10 => (0, 1), 11 => (1, 1),
        _ => return Err(format!("Patrón Bayer GPU no soportado: {cid}")),
    };
    let params = PixelParams {
        w: 0, tile_h: 0, full_h: 0, channels: 1,
        global_y0: 0, halo: 0, rx, ry,
        grid_w: 1, grid_h: 1, same_color_step: 1, pad0: 0,
        med: [0.0; 4], noise: [1.0; 4],
    };
    run_pixel_tiles(data, w, h, 1, 3, 1, params, &[], &[], PixelKernel::Debayer)
}

pub fn star_candidate_map(
    luma: &[f32],
    w: usize,
    h: usize,
    bg: &[f32],
    noise: &[f32],
    grid_w: usize,
    grid_h: usize,
) -> Result<Vec<f32>, String> {
    if bg.len() != grid_w * grid_h || noise.len() != grid_w * grid_h || grid_w == 0 || grid_h == 0 {
        return Err("Grillas de estrellas GPU inválidas".into());
    }
    let params = PixelParams {
        w: 0, tile_h: 0, full_h: 0, channels: 1,
        global_y0: 0, halo: 0, rx: 0, ry: 0,
        grid_w: grid_w as u32, grid_h: grid_h as u32, same_color_step: 1, pad0: 0,
        med: [0.0; 4], noise: [1.0; 4],
    };
    run_pixel_tiles(luma, w, h, 1, 1, 1, params, bg, noise, PixelKernel::StarMap)
}

/// Calibración lineal en GPU, fragmentada para respetar VRAM y binding size.
/// Los masters deben compartir la geometría ya resuelta por el caller; una
/// ausencia se expresa como `None`, no como un buffer ficticio aplicado.
pub fn calibrate(
    light: &mut [f32],
    bias: Option<&[f32]>,
    dark: Option<&[f32]>,
    flat: Option<&[f32]>,
    dark_scale: f32,
) -> Result<u64, String> {
    if light.is_empty() {
        return Ok(0);
    }
    for (name, data) in [("bias", bias), ("dark", dark), ("flat", flat)] {
        if data.is_some_and(|v| v.len() != light.len()) {
            return Err(format!("Master {name} con geometría distinta para calibración GPU"));
        }
    }
    let rt = crate::gpu_stack::gpu_runtime().ok_or("No hay runtime wgpu")?;
    let pipes = calibration_pipelines(rt);
    // Cuatro storage buffers residentes. Reserva margen para uniform/readback.
    let by_binding = (rt.max_binding / 4) as usize;
    let by_budget = (rt.vram_budget / 20) as usize;
    let chunk = light.len().min(by_binding).min(by_budget).max(1);
    let bytes = (chunk * 4) as u64;
    let params = rt.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("zas-deepsky-calibrate-params"),
        size: std::mem::size_of::<CalParams>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mk = |label, readback| rt.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: bytes.max(4),
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | if readback { wgpu::BufferUsages::COPY_SRC } else { wgpu::BufferUsages::empty() },
        mapped_at_creation: false,
    });
    let light_buf = mk("zas-deepsky-calibrate-light", true);
    let bias_buf = mk("zas-deepsky-calibrate-bias", false);
    let dark_buf = mk("zas-deepsky-calibrate-dark", false);
    let flat_buf = mk("zas-deepsky-calibrate-flat", false);
    let bind = rt.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("zas-deepsky-calibrate-bind"),
        layout: &pipes.layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: light_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 2, resource: bias_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 3, resource: dark_buf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 4, resource: flat_buf.as_entire_binding() },
        ],
    });

    let mut offset = 0usize;
    while offset < light.len() {
        let n = chunk.min(light.len() - offset);
        let end = offset + n;
        rt.queue.write_buffer(&light_buf, 0, bytemuck::cast_slice(&light[offset..end]));
        if let Some(v) = bias { rt.queue.write_buffer(&bias_buf, 0, bytemuck::cast_slice(&v[offset..end])); }
        if let Some(v) = dark { rt.queue.write_buffer(&dark_buf, 0, bytemuck::cast_slice(&v[offset..end])); }
        if let Some(v) = flat { rt.queue.write_buffer(&flat_buf, 0, bytemuck::cast_slice(&v[offset..end])); }
        let flags = (bias.is_some() as u32)
            | ((dark.is_some() as u32) << 1)
            | ((flat.is_some() as u32) << 2);
        let p = CalParams { n: n as u32, flags, p0: 0, p1: 0, dark_scale, p2: 0.0, p3: 0.0, p4: 0.0 };
        rt.queue.write_buffer(&params, 0, bytemuck::bytes_of(&p));
        let mut enc = rt.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("zas-deepsky-calibrate-encoder"),
        });
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("zas-deepsky-calibrate-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pipes.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups((n as u32).div_ceil(256), 1, 1);
        }
        rt.queue.submit(Some(enc.finish()));
        let out = read_f32(rt, &light_buf, n)?;
        light[offset..end].copy_from_slice(&out);
        if crate::gpu_stack::take_gpu_error() {
            return Err("Device loss/OOM durante calibración".into());
        }
        offset = end;
    }
    Ok(bytes.saturating_mul(5))
}

#[allow(clippy::too_many_arguments)]
pub fn integrate_pass(
    cfg: &IntegrateConfig,
    frames: &[FrameMeta],
    local_fields: &[Option<Vec<f32>>],
    bounds: Option<(&[f32], &[f32])>,
    load: &dyn Fn(usize) -> Result<Vec<f32>, String>,
    cancel: &std::sync::atomic::AtomicBool,
    mut on_progress: impl FnMut(usize, usize, u64),
) -> Result<GpuPassResult, String> {
    if !matches!(cfg.channels, 1 | 3) || cfg.width < 8 || cfg.height < 8 {
        return Err("Geometría GPU de cielo profundo no soportada".into());
    }
    if frames.is_empty() || local_fields.len() != frames.len() {
        return Err("Metadatos de frames GPU incompletos".into());
    }
    let rt = crate::gpu_stack::gpu_runtime().ok_or("No hay runtime wgpu")?;
    let pipes = pipelines(rt);

    // Encuentra la mayor banda que cumple simultáneamente presupuesto total y
    // límite por binding. Se recalcula por y porque una rotación cambia el bbox.
    let mut final_mean = vec![0.0f32; cfg.width * cfg.height * cfg.channels];
    let mut final_m2 = vec![0.0f32; final_mean.len()];
    let mut final_weight = vec![0.0f32; cfg.width * cfg.height * cfg.channels];
    let mut final_rejected_low = vec![0.0f32; cfg.width * cfg.height];
    let mut final_rejected_high = vec![0.0f32; cfg.width * cfg.height];
    let total = frames.len();
    let mut y0 = 0usize;
    let mut tiles = 0usize;
    let mut peak = 0u64;

    while y0 < cfg.height {
        crate::pipeline::cancellation_checkpoint(cancel, "integración GPU tiled")?;
        let mut rows = cfg.height - y0;
        let (crops, frame_capacity, bytes_needed) = loop {
            let y1 = (y0 + rows).min(cfg.height);
            let crops: Vec<Crop> = frames.iter().map(|&m| crop_for(cfg, m, y0, y1)).collect();
            let frame_capacity = crops.iter().map(|c| c.w * c.h * cfg.channels).max().unwrap_or(1);
            let out_elems = cfg.width * rows * cfg.channels;
            let out_px = cfg.width * rows;
            let bindings_ok = (out_elems * 4) as u64 <= rt.max_binding
                && (frame_capacity * 4) as u64 <= rt.max_binding;
            // mean + M2 + lower + upper + weight + rechazo bajo/alto + fuente + grids/LUT.
            let bytes = (out_elems * 16 + out_px * 12 + frame_capacity * 4
                + cfg.local_grid_size * cfg.local_grid_size * 4 + 32 * 1024) as u64;
            if bindings_ok && bytes <= rt.vram_budget { break (crops, frame_capacity, bytes); }
            if rows == 1 {
                return Err(format!(
                    "La banda mínima GPU requiere {} MB; presupuesto {} MB / binding {} MB",
                    bytes / (1024 * 1024), rt.vram_budget / (1024 * 1024), rt.max_binding / (1024 * 1024)
                ));
            }
            rows = (rows / 2).max(1);
        };
        peak = peak.max(bytes_needed);
        let out_elems = cfg.width * rows * cfg.channels;
        let out_px = cfg.width * rows;
        let mk = |label: &'static str, size: usize, usage: wgpu::BufferUsages| {
            rt.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (size.max(1) * 4) as u64,
                usage,
                mapped_at_creation: false,
            })
        };
        let params_buf = rt.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zas-deepsky-params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let frame_buf = mk("zas-deepsky-frame-crop", frame_capacity, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST);
        let local_buf = mk("zas-deepsky-local-grid", cfg.local_grid_size * cfg.local_grid_size, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST);
        let lower_buf = mk("zas-deepsky-lower", out_elems, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST);
        let upper_buf = mk("zas-deepsky-upper", out_elems, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST);
        let rw = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC;
        let mean_buf = mk("zas-deepsky-mean", out_elems, rw);
        let m2_buf = mk("zas-deepsky-m2", out_elems, rw);
        // Weight is now per-channel (per-channel rejection), same size as mean/m2.
        let weight_buf = mk("zas-deepsky-weight", out_elems, rw);
        let rejected_low_buf = mk("zas-deepsky-rejected-low", out_px, rw);
        let rejected_high_buf = mk("zas-deepsky-rejected-high", out_px, rw);

        if let Some((lo, hi)) = bounds {
            let off = y0 * cfg.width * cfg.channels;
            rt.queue.write_buffer(&lower_buf, 0, bytemuck::cast_slice(&lo[off..off + out_elems]));
            rt.queue.write_buffer(&upper_buf, 0, bytemuck::cast_slice(&hi[off..off + out_elems]));
        }
        let bind = rt.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("zas-deepsky-integrate-bind"),
            layout: &pipes.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: params_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: frame_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: local_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: lower_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: upper_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 5, resource: mean_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 6, resource: m2_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 7, resource: weight_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 8, resource: pipes.lut.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 9, resource: rejected_low_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 10, resource: rejected_high_buf.as_entire_binding() },
            ],
        });

        // Single-threaded prefetch (P2.A): load the NEXT frame's pixels on the CPU
        // while the GPU drains the current frame's queued dispatches. The per-band
        // poll is non-blocking (Maintain::Poll) so the CPU runs ahead; prefetching
        // keeps the GPU queue fuller without threading the `load` closure.
        let mut prefetched: Option<Vec<f32>> = if frames.is_empty() {
            None
        } else {
            Some(load(frames[0].index)?)
        };
        for (k, (&meta, crop)) in frames.iter().zip(crops.iter()).enumerate() {
            crate::pipeline::cancellation_checkpoint(cancel, "integración GPU tiled")?;
            let full = prefetched.take().ok_or("prefetch de integración vacío")?;
            if full.len() != cfg.width * cfg.height * cfg.channels {
                return Err(format!("Frame {} cambió de geometría", meta.index));
            }
            let cropped = copy_crop(&full, cfg, *crop);
            rt.queue.write_buffer(&frame_buf, 0, bytemuck::cast_slice(&cropped));
            let has_local = local_fields[k].as_ref().is_some_and(|f| !f.is_empty());
            if let Some(field) = local_fields[k].as_ref() {
                rt.queue.write_buffer(&local_buf, 0, bytemuck::cast_slice(field));
            }
            let mut flags = 0u32;
            if cfg.lanczos { flags |= 1; }
            if bounds.is_some() { flags |= 2; }
            if has_local { flags |= 4; }
            if cfg.track_m2 { flags |= 8; }
            if meta.transform.local_distortion { flags |= 16; }
            let m = meta.transform.inverse_h;
            let q = meta.transform.poly;
            let p = Params {
                src_w: crop.w as u32, src_h: crop.h as u32,
                out_w: cfg.width as u32, tile_rows: rows as u32,
                tile_y0: y0 as u32, channels: cfg.channels as u32,
                flags, ln_g: cfg.local_grid_size as u32,
                full_w: cfg.width as u32, full_h: cfg.height as u32,
                src_x0: crop.x as u32, src_y0: crop.y as u32,
                out_h: cfg.height as u32, p0: 0, p1: 0, p2: 0,
                m0: m[0], m1: m[1], m2: m[2], m3: m[3],
                m4: m[4], m5: m[5], m6: m[6], m7: m[7], m8: m[8],
                inv_scale: 1.0 / cfg.scale.max(1.0),
                norm_mul: meta.norm.0[0], norm_add: meta.norm.1[0],
                frame_weight: meta.weight,
                norm_cx: meta.transform.norm[0], norm_cy: meta.transform.norm[1],
                norm_scale: meta.transform.norm[2],
                q0: q[0], q1: q[1], q2: q[2], q3: q[3],
                q4: q[4], q5: q[5], q6: q[6], q7: q[7],
                q8: q[8], q9: q[9], q10: q[10], q11: q[11],
                norm_mul1: meta.norm.0[1], norm_add1: meta.norm.1[1],
                norm_mul2: meta.norm.0[2], norm_add2: meta.norm.1[2],
            };
            // Band each frame's tile dispatch under ~2M px so no single compute
            // pass runs long enough to trip the Metal/Windows GPU watchdog on a
            // large tile or a slow iGPU (the planetary accumulator does the same).
            // Bands are disjoint output rows → numerically identical to one pass.
            const MAX_BAND_PX: usize = 2_000_000;
            let band_rows = (MAX_BAND_PX / cfg.width.max(1)).clamp(1, rows);
            let mut by = 0usize;
            while by < rows {
                let this = band_rows.min(rows - by);
                let mut pb = p;
                pb.p0 = by as u32; // band_y0
                pb.p1 = (by + this) as u32; // band_y1
                rt.queue.write_buffer(&params_buf, 0, bytemuck::bytes_of(&pb));
                let mut enc = rt.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("zas-deepsky-integrate-encoder"),
                });
                {
                    let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                        label: Some("zas-deepsky-integrate-pass"),
                        timestamp_writes: None,
                    });
                    pass.set_pipeline(&pipes.pipeline);
                    pass.set_bind_group(0, &bind, &[]);
                    pass.dispatch_workgroups((cfg.width as u32).div_ceil(8), (this as u32).div_ceil(8), 1);
                }
                rt.queue.submit(Some(enc.finish()));
                rt.device.poll(wgpu::Maintain::Poll);
                if crate::gpu_stack::take_gpu_error() {
                    return Err("Device loss/OOM durante integración de cielo profundo".into());
                }
                by += this;
            }
            // Prefetch the next frame's pixels while this frame's GPU work drains.
            if k + 1 < frames.len() {
                prefetched = Some(load(frames[k + 1].index)?);
            }
            on_progress(k + 1, total, peak);
        }

        let tile_mean = read_f32(rt, &mean_buf, out_elems)?;
        let tile_m2 = read_f32(rt, &m2_buf, out_elems)?;
        let tile_weight = read_f32(rt, &weight_buf, out_elems)?;
        let tile_rejected_low = read_f32(rt, &rejected_low_buf, out_px)?;
        let tile_rejected_high = read_f32(rt, &rejected_high_buf, out_px)?;
        let eo = y0 * cfg.width * cfg.channels;
        final_mean[eo..eo + out_elems].copy_from_slice(&tile_mean);
        final_m2[eo..eo + out_elems].copy_from_slice(&tile_m2);
        final_weight[eo..eo + out_elems].copy_from_slice(&tile_weight);
        let wo = y0 * cfg.width;
        final_rejected_low[wo..wo + out_px].copy_from_slice(&tile_rejected_low);
        final_rejected_high[wo..wo + out_px].copy_from_slice(&tile_rejected_high);
        y0 += rows;
        tiles += 1;
    }
    Ok(GpuPassResult {
        mean: final_mean,
        moment2: final_m2,
        weight: final_weight,
        rejected_low: final_rejected_low,
        rejected_high: final_rejected_high,
        peak_vram_bytes: peak,
        tiles,
    })
}

/// Mini-stack sintético que valida la ruta real (warp, normalización y media
/// ponderada). Se ejecuta una sola vez por sesión antes de aceptar Hybrid/GPU.
pub fn ensure_parity() -> bool {
    match DS_PARITY.load(Ordering::Acquire) {
        PARITY_OK => return true,
        PARITY_FAILED => return false,
        _ => {}
    }
    // Calibración: negativos válidos, dark scale y flat division deben seguir
    // la fórmula CPU con error sub-ADU.
    let mut cal_light = vec![120.0f32, 800.0, 12_000.0, 65_000.0, 50.0];
    let bias = vec![100.0f32, 100.0, 120.0, 90.0, 80.0];
    let dark = vec![30.0f32, 50.0, 80.0, 25.0, 10.0];
    let flat = vec![0.8f32, 1.1, 0.95, 1.02, 0.7];
    let expected: Vec<f32> = cal_light
        .iter()
        .zip(&bias)
        .zip(&dark)
        .zip(&flat)
        .map(|(((&l, &b), &d), &f)| (l - b - 1.25 * d) / f.max(0.05))
        .collect();
    let cal_ok = calibrate(&mut cal_light, Some(&bias), Some(&dark), Some(&flat), 1.25)
        .ok()
        .is_some_and(|_| cal_light.iter().zip(expected.iter()).all(|(a, b)| (*a - *b).abs() <= 0.05));
    if !cal_ok {
        DS_PARITY.store(PARITY_FAILED, Ordering::Release);
        return false;
    }

    let w = 48usize;
    let h = 32usize;
    let frames: Vec<Vec<f32>> = (0..3)
        .map(|k| (0..w * h).map(|i| ((i * 17 + k * 31) % 60000) as f32 + 100.0).collect())
        .collect();
    let metas = vec![
        FrameMeta { index: 0, transform: WarpTransform::from_similarity((1.0, 0.0, 0.0, 0.0)), weight: 1.0, norm: ([1.0; 3], [0.0; 3]) },
        FrameMeta { index: 1, transform: WarpTransform::from_similarity((1.0, 0.0, 0.35, -0.2)), weight: 0.8, norm: ([0.98; 3], [12.0; 3]) },
        FrameMeta { index: 2, transform: WarpTransform::from_similarity((1.0, 0.0, -0.25, 0.3)), weight: 0.65, norm: ([1.02; 3], [-9.0; 3]) },
    ];
    let cfg = IntegrateConfig { width: w, height: h, channels: 1, scale: 1.0, lanczos: false, local_grid_size: 1, track_m2: true };
    let local = vec![None, None, None];
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let gpu = integrate_pass(&cfg, &metas, &local, None, &|i| Ok(frames[i].clone()), &cancel, |_, _, _| {});
    let ok = gpu.ok().is_some_and(|got| {
        let mut cpu = vec![0.0f64; w * h];
        let mut wt = vec![0.0f64; w * h];
        for m in &metas {
            let src = &frames[m.index];
            for y in 0..h { for x in 0..w {
                let Some((sx, sy)) = m.transform.inverse(x as f32, y as f32) else { continue; };
                if sx < 0.0 || sy < 0.0 || sx >= (w - 1) as f32 || sy >= (h - 1) as f32 { continue; }
                let x0 = sx.floor() as usize;
                let y0 = sy.floor() as usize;
                let fx = sx - x0 as f32;
                let fy = sy - y0 as f32;
                let v = src[y0 * w + x0] * (1.0 - fx) * (1.0 - fy)
                    + src[y0 * w + x0 + 1] * fx * (1.0 - fy)
                    + src[(y0 + 1) * w + x0] * (1.0 - fx) * fy
                    + src[(y0 + 1) * w + x0 + 1] * fx * fy;
                let v = v * m.norm.0[0] + m.norm.1[0];
                cpu[y * w + x] += v as f64 * m.weight as f64;
                wt[y * w + x] += m.weight as f64;
            }}
        }
        let mut se = 0.0f64;
        let mut n = 0usize;
        let mut reference_flux = 0.0f64;
        let mut candidate_flux = 0.0f64;
        for i in 0..cpu.len() {
            if wt[i] > 0.0 {
                let reference = cpu[i] / wt[i];
                let candidate = got.mean[i] as f64;
                let e = candidate - reference;
                se += e * e;
                reference_flux += reference;
                candidate_flux += candidate;
                n += 1;
            }
        }
        let rmse = (se / n.max(1) as f64).sqrt();
        let photometric_error = (candidate_flux - reference_flux).abs()
            / reference_flux.abs().max(1e-9);
        if rmse > 0.5 || photometric_error > 0.001 {
            eprintln!(
                "[gpu deep-sky parity] RMSE={rmse:.6} ADU, error fotométrico={:.6}%",
                photometric_error * 100.0
            );
        }
        n > 0 && rmse <= 0.5 && photometric_error <= 0.001
    });
    DS_PARITY.store(if ok { PARITY_OK } else { PARITY_FAILED }, Ordering::Release);
    ok
}

/// Runtime gate for the projective and local-distortion branches of the warp
/// kernel. The basic parity test exercises similarity only; accepting it as
/// proof for the extra homography/polynomial paths could let a backend-specific
/// compiler error escape into Auto/Hybrid results.
pub fn ensure_advanced_warp_parity() -> bool {
    match DS_ADVANCED_WARP_PARITY.load(Ordering::Acquire) {
        PARITY_OK => return true,
        PARITY_FAILED => return false,
        _ => {}
    }
    let (w, h) = (48usize, 36usize);
    let frame: Vec<f32> = (0..w * h)
        .map(|i| (i % w) as f32 * 7.0 + (i / w) as f32 * 13.0 + 100.0)
        .collect();
    let projective = WarpTransform {
        inverse_h: [1.002, 0.011, -1.3, -0.007, 0.997, 0.8, 0.00008, -0.00005, 1.0],
        poly: [0.0; 12],
        norm: [0.0, 0.0, 1.0],
        local_distortion: false,
    };
    let local = WarpTransform {
        inverse_h: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
        poly: [24.0, 0.0, 24.0, 0.7, -0.25, 0.2, 0.0, 24.0, 18.0, -0.15, 0.3, -0.6],
        norm: [24.0, 18.0, 24.0],
        local_distortion: true,
    };
    let cfg = IntegrateConfig {
        width: w,
        height: h,
        channels: 1,
        scale: 1.0,
        lanczos: false,
        local_grid_size: 1,
        track_m2: true,
    };
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let mut all_ok = true;
    for transform in [projective, local] {
        let meta = FrameMeta {
            index: 0,
            transform,
            weight: 1.0,
            norm: ([1.0; 3], [0.0; 3]),
        };
        let got = integrate_pass(
            &cfg,
            &[meta],
            &[None],
            None,
            &|_| Ok(frame.clone()),
            &cancel,
            |_, _, _| {},
        );
        let Some(got) = got.ok() else {
            all_ok = false;
            break;
        };
        let mut squared_error = 0.0f64;
        let mut compared = 0usize;
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let Some((sx, sy)) = transform.inverse(x as f32, y as f32) else {
                    continue;
                };
                if sx < 0.0 || sy < 0.0 || sx >= (w - 1) as f32 || sy >= (h - 1) as f32 {
                    continue;
                }
                let (x0, y0) = (sx.floor() as usize, sy.floor() as usize);
                let (fx, fy) = (sx - x0 as f32, sy - y0 as f32);
                let cpu = frame[y0 * w + x0] * (1.0 - fx) * (1.0 - fy)
                    + frame[y0 * w + x0 + 1] * fx * (1.0 - fy)
                    + frame[(y0 + 1) * w + x0] * (1.0 - fx) * fy
                    + frame[(y0 + 1) * w + x0 + 1] * fx * fy;
                if got.weight[i] > 0.0 {
                    squared_error += (got.mean[i] - cpu).powi(2) as f64;
                    compared += 1;
                }
            }
        }
        let rmse = (squared_error / compared.max(1) as f64).sqrt();
        if compared <= w * h / 2 || rmse > 0.5 {
            eprintln!(
                "[gpu deep-sky advanced warp parity] samples={compared}, RMSE={rmse:.6} ADU"
            );
            all_ok = false;
            break;
        }
    }
    DS_ADVANCED_WARP_PARITY.store(
        if all_ok { PARITY_OK } else { PARITY_FAILED },
        Ordering::Release,
    );
    all_ok
}

pub fn ensure_pixel_preprocess_parity() -> bool {
    match DS_PIXEL_PARITY.load(Ordering::Acquire) {
        PARITY_OK => return true,
        PARITY_FAILED => return false,
        _ => {}
    }
    let (w, h) = (64usize, 48usize);
    let cfa: Vec<f32> = (0..w * h)
        .map(|i| ((i * 37 + (i / w) * 101) % 68000) as f32 - 1200.0)
        .collect();
    let cpu_debayer = crate::ds_debayer_image(
        crate::DsImage { data: cfa.clone(), w, h, ch: 1, bayer: Some(8) },
        8,
    );
    let debayer_ok = debayer_float32(&cfa, w, h, 8).ok().is_some_and(|gpu| {
        cpu_debayer.data.iter().zip(&gpu).all(|(a, b)| (a - b).abs() <= 0.01)
    });

    let mut raw: Vec<f32> = (0..w * h)
        .map(|i| 1000.0 + ((i * 13 + i / w * 5) % 29) as f32)
        .collect();
    raw[17 * w + 22] = 62000.0;
    raw[31 * w + 49] = -5000.0;
    let mut cpu_cosmetic = crate::DsImage { data: raw.clone(), w, h, ch: 1, bayer: None };
    let (med, noise) = crate::ds_cosmetic_stats(&cpu_cosmetic);
    crate::ds_cosmetic_hot_pixels_with_stats(&mut cpu_cosmetic, &med, &noise);
    let cosmetic_ok = cosmetic_hot_pixels(&mut raw, w, h, 1, false, &med, &noise)
        .is_ok_and(|_| raw == cpu_cosmetic.data);

    let mut scene = vec![800.0f32; w * h];
    for &(cx, cy, amp) in &[(15.3, 13.7, 18000.0), (34.2, 20.4, 22000.0), (49.1, 35.8, 20000.0)] {
        for y in 0..h {
            for x in 0..w {
                let r2 = (x as f32 - cx).powi(2) + (y as f32 - cy).powi(2);
                scene[y * w + x] += amp * (-r2 / 4.5).exp();
            }
        }
    }
    let cpu_stars = crate::ds_detect_stars_impl(&scene, w, h, 120, false);
    let gpu_stars = crate::ds_detect_stars_impl(&scene, w, h, 120, true);
    let stars_ok = cpu_stars.ok().zip(gpu_stars.ok()).is_some_and(|(cpu, gpu)| {
        cpu.len() == gpu.len() && cpu.iter().zip(gpu).all(|(a, b)| {
            (a.0 - b.0).abs() <= 0.01
                && (a.1 - b.1).abs() <= 0.01
                && (a.2 - b.2).abs() <= a.2.abs().max(1.0) * 0.001
        })
    });
    let ok = debayer_ok && cosmetic_ok && stars_ok;
    DS_PIXEL_PARITY.store(if ok { PARITY_OK } else { PARITY_FAILED }, Ordering::Release);
    ok
}

pub fn ensure_tiled_parity() -> bool {
    match DS_TILED_PARITY.load(Ordering::Acquire) {
        PARITY_OK => return true,
        PARITY_FAILED => return false,
        _ => {}
    }
    let (pixels, channels, frames) = (7usize, 2usize, 11usize);
    let weights: Vec<f32> = (0..frames).map(|k| 0.55 + k as f32 * 0.07).collect();
    let mut stack = vec![0.0f32; pixels * channels * frames];
    for p in 0..pixels {
        for c in 0..channels {
            for k in 0..frames {
                let i = (p * channels + c) * frames + k;
                stack[i] = 900.0 + p as f32 * 31.0 + c as f32 * 17.0
                    + (k as f32 - 5.0) * 2.5;
            }
            stack[(p * channels + c) * frames + (p + 2) % frames] += 900.0;
            stack[(p * channels + c) * frames + (p + 7) % frames] -= 420.0;
        }
    }
    stack[frames * channels + 1] = f32::NAN;

    let mut ok = true;
    for method in ["winsorized", "linearfit"] {
        let got = match reject_tiled_pass(
            &stack,
            &weights,
            pixels,
            channels,
            method,
            3.0,
            2.5,
            4.0,
        ) {
            Ok(v) => v,
            Err(_) => {
                ok = false;
                break;
            }
        };
        let mut max_data = 0.0f32;
        let mut max_cov = 0.0f64;
        let mut max_rej = 0.0f64;
        for p in 0..pixels {
            for c in 0..channels {
                let base = (p * channels + c) * frames;
                let mut samples: Vec<(f32, f64)> = (0..frames)
                    .filter_map(|k| {
                        let v = stack[base + k];
                        v.is_finite().then_some((v, weights[k] as f64))
                    })
                    .collect();
                let (mut cov, mut lo, mut hi) = (0.0, 0.0, 0.0);
                let expected = crate::ds_reject_pixel(
                    &mut samples,
                    method,
                    3.0,
                    2.5,
                    &mut cov,
                    &mut lo,
                    &mut hi,
                    4.0,
                );
                max_data = max_data.max((got.data[p * channels + c] - expected).abs());
                if (got.data[p * channels + c] - expected).abs() > 0.5 {
                    eprintln!(
                        "[gpu tiled mismatch] {method} p={p} c={c} gpu={} cpu={} gpu_cov={} cpu_cov={} low={} high={}",
                        got.data[p * channels + c],
                        expected,
                        got.coverage[p],
                        cov,
                        got.rejected_low[p],
                        got.rejected_high[p],
                    );
                    ok = false;
                }
                if c == 0 {
                    let rejected_gpu = got.rejected_low[p] + got.rejected_high[p];
                    max_cov = max_cov.max((got.coverage[p] as f64 - cov).abs());
                    max_rej = max_rej.max((rejected_gpu as f64 - (lo + hi)).abs());
                    if (got.coverage[p] as f64 - cov).abs() > 0.002
                        || (rejected_gpu as f64 - (lo + hi)).abs() > 0.002
                    {
                        ok = false;
                    }
                }
            }
        }
        if max_data > 0.5 || max_cov > 0.002 || max_rej > 0.002 {
            eprintln!("[gpu tiled parity] {method}: data={max_data} coverage={max_cov} rejection={max_rej}");
        }
    }
    DS_TILED_PARITY.store(if ok { PARITY_OK } else { PARITY_FAILED }, Ordering::Release);
    ok
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "requiere GPU física Metal/DX12/Vulkan"]
    fn deepsky_gpu_parity_physical() {
        assert!(super::ensure_parity());
    }

    #[test]
    #[ignore = "requiere GPU física; obligatorio en la matriz de release"]
    fn deepsky_gpu_advanced_warp_parity_physical() {
        assert!(super::ensure_advanced_warp_parity());
    }

    #[test]
    #[ignore = "requiere GPU física; cosmética/debayer/mapa estelar"]
    fn deepsky_gpu_pixel_preprocess_parity_physical() {
        assert!(super::ensure_pixel_preprocess_parity());
        let (w, h) = (96usize, 72usize);

        // Debayer float32: conserva negativos/headroom y coincide con CPU.
        let cfa: Vec<f32> = (0..w * h)
            .map(|i| ((i * 37 + (i / w) * 101) % 70000) as f32 - 1500.0)
            .collect();
        for cid in 8..=11 {
            let cpu = crate::ds_debayer_image(
                crate::DsImage { data: cfa.clone(), w, h, ch: 1, bayer: Some(cid) },
                cid,
            );
            let gpu = super::debayer_float32(&cfa, w, h, cid).expect("debayer GPU");
            let max = cpu.data.iter().zip(&gpu).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            assert!(max <= 0.01, "debayer cid={cid}, max diff={max}");
        }

        // Cosmética: mismos parámetros MAD y reemplazos exactos.
        let mut base: Vec<f32> = (0..w * h)
            .map(|i| 1200.0 + ((i * 19 + i / w * 7) % 31) as f32)
            .collect();
        base[22 * w + 31] = 62000.0;
        base[45 * w + 67] = -4000.0;
        let mut cpu_img = crate::DsImage { data: base.clone(), w, h, ch: 1, bayer: None };
        let (med, noise) = crate::ds_cosmetic_stats(&cpu_img);
        crate::ds_cosmetic_hot_pixels_with_stats(&mut cpu_img, &med, &noise);
        let mut gpu = base;
        super::cosmetic_hot_pixels(&mut gpu, w, h, 1, false, &med, &noise).expect("cosmética GPU");
        assert_eq!(cpu_img.data, gpu);

        // Mapa denso GPU + centroides CPU debe entregar el mismo catálogo.
        let mut stars_img = vec![850.0f32; w * h];
        for &(cx, cy, amp) in &[(18.3, 17.7, 16000.0), (43.2, 25.4, 22000.0), (72.1, 19.8, 18000.0), (30.5, 53.1, 24000.0), (69.6, 51.9, 21000.0)] {
            for y in 0..h {
                for x in 0..w {
                    let r2 = (x as f32 - cx).powi(2) + (y as f32 - cy).powi(2);
                    stars_img[y * w + x] += amp * (-r2 / 4.5).exp();
                }
            }
        }
        let cpu = crate::ds_detect_stars_impl(&stars_img, w, h, 120, false).unwrap();
        let gpu = crate::ds_detect_stars_impl(&stars_img, w, h, 120, true).unwrap();
        assert_eq!(cpu.len(), gpu.len());
        for (a, b) in cpu.iter().zip(&gpu) {
            assert!((a.0 - b.0).abs() <= 0.01 && (a.1 - b.1).abs() <= 0.01);
            assert!((a.2 - b.2).abs() <= a.2.abs().max(1.0) * 0.001);
        }
    }

    #[test]
    #[ignore = "requiere GPU física; obligatorio en la matriz de release"]
    fn deepsky_gpu_tiled_rejection_parity_physical() {
        assert!(super::ensure_tiled_parity());
    }
}
