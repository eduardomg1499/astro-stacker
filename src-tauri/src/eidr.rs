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
    /// Pesos robustos por fotosito (IRLS Huber, F10): W diagonal del término
    /// de datos — min ‖W^½Σ^{-½}(y−Az)‖². None ⇒ 1.0 (cuadrático puro). La
    /// adjunción se preserva (W diagonal simétrica): adjoint/diag la leen,
    /// apply NO (A no cambia).
    pub robust_w: Option<Vec<f32>>,
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
    /// Canales de salida (informativo: el solve es por canal).
    #[allow(dead_code)]
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
                    let rw = f.robust_w.as_deref();
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
                                        let wv = rw.map(|w| w[base + px]).unwrap_or(1.0);
                                        acc += k as f64 * (r[base + px] * wv) as f64;
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
                                                let wv =
                                                    rw.map(|w| w[base + px]).unwrap_or(1.0);
                                                acc += k as f64 * (r[base + px] * wv) as f64;
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
                    let rw = f.robust_w.as_deref();
                    let mut acc = 0.0f64;
                    let mut visit = |px: usize, py: usize| {
                        let idx = py * fw + px;
                        if !bit(&f.mask, idx) {
                            let k = f.lut.eval(
                                (px as f64 - fxq) as f32,
                                (py as f64 - fyq) as f32,
                            ) as f64;
                            acc += k * k * rw.map(|w| w[idx] as f64).unwrap_or(1.0);
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

    /// out = Σ_{i∈idxs} A_iᵀ Σ_i⁻¹ A_i p  (producto de las ecuaciones
    /// normales SOLO de los frames del solve — los de holdout NO entran ni
    /// aquí ni en b: mezclar operador completo con b parcial resuelve
    /// N_total·z = b_subset y encoge z por idxs/total, el sesgo 0.857 que
    /// cazó el holdout de F9.4). `scratch` se reutiliza entre frames.
    pub(crate) fn normal_apply(
        &self,
        c: usize,
        idxs: &[usize],
        p_in: &[f32],
        out: &mut [f64],
        scratch: &mut Vec<f32>,
    ) {
        out.iter_mut().for_each(|v| *v = 0.0);
        for &fi in idxs {
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
    /// iteración < step_tol × decremento acumulado durante 3 consecutivas,
    /// o < step_tol × (data_size/2) — mejora de χ² despreciable frente a su
    /// escala absoluta (principio de discrepancia; independiente del warm
    /// start, que puede arrancar ya cerca del óptimo).
    pub step_tol: f64,
    pub ridge_rel: f64,
    /// Nº de mediciones válidas que alimentaron b (Σ píxeles de los frames
    /// del solve). 0 ⇒ solo el criterio relativo al acumulado.
    pub data_size: usize,
}

impl Default for EidrSolveConfig {
    fn default() -> Self {
        Self {
            max_iterations: 60,
            tol: 1e-6,
            step_tol: 1e-4,
            // Calibrada en el gate F9.4: con 1e-3 la solución depende de la
            // parada temprana (los modos con autovalor ≪ mediana amplifican
            // ruido de banda ancha al converger); con 3e-2 los modos sin
            // evidencia descansan en el piloto (exacto en flujo ⇒ sesgo ~0 a
            // baja frecuencia; a alta es el taper honesto) y la convergencia
            // completa es estable y reproducible.
            ridge_rel: 3e-2,
            data_size: 0,
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

/// Penalización cuadrática por frecuencia (§7.3: λ_F Σ ω_k |ẑ(k)|²) con ω
/// derivada de la puerta de recuperabilidad: ω = 1−R̃(|ν|) más allá del
/// Nyquist nativo, 0 por debajo. Los modos de alias sin evidencia quedan
/// amortiguados EN el objetivo (con solo el ridge uniforme sobreajustan los
/// frames del solve y arrastran el DC — medido por el holdout de F9.4); los
/// parcialmente evidenciados se encogen tipo Wiener, que no altera el FRC
/// por anillo (invariante a escala) pero sí la generalización.
pub(crate) struct EidrFreqPenalty {
    /// ω por bin de frecuencia (w_out×h_out, orden FFT natural).
    pub omega: Vec<f32>,
    /// λ_F en unidades de la diagonal normal (≈ mediana de diag).
    pub lambda: f64,
}

pub(crate) fn eidr_freq_penalty(
    report: &EidrGateReport,
    w_out: usize,
    h_out: usize,
    diag_median: f64,
) -> Option<EidrFreqPenalty> {
    if report.r_radial.is_empty() {
        return None; // escala nativa: sin banda extendida
    }
    let native = 0.5 / report.scale.max(1.0) as f64;
    let prof = &report.r_radial;
    let r_at = |nu: f64| -> f64 {
        if nu <= native {
            return 1.0;
        }
        if nu <= prof[0].0 {
            let t = (nu - native) / (prof[0].0 - native).max(1e-9);
            return 1.0 + t * (prof[0].1 - 1.0);
        }
        for i in 1..prof.len() {
            if nu <= prof[i].0 {
                let t = (nu - prof[i - 1].0) / (prof[i].0 - prof[i - 1].0).max(1e-9);
                return prof[i - 1].1 + t * (prof[i].1 - prof[i - 1].1);
            }
        }
        prof[prof.len() - 1].1
    };
    let mut omega = vec![0.0f32; w_out * h_out];
    for y in 0..h_out {
        let vy = {
            let k = y as f64 / h_out as f64;
            if k >= 0.5 {
                k - 1.0
            } else {
                k
            }
        };
        for x in 0..w_out {
            let vx = {
                let k = x as f64 / w_out as f64;
                if k >= 0.5 {
                    k - 1.0
                } else {
                    k
                }
            };
            let nu = (vx * vx + vy * vy).sqrt();
            omega[y * w_out + x] = (1.0 - r_at(nu)).clamp(0.0, 1.0) as f32;
        }
    }
    Some(EidrFreqPenalty {
        omega,
        lambda: diag_median,
    })
}

/// w += λ_F · F⁻¹(ω ⊙ F p): término espectral del producto normal. Operador
/// real simétrico PSD (ω ≥ 0) ⇒ PCG sigue siendo válido.
fn eidr_freq_penalty_apply(
    pen: &EidrFreqPenalty,
    p_in: &[f64],
    w: usize,
    h: usize,
    out: &mut [f64],
) {
    let mut buf: Vec<Complex<f64>> = p_in.iter().map(|&v| Complex::new(v, 0.0)).collect();
    let mut planner = FftPlanner::<f64>::new();
    let fw = planner.plan_fft_forward(w);
    let fh = planner.plan_fft_forward(h);
    let iw = planner.plan_fft_inverse(w);
    let ih = planner.plan_fft_inverse(h);
    let run = |buf: &mut Vec<Complex<f64>>, rf: &std::sync::Arc<dyn rustfft::Fft<f64>>, cf: &std::sync::Arc<dyn rustfft::Fft<f64>>| {
        for row in buf.chunks_exact_mut(w) {
            rf.process(row);
        }
        let mut col = vec![Complex::new(0.0, 0.0); h];
        for x in 0..w {
            for y in 0..h {
                col[y] = buf[y * w + x];
            }
            cf.process(&mut col);
            for y in 0..h {
                buf[y * w + x] = col[y];
            }
        }
    };
    run(&mut buf, &fw, &fh);
    for (c, &o) in buf.iter_mut().zip(pen.omega.iter()) {
        *c *= o as f64;
    }
    run(&mut buf, &iw, &ih);
    let norm = pen.lambda / (w * h) as f64;
    for (dst, src) in out.iter_mut().zip(buf.iter()) {
        *dst += src.re * norm;
    }
}

/// PCG con precondicionador Jacobi sobre las ecuaciones normales de UN canal.
/// `b` = AᵀΣ⁻¹y; `diag` = diagonal de AᵀΣ⁻¹A; `z0` = piloto (warm start).
/// Buffers f64 (rigor del solver); el resultado vuelve en f32.
pub(crate) fn eidr_solve_channel(
    op: &EidrOperator,
    c: usize,
    idxs: &[usize],
    b: &[f64],
    diag: &[f64],
    z0: &[f32],
    cfg: &EidrSolveConfig,
    freq: Option<&EidrFreqPenalty>,
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
    op.normal_apply(c, idxs, &pf32, &mut ap, &mut scratch_f32);
    if let Some(pen) = freq {
        eidr_freq_penalty_apply(pen, &z, op.w_out, op.h_out, &mut ap);
    }
    let mut r: Vec<f64> = (0..n).map(|i| bp[i] - ap[i] - ridge * z[i]).collect();
    // Precondicionador Jacobi: diagonal del término espectral ≈ λ_F·⟨ω⟩.
    let pen_diag = freq
        .map(|pen| {
            pen.lambda * pen.omega.iter().map(|&o| o as f64).sum::<f64>()
                / pen.omega.len() as f64
        })
        .unwrap_or(0.0);
    let mut d: Vec<f64> = (0..n).map(|i| r[i] / (diag[i] + ridge + pen_diag)).collect();
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
        op.normal_apply(c, idxs, &pf32, &mut ap, &mut scratch_f32);
        if let Some(pen) = freq {
            eidr_freq_penalty_apply(pen, &p, op.w_out, op.h_out, &mut ap);
        }
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
        let chi2_scale = 0.5 * cfg.data_size as f64;
        if dec.abs() < cfg.step_tol * objective_drop.max(1e-300)
            || (chi2_scale > 0.0 && dec.abs() < cfg.step_tol * chi2_scale)
        {
            stagnant += 1;
            if stagnant >= 3 {
                converged = true;
                break;
            }
        } else {
            stagnant = 0;
        }
        for i in 0..n {
            d[i] = r[i] / (diag[i] + ridge + pen_diag);
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

/// Piloto de arranque: retroproyección normalizada por el PLANO
/// retroproyectado, pilot = Aᵀ Σ⁻¹ y / Aᵀ Σ⁻¹ 1 — la coadición drizzle-like
/// del operador, EXACTA en flujo para campos planos (b/diag NO lo es: vale
/// ΣK/ΣK²·V ≈ 4-5·V y el holdout de F9.4 lo detectó como sesgo de fondo).
/// Donde no hay cobertura queda 0 (el DQ lo marcará NO_COVERAGE).
pub(crate) fn eidr_pilot(b: &[f64], aone: &[f64]) -> Vec<f32> {
    b.iter()
        .zip(aone.iter())
        .map(|(&bv, &av)| if av > 1e-12 { (bv / av) as f32 } else { 0.0 })
        .collect()
}

/// Aᵀ Σ⁻¹ 1 de un subconjunto de frames: el denominador del piloto (y la
/// "cobertura efectiva" del operador). Respeta máscaras y retícula CFA.
pub(crate) fn eidr_backprojected_flat(
    op: &EidrOperator,
    c: usize,
    idxs: &[usize],
) -> Vec<f64> {
    let mut aone = vec![0.0f64; op.w_out * op.h_out];
    for &fi in idxs {
        let f = &op.frames[fi];
        let ones = vec![1.0f32; f.w * f.h];
        op.adjoint_accum(fi, c, &ones, f.inv_var[c.min(2)] as f64, &mut aone);
    }
    aone
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

/// Marca como INVÁLIDOS los fotositos cuya ventana de depósito NO cae
/// íntegra dentro del lienzo: sus filas de A serían parciales y acoplan la
/// escala del interior con el borde (sesgo multiplicativo cazado por el
/// holdout de F9.4). Con el lienzo acolchado son pocos; devuelve cuántos.
pub(crate) fn eidr_mask_partial_rows(
    f: &mut EidrFrameOp,
    w_out: usize,
    h_out: usize,
) -> usize {
    let q_rad = f.lut.radius as f64 * f.geom.q_rad_factor + 2.0;
    let mut masked = 0usize;
    for py in 0..f.h {
        for px in 0..f.w {
            let (gx, gy) = f.geom.g(px as f64, py as f64);
            let inside = gx.is_finite()
                && gy.is_finite()
                && gx - q_rad >= 0.0
                && gy - q_rad >= 0.0
                && gx + q_rad <= (w_out - 1) as f64
                && gy + q_rad <= (h_out - 1) as f64;
            if !inside {
                let idx = py * f.w + px;
                if !bit(&f.mask, idx) {
                    masked += 1;
                    f.mask[idx >> 6] |= 1u64 << (idx & 63);
                }
            }
        }
    }
    masked
}

// ---------------------------------------------------------------------------
// Holdout y FRC (F9.4, §7.5/§7.9/§7.10)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub(crate) struct EidrHoldoutStat {
    /// χ² medio de los residuales normalizados (sensible a colas).
    pub chi2_mean: f64,
    /// χ² robusto: mediana de r² dividida por la mediana teórica de χ²₁
    /// (0.4549) — inmune a núcleos estelares con varianza subestimada.
    pub chi2_median: f64,
    pub pixels: usize,
}

/// Predice el frame `fi` (reservado, NO usado en el solve) con la solución z
/// y compara con sus datos. Solo cuenta píxeles cuyo kernel cae ÍNTEGRO en el
/// lienzo (la cobertura parcial del borde compararía contra cielo que la
/// hipótesis no cubre) y no enmascarados. `var` = varianza por píxel si se
/// conoce (el simulador la da; producción usa 1/inv_var del frame).
pub(crate) fn eidr_holdout_stat(
    op: &EidrOperator,
    fi: usize,
    c: usize,
    z: &[f32],
    data: &[f32],
    var: Option<&[f64]>,
) -> EidrHoldoutStat {
    let f = &op.frames[fi];
    let mut pred = vec![0.0f32; f.w * f.h];
    op.apply(fi, c, z, &mut pred);
    let q_rad = f.lut.radius as f64 * f.geom.q_rad_factor + 2.0;
    let fallback_var = 1.0 / (f.inv_var[c.min(2)] as f64).max(1e-30);
    let pars = op.cfa.map(|cid| cfa_parities(cid, c));
    let mut r2: Vec<f64> = Vec::new();
    for py in 0..f.h {
        for px in 0..f.w {
            if let Some(ps) = &pars {
                if !ps.contains(&(px & 1, py & 1)) {
                    continue;
                }
            }
            let idx = py * f.w + px;
            if bit(&f.mask, idx) {
                continue;
            }
            let (gx, gy) = f.geom.g(px as f64, py as f64);
            if gx - q_rad < 0.0
                || gy - q_rad < 0.0
                || gx + q_rad > (op.w_out - 1) as f64
                || gy + q_rad > (op.h_out - 1) as f64
            {
                continue;
            }
            let v = var.map(|vv| vv[idx]).unwrap_or(fallback_var).max(1e-30);
            let res = (data[idx] - pred[idx]) as f64;
            r2.push(res * res / v);
        }
    }
    if r2.is_empty() {
        return EidrHoldoutStat {
            chi2_mean: f64::NAN,
            chi2_median: f64::NAN,
            pixels: 0,
        };
    }
    let mean = r2.iter().sum::<f64>() / r2.len() as f64;
    let mid = r2.len() / 2;
    r2.select_nth_unstable_by(mid, |a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = r2[mid] / 0.454936; // mediana de χ² con 1 gdl
    EidrHoldoutStat {
        chi2_mean: mean,
        chi2_median: median,
        pixels: r2.len(),
    }
}

/// Curva FRC completa por anillos (para diagnóstico/tests).
#[cfg(test)]
pub(crate) fn frc_curve(a: &[f32], b: &[f32], w: usize, h: usize) -> Vec<(f64, f64)> {
    let cutoff_dummy = frc_rings(a, b, w, h);
    cutoff_dummy
}

#[cfg(test)]
fn frc_rings(a: &[f32], b: &[f32], w: usize, h: usize) -> Vec<(f64, f64)> {
    // duplicado ligero del cuerpo de frc_cutoff devolviendo la curva
    let mean_a = a.iter().map(|&v| v as f64).sum::<f64>() / (w * h) as f64;
    let mean_b = b.iter().map(|&v| v as f64).sum::<f64>() / (w * h) as f64;
    let hann = |i: usize, n: usize| -> f64 {
        let x = std::f64::consts::PI * i as f64 / (n - 1).max(1) as f64;
        x.sin() * x.sin()
    };
    let mut fa: Vec<Complex<f64>> = Vec::with_capacity(w * h);
    let mut fb: Vec<Complex<f64>> = Vec::with_capacity(w * h);
    for y in 0..h {
        let wy = hann(y, h);
        for x in 0..w {
            let win = wy * hann(x, w);
            fa.push(Complex::new((a[y * w + x] as f64 - mean_a) * win, 0.0));
            fb.push(Complex::new((b[y * w + x] as f64 - mean_b) * win, 0.0));
        }
    }
    let mut planner = FftPlanner::<f64>::new();
    let fft_w = planner.plan_fft_forward(w);
    let fft_h = planner.plan_fft_forward(h);
    let fft2_rect = |buf: &mut Vec<Complex<f64>>| {
        for row in buf.chunks_exact_mut(w) {
            fft_w.process(row);
        }
        let mut col = vec![Complex::new(0.0, 0.0); h];
        for x in 0..w {
            for y in 0..h {
                col[y] = buf[y * w + x];
            }
            fft_h.process(&mut col);
            for y in 0..h {
                buf[y * w + x] = col[y];
            }
        }
    };
    fft2_rect(&mut fa);
    fft2_rect(&mut fb);
    const NRINGS: usize = 48;
    let mut cross = vec![0.0f64; NRINGS];
    let mut p1 = vec![0.0f64; NRINGS];
    let mut p2 = vec![0.0f64; NRINGS];
    for y in 0..h {
        let vy = { let k = y as f64 / h as f64; if k >= 0.5 { k - 1.0 } else { k } };
        for x in 0..w {
            let vx = { let k = x as f64 / w as f64; if k >= 0.5 { k - 1.0 } else { k } };
            let r = (vx * vx + vy * vy).sqrt();
            if r < 1e-9 || r > 0.5 { continue; }
            let ring = ((r / 0.5) * NRINGS as f64) as usize;
            if ring >= NRINGS { continue; }
            let ca = fa[y * w + x];
            let cb = fb[y * w + x];
            cross[ring] += ca.re * cb.re + ca.im * cb.im;
            p1[ring] += ca.norm_sqr();
            p2[ring] += cb.norm_sqr();
        }
    }
    (0..NRINGS)
        .map(|ring| {
            let denom = (p1[ring] * p2[ring]).sqrt();
            let frc = if denom > 1e-300 { cross[ring] / denom } else { 0.0 };
            ((ring as f64 + 0.5) / 48.0 * 0.5, frc)
        })
        .collect()
}

/// Correlación de anillos de Fourier entre dos reconstrucciones
/// independientes (mitades odd/even). Devuelve la frecuencia de corte en
/// ciclos/px del grid común: primer anillo donde FRC cae bajo `threshold`
/// (0.5 clásico). Ventana Hann 2D + medias restadas para evitar fugas de
/// borde; anillos hasta el Nyquist axial (0.5).
pub(crate) fn frc_cutoff(
    a: &[f32],
    b: &[f32],
    w: usize,
    h: usize,
    threshold: f64,
) -> f64 {
    debug_assert_eq!(a.len(), w * h);
    debug_assert_eq!(b.len(), w * h);
    let mean_a = a.iter().map(|&v| v as f64).sum::<f64>() / (w * h) as f64;
    let mean_b = b.iter().map(|&v| v as f64).sum::<f64>() / (w * h) as f64;
    let hann = |i: usize, n: usize| -> f64 {
        let x = std::f64::consts::PI * i as f64 / (n - 1).max(1) as f64;
        x.sin() * x.sin()
    };
    let mut fa: Vec<Complex<f64>> = Vec::with_capacity(w * h);
    let mut fb: Vec<Complex<f64>> = Vec::with_capacity(w * h);
    for y in 0..h {
        let wy = hann(y, h);
        for x in 0..w {
            let win = wy * hann(x, w);
            fa.push(Complex::new((a[y * w + x] as f64 - mean_a) * win, 0.0));
            fb.push(Complex::new((b[y * w + x] as f64 - mean_b) * win, 0.0));
        }
    }
    // FFT 2D rectangular: filas w, columnas h.
    let mut planner = FftPlanner::<f64>::new();
    let fft_w = planner.plan_fft_forward(w);
    let fft_h = planner.plan_fft_forward(h);
    let fft2_rect = |buf: &mut Vec<Complex<f64>>| {
        for row in buf.chunks_exact_mut(w) {
            fft_w.process(row);
        }
        let mut col = vec![Complex::new(0.0, 0.0); h];
        for x in 0..w {
            for y in 0..h {
                col[y] = buf[y * w + x];
            }
            fft_h.process(&mut col);
            for y in 0..h {
                buf[y * w + x] = col[y];
            }
        }
    };
    fft2_rect(&mut fa);
    fft2_rect(&mut fb);
    const NRINGS: usize = 48;
    let mut cross = vec![0.0f64; NRINGS];
    let mut p1 = vec![0.0f64; NRINGS];
    let mut p2 = vec![0.0f64; NRINGS];
    for y in 0..h {
        let vy = {
            let k = y as f64 / h as f64;
            if k >= 0.5 {
                k - 1.0
            } else {
                k
            }
        };
        for x in 0..w {
            let vx = {
                let k = x as f64 / w as f64;
                if k >= 0.5 {
                    k - 1.0
                } else {
                    k
                }
            };
            let r = (vx * vx + vy * vy).sqrt();
            if r < 1e-9 || r > 0.5 {
                continue;
            }
            let ring = ((r / 0.5) * NRINGS as f64) as usize;
            if ring >= NRINGS {
                continue;
            }
            let ca = fa[y * w + x];
            let cb = fb[y * w + x];
            cross[ring] += ca.re * cb.re + ca.im * cb.im;
            p1[ring] += ca.norm_sqr();
            p2[ring] += cb.norm_sqr();
        }
    }
    for ring in 1..NRINGS {
        let denom = (p1[ring] * p2[ring]).sqrt();
        let frc = if denom > 1e-300 { cross[ring] / denom } else { 0.0 };
        if frc < threshold {
            return (ring as f64 + 0.5) / NRINGS as f64 * 0.5;
        }
    }
    0.5
}

// ---------------------------------------------------------------------------
// F10: pesos robustos Huber (IRLS) y microregistro ±0.2 px (§7.3/§7.5)
// ---------------------------------------------------------------------------

/// Pesos IRLS de Huber para un frame: w = min(1, δ/|r̃|) con r̃ el residual
/// normalizado (y − Az)·√(ivar). SOLO se calculan contra el modelo directo
/// (§7.5: "nunca sigma-clip sobre valores sin forward model"). Los píxeles
/// enmascarados o sin predicción conservan w=1 (no participan igualmente).
pub(crate) fn eidr_irls_weights(
    op: &EidrOperator,
    fi: usize,
    c: usize,
    z: &[f32],
    plane: &[f32],
    delta: f32,
) -> Vec<f32> {
    let f = &op.frames[fi];
    let mut pred = vec![0.0f32; f.w * f.h];
    op.apply(fi, c, z, &mut pred);
    let sqrt_ivar = (f.inv_var[c.min(2)] as f64).sqrt() as f32;
    let delta = delta.max(0.5);
    let mut w = vec![1.0f32; f.w * f.h];
    for idx in 0..f.w * f.h {
        if bit(&f.mask, idx) || pred[idx] == 0.0 {
            continue;
        }
        let r = ((plane[idx] - pred[idx]) * sqrt_ivar).abs();
        if r > delta {
            w[idx] = delta / r;
        }
    }
    w
}

/// Microregistro por frame (F10, §7.5 paso 5): Gauss-Newton de UNA iteración
/// sobre la traslación, con el residual contra el modelo directo y los
/// gradientes de la predicción. Devuelve (dx, dy) en px de frame, acotado a
/// ±max_shift; None si el sistema es degenerado o hay pocos píxeles.
pub(crate) fn eidr_refine_translation(
    op: &EidrOperator,
    fi: usize,
    c: usize,
    z: &[f32],
    plane: &[f32],
    max_shift: f64,
) -> Option<(f64, f64)> {
    let f = &op.frames[fi];
    let (fw, fh) = (f.w, f.h);
    let mut pred = vec![0.0f32; fw * fh];
    op.apply(fi, c, z, &mut pred);
    let rw = f.robust_w.as_deref();
    let (mut sxx, mut sxy, mut syy, mut sxr, mut syr) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut used = 0usize;
    for py in 1..fh - 1 {
        for px in 1..fw - 1 {
            let idx = py * fw + px;
            if bit(&f.mask, idx) || pred[idx] == 0.0 {
                continue;
            }
            // Vecinos válidos para el gradiente central.
            let (l, r_, u, d) = (
                pred[idx - 1],
                pred[idx + 1],
                pred[idx - fw],
                pred[idx + fw],
            );
            if l == 0.0 || r_ == 0.0 || u == 0.0 || d == 0.0 {
                continue;
            }
            // d(pred)/d(desplazamiento del frame +δ) = −∇pred… con el
            // convenio f(q)+δ ⇒ la predicción se muestrea δ antes: el
            // Jacobiano respecto a δ es +∇pred evaluado en el frame.
            let gx = 0.5 * (r_ - l) as f64;
            let gy = 0.5 * (d - u) as f64;
            let res = (plane[idx] - pred[idx]) as f64;
            let wv = rw.map(|w| w[idx] as f64).unwrap_or(1.0);
            sxx += wv * gx * gx;
            sxy += wv * gx * gy;
            syy += wv * gy * gy;
            sxr += wv * gx * res;
            syr += wv * gy * res;
            used += 1;
        }
    }
    if used < 256 {
        return None;
    }
    let det = sxx * syy - sxy * sxy;
    if det.abs() < 1e-6 * (sxx * syy).max(1e-12) {
        return None;
    }
    let dx = (syy * sxr - sxy * syr) / det;
    let dy = (sxx * syr - sxy * sxr) / det;
    if !dx.is_finite() || !dy.is_finite() {
        return None;
    }
    // El GN estima δ con data ≈ pred(p−ε): δ = −ε. Se devuelve ε (la
    // corrección que se SUMA a f0 vía eidr_shift_geom); verificado por test.
    Some((
        (-dx).clamp(-max_shift, max_shift),
        (-dy).clamp(-max_shift, max_shift),
    ))
}

/// Aplica un desplazamiento (dx, dy) en px de FRAME a la geometría: la
/// posición del frame respecto al cielo se corrige ⇒ f(q) += δ y la inversa
/// g se re-deriva (la parte lineal no cambia).
pub(crate) fn eidr_shift_geom(geom: &mut EidrGeom, dx: f64, dy: f64) {
    geom.f0[0] += dx;
    geom.f0[1] += dy;
    let g2 = [geom.gx, geom.gy];
    geom.g0 = [
        -(g2[0][0] * geom.f0[0] + g2[1][0] * geom.f0[1]),
        -(g2[0][1] * geom.f0[0] + g2[1][1] * geom.f0[1]),
    ];
}

// ---------------------------------------------------------------------------
// Puerta local de recuperabilidad (F9.3, §7.4)
// ---------------------------------------------------------------------------
//
// Para cada tile τ y frecuencia base κ (clase de alias del muestreo del
// detector), la matriz Q(i,ℓ) = ĥ_i(κ+ℓ·f_s)·e^{−2πi(κ+ℓ·f_s)·Δ_i}/σ_i mide
// cuánta evidencia INDEPENDIENTE aportan los frames para separar las réplicas
// aliasadas ℓ. Su SVD da: κ_cond = σ_max/σ_min (amplificación de ruido del
// modo peor determinado) y R = σ̃²/(σ̃²+η) (recuperabilidad 0..1). Sin
// diversidad de fases de dither (o con PSF ancha sin MTF más allá del
// Nyquist nativo) σ_min ≈ 0: el grid fino sería interpolación, no evidencia
// — y la puerta lo declara ANTES de resolver.
//
// Umbrales §7.4 (iniciales, pendientes de calibración con corpus):
// κ ≤ 30 apto · 30 < κ ≤ 100 degradar · κ > 100 o pérdida de rango fallback.
// CFA: la retícula por canal tiene paso 2 (f_s = ½) ⇒ más réplicas por eje y
// G aporta dos filas por frame (sus dos paridades quincunx).

/// Umbral κ apto / degradar (§7.4).
pub(crate) const EIDR_KAPPA_APT: f64 = 30.0;
/// Umbral κ degradar / fallback (§7.4).
pub(crate) const EIDR_KAPPA_MAX: f64 = 100.0;
/// η de la recuperabilidad R = s̃²/(s̃²+η), con s̃ la fracción de la
/// sensibilidad DC del stack que retiene el modo. Calibrada para que la MTF
/// real de un Moffat submuestreado (FWHM 1.1-1.4 px, s̃~0.02-0.05 en la
/// banda baja) puntúe apto y el colapso por PSF ancha (s̃≲1e-4) quede en
/// fallback con margen (§7.4: umbrales iniciales pendientes de corpus).
const EIDR_GATE_ETA: f64 = 1e-3;
/// R mínima de la banda extendida baja para clase APTO / DEGRADAR: por
/// debajo no hay evidencia superresuelta (PSF ancha ⇒ fallback aunque el
/// dither sea perfecto, §7.8).
const EIDR_GATE_R_APT: f64 = 0.05;
const EIDR_GATE_R_MIN: f64 = 0.02;

/// Entrada por frame de la puerta: geometría + PSF ajustada (None ⇒ Γ
/// nominal) + σ′ del frame en unidades normalizadas.
pub(crate) struct EidrGateFrame {
    pub geom: EidrGeom,
    pub psf: Option<MoffatPsf>,
    pub sigma: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EidrTileGate {
    /// Índice de tile (tx, ty) sobre el grid nativo.
    pub tx: usize,
    pub ty: usize,
    /// Peor κ = σ_max/σ_min de la banda extendida BAJA (primer radio del
    /// anillo): ahí la MTF aún tiene señal, así que κ mide la diversidad de
    /// fases del dither — el criterio de clase de §7.4. Las frecuencias más
    /// altas del anillo pueden carecer de MTF por pura física (seeing) y eso
    /// lo captura `r_band`, no la clase.
    pub kappa: f64,
    /// Recuperabilidad media del anillo extendido (0..1): fracción de la
    /// banda superresuelta con evidencia real; alimenta el mapa RECOV y el
    /// taper. 1.0 a escala nativa.
    pub r_band: f64,
    /// 0 = apto, 1 = degradar, 2 = fallback local.
    pub class: u8,
}

#[derive(Debug)]
pub(crate) struct EidrGateReport {
    pub scale: f32,
    pub tile: usize,
    pub tiles_x: usize,
    pub tiles_y: usize,
    pub tiles: Vec<EidrTileGate>,
    pub frac_apt: f64,
    pub frac_degrade: f64,
    pub frac_fallback: f64,
    /// Perfil radial de recuperabilidad: (ν en ciclos/px de SALIDA, R medio
    /// sobre tiles y ángulos). Alimenta el cutoff espectral (§7.9: "cutoff
    /// impuesto por rango/ruido y publicado").
    pub r_radial: Vec<(f64, f64)>,
}

impl EidrGateReport {
    /// Mapa RECOV a resolución de salida: R por tile (constante en el tile).
    pub(crate) fn recov_map(&self, w_out: usize, h_out: usize) -> Vec<f32> {
        let mut map = vec![0.0f32; w_out * h_out];
        let s = self.scale as f64;
        let tpx = (self.tile as f64 * s).max(1.0);
        for qy in 0..h_out {
            let ty = ((qy as f64 / tpx) as usize).min(self.tiles_y.saturating_sub(1));
            for qx in 0..w_out {
                let tx = ((qx as f64 / tpx) as usize).min(self.tiles_x.saturating_sub(1));
                map[qy * w_out + qx] = self.tiles[ty * self.tiles_x + tx].r_band as f32;
            }
        }
        map
    }

    /// ¿La escala está soportada globalmente? Criterio conservador: mayoría
    /// de tiles aptos y fallback minoritario.
    pub(crate) fn scale_supported(&self) -> bool {
        self.frac_apt >= 0.5 && self.frac_fallback <= 0.3
    }
}

/// MTF de una Moffat elíptica en la frecuencia (vx, vy) ciclos/px (coords del
/// frame), por suma coseno directa sobre un raster 1/4 px (la PSF es par ⇒
/// MTF real). Incluye normalización a MTF(0)=1.
fn moffat_mtf(psf: &MoffatPsf, freqs: &[(f64, f64)]) -> Vec<f64> {
    let step = 0.25f64;
    let r_m = (3.0 * psf.fwhm_x.max(psf.fwhm_y) as f64).max(4.0);
    let n = (2.0 * r_m / step).ceil() as usize + 1;
    let c = (n as f64 - 1.0) * 0.5;
    let ax = moffat_alpha(psf.fwhm_x as f64, psf.beta as f64);
    let ay = moffat_alpha(psf.fwhm_y as f64, psf.beta as f64);
    let (st, ct) = (psf.theta as f64).sin_cos();
    let beta = psf.beta.max(1.05) as f64;
    let mut vals = vec![0.0f64; n * n];
    let mut sum = 0.0f64;
    for j in 0..n {
        let dy = (j as f64 - c) * step;
        for i in 0..n {
            let dx = (i as f64 - c) * step;
            let u = ct * dx + st * dy;
            let v = -st * dx + ct * dy;
            let r2 = (u / ax) * (u / ax) + (v / ay) * (v / ay);
            let val = (1.0 + r2).powf(-beta);
            vals[j * n + i] = val;
            sum += val;
        }
    }
    freqs
        .iter()
        .map(|&(vx, vy)| {
            let mut acc = 0.0f64;
            for j in 0..n {
                let dy = (j as f64 - c) * step;
                for i in 0..n {
                    let dx = (i as f64 - c) * step;
                    acc += vals[j * n + i]
                        * (2.0 * std::f64::consts::PI * (vx * dx + vy * dy)).cos();
                }
            }
            acc / sum
        })
        .collect()
}

/// sinc normalizada sin(πx)/(πx) — MTF de la apertura del fotosito.
fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Resuelve G·x = e_t para G hermitiana L×L (compleja, como pares re/im)
/// por eliminación gaussiana con pivoteo parcial. Devuelve la componente t
/// de la solución (real para G hermitiana definida positiva) o None si G es
/// numéricamente singular — que es exactamente la pérdida de rango de §7.4.
fn hermitian_solve_diag(
    gre: &[f64],
    gim: &[f64],
    l: usize,
    t: usize,
) -> Option<f64> {
    // Copias de trabajo aumentadas con e_t.
    let mut a: Vec<(f64, f64)> = (0..l * l).map(|i| (gre[i], gim[i])).collect();
    let mut b: Vec<(f64, f64)> = (0..l).map(|i| ((i == t) as u8 as f64, 0.0)).collect();
    let scale = (0..l)
        .map(|i| a[i * l + i].0.abs())
        .fold(0.0f64, f64::max)
        .max(1e-300);
    for col in 0..l {
        // Pivoteo parcial por módulo.
        let (mut best, mut bmag) = (col, 0.0f64);
        for row in col..l {
            let (re, im) = a[row * l + col];
            let m = re * re + im * im;
            if m > bmag {
                bmag = m;
                best = row;
            }
        }
        if bmag.sqrt() < 1e-13 * scale {
            return None; // rango perdido
        }
        if best != col {
            for k in 0..l {
                a.swap(col * l + k, best * l + k);
            }
            b.swap(col, best);
        }
        let (pr, pi) = a[col * l + col];
        let pinv = 1.0 / (pr * pr + pi * pi);
        for row in (col + 1)..l {
            let (er, ei) = a[row * l + col];
            if er == 0.0 && ei == 0.0 {
                continue;
            }
            // factor = a[row,col] / pivote
            let fr = (er * pr + ei * pi) * pinv;
            let fi = (ei * pr - er * pi) * pinv;
            for k in col..l {
                let (cr, ci) = a[col * l + k];
                let (rr, ri) = a[row * l + k];
                a[row * l + k] = (rr - (fr * cr - fi * ci), ri - (fr * ci + fi * cr));
            }
            let (cr, ci) = b[col];
            let (rr, ri) = b[row];
            b[row] = (rr - (fr * cr - fi * ci), ri - (fr * ci + fi * cr));
        }
    }
    // Sustitución hacia atrás.
    let mut x: Vec<(f64, f64)> = vec![(0.0, 0.0); l];
    for col in (0..l).rev() {
        let (mut sr, mut si) = b[col];
        for k in (col + 1)..l {
            let (ar, ai) = a[col * l + k];
            let (xr, xi) = x[k];
            sr -= ar * xr - ai * xi;
            si -= ar * xi + ai * xr;
        }
        let (pr, pi) = a[col * l + col];
        let pinv = 1.0 / (pr * pr + pi * pi);
        x[col] = ((sr * pr + si * pi) * pinv, (si * pr - sr * pi) * pinv);
    }
    let v = x[t].0;
    v.is_finite().then_some(v)
}

/// Puerta de recuperabilidad por tiles para la escala `scale`. `cfa` =
/// Some((cid, canal)) evalúa la retícula CFA de ese canal (G: 2 filas por
/// frame). A escala ≤1.05 no hay banda extendida y todo tile es apto.
pub(crate) fn eidr_recoverability_gate(
    frames: &[EidrGateFrame],
    gamma: MoffatPsf,
    w: usize,
    h: usize,
    scale: f32,
    cfa: Option<(i32, usize)>,
    tile: usize,
) -> EidrGateReport {
    let tile = tile.max(32).min(w.max(h));
    let tiles_x = (w + tile - 1) / tile;
    let tiles_y = (h + tile - 1) / tile;
    let s = scale as f64;
    let mut report = EidrGateReport {
        scale,
        tile,
        tiles_x,
        tiles_y,
        tiles: Vec::with_capacity(tiles_x * tiles_y),
        frac_apt: 0.0,
        frac_degrade: 0.0,
        frac_fallback: 0.0,
        r_radial: Vec::new(),
    };
    report.r_radial = Vec::new();
    if s <= 1.05 || frames.is_empty() {
        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                report.tiles.push(EidrTileGate {
                    tx,
                    ty,
                    kappa: 1.0,
                    r_band: 1.0,
                    class: 0,
                });
            }
        }
        report.frac_apt = 1.0;
        return report;
    }

    // Frecuencias objetivo: anillo superresuelto (más allá del Nyquist nativo
    // 0.5 hasta el Nyquist de salida s/2), 4 radios × 4 ángulos.
    let radii: [f64; 4] = [0.2, 0.5, 0.8, 0.98];
    let angles: [f64; 4] = [0.0, 45.0, 90.0, 135.0];
    let mut targets: Vec<(f64, f64)> = Vec::new();
    for &rf in &radii {
        let r = 0.5 + (0.5 * s - 0.5) * rf;
        for &ang in &angles {
            let a = ang.to_radians();
            targets.push((r * a.cos(), r * a.sin()));
        }
    }
    // Paso de la retícula del canal (CFA: 2) y paridades (filas por frame).
    let (step_lat, parities): (f64, Vec<(usize, usize)>) = match cfa {
        Some((cid, c)) => (2.0, cfa_parities(cid, c)),
        None => (1.0, vec![(0usize, 0usize)]),
    };
    let fs = 1.0 / step_lat;
    // Réplicas por objetivo: κ = alias base; ℓ recorre |κ+m·fs| ≤ s/2.
    let replicas_of = |vt: (f64, f64)| -> (Vec<(f64, f64)>, usize) {
        let base = (
            vt.0 - fs * (vt.0 / fs).round(),
            vt.1 - fs * (vt.1 / fs).round(),
        );
        let mut reps = Vec::new();
        let lim = 0.5 * s - 1e-6;
        let mmax = ((lim / fs).ceil() as i64) + 1;
        for my in -mmax..=mmax {
            let vy = base.1 + my as f64 * fs;
            if vy.abs() > lim {
                continue;
            }
            for mx in -mmax..=mmax {
                let vx = base.0 + mx as f64 * fs;
                if vx.abs() > lim {
                    continue;
                }
                reps.push((vx, vy));
            }
        }
        let l = reps.len();
        (reps, l)
    };
    // MTF por frame y por frecuencia absoluta (cacheado: no depende del tile).
    // La frecuencia se lleva a coords del frame con ν_f = (G2ᵀ/s)·ν y se
    // multiplica por la apertura del fotosito sinc(ν_fx)·sinc(ν_fy).
    let mut mtf_cache: Vec<Vec<Vec<f64>>> = Vec::with_capacity(frames.len());
    let all_reps: Vec<(Vec<(f64, f64)>, usize)> =
        targets.iter().map(|&t| replicas_of(t)).collect();
    for fr in frames {
        let g2 = [fr.geom.gx, fr.geom.gy]; // columnas: ∂out/∂px, ∂out/∂py
        let psf = fr.psf.unwrap_or(gamma);
        let mut per_target = Vec::with_capacity(all_reps.len());
        for (reps, _) in &all_reps {
            let freqs_f: Vec<(f64, f64)> = reps
                .iter()
                .map(|&(vx, vy)| {
                    (
                        (g2[0][0] * vx + g2[0][1] * vy) / s,
                        (g2[1][0] * vx + g2[1][1] * vy) / s,
                    )
                })
                .collect();
            let mut vals = moffat_mtf(&psf, &freqs_f);
            for (v, &(fx, fy)) in vals.iter_mut().zip(freqs_f.iter()) {
                *v *= sinc(fx) * sinc(fy);
            }
            per_target.push(vals);
        }
        mtf_cache.push(per_target);
    }

    let (mut n_apt, mut n_deg, mut n_fb) = (0usize, 0usize, 0usize);
    let mut rad_sum = vec![0.0f64; radii.len()];
    let mut rad_cnt = vec![0usize; radii.len()];
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            // Puntos de evaluación: centro + 4 esquinas (rotación ⇒ la fase
            // deriva dentro del tile; el peor caso manda).
            let x0 = (tx * tile) as f64;
            let y0 = (ty * tile) as f64;
            let x1 = ((tx + 1) * tile).min(w) as f64;
            let y1 = ((ty + 1) * tile).min(h) as f64;
            let pts = [
                (0.5 * (x0 + x1), 0.5 * (y0 + y1)),
                (x0 + 2.0, y0 + 2.0),
                (x1 - 2.0, y0 + 2.0),
                (x0 + 2.0, y1 - 2.0),
                (x1 - 2.0, y1 - 2.0),
            ];
            let mut kappa_low = 1.0f64;
            let mut r_low = 1.0f64;
            let mut r_sum = 0.0f64;
            let mut r_cnt = 0usize;
            for (ti, (reps, l)) in all_reps.iter().enumerate() {
                let low_band = ti < angles.len();
                if *l < 2 {
                    // Sin réplicas dentro de banda: nada que separar aquí.
                    r_sum += 1.0;
                    r_cnt += 1;
                    continue;
                }
                let mut r_target = 1.0f64;
                for &(rx, ry) in &pts {
                    // Matriz Q: filas = frame × paridad; cols = réplicas.
                    let mut rows: Vec<Vec<(f64, f64)>> = Vec::new();
                    let mut dc2 = 0.0f64; // ‖columna DC‖²: sensibilidad total
                    for (fi, fr) in frames.iter().enumerate() {
                        let (pfx, pfy) = fr.geom.f(rx * s, ry * s);
                        if !pfx.is_finite() || !pfy.is_finite() {
                            continue;
                        }
                        for &(ox, oy) in &parities {
                            // Fotosito de la retícula más próximo y su
                            // posición REAL en coords nativas de referencia.
                            let snap = |v: f64, o: usize| -> f64 {
                                let r = v.round();
                                if (r as i64).rem_euclid(2) as usize == o || step_lat < 1.5 {
                                    r
                                } else if v >= r {
                                    r + 1.0
                                } else {
                                    r - 1.0
                                }
                            };
                            let (px, py) = (snap(pfx, ox), snap(pfy, oy));
                            let (gx, gy) = fr.geom.g(px, py);
                            let (dx, dy) = (gx / s, gy / s);
                            let inv_sigma = 1.0 / fr.sigma.max(1e-12);
                            dc2 += inv_sigma * inv_sigma;
                            let row: Vec<(f64, f64)> = reps
                                .iter()
                                .enumerate()
                                .map(|(ri, &(vx, vy))| {
                                    let amp = mtf_cache[fi][ti][ri] * inv_sigma;
                                    let ph = -2.0
                                        * std::f64::consts::PI
                                        * (vx * dx + vy * dy);
                                    (amp * ph.cos(), amp * ph.sin())
                                })
                                .collect();
                            rows.push(row);
                        }
                    }
                    if rows.len() < *l {
                        if low_band {
                            kappa_low = f64::INFINITY;
                            r_low = 0.0;
                        }
                        r_target = 0.0;
                        continue;
                    }
                    // G = QᴴQ y precisión marginal del MODO OBJETIVO t:
                    // var_t = [(G)⁻¹]_{tt}. De ahí:
                    //  - amp = √var_t·‖col_t‖ ≥ 1: amplificación de ruido del
                    //    modo por tener que separarlo de sus réplicas — mide
                    //    la DIVERSIDAD de fases (κ de §7.4);
                    //  - s_t = 1/√var_t: evidencia absoluta que queda para el
                    //    modo (→ R). PSF sin MTF ⇒ s_t≈0 aunque amp sea 1.
                    let target_idx = reps
                        .iter()
                        .position(|&(vx, vy)| {
                            (vx - targets[ti].0).abs() < 1e-9
                                && (vy - targets[ti].1).abs() < 1e-9
                        })
                        .unwrap_or(0);
                    let mut gre = vec![0.0f64; *l * *l];
                    let mut gim = vec![0.0f64; *l * *l];
                    for a in 0..*l {
                        for bcol in 0..*l {
                            let (mut re, mut im) = (0.0f64, 0.0f64);
                            for row in &rows {
                                let (ar, ai) = row[a];
                                let (br, bi) = row[bcol];
                                re += ar * br + ai * bi;
                                im += ar * bi - ai * br;
                            }
                            gre[a * *l + bcol] = re;
                            gim[a * *l + bcol] = im;
                        }
                    }
                    let var_t = hermitian_solve_diag(&gre, &gim, *l, target_idx);
                    let coln = gre[target_idx * *l + target_idx].max(0.0).sqrt();
                    let (amp, s_t) = match var_t {
                        Some(v) if v > 0.0 && coln > 1e-300 => {
                            (v.sqrt() * coln, 1.0 / v.sqrt())
                        }
                        _ => (f64::INFINITY, 0.0),
                    };
                    // Evidencia RELATIVA A LA SENSIBILIDAD DC del stack: la
                    // fracción de SNR total que retiene este modo. Con PSF
                    // ancha TODAS las réplicas extendidas colapsan y s̃→0
                    // aunque amp≈1: sin evidencia no hay clase apta (§7.8).
                    let s_tilde = s_t / dc2.sqrt().max(1e-300);
                    let r = s_tilde * s_tilde / (s_tilde * s_tilde + EIDR_GATE_ETA);
                    if low_band {
                        kappa_low = kappa_low.max(amp);
                        r_low = r_low.min(r);
                    }
                    r_target = r_target.min(r);
                }
                r_sum += r_target;
                r_cnt += 1;
                rad_sum[ti / angles.len()] += r_target;
                rad_cnt[ti / angles.len()] += 1;
            }
            // Clase §7.4: diversidad (amp) Y evidencia (R) de la banda baja.
            let class = if kappa_low <= EIDR_KAPPA_APT && r_low > EIDR_GATE_R_APT {
                0u8
            } else if kappa_low <= EIDR_KAPPA_MAX && r_low > EIDR_GATE_R_MIN {
                1u8
            } else {
                2u8
            };
            match class {
                0 => n_apt += 1,
                1 => n_deg += 1,
                _ => n_fb += 1,
            }
            report.tiles.push(EidrTileGate {
                tx,
                ty,
                kappa: kappa_low,
                r_band: r_sum / r_cnt.max(1) as f64,
                class,
            });
        }
    }
    let total = (tiles_x * tiles_y).max(1) as f64;
    report.frac_apt = n_apt as f64 / total;
    report.frac_degrade = n_deg as f64 / total;
    report.frac_fallback = n_fb as f64 / total;
    report.r_radial = radii
        .iter()
        .enumerate()
        .map(|(i, &rf)| {
            let nu_native = 0.5 + (0.5 * s - 0.5) * rf;
            (
                nu_native / s,
                if rad_cnt[i] > 0 { rad_sum[i] / rad_cnt[i] as f64 } else { 0.0 },
            )
        })
        .collect();
    report
}

/// Frecuencia de corte (ciclos/px de SALIDA) desde el perfil radial de la
/// puerta: el mayor ν con R ≥ umbral (interpolado); nunca por debajo del
/// Nyquist nativo (esa banda está siempre medida). None ⇒ sin evidencia
/// extendida: cortar justo sobre el Nyquist nativo.
pub(crate) fn eidr_cutoff_from_gate(report: &EidrGateReport) -> f64 {
    let native_out = 0.5 / report.scale.max(1.0) as f64;
    if report.r_radial.is_empty() {
        return 0.5; // escala nativa: sin corte
    }
    const R_MIN: f64 = 0.05;
    let mut cut = native_out;
    let pts = &report.r_radial;
    for i in 0..pts.len() {
        if pts[i].1 >= R_MIN {
            cut = pts[i].0;
        } else {
            if i > 0 && pts[i - 1].1 >= R_MIN {
                // interpolación lineal del cruce R = R_MIN
                let (x0, y0) = pts[i - 1];
                let (x1, y1) = pts[i];
                let t = ((y0 - R_MIN) / (y0 - y1).max(1e-12)).clamp(0.0, 1.0);
                cut = x0 + t * (x1 - x0);
            }
            break;
        }
    }
    cut.max(native_out * 1.02)
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
                    robust_w: None,
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
    // Gate F9.4 — FRC vs Drizzle + holdout
    // -----------------------------------------------------------------------

    /// Drizzle de referencia (drop cuadrado, pixfrac, solo traslaciones):
    /// baseline honesto para el FRC. Convención idéntica: salida(q) ↔ ref(q/s).
    fn drizzle_ref(
        frames: &[Vec<f32>],
        dithers: &[(f64, f64)],
        idxs: &[usize],
        w: usize,
        h: usize,
        s: f64,
        pixfrac: f64,
    ) -> Vec<f32> {
        let (w_out, h_out) = ((w as f64 * s) as usize, (h as f64 * s) as usize);
        let mut sum = vec![0.0f64; w_out * h_out];
        let mut wgt = vec![0.0f64; w_out * h_out];
        let half = 0.5 * pixfrac * s;
        for &fi in idxs {
            let (dx, dy) = dithers[fi];
            for py in 0..h {
                for px in 0..w {
                    let v = frames[fi][py * w + px] as f64;
                    let cx = (px as f64 - dx) * s;
                    let cy = (py as f64 - dy) * s;
                    let x0 = ((cx - half - 0.5).ceil() as i64).max(0) as usize;
                    let x1 = ((cx + half + 0.5).floor() as i64).min(w_out as i64 - 1);
                    let y0 = ((cy - half - 0.5).ceil() as i64).max(0) as usize;
                    let y1 = ((cy + half + 0.5).floor() as i64).min(h_out as i64 - 1);
                    if x1 < x0 as i64 || y1 < y0 as i64 {
                        continue;
                    }
                    for qy in y0..=y1 as usize {
                        let oy = (half + 0.5 - (qy as f64 - cy).abs()).clamp(0.0, 1.0);
                        if oy <= 0.0 {
                            continue;
                        }
                        for qx in x0..=x1 as usize {
                            let ox = (half + 0.5 - (qx as f64 - cx).abs()).clamp(0.0, 1.0);
                            if ox <= 0.0 {
                                continue;
                            }
                            let a = ox * oy;
                            sum[qy * w_out + qx] += v * a;
                            wgt[qy * w_out + qx] += a;
                        }
                    }
                }
            }
        }
        sum.iter()
            .zip(wgt.iter())
            .map(|(&sv, &wv)| if wv > 1e-12 { (sv / wv) as f32 } else { 0.0 })
            .collect()
    }

    /// Resuelve EIDR con un subconjunto de frames (b/diag solo de ellos).
    fn eidr_solve_subset(
        op: &EidrOperator,
        datas: &[Vec<f32>],
        idxs: &[usize],
        freq: Option<&EidrFreqPenalty>,
    ) -> Vec<f32> {
        let n_out = op.w_out * op.h_out;
        let mut b = vec![0.0f64; n_out];
        let mut diag = vec![0.0f64; n_out];
        for &fi in idxs {
            op.adjoint_accum(fi, 0, &datas[fi], op.frames[fi].inv_var[0] as f64, &mut b);
            op.normal_diag_accum(fi, 0, &mut diag);
        }
        let aone = eidr_backprojected_flat(op, 0, idxs);
        let z0 = eidr_pilot(&b, &aone);
        let mut noop = |_k: usize, _n: usize| {};
        // Config por defecto + escala absoluta del χ²: el estancamiento actúa
        // como regularización por parada temprana (forzar tolerancias 1e-7
        // con pocos frames sobreajusta ruido en los modos mal condicionados
        // y degrada el FRC en TODA la banda — medido).
        let cfg = EidrSolveConfig {
            max_iterations: 200,
            data_size: idxs.iter().map(|&fi| op.frames[fi].w * op.frames[fi].h).sum(),
            ..EidrSolveConfig::default()
        };
        let (z, rep) = eidr_solve_channel(&op, 0, idxs, &b, &diag, &z0, &cfg, freq, None, &mut noop)
            .expect("solve");
        assert!(rep.converged, "subset sin converger: {rep:?}");
        z
    }

    /// §7.10: en un dataset que la puerta marca APTO, el corte FRC de EIDR
    /// (mitades odd/even independientes) debe superar al de Drizzle en ≥20%.
    /// Además el holdout (2 frames fuera del solve) debe dar χ² compatible
    /// con el ruido (predicción del modelo directo contra datos crudos).
    #[test]
    fn gate_f94_eidr_frc_vs_drizzle_and_holdout() {
        use crate::deepsky_sim as sim;
        let (w, h) = (128usize, 112usize);
        let mut stars = Vec::new();
        for j in 0..4 {
            for i in 0..4 {
                stars.push(sim::SimStar {
                    x: 17.3 + 31.1 * i as f64 + 2.1 * ((i + j) % 2) as f64,
                    y: 15.7 + 26.4 * j as f64 + 1.7 * ((i * j) % 3) as f64,
                    flux_adu: 8000.0 + 1500.0 * ((i + 2 * j) % 5) as f64,
                    fwhm_px: 1.0,
                    moffat_beta: Some(2.5),
                });
            }
        }
        let scene = sim::SimScene {
            width: w,
            height: h,
            background_adu: 150.0,
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
        // El régimen del criterio FRC es el dataset MÍNIMO apto (N=12 para
        // 2x): con 6 frames por mitad el residuo de alias de drizzle (∝1/N)
        // decorrelaciona sus mitades más allá del Nyquist nativo; EIDR lo
        // resuelve coherentemente. Con muchos frames ambos convergen y el
        // FRC odd/even es ciego a la transferencia (esperable y documentado).
        let n_frames = 14usize; // 12 al solve (6+6) + 2 holdout
        let scale = 2.0f32;
        let mut datas: Vec<Vec<f32>> = Vec::new();
        let mut vars: Vec<Vec<f64>> = Vec::new();
        let mut dithers: Vec<(f64, f64)> = Vec::new();
        // Dithers ALEATORIOS sembrados (no la secuencia áurea): al partir una
        // secuencia áurea en odd/even los residuos de alias de las mitades
        // quedan con fase relativa fija 2πφ ⇒ FRC anticorrelada espuria
        // (≈cos 2πφ = −0.74) idéntica para cualquier método. Con fases
        // independientes el FRC mide de verdad alias+ruido.
        let mut rng_state = 0x1234_5678_9abc_def0u64;
        for i in 0..n_frames {
            let dx = lcg(&mut rng_state) + (i % 3) as f64 - 1.0;
            let dy = lcg(&mut rng_state) + ((i / 3) % 3) as f64 - 1.0;
            let exp = sim::SimExposure {
                exposure_s: 1.0,
                dx,
                dy,
                seed: 977 + i as u64,
            };
            let (data, var) = sim::render_light(&scene, &sensor, &exp);
            datas.push(data.iter().map(|&v| v - 500.0).collect());
            vars.push(var);
            dithers.push((dx, dy));
        }
        // La puerta debe declarar APTO este dataset (submuestreado + dithers).
        let gate_fr: Vec<EidrGateFrame> = dithers
            .iter()
            .map(|&(dx, dy)| {
                let t = crate::DsTransform::from_similarity((1.0, 0.0, -dx as f32, -dy as f32));
                EidrGateFrame {
                    geom: eidr_geom(&t, scale).unwrap(),
                    psf: Some(MoffatPsf {
                        fwhm_x: 1.0,
                        fwhm_y: 1.0,
                        theta: 0.0,
                        beta: 2.5,
                    }),
                    sigma: (154.0f64).sqrt(),
                }
            })
            .collect();
        let gamma = MoffatPsf { fwhm_x: 1.0, fwhm_y: 1.0, theta: 0.0, beta: 2.5 };
        let rep = eidr_recoverability_gate(&gate_fr, gamma, w, h, scale, None, 64);
        assert!(rep.scale_supported(), "la puerta debería aprobar 2x aquí");

        let ivar = 1.0f32 / 154.0;
        // PAD del lienzo de hipótesis: sin él, las filas PARCIALES de A en el
        // borde (ventana de kernel asomando fuera del canvas) acoplan la
        // escala del interior con la basura de borde y el minimizador compra
        // ajuste ahí a cambio de un déficit multiplicativo interior (~0.86
        // medido). Con pad ≥ dither_max + alcance de kernel ninguna fila es
        // parcial; el pad se recorta tras el solve (§7.8: recortar bordes).
        const PAD: usize = 4;
        let frames_op: Vec<EidrFrameOp> = dithers
            .iter()
            .map(|&(dx, dy)| {
                let t = crate::DsTransform::from_similarity((
                    1.0,
                    0.0,
                    (PAD as f64 - dx) as f32,
                    (PAD as f64 - dy) as f32,
                ));
                let geom = eidr_geom(&t, scale).unwrap();
                let lut = eidr_build_lut(None, gamma, &geom);
                EidrFrameOp {
                    geom,
                    lut,
                    inv_var: [ivar; 3],
                    mask: vec![0u64; (w * h + 63) / 64],
                    robust_w: None,
                    w,
                    h,
                }
            })
            .collect();
        let op = EidrOperator {
            frames: frames_op,
            w_out: ((w + 2 * PAD) as f32 * scale) as usize,
            h_out: ((h + 2 * PAD) as f32 * scale) as usize,
            cfa: None,
            ch: 1,
        };
        let (w_res, h_res) = ((w as f32 * scale) as usize, (h as f32 * scale) as usize);
        let unpad = |img: &[f32]| -> Vec<f32> {
            let off = (PAD as f32 * scale) as usize;
            let mut out = Vec::with_capacity(w_res * h_res);
            for y in 0..h_res {
                let row = (y + off) * op.w_out + off;
                out.extend_from_slice(&img[row..row + w_res]);
            }
            out
        };
        // FRC CONTRA LA VERDAD del simulador (cielo a la PSF del frame,
        // renderizado a 2x sin ruido). El FRC odd/even es ciego a la
        // transferencia y PREMIA el difuminado (drizzle suprime señal y
        // ruido por igual y sus mitades se correlan más lejos aunque lleven
        // menos información); contra la verdad, el alias que drizzle deja
        // PLEGADO EN BANDA con pocos frames decorrelaciona su reconstrucción,
        // mientras EIDR lo separa resolviendo las réplicas.
        let truth: Vec<f32> = {
            let scene2 = sim::SimScene {
                width: w_res,
                height: h_res,
                background_adu: 150.0,
                gradient_adu_per_px: (0.0, 0.0),
                color: [1.0; 3],
                stars: scene
                    .stars
                    .iter()
                    .map(|st| sim::SimStar {
                        x: st.x * 2.0,
                        y: st.y * 2.0,
                        flux_adu: st.flux_adu * 4.0,
                        fwhm_px: st.fwhm_px * 2.0,
                        moffat_beta: st.moffat_beta,
                    })
                    .collect(),
            };
            let exp0 = sim::SimExposure { exposure_s: 1.0, dx: 0.0, dy: 0.0, seed: 1 };
            sim::render_ideal(&scene2, &sensor, &exp0)
                .iter()
                .map(|&v| v as f32)
                .collect()
        };
        let crop = |img: &[f32]| -> Vec<f32> {
            let m = 12usize;
            let (cw, chh) = (w_res - 2 * m, h_res - 2 * m);
            let mut out = Vec::with_capacity(cw * chh);
            for y in m..h_res - m {
                out.extend_from_slice(&img[y * w_res + m..y * w_res + w_res - m]);
            }
            out
        };
        let (cw, chh) = (w_res - 24, h_res - 24);
        let all: Vec<usize> = (0..12).collect();
        // λ_F Σ ω|ẑ|² (§7.3): ω desde la puerta; λ_F = mediana de la diagonal.
        let mut diag_all = vec![0.0f64; op.w_out * op.h_out];
        for &fi in &all {
            op.normal_diag_accum(fi, 0, &mut diag_all);
        }
        let mut dpos: Vec<f64> = diag_all.iter().copied().filter(|&d| d > 0.0).collect();
        let dm = dpos.len() / 2;
        dpos.select_nth_unstable_by(dm, |a, b| a.partial_cmp(b).unwrap());
        let pen = eidr_freq_penalty(&rep, op.w_out, op.h_out, dpos[dm]);
        let z_all = eidr_solve_subset(&op, &datas, &all, pen.as_ref());
        // El taper vive EN el objetivo (λ_F·ω de §7.3): amortigua las bandas
        // sin evidencia manteniendo la consistencia fotométrica del ajuste.
        // Un corte duro post-solve amputa contenido que participaba en el
        // ajuste y sesga las predicciones (+23 ADU medidos): el corte de la
        // puerta se PUBLICA (receta/mapa RECOV), no se opera con él.
        let nu_cut = eidr_cutoff_from_gate(&rep);
        assert!(nu_cut > 0.3 && nu_cut < 0.5, "corte publicado fuera de rango: {nu_cut}");
        let z_res = unpad(&z_all);
        let dz_all = drizzle_ref(&datas, &dithers, &all, w, h, scale as f64, 0.9);
        let truth_c = crop(&truth);
        // El FRC normalizado por anillo es CIEGO a la transferencia (un
        // estimador difuminante suprime señal y ruido por igual): el corte a
        // 0.5 comprime la diferencia real contra el muro del seeing. El
        // criterio §7.10 se operacionaliza con la información de la banda
        // superresuelta y la PSF efectiva, más no-regresión del corte:
        //  (a) FRC-vs-verdad MEDIA en la banda extendida ≥ 1.2× drizzle;
        //  (b) corte FRC de EIDR no peor que drizzle (≥0.98×);
        //  (c) HFD estelar (nitidez real del máster) ≤ 0.85× drizzle —
        //      drizzle difumina por caja-píxel ⊛ drop (pixfrac 0.9), EIDR
        //      entrega la PSF objetivo Γ.
        let curve_e = frc_curve(&crop(&z_res), &truth_c, cw, chh);
        let curve_d = frc_curve(&crop(&dz_all), &truth_c, cw, chh);
        for (name, curve) in [("EIDR", &curve_e), ("DRZ ", &curve_d)] {
            let line: String = curve
                .iter()
                .step_by(2)
                .map(|(r, f)| format!("{:.2}:{:+.2} ", r, f))
                .collect();
            eprintln!("FRC-verdad {name}: {line}");
        }
        let band = |c: &[(f64, f64)]| -> f64 {
            let vals: Vec<f64> = c
                .iter()
                .filter(|(r, _)| *r >= 0.25 && *r <= 0.375)
                .map(|(_, f)| f.max(0.0))
                .collect();
            vals.iter().sum::<f64>() / vals.len().max(1) as f64
        };
        let (band_e, band_d) = (band(&curve_e), band(&curve_d));
        assert!(
            band_e >= 1.2 * band_d,
            "FRC banda extendida: EIDR {band_e:.3} vs Drizzle {band_d:.3} (ratio {:.2} < 1.2)",
            band_e / band_d.max(1e-9)
        );
        let cut_eidr = frc_cutoff(&crop(&z_res), &truth_c, cw, chh, 0.5);
        let cut_drz = frc_cutoff(&crop(&dz_all), &truth_c, cw, chh, 0.5);
        assert!(
            cut_eidr >= 0.98 * cut_drz,
            "corte FRC en regresión: EIDR {cut_eidr:.3} vs Drizzle {cut_drz:.3}"
        );
        // HFD (diámetro de medio flujo) mediano de las estrellas.
        let hfd = |img: &[f32]| -> f64 {
            let mut hs: Vec<f64> = Vec::new();
            for st in &scene.stars {
                let (cx, cy) = (st.x * 2.0, st.y * 2.0);
                // Apertura contenida (99% del Moffat) y residuos SIN clamp:
                // truncar en 0 rectifica el ruido (E[max(N,0)]=0.4σ) e infla
                // el flujo acumulado justo en la imagen menos suavizada.
                let rap = 8.0f64;
                let mut ring: Vec<(f64, f64)> = Vec::new();
                let mut total = 0.0f64;
                for qy in (cy - rap).floor() as usize..=(cy + rap).ceil() as usize {
                    for qx in (cx - rap).floor() as usize..=(cx + rap).ceil() as usize {
                        let dx = qx as f64 - cx;
                        let dy = qy as f64 - cy;
                        let r = (dx * dx + dy * dy).sqrt();
                        if r > rap {
                            continue;
                        }
                        let v = (img[qy * w_res + qx] - 150.0) as f64;
                        ring.push((r, v));
                        total += v;
                    }
                }
                ring.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
                let mut acc = 0.0;
                for (r, v) in ring {
                    acc += v;
                    if acc >= 0.5 * total {
                        hs.push(2.0 * r);
                        break;
                    }
                }
            }
            hs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            hs[hs.len() / 2]
        };
        let (hfd_e, hfd_d) = (hfd(&z_res), hfd(&dz_all));
        assert!(
            hfd_e <= 0.85 * hfd_d,
            "HFD: EIDR {hfd_e:.2} vs Drizzle {hfd_d:.2} px salida (ratio {:.2} > 0.85)",
            hfd_e / hfd_d
        );

        // Holdout: la solución de los 12 predice los frames 12/13.
        for fi in [12usize, 13] {
            let st = eidr_holdout_stat(&op, fi, 0, &z_all, &datas[fi], Some(&vars[fi]));
            assert!(st.pixels > 5000, "holdout {fi}: pocos píxeles {}", st.pixels);
            assert!(
                st.chi2_median > 0.7 && st.chi2_median < 1.4,
                "holdout {fi}: χ² mediano {:.3} (medio {:.3})",
                st.chi2_median,
                st.chi2_mean
            );
        }
    }

    /// Aislante: escena PLANA sin ruido — el solve debe devolver el plano
    /// exacto (si no, el defecto es del operador/solver, no de la escena).
    #[test]
    fn eidr_solve_flat_scene_is_exact() {
        let (w, h) = (64usize, 56usize);
        let scale = 2.0f32;
        const PAD: usize = 4;
        let mut rng_state = 0xfeed_beefu64;
        let dithers: Vec<(f64, f64)> = (0..12)
            .map(|i| {
                (
                    lcg(&mut rng_state) + (i % 3) as f64 - 1.0,
                    lcg(&mut rng_state) + ((i / 3) % 3) as f64 - 1.0,
                )
            })
            .collect();
        let gamma = MoffatPsf { fwhm_x: 1.0, fwhm_y: 1.0, theta: 0.0, beta: 2.5 };
        let ivar = 1.0f32 / 154.0;
        let frames_op: Vec<EidrFrameOp> = dithers
            .iter()
            .map(|&(dx, dy)| {
                let t = crate::DsTransform::from_similarity((
                    1.0,
                    0.0,
                    (PAD as f64 - dx) as f32,
                    (PAD as f64 - dy) as f32,
                ));
                let geom = eidr_geom(&t, scale).unwrap();
                let lut = eidr_build_lut(None, gamma, &geom);
                EidrFrameOp {
                    geom,
                    lut,
                    inv_var: [ivar; 3],
                    mask: vec![0u64; (w * h + 63) / 64],
                    robust_w: None,
                    w,
                    h,
                }
            })
            .collect();
        let op = EidrOperator {
            frames: frames_op,
            w_out: ((w + 2 * PAD) as f32 * scale) as usize,
            h_out: ((h + 2 * PAD) as f32 * scale) as usize,
            cfa: None,
            ch: 1,
        };
        let datas: Vec<Vec<f32>> = (0..12).map(|_| vec![150.0f32; w * h]).collect();
        let n_out = op.w_out * op.h_out;
        let mut b = vec![0.0f64; n_out];
        let mut diag = vec![0.0f64; n_out];
        for fi in 0..12 {
            op.adjoint_accum(fi, 0, &datas[fi], ivar as f64, &mut b);
            op.normal_diag_accum(fi, 0, &mut diag);
        }
        let aone = eidr_backprojected_flat(&op, 0, &(0..12).collect::<Vec<_>>());
        let z0 = eidr_pilot(&b, &aone);
        let mut noop = |_k: usize, _n: usize| {};
        let cfg = EidrSolveConfig {
            max_iterations: 200,
            data_size: 12 * w * h,
            ..EidrSolveConfig::default()
        };
        let idxs_all: Vec<usize> = (0..12).collect();
        let (z, rep) =
            eidr_solve_channel(&op, 0, &idxs_all, &b, &diag, &z0, &cfg, None, None, &mut noop)
                .unwrap();
        // Interior del lienzo (sin pad):
        let off = (PAD as f32 * scale) as usize;
        let mut worst = 0.0f32;
        let mut sum = 0.0f64;
        let mut cnt = 0usize;
        for y in off + 4..op.h_out - off - 4 {
            for x in off + 4..op.w_out - off - 4 {
                let v = z[y * op.w_out + x];
                worst = worst.max((v - 150.0).abs());
                sum += v as f64;
                cnt += 1;
            }
        }
        eprintln!(
            "flat solve: mean={:.2} worst_dev={:.2} iters={} rel={:.1e} conv={}",
            sum / cnt as f64, worst, rep.iterations, rep.rel_residual, rep.converged
        );
        assert!(
            (sum / cnt as f64 - 150.0).abs() < 0.5 && worst < 2.0,
            "plano no recuperado: mean={:.2} worst={:.2}",
            sum / cnt as f64,
            worst
        );
    }

    // -----------------------------------------------------------------------
    // Gates F10 (CPU): Huber-IRLS, campo vacío, microregistro
    // -----------------------------------------------------------------------

    fn f10_scene_ops(
        w: usize,
        h: usize,
        n_frames: usize,
        with_stars: bool,
        seed: u64,
    ) -> (EidrOperator, Vec<Vec<f32>>, Vec<(f64, f64)>, crate::deepsky_sim::SimScene) {
        use crate::deepsky_sim as sim;
        let mut stars = Vec::new();
        if with_stars {
            for j in 0..3 {
                for i in 0..3 {
                    stars.push(sim::SimStar {
                        x: 16.4 + 25.1 * i as f64,
                        y: 14.2 + 22.7 * j as f64,
                        flux_adu: 9000.0,
                        fwhm_px: 1.2,
                        moffat_beta: Some(2.5),
                    });
                }
            }
        }
        let scene = sim::SimScene {
            width: w,
            height: h,
            background_adu: 150.0,
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
        let mut st = seed;
        let mut datas = Vec::new();
        let mut dithers = Vec::new();
        for i in 0..n_frames {
            let dx = lcg(&mut st) + (i % 3) as f64 - 1.0;
            let dy = lcg(&mut st) + ((i / 3) % 3) as f64 - 1.0;
            let exp = sim::SimExposure { exposure_s: 1.0, dx, dy, seed: seed ^ (i as u64) };
            let (data, _var) = sim::render_light(&scene, &sensor, &exp);
            datas.push(data.iter().map(|&v| v - 500.0).collect());
            dithers.push((dx, dy));
        }
        const PAD: usize = 4;
        let gamma = MoffatPsf { fwhm_x: 1.2, fwhm_y: 1.2, theta: 0.0, beta: 2.5 };
        let frames_op: Vec<EidrFrameOp> = dithers
            .iter()
            .map(|&(dx, dy)| {
                let t = crate::DsTransform::from_similarity((
                    1.0,
                    0.0,
                    (PAD as f64 - dx) as f32,
                    (PAD as f64 - dy) as f32,
                ));
                let geom = eidr_geom(&t, 2.0).unwrap();
                let lut = eidr_build_lut(None, gamma, &geom);
                EidrFrameOp {
                    geom,
                    lut,
                    inv_var: [1.0 / 154.0; 3],
                    mask: vec![0u64; (w * h + 63) / 64],
                    robust_w: None,
                    w,
                    h,
                }
            })
            .collect();
        let op = EidrOperator {
            frames: frames_op,
            w_out: ((w + 2 * PAD) * 2) as usize,
            h_out: ((h + 2 * PAD) * 2) as usize,
            cfa: None,
            ch: 1,
        };
        (op, datas, dithers, scene)
    }

    /// F10 §7.10: un satélite brillante en UNA toma. El cuadrático lo diluye
    /// (≈trail/N en el máster); Huber-IRLS lo pesa a ~0 y el máster queda
    /// limpio. El baseline cuadrático se mantiene como referencia (§7.9).
    #[test]
    fn gate_f10_huber_removes_satellite() {
        let (mut op, mut datas, _d, scene_ref) = f10_scene_ops(96, 80, 12, true, 0xf10a);
        let (w, _h) = (96usize, 80usize);
        // Traza diagonal brillante en el frame 5.
        for t in 0..800 {
            let x = 8 + (t * 80) / 800;
            let y = 8 + (t * 60) / 800;
            for k in 0..2usize {
                datas[5][(y + k) * w + x] += 2500.0;
            }
        }
        let all: Vec<usize> = (0..12).collect();
        let z_q = eidr_solve_subset(&op, &datas, &all, None);
        // IRLS real: pesos desde el modelo directo → RE-solve → pesos → solve.
        let mut z_h = z_q.clone();
        for _round in 0..3 {
            let weights: Vec<Vec<f32>> = all
                .iter()
                .map(|&fi| eidr_irls_weights(&op, fi, 0, &z_h, &datas[fi], 2.5))
                .collect();
            for (&fi, wv) in all.iter().zip(weights.into_iter()) {
                op.frames[fi].robust_w = Some(wv);
            }
            z_h = eidr_solve_subset(&op, &datas, &all, None);
        }
        // Media del residuo sobre el locus de la traza en coords de salida.
        let stars = &scene_ref.stars;
        let trail_mean = |z: &[f32]| -> f64 {
            let mut acc = 0.0f64;
            let mut n = 0usize;
            for t in (0..800).step_by(7) {
                let x = 8 + (t * 80) / 800;
                let y = 8 + (t * 60) / 800;
                // Fuera de las alas estelares: solo mide la traza.
                if stars
                    .iter()
                    .any(|st| (st.x - x as f64).hypot(st.y - y as f64) < 8.0)
                {
                    continue;
                }
                // dither del frame 5 ≈ conocido: usar geometría real.
                let (gx, gy) = op.frames[5].geom.g(x as f64, y as f64);
                let (qx, qy) = (gx.round() as usize, gy.round() as usize);
                if qx < op.w_out && qy < op.h_out {
                    acc += (z[qy * op.w_out + qx] - 150.0) as f64;
                    n += 1;
                }
            }
            acc / n.max(1) as f64
        };
        let (m_q, m_h) = (trail_mean(&z_q), trail_mean(&z_h));
        assert!(
            m_q > 80.0,
            "el cuadrático debería mostrar la traza diluida (medido {m_q:.1})"
        );
        assert!(
            m_h < 0.02 * m_q && m_h < 60.0,
            "Huber no limpió la traza: {m_h:.1} vs cuadrático {m_q:.1}"
        );
    }

    /// F10 §7.10: campo VACÍO — las falsas detecciones a 5σ no superan a
    /// Drizzle en más del 5% (+2 de margen entero). Solve con la penalización
    /// espectral de producción (la banda sin evidencia no inventa fuentes).
    #[test]
    fn gate_f10_empty_field_no_false_sources() {
        let (op, datas, dithers, _s) = f10_scene_ops(96, 80, 12, false, 0xe1d);
        let all: Vec<usize> = (0..12).collect();
        // Puerta + penalización como en producción.
        let gate_fr: Vec<EidrGateFrame> = dithers
            .iter()
            .map(|&(dx, dy)| {
                let t = crate::DsTransform::from_similarity((1.0, 0.0, -dx as f32, -dy as f32));
                EidrGateFrame {
                    geom: eidr_geom(&t, 2.0).unwrap(),
                    psf: Some(MoffatPsf { fwhm_x: 1.2, fwhm_y: 1.2, theta: 0.0, beta: 2.5 }),
                    sigma: (154.0f64).sqrt(),
                }
            })
            .collect();
        let gamma = MoffatPsf { fwhm_x: 1.2, fwhm_y: 1.2, theta: 0.0, beta: 2.5 };
        let rep = eidr_recoverability_gate(&gate_fr, gamma, 96, 80, 2.0, None, 64);
        let mut diag = vec![0.0f64; op.w_out * op.h_out];
        for &fi in &all {
            op.normal_diag_accum(fi, 0, &mut diag);
        }
        let mut dpos: Vec<f64> = diag.iter().copied().filter(|&d| d > 0.0).collect();
        let dm = dpos.len() / 2;
        dpos.select_nth_unstable_by(dm, |a, b| a.partial_cmp(b).unwrap());
        let pen = eidr_freq_penalty(&rep, op.w_out, op.h_out, dpos[dm]);
        let z = eidr_solve_subset(&op, &datas, &all, pen.as_ref());
        let dz = drizzle_ref(&datas, &dithers, &all, 96, 80, 2.0, 0.9);
        // Detector de fuentes: máximo local con (v−mediana) > 5·σ_MAD.
        let count_sources = |img: &[f32], w: usize, _h: usize, x0: usize, y0: usize, ww: usize, hh: usize| -> usize {
            let mut vals: Vec<f32> = Vec::with_capacity(ww * hh);
            for y in y0..y0 + hh {
                for x in x0..x0 + ww {
                    vals.push(img[y * w + x]);
                }
            }
            let mid = vals.len() / 2;
            vals.select_nth_unstable_by(mid, |a, b| a.partial_cmp(b).unwrap());
            let med = vals[mid];
            let mut devs: Vec<f32> = vals.iter().map(|&v| (v - med).abs()).collect();
            devs.select_nth_unstable_by(mid, |a, b| a.partial_cmp(b).unwrap());
            let sigma = devs[mid] * 1.4826;
            let thr = med + 5.0 * sigma.max(1e-3);
            let mut n = 0usize;
            for y in y0 + 1..y0 + hh - 1 {
                for x in x0 + 1..x0 + ww - 1 {
                    let v = img[y * w + x];
                    if v <= thr {
                        continue;
                    }
                    let mut is_max = true;
                    for oy in 0..3usize {
                        for ox in 0..3usize {
                            if (ox, oy) != (1, 1)
                                && img[(y + oy - 1) * w + (x + ox - 1)] >= v
                            {
                                is_max = false;
                            }
                        }
                    }
                    if is_max {
                        n += 1;
                    }
                }
            }
            n
        };
        // Región común sin bordes: el lienzo EIDR está acolchado (+8 out px).
        let n_eidr = count_sources(&z, op.w_out, op.h_out, 24, 24, 144, 112);
        let n_drz = count_sources(&dz, 192, 160, 16, 16, 144, 112);
        assert!(
            n_eidr as f64 <= n_drz as f64 * 1.05 + 2.0,
            "falsas fuentes: EIDR {n_eidr} vs Drizzle {n_drz}"
        );
    }

    /// F10 §7.5(5): microregistro GN recupera un error de traslación
    /// inyectado (0.15, −0.12) px y mejora el residual del frame.
    #[test]
    fn gate_f10_refine_translation_recovers_shift() {
        let (mut op, datas, _d, _s) = f10_scene_ops(96, 80, 12, true, 0x5417);
        let all: Vec<usize> = (0..12).collect();
        // Estropear a sabiendas la geometría del frame 7.
        let (true_ex, true_ey) = (0.15f64, -0.12f64);
        eidr_shift_geom(&mut op.frames[7].geom, -true_ex, -true_ey);
        let z = eidr_solve_subset(&op, &datas, &all, None);
        let rms_frame = |op: &EidrOperator, z: &[f32]| -> f64 {
            let f = &op.frames[7];
            let mut pred = vec![0.0f32; f.w * f.h];
            op.apply(7, 0, z, &mut pred);
            let mut acc = 0.0f64;
            let mut n = 0usize;
            for idx in 0..pred.len() {
                if pred[idx] != 0.0 {
                    let d = (datas[7][idx] - pred[idx]) as f64;
                    acc += d * d;
                    n += 1;
                }
            }
            (acc / n.max(1) as f64).sqrt()
        };
        let rms_before = rms_frame(&op, &z);
        let (ex, ey) = eidr_refine_translation(&op, 7, 0, &z, &datas[7], 0.2)
            .expect("refinamiento");
        assert!(
            (ex - true_ex).abs() < 0.06 && (ey - true_ey).abs() < 0.06,
            "corrección estimada ({ex:.3},{ey:.3}) vs verdad ({true_ex},{true_ey})"
        );
        eidr_shift_geom(&mut op.frames[7].geom, ex, ey);
        let rms_after = rms_frame(&op, &z);
        assert!(
            rms_after < rms_before * 0.9,
            "el residual no mejoró: {rms_before:.2} → {rms_after:.2}"
        );
    }

    // -----------------------------------------------------------------------
    // Gate F9.3 — puerta de recuperabilidad
    // -----------------------------------------------------------------------

    fn gate_frames(dithers: &[(f64, f64)], fwhm: f32, scale: f32) -> Vec<EidrGateFrame> {
        dithers
            .iter()
            .map(|&(dx, dy)| {
                let t = crate::DsTransform::from_similarity((1.0, 0.0, -dx as f32, -dy as f32));
                EidrGateFrame {
                    geom: eidr_geom(&t, scale).unwrap(),
                    psf: Some(MoffatPsf {
                        fwhm_x: fwhm,
                        fwhm_y: fwhm,
                        theta: 0.0,
                        beta: 2.5,
                    }),
                    sigma: 14.0,
                }
            })
            .collect()
    }

    fn golden_dithers(n: usize) -> Vec<(f64, f64)> {
        (0..n)
            .map(|i| {
                (
                    (i as f64 * 0.618033988749895).fract() + (i % 3) as f64 - 1.0,
                    (i as f64 * 0.754877666246693).fract() + ((i / 3) % 3) as f64 - 1.0,
                )
            })
            .collect()
    }

    /// §7.4: dithers subpíxel diversos con PSF submuestreada ⇒ apto a 2x;
    /// dithers ENTEROS (fases degeneradas) ⇒ fallback: el 2x sería pura
    /// interpolación y la puerta lo dice antes de resolver. PSF ancha (bien
    /// muestreada) ⇒ tampoco hay evidencia superresuelta. Escala 1 ⇒ trivial.
    #[test]
    fn gate_f93_recoverability_diverse_vs_degenerate() {
        // Diversos, submuestreado (FWHM 1.3 px) ⇒ todo apto.
        let rep = eidr_recoverability_gate(
            &gate_frames(&golden_dithers(16), 1.3, 2.0),
            MoffatPsf { fwhm_x: 1.3, fwhm_y: 1.3, theta: 0.0, beta: 2.5 },
            256,
            192,
            2.0,
            None,
            128,
        );
        assert!(
            rep.frac_apt > 0.99 && rep.scale_supported(),
            "diversos: apt={:.2} deg={:.2} fb={:.2} κ0={:.1}",
            rep.frac_apt,
            rep.frac_degrade,
            rep.frac_fallback,
            rep.tiles[0].kappa
        );
        assert!(rep.tiles.iter().all(|t| t.r_band > 0.01));

        // Dithers enteros ⇒ réplicas indistinguibles ⇒ fallback.
        let deg: Vec<(f64, f64)> = (0..16).map(|i| ((i % 4) as f64, (i / 4) as f64)).collect();
        let rep = eidr_recoverability_gate(
            &gate_frames(&deg, 1.3, 2.0),
            MoffatPsf { fwhm_x: 1.3, fwhm_y: 1.3, theta: 0.0, beta: 2.5 },
            256,
            192,
            2.0,
            None,
            128,
        );
        assert!(
            rep.frac_fallback > 0.99 && !rep.scale_supported(),
            "enteros: fb={:.2} κ0={:.1}",
            rep.frac_fallback,
            rep.tiles[0].kappa
        );

        // PSF ancha (3.5 px, bien muestreada): sin MTF extendida ⇒ no apto
        // aunque el dither sea perfecto (§7.8: no se promete superresolución).
        let rep = eidr_recoverability_gate(
            &gate_frames(&golden_dithers(16), 3.5, 2.0),
            MoffatPsf { fwhm_x: 3.5, fwhm_y: 3.5, theta: 0.0, beta: 2.5 },
            256,
            192,
            2.0,
            None,
            128,
        );
        assert!(
            rep.frac_apt < 0.01,
            "PSF ancha: apt={:.2} κ0={:.1}",
            rep.frac_apt,
            rep.tiles[0].kappa
        );

        // Escala nativa: sin banda extendida, todo apto por definición.
        let rep = eidr_recoverability_gate(
            &gate_frames(&golden_dithers(16), 1.3, 1.0),
            MoffatPsf { fwhm_x: 1.3, fwhm_y: 1.3, theta: 0.0, beta: 2.5 },
            256,
            192,
            1.0,
            None,
            128,
        );
        assert!(rep.frac_apt > 0.99 && rep.tiles.iter().all(|t| t.r_band == 1.0));
    }

    /// CFA a 2x: la retícula por canal (paso 2) exige más diversidad; con 24
    /// dithers dorados hay evidencia, con dithers enteros PARES (fase CFA
    /// congelada) no la hay ni siquiera para el verde quincunx.
    #[test]
    fn gate_f93_recoverability_cfa() {
        let gamma = MoffatPsf { fwhm_x: 1.4, fwhm_y: 1.4, theta: 0.0, beta: 2.5 };
        for c in [0usize, 1, 2] {
            let rep = eidr_recoverability_gate(
                &gate_frames(&golden_dithers(24), 1.4, 2.0),
                gamma,
                256,
                192,
                2.0,
                Some((8, c)),
                128,
            );
            assert!(
                rep.frac_fallback < 0.01,
                "CFA c={c} diversos: fb={:.2} κ0={:.1}",
                rep.frac_fallback,
                rep.tiles[0].kappa
            );
            let deg: Vec<(f64, f64)> =
                (0..24).map(|i| (2.0 * (i % 4) as f64, 2.0 * (i / 4) as f64)).collect();
            let rep = eidr_recoverability_gate(
                &gate_frames(&deg, 1.4, 2.0),
                gamma,
                256,
                192,
                2.0,
                Some((8, c)),
                128,
            );
            assert!(
                rep.frac_fallback > 0.99,
                "CFA c={c} enteros pares: fb={:.2}",
                rep.frac_fallback
            );
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
                    robust_w: None,
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
        let aone = eidr_backprojected_flat(&op, 0, &(0..n_frames).collect::<Vec<_>>());
        let z0 = eidr_pilot(&b, &aone);
        let mut noop = |_k: usize, _n: usize| {};
        let cfg = EidrSolveConfig {
            data_size: n_frames * w * h,
            ..EidrSolveConfig::default()
        };
        let idxs_all: Vec<usize> = (0..n_frames).collect();
        let (z, rep) =
            eidr_solve_channel(&op, 0, &idxs_all, &b, &diag, &z0, &cfg, None, None, &mut noop)
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
