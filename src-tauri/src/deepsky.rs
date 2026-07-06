// ==========================================
// 9. DEEP-SKY STACKING MODULE (Cielo Profundo)
// ==========================================
// Automated-but-tunable pipeline taking the proven ideas from SIRIL/WBPP:
//   1. CALIBRATION   — master bias/dark/flat (mean-combined); light is
//      bias/dark-subtracted and flat-divided (flat normalized to mean 1).
//   2. STAR DETECTION — background (median) + MAD noise threshold, local
//      maxima with hot-pixel rejection, 7×7 centroid, flux-ranked.
//   3. REGISTRATION  — triangle-similarity voting on the brightest stars
//      (rotation + scale + translation), least-squares fit refined on
//      inliers. Frames that fail to register are skipped, not stacked.
//   4. INTEGRATION   — two-pass kappa-sigma clipped mean per pixel (the same
//      statistical machinery validated in the planetary stacker): satellites,
//      planes and cosmic rays are rejected; clean frames keep full weight.
//   5. AUTOSTRETCH   — STF preview (PixInsight-style midtones transfer
//      function). The STORED result stays LINEAR 16-bit and flows into the
//      existing post-processing pipeline (wavelets/deconv/color) untouched.
//
// Reference frame, alignment and stretch are fully automatic — the flow needs
// no tutorial. Kappa and sigma-clip remain user-tunable for control.

/// Linear image buffer in 16-bit scale (f32 for headroom), `ch`-interleaved.
/// `bayer` is Some(cid) while the frame is still a RAW CFA mono plane (ch==1)
/// that must be calibrated BEFORE debayering — None once it is true RGB/mono.
#[derive(Clone)]
struct DsImage {
    data: Vec<f32>,
    w: usize,
    h: usize,
    ch: usize, // 1 = mono, 3 = RGB
    bayer: Option<i32>,
}

/// FITS BAYERPAT header → the app's Bayer color id (RGGB=8 GRBG=9 GBRG=10 BGGR=11).
fn ds_bayer_id(hdu: &fitrs::Hdu) -> Option<i32> {
    let raw = format!("{:?}", hdu.value("BAYERPAT")?).to_uppercase();
    if raw.contains("RGGB") {
        Some(8)
    } else if raw.contains("GRBG") {
        Some(9)
    } else if raw.contains("GBRG") {
        Some(10)
    } else if raw.contains("BGGR") {
        Some(11)
    } else {
        None
    }
}

fn ds_read_image(path: &str) -> Result<DsImage, String> {
    let lower = path.to_lowercase();
    if lower.ends_with(".fits") || lower.ends_with(".fit") {
        let fits = fitrs::Fits::open(path).map_err(|e| format!("FITS open: {:?}", e))?;
        let hdu = fits.iter().next().ok_or("FITS sin HDU primario")?;
        // OSC cameras store a CFA mono plane + BAYERPAT — read it BEFORE the
        // pixel data so the mono branch can debayer to real color.
        let bayer_id = ds_bayer_id(&hdu);
        let (shape, data): (Vec<usize>, Vec<f32>) = match hdu.read_data() {
            fitrs::FitsData::IntegersI32(arr) => (
                arr.shape.clone(),
                arr.data
                    .iter()
                    .map(|v| v.unwrap_or(0).clamp(0, 65535) as f32)
                    .collect(),
            ),
            fitrs::FitsData::IntegersU32(arr) => (
                arr.shape.clone(),
                arr.data
                    .iter()
                    .map(|v| v.unwrap_or(0).min(65535) as f32)
                    .collect(),
            ),
            fitrs::FitsData::FloatingPoint32(arr) => {
                // SIRIL/PI floats are 0..1; some tools store 0..65535 — detect.
                let maxv = arr.data.iter().cloned().fold(0.0f32, f32::max);
                let scale = if maxv <= 2.0 { 65535.0 } else { 1.0 };
                (
                    arr.shape.clone(),
                    arr.data
                        .iter()
                        .map(|v| (v * scale).clamp(0.0, 65535.0))
                        .collect(),
                )
            }
            fitrs::FitsData::FloatingPoint64(arr) => {
                let maxv = arr.data.iter().cloned().fold(0.0f64, f64::max);
                let scale = if maxv <= 2.0 { 65535.0 } else { 1.0 };
                (
                    arr.shape.clone(),
                    arr.data
                        .iter()
                        .map(|v| ((v * scale).clamp(0.0, 65535.0)) as f32)
                        .collect(),
                )
            }
            fitrs::FitsData::Characters(arr) => (
                arr.shape.clone(),
                arr.data.iter().map(|c| (*c as u8 as f32) * 257.0).collect(),
            ),
        };
        if shape.len() < 2 {
            return Err("FITS con forma inválida".into());
        }
        let w = shape[0];
        let h = shape[1];
        let ch = if shape.len() >= 3 { shape[2].clamp(1, 3) } else { 1 };
        if w * h * ch > data.len() {
            return Err(format!("FITS truncado: {}x{}x{}", w, h, ch));
        }
        if ch == 1 {
            // OSC raw CFA: keep it MONO and tag the pattern. Debayer happens
            // AFTER calibration (correct order — calibrating post-debayer left
            // vignetting/green because the interpolation already mixed the CFA).
            Ok(DsImage { data, w, h, ch: 1, bayer: bayer_id })
        } else {
            // FITS planar (RRR..GGG..BBB) → interleaved RGB.
            let plane = w * h;
            let mut inter = vec![0.0f32; plane * 3];
            for i in 0..plane {
                inter[i * 3] = data[i];
                inter[i * 3 + 1] = data[plane + i];
                inter[i * 3 + 2] = data[plane * 2 + i];
            }
            Ok(DsImage { data: inter, w, h, ch: 3, bayer: None })
        }
    } else {
        // TIF / PNG / JPG via the image crate (16-bit aware).
        let img = image::open(path).map_err(|e| format!("Imagen: {}", e))?;
        match img {
            image::DynamicImage::ImageLuma16(g) => {
                let (w, h) = (g.width() as usize, g.height() as usize);
                Ok(DsImage {
                    data: g.into_raw().into_iter().map(|v| v as f32).collect(),
                    w,
                    h,
                    ch: 1,
                    bayer: None,
                })
            }
            image::DynamicImage::ImageLuma8(g) => {
                let (w, h) = (g.width() as usize, g.height() as usize);
                Ok(DsImage {
                    data: g.into_raw().into_iter().map(|v| v as f32 * 257.0).collect(),
                    w,
                    h,
                    ch: 1,
                    bayer: None,
                })
            }
            other => {
                let rgb = other.to_rgb16();
                let (w, h) = (rgb.width() as usize, rgb.height() as usize);
                Ok(DsImage {
                    data: rgb.into_raw().into_iter().map(|v| v as f32).collect(),
                    w,
                    h,
                    ch: 3,
                    bayer: None,
                })
            }
        }
    }
}

/// Debayer a calibrated CFA mono frame to interleaved RGB (post-calibration).
fn ds_debayer_image(img: DsImage, cid: i32) -> DsImage {
    if img.w < 4 || img.h < 4 {
        return img;
    }
    let cfa: Vec<u16> = img.data.iter().map(|&v| v.clamp(0.0, 65535.0) as u16).collect();
    let rgb = debayer_to_rgb(&cfa, img.w, img.h, cid);
    DsImage {
        data: rgb.into_iter().map(|v| v as f32).collect(),
        w: img.w,
        h: img.h,
        ch: 3,
        bayer: None,
    }
}

/// Calibration master (bias/dark/flat). MEDIAN-combined when ≥5 frames fit in
/// a sane RAM budget (the robust choice: cosmic rays and passing satellites in
/// darks/flats vanish instead of averaging in — same policy as WBPP/SIRIL);
/// falls back to mean otherwise. Dimension mismatches are skipped with a log.
fn ds_build_master(
    app: &tauri::AppHandle,
    paths: &[String],
    label: &str,
) -> Option<DsImage> {
    if paths.is_empty() {
        return None;
    }
    let mut imgs: Vec<Vec<u16>> = Vec::new();
    let mut dims: Option<(usize, usize, usize)> = None;
    for p in paths {
        match ds_read_image(p) {
            Ok(img) => {
                match dims {
                    None => dims = Some((img.w, img.h, img.ch)),
                    Some((w, h, ch)) => {
                        if img.w != w || img.h != h || img.ch != ch {
                            log_to_front(app, "WARN", &format!("{}: dimensiones distintas, omitido: {}", label, p));
                            continue;
                        }
                    }
                }
                imgs.push(img.data.iter().map(|&v| v.clamp(0.0, 65535.0) as u16).collect());
            }
            Err(e) => log_to_front(app, "WARN", &format!("{}: no se pudo leer {} ({})", label, p, e)),
        }
    }
    let (w, h, ch) = dims?;
    if imgs.is_empty() {
        return None;
    }
    let n = imgs.len();
    let bytes = n * w * h * ch * 2;
    let use_median = n >= 5 && bytes <= 3 * 1024 * 1024 * 1024;

    let data: Vec<f32> = if use_median {
        let npx = w * h * ch;
        (0..npx)
            .into_par_iter()
            .map(|i| {
                let mut vals: Vec<u16> = imgs.iter().map(|im| im[i]).collect();
                let mid = vals.len() / 2;
                let (_, m, _) = vals.select_nth_unstable(mid);
                *m as f32
            })
            .collect()
    } else {
        let npx = w * h * ch;
        let mut sum = vec![0.0f64; npx];
        for im in &imgs {
            for (a, &v) in sum.iter_mut().zip(im.iter()) {
                *a += v as f64;
            }
        }
        sum.iter().map(|&v| (v / n as f64) as f32).collect()
    };

    log_to_front(
        app,
        "INFO",
        &format!(
            "Master {}: {} frames combinados por {}.",
            label,
            n,
            if use_median { "MEDIANA (robusto a cosmics)" } else { "media" }
        ),
    );
    // Masters are used for per-pixel arithmetic in the SAME (CFA or RGB) space
    // as the lights, so the bayer tag is irrelevant here.
    Some(DsImage { data, w, h, ch, bayer: None })
}

/// Cosmetic hot-pixel correction (SIRIL parity): without darks, sensor hot
/// pixels survive calibration and dodge the σ-clip (they sit at the SAME
/// pixel in every frame after registration only if the mount never moved —
/// with dithering/drift they become spurious stars). A pixel far above its
/// 8-neighbour median is replaced by that median.
fn ds_cosmetic_hot_pixels(img: &mut DsImage) {
    let (w, h, ch) = (img.w, img.h, img.ch);
    if w < 8 || h < 8 {
        return;
    }
    // CFA-aware neighbourhood: on a raw Bayer frame the 8 immediate neighbours
    // are DIFFERENT colours, so we must compare each photosite against its
    // SAME-colour neighbours (stride 2). On true RGB/mono the stride is 1.
    // `d` = neighbour stride, `pad` = border to skip so all 8 offsets are valid.
    let d: usize = if img.bayer.is_some() { 2 } else { 1 };
    let pad = d;
    for c in 0..ch {
        let plane: Vec<f32> = img.data.iter().skip(c).step_by(ch).copied().collect();
        // Global noise (MAD on a sample).
        let step = (plane.len() / 150_000).max(1);
        let mut s: Vec<f32> = plane.iter().step_by(step).copied().collect();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med = s[s.len() / 2];
        let mut dev: Vec<f32> = s.iter().map(|v| (v - med).abs()).collect();
        dev.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let noise = (dev[dev.len() / 2] * 1.4826).max(2.0);

        let data_ptr = img.data.as_mut_ptr() as usize;
        (pad..h - pad).into_par_iter().for_each(|y| {
            let out = unsafe {
                std::slice::from_raw_parts_mut((data_ptr as *mut f32).add(y * w * ch), w * ch)
            };
            for x in pad..w - pad {
                let v = plane[y * w + x];
                let mut nb = [
                    plane[(y - d) * w + x - d],
                    plane[(y - d) * w + x],
                    plane[(y - d) * w + x + d],
                    plane[y * w + x - d],
                    plane[y * w + x + d],
                    plane[(y + d) * w + x - d],
                    plane[(y + d) * w + x],
                    plane[(y + d) * w + x + d],
                ];
                nb.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let m8 = (nb[3] + nb[4]) * 0.5;
                // Hot pixel: far ABOVE its same-colour neighbours.
                if v > m8 + 6.0 * noise && v > m8 * 1.5 {
                    out[x * ch + c] = m8;
                // Cold/dead pixel: far BELOW (stuck-low sensel → black hole after
                // stretch). Symmetric MAD test, no multiplicative gate near zero.
                } else if v < m8 - 6.0 * noise && v < med - 3.0 * noise {
                    out[x * ch + c] = m8;
                }
            }
        });
    }
}

/// Generic Gaussian-elimination solver for an n×(n+1) augmented system.
fn ds_solve_linear_n(a: &mut [Vec<f64>], n: usize) -> Option<Vec<f64>> {
    for col in 0..n {
        let mut piv = col;
        for r in (col + 1)..n {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if a[piv][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, piv);
        let d = a[col][col];
        for k in col..=n {
            a[col][k] /= d;
        }
        for r in 0..n {
            if r != col {
                let f = a[r][col];
                for k in col..=n {
                    a[r][k] -= f * a[col][k];
                }
            }
        }
    }
    Some((0..n).map(|i| a[i][n]).collect())
}

/// 2D polynomial basis up to total degree `deg` (normalized coords), e.g.
/// deg 4 → 15 terms. Powerful enough to model steep OSC vignetting.
#[inline]
fn ds_poly_basis(x: f64, y: f64, deg: usize) -> Vec<f64> {
    let mut b = Vec::new();
    for j in 0..=deg {
        for i in 0..=(deg - j) {
            b.push(x.powi(i as i32) * y.powi(j as i32));
        }
    }
    b
}

/// ABE background gradient extraction (PixInsight ABE / SIRIL background
/// extraction parity): per channel, a 32×32 grid of robust background samples
/// (cell 15th percentile — immune to stars and most nebulosity) fits a
/// degree-4 2D polynomial with ITERATIVE OUTLIER REJECTION (samples sitting
/// far ABOVE the fit are objects → dropped, refit); the fitted gradient is
/// subtracted median-preserved so the sky LEVEL stays and only the
/// tilt/vignetting residual goes. Handles heavy vignetting (no-flats OSC).
fn ds_extract_background_gradient(data: &mut [f32], w: usize, h: usize, ch: usize) {
    const GRID: usize = 32;
    // Degree 2 (6 terms): captures vignetting / light-pollution gradients WITHOUT
    // following extended nebulosity (a degree-4 fit over-fits and subtracts the
    // object itself — the classic ABE failure on nebula-filling targets like M16).
    const DEG: usize = 2;
    let nterms = (DEG + 1) * (DEG + 2) / 2; // 6
    if w < GRID * 3 || h < GRID * 3 {
        return;
    }
    for c in 0..ch {
        // 1. Robust background samples (cell 15th percentile).
        let mut xs: Vec<f64> = Vec::new();
        let mut ys: Vec<f64> = Vec::new();
        let mut vs: Vec<f64> = Vec::new();
        for gy in 0..GRID {
            for gx in 0..GRID {
                let x0 = gx * w / GRID;
                let x1 = ((gx + 1) * w / GRID).min(w);
                let y0 = gy * h / GRID;
                let y1 = ((gy + 1) * h / GRID).min(h);
                let mut cell: Vec<f32> = Vec::with_capacity(256);
                let sx = ((x1 - x0) / 16).max(1);
                let sy = ((y1 - y0) / 16).max(1);
                let mut y = y0;
                while y < y1 {
                    let mut x = x0;
                    while x < x1 {
                        cell.push(data[(y * w + x) * ch + c]);
                        x += sx;
                    }
                    y += sy;
                }
                if cell.len() < 8 {
                    continue;
                }
                cell.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                xs.push((x0 + x1) as f64 / 2.0 / w as f64);
                ys.push((y0 + y1) as f64 / 2.0 / h as f64);
                vs.push(cell[cell.len() * 3 / 20] as f64); // 15th pct
            }
        }
        if vs.len() < nterms * 2 {
            continue;
        }

        // 2. LS fit with 2 rejection passes (drop samples > fit + 2.5σ).
        let mut keep = vec![true; vs.len()];
        let mut coef: Vec<f64> = Vec::new();
        for pass in 0..3 {
            let mut mat: Vec<Vec<f64>> = vec![vec![0.0; nterms + 1]; nterms];
            let mut count = 0;
            for i in 0..vs.len() {
                if !keep[i] {
                    continue;
                }
                count += 1;
                let b = ds_poly_basis(xs[i], ys[i], DEG);
                for r in 0..nterms {
                    for cc in 0..nterms {
                        mat[r][cc] += b[r] * b[cc];
                    }
                    mat[r][nterms] += b[r] * vs[i];
                }
            }
            if count < nterms * 2 {
                break;
            }
            coef = match ds_solve_linear_n(&mut mat, nterms) {
                Some(v) => v,
                None => break,
            };
            if pass == 2 {
                break;
            }
            // residuals → reject positive outliers (objects)
            let mut res: Vec<f64> = Vec::new();
            for i in 0..vs.len() {
                if keep[i] {
                    let f: f64 = ds_poly_basis(xs[i], ys[i], DEG).iter().zip(&coef).map(|(b, c)| b * c).sum();
                    res.push(vs[i] - f);
                }
            }
            let mut ares: Vec<f64> = res.iter().map(|r| r.abs()).collect();
            ares.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let sigma = ares[ares.len() / 2] * 1.4826 + 1e-6;
            for i in 0..vs.len() {
                if keep[i] {
                    let f: f64 = ds_poly_basis(xs[i], ys[i], DEG).iter().zip(&coef).map(|(b, c)| b * c).sum();
                    if vs[i] - f > 2.5 * sigma {
                        keep[i] = false;
                    }
                }
            }
        }
        if coef.is_empty() {
            continue;
        }

        // 3. Median-preserving subtraction.
        let mut fitted: Vec<f64> = (0..vs.len())
            .filter(|&i| keep[i])
            .map(|i| ds_poly_basis(xs[i], ys[i], DEG).iter().zip(&coef).map(|(b, cc)| b * cc).sum())
            .collect();
        fitted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let level = fitted[fitted.len() / 2];

        let coef_ref = &coef;
        let data_ptr = data.as_mut_ptr() as usize;
        (0..h).into_par_iter().for_each(|y| {
            let row = unsafe {
                std::slice::from_raw_parts_mut((data_ptr as *mut f32).add(y * w * ch), w * ch)
            };
            let yn = y as f64 / h as f64;
            for x in 0..w {
                let xn = x as f64 / w as f64;
                let g: f64 = ds_poly_basis(xn, yn, DEG).iter().zip(coef_ref).map(|(b, cc)| b * cc).sum();
                let v = row[x * ch + c] as f64 - (g - level);
                row[x * ch + c] = v.clamp(0.0, 65535.0) as f32;
            }
        });
    }
}

/// Standard calibration: light' = (light − bias − dark) / flat_norm.
/// DARK OPTIMIZATION (SIRIL-style): the scalar k that best cancels the master
/// dark's thermal/hot-pixel pattern in THIS light (temperature/exposure often
/// differ). Estimated robustly from the pixels where the dark signal dominates
/// (99th percentile) as the median of (light−bias)/dark. Returns 1.0 when it
/// can't be estimated (missing/mismatched dark, too few hot pixels). `dark` is
/// expected to already be bias-subtracted (pure thermal signal D_cal).
fn ds_optimize_dark_scale(light: &DsImage, bias: &Option<DsImage>, dark: &Option<DsImage>) -> f32 {
    let dark = match dark {
        Some(d) if d.w == light.w && d.h == light.h && d.ch == light.ch => d,
        _ => return 1.0,
    };
    let n = light.data.len();
    let step = (n / 300_000).max(1);
    let mut samp: Vec<f32> = dark.data.iter().step_by(step).copied().collect();
    if samp.len() < 50 {
        return 1.0;
    }
    samp.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // Threshold = 99th percentile of the dark → the thermal/hot pixels where the
    // dark dominates over sky signal, so (L−bias)/dark ≈ true scale.
    let thr = samp[samp.len() * 99 / 100].max(1.0);
    let bval = |i: usize| -> f32 {
        match bias {
            Some(b) if b.w == light.w && b.h == light.h && b.ch == light.ch => b.data[i],
            _ => 0.0,
        }
    };
    let mut ratios: Vec<f32> = Vec::new();
    for i in 0..n {
        let dv = dark.data[i];
        if dv >= thr {
            let lcal = light.data[i] - bval(i);
            if dv > 1.0 && lcal > 0.0 {
                ratios.push((lcal / dv).clamp(0.0, 4.0));
            }
        }
    }
    if ratios.len() < 20 {
        return 1.0;
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    ratios[ratios.len() / 2].clamp(0.3, 3.0)
}

fn ds_calibrate(
    light: &mut DsImage,
    bias: &Option<DsImage>,
    dark: &Option<DsImage>,
    flat_norm: &Option<DsImage>,
    dark_scale: f32,
) {
    let n = light.data.len();
    let sub = |img: &Option<DsImage>, i: usize, ch_i: usize| -> f32 {
        match img {
            Some(m) if m.w == light.w && m.h == light.h => {
                if m.ch == light.ch {
                    m.data[i]
                } else {
                    // mono master on color light (or vice versa): use its luma
                    m.data[(i / light.ch) * m.ch + ch_i.min(m.ch - 1)]
                }
            }
            _ => 0.0,
        }
    };
    for i in 0..n {
        let ch_i = i % light.ch;
        let mut v = light.data[i] - sub(bias, i, ch_i) - dark_scale * sub(dark, i, ch_i);
        if let Some(f) = flat_norm {
            if f.w == light.w && f.h == light.h {
                let fv = if f.ch == light.ch {
                    f.data[i]
                } else {
                    f.data[(i / light.ch) * f.ch + ch_i.min(f.ch - 1)]
                };
                v /= fv.max(0.05);
            }
        }
        light.data[i] = v.clamp(0.0, 65535.0);
    }
}

/// Robust background level (median) and noise scale (MAD·1.4826) of a plane.
fn ds_bg_noise(luma: &[f32]) -> (f32, f32) {
    if luma.is_empty() {
        return (0.0, 1.0);
    }
    let step = (luma.len() / 200_000).max(1);
    let mut s: Vec<f32> = luma.iter().step_by(step).copied().collect();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let bg = s[s.len() / 2];
    let mut d: Vec<f32> = s.iter().map(|v| (v - bg).abs()).collect();
    d.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (bg, (d[d.len() / 2] * 1.4826).max(1.0))
}

/// Coarse per-cell background grid (`gw`×`gh`) of a luma plane — the robust
/// low-percentile (25th) of each cell, so stars/nebulae don't inflate it. Used
/// by LOCAL NORMALIZATION to model each frame's spatially-varying sky level.
fn ds_local_bg_grid(luma: &[f32], w: usize, h: usize, gw: usize, gh: usize) -> Vec<f32> {
    let mut grid = vec![0.0f32; gw * gh];
    for gy in 0..gh {
        let y0 = gy * h / gh;
        let y1 = ((gy + 1) * h / gh).max(y0 + 1).min(h);
        for gx in 0..gw {
            let x0 = gx * w / gw;
            let x1 = ((gx + 1) * w / gw).max(x0 + 1).min(w);
            let mut cell: Vec<f32> = Vec::with_capacity((y1 - y0) * (x1 - x0));
            for y in y0..y1 {
                for x in x0..x1 {
                    let v = luma[y * w + x];
                    if v > 0.0 {
                        cell.push(v);
                    }
                }
            }
            if cell.is_empty() {
                grid[gy * gw + gx] = 0.0;
                continue;
            }
            cell.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            grid[gy * gw + gx] = cell[cell.len() / 4]; // 25th percentile ≈ sky
        }
    }
    grid
}

/// Bilinearly sample a `gw`×`gh` grid at normalized coords (u,v) ∈ [0,1].
#[inline]
fn ds_sample_grid(grid: &[f32], gw: usize, gh: usize, u: f32, v: f32) -> f32 {
    if grid.is_empty() {
        return 0.0;
    }
    let fx = (u.clamp(0.0, 1.0) * (gw as f32 - 1.0)).max(0.0);
    let fy = (v.clamp(0.0, 1.0) * (gh as f32 - 1.0)).max(0.0);
    let x0 = (fx.floor() as usize).min(gw - 1);
    let y0 = (fy.floor() as usize).min(gh - 1);
    let x1 = (x0 + 1).min(gw - 1);
    let y1 = (y0 + 1).min(gh - 1);
    let tx = fx - x0 as f32;
    let ty = fy - y0 as f32;
    let a = grid[y0 * gw + x0];
    let b = grid[y0 * gw + x1];
    let c = grid[y1 * gw + x0];
    let d = grid[y1 * gw + x1];
    let top = a * (1.0 - tx) + b * tx;
    let bot = c * (1.0 - tx) + d * tx;
    top * (1.0 - ty) + bot * ty
}

fn ds_luma(img: &DsImage) -> Vec<f32> {
    if img.ch == 1 {
        return img.data.clone();
    }
    img.data
        .chunks_exact(3)
        .map(|p| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2])
        .collect()
}

/// Star detection: background + MAD threshold, 3×3 local maxima with
/// hot-pixel rejection (a real star is EXTENDED), 7×7 centroid, flux-ranked.
fn ds_detect_stars(luma: &[f32], w: usize, h: usize, max_n: usize) -> Vec<(f32, f32, f32)> {
    if w < 32 || h < 32 || luma.len() < w * h {
        return Vec::new();
    }
    // Background and noise from a sample.
    let step = (luma.len() / 200_000).max(1);
    let mut sample: Vec<f32> = luma.iter().step_by(step).copied().collect();
    sample.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let bg = sample[sample.len() / 2];
    let mut devs: Vec<f32> = sample.iter().map(|v| (v - bg).abs()).collect();
    devs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let noise = (devs[devs.len() / 2] * 1.4826).max(1.0);
    let thr_peak = bg + 5.0 * noise;
    let thr_ext = bg + 2.5 * noise;

    let mut cands: Vec<(usize, usize, f32)> = Vec::new();
    for y in 4..h - 4 {
        let row = y * w;
        for x in 4..w - 4 {
            let v = luma[row + x];
            if v <= thr_peak {
                continue;
            }
            // 3×3 local maximum
            let mut is_max = true;
            let mut extended = 0;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let nv = luma[(y as i32 + dy) as usize * w + (x as i32 + dx) as usize];
                    if nv > v {
                        is_max = false;
                    }
                    if nv > thr_ext {
                        extended += 1;
                    }
                }
            }
            // ≥4 bright neighbours ⇒ extended object (rejects hot pixels).
            if is_max && extended >= 4 {
                cands.push((x, y, v));
            }
        }
    }
    cands.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    // Centroid + minimum separation dedupe.
    let mut stars: Vec<(f32, f32, f32)> = Vec::new();
    'cand: for &(cx, cy, _) in cands.iter() {
        for &(sx, sy, _) in stars.iter() {
            let d2 = (sx - cx as f32).powi(2) + (sy - cy as f32).powi(2);
            if d2 < 64.0 {
                continue 'cand; // within 8 px of an accepted star
            }
        }
        let (mut sw, mut sx, mut sy) = (0.0f64, 0.0f64, 0.0f64);
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                let px = (cx as i32 + dx) as usize;
                let py = (cy as i32 + dy) as usize;
                let wgt = (luma[py * w + px] - bg).max(0.0) as f64;
                sw += wgt;
                sx += wgt * px as f64;
                sy += wgt * py as f64;
            }
        }
        if sw > 1e-6 {
            stars.push(((sx / sw) as f32, (sy / sw) as f32, sw as f32));
        }
        if stars.len() >= max_n {
            break;
        }
    }
    stars
}

/// Frame sharpness proxy (WBPP-style weighting input): the median radial
/// second moment (≈ FWHM/2.355·k) of the brightest detected stars. Lower =
/// sharper frame; robust because it is a MEDIAN over up to 20 stars.
fn ds_frame_fwhm_proxy(luma: &[f32], w: usize, h: usize, stars: &[(f32, f32, f32)]) -> f32 {
    let mut sigmas: Vec<f32> = Vec::new();
    // Local background from the global median (cheap and stable enough here).
    let step = (luma.len() / 100_000).max(1);
    let mut s: Vec<f32> = luma.iter().step_by(step).copied().collect();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let bg = s[s.len() / 2];

    for &(sx, sy, _) in stars.iter().take(20) {
        let cx = sx.round() as i32;
        let cy = sy.round() as i32;
        if cx < 4 || cy < 4 || cx >= (w as i32 - 4) || cy >= (h as i32 - 4) {
            continue;
        }
        let (mut sw, mut sr2) = (0.0f64, 0.0f64);
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                let v = (luma[((cy + dy) as usize) * w + (cx + dx) as usize] - bg).max(0.0) as f64;
                sw += v;
                sr2 += v * ((dx * dx + dy * dy) as f64);
            }
        }
        if sw > 1e-6 {
            sigmas.push(((sr2 / sw) / 2.0).sqrt() as f32); // per-axis sigma
        }
    }
    if sigmas.is_empty() {
        return 0.0;
    }
    sigmas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sigmas[sigmas.len() / 2]
}

/// Median star ECCENTRICITY of a frame (0 = perfectly round, →1 = elongated).
/// Computed from the flux-weighted second moments (Ixx, Iyy, Ixy) of the top
/// stars; elongation from wind/tracking/wind-shake down-weights the frame like
/// PixInsight's eccentricity metric.
fn ds_frame_roundness(luma: &[f32], w: usize, h: usize, stars: &[(f32, f32, f32)]) -> f32 {
    let step = (luma.len() / 100_000).max(1);
    let mut s: Vec<f32> = luma.iter().step_by(step).copied().collect();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let bg = s[s.len() / 2];

    let mut eccs: Vec<f32> = Vec::new();
    for &(sx, sy, _) in stars.iter().take(30) {
        let cx = sx.round() as i32;
        let cy = sy.round() as i32;
        if cx < 4 || cy < 4 || cx >= (w as i32 - 4) || cy >= (h as i32 - 4) {
            continue;
        }
        let (mut sw, mut ixx, mut iyy, mut ixy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                let v = (luma[((cy + dy) as usize) * w + (cx + dx) as usize] - bg).max(0.0) as f64;
                sw += v;
                ixx += v * (dx * dx) as f64;
                iyy += v * (dy * dy) as f64;
                ixy += v * (dx * dy) as f64;
            }
        }
        if sw <= 1e-6 {
            continue;
        }
        ixx /= sw;
        iyy /= sw;
        ixy /= sw;
        // Eigenvalues of the [[ixx,ixy],[ixy,iyy]] moment matrix.
        let half = (ixx + iyy) / 2.0;
        let disc = (((ixx - iyy) / 2.0).powi(2) + ixy * ixy).max(0.0).sqrt();
        let l1 = half + disc; // major axis²
        let l2 = half - disc; // minor axis²
        if l1 > 1e-6 {
            let e = (1.0 - (l2 / l1).clamp(0.0, 1.0)).sqrt();
            eccs.push(e as f32);
        }
    }
    if eccs.is_empty() {
        return 0.0;
    }
    eccs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    eccs[eccs.len() / 2]
}

/// Least-squares similarity fit: p' ≈ (a·x − b·y + tx, b·x + a·y + ty).
fn ds_solve_similarity(pairs: &[((f32, f32), (f32, f32))]) -> Option<(f32, f32, f32, f32)> {
    let n = pairs.len();
    if n < 2 {
        return None;
    }
    let nf = n as f64;
    let (mut sx, mut sy, mut su, mut sv) = (0.0f64, 0.0, 0.0, 0.0);
    let (mut sxx, mut sxu, mut sxv, mut syu, mut syv) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    for &((x, y), (u, v)) in pairs {
        let (x, y, u, v) = (x as f64, y as f64, u as f64, v as f64);
        sx += x;
        sy += y;
        su += u;
        sv += v;
        sxx += x * x + y * y;
        sxu += x * u + y * v;
        sxv += x * v - y * u;
        syu += 0.0;
        syv += 0.0;
    }
    let _ = (syu, syv);
    let d = sxx - (sx * sx + sy * sy) / nf;
    if d.abs() < 1e-9 {
        return None;
    }
    let a = (sxu - (sx * su + sy * sv) / nf) / d;
    let b = (sxv - (sx * sv - sy * su) / nf) / d;
    let tx = (su - a * sx + b * sy) / nf;
    let ty = (sv - b * sx - a * sy) / nf;
    Some((a as f32, b as f32, tx as f32, ty as f32))
}

#[inline]
fn ds_apply_t(t: (f32, f32, f32, f32), x: f32, y: f32) -> (f32, f32) {
    (t.0 * x - t.1 * y + t.2, t.1 * x + t.0 * y + t.3)
}

/// Triangle-similarity registration (astroalign-style, local triangles from
/// each star's nearest neighbours). Returns the target→reference transform
/// and the inlier count; None when the field could not be matched.
fn ds_match_triangles(
    ref_stars: &[(f32, f32, f32)],
    tgt_stars: &[(f32, f32, f32)],
) -> Option<((f32, f32, f32, f32), usize)> {
    let take = 40usize;
    let r: Vec<(f32, f32)> = ref_stars.iter().take(take).map(|s| (s.0, s.1)).collect();
    let t: Vec<(f32, f32)> = tgt_stars.iter().take(take).map(|s| (s.0, s.1)).collect();
    if r.len() < 6 || t.len() < 6 {
        return None;
    }

    // Local triangles: each star with its 5 nearest neighbours. Vertices are
    // stored in GEOMETRIC ROLE order — [opposite-longest, opposite-middle,
    // opposite-shortest side] — so a matched pair of triangles yields the
    // correct vertex correspondence directly (index/flux order would pair
    // vertices arbitrarily and dilute the transform vote on real fields).
    let tris = |pts: &[(f32, f32)]| -> Vec<([usize; 3], f32, f32)> {
        let mut out: Vec<([usize; 3], f32, f32)> = Vec::new();
        let mut seen: std::collections::HashSet<[usize; 3]> = std::collections::HashSet::new();
        let d = |p: (f32, f32), q: (f32, f32)| -> f32 {
            ((p.0 - q.0).powi(2) + (p.1 - q.1).powi(2)).sqrt()
        };
        for i in 0..pts.len() {
            let mut near: Vec<(f32, usize)> = pts
                .iter()
                .enumerate()
                .filter(|&(j, _)| j != i)
                .map(|(j, p)| ((p.0 - pts[i].0).powi(2) + (p.1 - pts[i].1).powi(2), j))
                .collect();
            near.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let k = near.len().min(5);
            for a in 0..k {
                for b in (a + 1)..k {
                    let v = [i, near[a].1, near[b].1];
                    let mut key = v;
                    key.sort_unstable();
                    if !seen.insert(key) {
                        continue;
                    }
                    // side s_m = side OPPOSITE vertex v[m]
                    let s = [d(pts[v[1]], pts[v[2]]), d(pts[v[0]], pts[v[2]]), d(pts[v[0]], pts[v[1]])];
                    let mut order = [0usize, 1, 2];
                    order.sort_by(|&p, &q| s[q].partial_cmp(&s[p]).unwrap_or(std::cmp::Ordering::Equal));
                    let (s_max, s_mid, s_min) = (s[order[0]], s[order[1]], s[order[2]]);
                    if s_max < 12.0 || s_min < 4.0 {
                        continue; // degenerate/tiny
                    }
                    // role-ordered vertices + invariants (mid/max, min/max)
                    out.push((
                        [v[order[0]], v[order[1]], v[order[2]]],
                        s_mid / s_max,
                        s_min / s_max,
                    ));
                }
            }
        }
        out.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        out
    };
    let rt = tris(&r);
    let tt = tris(&t);
    if rt.is_empty() || tt.is_empty() {
        return None;
    }

    // Vote on (rotation, log-scale) buckets over invariant-matched triangles.
    let mut votes: std::collections::HashMap<(i32, i32), Vec<(usize, usize)>> =
        std::collections::HashMap::new();
    const TOL: f32 = 0.01;
    let mut lo = 0usize;
    for &(ti, r1, r2) in tt.iter() {
        while lo < rt.len() && rt[lo].1 < r1 - TOL {
            lo += 1;
        }
        let mut k = lo;
        while k < rt.len() && rt[k].1 <= r1 + TOL {
            if (rt[k].2 - r2).abs() <= TOL {
                let (ri, _, _) = rt[k];
                // Correspondence by matching the two longest-side vertices:
                // estimate transform from the 3 vertex pairs (sorted order is
                // a heuristic; wrong pairings simply don't accumulate votes).
                let pairs: Vec<((f32, f32), (f32, f32))> = (0..3)
                    .map(|m| (t[ti[m]], r[ri[m]]))
                    .collect();
                if let Some(tr) = ds_solve_similarity(&pairs) {
                    let scale = (tr.0 * tr.0 + tr.1 * tr.1).sqrt();
                    if scale > 0.5 && scale < 2.0 {
                        let ang = tr.1.atan2(tr.0);
                        let key = ((ang * 60.0).round() as i32, (scale.ln() * 60.0).round() as i32);
                        let e = votes.entry(key).or_default();
                        for m in 0..3 {
                            e.push((ti[m], ri[m]));
                        }
                    }
                }
            }
            k += 1;
        }
    }

    let (_, best_pairs) = votes.into_iter().max_by_key(|(_, v)| v.len())?;
    if best_pairs.len() < 9 {
        return None;
    }
    let mut seen = std::collections::HashSet::new();
    let uniq: Vec<((f32, f32), (f32, f32))> = best_pairs
        .into_iter()
        .filter(|&(a, b)| seen.insert((a, b)))
        .map(|(a, b)| (t[a], r[b]))
        .collect();
    let coarse = ds_solve_similarity(&uniq)?;

    // Inlier refinement: project ALL target stars, pair to nearest reference
    // star within 3 px, refit.
    let mut inl: Vec<((f32, f32), (f32, f32))> = Vec::new();
    for &tp in t.iter() {
        let p = ds_apply_t(coarse, tp.0, tp.1);
        let mut best = (9.0f32, None);
        for &rp in r.iter() {
            let d2 = (p.0 - rp.0).powi(2) + (p.1 - rp.1).powi(2);
            if d2 < best.0 {
                best = (d2, Some(rp));
            }
        }
        if let (d2, Some(rp)) = best {
            if d2 <= 9.0 {
                inl.push((tp, rp));
            }
        }
    }
    if inl.len() < 6 {
        return None;
    }
    let fine = ds_solve_similarity(&inl)?;
    Some((fine, inl.len()))
}

/// Inverse-map bilinear warp of `img` (target frame) into the reference
/// canvas, accumulating per-pixel into `sum`/`wgt` (and `sumsq` when given).
/// When `bounds` are provided, out-of-window samples are REJECTED (pass 2).
#[allow(clippy::too_many_arguments)]
fn ds_warp_accumulate(
    img: &DsImage,
    t: (f32, f32, f32, f32), // target → reference
    sum: &mut [f64],
    sumsq: Option<&mut Vec<f64>>,
    wgt: &mut [f64],
    bounds: Option<(&[f32], &[f32])>,
    w: usize, // OUTPUT canvas width (= input·scale under drizzle)
    h: usize,
    ch: usize,
    frame_w: f64, // WBPP-style per-frame quality weight (0.3..1.0)
    scale: f32,   // drizzle factor (1.0 = none): output(x,y) ↔ ref(x/scale)
    norm: (f32, f32), // per-frame normalization (mul, add): v' = v·mul + add
    loc: Option<(&[f32], usize, usize)>, // local-norm offset grid (output space)
) {
    // Invert the similarity: ref → target sampling.
    let det = t.0 * t.0 + t.1 * t.1;
    if det < 1e-9 {
        return;
    }
    let ia = t.0 / det;
    let ib = -t.1 / det;
    let inv_scale = 1.0 / scale.max(1.0);

    // Parallel per-row: each worker touches DISJOINT row slices of the
    // accumulators (raw pointers reconstructed per row — safe by row split).
    let sum_ptr = sum.as_mut_ptr() as usize;
    let wgt_ptr = wgt.as_mut_ptr() as usize;
    let sq_ptr: Option<usize> = sumsq.map(|v| v.as_mut_ptr() as usize);

    (0..h).into_par_iter().for_each(|y| {
        let sum_row = unsafe { std::slice::from_raw_parts_mut((sum_ptr as *mut f64).add(y * w * ch), w * ch) };
        let wgt_row = unsafe { std::slice::from_raw_parts_mut((wgt_ptr as *mut f64).add(y * w), w) };
        let mut sq_row = sq_ptr.map(|p| unsafe {
            std::slice::from_raw_parts_mut((p as *mut f64).add(y * w * ch), w * ch)
        });
        // Reference-frame coordinate of this output pixel (drizzle-scaled).
        let ry_ref = y as f32 * inv_scale;
        for x in 0..w {
            let rx_ref = x as f32 * inv_scale;
            let dx = rx_ref - t.2;
            let dy = ry_ref - t.3;
            let sxf = ia * dx - ib * dy;
            let syf = ib * dx + ia * dy;
            if sxf < 0.0 || syf < 0.0 || sxf >= (img.w - 1) as f32 || syf >= (img.h - 1) as f32 {
                continue;
            }
            let x0 = sxf as usize;
            let y0 = syf as usize;
            let fx = sxf - x0 as f32;
            let fy = syf - y0 as f32;
            let mut ok = true;
            let mut vals = [0.0f32; 3];
            // Local-normalization offset for this output pixel (0 when disabled).
            let loc_off = loc
                .map(|(g, gw, gh)| ds_sample_grid(g, gw, gh, x as f32 / w as f32, y as f32 / h as f32))
                .unwrap_or(0.0);
            for c in 0..ch {
                let i00 = (y0 * img.w + x0) * ch + c;
                let i01 = i00 + img.w * ch;
                let vraw = img.data[i00] * (1.0 - fx) * (1.0 - fy)
                    + img.data[i00 + ch] * fx * (1.0 - fy)
                    + img.data[i01] * (1.0 - fx) * fy
                    + img.data[i01 + ch] * fx * fy;
                // Per-frame normalization (sky level + scale) + local offset.
                let v = vraw * norm.0 + norm.1 + loc_off;
                vals[c] = v;
                if let Some((lo, hi)) = bounds {
                    let bi = (y * w + x) * ch + c;
                    if v < lo[bi] || v > hi[bi] {
                        ok = false; // kappa-sigma rejection (whole pixel)
                    }
                }
            }
            if !ok {
                continue;
            }
            for c in 0..ch {
                sum_row[x * ch + c] += vals[c] as f64 * frame_w;
                if let Some(sq) = sq_row.as_mut() {
                    sq[x * ch + c] += (vals[c] as f64) * (vals[c] as f64) * frame_w;
                }
            }
            wgt_row[x] += frame_w;
        }
    });
}

/// Warp a SINGLE image onto the reference grid (inverse similarity, bilinear),
/// same convention as `ds_warp_accumulate` but producing a plain buffer — used
/// to co-register per-filter channel masters before LRGB/narrowband combine.
/// Out-of-bounds samples become 0.
fn ds_warp_single(img: &DsImage, t: (f32, f32, f32, f32), w: usize, h: usize) -> Vec<f32> {
    let ch = img.ch;
    let mut out = vec![0.0f32; w * h * ch];
    let det = t.0 * t.0 + t.1 * t.1;
    if det < 1e-9 {
        return out;
    }
    let ia = t.0 / det;
    let ib = -t.1 / det;
    let out_ptr = out.as_mut_ptr() as usize;
    (0..h).into_par_iter().for_each(|y| {
        let row = unsafe { std::slice::from_raw_parts_mut((out_ptr as *mut f32).add(y * w * ch), w * ch) };
        for x in 0..w {
            let dx = x as f32 - t.2;
            let dy = y as f32 - t.3;
            let sxf = ia * dx - ib * dy;
            let syf = ib * dx + ia * dy;
            if sxf < 0.0 || syf < 0.0 || sxf >= (img.w - 1) as f32 || syf >= (img.h - 1) as f32 {
                continue;
            }
            let x0 = sxf as usize;
            let y0 = syf as usize;
            let fx = sxf - x0 as f32;
            let fy = syf - y0 as f32;
            for c in 0..ch {
                let i00 = (y0 * img.w + x0) * ch + c;
                let i01 = i00 + img.w * ch;
                row[x * ch + c] = img.data[i00] * (1.0 - fx) * (1.0 - fy)
                    + img.data[i00 + ch] * fx * (1.0 - fy)
                    + img.data[i01] * (1.0 - fx) * fy
                    + img.data[i01 + ch] * fx * fy;
            }
        }
    });
    out
}

/// TRUE DROP-KERNEL DRIZZLE (Fruchter & Hook) — used instead of inverse-bilinear
/// when the user enables real drizzle. Each input pixel is a "drop" shrunk by
/// `pixfrac`; its flux is scattered onto the finer output grid by geometric
/// overlap area, so dithered subframes recover resolution WITHOUT interpolation
/// blur. Same sum/sumsq/wgt/bounds interface as `ds_warp_accumulate` (both κσ
/// passes work). Parallelised over disjoint output row-bands; each band derives
/// its contributing input bbox by inverse-mapping the band corners.
#[allow(clippy::too_many_arguments)]
fn ds_drizzle_accumulate(
    img: &DsImage,
    t: (f32, f32, f32, f32), // input → reference
    sum: &mut [f64],
    sumsq: Option<&mut Vec<f64>>,
    wgt: &mut [f64],
    bounds: Option<(&[f32], &[f32])>,
    w: usize, // OUTPUT canvas width
    h: usize,
    ch: usize,
    frame_w: f64,
    scale: f32,   // drizzle factor (>1)
    pixfrac: f32, // drop shrink 0.4..1.0 (smaller = sharper, needs more frames)
    norm: (f32, f32),
    loc: Option<(&[f32], usize, usize)>, // local-norm offset grid (output space)
) {
    let (a, b, tx, ty) = t;
    let det = a * a + b * b;
    if det < 1e-9 {
        return;
    }
    let ia = a / det;
    let ib = -b / det;
    let half = 0.5 * pixfrac.clamp(0.2, 1.0) * scale; // drop half-size (output px)

    let sum_ptr = sum.as_mut_ptr() as usize;
    let wgt_ptr = wgt.as_mut_ptr() as usize;
    let sq_ptr: Option<usize> = sumsq.map(|v| v.as_mut_ptr() as usize);

    let n_bands = rayon::current_num_threads().max(1);
    let band_h = h.div_ceil(n_bands);

    (0..n_bands).into_par_iter().for_each(|band| {
        let oy0 = band * band_h;
        let oy1 = ((band + 1) * band_h).min(h);
        if oy0 >= oy1 {
            return;
        }
        // DISJOINT per-band slices (rows [oy0,oy1) only) → no aliasing across
        // threads. Local pixel index = (opy-oy0)*w + opx.
        let rows = oy1 - oy0;
        let sum_band = unsafe { std::slice::from_raw_parts_mut((sum_ptr as *mut f64).add(oy0 * w * ch), rows * w * ch) };
        let wgt_band = unsafe { std::slice::from_raw_parts_mut((wgt_ptr as *mut f64).add(oy0 * w), rows * w) };
        let mut sq_band = sq_ptr.map(|p| unsafe { std::slice::from_raw_parts_mut((p as *mut f64).add(oy0 * w * ch), rows * w * ch) });

        // Input bbox that can splat into this band: inverse-map the band corners
        // (expanded by the drop half-size) output → ref → input.
        let mut ixmin = i32::MAX;
        let mut ixmax = i32::MIN;
        let mut iymin = i32::MAX;
        let mut iymax = i32::MIN;
        for &(ox, oy) in &[
            (-half, oy0 as f32 - half),
            (w as f32 + half, oy0 as f32 - half),
            (-half, oy1 as f32 + half),
            (w as f32 + half, oy1 as f32 + half),
        ] {
            let rx = ox / scale;
            let ry = oy / scale;
            let dx = rx - tx;
            let dy = ry - ty;
            let sx = ia * dx - ib * dy;
            let sy = ib * dx + ia * dy;
            ixmin = ixmin.min(sx.floor() as i32);
            ixmax = ixmax.max(sx.ceil() as i32);
            iymin = iymin.min(sy.floor() as i32);
            iymax = iymax.max(sy.ceil() as i32);
        }
        ixmin = ixmin.max(0);
        iymin = iymin.max(0);
        ixmax = ixmax.min(img.w as i32 - 1);
        iymax = iymax.min(img.h as i32 - 1);
        if ixmin > ixmax || iymin > iymax {
            return;
        }

        for iy in iymin..=iymax {
            for ix in ixmin..=ixmax {
                let base = (iy as usize * img.w + ix as usize) * ch;
                // Forward-map the input pixel centre to the output grid.
                let rx = a * ix as f32 - b * iy as f32 + tx;
                let ry = b * ix as f32 + a * iy as f32 + ty;
                let ox = rx * scale;
                let oy = ry * scale;
                let dx0 = ox - half;
                let dx1 = ox + half;
                let dy0 = oy - half;
                let dy1 = oy + half;
                let px0 = dx0.floor() as i32;
                let px1 = (dx1.ceil() as i32 - 1).max(px0);
                let py0 = dy0.floor() as i32;
                let py1 = (dy1.ceil() as i32 - 1).max(py0);

                // Value with per-frame normalization (+ local offset sampled at
                // the drop centre — the field is smooth over one input pixel).
                let loc_off = loc
                    .map(|(g, gw, gh)| ds_sample_grid(g, gw, gh, ox / w as f32, oy / h as f32))
                    .unwrap_or(0.0);
                let mut vals = [0.0f32; 3];
                for c in 0..ch {
                    vals[c] = img.data[base + c] * norm.0 + norm.1 + loc_off;
                }

                for opy in py0.max(oy0 as i32)..=py1.min(oy1 as i32 - 1) {
                    if opy < 0 {
                        continue;
                    }
                    let ay = (dy1.min(opy as f32 + 1.0) - dy0.max(opy as f32)).max(0.0);
                    if ay <= 0.0 {
                        continue;
                    }
                    for opx in px0.max(0)..=px1.min(w as i32 - 1) {
                        let ax = (dx1.min(opx as f32 + 1.0) - dx0.max(opx as f32)).max(0.0);
                        if ax <= 0.0 {
                            continue;
                        }
                        let area = (ax * ay) as f64;
                        if area <= 0.0 {
                            continue;
                        }
                        let gpix = opy as usize * w + opx as usize; // global (bounds)
                        let lpix = (opy as usize - oy0) * w + opx as usize; // band-local
                        // κσ rejection (pass 2): drop this contribution if any
                        // channel falls outside the per-output-pixel window.
                        if let Some((lo, hi)) = bounds {
                            let mut ok = true;
                            for c in 0..ch {
                                let bi = gpix * ch + c;
                                if vals[c] < lo[bi] || vals[c] > hi[bi] {
                                    ok = false;
                                    break;
                                }
                            }
                            if !ok {
                                continue;
                            }
                        }
                        let wv = area * frame_w;
                        for c in 0..ch {
                            sum_band[lpix * ch + c] += vals[c] as f64 * wv;
                            if let Some(sq) = sq_band.as_deref_mut() {
                                sq[lpix * ch + c] += (vals[c] as f64) * (vals[c] as f64) * wv;
                            }
                        }
                        wgt_band[lpix] += wv;
                    }
                }
            }
        }
    });
}

/// BACKGROUND NEUTRALIZATION (SIRIL parity — the fix for the green OSC cast):
/// a one-shot-color sensor has TWICE the green pixels, so its sky background
/// is green even after calibration. Estimate each channel's background level
/// (robust: the mode of the lower half, approximated by the 25th percentile of
/// a sample) and offset every channel so all three backgrounds meet at the
/// LOWEST of them → the sky becomes neutral gray while real object color is
/// untouched (only a constant per-channel offset is applied). Mono is a no-op.
fn ds_neutralize_background(data: &mut [f32], w: usize, h: usize, ch: usize) -> [f32; 3] {
    if ch != 3 {
        return [0.0; 3];
    }
    let n = w * h;
    let step = (n / 300_000).max(1);
    let mut bg = [0.0f32; 3];
    for c in 0..3 {
        let mut s: Vec<f32> = (0..n).step_by(step).map(|i| data[i * 3 + c]).collect();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // 25th percentile ≈ background (object is a minority of pixels).
        bg[c] = s[s.len() / 4];
    }
    let target = bg[0].min(bg[1]).min(bg[2]);
    let off = [bg[0] - target, bg[1] - target, bg[2] - target];
    data.par_chunks_mut(3).for_each(|px| {
        px[0] = (px[0] - off[0]).max(0.0);
        px[1] = (px[1] - off[1]).max(0.0);
        px[2] = (px[2] - off[2]).max(0.0);
    });
    off
}

/// SCNR — Subtractive Chromatic Noise Reduction (SIRIL/PixInsight's canonical
/// green-removal, "average neutral"): G' = min(G, (R+B)/2). Astrophotos have
/// essentially NO real green light, so any green ABOVE the red/blue average is
/// a sensor/processing cast (the classic OSC green sky and green star fringes)
/// and is clipped down. This is what finally kills the green that a constant
/// background offset cannot (it also works on the gradient, per pixel).
fn ds_scnr_green(data: &mut [f32], ch: usize, amount: f32) {
    if ch != 3 {
        return;
    }
    let a = amount.clamp(0.0, 1.0);
    data.par_chunks_mut(3).for_each(|px| {
        let neutral = 0.5 * (px[0] + px[2]);
        if px[1] > neutral {
            px[1] = px[1] * (1.0 - a) + neutral * a;
        }
    });
}

/// AUTO-CROP the low-coverage borders of a dithered/rotated stack. `cov` is the
/// per-output-pixel integration weight (frame overlap); pixels covered by less
/// than 85% of the peak are the ragged, noisier edges DSS/PixInsight trim. Finds
/// the bounding box of well-covered pixels and returns the cropped raster with
/// its new dimensions. No-op (returns input) when nothing to trim or the crop
/// would be pathological (<25% area).
fn ds_autocrop(data: &[f32], cov: &[f64], w: usize, h: usize, ch: usize) -> (Vec<f32>, usize, usize) {
    let max_cov = cov.iter().cloned().fold(0.0f64, f64::max);
    if max_cov <= 0.0 {
        return (data.to_vec(), w, h);
    }
    let thr = max_cov * 0.85;
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            if cov[y * w + x] >= thr {
                any = true;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if !any || x1 < x0 || y1 < y0 {
        return (data.to_vec(), w, h);
    }
    let (nw, nh) = (x1 - x0 + 1, y1 - y0 + 1);
    // Guard against pathological over-crop and the no-op full-frame case.
    if nw == w && nh == h {
        return (data.to_vec(), w, h);
    }
    if (nw * nh) * 4 < w * h {
        return (data.to_vec(), w, h);
    }
    let mut out = vec![0.0f32; nw * nh * ch];
    for y in 0..nh {
        let src = ((y + y0) * w + x0) * ch;
        let dst = y * nw * ch;
        out[dst..dst + nw * ch].copy_from_slice(&data[src..src + nw * ch]);
    }
    (out, nw, nh)
}

/// Screen-transfer render with SIRIL-style modes. `per_channel=false` (linked)
/// computes one STF on luminance and applies it to all channels — preserves
/// the true color balance. `per_channel=true` (unlinked) autostretches each
/// channel independently — equalizes the channels for a neutral, punchy view.
/// `shadow_k` controls the black clip (2.8 default) and `target` the midtone
/// (0.25 default). Returns 8-bit RGB.
/// Solve the STF (clip c, midtone m) from a normalized 0..1 value sample so the
/// median maps to `target` after a `shadow_k`·MAD black point (PixInsight AutoSTF).
fn ds_stf_params(mut sample: Vec<f32>, shadow_k: f32, target: f32) -> (f32, f32) {
    if sample.is_empty() {
        return (0.0, 0.5);
    }
    sample.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let med = sample[sample.len() / 2];
    let mut dev: Vec<f32> = sample.iter().map(|v| (v - med).abs()).collect();
    dev.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mad = (dev[dev.len() / 2] * 1.4826).max(1.0 / 65535.0);
    let c = (med - shadow_k * mad).max(0.0);
    let m_in = ((med - c) / (1.0 - c)).clamp(1.0 / 65535.0, 0.99);
    // solve mtf(m_in, m) = target
    let m = ((m_in * (target - 1.0)) / ((2.0 * target - 1.0) * m_in - target)).clamp(0.001, 0.999);
    (c, m)
}

/// Compute the per-channel STF params for a linear RGB16 buffer (shared by the
/// 8-bit preview and the 16-bit export so what you SEE is what you SAVE).
fn ds_stretch_params(rgb: &[u16], w: usize, h: usize, per_channel: bool, shadow_k: f32, target: f32) -> [(f32, f32); 3] {
    let n = w * h;
    let step = (n / 300_000).max(1);
    let target = target.clamp(0.05, 0.9);
    if per_channel {
        let mut params = [(0.0f32, 0.5f32); 3];
        for cch in 0..3 {
            let s: Vec<f32> = (0..n).step_by(step).map(|i| rgb[i * 3 + cch] as f32 / 65535.0).collect();
            params[cch] = ds_stf_params(s, shadow_k, target);
        }
        params
    } else {
        let s: Vec<f32> = (0..n)
            .step_by(step)
            .map(|i| (0.2126 * rgb[i * 3] as f32 + 0.7152 * rgb[i * 3 + 1] as f32 + 0.0722 * rgb[i * 3 + 2] as f32) / 65535.0)
            .collect();
        let p = ds_stf_params(s, shadow_k, target);
        [p, p, p]
    }
}

fn ds_render_stretch(rgb: &[u16], w: usize, h: usize, per_channel: bool, shadow_k: f32, target: f32) -> Vec<u8> {
    let n = w * h;
    let params = ds_stretch_params(rgb, w, h, per_channel, shadow_k, target);
    let mut out = vec![0u8; n * 3];
    out.par_chunks_mut(3).enumerate().for_each(|(i, px)| {
        for cch in 0..3 {
            let (c, m) = params[cch];
            let x = ((rgb[i * 3 + cch] as f32 / 65535.0 - c) / (1.0 - c)).clamp(0.0, 1.0);
            px[cch] = (ds_mtf(x, m) * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    });
    out
}

/// 16-bit STF stretch for EXPORT — identical math to the on-screen preview but
/// preserving full depth. `mode`: "linked"/"unlinked"/"channels"/"linear".
fn ds_stretch16(rgb: &[u16], w: usize, h: usize, mode: &str, strength: f32) -> Vec<u16> {
    let n = w * h;
    let s = strength.clamp(0.0, 1.0);
    let target = (0.45 - 0.35 * s).clamp(0.08, 0.45);
    if mode == "linear" {
        let step = (n / 300_000).max(1);
        let mut smp: Vec<f32> = (0..n)
            .step_by(step)
            .map(|i| (rgb[i * 3] as f32 + rgb[i * 3 + 1] as f32 + rgb[i * 3 + 2] as f32) / 3.0)
            .collect();
        smp.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let lo = smp[smp.len() / 1000];
        let hi = (smp[smp.len() - 1 - smp.len() / 1000]).max(lo + 1.0);
        let mut out = vec![0u16; n * 3];
        out.par_chunks_mut(3).enumerate().for_each(|(i, px)| {
            for c in 0..3 {
                let v = ((rgb[i * 3 + c] as f32 - lo) / (hi - lo)).clamp(0.0, 1.0);
                px[c] = (v * 65535.0).round() as u16;
            }
        });
        return out;
    }
    let per_channel = mode == "unlinked" || mode == "channels";
    let params = ds_stretch_params(rgb, w, h, per_channel, 2.8, target);
    let mut out = vec![0u16; n * 3];
    out.par_chunks_mut(3).enumerate().for_each(|(i, px)| {
        for cch in 0..3 {
            let (c, m) = params[cch];
            let x = ((rgb[i * 3 + cch] as f32 / 65535.0 - c) / (1.0 - c)).clamp(0.0, 1.0);
            px[cch] = (ds_mtf(x, m) * 65535.0).round().clamp(0.0, 65535.0) as u16;
        }
    });
    out
}

/// PixInsight-style midtones transfer function.
#[inline]
fn ds_mtf(x: f32, m: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    ((m - 1.0) * x) / (((2.0 * m - 1.0) * x) - m)
}

/// RE-STRETCH the stored linear deep-sky result with a chosen view mode
/// (SIRIL-style, PREVIEW only — the stored data stays linear for the post
/// pipeline). Modes: "linked" (color-true), "unlinked"/"channels" (balanced),
/// "linear" (no stretch, min-max), plus a strength 0..1 shifting the midtone.
#[tauri::command]
fn deepsky_restretch(
    state: State<'_, AppState>,
    mode: String,
    strength: Option<f32>,
) -> Result<String, String> {
    let (rgb16, w, h) = {
        let guard = state.stacked_image.lock().unwrap();
        let img = guard.as_ref().ok_or("No hay resultado apilado en memoria.")?;
        (img.data.clone(), img.width, img.height)
    };
    if rgb16.len() < w * h * 3 {
        return Err("Resultado no compatible con re-estirado RGB.".into());
    }
    // strength 0.5 = default 0.25 midtone; higher = brighter (lower midtone).
    let s = strength.unwrap_or(0.5).clamp(0.0, 1.0);
    let target = (0.45 - 0.35 * s).clamp(0.08, 0.45);

    let preview8 = match mode.as_str() {
        "linear" => {
            // Simple min→max per luma, no MTF.
            let n = w * h;
            let step = (n / 300_000).max(1);
            let mut s: Vec<f32> = (0..n)
                .step_by(step)
                .map(|i| (rgb16[i * 3] as f32 + rgb16[i * 3 + 1] as f32 + rgb16[i * 3 + 2] as f32) / 3.0)
                .collect();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let lo = s[s.len() / 1000] as f32;
            let hi = (s[s.len() - 1 - s.len() / 1000] as f32).max(lo + 1.0);
            let mut out = vec![0u8; n * 3];
            out.par_chunks_mut(3).enumerate().for_each(|(i, px)| {
                for c in 0..3 {
                    let v = ((rgb16[i * 3 + c] as f32 - lo) / (hi - lo)).clamp(0.0, 1.0);
                    px[c] = (v * 255.0) as u8;
                }
            });
            out
        }
        "unlinked" | "channels" => ds_render_stretch(&rgb16, w, h, true, 2.8, target),
        _ => ds_render_stretch(&rgb16, w, h, false, 2.8, target),
    };

    let mut rgba = Vec::with_capacity(w * h * 4);
    for i in 0..(w * h) {
        rgba.push(preview8[i * 3]);
        rgba.push(preview8[i * 3 + 1]);
        rgba.push(preview8[i * 3 + 2]);
        rgba.push(255);
    }
    let img_out = RgbaImage::from_raw(w as u32, h as u32, rgba).ok_or("preview")?;
    let mut enc = Vec::new();
    DynamicImage::ImageRgba8(img_out)
        .write_to(&mut Cursor::new(&mut enc), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(format!("data:image/png;base64,{}", general_purpose::STANDARD.encode(&enc)))
}

/// EXPORT the deep-sky result to disk next to the reference light. Saves the
/// STRETCHED image (exactly what the STF preview shows — 16-bit TIFF or 8-bit
/// PNG) and/or the LINEAR 16-bit master (for further work in PixInsight/PS).
/// Returns a human summary of the files written.
#[tauri::command]
fn deepsky_export(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    base_path: String,
    mode: String,
    strength: Option<f32>,
    save_stretched: bool,
    save_linear: bool,
    as_png: bool,
) -> Result<String, String> {
    state.license_manager.check_access()?;
    let (rgb16, w, h, is_mono) = {
        let guard = state.stacked_image.lock().unwrap();
        let img = guard.as_ref().ok_or("No hay resultado de cielo profundo en memoria.")?;
        (img.data.clone(), img.width, img.height, img.is_mono)
    };
    if rgb16.len() < w * h * 3 {
        return Err("El resultado no es RGB de 16 bits, no se puede exportar.".into());
    }
    // 16-bit TIFF is a PRO feature (matches the planetary save gate). PNG is free.
    let wants_tiff = (save_stretched && !as_png) || save_linear;
    if wants_tiff && !state.license_manager.is_pro() {
        return Err("Exportar en TIFF 16-bit requiere licencia PRO o periodo de prueba activo.".into());
    }

    let parent = std::path::Path::new(&base_path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(std::env::temp_dir);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let kind = if is_mono { "Mono" } else { "RGB" };

    let mut saved: Vec<String> = Vec::new();
    if save_stretched {
        let s = strength.unwrap_or(0.5);
        let st16 = ds_stretch16(&rgb16, w, h, &mode, s);
        if as_png {
            // Downshift the stretched 16-bit to a shareable 8-bit PNG.
            let mut rgba = Vec::with_capacity(w * h * 4);
            for i in 0..(w * h) {
                rgba.push((st16[i * 3] >> 8) as u8);
                rgba.push((st16[i * 3 + 1] >> 8) as u8);
                rgba.push((st16[i * 3 + 2] >> 8) as u8);
                rgba.push(255);
            }
            let img_out = RgbaImage::from_raw(w as u32, h as u32, rgba).ok_or("PNG")?;
            let out = parent.join(format!("ZenithDeepSky_{}_{}.png", kind, stamp));
            img_out.save(&out).map_err(|e| format!("PNG: {}", e))?;
            saved.push(out.display().to_string());
        } else {
            let out = parent.join(format!("ZenithDeepSky_{}_Estirado_16bit_{}.tiff", kind, stamp));
            derot_save_rgb16_tiff(&out, &st16, w, h)?;
            saved.push(out.display().to_string());
        }
    }
    if save_linear {
        let out = parent.join(format!("ZenithDeepSky_{}_Lineal_16bit_{}.tiff", kind, stamp));
        derot_save_rgb16_tiff(&out, &rgb16, w, h)?;
        saved.push(out.display().to_string());
    }
    if saved.is_empty() {
        return Err("Nada seleccionado para exportar.".into());
    }
    log_to_front(&app, "SUCCESS", &format!("Cielo Profundo exportado: {}", saved.join(" · ")));
    Ok(format!("Exportado ({} archivo/s):\n{}", saved.len(), saved.join("\n")))
}

/// Histogram + basic statistics of the deep-sky result for the final-view panel
/// (PixInsight-style readout). Bins are taken over the STRETCHED view (current
/// mode/strength) so the user can judge black point and clipping; `bg` is the
/// LINEAR per-channel background level in ADU.
#[derive(serde::Serialize)]
struct DsHistogram {
    bins: usize,
    r: Vec<u32>,
    g: Vec<u32>,
    b: Vec<u32>,
    clip_low: f32,  // % of pixels crushed to black by the stretch
    clip_high: f32, // % of pixels blown to white by the stretch
    bg: [f32; 3],   // linear median background per channel (ADU 0..65535)
    is_mono: bool,
}

#[tauri::command]
fn deepsky_histogram(
    state: State<'_, AppState>,
    mode: String,
    strength: Option<f32>,
) -> Result<DsHistogram, String> {
    let (rgb16, w, h, is_mono) = {
        let guard = state.stacked_image.lock().unwrap();
        let img = guard.as_ref().ok_or("No hay resultado de cielo profundo en memoria.")?;
        (img.data.clone(), img.width, img.height, img.is_mono)
    };
    if rgb16.len() < w * h * 3 {
        return Err("Resultado no compatible con histograma RGB.".into());
    }
    let n = w * h;
    let s = strength.unwrap_or(0.5);
    let st = ds_stretch16(&rgb16, w, h, &mode, s);
    const BINS: usize = 256;
    let (mut r, mut g, mut b) = (vec![0u32; BINS], vec![0u32; BINS], vec![0u32; BINS]);
    let (mut clow, mut chigh) = (0u64, 0u64);
    for i in 0..n {
        let (rv, gv, bv) = (st[i * 3], st[i * 3 + 1], st[i * 3 + 2]);
        r[(rv as usize * BINS) >> 16] += 1;
        g[(gv as usize * BINS) >> 16] += 1;
        b[(bv as usize * BINS) >> 16] += 1;
        let mx = rv.max(gv).max(bv);
        let mn = rv.min(gv).min(bv);
        if mx == 0 {
            clow += 1;
        }
        if mn == 65535 {
            chigh += 1;
        }
    }
    // Linear per-channel background (median of a sample).
    let step = (n / 200_000).max(1);
    let median_ch = |c: usize| -> f32 {
        let mut smp: Vec<u16> = (0..n).step_by(step).map(|i| rgb16[i * 3 + c]).collect();
        if smp.is_empty() {
            return 0.0;
        }
        smp.sort_unstable();
        smp[smp.len() / 2] as f32
    };
    Ok(DsHistogram {
        bins: BINS,
        r,
        g,
        b,
        clip_low: (clow as f64 / n as f64 * 100.0) as f32,
        clip_high: (chigh as f64 / n as f64 * 100.0) as f32,
        bg: [median_ch(0), median_ch(1), median_ch(2)],
        is_mono,
    })
}

/// LRGB / NARROWBAND CHANNEL COMBINATION (PixInsight ChannelCombination +
/// LRGBCombination parity). Each argument is a pre-stacked MONO master already
/// assigned to an output channel (for SHO the frontend maps SII→R, Ha→G,
/// OIII→B). Channels are star-registered to R, combined into RGB, and — when an
/// L master is given — the luminance is replaced via the chroma-preserving ratio
/// method. Post: optional ABE, SCNR and background neutralization (off for
/// narrowband palettes so the mapped colours are preserved).
#[tauri::command]
async fn deepsky_combine_channels(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    r_path: String,
    g_path: String,
    b_path: String,
    l_path: Option<String>,
    register: Option<bool>,
    neutralize: Option<bool>,
    scnr: Option<bool>,
    gradient: Option<bool>,
) -> Result<String, String> {
    state.license_manager.check_access()?;
    let do_reg = register.unwrap_or(true);
    let do_neut = neutralize.unwrap_or(true);
    let do_scnr = scnr.unwrap_or(do_neut);
    let do_grad = gradient.unwrap_or(true);

    // Load any path as a single mono plane (debayer→luma for CFA, luma for RGB).
    let load_mono = |p: &str| -> Result<DsImage, String> {
        let img = ds_read_image(p)?;
        let img = if let Some(cid) = img.bayer { ds_debayer_image(img, cid) } else { img };
        if img.ch == 1 {
            Ok(img)
        } else {
            let luma = ds_luma(&img);
            Ok(DsImage { data: luma, w: img.w, h: img.h, ch: 1, bayer: None })
        }
    };

    emit_progress(&app, "Combinar canales: cargando masters...", 6.0, None);
    let rimg = load_mono(&r_path)?;
    let (w, h) = (rimg.w, rimg.h);
    let gimg = load_mono(&g_path)?;
    let bimg = load_mono(&b_path)?;

    // Reference stars from R for co-registration.
    emit_progress(&app, "Combinar canales: registrando canales a R...", 22.0, None);
    let ref_stars = ds_detect_stars(&rimg.data, w, h, 120);
    let align = |img: &DsImage, name: &str| -> Result<Vec<f32>, String> {
        if img.w != w || img.h != h {
            return Err(format!(
                "El canal {} es {}×{} pero R es {}×{} — todos los canales deben coincidir.",
                name, img.w, img.h, w, h
            ));
        }
        if !do_reg {
            return Ok(img.data.clone());
        }
        let st = ds_detect_stars(&img.data, w, h, 120);
        match ds_match_triangles(&ref_stars, &st) {
            Some((t, inl)) => {
                log_to_front(&app, "INFO", &format!("Canal {} registrado a R ({} inliers).", name, inl));
                Ok(ds_warp_single(img, t, w, h))
            }
            None => {
                log_to_front(&app, "WARN", &format!("Canal {}: registro estelar falló, se asume alineado.", name));
                Ok(img.data.clone())
            }
        }
    };

    let gd = align(&gimg, "G")?;
    let bd = align(&bimg, "B")?;

    let n = w * h;
    let mut final_data = vec![0.0f32; n * 3];
    for i in 0..n {
        final_data[i * 3] = rimg.data[i];
        final_data[i * 3 + 1] = gd[i];
        final_data[i * 3 + 2] = bd[i];
    }

    // Optional luminance (LRGB): replace the RGB luma with L, preserving chroma
    // via the ratio method (R'=R·L/Y). L carries the detail/SNR, RGB the colour.
    if let Some(lp) = l_path.filter(|s| !s.is_empty()) {
        emit_progress(&app, "Combinar canales: aplicando luminancia (LRGB)...", 55.0, None);
        let limg = load_mono(&lp)?;
        let ld = align(&limg, "L")?;
        for i in 0..n {
            let y = 0.2126 * final_data[i * 3] + 0.7152 * final_data[i * 3 + 1] + 0.0722 * final_data[i * 3 + 2];
            let l = ld[i];
            if y > 1.0 {
                let k = (l / y).clamp(0.0, 8.0);
                final_data[i * 3] = (final_data[i * 3] * k).min(65535.0);
                final_data[i * 3 + 1] = (final_data[i * 3 + 1] * k).min(65535.0);
                final_data[i * 3 + 2] = (final_data[i * 3 + 2] * k).min(65535.0);
            } else {
                final_data[i * 3] = l;
                final_data[i * 3 + 1] = l;
                final_data[i * 3 + 2] = l;
            }
        }
    }

    // Post-processing (same operators as the stack pipeline).
    if do_grad {
        emit_progress(&app, "Combinar canales: extrayendo gradiente (ABE)...", 78.0, None);
        ds_extract_background_gradient(&mut final_data, w, h, 3);
    }
    if do_scnr {
        emit_progress(&app, "Combinar canales: SCNR (elimina verde)...", 86.0, None);
        ds_scnr_green(&mut final_data, 3, 1.0);
    }
    if do_neut {
        emit_progress(&app, "Combinar canales: neutralización de fondo...", 90.0, None);
        let off = ds_neutralize_background(&mut final_data, w, h, 3);
        log_to_front(&app, "INFO", &format!("Neutralización R/G/B = {:.0}/{:.0}/{:.0} ADU.", off[0], off[1], off[2]));
    }

    // Result → linear RGB16 + STF preview (same as the stacker).
    emit_progress(&app, "Combinar canales: estiramiento automático (STF)...", 95.0, None);
    let mut rgb16 = vec![0u16; n * 3];
    for i in 0..n * 3 {
        rgb16[i] = final_data[i].clamp(0.0, 65535.0) as u16;
    }
    let preview8 = ds_render_stretch(&rgb16, w, h, false, 2.8, 0.25);
    let mut rgba = Vec::with_capacity(n * 4);
    for i in 0..n {
        rgba.push(preview8[i * 3]);
        rgba.push(preview8[i * 3 + 1]);
        rgba.push(preview8[i * 3 + 2]);
        rgba.push(255);
    }
    let img_out = RgbaImage::from_raw(w as u32, h as u32, rgba).ok_or("preview")?;
    let mut enc = Vec::new();
    DynamicImage::ImageRgba8(img_out)
        .write_to(&mut Cursor::new(&mut enc), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    {
        let mut res = state.stacked_image.lock().unwrap();
        *res = Some(StackResult { width: w, height: h, data: rgb16, is_mono: false, is_surface: false });
        state.deconv_cache.lock().unwrap().clear();
        state.wavelet_cache.lock().unwrap().clear();
        state.filter_cache.lock().unwrap().clear();
    }
    log_to_front(&app, "SUCCESS", &format!("Canales combinados {}×{} · registro {} · SCNR {} · neutralización {}.", w, h, if do_reg { "ON" } else { "OFF" }, if do_scnr { "ON" } else { "OFF" }, if do_neut { "ON" } else { "OFF" }));
    emit_progress(&app, "Combinar canales: completado.", 100.0, None);
    Ok(format!("data:image/png;base64,{}", general_purpose::STANDARD.encode(&enc)))
}

/// Technical metadata of one candidate frame (PixInsight-style file listing):
/// dimensions, channels and exposure straight from the FITS header (no pixel
/// data is read — probing hundreds of files stays instant).
#[derive(serde::Serialize, Clone)]
struct DsProbe {
    path: String,
    name: String,
    w: usize,
    h: usize,
    ch: usize,
    exptime: Option<f32>,
    bayer: Option<String>,
    ok: bool,
    error: Option<String>,
}

fn ds_hdr_num(hdu: &fitrs::Hdu, key: &str) -> Option<f64> {
    let raw = format!("{:?}", hdu.value(key)?);
    let clean: String = raw
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
        .collect();
    clean.parse::<f64>().ok()
}

/// Recursively collect image files under a root (FITS/TIF/PNG/JPG). Handles
/// the "folder → per-night subfolders → frames" layout the user described.
fn ds_walk_images(root: &std::path::Path, out: &mut Vec<String>, depth: usize) {
    if depth > 8 {
        return;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            ds_walk_images(&p, out, depth + 1);
        } else if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
            let ext = ext.to_lowercase();
            if matches!(ext.as_str(), "fits" | "fit" | "tif" | "tiff" | "png" | "jpg" | "jpeg") {
                out.push(p.to_string_lossy().to_string());
            }
        }
    }
}

/// Classify a path into a frame bucket by WBPP-style keyword matching. The
/// FILENAME is checked FIRST and wins over the folder — a file named
/// "…DARK_600s.tif" sitting in a "Bias" folder is a dark (this exact case put
/// darks in bias before). Only if the filename has no frame-type keyword do we
/// fall back to the folder path.
fn ds_classify(path: &str) -> &'static str {
    let fname = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let full = path.to_lowercase();

    let check = |s: &str| -> Option<&'static str> {
        let has_flat = s.contains("flat");
        let has_dark = s.contains("dark");
        // "dark" is checked before "bias": a "…DARK…" name is unambiguous even
        // if the folder is "Bias"; flat-darks (flat+dark) are skipped.
        if has_flat && has_dark {
            return Some("skip");
        }
        if has_dark {
            return Some("darks");
        }
        if has_flat {
            return Some("flats");
        }
        if s.contains("bias") || s.contains("offset") {
            return Some("bias");
        }
        if s.contains("light") {
            return Some("lights");
        }
        None
    };
    check(&fname).or_else(|| check(&full)).unwrap_or("lights")
}

#[derive(serde::Serialize)]
struct DsClassified {
    lights: Vec<DsProbe>,
    darks: Vec<DsProbe>,
    flats: Vec<DsProbe>,
    bias: Vec<DsProbe>,
}

/// Scan a folder recursively, auto-classify every image into lights/darks/
/// flats/bias by path keywords, and technical-probe them all (WBPP one-click).
#[tauri::command]
fn deepsky_scan_classify(root: String) -> DsClassified {
    let mut files = Vec::new();
    ds_walk_images(std::path::Path::new(&root), &mut files, 0);
    let probes = deepsky_probe(files);
    let mut out = DsClassified { lights: vec![], darks: vec![], flats: vec![], bias: vec![] };
    for pr in probes {
        match ds_classify(&pr.path) {
            "bias" => out.bias.push(pr),
            "flats" => out.flats.push(pr),
            "darks" => out.darks.push(pr),
            "skip" => {}
            _ => out.lights.push(pr),
        }
    }
    out
}

#[tauri::command]
fn deepsky_probe(paths: Vec<String>) -> Vec<DsProbe> {
    paths
        .par_iter()
        .map(|p| {
            let name = std::path::Path::new(p)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| p.clone());
            let lower = p.to_lowercase();
            let base = DsProbe {
                path: p.clone(),
                name,
                w: 0,
                h: 0,
                ch: 1,
                exptime: None,
                bayer: None,
                ok: false,
                error: None,
            };
            if lower.ends_with(".fits") || lower.ends_with(".fit") {
                match fitrs::Fits::open(p) {
                    Ok(fits) => match fits.iter().next() {
                        Some(hdu) => {
                            let w = ds_hdr_num(&hdu, "NAXIS1").unwrap_or(0.0) as usize;
                            let h = ds_hdr_num(&hdu, "NAXIS2").unwrap_or(0.0) as usize;
                            let ch = ds_hdr_num(&hdu, "NAXIS3").unwrap_or(1.0).max(1.0) as usize;
                            let exptime = ds_hdr_num(&hdu, "EXPTIME")
                                .or_else(|| ds_hdr_num(&hdu, "EXPOSURE"))
                                .map(|v| v as f32);
                            let bayer = ds_bayer_id(&hdu).map(|cid| match cid {
                                8 => "RGGB".to_string(),
                                9 => "GRBG".to_string(),
                                10 => "GBRG".to_string(),
                                _ => "BGGR".to_string(),
                            });
                            DsProbe {
                                w,
                                h,
                                ch: ch.min(3),
                                exptime,
                                bayer,
                                ok: w > 0 && h > 0,
                                ..base
                            }
                        }
                        None => DsProbe {
                            error: Some("FITS sin HDU".into()),
                            ..base
                        },
                    },
                    Err(e) => DsProbe {
                        error: Some(format!("{:?}", e)),
                        ..base
                    },
                }
            } else {
                match image::image_dimensions(p) {
                    Ok((w, h)) => DsProbe {
                        w: w as usize,
                        h: h as usize,
                        ch: 3,
                        ok: true,
                        ..base
                    },
                    Err(e) => DsProbe {
                        error: Some(e.to_string()),
                        ..base
                    },
                }
            }
        })
        .collect()
}

#[tauri::command]
async fn stack_deepsky(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    lights: Vec<String>,
    darks: Vec<String>,
    flats: Vec<String>,
    bias: Vec<String>,
    kappa: Option<f32>,
    sigma_clip: Option<bool>,
    cosmetic: Option<bool>,
    gradient: Option<bool>,
    drizzle: Option<f32>,
    optimize_dark: Option<bool>,
    pixfrac: Option<f32>,
    local_norm: Option<bool>,
    auto_crop: Option<bool>,
) -> Result<String, String> {
    state.license_manager.check_access()?;
    state
        .cancel_requested
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let cancel = state.cancel_requested.clone();
    let kappa = kappa.unwrap_or(3.0).clamp(1.5, 6.0);
    let use_clip = sigma_clip.unwrap_or(true);
    // Cosmetic por defecto SOLO cuando no hay darks (SIRIL-style): con darks
    // el master ya elimina los pixeles calientes.
    let use_cosmetic = cosmetic.unwrap_or(darks.is_empty());
    // Background/colour cleanup OFF by default → clean linear master (WBPP flow).
    let use_gradient = gradient.unwrap_or(false);
    let drz = drizzle.unwrap_or(1.0).clamp(1.0, 3.0);
    // Dark optimization por defecto ON cuando hay darks Y bias (necesita bias
    // para aislar la señal térmica); escala el master dark a cada light.
    let use_dark_opt = optimize_dark.unwrap_or(!darks.is_empty() && !bias.is_empty());
    let pixfrac = pixfrac.unwrap_or(0.8).clamp(0.4, 1.0); // drizzle drop shrink
    let use_local_norm = local_norm.unwrap_or(false); // opt-in local normalization
    let use_crop = auto_crop.unwrap_or(true); // trim low-coverage borders

    if lights.is_empty() {
        return Err("Selecciona al menos 1 light.".into());
    }
    let single_light = lights.len() == 1; // calibrar + estirar, sin integracion

    // --- 1. CALIBRATION MASTERS ---
    emit_progress(&app, "Cielo Profundo: creando masters de calibracion...", 2.0, None);
    let master_bias = ds_build_master(&app, &bias, "bias");
    let mut master_dark = ds_build_master(&app, &darks, "dark");
    if let (Some(d), Some(b)) = (&mut master_dark, &master_bias) {
        if d.w == b.w && d.h == b.h && d.ch == b.ch {
            for (dv, bv) in d.data.iter_mut().zip(b.data.iter()) {
                *dv = (*dv - *bv).max(0.0);
            }
        }
    }
    let master_flat = ds_build_master(&app, &flats, "flat").map(|mut f| {
        if let Some(b) = &master_bias {
            if f.w == b.w && f.h == b.h && f.ch == b.ch {
                for (fv, bv) in f.data.iter_mut().zip(b.data.iter()) {
                    *fv = (*fv - *bv).max(1.0);
                }
            }
        }
        // Normalize to mean 1.0 (per full image — vignetting correction).
        let mean = (f.data.iter().map(|&v| v as f64).sum::<f64>() / f.data.len() as f64).max(1.0);
        for v in f.data.iter_mut() {
            *v = (*v as f64 / mean) as f32;
        }
        f
    });

    // --- 2. LOAD + CALIBRATE + DETECT (streaming, dims locked to 1st light) ---
    let mut frames: Vec<(String, Vec<(f32, f32, f32)>, f32, f32, f32)> = Vec::new();
    let mut dims: Option<(usize, usize, usize)> = None;
    let cache_dir = std::env::temp_dir().join("astro_stacker_cache");
    let _ = std::fs::create_dir_all(&cache_dir);
    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let cache_path = |i: usize| cache_dir.join(format!("ds_{}_{}.lz4", run_id, i));

    for (i, p) in lights.iter().enumerate() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("Cancelado por el usuario".into());
        }
        emit_progress(
            &app,
            &format!("Cielo Profundo: calibrando y detectando estrellas {}/{}", i + 1, lights.len()),
            2.0 + (i as f32 / lights.len() as f32) * 28.0,
            None,
        );
        let mut img = match ds_read_image(p) {
            Ok(v) => v,
            Err(e) => {
                log_to_front(&app, "WARN", &format!("Light omitido {} ({})", p, e));
                continue;
            }
        };
        // Consistency check on the RAW dimensions (before debayer changes ch).
        match dims {
            None => {}
            Some((w, h, _)) => {
                if img.w != w || img.h != h {
                    log_to_front(&app, "WARN", &format!("Light con dimensiones distintas, omitido: {}", p));
                    continue;
                }
            }
        }
        // CORRECT ORDER: calibrate in the RAW (CFA) space, THEN debayer.
        // Dark optimization: scale the master dark to best cancel this light's
        // thermal signal (SIRIL-style); k=1 when off or not estimable.
        let dark_k = if use_dark_opt {
            ds_optimize_dark_scale(&img, &master_bias, &master_dark)
        } else {
            1.0
        };
        if use_dark_opt && (dark_k - 1.0).abs() > 0.02 && frames.is_empty() {
            log_to_front(&app, "INFO", &format!("Optimización de dark: escala k≈{:.2} (ajuste térmico).", dark_k));
        }
        ds_calibrate(&mut img, &master_bias, &master_dark, &master_flat, dark_k);
        if use_cosmetic {
            ds_cosmetic_hot_pixels(&mut img);
        }
        if let Some(cid) = img.bayer {
            img = ds_debayer_image(img, cid);
        }
        // Lock the output geometry from the FIRST fully-processed light.
        if dims.is_none() {
            dims = Some((img.w, img.h, img.ch));
        }
        let luma = ds_luma(&img);
        let stars = ds_detect_stars(&luma, img.w, img.h, 120);
        let fwhm = ds_frame_fwhm_proxy(&luma, img.w, img.h, &stars);
        // Background noise scale for SNR-aware weighting (WBPP PSF Signal Weight).
        let (_, noise) = ds_bg_noise(&luma);
        // Star eccentricity (0 round → 1 elongated): wind/tracking quality.
        let ecc = ds_frame_roundness(&luma, img.w, img.h, &stars);
        if stars.len() < 6 && !single_light {
            log_to_front(&app, "WARN", &format!("Muy pocas estrellas ({}) — omitido: {}", stars.len(), p));
            continue;
        }
        // Cache the calibrated frame (u16 LZ4) for the integration passes.
        let u16_data: Vec<u16> = img.data.iter().map(|&v| v.clamp(0.0, 65535.0) as u16).collect();
        let ser = bincode::serialize(&u16_data).map_err(|e| e.to_string())?;
        std::fs::write(cache_path(frames.len()), lz4_flex::compress_prepend_size(&ser))
            .map_err(|e| format!("Cache de frame: {}", e))?;
        frames.push((p.clone(), stars, fwhm, noise, ecc));
    }

    let (w, h, ch) = dims.ok_or("Ningun light valido.")?;
    // DRIZZLE output canvas (integer upscale during integration): with dithered
    // subframes each landing at a different sub-pixel offset in the finer grid,
    // this recovers resolution and cuts pixelation (PixInsight/DSS drizzle).
    let w_out = ((w as f32 * drz).round() as usize).max(w);
    let h_out = ((h as f32 * drz).round() as usize).max(h);
    if drz > 1.01 {
        log_to_front(&app, "INFO", &format!("Drizzle {:.0}×: lienzo {}×{} → {}×{}.", drz, w, h, w_out, h_out));
    }
    if frames.is_empty() {
        return Err("Ningun light utilizable tras calibracion/deteccion.".into());
    }

    // --- 3. REFERENCE + REGISTRATION ---
    // Reference = the highest-QUALITY frame (sharpest + richest + roundest), not
    // merely the one with most stars — a good reference improves every frame's
    // registration accuracy (PixInsight picks by a quality metric too).
    let ref_idx = frames
        .iter()
        .enumerate()
        .max_by(|(_, (_, sa, fa, _, ea)), (_, (_, sb, fb, _, eb))| {
            let score = |s: usize, f: f32, e: f32| (s as f32) / f.max(0.5) * (1.0 - e).max(0.1);
            score(sa.len(), *fa, *ea)
                .partial_cmp(&score(sb.len(), *fb, *eb))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
        .unwrap_or(0);
    let ref_stars = frames[ref_idx].1.clone();
    {
        let rf = &frames[ref_idx];
        let nm = std::path::Path::new(&rf.0).file_name().and_then(|s| s.to_str()).unwrap_or(&rf.0);
        log_to_front(
            &app,
            "INFO",
            &format!("Referencia (mejor calidad): {} · {} estrellas · FWHM {:.2} · ecc {:.2}.", nm, ref_stars.len(), rf.2, rf.4),
        );
    }

    emit_progress(&app, "Cielo Profundo: registrando (triangulos estelares)...", 32.0, None);
    let transforms: Vec<Option<((f32, f32, f32, f32), usize)>> = frames
        .par_iter()
        .enumerate()
        .map(|(i, (_, stars, _, _, _))| {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            if i == ref_idx {
                return Some(((1.0, 0.0, 0.0, 0.0), stars.len()));
            }
            ds_match_triangles(&ref_stars, stars)
        })
        .collect();
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("Cancelado por el usuario".into());
    }

    // WBPP-style quality weight (PSF Signal Weight proxy): sharper frames (lower
    // FWHM), richer star fields, lower background noise AND rounder stars weigh
    // more. Floor 0.3 so no accepted frame is wasted, cap 1.0.
    let best_fwhm = frames
        .iter()
        .map(|(_, _, f, _, _)| *f)
        .filter(|f| *f > 0.0)
        .fold(f32::MAX, f32::min);
    let max_stars = frames.iter().map(|(_, s, _, _, _)| s.len()).max().unwrap_or(1) as f32;
    let best_noise = frames
        .iter()
        .map(|(_, _, _, nz, _)| *nz)
        .filter(|n| *n > 0.0)
        .fold(f32::MAX, f32::min);
    let mut worst_ecc = (0.0f32, String::new()); // (ecc, name) para avisar
    let registered: Vec<(usize, (f32, f32, f32, f32), f64)> = transforms
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            t.map(|(tr, _)| {
                let (ref name, ref stars, fwhm, noise, ecc) = frames[i];
                let wf = if fwhm > 0.0 && best_fwhm.is_finite() {
                    (best_fwhm / fwhm).powi(2)
                } else {
                    1.0
                };
                let ws = (stars.len() as f32 / max_stars).sqrt();
                // Noise term: lower background noise ⇒ higher SNR ⇒ more weight.
                let wn = if noise > 0.0 && best_noise.is_finite() {
                    (best_noise / noise).clamp(0.3, 1.0)
                } else {
                    1.0
                };
                // Roundness term: elongated stars (wind/tracking) ⇒ less weight.
                let we = (1.0 - ecc).clamp(0.4, 1.0);
                if ecc > worst_ecc.0 {
                    worst_ecc = (ecc, name.clone());
                }
                (i, tr, (wf * ws * wn * we).clamp(0.3, 1.0) as f64)
            })
        })
        .collect();
    if worst_ecc.0 > 0.55 {
        let nm = std::path::Path::new(&worst_ecc.1).file_name().and_then(|s| s.to_str()).unwrap_or(&worst_ecc.1);
        log_to_front(&app, "INFO", &format!("Estrellas más alargadas: ecc {:.2} en {} (peso reducido).", worst_ecc.0, nm));
    }
    let rejected = frames.len() - registered.len();

    // WBPP-style per-frame QUALITY REPORT: surface every metric (FWHM proxy,
    // eccentricity, noise, star count, final weight, used/rejected) to the UI.
    {
        let wmap: std::collections::HashMap<usize, f64> = registered.iter().map(|&(i, _, w)| (i, w)).collect();
        let is_ref_idx = ref_idx;
        let report: Vec<serde_json::Value> = frames
            .iter()
            .enumerate()
            .map(|(i, (path, stars, fwhm, noise, ecc))| {
                let name = std::path::Path::new(path).file_name().and_then(|s| s.to_str()).unwrap_or(path);
                let w = wmap.get(&i).copied();
                serde_json::json!({
                    "name": name,
                    "stars": stars.len(),
                    "fwhm": (*fwhm * 100.0).round() / 100.0,
                    "ecc": (*ecc * 1000.0).round() / 1000.0,
                    "noise": noise.round(),
                    "weight": w.map(|v| (v * 1000.0).round() / 1000.0),
                    "used": w.is_some(),
                    "reference": i == is_ref_idx,
                })
            })
            .collect();
        let _ = app.emit("ds-report", &report);
    }
    if registered.is_empty() {
        return Err("El registro estelar fallo en todos los frames (¿campos distintos?).".into());
    }
    if rejected > 0 {
        log_to_front(&app, "WARN", &format!("{} frame(s) no registraron y fueron descartados.", rejected));
    }

    let load_cached = |i: usize| -> Result<DsImage, String> {
        let raw = std::fs::read(cache_path(i)).map_err(|e| e.to_string())?;
        let ser = lz4_flex::decompress_size_prepended(&raw).map_err(|e| e.to_string())?;
        let u16_data: Vec<u16> = bincode::deserialize(&ser).map_err(|e| e.to_string())?;
        Ok(DsImage {
            data: u16_data.into_iter().map(|v| v as f32).collect(),
            w,
            h,
            ch,
            bayer: None,
        })
    };

    // --- 3.5 PER-FRAME NORMALIZATION (SIRIL/WBPP): match every frame's sky
    // level and noise scale to the reference so moon/cloud/gradient variations
    // don't bias the stacked background and the σ-clip stays effective. ---
    emit_progress(&app, "Cielo Profundo: normalizando frames al de referencia...", 33.0, None);
    let (bg_ref, nz_ref) = {
        let img = load_cached(ref_idx)?;
        ds_bg_noise(&ds_luma(&img))
    };
    let mut norms: Vec<(f32, f32)> = Vec::with_capacity(registered.len());
    for &(i, _, _) in &registered {
        if i == ref_idx {
            norms.push((1.0, 0.0));
            continue;
        }
        let img = load_cached(i)?;
        let (bg, nz) = ds_bg_noise(&ds_luma(&img));
        let mul = (nz_ref / nz.max(1e-3)).clamp(0.5, 2.0);
        let add = bg_ref - bg * mul;
        norms.push((mul, add));
    }

    // --- 3.6 LOCAL NORMALIZATION (opt-in, PixInsight LocalNormalization): a
    // coarse per-cell sky-offset field per frame (vs the reference, in ref
    // space) so spatially-varying gradients — drifting light pollution, a moon
    // gradient — are matched LOCALLY, not just by one global offset. Applied
    // additively during integration; the global scalar offset is folded in. ---
    const LN_G: usize = 24;
    let mut loc_fields: Vec<Option<Vec<f32>>> = vec![None; registered.len()];
    if use_local_norm {
        emit_progress(&app, "Cielo Profundo: normalización local (rejilla)...", 34.0, None);
        let ref_grid = {
            let img = load_cached(ref_idx)?;
            ds_local_bg_grid(&ds_luma(&img), img.w, img.h, LN_G, LN_G)
        };
        for (k, &(i, t, _)) in registered.iter().enumerate() {
            if i == ref_idx {
                continue;
            }
            let img = load_cached(i)?;
            let mono = DsImage { data: ds_luma(&img), w: img.w, h: img.h, ch: 1, bayer: None };
            // Warp this frame's luma into reference space so the cells align.
            let warped = ds_warp_single(&mono, t, img.w, img.h);
            let k_grid = ds_local_bg_grid(&warped, img.w, img.h, LN_G, LN_G);
            let mul = norms[k].0;
            // Offset consistent with v = vraw·mul + field (scalar add zeroed below).
            let field: Vec<f32> = ref_grid
                .iter()
                .zip(k_grid.iter())
                .map(|(r, kk)| r - mul * kk)
                .collect();
            loc_fields[k] = Some(field);
        }
        for nrm in norms.iter_mut() {
            nrm.1 = 0.0;
        }
    }

    // --- 4. INTEGRATION: PASS 1 (mean + variance) — output at drizzle res ---
    let npx = w_out * h_out;
    let mut sum1 = vec![0.0f64; npx * ch];
    let mut sq1 = vec![0.0f64; npx * ch];
    let mut wgt1 = vec![0.0f64; npx];
    for (k, &(i, t, fw)) in registered.iter().enumerate() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("Cancelado por el usuario".into());
        }
        emit_progress(
            &app,
            &format!("Cielo Profundo: integrando (pasada 1) {}/{}", k + 1, registered.len()),
            35.0 + (k as f32 / registered.len() as f32) * 30.0,
            None,
        );
        let img = load_cached(i)?;
        let loc_ref = loc_fields[k].as_ref().map(|f| (f.as_slice(), LN_G, LN_G));
        if drz > 1.01 {
            ds_drizzle_accumulate(&img, t, &mut sum1, Some(&mut sq1), &mut wgt1, None, w_out, h_out, ch, fw, drz, pixfrac, norms[k], loc_ref);
        } else {
            ds_warp_accumulate(&img, t, &mut sum1, Some(&mut sq1), &mut wgt1, None, w_out, h_out, ch, fw, drz, norms[k], loc_ref);
        }
    }

    let mean1: Vec<f32> = (0..npx * ch)
        .map(|i| {
            let wv = wgt1[i / ch];
            if wv > 0.0 { (sum1[i] / wv) as f32 } else { 0.0 }
        })
        .collect();

    let final_data: Vec<f32> = if use_clip && registered.len() >= 4 {
        // Kappa-sigma window from pass-1 statistics.
        let mut lo = vec![f32::MIN; npx * ch];
        let mut hi = vec![f32::MAX; npx * ch];
        for i in 0..npx * ch {
            let wv = wgt1[i / ch];
            if wv > 1.0 {
                let mu = sum1[i] / wv;
                let var = (sq1[i] / wv - mu * mu).max(0.0);
                let sd = var.sqrt().max(4.0);
                lo[i] = (mu - kappa as f64 * sd) as f32;
                hi[i] = (mu + kappa as f64 * sd) as f32;
            }
        }
        drop(sum1);
        drop(sq1);

        // --- PASS 2: clipped mean (satellites/cosmics rejected) ---
        let mut sum2 = vec![0.0f64; npx * ch];
        let mut wgt2 = vec![0.0f64; npx];
        for (k, &(i, t, fw)) in registered.iter().enumerate() {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("Cancelado por el usuario".into());
            }
            emit_progress(
                &app,
                &format!("Cielo Profundo: integrando (pasada 2, σ-clip) {}/{}", k + 1, registered.len()),
                65.0 + (k as f32 / registered.len() as f32) * 28.0,
                None,
            );
            let img = load_cached(i)?;
            let loc_ref = loc_fields[k].as_ref().map(|f| (f.as_slice(), LN_G, LN_G));
            if drz > 1.01 {
                ds_drizzle_accumulate(&img, t, &mut sum2, None, &mut wgt2, Some((&lo, &hi)), w_out, h_out, ch, fw, drz, pixfrac, norms[k], loc_ref);
            } else {
                ds_warp_accumulate(&img, t, &mut sum2, None, &mut wgt2, Some((&lo, &hi)), w_out, h_out, ch, fw, drz, norms[k], loc_ref);
            }
        }
        (0..npx * ch)
            .map(|i| {
                let wv = wgt2[i / ch];
                if wv > 0.0 { (sum2[i] / wv) as f32 } else { mean1[i] }
            })
            .collect()
    } else {
        mean1.clone()
    };

    // Cleanup frame cache.
    for i in 0..frames.len() {
        let _ = std::fs::remove_file(cache_path(i));
    }

    // --- 4.4 AUTO-CROP low-coverage borders (dithered/rotated stacks) ---
    let (mut final_data, w_out, h_out, npx) = if use_crop {
        let (cropped, nw, nh) = ds_autocrop(&final_data, &wgt1, w_out, h_out, ch);
        if nw != w_out || nh != h_out {
            log_to_front(&app, "INFO", &format!("Recorte de bordes: {}×{} → {}×{} (cobertura parcial).", w_out, h_out, nw, nh));
        }
        (cropped, nw, nh, nw * nh)
    } else {
        (final_data, w_out, h_out, w_out * h_out)
    };

    // Diagnostic: the LINEAR master's value range (helps spot a dead/clipped
    // integration before any cosmetic step touches it).
    {
        let luma = if ch == 3 {
            final_data.chunks_exact(3).map(|p| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]).collect::<Vec<f32>>()
        } else {
            final_data.clone()
        };
        let (bg, nz) = ds_bg_noise(&luma);
        let mx = luma.iter().cloned().fold(0.0f32, f32::max);
        let nonzero = luma.iter().filter(|&&v| v > 1.0).count() as f64 / luma.len().max(1) as f64 * 100.0;
        log_to_front(
            &app,
            "INFO",
            &format!("Máster lineal: fondo≈{:.0} ADU · ruido≈{:.0} · máx {:.0} · {:.0}% con señal.", bg, nz, mx, nonzero),
        );
    }

    // --- 4.5 OPTIONAL BACKGROUND & COLOR CLEANUP (OFF by default = WBPP flow) ---
    // WBPP/PixInsight output a CLEAN LINEAR master; background extraction and
    // colour calibration are DELIBERATE post steps (with masks/previews), not
    // baked into integration. So this only runs when the user opts in — a safe
    // degree-2 ABE + SCNR + neutralization for a one-click finished look.
    if use_gradient {
        emit_progress(&app, "Cielo Profundo: limpieza de fondo y color (ABE + SCNR)...", 93.0, None);
        ds_extract_background_gradient(&mut final_data, w_out, h_out, ch);
        if ch == 3 {
            ds_scnr_green(&mut final_data, ch, 1.0);
            let off = ds_neutralize_background(&mut final_data, w_out, h_out, ch);
            log_to_front(
                &app,
                "INFO",
                &format!("Limpieza fondo/color: offsets R/G/B = {:.0}/{:.0}/{:.0} ADU.", off[0], off[1], off[2]),
            );
        }
    }

    // --- 5. RESULT (LINEAR) + AUTO STF PREVIEW (linked) ---
    emit_progress(&app, "Cielo Profundo: estiramiento automatico (STF)...", 96.0, None);
    let mut rgb16 = vec![0u16; npx * 3];
    for i in 0..npx {
        for c in 0..3 {
            let v = final_data[i * ch + c.min(ch - 1)];
            rgb16[i * 3 + c] = v.clamp(0.0, 65535.0) as u16;
        }
    }

    let preview8 = ds_render_stretch(&rgb16, w_out, h_out, false, 2.8, 0.25);
    let mut rgba = Vec::with_capacity(npx * 4);
    for i in 0..npx {
        rgba.push(preview8[i * 3]);
        rgba.push(preview8[i * 3 + 1]);
        rgba.push(preview8[i * 3 + 2]);
        rgba.push(255);
    }
    let img_out = RgbaImage::from_raw(w_out as u32, h_out as u32, rgba).ok_or("preview")?;
    let mut enc = Vec::new();
    DynamicImage::ImageRgba8(img_out)
        .write_to(&mut Cursor::new(&mut enc), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;

    {
        let mut res = state.stacked_image.lock().unwrap();
        *res = Some(StackResult {
            width: w_out,
            height: h_out,
            data: rgb16,
            is_mono: ch == 1,
            is_surface: false,
        });
        state.deconv_cache.lock().unwrap().clear();
        state.wavelet_cache.lock().unwrap().clear();
        state.filter_cache.lock().unwrap().clear();
    }

    log_to_front(
        &app,
        "SUCCESS",
        &format!(
            "Cielo Profundo: {} frames ({} rechazados) · κ={} · σ-clip {} · cosmética {} · limpieza-fondo/color {} · opt.dark {} · drizzle {} · norm.local {} · peso FWHM+estrellas+ruido+redondez.",
            registered.len(),
            rejected,
            kappa,
            if use_clip { "ON" } else { "OFF" },
            if use_cosmetic { "ON" } else { "OFF" },
            if use_gradient { "ON" } else { "OFF" },
            if use_dark_opt { "ON" } else { "OFF" },
            if drz > 1.01 { format!("{:.0}× drop pf{:.2}", drz, pixfrac) } else { "OFF".to_string() },
            if use_local_norm { "ON" } else { "OFF" }
        ),
    );
    emit_progress(&app, "Cielo Profundo: completado.", 100.0, None);
    Ok(format!(
        "data:image/png;base64,{}",
        general_purpose::STANDARD.encode(&enc)
    ))
}

#[cfg(test)]
mod ds_tests {
    use super::*;

    fn synth_field(stars: &[(f32, f32, f32)], w: usize, h: usize) -> Vec<f32> {
        let mut img = vec![500.0f32; w * h];
        // deterministic noise ±25
        for (i, v) in img.iter_mut().enumerate() {
            let mut z = (i as u32) ^ 0x9E37_79B9;
            z ^= z >> 16;
            z = z.wrapping_mul(0x85EB_CA6B);
            z ^= z >> 13;
            *v += (z % 51) as f32 - 25.0;
        }
        for &(sx, sy, flux) in stars {
            for dy in -5i32..=5 {
                for dx in -5i32..=5 {
                    let x = sx + dx as f32;
                    let y = sy + dy as f32;
                    if x < 0.0 || y < 0.0 || x >= w as f32 || y >= h as f32 {
                        continue;
                    }
                    let d2 = (x - sx).powi(2) + (y - sy).powi(2);
                    img[(y as usize) * w + x as usize] += flux * (-d2 / (2.0 * 1.4 * 1.4)).exp();
                }
            }
        }
        img
    }

    fn star_grid(n: usize, w: usize, h: usize) -> Vec<(f32, f32, f32)> {
        // pseudo-random but deterministic positions, min separation by grid
        let mut out = Vec::new();
        let mut z = 12345u32;
        while out.len() < n {
            z ^= z << 13;
            z ^= z >> 17;
            z ^= z << 5;
            let x: f32 = 20.0 + (z % (w as u32 - 40)) as f32;
            z ^= z << 13;
            z ^= z >> 17;
            z ^= z << 5;
            let y: f32 = 20.0 + (z % (h as u32 - 40)) as f32;
            if out
                .iter()
                .all(|&(sx, sy, _): &(f32, f32, f32)| (sx - x).powi(2) + (sy - y).powi(2) > 400.0)
            {
                let flux = 3000.0 + (z % 20000) as f32;
                out.push((x, y, flux));
            }
        }
        out
    }

    #[test]
    fn test_ds_gradient_extraction_removes_tilt() {
        // Synthetic sky: flat background 2000 + strong linear+quadratic
        // gradient + a few stars. After ABE the background must be flat
        // (tilt gone) while the median LEVEL is preserved.
        let (w, h) = (256usize, 256usize);
        let mut img = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let xn = x as f32 / w as f32;
                let yn = y as f32 / h as f32;
                img[y * w + x] = 2000.0 + 3000.0 * xn + 1500.0 * yn * yn;
            }
        }
        // a few bright stars must not bias the fit
        for &(sx, sy) in &[(40usize, 60usize), (180, 90), (120, 200)] {
            for dy in 0..3 {
                for dx in 0..3 {
                    img[(sy + dy) * w + sx + dx] = 40000.0;
                }
            }
        }
        let before_corner_delta = {
            let tl = img[10 * w + 10];
            let br = img[(h - 10) * w + (w - 10)];
            (br - tl).abs()
        };
        assert!(before_corner_delta > 2000.0, "el sintético debe tener gradiente");

        ds_extract_background_gradient(&mut img, w, h, 1);

        let tl = img[10 * w + 10];
        let br = img[(h - 10) * w + (w - 10)];
        let tr = img[10 * w + (w - 10)];
        let bl = img[(h - 10) * w + 10];
        let maxd = [tl, br, tr, bl]
            .iter()
            .fold((f32::MAX, f32::MIN), |(mn, mx), &v| (mn.min(v), mx.max(v)));
        assert!(
            maxd.1 - maxd.0 < 220.0,
            "gradiente residual {} tras ABE",
            maxd.1 - maxd.0
        );
        // level preserved: corners should sit near the median of the original
        // fitted background (~3700), definitely not near zero
        assert!(tl > 2500.0 && tl < 5200.0, "nivel de fondo perdido: {}", tl);
    }

    #[test]
    fn test_ds_star_detection_finds_synthetic_stars() {
        let (w, h) = (512usize, 512usize);
        let truth = star_grid(50, w, h);
        let img = synth_field(&truth, w, h);
        let found = ds_detect_stars(&img, w, h, 120);
        assert!(found.len() >= 40, "solo {} estrellas detectadas", found.len());
        // ≥80% of the true stars matched within 0.8 px
        let mut matched = 0;
        for &(tx, ty, _) in &truth {
            if found
                .iter()
                .any(|&(fx, fy, _)| (fx - tx).powi(2) + (fy - ty).powi(2) < 0.64)
            {
                matched += 1;
            }
        }
        assert!(matched >= 40, "solo {} coincidencias sub-pixel", matched);
    }

    #[test]
    fn test_ds_triangle_registration_recovers_transform() {
        let (w, h) = (512usize, 512usize);
        let ref_stars = star_grid(45, w, h);
        // Known similarity: rot 2.5°, scale 1.01, shift (24.3, −11.7)
        let ang = 2.5f32.to_radians();
        let s = 1.01f32;
        let (a, b) = (s * ang.cos(), s * ang.sin());
        let t_true = (a, b, 24.3f32, -11.7f32);
        // target stars = T⁻¹? — target→ref transform is what we solve; build
        // target as the INVERSE mapping of ref stars.
        let det = a * a + b * b;
        let tgt_stars: Vec<(f32, f32, f32)> = ref_stars
            .iter()
            .map(|&(x, y, f)| {
                let dx = x - t_true.2;
                let dy = y - t_true.3;
                ((a * dx + b * dy) / det, (-b * dx + a * dy) / det, f)
            })
            .collect();

        let (t_est, inliers) =
            ds_match_triangles(&ref_stars, &tgt_stars).expect("registro fallido");
        assert!(inliers >= 20, "pocos inliers: {}", inliers);
        // Verify corner mapping accuracy < 0.35 px
        for &(cx, cy) in &[(30.0f32, 30.0f32), (480.0, 30.0), (30.0, 480.0), (480.0, 480.0)] {
            // true target position of this ref corner:
            let dx = cx - t_true.2;
            let dy = cy - t_true.3;
            let tx = (a * dx + b * dy) / det;
            let ty = (-b * dx + a * dy) / det;
            let back = ds_apply_t(t_est, tx, ty);
            let err = ((back.0 - cx).powi(2) + (back.1 - cy).powi(2)).sqrt();
            assert!(err < 0.35, "error de registro {} px en esquina", err);
        }
    }

    #[test]
    fn test_ds_dark_optimization_recovers_scale() {
        // A light built as bias + k_true·D_cal + sky. The dark-optimization must
        // recover k_true from the hot-pixel population (robust to the sky pedestal).
        let (w, h, ch) = (128usize, 128usize, 1usize);
        let n = w * h;
        let bias = DsImage { data: vec![200.0; n], w, h, ch, bayer: None };
        // D_cal: mostly 0, two hot-pixel populations (2000 and 4000).
        let mut dcal = vec![0.0f32; n];
        for i in (5..n).step_by(37) { dcal[i] = 2000.0; }
        for i in (11..n).step_by(53) { dcal[i] = 4000.0; }
        let dark = DsImage { data: dcal.clone(), w, h, ch, bayer: None };
        let k_true = 1.5f32;
        let sky = 40.0f32;
        let light = DsImage {
            data: (0..n).map(|i| 200.0 + k_true * dcal[i] + sky).collect(),
            w, h, ch, bayer: None,
        };
        let k = ds_optimize_dark_scale(&light, &Some(bias.clone()), &Some(dark));
        assert!((k - k_true).abs() < 0.08, "k recuperado {} esperado ~{}", k, k_true);
        // Sanity: with no dark it must be a no-op (k = 1.0).
        let none: Option<DsImage> = None;
        assert_eq!(ds_optimize_dark_scale(&light, &Some(bias), &none), 1.0);
    }

    #[test]
    fn test_ds_drizzle_conserves_flux_and_mean() {
        // Constant input, identity transform, 2× drizzle, pixfrac 1.0. The
        // drop-kernel must conserve flux (total weight ≈ N·(pixfrac·scale)²) and
        // preserve the value (every covered output pixel ≈ the input level).
        let (w, h, ch) = (64usize, 64usize, 1usize);
        let img = DsImage { data: vec![1000.0f32; w * h], w, h, ch, bayer: None };
        let scale = 2.0f32;
        let pixfrac = 1.0f32;
        let (wo, ho) = (w * 2, h * 2);
        let mut sum = vec![0.0f64; wo * ho * ch];
        let mut wgt = vec![0.0f64; wo * ho];
        ds_drizzle_accumulate(
            &img, (1.0, 0.0, 0.0, 0.0), &mut sum, None, &mut wgt, None,
            wo, ho, ch, 1.0, scale, pixfrac, (1.0, 0.0), None,
        );
        let tw: f64 = wgt.iter().sum();
        let expected = (w * h) as f64 * (pixfrac * scale).powi(2) as f64;
        // Edge drops fall partly outside → slightly less than the ideal total.
        assert!(tw > expected * 0.9 && tw <= expected * 1.001, "peso total {} vs esperado {}", tw, expected);
        let mean_out = sum.iter().sum::<f64>() / tw;
        assert!((mean_out - 1000.0).abs() < 0.5, "media {} (esperada 1000)", mean_out);
        // A central output pixel must carry the input level.
        let cpix = (ho / 2) * wo + wo / 2;
        assert!(wgt[cpix] > 0.0 && (sum[cpix] / wgt[cpix] - 1000.0).abs() < 0.5, "pixel central {}", sum[cpix] / wgt[cpix].max(1.0));
    }

    #[test]
    fn test_ds_roundness_detects_elongation() {
        let (w, h) = (128usize, 128usize);
        let centers = [(30.0f32, 30.0f32), (90.0, 40.0), (60.0, 90.0), (100.0, 100.0)];
        let star_list: Vec<(f32, f32, f32)> = centers.iter().map(|&(x, y)| (x, y, 3000.0)).collect();

        // Round stars (σ 1.5 in both axes).
        let mut round_img = vec![100.0f32; w * h];
        for &(sx, sy) in &centers {
            for dy in -5i32..=5 {
                for dx in -5i32..=5 {
                    let (x, y) = (sx as i32 + dx, sy as i32 + dy);
                    if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
                        continue;
                    }
                    let v = 3000.0 * (-((dx * dx + dy * dy) as f32) / (2.0 * 1.5 * 1.5)).exp();
                    round_img[y as usize * w + x as usize] += v;
                }
            }
        }
        let e_round = ds_frame_roundness(&round_img, w, h, &star_list);
        assert!(e_round < 0.35, "estrellas redondas dieron ecc {}", e_round);

        // Elongated stars (σx 1.2, σy 3.0 → clearly non-round).
        let mut elong_img = vec![100.0f32; w * h];
        for &(sx, sy) in &centers {
            for dy in -6i32..=6 {
                for dx in -6i32..=6 {
                    let (x, y) = (sx as i32 + dx, sy as i32 + dy);
                    if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
                        continue;
                    }
                    let v = 3000.0 * (-(dx * dx) as f32 / (2.0 * 1.2 * 1.2) - (dy * dy) as f32 / (2.0 * 3.0 * 3.0)).exp();
                    elong_img[y as usize * w + x as usize] += v;
                }
            }
        }
        let e_elong = ds_frame_roundness(&elong_img, w, h, &star_list);
        assert!(e_elong > e_round + 0.2, "elongación no detectada: redonda {} alargada {}", e_round, e_elong);
    }

    #[test]
    fn test_ds_autocrop_trims_low_coverage() {
        let (w, h, ch) = (20usize, 20usize, 1usize);
        let data: Vec<f32> = (0..w * h).map(|i| i as f32).collect();
        // Full coverage only in the inner [4,16)×[4,16); ragged low-cov border.
        let mut cov = vec![5.0f64; w * h];
        for y in 4..16 {
            for x in 4..16 {
                cov[y * w + x] = 100.0;
            }
        }
        let (out, nw, nh) = ds_autocrop(&data, &cov, w, h, ch);
        assert_eq!((nw, nh), (12, 12), "recorte {}×{}", nw, nh);
        assert_eq!(out.len(), 12 * 12 * ch);
        assert_eq!(out[0], data[4 * w + 4], "esquina del recorte");
        // Uniform coverage → no crop.
        let uni = vec![50.0f64; w * h];
        let (_, uw, uh) = ds_autocrop(&data, &uni, w, h, ch);
        assert_eq!((uw, uh), (w, h), "cobertura uniforme no debe recortar");
    }
}
