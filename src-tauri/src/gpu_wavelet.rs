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
                        has_dynamic_offset: false,
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
    let m_ideal = (12.0 * sigma * sigma
        - (n * (wl * wl) as f32)
        - (4.0 * n * wl as f32)
        - (3.0 * n))
        / (-4.0 * wl as f32 - 4.0);
    let m = m_ideal.round().clamp(0.0, n) as usize;
    let sizes = [
        if 0 < m { wl } else { wu },
        if 1 < m { wl } else { wu },
        if 2 < m { wl } else { wu },
    ];
    [
        (sizes[0] - 1) / 2,
        (sizes[1] - 1) / 2,
        (sizes[2] - 1) / 2,
    ]
}

const SIGMAS: [f32; 6] = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0];

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
    // 8 buffers f32 (base + tmp + 6 blurs). Guarda de VRAM.
    let needed = (n as u64) * 4 * 8;
    if needed > rt.vram_budget {
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

    // Un compute pass por dispatch (frontera de pass = barrera de memoria entre
    // pasadas → la escritura de blur_h en tmp es visible por blur_v). Todo en un
    // solo encoder/submit.
    let mut enc = dev.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("zas-blur-enc"),
    });

    // Mantener vivos los uniform buffers y bind groups hasta el submit.
    let mut keep_ubuf: Vec<wgpu::Buffer> = Vec::new();
    let mut keep_bg: Vec<wgpu::BindGroup> = Vec::new();

    // Graba una pasada (blur_h o blur_v) src→dst con radio r y eje.
    // Devuelve el bind group creado (se guarda vivo por el caller).
    let radii_all: Vec<[usize; 3]> = SIGMAS.iter().map(|&s| box_radii(s)).collect();

    for (si, radii) in radii_all.iter().enumerate() {
        // blur_bufs[si] = copia de base, luego box-passes in-place vía tmp.
        enc.copy_buffer_to_buffer(&base_buf, 0, &blur_bufs[si], 0, n4);
        for &r in radii.iter() {
            if r == 0 {
                continue;
            }
            // blur_h: src = blur_bufs[si], dst = tmp
            record_pass(
                rt, &mut enc, bg, &blur_bufs[si], &tmp_buf, width, height, r, 0, &mut keep_ubuf,
                &mut keep_bg,
            );
            // blur_v: src = tmp, dst = blur_bufs[si]
            record_pass(
                rt, &mut enc, bg, &tmp_buf, &blur_bufs[si], width, height, r, 1, &mut keep_ubuf,
                &mut keep_bg,
            );
        }
    }
    rt.queue.submit(Some(enc.finish()));

    // PR-2.3: descargar las 6 blurs con UN solo encoder+submit+map (antes:
    // 6 round-trips submit→map_async→wait SECUENCIALES sobre un staging
    // reutilizado — 6 sincronizaciones CPU↔GPU en el lazo interactivo del
    // editor, anulando parte de la ventaja de la GPU). Staging de 6·n4.
    let staging = dev.create_buffer(&wgpu::BufferDescriptor {
        label: Some("zas-blur-staging"),
        size: n4 * 6,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut blurs: Vec<Vec<f32>> = Vec::with_capacity(6);
    {
        let mut denc = dev.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("zas-blur-download"),
        });
        for (bi, buf) in blur_bufs.iter().enumerate() {
            denc.copy_buffer_to_buffer(buf, 0, &staging, bi as u64 * n4, n4);
        }
        rt.queue.submit(Some(denc.finish()));
        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        if crate::gpu_stack::wait_for_readback(&dev, &rx, "readback wavelet").is_err() {
            return None;
        }
        {
            let data = slice.get_mapped_range();
            let vals: &[f32] = bytemuck::cast_slice(&data);
            let stride = (n4 / 4) as usize;
            for bi in 0..6 {
                blurs.push(vals[bi * stride..bi * stride + n].to_vec());
            }
        }
        staging.unmap();
    }

    if crate::gpu_stack::take_gpu_error() {
        return None;
    }
    if let Some(err) = pollster::block_on(dev.pop_error_scope()) {
        eprintln!("[gpu_wavelet] error de validación en la descomposición: {err}");
        return None;
    }

    // Recombinar en las 7 capas (idéntico a la descomposición de CPU):
    // ls[0] = base - blur[0]; ls[i] = blur[i-1] - blur[i] (1..=5); ls[6] = blur[5].
    let mut ls: Vec<Vec<f32>> = Vec::with_capacity(7);
    ls.push(
        base.iter()
            .zip(&blurs[0])
            .map(|(a, b)| a - b)
            .collect(),
    );
    for i in 0..5 {
        ls.push(
            blurs[i]
                .iter()
                .zip(&blurs[i + 1])
                .map(|(a, b)| a - b)
                .collect(),
        );
    }
    ls.push(blurs[5].clone());
    Some(ls)
}

#[allow(clippy::too_many_arguments)]
fn record_pass(
    rt: &crate::gpu_stack::GpuRuntime,
    enc: &mut wgpu::CommandEncoder,
    bg: &BlurGpu,
    src: &wgpu::Buffer,
    dst: &wgpu::Buffer,
    width: usize,
    height: usize,
    r: usize,
    axis: u32,
    keep_ubuf: &mut Vec<wgpu::Buffer>,
    keep_bg: &mut Vec<wgpu::BindGroup>,
) {
    let dev = &rt.device;
    let params = BlurParams {
        w: width as u32,
        h: height as u32,
        r: r as u32,
        axis,
    };
    let ubuf = dev.create_buffer(&wgpu::BufferDescriptor {
        label: Some("zas-blur-params"),
        size: std::mem::size_of::<BlurParams>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    // El uniform se rellena por la cola ANTES del submit del compute (todas las
    // escrituras de la cola preceden al submit siguiente).
    keep_ubuf.push(ubuf);
    let ubuf_ref = keep_ubuf.last().unwrap();
    rt.queue
        .write_buffer(ubuf_ref, 0, bytemuck::bytes_of(&params));

    let bind = dev.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("zas-blur-bg"),
        layout: &bg.layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: ubuf_ref.as_entire_binding(),
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
    pass.set_bind_group(0, bind_ref, &[]);
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
                    eprintln!("[gpu_wavelet] paridad descomposición GPU-vs-CPU: RMSE {rmse:.5} ADU");
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
        cpu.push(blurs[i].iter().zip(&blurs[i + 1]).map(|(a, b)| a - b).collect());
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
            has_dynamic_offset: false,
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
        let (ratio_pipe, ratio_layout) =
            build_pipeline(&rt.device, RATIO_WGSL, "zas-rl-ratio", 2)?;
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
    rt: &crate::gpu_stack::GpuRuntime,
    enc: &mut wgpu::CommandEncoder,
    bg: &BlurGpu,
    src: &wgpu::Buffer,
    out: &wgpu::Buffer,
    tmp: &wgpu::Buffer,
    w: usize,
    h: usize,
    sigma: f32,
    keep_ubuf: &mut Vec<wgpu::Buffer>,
    keep_bg: &mut Vec<wgpu::BindGroup>,
) {
    let n4 = (w * h * 4) as u64;
    enc.copy_buffer_to_buffer(src, 0, out, 0, n4);
    for &r in box_radii(sigma).iter() {
        if r == 0 {
            continue;
        }
        record_pass(rt, enc, bg, out, tmp, w, h, r, 0, keep_ubuf, keep_bg);
        record_pass(rt, enc, bg, tmp, out, w, h, r, 1, keep_ubuf, keep_bg);
    }
}

/// Graba un kernel pointwise (workgroup 64 sobre n píxeles). `bufs` en orden de
/// binding 1.. (los RO primero, el RW al final, como el layout).
#[allow(clippy::too_many_arguments)]
fn record_pointwise(
    rt: &crate::gpu_stack::GpuRuntime,
    enc: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::ComputePipeline,
    layout: &wgpu::BindGroupLayout,
    uniform: [u32; 4],
    bufs: &[&wgpu::Buffer],
    n: usize,
    keep_ubuf: &mut Vec<wgpu::Buffer>,
    keep_bg: &mut Vec<wgpu::BindGroup>,
) {
    let dev = &rt.device;
    let ubuf = dev.create_buffer(&wgpu::BufferDescriptor {
        label: Some("zas-rl-params"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    keep_ubuf.push(ubuf);
    let ubuf_ref = keep_ubuf.last().unwrap();
    rt.queue
        .write_buffer(ubuf_ref, 0, bytemuck::cast_slice(&uniform));
    let mut entries = vec![wgpu::BindGroupEntry {
        binding: 0,
        resource: ubuf_ref.as_entire_binding(),
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
    pass.set_bind_group(0, bind_ref, &[]);
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
    let needed = (width * height) as u64 * 4 * 7;
    if needed > rt.vram_budget {
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
    if n == 0 || input.len() != n || original.len() != n {
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

    rt.queue.write_buffer(&est_a, 0, bytemuck::cast_slice(input));
    rt.queue
        .write_buffer(&orig_buf, 0, bytemuck::cast_slice(original));
    rt.queue
        .write_buffer(&mask_buf, 0, bytemuck::cast_slice(&mask));

    dev.push_error_scope(wgpu::ErrorFilter::Validation);
    let mut enc = dev.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("zas-rl-enc"),
    });
    let mut keep_ubuf: Vec<wgpu::Buffer> = Vec::new();
    let mut keep_bg: Vec<wgpu::BindGroup> = Vec::new();

    let (mut cur, mut next) = (&est_a, &est_b);
    for _ in 0..iterations {
        // blurred_est = gaussian(cur) → blur_buf
        record_gaussian(
            rt, &mut enc, bg, cur, &blur_buf, &tmp_buf, width, height, sigma, &mut keep_ubuf,
            &mut keep_bg,
        );
        // ratio: (orig, blur_buf) → ratio_buf
        record_pointwise(
            rt, &mut enc, &rl.ratio_pipe, &rl.ratio_layout, [n as u32, 0, 0, 0],
            &[&orig_buf, &blur_buf, &ratio_buf], n, &mut keep_ubuf, &mut keep_bg,
        );
        // blurred_ratio = gaussian(ratio_buf) → blur_buf
        record_gaussian(
            rt, &mut enc, bg, &ratio_buf, &blur_buf, &tmp_buf, width, height, sigma,
            &mut keep_ubuf, &mut keep_bg,
        );
        // update (Jacobi): (cur, blur_buf, mask, orig) → next
        record_pointwise(
            rt, &mut enc, &rl.update_pipe, &rl.update_layout,
            [width as u32, height as u32, n as u32, 0],
            &[cur, &blur_buf, &mask_buf, &orig_buf, next], n, &mut keep_ubuf, &mut keep_bg,
        );
        std::mem::swap(&mut cur, &mut next);
    }
    rt.queue.submit(Some(enc.finish()));

    // Descargar `cur` (tras el último swap contiene el estimado final).
    let staging = dev.create_buffer(&wgpu::BufferDescriptor {
        label: Some("zas-rl-staging"),
        size: n4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut denc = dev.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("zas-rl-download"),
    });
    denc.copy_buffer_to_buffer(cur, 0, &staging, 0, n4);
    rt.queue.submit(Some(denc.finish()));
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

    /// Requiere GPU física (Metal/DX12/Vulkan). Correr con:
    ///   cargo test --bin astro-stacker gpu_wavelet_parity -- --ignored --nocapture
    #[test]
    #[ignore]
    fn gpu_wavelet_parity() {
        match wavelet_parity_rmse() {
            Ok(rmse) => {
                eprintln!("RMSE descomposición GPU vs CPU = {rmse:.6} ADU16");
                assert!(rmse <= WAVELET_PARITY_TOL, "paridad wavelets GPU insuficiente: {rmse}");
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
