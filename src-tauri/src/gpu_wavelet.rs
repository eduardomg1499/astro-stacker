// ===========================================================================
// GPU COMPUTE (wgpu) — DESCOMPOSICIÓN WAVELET (blur separable en GPU).
//
// Reutiliza el device/queue del runtime de `gpu_stack` (no crea otro contexto).
// El blur GPU REPLICA línea a línea el `box_blur_parallel` de CPU (running-sum
// separable con bordes por clamp) y usa los MISMOS radios de la Kuckir sizing
// de `apply_gaussian_blur` → el resultado es numéricamente idéntico (mismo
// orden de sumas f32) y NO hay divergencia de "look" GPU vs CPU.
//
// GARANTÍAS DE NO-ROTURA (igual que gpu_stack):
//  1. Detección: sin runtime GPU o si el pipeline no valida → None → CPU intacta.
//  2. Paridad: al primer uso se descompone una escena sintética por AMBAS rutas
//     y se compara el RMSE por banda; si supera la tolerancia, la GPU de wavelets
//     queda deshabilitada la sesión.
//  3. Alcance conservador: SOLO la descomposición Gaussiana pura (sin edge-aware
//     bilateral) y solo si la imagen es suficientemente grande (lo decide el
//     caller). Cualquier error a mitad → None → el caller usa la descomposición
//     de CPU existente.
//
// `ZAS_FORCE_CPU=1` ya apaga el runtime en gpu_stack, así que esto también queda
// inhabilitado por esa vía.
// ===========================================================================

use crate::gpu_stack::gpu_runtime;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

// Un hilo por LÍNEA (fila si axis=0, columna si axis=1). Running-sum idéntico al
// de box_blur_parallel: inicializa la ventana [-r, r] con clamp a los bordes y la
// desliza restando el que sale y sumando el que entra.
const BLUR_WGSL: &str = r#"
struct P { w: u32, h: u32, r: u32, axis: u32 };
@group(0) @binding(0) var<uniform> p: P;
@group(0) @binding(1) var<storage, read> src: array<f32>;
@group(0) @binding(2) var<storage, read_write> dst: array<f32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let line = gid.x;
    let w = p.w;
    let h = p.h;
    let r = i32(p.r);
    let iarr = 1.0 / (2.0 * f32(r) + 1.0);

    if (p.axis == 0u) {
        // Horizontal: line = fila y; recorre x.
        if (line >= h) { return; }
        let y = line;
        let n = i32(w);
        var sum = 0.0;
        for (var i = -r; i <= r; i = i + 1) {
            let xc = clamp(i, 0, n - 1);
            sum = sum + src[y * w + u32(xc)];
        }
        dst[y * w] = sum * iarr;
        for (var x = 1; x < n; x = x + 1) {
            let leaving = clamp(x - 1 - r, 0, n - 1);
            let entering = clamp(x + r, 0, n - 1);
            sum = sum - src[y * w + u32(leaving)] + src[y * w + u32(entering)];
            dst[y * w + u32(x)] = sum * iarr;
        }
    } else {
        // Vertical: line = columna x; recorre y.
        if (line >= w) { return; }
        let x = line;
        let n = i32(h);
        var sum = 0.0;
        for (var i = -r; i <= r; i = i + 1) {
            let yc = clamp(i, 0, n - 1);
            sum = sum + src[u32(yc) * w + x];
        }
        dst[x] = sum * iarr;
        for (var y = 1; y < n; y = y + 1) {
            let leaving = clamp(y - 1 - r, 0, n - 1);
            let entering = clamp(y + r, 0, n - 1);
            sum = sum - src[u32(leaving) * w + x] + src[u32(entering) * w + x];
            dst[u32(y) * w + x] = sum * iarr;
        }
    }
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BlurParams {
    w: u32,
    h: u32,
    r: u32,
    axis: u32,
}

/// Arena de uniforms con offsets dinámicos. Antes cada pasada de blur/RL
/// creaba un buffer y hacía un `queue.write_buffer`; una edición wavelet podía
/// generar decenas y RL cientos de allocs/uploads de 16 bytes. Ahora todas las
/// constantes de una operación viajan en un buffer y una sola escritura.
const UNIFORM_STRIDE: usize = 256;

struct UniformArena {
    buffer: wgpu::Buffer,
    bytes: Vec<u8>,
    len: usize,
    capacity: usize,
}

impl UniformArena {
    fn new(dev: &wgpu::Device, capacity: usize, label: &str) -> Option<Self> {
        let capacity = capacity.max(1);
        let size = capacity.checked_mul(UNIFORM_STRIDE)?;
        if size > u32::MAX as usize || size as u64 > dev.limits().max_buffer_size {
            return None;
        }
        let buffer = dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: size as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Some(Self {
            buffer,
            bytes: vec![0; size],
            len: 0,
            capacity,
        })
    }

    fn push<T: bytemuck::Pod>(&mut self, value: &T) -> u32 {
        let value = bytemuck::bytes_of(value);
        assert!(value.len() <= 16, "uniform wavelet mayor de 16 bytes");
        assert!(
            self.len < self.capacity,
            "arena de uniforms wavelet agotada"
        );
        let offset = self.len * UNIFORM_STRIDE;
        self.bytes[offset..offset + value.len()].copy_from_slice(value);
        self.len += 1;
        offset as u32
    }

    fn upload(&self, queue: &wgpu::Queue) {
        if self.len != 0 {
            let used = (self.len - 1) * UNIFORM_STRIDE + 16;
            queue.write_buffer(&self.buffer, 0, &self.bytes[..used]);
        }
    }
}

struct BlurGpu {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

/// Pipeline de blur cacheado (una sola construcción). None si no hay runtime GPU
/// o si el shader/pipeline no valida (blindaje: no rompe, solo desactiva GPU).
fn blur_gpu() -> Option<&'static BlurGpu> {
    static B: OnceLock<Option<BlurGpu>> = OnceLock::new();
    B.get_or_init(|| {
        let rt = gpu_runtime()?;
        let dev = &rt.device;
        dev.push_error_scope(wgpu::ErrorFilter::Validation);
        let shader = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("zas-blur-shader"),
            source: wgpu::ShaderSource::Wgsl(BLUR_WGSL.into()),
        });
        let layout = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zas-blur-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipe_layout = dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("zas-blur-pipe-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = dev.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("zas-blur-pipeline"),
            layout: Some(&pipe_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        if let Some(err) = pollster::block_on(dev.pop_error_scope()) {
            eprintln!("[gpu_wavelet] shader/pipeline de blur no valida: {err}");
            return None;
        }
        Some(BlurGpu { pipeline, layout })
    })
    .as_ref()
}

/// Radios de las 3 pasadas box de un Gaussiano — RÉPLICA EXACTA de la sizing de
/// `apply_gaussian_blur` (Kuckir). Un radio 0 = pasada omitida.
fn box_radii(sigma: f32) -> [usize; 3] {
    let n = 3.0f32;
    let sigma = sigma.max(0.3);
    let w_ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut wl = w_ideal.floor() as isize;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wl = wl.max(1) as usize;
    let wu = wl + 2;
    let m_ideal =
        (12.0 * sigma * sigma - (n * (wl * wl) as f32) - (4.0 * n * wl as f32) - (3.0 * n))
            / (-4.0 * wl as f32 - 4.0);
    let m = m_ideal.round().clamp(0.0, n) as usize;
    let sizes = [
        if 0 < m { wl } else { wu },
        if 1 < m { wl } else { wu },
        if 2 < m { wl } else { wu },
    ];
    [(sizes[0] - 1) / 2, (sizes[1] - 1) / 2, (sizes[2] - 1) / 2]
}

const SIGMAS: [f32; 6] = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0];

fn nonzero_box_radii(sigma: f32) -> usize {
    box_radii(sigma).into_iter().filter(|&r| r != 0).count()
}

fn wavelet_vram_bytes(n: usize) -> u64 {
    // base + tmp + 6 salidas residentes, más staging de las 6 salidas.
    (n as u64).saturating_mul(4).saturating_mul(14)
}

fn rl_vram_bytes(n: usize) -> u64 {
    // 7 buffers de trabajo + un staging de salida.
    (n as u64).saturating_mul(4).saturating_mul(8)
}

fn wavelet_sigmas_per_submit(n: usize, is_metal: bool) -> usize {
    // Peor sigma: tres box blurs separables = seis recorridos de la imagen.
    // DX12/Vulkan usan un margen estricto por TDR; Metal admite un buffer más
    // grande, pero también se trocea 8K para no monopolizar la cola durante un
    // command buffer gigantesco.
    let work_per_sigma = n.saturating_mul(6).max(1);
    let budget = if is_metal {
        600_000_000usize
    } else {
        150_000_000usize
    };
    (budget / work_per_sigma).clamp(1, SIGMAS.len())
}

fn rl_iterations_per_submit(n: usize, sigma: f32, is_metal: bool, total: usize) -> usize {
    if total == 0 {
        return 1;
    }
    // Cada iteración: dos gaussianos (2 ejes por radio) + ratio + update.
    let passes = nonzero_box_radii(sigma).saturating_mul(4).saturating_add(2);
    let work_per_iteration = n.saturating_mul(passes).max(1);
    let budget = if is_metal {
        600_000_000usize
    } else {
        120_000_000usize
    };
    (budget / work_per_iteration).clamp(1, total.min(8).max(1))
}

/// Estado del self-test de paridad de wavelets: 0 pendiente, 1 ok, 2 fallido.
static WAVELET_PARITY: AtomicU8 = AtomicU8::new(0);
const WAVELET_PARITY_TOL: f64 = 0.5; // ADU16 (mismo algoritmo → debe ser ~0)

/// Descompone `base` (un canal f32) en 7 capas wavelet (6 bandas DoG σ1..32 +
/// residuo) EN GPU. Devuelve None si no hay GPU utilizable, si la paridad no ha
/// pasado, si no cabe en el presupuesto de VRAM, o ante cualquier error wgpu.
/// El caller debe usar entonces la descomposición de CPU (idéntica).
pub fn gpu_decompose(base: &[f32], width: usize, height: usize) -> Option<Vec<Vec<f32>>> {
    if !wavelet_parity_ok() {
        return None;
    }
    let rt = gpu_runtime()?;
    let n = width * height;
    let n4 = (n as u64).saturating_mul(4);
    let needed = wavelet_vram_bytes(n);
    if n4 > rt.max_binding || n4 > rt.max_buffer_size || needed > rt.vram_budget {
        return None;
    }
    gpu_decompose_raw(base, width, height)
}

/// Descomposición GPU sin el gate de paridad (lo usa el propio self-test).
fn gpu_decompose_raw(base: &[f32], width: usize, height: usize) -> Option<Vec<Vec<f32>>> {
    let rt = gpu_runtime()?;
    let bg = blur_gpu()?;
    let dev = &rt.device;
    let n = width * height;
    if n == 0 || base.len() != n {
        return None;
    }
    let n4 = (n * 4) as u64;
    if n4 > rt.max_binding || n4 > rt.max_buffer_size {
        return None;
    }

    let mk_storage = |label: &str, extra: wgpu::BufferUsages| {
        dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: n4,
            usage: wgpu::BufferUsages::STORAGE | extra,
            mapped_at_creation: false,
        })
    };

    dev.push_error_scope(wgpu::ErrorFilter::Validation);

    let base_buf = mk_storage(
        "zas-blur-base",
        wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
    );
    rt.queue
        .write_buffer(&base_buf, 0, bytemuck::cast_slice(base));
    let tmp_buf = mk_storage("zas-blur-tmp", wgpu::BufferUsages::empty());
    let blur_bufs: Vec<wgpu::Buffer> = (0..6)
        .map(|i| {
            mk_storage(
                &format!("zas-blur-out-{i}"),
                wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            )
        })
        .collect();

    // Mantener vivos los bind groups hasta que todos los command buffers se
    // hayan enviado. Los uniforms viven en una única arena dinámica.
    let mut keep_bg: Vec<wgpu::BindGroup> = Vec::new();
    let radii_all: Vec<[usize; 3]> = SIGMAS.iter().map(|&s| box_radii(s)).collect();
    let uniform_slots = radii_all
        .iter()
        .map(|radii| radii.iter().filter(|&&r| r != 0).count() * 2)
        .sum();
    let mut uniforms = UniformArena::new(dev, uniform_slots, "zas-blur-uniform-arena")?;

    // Staging existe desde el inicio para que la última command buffer incluya
    // también las copias: compute + descarga requieren una sola secuencia de
    // submits y una única sincronización map_async.
    let planes_per_staging = (rt.max_buffer_size / n4).clamp(1, 6) as usize;
    let staging_count = 6usize.div_ceil(planes_per_staging);
    let staging: Vec<wgpu::Buffer> = (0..staging_count)
        .map(|group| {
            let planes = planes_per_staging.min(6 - group * planes_per_staging);
            dev.create_buffer(&wgpu::BufferDescriptor {
                label: Some("zas-blur-staging"),
                size: n4 * planes as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            })
        })
        .collect();

    let sigmas_per_submit = wavelet_sigmas_per_submit(n, rt.backend == "Metal");
    let sigma_indices: Vec<usize> = (0..SIGMAS.len()).collect();
    let sigma_chunks: Vec<&[usize]> = sigma_indices.chunks(sigmas_per_submit).collect();
    let mut command_buffers = Vec::with_capacity(sigma_chunks.len());
    for (chunk_index, chunk) in sigma_chunks.iter().enumerate() {
        let mut enc = dev.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("zas-blur-enc"),
        });
        for &si in chunk.iter() {
            let radii = &radii_all[si];
            let mut emitted = false;
            for &r in radii.iter().filter(|&&r| r != 0) {
                // La copia base→salida era redundante: la primera horizontal
                // puede leer base directamente y produce exactamente los mismos
                // bits. Las siguientes iteraciones sí leen la salida anterior.
                let src = if emitted { &blur_bufs[si] } else { &base_buf };
                record_pass(
                    dev,
                    &mut enc,
                    bg,
                    src,
                    &tmp_buf,
                    width,
                    height,
                    r,
                    0,
                    &mut uniforms,
                    &mut keep_bg,
                );
                record_pass(
                    dev,
                    &mut enc,
                    bg,
                    &tmp_buf,
                    &blur_bufs[si],
                    width,
                    height,
                    r,
                    1,
                    &mut uniforms,
                    &mut keep_bg,
                );
                emitted = true;
            }
            if !emitted {
                enc.copy_buffer_to_buffer(&base_buf, 0, &blur_bufs[si], 0, n4);
            }
        }
        if chunk_index + 1 == sigma_chunks.len() {
            for (bi, buf) in blur_bufs.iter().enumerate() {
                let group = bi / planes_per_staging;
                let local = bi % planes_per_staging;
                enc.copy_buffer_to_buffer(buf, 0, &staging[group], local as u64 * n4, n4);
            }
        }
        command_buffers.push(enc.finish());
    }

    uniforms.upload(&rt.queue);
    for command_buffer in command_buffers {
        rt.queue.submit(Some(command_buffer));
    }

    let slices: Vec<wgpu::BufferSlice<'_>> =
        staging.iter().map(|buffer| buffer.slice(..)).collect();
    let mut receivers = Vec::with_capacity(slices.len());
    for slice in &slices {
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        receivers.push(rx);
    }
    for rx in &receivers {
        if crate::gpu_stack::wait_for_readback(&dev, rx, "readback wavelet").is_err() {
            return None;
        }
    }

    // Recombinar directamente desde el mapping. La versión anterior copiaba
    // primero seis imágenes completas a Vec y luego volvía a recorrerlas para
    // crear siete capas (pico de RAM y ancho de banda host casi duplicados).
    let ls = {
        let views: Vec<wgpu::BufferView<'_>> = slices
            .iter()
            .map(|slice| slice.get_mapped_range())
            .collect();
        let groups: Vec<&[f32]> = views
            .iter()
            .map(|view| bytemuck::cast_slice::<u8, f32>(view))
            .collect();
        let plane = |i: usize| {
            let group = i / planes_per_staging;
            let local = i % planes_per_staging;
            &groups[group][local * n..(local + 1) * n]
        };
        let mut layers: Vec<Vec<f32>> = Vec::with_capacity(7);
        layers.push(base.iter().zip(plane(0)).map(|(a, b)| a - b).collect());
        for i in 0..5 {
            layers.push(
                plane(i)
                    .iter()
                    .zip(plane(i + 1))
                    .map(|(a, b)| a - b)
                    .collect(),
            );
        }
        layers.push(plane(5).to_vec());
        layers
    };
    drop(slices);
    for buffer in &staging {
        buffer.unmap();
    }

    if crate::gpu_stack::take_gpu_error() {
        return None;
    }
    if let Some(err) = pollster::block_on(dev.pop_error_scope()) {
        eprintln!("[gpu_wavelet] error de validación en la descomposición: {err}");
        return None;
    }

    Some(ls)
}

#[allow(clippy::too_many_arguments)]
fn record_pass(
    dev: &wgpu::Device,
    enc: &mut wgpu::CommandEncoder,
    bg: &BlurGpu,
    src: &wgpu::Buffer,
    dst: &wgpu::Buffer,
    width: usize,
    height: usize,
    r: usize,
    axis: u32,
    uniforms: &mut UniformArena,
    keep_bg: &mut Vec<wgpu::BindGroup>,
) {
    let params = BlurParams {
        w: width as u32,
        h: height as u32,
        r: r as u32,
        axis,
    };
    let uniform_offset = uniforms.push(&params);

    let bind = dev.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("zas-blur-bg"),
        layout: &bg.layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &uniforms.buffer,
                    offset: 0,
                    size: std::num::NonZeroU64::new(16),
                }),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: src.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: dst.as_entire_binding(),
            },
        ],
    });
    keep_bg.push(bind);
    let bind_ref = keep_bg.last().unwrap();

    let lines = if axis == 0 { height } else { width } as u32;
    let groups = (lines + 63) / 64;
    let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
        label: Some("zas-blur-pass"),
        timestamp_writes: None,
    });
    pass.set_pipeline(&bg.pipeline);
    pass.set_bind_group(0, bind_ref, &[uniform_offset]);
    pass.dispatch_workgroups(groups, 1, 1);
}

// ---------------------------------------------------------------------------
// Self-test de paridad GPU-vs-CPU de la descomposición.
// ---------------------------------------------------------------------------

fn wavelet_parity_ok() -> bool {
    match WAVELET_PARITY.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let ok = match wavelet_parity_rmse() {
                Ok(rmse) => {
                    eprintln!(
                        "[gpu_wavelet] paridad descomposición GPU-vs-CPU: RMSE {rmse:.5} ADU"
                    );
                    rmse <= WAVELET_PARITY_TOL
                }
                Err(e) => {
                    eprintln!("[gpu_wavelet] self-test de paridad falló: {e}");
                    false
                }
            };
            WAVELET_PARITY.store(if ok { 1 } else { 2 }, Ordering::Relaxed);
            ok
        }
    }
}

/// Descompone una escena sintética por CPU y por GPU y devuelve el PEOR RMSE por
/// banda (ADU16). Público para el test `#[ignore]` que se corre en GPU física.
pub fn wavelet_parity_rmse() -> Result<f64, String> {
    let (w, h) = (128usize, 112usize);
    let tau = std::f32::consts::TAU;
    let mut base = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let xf = x as f32;
            let yf = y as f32;
            base[y * w + x] = 9000.0
                + 5000.0 * (xf * tau / 7.3).sin() * (yf * tau / 9.1).cos()
                + 3000.0 * ((xf * 0.6 + yf * 0.8) * tau / 13.7).sin();
        }
    }

    // CPU: réplica EXACTA de la descomposición Gaussiana de producción.
    let mut cpu: Vec<Vec<f32>> = Vec::with_capacity(7);
    let blurs: Vec<Vec<f32>> = SIGMAS
        .iter()
        .map(|&s| crate::apply_gaussian_blur(&base, w, h, s))
        .collect();
    cpu.push(base.iter().zip(&blurs[0]).map(|(a, b)| a - b).collect());
    for i in 0..5 {
        cpu.push(
            blurs[i]
                .iter()
                .zip(&blurs[i + 1])
                .map(|(a, b)| a - b)
                .collect(),
        );
    }
    cpu.push(blurs[5].clone());

    let gpu = gpu_decompose_raw(&base, w, h).ok_or("gpu_decompose_raw devolvió None")?;
    if gpu.len() != cpu.len() {
        return Err("nº de bandas GPU != CPU".into());
    }

    let mut worst = 0.0f64;
    for (g, c) in gpu.iter().zip(cpu.iter()) {
        let mut se = 0.0f64;
        for (a, b) in g.iter().zip(c.iter()) {
            let d = (*a - *b) as f64;
            se += d * d;
        }
        worst = worst.max((se / (w * h) as f64).sqrt());
    }
    Ok(worst)
}

// ===========================================================================
// GPU RICHARDSON-LUCY (deconvolución, PSF Gaussiana) — v2.
// RESIDENTE: est/original/mask/scratch viven en GPU durante TODAS las
// iteraciones (cero transferencias por iteración → aquí sí gana la GPU).
// Kernels: blur (reusado del decompose), `ratio` y `update` (Jacobi). Réplica
// EXACTA de `richardson_lucy_core` (misma aritmética f32) → paridad estrecha.
// Solo PSF Gaussiana; con measured_psf el caller se queda en CPU.
// ===========================================================================

const RATIO_WGSL: &str = r#"
struct RP { n: u32, a: u32, b: u32, c: u32 };
@group(0) @binding(0) var<uniform> rp: RP;
@group(0) @binding(1) var<storage, read> orig: array<f32>;
@group(0) @binding(2) var<storage, read> blur_est: array<f32>;
@group(0) @binding(3) var<storage, read_write> ratio: array<f32>;
@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let j = gid.x;
    if (j >= rp.n) { return; }
    let denom = max(blur_est[j], 1.0);
    var r = 1.0;
    if (orig[j] > 1.0) { r = orig[j] / denom; }
    ratio[j] = clamp(r, 0.5, 2.0);
}
"#;

const UPDATE_WGSL: &str = r#"
struct UP { w: u32, h: u32, n: u32, c: u32 };
@group(0) @binding(0) var<uniform> up: UP;
@group(0) @binding(1) var<storage, read> est: array<f32>;
@group(0) @binding(2) var<storage, read> bratio: array<f32>;
@group(0) @binding(3) var<storage, read> mask: array<f32>;
@group(0) @binding(4) var<storage, read> orig: array<f32>;
@group(0) @binding(5) var<storage, read_write> est_out: array<f32>;
@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let j = gid.x;
    if (j >= up.n) { return; }
    let w = up.w;
    let h = up.h;
    let x = j % w;
    let y = j / w;
    // Bordes intactos (como la CPU, que no toca la orla).
    if (x == 0u || y == 0u || x == (w - 1u) || y == (h - 1u)) {
        est_out[j] = est[j];
        return;
    }
    let n1 = est[j - 1u];
    let n2 = est[j + 1u];
    let n3 = est[j - w];
    let n4 = est[j + w];
    let local_mean = (n1 + n2 + n3 + n4) * 0.25;
    let tv_gradient = local_mean - est[j];
    var upd = est[j] * bratio[j];
    upd = upd + tv_gradient * 0.05;
    let cw = mask[j] * 0.58;
    var fv = est[j] * (1.0 - cw) + upd * cw;
    if (!(fv == fv) || abs(fv) > 3.0e38) { fv = orig[j]; }
    fv = clamp(fv, 0.0, 65535.0);
    est_out[j] = fv;
}
"#;

struct RlGpu {
    ratio_pipe: wgpu::ComputePipeline,
    ratio_layout: wgpu::BindGroupLayout,
    update_pipe: wgpu::ComputePipeline,
    update_layout: wgpu::BindGroupLayout,
}

fn storage_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// Construye un pipeline pointwise: binding 0 = uniform, 1..=n_ro storage RO, y
/// n_ro+1 storage RW. None si el shader/pipeline no valida.
fn build_pipeline(
    dev: &wgpu::Device,
    wgsl: &str,
    label: &str,
    n_ro: u32,
) -> Option<(wgpu::ComputePipeline, wgpu::BindGroupLayout)> {
    dev.push_error_scope(wgpu::ErrorFilter::Validation);
    let shader = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(wgsl.into()),
    });
    let mut entries = vec![wgpu::BindGroupLayoutEntry {
        binding: 0,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: true,
            min_binding_size: None,
        },
        count: None,
    }];
    for b in 1..=n_ro {
        entries.push(storage_entry(b, true));
    }
    entries.push(storage_entry(n_ro + 1, false));
    let layout = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &entries,
    });
    let pipe_layout = dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[&layout],
        push_constant_ranges: &[],
    });
    let pipeline = dev.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&pipe_layout),
        module: &shader,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });
    if let Some(err) = pollster::block_on(dev.pop_error_scope()) {
        eprintln!("[gpu_wavelet] pipeline '{label}' no valida: {err}");
        return None;
    }
    Some((pipeline, layout))
}

fn rl_gpu() -> Option<&'static RlGpu> {
    static R: OnceLock<Option<RlGpu>> = OnceLock::new();
    R.get_or_init(|| {
        let rt = gpu_runtime()?;
        let (ratio_pipe, ratio_layout) = build_pipeline(&rt.device, RATIO_WGSL, "zas-rl-ratio", 2)?;
        let (update_pipe, update_layout) =
            build_pipeline(&rt.device, UPDATE_WGSL, "zas-rl-update", 4)?;
        Some(RlGpu {
            ratio_pipe,
            ratio_layout,
            update_pipe,
            update_layout,
        })
    })
    .as_ref()
}

/// Graba un Gaussiano (3 box-pass con los radios Kuckir) src→out usando tmp.
fn record_gaussian(
    dev: &wgpu::Device,
    enc: &mut wgpu::CommandEncoder,
    bg: &BlurGpu,
    src: &wgpu::Buffer,
    out: &wgpu::Buffer,
    tmp: &wgpu::Buffer,
    w: usize,
    h: usize,
    sigma: f32,
    uniforms: &mut UniformArena,
    keep_bg: &mut Vec<wgpu::BindGroup>,
) {
    let n4 = (w * h * 4) as u64;
    let mut emitted = false;
    for r in box_radii(sigma).into_iter().filter(|&r| r != 0) {
        let input = if emitted { out } else { src };
        record_pass(dev, enc, bg, input, tmp, w, h, r, 0, uniforms, keep_bg);
        record_pass(dev, enc, bg, tmp, out, w, h, r, 1, uniforms, keep_bg);
        emitted = true;
    }
    if !emitted {
        enc.copy_buffer_to_buffer(src, 0, out, 0, n4);
    }
}

/// Graba un kernel pointwise (workgroup 64 sobre n píxeles). `bufs` en orden de
/// binding 1.. (los RO primero, el RW al final, como el layout).
#[allow(clippy::too_many_arguments)]
fn record_pointwise(
    dev: &wgpu::Device,
    enc: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::ComputePipeline,
    layout: &wgpu::BindGroupLayout,
    uniform: [u32; 4],
    bufs: &[&wgpu::Buffer],
    n: usize,
    uniforms: &mut UniformArena,
    keep_bg: &mut Vec<wgpu::BindGroup>,
) {
    let uniform_offset = uniforms.push(&uniform);
    let mut entries = vec![wgpu::BindGroupEntry {
        binding: 0,
        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
            buffer: &uniforms.buffer,
            offset: 0,
            size: std::num::NonZeroU64::new(16),
        }),
    }];
    for (i, b) in bufs.iter().enumerate() {
        entries.push(wgpu::BindGroupEntry {
            binding: (i + 1) as u32,
            resource: b.as_entire_binding(),
        });
    }
    let bind = dev.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("zas-rl-bg"),
        layout,
        entries: &entries,
    });
    keep_bg.push(bind);
    let bind_ref = keep_bg.last().unwrap();
    let groups = (n as u32 + 63) / 64;
    let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
        label: Some("zas-rl-pass"),
        timestamp_writes: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind_ref, &[uniform_offset]);
    pass.dispatch_workgroups(groups, 1, 1);
}

/// Deconvolución Richardson-Lucy (PSF Gaussiana) EN GPU. None si no hay GPU
/// utilizable, si la paridad no ha pasado, si no cabe en VRAM, o ante error wgpu.
pub fn gpu_richardson_lucy(
    input: &[f32],
    original: &[f32],
    width: usize,
    height: usize,
    iterations: usize,
    sigma: f32,
) -> Option<Vec<f32>> {
    if !rl_parity_ok() {
        return None;
    }
    let rt = gpu_runtime()?;
    let n = width.saturating_mul(height);
    let n4 = (n as u64).saturating_mul(4);
    let needed = rl_vram_bytes(n);
    if n4 > rt.max_binding || needed > rt.vram_budget {
        return None;
    }
    gpu_richardson_lucy_raw(input, original, width, height, iterations, sigma)
}

fn gpu_richardson_lucy_raw(
    input: &[f32],
    original: &[f32],
    width: usize,
    height: usize,
    iterations: usize,
    sigma: f32,
) -> Option<Vec<f32>> {
    if iterations == 0 {
        return Some(input.to_vec());
    }
    let rt = gpu_runtime()?;
    let bg = blur_gpu()?;
    let rl = rl_gpu()?;
    let dev = &rt.device;
    let n = width * height;
    if n == 0 || n > u32::MAX as usize || input.len() != n || original.len() != n {
        return None;
    }
    let n4 = (n * 4) as u64;

    // Máscara idéntica a producción (helper compartido).
    let mask = crate::richardson_lucy_mask(original, width, height, sigma);

    let mk = |label: &str| {
        dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: n4,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        })
    };
    let est_a = mk("rl-est-a");
    let est_b = mk("rl-est-b");
    let orig_buf = mk("rl-orig");
    let mask_buf = mk("rl-mask");
    let tmp_buf = mk("rl-tmp");
    let blur_buf = mk("rl-blur");
    let ratio_buf = mk("rl-ratio");

    rt.queue
        .write_buffer(&est_a, 0, bytemuck::cast_slice(input));
    rt.queue
        .write_buffer(&orig_buf, 0, bytemuck::cast_slice(original));
    rt.queue
        .write_buffer(&mask_buf, 0, bytemuck::cast_slice(&mask));

    dev.push_error_scope(wgpu::ErrorFilter::Validation);
    let uniforms_per_iteration = nonzero_box_radii(sigma).saturating_mul(4).saturating_add(2);
    let uniform_slots = iterations.checked_mul(uniforms_per_iteration)?;
    let mut uniforms = UniformArena::new(dev, uniform_slots, "zas-rl-uniform-arena")?;
    let mut keep_bg: Vec<wgpu::BindGroup> = Vec::new();

    // Descargar `cur` dentro de la última command buffer evita un submit
    // adicional y conserva un solo map_async al final.
    let staging = dev.create_buffer(&wgpu::BufferDescriptor {
        label: Some("zas-rl-staging"),
        size: n4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let (mut cur, mut next) = (&est_a, &est_b);
    let iterations_per_submit =
        rl_iterations_per_submit(n, sigma, rt.backend == "Metal", iterations);
    let mut command_buffers = Vec::with_capacity(iterations.div_ceil(iterations_per_submit));
    let mut done = 0usize;
    while done < iterations {
        let end = (done + iterations_per_submit).min(iterations);
        let mut enc = dev.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("zas-rl-enc"),
        });
        for _ in done..end {
            // blurred_est = gaussian(cur) → blur_buf
            record_gaussian(
                dev,
                &mut enc,
                bg,
                cur,
                &blur_buf,
                &tmp_buf,
                width,
                height,
                sigma,
                &mut uniforms,
                &mut keep_bg,
            );
            // ratio: (orig, blur_buf) → ratio_buf
            record_pointwise(
                dev,
                &mut enc,
                &rl.ratio_pipe,
                &rl.ratio_layout,
                [n as u32, 0, 0, 0],
                &[&orig_buf, &blur_buf, &ratio_buf],
                n,
                &mut uniforms,
                &mut keep_bg,
            );
            // blurred_ratio = gaussian(ratio_buf) → blur_buf
            record_gaussian(
                dev,
                &mut enc,
                bg,
                &ratio_buf,
                &blur_buf,
                &tmp_buf,
                width,
                height,
                sigma,
                &mut uniforms,
                &mut keep_bg,
            );
            // update (Jacobi): (cur, blur_buf, mask, orig) → next
            record_pointwise(
                dev,
                &mut enc,
                &rl.update_pipe,
                &rl.update_layout,
                [width as u32, height as u32, n as u32, 0],
                &[cur, &blur_buf, &mask_buf, &orig_buf, next],
                n,
                &mut uniforms,
                &mut keep_bg,
            );
            std::mem::swap(&mut cur, &mut next);
        }
        if end == iterations {
            enc.copy_buffer_to_buffer(cur, 0, &staging, 0, n4);
        }
        command_buffers.push(enc.finish());
        done = end;
    }
    uniforms.upload(&rt.queue);
    for command_buffer in command_buffers {
        rt.queue.submit(Some(command_buffer));
    }

    let slice = staging.slice(0..n4);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    if crate::gpu_stack::wait_for_readback(&dev, &rx, "readback deconvolución RL").is_err() {
        return None;
    }
    let result: Vec<f32> = {
        let data = slice.get_mapped_range();
        let vals: &[f32] = bytemuck::cast_slice(&data);
        vals[..n].to_vec()
    };
    staging.unmap();

    if crate::gpu_stack::take_gpu_error() {
        return None;
    }
    if let Some(err) = pollster::block_on(dev.pop_error_scope()) {
        eprintln!("[gpu_wavelet] error de validación en RL: {err}");
        return None;
    }
    Some(result)
}

static RL_PARITY: AtomicU8 = AtomicU8::new(0);
const RL_PARITY_TOL: f64 = 0.5; // ADU16

fn rl_parity_ok() -> bool {
    match RL_PARITY.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let ok = match rl_parity_rmse() {
                Ok(rmse) => {
                    eprintln!("[gpu_wavelet] paridad RL GPU-vs-CPU: RMSE {rmse:.5} ADU");
                    rmse <= RL_PARITY_TOL
                }
                Err(e) => {
                    eprintln!("[gpu_wavelet] self-test de paridad RL falló: {e}");
                    false
                }
            };
            RL_PARITY.store(if ok { 1 } else { 2 }, Ordering::Relaxed);
            ok
        }
    }
}

/// RL sobre una escena sintética por CPU (richardson_lucy_core) y por GPU; peor
/// RMSE (ADU16). Público para el test `#[ignore]` en GPU física.
pub fn rl_parity_rmse() -> Result<f64, String> {
    let (w, h) = (128usize, 112usize);
    let tau = std::f32::consts::TAU;
    let mut scene = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let xf = x as f32;
            let yf = y as f32;
            scene[y * w + x] = 9000.0
                + 5000.0 * (xf * tau / 7.3).sin() * (yf * tau / 9.1).cos()
                + 3000.0 * ((xf * 0.6 + yf * 0.8) * tau / 13.7).sin();
        }
    }
    let iters = 5usize;
    let sigma = 2.0f32;

    // CPU: MISMO núcleo que producción, con blur Gaussiano.
    let cpu = crate::richardson_lucy_core(
        &scene,
        &scene,
        w,
        h,
        iters,
        sigma,
        &|img| crate::apply_gaussian_blur(img, w, h, sigma),
        &|| false,
        &|_| {},
    )
    .ok_or("richardson_lucy_core devolvió None")?;

    let gpu = gpu_richardson_lucy_raw(&scene, &scene, w, h, iters, sigma)
        .ok_or("gpu_richardson_lucy_raw devolvió None")?;

    if gpu.len() != cpu.len() {
        return Err("tamaños GPU != CPU".into());
    }
    let mut se = 0.0f64;
    for (a, b) in gpu.iter().zip(cpu.iter()) {
        let d = (*a - *b) as f64;
        se += d * d;
    }
    Ok((se / (w * h) as f64).sqrt())
}

#[cfg(test)]
mod gpu_wavelet_tests {
    use super::*;

    #[test]
    fn memory_guards_include_transient_readback_buffers() {
        let n = 4096usize * 3072;
        assert_eq!(wavelet_vram_bytes(n), n as u64 * 4 * 14);
        assert_eq!(rl_vram_bytes(n), n as u64 * 4 * 8);
    }

    #[test]
    fn non_metal_command_buffers_are_bounded_by_kernel_work() {
        assert_eq!(wavelet_sigmas_per_submit(1920 * 1080, false), 6);
        assert_eq!(wavelet_sigmas_per_submit(3840 * 2160, false), 3);
        assert_eq!(wavelet_sigmas_per_submit(7680 * 4320, false), 1);
        assert_eq!(wavelet_sigmas_per_submit(7680 * 4320, true), 3);

        assert_eq!(rl_iterations_per_submit(1920 * 1080, 2.0, false, 20), 4);
        assert_eq!(rl_iterations_per_submit(3840 * 2160, 2.0, false, 20), 1);
        assert_eq!(rl_iterations_per_submit(3840 * 2160, 2.0, true, 20), 5);
    }

    /// Requiere GPU física (Metal/DX12/Vulkan). Correr con:
    ///   cargo test --bin astro-stacker gpu_wavelet_parity -- --ignored --nocapture
    #[test]
    #[ignore]
    fn gpu_wavelet_parity() {
        match wavelet_parity_rmse() {
            Ok(rmse) => {
                eprintln!("RMSE descomposición GPU vs CPU = {rmse:.6} ADU16");
                assert!(
                    rmse <= WAVELET_PARITY_TOL,
                    "paridad wavelets GPU insuficiente: {rmse}"
                );
            }
            Err(e) => panic!("no se pudo correr el self-test (¿sin GPU?): {e}"),
        }
    }

    /// Paridad de la deconvolución Richardson-Lucy GPU vs CPU (núcleo compartido).
    /// Requiere GPU física. Correr con:
    ///   cargo test --bin astro-stacker gpu_rl_parity -- --ignored --nocapture
    #[test]
    #[ignore]
    fn gpu_rl_parity() {
        match rl_parity_rmse() {
            Ok(rmse) => {
                eprintln!("RMSE Richardson-Lucy GPU vs CPU = {rmse:.6} ADU16");
                assert!(rmse <= RL_PARITY_TOL, "paridad RL GPU insuficiente: {rmse}");
            }
            Err(e) => panic!("no se pudo correr el self-test RL (¿sin GPU?): {e}"),
        }
    }
}
