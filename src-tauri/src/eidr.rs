// ============================================================================
// EIDR — sucesor forward-model de Drizzle (F9 del plan NebulaFusion/EIDR).
//
// Modelo directo (§7.2 del PLAN_TECNICO): para el frame i y el píxel detector
// p, la predicción es y_i(p) = [A_i z](p) con A_i = C·D·P·H_i·W_i. En lugar de
// materializar cada operador, se compone un ÚNICO kernel de depósito continuo
// por frame:
//
//     K_i(Δ) = (1_píxel_nativo ⊛ B_i ⊛ 1_celda_salida)(Δ),   Δ = p − f_i(q)
//
// donde:
//  - z es la escena A LA PSF OBJETIVO Γ sobre el grid de salida (hipótesis
//    pixelizada: constante en cada celda). Convención de brillo superficial:
//    un fondo plano V produce predicción V, igual que el drizzle actual.
//  - B_i es la PSF RELATIVA del frame respecto a Γ: H_i = Γ_frame ⊛ B_i,
//    calculada por deconvolución en Fourier con guarda de Tikhonov sobre el
//    raster fino. Sin ajuste PSF fiable, B_i = δ (modo geometría pura).
//  - 1_píxel_nativo es el indicador del píxel detector (lado 1, integración
//    de área P·D) y 1_celda_salida el paralelogramo imagen de la celda de
//    salida bajo el registro (celdas que TESELAN el plano ⇒ Σ_q K_i = 1:
//    conservación de flujo exacta, verificada por test).
//  - f_i(q) = t_i.inverse(q/s): afín exacta para Similarity/Affine (los
//    modelos admitidos en F9; Projective con perspectiva real y
//    LocalDistortion se rechazan en el preflight).
//
// K_i se precalcula UNA VEZ por frame en una LUT 2D supersampleada (paso
// EIDR_LUT_STEP) y apply/adjoint la evalúan con interpolación bilineal. Ambas
// direcciones son GATHERS por filas (sin escrituras cruzadas ⇒ paralelismo
// por chunks sin unsafe) y evalúan la MISMA matriz ⇒ el adjunto es exacto por
// construcción (identidad <1e-5 en float64 exigida por §7.7; el test la mide).
//
// La cobertura parcial en bordes/huecos queda modelada de forma EXACTA por
// las ecuaciones normales (la fila ausente desaparece de AᵀΣ⁻¹A y de AᵀΣ⁻¹y
// a la vez): la clase de sesgo corregida en NF-Full es imposible aquí.
//
// El módulo es cómputo puro (sin tauri): la integración vive en deepsky.rs.
// ============================================================================

use rayon::prelude::*;
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use crate::deepsky_psf::MoffatPsf;

/// Paso del raster fino de la LUT (px de frame). El rizado de la evaluación
/// bilineal sobre el kernel más compacto (B=δ: caja⊛celda, curvatura alta)
/// decae con δ²: a 1/32 px la transferencia plana queda en ~5e-4 (test).
const EIDR_LUT_STEP: f64 = 0.03125;
/// Guarda de Tikhonov para la deconvolución H/Γ (relativa a |Γ̂|²max = 1).
const EIDR_DECONV_TAU: f64 = 1e-3;
/// Recorte de la LUT: se descartan colas bajo este umbral relativo al pico.
/// 2e-5 mantiene el déficit de flujo de la truncación bajo ~1e-3 incluso con
/// B de lóbulos largos (H más ancha que Γ ⇒ kernel afilador con ringing).
const EIDR_LUT_TRIM: f32 = 2e-5;

// ---------------------------------------------------------------------------
// Geometría por frame
// ---------------------------------------------------------------------------

/// Geometría afín precalculada de un frame: f(q) = f0 + fx·qx + fy·qy lleva
/// el píxel de SALIDA q a coordenadas del frame; g(p) es la inversa (frame →
/// salida). `map_scale`/`map_rot` describen la similitud ref→frame a escala
/// NATIVA (para mapear Γ al frame); `q_rad_factor` convierte el radio de la
/// LUT (px de frame) en radio de ventana en px de salida.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EidrGeom {
    pub f0: [f64; 2],
    pub fx: [f64; 2],
    pub fy: [f64; 2],
    pub g0: [f64; 2],
    pub gx: [f64; 2],
    pub gy: [f64; 2],
    pub map_scale: f64,
    pub map_rot: f64,
    pub q_rad_factor: f64,
}

/// Extrae la geometría afín de un `DsTransform` (target→reference) a la
/// escala de salida `s`. Devuelve `None` si el modelo no es afín (perspectiva
/// real o distorsión local): el preflight de F9 los excluye antes.
pub(crate) fn eidr_geom(t: &crate::DsTransform, scale: f32) -> Option<EidrGeom> {
    let s = scale.max(0.25) as f64;
    let at = |x: f64, y: f64| -> Option<[f64; 2]> {
        let (fx, fy) = t.inverse(x as f32, y as f32)?;
        (fx.is_finite() && fy.is_finite()).then_some([fx as f64, fy as f64])
    };
    // Muestreo en una base amplia para promediar el error f32 de inverse().
    const B: f64 = 256.0;
    let f00 = at(0.0, 0.0)?;
    let fx0 = at(B, 0.0)?;
    let f0y = at(0.0, B)?;
    // Verificación de afinidad: el punto diagonal debe ser consistente.
    let fdiag = at(B, B)?;
    let ex = f00[0] + (fx0[0] - f00[0]) + (f0y[0] - f00[0]) - fdiag[0];
    let ey = f00[1] + (fx0[1] - f00[1]) + (f0y[1] - f00[1]) - fdiag[1];
    if ex.abs() > 0.02 || ey.abs() > 0.02 {
        return None; // curvatura real (proyectiva/local): no representable
    }
    // Jacobiano ref→frame a escala nativa y por píxel de salida (÷s).
    let jn = [
        [(fx0[0] - f00[0]) / B, (f0y[0] - f00[0]) / B],
        [(fx0[1] - f00[1]) / B, (f0y[1] - f00[1]) / B],
    ];
    let det = jn[0][0] * jn[1][1] - jn[0][1] * jn[1][0];
    if !det.is_finite() || det.abs() < 1e-6 {
        return None;
    }
    let fx = [jn[0][0] / s, jn[1][0] / s];
    let fy = [jn[0][1] / s, jn[1][1] / s];
    // Inversa exacta del afín de salida: g(p) = g0 + gx·px + gy·py.
    let dets = det / (s * s);
    let gx = [fy[1] / dets, -fx[1] / dets];
    let gy = [-fy[0] / dets, fx[0] / dets];
    let g0 = [
        -(gx[0] * f00[0] + gy[0] * f00[1]),
        -(gx[1] * f00[0] + gy[1] * f00[1]),
    ];
    // Valor singular mínimo del 2×2 [fx fy] (px frame por px salida):
    // ‖q−g(p)‖ ≤ ‖Δ‖/σ_min ⇒ radio de ventana en salida.
    let a = fx[0] * fx[0] + fx[1] * fx[1];
    let b = fx[0] * fy[0] + fx[1] * fy[1];
    let c = fy[0] * fy[0] + fy[1] * fy[1];
    let tr = a + c;
    let disc = ((a - c) * (a - c) + 4.0 * b * b).sqrt();
    let smin2 = 0.5 * (tr - disc);
    if smin2 <= 1e-12 {
        return None;
    }
    Some(EidrGeom {
        f0: f00,
        fx,
        fy,
        g0,
        gx,
        gy,
        map_scale: det.abs().sqrt(),
        map_rot: jn[1][0].atan2(jn[0][0]),
        q_rad_factor: 1.0 / smin2.sqrt(),
    })
}

impl EidrGeom {
    #[inline]
    fn f(&self, qx: f64, qy: f64) -> (f64, f64) {
        (
            self.f0[0] + self.fx[0] * qx + self.fy[0] * qy,
            self.f0[1] + self.fx[1] * qx + self.fy[1] * qy,
        )
    }
    #[inline]
    fn g(&self, px: f64, py: f64) -> (f64, f64) {
        (
            self.g0[0] + self.gx[0] * px + self.gy[0] * py,
            self.g0[1] + self.gx[1] * px + self.gy[1] * py,
        )
    }
}

// ---------------------------------------------------------------------------
// LUT del kernel de depósito
// ---------------------------------------------------------------------------

pub(crate) struct EidrLut {
    data: Vec<f32>,
    n: usize,
    /// Radio de soporte en px de frame (tras el recorte de colas).
    pub radius: f32,
}

impl EidrLut {
    /// K(Δ) por interpolación bilineal; 0 fuera del soporte.
    #[inline]
    pub(crate) fn eval(&self, dx: f32, dy: f32) -> f32 {
        let inv = 1.0 / EIDR_LUT_STEP as f32;
        let c = (self.n as f32 - 1.0) * 0.5;
        let x = dx * inv + c;
        let y = dy * inv + c;
        if x < 0.0 || y < 0.0 {
            return 0.0;
        }
        let x0 = x as usize;
        let y0 = y as usize;
        if x0 + 1 >= self.n || y0 + 1 >= self.n {
            return 0.0;
        }
        let fx = x - x0 as f32;
        let fy = y - y0 as f32;
        let r0 = y0 * self.n + x0;
        let r1 = r0 + self.n;
        self.data[r0] * (1.0 - fx) * (1.0 - fy)
            + self.data[r0 + 1] * fx * (1.0 - fy)
            + self.data[r1] * (1.0 - fx) * fy
            + self.data[r1 + 1] * fx * fy
    }
}

fn moffat_alpha(fwhm: f64, beta: f64) -> f64 {
    fwhm.max(1e-3) / (2.0 * (2f64.powf(1.0 / beta.max(1.05)) - 1.0).sqrt())
}

/// Raster fino de una Moffat elíptica normalizada (Σ·δ² = 1), centrada en el
/// centro geométrico de un grid n×n impar.
fn raster_moffat(psf: &MoffatPsf, n: usize) -> Vec<f64> {
    let c = (n as f64 - 1.0) * 0.5;
    let ax = moffat_alpha(psf.fwhm_x as f64, psf.beta as f64);
    let ay = moffat_alpha(psf.fwhm_y as f64, psf.beta as f64);
    let (st, ct) = (psf.theta as f64).sin_cos();
    let beta = psf.beta.max(1.05) as f64;
    let mut out = vec![0.0f64; n * n];
    let mut sum = 0.0f64;
    for j in 0..n {
        let dy = (j as f64 - c) * EIDR_LUT_STEP;
        for i in 0..n {
            let dx = (i as f64 - c) * EIDR_LUT_STEP;
            let u = ct * dx + st * dy;
            let v = -st * dx + ct * dy;
            let r2 = (u / ax) * (u / ax) + (v / ay) * (v / ay);
            let val = (1.0 + r2).powf(-beta);
            out[j * n + i] = val;
            sum += val;
        }
    }
    let norm = 1.0 / (sum * EIDR_LUT_STEP * EIDR_LUT_STEP);
    for v in &mut out {
        *v *= norm;
    }
    out
}

/// FFT 2D in-place sobre un grid n×n (filas + transposición doble).
fn fft2(buf: &mut [Complex<f64>], n: usize, planner: &mut FftPlanner<f64>, inverse: bool) {
    let fft = if inverse {
        planner.plan_fft_inverse(n)
    } else {
        planner.plan_fft_forward(n)
    };
    for row in buf.chunks_exact_mut(n) {
        fft.process(row);
    }
    // Transposición cuadrada in-place.
    for j in 0..n {
        for i in (j + 1)..n {
            buf.swap(j * n + i, i * n + j);
        }
    }
    for row in buf.chunks_exact_mut(n) {
        fft.process(row);
    }
    for j in 0..n {
        for i in (j + 1)..n {
            buf.swap(j * n + i, i * n + j);
        }
    }
}

/// PSF relativa B tal que H ≈ Γf ⊛ B, vía división en Fourier con guarda de
/// Tikhonov. Ambos rasters comparten centro ⇒ las fases se cancelan y B queda
/// centrada en el origen; se recentra con un fftshift y se renormaliza a 1
/// (la guarda encoge ligeramente el DC: renormalizar preserva el flujo).
fn relative_psf(h: &[f64], gamma: &[f64], n: usize) -> Vec<f64> {
    let mut planner = FftPlanner::<f64>::new();
    let mut hf: Vec<Complex<f64>> = h.iter().map(|&v| Complex::new(v, 0.0)).collect();
    let mut gf: Vec<Complex<f64>> = gamma.iter().map(|&v| Complex::new(v, 0.0)).collect();
    fft2(&mut hf, n, &mut planner, false);
    fft2(&mut gf, n, &mut planner, false);
    let g0 = gf[0].norm_sqr().max(1e-30);
    let tau = EIDR_DECONV_TAU * g0;
    for (a, b) in hf.iter_mut().zip(gf.iter()) {
        *a = *a * b.conj() / (b.norm_sqr() + tau);
    }
    fft2(&mut hf, n, &mut planner, true);
    let scale = 1.0 / (n * n) as f64;
    let c = n / 2; // n impar: (n-1)/2 = n/2 en división entera
    let mut out = vec![0.0f64; n * n];
    for j in 0..n {
        for i in 0..n {
            // fftshift: B está centrada en (0,0) con wraparound.
            let sj = (j + n - c) % n;
            let si = (i + n - c) % n;
            out[j * n + i] = hf[sj * n + si].re * scale;
        }
    }
    let sum: f64 = out.iter().sum::<f64>() * EIDR_LUT_STEP * EIDR_LUT_STEP;
    if sum.abs() > 1e-9 {
        let k = 1.0 / sum;
        for v in &mut out {
            *v *= k;
        }
    }
    out
}

/// Construye la LUT del kernel de depósito de un frame.
///
/// `h_psf = None` ⇒ B = δ (modo geometría pura: el frame se modela con PSF
/// igual a Γ; se usa cuando no hay ajuste Moffat fiable y queda declarado).
pub(crate) fn eidr_build_lut(
    h_psf: Option<MoffatPsf>,
    gamma: MoffatPsf,
    geom: &EidrGeom,
) -> EidrLut {
    // Γ mapeada al frame (similitud local): FWHM × escala, θ + rotación.
    let gamma_f = MoffatPsf {
        fwhm_x: gamma.fwhm_x * geom.map_scale as f32,
        fwhm_y: gamma.fwhm_y * geom.map_scale as f32,
        theta: gamma.theta + geom.map_rot as f32,
        beta: gamma.beta,
    };
    let fmax = h_psf
        .map(|p| p.fwhm_x.max(p.fwhm_y))
        .unwrap_or(0.0)
        .max(gamma_f.fwhm_x.max(gamma_f.fwhm_y)) as f64;
    // Extensión de la celda de salida en el frame.
    let cell_ext = (geom.fx[0].abs() + geom.fy[0].abs())
        .max(geom.fx[1].abs() + geom.fy[1].abs())
        .max(0.25);
    let r0 = (2.0 * fmax).max(3.0) + 0.8 + 0.8 * cell_ext;
    let half = ((r0 / EIDR_LUT_STEP).ceil() as usize).min(520);
    let n = 2 * half + 1;

    // B: PSF relativa (o δ) como densidad en el grid fino.
    let b = match h_psf {
        Some(h) => {
            let hr = raster_moffat(&h, n);
            let gr = raster_moffat(&gamma_f, n);
            relative_psf(&hr, &gr, n)
        }
        None => {
            let mut d = vec![0.0f64; n * n];
            d[(n / 2) * n + (n / 2)] = 1.0 / (EIDR_LUT_STEP * EIDR_LUT_STEP);
            d
        }
    };

    // ⊛ píxel nativo (indicador [-0.5,0.5]²): separable, taps trapezoidales
    // (los extremos cubren media celda fina) × δ por eje.
    let taps: Vec<f64> = {
        let m = (0.5 / EIDR_LUT_STEP).round() as i64; // 4
        (-m..=m)
            .map(|k| if k.abs() == m { 0.5 } else { 1.0 } * EIDR_LUT_STEP)
            .collect()
    };
    let th = (taps.len() / 2) as i64;
    let mut k1 = vec![0.0f64; n * n];
    for j in 0..n {
        for i in 0..n {
            let mut acc = 0.0;
            for (t, wt) in taps.iter().enumerate() {
                let ii = i as i64 + (t as i64 - th);
                if ii >= 0 && (ii as usize) < n {
                    acc += b[j * n + ii as usize] * wt;
                }
            }
            k1[j * n + i] = acc;
        }
    }
    let mut k2 = vec![0.0f64; n * n];
    for j in 0..n {
        for i in 0..n {
            let mut acc = 0.0;
            for (t, wt) in taps.iter().enumerate() {
                let jj = j as i64 + (t as i64 - th);
                if jj >= 0 && (jj as usize) < n {
                    acc += k1[jj as usize * n + i] * wt;
                }
            }
            k2[j * n + i] = acc;
        }
    }

    // ⊛ celda de salida: paralelogramo {u·fx + v·fy : |u|,|v| ≤ ½} rasterizado
    // con antialiasing 4×4. Convolución directa (kernel pequeño) × δ².
    let cw = ((cell_ext * 0.5 + 2.0 * EIDR_LUT_STEP) / EIDR_LUT_STEP).ceil() as i64;
    let det = geom.fx[0] * geom.fy[1] - geom.fx[1] * geom.fy[0];
    let inv = [
        geom.fy[1] / det,
        -geom.fy[0] / det,
        -geom.fx[1] / det,
        geom.fx[0] / det,
    ];
    let mut cell: Vec<(i64, i64, f64)> = Vec::new();
    for cj in -cw..=cw {
        for ci in -cw..=cw {
            let mut inside = 0u32;
            for sj in 0..4 {
                for si in 0..4 {
                    let x = (ci as f64 + (si as f64 + 0.5) / 4.0 - 0.5) * EIDR_LUT_STEP;
                    let y = (cj as f64 + (sj as f64 + 0.5) / 4.0 - 0.5) * EIDR_LUT_STEP;
                    let u = inv[0] * x + inv[1] * y;
                    let v = inv[2] * x + inv[3] * y;
                    if u.abs() <= 0.5 && v.abs() <= 0.5 {
                        inside += 1;
                    }
                }
            }
            if inside > 0 {
                cell.push((ci, cj, inside as f64 / 16.0));
            }
        }
    }
    let d2 = EIDR_LUT_STEP * EIDR_LUT_STEP;
    let mut kfin = vec![0.0f64; n * n];
    for j in 0..n as i64 {
        for i in 0..n as i64 {
            let mut acc = 0.0;
            for &(ci, cj, wc) in &cell {
                let ii = i - ci;
                let jj = j - cj;
                if ii >= 0 && jj >= 0 && (ii as usize) < n && (jj as usize) < n {
                    acc += k2[jj as usize * n + ii as usize] * wc;
                }
            }
            kfin[(j as usize) * n + i as usize] = acc * d2;
        }
    }

    // Suavizado C¹: carpa separable de anchura 2δ. K = caja⊛celda⊛B tiene
    // pliegues C⁰ (bordes de los indicadores) y el error de la bilineal sobre
    // pliegues decae solo O(δ); la carpa (≡ difuminar la celda 1/16 px,
    // idéntica en apply/adjoint y de integral 1) lo devuelve a O(δ²): la
    // transferencia plana medida pasa de ~2e-3 a ~5e-4.
    for pass in 0..2 {
        let mut tmp = kfin.clone();
        for j in 0..n {
            for i in 0..n {
                let get = |jj: i64, ii: i64| -> f64 {
                    if jj < 0 || ii < 0 || jj >= n as i64 || ii >= n as i64 {
                        0.0
                    } else {
                        kfin[jj as usize * n + ii as usize]
                    }
                };
                let (j64, i64_) = (j as i64, i as i64);
                tmp[j * n + i] = if pass == 0 {
                    0.25 * get(j64, i64_ - 1) + 0.5 * get(j64, i64_) + 0.25 * get(j64, i64_ + 1)
                } else {
                    0.25 * get(j64 - 1, i64_) + 0.5 * get(j64, i64_) + 0.25 * get(j64 + 1, i64_)
                };
            }
        }
        kfin = tmp;
    }

    // Recorte de colas: soporte cuadrado mínimo con |K| ≥ trim·max.
    let kmax = kfin.iter().fold(0.0f64, |m, &v| m.max(v.abs()));
    let thr = kmax * EIDR_LUT_TRIM as f64;
    let c = half as i64;
    let mut keep = 1i64;
    for j in 0..n as i64 {
        for i in 0..n as i64 {
            if kfin[(j as usize) * n + i as usize].abs() >= thr {
                keep = keep.max((i - c).abs()).max((j - c).abs());
            }
        }
    }
    let keep = (keep + 1).min(c) as usize;
    let nn = 2 * keep + 1;
    let mut data = vec![0.0f32; nn * nn];
    let mut ksum = 0.0f64;
    for j in 0..nn {
        for i in 0..nn {
            let v = kfin[(j + half - keep) * n + (i + half - keep)];
            ksum += v;
            data[j * nn + i] = v as f32;
        }
    }
    // Normalización analítica de la suma de retícula: las celdas de salida
    // teselan el plano ⇒ Σ_q K(p−f(q)) = ∫K / área_celda. Forzarla a 1 tras
    // el recorte anula TODO déficit sistemático de flujo (colas power-law de
    // Moffat fuera del raster, recorte, cuadratura discreta); solo queda el
    // rizado local O(δ²), medido por el test de transferencia plana.
    let cell_area = (geom.fx[0] * geom.fy[1] - geom.fx[1] * geom.fy[0]).abs();
    let lattice_sum = ksum * EIDR_LUT_STEP * EIDR_LUT_STEP / cell_area.max(1e-12);
    if lattice_sum > 1e-6 {
        let fnorm = (1.0 / lattice_sum) as f32;
        for v in &mut data {
            *v *= fnorm;
        }
    }
    EidrLut {
        data,
        n: nn,
        radius: (keep as f64 * EIDR_LUT_STEP) as f32,
    }
}

/// PSF objetivo Γ del conjunto: Moffat CIRCULAR con FWHM en el percentil 80
/// de las medias por frame (≈ el frame más ancho razonable: B_i queda casi
/// siempre difuminante ⇒ mínima deconvolución, máxima honestidad) y β la
/// mediana. Sin ajustes ⇒ None (modo geometría pura con Γ nominal).
pub(crate) fn eidr_target_psf(psfs: &[Option<MoffatPsf>]) -> Option<MoffatPsf> {
    let mut fw: Vec<f32> = psfs
        .iter()
        .flatten()
        .map(|p| p.fwhm_mean())
        .filter(|f| f.is_finite() && *f > 0.3)
        .collect();
    if fw.is_empty() {
        return None;
    }
    fw.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p80 = fw[((fw.len() - 1) as f32 * 0.8).round() as usize];
    let mut betas: Vec<f32> = psfs
        .iter()
        .flatten()
        .map(|p| p.beta)
        .filter(|b| b.is_finite())
        .collect();
    betas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let beta = if betas.is_empty() {
        2.5
    } else {
        betas[betas.len() / 2].clamp(1.5, 6.0)
    };
    Some(MoffatPsf {
        fwhm_x: p80,
        fwhm_y: p80,
        theta: 0.0,
        beta,
    })
}

// ---------------------------------------------------------------------------
// Operador
// ---------------------------------------------------------------------------

/// Estado por frame del operador (solo geometría + estadística: los DATOS del
/// frame no viven aquí — únicamente entran en b = AᵀΣ⁻¹y y en el holdout).
pub(crate) struct EidrFrameOp {
    pub geom: EidrGeom,
    pub lut: EidrLut,
    /// 1/σ′² por canal en unidades NORMALIZADAS (σ′ = mul·σ del frame).
    pub inv_var: [f32; 3],
    /// Bitset w·h: bit=1 ⇒ píxel INVÁLIDO (no finito / marcado). La fila
    /// desaparece del sistema: apply lo deja a 0 y adjoint/diag lo saltan.
    pub mask: Vec<u64>,
    pub w: usize,
    pub h: usize,
}

#[inline]
fn bit(mask: &[u64], p: usize) -> bool {
    (mask[p >> 6] >> (p & 63)) & 1 == 1
}

/// Bitset de píxeles inválidos (algún canal no finito).
pub(crate) fn eidr_invalid_mask(data: &[f32], w: usize, h: usize, ch: usize) -> Vec<u64> {
    let mut m = vec![0u64; (w * h + 63) / 64];
    for p in 0..w * h {
        if (0..ch).any(|c| !data[p * ch + c].is_finite()) {
            m[p >> 6] |= 1u64 << (p & 63);
        }
    }
    m
}

/// Paridades (x&1, y&1) de los fotositos del canal `c` en el patrón CFA
/// `cid` (8..11, convención de `ds_cfa_channel`). R/B: 1 paridad; G: 2.
fn cfa_parities(cid: i32, c: usize) -> Vec<(usize, usize)> {
    (0..2usize)
        .flat_map(|y| (0..2usize).map(move |x| (x, y)))
        .filter(|&(x, y)| crate::ds_cfa_channel(cid, x, y) == c)
        .collect()
}

pub(crate) struct EidrOperator {
    pub frames: Vec<EidrFrameOp>,
    pub w_out: usize,
    pub h_out: usize,
    /// `Some(cid)` ⇒ frames CFA de 1 plano; el canal selecciona la retícula.
    pub cfa: Option<i32>,
    pub ch: usize,
}

impl EidrOperator {
    /// predicted = A_i z (plano del frame `fi`, canal `c`). Los píxeles
    /// enmascarados o fuera de la retícula CFA quedan a 0.
    pub(crate) fn apply(&self, fi: usize, c: usize, z: &[f32], predicted: &mut [f32]) {
        let f = &self.frames[fi];
        debug_assert_eq!(z.len(), self.w_out * self.h_out);
        debug_assert_eq!(predicted.len(), f.w * f.h);
        let pars = self.cfa.map(|cid| cfa_parities(cid, c));
        let q_rad = f.lut.radius as f64 * f.geom.q_rad_factor + 1.0;
        let (w_out, h_out) = (self.w_out, self.h_out);
        let fw = f.w;
        predicted
            .par_chunks_mut(fw)
            .enumerate()
            .for_each(|(py, row)| {
                for (px, out) in row.iter_mut().enumerate() {
                    *out = 0.0;
                    if let Some(ps) = &pars {
                        if !ps.contains(&(px & 1, py & 1)) {
                            continue;
                        }
                    }
                    if bit(&f.mask, py * fw + px) {
                        continue;
                    }
                    let (gx, gy) = f.geom.g(px as f64, py as f64);
                    if !gx.is_finite() || !gy.is_finite() {
                        continue;
                    }
                    let q0x = ((gx - q_rad).ceil() as i64).max(0) as usize;
                    let q1x = ((gx + q_rad).floor() as i64).min(w_out as i64 - 1);
                    let q0y = ((gy - q_rad).ceil() as i64).max(0) as usize;
                    let q1y = ((gy + q_rad).floor() as i64).min(h_out as i64 - 1);
                    if q1x < q0x as i64 || q1y < q0y as i64 {
                        continue;
                    }
                    let (q1x, q1y) = (q1x as usize, q1y as usize);
                    let mut acc = 0.0f64;
                    for qy in q0y..=q1y {
                        let (mut fxq, mut fyq) =
                            f.geom.f(q0x as f64, qy as f64);
                        let zrow = &z[qy * w_out..qy * w_out + w_out];
                        for qx in q0x..=q1x {
                            let dx = px as f64 - fxq;
                            let dy = py as f64 - fyq;
                            fxq += f.geom.fx[0];
                            fyq += f.geom.fx[1];
                            let k = f.lut.eval(dx as f32, dy as f32);
                            if k != 0.0 {
                                acc += k as f64 * zrow[qx] as f64;
                            }
                        }
                    }
                    *out = acc as f32;
                }
            });
    }

    /// grad += weight · A_iᵀ r (r en el plano del frame `fi`, canal `c`).
    pub(crate) fn adjoint_accum(
        &self,
        fi: usize,
        c: usize,
        r: &[f32],
        weight: f64,
        grad: &mut [f64],
    ) {
        let f = &self.frames[fi];
        debug_assert_eq!(r.len(), f.w * f.h);
        debug_assert_eq!(grad.len(), self.w_out * self.h_out);
        let pars = self.cfa.map(|cid| cfa_parities(cid, c));
        let rad = f.lut.radius as f64 + 0.5;
        let (fw, fh) = (f.w, f.h);
        let w_out = self.w_out;
        grad.par_chunks_mut(w_out)
            .enumerate()
            .for_each(|(qy, grow)| {
                for (qx, gout) in grow.iter_mut().enumerate() {
                    let (fxq, fyq) = f.geom.f(qx as f64, qy as f64);
                    if !fxq.is_finite() || !fyq.is_finite() {
                        continue;
                    }
                    let p0x = ((fxq - rad).ceil() as i64).max(0) as usize;
                    let p1x = ((fxq + rad).floor() as i64).min(fw as i64 - 1);
                    let p0y = ((fyq - rad).ceil() as i64).max(0) as usize;
                    let p1y = ((fyq + rad).floor() as i64).min(fh as i64 - 1);
                    if p1x < p0x as i64 || p1y < p0y as i64 {
                        continue;
                    }
                    let (p1x, p1y) = (p1x as usize, p1y as usize);
                    let mut acc = 0.0f64;
                    match &pars {
                        None => {
                            for py in p0y..=p1y {
                                let base = py * fw;
                                for px in p0x..=p1x {
                                    if bit(&f.mask, base + px) {
                                        continue;
                                    }
                                    let k = f.lut.eval(
                                        (px as f64 - fxq) as f32,
                                        (py as f64 - fyq) as f32,
                                    );
                                    if k != 0.0 {
                                        acc += k as f64 * r[base + px] as f64;
                                    }
                                }
                            }
                        }
                        Some(ps) => {
                            for &(ox, oy) in ps {
                                let sy = p0y + ((oy + 2 - (p0y & 1)) & 1);
                                let mut py = sy;
                                while py <= p1y {
                                    let base = py * fw;
                                    let sx = p0x + ((ox + 2 - (p0x & 1)) & 1);
                                    let mut px = sx;
                                    while px <= p1x {
                                        if !bit(&f.mask, base + px) {
                                            let k = f.lut.eval(
                                                (px as f64 - fxq) as f32,
                                                (py as f64 - fyq) as f32,
                                            );
                                            if k != 0.0 {
                                                acc += k as f64 * r[base + px] as f64;
                                            }
                                        }
                                        px += 2;
                                    }
                                    py += 2;
                                }
                            }
                        }
                    }
                    *gout += weight * acc;
                }
            });
    }

    /// diag += Σ_p K²(p−f(q))·inv_var — diagonal de AᵀΣ⁻¹A del frame `fi`
    /// (precondicionador Jacobi y varianza aproximada del resultado).
    pub(crate) fn normal_diag_accum(&self, fi: usize, c: usize, diag: &mut [f64]) {
        let f = &self.frames[fi];
        debug_assert_eq!(diag.len(), self.w_out * self.h_out);
        let pars = self.cfa.map(|cid| cfa_parities(cid, c));
        let rad = f.lut.radius as f64 + 0.5;
        let (fw, fh) = (f.w, f.h);
        let w_out = self.w_out;
        let ivar = f.inv_var[c.min(2)] as f64;
        diag.par_chunks_mut(w_out)
            .enumerate()
            .for_each(|(qy, drow)| {
                for (qx, dout) in drow.iter_mut().enumerate() {
                    let (fxq, fyq) = f.geom.f(qx as f64, qy as f64);
                    if !fxq.is_finite() || !fyq.is_finite() {
                        continue;
                    }
                    let p0x = ((fxq - rad).ceil() as i64).max(0) as usize;
                    let p1x = ((fxq + rad).floor() as i64).min(fw as i64 - 1);
                    let p0y = ((fyq - rad).ceil() as i64).max(0) as usize;
                    let p1y = ((fyq + rad).floor() as i64).min(fh as i64 - 1);
                    if p1x < p0x as i64 || p1y < p0y as i64 {
                        continue;
                    }
                    let (p1x, p1y) = (p1x as usize, p1y as usize);
                    let mut acc = 0.0f64;
                    let mut visit = |px: usize, py: usize| {
                        if !bit(&f.mask, py * fw + px) {
                            let k = f.lut.eval(
                                (px as f64 - fxq) as f32,
                                (py as f64 - fyq) as f32,
                            ) as f64;
                            acc += k * k;
                        }
                    };
                    match &pars {
                        None => {
                            for py in p0y..=p1y {
                                for px in p0x..=p1x {
                                    visit(px, py);
                                }
                            }
                        }
                        Some(ps) => {
                            for &(ox, oy) in ps {
                                let mut py = p0y + ((oy + 2 - (p0y & 1)) & 1);
                                while py <= p1y {
                                    let mut px = p0x + ((ox + 2 - (p0x & 1)) & 1);
                                    while px <= p1x {
                                        visit(px, py);
                                        px += 2;
                                    }
                                    py += 2;
                                }
                            }
                        }
                    }
                    *dout += ivar * acc;
                }
            });
    }

    /// out = Σ_i A_iᵀ Σ_i⁻¹ A_i p  (producto de las ecuaciones normales;
    /// `scratch` se redimensiona al frame mayor y se reutiliza).
    pub(crate) fn normal_apply(
        &self,
        c: usize,
        p_in: &[f32],
        out: &mut [f64],
        scratch: &mut Vec<f32>,
    ) {
        out.iter_mut().for_each(|v| *v = 0.0);
        for fi in 0..self.frames.len() {
            let f = &self.frames[fi];
            scratch.resize(f.w * f.h, 0.0);
            self.apply(fi, c, p_in, scratch);
            let wgt = f.inv_var[c.min(2)] as f64;
            self.adjoint_accum(fi, c, scratch, wgt, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Solver PCG cuadrático (F9.2) — EidrSolveMode::ScientificQuadratic
// ---------------------------------------------------------------------------

/// Configuración del solve cuadrático. La regularización es una cresta
/// UNIFORME λ = ridge_rel·mediana(diag>0) que ancla al piloto z0:
/// (AᵀΣ⁻¹A + λI) z = AᵀΣ⁻¹y + λ·z0. En los modos bien determinados
/// (diag ≫ λ) el sesgo es ≤ ridge_rel; en el espacio nulo (diag = 0, p.ej.
/// zonas sin cobertura) la solución pasa a ser el piloto — que el DQ marca
/// aparte como NO_COVERAGE. Nunca hay relleno inventado.
pub(crate) struct EidrSolveConfig {
    pub max_iterations: usize,
    /// Convergencia por residual: ‖r‖/‖b‖ < tol.
    pub tol: f64,
    /// Convergencia por estancamiento (§7.5): decremento del objetivo por
    /// iteración < step_tol × decremento acumulado durante 3 consecutivas.
    pub step_tol: f64,
    pub ridge_rel: f64,
}

impl Default for EidrSolveConfig {
    fn default() -> Self {
        Self {
            max_iterations: 60,
            tol: 1e-6,
            step_tol: 1e-4,
            ridge_rel: 1e-3,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EidrSolveReport {
    pub iterations: usize,
    pub rel_residual: f64,
    pub converged: bool,
    /// λ efectiva empleada (unidades de la diagonal normal).
    pub ridge: f64,
}

/// PCG con precondicionador Jacobi sobre las ecuaciones normales de UN canal.
/// `b` = AᵀΣ⁻¹y; `diag` = diagonal de AᵀΣ⁻¹A; `z0` = piloto (warm start).
/// Buffers f64 (rigor del solver); el resultado vuelve en f32.
pub(crate) fn eidr_solve_channel(
    op: &EidrOperator,
    c: usize,
    b: &[f64],
    diag: &[f64],
    z0: &[f32],
    cfg: &EidrSolveConfig,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<(Vec<f32>, EidrSolveReport), String> {
    let n = op.w_out * op.h_out;
    debug_assert_eq!(b.len(), n);
    debug_assert_eq!(diag.len(), n);
    debug_assert_eq!(z0.len(), n);

    // λ = ridge_rel · mediana de la diagonal positiva.
    let mut pos: Vec<f64> = diag.iter().copied().filter(|&d| d > 0.0).collect();
    if pos.is_empty() {
        return Err("EIDR: ningún píxel de salida tiene cobertura".into());
    }
    let mid = pos.len() / 2;
    pos.select_nth_unstable_by(mid, |a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let ridge = (cfg.ridge_rel * pos[mid]).max(1e-30);
    drop(pos);

    // b' = b + λ z0; M = diag + λ.
    let mut z: Vec<f64> = z0.iter().map(|&v| v as f64).collect();
    let bp: Vec<f64> = b
        .iter()
        .zip(z.iter())
        .map(|(&bv, &zv)| bv + ridge * zv)
        .collect();
    let norm_b = bp.iter().map(|v| v * v).sum::<f64>().sqrt().max(1e-30);

    let mut scratch_f32 = Vec::new();
    let mut pf32 = vec![0.0f32; n];
    let mut ap = vec![0.0f64; n];

    // r = b' − (N + λ)z.
    for (dst, &src) in pf32.iter_mut().zip(z.iter()) {
        *dst = src as f32;
    }
    op.normal_apply(c, &pf32, &mut ap, &mut scratch_f32);
    let mut r: Vec<f64> = (0..n).map(|i| bp[i] - ap[i] - ridge * z[i]).collect();
    let mut d: Vec<f64> = (0..n).map(|i| r[i] / (diag[i] + ridge)).collect();
    let mut p = d.clone();
    let mut rho: f64 = r.iter().zip(d.iter()).map(|(&a, &b)| a * b).sum();

    let mut iterations = 0usize;
    let mut converged = false;
    let mut stagnant = 0u32;
    let mut objective_drop = 0.0f64;
    for k in 0..cfg.max_iterations {
        if let Some(cn) = cancel {
            if cn.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("Cancelado".into());
            }
        }
        iterations = k + 1;
        for (dst, &src) in pf32.iter_mut().zip(p.iter()) {
            *dst = src as f32;
        }
        op.normal_apply(c, &pf32, &mut ap, &mut scratch_f32);
        for i in 0..n {
            ap[i] += ridge * p[i];
        }
        let pap: f64 = p.iter().zip(ap.iter()).map(|(&a, &b)| a * b).sum();
        if pap <= 0.0 || !pap.is_finite() {
            break; // dirección degenerada: el estado actual es lo mejor honesto
        }
        let alpha = rho / pap;
        for i in 0..n {
            z[i] += alpha * p[i];
            r[i] -= alpha * ap[i];
        }
        let rel_res = r.iter().map(|v| v * v).sum::<f64>().sqrt() / norm_b;
        progress(k + 1, cfg.max_iterations);
        if rel_res < cfg.tol {
            converged = true;
            break;
        }
        // Estancamiento del OBJETIVO (§7.5): en PCG el decremento de
        // φ = ½zᵀ(N+λ)z − b′ᵀz por iteración es ½αρ; cuando cae por debajo
        // de step_tol × el decremento acumulado durante 3 iteraciones, la
        // solución ya no cambia de forma relevante.
        let dec = 0.5 * alpha * rho;
        objective_drop += dec.max(0.0);
        if dec.abs() < cfg.step_tol * objective_drop.max(1e-300) {
            stagnant += 1;
            if stagnant >= 3 {
                converged = true;
                break;
            }
        } else {
            stagnant = 0;
        }
        for i in 0..n {
            d[i] = r[i] / (diag[i] + ridge);
        }
        let rho_new: f64 = r.iter().zip(d.iter()).map(|(&a, &b)| a * b).sum();
        let beta = rho_new / rho.max(1e-300);
        rho = rho_new;
        for i in 0..n {
            p[i] = d[i] + beta * p[i];
        }
    }
    let rel_residual = r.iter().map(|v| v * v).sum::<f64>().sqrt() / norm_b;
    Ok((
        z.iter().map(|&v| v as f32).collect(),
        EidrSolveReport {
            iterations,
            rel_residual,
            converged,
            ridge,
        },
    ))
}

/// Piloto de arranque: retroproyección normalizada b/diag (coadición
/// ponderada por K² — el análogo drizzle del operador). Donde no hay
/// cobertura queda 0 (el DQ lo marcará NO_COVERAGE).
pub(crate) fn eidr_pilot(b: &[f64], diag: &[f64]) -> Vec<f32> {
    b.iter()
        .zip(diag.iter())
        .map(|(&bv, &dv)| if dv > 0.0 { (bv / dv) as f32 } else { 0.0 })
        .collect()
}

/// Prolongación bilineal coarse→fine para multigrid (1x → escala final).
/// Ambos grids comparten la convención salida(q) ↔ ref(q/s): la razón de
/// muestreo es w1/w2 exacta.
pub(crate) fn eidr_prolong(
    z: &[f32],
    w1: usize,
    h1: usize,
    w2: usize,
    h2: usize,
) -> Vec<f32> {
    let rx = w1 as f64 / w2 as f64;
    let ry = h1 as f64 / h2 as f64;
    let mut out = vec![0.0f32; w2 * h2];
    for y2 in 0..h2 {
        let sy = (y2 as f64 * ry).min((h1 - 1) as f64);
        let y0 = sy as usize;
        let y1 = (y0 + 1).min(h1 - 1);
        let fy = (sy - y0 as f64) as f32;
        for x2 in 0..w2 {
            let sx = (x2 as f64 * rx).min((w1 - 1) as f64);
            let x0 = sx as usize;
            let x1 = (x0 + 1).min(w1 - 1);
            let fx = (sx - x0 as f64) as f32;
            out[y2 * w2 + x2] = z[y0 * w1 + x0] * (1.0 - fx) * (1.0 - fy)
                + z[y0 * w1 + x1] * fx * (1.0 - fy)
                + z[y1 * w1 + x0] * (1.0 - fx) * fy
                + z[y1 * w1 + x1] * fx * fy;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests — F9.1: identidad adjunta, conservación de flujo, CFA
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(state: &mut u64) -> f64 {
        *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*state >> 33) as f64) / (u32::MAX as f64 + 1.0) // [0,1)
    }

    /// Similitud frame→ref: rotación θ, escala k, traslación (tx,ty).
    fn sim_transform(theta: f32, k: f32, tx: f32, ty: f32) -> crate::DsTransform {
        let (s, c) = theta.sin_cos();
        crate::DsTransform::from_similarity((c * k, s * k, tx, ty))
    }

    fn psf(fwhm: f32, beta: f32) -> MoffatPsf {
        MoffatPsf {
            fwhm_x: fwhm,
            fwhm_y: fwhm * 0.9,
            theta: 0.35,
            beta,
        }
    }

    fn build_op(scale: f32, cfa: Option<i32>, fw: usize, fh: usize) -> EidrOperator {
        let gamma = MoffatPsf {
            fwhm_x: 2.4,
            fwhm_y: 2.4,
            theta: 0.0,
            beta: 2.6,
        };
        let trs = [
            sim_transform(0.0, 1.0, 0.0, 0.0),
            sim_transform(0.0, 1.0, 0.37, -0.61),
            sim_transform(0.17, 1.01, 1.3, 0.8),
        ];
        let psfs = [Some(psf(2.2, 2.4)), Some(psf(2.6, 2.8)), None];
        let w_out = (fw as f32 * scale).round() as usize;
        let h_out = (fh as f32 * scale).round() as usize;
        let frames = trs
            .iter()
            .zip(psfs.iter())
            .map(|(t, p)| {
                let geom = eidr_geom(t, scale).expect("similitud ⇒ afín");
                let lut = eidr_build_lut(*p, gamma, &geom);
                EidrFrameOp {
                    geom,
                    lut,
                    inv_var: [1.0, 0.8, 1.2],
                    mask: vec![0u64; (fw * fh + 63) / 64],
                    w: fw,
                    h: fh,
                }
            })
            .collect();
        EidrOperator {
            frames,
            w_out,
            h_out,
            cfa,
            ch: if cfa.is_some() { 3 } else { 1 },
        }
    }

    /// ⟨Az, y⟩ = ⟨z, Aᵀy⟩ con acumulación f64: §7.7 exige rel < 1e-5.
    #[test]
    fn eidr_adjoint_identity() {
        for (cfa, c) in [(None, 0usize), (Some(8), 0), (Some(8), 1), (Some(9), 2)] {
            let op = build_op(2.0, cfa, 26, 22);
            let mut st = 0x9e3779b97f4a7c15u64;
            let z: Vec<f32> = (0..op.w_out * op.h_out)
                .map(|_| (lcg(&mut st) * 2.0 - 1.0) as f32)
                .collect();
            for fi in 0..op.frames.len() {
                let f = &op.frames[fi];
                let y: Vec<f32> = (0..f.w * f.h)
                    .map(|_| (lcg(&mut st) * 2.0 - 1.0) as f32)
                    .collect();
                let mut az = vec![0.0f32; f.w * f.h];
                op.apply(fi, c, &z, &mut az);
                let mut aty = vec![0.0f64; op.w_out * op.h_out];
                op.adjoint_accum(fi, c, &y, 1.0, &mut aty);
                let lhs: f64 = az
                    .iter()
                    .zip(y.iter())
                    .map(|(&a, &b)| a as f64 * b as f64)
                    .sum();
                let rhs: f64 = aty
                    .iter()
                    .zip(z.iter())
                    .map(|(&a, &b)| a * b as f64)
                    .sum();
                let denom = lhs.abs().max(rhs.abs()).max(1e-12);
                let rel = (lhs - rhs).abs() / denom;
                assert!(
                    rel < 1e-5,
                    "adjunto cfa={cfa:?} c={c} frame={fi}: lhs={lhs:.9} rhs={rhs:.9} rel={rel:.2e}"
                );
            }
        }
    }

    /// La máscara elimina la fila del sistema en AMBAS direcciones: la
    /// identidad adjunta debe sostenerse también con píxeles inválidos.
    #[test]
    fn eidr_adjoint_identity_with_mask() {
        let mut op = build_op(1.5, None, 30, 24);
        let f = &mut op.frames[1];
        let mut st = 0xabcdef12345u64;
        for p in 0..f.w * f.h {
            if lcg(&mut st) < 0.15 {
                f.mask[p >> 6] |= 1u64 << (p & 63);
            }
        }
        let z: Vec<f32> = (0..op.w_out * op.h_out)
            .map(|_| (lcg(&mut st) * 2.0 - 1.0) as f32)
            .collect();
        let f = &op.frames[1];
        let y: Vec<f32> = (0..f.w * f.h)
            .map(|_| (lcg(&mut st) * 2.0 - 1.0) as f32)
            .collect();
        let mut az = vec![0.0f32; f.w * f.h];
        op.apply(1, 0, &z, &mut az);
        let mut aty = vec![0.0f64; op.w_out * op.h_out];
        op.adjoint_accum(1, 0, &y, 1.0, &mut aty);
        let lhs: f64 = az.iter().zip(y.iter()).map(|(&a, &b)| a as f64 * b as f64).sum();
        let rhs: f64 = aty.iter().zip(z.iter()).map(|(&a, &b)| a * b as f64).sum();
        let rel = (lhs - rhs).abs() / lhs.abs().max(rhs.abs()).max(1e-12);
        assert!(rel < 1e-5, "adjunto con máscara: rel={rel:.2e}");
    }

    /// Fondo plano V ⇒ predicción V donde la ventana de depósito cae íntegra
    /// dentro del lienzo (las celdas teselan ⇒ Σ_q K = 1): transferencia de
    /// unidad y conservación de flujo. Los píxeles del frame que asoman fuera
    /// del lienzo tienen cobertura parcial REAL y quedan excluidos (el modelo
    /// directo los describe exactamente; no son un sesgo).
    #[test]
    fn eidr_flat_field_unit_transfer() {
        let op = build_op(2.0, None, 40, 34);
        let z = vec![500.0f32; op.w_out * op.h_out];
        for fi in 0..op.frames.len() {
            let f = &op.frames[fi];
            let mut pred = vec![0.0f32; f.w * f.h];
            op.apply(fi, 0, &z, &mut pred);
            let q_rad = f.lut.radius as f64 * f.geom.q_rad_factor + 2.0;
            let mut worst = 0.0f32;
            let mut checked = 0usize;
            for py in 0..f.h {
                for px in 0..f.w {
                    let (gx, gy) = f.geom.g(px as f64, py as f64);
                    if gx - q_rad < 0.0
                        || gy - q_rad < 0.0
                        || gx + q_rad > (op.w_out - 1) as f64
                        || gy + q_rad > (op.h_out - 1) as f64
                    {
                        continue;
                    }
                    checked += 1;
                    let err = (pred[py * f.w + px] - 500.0).abs() / 500.0;
                    worst = worst.max(err);
                }
            }
            assert!(checked > 200, "frame {fi}: interior insuficiente ({checked})");
            assert!(
                worst < 1.5e-3,
                "frame {fi}: transferencia plana err máx {worst:.2e} ({checked} px)"
            );
        }
    }

    /// normal_diag coincide con la fuerza bruta ⟨A e_q, Σ⁻¹ A e_q⟩.
    #[test]
    fn eidr_normal_diag_matches_bruteforce() {
        let op = build_op(1.5, None, 22, 18);
        let n_out = op.w_out * op.h_out;
        let mut diag = vec![0.0f64; n_out];
        for fi in 0..op.frames.len() {
            op.normal_diag_accum(fi, 0, &mut diag);
        }
        let mut st = 0x5a5a5a5au64;
        for _ in 0..6 {
            let q = (lcg(&mut st) * n_out as f64) as usize;
            let mut e = vec![0.0f32; n_out];
            e[q] = 1.0;
            let mut brute = 0.0f64;
            for fi in 0..op.frames.len() {
                let f = &op.frames[fi];
                let mut pred = vec![0.0f32; f.w * f.h];
                op.apply(fi, 0, &e, &mut pred);
                let iv = f.inv_var[0] as f64;
                brute += pred.iter().map(|&v| v as f64 * v as f64).sum::<f64>() * iv;
            }
            let rel = (diag[q] - brute).abs() / brute.abs().max(1e-12);
            assert!(rel < 1e-4, "diag[{q}]={} brute={} rel={rel:.2e}", diag[q], brute);
        }
    }

    /// CFA: la retícula del canal selecciona exactamente los fotositos de
    /// ds_cfa_channel y el flujo plano se conserva también por canal.
    #[test]
    fn eidr_cfa_lattice_and_flat() {
        let op = build_op(2.0, Some(10), 40, 34);
        let z = vec![300.0f32; op.w_out * op.h_out];
        for c in 0..3usize {
            let f = &op.frames[0];
            let mut pred = vec![0.0f32; f.w * f.h];
            op.apply(0, c, &z, &mut pred);
            let margin = (f.lut.radius * f.geom.q_rad_factor as f32).ceil() as usize + 2;
            for py in margin..f.h - margin {
                for px in margin..f.w - margin {
                    let v = pred[py * f.w + px];
                    if crate::ds_cfa_channel(10, px, py) == c {
                        assert!(
                            (v - 300.0).abs() / 300.0 < 2.5e-3,
                            "c={c} ({px},{py}): {v}"
                        );
                    } else {
                        assert_eq!(v, 0.0, "fuera de retícula c={c} ({px},{py})");
                    }
                }
            }
        }
    }

    /// La Γ objetivo es el p80 de las FWHM medias y β la mediana.
    #[test]
    fn eidr_target_psf_percentile() {
        let mk = |f: f32| Some(MoffatPsf { fwhm_x: f, fwhm_y: f, theta: 0.0, beta: 2.0 + f * 0.1 });
        let psfs: Vec<Option<MoffatPsf>> =
            (0..10).map(|i| mk(1.0 + i as f32 * 0.2)).chain([None]).collect();
        let g = eidr_target_psf(&psfs).unwrap();
        assert!((g.fwhm_x - 2.4).abs() < 0.11, "p80 = {}", g.fwhm_x);
        assert_eq!(g.fwhm_x, g.fwhm_y);
        assert!(eidr_target_psf(&[None, None]).is_none());
    }

    /// La prolongación bilineal conserva campos planos y rampas.
    #[test]
    fn eidr_prolong_flat_and_ramp() {
        let (w1, h1) = (10usize, 8usize);
        let flat = vec![7.5f32; w1 * h1];
        let up = eidr_prolong(&flat, w1, h1, 20, 16);
        assert!(up.iter().all(|&v| (v - 7.5).abs() < 1e-6));
        let ramp: Vec<f32> = (0..w1 * h1).map(|i| (i % w1) as f32).collect();
        let up = eidr_prolong(&ramp, w1, h1, 20, 16);
        // salida(x2) ↔ coarse(x2/2): la rampa se conserva a mitad de paso.
        for y2 in 0..16 {
            for x2 in 0..18 {
                let expect = (x2 as f32) * 0.5;
                assert!(
                    (up[y2 * 20 + x2] - expect).abs() < 1e-5,
                    "({x2},{y2}): {} vs {expect}",
                    up[y2 * 20 + x2]
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Gate F9.2 — solve cuadrático end-to-end con verdad del simulador
    // -----------------------------------------------------------------------

    /// Criterios §7.10: sesgo de flujo <0.5% (estrellas SNR alto) y sesgo
    /// centroidal <0.02 px nativos, resolviendo a 2x un campo SUBMUESTREADO
    /// (FWHM 1.3 px, Moffat β=2.5) con 16 frames y dithers subpíxel diversos.
    /// Modo geometría pura (B=δ): z queda a la PSF común de los frames y la
    /// verdad en apertura es analítica (Moffat integrada).
    #[test]
    fn gate_f92_eidr_quadratic_flux_centroid() {
        use crate::deepsky_sim as sim;
        let (w, h) = (96usize, 80usize);
        let flux = 60000.0f64;
        let fwhm = 1.3f64;
        let beta = 2.5f64;
        let mut stars = Vec::new();
        let mut truth = Vec::new();
        for j in 0..3 {
            for i in 0..3 {
                let x = 20.37 + 27.83 * i as f64;
                let y = 16.21 + 23.9 * j as f64;
                stars.push(sim::SimStar {
                    x,
                    y,
                    flux_adu: flux,
                    fwhm_px: fwhm,
                    moffat_beta: Some(beta),
                });
                truth.push((x, y));
            }
        }
        let scene = sim::SimScene {
            width: w,
            height: h,
            background_adu: 200.0,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0; 3],
            stars,
        };
        let sensor = sim::SimSensor {
            gain_e_per_adu: 1.0,
            read_noise_e: 2.0,
            bias_adu: 500.0,
            dark_adu_per_s: 0.0,
            full_well_adu: 1e12,
            hot_pixels: vec![],
            bayer: None,
            vignette: None,
        };
        let scale = 2.0f32;
        let n_frames = 16usize;
        let mut datas: Vec<Vec<f32>> = Vec::new();
        let mut trs: Vec<crate::DsTransform> = Vec::new();
        for i in 0..n_frames {
            let dx = (i as f64 * 0.618033988749895).fract() + (i % 3) as f64 - 1.0;
            let dy = (i as f64 * 0.754877666246693).fract() + ((i / 3) % 3) as f64 - 1.0;
            let exp = sim::SimExposure {
                exposure_s: 1.0,
                dx,
                dy,
                seed: 4200 + i as u64,
            };
            let (data, _var) = sim::render_light(&scene, &sensor, &exp);
            // Calibración trivial: solo pedestal (sin dark/flat en la escena).
            datas.push(data.iter().map(|&v| v - 500.0).collect());
            // La escena se desplaza +d en el frame ⇒ ref = frame − d.
            trs.push(crate::DsTransform::from_similarity((
                1.0,
                0.0,
                -dx as f32,
                -dy as f32,
            )));
        }
        let w_out = (w as f32 * scale).round() as usize;
        let h_out = (h as f32 * scale).round() as usize;
        let gamma_nominal = MoffatPsf {
            fwhm_x: fwhm as f32,
            fwhm_y: fwhm as f32,
            theta: 0.0,
            beta: beta as f32,
        };
        let ivar = 1.0f32 / 204.0; // fondo 200 + lectura 2e (gain 1)
        let frames_op: Vec<EidrFrameOp> = trs
            .iter()
            .map(|t| {
                let geom = eidr_geom(t, scale).expect("similitud");
                let lut = eidr_build_lut(None, gamma_nominal, &geom);
                EidrFrameOp {
                    geom,
                    lut,
                    inv_var: [ivar; 3],
                    mask: vec![0u64; (w * h + 63) / 64],
                    w,
                    h,
                }
            })
            .collect();
        let op = EidrOperator {
            frames: frames_op,
            w_out,
            h_out,
            cfa: None,
            ch: 1,
        };
        let n_out = w_out * h_out;
        let mut b = vec![0.0f64; n_out];
        let mut diag = vec![0.0f64; n_out];
        for fi in 0..n_frames {
            op.adjoint_accum(fi, 0, &datas[fi], ivar as f64, &mut b);
            op.normal_diag_accum(fi, 0, &mut diag);
        }
        let z0 = eidr_pilot(&b, &diag);
        let mut noop = |_k: usize, _n: usize| {};
        let (z, rep) = eidr_solve_channel(
            &op,
            0,
            &b,
            &diag,
            &z0,
            &EidrSolveConfig::default(),
            None,
            &mut noop,
        )
        .expect("solve");
        assert!(
            rep.converged,
            "PCG sin converger: iters={} rel_res={:.2e}",
            rep.iterations, rep.rel_residual
        );

        // Verdad analítica en apertura: Moffat integrada hasta r_ap nativos.
        let alpha = fwhm / (2.0 * (2f64.powf(1.0 / beta) - 1.0).sqrt());
        let r_ap = 6.0f64; // px nativos
        let frac = 1.0 - (1.0 + (r_ap / alpha).powi(2)).powf(1.0 - beta);
        let expect = flux * frac;
        let s = scale as f64;
        for (k, &(sx, sy)) in truth.iter().enumerate() {
            let (cx, cy) = (sx * s, sy * s);
            let rq = r_ap * s;
            let (mut sum, mut mx, mut my) = (0.0f64, 0.0f64, 0.0f64);
            let x0 = (cx - rq).floor() as usize;
            let x1 = (cx + rq).ceil() as usize;
            let y0 = (cy - rq).floor() as usize;
            let y1 = (cy + rq).ceil() as usize;
            for qy in y0..=y1 {
                for qx in x0..=x1 {
                    let dx = qx as f64 - cx;
                    let dy = qy as f64 - cy;
                    if dx * dx + dy * dy > rq * rq {
                        continue;
                    }
                    let v = (z[qy * w_out + qx] - 200.0) as f64;
                    sum += v;
                    mx += v * qx as f64;
                    my += v * qy as f64;
                }
            }
            let flux_est = sum / (s * s);
            let rel = (flux_est - expect).abs() / expect;
            assert!(
                rel < 0.005,
                "estrella {k}: flujo {flux_est:.0} vs {expect:.0} ({:+.2}%)",
                100.0 * (flux_est / expect - 1.0)
            );
            let (ex, ey) = ((mx / sum - cx) / s, (my / sum - cy) / s);
            let cerr = (ex * ex + ey * ey).sqrt();
            assert!(
                cerr < 0.02,
                "estrella {k}: centroide desviado {cerr:.4} px nativos"
            );
        }
    }
}
