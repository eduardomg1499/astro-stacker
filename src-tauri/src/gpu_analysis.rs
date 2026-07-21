//! Preprocesado GPU del análisis planetario.
//!
//! Convierte la imagen mono ya interpretada por `FrameSource` en la pirámide
//! 2×, blur binomial y Laplaciano usados por SAD/calidad. Un mutex conserva los
//! buffers y serializa únicamente los submits; mientras la GPU procesa N, los
//! workers Rayon siguen con SAD/CoG/decisiones de frames anteriores.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

static PARITY: AtomicU8 = AtomicU8::new(0); // 0 pending, 1 ok, 2 failed
static SAD_PARITY: AtomicU8 = AtomicU8::new(0); // kernel SAD batched independiente

const WGSL: &str = r#"
struct Params {
    w: u32, h: u32, hw: u32, hh: u32,
    threshold0: f32, threshold1: f32, threshold2: f32, _fp0: f32,
    grid_size: u32, analytics_flags: u32, _up0: u32, _up1: u32,
}
@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> src: array<u32>;
// PR-2.3: src viaja EMPAQUETADO (2 muestras u16 por palabra u32). Antes la
// CPU expandía cada frame a un Vec<u32> (~47 MB a 11.7 Mpx) solo para subirlo
// con el doble de ancho de banda; ahora sube los bits tal cual y el shader
// desempaqueta al leer.
fn src_at(i: u32) -> u32 {
    return (src[i >> 1u] >> ((i & 1u) * 16u)) & 0xFFFFu;
}
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
    let sum = src_at(i0) + src_at(i0 + 1u) + src_at(i0 + P.w) + src_at(i0 + P.w + 1u);
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

// Los tres mapas de salida son u16 lógicos, aunque los kernels los conservan
// en u32 porque WGSL/storage no ofrece un array<u16> portable. Empaquetarlos
// aquí antes del readback evita transferir el doble de bytes a la CPU. `temp`
// ya no se usa después del blur vertical, de modo que sirve como staging GPU
// sin reservar otro buffer ni aumentar el límite de bindings del adapter.
@compute @workgroup_size(256)
fn pack_half_u16(@builtin(global_invocation_id) gid: vec3<u32>) {
    let count = P.hw * P.hh;
    let i = gid.x * 2u;
    if (i >= count) { return; }
    var hi = 0u;
    if (i + 1u < count) { hi = half[i + 1u] & 0xffffu; }
    temp[gid.x] = (half[i] & 0xffffu) | (hi << 16u);
}

@compute @workgroup_size(256)
fn pack_blur_u16(@builtin(global_invocation_id) gid: vec3<u32>) {
    let count = P.hw * P.hh;
    let i = gid.x * 2u;
    if (i >= count) { return; }
    var hi = 0u;
    if (i + 1u < count) { hi = blur[i + 1u] & 0xffffu; }
    temp[gid.x] = (blur[i] & 0xffffu) | (hi << 16u);
}

@compute @workgroup_size(256)
fn pack_lap_u16(@builtin(global_invocation_id) gid: vec3<u32>) {
    let count = P.hw * P.hh;
    let i = gid.x * 2u;
    if (i >= count) { return; }
    var hi = 0u;
    if (i + 1u < count) { hi = lap[i + 1u] & 0xffffu; }
    temp[gid.x] = (lap[i] & 0xffffu) | (hi << 16u);
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
        let v = f32(src_at(y * P.w + x));
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
        let v = f32(src_at(y * P.w + x));
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
    pack_half: wgpu::ComputePipeline,
    pack_blur: wgpu::ComputePipeline,
    pack_lap: wgpu::ComputePipeline,
    cog_x: wgpu::ComputePipeline,
    cog_y: wgpu::ComputePipeline,
    score_rows: wgpu::ComputePipeline,
    grid: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

static PIPELINES: std::sync::OnceLock<Pipelines> = std::sync::OnceLock::new();

fn pipelines(rt: &'static crate::gpu_stack::GpuRuntime) -> &'static Pipelines {
    PIPELINES.get_or_init(|| {
        let module = rt
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
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
        let layout = rt
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
                    storage(1, true),
                    storage(2, false),
                    storage(3, false),
                    storage(4, false),
                    storage(5, false),
                    storage(6, false),
                    storage(7, false),
                    storage(8, false),
                    storage(9, false),
                ],
            });
        let pl = rt
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("zas-analysis-preprocess-pipeline-layout"),
                bind_group_layouts: &[&layout],
                push_constant_ranges: &[],
            });
        let make = |entry| {
            rt.device
                .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(entry),
                    layout: Some(&pl),
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    cache: None,
                })
        };
        Pipelines {
            downscale: make("downscale"),
            blur_h: make("blur_h"),
            blur_v: make("blur_v"),
            lap: make("laplacian"),
            pack_half: make("pack_half_u16"),
            pack_blur: make("pack_blur_u16"),
            pack_lap: make("pack_lap_u16"),
            cog_x: make("cog_x"),
            cog_y: make("cog_y"),
            score_rows: make("score_by_row"),
            grid: make("grid_quality"),
            layout,
        }
    })
}

struct Engine {
    params: wgpu::Buffer,
    src: wgpu::Buffer,
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
    // src empaquetado: 2 muestras u16 por u32 (PR-2.3).
    let full_bytes = (w * h).div_ceil(2) as u64 * 4;
    let half_bytes = (hw * hh * 4) as u64;
    let cog_x_bytes = (w * 3 * 4) as u64;
    let cog_y_bytes = (h * 3 * 4) as u64;
    let grid_bytes = (40 * 40 * 4) as u64;
    let score_bytes = (hh * 2 * 4) as u64;
    full_bytes + half_bytes * 4 + cog_x_bytes + cog_y_bytes + grid_bytes + score_bytes + 4096
}

impl Engine {
    fn new(rt: &'static crate::gpu_stack::GpuRuntime, w: usize, h: usize) -> Result<Self, String> {
        if rt.max_storage_buffers_per_shader_stage < 9 {
            return Err(format!(
                "Análisis GPU requiere 9 storage buffers por etapa y el adapter ofrece {}; la acumulación GPU sigue disponible",
                rt.max_storage_buffers_per_shader_stage
            ));
        }
        let hw = w / 2;
        let hh = h / 2;
        if hw < 8 || hh < 8 {
            return Err("ROI demasiado pequeña para análisis GPU".into());
        }
        // src empaquetado: 2 muestras u16 por u32 (PR-2.3).
        let full_bytes = (w * h).div_ceil(2) as u64 * 4;
        let half_bytes = (hw * hh * 4) as u64;
        let cog_x_bytes = (w * 3 * 4) as u64;
        let cog_y_bytes = (h * 3 * 4) as u64;
        let grid_bytes = (40 * 40 * 4) as u64;
        let score_bytes = (hh * 2 * 4) as u64;
        let needed = engine_vram_bytes(w, h);
        if full_bytes > rt.max_binding || half_bytes > rt.max_binding || needed > rt.vram_budget {
            return Err(format!(
                "Análisis GPU requiere {} MB y el presupuesto es {} MB",
                needed / 1_048_576,
                rt.vram_budget / 1_048_576
            ));
        }
        let pp = pipelines(rt);
        let params = rt.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zas-analysis-params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mk = |label, size, ro| {
            rt.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_DST
                    | if ro {
                        wgpu::BufferUsages::empty()
                    } else {
                        wgpu::BufferUsages::COPY_SRC
                    },
                mapped_at_creation: false,
            })
        };
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
            label: Some("zas-analysis-bind"),
            layout: &pp.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: src.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: half.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: temp.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: blur.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: lap.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: cog_x.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: cog_y.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 8,
                    resource: grid.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 9,
                    resource: score_rows.as_entire_binding(),
                },
            ],
        });
        Ok(Self {
            params,
            src,
            temp,
            blur,
            lap,
            cog_x,
            cog_y,
            grid,
            score_rows,
            bind,
        })
    }
}

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
    want_half: bool,
    packed_image_words: usize,
}

impl BatchReadbackLayout {
    /// Layout compacto de readback. Las imágenes u16 viajan empaquetadas de dos
    /// muestras por palabra; CoG/rejilla sólo ocupan staging cuando se piden.
    /// Superficie no consume `half` después del preprocesado (usa blur+lap),
    /// por lo que tampoco se descarga ese mapa de varios megapíxeles.
    fn new(
        w: usize,
        h: usize,
        want_cog: bool,
        want_grid: bool,
        want_half: bool,
        want_score: bool,
    ) -> Self {
        let n = (w / 2) * (h / 2);
        let packed_image_words = n.div_ceil(2);
        let half = 0;
        let blur = half + if want_half { packed_image_words } else { 0 };
        let lap = blur + packed_image_words;
        let score = lap + packed_image_words;
        // A3 2026-07-17: el camino de análisis re-puntúa SIEMPRE en CPU sobre
        // el lap crudo ("paridad por construcción") y descartaba este score —
        // sin want_score ni se despacha el kernel ni viaja su readback.
        let cog_x = score + if want_score { (h / 2) * 2 } else { 0 };
        let cog_y = cog_x + if want_cog { w * 3 } else { 0 };
        let grid = cog_y + if want_cog { h * 3 } else { 0 };
        let stride = grid + if want_grid { 40 * 40 } else { 0 };
        Self {
            half,
            blur,
            lap,
            score,
            cog_x,
            cog_y,
            grid,
            stride,
            want_half,
            packed_image_words,
        }
    }
}

fn batch_vram_bytes(w: usize, h: usize, capacity: usize) -> u64 {
    // Contrato conservador usado al recomendar un lote: reserva el peor caso
    // (CoG + rejilla). La asignación real de BatchEngine usa el layout compacto.
    let layout = BatchReadbackLayout::new(w, h, true, true, true, true);
    batch_vram_bytes_for_layout(w, h, capacity, layout.stride)
}

fn batch_vram_bytes_for_layout(
    w: usize,
    h: usize,
    capacity: usize,
    staging_words_per_slot: usize,
) -> u64 {
    engine_vram_bytes(w, h)
        .saturating_mul(capacity as u64)
        .saturating_add(
            (staging_words_per_slot as u64)
                .saturating_mul(capacity as u64)
                .saturating_mul(4),
        )
}

/// Working-set aproximado del motor multiframe, para telemetría visible. El
/// pool usa exactamente los slots necesarios (mínimo dos), sin el 25–33% de
/// VRAM desperdiciada que introducía `next_power_of_two` en lotes de 3/6.
pub fn estimated_batch_vram_mb(w: usize, h: usize, frames_per_batch: usize) -> u64 {
    let capacity = frames_per_batch.clamp(2, 8);
    batch_vram_bytes(w, h, capacity).div_ceil(1_048_576)
}

/// Tamaño de lote recomendado para el análisis multiframe. El pool reutilizable
/// reserva exactamente 6, 4 o 2 slots. GPUs con presupuesto amplio amortizan más
/// frames por submit/readback; iGPU modestas conservan el working-set mínimo.
/// Cualquier exceso real lo detiene igualmente validate_batch_vram (fallback
/// CPU por lote), esto sólo decide el punto de partida.
pub fn recommended_batch_len(w: usize, h: usize) -> usize {
    let Some(rt) = crate::gpu_stack::gpu_runtime() else {
        return 3;
    };
    let comfortable = rt.vram_budget.saturating_mul(35) / 100;
    for candidate in [6usize, 4] {
        if batch_vram_bytes(w, h, candidate) <= comfortable {
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
    let capacity = recommended_batch_len(w, h).clamp(2, 8);
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

fn max_batch_len_for_limits(
    w: usize,
    h: usize,
    requested: usize,
    staging_words_per_slot: usize,
    budget: u64,
    max_buffer_size: u64,
) -> usize {
    for len in (1..=requested.clamp(1, 8)).rev() {
        let capacity = len.max(2);
        let staging_bytes = (staging_words_per_slot as u64)
            .saturating_mul(capacity as u64)
            .saturating_mul(4);
        if staging_bytes <= max_buffer_size
            && batch_vram_bytes_for_layout(w, h, capacity, staging_words_per_slot) <= budget
        {
            return len;
        }
    }
    0
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
    staging_words_per_slot: usize,
    _vram_reservation: crate::gpu_stack::PlanetaryVramReservation,
}

impl BatchEngine {
    fn new(
        rt: &'static crate::gpu_stack::GpuRuntime,
        w: usize,
        h: usize,
        capacity: usize,
        staging_words_per_slot: usize,
    ) -> Result<Self, String> {
        let capacity = capacity.clamp(2, 8);
        let staging_bytes = (staging_words_per_slot as u64)
            .saturating_mul(capacity as u64)
            .saturating_mul(4);
        if staging_bytes > rt.max_buffer_size {
            return Err(format!(
                "Staging de análisis ({} MB) excede el buffer máximo del adapter ({} MB)",
                staging_bytes / 1_048_576,
                rt.max_buffer_size / 1_048_576
            ));
        }
        let needed = batch_vram_bytes_for_layout(w, h, capacity, staging_words_per_slot);
        if needed > rt.vram_budget {
            return Err(format!(
                "Análisis GPU por lotes requiere {} MB y el presupuesto es {} MB",
                needed / 1_048_576,
                rt.vram_budget / 1_048_576
            ));
        }
        let vram_reservation = rt
            .planetary_vram
            .try_reserve(needed, "lotes de análisis planetario")?;
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
        Ok(Self {
            w,
            h,
            capacity,
            slots,
            staging,
            staging_words_per_slot,
            _vram_reservation: vram_reservation,
        })
    }
}

static BATCH_ENGINE: std::sync::OnceLock<std::sync::Mutex<Option<BatchEngine>>> =
    std::sync::OnceLock::new();

pub struct AnalysisGpuOutput {
    /// Vacío para superficie: esa ruta consume `blurred` + `laplacian` y no
    /// necesita descargar el mapa intermedio. En planeta conserva el mapa 2×.
    pub half: Vec<u16>,
    pub blurred: Vec<u16>,
    pub laplacian: Vec<u16>,
    pub score: u64,
    pub geometric_center: Option<(f32, f32)>,
    pub grid_scores: Option<Vec<u64>>,
}

fn finish_analysis_output(
    half_raw: Option<&[u32]>,
    blur_raw: &[u32],
    lap_raw: &[u32],
    score_raw: Option<&[u32]>,
    cog_x_raw: Option<&[u32]>,
    cog_y_raw: Option<&[u32]>,
    grid_raw: Option<&[u32]>,
    w: usize,
    h: usize,
) -> AnalysisGpuOutput {
    let image_len = (w / 2) * (h / 2);
    let unpack_u16 = |raw: &[u32]| {
        let mut values = Vec::with_capacity(image_len);
        for &packed in raw {
            if values.len() >= image_len {
                break;
            }
            values.push((packed & 0xffff) as u16);
            if values.len() < image_len {
                values.push((packed >> 16) as u16);
            }
        }
        values
    };
    let score = score_raw
        .map(|raw| {
            raw.chunks_exact(2)
                .map(|v| ((v[1] as u64) << 32) | v[0] as u64)
                .sum()
        })
        .unwrap_or(0);
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
    let half = half_raw.map(unpack_u16).unwrap_or_default();
    let blurred = unpack_u16(blur_raw);
    // El Laplaciano se devuelve CRUDO: el consumidor puntúa nitidez v2 sobre
    // magnitudes reales y solo después llama a normalize_lap_for_sad (la
    // normalización aquí invertía el ranking del scorer — baseline F0).
    let laplacian = unpack_u16(lap_raw);
    AnalysisGpuOutput {
        half,
        blurred,
        laplacian,
        score,
        geometric_center,
        grid_scores,
    }
}

// ---------------------------------------------------------------------------
// SAD por lotes. Cada invocation resuelve la búsqueda gruesa completa de un
// punto de alineación sobre los mapas 4× reducidos. La CPU recibe solamente
// (dx,dy,SAD) por AP y conserva el refinamiento subpíxel/LK y los filtros
// espaciales. De esta forma no se hace un submit por AP ni se descarga una
// superficie de costes de (2r+1)² elementos.
// ---------------------------------------------------------------------------

const SAD_WGSL: &str = r#"
struct SadParams { w: u32, h: u32, count: u32, start: u32 }
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

fn reference_at(i: u32) -> u32 {
    return (reference[i >> 1u] >> ((i & 1u) * 16u)) & 0xffffu;
}
fn target_at(i: u32) -> u32 {
    return (tgt_image[i >> 1u] >> ((i & 1u) * 16u)) & 0xffffu;
}

fn less64(ahi: u32, alo: u32, bhi: u32, blo: u32) -> bool {
    return ahi < bhi || (ahi == bhi && alo < blo);
}

@compute @workgroup_size(64)
fn sad_points(@builtin(global_invocation_id) gid: vec3<u32>) {
    let pi = P.start + gid.x;
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
    if (rx0 < 0 || ry0 < 0 || rx0 + ap.box_w > i32(P.w)
        || ry0 + ap.box_h > i32(P.h)) {
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
                if (tx0 < 0 || ty0 < 0 || tx0 + ap.box_w > i32(P.w)
                    || ty0 + ap.box_h > i32(P.h)) { continue; }
                var lo = 0u;
                var hi = 0u;
                var pruned = false;
                for (var py = 0; py < ap.box_h; py = py + 1) {
                    let rr = u32(ry0 + py) * P.w;
                    let tr = u32(ty0 + py) * P.w;
                    for (var px = 0; px < ap.box_w; px = px + 1) {
                        let a = reference_at(rr + u32(rx0 + px));
                        let b = target_at(tr + u32(tx0 + px));
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
    start: u32,
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
        let module = rt
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
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
        let layout = rt
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
        let pl = rt
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("zas-analysis-sad-pipeline-layout"),
                bind_group_layouts: &[&layout],
                push_constant_ranges: &[],
            });
        let pipeline = rt
            .device
            .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
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
    /// Arc permite que una llamada conserve la reserva hasta terminar su
    /// readback aunque el LRU desaloje el engine después del submit.
    _vram_reservation: Arc<crate::gpu_stack::PlanetaryVramReservation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SadEngineAllocation {
    image_bytes: u64,
    point_bytes: u64,
    result_bytes: u64,
    /// Buffers persistentes. Cada llamada reserva aparte su staging real para
    /// que un batch de varios targets también quede cubierto exactamente.
    reserved_bytes: u64,
}

fn sad_engine_allocation(
    w: usize,
    h: usize,
    capacity: usize,
) -> Result<SadEngineAllocation, String> {
    let pixels = w.checked_mul(h).ok_or("Mapa SAD demasiado grande")?;
    // Dos u16 por palabra u32: mitad de VRAM y ancho de upload que una
    // expansión u16 -> u32.
    let image_bytes = (pixels.div_ceil(2) as u64)
        .checked_mul(4)
        .ok_or("Mapa SAD demasiado grande")?;
    let capacity = capacity.max(1);
    let point_bytes = (capacity as u64)
        .checked_mul(std::mem::size_of::<SadPoint>() as u64)
        .ok_or("Demasiados puntos SAD")?;
    let result_bytes = (capacity as u64)
        .checked_mul(16)
        .ok_or("Demasiados resultados SAD")?;
    let reserved_bytes = image_bytes
        .checked_mul(2)
        .and_then(|v| v.checked_add(point_bytes))
        .and_then(|v| v.checked_add(result_bytes))
        .and_then(|v| v.checked_add(4096))
        .ok_or("Reserva SAD GPU demasiado grande")?;
    Ok(SadEngineAllocation {
        image_bytes,
        point_bytes,
        result_bytes,
        reserved_bytes,
    })
}

impl SadEngine {
    fn new(
        rt: &'static crate::gpu_stack::GpuRuntime,
        w: usize,
        h: usize,
        capacity: usize,
    ) -> Result<Self, String> {
        let allocation = sad_engine_allocation(w, h, capacity)?;
        if allocation.image_bytes > rt.max_binding
            || allocation.point_bytes > rt.max_binding
            || allocation.result_bytes > rt.max_binding
            || allocation.reserved_bytes > rt.vram_budget
        {
            return Err(format!(
                "SAD GPU requiere {} MB y el presupuesto es {} MB",
                allocation.reserved_bytes / 1_048_576,
                rt.vram_budget / 1_048_576
            ));
        }
        let vram_reservation = Arc::new(
            rt.planetary_vram
                .try_reserve(allocation.reserved_bytes, "motor SAD planetario")?,
        );
        let params = rt.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zas-analysis-sad-params"),
            size: std::mem::size_of::<SadParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mk = |label, size, copy_src| {
            rt.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_DST
                    | if copy_src {
                        wgpu::BufferUsages::COPY_SRC
                    } else {
                        wgpu::BufferUsages::empty()
                    },
                mapped_at_creation: false,
            })
        };
        let reference = mk("zas-analysis-sad-reference", allocation.image_bytes, false);
        let target = mk("zas-analysis-sad-target", allocation.image_bytes, false);
        let points = mk("zas-analysis-sad-points", allocation.point_bytes, false);
        let results = mk("zas-analysis-sad-results", allocation.result_bytes, true);
        let pp = sad_pipelines(rt);
        let bind = rt.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("zas-analysis-sad-bind"),
            layout: &pp.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: reference.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: target.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: points.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: results.as_entire_binding(),
                },
            ],
        });
        Ok(Self {
            w,
            h,
            capacity: capacity.max(1),
            params,
            reference,
            target,
            points,
            results,
            bind,
            _vram_reservation: vram_reservation,
        })
    }
}

const SAD_ENGINE_POOL_MAX_ENTRIES: usize = 4;
const SAD_ENGINE_POOL_MAX_BYTES: u64 = 512 * 1024 * 1024;

fn sad_engine_pool_budget(vram_budget: u64) -> u64 {
    // SAD comparte el adapter con preprocess/enhance/stack. Una cuarta parte
    // permite mantener coarse y fine a la vez sin apropiarse de toda la VRAM.
    (vram_budget / 4).min(SAD_ENGINE_POOL_MAX_BYTES).max(1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SadCacheRecord {
    w: usize,
    h: usize,
    capacity: usize,
    reserved_bytes: u64,
    last_used: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SadCachePlan {
    Reuse(usize),
    Insert { evict: Vec<usize> },
}

/// Decide reutilización/evicción sin tocar wgpu, para que el límite del pool
/// pueda probarse de forma determinista. Un engine mayor que la cuota normal
/// se admite sólo como singleton (siempre que `SadEngine::new` confirme que
/// cabe en la VRAM real); así se conserva compatibilidad sin crecimiento sin
/// límite.
fn plan_sad_cache(
    records: &[SadCacheRecord],
    w: usize,
    h: usize,
    capacity: usize,
    reserved_bytes: u64,
    budget: u64,
    max_entries: usize,
) -> SadCachePlan {
    if let Some((index, _)) = records
        .iter()
        .enumerate()
        .filter(|(_, r)| r.w == w && r.h == h && r.capacity >= capacity)
        .min_by_key(|(_, r)| r.capacity)
    {
        return SadCachePlan::Reuse(index);
    }

    let mut evict = Vec::new();
    // Un capacity nuevo sustituye engines menores de la misma geometría; el
    // mayor también sirve las llamadas pequeñas posteriores.
    for (index, record) in records.iter().enumerate() {
        if record.w == w && record.h == h {
            evict.push(index);
        }
    }
    let effective_budget = budget.max(reserved_bytes);
    let max_entries = max_entries.max(1);
    loop {
        let kept_bytes = records
            .iter()
            .enumerate()
            .filter(|(i, _)| !evict.contains(i))
            .map(|(_, r)| r.reserved_bytes)
            .sum::<u64>();
        let kept_count = records.len().saturating_sub(evict.len());
        if kept_count < max_entries && kept_bytes.saturating_add(reserved_bytes) <= effective_budget
        {
            break;
        }
        let Some((index, _)) = records
            .iter()
            .enumerate()
            .filter(|(i, _)| !evict.contains(i))
            .min_by_key(|(_, r)| r.last_used)
        else {
            break;
        };
        evict.push(index);
    }
    evict.sort_unstable_by(|a, b| b.cmp(a));
    SadCachePlan::Insert { evict }
}

struct SadEngineEntry {
    record: SadCacheRecord,
    engine: SadEngine,
}

#[derive(Default)]
struct SadEnginePool {
    entries: Vec<SadEngineEntry>,
    clock: u64,
}

impl SadEnginePool {
    fn engine_for(
        &mut self,
        rt: &'static crate::gpu_stack::GpuRuntime,
        w: usize,
        h: usize,
        capacity: usize,
    ) -> Result<&SadEngine, String> {
        let capacity = capacity.max(1);
        let allocation = sad_engine_allocation(w, h, capacity)?;
        let records: Vec<_> = self.entries.iter().map(|entry| entry.record).collect();
        let plan = plan_sad_cache(
            &records,
            w,
            h,
            capacity,
            allocation.reserved_bytes,
            sad_engine_pool_budget(rt.vram_budget),
            SAD_ENGINE_POOL_MAX_ENTRIES,
        );
        self.clock = self.clock.wrapping_add(1).max(1);
        let index = match plan {
            SadCachePlan::Reuse(index) => index,
            SadCachePlan::Insert { evict } => {
                for index in evict {
                    self.entries.remove(index);
                }
                let engine = SadEngine::new(rt, w, h, capacity)?;
                self.entries.push(SadEngineEntry {
                    record: SadCacheRecord {
                        w,
                        h,
                        capacity,
                        reserved_bytes: allocation.reserved_bytes,
                        last_used: self.clock,
                    },
                    engine,
                });
                self.entries.len() - 1
            }
        };
        self.entries[index].record.last_used = self.clock;
        Ok(&self.entries[index].engine)
    }
}

static SAD_ENGINE_POOL: std::sync::OnceLock<std::sync::Mutex<SadEnginePool>> =
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

/// A1 (2026-07-17): ventana fina DENSA del SAD global de superficie en GPU.
/// Evalúa las (2r+1)² sumas SAD enteras de una caja `roi_w×roi_h` (esquina
/// `roi_x,roi_y`) contra el target desplazado `guess+(dx,dy)`, y devuelve el
/// argmin con EXACTAMENTE la política del barrido CPU de referencia
/// (`find_best_match_sad_internal`): dy exterior desde −r, dx interior desde
/// −r, comparación estricta `<` — primer mínimo global del barrido. Las sumas
/// del kernel son enteras sobre el mismo conjunto de píxeles (origen = centro
/// − caja/2), así que sobre ventanas interiores el trío (dx, dy, sad) es
/// bit-idéntico al de la CPU. Si CUALQUIER candidato no se pudo evaluar
/// (bordes: el kernel es 1 px más estricto que la CPU), devuelve None y el
/// caller usa la ruta CPU completa — la equivalencia deja de ser demostrable.
#[allow(clippy::too_many_arguments)]
pub fn search_sad_dense_window(
    reference: &[u16],
    target: &[u16],
    w: usize,
    h: usize,
    roi_x: usize,
    roi_y: usize,
    roi_w: usize,
    roi_h: usize,
    guess_dx: isize,
    guess_dy: isize,
    radius: i32,
) -> Result<Option<(isize, isize, u64)>, String> {
    if radius < 0 || roi_w == 0 || roi_h == 0 {
        return Ok(None);
    }
    // Convención del kernel: origen muestreado = centro − caja/2. Con centro
    // = esquina + caja/2 el origen reconstruido es EXACTAMENTE la esquina,
    // para caja par e impar (misma división entera truncada).
    let ref_cx = (roi_x + roi_w / 2) as i32;
    let ref_cy = (roi_y + roi_h / 2) as i32;
    let side = (2 * radius + 1) as usize;
    let mut points = Vec::with_capacity(side * side);
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            points.push(SadPoint::rectangular(
                ref_cx,
                ref_cy,
                ref_cx + guess_dx as i32 + dx,
                ref_cy + guess_dy as i32 + dy,
                roi_w as i32,
                roi_h as i32,
                0,
                true,
            ));
        }
    }
    let results = search_sad_points(reference, target, w, h, &points)?;
    if results.len() != points.len() {
        return Ok(None);
    }
    let mut best: Option<(isize, isize, u64)> = None;
    let mut index = 0usize;
    for dy in -(radius as isize)..=(radius as isize) {
        for dx in -(radius as isize)..=(radius as isize) {
            let Some(m) = results[index].as_ref() else {
                return Ok(None);
            };
            index += 1;
            if best.as_ref().is_none_or(|&(_, _, b)| m.sad < b) {
                best = Some((guess_dx + dx, guess_dy + dy, m.sad));
            }
        }
    }
    Ok(best)
}

// ---------------------------------------------------------------------------
// P1 (2026-07-17): blur del enhance de alineación en GPU. SOLO las dos
// pasadas de box blur radio 1 (aritmética ENTERA u32 con división truncada
// por count 2/3, misma que la ruta escalar CPU) viven aquí; la cola f32 con
// noise-gate se queda SIEMPRE en CPU (`enhance_highpass_from_blur`) para no
// depender del modo fast-math del backend (lección Kahan de gpu_stack). Con
// blur bit-idéntico, el enhance completo es bit-idéntico por construcción.
// Patrón PR-30: lock durante upload+submit, staging por llamada, readback
// fuera del lock. Paridad de sesión: `ensure_enhance_parity()`.
// ---------------------------------------------------------------------------

const ENHANCE_WGSL: &str = r#"
struct BlurParams { w: u32, h: u32, dir: u32, pad: u32 }
@group(0) @binding(0) var<uniform> P: BlurParams;
@group(0) @binding(1) var<storage, read> src: array<u32>;
@group(0) @binding(2) var<storage, read_write> dst: array<u32>;

fn src_at(i: u32) -> u32 {
    return (src[i >> 1u] >> ((i & 1u) * 16u)) & 0xffffu;
}

// Cada invocation produce UNA palabra de dst (dos píxeles) para no hacer
// read-modify-write parcial de palabras compartidas entre hilos.
@compute @workgroup_size(256)
fn blur_pass(@builtin(global_invocation_id) gid: vec3<u32>) {
    let n = P.w * P.h;
    if (gid.x >= (n + 1u) / 2u) { return; }
    var packed = 0u;
    for (var k = 0u; k < 2u; k = k + 1u) {
        let i = gid.x * 2u + k;
        if (i >= n) { break; }
        let x = i % P.w;
        let y = i / P.w;
        var sum = 0u;
        var start = 0u;
        var end = 0u;
        if (P.dir == 0u) {
            start = select(x - 1u, 0u, x == 0u);
            end = min(x + 2u, P.w);
            for (var ix = start; ix < end; ix = ix + 1u) {
                sum = sum + src_at(y * P.w + ix);
            }
        } else {
            start = select(y - 1u, 0u, y == 0u);
            end = min(y + 2u, P.h);
            for (var iy = start; iy < end; iy = iy + 1u) {
                sum = sum + src_at(iy * P.w + x);
            }
        }
        let v = sum / (end - start);
        packed = packed | (v << (k * 16u));
    }
    dst[gid.x] = packed;
}
"#;

struct EnhancePipelines {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

static ENHANCE_PIPELINES: std::sync::OnceLock<EnhancePipelines> = std::sync::OnceLock::new();

fn enhance_pipelines(rt: &'static crate::gpu_stack::GpuRuntime) -> &'static EnhancePipelines {
    ENHANCE_PIPELINES.get_or_init(|| {
        let module = rt
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("zas-enhance-blur-shader"),
                source: wgpu::ShaderSource::Wgsl(ENHANCE_WGSL.into()),
            });
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = rt
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("zas-enhance-blur-layout"),
                entries: &[
                    entry(0, wgpu::BufferBindingType::Uniform),
                    entry(1, wgpu::BufferBindingType::Storage { read_only: true }),
                    entry(2, wgpu::BufferBindingType::Storage { read_only: false }),
                ],
            });
        let pl = rt
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("zas-enhance-blur-pipeline-layout"),
                bind_group_layouts: &[&layout],
                push_constant_ranges: &[],
            });
        let pipeline = rt
            .device
            .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("zas-enhance-blur-pipeline"),
                layout: Some(&pl),
                module: &module,
                entry_point: Some("blur_pass"),
                compilation_options: Default::default(),
                cache: None,
            });
        EnhancePipelines { pipeline, layout }
    })
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BlurParams {
    w: u32,
    h: u32,
    dir: u32,
    pad: u32,
}

struct EnhanceEngine {
    w: usize,
    h: usize,
    src: wgpu::Buffer,
    tmp: wgpu::Buffer,
    dst: wgpu::Buffer,
    bind_h: wgpu::BindGroup,
    bind_v: wgpu::BindGroup,
    _vram_reservation: crate::gpu_stack::PlanetaryVramReservation,
}

impl EnhanceEngine {
    fn new(rt: &'static crate::gpu_stack::GpuRuntime, w: usize, h: usize) -> Result<Self, String> {
        let image_bytes = w
            .checked_mul(h)
            .ok_or("Plano de enhance demasiado grande")?
            .div_ceil(2) as u64
            * 4;
        let persistent_bytes = image_bytes.saturating_mul(3).saturating_add(4096);
        let peak_bytes = persistent_bytes.saturating_add(image_bytes);
        if image_bytes > rt.max_binding || peak_bytes > rt.vram_budget {
            return Err(format!(
                "Blur GPU requiere {} MB y el presupuesto es {} MB",
                peak_bytes / 1_048_576,
                rt.vram_budget / 1_048_576
            ));
        }
        let vram_reservation = rt
            .planetary_vram
            .try_reserve(persistent_bytes, "motor enhance planetario")?;
        let mk = |label: &str, copy_dst: bool, copy_src: bool| {
            rt.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: image_bytes,
                usage: wgpu::BufferUsages::STORAGE
                    | if copy_dst {
                        wgpu::BufferUsages::COPY_DST
                    } else {
                        wgpu::BufferUsages::empty()
                    }
                    | if copy_src {
                        wgpu::BufferUsages::COPY_SRC
                    } else {
                        wgpu::BufferUsages::empty()
                    },
                mapped_at_creation: false,
            })
        };
        let src = mk("zas-enhance-src", true, false);
        let tmp = mk("zas-enhance-tmp", false, false);
        let dst = mk("zas-enhance-dst", false, true);
        let mk_params = |dir: u32| {
            let buffer = rt.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("zas-enhance-params"),
                size: std::mem::size_of::<BlurParams>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let params = BlurParams {
                w: w as u32,
                h: h as u32,
                dir,
                pad: 0,
            };
            rt.queue
                .write_buffer(&buffer, 0, bytemuck::bytes_of(&params));
            buffer
        };
        let params_h = mk_params(0);
        let params_v = mk_params(1);
        let pp = enhance_pipelines(rt);
        let mk_bind = |params: &wgpu::Buffer, pass_src: &wgpu::Buffer, pass_dst: &wgpu::Buffer| {
            rt.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("zas-enhance-bind"),
                layout: &pp.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: pass_src.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: pass_dst.as_entire_binding(),
                    },
                ],
            })
        };
        let bind_h = mk_bind(&params_h, &src, &tmp);
        let bind_v = mk_bind(&params_v, &tmp, &dst);
        Ok(Self {
            w,
            h,
            src,
            tmp,
            dst,
            bind_h,
            bind_v,
            _vram_reservation: vram_reservation,
        })
    }
}

static ENHANCE_ENGINE: std::sync::OnceLock<std::sync::Mutex<Option<EnhanceEngine>>> =
    std::sync::OnceLock::new();
// El staging de readback también consume VRAM. Serializar la llamada completa
// mantiene un único staging vivo y evita que la concurrencia de frames exceda
// silenciosamente el presupuesto aunque el engine persistente sea compartido.
static ENHANCE_CALL_GATE: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();

/// Libera caches planetarios únicamente cuando cada motor está ocioso. Se usa
/// antes de un acumulador grande para que buffers de una etapa ya terminada no
/// fuercen un fallback CPU artificial. Todos los locks son `try_lock`: esta
/// función jamás espera ni puede formar un ciclo con una operación activa.
pub(crate) fn evict_idle_planetary_gpu_caches() {
    if let Some(mx) = BATCH_ENGINE.get() {
        if let Ok(mut cache) = mx.try_lock() {
            drop(cache.take());
        }
    }
    if let Some(mx) = SAD_ENGINE_POOL.get() {
        if let Ok(mut pool) = mx.try_lock() {
            pool.entries.clear();
        }
    }
    // Enhance suelta el mutex del engine antes del readback, pero conserva el
    // call gate durante toda la llamada. Exigir ambos garantiza que no se
    // libere la reserva mientras el command buffer aún usa sus buffers.
    if let Some(call_gate) = ENHANCE_CALL_GATE.get() {
        if let Ok(_call) = call_gate.try_lock() {
            if let Some(mx) = ENHANCE_ENGINE.get() {
                if let Ok(mut cache) = mx.try_lock() {
                    drop(cache.take());
                }
            }
        }
    }
}

/// Blur radio 1 (dos pasadas enteras) en GPU, bit-idéntico a
/// `crate::alignment::box_blur_r1_edge_aware`. `out` recibe exactamente
/// `w*h` muestras. Errores → el caller usa la ruta CPU completa.
pub fn gpu_box_blur_r1_into(
    input: &[u16],
    w: usize,
    h: usize,
    out: &mut Vec<u16>,
) -> Result<(), String> {
    let _call_guard = ENHANCE_CALL_GATE
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .map_err(|_| "Mutex de presupuesto GPU de enhance dañado")?;
    let n = w
        .checked_mul(h)
        .ok_or("Geometría de blur demasiado grande")?;
    if n == 0 {
        return Err("Blur GPU con imagen vacía".into());
    }
    if input.len() < n {
        return Err("Blur GPU con frame truncado".into());
    }
    let words = n.div_ceil(2);
    let workgroups = words.div_ceil(256);
    if workgroups > 65_535 {
        // Límite del dispatch 1D de wgpu; imágenes >~34 Mpx mono siguen en CPU.
        return Err("Blur GPU: la imagen excede el dispatch 1D".into());
    }
    let rt = crate::gpu_stack::gpu_runtime().ok_or("No hay runtime wgpu")?;
    let error_epoch = crate::gpu_stack::begin_gpu_operation();
    if crate::gpu_stack::gpu_error_since(error_epoch) {
        return Err("El device GPU se perdió antes del blur".into());
    }
    let mx = ENHANCE_ENGINE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = mx.lock().map_err(|_| "Mutex GPU de enhance dañado")?;
    let rebuild = guard.as_ref().is_none_or(|e| e.w != w || e.h != h);
    if rebuild {
        // Liberar la geometría anterior antes de reservar la nueva. Crear
        // primero ambas simultáneamente produciría un falso OOM del contador.
        drop(guard.take());
        *guard = Some(EnhanceEngine::new(rt, w, h)?);
    }
    let engine = guard.as_ref().unwrap();
    write_packed_u16(&rt.queue, &engine.src, &input[..n]);
    // Staging POR LLAMADA, pero con una única llamada activa: el gate exterior
    // lo contabiliza como parte del presupuesto de VRAM hasta terminar readback.
    let _staging_vram_reservation = rt
        .planetary_vram
        .try_reserve((words * 4) as u64, "readback enhance planetario")?;
    let staging = rt.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("zas-enhance-staging"),
        size: (words * 4) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let pp = enhance_pipelines(rt);
    let mut enc = rt
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("zas-enhance-encoder"),
        });
    for bind in [&engine.bind_h, &engine.bind_v] {
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("zas-enhance-blur-pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pp.pipeline);
        pass.set_bind_group(0, bind, &[]);
        pass.dispatch_workgroups(workgroups as u32, 1, 1);
    }
    enc.copy_buffer_to_buffer(&engine.dst, 0, &staging, 0, (words * 4) as u64);
    rt.queue.submit(Some(enc.finish()));
    drop(guard);

    let slice = staging.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    crate::gpu_stack::wait_for_readback_since(
        &rt.device,
        &rx,
        "readback del blur de enhance",
        error_epoch,
    )?;
    {
        let mapped = slice.get_mapped_range();
        let packed: &[u32] = bytemuck::cast_slice(&mapped);
        out.clear();
        out.reserve(n);
        for &word in packed.iter().take(words) {
            out.push((word & 0xffff) as u16);
            if out.len() < n {
                out.push((word >> 16) as u16);
            }
        }
    }
    staging.unmap();
    if crate::gpu_stack::gpu_error_since(error_epoch) {
        return Err("Device loss/OOM durante el blur de enhance".into());
    }
    if out.len() != n {
        return Err("Blur GPU devolvió un plano incompleto".into());
    }
    Ok(())
}

static ENHANCE_PARITY: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Self-test de sesión del blur GPU (como `ensure_parity` del análisis):
/// imagen determinista de dimensiones IMPARES (bordes + palabra a medias)
/// comparada bit a bit contra la referencia CPU. Falla → enhance en CPU toda
/// la sesión.
pub fn ensure_enhance_parity() -> bool {
    use std::sync::atomic::Ordering;
    match ENHANCE_PARITY.load(Ordering::Acquire) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    let w = 129usize;
    let h = 67usize;
    let mono: Vec<u16> = (0..w * h)
        .map(|i| ((i * 137 + (i / w) * 79) % 65535) as u16)
        .collect();
    let mut tmp = Vec::new();
    let mut cpu = Vec::new();
    crate::alignment::box_blur_r1_edge_aware(&mono, w, h, &mut tmp, &mut cpu);
    let mut gpu = Vec::new();
    let ok = gpu_box_blur_r1_into(&mono, w, h, &mut gpu).is_ok() && gpu[..w * h] == cpu[..w * h];
    ENHANCE_PARITY.store(if ok { 1 } else { 2 }, Ordering::Release);
    if !ok {
        eprintln!(
            "[gpu-enhance] paridad blur GPU/CPU no disponible; el enhance sigue en CPU (resultado idéntico)"
        );
    }
    ok
}

/// Ejecuta búsquedas SAD gruesas para todos los AP. El caso normal usa un solo
/// dispatch; cargas grandes se dividen en command buffers acotados por TDR.
/// Los mapas deben tener geometría idéntica y normalmente son pirámides 4×.
const MAX_SAD_WORK_PER_INVOCATION: u64 = 20_000_000;

fn sad_point_work(point: &SadPoint) -> u64 {
    if point.enabled == 0 || point.box_w <= 0 || point.box_h <= 0 {
        return 0;
    }
    let diameter = (point.search_r.max(0) as u64)
        .saturating_mul(2)
        .saturating_add(1);
    (point.box_w as u64)
        .saturating_mul(point.box_h as u64)
        .saturating_mul(diameter.saturating_mul(diameter))
}

fn sad_submit_ranges(points: &[SadPoint], budget: u64) -> Result<Vec<(usize, usize)>, String> {
    let budget = budget.max(1);
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut work = 0u64;
    for (i, point) in points.iter().enumerate() {
        let point_work = sad_point_work(point);
        if point_work > MAX_SAD_WORK_PER_INVOCATION {
            return Err(format!(
                "SAD GPU serial patológico en punto {i} ({point_work} muestras); use la búsqueda paralela por desplazamiento"
            ));
        }
        if i > start && work.saturating_add(point_work) > budget {
            ranges.push((start, i));
            start = i;
            work = 0;
        }
        work = work.saturating_add(point_work);
    }
    ranges.push((start, points.len()));
    Ok(ranges)
}

#[derive(Clone, Copy)]
pub struct SadBatchRequest<'a> {
    pub target: &'a [u16],
    pub points: &'a [SadPoint],
}

impl<'a> SadBatchRequest<'a> {
    pub fn new(target: &'a [u16], points: &'a [SadPoint]) -> Self {
        Self { target, points }
    }
}

fn sad_batch_ranges(
    result_bytes: &[u64],
    target_cap: usize,
    readback_budget: u64,
    max_buffer_size: u64,
) -> Result<Vec<(usize, usize)>, String> {
    if result_bytes.is_empty() {
        return Ok(Vec::new());
    }
    let target_cap = target_cap.max(1);
    let readback_budget = readback_budget.max(16);
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut bytes = 0u64;
    for (index, &item_bytes) in result_bytes.iter().enumerate() {
        if item_bytes > max_buffer_size {
            return Err(format!(
                "Readback SAD del target {index} excede max_buffer_size"
            ));
        }
        let target_limit = index.saturating_sub(start) >= target_cap;
        let byte_limit = index > start && bytes.saturating_add(item_bytes) > readback_budget;
        if target_limit || byte_limit {
            ranges.push((start, index));
            start = index;
            bytes = 0;
        }
        bytes = bytes
            .checked_add(item_bytes)
            .ok_or("Readback SAD por lote demasiado grande")?;
    }
    ranges.push((start, result_bytes.len()));
    Ok(ranges)
}

fn decode_sad_matches(raw: &[u32], count: usize) -> Vec<Option<SadMatch>> {
    let mut out = Vec::with_capacity(count);
    for r in raw.chunks_exact(4).take(count) {
        let sad = ((r[3] as u64) << 32) | r[2] as u64;
        if sad == u64::MAX {
            out.push(None);
        } else {
            out.push(Some(SadMatch {
                dx: r[0] as i32,
                dy: r[1] as i32,
                sad,
            }));
        }
    }
    out
}

fn search_sad_points_batch_chunk(
    reference: &[u16],
    requests: &[SadBatchRequest<'_>],
    w: usize,
    h: usize,
    rt: &'static crate::gpu_stack::GpuRuntime,
) -> Result<Vec<Vec<Option<SadMatch>>>, String> {
    if requests.iter().all(|request| request.points.is_empty()) {
        return Ok(requests.iter().map(|_| Vec::new()).collect());
    }
    let n = w.checked_mul(h).ok_or("Mapa SAD demasiado grande")?;
    let capacity = requests
        .iter()
        .map(|request| request.points.len())
        .max()
        .unwrap_or(1)
        .max(1)
        .checked_next_power_of_two()
        .ok_or("Demasiados puntos SAD")?;
    let result_sizes: Vec<u64> = requests
        .iter()
        .map(|request| {
            (request.points.len() as u64)
                .checked_mul(16)
                .ok_or_else(|| "Demasiados resultados SAD".to_string())
        })
        .collect::<Result<_, _>>()?;
    let staging_bytes = result_sizes.iter().try_fold(0u64, |total, &bytes| {
        total
            .checked_add(bytes)
            .ok_or_else(|| "Readback SAD por lote demasiado grande".to_string())
    })?;
    if staging_bytes > rt.max_buffer_size {
        return Err(format!(
            "Readback SAD por lote requiere {} MB y max_buffer_size permite {} MB",
            staging_bytes / 1_048_576,
            rt.max_buffer_size / 1_048_576
        ));
    }

    let error_epoch = crate::gpu_stack::begin_gpu_operation();
    if crate::gpu_stack::gpu_error_since(error_epoch) {
        return Err("El device GPU se perdió antes de iniciar SAD".into());
    }
    let work_budget = if rt.backend == "Metal" {
        600_000_000u64
    } else {
        200_000_000u64
    };
    // Valida TODOS los targets antes de enviar el primero: un punto serial
    // patológico no puede dejar un prefijo del batch ejecutado.
    let submit_ranges: Vec<Vec<(usize, usize)>> = requests
        .iter()
        .map(|request| sad_submit_ranges(request.points, work_budget))
        .collect::<Result<_, _>>()?;

    let _staging_vram_reservation = rt
        .planetary_vram
        .try_reserve(staging_bytes.max(16), "readback SAD planetario")?;
    let call_staging = rt.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("zas-analysis-sad-staging-batch"),
        size: staging_bytes.max(16),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mx = SAD_ENGINE_POOL.get_or_init(|| std::sync::Mutex::new(SadEnginePool::default()));
    let mut guard = mx.lock().map_err(|_| "Mutex GPU SAD dañado")?;
    let e = guard.engine_for(rt, w, h, capacity)?;
    let _engine_vram_lease = Arc::clone(&e._vram_reservation);
    write_packed_u16(&rt.queue, &e.reference, &reference[..n]);
    let pp = sad_pipelines(rt);
    let mut readback_offset = 0u64;

    // Diseño deliberadamente conservador: cada target termina su submit y
    // copia antes de que el siguiente sobrescriba target/points/results. La
    // Queue garantiza el orden; todos comparten UN map/readback al final. No
    // se construye un command buffer multiframe gigante, preservando los
    // cortes de trabajo que protegen Metal y el TDR de Windows.
    for ((request, ranges), &used_bytes) in requests
        .iter()
        .zip(submit_ranges.iter())
        .zip(result_sizes.iter())
    {
        if request.points.is_empty() {
            continue;
        }
        write_packed_u16(&rt.queue, &e.target, &request.target[..n]);
        rt.queue
            .write_buffer(&e.points, 0, bytemuck::cast_slice(request.points));
        for (range_index, &(start, end)) in ranges.iter().enumerate() {
            rt.queue.write_buffer(
                &e.params,
                0,
                bytemuck::bytes_of(&SadParams {
                    w: w as u32,
                    h: h as u32,
                    // `count` es el fin exclusivo; `start` conserva el índice
                    // global y evita que el padding invada el siguiente rango.
                    count: end as u32,
                    start: start as u32,
                }),
            );
            let mut enc = rt
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("zas-analysis-sad-batch-encoder"),
                });
            {
                let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("zas-analysis-sad-batch-pass"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(&pp.pipeline);
                pass.set_bind_group(0, &e.bind, &[]);
                pass.dispatch_workgroups(((end - start) as u32).div_ceil(64), 1, 1);
            }
            if range_index + 1 == ranges.len() {
                enc.copy_buffer_to_buffer(
                    &e.results,
                    0,
                    &call_staging,
                    readback_offset,
                    used_bytes,
                );
            }
            rt.queue.submit(Some(enc.finish()));
        }
        readback_offset += used_bytes;
    }
    debug_assert_eq!(readback_offset, staging_bytes);
    // La copia del lote ya quedó ordenada en la Queue. Otro caller puede usar
    // el engine mientras éste espera, sin alterar el staging exclusivo.
    drop(guard);

    let slice = call_staging.slice(0..staging_bytes);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    crate::gpu_stack::wait_for_readback_since(
        &rt.device,
        &rx,
        "readback SAD GPU por lote",
        error_epoch,
    )?;
    let out = {
        let mapped = slice.get_mapped_range();
        let raw: &[u32] = bytemuck::cast_slice(&mapped);
        let mut offset_words = 0usize;
        let mut out = Vec::with_capacity(requests.len());
        for request in requests {
            let words = request.points.len() * 4;
            out.push(decode_sad_matches(
                &raw[offset_words..offset_words + words],
                request.points.len(),
            ));
            offset_words += words;
        }
        out
    };
    call_staging.unmap();
    if crate::gpu_stack::gpu_error_since(error_epoch) {
        return Err("Device loss/OOM durante SAD GPU por lote".into());
    }
    Ok(out)
}

/// Ejecuta varios targets que comparten referencia y geometría. El API acota
/// cada lote por backend/tamaño y por bytes de readback; dentro de cada lote
/// conserva submits ordenados y realiza un solo punto de coordinación (`map`).
/// Las sumas u64, el orden de puntos y la política de empate del shader no se
/// modifican. El límite actual NO fusiona varios targets en un command buffer:
/// hacerlo requeriría buffers target/result por slot y se habilitará sólo tras
/// medir VRAM y TDR en hardware Windows/Metal.
pub fn search_sad_points_batch(
    reference: &[u16],
    requests: &[SadBatchRequest<'_>],
    w: usize,
    h: usize,
) -> Result<Vec<Vec<Option<SadMatch>>>, String> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    if requests.iter().all(|request| request.points.is_empty()) {
        return Ok(requests.iter().map(|_| Vec::new()).collect());
    }
    let n = w.checked_mul(h).ok_or("Mapa SAD demasiado grande")?;
    if w > u32::MAX as usize || h > u32::MAX as usize {
        return Err("Geometría SAD excede el contrato u32 del shader".into());
    }
    if reference.len() < n
        || requests
            .iter()
            .any(|request| !request.points.is_empty() && request.target.len() < n)
    {
        return Err("Mapa SAD GPU truncado".into());
    }
    if requests
        .iter()
        .any(|request| request.points.len() > u32::MAX as usize)
    {
        return Err("Geometría SAD excede el contrato u32 del shader".into());
    }
    let result_sizes: Vec<u64> = requests
        .iter()
        .map(|request| {
            (request.points.len() as u64)
                .checked_mul(16)
                .ok_or_else(|| "Demasiados resultados SAD".to_string())
        })
        .collect::<Result<_, _>>()?;
    let rt = crate::gpu_stack::gpu_runtime().ok_or("No hay runtime wgpu")?;
    let target_cap = crate::gpu_stack::analysis_submit_cap(w, h).clamp(1, 8);
    let readback_budget = (rt.vram_budget / 64)
        .max(16)
        .min(64 * 1024 * 1024)
        .min(rt.max_buffer_size);
    let batch_ranges = sad_batch_ranges(
        &result_sizes,
        target_cap,
        readback_budget,
        rt.max_buffer_size,
    )?;
    let mut out = Vec::with_capacity(requests.len());
    for (start, end) in batch_ranges {
        out.extend(search_sad_points_batch_chunk(
            reference,
            &requests[start..end],
            w,
            h,
            rt,
        )?);
    }
    Ok(out)
}

pub fn search_sad_points(
    reference: &[u16],
    target: &[u16],
    w: usize,
    h: usize,
    points: &[SadPoint],
) -> Result<Vec<Option<SadMatch>>, String> {
    let mut batch =
        search_sad_points_batch(reference, &[SadBatchRequest::new(target, points)], w, h)?;
    Ok(batch.pop().unwrap_or_default())
}

/// Gate de sesión específico del kernel SAD por AP. La paridad del
/// preprocesado no demuestra este shader: aquí se comparan desplazamiento,
/// desempate y acumulación u64 exacta contra el oráculo CPU antes de permitir
/// que CoarseSad aparezca como apto en un plan de producción.
pub fn ensure_sad_parity() -> bool {
    match SAD_PARITY.load(Ordering::Acquire) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    let (w, h) = (96usize, 72usize);
    let reference: Vec<u16> = (0..w * h)
        .map(|i| {
            let x = i % w;
            let y = i / w;
            (((x * 977 + y * 613 + x * y * 17) ^ (x << 7) ^ (y << 5)) & 0xffff) as u16
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
        SadPoint::new(30, 24, 30, 24, 18, 6, true),
        SadPoint::new(64, 46, 64, 46, 24, 6, true),
        SadPoint::new(48, 36, 48, 36, 20, 6, false),
    ];
    let gpu = match search_sad_points(&reference, &target, w, h, &points) {
        Ok(gpu) => gpu,
        Err(error) => {
            eprintln!("[gpu-sad parity] fallo de transporte: {error}");
            return false;
        }
    };
    let translated_ok = gpu.len() == points.len()
        && points.iter().zip(gpu.iter()).all(|(point, got)| {
            if point.enabled == 0 {
                return got.is_none();
            }
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
            got.as_ref().is_some_and(|got| {
                (got.dx, got.dy, got.sad) == (dx as i32, dy as i32, sad)
                    && (got.dx, got.dy) == (truth_dx, truth_dy)
            })
        });
    // Extremos exclusivos exactamente en width/height son válidos en CPU.
    // Incluye caja par e impar para mantener idéntica la división del centro.
    let edge_points = vec![
        SadPoint::new((w - 9) as i32, 30, (w - 9) as i32, 30, 18, 0, true),
        SadPoint::new(48, (h - 9) as i32, 48, (h - 9) as i32, 17, 0, true),
    ];
    let edge_ok = search_sad_points(&reference, &reference, w, h, &edge_points)
        .ok()
        .is_some_and(|matches| {
            matches.len() == edge_points.len()
                && matches
                    .iter()
                    .zip(edge_points.iter())
                    .all(|(value, point)| {
                        let (dx, dy, sad) = crate::alignment::find_best_match_sad(
                            &reference,
                            &reference,
                            w,
                            point.ref_x as usize,
                            point.ref_y as usize,
                            point.tgt_x as usize,
                            point.tgt_y as usize,
                            point.box_w as usize,
                            point.search_r,
                        );
                        value.as_ref().is_some_and(|gpu| {
                            (gpu.dx, gpu.dy, gpu.sad) == (dx as i32, dy as i32, sad)
                                && (gpu.dx, gpu.dy, gpu.sad) == (0, 0, 0)
                        })
                    })
        });
    let ok = translated_ok && edge_ok;
    SAD_PARITY.store(if ok { 1 } else { 2 }, Ordering::Release);
    if !ok {
        eprintln!("[gpu-sad parity] el kernel batched no igualó el oráculo CPU");
    }
    ok
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
    [0.12f32, 0.25, 0.45].map(|p| (noise + (peak - noise).max(0.0) * p).max(noise + 100.0))
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
        centers.push((
            (first(xs) + last(xs)) as f32 * 0.5,
            (first(ys) + last(ys)) as f32 * 0.5,
        ));
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
    want_score: bool,
) -> Result<AnalysisGpuOutput, String> {
    // PR-2.3: delega en el motor por LOTES (un submit + un staging + UN solo
    // map_async). El antiguo cuerpo unitario hacía hasta 7 readbacks
    // bloqueantes por frame (half/blur/lap/score/cog_x/cog_y/grid), cada uno
    // con su propio staging buffer y submit — la latencia de sincronización
    // dominaba sobre el cómputo. Mismos kernels y parámetros → bit-idéntico.
    let frame_len = w
        .checked_mul(h)
        .ok_or("Geometría de análisis demasiado grande")?;
    if mono.len() < frame_len {
        return Err("Frame mono truncado".into());
    }
    process_batch_slices(&[mono], w, h, surface_grid, want_grid, want_cog, want_score).map(
        |mut v| {
            v.pop()
                .expect("el lote de un frame produce exactamente un output")
        },
    )
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
    want_score: bool,
) -> Result<Vec<AnalysisGpuOutput>, String> {
    let refs: Vec<&[u16]> = frames.iter().map(|f| f.as_slice()).collect();
    process_batch_slices(&refs, w, h, surface_grid, want_grid, want_cog, want_score)
}

/// Sube muestras u16 al storage `array<u32>` sin la expansión/copia temporal
/// u16→u32. En targets little-endian (todos los soportados por la app) dos
/// muestras contiguas ya tienen exactamente el layout que desempaqueta WGSL.
/// Un último píxel impar se completa con una escritura de cuatro bytes.
fn write_packed_u16(queue: &wgpu::Queue, dst: &wgpu::Buffer, values: &[u16]) {
    #[cfg(target_endian = "little")]
    {
        let paired = values.len() & !1;
        if paired != 0 {
            queue.write_buffer(dst, 0, bytemuck::cast_slice(&values[..paired]));
        }
        if paired != values.len() {
            let sample = values[paired].to_le_bytes();
            let tail = [sample[0], sample[1], 0, 0];
            queue.write_buffer(dst, (paired * 2) as u64, &tail);
        }
    }
    #[cfg(target_endian = "big")]
    {
        // Ruta portable de respaldo; no se compila en macOS/Windows/Linux
        // soportados actualmente, pero mantiene el contrato explícito.
        let packed: Vec<u32> = values
            .chunks(2)
            .map(|p| p[0] as u32 | ((p.get(1).copied().unwrap_or(0) as u32) << 16))
            .collect();
        queue.write_buffer(dst, 0, bytemuck::cast_slice(&packed));
    }
}

fn process_batch_slices(
    frames: &[&[u16]],
    w: usize,
    h: usize,
    surface_grid: bool,
    want_grid: bool,
    want_cog: bool,
    want_score: bool,
) -> Result<Vec<AnalysisGpuOutput>, String> {
    if frames.is_empty() {
        return Ok(Vec::new());
    }
    let frame_len = w
        .checked_mul(h)
        .ok_or("Geometría de análisis demasiado grande")?;
    // PR-2.3: el lote de UN frame ya NO delega en el antiguo camino unitario
    // (que hacía 7 readbacks bloqueantes); el motor por lotes lo sirve con
    // un submit + un staging + un solo map también para len == 1.
    // PR-2.4: el tope por submit depende del backend — fuera de Metal se
    // acota por presupuesto de TIEMPO (TDR de Windows), no solo por VRAM.
    let submit_cap = crate::gpu_stack::analysis_submit_cap(w, h);
    if frames.len() > submit_cap {
        let mut out = Vec::with_capacity(frames.len());
        for chunk in frames.chunks(submit_cap) {
            out.extend(process_batch_slices(
                chunk,
                w,
                h,
                surface_grid,
                want_grid,
                want_cog,
                want_score,
            )?);
        }
        return Ok(out);
    }
    if frames.iter().any(|f| f.len() < frame_len) {
        return Err("Lote planetario contiene un frame mono truncado".into());
    }

    let rt = crate::gpu_stack::gpu_runtime().ok_or("No hay runtime wgpu")?;
    // En superficie `process_analysis_frame` usa blur para calidad/grilla y
    // lap para SAD; el mapa half sólo alimenta el camino planetario/CoG.
    let layout = BatchReadbackLayout::new(w, h, want_cog, want_grid, !surface_grid, want_score);
    let memory_cap = max_batch_len_for_limits(
        w,
        h,
        frames.len(),
        layout.stride,
        rt.vram_budget,
        rt.max_buffer_size,
    );
    if memory_cap == 0 {
        return Err("Ni un lote mínimo de análisis GPU cabe en los límites del adapter".into());
    }
    if frames.len() > memory_cap {
        let mut out = Vec::with_capacity(frames.len());
        for chunk in frames.chunks(memory_cap) {
            out.extend(process_batch_slices(
                chunk,
                w,
                h,
                surface_grid,
                want_grid,
                want_cog,
                want_score,
            )?);
        }
        return Ok(out);
    }

    // Los umbrales son CPU y pueden prepararse fuera del mutex del motor. Así,
    // callers concurrentes solapan esta pasada mientras la GPU termina el lote
    // anterior en vez de serializar también el trabajo de host.
    use rayon::prelude::*;
    let thresholds: Vec<[f32; 3]> = frames
        .par_iter()
        .map(|mono| {
            if want_cog {
                cog_thresholds(mono, w, h)
            } else {
                [0.0; 3]
            }
        })
        .collect();

    let error_epoch = crate::gpu_stack::begin_gpu_operation();
    if crate::gpu_stack::gpu_error_since(error_epoch) {
        return Err("El device GPU se perdió antes de iniciar el lote de análisis".into());
    }
    let capacity = frames.len().clamp(2, 8);
    let mx = BATCH_ENGINE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = mx.lock().map_err(|_| "Mutex GPU de lotes dañado")?;
    let rebuild = guard.as_ref().is_none_or(|e| {
        e.w != w
            || e.h != h
            || e.capacity < frames.len()
            || e.staging_words_per_slot < layout.stride
    });
    if rebuild {
        // La cache vieja debe liberar su token antes de reservar la nueva;
        // ambas geometrías nunca se necesitan a la vez.
        drop(guard.take());
        *guard = Some(BatchEngine::new(rt, w, h, capacity, layout.stride)?);
    }
    let batch = guard.as_ref().unwrap();
    let pp = pipelines(rt);
    let hw = w / 2;
    let hh = h / 2;
    let mut enc = rt
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("zas-analysis-batch-encoder"),
        });
    for (slot_index, mono) in frames.iter().enumerate() {
        let slot = &batch.slots[slot_index];
        // Upload directo: evita un Vec<u32> y una pasada/memcpy completa por
        // frame (≈24 MB ahorrados por frame 4K mono), sin cambiar un bit.
        write_packed_u16(&rt.queue, &slot.src, &mono[..frame_len]);
        let thresholds = thresholds[slot_index];
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
        rt.queue
            .write_buffer(&slot.params, 0, bytemuck::bytes_of(&params));
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
        if want_score {
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

        let base = layout.stride * slot_index * 4;
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
        // Compacta dos u16 por cada palabra de staging. Los tres dispatches
        // reutilizan `temp`; el copy intermedio queda ordenado dentro del mismo
        // command buffer antes de que el siguiente pack lo sobrescriba.
        let mut pack_and_copy = |pipeline: &wgpu::ComputePipeline, word_offset: usize| {
            {
                let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("zas-analysis-pack-u16"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &slot.bind, &[]);
                pass.dispatch_workgroups((layout.packed_image_words as u32).div_ceil(256), 1, 1);
            }
            copy(&mut enc, &slot.temp, word_offset, layout.packed_image_words);
        };
        if layout.want_half {
            pack_and_copy(&pp.pack_half, layout.half);
        }
        pack_and_copy(&pp.pack_blur, layout.blur);
        pack_and_copy(&pp.pack_lap, layout.lap);
        if want_score {
            copy(&mut enc, &slot.score_rows, layout.score, hh * 2);
        }
        if want_cog {
            copy(&mut enc, &slot.cog_x, layout.cog_x, w * 3);
            copy(&mut enc, &slot.cog_y, layout.cog_y, h * 3);
        }
        if want_grid {
            copy(&mut enc, &slot.grid, layout.grid, 40 * 40);
        }
    }

    rt.queue.submit(Some(enc.finish()));
    let used_bytes = (layout.stride * frames.len() * 4) as u64;
    let slice = batch.staging.slice(0..used_bytes);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    crate::gpu_stack::wait_for_readback_since(
        &rt.device,
        &rx,
        "readback del lote de análisis",
        error_epoch,
    )?;
    let outputs = {
        let mapped = slice.get_mapped_range();
        let words: &[u32] = bytemuck::cast_slice(&mapped);
        let mut outputs = Vec::with_capacity(frames.len());
        for slot_index in 0..frames.len() {
            let base = layout.stride * slot_index;
            let at = |off: usize, len: usize| &words[base + off..base + off + len];
            outputs.push(finish_analysis_output(
                layout
                    .want_half
                    .then(|| at(layout.half, layout.packed_image_words)),
                at(layout.blur, layout.packed_image_words),
                at(layout.lap, layout.packed_image_words),
                want_score.then(|| at(layout.score, hh * 2)),
                want_cog.then(|| at(layout.cog_x, w * 3)),
                want_cog.then(|| at(layout.cog_y, h * 3)),
                want_grid.then(|| at(layout.grid, 40 * 40)),
                w,
                h,
            ));
        }
        outputs
    };
    batch.staging.unmap();
    if crate::gpu_stack::gpu_error_since(error_epoch) {
        return Err("Device loss/OOM durante el lote de análisis GPU".into());
    }
    drop(guard);
    Ok(outputs)
}

pub fn ensure_parity() -> bool {
    match PARITY.load(Ordering::Acquire) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    let w = 96usize;
    let h = 72usize;
    let mono: Vec<u16> = (0..w * h)
        .map(|i| ((i * 137 + (i / w) * 79) % 65535) as u16)
        .collect();
    let gpu = process_with_options(&mono, w, h, true, true, true, true);
    let gpu_planet_grid = process_with_options(&mono, w, h, false, true, false, true);
    let batch_frames = vec![
        mono.clone(),
        mono.iter().map(|&v| v.saturating_add(731)).collect(),
        mono.iter().map(|&v| v.saturating_sub(419)).collect(),
    ];
    let batch = process_batch_with_options(&batch_frames, w, h, true, true, true, true);
    let singles: Result<Vec<_>, _> = batch_frames
        .iter()
        .map(|f| process_with_options(f, w, h, true, true, true, true))
        .collect();
    let mut half = Vec::new();
    let (hw, hh) = crate::alignment::downscale_2x_into(&mono, w, h, &mut half);
    let mut temp = vec![0u16; hw * hh];
    let mut blur = vec![0u16; hw * hh];
    let mut lap = vec![0u16; hw * hh];
    // Referencia CPU en CRUDO: el shader no normaliza y finish_analysis_output
    // ya no normaliza tampoco (el scorer v2 necesita magnitudes reales).
    let score = crate::enhance_and_lap_raw(&half, hw, hh, &mut temp, &mut blur, &mut lap);
    let cpu_center = crate::compute_robust_geometric_center(&mono, w, h, 0, 0);
    let cpu_surface_grid = crate::calculate_grid_quality(&blur, hw, hh, 40, true);
    let cpu_planet_grid = crate::calculate_grid_quality(&half, hw, hh, 40, false);
    let grid_close = |gpu: &[u64], cpu: &[u64]| {
        gpu.len() == cpu.len()
            && gpu
                .iter()
                .zip(cpu)
                .all(|(&a, &b)| a.abs_diff(b) <= ((b as f64 * 0.002).ceil() as u64).max(4096))
    };
    let ok_surface = gpu.as_ref().ok().is_some_and(|g| {
        let center_ok = g.geometric_center.is_some_and(|c| {
            (c.0 - cpu_center.0).abs() <= 0.51 && (c.1 - cpu_center.1).abs() <= 0.51
        });
        // La ruta superficie no devuelve half: ningún consumidor de esa ruta
        // lo usa. El mismo kernel se valida abajo mediante la ruta planetaria.
        g.half.is_empty()
            && g.blurred == blur
            && g.laplacian == lap
            && g.score == score
            && center_ok
            && g.grid_scores
                .as_deref()
                .is_some_and(|m| grid_close(m, &cpu_surface_grid))
    });
    let ok_planet = gpu_planet_grid.as_ref().ok().is_some_and(|g| {
        g.half == half
            && g.grid_scores
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
    let batch_ok = batch
        .as_ref()
        .ok()
        .zip(singles.as_ref().ok())
        .is_some_and(|(a, b)| {
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
                m.iter()
                    .zip(&cpu_surface_grid)
                    .map(|(&a, &b)| a.abs_diff(b))
                    .max()
                    .unwrap_or(0)
            });
            let surface_worst = g.grid_scores.as_deref().and_then(|m| {
                m.iter()
                    .zip(&cpu_surface_grid)
                    .enumerate()
                    .max_by_key(|(_, (&a, &b))| a.abs_diff(b))
                    .map(|(i, (&a, &b))| (i, a, b))
            });
            eprintln!(
                "[gpu-analysis parity] pixels={}/{}/{} score={} cpu_score={} center={:?} cpu_center={:?} surface_grid_diff={:?} worst={:?}",
                g.half.is_empty(),
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
                m.iter()
                    .zip(&cpu_planet_grid)
                    .map(|(&a, &b)| a.abs_diff(b))
                    .max()
                    .unwrap_or(0)
            });
            eprintln!("[gpu-analysis parity] planet_grid_diff={diff:?}");
        }
    }
    let state = if ok {
        1
    } else if transport_failed {
        eprintln!(
            "[gpu-analysis parity] fallo de transporte GPU; se reintentará en el próximo análisis"
        );
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
    fn sad_submit_planner_bounds_total_and_per_invocation_work() {
        let point = super::SadPoint::rectangular(100, 100, 100, 100, 100, 100, 0, true);
        let points = vec![point; 5];
        assert_eq!(super::sad_point_work(&point), 10_000);
        assert_eq!(
            super::sad_submit_ranges(&points, 25_000).unwrap(),
            vec![(0, 2), (2, 4), (4, 5)]
        );

        let pathological = super::SadPoint::rectangular(0, 0, 0, 0, 5_000, 5_000, 0, true);
        assert!(super::sad_submit_ranges(&[pathological], u64::MAX).is_err());
    }

    #[test]
    fn sad_engine_pool_reuses_geometry_and_evicts_within_budget() {
        let records = vec![
            super::SadCacheRecord {
                w: 960,
                h: 540,
                capacity: 128,
                reserved_bytes: 10,
                last_used: 5,
            },
            super::SadCacheRecord {
                w: 1920,
                h: 1080,
                capacity: 256,
                reserved_bytes: 30,
                last_used: 10,
            },
            super::SadCacheRecord {
                w: 320,
                h: 240,
                capacity: 64,
                reserved_bytes: 20,
                last_used: 1,
            },
        ];
        assert_eq!(
            super::plan_sad_cache(&records, 960, 540, 64, 8, 60, 4),
            super::SadCachePlan::Reuse(0),
            "un capacity mayor de la misma geometría debe reutilizarse"
        );
        assert_eq!(
            super::plan_sad_cache(&records, 640, 360, 32, 25, 60, 4),
            super::SadCachePlan::Insert { evict: vec![2, 0] },
            "LRU debe desalojar sólo lo necesario para respetar bytes"
        );
        assert_eq!(
            super::plan_sad_cache(&records[..2], 960, 540, 512, 25, 80, 4),
            super::SadCachePlan::Insert { evict: vec![0] },
            "un capacity mayor sustituye el engine pequeño de igual geometría"
        );
        assert_eq!(
            super::plan_sad_cache(&records, 8000, 8000, 1, 100, 60, 4),
            super::SadCachePlan::Insert {
                evict: vec![2, 1, 0]
            },
            "una petición mayor que la cuota sólo se admite como singleton"
        );
    }

    #[test]
    fn sad_engine_allocation_and_batch_limits_are_bounded() {
        let allocation = super::sad_engine_allocation(101, 51, 7).unwrap();
        assert_eq!(allocation.image_bytes, 10_304);
        assert_eq!(
            allocation.point_bytes,
            7 * std::mem::size_of::<super::SadPoint>() as u64
        );
        assert_eq!(allocation.result_bytes, 7 * 16);
        assert_eq!(
            allocation.reserved_bytes,
            allocation.image_bytes * 2
                + allocation.point_bytes
                + allocation.result_bytes
                + 4096
        );
        assert_eq!(super::sad_engine_pool_budget(8 * 1024), 2 * 1024);
        assert_eq!(
            super::sad_engine_pool_budget(8 * 1024 * 1024 * 1024),
            super::SAD_ENGINE_POOL_MAX_BYTES
        );

        assert_eq!(
            super::sad_batch_ranges(&[16, 32, 48, 96], 2, 64, 100).unwrap(),
            vec![(0, 2), (2, 3), (3, 4)]
        );
        assert!(super::sad_batch_ranges(&[101], 8, 64, 100).is_err());
    }

    #[test]
    fn optional_analysis_products_do_not_consume_readback_bandwidth() {
        let (w, h) = (3840usize, 2160usize);
        let base = super::BatchReadbackLayout::new(w, h, false, false, true, true);
        let grid = super::BatchReadbackLayout::new(w, h, false, true, true, true);
        let cog = super::BatchReadbackLayout::new(w, h, true, false, true, true);
        let full = super::BatchReadbackLayout::new(w, h, true, true, true, true);
        assert_eq!(grid.stride - base.stride, 40 * 40);
        assert_eq!(cog.stride - base.stride, (w + h) * 3);
        assert_eq!(full.stride, base.stride + (w + h) * 3 + 40 * 40);
        // A3: sin want_score el layout tampoco reserva las filas de score.
        let no_score = super::BatchReadbackLayout::new(w, h, false, false, true, false);
        assert_eq!(base.stride - no_score.stride, (h / 2) * 2);
    }

    #[test]
    fn surface_readback_omits_half_and_packs_u16_exactly() {
        let (w, h) = (3312usize, 5888usize);
        let image_words = ((w / 2) * (h / 2)).div_ceil(2);
        let planet = super::BatchReadbackLayout::new(w, h, false, false, true, true);
        let surface = super::BatchReadbackLayout::new(w, h, false, false, false, true);
        assert_eq!(planet.packed_image_words, image_words);
        assert_eq!(planet.stride - surface.stride, image_words);
        assert_eq!(surface.stride, image_words * 2 + (h / 2) * 2);

        let packed = [0x1234_abcd, 0xffff_0000];
        let output =
            super::finish_analysis_output(None, &packed, &packed, None, None, None, None, 4, 2);
        assert_eq!(output.half, Vec::<u16>::new());
        assert_eq!(output.blurred, [0xabcd, 0x1234]);
        assert_eq!(output.laplacian, [0xabcd, 0x1234]);
    }

    #[test]
    fn batch_pool_uses_exact_capacity_and_chunks_before_gpu_fallback() {
        let (w, h) = (1920usize, 1080usize);
        let layout = super::BatchReadbackLayout::new(w, h, false, false, true, true);
        let budget = super::batch_vram_bytes_for_layout(w, h, 3, layout.stride);
        let max_buffer = layout.stride as u64 * 3 * 4;
        assert_eq!(
            super::max_batch_len_for_limits(w, h, 6, layout.stride, budget, max_buffer),
            3
        );
        assert!(
            super::batch_vram_bytes_for_layout(w, h, 3, layout.stride)
                < super::batch_vram_bytes_for_layout(w, h, 4, layout.stride)
        );
    }

    #[test]
    fn planetary_batch_rejects_insufficient_vram_before_allocating() {
        let needed = super::batch_vram_bytes(4096, 3072, 3);
        assert!(needed > 1);
        let err = super::validate_batch_vram(4096, 3072, 3, needed - 1).unwrap_err();
        assert!(err.contains("presupuesto"));
        assert_eq!(
            super::validate_batch_vram(4096, 3072, 3, needed).unwrap(),
            needed
        );
    }

    #[test]
    #[ignore = "requiere GPU física Metal/DX12/Vulkan"]
    fn planetary_analysis_gpu_parity_physical() {
        assert!(super::ensure_parity());
    }

    /// P1: el blur GPU debe ser bit-idéntico a la referencia CPU
    /// `box_blur_r1_edge_aware` (incluidas dimensiones impares: bordes y
    /// palabra de empaquetado a medias), y el enhance partido (blur GPU +
    /// cola f32 CPU) bit-idéntico a la ruta de producción completa.
    #[test]
    #[ignore = "requiere GPU física Metal/DX12/Vulkan"]
    fn gpu_blur_and_split_enhance_match_cpu_physical() {
        for (w, h) in [(97usize, 61usize), (640, 480), (1023, 511)] {
            let mut state = 0xfeed_beefu32;
            let mut next = || {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 16) as u16
            };
            // Zonas oscuras intercaladas: cubren la rama f32 del noise-gate.
            let mono: Vec<u16> = (0..w * h)
                .map(|i| {
                    let v = next();
                    if (i / w) % 5 == 0 {
                        v / 64
                    } else {
                        v
                    }
                })
                .collect();
            let mut tmp = Vec::new();
            let mut cpu_blur = Vec::new();
            crate::alignment::box_blur_r1_edge_aware(&mono, w, h, &mut tmp, &mut cpu_blur);
            let mut gpu_blur = Vec::new();
            super::gpu_box_blur_r1_into(&mono, w, h, &mut gpu_blur)
                .expect("runtime GPU disponible");
            assert_eq!(cpu_blur[..w * h], gpu_blur[..w * h], "blur {w}x{h}");
            for amount in [4.0f32, 6.0] {
                let mut s1 = Vec::new();
                let mut s2 = Vec::new();
                let mut full = Vec::new();
                crate::alignment::enhance_for_alignment_into_amount(
                    &mono, w, h, &mut s1, &mut s2, &mut full, amount,
                );
                let mut split = Vec::new();
                crate::alignment::enhance_highpass_from_blur(
                    &mono, &gpu_blur, w, h, &mut split, amount,
                );
                assert_eq!(
                    full[..w * h],
                    split[..w * h],
                    "enhance {w}x{h} amount={amount}"
                );
            }
        }
        assert!(super::ensure_enhance_parity());
    }

    /// A1: la ventana fina densa del SAD global de superficie resuelta en GPU
    /// debe producir el MISMO trío (dx, dy, sad) que el barrido CPU de
    /// referencia y, encadenada al subpíxel compartido, el mismo resultado
    /// bit a bit que `refine_best_match_sad_offset`. Cubre la igualdad de
    /// sumas del kernel (mismo conjunto de píxeles) y la política de empates
    /// del argmin replicada en `search_sad_dense_window`.
    #[test]
    #[ignore = "requiere GPU física Metal/DX12/Vulkan"]
    fn dense_window_matches_cpu_refine_physical() {
        let w = 640usize;
        let h = 480usize;
        let mut state = 0x8bad_f00du32;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 16) as u16
        };
        let reference: Vec<u16> = (0..w * h).map(|_| next()).collect();
        let (shift_x, shift_y) = (7isize, -11isize);
        let target: Vec<u16> = (0..w * h)
            .map(|i| {
                let x = (i % w) as isize - shift_x;
                let y = (i / w) as isize - shift_y;
                if x >= 0 && (x as usize) < w && y >= 0 && (y as usize) < h {
                    reference[y as usize * w + x as usize]
                } else {
                    0
                }
            })
            .collect();
        let (roi_x, roi_y, roi_w, roi_h) = (160usize, 120usize, 320usize, 240usize);
        for (guess_dx, guess_dy) in [(0isize, 0isize), (5, -4)] {
            let dense = super::search_sad_dense_window(
                &reference, &target, w, h, roi_x, roi_y, roi_w, roi_h, guess_dx, guess_dy, 16,
            )
            .expect("runtime GPU disponible")
            .expect("ventana interior: todos los candidatos evaluables");
            assert_eq!((dense.0, dense.1), (shift_x, shift_y));
            let cpu = crate::alignment::refine_best_match_sad_offset(
                &reference, &target, w, h, roi_x, roi_y, roi_w, roi_h, guess_dx, guess_dy, 16,
            );
            let split = crate::alignment::subpixel_after_integer_sad(
                &reference, &target, w, roi_x, roi_y, roi_w, roi_h, dense.0, dense.1, dense.2,
            );
            assert_eq!(cpu.0.to_bits(), split.0.to_bits());
            assert_eq!(cpu.1.to_bits(), split.1.to_bits());
        }
    }

    #[test]
    #[ignore = "requiere GPU física; valida submit/readback multiframe"]
    fn planetary_analysis_gpu_batch_parity_physical() {
        assert!(super::ensure_parity());
    }

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
            ("3312x5888", 3312usize, 5888usize),
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
            let out = super::process_batch_with_options(&frames, w, h, true, true, true, true)
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
            &[super::SadPoint::rectangular(
                cx, cy, cx, cy, 60, 40, 8, true,
            )],
        )
        .unwrap()[0]
            .expect("candidato válido");
        let parallel =
            super::search_sad_single_parallel(&reference, &target, w, h, cx, cy, cx, cy, 60, 40, 8)
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
                (((x * 977 + y * 613 + x * y * 17) ^ (x << 7) ^ (y << 5)) & 0xffff) as u16
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

    /// P2: varios targets con referencia/geometría comunes deben conservar el
    /// mismo orden y los mismos u64 exactos que llamadas unitarias. Para AP
    /// cuadrados también se contrasta cada resultado con el oráculo CPU.
    #[test]
    #[ignore = "requiere GPU física; SAD multiframe con un readback por lote"]
    fn sad_multitarget_batch_matches_single_and_cpu_physical() {
        let (w, h) = (160usize, 112usize);
        let reference: Vec<u16> = (0..w * h)
            .map(|i| {
                let x = i % w;
                let y = i / w;
                (((x * 977 + y * 613 + x * y * 17) ^ (x << 7) ^ (y << 5)) & 0xffff) as u16
            })
            .collect();
        let shifts = [(3i32, -2i32), (-4, 1), (0, 0)];
        let targets: Vec<Vec<u16>> = shifts
            .iter()
            .map(|&(dx, dy)| {
                let mut target = vec![0u16; w * h];
                for y in 0..h as i32 {
                    for x in 0..w as i32 {
                        let tx = x + dx;
                        let ty = y + dy;
                        if tx >= 0 && ty >= 0 && tx < w as i32 && ty < h as i32 {
                            target[ty as usize * w + tx as usize] =
                                reference[y as usize * w + x as usize];
                        }
                    }
                }
                target
            })
            .collect();
        let points = [
            vec![
                super::SadPoint::new(42, 35, 42, 35, 20, 6, true),
                super::SadPoint::new(104, 72, 104, 72, 28, 6, true),
            ],
            vec![
                super::SadPoint::new(44, 36, 44, 36, 22, 6, true),
                super::SadPoint::new(80, 58, 80, 58, 30, 6, true),
                super::SadPoint::new(116, 76, 116, 76, 18, 6, true),
            ],
            vec![super::SadPoint::new(80, 56, 80, 56, 32, 6, true)],
        ];
        let requests = [
            super::SadBatchRequest::new(&targets[0], &points[0]),
            super::SadBatchRequest::new(&targets[1], &points[1]),
            super::SadBatchRequest::new(&targets[2], &points[2]),
        ];
        let batched = super::search_sad_points_batch(&reference, &requests, w, h).unwrap();
        assert_eq!(batched.len(), requests.len());
        for (request_index, request) in requests.iter().enumerate() {
            let single =
                super::search_sad_points(&reference, request.target, w, h, request.points).unwrap();
            assert_eq!(batched[request_index], single);
            for (point, gpu) in request.points.iter().zip(&batched[request_index]) {
                let gpu = gpu.expect("AP interior válido");
                let (dx, dy, sad) = crate::alignment::find_best_match_sad(
                    &reference,
                    request.target,
                    w,
                    point.ref_x as usize,
                    point.ref_y as usize,
                    point.tgt_x as usize,
                    point.tgt_y as usize,
                    point.box_w as usize,
                    point.search_r,
                );
                assert_eq!((gpu.dx, gpu.dy, gpu.sad), (dx as i32, dy as i32, sad));
            }
        }
    }
}
