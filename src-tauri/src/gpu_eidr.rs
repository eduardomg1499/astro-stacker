// ============================================================================
// GPU EIDR (F10, §7.7): apply/adjoint/diag del operador forward-model en
// wgpu. El producto de las ecuaciones normales (el bucle caliente del PCG:
// iteraciones × frames × 2 gathers) corre en GPU; el PCG, la penalización
// espectral y las reducciones f64 permanecen en CPU (§7.7: float32 para
// imágenes, reducciones en f64).
//
// Los kernels replican EXACTAMENTE los gathers de eidr.rs: misma LUT
// bilineal (f32), misma máscara (bitset u64 reinterpretado como u32 LE),
// mismas paridades CFA y pesos robustos. La paridad CPU/GPU se verifica con
// un gate dedicado (RMSE ≤ 0.5 ADU) que corre en hardware real (--ignored),
// y en producción con un mini-chequeo cacheado antes del primer uso — sin
// paridad no hay GPU (fallback CPU declarado, jamás silencioso).
// ============================================================================

use std::sync::OnceLock;

use crate::eidr::EidrOperator;
use crate::gpu_stack::{gpu_runtime, wait_for_readback};

const EIDR_WGSL: &str = r#"
struct Params {
    fa: vec4<f32>,   // f0x f0y fx0 fx1
    fb: vec4<f32>,   // fy0 fy1 g0x g0y
    fc: vec4<f32>,   // gx0 gx1 gy0 gy1
    dims: vec4<u32>, // frame_w frame_h out_w out_h
    lutp: vec4<f32>, // center inv_step radius q_rad
    misc: vec4<f32>, // p_rad weight _ _
    mode: vec4<u32>, // mode(0 apply/1 adjoint/2 diag) parity_mask lut_n _
};

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> lut: array<f32>;
@group(0) @binding(2) var<storage, read> input_buf: array<f32>;
@group(0) @binding(3) var<storage, read_write> out_buf: array<f32>;
@group(0) @binding(4) var<storage, read> mask_buf: array<u32>;
@group(0) @binding(5) var<storage, read> rw_buf: array<f32>;

fn lut_eval(dx: f32, dy: f32) -> f32 {
    let x = dx * P.lutp.y + P.lutp.x;
    let y = dy * P.lutp.y + P.lutp.x;
    if (x < 0.0 || y < 0.0) {
        return 0.0;
    }
    let x0 = u32(x);
    let y0 = u32(y);
    let n = P.mode.z;
    if (x0 + 1u >= n || y0 + 1u >= n) {
        return 0.0;
    }
    let fx = x - f32(x0);
    let fy = y - f32(y0);
    let r0 = y0 * n + x0;
    let r1 = r0 + n;
    return lut[r0] * (1.0 - fx) * (1.0 - fy) + lut[r0 + 1u] * fx * (1.0 - fy)
        + lut[r1] * (1.0 - fx) * fy + lut[r1 + 1u] * fx * fy;
}

fn is_masked(p: u32) -> bool {
    return ((mask_buf[p >> 5u] >> (p & 31u)) & 1u) == 1u;
}

fn parity_ok(px: u32, py: u32) -> bool {
    let bit = ((py & 1u) << 1u) | (px & 1u);
    return ((P.mode.y >> bit) & 1u) == 1u;
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let mode = P.mode.x;
    if (mode == 0u) {
        // APPLY: predicción del frame — gather sobre la ventana de salida.
        let px = gid.x;
        let py = gid.y;
        if (px >= P.dims.x || py >= P.dims.y) {
            return;
        }
        let pidx = py * P.dims.x + px;
        out_buf[pidx] = 0.0;
        if (!parity_ok(px, py) || is_masked(pidx)) {
            return;
        }
        let gx = P.fb.z + P.fc.x * f32(px) + P.fc.z * f32(py);
        let gy = P.fb.w + P.fc.y * f32(px) + P.fc.w * f32(py);
        let q_rad = P.lutp.w;
        let q0x = u32(max(ceil(gx - q_rad), 0.0));
        let q1xf = min(floor(gx + q_rad), f32(P.dims.z - 1u));
        let q0y = u32(max(ceil(gy - q_rad), 0.0));
        let q1yf = min(floor(gy + q_rad), f32(P.dims.w - 1u));
        if (q1xf < f32(q0x) || q1yf < f32(q0y)) {
            return;
        }
        let q1x = u32(q1xf);
        let q1y = u32(q1yf);
        var acc: f32 = 0.0;
        for (var qy: u32 = q0y; qy <= q1y; qy = qy + 1u) {
            var fxq = P.fa.x + P.fa.z * f32(q0x) + P.fb.x * f32(qy);
            var fyq = P.fa.y + P.fa.w * f32(q0x) + P.fb.y * f32(qy);
            for (var qx: u32 = q0x; qx <= q1x; qx = qx + 1u) {
                let k = lut_eval(f32(px) - fxq, f32(py) - fyq);
                fxq = fxq + P.fa.z;
                fyq = fyq + P.fa.w;
                if (k != 0.0) {
                    acc = acc + k * input_buf[qy * P.dims.z + qx];
                }
            }
        }
        out_buf[pidx] = acc;
        return;
    }
    // ADJOINT (1) / DIAG (2): gather sobre la ventana del frame, por píxel
    // de salida; ACUMULA (+=) — el caller inicializa el buffer.
    let qx = gid.x;
    let qy = gid.y;
    if (qx >= P.dims.z || qy >= P.dims.w) {
        return;
    }
    let fxq = P.fa.x + P.fa.z * f32(qx) + P.fb.x * f32(qy);
    let fyq = P.fa.y + P.fa.w * f32(qx) + P.fb.y * f32(qy);
    let rad = P.misc.x;
    let p0x = u32(max(ceil(fxq - rad), 0.0));
    let p1xf = min(floor(fxq + rad), f32(P.dims.x - 1u));
    let p0y = u32(max(ceil(fyq - rad), 0.0));
    let p1yf = min(floor(fyq + rad), f32(P.dims.y - 1u));
    if (p1xf < f32(p0x) || p1yf < f32(p0y)) {
        return;
    }
    let p1x = u32(p1xf);
    let p1y = u32(p1yf);
    var acc: f32 = 0.0;
    for (var py: u32 = p0y; py <= p1y; py = py + 1u) {
        for (var px: u32 = p0x; px <= p1x; px = px + 1u) {
            if (!parity_ok(px, py)) {
                continue;
            }
            let pidx = py * P.dims.x + px;
            if (is_masked(pidx)) {
                continue;
            }
            let k = lut_eval(f32(px) - fxq, f32(py) - fyq);
            if (k != 0.0) {
                if (P.mode.x == 1u) {
                    acc = acc + k * input_buf[pidx] * rw_buf[pidx];
                } else {
                    acc = acc + k * k * rw_buf[pidx];
                }
            }
        }
    }
    let qidx = qy * P.dims.z + qx;
    out_buf[qidx] = out_buf[qidx] + P.misc.y * acc;
}
"#;

struct EidrGpuPipe {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

fn eidr_pipe() -> Option<&'static EidrGpuPipe> {
    static P: OnceLock<Option<EidrGpuPipe>> = OnceLock::new();
    P.get_or_init(|| {
        let rt = gpu_runtime()?;
        let dev = &rt.device;
        dev.push_error_scope(wgpu::ErrorFilter::Validation);
        let shader = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("zas-eidr-shader"),
            source: wgpu::ShaderSource::Wgsl(EIDR_WGSL.into()),
        });
        let storage_ro = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("zas-eidr-layout"),
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
                storage_ro(1),
                storage_ro(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                storage_ro(4),
                storage_ro(5),
            ],
        });
        let pipe_layout = dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("zas-eidr-pipe-layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = dev.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("zas-eidr-pipeline"),
            layout: Some(&pipe_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        if let Some(err) = pollster::block_on(dev.pop_error_scope()) {
            eprintln!("[gpu_eidr] shader/pipeline no valida: {err}");
            return None;
        }
        Some(EidrGpuPipe { pipeline, layout })
    })
    .as_ref()
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct EidrParams {
    fa: [f32; 4],
    fb: [f32; 4],
    fc: [f32; 4],
    dims: [u32; 4],
    lutp: [f32; 4],
    misc: [f32; 4],
    mode: [u32; 4],
}

fn make_buffer(dev: &wgpu::Device, label: &str, bytes: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
    use wgpu::util::DeviceExt;
    dev.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents: bytes,
        usage,
    })
}

/// Paridades CFA del canal como máscara de 4 bits sobre (y&1)·2+(x&1).
fn parity_mask(cfa: Option<i32>, c: usize) -> u32 {
    match cfa {
        None => 0b1111,
        Some(cid) => {
            let mut m = 0u32;
            for y in 0..2usize {
                for x in 0..2usize {
                    if crate::ds_cfa_channel(cid, x, y) == c {
                        m |= 1 << ((y << 1) | x);
                    }
                }
            }
            m
        }
    }
}

/// Contexto GPU para el matvec de las ecuaciones normales de UN solve
/// (canal + subconjunto de frames fijos). Buffers estáticos por frame (LUT,
/// máscara, pesos, uniforms de apply/adjoint) + z/tmp/out reutilizados.
pub(crate) struct EidrGpuMatvec {
    frames: Vec<(EidrParams, EidrParams, wgpu::Buffer, wgpu::Buffer, wgpu::Buffer)>,
    z_buf: wgpu::Buffer,
    tmp_buf: wgpu::Buffer,
    out_buf: wgpu::Buffer,
    read_buf: wgpu::Buffer,
    n_out: usize,
}

impl EidrGpuMatvec {
    /// None si no hay runtime/pipeline o los buffers exceden los límites.
    pub(crate) fn new(op: &EidrOperator, idxs: &[usize], c: usize) -> Option<Self> {
        let rt = gpu_runtime()?;
        let pipe = eidr_pipe()?;
        let _ = &pipe.pipeline;
        let dev = &rt.device;
        let n_out = op.w_out * op.h_out;
        let out_bytes = (n_out * 4) as u64;
        let max_frame = idxs
            .iter()
            .map(|&fi| op.frames[fi].w * op.frames[fi].h)
            .max()?;
        if out_bytes > rt.max_binding || (max_frame * 4) as u64 > rt.max_binding {
            return None;
        }
        let pm = parity_mask(op.cfa, c);
        let mut frames = Vec::with_capacity(idxs.len());
        for &fi in idxs {
            let f = &op.frames[fi];
            let g = &f.geom;
            let (lut_data, lut_n, lut_radius, lut_inv_step) = f.lut.raw();
            let lut_center = (lut_n as f32 - 1.0) * 0.5;
            let base = EidrParams {
                fa: [g.f0[0] as f32, g.f0[1] as f32, g.fx[0] as f32, g.fx[1] as f32],
                fb: [g.fy[0] as f32, g.fy[1] as f32, g.g0[0] as f32, g.g0[1] as f32],
                fc: [g.gx[0] as f32, g.gx[1] as f32, g.gy[0] as f32, g.gy[1] as f32],
                dims: [f.w as u32, f.h as u32, op.w_out as u32, op.h_out as u32],
                lutp: [
                    lut_center,
                    lut_inv_step,
                    lut_radius,
                    (lut_radius as f64 * g.q_rad_factor + 1.0) as f32,
                ],
                misc: [lut_radius + 0.5, f.inv_var[c.min(2)], 0.0, 0.0],
                mode: [0, pm, lut_n as u32, 0],
            };
            let mut adj = base;
            adj.mode[0] = 1;
            let lut_buf = make_buffer(
                dev,
                "zas-eidr-lut",
                bytemuck::cast_slice(lut_data),
                wgpu::BufferUsages::STORAGE,
            );
            let mask_buf = make_buffer(
                dev,
                "zas-eidr-mask",
                bytemuck::cast_slice(&f.mask),
                wgpu::BufferUsages::STORAGE,
            );
            let ones;
            let rw_slice: &[f32] = match f.robust_w.as_deref() {
                Some(w) => w,
                None => {
                    ones = vec![1.0f32; f.w * f.h];
                    &ones
                }
            };
            let rw_buf = make_buffer(
                dev,
                "zas-eidr-rw",
                bytemuck::cast_slice(rw_slice),
                wgpu::BufferUsages::STORAGE,
            );
            frames.push((base, adj, lut_buf, mask_buf, rw_buf));
        }
        let z_buf = dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zas-eidr-z"),
            size: out_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let tmp_buf = dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zas-eidr-tmp"),
            size: (max_frame * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let out_buf = dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zas-eidr-out"),
            size: out_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let read_buf = dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zas-eidr-read"),
            size: out_bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Some(Self {
            frames,
            z_buf,
            tmp_buf,
            out_buf,
            read_buf,
            n_out,
        })
    }

    /// out = Σ_i A_iᵀ W_i Σ_i⁻¹ A_i p — el producto normal completo en GPU.
    pub(crate) fn matvec(&self, p_in: &[f32], out: &mut [f64]) -> Result<(), String> {
        let rt = gpu_runtime().ok_or("GPU: runtime perdido")?;
        let pipe = eidr_pipe().ok_or("GPU: pipeline EIDR no disponible")?;
        let (dev, queue) = (&rt.device, &rt.queue);
        debug_assert_eq!(p_in.len(), self.n_out);
        queue.write_buffer(&self.z_buf, 0, bytemuck::cast_slice(p_in));
        queue.write_buffer(&self.out_buf, 0, &vec![0u8; self.n_out * 4]);
        // Lotes de frames por submit: dispatches cortos bajo el watchdog.
        for chunk in self.frames.chunks(8) {
            let mut enc = dev.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("zas-eidr-matvec"),
            });
            for (base, adj, lut_buf, mask_buf, rw_buf) in chunk {
                let pbuf_a = make_buffer(
                    dev,
                    "zas-eidr-params-a",
                    bytemuck::bytes_of(base),
                    wgpu::BufferUsages::UNIFORM,
                );
                let pbuf_b = make_buffer(
                    dev,
                    "zas-eidr-params-b",
                    bytemuck::bytes_of(adj),
                    wgpu::BufferUsages::UNIFORM,
                );
                let bind = |params: &wgpu::Buffer, input: &wgpu::Buffer, output: &wgpu::Buffer| {
                    dev.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("zas-eidr-bind"),
                        layout: &pipe.layout,
                        entries: &[
                            wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() },
                            wgpu::BindGroupEntry { binding: 1, resource: lut_buf.as_entire_binding() },
                            wgpu::BindGroupEntry { binding: 2, resource: input.as_entire_binding() },
                            wgpu::BindGroupEntry { binding: 3, resource: output.as_entire_binding() },
                            wgpu::BindGroupEntry { binding: 4, resource: mask_buf.as_entire_binding() },
                            wgpu::BindGroupEntry { binding: 5, resource: rw_buf.as_entire_binding() },
                        ],
                    })
                };
                let bg_apply = bind(&pbuf_a, &self.z_buf, &self.tmp_buf);
                let bg_adj = bind(&pbuf_b, &self.tmp_buf, &self.out_buf);
                let (fw, fh) = (base.dims[0], base.dims[1]);
                let (ow, oh) = (base.dims[2], base.dims[3]);
                {
                    let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                        label: Some("zas-eidr-apply"),
                        timestamp_writes: None,
                    });
                    pass.set_pipeline(&pipe.pipeline);
                    pass.set_bind_group(0, &bg_apply, &[]);
                    pass.dispatch_workgroups(fw.div_ceil(8), fh.div_ceil(8), 1);
                }
                {
                    let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                        label: Some("zas-eidr-adjoint"),
                        timestamp_writes: None,
                    });
                    pass.set_pipeline(&pipe.pipeline);
                    pass.set_bind_group(0, &bg_adj, &[]);
                    pass.dispatch_workgroups(ow.div_ceil(8), oh.div_ceil(8), 1);
                }
            }
            queue.submit([enc.finish()]);
        }
        // Readback único del vector resultado.
        let mut enc = dev.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("zas-eidr-read"),
        });
        enc.copy_buffer_to_buffer(&self.out_buf, 0, &self.read_buf, 0, (self.n_out * 4) as u64);
        queue.submit([enc.finish()]);
        let slice = self.read_buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        wait_for_readback(dev, &rx, "EIDR matvec")?;
        {
            let view = slice.get_mapped_range();
            let vals: &[f32] = bytemuck::cast_slice(&view);
            for (dst, &src) in out.iter_mut().zip(vals.iter()) {
                *dst = src as f64;
            }
        }
        self.read_buf.unmap();
        Ok(())
    }
}

/// Caso sintético propio para el gate de paridad (autocontenido: no depende
/// del dataset del usuario).
fn parity_op() -> EidrOperator {
    use crate::deepsky_psf::MoffatPsf;
    use crate::eidr::*;
    let gamma = MoffatPsf { fwhm_x: 2.2, fwhm_y: 2.2, theta: 0.0, beta: 2.5 };
    let frames = [
        (0.0f32, (0.0f32, 0.0f32)),
        (0.12, (0.4, -0.7)),
        (0.0, (1.3, 0.6)),
    ]
    .iter()
    .map(|&(th, (dx, dy))| {
        let (sn, cs) = th.sin_cos();
        let t = crate::DsTransform::from_similarity((cs, sn, dx, dy));
        let geom = eidr_geom(&t, 2.0).expect("similitud");
        let lut = eidr_build_lut(
            Some(MoffatPsf { fwhm_x: 2.0, fwhm_y: 1.9, theta: 0.2, beta: 2.4 }),
            gamma,
            &geom,
        );
        EidrFrameOp {
            geom,
            lut,
            inv_var: [1.0 / 150.0; 3],
            mask: vec![0u64; (48 * 40 + 63) / 64],
            robust_w: None,
            w: 48,
            h: 40,
        }
    })
    .collect();
    EidrOperator { frames, w_out: 96, h_out: 80, cfa: None, ch: 1 }
}

/// Paridad CPU/GPU del matvec sobre un caso sintético pequeño, cacheada: el
/// primer uso real la exige (gate físico estilo ensure_*_parity). RMSE ≤
/// 0.5 ADU con señal ~1e3 (criterio F8/F10).
pub(crate) fn ensure_eidr_parity() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let op = parity_op();
        let idxs: Vec<usize> = (0..op.frames.len()).collect();
        let c = 0usize;
        let Some(ctx) = EidrGpuMatvec::new(&op, &idxs, c) else {
            return false;
        };
        let n = op.w_out * op.h_out;
        let mut state = 0x1234_5678u64;
        let p_in: Vec<f32> = (0..n)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((state >> 33) as f64 / (u32::MAX as f64) * 2000.0 - 1000.0) as f32
            })
            .collect();
        let mut cpu = vec![0.0f64; n];
        let mut scratch = Vec::new();
        op.normal_apply(c, &idxs, &p_in, &mut cpu, &mut scratch);
        let mut gpu = vec![0.0f64; n];
        if ctx.matvec(&p_in, &mut gpu).is_err() {
            return false;
        }
        let mut se = 0.0f64;
        let mut scale = 0.0f64;
        for (a, b) in cpu.iter().zip(gpu.iter()) {
            se += (a - b) * (a - b);
            scale = scale.max(a.abs());
        }
        let rmse = (se / n as f64).sqrt();
        // El matvec está en unidades ADU/σ²: normalizar a ADU con la escala.
        let ok = scale > 0.0 && rmse <= 0.5 * (scale / 1000.0).max(1e-9);
        if !ok {
            eprintln!("[gpu_eidr] paridad CPU/GPU fallida: rmse={rmse:.3e} escala={scale:.3e}");
        }
        ok
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deepsky_psf::MoffatPsf;
    use crate::eidr::*;

    fn tiny_op() -> EidrOperator {
        let gamma = MoffatPsf { fwhm_x: 2.2, fwhm_y: 2.2, theta: 0.0, beta: 2.5 };
        let frames = [
            (0.0f32, (0.0f32, 0.0f32)),
            (0.12, (0.4, -0.7)),
            (0.0, (1.3, 0.6)),
        ]
        .iter()
        .map(|&(th, (dx, dy))| {
            let (s, c) = th.sin_cos();
            let t = crate::DsTransform::from_similarity((c, s, dx, dy));
            let geom = eidr_geom(&t, 2.0).unwrap();
            let lut = eidr_build_lut(
                Some(MoffatPsf { fwhm_x: 2.0, fwhm_y: 1.9, theta: 0.2, beta: 2.4 }),
                gamma,
                &geom,
            );
            EidrFrameOp {
                geom,
                lut,
                inv_var: [1.0 / 150.0; 3],
                mask: vec![0u64; (48 * 40 + 63) / 64],
                robust_w: None,
                w: 48,
                h: 40,
            }
        })
        .collect();
        EidrOperator { frames, w_out: 96, h_out: 80, cfa: None, ch: 1 }
    }

    /// Paridad CPU/GPU del matvec completo (requiere GPU real: --ignored).
    #[test]
    #[ignore]
    fn gate_f10_gpu_matvec_parity() {
        let op = tiny_op();
        let idxs: Vec<usize> = (0..op.frames.len()).collect();
        let ctx = EidrGpuMatvec::new(&op, &idxs, 0).expect("runtime GPU");
        let n = op.w_out * op.h_out;
        let mut state = 0xabcdefu64;
        let p_in: Vec<f32> = (0..n)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                ((state >> 33) as f64 / (u32::MAX as f64) * 2000.0 - 1000.0) as f32
            })
            .collect();
        let mut cpu = vec![0.0f64; n];
        let mut scratch = Vec::new();
        op.normal_apply(0, &idxs, &p_in, &mut cpu, &mut scratch);
        let mut gpu = vec![0.0f64; n];
        ctx.matvec(&p_in, &mut gpu).expect("matvec GPU");
        let mut se = 0.0f64;
        let mut maxdev = 0.0f64;
        let mut scale = 0.0f64;
        for (a, b) in cpu.iter().zip(gpu.iter()) {
            se += (a - b) * (a - b);
            maxdev = maxdev.max((a - b).abs());
            scale = scale.max(a.abs());
        }
        let rmse = (se / n as f64).sqrt();
        // Unidades del matvec: ADU/σ² — el criterio 0.5 ADU se escala.
        let adu = scale / 1000.0;
        assert!(
            rmse <= 0.5 * adu && maxdev <= 2.0 * adu,
            "paridad: rmse={rmse:.3e} max={maxdev:.3e} escala={scale:.3e}"
        );
        assert!(ensure_eidr_parity());
    }
}
