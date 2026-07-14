//! Preprocesado GPU del análisis planetario.
//!
//! Convierte la imagen mono ya interpretada por `FrameSource` en la pirámide
//! 2×, blur binomial y Laplaciano usados por SAD/calidad. Un mutex conserva los
//! buffers y serializa únicamente los submits; mientras la GPU procesa N, los
//! workers Rayon siguen con SAD/CoG/decisiones de frames anteriores.

use std::sync::atomic::{AtomicU8, Ordering};

static PARITY: AtomicU8 = AtomicU8::new(0); // 0 pending, 1 ok, 2 failed

const WGSL: &str = r#"
struct Params {
    w: u32, h: u32, hw: u32, hh: u32,
    threshold0: f32, threshold1: f32, threshold2: f32, _fp0: f32,
    grid_size: u32, analytics_flags: u32, _up0: u32, _up1: u32,
}
@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> src: array<u32>;
@group(0) @binding(2) var<storage, read_write> half: array<u32>;
@group(0) @binding(3) var<storage, read_write> temp: array<u32>;
@group(0) @binding(4) var<storage, read_write> blur: array<u32>;
@group(0) @binding(5) var<storage, read_write> lap: array<u32>;
@group(0) @binding(6) var<storage, read_write> cog_proj_x: array<f32>;
@group(0) @binding(7) var<storage, read_write> cog_proj_y: array<f32>;
@group(0) @binding(8) var<storage, read_write> quality_grid: array<f32>;
@group(0) @binding(9) var<storage, read_write> score_rows: array<u32>;

@compute @workgroup_size(16, 16)
fn downscale(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= P.hw || gid.y >= P.hh) { return; }
    let x = gid.x * 2u;
    let y = gid.y * 2u;
    let i0 = y * P.w + x;
    let sum = src[i0] + src[i0 + 1u] + src[i0 + P.w] + src[i0 + P.w + 1u];
    half[gid.y * P.hw + gid.x] = sum >> 2u;
}

@compute @workgroup_size(16, 16)
fn blur_h(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x < 2u || gid.x + 2u >= P.hw || gid.y >= P.hh) { return; }
    let i = gid.y * P.hw + gid.x;
    temp[i] = (half[i - 2u] + 4u * half[i - 1u] + 6u * half[i]
        + 4u * half[i + 1u] + half[i + 2u]) >> 4u;
}

@compute @workgroup_size(16, 16)
fn blur_v(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x < 2u || gid.x + 2u >= P.hw || gid.y < 2u || gid.y + 2u >= P.hh) { return; }
    let i = gid.y * P.hw + gid.x;
    blur[i] = (temp[i - 2u * P.hw] + 4u * temp[i - P.hw] + 6u * temp[i]
        + 4u * temp[i + P.hw] + temp[i + 2u * P.hw]) >> 4u;
}

@compute @workgroup_size(16, 16)
fn laplacian(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x < 1u || gid.x + 1u >= P.hw || gid.y < 1u || gid.y + 1u >= P.hh) { return; }
    let i = gid.y * P.hw + gid.x;
    let sum = blur[i - 1u] + blur[i + 1u] + blur[i - P.hw] + blur[i + P.hw]
        + blur[i - P.hw - 1u] + blur[i - P.hw + 1u]
        + blur[i + P.hw - 1u] + blur[i + P.hw + 1u];
    lap[i] = u32(abs(i32(8u * blur[i]) - i32(sum)));
}

fn threshold_for(t: u32) -> f32 {
    if (t == 0u) { return P.threshold0; }
    if (t == 1u) { return P.threshold1; }
    return P.threshold2;
}

// Proyecciones exactas del CoG robusto: un invocation por columna/fila y por
// umbral. La CPU sólo recorre estas proyecciones pequeñas para descartar el
// 0.5% de masa de cada cola y validar el centro.
@compute @workgroup_size(64)
fn cog_x(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = gid.x;
    let t = gid.y;
    if (x >= P.w || t >= 3u) { return; }
    let threshold = threshold_for(t);
    var sum = 0.0;
    for (var y = 0u; y < P.h; y = y + 1u) {
        let v = f32(src[y * P.w + x]);
        if (v > threshold) {
            let d = v - threshold;
            sum = sum + d * d * d;
        }
    }
    cog_proj_x[t * P.w + x] = sum;
}

@compute @workgroup_size(64)
fn cog_y(@builtin(global_invocation_id) gid: vec3<u32>) {
    let y = gid.x;
    let t = gid.y;
    if (y >= P.h || t >= 3u) { return; }
    let threshold = threshold_for(t);
    var sum = 0.0;
    for (var x = 0u; x < P.w; x = x + 1u) {
        let v = f32(src[y * P.w + x]);
        if (v > threshold) {
            let d = v - threshold;
            sum = sum + d * d * d;
        }
    }
    cog_proj_y[t * P.h + y] = sum;
}

fn add_square64(v: u32, lo0: u32, hi0: u32) -> vec2<u32> {
    let a = v & 0xffffu;
    let b = v >> 16u;
    let p0 = a * a;
    let p1 = 2u * a * b;
    let shifted = p1 << 16u;
    let square_lo = p0 + shifted;
    let square_hi = b * b + (p1 >> 16u) + select(0u, 1u, square_lo < p0);
    let lo = lo0 + square_lo;
    let hi = hi0 + square_hi + select(0u, 1u, lo < lo0);
    return vec2<u32>(lo, hi);
}

// Una suma de 64 bits por fila evita atomics no portables y descarga sólo
// 2·height u32 para la métrica global, no el mapa completo de costes.
@compute @workgroup_size(64)
fn score_by_row(@builtin(global_invocation_id) gid: vec3<u32>) {
    let y = gid.x;
    if (y >= P.hh) { return; }
    var lo = 0u;
    var hi = 0u;
    for (var x = 0u; x < P.hw; x = x + 1u) {
        let v = lap[y * P.hw + x];
        if (v > 100u) {
            let s = add_square64(v, lo, hi);
            lo = s.x;
            hi = s.y;
        }
    }
    score_rows[y * 2u] = lo;
    score_rows[y * 2u + 1u] = hi;
}

// Un invocation por celda produce el mapa AP 40×40. La suma se conserva en
// f32 porque el mapa sólo se usa para ranking relativo; la ruta CPU valida las
// decisiones/shift y sigue disponible cuando no hay paridad.
@compute @workgroup_size(64)
fn grid_quality(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = gid.x;
    let g = P.grid_size;
    if (g == 0u || cell >= g * g) { return; }
    let gx = cell % g;
    let gy = cell / g;
    var score = 0.0;
    if ((P.analytics_flags & 1u) != 0u) {
        let step = select(2u, 4u, P.hw > 1200u || P.hh > 1200u);
        let xlo0 = (gx * P.hw) / g;
        let xhi0 = ((gx + 1u) * P.hw + g - 1u) / g + 1u;
        let ylo0 = (gy * P.hh) / g;
        let yhi0 = ((gy + 1u) * P.hh + g - 1u) / g + 1u;
        let xlo = max(2u, xlo0);
        let xhi = min(P.hw - 2u, xhi0);
        let ylo = max(2u, ylo0);
        let yhi = min(P.hh - 2u, yhi0);
        var y = 2u + ((max(ylo, 2u) - 2u + step - 1u) / step) * step;
        loop {
            if (y >= yhi) { break; }
            var x = 2u + ((max(xlo, 2u) - 2u + step - 1u) / step) * step;
            loop {
                if (x >= xhi) { break; }
                let mapped_x = min((x * g) / max(P.hw, 1u), g - 1u);
                let mapped_y = min((y * g) / max(P.hh, 1u), g - 1u);
                if (mapped_x != gx || mapped_y != gy) {
                    x = x + step;
                    continue;
                }
                let i = y * P.hw + x;
                let n = blur[i - 1u] + blur[i + 1u] + blur[i - P.hw]
                    + blur[i + P.hw] + blur[i - P.hw - 1u]
                    + blur[i - P.hw + 1u] + blur[i + P.hw - 1u]
                    + blur[i + P.hw + 1u];
                let lv = abs(i32(8u * blur[i]) - i32(n));
                if (lv > 100) { score = score + f32(lv) * f32(lv); }
                x = x + step;
            }
            y = y + step;
        }
    } else {
        let tw = max(P.hw / g, 1u);
        let th = max(P.hh / g, 1u);
        let x0 = gx * tw;
        let y0 = gy * th;
        let x1 = min(x0 + tw, P.hw - 2u);
        let y1 = min(y0 + th, P.hh - 2u);
        var y = max(y0, 2u);
        loop {
            if (y >= y1) { break; }
            var x = max(x0, 2u);
            loop {
                if (x >= x1) { break; }
                let i = y * P.hw + x;
                let lv = abs(i32(4u * half[i]) - i32(half[i - 1u] + half[i + 1u]
                    + half[i - P.hw] + half[i + P.hw]));
                if (lv > 2000) { score = score + f32(lv) * f32(lv); }
                x = x + 2u;
            }
            y = y + 2u;
        }
    }
    quality_grid[cell] = score;
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    w: u32,
    h: u32,
    hw: u32,
    hh: u32,
    threshold0: f32,
    threshold1: f32,
    threshold2: f32,
    fp0: f32,
    grid_size: u32,
    analytics_flags: u32,
    up0: u32,
    up1: u32,
}

struct Pipelines {
    downscale: wgpu::ComputePipeline,
    blur_h: wgpu::ComputePipeline,
    blur_v: wgpu::ComputePipeline,
    lap: wgpu::ComputePipeline,
    cog_x: wgpu::ComputePipeline,
    cog_y: wgpu::ComputePipeline,
    score_rows: wgpu::ComputePipeline,
    grid: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

static PIPELINES: std::sync::OnceLock<Pipelines> = std::sync::OnceLock::new();

fn pipelines(rt: &'static crate::gpu_stack::GpuRuntime) -> &'static Pipelines {
    PIPELINES.get_or_init(|| {
        let module = rt.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("zas-analysis-preprocess-wgsl"),
            source: wgpu::ShaderSource::Wgsl(WGSL.into()),
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
            label: Some("zas-analysis-preprocess-layout"),
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
                storage(1, true), storage(2, false), storage(3, false), storage(4, false), storage(5, false),
                storage(6, false), storage(7, false), storage(8, false), storage(9, false),
            ],
        });
        let pl = rt.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("zas-analysis-preprocess-pipeline-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let make = |entry| rt.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(entry), layout: Some(&pl), module: &module, entry_point: Some(entry),
            compilation_options: Default::default(), cache: None,
        });
        Pipelines {
            downscale: make("downscale"),
            blur_h: make("blur_h"),
            blur_v: make("blur_v"),
            lap: make("laplacian"),
            cog_x: make("cog_x"),
            cog_y: make("cog_y"),
            score_rows: make("score_by_row"),
            grid: make("grid_quality"),
            layout,
        }
    })
}

struct Engine {
    w: usize,
    h: usize,
    params: wgpu::Buffer,
    src: wgpu::Buffer,
    half: wgpu::Buffer,
    temp: wgpu::Buffer,
    blur: wgpu::Buffer,
    lap: wgpu::Buffer,
    cog_x: wgpu::Buffer,
    cog_y: wgpu::Buffer,
    grid: wgpu::Buffer,
    score_rows: wgpu::Buffer,
    bind: wgpu::BindGroup,
}

fn engine_vram_bytes(w: usize, h: usize) -> u64 {
    let hw = w / 2;
    let hh = h / 2;
    let full_bytes = (w * h * 4) as u64;
    let half_bytes = (hw * hh * 4) as u64;
    let cog_x_bytes = (w * 3 * 4) as u64;
    let cog_y_bytes = (h * 3 * 4) as u64;
    let grid_bytes = (40 * 40 * 4) as u64;
    let score_bytes = (hh * 2 * 4) as u64;
    full_bytes + half_bytes * 4 + cog_x_bytes + cog_y_bytes + grid_bytes + score_bytes + 4096
}

impl Engine {
    fn new(rt: &'static crate::gpu_stack::GpuRuntime, w: usize, h: usize) -> Result<Self, String> {
        let hw = w / 2;
        let hh = h / 2;
        if hw < 8 || hh < 8 { return Err("ROI demasiado pequeña para análisis GPU".into()); }
        let full_bytes = (w * h * 4) as u64;
        let half_bytes = (hw * hh * 4) as u64;
        let cog_x_bytes = (w * 3 * 4) as u64;
        let cog_y_bytes = (h * 3 * 4) as u64;
        let grid_bytes = (40 * 40 * 4) as u64;
        let score_bytes = (hh * 2 * 4) as u64;
        let needed = engine_vram_bytes(w, h);
        if full_bytes > rt.max_binding || half_bytes > rt.max_binding || needed > rt.vram_budget {
            return Err(format!("Análisis GPU requiere {} MB y el presupuesto es {} MB", needed / 1_048_576, rt.vram_budget / 1_048_576));
        }
        let pp = pipelines(rt);
        let params = rt.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zas-analysis-params"), size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false,
        });
        let mk = |label, size, ro| rt.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label), size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST
                | if ro { wgpu::BufferUsages::empty() } else { wgpu::BufferUsages::COPY_SRC },
            mapped_at_creation: false,
        });
        let src = mk("zas-analysis-src", full_bytes, true);
        let half = mk("zas-analysis-half", half_bytes, false);
        let temp = mk("zas-analysis-temp", half_bytes, false);
        let blur = mk("zas-analysis-blur", half_bytes, false);
        let lap = mk("zas-analysis-lap", half_bytes, false);
        let cog_x = mk("zas-analysis-cog-x", cog_x_bytes, false);
        let cog_y = mk("zas-analysis-cog-y", cog_y_bytes, false);
        let grid = mk("zas-analysis-grid", grid_bytes, false);
        let score_rows = mk("zas-analysis-score-rows", score_bytes, false);
        let bind = rt.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("zas-analysis-bind"), layout: &pp.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: src.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: half.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: temp.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: blur.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 5, resource: lap.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 6, resource: cog_x.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 7, resource: cog_y.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 8, resource: grid.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 9, resource: score_rows.as_entire_binding() },
            ],
        });
        Ok(Self { w, h, params, src, half, temp, blur, lap, cog_x, cog_y, grid, score_rows, bind })
    }
}

static ENGINE: std::sync::OnceLock<std::sync::Mutex<Option<Engine>>> = std::sync::OnceLock::new();

#[derive(Clone, Copy)]
struct BatchReadbackLayout {
    half: usize,
    blur: usize,
    lap: usize,
    score: usize,
    cog_x: usize,
    cog_y: usize,
    grid: usize,
    stride: usize,
}

impl BatchReadbackLayout {
    fn new(w: usize, h: usize) -> Self {
        let n = (w / 2) * (h / 2);
        let half = 0;
        let blur = half + n;
        let lap = blur + n;
        let score = lap + n;
        let cog_x = score + (h / 2) * 2;
        let cog_y = cog_x + w * 3;
        let grid = cog_y + h * 3;
        let stride = grid + 40 * 40;
        Self { half, blur, lap, score, cog_x, cog_y, grid, stride }
    }
}

fn batch_vram_bytes(w: usize, h: usize, capacity: usize) -> u64 {
    let layout = BatchReadbackLayout::new(w, h);
    engine_vram_bytes(w, h)
        .saturating_mul(capacity as u64)
        .saturating_add((layout.stride * capacity * 4) as u64)
}

/// Working-set aproximado del motor multiframe, para telemetría visible. El
/// lote normal de tres frames usa cuatro slots por el `next_power_of_two` del
/// pool reutilizable.
pub fn estimated_batch_vram_mb(w: usize, h: usize, frames_per_batch: usize) -> u64 {
    let capacity = frames_per_batch.next_power_of_two().clamp(2, 8);
    batch_vram_bytes(w, h, capacity).div_ceil(1_048_576)
}

/// Tamaño de lote recomendado para el análisis multiframe. El pool reutilizable
/// redondea a next_power_of_two slots, así que los escalones útiles son 6 (8
/// slots), 4 (4 slots) y 2 (2 slots). GPUs con presupuesto amplio amortizan más
/// frames por submit/readback; iGPU modestas conservan el working-set mínimo.
/// Cualquier exceso real lo detiene igualmente validate_batch_vram (fallback
/// CPU por lote), esto sólo decide el punto de partida.
pub fn recommended_batch_len(w: usize, h: usize) -> usize {
    let Some(rt) = crate::gpu_stack::gpu_runtime() else {
        return 3;
    };
    let comfortable = rt.vram_budget.saturating_mul(35) / 100;
    for candidate in [6usize, 4] {
        let capacity = candidate.next_power_of_two().clamp(2, 8);
        if batch_vram_bytes(w, h, capacity) <= comfortable {
            return candidate;
        }
    }
    2
}

/// Validación aritmética (sin submits) de que el pool de lotes recomendado
/// cabe en el presupuesto de VRAM del adapter. Hybrid/GpuOnly deciden con
/// esto + paridad, sin microprueba cronometrada.
pub fn validate_recommended_batch(w: usize, h: usize) -> Result<u64, String> {
    let rt = crate::gpu_stack::gpu_runtime().ok_or("No hay runtime wgpu")?;
    let capacity = recommended_batch_len(w, h).next_power_of_two().clamp(2, 8);
    validate_batch_vram(w, h, capacity, rt.vram_budget)
}

fn validate_batch_vram(w: usize, h: usize, capacity: usize, budget: u64) -> Result<u64, String> {
    let needed = batch_vram_bytes(w, h, capacity);
    if needed > budget {
        Err(format!(
            "Análisis GPU por lotes requiere {} MB y el presupuesto es {} MB",
            needed / 1_048_576,
            budget / 1_048_576
        ))
    } else {
        Ok(needed)
    }
}

/// Triple-buffer reutilizable para análisis planetario. Cada slot posee sus
/// buffers de trabajo y bind group, pero todos los frames del lote se codifican
/// en un único command buffer y se descargan mediante un solo staging map.
/// Así se evita el coste dominante de submit/map por frame y el caller puede
/// preparar el lote N+1 mientras la GPU termina N.
struct BatchEngine {
    w: usize,
    h: usize,
    capacity: usize,
    slots: Vec<Engine>,
    staging: wgpu::Buffer,
    layout: BatchReadbackLayout,
}

impl BatchEngine {
    fn new(
        rt: &'static crate::gpu_stack::GpuRuntime,
        w: usize,
        h: usize,
        capacity: usize,
    ) -> Result<Self, String> {
        let capacity = capacity.clamp(2, 8);
        let layout = BatchReadbackLayout::new(w, h);
        let staging_bytes = (layout.stride * capacity * 4) as u64;
        validate_batch_vram(w, h, capacity, rt.vram_budget)?;
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(Engine::new(rt, w, h)?);
        }
        let staging = rt.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zas-analysis-batch-staging"),
            size: staging_bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Ok(Self { w, h, capacity, slots, staging, layout })
    }
}

static BATCH_ENGINE: std::sync::OnceLock<std::sync::Mutex<Option<BatchEngine>>> =
    std::sync::OnceLock::new();

fn read_u32(rt: &crate::gpu_stack::GpuRuntime, src: &wgpu::Buffer, n: usize) -> Result<Vec<u32>, String> {
    let bytes = (n * 4) as u64;
    let staging = rt.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("zas-analysis-readback"), size: bytes,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false,
    });
    let mut enc = rt.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("zas-analysis-readback-enc") });
    enc.copy_buffer_to_buffer(src, 0, &staging, 0, bytes);
    rt.queue.submit(Some(enc.finish()));
    let slice = staging.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| { let _ = tx.send(r); });
    crate::gpu_stack::wait_for_readback(&rt.device, &rx, "readback de análisis")?;
    let out = { let m = slice.get_mapped_range(); bytemuck::cast_slice::<u8, u32>(&m).to_vec() };
    staging.unmap();
    Ok(out)
}

pub struct AnalysisGpuOutput {
    pub half: Vec<u16>,
    pub blurred: Vec<u16>,
    pub laplacian: Vec<u16>,
    pub score: u64,
    pub geometric_center: Option<(f32, f32)>,
    pub grid_scores: Option<Vec<u64>>,
}

fn finish_analysis_output(
    half_raw: &[u32],
    blur_raw: &[u32],
    lap_raw: &[u32],
    score_raw: &[u32],
    cog_x_raw: Option<&[u32]>,
    cog_y_raw: Option<&[u32]>,
    grid_raw: Option<&[u32]>,
    w: usize,
    h: usize,
) -> AnalysisGpuOutput {
    let score = score_raw
        .chunks_exact(2)
        .map(|v| ((v[1] as u64) << 32) | v[0] as u64)
        .sum();
    let geometric_center = cog_x_raw.zip(cog_y_raw).map(|(px, py)| {
        let px: Vec<f32> = px.iter().map(|&v| f32::from_bits(v)).collect();
        let py: Vec<f32> = py.iter().map(|&v| f32::from_bits(v)).collect();
        center_from_projections(&px, &py, w, h)
    });
    let grid_scores = grid_raw.map(|raw| {
        raw.iter()
            .map(|&v| f32::from_bits(v).max(0.0).round() as u64)
            .collect()
    });
    let half: Vec<u16> = half_raw.iter().map(|&v| v as u16).collect();
    let blurred: Vec<u16> = blur_raw.iter().map(|&v| v as u16).collect();
    let mut laplacian: Vec<u16> = lap_raw.iter().map(|&v| v as u16).collect();
    let min = laplacian.iter().copied().min().unwrap_or(0);
    let max = laplacian.iter().copied().max().unwrap_or(0);
    if max > min {
        let scale = 60000.0 / (max - min) as f32;
        for v in &mut laplacian {
            if *v > 0 { *v = ((*v as f32 - min as f32) * scale) as u16; }
        }
    }
    AnalysisGpuOutput { half, blurred, laplacian, score, geometric_center, grid_scores }
}

// ---------------------------------------------------------------------------
// SAD por lotes. Cada invocation resuelve la búsqueda gruesa completa de un
// punto de alineación sobre los mapas 4× reducidos. La CPU recibe solamente
// (dx,dy,SAD) por AP y conserva el refinamiento subpíxel/LK y los filtros
// espaciales. De esta forma no se hace un submit por AP ni se descarga una
// superficie de costes de (2r+1)² elementos.
// ---------------------------------------------------------------------------

const SAD_WGSL: &str = r#"
struct SadParams { w: u32, h: u32, count: u32, _pad: u32 }
struct SadPoint {
    ref_x: i32, ref_y: i32, tgt_x: i32, tgt_y: i32,
    box_w: i32, box_h: i32, search_r: i32, enabled: u32,
}
struct SadResult { dx: i32, dy: i32, sad_lo: u32, sad_hi: u32 }

@group(0) @binding(0) var<uniform> P: SadParams;
@group(0) @binding(1) var<storage, read> reference: array<u32>;
@group(0) @binding(2) var<storage, read> tgt_image: array<u32>;
@group(0) @binding(3) var<storage, read> points: array<SadPoint>;
@group(0) @binding(4) var<storage, read_write> results: array<SadResult>;

fn less64(ahi: u32, alo: u32, bhi: u32, blo: u32) -> bool {
    return ahi < bhi || (ahi == bhi && alo < blo);
}

@compute @workgroup_size(64)
fn sad_points(@builtin(global_invocation_id) gid: vec3<u32>) {
    let pi = gid.x;
    if (pi >= P.count) { return; }
    let ap = points[pi];
    if (ap.enabled == 0u || ap.box_w <= 0 || ap.box_h <= 0) {
        results[pi] = SadResult(0, 0, 0xffffffffu, 0xffffffffu);
        return;
    }
    let half_w = ap.box_w / 2;
    let half_h = ap.box_h / 2;
    let rx0 = ap.ref_x - half_w;
    let ry0 = ap.ref_y - half_h;
    if (rx0 < 0 || ry0 < 0 || rx0 + ap.box_w >= i32(P.w)
        || ry0 + ap.box_h >= i32(P.h)) {
        results[pi] = SadResult(0, 0, 0xffffffffu, 0xffffffffu);
        return;
    }

    var best_lo = 0xffffffffu;
    var best_hi = 0xffffffffu;
    var best_dx = 0;
    var best_dy = 0;
    // El centro se evalúa primero, como la referencia SIMD, para disponer de
    // una cota fuerte. El orden posterior no cambia el mínimo exacto.
    for (var phase = 0; phase < 2; phase = phase + 1) {
        for (var dy = -ap.search_r; dy <= ap.search_r; dy = dy + 1) {
            for (var dx = -ap.search_r; dx <= ap.search_r; dx = dx + 1) {
                if ((phase == 0) != (dx == 0 && dy == 0)) { continue; }
                let tx0 = ap.tgt_x + dx - half_w;
                let ty0 = ap.tgt_y + dy - half_h;
                if (tx0 < 0 || ty0 < 0 || tx0 + ap.box_w >= i32(P.w)
                    || ty0 + ap.box_h >= i32(P.h)) { continue; }
                var lo = 0u;
                var hi = 0u;
                var pruned = false;
                for (var py = 0; py < ap.box_h; py = py + 1) {
                    let rr = u32(ry0 + py) * P.w;
                    let tr = u32(ty0 + py) * P.w;
                    for (var px = 0; px < ap.box_w; px = px + 1) {
                        let a = reference[rr + u32(rx0 + px)];
                        let b = tgt_image[tr + u32(tx0 + px)];
                        let d = select(b - a, a - b, a >= b);
                        let old = lo;
                        lo = lo + d;
                        if (lo < old) { hi = hi + 1u; }
                    }
                    if (!less64(hi, lo, best_hi, best_lo)) {
                        pruned = true;
                        break;
                    }
                }
                if (!pruned && less64(hi, lo, best_hi, best_lo)) {
                    best_lo = lo;
                    best_hi = hi;
                    best_dx = dx;
                    best_dy = dy;
                }
            }
        }
    }
    results[pi] = SadResult(best_dx, best_dy, best_lo, best_hi);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SadParams {
    w: u32,
    h: u32,
    count: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SadPoint {
    pub ref_x: i32,
    pub ref_y: i32,
    pub tgt_x: i32,
    pub tgt_y: i32,
    pub box_w: i32,
    pub box_h: i32,
    pub search_r: i32,
    pub enabled: u32,
}

impl SadPoint {
    pub fn new(
        ref_x: i32,
        ref_y: i32,
        tgt_x: i32,
        tgt_y: i32,
        box_size: i32,
        search_r: i32,
        enabled: bool,
    ) -> Self {
        Self {
            ref_x,
            ref_y,
            tgt_x,
            tgt_y,
            box_w: box_size,
            box_h: box_size,
            search_r,
            enabled: u32::from(enabled),
        }
    }

    pub fn rectangular(
        ref_x: i32,
        ref_y: i32,
        tgt_x: i32,
        tgt_y: i32,
        box_w: i32,
        box_h: i32,
        search_r: i32,
        enabled: bool,
    ) -> Self {
        Self {
            ref_x,
            ref_y,
            tgt_x,
            tgt_y,
            box_w,
            box_h,
            search_r,
            enabled: u32::from(enabled),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SadMatch {
    pub dx: i32,
    pub dy: i32,
    pub sad: u64,
}

struct SadPipelines {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

static SAD_PIPELINES: std::sync::OnceLock<SadPipelines> = std::sync::OnceLock::new();

fn sad_pipelines(rt: &'static crate::gpu_stack::GpuRuntime) -> &'static SadPipelines {
    SAD_PIPELINES.get_or_init(|| {
        let module = rt.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("zas-analysis-sad-wgsl"),
            source: wgpu::ShaderSource::Wgsl(SAD_WGSL.into()),
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
            label: Some("zas-analysis-sad-layout"),
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
                storage(1, true),
                storage(2, true),
                storage(3, true),
                storage(4, false),
            ],
        });
        let pl = rt.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("zas-analysis-sad-pipeline-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = rt.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("zas-analysis-sad-pipeline"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("sad_points"),
            compilation_options: Default::default(),
            cache: None,
        });
        SadPipelines { pipeline, layout }
    })
}

struct SadEngine {
    w: usize,
    h: usize,
    capacity: usize,
    params: wgpu::Buffer,
    reference: wgpu::Buffer,
    target: wgpu::Buffer,
    points: wgpu::Buffer,
    results: wgpu::Buffer,
    bind: wgpu::BindGroup,
}

impl SadEngine {
    fn new(
        rt: &'static crate::gpu_stack::GpuRuntime,
        w: usize,
        h: usize,
        capacity: usize,
    ) -> Result<Self, String> {
        let image_bytes = (w * h * 4) as u64;
        let point_bytes = (capacity.max(1) * std::mem::size_of::<SadPoint>()) as u64;
        let result_bytes = (capacity.max(1) * 16) as u64;
        let needed = image_bytes * 2 + point_bytes + result_bytes + 4096;
        if image_bytes > rt.max_binding
            || point_bytes > rt.max_binding
            || result_bytes > rt.max_binding
            || needed > rt.vram_budget
        {
            return Err(format!(
                "SAD GPU requiere {} MB y el presupuesto es {} MB",
                needed / 1_048_576,
                rt.vram_budget / 1_048_576
            ));
        }
        let params = rt.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zas-analysis-sad-params"),
            size: std::mem::size_of::<SadParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mk = |label, size, copy_src| rt.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | if copy_src { wgpu::BufferUsages::COPY_SRC } else { wgpu::BufferUsages::empty() },
            mapped_at_creation: false,
        });
        let reference = mk("zas-analysis-sad-reference", image_bytes, false);
        let target = mk("zas-analysis-sad-target", image_bytes, false);
        let points = mk("zas-analysis-sad-points", point_bytes, false);
        let results = mk("zas-analysis-sad-results", result_bytes, true);
        let pp = sad_pipelines(rt);
        let bind = rt.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("zas-analysis-sad-bind"),
            layout: &pp.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: reference.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: target.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: points.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: results.as_entire_binding() },
            ],
        });
        Ok(Self { w, h, capacity: capacity.max(1), params, reference, target, points, results, bind })
    }
}

static SAD_ENGINE: std::sync::OnceLock<std::sync::Mutex<Option<SadEngine>>> =
    std::sync::OnceLock::new();

/// Búsqueda SAD exhaustiva de UN punto grande, paralelizada por
/// desplazamiento. El kernel `sad_points` resuelve cada SadPoint en UN solo
/// hilo GPU: con la caja global del análisis (¼×¼ del mapa 4×, r=32 → hasta
/// ~770 M de operaciones a 4144×2822) ese único hilo tardaba varios segundos
/// y el watchdog de Metal mataba el command buffer EN EL PRIMER FRAME REAL —
/// la sesión entera quedaba condenada a "scoring CPU". Aquí cada candidato
/// (dx,dy) es su propio hilo con search_r=0 (mismo trabajo total, ~4225 hilos
/// en paralelo) y la reducción en CPU replica EXACTAMENTE el orden de barrido
/// del kernel mono-punto (centro primero, luego row-major, comparación
/// estricta) — resultado bit-idéntico.
#[allow(clippy::too_many_arguments)]
pub fn search_sad_single_parallel(
    reference: &[u16],
    target: &[u16],
    w: usize,
    h: usize,
    ref_x: i32,
    ref_y: i32,
    tgt_x: i32,
    tgt_y: i32,
    box_w: i32,
    box_h: i32,
    search_r: i32,
) -> Result<Option<SadMatch>, String> {
    let candidates = ((2 * search_r + 1) * (2 * search_r + 1)).max(1) as usize;
    let mut points = Vec::with_capacity(candidates);
    let mut offsets = Vec::with_capacity(candidates);
    for phase in 0..2 {
        for dy in -search_r..=search_r {
            for dx in -search_r..=search_r {
                if (phase == 0) != (dx == 0 && dy == 0) {
                    continue;
                }
                points.push(SadPoint::rectangular(
                    ref_x,
                    ref_y,
                    tgt_x + dx,
                    tgt_y + dy,
                    box_w,
                    box_h,
                    0,
                    true,
                ));
                offsets.push((dx, dy));
            }
        }
    }
    let results = search_sad_points(reference, target, w, h, &points)?;
    let mut best: Option<SadMatch> = None;
    for (result, &(dx, dy)) in results.iter().zip(&offsets) {
        let Some(m) = result else { continue };
        if best.is_none_or(|b| m.sad < b.sad) {
            best = Some(SadMatch { dx, dy, sad: m.sad });
        }
    }
    Ok(best)
}

/// Ejecuta búsquedas SAD gruesas para todos los AP en un solo dispatch. Los
/// mapas deben tener geometría idéntica y normalmente son las pirámides 4×.
pub fn search_sad_points(
    reference: &[u16],
    target: &[u16],
    w: usize,
    h: usize,
    points: &[SadPoint],
) -> Result<Vec<Option<SadMatch>>, String> {
    if points.is_empty() {
        return Ok(Vec::new());
    }
    if reference.len() < w * h || target.len() < w * h {
        return Err("Mapa SAD GPU truncado".into());
    }
    let rt = crate::gpu_stack::gpu_runtime().ok_or("No hay runtime wgpu")?;
    let mx = SAD_ENGINE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = mx.lock().map_err(|_| "Mutex GPU SAD dañado")?;
    let rebuild = guard
        .as_ref()
        .is_none_or(|e| e.w != w || e.h != h || e.capacity < points.len());
    if rebuild {
        *guard = Some(SadEngine::new(rt, w, h, points.len().next_power_of_two())?);
    }
    let e = guard.as_ref().unwrap();
    let ref_u32: Vec<u32> = reference[..w * h].iter().map(|&v| v as u32).collect();
    let tgt_u32: Vec<u32> = target[..w * h].iter().map(|&v| v as u32).collect();
    rt.queue.write_buffer(&e.reference, 0, bytemuck::cast_slice(&ref_u32));
    rt.queue.write_buffer(&e.target, 0, bytemuck::cast_slice(&tgt_u32));
    rt.queue.write_buffer(&e.points, 0, bytemuck::cast_slice(points));
    rt.queue.write_buffer(
        &e.params,
        0,
        bytemuck::bytes_of(&SadParams { w: w as u32, h: h as u32, count: points.len() as u32, pad: 0 }),
    );
    let pp = sad_pipelines(rt);
    let mut enc = rt.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("zas-analysis-sad-encoder"),
    });
    {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("zas-analysis-sad-pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pp.pipeline);
        pass.set_bind_group(0, &e.bind, &[]);
        pass.dispatch_workgroups((points.len() as u32).div_ceil(64), 1, 1);
    }
    rt.queue.submit(Some(enc.finish()));
    let raw = read_u32(rt, &e.results, points.len() * 4)?;
    if crate::gpu_stack::take_gpu_error() {
        return Err("Device loss/OOM durante SAD GPU".into());
    }
    let mut out = Vec::with_capacity(points.len());
    for r in raw.chunks_exact(4) {
        let sad = ((r[3] as u64) << 32) | r[2] as u64;
        if sad == u64::MAX {
            out.push(None);
        } else {
            out.push(Some(SadMatch { dx: r[0] as i32, dy: r[1] as i32, sad }));
        }
    }
    Ok(out)
}

fn cog_thresholds(mono: &[u16], w: usize, h: usize) -> [f32; 3] {
    let mut noise_sample = Vec::with_capacity(200);
    let block = 10usize.min(w.max(1)).min(h.max(1));
    for &by in &[0usize, h.saturating_sub(block)] {
        for &bx in &[0usize, w.saturating_sub(block)] {
            for y in (by..(by + block).min(h)).step_by(2) {
                for x in (bx..(bx + block).min(w)).step_by(2) {
                    noise_sample.push(mono[y * w + x]);
                }
            }
        }
    }
    noise_sample.sort_unstable();
    let noise = noise_sample
        .get(noise_sample.len() / 2)
        .copied()
        .unwrap_or(0) as f32;
    let peak = mono.iter().step_by(4).copied().max().unwrap_or(0) as f32;
    [0.12f32, 0.25, 0.45].map(|p| {
        (noise + (peak - noise).max(0.0) * p).max(noise + 100.0)
    })
}

fn center_from_projections(px: &[f32], py: &[f32], w: usize, h: usize) -> (f32, f32) {
    let mut centers = Vec::with_capacity(3);
    for t in 0..3 {
        let xs = &px[t * w..(t + 1) * w];
        let ys = &py[t * h..(t + 1) * h];
        let total = xs.iter().copied().sum::<f32>();
        if !total.is_finite() || total <= 0.0 {
            continue;
        }
        let tail = total * 0.005;
        let first = |values: &[f32]| {
            let mut acc = 0.0f32;
            for (i, &v) in values.iter().enumerate() {
                acc += v;
                if acc > tail {
                    return i;
                }
            }
            0
        };
        let last = |values: &[f32]| {
            let mut acc = 0.0f32;
            for (i, &v) in values.iter().enumerate().rev() {
                acc += v;
                if acc > tail {
                    return i;
                }
            }
            values.len().saturating_sub(1)
        };
        centers.push(((first(xs) + last(xs)) as f32 * 0.5, (first(ys) + last(ys)) as f32 * 0.5));
    }
    if centers.is_empty() {
        (w as f32 * 0.5, h as f32 * 0.5)
    } else {
        (
            centers.iter().map(|v| v.0).sum::<f32>() / centers.len() as f32,
            centers.iter().map(|v| v.1).sum::<f32>() / centers.len() as f32,
        )
    }
}

/// Preprocesado y analítica GPU de un frame. `surface_grid` elige la métrica
/// lunar/superficie; `want_grid` y `want_cog` evitan trabajo/readback cuando
/// la receta no necesita esos productos.
pub fn process_with_options(
    mono: &[u16],
    w: usize,
    h: usize,
    surface_grid: bool,
    want_grid: bool,
    want_cog: bool,
) -> Result<AnalysisGpuOutput, String> {
    if mono.len() < w * h { return Err("Frame mono truncado".into()); }
    let rt = crate::gpu_stack::gpu_runtime().ok_or("No hay runtime wgpu")?;
    let mx = ENGINE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = mx.lock().map_err(|_| "Mutex GPU de análisis dañado")?;
    let rebuild = guard.as_ref().is_none_or(|e| e.w != w || e.h != h);
    if rebuild { *guard = Some(Engine::new(rt, w, h)?); }
    let e = guard.as_ref().unwrap();
    let src_u32: Vec<u32> = mono[..w * h].iter().map(|&v| v as u32).collect();
    rt.queue.write_buffer(&e.src, 0, bytemuck::cast_slice(&src_u32));
    let thresholds = if want_cog { cog_thresholds(mono, w, h) } else { [0.0; 3] };
    let p = Params {
        w: w as u32,
        h: h as u32,
        hw: (w / 2) as u32,
        hh: (h / 2) as u32,
        threshold0: thresholds[0],
        threshold1: thresholds[1],
        threshold2: thresholds[2],
        fp0: 0.0,
        grid_size: if want_grid { 40 } else { 0 },
        analytics_flags: u32::from(surface_grid),
        up0: 0,
        up1: 0,
    };
    rt.queue.write_buffer(&e.params, 0, bytemuck::bytes_of(&p));
    let pp = pipelines(rt);
    let mut enc = rt.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("zas-analysis-encoder") });
    // Los kernels sólo escriben el interior de blur/Laplaciano. Limpiar los
    // bordes evita que un slot reutilizado conserve píxeles del frame anterior.
    enc.clear_buffer(&e.temp, 0, None);
    enc.clear_buffer(&e.blur, 0, None);
    enc.clear_buffer(&e.lap, 0, None);
    for pipeline in [&pp.downscale, &pp.blur_h, &pp.blur_v, &pp.lap] {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("zas-analysis-stage"), timestamp_writes: None });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &e.bind, &[]);
        pass.dispatch_workgroups(((w / 2) as u32).div_ceil(16), ((h / 2) as u32).div_ceil(16), 1);
    }
    {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("zas-analysis-score-stage"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pp.score_rows);
        pass.set_bind_group(0, &e.bind, &[]);
        pass.dispatch_workgroups(((h / 2) as u32).div_ceil(64), 1, 1);
    }
    if want_cog {
        for (pipeline, len) in [(&pp.cog_x, w), (&pp.cog_y, h)] {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("zas-analysis-cog-stage"),
                timestamp_writes: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &e.bind, &[]);
            pass.dispatch_workgroups((len as u32).div_ceil(64), 3, 1);
        }
    }
    if want_grid {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("zas-analysis-grid-stage"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pp.grid);
        pass.set_bind_group(0, &e.bind, &[]);
        pass.dispatch_workgroups((40u32 * 40).div_ceil(64), 1, 1);
    }
    rt.queue.submit(Some(enc.finish()));
    let n = (w / 2) * (h / 2);
    let half_raw = read_u32(rt, &e.half, n)?;
    let blur_raw = read_u32(rt, &e.blur, n)?;
    let lap_raw = read_u32(rt, &e.lap, n)?;
    let score_raw = read_u32(rt, &e.score_rows, (h / 2) * 2)?;
    let cog_x_raw = want_cog.then(|| read_u32(rt, &e.cog_x, w * 3)).transpose()?;
    let cog_y_raw = want_cog.then(|| read_u32(rt, &e.cog_y, h * 3)).transpose()?;
    let grid_raw = want_grid.then(|| read_u32(rt, &e.grid, 40 * 40)).transpose()?;
    if crate::gpu_stack::take_gpu_error() { return Err("Device loss/OOM durante análisis GPU".into()); }
    drop(guard);
    Ok(finish_analysis_output(
        &half_raw,
        &blur_raw,
        &lap_raw,
        &score_raw,
        cog_x_raw.as_deref(),
        cog_y_raw.as_deref(),
        grid_raw.as_deref(),
        w,
        h,
    ))
}

/// Procesa varios frames con un único submit y un único map de readback.
/// Los lotes mayores de ocho se dividen en chunks para acotar VRAM; el camino
/// habitual usa tres slots, suficiente para el solapamiento decode/GPU/CPU.
pub fn process_batch_with_options(
    frames: &[Vec<u16>],
    w: usize,
    h: usize,
    surface_grid: bool,
    want_grid: bool,
    want_cog: bool,
) -> Result<Vec<AnalysisGpuOutput>, String> {
    if frames.is_empty() {
        return Ok(Vec::new());
    }
    if frames.len() == 1 {
        return process_with_options(&frames[0], w, h, surface_grid, want_grid, want_cog)
            .map(|v| vec![v]);
    }
    if frames.len() > 8 {
        let mut out = Vec::with_capacity(frames.len());
        for chunk in frames.chunks(8) {
            out.extend(process_batch_with_options(
                chunk,
                w,
                h,
                surface_grid,
                want_grid,
                want_cog,
            )?);
        }
        return Ok(out);
    }
    if frames.iter().any(|f| f.len() < w * h) {
        return Err("Lote planetario contiene un frame mono truncado".into());
    }

    let rt = crate::gpu_stack::gpu_runtime().ok_or("No hay runtime wgpu")?;
    let capacity = frames.len().next_power_of_two().clamp(2, 8);
    let mx = BATCH_ENGINE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = mx.lock().map_err(|_| "Mutex GPU de lotes dañado")?;
    let rebuild = guard.as_ref().is_none_or(|e| {
        e.w != w || e.h != h || e.capacity < frames.len()
    });
    if rebuild {
        *guard = Some(BatchEngine::new(rt, w, h, capacity)?);
    }
    let batch = guard.as_ref().unwrap();
    let pp = pipelines(rt);
    let hw = w / 2;
    let hh = h / 2;
    let n = hw * hh;

    // Preparación por frame EN PARALELO (expansión u16→u32 de ~47 MB + pasada
    // de umbrales CoG): en serie bloqueaba el lote 30-50 ms bajo el mutex.
    // Mismas operaciones por frame → resultado bit-idéntico; los write_buffer
    // conservan su orden secuencial por slot.
    use rayon::prelude::*;
    let prepared: Vec<(Vec<u32>, [f32; 3])> = frames
        .par_iter()
        .map(|mono| {
            let src: Vec<u32> = mono[..w * h].iter().map(|&v| v as u32).collect();
            let thresholds = if want_cog { cog_thresholds(mono, w, h) } else { [0.0; 3] };
            (src, thresholds)
        })
        .collect();
    let mut enc = rt.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("zas-analysis-batch-encoder"),
    });
    for (slot_index, _mono) in frames.iter().enumerate() {
        let slot = &batch.slots[slot_index];
        let (src_u32, thresholds) = &prepared[slot_index];
        rt.queue.write_buffer(&slot.src, 0, bytemuck::cast_slice(src_u32));
        let thresholds = *thresholds;
        let params = Params {
            w: w as u32,
            h: h as u32,
            hw: hw as u32,
            hh: hh as u32,
            threshold0: thresholds[0],
            threshold1: thresholds[1],
            threshold2: thresholds[2],
            fp0: 0.0,
            grid_size: if want_grid { 40 } else { 0 },
            analytics_flags: u32::from(surface_grid),
            up0: 0,
            up1: 0,
        };
        rt.queue.write_buffer(&slot.params, 0, bytemuck::bytes_of(&params));
        enc.clear_buffer(&slot.temp, 0, None);
        enc.clear_buffer(&slot.blur, 0, None);
        enc.clear_buffer(&slot.lap, 0, None);
        for pipeline in [&pp.downscale, &pp.blur_h, &pp.blur_v, &pp.lap] {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("zas-analysis-batch-stage"),
                timestamp_writes: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &slot.bind, &[]);
            pass.dispatch_workgroups((hw as u32).div_ceil(16), (hh as u32).div_ceil(16), 1);
        }
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("zas-analysis-batch-score"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pp.score_rows);
            pass.set_bind_group(0, &slot.bind, &[]);
            pass.dispatch_workgroups((hh as u32).div_ceil(64), 1, 1);
        }
        if want_cog {
            for (pipeline, len) in [(&pp.cog_x, w), (&pp.cog_y, h)] {
                let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("zas-analysis-batch-cog"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &slot.bind, &[]);
                pass.dispatch_workgroups((len as u32).div_ceil(64), 3, 1);
            }
        }
        if want_grid {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("zas-analysis-batch-grid"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pp.grid);
            pass.set_bind_group(0, &slot.bind, &[]);
            pass.dispatch_workgroups((40u32 * 40).div_ceil(64), 1, 1);
        }

        let base = batch.layout.stride * slot_index * 4;
        let copy = |enc: &mut wgpu::CommandEncoder,
                    src: &wgpu::Buffer,
                    word_offset: usize,
                    word_len: usize| {
            enc.copy_buffer_to_buffer(
                src,
                0,
                &batch.staging,
                (base + word_offset * 4) as u64,
                (word_len * 4) as u64,
            );
        };
        copy(&mut enc, &slot.half, batch.layout.half, n);
        copy(&mut enc, &slot.blur, batch.layout.blur, n);
        copy(&mut enc, &slot.lap, batch.layout.lap, n);
        copy(&mut enc, &slot.score_rows, batch.layout.score, hh * 2);
        if want_cog {
            copy(&mut enc, &slot.cog_x, batch.layout.cog_x, w * 3);
            copy(&mut enc, &slot.cog_y, batch.layout.cog_y, h * 3);
        }
        if want_grid {
            copy(&mut enc, &slot.grid, batch.layout.grid, 40 * 40);
        }
    }

    rt.queue.submit(Some(enc.finish()));
    let used_bytes = (batch.layout.stride * frames.len() * 4) as u64;
    let slice = batch.staging.slice(0..used_bytes);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| { let _ = tx.send(result); });
    crate::gpu_stack::wait_for_readback(&rt.device, &rx, "readback del lote de análisis")?;
    let outputs = {
        let mapped = slice.get_mapped_range();
        let words: &[u32] = bytemuck::cast_slice(&mapped);
        let mut outputs = Vec::with_capacity(frames.len());
        for slot_index in 0..frames.len() {
            let base = batch.layout.stride * slot_index;
            let at = |off: usize, len: usize| &words[base + off..base + off + len];
            outputs.push(finish_analysis_output(
                at(batch.layout.half, n),
                at(batch.layout.blur, n),
                at(batch.layout.lap, n),
                at(batch.layout.score, hh * 2),
                want_cog.then(|| at(batch.layout.cog_x, w * 3)),
                want_cog.then(|| at(batch.layout.cog_y, h * 3)),
                want_grid.then(|| at(batch.layout.grid, 40 * 40)),
                w,
                h,
            ));
        }
        outputs
    };
    batch.staging.unmap();
    if crate::gpu_stack::take_gpu_error() {
        return Err("Device loss/OOM durante el lote de análisis GPU".into());
    }
    drop(guard);
    Ok(outputs)
}

pub fn ensure_parity() -> bool {
    match PARITY.load(Ordering::Acquire) { 1 => return true, 2 => return false, _ => {} }
    let w = 96usize; let h = 72usize;
    let mono: Vec<u16> = (0..w * h).map(|i| ((i * 137 + (i / w) * 79) % 65535) as u16).collect();
    let gpu = process_with_options(&mono, w, h, true, true, true);
    let gpu_planet_grid = process_with_options(&mono, w, h, false, true, false);
    let batch_frames = vec![
        mono.clone(),
        mono.iter().map(|&v| v.saturating_add(731)).collect(),
        mono.iter().map(|&v| v.saturating_sub(419)).collect(),
    ];
    let batch = process_batch_with_options(&batch_frames, w, h, true, true, true);
    let singles: Result<Vec<_>, _> = batch_frames
        .iter()
        .map(|f| process_with_options(f, w, h, true, true, true))
        .collect();
    let mut half = Vec::new();
    let (hw, hh) = crate::alignment::downscale_2x_into(&mono, w, h, &mut half);
    let mut temp = vec![0u16; hw * hh];
    let mut blur = vec![0u16; hw * hh];
    let mut lap = vec![0u16; hw * hh];
    let score = crate::enhance_and_score_surface_buffered(&half, hw, hh, &mut temp, &mut blur, &mut lap);
    let cpu_center = crate::compute_robust_geometric_center(&mono, w, h, 0, 0);
    let cpu_surface_grid = crate::calculate_grid_quality(&blur, hw, hh, 40, true);
    let cpu_planet_grid = crate::calculate_grid_quality(&half, hw, hh, 40, false);
    let grid_close = |gpu: &[u64], cpu: &[u64]| {
        gpu.len() == cpu.len()
            && gpu.iter().zip(cpu).all(|(&a, &b)| {
                a.abs_diff(b) <= ((b as f64 * 0.002).ceil() as u64).max(4096)
            })
    };
    let ok_surface = gpu.as_ref().ok().is_some_and(|g| {
        let center_ok = g.geometric_center.is_some_and(|c| {
            (c.0 - cpu_center.0).abs() <= 0.51 && (c.1 - cpu_center.1).abs() <= 0.51
        });
        g.half == half
            && g.blurred == blur
            && g.laplacian == lap
            && g.score == score
            && center_ok
            && g.grid_scores
                .as_deref()
                .is_some_and(|m| grid_close(m, &cpu_surface_grid))
    });
    let ok_planet = gpu_planet_grid.as_ref().ok().is_some_and(|g| {
        g.grid_scores
            .as_deref()
            .is_some_and(|m| grid_close(m, &cpu_planet_grid))
    });
    let output_equal = |a: &AnalysisGpuOutput, b: &AnalysisGpuOutput| {
        a.half == b.half
            && a.blurred == b.blurred
            && a.laplacian == b.laplacian
            && a.score == b.score
            && a.grid_scores == b.grid_scores
            && match (a.geometric_center, b.geometric_center) {
                (Some(a), Some(b)) => (a.0 - b.0).abs() <= 1e-4 && (a.1 - b.1).abs() <= 1e-4,
                (None, None) => true,
                _ => false,
            }
    };
    let batch_ok = batch.as_ref().ok().zip(singles.as_ref().ok()).is_some_and(|(a, b)| {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| output_equal(x, y))
    });
    // Distinguir FALLO DE TRANSPORTE (device loss, timeout de readback, VRAM)
    // de una desigualdad numérica real: solo la segunda condena la GPU para
    // toda la sesión; la primera deja la paridad "pendiente" y el siguiente
    // análisis reintenta con el dispositivo recuperado.
    let transport_failed =
        gpu.is_err() || gpu_planet_grid.is_err() || batch.is_err() || singles.is_err();
    let ok = ok_surface && ok_planet && batch_ok;
    if !ok {
        if let Ok(g) = &gpu {
            let surface_diff = g.grid_scores.as_deref().map(|m| {
                m.iter().zip(&cpu_surface_grid).map(|(&a, &b)| a.abs_diff(b)).max().unwrap_or(0)
            });
            let surface_worst = g.grid_scores.as_deref().and_then(|m| {
                m.iter().zip(&cpu_surface_grid).enumerate()
                    .max_by_key(|(_, (&a, &b))| a.abs_diff(b))
                    .map(|(i, (&a, &b))| (i, a, b))
            });
            eprintln!(
                "[gpu-analysis parity] pixels={}/{}/{} score={} cpu_score={} center={:?} cpu_center={:?} surface_grid_diff={:?} worst={:?}",
                g.half == half,
                g.blurred == blur,
                g.laplacian == lap,
                g.score,
                score,
                g.geometric_center,
                cpu_center,
                surface_diff,
                surface_worst,
            );
        }
        if let Ok(g) = &gpu_planet_grid {
            let diff = g.grid_scores.as_deref().map(|m| {
                m.iter().zip(&cpu_planet_grid).map(|(&a, &b)| a.abs_diff(b)).max().unwrap_or(0)
            });
            eprintln!("[gpu-analysis parity] planet_grid_diff={diff:?}");
        }
    }
    let state = if ok {
        1
    } else if transport_failed {
        eprintln!("[gpu-analysis parity] fallo de transporte GPU; se reintentará en el próximo análisis");
        0
    } else {
        2
    };
    PARITY.store(state, Ordering::Release);
    ok
}

#[cfg(test)]
mod tests {
    #[test]
    fn planetary_batch_rejects_insufficient_vram_before_allocating() {
        let needed = super::batch_vram_bytes(4096, 3072, 3);
        assert!(needed > 1);
        let err = super::validate_batch_vram(4096, 3072, 3, needed - 1).unwrap_err();
        assert!(err.contains("presupuesto"));
        assert_eq!(super::validate_batch_vram(4096, 3072, 3, needed).unwrap(), needed);
    }

    #[test]
    #[ignore = "requiere GPU física Metal/DX12/Vulkan"]
    fn planetary_analysis_gpu_parity_physical() { assert!(super::ensure_parity()); }

    #[test]
    #[ignore = "requiere GPU física; valida submit/readback multiframe"]
    fn planetary_analysis_gpu_batch_parity_physical() { assert!(super::ensure_parity()); }

    /// GUARDIA DE REGRESIÓN del probe de calentamiento. La versión antigua
    /// lanzaba UN SadPoint con box sw/2×sh/2 y r=32: cada SadPoint es UN SOLO
    /// hilo GPU, y con mapas disímiles (poda inútil) se midieron 9.2 s a
    /// 1080p en Apple M5 — suficiente para el watchdog de Metal (fallback CPU
    /// permanente) o para congelar el readback (SER en 0, Cancelar muerto).
    /// Este test ejecuta la forma NUEVA del probe (rejilla 3×3, caja 24,
    /// r=16) con la misma disimilitud y exige que sea casi instantánea a
    /// geometrías 1080p y 4K. Corre en hilo con timeout para no colgar la
    /// suite si reapareciera un dispatch patológico.
    #[test]
    #[ignore = "requiere GPU física; acota el coste del probe de calentamiento"]
    fn analysis_warmup_probe_shape_is_bounded_at_4k() {
        for (label, sw, sh) in [("1080p", 480usize, 270usize), ("4k", 960usize, 540usize)] {
            // Mapas DISÍMILES a propósito: el probe real compara la pirámide
            // del mapa realzado contra el laplaciano del frame — el centro no
            // da SAD≈0 y la poda por fila apenas recorta.
            let reference: Vec<u16> = (0..sw * sh)
                .map(|i| ((i * 2654435761usize) & 0xffff) as u16)
                .collect();
            let target: Vec<u16> = (0..sw * sh)
                .map(|i| (((i * 40503usize) ^ (i >> 3)) & 0xffff) as u16)
                .collect();
            // MISMA construcción que el probe de perform_standardized_analysis.
            let box_size = 24i32
                .min((sw as i32 / 4).max(8))
                .min((sh as i32 / 4).max(8));
            let mut points = Vec::with_capacity(9);
            for gy in 1..=3i32 {
                for gx in 1..=3i32 {
                    let x = (sw as i32 * gx) / 4;
                    let y = (sh as i32 * gy) / 4;
                    points.push(super::SadPoint::new(x, y, x, y, box_size, 16, true));
                }
            }
            let (tx, rx) = std::sync::mpsc::channel();
            let (r2, t2) = (reference.clone(), target.clone());
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                let result = super::search_sad_points(&r2, &t2, sw, sh, &points);
                let _ = tx.send((result.map(|_| ()), started.elapsed()));
            });
            match rx.recv_timeout(std::time::Duration::from_secs(20)) {
                Ok((Ok(()), elapsed)) => {
                    eprintln!("probe {label}: {:.2?}", elapsed);
                    assert!(
                        elapsed.as_secs_f32() < 2.0,
                        "probe {label} tardó {elapsed:?}: el probe volvió a ser patológico (riesgo watchdog)"
                    );
                }
                Ok((Err(e), elapsed)) => {
                    panic!("probe {label} falló tras {elapsed:?}: {e}");
                }
                Err(_) => panic!("probe {label} COLGADO >20 s (readback nunca volvió)"),
            }
        }
    }

    /// Reproduce la decisión híbrida COMPLETA + primer lote a tamaños reales
    /// de cámara (la del usuario es 4144×2822). Si esto pasa y la app aún cae
    /// a CPU, el motivo está en la política elegida (p.ej. Auto en
    /// localStorage) — no en el motor GPU.
    #[test]
    #[ignore = "requiere GPU física; decisión híbrida + primer lote a tamaños reales"]
    fn hybrid_analysis_first_batch_works_at_real_camera_sizes() {
        assert!(super::ensure_parity(), "paridad debe pasar en GPU física");
        for (label, w, h) in [
            ("4144x2822", 4144usize, 2822usize),
            ("4k", 3840, 2160),
            ("1080p", 1920, 1080),
        ] {
            let batch_len = super::recommended_batch_len(w, h);
            let vram = super::validate_recommended_batch(w, h)
                .unwrap_or_else(|e| panic!("{label}: validate_recommended_batch falló: {e}"));
            let frames: Vec<Vec<u16>> = (0..batch_len)
                .map(|k| {
                    (0..w * h)
                        .map(|i| ((i * 31 + k * 7919) & 0xffff) as u16)
                        .collect()
                })
                .collect();
            let t0 = std::time::Instant::now();
            let out = super::process_batch_with_options(&frames, w, h, true, true, true)
                .unwrap_or_else(|e| panic!("{label}: primer lote GPU falló: {e}"));
            eprintln!(
                "{label}: batch_len={batch_len} · working-set {} MB · lote en {:.2?} ({:.1} ms/frame)",
                vram / 1_048_576,
                t0.elapsed(),
                t0.elapsed().as_secs_f32() * 1000.0 / batch_len as f32
            );
            assert_eq!(out.len(), batch_len);
        }
    }

    /// La búsqueda global por frame usa ahora search_sad_single_parallel.
    /// (1) Paridad exacta contra el SadPoint único a tamaño pequeño (donde el
    /// mono-hilo es viable). (2) Velocidad a la geometría REAL del usuario
    /// (4144×2822 → mapa 4× de 1036×705, caja 518×352, r=32): el punto único
    /// tardaba segundos y disparaba el watchdog; la variante paralela debe
    /// resolverse en milisegundos.
    #[test]
    #[ignore = "requiere GPU física; SAD global por frame paralelizado"]
    fn per_frame_global_sad_is_parallel_and_bit_identical() {
        // (1) Paridad exacta a 240×160, caja 60×40, r=8.
        let (w, h) = (240usize, 160usize);
        let reference: Vec<u16> = (0..w * h)
            .map(|i| ((i * 2654435761usize) & 0xffff) as u16)
            .collect();
        let target: Vec<u16> = (0..w * h)
            .map(|i| (((i * 40503usize) ^ (i >> 2)) & 0xffff) as u16)
            .collect();
        let (cx, cy) = ((w / 2) as i32, (h / 2) as i32);
        let single = super::search_sad_points(
            &reference,
            &target,
            w,
            h,
            &[super::SadPoint::rectangular(cx, cy, cx, cy, 60, 40, 8, true)],
        )
        .unwrap()[0]
        .expect("candidato válido");
        let parallel = super::search_sad_single_parallel(
            &reference, &target, w, h, cx, cy, cx, cy, 60, 40, 8,
        )
        .unwrap()
        .expect("candidato válido");
        assert_eq!(
            (single.dx, single.dy, single.sad),
            (parallel.dx, parallel.dy, parallel.sad),
            "la variante paralela debe ser bit-idéntica al punto único"
        );

        // (2) Velocidad a la geometría real del análisis del usuario.
        let (sw, sh) = (1036usize, 705usize);
        let reference: Vec<u16> = (0..sw * sh)
            .map(|i| ((i * 2654435761usize) & 0xffff) as u16)
            .collect();
        let target: Vec<u16> = (0..sw * sh)
            .map(|i| (((i * 40503usize) ^ (i >> 3)) & 0xffff) as u16)
            .collect();
        let t0 = std::time::Instant::now();
        let m = super::search_sad_single_parallel(
            &reference,
            &target,
            sw,
            sh,
            (sw / 2) as i32,
            (sh / 2) as i32,
            (sw / 2) as i32,
            (sh / 2) as i32,
            518,
            352,
            32,
        )
        .unwrap();
        let elapsed = t0.elapsed();
        eprintln!("SAD global 1036x705 caja 518x352 r=32: {elapsed:.2?} → {m:?}");
        assert!(m.is_some());
        assert!(
            elapsed.as_secs_f32() < 3.0,
            "el SAD global por frame tardó {elapsed:?}: volvería a disparar el watchdog"
        );
    }

    #[test]
    #[ignore = "requiere GPU física Metal/DX12/Vulkan"]
    fn planetary_sad_batch_gpu_parity_physical() {
        let (w, h) = (128usize, 96usize);
        let reference: Vec<u16> = (0..w * h)
            .map(|i| {
                let x = i % w;
                let y = i / w;
                (((x * 977 + y * 613 + x * y * 17) ^ (x << 7) ^ (y << 5)) & 0xffff)
                    as u16
            })
            .collect();
        let mut target = vec![0u16; w * h];
        let (truth_dx, truth_dy) = (3i32, -2i32);
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let tx = x + truth_dx;
                let ty = y + truth_dy;
                if tx >= 0 && ty >= 0 && tx < w as i32 && ty < h as i32 {
                    target[ty as usize * w + tx as usize] = reference[y as usize * w + x as usize];
                }
            }
        }
        let points = vec![
            super::SadPoint::new(40, 32, 40, 32, 20, 6, true),
            super::SadPoint::new(88, 62, 88, 62, 28, 6, true),
            super::SadPoint::rectangular(64, 48, 64, 48, 44, 24, 6, true),
        ];
        let got = super::search_sad_points(&reference, &target, w, h, &points).unwrap();
        for (point, gpu) in points.iter().zip(got) {
            let gpu = gpu.expect("AP válido");
            let (dx, dy, sad) = crate::alignment::find_best_match_sad(
                &reference,
                &target,
                w,
                point.ref_x as usize,
                point.ref_y as usize,
                point.tgt_x as usize,
                point.tgt_y as usize,
                point.box_w as usize,
                point.search_r,
            );
            // Los dos primeros son cuadrados y comparan también el SAD exacto.
            // El tercero valida el kernel rectangular contra el ground truth.
            assert_eq!((gpu.dx, gpu.dy), (dx as i32, dy as i32));
            if point.box_w == point.box_h {
                assert_eq!(gpu.sad, sad);
            }
            assert_eq!((gpu.dx, gpu.dy), (truth_dx, truth_dy));
        }
    }
}
