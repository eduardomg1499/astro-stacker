//! NebulaFusion Lite (F3): primer motor científico de integración.
//!
//! Coadición por media ponderada con PESOS INVERSO-VARIANZA independientes
//! del brillo del objeto (§2 del plan técnico: pesar por señal cambiaría la
//! PSF del máster) y máscaras de outlier CONGELADAS por cross-fit (módulo
//! `deepsky_masks`) en lugar del κσ online. Produce, además del máster SCI:
//!
//! - `VAR` — varianza por píxel-canal: con pesos w=1/σ² exactos,
//!   Var(media ponderada) = 1/Σw.
//! - `NEFF` — número efectivo de muestras: (Σw)²/Σw².
//! - `DQ` — máscara de calidad de la salida (bit NO_COVERAGE en huecos).
//!
//! Decisiones documentadas de la fase Lite:
//! - σ por celda (~64 px nativos) medida con el ruido MRS del frame calibrado
//!   y NORMALIZADO (σ′ = mul·σ); un solo σ de luma para los 3 canales (los
//!   pesos por canal llegan con el modo CFA de F4).
//! - La correlación introducida por el remuestreo Lanczos se ignora en Lite
//!   (subestima levemente VAR); NF-Full la corrige vía PSD (F6).
//! - Los pesos por frame de calidad WBPP (FWHM/PSF/redondez) NO se usan:
//!   dependen de la señal. Solo ruido de fondo manda.
//! - Huecos sin cobertura: el máster lleva 0.0 (como el clásico, para no
//!   romper preview/stretch) pero VAR=NaN, NEFF=0 y DQ|=NO_COVERAGE los
//!   declaran; el relleno visual de drizzle NUNCA se aplica aquí.
//! - CPU siempre (la paridad GPU llega en F8). Drizzle/CFA-directo excluidos
//!   por el preflight en esta fase.

#![allow(dead_code)]

use std::sync::atomic::AtomicBool;

/// Lado de la celda nativa para la medición de σ (px).
const SIGMA_CELL_PX: usize = 64;
/// Rejilla de pesos en espacio de referencia (RG×RG, muestreada bilineal).
const WEIGHT_GRID: usize = 16;
/// Tope de peso relativo por celda (× mediana del frame): una celda muerta
/// (σ≈0) no puede dominar el máster.
const MAX_RELATIVE_WEIGHT: f32 = 25.0;

pub(crate) struct NfLiteProducts {
    /// Varianza por píxel-canal (layout del máster). NaN sin cobertura.
    pub variance: Vec<f32>,
    /// Número efectivo de muestras por píxel-canal. 0 sin cobertura.
    pub neff: Vec<f32>,
    /// DQ por píxel (u32, bits de `deepsky_variance::dq`).
    pub dq: Vec<u32>,
    /// Píxeles×frame enmascarados por el cross-fit (telemetría/receta).
    pub masked_samples: usize,
    /// Origen de la varianza para la receta.
    pub variance_origin: crate::deepsky_variance::VarianceOrigin,
    /// Máximo |mediana(G1)−mediana(G2)| entre frames (solo CFA directo):
    /// auditoría del mismo canal físico (§6.8 del plan técnico).
    pub g1g2_offset_max: Option<f32>,
}

pub(crate) struct NfLiteOutput {
    pub final_data: Vec<f32>,
    /// Cobertura de la pasada de totales (pre-máscaras) — la usa el auto-crop.
    pub wgt1: Vec<f64>,
    /// Cobertura/peso final (post-máscaras), media entre canales.
    pub weight_map: Vec<f64>,
    pub rejection_low: Vec<f64>,
    pub rejection_high: Vec<f64>,
    pub rej_pct: f64,
    /// Frames efectivos por píxel (aprox.: N·Σw_final/Σw_total).
    pub mean_cov: f64,
    pub products: NfLiteProducts,
}

pub(crate) struct NfLiteContext<'a> {
    pub registered: &'a [(usize, crate::DsTransform, f64)],
    pub norms: &'a [([f32; 3], [f32; 3])],
    pub loc_fields: &'a [Option<Vec<f32>>],
    pub loc_grid: usize,
    pub w_out: usize,
    pub h_out: usize,
    pub ch: usize,
    pub use_lanczos: bool,
    pub cancel: &'a AtomicBool,
    /// Some(cid) = modo CFA DIRECTO (F4): los frames llegan como plano CFA
    /// mono calibrado SIN debayer y cada fotosito deposita solo en su canal
    /// del máster RGB (`ch` debe ser 3). None = ruta mono/RGB demosaiced.
    pub cfa: Option<i32>,
}

/// Pesos de un frame: rejilla espacial 1/σ² (mono/RGB) o escalar por canal
/// (CFA directo: la varianza se mide por sub-plano Bayer; la variación
/// espacial por canal llega con NF-Full).
enum FrameWeights {
    Grid(Vec<f32>),
    Rgb([f64; 3]),
}

/// σ MRS por canal desde los SUB-PLANOS Bayer del frame CFA calibrado (cada
/// sub-plano es una imagen coherente a media resolución — el mosaico entero
/// inflaría la capa fina de la wavelet con el patrón 2×2). Devuelve también
/// el offset mediano G1−G2 (mismo canal físico, auditable: un offset grande
/// delata un problema de calibración por fila/columna).
pub(crate) fn cfa_channel_sigmas(img: &crate::DsImage, cid: i32) -> ([f32; 3], f32) {
    let hw = (img.w / 2).max(1);
    let hh = (img.h / 2).max(1);
    let mut planes: [Vec<f32>; 4] = [
        Vec::with_capacity(hw * hh),
        Vec::with_capacity(hw * hh),
        Vec::with_capacity(hw * hh),
        Vec::with_capacity(hw * hh),
    ];
    for y in 0..hh * 2 {
        for x in 0..hw * 2 {
            planes[(y & 1) * 2 + (x & 1)].push(img.data[y * img.w + x]);
        }
    }
    let mut sigma_ch = [0.0f32; 3];
    let mut g_sigmas: Vec<f32> = Vec::new();
    let mut g_medians: Vec<f32> = Vec::new();
    for (pos, plane) in planes.iter().enumerate() {
        let (py, px) = (pos / 2, pos % 2);
        let c = crate::ds_cfa_channel(cid, px, py);
        let sigma = crate::ds_mrs_noise(plane, hw, hh).max(1e-3);
        if c == 1 {
            g_sigmas.push(sigma);
            let step = (plane.len() / 100_000).max(1);
            let mut s: Vec<f32> = plane.iter().step_by(step).copied().collect();
            s.sort_by(|a, b| a.total_cmp(b));
            g_medians.push(s[s.len() / 2]);
        } else {
            sigma_ch[c] = sigma;
        }
    }
    sigma_ch[1] = g_sigmas.iter().sum::<f32>() / g_sigmas.len().max(1) as f32;
    let g_offset = if g_medians.len() == 2 {
        g_medians[0] - g_medians[1]
    } else {
        0.0
    };
    (sigma_ch, g_offset)
}

/// Una acumulación NF (despacho mono/RGB vs CFA directo) con los parámetros
/// comunes del contexto. Mantiene la geometría idéntica entre pasadas.
#[allow(clippy::too_many_arguments)]
fn nf_accumulate(
    ctx: &NfLiteContext,
    k: usize,
    t: crate::DsTransform,
    img: &crate::DsImage,
    weights: &FrameWeights,
    sum: &mut [f64],
    wgt: &mut [f64],
    mask: Option<&[u64]>,
    wsq: Option<&mut Vec<f64>>,
) {
    let loc_ref = ctx.loc_fields[k]
        .as_ref()
        .map(|f| (f.as_slice(), ctx.loc_grid, ctx.loc_grid));
    match (ctx.cfa, weights) {
        (Some(cid), FrameWeights::Rgb(fw_rgb)) => {
            crate::ds_drizzle_cfa_accumulate(
                img, cid, t, sum, None, wgt, None, None, ctx.w_out, ctx.h_out, 1.0, 1.0, 1.0,
                ctx.norms[k], loc_ref, None, Some(*fw_rgb), mask, wsq,
            );
        }
        (_, FrameWeights::Grid(grid)) => {
            crate::ds_warp_accumulate(
                img,
                t,
                sum,
                None,
                wgt,
                None,
                None,
                ctx.w_out,
                ctx.h_out,
                ctx.ch,
                1.0,
                1.0,
                ctx.norms[k],
                loc_ref,
                Some((grid.as_slice(), WEIGHT_GRID, WEIGHT_GRID)),
                ctx.use_lanczos,
                mask,
                wsq,
            );
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Super-binning (F4): salida 0.75x/0.5x para datos sobremuestreados.
// ---------------------------------------------------------------------------

/// Remuestreo por ÁREA de un plano interleaved f32 al factor s = num/den < 1.
/// SCI: media ponderada por área (fotometría de superficie conservada).
/// Con `as_variance`, propaga varianza: VAR' = Σa²·VAR/(Σa)² (independencia
/// aproximada — la correlación del Lanczos se ignora en Lite, documentado).
/// Los NaN (huecos) se excluyen del numerador y del denominador.
pub(crate) fn bin_area_f32(
    data: &[f32],
    w: usize,
    h: usize,
    ch: usize,
    num: usize,
    den: usize,
    as_variance: bool,
) -> (Vec<f32>, usize, usize) {
    let nw = (w * num / den).max(1);
    let nh = (h * num / den).max(1);
    let inv = den as f64 / num as f64; // lado del bloque de entrada por píxel de salida
    let mut out = vec![f32::NAN; nw * nh * ch];
    for oy in 0..nh {
        let y0 = oy as f64 * inv;
        let y1 = ((oy + 1) as f64 * inv).min(h as f64);
        for ox in 0..nw {
            let x0 = ox as f64 * inv;
            let x1 = ((ox + 1) as f64 * inv).min(w as f64);
            for c in 0..ch {
                let mut acc = 0.0f64;
                let mut acc_sq = 0.0f64;
                let mut area_sum = 0.0f64;
                let mut iy = y0.floor() as usize;
                while (iy as f64) < y1 && iy < h {
                    let ay = (y1.min(iy as f64 + 1.0) - y0.max(iy as f64)).max(0.0);
                    let mut ix = x0.floor() as usize;
                    while (ix as f64) < x1 && ix < w {
                        let ax = (x1.min(ix as f64 + 1.0) - x0.max(ix as f64)).max(0.0);
                        let a = ax * ay;
                        let v = data[(iy * w + ix) * ch + c];
                        if a > 0.0 && v.is_finite() {
                            area_sum += a;
                            if as_variance {
                                acc_sq += a * a * v as f64;
                            } else {
                                acc += a * v as f64;
                            }
                        }
                        ix += 1;
                    }
                    iy += 1;
                }
                if area_sum > 0.0 {
                    out[(oy * nw + ox) * ch + c] = if as_variance {
                        (acc_sq / (area_sum * area_sum)) as f32
                    } else {
                        (acc / area_sum) as f32
                    };
                }
            }
        }
    }
    (out, nw, nh)
}

/// Binning del plano DQ: NO_COVERAGE solo si TODOS los píxeles del bloque lo
/// llevan; el resto de bits se acumulan con OR (defecto en cualquier parte
/// del bloque ⇒ declarado).
pub(crate) fn bin_dq(dq: &[u32], w: usize, h: usize, num: usize, den: usize) -> Vec<u32> {
    let nw = (w * num / den).max(1);
    let nh = (h * num / den).max(1);
    let inv = den as f64 / num as f64;
    let mut out = vec![0u32; nw * nh];
    for oy in 0..nh {
        for ox in 0..nw {
            let y0 = (oy as f64 * inv).floor() as usize;
            let y1 = (((oy + 1) as f64 * inv).ceil() as usize).min(h);
            let x0 = (ox as f64 * inv).floor() as usize;
            let x1 = (((ox + 1) as f64 * inv).ceil() as usize).min(w);
            let mut bits = 0u32;
            let mut all_uncovered = true;
            for iy in y0..y1.max(y0 + 1) {
                for ix in x0..x1.max(x0 + 1) {
                    let b = dq[iy.min(h - 1) * w + ix.min(w - 1)];
                    bits |= b & !crate::deepsky_variance::dq::NO_COVERAGE;
                    if b & crate::deepsky_variance::dq::NO_COVERAGE == 0 {
                        all_uncovered = false;
                    }
                }
            }
            if all_uncovered {
                bits |= crate::deepsky_variance::dq::NO_COVERAGE;
            }
            out[oy * nw + ox] = bits;
        }
    }
    out
}

/// σ MRS por celda nativa (~64 px) sobre la luma del frame calibrado.
pub(crate) fn native_sigma_grid(img: &crate::DsImage) -> (Vec<f32>, usize, usize) {
    let gw = (img.w / SIGMA_CELL_PX).max(1);
    let gh = (img.h / SIGMA_CELL_PX).max(1);
    let npx = img.w * img.h;
    let luma: Vec<f32> = if img.ch == 1 {
        img.data.clone()
    } else {
        (0..npx)
            .map(|p| {
                (img.data[p * img.ch] + img.data[p * img.ch + 1] + img.data[p * img.ch + 2]) / 3.0
            })
            .collect()
    };
    let mut grid = vec![0.0f32; gw * gh];
    for gy in 0..gh {
        for gx in 0..gw {
            let x0 = gx * img.w / gw;
            let x1 = ((gx + 1) * img.w / gw).min(img.w);
            let y0 = gy * img.h / gh;
            let y1 = ((gy + 1) * img.h / gh).min(img.h);
            let (cw, chh) = (x1 - x0, y1 - y0);
            let mut cell = Vec::with_capacity(cw * chh);
            for y in y0..y1 {
                cell.extend_from_slice(&luma[y * img.w + x0..y * img.w + x1]);
            }
            grid[gy * gw + gx] = crate::ds_mrs_noise(&cell, cw, chh).max(1e-3);
        }
    }
    (grid, gw, gh)
}

/// Rejilla de pesos 1/σ′² en ESPACIO DE REFERENCIA: para cada celda de la
/// rejilla de salida, el centro se lleva al frame nativo con la inversa del
/// transform y se toma la σ de la celda nativa correspondiente, escalada por
/// la normalización multiplicativa (σ′ = mul·σ). Celdas fuera del frame
/// heredan el peso del borde más cercano (no aportan de todos modos: el
/// acumulador descarta muestras fuera de rango).
pub(crate) fn reference_weight_grid(
    sigma: &(Vec<f32>, usize, usize),
    t: &crate::DsTransform,
    mul_mean: f32,
    native_w: usize,
    native_h: usize,
    w_out: usize,
    h_out: usize,
) -> Vec<f32> {
    let (sg, sgw, sgh) = (&sigma.0, sigma.1, sigma.2);
    let mut grid = vec![0.0f32; WEIGHT_GRID * WEIGHT_GRID];
    for gy in 0..WEIGHT_GRID {
        for gx in 0..WEIGHT_GRID {
            let ox = (gx as f32 + 0.5) / WEIGHT_GRID as f32 * w_out as f32;
            let oy = (gy as f32 + 0.5) / WEIGHT_GRID as f32 * h_out as f32;
            let (nx, ny) = t
                .inverse(ox, oy)
                .unwrap_or((native_w as f32 / 2.0, native_h as f32 / 2.0));
            let cx = ((nx as usize).min(native_w.saturating_sub(1)) * sgw / native_w.max(1))
                .min(sgw - 1);
            let cy = ((ny as usize).min(native_h.saturating_sub(1)) * sgh / native_h.max(1))
                .min(sgh - 1);
            let s = (sg[cy * sgw + cx] * mul_mean.max(1e-6)).max(1e-3);
            grid[gy * WEIGHT_GRID + gx] = 1.0 / (s * s);
        }
    }
    // Tope relativo: mediana × MAX_RELATIVE_WEIGHT.
    let mut sorted = grid.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = sorted[sorted.len() / 2].max(1e-12);
    for v in grid.iter_mut() {
        *v = v.min(median * MAX_RELATIVE_WEIGHT);
    }
    grid
}

/// Curva ruido-vs-nivel por canal (transferencia fotónica EMPÍRICA): σ del
/// residual LOO |y−piloto| en bins del nivel del piloto. Incluye por
/// construcción el shot noise del objeto y el ruido del piloto — es el
/// denominador correcto del residual sin necesitar el gain de la cámara.
/// Los núcleos estelares (nivel alto ⇒ σ alta) dejan de confundirse con
/// outliers, exactamente el fallo del κσ clásico que el plan exige evitar.
pub(crate) struct NoiseCurve {
    lo: f32,
    step: f32,
    sigma: Vec<f32>,
}

const NOISE_BINS: usize = 64;
/// Muestras mínimas por bin para confiar en su σ.
const NOISE_BIN_MIN: f64 = 100.0;
/// E|N(0,σ)| = σ·√(2/π) ⇒ σ = 1.2533·mean|d|.
const MAD_TO_SIGMA: f64 = 1.253_314;

impl NoiseCurve {
    fn bin_of(&self, level: f32) -> usize {
        if !level.is_finite() {
            return 0;
        }
        (((level - self.lo) / self.step) as isize).clamp(0, NOISE_BINS as isize - 1) as usize
    }

    pub(crate) fn eval(&self, level: f32) -> f32 {
        self.sigma[self.bin_of(level)]
    }

    /// Construye la curva desde los acumuladores por bin (Σ|d| y conteo).
    fn from_bins(lo: f32, step: f32, abs_sum: &[f64], count: &[f64]) -> NoiseCurve {
        let mut sigma = vec![0.0f32; NOISE_BINS];
        // 1. σ por bin con muestras suficientes.
        for b in 0..NOISE_BINS {
            if count[b] >= NOISE_BIN_MIN {
                sigma[b] = (MAD_TO_SIGMA * abs_sum[b] / count[b]) as f32;
            }
        }
        // 2. Bins vacíos heredan el vecino inferior (y el primero, el primer
        //    bin poblado por encima).
        let mut last = None;
        for b in 0..NOISE_BINS {
            if sigma[b] > 0.0 {
                last = Some(sigma[b]);
            } else if let Some(v) = last {
                sigma[b] = v;
            }
        }
        let mut next = None;
        for b in (0..NOISE_BINS).rev() {
            if sigma[b] > 0.0 {
                next = Some(sigma[b]);
            } else if let Some(v) = next {
                sigma[b] = v;
            }
        }
        // 3. Monótona no decreciente (el ruido crece con el nivel) + suelo.
        let mut running = 1e-3f32;
        for b in 0..NOISE_BINS {
            running = running.max(sigma[b].max(1e-3));
            sigma[b] = running;
        }
        NoiseCurve { lo, step, sigma }
    }
}

/// Rango robusto (p0.5–p99.9) del nivel por canal, muestreado de los totales.
fn pilot_level_range(
    total_sum: &[f64],
    total_wgt: &[f64],
    npx: usize,
    ch: usize,
) -> Vec<(f32, f32)> {
    let mut out = Vec::with_capacity(ch);
    let step = (npx / 200_000).max(1);
    for c in 0..ch {
        let mut vals: Vec<f32> = (0..npx)
            .step_by(step)
            .filter_map(|p| {
                let i = p * ch + c;
                if total_wgt[i] > 0.0 {
                    Some((total_sum[i] / total_wgt[i]) as f32)
                } else {
                    None
                }
            })
            .collect();
        if vals.is_empty() {
            out.push((0.0, 1.0));
            continue;
        }
        vals.sort_by(|a, b| a.total_cmp(b));
        let lo = vals[vals.len() / 200];
        let hi = vals[(vals.len() as f64 * 0.999) as usize % vals.len()].max(lo + 1.0);
        out.push((lo, hi));
    }
    out
}

fn cov_reduce(wgt: &[f64], npx: usize, ch: usize) -> Vec<f64> {
    (0..npx)
        .map(|p| {
            let mut s = 0.0f64;
            for c in 0..ch {
                s += wgt[p * ch + c];
            }
            s / ch as f64
        })
        .collect()
}

/// Motor NF-Lite completo: tres pasadas de streaming sobre los frames ya
/// calibrados/registrados/normalizados (escala nativa, drizzle excluido).
pub(crate) fn run_lite(
    ctx: &NfLiteContext,
    load: &dyn Fn(usize) -> Result<crate::DsImage, String>,
    progress: &mut dyn FnMut(&str, usize, usize),
) -> Result<NfLiteOutput, String> {
    let (w_out, h_out, ch) = (ctx.w_out, ctx.h_out, ctx.ch);
    let npx = w_out * h_out;
    let n = ctx.registered.len();
    if n == 0 {
        return Err("NebulaFusion: sin frames registrados".into());
    }

    let mut total_sum = vec![0.0f64; npx * ch];
    let mut total_wgt = vec![0.0f64; npx * ch];
    let mut frame_weights: Vec<FrameWeights> = Vec::with_capacity(n);
    let mut g1g2_offset_max = 0.0f32;

    // --- Pasada A: pesos por frame + totales S=Σw·y, W=Σw ---
    for (k, &(i, t, _fw)) in ctx.registered.iter().enumerate() {
        crate::pipeline::cancellation_checkpoint(ctx.cancel, "NebulaFusion: totales")?;
        progress("pesos y totales", k + 1, n);
        let img = load(i)?;
        let weights = if let Some(cid) = ctx.cfa {
            // CFA directo: 1/σ′² POR CANAL desde los sub-planos Bayer.
            let (sigmas, g_off) = cfa_channel_sigmas(&img, cid);
            g1g2_offset_max = g1g2_offset_max.max(g_off.abs());
            let mut fw = [0.0f64; 3];
            for c in 0..3 {
                let s = (sigmas[c] * ctx.norms[k].0[c].max(1e-6)).max(1e-3) as f64;
                fw[c] = 1.0 / (s * s);
            }
            FrameWeights::Rgb(fw)
        } else {
            let sigma = native_sigma_grid(&img);
            let mul_mean = {
                let m = ctx.norms[k].0;
                if ch == 1 {
                    m[0]
                } else {
                    (m[0] + m[1] + m[2]) / 3.0
                }
            };
            FrameWeights::Grid(reference_weight_grid(
                &sigma, &t, mul_mean, img.w, img.h, w_out, h_out,
            ))
        };
        nf_accumulate(
            ctx,
            k,
            t,
            &img,
            &weights,
            &mut total_sum,
            &mut total_wgt,
            None,
            None,
        );
        frame_weights.push(weights);
    }
    let wgt1 = cov_reduce(&total_wgt, npx, ch);
    let total_cov: f64 = wgt1.iter().sum();

    let cfg = crate::deepsky_masks::CrossFitConfig {
        dilate_px: if ctx.use_lanczos { 2 } else { 1 },
        ..crate::deepsky_masks::CrossFitConfig::default()
    };
    let crossfit_enabled = n >= cfg.min_frames;
    let mut frame_sum = vec![0.0f64; npx * ch];
    let mut frame_wgt = vec![0.0f64; npx * ch];
    // Warp de la aportación de UN frame (reutiliza los buffers de arriba).
    let frame_weights_ref = &frame_weights;
    let warp_frame = |k: usize,
                      i: usize,
                      t: crate::DsTransform,
                      fs: &mut Vec<f64>,
                      fw: &mut Vec<f64>|
     -> Result<crate::DsImage, String> {
        let img = load(i)?;
        fs.iter_mut().for_each(|v| *v = 0.0);
        fw.iter_mut().for_each(|v| *v = 0.0);
        nf_accumulate(ctx, k, t, &img, &frame_weights_ref[k], fs, fw, None, None);
        Ok(img)
    };

    // --- Pasada B1: curva ruido-vs-nivel por canal (residuales LOO) ---
    let ranges = pilot_level_range(&total_sum, &total_wgt, npx, ch);
    let mut abs_bins = vec![vec![0.0f64; NOISE_BINS]; ch];
    let mut cnt_bins = vec![vec![0.0f64; NOISE_BINS]; ch];
    if crossfit_enabled {
        for (k, &(i, t, _fw)) in ctx.registered.iter().enumerate() {
            crate::pipeline::cancellation_checkpoint(ctx.cancel, "NebulaFusion: curva de ruido")?;
            progress("curva de ruido", k + 1, n);
            warp_frame(k, i, t, &mut frame_sum, &mut frame_wgt)?;
            for c in 0..ch {
                let (lo, hi) = ranges[c];
                let step = ((hi - lo) / NOISE_BINS as f32).max(1e-6);
                for p in 0..npx {
                    let idx = p * ch + c;
                    let wi = frame_wgt[idx];
                    let wrest = total_wgt[idx] - wi;
                    if wi <= 0.0 || wrest <= 0.0 {
                        continue;
                    }
                    let y = frame_sum[idx] / wi;
                    let pilot = (total_sum[idx] - frame_sum[idx]) / wrest;
                    let b = (((pilot as f32 - lo) / step) as isize)
                        .clamp(0, NOISE_BINS as isize - 1) as usize;
                    abs_bins[c][b] += (y - pilot).abs();
                    cnt_bins[c][b] += 1.0;
                }
            }
        }
    }
    let curves: Vec<NoiseCurve> = (0..ch)
        .map(|c| {
            let (lo, hi) = ranges[c];
            let step = ((hi - lo) / NOISE_BINS as f32).max(1e-6);
            NoiseCurve::from_bins(lo, step, &abs_bins[c], &cnt_bins[c])
        })
        .collect();

    // Residual normalizado del frame contra un piloto (peor canal, signo).
    let residual_plane = |fs: &[f64],
                          fw: &[f64],
                          pilot_sum: &[f64],
                          pilot_wgt: &[f64],
                          exclude_self: bool,
                          out: &mut Vec<f32>| {
        out.iter_mut().for_each(|v| *v = 0.0);
        for p in 0..npx {
            let mut worst = 0.0f64;
            for c in 0..ch {
                let idx = p * ch + c;
                let wi = fw[idx];
                if wi <= 0.0 {
                    continue;
                }
                let (ps, pw) = if exclude_self {
                    (pilot_sum[idx] - fs[idx], pilot_wgt[idx] - wi)
                } else {
                    (pilot_sum[idx], pilot_wgt[idx])
                };
                if pw <= 0.0 {
                    continue;
                }
                let y = fs[idx] / wi;
                let pilot = ps / pw;
                let sigma = curves[c].eval(pilot as f32) as f64;
                let r = (y - pilot) / sigma.max(1e-6);
                if r.abs() > worst.abs() {
                    worst = r;
                }
            }
            out[p] = worst as f32;
        }
    };

    // --- Pasada B2: máscaras ronda 1 (piloto contaminable) + totales limpios ---
    // El piloto LOO medio se contamina con el propio outlier visto desde los
    // DEMÁS frames (un cósmico desplaza el piloto y todos parecen desviados).
    // Por eso la ronda 1 solo separa candidatos y construye unos totales
    // LIMPIOS; la ronda 2 revalida cada máscara contra el piloto limpio.
    let mut masks: Vec<crate::deepsky_masks::FrozenFrameMask> = Vec::with_capacity(n);
    let mut clean_sum = vec![0.0f64; npx * ch];
    let mut clean_wgt = vec![0.0f64; npx * ch];
    let mut residual = vec![0.0f32; npx];
    for (k, &(i, t, _fw)) in ctx.registered.iter().enumerate() {
        crate::pipeline::cancellation_checkpoint(ctx.cancel, "NebulaFusion: máscaras r1")?;
        progress("máscaras (ronda 1)", k + 1, n);
        if !crossfit_enabled {
            masks.push(crate::deepsky_masks::FrozenFrameMask {
                positive: Vec::new(),
                negative: Vec::new(),
            });
            continue;
        }
        let img = warp_frame(k, i, t, &mut frame_sum, &mut frame_wgt)?;
        residual_plane(
            &frame_sum,
            &frame_wgt,
            &total_sum,
            &total_wgt,
            true,
            &mut residual,
        );
        let mask =
            crate::deepsky_masks::build_frozen_mask(&residual, w_out, h_out, n, &cfg, false);
        // Acumular este frame en los totales limpios saltando su máscara r1.
        let bits;
        let mask_ref = if mask.is_empty() {
            None
        } else {
            bits = mask.to_bitset(npx);
            Some(bits.as_slice())
        };
        nf_accumulate(
            ctx,
            k,
            t,
            &img,
            &frame_weights_ref[k],
            &mut clean_sum,
            &mut clean_wgt,
            mask_ref,
            None,
        );
        masks.push(mask);
    }
    if !crossfit_enabled {
        clean_sum.copy_from_slice(&total_sum);
        clean_wgt.copy_from_slice(&total_wgt);
    }

    // --- Pasada B3: revalidación contra el piloto limpio + dilatación ---
    let mut rejection_low = vec![0.0f64; npx];
    let mut rejection_high = vec![0.0f64; npx];
    let mut masked_samples = 0usize;
    if crossfit_enabled {
        for (k, &(i, t, _fw)) in ctx.registered.iter().enumerate() {
            crate::pipeline::cancellation_checkpoint(ctx.cancel, "NebulaFusion: máscaras r2")?;
            progress("máscaras (ronda 2)", k + 1, n);
            warp_frame(k, i, t, &mut frame_sum, &mut frame_wgt)?;
            // Piloto limpio EXCLUYENDO la aportación limpia de este frame:
            // donde el frame estaba enmascarado en r1, su aporte a los
            // totales limpios ya es cero.
            let bits_r1 = masks[k].to_bitset(npx);
            for p in 0..npx {
                residual[p] = 0.0;
                let self_in_clean = (bits_r1[p >> 6] >> (p & 63)) & 1 == 0;
                for c in 0..ch {
                    let idx = p * ch + c;
                    let wi = frame_wgt[idx];
                    if wi <= 0.0 {
                        continue;
                    }
                    let (ps, pw) = if self_in_clean {
                        (clean_sum[idx] - frame_sum[idx], clean_wgt[idx] - wi)
                    } else {
                        (clean_sum[idx], clean_wgt[idx])
                    };
                    if pw <= 0.0 {
                        continue;
                    }
                    let y = frame_sum[idx] / wi;
                    let pilot = ps / pw;
                    let sigma = curves[c].eval(pilot as f32) as f64;
                    let r = ((y - pilot) / sigma.max(1e-6)) as f32;
                    if r.abs() > residual[p].abs() {
                        residual[p] = r;
                    }
                }
            }
            let mask =
                crate::deepsky_masks::build_frozen_mask(&residual, w_out, h_out, n, &cfg, true);
            for &p in &mask.positive {
                let p = p as usize;
                let mut wsum = 0.0f64;
                for c in 0..ch {
                    wsum += frame_wgt[p * ch + c];
                }
                rejection_high[p] += wsum / ch as f64;
            }
            for &p in &mask.negative {
                let p = p as usize;
                let mut wsum = 0.0f64;
                for c in 0..ch {
                    wsum += frame_wgt[p * ch + c];
                }
                rejection_low[p] += wsum / ch as f64;
            }
            masked_samples += mask.len();
            masks[k] = mask;
        }
    }

    // --- Pasada C: integración final con máscaras congeladas + Σw² ---
    total_sum.iter_mut().for_each(|v| *v = 0.0);
    total_wgt.iter_mut().for_each(|v| *v = 0.0);
    let mut weight_sq = vec![0.0f64; npx * ch];
    for (k, &(i, t, _fw)) in ctx.registered.iter().enumerate() {
        crate::pipeline::cancellation_checkpoint(ctx.cancel, "NebulaFusion: integración")?;
        progress("integración final", k + 1, n);
        let img = load(i)?;
        let bits;
        let mask_ref = if masks[k].is_empty() {
            None
        } else {
            bits = masks[k].to_bitset(npx);
            Some(bits.as_slice())
        };
        nf_accumulate(
            ctx,
            k,
            t,
            &img,
            &frame_weights_ref[k],
            &mut total_sum,
            &mut total_wgt,
            mask_ref,
            Some(&mut weight_sq),
        );
    }

    // --- Productos ---
    let mut final_data = vec![0.0f32; npx * ch];
    let mut variance = vec![f32::NAN; npx * ch];
    let mut neff = vec![0.0f32; npx * ch];
    let mut dq = vec![0u32; npx];
    for p in 0..npx {
        let mut covered = false;
        for c in 0..ch {
            let i = p * ch + c;
            let wv = total_wgt[i];
            if wv > 0.0 {
                covered = true;
                final_data[i] = (total_sum[i] / wv) as f32;
                variance[i] = (1.0 / wv) as f32;
                if weight_sq[i] > 0.0 {
                    neff[i] = ((wv * wv) / weight_sq[i]) as f32;
                }
            }
        }
        if !covered {
            dq[p] |= crate::deepsky_variance::dq::NO_COVERAGE;
        }
    }
    let weight_map = cov_reduce(&total_wgt, npx, ch);
    let final_cov: f64 = weight_map.iter().sum();
    let rej_pct = if total_cov > 0.0 {
        100.0 * (1.0 - final_cov / total_cov)
    } else {
        0.0
    };
    let mean_cov = if total_cov > 0.0 {
        n as f64 * final_cov / total_cov
    } else {
        0.0
    };

    Ok(NfLiteOutput {
        final_data,
        wgt1,
        weight_map,
        rejection_low,
        rejection_high,
        rej_pct,
        mean_cov,
        products: NfLiteProducts {
            variance,
            neff,
            dq,
            masked_samples,
            variance_origin: crate::deepsky_variance::VarianceOrigin::Empirical,
            g1g2_offset_max: ctx.cfa.map(|_| g1g2_offset_max),
        },
    })
}

/// Recorte de un plano interleaved f32 (layout del máster) al rectángulo del
/// auto-crop — para VAR/NEFF, que comparten geometría con el máster.
pub(crate) fn crop_interleaved_f32(
    data: &[f32],
    w: usize,
    _h: usize,
    ch: usize,
    x0: usize,
    y0: usize,
    nw: usize,
    nh: usize,
) -> Vec<f32> {
    let mut out = Vec::with_capacity(nw * nh * ch);
    for y in 0..nh {
        let src = ((y0 + y) * w + x0) * ch;
        out.extend_from_slice(&data[src..src + nw * ch]);
    }
    out
}

/// Recorte del plano DQ (u32 por píxel).
pub(crate) fn crop_plane_u32(
    data: &[u32],
    w: usize,
    x0: usize,
    y0: usize,
    nw: usize,
    nh: usize,
) -> Vec<u32> {
    let mut out = Vec::with_capacity(nw * nh);
    for y in 0..nh {
        let src = (y0 + y) * w + x0;
        out.extend_from_slice(&data[src..src + nw]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_registered(n: usize) -> Vec<(usize, crate::DsTransform, f64)> {
        (0..n)
            .map(|i| (i, crate::DsTransform::from_similarity((1.0, 0.0, 0.0, 0.0)), 1.0))
            .collect()
    }

    fn neutral_norms(n: usize) -> Vec<([f32; 3], [f32; 3])> {
        vec![([1.0, 1.0, 1.0], [0.0, 0.0, 0.0]); n]
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn flat_sensor(read_noise_e: f64) -> crate::deepsky_sim::SimSensor {
        crate::deepsky_sim::SimSensor {
            gain_e_per_adu: 1.0,
            read_noise_e,
            bias_adu: 0.0,
            dark_adu_per_s: 0.0,
            full_well_adu: 65535.0,
            hot_pixels: Vec::new(),
            bayer: None,
            vignette: None,
        }
    }

    fn run_on_frames(frames: Vec<Vec<f32>>, w: usize, h: usize) -> NfLiteOutput {
        let n = frames.len();
        let registered = identity_registered(n);
        let norms = neutral_norms(n);
        let loc: Vec<Option<Vec<f32>>> = vec![None; n];
        let cancel = no_cancel();
        let ctx = NfLiteContext {
            registered: &registered,
            norms: &norms,
            loc_fields: &loc,
            loc_grid: 24,
            w_out: w,
            h_out: h,
            ch: 1,
            use_lanczos: false,
            cancel: &cancel,
            cfa: None,
        };
        let load = |i: usize| -> Result<crate::DsImage, String> {
            Ok(crate::DsImage {
                data: frames[i].clone(),
                w,
                h,
                ch: 1,
                bayer: None,
            })
        };
        run_lite(&ctx, &load, &mut |_, _, _| {}).expect("run_lite")
    }

    /// Gate F3: SNR de fondo ≥98% del óptimo 1/σ². Dos poblaciones de ruido
    /// (σ=3 y σ=12): la media óptima tiene Var=1/Σ(1/σ²); la salida NF-Lite
    /// no puede superar Var_opt/0.98² (y debe batir a la media uniforme).
    #[test]
    fn gate_f3_background_snr_at_least_98pct_of_optimal() {
        let (w, h) = (160, 160);
        let scene = crate::deepsky_sim::SimScene {
            width: w,
            height: h,
            background_adu: 400.0,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0, 1.0, 1.0],
            stars: Vec::new(),
        };
        let mut frames = Vec::new();
        let mut inv_var_sum = 0.0f64;
        let mut var_sum = 0.0f64;
        for k in 0..12 {
            // Frames 0-5: σ_lectura=3 (σ²≈409); 6-11: σ_lectura=25 (σ²≈1025).
            let rn = if k < 6 { 3.0 } else { 25.0 };
            let sensor = flat_sensor(rn);
            let exp = crate::deepsky_sim::SimExposure {
                exposure_s: 60.0,
                dx: 0.0,
                dy: 0.0,
                seed: 7_000 + k as u64,
            };
            let (frame, tv) = crate::deepsky_sim::render_light(&scene, &sensor, &exp);
            let mean_var = tv.iter().sum::<f64>() / tv.len() as f64;
            inv_var_sum += 1.0 / mean_var;
            var_sum += mean_var;
            frames.push(frame);
        }
        let optimal_var = 1.0 / inv_var_sum;
        let uniform_var = var_sum / (12.0f64 * 12.0);
        let out = run_on_frames(frames, w, h);
        // Varianza espacial del fondo de salida (escena plana ⇒ toda la
        // dispersión es ruido; excluye 4 px de borde).
        let mut vals = Vec::new();
        for y in 4..h - 4 {
            for x in 4..w - 4 {
                vals.push(out.final_data[y * w + x] as f64);
            }
        }
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (vals.len() as f64 - 1.0);
        assert!(
            var < optimal_var / (0.98 * 0.98),
            "Var salida {var:.2} > óptimo/0.98² ({:.2})",
            optimal_var / (0.98 * 0.98)
        );
        // Con σ² de 409 vs 1025 el óptimo teórico es ~0.81× la media uniforme:
        // exigir mejora real (<0.9×), no un margen imposible para el dataset.
        assert!(
            var < uniform_var * 0.9,
            "NF-Lite ({var:.2}) no mejora la media uniforme ({uniform_var:.2})"
        );
        // VAR reportada coherente con la varianza real (±15%).
        let center = (h / 2) * w + w / 2;
        let reported = out.products.variance[center] as f64;
        assert!(
            (reported / var - 1.0).abs() < 0.15,
            "VAR reportada {reported:.2} vs medida {var:.2}"
        );
    }

    /// Gate F3: cósmicos y traza de satélite inyectados — recall >95%,
    /// falso rechazo de píxeles limpios <1e-4, precisión >99% contando la
    /// dilatación por interpolación como acierto (radio 2 del inyectado).
    #[test]
    fn gate_f3_crossfit_rejection_precision_and_recall() {
        let (w, h) = (128, 128);
        let scene = crate::deepsky_sim::SimScene {
            width: w,
            height: h,
            background_adu: 300.0,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0, 1.0, 1.0],
            stars: Vec::new(),
        };
        let sensor = flat_sensor(3.0);
        let mut frames = Vec::new();
        let mut injected: Vec<(usize, usize)> = Vec::new(); // (frame, px)
        for k in 0..10 {
            let exp = crate::deepsky_sim::SimExposure {
                exposure_s: 60.0,
                dx: 0.0,
                dy: 0.0,
                seed: 9_500 + k as u64,
            };
            let (mut frame, _) = crate::deepsky_sim::render_light(&scene, &sensor, &exp);
            if k == 2 {
                // 12 cósmicos sueltos de +2000 ADU (~100σ).
                for j in 0..12 {
                    let p = (10 + j * 9) * w + (15 + j * 8);
                    frame[p] += 2000.0;
                    injected.push((k, p));
                }
            }
            if k == 6 {
                // Traza de satélite: diagonal de +400 ADU (~20σ).
                for s in 20..100 {
                    let p = s * w + s;
                    frame[p] += 400.0;
                    injected.push((k, p));
                }
            }
            frames.push(frame);
        }
        let n = frames.len();
        let registered = identity_registered(n);
        let norms = neutral_norms(n);
        let loc: Vec<Option<Vec<f32>>> = vec![None; n];
        let cancel = no_cancel();
        let ctx = NfLiteContext {
            registered: &registered,
            norms: &norms,
            loc_fields: &loc,
            loc_grid: 24,
            w_out: w,
            h_out: h,
            ch: 1,
            use_lanczos: false,
            cancel: &cancel,
            cfa: None,
        };
        let frames_ref = &frames;
        let load = |i: usize| -> Result<crate::DsImage, String> {
            Ok(crate::DsImage {
                data: frames_ref[i].clone(),
                w,
                h,
                ch: 1,
                bayer: None,
            })
        };
        let out = run_lite(&ctx, &load, &mut |_, _, _| {}).expect("run_lite");
        // Recall: cada píxel inyectado debe estar rechazado (mapa alto > 0).
        let hit = injected
            .iter()
            .filter(|&&(_, p)| out.rejection_high[p] > 0.0)
            .count();
        let recall = hit as f64 / injected.len() as f64;
        assert!(recall > 0.95, "recall {recall:.3} <= 0.95");
        // Falso rechazo: rechazos lejos (>3 px) de cualquier inyección.
        let mut injected_map = vec![false; w * h];
        for &(_, p) in &injected {
            let (px, py) = (p % w, p / w);
            for dy in -3i64..=3 {
                for dx in -3i64..=3 {
                    let nx = px as i64 + dx;
                    let ny = py as i64 + dy;
                    if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                        injected_map[ny as usize * w + nx as usize] = true;
                    }
                }
            }
        }
        let mut false_rejected = 0usize;
        let mut rejected_total = 0usize;
        for p in 0..w * h {
            if out.rejection_high[p] > 0.0 || out.rejection_low[p] > 0.0 {
                rejected_total += 1;
                if !injected_map[p] {
                    false_rejected += 1;
                }
            }
        }
        let false_rate = false_rejected as f64 / (w * h) as f64;
        assert!(false_rate < 1e-4, "falso rechazo {false_rate:.2e}");
        let precision = 1.0 - false_rejected as f64 / rejected_total.max(1) as f64;
        assert!(precision > 0.99, "precisión {precision:.4}");
        // El máster no conserva el cósmico: el valor queda cerca del fondo.
        let sample = injected[0].1;
        assert!(
            (out.final_data[sample] - 300.0).abs() < 30.0,
            "el cósmico sobrevivió: {}",
            out.final_data[sample]
        );
    }

    /// Gate F3: fotometría — el flujo de una estrella se conserva 1±0.005.
    #[test]
    fn gate_f3_photometry_slope_within_half_percent() {
        let (w, h) = (96, 96);
        let star = crate::deepsky_sim::SimStar {
            x: 48.0,
            y: 48.0,
            flux_adu: 50_000.0,
            fwhm_px: 3.0,
            moffat_beta: None,
        };
        let scene = crate::deepsky_sim::SimScene {
            width: w,
            height: h,
            background_adu: 200.0,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0, 1.0, 1.0],
            stars: vec![star],
        };
        let sensor = flat_sensor(3.0);
        let mut frames = Vec::new();
        for k in 0..10 {
            let exp = crate::deepsky_sim::SimExposure {
                exposure_s: 60.0,
                dx: 0.0,
                dy: 0.0,
                seed: 21_000 + k as u64,
            };
            frames.push(crate::deepsky_sim::render_light(&scene, &sensor, &exp).0);
        }
        let out = run_on_frames(frames, w, h);
        // Apertura radio 12 px (4·FWHM): flujo = Σ(v − fondo).
        let mut flux = 0.0f64;
        for y in 0..h {
            for x in 0..w {
                let dx = x as f64 - 48.0;
                let dy = y as f64 - 48.0;
                if dx * dx + dy * dy <= 144.0 {
                    flux += out.final_data[y * w + x] as f64 - 200.0;
                }
            }
        }
        let ratio = flux / 50_000.0;
        assert!(
            (ratio - 1.0).abs() < 0.005,
            "pendiente de flujo {ratio:.4} fuera de 1±0.005"
        );
    }

    /// Gate F4: CFA directo — cociente de canales sin sesgo (<1%) y SIN
    /// patrón 2×2 residual en la salida (las medias por fase de paridad de
    /// cada canal coinciden). 8 frames RGGB con dithers que cubren las 4
    /// paridades para que cada canal tenga cobertura completa.
    #[test]
    fn gate_f4_cfa_channel_ratio_and_no_mosaic_pattern() {
        let (w, h) = (96, 96);
        let color = [0.8f64, 1.0, 0.6];
        let scene = crate::deepsky_sim::SimScene {
            width: w,
            height: h,
            background_adu: 400.0,
            gradient_adu_per_px: (0.0, 0.0),
            color,
            stars: Vec::new(),
        };
        let sensor = crate::deepsky_sim::SimSensor {
            gain_e_per_adu: 1.0,
            read_noise_e: 3.0,
            bias_adu: 0.0,
            dark_adu_per_s: 0.0,
            full_well_adu: 65535.0,
            hot_pixels: Vec::new(),
            bayer: Some(8), // RGGB
            vignette: None,
        };
        let dithers = [(0.0f32, 0.0f32), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)];
        let mut frames = Vec::new();
        let mut registered = Vec::new();
        for k in 0..8usize {
            let exp = crate::deepsky_sim::SimExposure {
                exposure_s: 60.0,
                dx: 0.0,
                dy: 0.0,
                seed: 51_000 + k as u64,
            };
            frames.push(crate::deepsky_sim::render_light(&scene, &sensor, &exp).0);
            let (dx, dy) = dithers[k % 4];
            registered.push((
                k,
                crate::DsTransform::from_similarity((1.0, 0.0, dx, dy)),
                1.0f64,
            ));
        }
        let norms = neutral_norms(8);
        let loc: Vec<Option<Vec<f32>>> = vec![None; 8];
        let cancel = no_cancel();
        let ctx = NfLiteContext {
            registered: &registered,
            norms: &norms,
            loc_fields: &loc,
            loc_grid: 24,
            w_out: w,
            h_out: h,
            ch: 3,
            use_lanczos: false,
            cancel: &cancel,
            cfa: Some(8),
        };
        let frames_ref = &frames;
        let load = |i: usize| -> Result<crate::DsImage, String> {
            Ok(crate::DsImage {
                data: frames_ref[i].clone(),
                w,
                h,
                ch: 1,
                bayer: Some(8),
            })
        };
        let out = run_lite(&ctx, &load, &mut |_, _, _| {}).expect("run_lite CFA");
        // Cociente de canales: media interior por canal vs verdad 400·color.
        let mut ch_mean = [0.0f64; 3];
        let mut ch_cnt = [0.0f64; 3];
        // Medias por fase de paridad 2×2 (patrón mosaico residual).
        let mut phase_mean = [[0.0f64; 4]; 3];
        let mut phase_cnt = [[0.0f64; 4]; 3];
        for y in 4..h - 4 {
            for x in 4..w - 4 {
                let p = y * w + x;
                for c in 0..3 {
                    let v = out.final_data[p * 3 + c] as f64;
                    if !v.is_finite() || v == 0.0 {
                        continue;
                    }
                    ch_mean[c] += v;
                    ch_cnt[c] += 1.0;
                    let phase = (y & 1) * 2 + (x & 1);
                    phase_mean[c][phase] += v;
                    phase_cnt[c][phase] += 1.0;
                }
            }
        }
        for c in 0..3 {
            let mean = ch_mean[c] / ch_cnt[c].max(1.0);
            let truth = 400.0 * color[c];
            assert!(
                (mean / truth - 1.0).abs() < 0.01,
                "canal {c}: media {mean:.2} vs verdad {truth:.2} (sesgo >1%)"
            );
            let phases: Vec<f64> = (0..4)
                .map(|ph| phase_mean[c][ph] / phase_cnt[c][ph].max(1.0))
                .collect();
            let pmax = phases.iter().cloned().fold(f64::MIN, f64::max);
            let pmin = phases.iter().cloned().fold(f64::MAX, f64::min);
            assert!(
                (pmax - pmin) / mean < 0.015,
                "canal {c}: patrón CFA residual {:.3}% entre fases 2×2",
                100.0 * (pmax - pmin) / mean
            );
        }
        assert_eq!(out.products.dq.len(), w * h);
        assert!(out.products.g1g2_offset_max.is_some());
    }

    /// Gate F4: super-binning — brillo de superficie conservado, varianza
    /// propagada VAR/4 en 0.5x, flujo de apertura escala con el área del
    /// píxel (unidad declarada en receta), dimensiones correctas en 0.75x.
    #[test]
    fn gate_f4_superbinning_flux_and_variance() {
        let (w, h) = (64, 64);
        let mut sci = vec![100.0f32; w * h];
        // "Estrella": bloque 3×3 de +100 ADU (flujo 900 sobre fondo).
        for dy in 0..3 {
            for dx in 0..3 {
                sci[(30 + dy) * w + 30 + dx] += 100.0;
            }
        }
        let var = vec![4.0f32; w * h];
        let (b_sci, nw, nh) = bin_area_f32(&sci, w, h, 1, 1, 2, false);
        assert_eq!((nw, nh), (32, 32));
        let (b_var, _, _) = bin_area_f32(&var, w, h, 1, 1, 2, true);
        // Fondo conservado (brillo de superficie) y VAR = 4·(1²·4)/4² = 1.
        assert!((b_sci[0] - 100.0).abs() < 1e-4);
        assert!((b_var[0] - 1.0).abs() < 1e-4);
        // Flujo de apertura: Σ(v−fondo) escala por el área del píxel (1/4).
        let flux_in: f64 = sci.iter().map(|&v| (v - 100.0) as f64).sum();
        let flux_out: f64 = b_sci.iter().map(|&v| (v - 100.0) as f64).sum();
        assert!(
            (flux_out / (flux_in * 0.25) - 1.0).abs() < 0.01,
            "flujo binned {flux_out:.1} vs esperado {:.1}",
            flux_in * 0.25
        );
        // 0.75x: dimensiones 3/4.
        let (_, nw75, nh75) = bin_area_f32(&sci, w, h, 1, 3, 4, false);
        assert_eq!((nw75, nh75), (48, 48));
        // DQ: NO_COVERAGE solo si todo el bloque lo lleva.
        let mut dq = vec![0u32; w * h];
        dq[0] = crate::deepsky_variance::dq::NO_COVERAGE;
        dq[1] = crate::deepsky_variance::dq::NO_COVERAGE;
        dq[w] = crate::deepsky_variance::dq::NO_COVERAGE;
        dq[w + 1] = crate::deepsky_variance::dq::NO_COVERAGE;
        dq[2] = crate::deepsky_variance::dq::HOT_COLD;
        let b_dq = bin_dq(&dq, w, h, 1, 2);
        assert_eq!(b_dq[0], crate::deepsky_variance::dq::NO_COVERAGE);
        assert_eq!(b_dq[1] & crate::deepsky_variance::dq::HOT_COLD, crate::deepsky_variance::dq::HOT_COLD);
        assert_eq!(b_dq[1] & crate::deepsky_variance::dq::NO_COVERAGE, 0);
    }

    /// Auditoría G1/G2: un offset inyectado entre los dos sub-planos verdes
    /// se mide con el signo/magnitud correctos.
    #[test]
    fn test_cfa_channel_sigmas_measures_g1g2_offset() {
        let (w, h) = (64, 64);
        let mut data = vec![0.0f32; w * h];
        // RGGB (cid 8): R=(par,par), G1=(impar,par), G2=(par,impar), B=(impar,impar).
        let mut lcg = 12345u64;
        let mut noise = || {
            lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((lcg >> 33) as f32 / (1u64 << 32) as f32) - 0.5
        };
        for y in 0..h {
            for x in 0..w {
                let base = match (x & 1, y & 1) {
                    (0, 0) => 100.0,        // R
                    (1, 0) => 200.0,        // G1
                    (0, 1) => 210.0,        // G2 (offset +10)
                    _ => 50.0,              // B
                };
                data[y * w + x] = base + noise();
            }
        }
        let img = crate::DsImage {
            data,
            w,
            h,
            ch: 1,
            bayer: Some(8),
        };
        let (sigmas, g_off) = cfa_channel_sigmas(&img, 8);
        assert!(
            g_off.abs() > 9.0 && g_off.abs() < 11.0,
            "offset G1G2 {g_off:.2} fuera de ±[9,11]"
        );
        assert!(sigmas.iter().all(|&s| s > 0.0 && s < 5.0));
    }

    /// VAR y NEFF con frames idénticos en ruido: VAR≈σ²/N (±10%), NEFF≈N.
    #[test]
    fn test_var_and_neff_for_equal_frames() {
        let (w, h) = (96, 96);
        let scene = crate::deepsky_sim::SimScene {
            width: w,
            height: h,
            background_adu: 250.0,
            gradient_adu_per_px: (0.0, 0.0),
            color: [1.0, 1.0, 1.0],
            stars: Vec::new(),
        };
        let sensor = flat_sensor(4.0);
        let n = 8usize;
        let mut frames = Vec::new();
        let mut true_var = 0.0f64;
        for k in 0..n {
            let exp = crate::deepsky_sim::SimExposure {
                exposure_s: 60.0,
                dx: 0.0,
                dy: 0.0,
                seed: 31_000 + k as u64,
            };
            let (f, tv) = crate::deepsky_sim::render_light(&scene, &sensor, &exp);
            true_var += tv.iter().sum::<f64>() / tv.len() as f64;
            frames.push(f);
        }
        true_var /= n as f64;
        let out = run_on_frames(frames, w, h);
        let center = (h / 2) * w + w / 2;
        let var = out.products.variance[center] as f64;
        let expected = true_var / n as f64;
        assert!(
            (var / expected - 1.0).abs() < 0.10,
            "VAR {var:.2} vs esperado {expected:.2}"
        );
        let neff = out.products.neff[center] as f64;
        assert!(
            (neff - n as f64).abs() < 0.5,
            "NEFF {neff:.2} vs {n}"
        );
        assert_eq!(out.products.dq[center], 0);
    }
}
