// ===========================================================================
// GPU COMPUTE (wgpu) — etapa de acumulación del apilado planetario.
//
// ALCANCE v1 (decisión de producto): SOLO el render del warp + acumulación
// (+ estadística sigma-clip) corre en GPU. La alineación per-AP (SAD+LK),
// el decode, el debayer y el post quedan en CPU — el pipeline se solapa:
// CPU alinea el frame N+1 mientras la GPU acumula el N.
//
// GARANTÍAS DE NO-ROTURA (3 capas):
//  1. Detección: sin adapter utilizable → None → la ruta CPU actual, intacta.
//  2. Paridad: al primer uso corre un mini-stack sintético por AMBAS rutas;
//     si el RMSE supera la tolerancia, GPU queda deshabilitada la sesión.
//  3. Fallback en caliente: cualquier error wgpu a mitad de pase marca el
//     pase como fallido y el caller lo REINICIA completo en CPU (barato: el
//     caché de decode por-frame re-sirve los frames desde disco).
//
// PRECISIÓN SIN f64 (Metal no lo soporta): los acumuladores viven en PUNTO
// FIJO de 64 bits emulado con pares (lo, hi) u32 y carry manual — Q40.24
// para las sumas (val ≤ 65535 × w ≤ 1 × ~20k frames) y Q56.8 para m2
// (val² ≤ 4.3e9). Por qué no Kahan-f32: el compilador Metal aplica fast-math
// y la reasociación DESTRUYE la compensación de Kahan; el punto fijo es
// inmune, determinista y reproducible (mejor aún que la CPU, cuyo orden de
// suma varía con el scheduling), con error acotado a la cuantización 2^-24
// por término (~0.3 ADU σ tras 5000 frames). Sin atomics: cada píxel de
// salida lo escribe UN solo hilo (gather), read-modify-write normal.
// ===========================================================================

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::OnceLock;

/// Estado del self-test de paridad: 0 = pendiente, 1 = ok, 2 = fallido.
static PARITY_STATE: AtomicU8 = AtomicU8::new(0);

/// Error no capturado del device (device lost / OOM asincrono): el submitter
/// lo consulta tras cada frame y dispara el fallback a CPU del pase.
static GPU_ERROR_FLAG: AtomicBool = AtomicBool::new(false);

/// Device PERDIDO de verdad (callback set_device_lost_callback): a partir de
/// aquí `gpu_runtime()` devuelve None y toda la sesión continúa en CPU, en
/// lugar de reintentar cada lote contra un dispositivo muerto (cada intento
/// costaría el deadline completo del readback).
static GPU_LOST: AtomicBool = AtomicBool::new(false);

pub fn take_gpu_error() -> bool {
    GPU_ERROR_FLAG.swap(false, Ordering::Relaxed)
}

#[cfg(test)]
fn inject_device_loss_for_test() {
    GPU_ERROR_FLAG.store(true, Ordering::Relaxed);
}

/// Espera ACOTADA de un `map_async`: alterna poll no bloqueante con recv con
/// timeout y un deadline duro. Un device perdido (watchdog Metal/TDR, OOM
/// asíncrono) puede dejar el callback sin disparar jamás — el patrón anterior
/// `poll(Wait)` + `recv()` bloqueante congelaba el análisis/apilado PARA
/// SIEMPRE (barra en 0, Cancelar muerto). Con esto, cualquier pérdida se
/// convierte en `Err` y el caller cae a CPU limpiamente.
pub fn wait_for_readback(
    device: &wgpu::Device,
    rx: &std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>,
    what: &str,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        device.poll(wgpu::Maintain::Poll);
        match rx.recv_timeout(std::time::Duration::from_millis(20)) {
            Ok(result) => return result.map_err(|e| format!("Map GPU ({what}): {e:?}")),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if GPU_ERROR_FLAG.load(Ordering::Relaxed) {
                    return Err(format!("Device loss/OOM durante {what}"));
                }
                if std::time::Instant::now() >= deadline {
                    GPU_ERROR_FLAG.store(true, Ordering::Relaxed);
                    return Err(format!(
                        "Timeout de readback GPU en {what} (>20 s): device sin respuesta — se usa CPU"
                    ));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(format!("Readback GPU cancelado ({what})"));
            }
        }
    }
}

pub struct GpuRuntime {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub adapter_name: String,
    pub backend: String,
    /// Presupuesto de VRAM utilizable para los buffers del pase (bytes):
    /// min(max_buffer_size, max_storage_buffer_binding_size) × 0.6.
    pub vram_budget: u64,
    pub max_binding: u64,
    pipeline: wgpu::ComputePipeline,
    bind_layout: wgpu::BindGroupLayout,
    /// LUT Lanczos idéntica a la de CPU (LanczosLUT::new(30000, 3.0)) subida
    /// una vez — la paridad del muestreo depende de compartir la MISMA tabla.
    lut_buf: wgpu::Buffer,
}

static GPU_RUNTIME: OnceLock<Option<GpuRuntime>> = OnceLock::new();

/// Punto único de acceso al runtime GPU. Inicializa una sola vez; cualquier
/// fallo (sin adapter, sin device, backend inservible) devuelve None y la app
/// se comporta EXACTAMENTE como antes de que existiera este módulo.
/// `ZAS_FORCE_CPU=1` salta la GPU por completo (verificación / soporte).
pub fn gpu_runtime() -> Option<&'static GpuRuntime> {
    if GPU_LOST.load(Ordering::Relaxed) {
        return None;
    }
    GPU_RUNTIME.get_or_init(init_runtime).as_ref()
}

fn backend_label(b: wgpu::Backend) -> &'static str {
    match b {
        wgpu::Backend::Metal => "Metal",
        wgpu::Backend::Dx12 => "DX12",
        wgpu::Backend::Vulkan => "Vulkan",
        wgpu::Backend::Gl => "OpenGL",
        wgpu::Backend::BrowserWebGpu => "WebGPU",
        wgpu::Backend::Empty => "Ninguno",
    }
}

// ===========================================================================
// KERNEL WGSL — réplica EXACTA (línea a línea) de la cadena CPU fusionada:
// accumulate_frame_liquid(_mono) → normalización per-frame → sigma-rejection
// → StripedAccum::accumulate. La fusión es válida porque la normalización
// divide el valor por ww y StripedAccum usa ww solo como gate/cobertura.
// Cada píxel de salida lo procesa UN hilo (gather): sin atomics.
// ===========================================================================
const ACCUM_WGSL: &str = r#"
struct Params {
    w_in: u32, h_in: u32, w_out: u32, h_out: u32,
    band_y0: u32, k: u32, n_px_out: u32, flags: u32,
    inv_drizzle: f32, roi_off_x: f32, roi_off_y: f32, drop_size: f32,
    render_dx: f32, render_dy: f32, gfw: f32, gw: f32,
    drz_h1: f32, drz_h2: f32, drz_full_cov: f32, lut_scale: f32,
    q_scale_x: f32, q_scale_y: f32, q_off_x: f32, q_off_y: f32,
    dq_w: u32, dq_h: u32, n_aps: u32, band_y1: u32,
}
// flags: 1=use_warp, 2=true_drizzle, 4=is_color, 8=track_m2, 16=use_bounds,
//        32=coverage_weighting

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read> frame_px: array<u32>;   // u16 x2 packed
@group(0) @binding(2) var<storage, read> warp_idx: array<u32>;   // u16 x2 packed
@group(0) @binding(3) var<storage, read> warp_w: array<f32>;
@group(0) @binding(4) var<storage, read> apq: array<vec4<f32>>;  // dx,dy,q_eff,0
@group(0) @binding(5) var<storage, read> lut: array<f32>;        // Lanczos 30000
@group(0) @binding(6) var<storage, read> dq: array<f32>;         // quality 1/4 res
@group(0) @binding(7) var<storage, read> bounds: array<f32>;     // lo/hi por canal
@group(0) @binding(8) var<storage, read_write> acc: array<vec2<u32>>; // punto fijo

fn frame_val(i: u32) -> f32 {
    let w = frame_px[i >> 1u];
    return f32((w >> ((i & 1u) * 16u)) & 0xFFFFu);
}

fn warp_index(p: u32) -> u32 {
    let w = warp_idx[p >> 1u];
    return (w >> ((p & 1u) * 16u)) & 0xFFFFu;
}

// round() de WGSL es half-to-even; el f32::round() de Rust es half-AWAY-from-
// zero. floor(v+0.5) replica a Rust para todos los casos que sobreviven al
// clamp posterior (los empates exactos .5 son FRECUENTES aqui: el lookup del
// dq map usa escalas 0.25 → sin esto, el 25% de las columnas leia OTRA celda
// de calidad y la paridad se iba a ~49 ADU).
fn round_rs(v: f32) -> f32 {
    return floor(v + 0.5);
}

// Réplica de LanczosLUT::get — MISMA tabla y MISMO truncado de índice que CPU.
fn lut_get(x: f32, drop: f32) -> f32 {
    let ax = abs(x / drop);
    if (ax >= 3.0) { return 0.0; }
    let idx = u32(ax * P.lut_scale);
    if (idx >= 30000u) { return 0.0; }
    return lut[idx];
}

// ---------------------------------------------------------------------------
// PUNTO FIJO 64-bit emulado (lo, hi) con carry manual — inmune al fast-math
// del compilador Metal (Kahan-f32 se rompería por reasociación) y
// determinista. Q40.24 para sumas; Q56.8 para m2 (términos hasta 4.3e9; techo 7.2e16 ≈ 1.6M frames).
// ---------------------------------------------------------------------------
fn fx_add_q24(slot: u32, v: f32) {
    let vi = floor(v);
    var ii = u32(vi);                              // parte entera (<= 65535)
    var ff = u32(round((v - vi) * 16777216.0));    // fracción en 2^-24
    if (ff >= 16777216u) { ii = ii + 1u; ff = 0u; }
    let add_lo = ((ii & 0xFFu) << 24u) | ff;
    let add_hi = ii >> 8u;
    let old = acc[slot];
    let nlo = old.x + add_lo;
    let carry = select(0u, 1u, nlo < old.x);
    acc[slot] = vec2<u32>(nlo, old.y + add_hi + carry);
}

// PR-1.3: Q56.8 (antes Q48.16). Con Q48.16 el techo era 2^48 ≈ 2.8e14 en
// unidades de m2: con términos de hasta val²·cw ≈ 4.3e9 por frame, la suma
// DESBORDABA a ~65k frames — justo los SER de alta velocidad. Q56.8 sube el
// techo a ~7.2e16 (>1.6M frames de headroom); la resolución de 1/256 ADU²
// es ruido despreciable frente a magnitudes de m2 ≥ 1e6.
fn fx_add_q8(slot: u32, v: f32) {
    var hi = floor(v / 16777216.0);
    let rem = v - hi * 16777216.0;
    let lo_f = rem * 256.0;
    var lo: u32;
    if (lo_f >= 4294967040.0) { hi = hi + 1.0; lo = 0u; }
    else { lo = u32(round(lo_f)); }
    let old = acc[slot];
    let nlo = old.x + lo;
    let carry = select(0u, 1u, nlo < old.x);
    acc[slot] = vec2<u32>(nlo, old.y + u32(hi) + carry);
}

// Muestreo Lanczos-3 6x6 con clamp anti-ringing simétrico — réplica del
// sample_pixel de CPU (versión con bounds check = slow path; en el interior
// produce exactamente los mismos términos que el fast path SIMD).
// Devuelve vec4(val_rgb, marcador>0) — w<=0 significa sin cobertura.
fn sample_lanczos(sx: f32, sy: f32, is_color: bool) -> vec4<f32> {
    let sxf = floor(sx);
    let syf = floor(sy);
    let fx = sx - sxf;
    let fy = sy - syf;
    let x0 = i32(sxf);
    let y0 = i32(syf);
    var w_xs: array<f32, 6>;
    for (var i = 0; i < 6; i = i + 1) {
        w_xs[i] = lut_get(fx - f32(i - 2), P.drop_size);
    }
    var sum = vec3<f32>(0.0, 0.0, 0.0);
    var sw = 0.0;
    var mn = vec3<f32>(65535.0, 65535.0, 65535.0);
    var mx = vec3<f32>(0.0, 0.0, 0.0);
    for (var ky = -2; ky <= 3; ky = ky + 1) {
        let py = y0 + ky;
        if (py < 0 || py >= i32(P.h_in)) { continue; }
        let wy = lut_get(fy - f32(ky), P.drop_size);
        if (abs(wy) < 0.001) { continue; }
        let row = u32(py) * P.w_in;
        for (var kx = -2; kx <= 3; kx = kx + 1) {
            let px = x0 + kx;
            if (px < 0 || px >= i32(P.w_in)) { continue; }
            let wf = w_xs[kx + 2] * wy;
            let base = row + u32(px);
            var v: vec3<f32>;
            if (is_color) {
                let o = base * 3u;
                v = vec3<f32>(frame_val(o), frame_val(o + 1u), frame_val(o + 2u));
            } else {
                let m = frame_val(base);
                v = vec3<f32>(m, m, m);
            }
            mn = min(mn, v);
            mx = max(mx, v);
            sum = sum + v * wf;
            sw = sw + wf;
        }
    }
    if (abs(sw) <= 0.00001) { return vec4<f32>(0.0, 0.0, 0.0, 0.0); }
    let band = (mx - mn) * 0.18 + vec3<f32>(32.0, 32.0, 32.0);
    let lov = max(mn - band, vec3<f32>(0.0, 0.0, 0.0));
    let hiv = min(mx + band, vec3<f32>(65535.0, 65535.0, 65535.0));
    let val = clamp(sum / sw, lov, hiv);
    return vec4<f32>(val, 1.0);
}

fn drop_overlap(d: f32) -> f32 {
    return clamp((P.drz_h1 + P.drz_h2) - abs(d), 0.0, 2.0 * min(P.drz_h1, P.drz_h2));
}

// Kernel drop del drizzle verdadero — réplica de drizzle_sample_mono/rgb.
// Devuelve vec4(val_rgb, dw) con dw = suma de solapes (para la cobertura).
fn sample_drop(sx: f32, sy: f32, is_color: bool) -> vec4<f32> {
    let x0 = i32(round_rs(sx));
    let y0 = i32(round_rs(sy));
    var sum = vec3<f32>(0.0, 0.0, 0.0);
    var sw = 0.0;
    for (var ky = -1; ky <= 1; ky = ky + 1) {
        let py = y0 + ky;
        if (py < 0 || py >= i32(P.h_in)) { continue; }
        let wy = drop_overlap(f32(py) - sy);
        if (wy <= 0.0) { continue; }
        let row = u32(py) * P.w_in;
        for (var kx = -1; kx <= 1; kx = kx + 1) {
            let px = x0 + kx;
            if (px < 0 || px >= i32(P.w_in)) { continue; }
            let wx = drop_overlap(f32(px) - sx);
            if (wx <= 0.0) { continue; }
            let wf = wx * wy;
            let base = row + u32(px);
            var v: vec3<f32>;
            if (is_color) {
                let o = base * 3u;
                v = vec3<f32>(frame_val(o), frame_val(o + 1u), frame_val(o + 2u));
            } else {
                let m = frame_val(base);
                v = vec3<f32>(m, m, m);
            }
            sum = sum + v * wf;
            sw = sw + wf;
        }
    }
    if (sw <= 0.000001) { return vec4<f32>(0.0, 0.0, 0.0, 0.0); }
    return vec4<f32>(sum / sw, sw);
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = gid.x;
    let y = P.band_y0 + gid.y;
    // band_y1 (exclusivo) evita que los hilos sobrantes del último workgroup
    // de una banda pisen las filas de la banda siguiente (doble acumulación).
    if (x >= P.w_out || y >= P.band_y1 || y >= P.h_out) { return; }
    // Bordes excluidos — paridad con StripedAccum (filas/cols 0 y última).
    if (x == 0u || y == 0u || x == P.w_out - 1u || y == P.h_out - 1u) { return; }
    let pix = y * P.w_out + x;

    let ref_x = f32(x) * P.inv_drizzle + P.roi_off_x;
    let ref_y = f32(y) * P.inv_drizzle + P.roi_off_y;
    var sx = ref_x + P.render_dx;
    var sy = ref_y + P.render_dy;
    var pw = 1.0;

    if ((P.flags & 1u) != 0u) {
        let base = pix * P.k;
        var dxs = 0.0;
        var dys = 0.0;
        var tw = 0.0;
        for (var kk = 0u; kk < P.k; kk = kk + 1u) {
            let ap = warp_index(base + kk);
            if (ap >= P.n_aps) { continue; }
            let a = apq[ap];
            if (a.z <= 0.0) { continue; }   // sin medición / AP rechazado
            let fw = warp_w[base + kk] * a.z;
            dxs = dxs + a.x * fw;
            dys = dys + a.y * fw;
            tw = tw + fw;
        }
        if (tw > 0.001) {
            sx = sx + dxs / tw;
            sy = sy + dys / tw;
            pw = tw;
        } else if (P.gfw > 0.001) {
            sx = ref_x + P.render_dx;
            sy = ref_y + P.render_dy;
            pw = P.gfw;
        } else {
            return;
        }
    }
    if (pw < 0.001) { return; }

    let is_color = (P.flags & 4u) != 0u;
    var val: vec3<f32>;
    var plane_w = pw;   // lo que la CPU escribe en el plano ww
    if ((P.flags & 2u) != 0u) {
        let s = sample_drop(sx, sy, is_color);
        if (s.w <= 0.000001) { return; }
        val = s.rgb;
        plane_w = pw * min(s.w / P.drz_full_cov, 1.0);
    } else {
        let s = sample_lanczos(sx, sy, is_color);
        if (s.w <= 0.0) { return; }
        val = s.rgb;
    }
    if (plane_w < 0.000000001) { return; }   // gate 1e-9 de StripedAccum

    // Sigma-rejection (pasada 2): cualquier canal fuera → píxel entero fuera.
    if ((P.flags & 16u) != 0u) {
        let n = P.n_px_out;
        if (is_color) {
            if (val.r < bounds[pix] || val.r > bounds[n + pix]) { return; }
            if (val.g < bounds[2u * n + pix] || val.g > bounds[3u * n + pix]) { return; }
            if (val.b < bounds[4u * n + pix] || val.b > bounds[5u * n + pix]) { return; }
        } else {
            if (val.g < bounds[pix] || val.g > bounds[n + pix]) { return; }
        }
    }

    // q del dense quality map — misma fórmula de redondeo/clamp que CPU.
    var q = 1.0;
    if (P.dq_w > 0u) {
        let qxf = round_rs(f32(x) * P.q_scale_x + P.q_off_x);
        let qyf = round_rs(f32(y) * P.q_scale_y + P.q_off_y);
        let qx = min(u32(max(qxf, 0.0)), P.dq_w - 1u);
        let qy = min(u32(max(qyf, 0.0)), P.dq_h - 1u);
        q = dq[qy * P.dq_w + qx];
    }
    var cw = q * q * P.gw;
    if ((P.flags & 32u) != 0u) { cw = cw * min(plane_w, 1.0); }
    if (cw < 0.0000001) { return; }   // gate 1e-7 de StripedAccum

    let n = P.n_px_out;
    if (is_color) {
        fx_add_q24(pix, val.r * cw);
        fx_add_q24(n + pix, val.g * cw);
        fx_add_q24(2u * n + pix, val.b * cw);
        fx_add_q24(3u * n + pix, cw);
        if ((P.flags & 8u) != 0u) {
            fx_add_q8(4u * n + pix, val.r * val.r * cw);
            fx_add_q8(5u * n + pix, val.g * val.g * cw);
            fx_add_q8(6u * n + pix, val.b * val.b * cw);
        }
    } else {
        fx_add_q24(pix, val.g * cw);
        fx_add_q24(n + pix, cw);
        if ((P.flags & 8u) != 0u) {
            fx_add_q8(2u * n + pix, val.g * val.g * cw);
        }
    }
}
"#;

fn init_runtime() -> Option<GpuRuntime> {
    if std::env::var("ZAS_FORCE_CPU").map(|v| v == "1").unwrap_or(false) {
        return None;
    }
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        // PRIMARY = Metal/DX12/Vulkan. GL queda fuera a propósito: sus límites
        // de storage buffer son demasiado chicos para los acumuladores y la
        // decisión caería siempre a CPU — mejor no crear el contexto siquiera.
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))?;
    let info = adapter.get_info();
    let alim = adapter.limits();

    // Pedimos los límites REALES del adapter para poder crear buffers grandes
    // (los defaults de wgpu capan storage bindings a 128 MB). El presupuesto
    // final igual se decide por pase en plan_pass.
    let mut req_limits = wgpu::Limits::default();
    req_limits.max_buffer_size = alim.max_buffer_size;
    req_limits.max_storage_buffer_binding_size = alim.max_storage_buffer_binding_size;
    req_limits.max_storage_buffers_per_shader_stage =
        alim.max_storage_buffers_per_shader_stage.min(12).max(8);

    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("zas-gpu-stack"),
            required_features: wgpu::Features::empty(),
            required_limits: req_limits,
            memory_hints: wgpu::MemoryHints::Performance,
        },
        None,
    ))
    .ok()?;

    let max_binding = alim.max_storage_buffer_binding_size as u64;
    let vram_budget =
        ((alim.max_buffer_size.min(max_binding) as f64) * 0.6) as u64;

    // Errores asincronos del device (device lost, OOM tardio): marcan el flag
    // que el submitter consulta tras cada frame → fallback del pase a CPU.
    device.on_uncaptured_error(Box::new(|e| {
        GPU_ERROR_FLAG.store(true, Ordering::Relaxed);
        eprintln!("[gpu_stack] error wgpu no capturado: {e}");
    }));

    // Pérdida REAL del device (watchdog de Metal/TDR de Windows): cerrojo de
    // sesión — sin él, cada lote posterior pagaría el deadline completo del
    // readback contra un dispositivo muerto.
    device.set_device_lost_callback(Box::new(|reason, message| {
        GPU_LOST.store(true, Ordering::Relaxed);
        GPU_ERROR_FLAG.store(true, Ordering::Relaxed);
        eprintln!(
            "[gpu_stack] device PERDIDO ({reason:?}): {message} — el resto de la sesión usa CPU"
        );
    }));

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("zas-accum-shader"),
        source: wgpu::ShaderSource::Wgsl(ACCUM_WGSL.into()),
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
    let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("zas-accum-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    // offset dinamico: un unico buffer de params con una copia
                    // por banda → un solo submit por frame.
                    has_dynamic_offset: true,
                    min_binding_size: None,
                },
                count: None,
            },
            storage_ro(1),
            storage_ro(2),
            storage_ro(3),
            storage_ro(4),
            storage_ro(5),
            storage_ro(6),
            storage_ro(7),
            wgpu::BindGroupLayoutEntry {
                binding: 8,
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
    let pipe_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("zas-accum-pipe-layout"),
        bind_group_layouts: &[&bind_layout],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("zas-accum-pipeline"),
        layout: Some(&pipe_layout),
        module: &shader,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });

    // LUT Lanczos: MISMOS valores que liquid_warping::LanczosLUT::new(30000, 3.0)
    // — la paridad del muestreo GPU-vs-CPU depende de compartir esta tabla.
    let lut_values = build_lanczos_lut();
    let lut_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("zas-lanczos-lut"),
        size: (lut_values.len() * 4) as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&lut_buf, 0, bytemuck::cast_slice(&lut_values));

    Some(GpuRuntime {
        device,
        queue,
        adapter_name: info.name,
        backend: backend_label(info.backend).to_string(),
        vram_budget,
        max_binding,
        pipeline,
        bind_layout,
        lut_buf,
    })
}

/// Réplica EXACTA de LanczosLUT::new(30000, 3.0) de liquid_warping.rs —
/// mantener en sincronía si aquella cambia (la paridad depende de ello).
fn build_lanczos_lut() -> Vec<f32> {
    let resolution = 30000usize;
    let max_val = 3.0f32;
    let scale = (resolution as f32 - 1.0) / max_val;
    (0..resolution)
        .map(|i| {
            let x = (i as f32) / scale;
            let ax = x.abs();
            if ax < 0.0001 {
                1.0
            } else if ax >= 3.0 {
                0.0
            } else {
                let pin_x = ax * std::f32::consts::PI;
                let sinc_x = pin_x.sin() / pin_x;
                let pin_x_3 = pin_x / 3.0;
                let sinc_x_3 = pin_x_3.sin() / pin_x_3;
                sinc_x * sinc_x_3
            }
        })
        .collect()
}

pub const LUT_SCALE: f32 = (30000.0 - 1.0) / 3.0;

/// Info de detección para la UI (comando `get_gpu_info`) y el reporte.
#[derive(Clone, serde::Serialize)]
pub struct GpuInfo {
    pub available: bool,
    pub name: String,
    pub backend: String,
    pub vram_budget_mb: u64,
    /// "pending" | "ok" | "failed" — estado del self-test de paridad numérica.
    pub parity: String,
}

pub fn parity_label() -> &'static str {
    match PARITY_STATE.load(Ordering::Relaxed) {
        1 => "ok",
        2 => "failed",
        _ => "pending",
    }
}

pub fn set_parity_state(ok: bool) {
    PARITY_STATE.store(if ok { 1 } else { 2 }, Ordering::Relaxed);
}

/// GPU utilizable para apilar = runtime presente Y paridad no-fallida.
pub fn gpu_usable() -> bool {
    gpu_runtime().is_some() && PARITY_STATE.load(Ordering::Relaxed) != 2
}

pub fn gpu_info() -> GpuInfo {
    match gpu_runtime() {
        Some(rt) => GpuInfo {
            available: true,
            name: rt.adapter_name.clone(),
            backend: rt.backend.clone(),
            vram_budget_mb: rt.vram_budget / (1024 * 1024),
            parity: parity_label().to_string(),
        },
        None => GpuInfo {
            available: false,
            name: String::new(),
            backend: String::new(),
            vram_budget_mb: 0,
            parity: "pending".to_string(),
        },
    }
}

/// Sufijo para la etiqueta de aceleración del overlay:
/// "NEON · macOS · GPU Metal (Apple M2)".
pub fn accel_suffix() -> String {
    match gpu_runtime() {
        Some(rt) => format!(" · GPU {} ({})", rt.backend, rt.adapter_name),
        None => String::new(),
    }
}

// ===========================================================================
// ACUMULADOR DE PASE
// ===========================================================================

/// Espejo binario del struct Params del WGSL (mismo orden, 112 bytes).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuParams {
    w_in: u32,
    h_in: u32,
    w_out: u32,
    h_out: u32,
    band_y0: u32,
    k: u32,
    n_px_out: u32,
    flags: u32,
    inv_drizzle: f32,
    roi_off_x: f32,
    roi_off_y: f32,
    drop_size: f32,
    render_dx: f32,
    render_dy: f32,
    gfw: f32,
    gw: f32,
    drz_h1: f32,
    drz_h2: f32,
    drz_full_cov: f32,
    lut_scale: f32,
    q_scale_x: f32,
    q_scale_y: f32,
    q_off_x: f32,
    q_off_y: f32,
    dq_w: u32,
    dq_h: u32,
    n_aps: u32,
    band_y1: u32,
}

/// Alineación del offset dinámico de uniforms (256 es el mínimo universal).
const PARAM_STRIDE: u64 = 256;
/// Píxeles por dispatch (~2M): mantiene cada dispatch < 50 ms incluso en
/// iGPU → sin TDR (timeout del driver) en Windows.
const PX_PER_DISPATCH: usize = 2_000_000;
/// Chunk de descarga (staging map): acota la memoria de readback.
const DOWNLOAD_CHUNK: u64 = 128 * 1024 * 1024;

/// Configuración inmutable de un pase de acumulación GPU.
#[derive(Clone)]
pub struct GpuPassConfig {
    pub w_in: usize,
    pub h_in: usize,
    pub w_out: usize,
    pub h_out: usize,
    pub is_color: bool,
    pub track_m2: bool,
    pub use_bounds: bool,
    pub coverage_weighting: bool,
    pub true_drizzle: bool,
    pub use_warp: bool,
    pub k: usize,
    pub n_aps: usize,
    pub drizzle: f32,
    pub roi_off_x: f32,
    pub roi_off_y: f32,
    pub drop_size: f32,
    pub global_fallback_weight: f32,
    pub dq_w: usize,
    pub dq_h: usize,
}

impl GpuPassConfig {
    pub fn planes(&self) -> usize {
        if self.is_color {
            4 + if self.track_m2 { 3 } else { 0 }
        } else {
            2 + if self.track_m2 { 1 } else { 0 }
        }
    }
    /// VRAM total del pase (acumuladores + warp map + frame + bounds).
    pub fn vram_needed(&self) -> u64 {
        let n_px = (self.w_out * self.h_out) as u64;
        let acc = n_px * 8 * self.planes() as u64;
        let warp = if self.use_warp {
            n_px * self.k as u64 * 6
        } else {
            0
        };
        let frame = (self.w_in * self.h_in * if self.is_color { 3 } else { 1 } * 2) as u64;
        let bounds = if self.use_bounds {
            n_px * 4 * if self.is_color { 6 } else { 2 }
        } else {
            0
        };
        acc + warp + frame + bounds + (self.dq_w * self.dq_h * 4) as u64
    }
    /// El binding más grande (los acumuladores) debe caber en el límite.
    pub fn largest_binding(&self) -> u64 {
        (self.w_out * self.h_out) as u64 * 8 * self.planes() as u64
    }
}

/// Trabajo por frame que el worker CPU entrega al submitter GPU.
pub struct GpuFrameJob {
    /// mono w_in*h_in o RGB interleaved w_in*h_in*3 (u16).
    pub pixels: Vec<u16>,
    /// Por AP: (dx, dy, q_eff) con q_eff = ap_w·clamp(q,0.05,1) pre-combinado
    /// en CPU (0 = AP sin medición o rechazado para este frame).
    pub apq: Vec<[f32; 4]>,
    pub dq_map: Vec<f32>,
    pub gw: f32,
    pub render_dx: f32,
    pub render_dy: f32,
    pub q_off_x: f32,
    pub q_off_y: f32,
}

/// Planos f64 descargados al final del pase — el caller los vuelca en los
/// GradientDomainStacker y TODO lo downstream queda idéntico a la ruta CPU.
pub struct GpuDownload {
    pub direct_r: Vec<f64>,
    pub direct_g: Vec<f64>,
    pub direct_b: Vec<f64>,
    pub direct_w: Vec<f64>,
    pub m2_r: Vec<f64>,
    pub m2_g: Vec<f64>,
    pub m2_b: Vec<f64>,
}

pub struct GpuPassAccumulator {
    rt: &'static GpuRuntime,
    cfg: GpuPassConfig,
    bands: Vec<u32>, // band_y0 de cada dispatch
    band_rows: u32,
    params_buf: wgpu::Buffer,
    frame_buf: wgpu::Buffer,
    warp_idx_buf: wgpu::Buffer,
    warp_w_buf: wgpu::Buffer,
    apq_buf: wgpu::Buffer,
    dq_buf: wgpu::Buffer,
    bounds_buf: wgpu::Buffer,
    acc_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    frame_scratch: Vec<u8>,
    params_scratch: Vec<u8>,
    /// Bytes de VRAM asignados (telemetría).
    pub vram_bytes: u64,
}

impl GpuPassAccumulator {
    pub fn new(
        rt: &'static GpuRuntime,
        cfg: GpuPassConfig,
        warp_indices: &[u16],
        warp_weights: &[f32],
    ) -> Result<Self, String> {
        let n_px = cfg.w_out * cfg.h_out;
        if cfg.largest_binding() > rt.max_binding {
            return Err(format!(
                "acumuladores ({} MB) exceden el binding máximo del adapter ({} MB)",
                cfg.largest_binding() / (1024 * 1024),
                rt.max_binding / (1024 * 1024)
            ));
        }
        if cfg.vram_needed() > rt.vram_budget {
            return Err(format!(
                "el pase necesita {} MB de VRAM y el presupuesto es {} MB",
                cfg.vram_needed() / (1024 * 1024),
                rt.vram_budget / (1024 * 1024)
            ));
        }
        if cfg.use_warp && cfg.n_aps > u16::MAX as usize {
            return Err("más de 65535 APs (índices u16)".into());
        }

        let dev = &rt.device;
        let mk_storage = |label: &str, size: u64, dst: bool| {
            dev.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size.max(16),
                usage: if dst {
                    wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST
                } else {
                    wgpu::BufferUsages::STORAGE
                        | wgpu::BufferUsages::COPY_DST
                        | wgpu::BufferUsages::COPY_SRC
                },
                mapped_at_creation: false,
            })
        };

        // Bandas de dispatch (filas múltiplo de 8 = workgroup_size.y).
        let band_rows = ((PX_PER_DISPATCH / cfg.w_out.max(1)).clamp(64, cfg.h_out.max(64))
            as u32
            + 7)
            & !7;
        let mut bands = Vec::new();
        let mut y0 = 0u32;
        while (y0 as usize) < cfg.h_out {
            bands.push(y0);
            y0 += band_rows;
        }

        let params_buf = dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zas-params"),
            size: PARAM_STRIDE * bands.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let frame_px = cfg.w_in * cfg.h_in * if cfg.is_color { 3 } else { 1 };
        let frame_buf = mk_storage("zas-frame", ((frame_px + 1) / 2 * 4) as u64, true);

        // Warp map: subido UNA vez por pase (u16 empaquetados de a pares).
        let (warp_idx_buf, warp_w_buf) = if cfg.use_warp {
            let mut packed = vec![0u32; (warp_indices.len() + 1) / 2];
            for (i, &v) in warp_indices.iter().enumerate() {
                packed[i / 2] |= (v as u32) << ((i % 2) * 16);
            }
            let ib = mk_storage("zas-warp-idx", (packed.len() * 4) as u64, true);
            rt.queue.write_buffer(&ib, 0, bytemuck::cast_slice(&packed));
            let wb = mk_storage("zas-warp-w", (warp_weights.len() * 4) as u64, true);
            rt.queue.write_buffer(&wb, 0, bytemuck::cast_slice(warp_weights));
            (ib, wb)
        } else {
            (
                mk_storage("zas-warp-idx", 16, true),
                mk_storage("zas-warp-w", 16, true),
            )
        };

        let apq_buf = mk_storage("zas-apq", (cfg.n_aps.max(1) * 16) as u64, true);
        let dq_buf = mk_storage("zas-dq", (cfg.dq_w * cfg.dq_h * 4) as u64, true);
        let bounds_buf = mk_storage(
            "zas-bounds",
            if cfg.use_bounds {
                (n_px * 4 * if cfg.is_color { 6 } else { 2 }) as u64
            } else {
                16
            },
            true,
        );
        // wgpu garantiza buffers inicializados a cero → acumulador limpio.
        let acc_buf = mk_storage("zas-acc", (n_px * 8 * cfg.planes()) as u64, false);

        let bind_group = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("zas-accum-bind"),
            layout: &rt.bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &params_buf,
                        offset: 0,
                        size: Some(std::num::NonZeroU64::new(112).unwrap()),
                    }),
                },
                wgpu::BindGroupEntry { binding: 1, resource: frame_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: warp_idx_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: warp_w_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: apq_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 5, resource: rt.lut_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 6, resource: dq_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 7, resource: bounds_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 8, resource: acc_buf.as_entire_binding() },
            ],
        });

        let vram_bytes = cfg.vram_needed();
        Ok(Self {
            rt,
            cfg,
            bands,
            band_rows,
            params_buf,
            frame_buf,
            warp_idx_buf,
            warp_w_buf,
            apq_buf,
            dq_buf,
            bounds_buf,
            acc_buf,
            bind_group,
            frame_scratch: Vec::new(),
            params_scratch: Vec::new(),
            vram_bytes,
        })
    }

    /// Sube los límites de winsorización de la pasada 2 (layout por planos:
    /// [lo_r][hi_r][lo_g][hi_g][lo_b][hi_b]; mono usa solo los dos primeros).
    pub fn set_sigma_bounds(&self, channels: &[(&[f32], &[f32])]) -> Result<(), String> {
        let n_px = self.cfg.w_out * self.cfg.h_out;
        let mut off = 0u64;
        for (lo, hi) in channels {
            if lo.len() != n_px || hi.len() != n_px {
                return Err("bounds con tamaño inesperado".into());
            }
            self.rt.queue.write_buffer(&self.bounds_buf, off, bytemuck::cast_slice(lo));
            off += (n_px * 4) as u64;
            self.rt.queue.write_buffer(&self.bounds_buf, off, bytemuck::cast_slice(hi));
            off += (n_px * 4) as u64;
        }
        Ok(())
    }

    fn build_params(&self, job: &GpuFrameJob, band_y0: u32) -> GpuParams {
        let c = &self.cfg;
        let mut flags = 0u32;
        if c.use_warp {
            flags |= 1;
        }
        if c.true_drizzle {
            flags |= 2;
        }
        if c.is_color {
            flags |= 4;
        }
        if c.track_m2 {
            flags |= 8;
        }
        if c.use_bounds {
            flags |= 16;
        }
        if c.coverage_weighting {
            flags |= 32;
        }
        let drz_h1 = 0.5 / c.drizzle;
        let drz_h2 = 0.5 * c.drop_size.clamp(0.3, 1.0);
        let m = 2.0 * drz_h1.min(drz_h2);
        GpuParams {
            w_in: c.w_in as u32,
            h_in: c.h_in as u32,
            w_out: c.w_out as u32,
            h_out: c.h_out as u32,
            band_y0,
            k: c.k as u32,
            n_px_out: (c.w_out * c.h_out) as u32,
            flags,
            inv_drizzle: 1.0 / c.drizzle,
            roi_off_x: c.roi_off_x,
            roi_off_y: c.roi_off_y,
            drop_size: c.drop_size,
            render_dx: job.render_dx,
            render_dy: job.render_dy,
            gfw: c.global_fallback_weight,
            gw: job.gw,
            drz_h1,
            drz_h2,
            drz_full_cov: (m * m).max(1e-9),
            lut_scale: LUT_SCALE,
            q_scale_x: if c.dq_w > 0 { c.dq_w as f32 / c.w_out as f32 } else { 0.0 },
            q_scale_y: if c.dq_h > 0 { c.dq_h as f32 / c.h_out as f32 } else { 0.0 },
            q_off_x: job.q_off_x,
            q_off_y: job.q_off_y,
            dq_w: c.dq_w as u32,
            dq_h: c.dq_h as u32,
            n_aps: c.n_aps as u32,
            band_y1: (band_y0 + self.band_rows).min(c.h_out as u32),
        }
    }

    /// Sube el frame + sus metadatos y despacha la acumulación por bandas.
    /// Un solo submit por frame (params por banda vía offsets dinámicos).
    pub fn accumulate_frame(&mut self, job: &GpuFrameJob) -> Result<(), String> {
        let expected = self.cfg.w_in * self.cfg.h_in * if self.cfg.is_color { 3 } else { 1 };
        if job.pixels.len() != expected {
            return Err("frame con tamaño inesperado".into());
        }
        // u16 → pares u32 (con padding a longitud par).
        self.frame_scratch.clear();
        self.frame_scratch
            .extend_from_slice(bytemuck::cast_slice(&job.pixels));
        if self.frame_scratch.len() % 4 != 0 {
            self.frame_scratch.extend_from_slice(&[0, 0]);
        }
        self.rt
            .queue
            .write_buffer(&self.frame_buf, 0, &self.frame_scratch);

        if self.cfg.use_warp {
            let mut apq = job.apq.clone();
            apq.resize(self.cfg.n_aps.max(1), [0.0; 4]);
            self.rt
                .queue
                .write_buffer(&self.apq_buf, 0, bytemuck::cast_slice(&apq));
        }
        if self.cfg.dq_w > 0 && !job.dq_map.is_empty() {
            self.rt
                .queue
                .write_buffer(&self.dq_buf, 0, bytemuck::cast_slice(&job.dq_map));
        }

        // Params de todas las bandas en un write (stride 256).
        self.params_scratch
            .resize(PARAM_STRIDE as usize * self.bands.len(), 0);
        for (bi, &y0) in self.bands.iter().enumerate() {
            let p = self.build_params(job, y0);
            let dst = &mut self.params_scratch
                [bi * PARAM_STRIDE as usize..bi * PARAM_STRIDE as usize + 112];
            dst.copy_from_slice(bytemuck::bytes_of(&p));
        }
        self.rt
            .queue
            .write_buffer(&self.params_buf, 0, &self.params_scratch);

        let mut enc = self
            .rt
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("zas-accum-enc"),
            });
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("zas-accum-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.rt.pipeline);
            let gx = (self.cfg.w_out as u32 + 7) / 8;
            for (bi, &y0) in self.bands.iter().enumerate() {
                let rows = (self.cfg.h_out as u32 - y0).min(self.band_rows);
                let gy = (rows + 7) / 8;
                pass.set_bind_group(0, &self.bind_group, &[bi as u32 * PARAM_STRIDE as u32]);
                pass.dispatch_workgroups(gx, gy, 1);
            }
        }
        self.rt.queue.submit(Some(enc.finish()));

        if take_gpu_error() {
            return Err("error del device GPU durante la acumulación".into());
        }
        Ok(())
    }

    /// Descarga los acumuladores y los convierte de punto fijo a f64.
    pub fn finish(self) -> Result<GpuDownload, String> {
        let n_px = self.cfg.w_out * self.cfg.h_out;
        let planes = self.cfg.planes();
        let total_bytes = (n_px * 8 * planes) as u64;

        let staging = self.rt.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("zas-staging"),
            size: DOWNLOAD_CHUNK.min(total_bytes.max(16)),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut raw = Vec::<u64>::with_capacity(n_px * planes);
        let mut off = 0u64;
        while off < total_bytes {
            let len = DOWNLOAD_CHUNK.min(total_bytes - off);
            let mut enc = self
                .rt
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("zas-download-enc"),
                });
            enc.copy_buffer_to_buffer(&self.acc_buf, off, &staging, 0, len);
            self.rt.queue.submit(Some(enc.finish()));

            let slice = staging.slice(0..len);
            let (tx, rx) = std::sync::mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
            wait_for_readback(&self.rt.device, &rx, "descarga del pase de apilado")?;
            {
                let data = slice.get_mapped_range();
                let pairs: &[u32] = bytemuck::cast_slice(&data);
                for ch in pairs.chunks_exact(2) {
                    raw.push((ch[1] as u64) << 32 | ch[0] as u64);
                }
            }
            staging.unmap();
            off += len;
        }
        if take_gpu_error() {
            return Err("error del device GPU durante la descarga".into());
        }

        // Punto fijo → f64: sums en Q40.24, m2 en Q56.8 (PR-1.3: Q48.16 desbordaba a ~65k frames).
        let to_f64_q24 = |v: u64| v as f64 / 16_777_216.0;
        let to_f64_q8 = |v: u64| v as f64 / 256.0;
        let plane = |i: usize| &raw[i * n_px..(i + 1) * n_px];

        let mut out = GpuDownload {
            direct_r: Vec::new(),
            direct_g: Vec::new(),
            direct_b: Vec::new(),
            direct_w: Vec::new(),
            m2_r: Vec::new(),
            m2_g: Vec::new(),
            m2_b: Vec::new(),
        };
        if self.cfg.is_color {
            out.direct_r = plane(0).iter().map(|&v| to_f64_q24(v)).collect();
            out.direct_g = plane(1).iter().map(|&v| to_f64_q24(v)).collect();
            out.direct_b = plane(2).iter().map(|&v| to_f64_q24(v)).collect();
            out.direct_w = plane(3).iter().map(|&v| to_f64_q24(v)).collect();
            if self.cfg.track_m2 {
                out.m2_r = plane(4).iter().map(|&v| to_f64_q8(v)).collect();
                out.m2_g = plane(5).iter().map(|&v| to_f64_q8(v)).collect();
                out.m2_b = plane(6).iter().map(|&v| to_f64_q8(v)).collect();
            }
        } else {
            out.direct_g = plane(0).iter().map(|&v| to_f64_q24(v)).collect();
            out.direct_w = plane(1).iter().map(|&v| to_f64_q24(v)).collect();
            if self.cfg.track_m2 {
                out.m2_g = plane(2).iter().map(|&v| to_f64_q8(v)).collect();
            }
        }
        Ok(out)
    }
}

// ===========================================================================
// SELF-TEST DE PARIDAD GPU-vs-CPU
//
// Corre un mini-stack sintético por AMBAS rutas (la CPU real de producción:
// accumulate_frame_liquid* + normalización + rejection + StripedAccum) y
// compara los planos finales. Si el peor RMSE supera la tolerancia, la GPU
// queda deshabilitada para la sesión — la app nunca apila con una GPU que no
// haya DEMOSTRADO producir el mismo resultado que la referencia.
// ===========================================================================

const PARITY_RMSE_TOLERANCE: f64 = 1.0; // ADU16

/// Corre todos los escenarios de paridad; devuelve el peor RMSE (ADU16).
pub fn run_parity_check() -> Result<f64, String> {
    let rt = gpu_runtime().ok_or("sin runtime GPU")?;
    let mut worst = 0.0f64;
    // A: mono, Lanczos (drizzle 1x), warp 4 APs, dq map activo.
    worst = worst.max(parity_scenario(rt, false, 1.0, false, false)?);
    // B: color, drizzle 2x (kernel drop + cobertura), con m2.
    worst = worst.max(parity_scenario(rt, true, 2.0, true, false)?);
    // C: mono, Lanczos, con sigma-bounds que rechazan una franja.
    worst = worst.max(parity_scenario(rt, false, 1.0, false, true)?);
    Ok(worst)
}

/// Gate cacheado: la primera llamada ejecuta el self-test; después es O(1).
pub fn ensure_parity() -> bool {
    match PARITY_STATE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let ok = match run_parity_check() {
                Ok(rmse) => {
                    eprintln!("[gpu_stack] paridad GPU-vs-CPU: RMSE {rmse:.4} ADU");
                    rmse <= PARITY_RMSE_TOLERANCE
                }
                Err(e) => {
                    eprintln!("[gpu_stack] self-test de paridad falló: {e}");
                    false
                }
            };
            set_parity_state(ok);
            ok
        }
    }
}

#[allow(clippy::too_many_lines)]
fn parity_scenario(
    rt: &'static GpuRuntime,
    is_color: bool,
    drizzle: f32,
    track_m2: bool,
    with_bounds: bool,
) -> Result<f64, String> {
    use crate::smart_grid::ApPoint;

    let (w_in, h_in) = (96usize, 80usize);
    let w_out = (w_in as f32 * drizzle) as usize;
    let h_out = (h_in as f32 * drizzle) as usize;
    let n_out = w_out * h_out;
    let true_drizzle = drizzle > 1.01;
    let drop_size = if true_drizzle { 0.75f32 } else { 1.0f32 };

    // Escena determinista con textura 2-D rica.
    let tau = std::f32::consts::TAU;
    let scene = |x: f32, y: f32| -> f32 {
        9000.0
            + 5000.0 * (x * tau / 7.3).sin() * (y * tau / 9.1).cos()
            + 3000.0 * ((x * 0.6 + y * 0.8) * tau / 13.7).sin()
    };
    let render = |dx: f32, dy: f32| -> Vec<u16> {
        let mut px = Vec::with_capacity(w_in * h_in * if is_color { 3 } else { 1 });
        for y in 0..h_in {
            for x in 0..w_in {
                let v = scene(x as f32 - dx, y as f32 - dy).clamp(0.0, 65535.0);
                if is_color {
                    px.push(v as u16);
                    px.push((v * 0.85) as u16);
                    px.push((v * 0.6) as u16);
                } else {
                    px.push(v as u16);
                }
            }
        }
        px
    };

    let points = vec![
        ApPoint { x: 26.0, y: 22.0, size: 32 },
        ApPoint { x: 70.0, y: 22.0, size: 32 },
        ApPoint { x: 26.0, y: 58.0, size: 32 },
        ApPoint { x: 70.0, y: 58.0, size: 32 },
    ];
    let k = 4usize;
    let (warp_idx, warp_w) = crate::liquid_warping::compute_idw_map_for_output(
        w_out, h_out, drizzle, 0.0, 0.0, &points, 1.55, k,
    );
    let accept = vec![true; points.len()];
    let ap_weights = vec![1.0f32, 0.9, 0.8, 1.0];

    // Dense quality map sintético (1/4 de resolución, valores variados).
    let (dq_w, dq_h) = (w_out / 4, h_out / 4);
    let dq: Vec<f32> = (0..dq_w * dq_h)
        .map(|i| 0.55 + 0.45 * (((i % 7) as f32) / 6.0))
        .collect();

    // Bounds de rejection (escenario C): franja central con techo bajo →
    // rechaza los píxeles brillantes SOLO ahí, igual en ambas rutas.
    let bounds_lo = vec![0.0f32; n_out];
    let mut bounds_hi = vec![65535.0f32; n_out];
    if with_bounds {
        for y in h_out / 3..(2 * h_out) / 3 {
            for x in 0..w_out {
                bounds_hi[y * w_out + x] = 9500.0;
            }
        }
    }

    let frames: [(f32, f32, f32); 3] = [(0.0, 0.0, 1.0), (1.3, -0.7, 0.85), (-0.6, 1.1, 0.6)];
    let shifts_for = |fi: usize| -> Vec<(f32, f32, f32)> {
        points
            .iter()
            .enumerate()
            .map(|(i, _)| {
                (
                    0.35 * ((fi + i) as f32).sin(),
                    0.35 * ((fi * 2 + i) as f32).cos(),
                    match (fi + i) % 4 {
                        0 => 0.9,
                        1 => 0.5,
                        2 => 0.0, // sin medición: el warp debe ignorarlo
                        _ => 1.0,
                    },
                )
            })
            .collect()
    };

    // ------------------------- RUTA CPU (producción) -------------------------
    let mk = |active: bool| {
        if !active {
            crate::GradientDomainStacker::new_empty()
        } else if track_m2 {
            crate::GradientDomainStacker::new_direct_tracked(w_out, h_out)
        } else {
            crate::GradientDomainStacker::new_direct_only(w_out, h_out)
        }
    };
    let acc_r = crate::StripedAccum::new(mk(is_color), 4);
    let acc_g = crate::StripedAccum::new(mk(true), 4);
    let acc_b = crate::StripedAccum::new(mk(is_color), 4);

    for (fi, &(rdx, rdy, gw)) in frames.iter().enumerate() {
        let px = render(rdx, rdy);
        let shifts = shifts_for(fi);
        let q_off_x = rdx * (dq_w as f32 / w_out as f32);
        let q_off_y = rdy * (dq_h as f32 / h_out as f32);
        if is_color {
            let mut wr = vec![0f32; n_out];
            let mut wg = vec![0f32; n_out];
            let mut wb = vec![0f32; n_out];
            let mut ww = vec![0f32; n_out];
            crate::liquid_warping::accumulate_frame_liquid(
                &mut wr, &mut wg, &mut wb, &mut ww, &px, w_in, h_in, w_out, h_out, drizzle,
                0.0, 0.0, rdx, rdy, &shifts, &warp_idx, &warp_w, &accept, &ap_weights,
                &points, 1.0, drop_size,
            );
            for i in 0..n_out {
                if ww[i] > 1e-9 {
                    wr[i] /= ww[i];
                    wg[i] /= ww[i];
                    wb[i] /= ww[i];
                }
            }
            if with_bounds {
                crate::apply_sigma_rejection(&wr, &mut ww, &bounds_lo, &bounds_hi);
                crate::apply_sigma_rejection(&wg, &mut ww, &bounds_lo, &bounds_hi);
                crate::apply_sigma_rejection(&wb, &mut ww, &bounds_lo, &bounds_hi);
            }
            acc_r.accumulate(&wr, &ww, &dq, dq_w, dq_h, q_off_x, q_off_y, gw, fi, true_drizzle);
            acc_g.accumulate(&wg, &ww, &dq, dq_w, dq_h, q_off_x, q_off_y, gw, fi, true_drizzle);
            acc_b.accumulate(&wb, &ww, &dq, dq_w, dq_h, q_off_x, q_off_y, gw, fi, true_drizzle);
        } else {
            let mut wg = vec![0f32; n_out];
            let mut ww = vec![0f32; n_out];
            crate::liquid_warping::accumulate_frame_liquid_mono(
                &mut wg, &mut ww, &px, w_in, h_in, w_out, h_out, drizzle, 0.0, 0.0, rdx, rdy,
                &shifts, &warp_idx, &warp_w, &accept, &ap_weights, 1.0, drop_size,
            );
            for i in 0..n_out {
                if ww[i] > 1e-9 {
                    wg[i] /= ww[i];
                }
            }
            if with_bounds {
                crate::apply_sigma_rejection(&wg, &mut ww, &bounds_lo, &bounds_hi);
            }
            acc_g.accumulate(&wg, &ww, &dq, dq_w, dq_h, q_off_x, q_off_y, gw, fi, true_drizzle);
        }
    }
    let cpu_g = acc_g.into_inner();
    let cpu_r = acc_r.into_inner();
    let cpu_b = acc_b.into_inner();

    // ------------------------------ RUTA GPU ------------------------------
    let cfg = GpuPassConfig {
        w_in,
        h_in,
        w_out,
        h_out,
        is_color,
        track_m2,
        use_bounds: with_bounds,
        coverage_weighting: true_drizzle,
        true_drizzle,
        use_warp: true,
        k,
        n_aps: points.len(),
        drizzle,
        roi_off_x: 0.0,
        roi_off_y: 0.0,
        drop_size,
        global_fallback_weight: 1.0,
        dq_w,
        dq_h,
    };
    let mut gpu = GpuPassAccumulator::new(rt, cfg, &warp_idx, &warp_w)?;
    if with_bounds {
        gpu.set_sigma_bounds(&[(&bounds_lo, &bounds_hi)])?;
    }
    for (fi, &(rdx, rdy, gw)) in frames.iter().enumerate() {
        let px = render(rdx, rdy);
        let shifts = shifts_for(fi);
        let apq: Vec<[f32; 4]> = shifts
            .iter()
            .enumerate()
            .map(|(i, &(dx, dy, q))| {
                if q <= 0.0 {
                    [0.0; 4]
                } else {
                    [dx, dy, ap_weights[i] * q.clamp(0.05, 1.0), 0.0]
                }
            })
            .collect();
        gpu.accumulate_frame(&GpuFrameJob {
            pixels: px,
            apq,
            dq_map: dq.clone(),
            gw,
            render_dx: rdx,
            render_dy: rdy,
            q_off_x: rdx * (dq_w as f32 / w_out as f32),
            q_off_y: rdy * (dq_h as f32 / h_out as f32),
        })?;
        let _ = fi;
    }
    let down = gpu.finish()?;

    // ----------------------------- COMPARACIÓN -----------------------------
    let rmse_plane = |cd: &[f64], cw: &[f64], gd: &[f64], gw_: &[f64]| -> (f64, usize) {
        let mut se = 0.0f64;
        let mut n = 0f64;
        let mut cover_mismatch = 0usize;
        for i in 0..n_out {
            let c_cov = cw[i] > 1e-9;
            let g_cov = gw_[i] > 1e-9;
            if c_cov != g_cov {
                cover_mismatch += 1;
                continue;
            }
            if !c_cov {
                continue;
            }
            let d = cd[i] / cw[i] - gd[i] / gw_[i];
            se += d * d;
            n += 1.0;
        }
        ((se / n.max(1.0)).sqrt(), cover_mismatch)
    };

    let (mut worst, mut mismatch) = rmse_plane(
        &cpu_g.direct,
        &cpu_g.direct_w,
        &down.direct_g,
        &down.direct_w,
    );
    if is_color {
        let (r, m1) = rmse_plane(&cpu_r.direct, &cpu_r.direct_w, &down.direct_r, &down.direct_w);
        let (b, m2c) = rmse_plane(&cpu_b.direct, &cpu_b.direct_w, &down.direct_b, &down.direct_w);
        worst = worst.max(r).max(b);
        mismatch = mismatch.max(m1).max(m2c);
    }
    if track_m2 && !cpu_g.m2.is_empty() {
        // Lo que importa fisicamente es la VARIANZA derivada (base de los
        // umbrales kσ): var = m2/w − (direct/w)². Compararla directamente.
        let mut var_se = 0.0f64;
        let mut var_ref = 0.0f64;
        let mut nn = 0.0f64;
        for i in 0..n_out {
            if cpu_g.direct_w[i] > 1e-9 && down.direct_w[i] > 1e-9 {
                let cm = cpu_g.direct[i] / cpu_g.direct_w[i];
                let gm = down.direct_g[i] / down.direct_w[i];
                let cv = (cpu_g.m2[i] / cpu_g.direct_w[i] - cm * cm).max(0.0);
                let gv = (down.m2_g[i] / down.direct_w[i] - gm * gm).max(0.0);
                var_se += (cv - gv) * (cv - gv);
                var_ref += cv;
                nn += 1.0;
            }
        }
        let var_rmse = (var_se / nn.max(1.0)).sqrt();
        let var_mean = var_ref / nn.max(1.0);
        eprintln!(
            "[paridad] var: rmse {var_rmse:.2} vs media {var_mean:.2} (σ_ref {:.2})",
            var_mean.max(0.0).sqrt()
        );
        // Tolerancia: el error de var debe ser pequeño frente a la propia var
        // media O frente a (floor de sigma-clip)² = 36 ADU² — por debajo de
        // eso el clamp SIGMA_CLIP_FLOOR de la CPU lo hace irrelevante.
        if var_rmse > (var_mean * 0.05).max(36.0) {
            return Err(format!(
                "varianza m2 fuera de tolerancia: rmse {var_rmse:.1} vs media {var_mean:.1}"
            ));
        }
    }
    eprintln!(
        "[paridad] escenario color={is_color} drz={drizzle} m2={track_m2} bounds={with_bounds}: rmse {worst:.4} ADU, cobertura difiere {mismatch}px"
    );
    if mismatch > n_out / 200 {
        return Err(format!(
            "cobertura difiere en {mismatch} píxeles (> 0.5% de {n_out})"
        ));
    }
    Ok(worst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_loss_flag_is_consumed_once_for_clean_fallback() {
        let _ = take_gpu_error();
        inject_device_loss_for_test();
        assert!(take_gpu_error(), "el coordinador debe observar device loss/OOM");
        assert!(!take_gpu_error(), "el siguiente pase no debe heredar un error ya manejado");
    }

    /// Deteccion real del adapter — requiere GPU fisica, por eso #[ignore]
    /// (correr manualmente: cargo test gpu_detect -- --ignored --nocapture).
    #[test]
    #[ignore]
    fn gpu_detect_reports_adapter() {
        let info = gpu_info();
        eprintln!(
            "GPU disponible: {} — {} ({}) · presupuesto {} MB · paridad {}",
            info.available, info.name, info.backend, info.vram_budget_mb, info.parity
        );
        // En una maquina con GPU (Metal/DX12/Vulkan) debe detectarla.
        assert!(info.available, "no se detecto adapter GPU utilizable");
        assert!(!info.name.is_empty());
    }

    /// DEBUG de paridad: 1 frame, sin warp, mono, dq uniforme — aísla el
    /// muestreo puro y mapea los píxeles con mayor diferencia.
    #[test]
    #[ignore]
    fn gpu_debug_sampling_diff_map() {
        let rt = gpu_runtime().expect("GPU");
        let (w_in, h_in) = (96usize, 80usize);
        let (w_out, h_out) = (w_in, h_in);
        let n_out = w_out * h_out;
        let tau = std::f32::consts::TAU;
        let scene = |x: f32, y: f32| -> f32 {
            9000.0
                + 5000.0 * (x * tau / 7.3).sin() * (y * tau / 9.1).cos()
                + 3000.0 * ((x * 0.6 + y * 0.8) * tau / 13.7).sin()
        };
        let mut px = Vec::with_capacity(w_in * h_in);
        for y in 0..h_in {
            for x in 0..w_in {
                px.push(scene(x as f32, y as f32).clamp(0.0, 65535.0) as u16);
            }
        }
        let (rdx, rdy) = (0.37f32, -0.53f32); // shift sub-pixel puro

        // CPU
        let mut wg = vec![0f32; n_out];
        let mut ww = vec![0f32; n_out];
        crate::liquid_warping::accumulate_frame_liquid_mono(
            &mut wg, &mut ww, &px, w_in, h_in, w_out, h_out, 1.0, 0.0, 0.0, rdx, rdy,
            &[], &[], &[], &[], &[], 1.0, 1.0,
        );
        for i in 0..n_out {
            if ww[i] > 1e-9 {
                wg[i] /= ww[i];
            }
        }

        // GPU (sin warp, dq desactivado, gw=1)
        let cfg = GpuPassConfig {
            w_in, h_in, w_out, h_out,
            is_color: false, track_m2: false, use_bounds: false,
            coverage_weighting: false, true_drizzle: false, use_warp: false,
            k: 1, n_aps: 0, drizzle: 1.0, roi_off_x: 0.0, roi_off_y: 0.0,
            drop_size: 1.0, global_fallback_weight: 1.0, dq_w: 0, dq_h: 0,
        };
        let mut gpu = GpuPassAccumulator::new(rt, cfg, &[], &[]).unwrap();
        gpu.accumulate_frame(&GpuFrameJob {
            pixels: px.clone(),
            apq: Vec::new(),
            dq_map: Vec::new(),
            gw: 1.0,
            render_dx: rdx,
            render_dy: rdy,
            q_off_x: 0.0,
            q_off_y: 0.0,
        })
        .unwrap();
        let down = gpu.finish().unwrap();

        let mut worst: Vec<(f64, usize)> = Vec::new();
        let mut over1 = 0usize;
        for i in 0..n_out {
            let (x, y) = (i % w_out, i / w_out);
            // El kernel GPU excluye bordes (paridad con StripedAccum); este
            // debug llama a la CPU SIN StripedAccum → comparar solo interior.
            if x == 0 || y == 0 || x == w_out - 1 || y == h_out - 1 {
                continue;
            }
            let c_cov = ww[i] > 1e-9;
            let g_cov = down.direct_w[i] > 1e-9;
            if c_cov != g_cov {
                eprintln!("cobertura difiere en px {} ({},{})", i, x, y);
                continue;
            }
            if !c_cov {
                continue;
            }
            let d = (wg[i] as f64 - down.direct_g[i] / down.direct_w[i]).abs();
            if d > 1.0 {
                over1 += 1;
            }
            worst.push((d, i));
        }
        worst.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        eprintln!("px con diff > 1 ADU: {over1} de {n_out}");
        for (d, i) in worst.iter().take(10) {
            let (x, y) = (i % w_out, i / w_out);
            eprintln!(
                "  px ({x},{y}): diff {d:.3} — cpu {:.2} gpu {:.2} (w cpu {:.4} gpu {:.4})",
                wg[*i],
                down.direct_g[*i] / down.direct_w[*i],
                ww[*i],
                down.direct_w[*i]
            );
        }
    }

    /// PARIDAD GPU-vs-CPU — el candado de calidad del modo GPU. Requiere GPU
    /// fisica: cargo test gpu_parity -- --ignored --nocapture
    #[test]
    #[ignore]
    fn gpu_parity_matches_cpu_reference() {
        let rmse = run_parity_check().expect("self-test de paridad ejecutable");
        eprintln!("PARIDAD GPU-vs-CPU: peor RMSE = {rmse:.4} ADU16");
        assert!(
            rmse <= PARITY_RMSE_TOLERANCE,
            "RMSE {rmse:.4} supera la tolerancia {PARITY_RMSE_TOLERANCE}"
        );
    }

    /// PR-1.3: réplica exacta en u64 de la aritmética Q56.8 del shader
    /// (fx_add_q8 + decode /256). Un SER de alta velocidad de 200k frames
    /// con píxeles saturados (término máximo val²·cw = 65535² ≈ 4.29e9) NO
    /// debe desbordar y el decode debe coincidir con la suma f64 dentro de
    /// la resolución del formato. En Q48.16 esta misma suma desbordaba a
    /// ~65k frames (techo 2^48 ≈ 2.8e14 < 200k × 4.29e9 ≈ 8.6e14).
    #[test]
    fn m2_q56_8_survives_200k_saturated_frames() {
        // Emulación bit-exacta de fx_add_q8 sobre (lo, hi) u32 con carry.
        let fx_add_q8 = |acc: &mut (u32, u32), v: f64| {
            let mut hi = (v / 16_777_216.0).floor();
            let rem = v - hi * 16_777_216.0;
            let lo_f = rem * 256.0;
            let lo: u32 = if lo_f >= 4_294_967_040.0 {
                hi += 1.0;
                0
            } else {
                lo_f.round() as u32
            };
            let (nlo, carry) = acc.0.overflowing_add(lo);
            acc.0 = nlo;
            acc.1 = acc.1.wrapping_add(hi as u32).wrapping_add(carry as u32);
        };
        let term = 65_535.0f64 * 65_535.0; // val²·cw con cw=1 (peor caso)
        let n_frames = 200_000u64;
        let mut acc = (0u32, 0u32);
        for _ in 0..n_frames {
            fx_add_q8(&mut acc, term);
        }
        let decoded = ((acc.1 as u64) << 32 | acc.0 as u64) as f64 / 256.0;
        let truth = term * n_frames as f64;
        // Comprobar que NO desbordó (Q48.16 habría dado un valor ~3x menor
        // por wrap) y que la precisión es sobrada para una sigma fiable.
        let rel_err = (decoded - truth).abs() / truth;
        assert!(
            rel_err < 1e-9,
            "Q56.8 debe representar 200k frames saturados sin desbordar: decode={decoded:.3e} truth={truth:.3e} rel={rel_err:.2e}"
        );
    }
}
