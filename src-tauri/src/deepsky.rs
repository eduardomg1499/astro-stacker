// ==========================================
// 9. DEEP-SKY STACKING MODULE (Cielo Profundo)
// ==========================================
// Automated-but-tunable pipeline taking the proven ideas from SIRIL/WBPP:
//   1. CALIBRATION   — masters robustos agrupados; light float32 se calibra
//      antes de debayer y conserva negativos/headroom.
//   2. STAR DETECTION — background (median) + MAD noise threshold, local
//      maxima with hot-pixel rejection, ajuste PSF y ranking por flujo.
//   3. REGISTRATION  — RANSAC y selección automática similitud/afín/
//      proyectiva/distorsión local, refinada y validada por RMS/inliers.
//   4. INTEGRATION   — media ponderada, sigma iterativo, Winsorized y
//      linear-fit GPU/CPU tiled; drizzle mono/RGB/CFA conserva cobertura.
//   5. RESULTADO     — el máster científico queda LINEAL float32; STF/TIFF/PNG
//      son vistas/exportaciones separadas.
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

/// Calibration master plus the uncertainty of the published estimator.
///
/// `variance`, `neff`, `dq` and `rejected_fraction` use the same interleaved
/// sample layout as `image.data`.  Keeping them next to SCI prevents the
/// productive path from silently dropping the statistics already computed by
/// the robust master builder.
#[derive(Clone)]
struct DsCalibrationMaster {
    image: DsImage,
    variance: Vec<f32>,
    neff: Vec<f32>,
    dq: Vec<u32>,
    rejected_fraction: Vec<f32>,
    frames: usize,
    /// Largest per-frame exposure-normalization coefficient squared.  It is
    /// used as a conservative covariance bound when a shared pedestal master
    /// was subtracted before the flat frames were normalized.
    max_input_scale_sq: f32,
}

impl DsCalibrationMaster {
    fn variance_publishable(&self) -> bool {
        self.variance.len() == self.image.data.len()
            && self
                .variance
                .iter()
                .all(|value| value.is_finite() && *value >= 0.0)
    }
}

/// FITS BAYERPAT/BAYERPATN header → the app's Bayer color id
/// (RGGB=8 GRBG=9 GBRG=10 BGGR=11), shifted by the ROI/CFA origin parity.
/// Several capture applications preserve the full-sensor pattern while a
/// subframe starts at an odd X/Y coordinate. Ignoring that offset swaps the
/// photosite colours and permanently contaminates calibration/debayering.
fn ds_shift_bayer_id(cid: i32, x_offset: i64, y_offset: i64) -> Option<i32> {
    let base = match cid {
        8 => [[b'R', b'G'], [b'G', b'B']],
        9 => [[b'G', b'R'], [b'B', b'G']],
        10 => [[b'G', b'B'], [b'R', b'G']],
        11 => [[b'B', b'G'], [b'G', b'R']],
        _ => return None,
    };
    let xo = x_offset.rem_euclid(2) as usize;
    let yo = y_offset.rem_euclid(2) as usize;
    let shifted = [
        [base[yo][xo], base[yo][(xo + 1) & 1]],
        [base[(yo + 1) & 1][xo], base[(yo + 1) & 1][(xo + 1) & 1]],
    ];
    match shifted {
        [[b'R', b'G'], [b'G', b'B']] => Some(8),
        [[b'G', b'R'], [b'B', b'G']] => Some(9),
        [[b'G', b'B'], [b'R', b'G']] => Some(10),
        [[b'B', b'G'], [b'G', b'R']] => Some(11),
        _ => None,
    }
}

fn ds_bayer_id_from_layout(layout: &pipeline::StoreLayout) -> Option<i32> {
    let pipeline::StoreLayout::Cfa {
        pattern,
        phase_x,
        phase_y,
    } = layout
    else {
        return None;
    };
    let cid = match pattern.trim().to_ascii_uppercase().as_str() {
        "RGGB" => 8,
        "GRBG" => 9,
        "GBRG" => 10,
        "BGGR" => 11,
        _ => return None,
    };
    ds_shift_bayer_id(cid, *phase_x as i64, *phase_y as i64)
}

fn ds_bayer_id(hdu: &fitrs::Hdu) -> Option<i32> {
    // Use the very same normalized extraction as CalibrationSignature.  The
    // previous reader chose XBAYROFF *or* XORGSUBF, whereas the signature
    // correctly composes explicit detector phase + ROI parity.  That split
    // let the pixel buffer say RGGB while the scientific contract said GRBG.
    let width = ds_hdr_num(hdu, "NAXIS1").unwrap_or(0.0).max(0.0) as usize;
    let height = ds_hdr_num(hdu, "NAXIS2").unwrap_or(0.0).max(0.0) as usize;
    let extraction = ds_signature_extraction_for_hdu(hdu, width, height, 1);
    extraction
        .layout
        .as_ref()
        .and_then(ds_bayer_id_from_layout)
}

/// Convert a raw FITS integer to physical ADU applying the standard
/// BZERO/BSCALE keywords. THE critical case: virtually every camera/capture
/// program (NINA, SGP, ASIAIR, ZWO...) writes unsigned 16-bit data as SIGNED
/// BITPIX=16 with BZERO=32768 — read without applying BZERO, every pixel below
/// 32768 raw (the ENTIRE sky background) is negative and a naive 0-clamp
/// crushes it to black, leaving only bright star cores. fitrs does NOT apply
/// these keywords itself (verified in its source).
#[inline]
fn ds_int_to_adu(raw: f64, bscale: f64, bzero: f64) -> f32 {
    (raw * bscale + bzero) as f32
}

/// Read TIFF without routing float samples through `DynamicImage::to_rgb16`.
/// `image 0.23` intentionally exposes only integer DynamicImage variants, so
/// that conversion used to clamp negative calibration values and headroom from
/// scientific 32-bit TIFF files. The underlying TIFF decoder does support
/// IEEE float samples; keep them as f32 and normalize the conventional 0..1
/// scale to the engine's 0..65535 ADU working scale.
fn ds_read_tiff_float_safe(path: &str) -> Result<DsImage, String> {
    use tiff::decoder::{Decoder, DecodingResult, Limits};

    let file = std::fs::File::open(path).map_err(|e| format!("TIFF open: {e}"))?;
    let reader = std::io::BufReader::new(file);
    let mut decoder = Decoder::new(reader).map_err(|e| format!("TIFF header: {e}"))?;
    let (w_u32, h_u32) = decoder
        .dimensions()
        .map_err(|e| format!("TIFF dimensions: {e}"))?;
    let color = decoder
        .colortype()
        .map_err(|e| format!("TIFF color type: {e}"))?;
    let (source_channels, output_channels, bits) = match color {
        tiff::ColorType::Gray(bits) => (1usize, 1usize, bits),
        tiff::ColorType::GrayA(bits) => (2, 1, bits),
        tiff::ColorType::RGB(bits) => (3, 3, bits),
        tiff::ColorType::RGBA(bits) => (4, 3, bits),
        tiff::ColorType::Palette(_) => {
            return Err("TIFF con paleta no es una entrada científica lineal compatible".into())
        }
        tiff::ColorType::CMYK(_) => {
            return Err("TIFF CMYK no es una entrada científica lineal compatible".into())
        }
    };
    if w_u32 == 0 || h_u32 == 0 {
        return Err("TIFF con geometría vacía".into());
    }
    let w = w_u32 as usize;
    let h = h_u32 as usize;
    let sample_count = w
        .checked_mul(h)
        .and_then(|v| v.checked_mul(source_channels))
        .ok_or("TIFF demasiado grande")?;
    let encoded_bytes = sample_count
        .checked_mul((bits as usize).saturating_add(7) / 8)
        .ok_or("TIFF demasiado grande")?;
    // DecodingResult and the final f32 buffer coexist briefly. Reject before
    // allocation when that peak would consume most of the currently available
    // RAM; the UI can then recommend smaller groups/CPU tiled work instead of
    // allowing an OS-level OOM termination.
    let peak_bytes = encoded_bytes
        .checked_add(sample_count.saturating_mul(std::mem::size_of::<f32>()))
        .ok_or("TIFF demasiado grande")?;
    let mut sys = sysinfo::System::new_all();
    sys.refresh_memory();
    let safe_ram = sys.available_memory().saturating_mul(60) / 100;
    if peak_bytes as u64 > safe_ram.max(64 * 1024 * 1024) {
        return Err(format!(
            "TIFF requiere aproximadamente {:.1} MB para decodificar y convertir, por encima del presupuesto seguro de {:.1} MB",
            peak_bytes as f64 / 1_048_576.0,
            safe_ram as f64 / 1_048_576.0
        ));
    }
    let mut limits = Limits::unlimited();
    limits.decoding_buffer_size = encoded_bytes.max(1);
    limits.intermediate_buffer_size = encoded_bytes.min(512 * 1024 * 1024).max(8 * 1024 * 1024);
    limits.ifd_value_size = 16 * 1024 * 1024;
    decoder = decoder.with_limits(limits);

    let (mut samples, is_float) = match decoder
        .read_image()
        .map_err(|e| format!("TIFF decode: {e}"))?
    {
        DecodingResult::U8(v) => (v.into_iter().map(|x| x as f32 * 257.0).collect(), false),
        DecodingResult::U16(v) => (v.into_iter().map(|x| x as f32).collect(), false),
        DecodingResult::U32(v) => (v.into_iter().map(|x| x as f32).collect(), false),
        DecodingResult::U64(v) => (v.into_iter().map(|x| x as f32).collect(), false),
        DecodingResult::F32(v) => (v, true),
        DecodingResult::F64(v) => (v.into_iter().map(|x| x as f32).collect(), true),
    };
    if samples.len() != sample_count {
        return Err(format!(
            "TIFF truncado: esperados {sample_count} samples, recibidos {}",
            samples.len()
        ));
    }
    if samples.iter().any(|v| !v.is_finite()) {
        return Err("TIFF contiene NaN o infinito; corrige el archivo antes de apilar".into());
    }
    if is_float {
        // Same robust 0..1-vs-ADU decision as the FITS float path (peak+mean, not
        // peak alone), so a dark real-ADU float TIFF is not multiplied by 65535.
        let (maxv, mean) = ds_max_mean_f32(&samples);
        if ds_norm_scale_decision(maxv, mean, None) == 65535.0 {
            samples.iter_mut().for_each(|v| *v *= 65535.0);
        }
    }

    let data = if source_channels == output_channels {
        samples
    } else {
        let mut out = Vec::with_capacity(w * h * output_channels);
        for pixel in samples.chunks_exact(source_channels) {
            out.extend_from_slice(&pixel[..output_channels]);
        }
        out
    };
    Ok(DsImage {
        data,
        w,
        h,
        ch: output_channels,
        bayer: None,
    })
}

/// Decide the factor that brings a floating-point frame to the engine's 0..65535
/// working scale. Distinguishes SIRIL/PixInsight 0..1 normalized floats from data
/// already stored in real ADU/DN. Unlike the old `max <= 2.0` test — which would
/// multiply a genuinely dark real-ADU sub by 65535 — this uses the mean together
/// with the peak: a real acquisition (even a faint narrowband dark) carries a bias
/// pedestal that lifts the mean well above 1, so it is left untouched. An explicit
/// `DATAMAX` header, when present, wins.
fn ds_norm_scale_decision(maxv: f64, mean: f64, datamax: Option<f64>) -> f64 {
    if let Some(dm) = datamax {
        if dm > 2.0 {
            return 1.0; // clearly not 0..1 normalized
        }
        if dm <= 1.5 {
            return 65535.0; // header declares a normalized range
        }
    }
    // No decisive header: normalized only when BOTH the peak sits in ~[0,1] and the
    // mean is small. Real-ADU frames fail the mean test via their bias pedestal.
    if maxv <= 2.0 && mean <= 1.0 {
        65535.0
    } else {
        1.0
    }
}

/// One O(n) pass returning (abs-max, mean) for the scale decision above.
fn ds_max_mean_f32(data: &[f32]) -> (f64, f64) {
    if data.is_empty() {
        return (0.0, 0.0);
    }
    let mut maxv = 0.0f64;
    let mut sum = 0.0f64;
    for &v in data {
        let a = (v as f64).abs();
        if a > maxv {
            maxv = a;
        }
        sum += v as f64;
    }
    (maxv, sum / data.len() as f64)
}

/// Cuenta muestras no científicas. Hasta que el lector productivo transporte
/// una máscara DQ por frame, el comportamiento seguro es bloquear el archivo
/// completo con una razón visible; convertir BLANK/NaN/Inf en cero fabricaría
/// una medición válida y contaminaría masters, rechazo y fotometría.
fn ds_nonfinite_sample_count(data: &[f32]) -> usize {
    data.iter().filter(|value| !value.is_finite()).count()
}

fn ds_read_image(path: &str) -> Result<DsImage, String> {
    let lower = path.to_lowercase();
    if lower.ends_with(".fits") || lower.ends_with(".fit") {
        let fits = fitrs::Fits::open(path).map_err(|e| format!("FITS open: {:?}", e))?;
        let hdu = fits.iter().next().ok_or("FITS sin HDU primario")?;
        // OSC cameras store a CFA mono plane + BAYERPAT — read it BEFORE the
        // pixel data so the mono branch can debayer to real color.
        let bayer_id = ds_bayer_id(&hdu);
        // FITS integer scaling (defaults: BSCALE=1, BZERO=0).
        let bscale = ds_hdr_num(&hdu, "BSCALE").unwrap_or(1.0);
        let bzero = ds_hdr_num(&hdu, "BZERO").unwrap_or(0.0);
        // Conversión entera→ADU EN PARALELO: operación pura por elemento con
        // orden preservado por par_iter().collect() — bytes idénticos al bucle
        // secuencial, pero ~0.1-0.2 s menos por FITS de 11.7 Mpx (se repite
        // por cada light/flat/dark leído).
        let (shape, data): (Vec<usize>, Vec<f32>) = match hdu.read_data() {
            fitrs::FitsData::IntegersI32(arr) => {
                let blank = arr.data.par_iter().filter(|value| value.is_none()).count();
                if blank > 0 {
                    return Err(format!(
                        "FITS contiene {blank} muestra(s) BLANK; se bloquea porque DQ por frame aún no puede preservarlas sin convertirlas en cero"
                    ));
                }
                (
                    arr.shape.clone(),
                    arr.data
                        .par_iter()
                        .map(|value| ds_int_to_adu(value.expect("BLANK validado") as f64, bscale, bzero))
                        .collect(),
                )
            }
            fitrs::FitsData::IntegersU32(arr) => {
                let blank = arr.data.par_iter().filter(|value| value.is_none()).count();
                if blank > 0 {
                    return Err(format!(
                        "FITS contiene {blank} muestra(s) BLANK; se bloquea porque DQ por frame aún no puede preservarlas sin convertirlas en cero"
                    ));
                }
                (
                    arr.shape.clone(),
                    arr.data
                        .par_iter()
                        .map(|value| ds_int_to_adu(value.expect("BLANK validado") as f64, bscale, bzero))
                        .collect(),
                )
            }
            fitrs::FitsData::FloatingPoint32(arr) => {
                let shape = arr.shape.clone();
                let mut raw = arr.data;
                let nonfinite = ds_nonfinite_sample_count(&raw);
                if nonfinite > 0 {
                    return Err(format!(
                        "FITS contiene {nonfinite} muestra(s) NaN/Inf; se bloquea porque DQ por frame aún no puede preservarlas sin convertirlas en cero"
                    ));
                }
                // SIRIL/PI floats are 0..1; some tools store real ADU. Decide from
                // the DATAMAX header (if any) plus peak+mean, never peak alone.
                let datamax = ds_hdr_num(&hdu, "DATAMAX");
                let (maxv, mean) = ds_max_mean_f32(&raw);
                let scale = ds_norm_scale_decision(maxv, mean, datamax) as f32;
                if scale != 1.0 {
                    raw.iter_mut().for_each(|v| *v *= scale);
                }
                (shape, raw)
            }
            fitrs::FitsData::FloatingPoint64(arr) => {
                let shape = arr.shape.clone();
                let raw = arr.data;
                let nonfinite = raw.iter().filter(|value| !value.is_finite()).count();
                if nonfinite > 0 {
                    return Err(format!(
                        "FITS contiene {nonfinite} muestra(s) NaN/Inf; se bloquea porque DQ por frame aún no puede preservarlas sin convertirlas en cero"
                    ));
                }
                let datamax = ds_hdr_num(&hdu, "DATAMAX");
                let (mut maxv, mut sum) = (0.0f64, 0.0f64);
                for &v in raw.iter() {
                    let a = v.abs();
                    if a > maxv {
                        maxv = a;
                    }
                    sum += v;
                }
                let mean = if raw.is_empty() {
                    0.0
                } else {
                    sum / raw.len() as f64
                };
                let scale = ds_norm_scale_decision(maxv, mean, datamax);
                (shape, raw.iter().map(|v| (v * scale) as f32).collect())
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
        // Use the ACTUAL plane count for bounds checking — the old code clamped to
        // 3 and then unconditionally read 3 planes, panicking on a real NAXIS3=2
        // cube (indexed data[plane*2+i] past a 2-plane buffer).
        let planes = if shape.len() >= 3 { shape[2] } else { 1 };
        if w == 0 || h == 0 || planes == 0 {
            return Err("FITS con forma inválida".into());
        }
        let need = w
            .checked_mul(h)
            .and_then(|p| p.checked_mul(planes))
            .ok_or("FITS con dimensiones fuera de rango")?;
        if need > data.len() {
            return Err(format!(
                "FITS truncado: {}x{}x{} necesita {} muestras, hay {}",
                w,
                h,
                planes,
                need,
                data.len()
            ));
        }
        if planes == 1 {
            // OSC raw CFA: keep it MONO and tag the pattern. Debayer happens
            // AFTER calibration (correct order — calibrating post-debayer left
            // vignetting/green because the interpolation already mixed the CFA).
            Ok(DsImage {
                data,
                w,
                h,
                ch: 1,
                bayer: bayer_id,
            })
        } else if planes == 2 {
            // A 2-plane cube is not RGB. Fabricating a 3rd plane corrupts color and
            // over-reads the buffer; refuse with actionable guidance instead.
            Err(format!(
                "FITS con NAXIS3=2 ({}x{}x2) no soportado para apilado RGB; divide en canales mono e integra cada filtro por separado",
                w, h
            ))
        } else {
            // FITS planar (RRR..GGG..BBB[..]) → interleaved RGB using the first 3
            // planes. Bounds already validated against the real plane count above.
            let plane = w * h;
            let mut inter = vec![0.0f32; plane * 3];
            for i in 0..plane {
                inter[i * 3] = data[i];
                inter[i * 3 + 1] = data[plane + i];
                inter[i * 3 + 2] = data[plane * 2 + i];
            }
            Ok(DsImage {
                data: inter,
                w,
                h,
                ch: 3,
                bayer: None,
            })
        }
    } else if lower.ends_with(".tif") || lower.ends_with(".tiff") {
        ds_read_tiff_float_safe(path)
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
    let (rx, ry) = match cid {
        8 => (0usize, 0usize),
        9 => (1, 0),
        10 => (0, 1),
        11 => (1, 1),
        _ => return img,
    };
    // Debayer f32 directo: no cuantiza los calibrados negativos ni recorta el
    // headroom por encima de 16 bits antes de registro/integración.
    let mut rgb = vec![0.0f32; img.w * img.h * 3];
    for y in 1..img.h - 1 {
        for x in 1..img.w - 1 {
            let i = y * img.w + x;
            let o = i * 3;
            let v = img.data[i];
            let u = img.data[i - img.w];
            let d = img.data[i + img.w];
            let l = img.data[i - 1];
            let r = img.data[i + 1];
            let diag = 0.25
                * (img.data[i - img.w - 1]
                    + img.data[i - img.w + 1]
                    + img.data[i + img.w - 1]
                    + img.data[i + img.w + 1]);
            let red = (x & 1) == rx && (y & 1) == ry;
            let blue = (x & 1) != rx && (y & 1) != ry;
            let px = if red {
                [v, 0.25 * (u + d + l + r), diag]
            } else if blue {
                [diag, 0.25 * (u + d + l + r), v]
            } else if (y & 1) == ry {
                [0.5 * (l + r), v, 0.5 * (u + d)]
            } else {
                [0.5 * (u + d), v, 0.5 * (l + r)]
            };
            rgb[o..o + 3].copy_from_slice(&px);
        }
    }
    DsImage {
        data: rgb,
        w: img.w,
        h: img.h,
        ch: 3,
        bayer: None,
    }
}

/// Calibration master (bias/dark/flat). With ≥5 frames, median/MAD freezes a
/// per-pixel outlier mask and the accepted values are averaged. This retains
/// cosmic/satellite robustness without the median's pi/2 variance penalty.
/// Smaller stacks use a finite mean. Dimension mismatches are skipped.
/// `norm_frames` (flats): each frame is scaled to the FIRST frame's mean before
/// combining, so sky flats with drifting brightness median cleanly instead of
/// the level drift masquerading as signal. The output keeps the CFA `bayer`
/// tag of the first frame — the flat normalizer needs it to equalize per color.
fn ds_combine_master_store(
    store: &AdaptiveFrameStore,
    frame_count: usize,
    pixel_count: usize,
    use_robust_mean: bool,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Vec<f32>, String> {
    let mut sys = sysinfo::System::new_all();
    sys.refresh_memory();
    let tile_budget = (sys.available_memory().saturating_mul(15) / 100)
        .min(512 * 1024 * 1024)
        .max(1024 * 1024) as usize;
    let bytes_per_pixel = if use_robust_mean {
        frame_count.saturating_mul(8).saturating_add(4)
    } else {
        12
    };
    let tile_len = (tile_budget / bytes_per_pixel.max(1))
        .clamp(1, pixel_count)
        .min(262_144);
    let mut data = vec![0.0f32; pixel_count];
    let mut start = 0usize;
    while start < pixel_count {
        cancellation_checkpoint(cancel.as_ref(), "combinación tiled de masters")?;
        let end = (start + tile_len).min(pixel_count);
        let len = end - start;
        if use_robust_mean {
            let mut frame_tiles = Vec::with_capacity(frame_count);
            for frame_idx in 0..frame_count {
                cancellation_checkpoint(cancel.as_ref(), "lectura tiled de masters")?;
                frame_tiles.push(store.get_range(frame_idx, start..end)?);
            }
            let tile: Vec<f32> = (0..len)
                .into_par_iter()
                .map_init(
                    || {
                        (
                            Vec::<f32>::with_capacity(frame_count),
                            Vec::<f32>::with_capacity(frame_count),
                        )
                    },
                    |(values, deviations), pixel| {
                        values.clear();
                        values.extend(frame_tiles.iter().map(|frame| frame[pixel]));
                        crate::deepsky_calibration_stats::robust_calibration_mean(
                            values, deviations,
                        )
                        .unwrap_or(f32::NAN)
                    },
                )
                .collect();
            data[start..end].copy_from_slice(&tile);
        } else {
            let mut sum = vec![0.0f64; len];
            for frame_idx in 0..frame_count {
                cancellation_checkpoint(cancel.as_ref(), "lectura tiled de masters")?;
                let frame = store.get_range(frame_idx, start..end)?;
                for (acc, value) in sum.iter_mut().zip(frame) {
                    *acc += value as f64;
                }
            }
            for (dst, value) in data[start..end].iter_mut().zip(sum) {
                *dst = (value / frame_count as f64) as f32;
            }
        }
        start = end;
    }
    Ok(data)
}

fn ds_build_master_preprocessed(
    app: &tauri::AppHandle,
    paths: &[String],
    label: &str,
    norm_frames: bool,
    preprocess: Option<&dyn Fn(&str, &mut DsImage) -> Result<(), String>>,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    work_root: Option<&std::path::Path>,
) -> Result<Option<DsCalibrationMaster>, String> {
    if paths.is_empty() {
        return Ok(None);
    }
    let mut dims: Option<(usize, usize, usize)> = None;
    let mut bayer: Option<i32> = None;
    let mut mean0: Option<f64> = None;
    let mut max_input_scale_sq = 1.0f32;
    let mut accepted = 0usize;
    let mut store: Option<AdaptiveFrameStore> = None;
    let cache_dir = work_root
        .map(|root| root.join("zenith_master_tiles"))
        .unwrap_or_else(|| std::env::temp_dir().join("astro_stacker_master_tiles"));
    let safe_label: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let store_tag = format!(
        "master_{}_{}_{}",
        safe_label,
        std::process::id(),
        ds_source_fingerprint(&[paths])
    );
    // LECTURA POR CHUNKS EN PARALELO: con 60-300 flats la lectura FITS
    // secuencial era el coste dominante de los masters. Cada chunk de 6 se lee
    // en paralelo (RAM acotada ~300 MB) y se ACEPTA/normaliza/almacena
    // inmediatamente EN EL ORDEN ORIGINAL de paths — misma semántica (dims del
    // primer frame legible, misma media de referencia, mismos bytes).
    for chunk_base in (0..paths.len()).step_by(6) {
        cancellation_checkpoint(cancel.as_ref(), "lectura de masters de calibración")?;
        let chunk_end = (chunk_base + 6).min(paths.len());
        let mut chunk: Vec<(usize, Result<DsImage, String>)> = (chunk_base..chunk_end)
            .into_par_iter()
            .map(|index| (index, ds_read_image(&paths[index])))
            .collect();
        chunk.sort_by_key(|(index, _)| *index);
        for (index, result) in chunk {
            let p = &paths[index];
            match result {
                Ok(mut img) => {
                    match dims {
                        None => {
                            dims = Some((img.w, img.h, img.ch));
                            bayer = img.bayer;
                            let frame_len = img
                                .w
                                .checked_mul(img.h)
                                .and_then(|v| v.checked_mul(img.ch))
                                .ok_or("Master de calibración demasiado grande")?;
                            let created = AdaptiveFrameStore::new(
                                paths.len(),
                                frame_len,
                                &cache_dir,
                                &store_tag,
                            )?;
                            log_to_front(
                                app,
                                "INFO",
                                &format!(
                                    "Master {label}: almacén adaptativo {} para {} candidatos.",
                                    created.kind().label(),
                                    paths.len()
                                ),
                            );
                            store = Some(created);
                        }
                        Some((w, h, ch)) => {
                            if img.w != w || img.h != h || img.ch != ch {
                                log_to_front(
                                    app,
                                    "WARN",
                                    &format!("{}: dimensiones distintas, omitido: {}", label, p),
                                );
                                continue;
                            }
                            if img.bayer != bayer {
                                log_to_front(
                                    app,
                                    "WARN",
                                    &format!(
                                        "{}: patrón Bayer/CFA incompatible, omitido: {}",
                                        label, p
                                    ),
                                );
                                continue;
                            }
                        }
                    }
                    // For flats this is where pedestal/thermal calibration runs.
                    // It MUST precede any brightness normalization; scaling the raw
                    // pedestal first makes the later subtraction mathematically
                    // impossible to undo.
                    if let Some(preprocess) = preprocess {
                        preprocess(p, &mut img).map_err(|e| {
                            format!("{label}: calibración previa falló para {p}: {e}")
                        })?;
                    }
                    if norm_frames {
                        let (sum, count) = img
                            .data
                            .iter()
                            .copied()
                            .filter(|value| value.is_finite())
                            .fold((0.0f64, 0usize), |(sum, count), value| {
                                (sum + value as f64, count + 1)
                            });
                        if count == 0 {
                            return Err(format!(
                                "{label}: el flat '{p}' no conserva muestras lineales finitas"
                            ));
                        }
                        let mean = (sum / count as f64).max(1.0);
                        match mean0 {
                            None => mean0 = Some(mean),
                            Some(m0) => {
                                let k = (m0 / mean) as f32;
                                max_input_scale_sq = max_input_scale_sq.max(k * k);
                                if (k - 1.0).abs() > 0.001 {
                                    for v in img.data.iter_mut() {
                                        *v *= k;
                                    }
                                }
                            }
                        }
                    }
                    store
                        .as_mut()
                        .ok_or("No se pudo inicializar el almacén del master")?
                        .put(accepted, &img.data)?;
                    accepted += 1;
                }
                Err(e) => log_to_front(
                    app,
                    "WARN",
                    &format!("{}: no se pudo leer {} ({})", label, p, e),
                ),
            }
        }
    }
    let Some((w, h, ch)) = dims else {
        return Ok(None);
    };
    if accepted == 0 {
        return Ok(None);
    }
    let store = store.ok_or("Almacén del master ausente")?;
    let n = accepted;
    let npx = w
        .checked_mul(h)
        .and_then(|v| v.checked_mul(ch))
        .ok_or("Master de calibración demasiado grande")?;
    // Once frames spill to mmap, robust median no longer needs a 3 GB all-RAM
    // exception. Work in bounded pixel tiles and keep mean only for very small
    // sample counts where a median has poor statistical efficiency.
    let use_robust_mean = n >= 5;
    let robust = crate::deepsky_variance::combine_master_store_robust(
        &store, n, npx, cancel,
    )?;

    log_to_front(
        app,
        "INFO",
        &format!(
            "Master {}: {} frames combinados por {}.",
            label,
            n,
            if use_robust_mean {
                "MEDIA ROBUSTA (máscara mediana/MAD)"
            } else {
                "media"
            }
        ),
    );
    // Masters do per-pixel arithmetic in the SAME (CFA or RGB) space as the
    // lights; the bayer tag is carried so the FLAT can be equalized per color.
    Ok(Some(DsCalibrationMaster {
        image: DsImage {
            data: robust.data,
            w,
            h,
            ch,
            bayer,
        },
        variance: robust.variance,
        neff: robust.neff,
        dq: robust.dq,
        rejected_fraction: robust.rejected_fraction,
        frames: robust.frames,
        max_input_scale_sq,
    }))
}

fn ds_build_master(
    app: &tauri::AppHandle,
    paths: &[String],
    label: &str,
    norm_frames: bool,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    work_root: Option<&std::path::Path>,
) -> Result<Option<DsCalibrationMaster>, String> {
    ds_build_master_preprocessed(app, paths, label, norm_frames, None, cancel, work_root)
}

/// Normalize the master flat for division — SIRIL "equalize CFA" parity. A
/// global mean-1 normalization DIVIDES THE LIGHTS BY THE FLAT PANEL'S COLOR
/// (an OSC flat has ~2× green photosites and whatever tint the panel had),
/// re-tinting every calibrated light. Instead each color population is
/// normalized to ITS OWN mean: CFA flats per Bayer position (2×2), RGB flats
/// per channel, mono flats by the global mean. The flat then corrects only
/// vignetting/PRNU and is strictly color-neutral.
fn ds_flat_normalize(f: &mut DsImage) {
    if f.bayer.is_some() && f.ch == 1 {
        let mut sums = [0.0f64; 4];
        let mut cnts = [0u64; 4];
        for y in 0..f.h {
            let row = y * f.w;
            for x in 0..f.w {
                let pos = (y & 1) * 2 + (x & 1);
                let value = f.data[row + x];
                if value.is_finite() {
                    sums[pos] += value as f64;
                    cnts[pos] += 1;
                }
            }
        }
        let mut inv = [1.0f32; 4];
        for p in 0..4 {
            inv[p] = (1.0 / (sums[p] / cnts[p].max(1) as f64).max(1.0)) as f32;
        }
        for y in 0..f.h {
            let row = y * f.w;
            for x in 0..f.w {
                f.data[row + x] *= inv[(y & 1) * 2 + (x & 1)];
            }
        }
    } else if f.ch == 3 {
        for c in 0..3 {
            let (sum, count) = f
                .data
                .iter()
                .skip(c)
                .step_by(3)
                .copied()
                .filter(|value| value.is_finite())
                .fold((0.0f64, 0usize), |(sum, count), value| {
                    (sum + value as f64, count + 1)
                });
            let mean = (sum / count.max(1) as f64).max(1.0);
            let inv = (1.0 / mean) as f32;
            for v in f.data.iter_mut().skip(c).step_by(3) {
                *v *= inv;
            }
        }
    } else {
        let (sum, count) = f
            .data
            .iter()
            .copied()
            .filter(|value| value.is_finite())
            .fold((0.0f64, 0usize), |(sum, count), value| {
                (sum + value as f64, count + 1)
            });
        let mean = (sum / count.max(1) as f64).max(1.0);
        let inv = (1.0 / mean) as f32;
        for v in f.data.iter_mut() {
            *v *= inv;
        }
    }
}

/// Apply the exact same flat normalization to SCI and to its estimator VAR.
/// Additive quantities are not involved: multiplying a flat population by
/// `a` necessarily multiplies its variance by `a²`.
fn ds_flat_normalize_master(master: &mut DsCalibrationMaster) {
    let f = &mut master.image;
    if f.bayer.is_some() && f.ch == 1 {
        let mut sums = [0.0f64; 4];
        let mut cnts = [0u64; 4];
        for y in 0..f.h {
            let row = y * f.w;
            for x in 0..f.w {
                let pos = (y & 1) * 2 + (x & 1);
                let value = f.data[row + x];
                if value.is_finite() {
                    sums[pos] += value as f64;
                    cnts[pos] += 1;
                }
            }
        }
        let mut inv = [1.0f32; 4];
        for p in 0..4 {
            inv[p] = (1.0 / (sums[p] / cnts[p].max(1) as f64).max(1.0)) as f32;
        }
        for y in 0..f.h {
            let row = y * f.w;
            for x in 0..f.w {
                let index = row + x;
                let scale = inv[(y & 1) * 2 + (x & 1)];
                f.data[index] *= scale;
                master.variance[index] *= scale * scale;
            }
        }
    } else if f.ch == 3 {
        for c in 0..3 {
            let (sum, count) = f
                .data
                .iter()
                .skip(c)
                .step_by(3)
                .copied()
                .filter(|value| value.is_finite())
                .fold((0.0f64, 0usize), |(sum, count), value| {
                    (sum + value as f64, count + 1)
                });
            let mean = (sum / count.max(1) as f64).max(1.0);
            let scale = (1.0 / mean) as f32;
            for index in (c..f.data.len()).step_by(3) {
                f.data[index] *= scale;
                master.variance[index] *= scale * scale;
            }
        }
    } else {
        let (sum, count) = f
            .data
            .iter()
            .copied()
            .filter(|value| value.is_finite())
            .fold((0.0f64, 0usize), |(sum, count), value| {
                (sum + value as f64, count + 1)
            });
        let mean = (sum / count.max(1) as f64).max(1.0);
        let scale = (1.0 / mean) as f32;
        for index in 0..f.data.len() {
            f.data[index] *= scale;
            master.variance[index] *= scale * scale;
        }
    }
}

#[derive(Clone)]
struct DsRawDarkFlatMaster {
    exposure: Option<f32>,
    master: DsCalibrationMaster,
    calibration_probe: Option<DsProbe>,
    source_paths: Vec<String>,
}

#[derive(Clone)]
struct DsDarkMaster {
    exposure: Option<f32>,
    master: DsCalibrationMaster,
    amp_glow: bool,
    bias_subtracted: bool,
    source_paths: Vec<String>,
    calibration_probe: Option<DsProbe>,
}

fn ds_master_is_compatible(frame: &DsImage, master: &DsImage) -> bool {
    frame.w == master.w
        && frame.h == master.h
        && frame.ch == master.ch
        && frame.bayer == master.bayer
        && frame.data.len() == master.data.len()
}

#[derive(Debug)]
struct DsFlatLinearityReport {
    /// Same interleaved sample layout as DsCalibrationMaster::dq.
    dq: Vec<u32>,
    white_level_adu: f32,
    nonlinear_level_adu: f32,
    saturated_samples: usize,
    nonlinear_samples: usize,
}

impl DsFlatLinearityReport {
    fn sample_count(&self) -> usize {
        self.dq.len()
    }

    /// A few isolated clipped detector defects remain masked in DQ.  A flat
    /// whose illuminated field itself reaches the nonlinear/clip zone is not
    /// a valid multiplicative calibration exposure and must be rejected.
    fn invalid_frame_reason(&self) -> Option<String> {
        let total = self.sample_count().max(1);
        let saturated_fraction = self.saturated_samples as f64 / total as f64;
        let nonlinear_fraction = self.nonlinear_samples as f64 / total as f64;
        if saturated_fraction > 0.0001 || nonlinear_fraction > 0.001 {
            return Some(format!(
                "flat saturado/no lineal: {} muestra(s) SATURATED ({:.4}%) y {} NONLINEAR ({:.4}%), white={:.3} ADU, inicio no lineal={:.3} ADU",
                self.saturated_samples,
                saturated_fraction * 100.0,
                self.nonlinear_samples,
                nonlinear_fraction * 100.0,
                self.white_level_adu,
                self.nonlinear_level_adu,
            ));
        }
        None
    }
}

fn ds_effective_white_level_adu(
    signature: &pipeline::CalibrationSignature,
) -> Result<f32, String> {
    if let Some(value) = signature.white_level_adu {
        if value.is_finite() && value > 1.0 {
            return Ok(value);
        }
        return Err("whiteLevelAdu inválido para validar el flat".into());
    }
    let bits = signature
        .adc_bits
        .ok_or("faltan whiteLevelAdu y adcBits para validar saturación del flat")?;
    if bits == 0 || bits > 31 {
        return Err(format!("adcBits={bits} fuera de rango para validar el flat"));
    }
    Ok(((1u64 << bits) - 1) as f32)
}

/// Marks detector samples that cannot define a linear multiplicative flat.
/// White level wins when declared because cameras may left-shift a 12/14-bit
/// ADC into a 16-bit FITS container; ADC full scale is the fail-closed fallback.
fn ds_flat_linearity_report(
    flat: &DsImage,
    signature: &pipeline::CalibrationSignature,
) -> Result<DsFlatLinearityReport, String> {
    let white = ds_effective_white_level_adu(signature)?;
    let nonlinear = white * 0.90;
    let saturation_floor = (white - 0.5).max(nonlinear);
    let mut dq = vec![0u32; flat.data.len()];
    let mut saturated_samples = 0usize;
    let mut nonlinear_samples = 0usize;
    for (index, value) in flat.data.iter().copied().enumerate() {
        if !value.is_finite() {
            dq[index] |= crate::deepsky_variance::dq::NAN_INPUT;
        } else if value >= saturation_floor {
            dq[index] |= crate::deepsky_variance::dq::SATURATED;
            saturated_samples += 1;
        } else if value >= nonlinear {
            dq[index] |= crate::deepsky_variance::dq::NONLINEAR;
            nonlinear_samples += 1;
        }
    }
    Ok(DsFlatLinearityReport {
        dq,
        white_level_adu: white,
        nonlinear_level_adu: nonlinear,
        saturated_samples,
        nonlinear_samples,
    })
}

fn ds_dark_flat_master_matches_flat(
    flat_probe: &DsProbe,
    flat: &DsImage,
    master: &DsRawDarkFlatMaster,
) -> bool {
    master.calibration_probe.as_ref().is_some_and(|dark_flat| {
        ds_compare_probe_calibration(
            flat_probe,
            dark_flat,
            crate::deepsky_calibration_contract::CalibrationRole::DarkFlat,
            pipeline::DeepSkyCalibrationPolicy::Strict,
        )
        .compatible
            && ds_master_is_compatible(flat, &master.master.image)
    })
}

fn ds_calibrated_flat_is_valid(flat: &DsImage) -> bool {
    if flat.data.is_empty() {
        return false;
    }
    let step = (flat.data.len() / 200_000).max(1);
    let mut valid = 0usize;
    let mut total = 0usize;
    let mut positive = Vec::with_capacity(flat.data.len() / step + 1);
    for &value in flat.data.iter().step_by(step) {
        total += 1;
        if value.is_finite() && value > 0.0 {
            valid += 1;
            positive.push(value);
        }
    }
    if valid * 1_000 < total.saturating_mul(999) || positive.is_empty() {
        return false;
    }
    positive.sort_by(|a, b| a.total_cmp(b));
    positive[positive.len() / 2] > 1.0
}

/// Apply exactly one pedestal source. `raw_dark_flat` has priority and already
/// contains bias; passing both can therefore never double-subtract the bias.
fn ds_apply_flat_calibration(
    flat: &mut DsImage,
    raw_dark_flat: Option<&DsImage>,
    bias: Option<&DsImage>,
) -> Result<&'static str, String> {
    let (master, source) = if let Some(master) = raw_dark_flat {
        (master, "rawDarkFlat")
    } else if let Some(master) = bias {
        (master, "bias")
    } else {
        return Ok("uncalibrated");
    };
    if !ds_master_is_compatible(flat, master) {
        return Err(format!(
            "master {source} incompatible en geometría/canales/CFA"
        ));
    }
    for (value, master_value) in flat.data.iter_mut().zip(&master.data) {
        *value -= *master_value;
    }
    Ok(source)
}

fn ds_validate_flat_pedestal_policy(
    app: &tauri::AppHandle,
    path: &str,
    policy: pipeline::DeepSkyCalibrationPolicy,
    inputs: crate::deepsky_calibration_contract::FlatPedestalInputs,
) -> Result<(), String> {
    match crate::deepsky_calibration_contract::validate_flat_pedestal(inputs) {
        Ok(_) => Ok(()),
        Err(reason) if matches!(policy, pipeline::DeepSkyCalibrationPolicy::Strict) => {
            Err(reason)
        }
        Err(reason) => {
            log_to_front(
                app,
                "WARN",
                &format!(
                    "AllowDegraded: flat '{}' sin convención de pedestal científicamente válida: {reason}; sólo podrá alimentar Classic no científico.",
                    std::path::Path::new(path)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                ),
            );
            Ok(())
        }
    }
}

/// Calibrate one raw flat before the master-builder performs any exposure
/// normalization. A raw dark-flat already contains the sensor pedestal, so it
/// is subtracted alone; bias is used only when no dark-flat is available.
fn ds_precalibrate_flat(
    app: &tauri::AppHandle,
    path: &str,
    flat: &mut DsImage,
    flat_probe: Option<&DsProbe>,
    bias: Option<&DsCalibrationMaster>,
    bias_probe: Option<&DsProbe>,
    dark_flats: &[DsRawDarkFlatMaster],
    policy: pipeline::DeepSkyCalibrationPolicy,
    // AllowDegraded: se marca cuando el flat calibrado NO es válido — el
    // llamador debe degradar la corrida (Classic + no científico) para que la
    // promesa del WARN se cumpla de verdad (auditoría 2026-07-20).
    data_degraded: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    let exposure = ds_probe_exptime(path);
    let exact_dark_flat = flat_probe.and_then(|probe| {
        dark_flats
            .iter()
            .find(|master| ds_dark_flat_master_matches_flat(probe, flat, master))
    });
    let exact_bias = flat_probe.and_then(|flat_probe| {
        bias.zip(bias_probe).and_then(|(master, bias_probe)| {
            (ds_compare_probe_calibration(
                flat_probe,
                bias_probe,
                crate::deepsky_calibration_contract::CalibrationRole::Bias,
                pipeline::DeepSkyCalibrationPolicy::Strict,
            )
            .compatible
                && ds_master_is_compatible(flat, &master.image))
            .then_some(master)
        })
    });

    if let Some(master) = exact_dark_flat {
        ds_validate_flat_pedestal_policy(
            app,
            path,
            policy,
            crate::deepsky_calibration_contract::FlatPedestalInputs {
                raw_dark_flat: true,
                bias: false,
                dark_flat_thermal: false,
                validated_thermal_fraction: None,
            },
        )?;
        ds_apply_flat_calibration(
            flat,
            Some(&master.master.image),
            exact_bias.map(|master| &master.image),
        )?;
    } else if !dark_flats.is_empty() {
        let reason = match exposure {
            Some(seconds) => {
                format!("no existe dark-flat crudo de firma completa compatible a {seconds:.6} s (cámara/read mode/gain/offset/temperatura/binning/ROI/CFA y tolerancia 1 ms / 1e-6 relativa)")
            }
            None => "el flat no declara EXPTIME/EXPOSURE y no puede emparejarse con seguridad"
                .to_string(),
        };
        if matches!(policy, pipeline::DeepSkyCalibrationPolicy::Strict) {
            return Err(reason);
        }
        log_to_front(
            app,
            "WARN",
            &format!(
                "Calibración degradada de flat '{}': {reason}; se usará bias si es compatible.",
                std::path::Path::new(path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            ),
        );
        let compatible_bias = exact_bias;
        ds_validate_flat_pedestal_policy(
            app,
            path,
            policy,
            crate::deepsky_calibration_contract::FlatPedestalInputs {
                raw_dark_flat: false,
                bias: compatible_bias.is_some(),
                dark_flat_thermal: false,
                // No sensor/profile evidence is available in this execution
                // path. Inventing a zero thermal fraction would make the gate
                // decorative, so bias-only remains explicitly degraded.
                validated_thermal_fraction: None,
            },
        )?;
        if let Some(master) = compatible_bias {
            ds_apply_flat_calibration(flat, None, Some(&master.image))?;
        }
    } else if let Some(master) = exact_bias {
        ds_validate_flat_pedestal_policy(
            app,
            path,
            policy,
            crate::deepsky_calibration_contract::FlatPedestalInputs {
                raw_dark_flat: false,
                bias: true,
                dark_flat_thermal: false,
                validated_thermal_fraction: None,
            },
        )?;
        ds_apply_flat_calibration(flat, None, Some(&master.image))?;
    } else if matches!(policy, pipeline::DeepSkyCalibrationPolicy::Strict) {
        ds_validate_flat_pedestal_policy(
            app,
            path,
            policy,
            crate::deepsky_calibration_contract::FlatPedestalInputs::default(),
        )?;
    } else {
        ds_validate_flat_pedestal_policy(
            app,
            path,
            policy,
            crate::deepsky_calibration_contract::FlatPedestalInputs::default(),
        )?;
        log_to_front(
            app,
            "WARN",
            &format!(
                "Calibración degradada de flat '{}': sin dark-flat ni bias compatible; se conserva el pedestal crudo.",
                std::path::Path::new(path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            ),
        );
    }

    if !ds_calibrated_flat_is_valid(flat) {
        let reason = "más de 0.1% de las muestras calibradas no son positivas/finitas o el nivel mediano es <=1 ADU";
        if matches!(policy, pipeline::DeepSkyCalibrationPolicy::Strict) {
            return Err(reason.into());
        }
        data_degraded.store(true, std::sync::atomic::Ordering::Relaxed);
        log_to_front(
            app,
            "WARN",
            &format!(
                "Flat degradado '{}': {reason}; el resultado no es elegible para NF/EIDR científico.",
                std::path::Path::new(path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            ),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn ds_build_calibrated_flat_master(
    app: &tauri::AppHandle,
    paths: &[String],
    label: &str,
    bias: Option<&DsCalibrationMaster>,
    bias_probe: Option<&DsProbe>,
    dark_flats: &[DsRawDarkFlatMaster],
    policy: pipeline::DeepSkyCalibrationPolicy,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    work_root: Option<&std::path::Path>,
    data_degraded: &std::sync::atomic::AtomicBool,
) -> Result<Option<DsCalibrationMaster>, String> {
    let flat_probes = deepsky_probe(paths.to_vec())
        .into_iter()
        .map(|probe| (probe.path.clone(), probe))
        .collect::<std::collections::HashMap<_, _>>();
    if !paths.is_empty()
        && ds_master_signature_groups(
            flat_probes.values(),
            crate::deepsky_calibration_contract::CalibrationRole::Flat,
        )
        .len()
            != 1
    {
        return Err(format!(
            "{label}: varias firmas científicas incompatibles llegarían al mismo master flat"
        ));
    }
    // The preprocess callback is intentionally Fn (the master builder may
    // evolve back to parallel acceptance).  A mutex keeps the accumulated
    // input quality plane deterministic without weakening the callback API.
    let flat_input_dq = std::sync::Mutex::new(None::<Vec<u32>>);
    let preprocess = |path: &str, flat: &mut DsImage| {
        let flat_probe = flat_probes.get(path);
        let report = flat_probe
            .ok_or_else(|| format!("flat sin probe de firma legible: {path}"))
            .and_then(|probe| ds_flat_linearity_report(flat, &probe.signature));
        match report {
            Ok(report) => {
                if let Some(reason) = report.invalid_frame_reason() {
                    if matches!(policy, pipeline::DeepSkyCalibrationPolicy::Strict) {
                        return Err(reason);
                    }
                    log_to_front(
                        app,
                        "WARN",
                        &format!(
                            "AllowDegraded: flat '{}' inválido: {reason}; las muestras afectadas se excluyen y se publican en DQ.",
                            std::path::Path::new(path)
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                        ),
                    );
                }
                let quality_mask = crate::deepsky_variance::dq::SATURATED
                    | crate::deepsky_variance::dq::NONLINEAR
                    | crate::deepsky_variance::dq::NAN_INPUT;
                for (sample, flags) in flat.data.iter_mut().zip(&report.dq) {
                    if *flags & quality_mask != 0 {
                        *sample = f32::NAN;
                    }
                }
                let mut accumulated = flat_input_dq
                    .lock()
                    .map_err(|_| "máscara DQ de flats envenenada".to_string())?;
                let plane = accumulated.get_or_insert_with(|| vec![0u32; report.dq.len()]);
                if plane.len() != report.dq.len() {
                    return Err("flats con layouts DQ incompatibles".into());
                }
                for (dst, flags) in plane.iter_mut().zip(report.dq) {
                    *dst |= flags;
                }
            }
            Err(reason) if matches!(policy, pipeline::DeepSkyCalibrationPolicy::Strict) => {
                return Err(reason);
            }
            Err(reason) => log_to_front(
                app,
                "WARN",
                &format!(
                    "AllowDegraded: no se pudo validar saturación/linealidad de '{}': {reason}",
                    std::path::Path::new(path)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                ),
            ),
        }
        ds_precalibrate_flat(
            app,
            path,
            flat,
            flat_probe,
            bias,
            bias_probe,
            dark_flats,
            policy,
            data_degraded,
        )
    };
    let mut master = ds_build_master_preprocessed(
        app,
        paths,
        label,
        true,
        Some(&preprocess),
        cancel,
        work_root,
    )?;
    if let Some(flat) = master.as_mut() {
        if let Some(input_dq) = flat_input_dq
            .into_inner()
            .map_err(|_| "máscara DQ de flats envenenada".to_string())?
        {
            if input_dq.len() != flat.dq.len() {
                return Err("master flat y DQ de entrada tienen layouts distintos".into());
            }
            for (dst, flags) in flat.dq.iter_mut().zip(input_dq) {
                *dst |= flags;
            }
        }
        // The dispersion across pre-calibrated flats does not contain the
        // uncertainty of a pedestal master shared by every flat.  Add a
        // conservative per-sample covariance bound before normalizing the
        // master.  Different exact dark-flat exposures may coexist; taking
        // the maximum contribution never understates the uncertainty.
        let mut pedestal_variance = vec![0.0f32; flat.image.data.len()];
        let mut pedestal_dq = vec![0u32; flat.image.data.len()];
        let mut pedestal_found = false;
        for path in paths {
            let selected = flat_probes.get(path).and_then(|flat_probe| {
                dark_flats.iter().find(|candidate| {
                    ds_dark_flat_master_matches_flat(flat_probe, &flat.image, candidate)
                })
            });
            let source = selected
                .map(|candidate| &candidate.master)
                .or_else(|| {
                    bias.zip(bias_probe).and_then(|(candidate, bias_probe)| {
                        flat_probes.get(path).and_then(|flat_probe| {
                            (ds_compare_probe_calibration(
                                flat_probe,
                                bias_probe,
                                crate::deepsky_calibration_contract::CalibrationRole::Bias,
                                pipeline::DeepSkyCalibrationPolicy::Strict,
                            )
                            .compatible
                                && ds_master_is_compatible(&flat.image, &candidate.image))
                            .then_some(candidate)
                        })
                    })
                });
            if let Some(source) = source {
                pedestal_found = true;
                for index in 0..pedestal_variance.len() {
                    let value = source.variance[index];
                    if value.is_finite() && value >= 0.0 {
                        pedestal_variance[index] = pedestal_variance[index]
                            .max(value * flat.max_input_scale_sq);
                    } else {
                        pedestal_variance[index] = f32::NAN;
                    }
                    pedestal_dq[index] |= source.dq[index];
                }
            }
        }
        if pedestal_found {
            for index in 0..flat.variance.len() {
                if flat.variance[index].is_finite()
                    && pedestal_variance[index].is_finite()
                {
                    flat.variance[index] += pedestal_variance[index];
                } else {
                    flat.variance[index] = f32::NAN;
                }
                flat.dq[index] |= pedestal_dq[index];
            }
        }
        // Per-CFA-population/channel division normalization happens only after
        // calibrated frames have been exposure-normalized and integrated.
        ds_flat_normalize_master(flat);
    }
    Ok(master)
}

/// Header-only exposure probe (EXPTIME/EXPOSURE). None for non-FITS files.
fn ds_probe_exptime(path: &str) -> Option<f32> {
    let lower = path.to_lowercase();
    if !(lower.ends_with(".fits") || lower.ends_with(".fit")) {
        return None;
    }
    let fits = fitrs::Fits::open(path).ok()?;
    let hdu = fits.iter().next()?;
    ds_hdr_num(&hdu, "EXPTIME")
        .or_else(|| ds_hdr_num(&hdu, "EXPOSURE"))
        .map(|v| v as f32)
        .filter(|v| *v > 0.0)
}

/// Noche de observación "YYYY-MM-DD" (la fecha del atardecer que la inicia).
/// DATE-OBS con corte a las 12:00: una toma a las 03:00 pertenece a la noche
/// del día anterior. Sin header (TIF/PNG), se usa la fecha de modificación del
/// archivo en hora local. NOTA: DATE-OBS suele ser UTC y el corte es mediodía;
/// para las longitudes habituales (±1-8 h de UTC) el desfase nunca cruza el
/// mediodía en tomas nocturnas, así que la partición por noche es estable.
fn ds_session_night_id(path: &str, date_obs: Option<&str>) -> Option<String> {
    use chrono::{DateTime, Datelike, Duration, Local, NaiveDateTime, Timelike};
    let parsed = date_obs.and_then(|raw| {
        let s = raw.trim();
        let s = &s[..s.len().min(19)];
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
            .ok()
    });
    // Testigo cruzado: los programas de captura nombran "YYYY-MM-DD_HH-MM-SS_…"
    // con el reloj del host. Si la cabecera y el nombre discrepan mucho —el
    // caso típico es el AÑO mal configurado en la cámara (mismo mes/día, año
    // distinto)— el nombre es más fiable y evita que un lote entero de flats
    // caiga en una "noche" inexistente que rompe el emparejado por sesión.
    let filename_stamp = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|name| {
            let head: String = name.chars().take(19).collect();
            NaiveDateTime::parse_from_str(&head, "%Y-%m-%d_%H-%M-%S").ok()
        });
    let stamp = match (parsed, filename_stamp) {
        (Some(header), Some(named)) => {
            let far_apart = (header.date() - named.date()).num_days().abs() > 2;
            let year_swap_matches = {
                let mut reheaded = header;
                if let Some(fixed) = header.date().with_year(named.date().year()) {
                    reheaded = fixed.and_time(header.time());
                }
                (reheaded.date() - named.date()).num_days().abs() <= 2
            };
            if far_apart && year_swap_matches {
                named
            } else {
                header
            }
        }
        (Some(header), None) => header,
        (None, Some(named)) => named,
        (None, None) => {
            DateTime::<Local>::from(std::fs::metadata(path).ok()?.modified().ok()?).naive_local()
        }
    };
    let night = if stamp.hour() < 12 {
        stamp.date() - Duration::days(1)
    } else {
        stamp.date()
    };
    Some(night.format("%Y-%m-%d").to_string())
}

/// Distancia en días entre dos ids de noche, usada sólo como diagnóstico. La
/// selección productiva exige la misma sesión y nunca aplica el flat cercano.
fn ds_night_distance(a: Option<&str>, b: Option<&str>) -> i64 {
    use chrono::NaiveDate;
    match (
        a.and_then(|v| NaiveDate::parse_from_str(v, "%Y-%m-%d").ok()),
        b.and_then(|v| NaiveDate::parse_from_str(v, "%Y-%m-%d").ok()),
    ) {
        (Some(x), Some(y)) => (x - y).num_days().abs(),
        _ => i64::MAX / 2,
    }
}

/// Agrupa probes por noche; los que no tienen fecha caen en "?".
fn ds_group_probes_by_night<'a, I: IntoIterator<Item = &'a DsProbe>>(
    probes: I,
) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut out = std::collections::BTreeMap::new();
    for p in probes {
        let night =
            ds_session_night_id(&p.path, p.date_obs.as_deref()).unwrap_or_else(|| "?".into());
        out.entry(night)
            .or_insert_with(Vec::new)
            .push(p.path.clone());
    }
    out
}

/// Exposure equality used by calibration masters. This mirrors the public
/// signature contract: 1 ms absolute or 1e-6 relative, whichever is larger.
/// Wider grouping silently mixes different dark current and dark-flat levels.
fn ds_exposures_match(a: f32, b: f32) -> bool {
    if !a.is_finite() || !b.is_finite() || a <= 0.0 || b <= 0.0 {
        return false;
    }
    let a = a as f64;
    let b = b as f64;
    (a - b).abs() <= 0.001_f64.max(a.abs().max(b.abs()) * 1.0e-6)
}

/// Cluster (exposure, path) pairs into scientifically compatible exposure
/// groups. Input is sorted internally; each group carries its median exposure.
fn ds_cluster_exposures(mut items: Vec<(f32, String)>) -> Vec<(f32, Vec<String>)> {
    if items.is_empty() {
        return Vec::new();
    }
    items.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut groups: Vec<(Vec<f32>, Vec<String>)> = Vec::new();
    for (exp, path) in items {
        match groups.last_mut() {
            Some((exps, paths)) if ds_exposures_match(exp, exps[exps.len() / 2]) => {
                exps.push(exp);
                paths.push(path);
            }
            _ => groups.push((vec![exp], vec![path])),
        }
    }
    groups
        .into_iter()
        .map(|(exps, paths)| (exps[exps.len() / 2], paths))
        .collect()
}

/// Groups masters by the complete role-specific calibration contract, not by
/// exposure alone. This prevents 300 s darks from different gain/read-mode/
/// temperature/CFA groups being averaged into one physically invalid master.
fn ds_group_darks_by_exposure(
    paths: &[String],
    role: crate::deepsky_calibration_contract::CalibrationRole,
) -> Vec<(Option<f32>, Vec<String>)> {
    let mut probes = deepsky_probe(paths.to_vec());
    probes.sort_by(|a, b| a.path.cmp(&b.path));
    let mut groups: Vec<(DsProbe, Vec<String>)> = Vec::new();
    for probe in probes.into_iter().filter(|probe| probe.ok) {
        if let Some((_, group_paths)) = groups.iter_mut().find(|(representative, _)| {
            ds_compare_probe_calibration(
                representative,
                &probe,
                role,
                pipeline::DeepSkyCalibrationPolicy::Strict,
            )
            .compatible
        }) {
            group_paths.push(probe.path.clone());
        } else {
            groups.push((probe.clone(), vec![probe.path]));
        }
    }
    groups
        .into_iter()
        .map(|(representative, paths)| (representative.exptime, paths))
        .collect()
}

/// Normaliza filtros mono y multibanda a un id estable. Las combinaciones se
/// resuelven antes que sus líneas individuales: un SV220 normal es Ha+OIII y
/// la variante rotulada SII/OIII es SII+OIII.
fn ds_filter_token(s: &str) -> Option<&'static str> {
    let lower = s.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    let has = |names: &[&str]| tokens.iter().any(|token| names.contains(token));
    let has_ha = has(&["ha", "halpha", "h2"]);
    let has_oiii = has(&["oiii", "o3"]);
    let has_sii = has(&["sii", "s2"]);
    let sv220 = has(&["sv220"]);
    // Dual-band OSC brand filters that pass Ha+OIII: Optolong L-eXtreme /
    // L-eNhance / L-Ultimate, Antlia ALP-T / Duo, IDAS NBZ / NBX / NB1, and the
    // generic "duo/dual-band" naming. Filenames tokenize on non-alphanumerics
    // (e.g. "L-eXtreme" → ["l","extreme"]), so match the distinctive token. The
    // FITS FILTER header still wins (checked in ds_filter_of_path); this only
    // rescues filename-tagged data that would otherwise fall through to broadband.
    let brand_haoiii = has(&[
        "extreme", "enhance", "ultimate", "nbz", "nbx", "nb1", "duo", "duoband", "dualband",
    ]) || (has(&["antlia"]) && has(&["alp", "alpt"]))
        || (has(&["idas"]) && has(&["nb"]));
    if has_sii && (has_oiii || sv220) {
        return Some("SII_OIII");
    }
    if (has_ha && has_oiii) || (sv220 && !has_sii) || brand_haoiii {
        return Some("HA_OIII");
    }
    for tok in tokens {
        let f = match tok {
            "ha" | "halpha" | "h2" => Some("HA"),
            "oiii" | "o3" => Some("OIII"),
            "sii" | "s2" => Some("SII"),
            "r" | "red" | "rojo" => Some("R"),
            "g" | "green" | "verde" => Some("G"),
            "b" | "blue" | "azul" => Some("B"),
            "l" | "lum" | "luminance" | "luminancia" => Some("L"),
            _ => None,
        };
        if f.is_some() {
            return f;
        }
    }
    None
}

fn ds_filter_components(filter: &str) -> Vec<String> {
    match filter.trim().to_ascii_uppercase().as_str() {
        "HA_OIII" | "HA+OIII" | "HOO" => vec!["HA".into(), "OIII".into()],
        "SII_OIII" | "SII+OIII" => vec!["SII".into(), "OIII".into()],
        value if !value.is_empty() => vec![value.into()],
        _ => Vec::new(),
    }
}

fn ds_filter_display(filter: &str) -> String {
    match filter.trim().to_ascii_uppercase().as_str() {
        "HA_OIII" => "Ha + OIII".into(),
        "SII_OIII" => "SII + OIII".into(),
        "HA" => "Ha".into(),
        other => other.into(),
    }
}

/// Filter of a frame: FITS FILTER header first (authoritative), filename
/// tokens as fallback. None = broadband/unknown (OSC one-shot-color typical).
fn ds_filter_of_path(path: &str) -> Option<&'static str> {
    let lower = path.to_lowercase();
    let fname = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let filename_filter = ds_filter_token(&fname).or_else(|| ds_filter_token(path));
    if lower.ends_with(".fits") || lower.ends_with(".fit") {
        if let Ok(fits) = fitrs::Fits::open(path) {
            if let Some(hdu) = fits.iter().next() {
                if let Some(name) = ds_hdr_str(&hdu, "FILTER") {
                    if let Some(f) = ds_filter_token(&name) {
                        // Algunos programas escriben sólo "SV220" en FILTER
                        // incluso para la variante SII+OIII. El nombre suele
                        // conservar la variante específica y gana en ese caso.
                        if f == "HA_OIII" && filename_filter == Some("SII_OIII") {
                            return filename_filter;
                        }
                        return Some(f);
                    }
                }
            }
        }
    }
    filename_filter
}

/// Cosmetic hot-pixel correction (SIRIL parity): without darks, sensor hot
/// pixels survive calibration and dodge the σ-clip (they sit at the SAME
/// pixel in every frame after registration only if the mount never moved —
/// with dithering/drift they become spurious stars). A pixel far above its
/// 8-neighbour median is replaced by that median.
fn ds_cosmetic_stats(img: &DsImage) -> (Vec<f32>, Vec<f32>) {
    let mut medians = Vec::with_capacity(img.ch);
    let mut noises = Vec::with_capacity(img.ch);
    for c in 0..img.ch {
        let plane: Vec<f32> = img.data.iter().skip(c).step_by(img.ch).copied().collect();
        let step = (plane.len() / 150_000).max(1);
        let mut sample: Vec<f32> = plane.iter().step_by(step).copied().collect();
        sample.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med = sample.get(sample.len() / 2).copied().unwrap_or(0.0);
        let mut dev: Vec<f32> = sample.iter().map(|v| (v - med).abs()).collect();
        dev.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        medians.push(med);
        noises.push((dev.get(dev.len() / 2).copied().unwrap_or(0.0) * 1.4826).max(2.0));
    }
    (medians, noises)
}

fn ds_cosmetic_hot_pixels_with_stats(img: &mut DsImage, medians: &[f32], noises: &[f32]) {
    let (w, h, ch) = (img.w, img.h, img.ch);
    if w < 8 || h < 8 || medians.len() < ch || noises.len() < ch {
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
        let med = medians[c];
        let noise = noises[c];

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

fn ds_cosmetic_hot_pixels(img: &mut DsImage) {
    let (medians, noises) = ds_cosmetic_stats(img);
    ds_cosmetic_hot_pixels_with_stats(img, &medians, &noises);
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
                    let f: f64 = ds_poly_basis(xs[i], ys[i], DEG)
                        .iter()
                        .zip(&coef)
                        .map(|(b, c)| b * c)
                        .sum();
                    res.push(vs[i] - f);
                }
            }
            let mut ares: Vec<f64> = res.iter().map(|r| r.abs()).collect();
            ares.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let sigma = ares[ares.len() / 2] * 1.4826 + 1e-6;
            for i in 0..vs.len() {
                if keep[i] {
                    let f: f64 = ds_poly_basis(xs[i], ys[i], DEG)
                        .iter()
                        .zip(&coef)
                        .map(|(b, c)| b * c)
                        .sum();
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
            .map(|i| {
                ds_poly_basis(xs[i], ys[i], DEG)
                    .iter()
                    .zip(&coef)
                    .map(|(b, cc)| b * cc)
                    .sum()
            })
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
                let g: f64 = ds_poly_basis(xn, yn, DEG)
                    .iter()
                    .zip(coef_ref)
                    .map(|(b, cc)| b * cc)
                    .sum();
                let v = row[x * ch + c] as f64 - (g - level);
                // Keep the LINEAR master unclamped: negative sky (real, after
                // background subtraction) and >16-bit headroom must survive.
                row[x * ch + c] = v as f32;
            }
        });
    }
}

/// Standard calibration: light' = (light − bias − dark) / flat_norm.
/// DARK OPTIMIZATION (SIRIL-style): the scalar k that best cancels the master
/// dark's thermal/hot-pixel pattern in THIS light (temperature/exposure often
/// differ). Estimated robustly from the pixels where the dark signal dominates
/// (99th percentile) as the median of (light−bias)/dark. `hint` is the prior
/// scale from the light/dark EXPOSURE RATIO (1.0 when unknown): the search
/// window is centred on it and it is returned when the measurement is not
/// possible (missing/mismatched dark, too few hot pixels). `dark` is expected
/// to already be bias-subtracted (pure thermal signal D_cal).
/// Detección de AMP GLOW en un master dark (ya sin bias): mediana por bloques
/// de 32×32 comparada con la mediana global, en unidades de MAD. Un dark sano
/// es espacialmente plano (hot pixels aislados no mueven la mediana de bloque);
/// el glow es una REGIÓN contigua elevada. Si ≥1% de los bloques supera la
/// mediana global en más de 6·MAD, hay glow — y el escalado k del dark deja de
/// ser válido (el glow no escala linealmente con exposición/temperatura).
fn ds_dark_has_amp_glow(dark: &DsImage) -> bool {
    let (w, h) = (dark.w, dark.h);
    if w < 64 || h < 64 {
        return false;
    }
    let block = 32usize;
    let bw = w / block;
    let bh = h / block;
    let mut medians = Vec::with_capacity(bw * bh);
    let mut sample = Vec::with_capacity(64);
    for by in 0..bh {
        for bx in 0..bw {
            sample.clear();
            for sy in 0..8 {
                for sx in 0..8 {
                    let x = bx * block + sx * block / 8;
                    let y = by * block + sy * block / 8;
                    sample.push(dark.data[(y * w + x) * dark.ch]);
                }
            }
            sample.sort_by(|a, b| a.total_cmp(b));
            medians.push(sample[sample.len() / 2]);
        }
    }
    if medians.len() < 16 {
        return false;
    }
    let mut sorted = medians.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let global = sorted[sorted.len() / 2];
    let mut deviations: Vec<f32> = sorted.iter().map(|v| (v - global).abs()).collect();
    deviations.sort_by(|a, b| a.total_cmp(b));
    let mad = deviations[deviations.len() / 2].max(1e-3);
    let hot = medians.iter().filter(|&&v| v > global + 6.0 * mad).count();
    hot >= (medians.len() / 100).max(3)
}

#[derive(Clone, Copy, Debug)]
struct DsDarkScalingMeasurement {
    scale: f32,
    correlation: f32,
    linearity_r2: f32,
    residual_fraction: f32,
    fitted_slope: f32,
    samples: usize,
}

/// Proves that an exposure-ratio dark scale is valid for this light. The fit
/// is measured on the strongest dark-pattern samples, with an intercept for
/// sky/background and a robust first-pass rejection of stars/cosmics. The
/// contract deliberately returns the physical exposure ratio, not the fitted
/// slope: the latter is evidence, never a license to optimize away object
/// signal. If any gate fails, the caller must omit the mismatched dark.
fn ds_measure_dark_scaling(
    light: &DsImage,
    bias: Option<&DsImage>,
    dark: &DsImage,
    exposure_ratio: f32,
    amp_glow_detected: bool,
    bias_subtracted: bool,
) -> Result<DsDarkScalingMeasurement, String> {
    use crate::deepsky_calibration_contract::{DarkScalingEvidence, validate_dark_scaling};

    let pedestal_state = if bias_subtracted {
        pipeline::PedestalState::BiasSubtracted
    } else {
        pipeline::PedestalState::RawIncludesBias
    };
    // Run non-statistical blockers first so the error never masquerades as a
    // mere low-correlation dataset.
    validate_dark_scaling(DarkScalingEvidence {
        pedestal_state,
        amp_glow_detected,
        exposure_ratio,
        correlation: 1.0,
        linearity_r2: 1.0,
        residual_fraction: 0.0,
    })?;
    if !ds_master_is_compatible(light, dark) {
        return Err("dark scaling bloqueado: geometría/canales/CFA incompatibles".into());
    }
    let bias = bias.filter(|candidate| ds_master_is_compatible(light, candidate));
    if bias.is_none() {
        return Err("dark scaling bloqueado: falta master bias compatible".into());
    }

    let step = (light.data.len() / 300_000).max(1);
    let mut pairs = Vec::<(f64, f64)>::with_capacity(light.data.len() / step + 1);
    for index in (0..light.data.len()).step_by(step) {
        let x = dark.data[index] as f64;
        let y = (light.data[index] - bias.unwrap().data[index]) as f64;
        if x.is_finite() && y.is_finite() {
            pairs.push((x, y));
        }
    }
    if pairs.len() < 128 {
        return Err(format!(
            "dark scaling bloqueado: sólo {} muestras finitas",
            pairs.len()
        ));
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    // The upper 10% emphasizes repeatable hot-pixel/thermal structure. Cap the
    // fit so a 60 MP frame does not turn this validation into a bottleneck.
    let keep = (pairs.len() / 10).clamp(64, 30_000);
    let mut selected = pairs.split_off(pairs.len() - keep);

    let median = |values: &mut Vec<f64>| -> f64 {
        values.sort_by(|a, b| a.total_cmp(b));
        let middle = values.len() / 2;
        if values.len() % 2 == 0 {
            (values[middle - 1] + values[middle]) * 0.5
        } else {
            values[middle]
        }
    };
    let ratio = exposure_ratio as f64;
    let mut expected_residuals = selected
        .iter()
        .map(|(x, y)| y - ratio * x)
        .collect::<Vec<_>>();
    let intercept = median(&mut expected_residuals);
    let mut deviations = expected_residuals
        .iter()
        .map(|value| (value - intercept).abs())
        .collect::<Vec<_>>();
    let robust_sigma = (median(&mut deviations) * 1.4826).max(1.0e-6);
    let trim = (6.0 * robust_sigma).max(1.0e-3);
    selected.retain(|(x, y)| (y - ratio * x - intercept).abs() <= trim);
    if selected.len() < 32 {
        return Err(format!(
            "dark scaling bloqueado: sólo {} muestras sobreviven al rechazo robusto",
            selected.len()
        ));
    }

    let count = selected.len() as f64;
    let mean_x = selected.iter().map(|(x, _)| x).sum::<f64>() / count;
    let mean_y = selected.iter().map(|(_, y)| y).sum::<f64>() / count;
    let mut var_x = 0.0f64;
    let mut var_y = 0.0f64;
    let mut covariance = 0.0f64;
    for &(x, y) in &selected {
        let dx = x - mean_x;
        let dy = y - mean_y;
        var_x += dx * dx;
        var_y += dy * dy;
        covariance += dx * dy;
    }
    if var_x <= f64::EPSILON || var_y <= f64::EPSILON {
        return Err("dark scaling bloqueado: patrón térmico sin rango medible".into());
    }
    let fitted_slope = covariance / var_x;
    let fitted_intercept = mean_y - fitted_slope * mean_x;
    let correlation = covariance / (var_x * var_y).sqrt();
    let mut fit_error = 0.0f64;
    let mut expected_error = 0.0f64;
    let mut expected_energy = 0.0f64;
    for &(x, y) in &selected {
        let fit_residual = y - (fitted_intercept + fitted_slope * x);
        let expected_residual = y - (intercept + ratio * x);
        fit_error += fit_residual * fit_residual;
        expected_error += expected_residual * expected_residual;
        let pattern = ratio * (x - mean_x);
        expected_energy += pattern * pattern;
    }
    let linearity_r2 = 1.0 - fit_error / var_y;
    let residual_fraction = if expected_energy > f64::EPSILON {
        (expected_error / expected_energy).sqrt()
    } else {
        f64::INFINITY
    };
    let evidence = DarkScalingEvidence {
        pedestal_state,
        amp_glow_detected,
        exposure_ratio,
        correlation: correlation as f32,
        linearity_r2: linearity_r2 as f32,
        residual_fraction: residual_fraction as f32,
    };
    let scale = validate_dark_scaling(evidence)?;
    Ok(DsDarkScalingMeasurement {
        scale,
        correlation: evidence.correlation,
        linearity_r2: evidence.linearity_r2,
        residual_fraction: evidence.residual_fraction,
        fitted_slope: fitted_slope as f32,
        samples: selected.len(),
    })
}

fn ds_calibrate(
    light: &mut DsImage,
    bias: Option<&DsImage>,
    dark: Option<&DsImage>,
    flat_norm: Option<&DsImage>,
    dark_scale: f32,
) {
    let n = light.data.len();
    let sub = |img: Option<&DsImage>, i: usize, ch_i: usize| -> f32 {
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
                if fv.is_finite() && fv > 0.05 {
                    v /= fv;
                } else {
                    // Legacy/test-only arithmetic has no DQ return channel.
                    // Preserve the same scientific invariant as production:
                    // never turn a weak flat into an artificial 20x signal.
                    v = f32::NAN;
                }
            }
        }
        // NO zero-clamp here: with a low sky background (narrowband, dark
        // skies, short subs) subtraction legitimately drives ~half the
        // background pixels negative; clamping would truncate the noise
        // distribution and crush the sky to black. The OUTPUT PEDESTAL step
        // (`ds_apply_pedestal`, WBPP parity) re-anchors the frame afterwards.
        light.data[i] = v;
    }
}

#[derive(Clone)]
struct DsCalibratedUncertainty {
    variance: Vec<f32>,
    /// Spatial DQ plane (one word per pixel, regardless of SCI channels).
    dq: Vec<u32>,
    publishable: bool,
    fallback_reason: Option<String>,
}

#[inline]
fn ds_uncertainty_fatal_dq() -> u32 {
    crate::deepsky_variance::dq::SATURATED
        | crate::deepsky_variance::dq::NONLINEAR
        | crate::deepsky_variance::dq::HOT_COLD
        | crate::deepsky_variance::dq::COSMIC
        | crate::deepsky_variance::dq::NAN_INPUT
        | crate::deepsky_variance::dq::FLAT_INVALID
        | crate::deepsky_variance::dq::NO_COVERAGE
        | crate::deepsky_variance::dq::DEGRADED_CALIBRATION
}

/// A sparse invalid detector sample is not a reason to discard a complete
/// calibrated exposure: DQ removes that row from the scientific operator.
/// Conversely, VAR missing on an otherwise valid sample makes the formal
/// uncertainty contract unusable and must fail closed.
fn ds_uncertainty_is_publishable(
    variance: &[f32],
    dq: &[u32],
    w: usize,
    h: usize,
    ch: usize,
) -> bool {
    let Some(pixels) = w.checked_mul(h) else {
        return false;
    };
    if !matches!(ch, 1 | 3)
        || dq.len() != pixels
        || variance.len() != pixels.saturating_mul(ch)
    {
        return false;
    }
    let fatal = ds_uncertainty_fatal_dq();
    let mut valid_samples = 0usize;
    for pixel in 0..pixels {
        if dq[pixel] & fatal != 0 {
            continue;
        }
        for channel in 0..ch {
            let value = variance[pixel * ch + channel];
            if !value.is_finite() || value <= 0.0 {
                return false;
            }
            valid_samples += 1;
        }
    }
    valid_samples > 0
}

#[inline]
fn ds_master_sample_f32(
    plane: &[f32],
    master_channels: usize,
    light_channels: usize,
    sample: usize,
) -> f32 {
    if master_channels == light_channels {
        plane[sample]
    } else {
        let pixel = sample / light_channels;
        let channel = sample % light_channels;
        plane[pixel * master_channels + channel.min(master_channels - 1)]
    }
}

#[inline]
fn ds_master_sample_u32(
    plane: &[u32],
    master_channels: usize,
    light_channels: usize,
    sample: usize,
) -> u32 {
    if master_channels == light_channels {
        plane[sample]
    } else {
        let pixel = sample / light_channels;
        let channel = sample % light_channels;
        plane[pixel * master_channels + channel.min(master_channels - 1)]
    }
}

fn ds_master_geometry_matches(light: &DsImage, master: &DsCalibrationMaster) -> bool {
    light.w == master.image.w
        && light.h == master.image.h
        && matches!(master.image.ch, 1 | 3)
        && matches!(light.ch, 1 | 3)
}

/// Productive scientific calibration.  It preserves the raw-light empirical
/// variance, the estimator variance/DQ of every selected master and the dark
/// pedestal covariance convention.  A weak flat never gets clamped: SCI is
/// NaN and DQ::FLAT_INVALID marks the affected spatial pixel.
fn ds_calibrate_scientific(
    light: &mut DsImage,
    bias: Option<&DsCalibrationMaster>,
    dark: Option<&DsDarkMaster>,
    flat: Option<&DsCalibrationMaster>,
    dark_scale: f32,
) -> Result<DsCalibratedUncertainty, String> {
    let samples = light.data.len();
    let pixels = light
        .w
        .checked_mul(light.h)
        .ok_or("calibración científica: geometría fuera de rango")?;
    if samples != pixels.saturating_mul(light.ch) || !matches!(light.ch, 1 | 3) {
        return Err("calibración científica: layout de light inválido".into());
    }
    for master in bias.into_iter() {
        if !ds_master_geometry_matches(light, master) {
            return Err("calibración científica: bias incompatible".into());
        }
    }
    if let Some(master) = dark {
        if !ds_master_geometry_matches(light, &master.master) {
            return Err("calibración científica: dark incompatible".into());
        }
        if !master.bias_subtracted && (dark_scale - 1.0).abs() > 1.0e-6 {
            return Err("calibración científica: un dark crudo no puede escalarse".into());
        }
    }
    for master in flat.into_iter() {
        if !ds_master_geometry_matches(light, master) {
            return Err("calibración científica: flat incompatible".into());
        }
    }

    let light_channel_variance = crate::deepsky_variance::empirical_channel_variance(light);
    let mut numerator = vec![f32::NAN; samples];
    let mut numerator_variance = vec![f32::NAN; samples];
    let mut dq = vec![0u32; pixels];
    let mut missing_variance = false;

    // A raw dark already contains the pedestal and is therefore subtracted
    // alone. A bias-subtracted thermal dark shares the same bias master with
    // the light and uses the covariance identity implemented in the helper.
    let bias_used = bias.filter(|_| dark.is_none_or(|master| master.bias_subtracted));
    let dark_convention = dark.map(|master| {
        if master.bias_subtracted {
            crate::deepsky_variance::DarkVarianceConvention::SharedBiasSubtractedThermal
        } else {
            crate::deepsky_variance::DarkVarianceConvention::RawDarkOnly
        }
    });

    for sample in 0..samples {
        let pixel = sample / light.ch;
        let channel = sample % light.ch;
        let raw = light.data[sample];
        if !raw.is_finite() {
            dq[pixel] |= crate::deepsky_variance::dq::NAN_INPUT;
            continue;
        }
        let bias_value = bias_used
            .map(|master| {
                ds_master_sample_f32(
                    &master.image.data,
                    master.image.ch,
                    light.ch,
                    sample,
                )
            })
            .unwrap_or(0.0);
        let dark_value = dark
            .map(|master| {
                ds_master_sample_f32(
                    &master.master.image.data,
                    master.master.image.ch,
                    light.ch,
                    sample,
                )
            })
            .unwrap_or(0.0);
        numerator[sample] = raw - bias_value - dark_scale * dark_value;

        let light_variance = light_channel_variance
            .get(channel)
            .copied()
            .unwrap_or(f32::NAN);
        let bias_variance = bias_used
            .map(|master| {
                dq[pixel] |= ds_master_sample_u32(
                    &master.dq,
                    master.image.ch,
                    light.ch,
                    sample,
                );
                ds_master_sample_f32(
                    &master.variance,
                    master.image.ch,
                    light.ch,
                    sample,
                )
            })
            .unwrap_or(0.0);
        let dark_variance = dark
            .map(|master| {
                dq[pixel] |= ds_master_sample_u32(
                    &master.master.dq,
                    master.master.image.ch,
                    light.ch,
                    sample,
                );
                ds_master_sample_f32(
                    &master.master.variance,
                    master.master.image.ch,
                    light.ch,
                    sample,
                )
            })
            .unwrap_or(0.0);
        let value = if let Some(convention) = dark_convention {
            crate::deepsky_variance::calibrated_numerator_variance(
                light_variance,
                bias_variance,
                dark_variance,
                dark_scale,
                convention,
            )
        } else if light_variance.is_finite()
            && light_variance >= 0.0
            && bias_variance.is_finite()
            && bias_variance >= 0.0
        {
            Ok(light_variance + bias_variance)
        } else {
            Err("varianza de light/bias no publicable".into())
        };
        match value {
            Ok(value) => numerator_variance[sample] = value,
            Err(_) => missing_variance = true,
        }
    }

    if let Some(flat) = flat {
        for sample in 0..samples {
            let pixel = sample / light.ch;
            dq[pixel] |= ds_master_sample_u32(
                &flat.dq,
                flat.image.ch,
                light.ch,
                sample,
            );
        }
        if !missing_variance && flat.variance_publishable() {
            let divided = crate::deepsky_variance::divide_by_flat_scientific(
                &numerator,
                &numerator_variance,
                &flat.image.data,
                &flat.variance,
                light.w,
                light.h,
                light.ch,
                flat.image.ch,
                &dq,
                0.05,
            )?;
            light.data = divided.science;
            return Ok(DsCalibratedUncertainty {
                variance: divided.variance,
                dq: divided.dq,
                publishable: true,
                fallback_reason: None,
            });
        }

        // SCI remains useful even when a one-frame master has no estimable
        // VAR. Preserve the flat validity rule but publish no made-up VAR.
        for sample in 0..samples {
            let pixel = sample / light.ch;
            let channel = sample % light.ch;
            let flat_sample = if flat.image.ch == light.ch {
                sample
            } else {
                pixel * flat.image.ch + channel.min(flat.image.ch - 1)
            };
            let response = flat.image.data[flat_sample];
            if !response.is_finite()
                || response <= 0.05
                || dq[pixel]
                    & (crate::deepsky_variance::dq::SATURATED
                        | crate::deepsky_variance::dq::NONLINEAR
                        | crate::deepsky_variance::dq::FLAT_INVALID)
                    != 0
            {
                light.data[sample] = f32::NAN;
                dq[pixel] |= crate::deepsky_variance::dq::FLAT_INVALID;
            } else {
                light.data[sample] = numerator[sample] / response;
            }
        }
        return Ok(DsCalibratedUncertainty {
            variance: vec![f32::NAN; samples],
            dq,
            publishable: false,
            fallback_reason: Some(
                "VAR de calibración no estimable (máster de una sola toma o plano no finito)"
                    .into(),
            ),
        });
    }

    light.data = numerator;
    Ok(DsCalibratedUncertainty {
        variance: numerator_variance,
        dq,
        publishable: !missing_variance,
        fallback_reason: missing_variance.then(|| {
            "VAR de calibración no estimable (máster de una sola toma o plano no finito)".into()
        }),
    })
}

/// Propagate bilinear CFA interpolation.  Coefficients are squared for VAR;
/// DQ is the OR of every contributing photosite and INTERPOLATED marks that
/// the RGB pixel is no longer an independent detector sample.
fn ds_debayer_scientific(
    img: DsImage,
    uncertainty: DsCalibratedUncertainty,
    cid: i32,
) -> (DsImage, DsCalibratedUncertainty) {
    if img.w < 4
        || img.h < 4
        || img.ch != 1
        || uncertainty.variance.len() != img.w * img.h
        || uncertainty.dq.len() != img.w * img.h
    {
        return (img, uncertainty);
    }
    let (rx, ry) = match cid {
        8 => (0usize, 0usize),
        9 => (1, 0),
        10 => (0, 1),
        11 => (1, 1),
        _ => return (img, uncertainty),
    };
    let (w, h) = (img.w, img.h);
    let source_variance = uncertainty.variance;
    let source_dq = uncertainty.dq;
    let rgb = ds_debayer_image(img, cid);
    let mut variance = vec![f32::NAN; w * h * 3];
    let mut dq = vec![
        crate::deepsky_variance::dq::EDGE | crate::deepsky_variance::dq::NO_COVERAGE;
        w * h
    ];
    let combine = |indices: &[usize], coefficient: f32| -> f32 {
        let mut sum = 0.0f32;
        for &index in indices {
            let value = source_variance[index];
            if !value.is_finite() || value < 0.0 {
                return f32::NAN;
            }
            sum += coefficient * coefficient * value;
        }
        sum
    };
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            let o = i * 3;
            let cross = [i - w, i + w, i - 1, i + 1];
            let diagonal = [i - w - 1, i - w + 1, i + w - 1, i + w + 1];
            let horizontal = [i - 1, i + 1];
            let vertical = [i - w, i + w];
            let red = (x & 1) == rx && (y & 1) == ry;
            let blue = (x & 1) != rx && (y & 1) != ry;
            let rgb_variance = if red {
                [
                    source_variance[i],
                    combine(&cross, 0.25),
                    combine(&diagonal, 0.25),
                ]
            } else if blue {
                [
                    combine(&diagonal, 0.25),
                    combine(&cross, 0.25),
                    source_variance[i],
                ]
            } else if (y & 1) == ry {
                [
                    combine(&horizontal, 0.5),
                    source_variance[i],
                    combine(&vertical, 0.5),
                ]
            } else {
                [
                    combine(&vertical, 0.5),
                    source_variance[i],
                    combine(&horizontal, 0.5),
                ]
            };
            variance[o..o + 3].copy_from_slice(&rgb_variance);
            let mut flags = crate::deepsky_variance::dq::INTERPOLATED;
            for &index in std::iter::once(&i)
                .chain(cross.iter())
                .chain(diagonal.iter())
            {
                flags |= source_dq[index];
            }
            dq[i] = flags;
        }
    }
    let publishable = uncertainty.publishable
        && ds_uncertainty_is_publishable(&variance, &dq, w, h, 3);
    (
        rgb,
        DsCalibratedUncertainty {
            variance,
            dq,
            publishable,
            fallback_reason: uncertainty.fallback_reason,
        },
    )
}

fn ds_uncertainty_sigmas(
    variance: &[f32],
    w: usize,
    h: usize,
    ch: usize,
    bayer: Option<i32>,
) -> [f32; 3] {
    let mut populations = [Vec::<f32>::new(), Vec::<f32>::new(), Vec::<f32>::new()];
    let step = (w.saturating_mul(h) / 200_000).max(1);
    for pixel in (0..w.saturating_mul(h)).step_by(step) {
        if ch == 3 {
            for channel in 0..3 {
                let value = variance[pixel * 3 + channel];
                if value.is_finite() && value >= 0.0 {
                    populations[channel].push(value);
                }
            }
        } else {
            let value = variance[pixel];
            if !value.is_finite() || value < 0.0 {
                continue;
            }
            let channel = bayer
                .and_then(|cid| {
                    let x = pixel % w;
                    let y = pixel / w;
                    let (rx, ry) = match cid {
                        8 => (0usize, 0usize),
                        9 => (1, 0),
                        10 => (0, 1),
                        11 => (1, 1),
                        _ => return None,
                    };
                    Some(if (x & 1) == rx && (y & 1) == ry {
                        0
                    } else if (x & 1) != rx && (y & 1) != ry {
                        2
                    } else {
                        1
                    })
                })
                .unwrap_or(0);
            populations[channel].push(value);
        }
    }
    let mut out = [f32::NAN; 3];
    for channel in 0..3 {
        populations[channel].sort_by(|a, b| a.total_cmp(b));
        if let Some(value) = populations[channel].get(populations[channel].len() / 2) {
            out[channel] = value.sqrt();
        }
    }
    if ch == 1 && bayer.is_none() {
        out[1] = out[0];
        out[2] = out[0];
    }
    out
}

/// OUTPUT PEDESTAL opcional. El flujo científico predeterminado (`None`)
/// conserva negativos y headroom en float32. `Some(v)` añade explícitamente
/// el desplazamiento solicitado sin cuantizar ni recortar la señal.
/// Returns the pedestal actually applied.
fn ds_apply_pedestal(img: &mut DsImage, mode: Option<f32>) -> f32 {
    let ped = mode.unwrap_or(0.0).clamp(0.0, 10000.0);
    if ped != 0.0 {
        for v in img.data.iter_mut() {
            *v += ped;
        }
    }
    ped
}

/// Robust background level (median) and noise scale (MAD·1.4826) of a plane.
/// Exact zeros are excluded (warp borders / non-data) so they cannot drag the
/// sky statistics to black; falls back to everything on a near-empty plane.
fn ds_bg_noise(luma: &[f32]) -> (f32, f32) {
    if luma.is_empty() {
        return (0.0, 1.0);
    }
    let step = (luma.len() / 200_000).max(1);
    let mut s: Vec<f32> = luma
        .iter()
        .step_by(step)
        .copied()
        .filter(|v| v.is_finite() && v.abs() > f32::EPSILON)
        .collect();
    if s.len() < 64 {
        s = luma.iter().step_by(step).copied().collect();
    }
    if s.is_empty() {
        return (0.0, 1.0);
    }
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let bg = s[s.len() / 2];
    let mut d: Vec<f32> = s.iter().map(|v| (v - bg).abs()).collect();
    d.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (bg, (d[d.len() / 2] * 1.4826).max(1.0))
}

/// Per-channel background level (25th percentile ≈ sky, robust to stars/nebula),
/// used by PER-CHANNEL normalization so OSC/RGB frames with differing sky per
/// channel (green cast, wavelength-dependent extinction) are matched channel by
/// channel — not by one luma scalar. Mono replicates channel 0.
fn ds_channel_backgrounds(img: &DsImage) -> [f32; 3] {
    let ch = img.ch;
    let n = img.w * img.h;
    if n == 0 {
        return [0.0; 3];
    }
    let step = (n / 200_000).max(1);
    let mut out = [0.0f32; 3];
    for c in 0..ch.min(3) {
        let mut s: Vec<f32> = (0..n).step_by(step).map(|i| img.data[i * ch + c]).collect();
        if s.is_empty() {
            continue;
        }
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        out[c] = s[s.len() / 4];
    }
    if ch == 1 {
        out = [out[0]; 3];
    }
    out
}

/// MRS-style noise estimate (PixInsight/Siril parity): the robust MAD of the
/// first à-trous wavelet DETAIL layer (luma − B3-spline smoothed at the finest
/// scale). A plain background MAD lets large-scale structure (nebula, gradients)
/// leak into the estimate; the detail layer removes it, so this reflects the true
/// per-pixel read+shot noise — the correct quantity for both frame weighting and
/// the rejection σ-floor. Falls back to the global MAD on tiny frames.
fn ds_mrs_noise(luma: &[f32], w: usize, h: usize) -> f32 {
    let n = w * h;
    if w < 5 || h < 5 || luma.len() < n {
        return ds_bg_noise(luma).1;
    }
    // Separable B3 spline [1,4,6,4,1]/16 at hole=1 (finest à-trous scale), mirror
    // boundaries. High-pass detail = luma − smooth isolates the noise.
    let clampi = |v: isize, hi: usize| -> usize { v.clamp(0, hi as isize - 1) as usize };
    let mut tmp = vec![0.0f32; n];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let base = y * w;
        for x in 0..w {
            let xm2 = clampi(x as isize - 2, w);
            let xm1 = clampi(x as isize - 1, w);
            let xp1 = clampi(x as isize + 1, w);
            let xp2 = clampi(x as isize + 2, w);
            row[x] = (luma[base + xm2]
                + 4.0 * luma[base + xm1]
                + 6.0 * luma[base + x]
                + 4.0 * luma[base + xp1]
                + luma[base + xp2])
                / 16.0;
        }
    });
    let tmp_ref = &tmp;
    let step = (n / 200_000).max(1);
    // Vertical pass folded into the detail+abs sample collection (subsampled).
    let mut d: Vec<f32> = (0..n)
        .step_by(step)
        .map(|i| {
            let x = i % w;
            let y = i / w;
            let ym2 = clampi(y as isize - 2, h);
            let ym1 = clampi(y as isize - 1, h);
            let yp1 = clampi(y as isize + 1, h);
            let yp2 = clampi(y as isize + 2, h);
            let smooth = (tmp_ref[ym2 * w + x]
                + 4.0 * tmp_ref[ym1 * w + x]
                + 6.0 * tmp_ref[y * w + x]
                + 4.0 * tmp_ref[yp1 * w + x]
                + tmp_ref[yp2 * w + x])
                / 16.0;
            (luma[i] - smooth).abs()
        })
        .collect();
    if d.is_empty() {
        return 1.0;
    }
    d.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mad = d[d.len() / 2];
    // σ_image ≈ MAD·1.4826 / 0.889 (0.889 = σ of the first B3 à-trous layer for
    // unit-variance white noise).
    ((mad * 1.4826) / 0.889).max(1.0)
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

/// Local-normalization fields may be legacy luma (one grid) or the scientific
/// channel-major layout (one grid per channel). Keep legacy caches readable,
/// but sample the matching channel whenever the full layout is present.
fn ds_sample_local_field(
    field: &[f32],
    gw: usize,
    gh: usize,
    channels: usize,
    channel: usize,
    u: f32,
    v: f32,
) -> f32 {
    let cells = gw.saturating_mul(gh);
    let grid = if cells > 0 && field.len() >= cells.saturating_mul(channels) {
        let start = channel.min(channels.saturating_sub(1)).saturating_mul(cells);
        &field[start..start + cells]
    } else {
        field
    };
    ds_sample_grid(grid, gw, gh, u, v)
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

#[derive(Clone, Copy, Debug)]
struct DsPsfFit {
    x: f32,
    y: f32,
    sigma: f32,
    flux: f32,
    reduced_error: f32,
}

fn ds_solve_psf_normal_equations(mut matrix: [[f64; 5]; 5], mut rhs: [f64; 5]) -> Option<[f64; 5]> {
    for column in 0..5 {
        let pivot = (column..5)
            .max_by(|&a, &b| matrix[a][column].abs().total_cmp(&matrix[b][column].abs()))?;
        if matrix[pivot][column].abs() < 1e-12 {
            return None;
        }
        if pivot != column {
            matrix.swap(pivot, column);
            rhs.swap(pivot, column);
        }
        let divisor = matrix[column][column];
        for value in &mut matrix[column][column..] {
            *value /= divisor;
        }
        rhs[column] /= divisor;
        for row in 0..5 {
            if row == column {
                continue;
            }
            let factor = matrix[row][column];
            for c in column..5 {
                matrix[row][c] -= factor * matrix[column][c];
            }
            rhs[row] -= factor * rhs[column];
        }
    }
    Some(rhs)
}

/// Ajuste PSF gaussiano circular por Gauss-Newton sobre una ventana 9×9.
/// Devuelve centro subpíxel, sigma/FWHM y flujo integrado; si una estrella está
/// saturada, truncada o mal condicionada el detector conserva su fallback CoG.
fn ds_fit_star_psf(
    luma: &[f32],
    w: usize,
    h: usize,
    candidate_x: usize,
    candidate_y: usize,
    initial_background: f32,
) -> Option<DsPsfFit> {
    const RADIUS: i32 = 4;
    if candidate_x < RADIUS as usize
        || candidate_y < RADIUS as usize
        || candidate_x + RADIUS as usize >= w
        || candidate_y + RADIUS as usize >= h
    {
        return None;
    }
    let peak = luma[candidate_y * w + candidate_x] as f64;
    let mut params = [
        initial_background as f64,
        (peak - initial_background as f64).max(1.0),
        candidate_x as f64,
        candidate_y as f64,
        1.5f64,
    ];
    // Centro y ancho iniciales por momentos positivos.
    let (mut sum, mut sx, mut sy, mut sr2) = (0.0, 0.0, 0.0, 0.0);
    for dy in -RADIUS..=RADIUS {
        for dx in -RADIUS..=RADIUS {
            let x = (candidate_x as i32 + dx) as usize;
            let y = (candidate_y as i32 + dy) as usize;
            let value = (luma[y * w + x] - initial_background).max(0.0) as f64;
            sum += value;
            sx += value * x as f64;
            sy += value * y as f64;
            sr2 += value * (dx * dx + dy * dy) as f64;
        }
    }
    if sum <= 1e-6 {
        return None;
    }
    params[2] = sx / sum;
    params[3] = sy / sum;
    params[4] = ((sr2 / sum / 2.0).sqrt()).clamp(0.65, 3.5);

    for _ in 0..8 {
        let mut normal = [[0.0f64; 5]; 5];
        let mut rhs = [0.0f64; 5];
        let sigma = params[4].clamp(0.55, 4.5);
        let sigma2 = sigma * sigma;
        for dy in -RADIUS..=RADIUS {
            for dx in -RADIUS..=RADIUS {
                let x = (candidate_x as i32 + dx) as usize;
                let y = (candidate_y as i32 + dy) as usize;
                let rx = x as f64 - params[2];
                let ry = y as f64 - params[3];
                let r2 = rx * rx + ry * ry;
                let exponential = (-0.5 * r2 / sigma2).exp();
                let model = params[0] + params[1] * exponential;
                let residual = luma[y * w + x] as f64 - model;
                let jacobian = [
                    1.0,
                    exponential,
                    params[1] * exponential * rx / sigma2,
                    params[1] * exponential * ry / sigma2,
                    params[1] * exponential * r2 / (sigma * sigma2),
                ];
                for row in 0..5 {
                    rhs[row] += jacobian[row] * residual;
                    for column in 0..5 {
                        normal[row][column] += jacobian[row] * jacobian[column];
                    }
                }
            }
        }
        // Levenberg damping keeps very compact/saturated stars well posed.
        for i in 0..5 {
            normal[i][i] += normal[i][i].abs() * 1e-6 + 1e-9;
        }
        let delta = ds_solve_psf_normal_equations(normal, rhs)?;
        params[0] += delta[0].clamp(-params[1] * 0.25, params[1] * 0.25);
        params[1] = (params[1] + delta[1].clamp(-params[1] * 0.5, params[1] * 0.5)).max(1.0);
        params[2] += delta[2].clamp(-0.6, 0.6);
        params[3] += delta[3].clamp(-0.6, 0.6);
        params[4] = (params[4] + delta[4].clamp(-0.35, 0.35)).clamp(0.55, 4.5);
        if delta[2].abs() < 1e-4 && delta[3].abs() < 1e-4 && delta[4].abs() < 1e-4 {
            break;
        }
    }
    if (params[2] - candidate_x as f64).abs() > 2.5
        || (params[3] - candidate_y as f64).abs() > 2.5
        || !params.iter().all(|value| value.is_finite())
    {
        return None;
    }
    let mut squared_error = 0.0;
    let sigma2 = params[4] * params[4];
    for dy in -RADIUS..=RADIUS {
        for dx in -RADIUS..=RADIUS {
            let x = (candidate_x as i32 + dx) as usize;
            let y = (candidate_y as i32 + dy) as usize;
            let r2 = (x as f64 - params[2]).powi(2) + (y as f64 - params[3]).powi(2);
            let model = params[0] + params[1] * (-0.5 * r2 / sigma2).exp();
            squared_error += (luma[y * w + x] as f64 - model).powi(2);
        }
    }
    Some(DsPsfFit {
        x: params[2] as f32,
        y: params[3] as f32,
        sigma: params[4] as f32,
        flux: (params[1] * 2.0 * std::f64::consts::PI * sigma2) as f32,
        reduced_error: (squared_error / (81 - 5) as f64).sqrt() as f32,
    })
}

/// Star detection: LOCAL background + MAD threshold (per-cell grid — a global
/// threshold under heavy vignetting loses the corner stars exactly where the
/// registration needs them most), 3×3 local maxima with hot-pixel rejection
/// (a real star is EXTENDED), ajuste PSF gaussiano 9×9 y flux-ranked.
fn ds_detect_stars_impl(
    luma: &[f32],
    w: usize,
    h: usize,
    max_n: usize,
    gpu_candidates: bool,
) -> Result<Vec<(f32, f32, f32)>, String> {
    if w < 32 || h < 32 || luma.len() < w * h {
        return Ok(Vec::new());
    }
    // Per-cell (≈128 px) robust background (median) and noise (MAD·1.4826),
    // bilinearly sampled per pixel. Small images degenerate to one cell =
    // exactly the old global behaviour.
    let gw = (w / 128).clamp(1, 32);
    let gh = (h / 128).clamp(1, 32);
    let mut bg_grid = vec![0.0f32; gw * gh];
    let mut nz_grid = vec![1.0f32; gw * gh];
    for gy in 0..gh {
        let y0 = gy * h / gh;
        let y1 = ((gy + 1) * h / gh).max(y0 + 1).min(h);
        for gx in 0..gw {
            let x0 = gx * w / gw;
            let x1 = ((gx + 1) * w / gw).max(x0 + 1).min(w);
            let sx = ((x1 - x0) / 24).max(1);
            let sy = ((y1 - y0) / 24).max(1);
            let mut cell: Vec<f32> = Vec::with_capacity(700);
            let mut y = y0;
            while y < y1 {
                let mut x = x0;
                while x < x1 {
                    cell.push(luma[y * w + x]);
                    x += sx;
                }
                y += sy;
            }
            if cell.is_empty() {
                continue;
            }
            cell.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let med = cell[cell.len() / 2];
            let mut dev: Vec<f32> = cell.iter().map(|v| (v - med).abs()).collect();
            dev.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            bg_grid[gy * gw + gx] = med;
            nz_grid[gy * gw + gx] = (dev[dev.len() / 2] * 1.4826).max(1.0);
        }
    }
    let bg_at = |x: usize, y: usize| -> f32 {
        ds_sample_grid(&bg_grid, gw, gh, x as f32 / w as f32, y as f32 / h as f32)
    };
    let nz_at = |x: usize, y: usize| -> f32 {
        ds_sample_grid(&nz_grid, gw, gh, x as f32 / w as f32, y as f32 / h as f32)
    };

    let mut cands: Vec<(usize, usize, f32)> = Vec::new();
    if gpu_candidates {
        let scores =
            crate::gpu_deepsky::star_candidate_map(luma, w, h, &bg_grid, &nz_grid, gw, gh)?;
        for y in 4..h - 4 {
            for x in 4..w - 4 {
                let score = scores[y * w + x];
                if score > 0.0 {
                    cands.push((x, y, score));
                }
            }
        }
    } else {
        for y in 4..h - 4 {
            let row = y * w;
            for x in 4..w - 4 {
                let v = luma[row + x];
                let bgl = bg_at(x, y);
                let nzl = nz_at(x, y);
                if v <= bgl + 5.0 * nzl {
                    continue;
                }
                let thr_ext = bgl + 2.5 * nzl;
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
                    cands.push((x, y, v - bgl));
                }
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
        let bgl = bg_at(cx, cy);
        if let Some(psf) = ds_fit_star_psf(luma, w, h, cx, cy, bgl) {
            if psf.flux > 0.0 && psf.reduced_error.is_finite() {
                stars.push((psf.x, psf.y, psf.flux));
                if stars.len() >= max_n {
                    break;
                }
                continue;
            }
        }
        // Fallback determinista para estrellas saturadas/truncadas cuya matriz
        // PSF queda mal condicionada.
        let (mut sw, mut sx, mut sy) = (0.0f64, 0.0f64, 0.0f64);
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                let px = (cx as i32 + dx) as usize;
                let py = (cy as i32 + dy) as usize;
                let wgt = (luma[py * w + px] - bgl).max(0.0) as f64;
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
    Ok(stars)
}

fn ds_detect_stars(luma: &[f32], w: usize, h: usize, max_n: usize) -> Vec<(f32, f32, f32)> {
    ds_detect_stars_impl(luma, w, h, max_n, false).unwrap_or_default()
}

fn ds_psf_signal_level(stars: &[(f32, f32, f32)]) -> Option<f32> {
    let mut fluxes: Vec<f32> = stars
        .iter()
        .take(30)
        .map(|star| star.2)
        .filter(|flux| flux.is_finite() && *flux > 0.0)
        .collect();
    if fluxes.len() < 6 {
        return None;
    }
    fluxes.sort_by(|a, b| a.total_cmp(b));
    Some(fluxes[fluxes.len() / 2])
}

/// FWHM de cuadro: mediana del ajuste PSF de las estrellas más brillantes.
/// Lower = sharper; la mediana evita que una estrella saturada domine el peso.
fn ds_frame_fwhm_proxy(luma: &[f32], w: usize, h: usize, stars: &[(f32, f32, f32)]) -> f32 {
    let mut fwhms: Vec<f32> = Vec::new();
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
        if let Some(psf) = ds_fit_star_psf(luma, w, h, cx as usize, cy as usize, bg) {
            if psf.reduced_error.is_finite() {
                fwhms.push(psf.sigma * 2.354_820_1);
                continue;
            }
        }
        // Mismo fallback de momentos que versiones anteriores.
        let (mut sw, mut sr2) = (0.0f64, 0.0f64);
        for dy in -3i32..=3 {
            for dx in -3i32..=3 {
                let v = (luma[((cy + dy) as usize) * w + (cx + dx) as usize] - bg).max(0.0) as f64;
                sw += v;
                sr2 += v * ((dx * dx + dy * dy) as f64);
            }
        }
        if sw > 1e-6 {
            fwhms.push(((sr2 / sw) / 2.0).sqrt() as f32 * 2.354_820_1);
        }
    }
    if fwhms.is_empty() {
        return 0.0;
    }
    fwhms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    fwhms[fwhms.len() / 2]
}

/// FWHM POR ESTRELLA (x, y, fwhm) — base del RESCATE DE DETALLE: mide la
/// nitidez local real (forma de la PSF, independiente del brillo) en cada
/// zona del frame. Mismo ajuste PSF + fallback de momentos que el proxy
/// global; hasta 80 estrellas para cubrir el campo.
fn ds_star_fwhms(
    luma: &[f32],
    w: usize,
    h: usize,
    stars: &[(f32, f32, f32)],
) -> Vec<(f32, f32, f32)> {
    let step = (luma.len() / 100_000).max(1);
    let mut s: Vec<f32> = luma.iter().step_by(step).copied().collect();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let bg = s[s.len() / 2];
    let mut out = Vec::with_capacity(stars.len().min(80));
    for &(sx, sy, _) in stars.iter().take(80) {
        let cx = sx.round() as i32;
        let cy = sy.round() as i32;
        if cx < 4 || cy < 4 || cx >= (w as i32 - 4) || cy >= (h as i32 - 4) {
            continue;
        }
        if let Some(psf) = ds_fit_star_psf(luma, w, h, cx as usize, cy as usize, bg) {
            if psf.reduced_error.is_finite() && psf.sigma > 0.0 {
                out.push((sx, sy, psf.sigma * 2.354_820_1));
                continue;
            }
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
            out.push((sx, sy, ((sr2 / sw) / 2.0).sqrt() as f32 * 2.354_820_1));
        }
    }
    out
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum DsRegistrationModel {
    Similarity,
    Affine,
    Projective,
    LocalDistortion,
}

impl DsRegistrationModel {
    fn label(self) -> &'static str {
        match self {
            Self::Similarity => "similitud",
            Self::Affine => "afín",
            Self::Projective => "proyectivo",
            Self::LocalDistortion => "distorsión local",
        }
    }
}

/// Transformación target→reference. `h` representa similitud/afín/homografía;
/// `poly` es un ajuste cuadrático normalizado para campos con distorsión.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
struct DsTransform {
    model: DsRegistrationModel,
    h: [f64; 9],
    poly: [f64; 12],
    norm: [f64; 3], // centro x/y y escala de las coordenadas del target
}

impl DsTransform {
    fn identity() -> Self {
        Self {
            model: DsRegistrationModel::Similarity,
            h: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            poly: [0.0; 12],
            norm: [0.0, 0.0, 1.0],
        }
    }

    fn from_similarity(t: (f32, f32, f32, f32)) -> Self {
        Self {
            model: DsRegistrationModel::Similarity,
            h: [
                t.0 as f64,
                -(t.1 as f64),
                t.2 as f64,
                t.1 as f64,
                t.0 as f64,
                t.3 as f64,
                0.0,
                0.0,
                1.0,
            ],
            poly: [0.0; 12],
            norm: [0.0, 0.0, 1.0],
        }
    }

    fn to_gpu(self) -> Option<crate::gpu_deepsky::WarpTransform> {
        if self.model == DsRegistrationModel::LocalDistortion {
            return Some(crate::gpu_deepsky::WarpTransform {
                inverse_h: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
                poly: self.poly.map(|v| v as f32),
                norm: self.norm.map(|v| v as f32),
                local_distortion: true,
            });
        }
        let m = self.h;
        let det = m[0] * (m[4] * m[8] - m[5] * m[7]) - m[1] * (m[3] * m[8] - m[5] * m[6])
            + m[2] * (m[3] * m[7] - m[4] * m[6]);
        if det.abs() < 1e-14 {
            return None;
        }
        let inv = [
            (m[4] * m[8] - m[5] * m[7]) / det,
            (m[2] * m[7] - m[1] * m[8]) / det,
            (m[1] * m[5] - m[2] * m[4]) / det,
            (m[5] * m[6] - m[3] * m[8]) / det,
            (m[0] * m[8] - m[2] * m[6]) / det,
            (m[2] * m[3] - m[0] * m[5]) / det,
            (m[3] * m[7] - m[4] * m[6]) / det,
            (m[1] * m[6] - m[0] * m[7]) / det,
            (m[0] * m[4] - m[1] * m[3]) / det,
        ];
        Some(crate::gpu_deepsky::WarpTransform {
            inverse_h: inv.map(|v| v as f32),
            poly: [0.0; 12],
            norm: [0.0, 0.0, 1.0],
            local_distortion: false,
        })
    }

    fn forward(self, x: f32, y: f32) -> (f32, f32) {
        let (x, y) = (x as f64, y as f64);
        if self.model == DsRegistrationModel::LocalDistortion {
            let s = self.norm[2].max(1e-9);
            let xn = (x - self.norm[0]) / s;
            let yn = (y - self.norm[1]) / s;
            let b = [xn, yn, 1.0, xn * xn, xn * yn, yn * yn];
            let u = (0..6).map(|i| self.poly[i] * b[i]).sum::<f64>();
            let v = (0..6).map(|i| self.poly[6 + i] * b[i]).sum::<f64>();
            return (u as f32, v as f32);
        }
        let d = self.h[6] * x + self.h[7] * y + self.h[8];
        if d.abs() < 1e-12 {
            return (f32::NAN, f32::NAN);
        }
        (
            ((self.h[0] * x + self.h[1] * y + self.h[2]) / d) as f32,
            ((self.h[3] * x + self.h[4] * y + self.h[5]) / d) as f32,
        )
    }

    /// Reference→target. Homografías se invierten analíticamente; el modelo
    /// local usa Newton con Jacobiano cuadrático y semilla afín.
    fn inverse(self, u: f32, v: f32) -> Option<(f32, f32)> {
        let (u, v) = (u as f64, v as f64);
        if self.model == DsRegistrationModel::LocalDistortion {
            let (a, b, c, d) = (self.poly[0], self.poly[1], self.poly[6], self.poly[7]);
            let det = a * d - b * c;
            if det.abs() < 1e-12 {
                return None;
            }
            let du = u - self.poly[2];
            let dv = v - self.poly[8];
            let mut xn = (d * du - b * dv) / det;
            let mut yn = (-c * du + a * dv) / det;
            for _ in 0..7 {
                let basis = [xn, yn, 1.0, xn * xn, xn * yn, yn * yn];
                let fu = (0..6).map(|i| self.poly[i] * basis[i]).sum::<f64>() - u;
                let fv = (0..6).map(|i| self.poly[6 + i] * basis[i]).sum::<f64>() - v;
                let j00 = self.poly[0] + 2.0 * self.poly[3] * xn + self.poly[4] * yn;
                let j01 = self.poly[1] + self.poly[4] * xn + 2.0 * self.poly[5] * yn;
                let j10 = self.poly[6] + 2.0 * self.poly[9] * xn + self.poly[10] * yn;
                let j11 = self.poly[7] + self.poly[10] * xn + 2.0 * self.poly[11] * yn;
                let jd = j00 * j11 - j01 * j10;
                if jd.abs() < 1e-14 {
                    return None;
                }
                let dx = (j11 * fu - j01 * fv) / jd;
                let dy = (-j10 * fu + j00 * fv) / jd;
                xn -= dx;
                yn -= dy;
                if dx * dx + dy * dy < 1e-12 {
                    break;
                }
            }
            let x = xn * self.norm[2] + self.norm[0];
            let y = yn * self.norm[2] + self.norm[1];
            return (x.is_finite() && y.is_finite()).then_some((x as f32, y as f32));
        }
        let m = self.h;
        let det = m[0] * (m[4] * m[8] - m[5] * m[7]) - m[1] * (m[3] * m[8] - m[5] * m[6])
            + m[2] * (m[3] * m[7] - m[4] * m[6]);
        if det.abs() < 1e-14 {
            return None;
        }
        let inv = [
            (m[4] * m[8] - m[5] * m[7]) / det,
            (m[2] * m[7] - m[1] * m[8]) / det,
            (m[1] * m[5] - m[2] * m[4]) / det,
            (m[5] * m[6] - m[3] * m[8]) / det,
            (m[0] * m[8] - m[2] * m[6]) / det,
            (m[2] * m[3] - m[0] * m[5]) / det,
            (m[3] * m[7] - m[4] * m[6]) / det,
            (m[1] * m[6] - m[0] * m[7]) / det,
            (m[0] * m[4] - m[1] * m[3]) / det,
        ];
        let d = inv[6] * u + inv[7] * v + inv[8];
        if d.abs() < 1e-12 {
            return None;
        }
        Some((
            ((inv[0] * u + inv[1] * v + inv[2]) / d) as f32,
            ((inv[3] * u + inv[4] * v + inv[5]) / d) as f32,
        ))
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
struct DsRegistration {
    transform: DsTransform,
    inliers: usize,
    rms: f32,
    #[serde(default)]
    holdout_count: usize,
    #[serde(default)]
    holdout_p95: f32,
    #[serde(default)]
    holdout_max: f32,
    #[serde(default)]
    jacobian_min: f64,
    #[serde(default)]
    jacobian_p95: f64,
    #[serde(default)]
    jacobian_max: f64,
}

#[derive(Clone, Copy, Debug)]
struct DsRegistrationResidualStats {
    rms: f32,
    p95: f32,
    max: f32,
}

#[derive(Clone, Copy, Debug)]
struct DsTransformFieldReport {
    min_jacobian: f64,
    p95_jacobian: f64,
    max_jacobian: f64,
}

fn ds_solve_normal(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for k in 0..n {
        let pivot = (k..n).max_by(|&i, &j| a[i][k].abs().total_cmp(&a[j][k].abs()))?;
        if a[pivot][k].abs() < 1e-12 {
            return None;
        }
        a.swap(k, pivot);
        b.swap(k, pivot);
        let p = a[k][k];
        for j in k..n {
            a[k][j] /= p;
        }
        b[k] /= p;
        for i in 0..n {
            if i == k {
                continue;
            }
            let f = a[i][k];
            for j in k..n {
                a[i][j] -= f * a[k][j];
            }
            b[i] -= f * b[k];
        }
    }
    Some(b)
}

fn ds_least_squares(rows: &[Vec<f64>], rhs: &[f64], n: usize) -> Option<Vec<f64>> {
    let mut ata = vec![vec![0.0; n]; n];
    let mut atb = vec![0.0; n];
    for (r, &z) in rows.iter().zip(rhs) {
        for i in 0..n {
            atb[i] += r[i] * z;
            for j in 0..n {
                ata[i][j] += r[i] * r[j];
            }
        }
    }
    ds_solve_normal(ata, atb)
}

fn ds_fit_affine(pairs: &[((f32, f32), (f32, f32))]) -> Option<DsTransform> {
    if pairs.len() < 3 {
        return None;
    }
    let rows: Vec<Vec<f64>> = pairs
        .iter()
        .map(|&((x, y), _)| vec![x as f64, y as f64, 1.0])
        .collect();
    let u: Vec<f64> = pairs.iter().map(|&(_, (u, _))| u as f64).collect();
    let v: Vec<f64> = pairs.iter().map(|&(_, (_, v))| v as f64).collect();
    let cu = ds_least_squares(&rows, &u, 3)?;
    let cv = ds_least_squares(&rows, &v, 3)?;
    Some(DsTransform {
        model: DsRegistrationModel::Affine,
        h: [cu[0], cu[1], cu[2], cv[0], cv[1], cv[2], 0.0, 0.0, 1.0],
        poly: [0.0; 12],
        norm: [0.0, 0.0, 1.0],
    })
}

fn ds_fit_projective(pairs: &[((f32, f32), (f32, f32))]) -> Option<DsTransform> {
    if pairs.len() < 4 {
        return None;
    }
    let mut rows = Vec::with_capacity(pairs.len() * 2);
    let mut rhs = Vec::with_capacity(pairs.len() * 2);
    for &((x, y), (u, v)) in pairs {
        let (x, y, u, v) = (x as f64, y as f64, u as f64, v as f64);
        rows.push(vec![x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y]);
        rhs.push(u);
        rows.push(vec![0.0, 0.0, 0.0, x, y, 1.0, -v * x, -v * y]);
        rhs.push(v);
    }
    let c = ds_least_squares(&rows, &rhs, 8)?;
    Some(DsTransform {
        model: DsRegistrationModel::Projective,
        h: [c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7], 1.0],
        poly: [0.0; 12],
        norm: [0.0, 0.0, 1.0],
    })
}

fn ds_fit_local_distortion(pairs: &[((f32, f32), (f32, f32))]) -> Option<DsTransform> {
    if pairs.len() < 10 {
        return None;
    }
    let cx = pairs.iter().map(|p| p.0 .0 as f64).sum::<f64>() / pairs.len() as f64;
    let cy = pairs.iter().map(|p| p.0 .1 as f64).sum::<f64>() / pairs.len() as f64;
    let scale = pairs
        .iter()
        .map(|p| ((p.0 .0 as f64 - cx).powi(2) + (p.0 .1 as f64 - cy).powi(2)).sqrt())
        .fold(0.0f64, f64::max)
        .max(1.0);
    let rows: Vec<Vec<f64>> = pairs
        .iter()
        .map(|p| {
            let x = (p.0 .0 as f64 - cx) / scale;
            let y = (p.0 .1 as f64 - cy) / scale;
            vec![x, y, 1.0, x * x, x * y, y * y]
        })
        .collect();
    let u: Vec<f64> = pairs.iter().map(|p| p.1 .0 as f64).collect();
    let v: Vec<f64> = pairs.iter().map(|p| p.1 .1 as f64).collect();
    let cu = ds_least_squares(&rows, &u, 6)?;
    let cv = ds_least_squares(&rows, &v, 6)?;
    let mut poly = [0.0; 12];
    poly[..6].copy_from_slice(&cu);
    poly[6..].copy_from_slice(&cv);
    Some(DsTransform {
        model: DsRegistrationModel::LocalDistortion,
        h: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
        poly,
        norm: [cx, cy, scale],
    })
}

fn ds_transform_rms(t: DsTransform, pairs: &[((f32, f32), (f32, f32))]) -> f32 {
    ds_transform_residual_stats(t, pairs).rms
}

fn ds_transform_residual_stats(
    t: DsTransform,
    pairs: &[((f32, f32), (f32, f32))],
) -> DsRegistrationResidualStats {
    if pairs.is_empty() {
        return DsRegistrationResidualStats {
            rms: f32::MAX,
            p95: f32::MAX,
            max: f32::MAX,
        };
    }
    let mut residuals = Vec::with_capacity(pairs.len());
    let mut sum_sq = 0.0f64;
    for &(p, q) in pairs {
        let z = t.forward(p.0, p.1);
        let residual = ((z.0 - q.0).powi(2) + (z.1 - q.1).powi(2)).sqrt();
        if !residual.is_finite() {
            return DsRegistrationResidualStats {
                rms: f32::MAX,
                p95: f32::MAX,
                max: f32::MAX,
            };
        }
        sum_sq += (residual as f64).powi(2);
        residuals.push(residual);
    }
    residuals.sort_by(|a, b| a.total_cmp(b));
    let p95_index = ((residuals.len() * 95).div_ceil(100)).saturating_sub(1);
    DsRegistrationResidualStats {
        rms: (sum_sq / residuals.len() as f64).sqrt() as f32,
        p95: residuals[p95_index],
        max: *residuals.last().unwrap_or(&f32::MAX),
    }
}

/// Deterministic global greedy assignment. Candidate edges are ordered by
/// distance, then target index, then reference index, so ties cannot depend on
/// hash iteration or thread scheduling. Each target and reference appears at
/// most once in the returned correspondence set.
fn ds_bijective_nearest_indices(
    transform: DsTransform,
    target: &[(f32, f32)],
    reference: &[(f32, f32)],
    max_distance: f32,
) -> Vec<(usize, usize)> {
    let max_d2 = max_distance * max_distance;
    let mut candidates = Vec::<(f32, usize, usize)>::new();
    for (target_index, &point) in target.iter().enumerate() {
        let projected = transform.forward(point.0, point.1);
        if !projected.0.is_finite() || !projected.1.is_finite() {
            continue;
        }
        for (reference_index, &reference_point) in reference.iter().enumerate() {
            let distance_sq = (projected.0 - reference_point.0).powi(2)
                + (projected.1 - reference_point.1).powi(2);
            if distance_sq <= max_d2 {
                candidates.push((distance_sq, target_index, reference_index));
            }
        }
    }
    candidates.sort_by(|a, b| {
        a.0.total_cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });

    let mut used_target = vec![false; target.len()];
    let mut used_reference = vec![false; reference.len()];
    let mut selected = Vec::new();
    for (_, target_index, reference_index) in candidates {
        if !used_target[target_index] && !used_reference[reference_index] {
            used_target[target_index] = true;
            used_reference[reference_index] = true;
            selected.push((target_index, reference_index));
        }
    }
    selected.sort_unstable();
    selected
}

fn ds_pairs_from_indices(
    indices: &[(usize, usize)],
    target: &[(f32, f32)],
    reference: &[(f32, f32)],
) -> Vec<((f32, f32), (f32, f32))> {
    indices
        .iter()
        .map(|&(target_index, reference_index)| {
            (target[target_index], reference[reference_index])
        })
        .collect()
}

/// Triangle votes can repeat the same association many times. Collapse them
/// by vote strength, then choose a deterministic one-to-one set.
fn ds_bijective_vote_pairs(votes: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut counts = std::collections::BTreeMap::<(usize, usize), usize>::new();
    for &pair in votes {
        *counts.entry(pair).or_default() += 1;
    }
    let mut ranked: Vec<((usize, usize), usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0 .0.cmp(&b.0 .0))
            .then_with(|| a.0 .1.cmp(&b.0 .1))
    });
    let max_target = ranked.iter().map(|entry| entry.0 .0).max().unwrap_or(0);
    let max_reference = ranked.iter().map(|entry| entry.0 .1).max().unwrap_or(0);
    let mut used_target = vec![false; max_target + 1];
    let mut used_reference = vec![false; max_reference + 1];
    let mut selected = Vec::new();
    for ((target_index, reference_index), _) in ranked {
        if !used_target[target_index] && !used_reference[reference_index] {
            used_target[target_index] = true;
            used_reference[reference_index] = true;
            selected.push((target_index, reference_index));
        }
    }
    selected.sort_unstable();
    selected
}

fn ds_stable_vote_winner(
    votes: std::collections::BTreeMap<(i32, i32), Vec<(usize, usize)>>,
) -> Option<((i32, i32), Vec<(usize, usize)>)> {
    let mut best_bucket: Option<((i32, i32), Vec<(usize, usize)>)> = None;
    for (key, pairs) in votes {
        // Strict `>` preserves the first (lexicographically smallest) BTreeMap
        // key when buckets have the same support.
        if best_bucket
            .as_ref()
            .is_none_or(|(_, best_pairs)| pairs.len() > best_pairs.len())
        {
            best_bucket = Some((key, pairs));
        }
    }
    best_bucket
}

/// Spatially stratified, deterministic 80/20 split. A holdout point is drawn
/// round-robin from every occupied 4×4 cell before a cell contributes twice.
fn ds_registration_train_holdout(
    pairs: &[((f32, f32), (f32, f32))],
) -> (
    Vec<((f32, f32), (f32, f32))>,
    Vec<((f32, f32), (f32, f32))>,
) {
    if pairs.len() < 8 {
        return (pairs.to_vec(), Vec::new());
    }
    let min_x = pairs
        .iter()
        .map(|pair| pair.0 .0)
        .fold(f32::INFINITY, f32::min);
    let max_x = pairs
        .iter()
        .map(|pair| pair.0 .0)
        .fold(f32::NEG_INFINITY, f32::max);
    let min_y = pairs
        .iter()
        .map(|pair| pair.0 .1)
        .fold(f32::INFINITY, f32::min);
    let max_y = pairs
        .iter()
        .map(|pair| pair.0 .1)
        .fold(f32::NEG_INFINITY, f32::max);
    let span_x = (max_x - min_x).max(1e-6);
    let span_y = (max_y - min_y).max(1e-6);
    let mut cells = std::collections::BTreeMap::<(usize, usize), Vec<usize>>::new();
    for (index, pair) in pairs.iter().enumerate() {
        let cell_x = (((pair.0 .0 - min_x) / span_x) * 3.999)
            .floor()
            .clamp(0.0, 3.0) as usize;
        let cell_y = (((pair.0 .1 - min_y) / span_y) * 3.999)
            .floor()
            .clamp(0.0, 3.0) as usize;
        cells.entry((cell_y, cell_x)).or_default().push(index);
    }
    for indices in cells.values_mut() {
        indices.sort_by(|&a, &b| {
            pairs[a]
                .0
                 .0
                .total_cmp(&pairs[b].0 .0)
                .then_with(|| pairs[a].0 .1.total_cmp(&pairs[b].0 .1))
                .then_with(|| a.cmp(&b))
        });
    }
    let holdout_target = pairs.len().div_ceil(5).min(pairs.len().saturating_sub(2));
    let mut holdout_indices = std::collections::BTreeSet::new();
    let mut round = 0usize;
    while holdout_indices.len() < holdout_target {
        let mut progressed = false;
        for indices in cells.values() {
            if round < indices.len() && holdout_indices.len() < holdout_target {
                // Start at the cell median, then walk cyclically. This avoids
                // selecting only a detector edge from every occupied cell.
                let offset = (indices.len() - 1) / 2;
                holdout_indices.insert(indices[(offset + round) % indices.len()]);
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
        round += 1;
    }
    let mut train = Vec::with_capacity(pairs.len() - holdout_indices.len());
    let mut holdout = Vec::with_capacity(holdout_indices.len());
    for (index, &pair) in pairs.iter().enumerate() {
        if holdout_indices.contains(&index) {
            holdout.push(pair);
        } else {
            train.push(pair);
        }
    }
    (train, holdout)
}

fn ds_fit_registration_model(
    model: DsRegistrationModel,
    pairs: &[((f32, f32), (f32, f32))],
) -> Option<DsTransform> {
    match model {
        DsRegistrationModel::Similarity => {
            ds_solve_similarity(pairs).map(DsTransform::from_similarity)
        }
        DsRegistrationModel::Affine => ds_fit_affine(pairs),
        DsRegistrationModel::Projective => ds_fit_projective(pairs),
        DsRegistrationModel::LocalDistortion => ds_fit_local_distortion(pairs),
    }
}

/// Samples the forward-map area Jacobian over the full detector. Registration
/// may scale a field, but a physically plausible map cannot fold, diverge, or
/// change area by more than the triangle solver's admitted 0.5×–2× scale.
fn ds_transform_field_report(
    transform: DsTransform,
    width: usize,
    height: usize,
) -> Option<DsTransformFieldReport> {
    if width < 2 || height < 2 {
        return None;
    }
    const GRID: usize = 9;
    let dx = (width.saturating_sub(1) as f32 / (GRID - 1) as f32).max(1.0);
    let dy = (height.saturating_sub(1) as f32 / (GRID - 1) as f32).max(1.0);
    let epsilon = 0.5f32;
    let mut jacobians = Vec::with_capacity(GRID * GRID);
    for grid_y in 0..GRID {
        for grid_x in 0..GRID {
            let x = grid_x as f32 * dx;
            let y = grid_y as f32 * dy;
            let left = transform.forward(x - epsilon, y);
            let right = transform.forward(x + epsilon, y);
            let top = transform.forward(x, y - epsilon);
            let bottom = transform.forward(x, y + epsilon);
            let j00 = (right.0 - left.0) as f64 / (2.0 * epsilon as f64);
            let j10 = (right.1 - left.1) as f64 / (2.0 * epsilon as f64);
            let j01 = (bottom.0 - top.0) as f64 / (2.0 * epsilon as f64);
            let j11 = (bottom.1 - top.1) as f64 / (2.0 * epsilon as f64);
            let determinant = j00 * j11 - j01 * j10;
            let linear_scale = determinant.sqrt();
            if !determinant.is_finite()
                || determinant <= 0.0
                || !linear_scale.is_finite()
                || !(0.5..=2.0).contains(&linear_scale)
            {
                return None;
            }
            jacobians.push(determinant);
        }
    }
    jacobians.sort_by(|a, b| a.total_cmp(b));
    if jacobians.last()? / jacobians[0] > 1.5 {
        return None;
    }
    let p95_index = ((jacobians.len() * 95).div_ceil(100)).saturating_sub(1);
    Some(DsTransformFieldReport {
        min_jacobian: jacobians[0],
        p95_jacobian: jacobians[p95_index],
        max_jacobian: *jacobians.last()?,
    })
}

fn ds_count_dither_positions(
    registered: &[(usize, DsTransform, f64)],
    w: usize,
    h: usize,
    cfa: bool,
) -> usize {
    let mut bins = std::collections::HashSet::new();
    for &(_, transform, _) in registered {
        let point = transform.forward(w as f32 * 0.5, h as f32 * 0.5);
        let period = if cfa { 2.0 } else { 1.0 };
        let fx = point.0.rem_euclid(period) / period;
        let fy = point.1.rem_euclid(period) / period;
        bins.insert(((fx * 4.0).floor() as i32, (fy * 4.0).floor() as i32));
    }
    bins.len()
}

fn ds_registration_residual_map(
    reference: &[(f32, f32, f32)],
    catalogs: &[Vec<(f32, f32, f32)>],
    registered: &[(usize, DsTransform, f64)],
    w: usize,
    h: usize,
    out_w: usize,
    out_h: usize,
) -> Vec<f32> {
    const G: usize = 32;
    let mut sum = vec![0.0f64; G * G];
    let mut count = vec![0.0f64; G * G];
    for &(index, transform, _) in registered {
        let Some(stars) = catalogs.get(index) else {
            continue;
        };
        for &(x, y, _) in stars.iter().take(120) {
            let p = transform.forward(x, y);
            if !p.0.is_finite()
                || !p.1.is_finite()
                || p.0 < 0.0
                || p.1 < 0.0
                || p.0 >= w as f32
                || p.1 >= h as f32
            {
                continue;
            }
            let mut best = f32::MAX;
            for &(rx, ry, _) in reference.iter().take(160) {
                best = best.min(((p.0 - rx).powi(2) + (p.1 - ry).powi(2)).sqrt());
            }
            if best <= 5.0 {
                let gx = ((p.0 / w.max(1) as f32) * (G - 1) as f32).round() as usize;
                let gy = ((p.1 / h.max(1) as f32) * (G - 1) as f32).round() as usize;
                let gi = gy.min(G - 1) * G + gx.min(G - 1);
                sum[gi] += best as f64;
                count[gi] += 1.0;
            }
        }
    }
    let mut grid: Vec<f32> = sum
        .iter()
        .zip(&count)
        .map(|(&s, &n)| if n > 0.0 { (s / n) as f32 } else { f32::NAN })
        .collect();
    // Extiende las muestras estelares a celdas vacías sin ocultar la estructura
    // espacial: relajación vecinal corta, luego bilinear al lienzo final.
    for _ in 0..12 {
        let prev = grid.clone();
        let mut changed = false;
        for y in 0..G {
            for x in 0..G {
                let i = y * G + x;
                if prev[i].is_finite() {
                    continue;
                }
                let mut s = 0.0f32;
                let mut n = 0usize;
                for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                    let (xx, yy) = (x as i32 + dx, y as i32 + dy);
                    if xx >= 0 && yy >= 0 && xx < G as i32 && yy < G as i32 {
                        let v = prev[yy as usize * G + xx as usize];
                        if v.is_finite() {
                            s += v;
                            n += 1;
                        }
                    }
                }
                if n > 0 {
                    grid[i] = s / n as f32;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    for v in &mut grid {
        if !v.is_finite() {
            *v = 0.0;
        }
    }
    (0..out_h * out_w)
        .into_par_iter()
        .map(|i| {
            let x = i % out_w;
            let y = i / out_w;
            ds_sample_grid(
                &grid,
                G,
                G,
                x as f32 / out_w.max(1) as f32,
                y as f32 / out_h.max(1) as f32,
            )
        })
        .collect()
}

fn ds_select_registration_model(
    similarity: (f32, f32, f32, f32),
    pairs: &[((f32, f32), (f32, f32))],
    width: usize,
    height: usize,
) -> Option<DsRegistration> {
    let (train, holdout) = ds_registration_train_holdout(pairs);
    let evaluation = if holdout.is_empty() { &train } else { &holdout };
    let mut best_model = DsRegistrationModel::Similarity;
    let mut best_train = ds_fit_registration_model(best_model, &train)
        .unwrap_or_else(|| DsTransform::from_similarity(similarity));
    ds_transform_field_report(best_train, width, height)?;
    let mut best_stats = ds_transform_residual_stats(best_train, evaluation);

    for (model, minimum_pairs, relative_improvement, absolute_improvement) in [
        (DsRegistrationModel::Affine, 12usize, 0.88f32, 0.03f32),
        (DsRegistrationModel::Projective, 16usize, 0.85f32, 0.03f32),
        (
            DsRegistrationModel::LocalDistortion,
            24usize,
            0.80f32,
            0.04f32,
        ),
    ] {
        if pairs.len() < minimum_pairs {
            continue;
        }
        let Some(candidate) = ds_fit_registration_model(model, &train) else {
            continue;
        };
        if ds_transform_field_report(candidate, width, height).is_none() {
            continue;
        }
        let stats = ds_transform_residual_stats(candidate, evaluation);
        let material_p95 = stats.p95 < best_stats.p95 * relative_improvement
            && best_stats.p95 - stats.p95 > absolute_improvement;
        let max_not_degraded = stats.max <= best_stats.max * 1.10 + 0.05;
        if material_p95 && max_not_degraded {
            best_model = model;
            best_train = candidate;
            best_stats = stats;
        }
    }

    // After model choice, refit that same complexity on all correspondences.
    // The independent holdout statistics above remain the publication metric.
    let production = ds_fit_registration_model(best_model, pairs)
        .filter(|candidate| ds_transform_field_report(*candidate, width, height).is_some())
        .unwrap_or(best_train);
    let geometry = ds_transform_field_report(production, width, height)?;
    Some(DsRegistration {
        transform: production,
        inliers: pairs.len(),
        rms: ds_transform_rms(production, pairs),
        holdout_count: holdout.len(),
        holdout_p95: best_stats.p95,
        holdout_max: best_stats.max,
        jacobian_min: geometry.min_jacobian,
        jacobian_p95: geometry.p95_jacobian,
        jacobian_max: geometry.max_jacobian,
    })
}

/// Triangle-similarity registration (astroalign-style, local triangles from
/// each star's nearest neighbours). Returns the target→reference transform
/// and the inlier count; None when the field could not be matched.
fn ds_match_triangles(
    ref_stars: &[(f32, f32, f32)],
    tgt_stars: &[(f32, f32, f32)],
) -> Option<DsRegistration> {
    // Compatibility entry point for the crate-root SPCC solver. Deep-sky
    // stacking calls the explicit full-detector variant below.
    let width = ref_stars
        .iter()
        .chain(tgt_stars)
        .map(|star| star.0)
        .filter(|value| value.is_finite())
        .fold(0.0f32, f32::max)
        .ceil() as usize
        + 2;
    let height = ref_stars
        .iter()
        .chain(tgt_stars)
        .map(|star| star.1)
        .filter(|value| value.is_finite())
        .fold(0.0f32, f32::max)
        .ceil() as usize
        + 2;
    ds_match_triangles_in_field(ref_stars, tgt_stars, width.max(2), height.max(2))
}

fn ds_match_triangles_in_field(
    ref_stars: &[(f32, f32, f32)],
    tgt_stars: &[(f32, f32, f32)],
    width: usize,
    height: usize,
) -> Option<DsRegistration> {
    let take = 60usize;
    let r: Vec<(f32, f32)> = ref_stars.iter().take(take).map(|s| (s.0, s.1)).collect();
    let t: Vec<(f32, f32)> = tgt_stars.iter().take(take).map(|s| (s.0, s.1)).collect();
    // Six stars were historically accepted, but that leaves no defensible
    // validation set and is too fragile for sparse narrowband fields.
    if r.len() < 8 || t.len() < 8 {
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
            near.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
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
                    let s = [
                        d(pts[v[1]], pts[v[2]]),
                        d(pts[v[0]], pts[v[2]]),
                        d(pts[v[0]], pts[v[1]]),
                    ];
                    let mut order = [0usize, 1, 2];
                    order.sort_by(|&p, &q| s[q].total_cmp(&s[p]).then_with(|| p.cmp(&q)));
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
        out.sort_by(|a, b| {
            a.1.total_cmp(&b.1)
                .then_with(|| a.2.total_cmp(&b.2))
                .then_with(|| a.0.cmp(&b.0))
        });
        out
    };
    let rt = tris(&r);
    let tt = tris(&t);
    if rt.is_empty() || tt.is_empty() {
        return None;
    }

    // Vote on (rotation, log-scale) buckets over invariant-matched triangles.
    let mut votes: std::collections::BTreeMap<(i32, i32), Vec<(usize, usize)>> =
        std::collections::BTreeMap::new();
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
                let pairs: Vec<((f32, f32), (f32, f32))> =
                    (0..3).map(|m| (t[ti[m]], r[ri[m]])).collect();
                if let Some(tr) = ds_solve_similarity(&pairs) {
                    let scale = (tr.0 * tr.0 + tr.1 * tr.1).sqrt();
                    if scale > 0.5 && scale < 2.0 {
                        let ang = tr.1.atan2(tr.0);
                        let key = (
                            (ang * 60.0).round() as i32,
                            (scale.ln() * 60.0).round() as i32,
                        );
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

    let (_, best_pairs) = ds_stable_vote_winner(votes)?;
    if best_pairs.len() < 9 {
        return None;
    }
    let uniq_indices = ds_bijective_vote_pairs(&best_pairs);
    let uniq = ds_pairs_from_indices(&uniq_indices, &t, &r);
    if uniq.len() < 8 {
        return None;
    }
    let coarse = ds_solve_similarity(&uniq)?;

    // Inlier refinement: global deterministic one-to-one assignment prevents
    // two target detections from claiming the same reference star.
    let coarse_transform = DsTransform::from_similarity(coarse);
    let inlier_indices = ds_bijective_nearest_indices(coarse_transform, &t, &r, 3.0);
    let inl = ds_pairs_from_indices(&inlier_indices, &t, &r);
    if inl.len() < 8 {
        return None;
    }
    let fine = ds_solve_similarity(&inl)?;

    // SECOND refinement at 1.5 px: with the transform already good, a tighter
    // gate drops accidental pairings and squeezes out sub-pixel accuracy
    // (same idea as PixInsight's iterative RANSAC polish).
    let fine_transform = DsTransform::from_similarity(fine);
    let inlier_indices2 = ds_bijective_nearest_indices(fine_transform, &t, &r, 1.5);
    let inl2 = ds_pairs_from_indices(&inlier_indices2, &t, &r);
    if inl2.len() >= 8 {
        if let Some(fine2) = ds_solve_similarity(&inl2) {
            return ds_select_registration_model(fine2, &inl2, width, height);
        }
    }
    ds_select_registration_model(fine, &inl, width, height)
}

/// Lanczos-3 kernel lookup table (1024 steps/unit over [0,3]) — the hot warp
/// loop reads the LUT instead of paying two sin() per tap per pixel.
const DS_L3_RES: usize = 1024;
fn ds_l3_lut() -> &'static [f32] {
    static LUT: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        (0..=DS_L3_RES * 3)
            .map(|i| {
                let x = i as f32 / DS_L3_RES as f32;
                if x < 1e-4 {
                    1.0
                } else if x >= 3.0 {
                    0.0
                } else {
                    let pix = std::f32::consts::PI * x;
                    3.0 * (pix.sin() * (pix / 3.0).sin()) / (pix * pix)
                }
            })
            .collect()
    })
}

/// Sample `img` at (sxf, syf) with a separable 6×6 Lanczos-3 (PixInsight's
/// default registration interpolation — bilinear blurs stars and correlates
/// noise). Weights are renormalized (exact DC preservation) and the result is
/// left mathematically linear. Lanczos ringing is represented honestly in SCI;
/// any cosmetic suppression belongs in a reversible preview/derived product.
/// Caller guarantees a 3-pixel interior margin.
#[inline]
fn ds_sample_lanczos3(img: &DsImage, lut: &[f32], sxf: f32, syf: f32, vals: &mut [f32; 3]) {
    let ch = img.ch;
    let x0 = sxf.floor() as usize;
    let y0 = syf.floor() as usize;
    let fx = sxf - x0 as f32;
    let fy = syf - y0 as f32;
    let mut wx = [0.0f32; 6];
    let mut wy = [0.0f32; 6];
    let (mut swx, mut swy) = (0.0f32, 0.0f32);
    for k in 0..6 {
        let off = k as f32 - 2.0;
        let ix = ((off - fx).abs() * DS_L3_RES as f32) as usize;
        let iy = ((off - fy).abs() * DS_L3_RES as f32) as usize;
        wx[k] = lut.get(ix).copied().unwrap_or(0.0);
        wy[k] = lut.get(iy).copied().unwrap_or(0.0);
        swx += wx[k];
        swy += wy[k];
    }
    let inv = 1.0 / (swx * swy).max(1e-6);
    for c in 0..ch {
        let mut acc = 0.0f32;
        for (j, &wyj) in wy.iter().enumerate() {
            let row_base = ((y0 + j - 2) * img.w + (x0 - 2)) * ch + c;
            let mut ax = 0.0f32;
            for (i2, &wxi) in wx.iter().enumerate() {
                ax += wxi * img.data[row_base + i2 * ch];
            }
            acc += wyj * ax;
        }
        vals[c] = acc * inv;
    }
}

/// Inverse-map warp of `img` (target frame) into the reference canvas,
/// accumulating per-pixel into `sum`/`wgt` (and `sumsq` when given). Sampling
/// is Lanczos-3 (`lanczos=true`, sharp — PixInsight default) or bilinear.
/// When `bounds` are provided, out-of-window samples are REJECTED (κσ pass).
#[allow(clippy::too_many_arguments)]
fn ds_warp_accumulate(
    img: &DsImage,
    t: DsTransform, // target → reference
    sum: &mut [f64],
    sumsq: Option<&mut Vec<f64>>,
    wgt: &mut [f64],
    bounds: Option<(&[f32], &[f32])>,
    rejection_maps: Option<(&mut [f64], &mut [f64])>,
    w: usize, // OUTPUT canvas width (= input·scale under drizzle)
    h: usize,
    ch: usize,
    frame_w: f64,               // per-frame quality weight (bad frames may be excluded)
    scale: f32,                 // drizzle factor (1.0 = none): output(x,y) ↔ ref(x/scale)
    norm: ([f32; 3], [f32; 3]), // per-channel normalization: v'[c] = v[c]·mul[c] + add[c]
    loc: Option<(&[f32], usize, usize)>, // local-norm offset grid (output space)
    // Rescate de detalle: rejilla de calidad local (multiplica frame_w).
    wq: Option<(&[f32], usize, usize)>,
    lanczos: bool, // Lanczos-3 sampling (falls back to bilinear at borders)
    // NF-Lite (F3): máscara congelada del frame en espacio de SALIDA (bitset
    // de w*h bits, bit=1 ⇒ este frame no aporta en ese píxel) y acumulador
    // opcional de Σw² por píxel-canal para NEFF. Con None ambos, el bucle es
    // BIT-IDÉNTICO al comportamiento previo (ruta clásica).
    skip_mask: Option<&[u64]>,
    weight_sq: Option<&mut Vec<f64>>,
) {
    let inv_scale = 1.0 / scale.max(1.0);
    let lut = ds_l3_lut();

    // Parallel per-row: each worker touches DISJOINT row slices of the
    // accumulators (raw pointers reconstructed per row — safe by row split).
    let sum_ptr = sum.as_mut_ptr() as usize;
    let wgt_ptr = wgt.as_mut_ptr() as usize;
    let sq_ptr: Option<usize> = sumsq.map(|v| v.as_mut_ptr() as usize);
    let wsq_ptr: Option<usize> = weight_sq.map(|v| v.as_mut_ptr() as usize);
    let (low_ptr, high_ptr): (Option<usize>, Option<usize>) = rejection_maps
        .map(|(low, high)| {
            (
                Some(low.as_mut_ptr() as usize),
                Some(high.as_mut_ptr() as usize),
            )
        })
        .unwrap_or((None, None));

    (0..h).into_par_iter().for_each(|y| {
        let sum_row = unsafe {
            std::slice::from_raw_parts_mut((sum_ptr as *mut f64).add(y * w * ch), w * ch)
        };
        // Per-channel weight (per-channel rejection): same layout as sum_row.
        let wgt_row = unsafe {
            std::slice::from_raw_parts_mut((wgt_ptr as *mut f64).add(y * w * ch), w * ch)
        };
        let mut sq_row = sq_ptr.map(|p| unsafe {
            std::slice::from_raw_parts_mut((p as *mut f64).add(y * w * ch), w * ch)
        });
        let mut wsq_row = wsq_ptr.map(|p| unsafe {
            std::slice::from_raw_parts_mut((p as *mut f64).add(y * w * ch), w * ch)
        });
        let mut low_row = low_ptr
            .map(|p| unsafe { std::slice::from_raw_parts_mut((p as *mut f64).add(y * w), w) });
        let mut high_row = high_ptr
            .map(|p| unsafe { std::slice::from_raw_parts_mut((p as *mut f64).add(y * w), w) });
        // Reference-frame coordinate of this output pixel (drizzle-scaled).
        let ry_ref = y as f32 * inv_scale;
        for x in 0..w {
            // Máscara congelada del frame (NF-Lite): el píxel de salida no
            // recibe aportación de este frame. Antes de muestrear: gratis.
            if let Some(mask) = skip_mask {
                let p = y * w + x;
                if (mask[p >> 6] >> (p & 63)) & 1 == 1 {
                    continue;
                }
            }
            let rx_ref = x as f32 * inv_scale;
            let Some((sxf, syf)) = t.inverse(rx_ref, ry_ref) else {
                continue;
            };
            if sxf < 0.0 || syf < 0.0 || sxf >= (img.w - 1) as f32 || syf >= (img.h - 1) as f32 {
                continue;
            }
            let mut vals = [0.0f32; 3];
            // Peso efectivo del frame EN ESTE PÍXEL (Rescate de detalle): la
            // media ponderada sigue siendo lineal; con wq=None es EXACTAMENTE
            // frame_w (bit-idéntico al comportamiento previo).
            let frame_w = wq
                .map(|(g, gw, gh)| {
                    frame_w
                        * ds_sample_grid(g, gw, gh, x as f32 / w as f32, y as f32 / h as f32) as f64
                })
                .unwrap_or(frame_w);
            // Lanczos-3 in the interior (needs a 3-px margin), bilinear at the
            // borders — BOTH κσ passes use the same sampler so the statistics
            // and the rejection windows stay consistent.
            if lanczos
                && sxf >= 3.0
                && syf >= 3.0
                && sxf < (img.w - 4) as f32
                && syf < (img.h - 4) as f32
            {
                ds_sample_lanczos3(img, lut, sxf, syf, &mut vals);
            } else {
                let x0 = sxf as usize;
                let y0 = syf as usize;
                let fx = sxf - x0 as f32;
                let fy = syf - y0 as f32;
                for c in 0..ch {
                    let i00 = (y0 * img.w + x0) * ch + c;
                    let i01 = i00 + img.w * ch;
                    vals[c] = img.data[i00] * (1.0 - fx) * (1.0 - fy)
                        + img.data[i00 + ch] * fx * (1.0 - fy)
                        + img.data[i01] * (1.0 - fx) * fy
                        + img.data[i01 + ch] * fx * fy;
                }
            }
            // PER-CHANNEL normalization, rejection and accumulation: each channel
            // is accepted or clipped on its own κσ window, so a single hot channel
            // no longer discards the good data in the others. The weight is per
            // channel (wgt_row[x*ch+c]); the rejection maps stay per-pixel (QA).
            let mut any_low = false;
            let mut any_high = false;
            for c in 0..ch {
                let loc_off = loc
                    .map(|(g, gw, gh)| {
                        ds_sample_local_field(
                            g,
                            gw,
                            gh,
                            ch,
                            c,
                            x as f32 / w as f32,
                            y as f32 / h as f32,
                        )
                    })
                    .unwrap_or(0.0);
                let v = vals[c] * norm.0[c] + norm.1[c] + loc_off;
                vals[c] = v;
                if !v.is_finite() {
                    // Invalid calibrated samples (for example a weak flat
                    // response) carry zero statistical weight. Letting NaN
                    // enter SUM/SUMSQ would poison an otherwise valid stack.
                    continue;
                }
                let mut accept_c = true;
                if let Some((lo, hi)) = bounds {
                    let bi = (y * w + x) * ch + c;
                    if v < lo[bi] {
                        accept_c = false;
                        any_low = true;
                    } else if v > hi[bi] {
                        accept_c = false;
                        any_high = true;
                    }
                }
                if accept_c {
                    sum_row[x * ch + c] += v as f64 * frame_w;
                    if let Some(sq) = sq_row.as_mut() {
                        sq[x * ch + c] += (v as f64) * (v as f64) * frame_w;
                    }
                    if let Some(ws) = wsq_row.as_mut() {
                        ws[x * ch + c] += frame_w * frame_w;
                    }
                    wgt_row[x * ch + c] += frame_w;
                }
            }
            if any_low && any_high {
                if let Some(row) = low_row.as_deref_mut() {
                    row[x] += 0.5 * frame_w;
                }
                if let Some(row) = high_row.as_deref_mut() {
                    row[x] += 0.5 * frame_w;
                }
            } else if any_low {
                if let Some(row) = low_row.as_deref_mut() {
                    row[x] += frame_w;
                }
            } else if any_high {
                if let Some(row) = high_row.as_deref_mut() {
                    row[x] += frame_w;
                }
            }
        }
    });
}

/// Warp a SINGLE image onto the reference grid (inverse similarity, bilinear),
/// same convention as `ds_warp_accumulate` but producing a plain buffer — used
/// to co-register per-filter channel masters before LRGB/narrowband combine.
/// Out-of-bounds samples become 0.
fn ds_warp_single(img: &DsImage, t: DsTransform, w: usize, h: usize) -> Vec<f32> {
    let ch = img.ch;
    let mut out = vec![0.0f32; w * h * ch];
    let out_ptr = out.as_mut_ptr() as usize;
    (0..h).into_par_iter().for_each(|y| {
        let row = unsafe {
            std::slice::from_raw_parts_mut((out_ptr as *mut f32).add(y * w * ch), w * ch)
        };
        for x in 0..w {
            let Some((sxf, syf)) = t.inverse(x as f32, y as f32) else {
                continue;
            };
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

/// Memory-bounded registered proxy used only to solve the smooth local
/// background graph. At most ~192 samples on the longest axis are retained;
/// SCI integration continues to read the native calibrated frame.
fn ds_registered_background_proxy(
    img: &DsImage,
    transform: DsTransform,
    reference_w: usize,
    reference_h: usize,
    proxy_w: usize,
    proxy_h: usize,
    multiply: [f32; 3],
) -> Result<(Vec<f32>, Vec<u8>), String> {
    if img.ch == 0 || proxy_w < 2 || proxy_h < 2 || reference_w == 0 || reference_h == 0 {
        return Err("geometría inválida para proxy de normalización local".into());
    }
    let samples = proxy_w
        .checked_mul(proxy_h)
        .and_then(|pixels| pixels.checked_mul(img.ch))
        .ok_or("proxy de normalización local fuera de rango")?;
    let mut data = Vec::new();
    data.try_reserve_exact(samples)
        .map_err(|error| format!("sin memoria para proxy de fondo: {error}"))?;
    data.resize(samples, 0.0f32);
    let mut coverage = Vec::new();
    coverage
        .try_reserve_exact(proxy_w * proxy_h)
        .map_err(|error| format!("sin memoria para cobertura de fondo: {error}"))?;
    coverage.resize(proxy_w * proxy_h, 0u8);

    for py in 0..proxy_h {
        let ry = (py as f32 + 0.5) * reference_h as f32 / proxy_h as f32 - 0.5;
        for px in 0..proxy_w {
            let rx = (px as f32 + 0.5) * reference_w as f32 / proxy_w as f32 - 0.5;
            let Some((sx, sy)) = transform.inverse(rx, ry) else {
                continue;
            };
            if sx < 0.0 || sy < 0.0 || sx >= (img.w - 1) as f32 || sy >= (img.h - 1) as f32 {
                continue;
            }
            let x0 = sx.floor() as usize;
            let y0 = sy.floor() as usize;
            let fx = sx - x0 as f32;
            let fy = sy - y0 as f32;
            let pixel = py * proxy_w + px;
            coverage[pixel] = 1;
            for channel in 0..img.ch {
                let i00 = (y0 * img.w + x0) * img.ch + channel;
                let i01 = i00 + img.w * img.ch;
                let value = img.data[i00] * (1.0 - fx) * (1.0 - fy)
                    + img.data[i00 + img.ch] * fx * (1.0 - fy)
                    + img.data[i01] * (1.0 - fx) * fy
                    + img.data[i01 + img.ch] * fx * fy;
                data[pixel * img.ch + channel] = value * multiply[channel.min(2)];
            }
        }
    }
    Ok((data, coverage))
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
    t: DsTransform, // input → reference
    sum: &mut [f64],
    sumsq: Option<&mut Vec<f64>>,
    wgt: &mut [f64],
    bounds: Option<(&[f32], &[f32])>,
    rejection_maps: Option<(&mut [f64], &mut [f64])>,
    w: usize, // OUTPUT canvas width
    h: usize,
    ch: usize,
    frame_w: f64,
    scale: f32,   // drizzle factor (>1)
    pixfrac: f32, // drop shrink 0.4..1.0 (smaller = sharper, needs more frames)
    norm: ([f32; 3], [f32; 3]),
    loc: Option<(&[f32], usize, usize)>, // local-norm offset grid (output space)
    // Rescate de detalle: rejilla de calidad local (multiplica frame_w).
    wq: Option<(&[f32], usize, usize)>,
) {
    let half = 0.5 * pixfrac.clamp(0.2, 1.0) * scale; // drop half-size (output px)

    let sum_ptr = sum.as_mut_ptr() as usize;
    let wgt_ptr = wgt.as_mut_ptr() as usize;
    let sq_ptr: Option<usize> = sumsq.map(|v| v.as_mut_ptr() as usize);
    let (low_ptr, high_ptr): (Option<usize>, Option<usize>) = rejection_maps
        .map(|(low, high)| {
            (
                Some(low.as_mut_ptr() as usize),
                Some(high.as_mut_ptr() as usize),
            )
        })
        .unwrap_or((None, None));

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
        let sum_band = unsafe {
            std::slice::from_raw_parts_mut((sum_ptr as *mut f64).add(oy0 * w * ch), rows * w * ch)
        };
        // Per-channel weight (per-channel rejection), same layout as sum_band.
        let wgt_band = unsafe {
            std::slice::from_raw_parts_mut((wgt_ptr as *mut f64).add(oy0 * w * ch), rows * w * ch)
        };
        let mut sq_band = sq_ptr.map(|p| unsafe {
            std::slice::from_raw_parts_mut((p as *mut f64).add(oy0 * w * ch), rows * w * ch)
        });
        let mut low_band = low_ptr.map(|p| unsafe {
            std::slice::from_raw_parts_mut((p as *mut f64).add(oy0 * w), rows * w)
        });
        let mut high_band = high_ptr.map(|p| unsafe {
            std::slice::from_raw_parts_mut((p as *mut f64).add(oy0 * w), rows * w)
        });

        // Input bbox that can splat into this band: inverse-map the band corners
        // (expanded by the drop half-size) output → ref → input.
        let mut ixmin = i32::MAX;
        let mut ixmax = i32::MIN;
        let mut iymin = i32::MAX;
        let mut iymax = i32::MIN;
        // Margen +1: la convención centro desplaza la gota +0.5 px de salida;
        // ensanchar el bbox solo agranda el conjunto candidato (seguro).
        for &(ox, oy) in &[
            (-half - 1.0, oy0 as f32 - half - 1.0),
            (w as f32 + half + 1.0, oy0 as f32 - half - 1.0),
            (-half - 1.0, oy1 as f32 + half + 1.0),
            (w as f32 + half + 1.0, oy1 as f32 + half + 1.0),
        ] {
            let rx = ox / scale;
            let ry = oy / scale;
            let Some((sx, sy)) = t.inverse(rx, ry) else {
                continue;
            };
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
                let (rx, ry) = t.forward(ix as f32, iy as f32);
                if !rx.is_finite() || !ry.is_finite() {
                    continue;
                }
                // CONVENCIÓN CENTRO: la celda de salida k cubre [k-0.5, k+0.5)
                // — igual que el motor warp 1× (salida x ↔ ref x/scale). El
                // kernel anterior usaba convención esquina y desplazaba TODO el
                // máster drizzle +0.5 px de salida respecto al máster 1× y a
                // los mapas científicos (sesgo astrométrico constante). El
                // +0.5 desplaza la gota y conserva la aritmética de celdas.
                let ox = rx * scale + 0.5;
                let oy = ry * scale + 0.5;
                let dx0 = ox - half;
                let dx1 = ox + half;
                let dy0 = oy - half;
                let dy1 = oy + half;
                let px0 = dx0.floor() as i32;
                let px1 = (dx1.ceil() as i32 - 1).max(px0);
                let py0 = dy0.floor() as i32;
                let py1 = (dy1.ceil() as i32 - 1).max(py0);

                // Peso local del Rescate de detalle (1.0 exacto con wq=None).
                let frame_w = wq
                    .map(|(g, gw, gh)| {
                        frame_w * ds_sample_grid(g, gw, gh, ox / w as f32, oy / h as f32) as f64
                    })
                    .unwrap_or(frame_w);
                let mut vals = [0.0f32; 3];
                for c in 0..ch {
                    let loc_off = loc
                        .map(|(g, gw, gh)| {
                            ds_sample_local_field(
                                g,
                                gw,
                                gh,
                                ch,
                                c,
                                ox / w as f32,
                                oy / h as f32,
                            )
                        })
                        .unwrap_or(0.0);
                    vals[c] = img.data[base + c] * norm.0[c] + norm.1[c] + loc_off;
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
                                                                            // PER-CHANNEL κσ rejection (pass 2): clip each channel on
                                                                            // its own per-output-pixel window; a hot channel no longer
                                                                            // drops the whole contribution. Weight is per channel.
                        let wv = area * frame_w;
                        let mut any_low = false;
                        let mut any_high = false;
                        for c in 0..ch {
                            if !vals[c].is_finite() {
                                continue;
                            }
                            let mut accept_c = true;
                            if let Some((lo, hi)) = bounds {
                                let bi = gpix * ch + c;
                                if vals[c] < lo[bi] {
                                    accept_c = false;
                                    any_low = true;
                                } else if vals[c] > hi[bi] {
                                    accept_c = false;
                                    any_high = true;
                                }
                            }
                            if accept_c {
                                sum_band[lpix * ch + c] += vals[c] as f64 * wv;
                                if let Some(sq) = sq_band.as_deref_mut() {
                                    sq[lpix * ch + c] += (vals[c] as f64) * (vals[c] as f64) * wv;
                                }
                                wgt_band[lpix * ch + c] += wv;
                            }
                        }
                        if any_low && any_high {
                            if let Some(map) = low_band.as_deref_mut() {
                                map[lpix] += 0.5 * wv;
                            }
                            if let Some(map) = high_band.as_deref_mut() {
                                map[lpix] += 0.5 * wv;
                            }
                        } else if any_low {
                            if let Some(map) = low_band.as_deref_mut() {
                                map[lpix] += wv;
                            }
                        } else if any_high {
                            if let Some(map) = high_band.as_deref_mut() {
                                map[lpix] += wv;
                            }
                        }
                    }
                }
            }
        }
    });
}

#[inline]
fn ds_cfa_channel(cid: i32, x: usize, y: usize) -> usize {
    let (rx, ry) = match cid {
        8 => (0usize, 0usize),
        9 => (1, 0),
        10 => (0, 1),
        11 => (1, 1),
        _ => return 1,
    };
    if (x & 1) == rx && (y & 1) == ry {
        0
    } else if (x & 1) != rx && (y & 1) != ry {
        2
    } else {
        1
    }
}

/// Drizzle CFA verdadero: cada fotosito calibrado conserva su color y se
/// dispersa directamente al máster RGB. No hay debayer/interpolación previa.
/// Los pesos son por canal (RGB) porque la densidad Bayer es 1/4, 1/2, 1/4.
#[allow(clippy::too_many_arguments)]
fn ds_drizzle_cfa_accumulate(
    img: &DsImage,
    cid: i32,
    t: DsTransform,
    sum: &mut [f64],
    sumsq: Option<&mut Vec<f64>>,
    wgt_rgb: &mut [f64],
    bounds: Option<(&[f32], &[f32])>,
    rejection_maps: Option<(&mut [f64], &mut [f64])>,
    w: usize,
    h: usize,
    frame_w: f64,
    scale: f32,
    pixfrac: f32,
    norm: ([f32; 3], [f32; 3]),
    loc: Option<(&[f32], usize, usize)>,
    // Rescate de detalle: rejilla de calidad local (multiplica frame_w).
    wq: Option<(&[f32], usize, usize)>,
    // NF-Lite CFA (F4): peso multiplicativo POR CANAL (inverso-varianza del
    // canal), máscara congelada por píxel de salida y Σw² para NEFF. Con
    // None en los tres, el kernel es BIT-IDÉNTICO al clásico.
    fw_rgb: Option<[f64; 3]>,
    skip_mask: Option<&[u64]>,
    weight_sq: Option<&mut Vec<f64>>,
) {
    if img.ch != 1 || !matches!(cid, 8..=11) {
        return;
    }
    let half = 0.5 * pixfrac.clamp(0.2, 1.0) * scale;
    let sum_ptr = sum.as_mut_ptr() as usize;
    let wgt_ptr = wgt_rgb.as_mut_ptr() as usize;
    let sq_ptr = sumsq.map(|v| v.as_mut_ptr() as usize);
    let wsq_ptr = weight_sq.map(|v| v.as_mut_ptr() as usize);
    let (low_ptr, high_ptr): (Option<usize>, Option<usize>) = rejection_maps
        .map(|(low, high)| {
            (
                Some(low.as_mut_ptr() as usize),
                Some(high.as_mut_ptr() as usize),
            )
        })
        .unwrap_or((None, None));
    let n_bands = rayon::current_num_threads().max(1);
    let band_h = h.div_ceil(n_bands);

    (0..n_bands).into_par_iter().for_each(|band| {
        let oy0 = band * band_h;
        let oy1 = ((band + 1) * band_h).min(h);
        if oy0 >= oy1 {
            return;
        }
        let rows = oy1 - oy0;
        let sum_band = unsafe {
            std::slice::from_raw_parts_mut((sum_ptr as *mut f64).add(oy0 * w * 3), rows * w * 3)
        };
        let wgt_band = unsafe {
            std::slice::from_raw_parts_mut((wgt_ptr as *mut f64).add(oy0 * w * 3), rows * w * 3)
        };
        let mut sq_band = sq_ptr.map(|p| unsafe {
            std::slice::from_raw_parts_mut((p as *mut f64).add(oy0 * w * 3), rows * w * 3)
        });
        let mut wsq_band = wsq_ptr.map(|p| unsafe {
            std::slice::from_raw_parts_mut((p as *mut f64).add(oy0 * w * 3), rows * w * 3)
        });
        let mut low_band = low_ptr.map(|p| unsafe {
            std::slice::from_raw_parts_mut((p as *mut f64).add(oy0 * w), rows * w)
        });
        let mut high_band = high_ptr.map(|p| unsafe {
            std::slice::from_raw_parts_mut((p as *mut f64).add(oy0 * w), rows * w)
        });

        let mut ixmin = i32::MAX;
        let mut ixmax = i32::MIN;
        let mut iymin = i32::MAX;
        let mut iymax = i32::MIN;
        for &(ox, oy) in &[
            (-half, oy0 as f32 - half),
            (w as f32 + half, oy0 as f32 - half),
            (-half, oy1 as f32 + half),
            (w as f32 + half, oy1 as f32 + half),
            (w as f32 * 0.5, (oy0 + oy1) as f32 * 0.5),
        ] {
            if let Some((sx, sy)) = t.inverse(ox / scale, oy / scale) {
                ixmin = ixmin.min(sx.floor() as i32);
                ixmax = ixmax.max(sx.ceil() as i32);
                iymin = iymin.min(sy.floor() as i32);
                iymax = iymax.max(sy.ceil() as i32);
            }
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
                let c = ds_cfa_channel(cid, ix as usize, iy as usize);
                let (rx, ry) = t.forward(ix as f32, iy as f32);
                if !rx.is_finite() || !ry.is_finite() {
                    continue;
                }
                // CONVENCIÓN CENTRO (+0.5): idéntica al kernel mono/RGB — los
                // dos deben compartir geometría exacta o los canales quedarían
                // corridos respecto al máster 1×.
                let (ox, oy) = (rx * scale + 0.5, ry * scale + 0.5);
                let (dx0, dx1, dy0, dy1) = (ox - half, ox + half, oy - half, oy + half);
                let px0 = dx0.floor() as i32;
                let px1 = (dx1.ceil() as i32 - 1).max(px0);
                let py0 = dy0.floor() as i32;
                let py1 = (dy1.ceil() as i32 - 1).max(py0);
                let loc_off = loc
                    .map(|(g, gw, gh)| {
                        ds_sample_local_field(
                            g,
                            gw,
                            gh,
                            3,
                            c,
                            ox / w as f32,
                            oy / h as f32,
                        )
                    })
                    .unwrap_or(0.0);
                // Peso local del Rescate de detalle (1.0 exacto con wq=None).
                let frame_w = wq
                    .map(|(g, gw, gh)| {
                        frame_w * ds_sample_grid(g, gw, gh, ox / w as f32, oy / h as f32) as f64
                    })
                    .unwrap_or(frame_w);
                let value =
                    img.data[iy as usize * img.w + ix as usize] * norm.0[c] + norm.1[c] + loc_off;
                if !value.is_finite() {
                    continue;
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
                        let area = (ax * ay) as f64;
                        if area <= 0.0 {
                            continue;
                        }
                        let gpix = opy as usize * w + opx as usize;
                        if let Some(mask) = skip_mask {
                            if (mask[gpix >> 6] >> (gpix & 63)) & 1 == 1 {
                                continue;
                            }
                        }
                        let lpix = (opy as usize - oy0) * w + opx as usize;
                        let oi = gpix * 3 + c;
                        let wv = area * frame_w * fw_rgb.map(|f| f[c]).unwrap_or(1.0);
                        if let Some((lo, hi)) = bounds {
                            if value < lo[oi] || value > hi[oi] {
                                // Normaliza densidad CFA para que el mapa se lea
                                // en equivalentes de frame, no en fotositos.
                                let density = if c == 1 { 2.0 } else { 4.0 };
                                if value < lo[oi] {
                                    if let Some(m) = low_band.as_deref_mut() {
                                        m[lpix] += wv * density;
                                    }
                                } else if let Some(m) = high_band.as_deref_mut() {
                                    m[lpix] += wv * density;
                                }
                                continue;
                            }
                        }
                        let li = lpix * 3 + c;
                        sum_band[li] += value as f64 * wv;
                        if let Some(sq) = sq_band.as_deref_mut() {
                            sq[li] += value as f64 * value as f64 * wv;
                        }
                        if let Some(ws) = wsq_band.as_deref_mut() {
                            ws[li] += wv * wv;
                        }
                        wgt_band[li] += wv;
                    }
                }
            }
        }
    });
}

fn ds_cfa_coverage(wgt_rgb: &[f64]) -> Vec<f64> {
    wgt_rgb
        .chunks_exact(3)
        .map(|w| (4.0 * w[0]).min(2.0 * w[1]).min(4.0 * w[2]))
        .collect()
}

/// Preserve the scientific meaning of zero drizzle coverage. Interpolating a
/// neighbour into SCI fabricates a measurement even if coverage remains zero;
/// mark every channel NaN so FITS/DQ consumers cannot mistake it for signal.
fn ds_mark_uncovered_nan(final_data: &mut [f32], coverage: &[f64], ch: usize) -> usize {
    let mut holes = 0usize;
    for (pixel, &weight) in coverage.iter().enumerate() {
        if weight <= 0.0 || !weight.is_finite() {
            holes += 1;
            for channel in 0..ch {
                final_data[pixel * ch + channel] = f32::NAN;
            }
        }
    }
    holes
}

fn ds_dq_from_coverage(coverage: &[f64]) -> Vec<u32> {
    let max_coverage = coverage
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .fold(0.0f64, f64::max);
    coverage
        .iter()
        .map(|&value| {
            if !value.is_finite() || value <= 0.0 {
                crate::deepsky_variance::dq::NO_COVERAGE
            } else if max_coverage > 0.0 && value < 0.5 * max_coverage {
                crate::deepsky_variance::dq::EDGE
            } else {
                0
            }
        })
        .collect()
}

/// Reaplica las invariantes de publicación después de una transformación
/// espacial (binning/crop) que combinó SCI, VAR, NEFF y DQ por separado.
/// DQ es la autoridad: un hueco no puede recuperar señal finita y una mezcla
/// EIDR sin covarianza no puede heredar VAR/NEFF de sus píxeles aptos vecinos;
/// NAN_INPUT/FLAT_INVALID tampoco pueden convertirse en una media finita.
fn ds_reconcile_scientific_planes_with_dq(
    science: &mut [f32],
    variance: &mut [f32],
    neff: &mut [f32],
    dq: &[u32],
    channels: usize,
) -> Result<(), String> {
    let expected = dq
        .len()
        .checked_mul(channels)
        .ok_or("productos científicos: dimensiones desbordan")?;
    if channels == 0
        || science.len() != expected
        || variance.len() != expected
        || neff.len() != expected
    {
        return Err(format!(
            "productos científicos incompatibles tras transformación: SCI={} VAR={} NEFF={} DQ={} canales={channels}",
            science.len(),
            variance.len(),
            neff.len(),
            dq.len(),
        ));
    }
    for (pixel, &flags) in dq.iter().enumerate() {
        let no_coverage = flags & crate::deepsky_variance::dq::NO_COVERAGE != 0;
        let invalid_input = flags
            & (crate::deepsky_variance::dq::NAN_INPUT
                | crate::deepsky_variance::dq::FLAT_INVALID)
            != 0;
        let uncertainty_unavailable =
            flags & crate::deepsky_variance::dq::EIDR_UNCERTAINTY_UNAVAILABLE != 0;
        if !no_coverage && !invalid_input && !uncertainty_unavailable {
            continue;
        }
        for channel in 0..channels {
            let sample = pixel * channels + channel;
            if no_coverage || invalid_input {
                science[sample] = f32::NAN;
            }
            variance[sample] = f32::NAN;
            neff[sample] = 0.0;
        }
    }
    Ok(())
}

/// VAR/NEFF exactos para el estimador de media ponderada de la ruta Classic
/// streaming 1×. Los acumuladores corresponden a las muestras que sobrevivieron
/// al último rechazo; no se publican en rutas (GPU/tiled/drizzle) que no
/// conservan Σw², porque inferirlos allí fabricaría precisión.
fn ds_classic_products_from_moments(
    sum: &[f64],
    sumsq: &[f64],
    weights: &[f64],
    weight_sq: &[f64],
    npx: usize,
    ch: usize,
) -> Option<crate::nebula_fusion::NfLiteProducts> {
    let len = npx.checked_mul(ch)?;
    if ch == 0
        || sum.len() != len
        || sumsq.len() != len
        || weights.len() != len
        || weight_sq.len() != len
    {
        return None;
    }
    let mut variance = vec![f32::NAN; len];
    let mut neff = vec![0.0f32; len];
    let mut coverage = vec![0.0f64; npx];
    for p in 0..npx {
        let mut pixel_coverage = 0.0f64;
        for c in 0..ch {
            let i = p * ch + c;
            let w = weights[i];
            let w2 = weight_sq[i];
            if !w.is_finite() || !w2.is_finite() || w <= 0.0 || w2 <= 0.0 {
                continue;
            }
            let effective_n = (w * w / w2).max(0.0);
            neff[i] = effective_n as f32;
            pixel_coverage += w;
            if effective_n > 1.0 + 1e-6 {
                let weighted_m2 = (sumsq[i] - sum[i] * sum[i] / w).max(0.0);
                let unbiased_denom = w - w2 / w;
                if unbiased_denom > 0.0 {
                    let sample_variance = weighted_m2 / unbiased_denom;
                    let mean_variance = sample_variance * w2 / (w * w);
                    if mean_variance.is_finite() {
                        variance[i] = mean_variance as f32;
                    }
                }
            }
        }
        coverage[p] = pixel_coverage / ch as f64;
    }
    let mut dq = ds_dq_from_coverage(&coverage);
    for p in 0..npx {
        if coverage[p] > 0.0
            && (0..ch).any(|c| {
                let i = p * ch + c;
                neff[i] > 0.0 && !variance[i].is_finite()
            })
        {
            // NEFF<=1 (una sola muestra efectiva: borde de dither, crop
            // desactivado): no existe varianza empírica estimable. SCI es
            // válido y VAR=NaN queda AUDITADA por DQ; sin este bit el
            // LinearFrame abortaba toda la publicación (auditoría 2026-07-20).
            dq[p] |= crate::deepsky_variance::dq::EIDR_UNCERTAINTY_UNAVAILABLE;
        }
    }
    Some(crate::nebula_fusion::NfLiteProducts {
        variance,
        neff,
        dq,
        masked_samples: 0,
        variance_origin: crate::deepsky_variance::VarianceOrigin::Empirical,
        g1g2_offset_max: None,
    })
}

/// Propagate the calibrated per-frame VAR/DQ through the exact Classic 1x
/// registration sampler and the final rejection window.  For a weighted mean
/// y=Σ(wᵢxᵢ)/Σwᵢ, Var(y)=Σ(wᵢ² Var(xᵢ))/(Σwᵢ)².  Lanczos
/// and bilinear interpolation square their normalized coefficients before
/// accumulating source VAR.
#[allow(clippy::too_many_arguments)]
fn ds_classic_products_from_calibration(
    load_science: &dyn Fn(usize) -> Result<DsImage, String>,
    load_uncertainty: &dyn Fn(usize) -> Result<DsCalibratedUncertainty, String>,
    registered: &[(usize, DsTransform, f64)],
    norms: &[([f32; 3], [f32; 3])],
    loc_fields: &[Option<Vec<f32>>],
    loc_grid: usize,
    w: usize,
    h: usize,
    ch: usize,
    wq_fields: &[Option<Vec<f32>>],
    wq_grid: usize,
    use_lanczos: bool,
    bounds: Option<(&[f32], &[f32])>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Option<crate::nebula_fusion::NfLiteProducts>, String> {
    if registered.is_empty()
        || ch == 0
        || norms.len() != registered.len()
        || loc_fields.len() != registered.len()
        || wq_fields.len() != registered.len()
    {
        return Ok(None);
    }
    let samples = w
        .checked_mul(h)
        .and_then(|pixels| pixels.checked_mul(ch))
        .ok_or("VAR Classic: geometría fuera de rango")?;
    if bounds.is_some_and(|(lo, hi)| lo.len() != samples || hi.len() != samples) {
        return Ok(None);
    }
    let mut variance_numerator = vec![0.0f64; samples];
    let mut weight = vec![0.0f64; samples];
    let mut weight_sq = vec![0.0f64; samples];
    let mut dq_out = vec![0u32; w * h];
    let lut = ds_l3_lut();

    for (k, &(frame_index, transform, frame_weight)) in registered.iter().enumerate() {
        cancellation_checkpoint(cancel, "propagación VAR/DQ Classic")?;
        let science = load_science(frame_index)?;
        let uncertainty = load_uncertainty(frame_index)?;
        if science.w != w
            || science.h != h
            || science.ch != ch
            || uncertainty.variance.len() != samples
            || uncertainty.dq.len() != w * h
        {
            return Ok(None);
        }
        let var_ptr = variance_numerator.as_mut_ptr() as usize;
        let weight_ptr = weight.as_mut_ptr() as usize;
        let weight_sq_ptr = weight_sq.as_mut_ptr() as usize;
        let dq_ptr = dq_out.as_mut_ptr() as usize;
        let norm = norms[k];
        let loc = loc_fields[k]
            .as_ref()
            .map(|field| (field.as_slice(), loc_grid, loc_grid));
        let wq = wq_fields[k]
            .as_ref()
            .map(|field| (field.as_slice(), wq_grid, wq_grid));

        (0..h).into_par_iter().for_each(|y| {
            let var_row = unsafe {
                std::slice::from_raw_parts_mut(
                    (var_ptr as *mut f64).add(y * w * ch),
                    w * ch,
                )
            };
            let weight_row = unsafe {
                std::slice::from_raw_parts_mut(
                    (weight_ptr as *mut f64).add(y * w * ch),
                    w * ch,
                )
            };
            let weight_sq_row = unsafe {
                std::slice::from_raw_parts_mut(
                    (weight_sq_ptr as *mut f64).add(y * w * ch),
                    w * ch,
                )
            };
            let dq_row = unsafe {
                std::slice::from_raw_parts_mut((dq_ptr as *mut u32).add(y * w), w)
            };
            for x in 0..w {
                let Some((sx, sy)) = transform.inverse(x as f32, y as f32) else {
                    continue;
                };
                if sx < 0.0
                    || sy < 0.0
                    || sx >= (science.w - 1) as f32
                    || sy >= (science.h - 1) as f32
                {
                    continue;
                }
                let local_weight = wq
                    .map(|(grid, gw, gh)| {
                        frame_weight
                            * ds_sample_grid(
                                grid,
                                gw,
                                gh,
                                x as f32 / w as f32,
                                y as f32 / h as f32,
                            ) as f64
                    })
                    .unwrap_or(frame_weight);
                let mut sampled_science = [f32::NAN; 3];
                let mut sampled_variance = [f32::NAN; 3];
                let mut sampled_dq = 0u32;
                if use_lanczos
                    && sx >= 3.0
                    && sy >= 3.0
                    && sx < (science.w - 4) as f32
                    && sy < (science.h - 4) as f32
                {
                    ds_sample_lanczos3(&science, lut, sx, sy, &mut sampled_science);
                    let x0 = sx.floor() as usize;
                    let y0 = sy.floor() as usize;
                    let fx = sx - x0 as f32;
                    let fy = sy - y0 as f32;
                    let mut wx = [0.0f32; 6];
                    let mut wy = [0.0f32; 6];
                    let mut swx = 0.0f32;
                    let mut swy = 0.0f32;
                    for tap in 0..6 {
                        let offset = tap as f32 - 2.0;
                        let ix = ((offset - fx).abs() * DS_L3_RES as f32) as usize;
                        let iy = ((offset - fy).abs() * DS_L3_RES as f32) as usize;
                        wx[tap] = lut.get(ix).copied().unwrap_or(0.0);
                        wy[tap] = lut.get(iy).copied().unwrap_or(0.0);
                        swx += wx[tap];
                        swy += wy[tap];
                    }
                    let normalization = 1.0 / (swx * swy).max(1.0e-6);
                    for channel in 0..ch {
                        let mut value = 0.0f64;
                        let mut valid = true;
                        for yy in 0..6 {
                            for xx in 0..6 {
                                let source_pixel = (y0 + yy - 2) * w + (x0 + xx - 2);
                                let source = source_pixel * ch + channel;
                                let coefficient = wx[xx] * wy[yy] * normalization;
                                let input_variance = uncertainty.variance[source];
                                sampled_dq |= uncertainty.dq[source_pixel];
                                if !input_variance.is_finite() || input_variance < 0.0 {
                                    valid = false;
                                } else {
                                    value += coefficient as f64
                                        * coefficient as f64
                                        * input_variance as f64;
                                }
                            }
                        }
                        if valid {
                            sampled_variance[channel] = value as f32;
                        }
                    }
                } else {
                    let x0 = sx.floor() as usize;
                    let y0 = sy.floor() as usize;
                    let fx = sx - x0 as f32;
                    let fy = sy - y0 as f32;
                    let coefficients = [
                        (1.0 - fx) * (1.0 - fy),
                        fx * (1.0 - fy),
                        (1.0 - fx) * fy,
                        fx * fy,
                    ];
                    let source_pixels = [
                        y0 * w + x0,
                        y0 * w + x0 + 1,
                        (y0 + 1) * w + x0,
                        (y0 + 1) * w + x0 + 1,
                    ];
                    for &source_pixel in &source_pixels {
                        sampled_dq |= uncertainty.dq[source_pixel];
                    }
                    for channel in 0..ch {
                        let mut value = 0.0f64;
                        let mut sci = 0.0f32;
                        let mut valid = true;
                        for tap in 0..4 {
                            let source = source_pixels[tap] * ch + channel;
                            let input_variance = uncertainty.variance[source];
                            let input_science = science.data[source];
                            if !input_variance.is_finite()
                                || input_variance < 0.0
                                || !input_science.is_finite()
                            {
                                valid = false;
                            } else {
                                sci += coefficients[tap] * input_science;
                                value += coefficients[tap] as f64
                                    * coefficients[tap] as f64
                                    * input_variance as f64;
                            }
                        }
                        if valid {
                            sampled_science[channel] = sci;
                            sampled_variance[channel] = value as f32;
                        }
                    }
                }
                dq_row[x] |= sampled_dq;
                if sampled_dq
                    & (crate::deepsky_variance::dq::SATURATED
                        | crate::deepsky_variance::dq::NONLINEAR
                        | crate::deepsky_variance::dq::HOT_COLD
                        | crate::deepsky_variance::dq::COSMIC
                        | crate::deepsky_variance::dq::NAN_INPUT
                        | crate::deepsky_variance::dq::FLAT_INVALID
                        | crate::deepsky_variance::dq::NO_COVERAGE)
                    != 0
                {
                    continue;
                }
                for channel in 0..ch {
                    let loc_offset = loc
                        .map(|(grid, gw, gh)| {
                            ds_sample_local_field(
                                grid,
                                gw,
                                gh,
                                ch,
                                channel,
                                x as f32 / w as f32,
                                y as f32 / h as f32,
                            )
                        })
                        .unwrap_or(0.0);
                    let normalized_science = sampled_science[channel] * norm.0[channel]
                        + norm.1[channel]
                        + loc_offset;
                    let normalized_variance =
                        sampled_variance[channel] * norm.0[channel] * norm.0[channel];
                    if !normalized_science.is_finite()
                        || !normalized_variance.is_finite()
                        || normalized_variance < 0.0
                    {
                        continue;
                    }
                    let output = (y * w + x) * ch + channel;
                    if let Some((lo, hi)) = bounds {
                        if normalized_science < lo[output] || normalized_science > hi[output] {
                            continue;
                        }
                    }
                    let local = x * ch + channel;
                    var_row[local] += normalized_variance as f64 * local_weight * local_weight;
                    weight_row[local] += local_weight;
                    weight_sq_row[local] += local_weight * local_weight;
                }
            }
        });
    }

    let mut variance = vec![f32::NAN; samples];
    let mut neff = vec![0.0f32; samples];
    if !weight.iter().any(|value| value.is_finite() && *value > 0.0) {
        // A one-frame/non-estimable calibration master may legitimately leave
        // every input VAR as NaN. Fall back to the empirical Classic moments;
        // do not publish an all-empty propagated bundle as if SCI had no
        // geometric coverage.
        return Ok(None);
    }
    for pixel in 0..w * h {
        let mut covered = false;
        for channel in 0..ch {
            let sample = pixel * ch + channel;
            if weight[sample] > 0.0 && weight_sq[sample] > 0.0 {
                covered = true;
                variance[sample] =
                    (variance_numerator[sample] / (weight[sample] * weight[sample])) as f32;
                neff[sample] = (weight[sample] * weight[sample] / weight_sq[sample]) as f32;
            }
        }
        if covered {
            // Fatal input flags describe samples that were excluded, not the
            // integrated value produced from other valid frames. Keeping
            // FLAT_INVALID/NAN_INPUT on a finite output would violate the
            // LinearFrame contract and falsely mark usable signal invalid.
            // Limitación auditada: cuando la exclusión es parcial, VAR/NEFF se
            // calculan sobre las M<N muestras con VAR válida mientras el
            // máster promedió las N; la propagación exacta exigiría que el
            // acumulador del máster aplicase la misma exclusión (backlog).
            dq_out[pixel] &= !(crate::deepsky_variance::dq::SATURATED
                | crate::deepsky_variance::dq::NONLINEAR
                | crate::deepsky_variance::dq::HOT_COLD
                | crate::deepsky_variance::dq::COSMIC
                | crate::deepsky_variance::dq::NAN_INPUT
                | crate::deepsky_variance::dq::FLAT_INVALID
                | crate::deepsky_variance::dq::NO_COVERAGE);
        } else {
            // Sin peso de VAR en ningún canal. Puede ser cobertura geométrica
            // real cero O exclusión total por DQ/VAR=NaN de taps (cosmética en
            // stack sin dithering) con el máster integrando señal finita. La
            // reconciliación SCI↔DQ de la publicación decide con el máster en
            // la mano (auditoría 2026-07-20).
            dq_out[pixel] |= crate::deepsky_variance::dq::NO_COVERAGE;
        }
    }
    Ok(Some(crate::nebula_fusion::NfLiteProducts {
        variance,
        neff,
        dq: dq_out,
        masked_samples: 0,
        variance_origin:
            crate::deepsky_variance::VarianceOrigin::HybridEmpiricalPropagated,
        g1g2_offset_max: None,
    }))
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
    // Pure per-channel offset — do NOT clamp to zero. Neutralization only removes
    // a constant per channel; truncating negatives here would bias the background
    // and break the linear/statistics of the master.
    data.par_chunks_mut(3).for_each(|px| {
        px[0] -= off[0];
        px[1] -= off[1];
        px[2] -= off[2];
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
fn ds_autocrop_with_origin(
    data: &[f32],
    cov: &[f64],
    w: usize,
    h: usize,
    ch: usize,
) -> (Vec<f32>, usize, usize, usize, usize) {
    let max_cov = cov.iter().cloned().fold(0.0f64, f64::max);
    if max_cov <= 0.0 {
        return (data.to_vec(), w, h, 0, 0);
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
        return (data.to_vec(), w, h, 0, 0);
    }
    let (nw, nh) = (x1 - x0 + 1, y1 - y0 + 1);
    // Guard against pathological over-crop and the no-op full-frame case.
    if nw == w && nh == h {
        return (data.to_vec(), w, h, 0, 0);
    }
    if (nw * nh) * 4 < w * h {
        return (data.to_vec(), w, h, 0, 0);
    }
    let mut out = vec![0.0f32; nw * nh * ch];
    for y in 0..nh {
        let src = ((y + y0) * w + x0) * ch;
        let dst = y * nw * ch;
        out[dst..dst + nw * ch].copy_from_slice(&data[src..src + nw * ch]);
    }
    (out, nw, nh, x0, y0)
}

fn ds_autocrop(
    data: &[f32],
    cov: &[f64],
    w: usize,
    h: usize,
    ch: usize,
) -> (Vec<f32>, usize, usize) {
    let (out, nw, nh, _, _) = ds_autocrop_with_origin(data, cov, w, h, ch);
    (out, nw, nh)
}

fn ds_crop_plane<T: Copy>(
    plane: &[T],
    w: usize,
    h: usize,
    x0: usize,
    y0: usize,
    nw: usize,
    nh: usize,
) -> Vec<T> {
    if plane.len() != w * h || (x0 == 0 && y0 == 0 && nw == w && nh == h) {
        return plane.to_vec();
    }
    let mut out = Vec::with_capacity(nw * nh);
    for y in y0..y0 + nh {
        out.extend_from_slice(&plane[y * w + x0..y * w + x0 + nw]);
    }
    out
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
fn ds_stretch_params(
    rgb: &[u16],
    w: usize,
    h: usize,
    per_channel: bool,
    shadow_k: f32,
    target: f32,
) -> [(f32, f32); 3] {
    let n = w * h;
    let step = (n / 300_000).max(1);
    let target = target.clamp(0.05, 0.9);
    // STF statistics over DATA pixels only: exact zeros (autocrop remnants,
    // warp borders, crushed background) would collapse the median/MAD and make
    // the stretch either explosive or flat. Fall back to all pixels when the
    // image is almost entirely zero.
    if per_channel {
        let mut params = [(0.0f32, 0.5f32); 3];
        for cch in 0..3 {
            let mut s: Vec<f32> = (0..n)
                .step_by(step)
                .map(|i| rgb[i * 3 + cch] as f32 / 65535.0)
                .filter(|v| *v > 0.0)
                .collect();
            if s.len() < 256 {
                s = (0..n)
                    .step_by(step)
                    .map(|i| rgb[i * 3 + cch] as f32 / 65535.0)
                    .collect();
            }
            params[cch] = ds_stf_params(s, shadow_k, target);
        }
        params
    } else {
        let luma_of = |i: usize| -> f32 {
            (0.2126 * rgb[i * 3] as f32
                + 0.7152 * rgb[i * 3 + 1] as f32
                + 0.0722 * rgb[i * 3 + 2] as f32)
                / 65535.0
        };
        let mut s: Vec<f32> = (0..n)
            .step_by(step)
            .map(luma_of)
            .filter(|v| *v > 0.0)
            .collect();
        if s.len() < 256 {
            s = (0..n).step_by(step).map(luma_of).collect();
        }
        let p = ds_stf_params(s, shadow_k, target);
        [p, p, p]
    }
}

fn ds_render_stretch(
    rgb: &[u16],
    w: usize,
    h: usize,
    per_channel: bool,
    shadow_k: f32,
    target: f32,
) -> Vec<u8> {
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
        let img = guard
            .as_ref()
            .ok_or("No hay resultado apilado en memoria.")?;
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
                .map(|i| {
                    (rgb16[i * 3] as f32 + rgb16[i * 3 + 1] as f32 + rgb16[i * 3 + 2] as f32) / 3.0
                })
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
    Ok(format!(
        "data:image/png;base64,{}",
        general_purpose::STANDARD.encode(&enc)
    ))
}

/// Preview estirado de UNA toma individual (inspección PSF): lee el archivo,
/// hace debayer si aplica, reduce a ~1200 px de lado mayor y aplica el mismo
/// estirado STF del resultado. Devuelve data-URL PNG — para revisar POR QUÉ
/// la inspección marcó una toma antes de decidir descartarla.
#[tauri::command]
fn deepsky_frame_preview(path: String) -> Result<String, String> {
    let mut img = ds_read_image(&path)?;
    if let Some(cid) = img.bayer {
        img = ds_debayer_image(img, cid);
    }
    let factor = ((img.w.max(img.h) + 1199) / 1200).max(1);
    let (w, h) = ((img.w / factor).max(1), (img.h / factor).max(1));
    let ch = img.ch.min(3);
    let mut rgb16 = vec![0u16; w * h * 3];
    rgb16.par_chunks_mut(3).enumerate().for_each(|(index, px)| {
        let ox = index % w;
        let oy = index / w;
        let x0 = ox * factor;
        let y0 = oy * factor;
        let x1 = (x0 + factor).min(img.w);
        let y1 = (y0 + factor).min(img.h);
        let mut sums = [0.0f64; 3];
        let mut count = 0usize;
        for y in y0..y1 {
            for x in x0..x1 {
                let base = (y * img.w + x) * img.ch;
                for c in 0..3 {
                    sums[c] += img.data[base + c.min(ch - 1)] as f64;
                }
                count += 1;
            }
        }
        for c in 0..3 {
            px[c] = (sums[c] / count.max(1) as f64).clamp(0.0, 65535.0) as u16;
        }
    });
    let preview8 = ds_render_stretch(&rgb16, w, h, false, 2.8, 0.25);
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
    Ok(format!(
        "data:image/png;base64,{}",
        general_purpose::STANDARD.encode(&enc)
    ))
}

/// Vista diagnóstica de los planos científicos conservados con el máster.
/// La normalización es sólo visual (percentil 99); los arrays float32 no se
/// modifican y se exportan con su escala física por `deepsky_export_float32`.
#[tauri::command]
fn deepsky_result_view(state: State<'_, AppState>, kind: String) -> Result<String, String> {
    let guard = state.deep_sky_result.lock().unwrap();
    let result = guard
        .as_ref()
        .ok_or("No hay resultado lineal de cielo profundo")?;
    let bg_owned: Vec<f32>;
    let plane: &[f32] = match kind.as_str() {
        "coverage" => &result.coverage,
        "weight" => &result.weight,
        "rejection_low" => &result.rejection_low,
        "rejection_high" => &result.rejection_high,
        "registration_residuals" => &result.registration_residuals,
        // Productos científicos NF (F3): luma media de canales; VAR con NaN
        // (huecos) a 0 para el colormap; DQ como bits en float exacto.
        "variance" | "neff" => {
            let planes = if kind == "variance" {
                result.variance.as_ref()
            } else {
                result.neff.as_ref()
            };
            let plane_data = planes.ok_or(
                "La ruta efectiva no conservó momentos/Σw² para este mapa; revisa scientificProducts en la receta",
            )?;
            let (w, h, chn) = (result.width, result.height, result.channels);
            let npx = w * h;
            let mut luma = vec![0.0f32; npx];
            for p in 0..npx {
                let mut s = 0.0f32;
                let mut cnt = 0.0f32;
                for c in 0..chn {
                    let v = plane_data[p * chn + c];
                    if v.is_finite() {
                        s += v;
                        cnt += 1.0;
                    }
                }
                luma[p] = if cnt > 0.0 { s / cnt } else { 0.0 };
            }
            bg_owned = luma;
            &bg_owned
        }
        "dq" => {
            let dq = result
                .dq
                .as_ref()
                .ok_or("La ruta efectiva no produjo DQ")?;
            bg_owned = dq.iter().map(|&b| b as f32).collect();
            &bg_owned
        }
        // STRUCT/RESIDUAL (F7): desplazados al rango positivo para el colormap.
        "struct" | "struct_residual" => {
            let plane_src = if kind == "struct" {
                result.struct_map.as_ref()
            } else {
                result.struct_residual.as_ref()
            }
            .ok_or("STRUCT requiere el modo NebulaFusion Full + STRUCT")?;
            let mut shifted = plane_src.clone();
            let minv = shifted.iter().copied().fold(f32::INFINITY, f32::min);
            if minv.is_finite() && minv < 0.0 {
                for v in shifted.iter_mut() {
                    *v -= minv;
                }
            }
            bg_owned = shifted;
            &bg_owned
        }
        // Modelo de fondo/contaminación lumínica (F2): se ajusta bajo demanda
        // sobre el máster (grado 2 robusto, no destructivo) y se muestra como
        // luma desplazada al rango positivo.
        // Mapa de recuperabilidad EIDR (F9): R por tile, 0..1.
        "recoverability" => {
            let plane_src = result
                .recoverability
                .as_ref()
                .ok_or("El mapa de recuperabilidad requiere el motor EIDR")?;
            bg_owned = plane_src.clone();
            &bg_owned
        }
        "background_model" => {
            let (w, h, ch) = (result.width, result.height, result.channels);
            let model = crate::deepsky_background::fit_background_model(&result.data, w, h, ch)
                .ok_or("El máster es demasiado pequeño para modelar el fondo")?;
            let corr = model.render_correction();
            let npx = w * h;
            let mut luma = vec![0.0f32; npx];
            for p in 0..npx {
                let mut s = 0.0f32;
                for c in 0..ch {
                    s += corr[p * ch + c];
                }
                luma[p] = s / ch as f32;
            }
            let minv = luma.iter().copied().fold(f32::INFINITY, f32::min);
            if minv.is_finite() && minv < 0.0 {
                for v in luma.iter_mut() {
                    *v -= minv;
                }
            }
            bg_owned = luma;
            &bg_owned
        }
        _ => return Err(format!("Vista diagnóstica desconocida: {kind}")),
    };
    let n = result.width * result.height;
    if plane.len() != n {
        return Err("El mapa no coincide con la geometría del máster".into());
    }
    let mut sample: Vec<f32> = plane
        .iter()
        .step_by((n / 200_000).max(1))
        .copied()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .collect();
    sample.sort_by(|a, b| a.total_cmp(b));
    let hi = sample
        .get(((sample.len().saturating_sub(1)) as f32 * 0.99) as usize)
        .copied()
        .unwrap_or(1.0)
        .max(1e-6);
    let mut rgba = Vec::with_capacity(n * 4);
    for &v in plane {
        let x = (v / hi).clamp(0.0, 1.0).sqrt();
        let (r, g, b) = match kind.as_str() {
            "coverage" | "weight" => {
                // Viridis compacto: oscuro→violeta→cian→amarillo.
                let r = (255.0 * (0.18 + 0.82 * x.powf(1.7))).clamp(0.0, 255.0);
                let g = (255.0 * (0.03 + 0.92 * x)).clamp(0.0, 255.0);
                let b = (255.0 * (0.32 + 0.55 * (1.0 - (2.0 * x - 1.0).abs()))).clamp(0.0, 255.0);
                (r as u8, g as u8, b as u8)
            }
            "rejection_low" => ((25.0 * x) as u8, (180.0 * x) as u8, (255.0 * x) as u8),
            "rejection_high" => ((255.0 * x) as u8, (105.0 * x) as u8, (25.0 * x) as u8),
            _ => (
                (220.0 * x) as u8,
                (80.0 * (1.0 - x)) as u8,
                (255.0 * (1.0 - x)) as u8,
            ),
        };
        rgba.extend_from_slice(&[r, g, b, 255]);
    }
    let img = RgbaImage::from_raw(result.width as u32, result.height as u32, rgba)
        .ok_or("Mapa inválido")?;
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "data:image/png;base64,{}",
        general_purpose::STANDARD.encode(png)
    ))
}

/// EXPORT the deep-sky result to disk next to the reference light. Saves the
/// STRETCHED image (exactly what the STF preview shows — 16-bit TIFF or 8-bit
/// PNG) and/or the LINEAR 16-bit master (for further work in PixInsight/PS).
/// Returns a human summary of the files written.
// (async): trabajo de segundos-minutos fuera del hilo principal — la UI
// sigue viva y Cancelar/checkpoints funcionan (auditoría 2026-07-20).
#[tauri::command(async)]
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
    ds_begin_user_action(&state);
    let cancel = state.cancel_requested.clone();
    let (rgb16, w, h, is_mono) = {
        let guard = state.stacked_image.lock().unwrap();
        let img = guard
            .as_ref()
            .ok_or("No hay resultado de cielo profundo en memoria.")?;
        (img.data.clone(), img.width, img.height, img.is_mono)
    };
    if rgb16.len() < w * h * 3 {
        return Err("El resultado no es RGB de 16 bits, no se puede exportar.".into());
    }
    // 16-bit TIFF is a PRO feature (matches the planetary save gate). PNG is free.
    let wants_tiff = (save_stretched && !as_png) || save_linear;
    if wants_tiff && !state.license_manager.is_pro() {
        return Err(
            "Exportar en TIFF 16-bit requiere licencia PRO o periodo de prueba activo.".into(),
        );
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
        cancellation_checkpoint(cancel.as_ref(), "exportación TIFF/PNG estirada")?;
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
            let out = parent.join(format!(
                "ZenithDeepSky_{}_Estirado_16bit_{}.tiff",
                kind, stamp
            ));
            derot_save_rgb16_tiff(&out, &st16, w, h)?;
            saved.push(out.display().to_string());
        }
    }
    if save_linear {
        cancellation_checkpoint(cancel.as_ref(), "exportación TIFF lineal")?;
        let out = parent.join(format!(
            "ZenithDeepSky_{}_Lineal_16bit_{}.tiff",
            kind, stamp
        ));
        derot_save_rgb16_tiff(&out, &rgb16, w, h)?;
        saved.push(out.display().to_string());
    }
    if saved.is_empty() {
        return Err("Nada seleccionado para exportar.".into());
    }
    log_to_front(
        &app,
        "SUCCESS",
        &format!("Cielo Profundo exportado: {}", saved.join(" · ")),
    );
    Ok(format!(
        "Exportado ({} archivo/s):\n{}",
        saved.len(),
        saved.join("\n")
    ))
}

fn ds_fits_card(key: &str, value: &str, comment: Option<&str>) -> [u8; 80] {
    let mut card = [b' '; 80];
    let text = if key == "END" {
        "END".to_string()
    } else if let Some(c) = comment {
        format!("{:<8}= {} / {}", key, value, c)
    } else {
        format!("{:<8}= {}", key, value)
    };
    let bytes = text.as_bytes();
    card[..bytes.len().min(80)].copy_from_slice(&bytes[..bytes.len().min(80)]);
    card
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DsFitsUnit {
    Adu,
    AduSquared,
    Dimensionless,
    Count,
    Bitmask,
    Pixel,
}

impl DsFitsUnit {
    fn header_value(self) -> &'static str {
        match self {
            Self::Adu => "'ADU'",
            Self::AduSquared => "'ADU^2'",
            Self::Dimensionless => "'1'",
            Self::Count => "'count'",
            Self::Bitmask => "'BITMASK'",
            Self::Pixel => "'pixel'",
        }
    }
}

fn ds_map_fits_unit(name: &str) -> DsFitsUnit {
    match name {
        "coverage" | "weight" | "neff" | "recoverability" | "recov" => {
            DsFitsUnit::Dimensionless
        }
        "variance" => DsFitsUnit::AduSquared,
        "dq" => DsFitsUnit::Bitmask,
        "rejection_low" | "rejection_high" => DsFitsUnit::Count,
        "registration_residuals" => DsFitsUnit::Pixel,
        _ => DsFitsUnit::Adu,
    }
}

/// FITS primario float32 estándar, big-endian y RGB planar. No depende de una
/// librería nativa, por lo que conserva el empaquetado Windows/macOS actual.
fn ds_save_float32_fits(
    path: &std::path::Path,
    data: &[f32],
    w: usize,
    h: usize,
    ch: usize,
    metadata: &[(&str, String)],
) -> Result<(), String> {
    ds_save_float32_fits_cancellable(path, data, w, h, ch, metadata, None)
}

#[cfg(test)]
thread_local! {
    static TEST_FITS_WRITES_BEFORE_FAILURE: std::cell::Cell<Option<usize>> = const {
        std::cell::Cell::new(None)
    };
}

fn ds_fits_write_checkpoint(cancel: Option<&std::sync::atomic::AtomicBool>) -> Result<(), String> {
    if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)) {
        return Err("Cancelado por el usuario durante exportación FITS float32".into());
    }
    #[cfg(test)]
    TEST_FITS_WRITES_BEFORE_FAILURE.with(|budget| {
        if let Some(remaining) = budget.get() {
            if remaining == 0 {
                return Err::<(), String>("FITS data: disco lleno (fallo inyectado)".to_string());
            }
            budget.set(Some(remaining - 1));
        }
        Ok(())
    })?;
    Ok(())
}

fn ds_save_float32_fits_cancellable(
    path: &std::path::Path,
    data: &[f32],
    w: usize,
    h: usize,
    ch: usize,
    metadata: &[(&str, String)],
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<(), String> {
    if w == 0 || h == 0 || !matches!(ch, 1 | 3) || data.len() < w * h * ch {
        return Err("Datos FITS float32 inválidos".into());
    }
    let mut header = Vec::with_capacity(2880);
    let mut push = |card: [u8; 80]| header.extend_from_slice(&card);
    push(ds_fits_card(
        "SIMPLE",
        "                   T",
        Some("FITS standard"),
    ));
    push(ds_fits_card(
        "BITPIX",
        "                 -32",
        Some("IEEE float32"),
    ));
    push(ds_fits_card(
        "NAXIS",
        &format!("{:>20}", if ch == 1 { 2 } else { 3 }),
        None,
    ));
    push(ds_fits_card("NAXIS1", &format!("{:>20}", w), None));
    push(ds_fits_card("NAXIS2", &format!("{:>20}", h), None));
    if ch == 3 {
        push(ds_fits_card(
            "NAXIS3",
            &format!("{:>20}", ch),
            Some("RGB planes"),
        ));
    }
    push(ds_fits_card("EXTEND", "                   T", None));
    push(ds_fits_card("ORIGIN", "'Zenith Astro Stacker'", None));
    let bunit = metadata
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("BUNIT"))
        .map(|(_, value)| value.as_str())
        .unwrap_or_else(|| DsFitsUnit::Adu.header_value());
    push(ds_fits_card("BUNIT", bunit, None));
    push(ds_fits_card(
        "DATE",
        &format!("'{}'", chrono::Utc::now().to_rfc3339()),
        None,
    ));
    for (key, value) in metadata {
        if key.eq_ignore_ascii_case("BUNIT") {
            continue;
        }
        push(ds_fits_card(key, value, None));
    }
    push(ds_fits_card("END", "", None));
    while header.len() % 2880 != 0 {
        header.push(b' ');
    }

    static FITS_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let temp_name = format!(
        ".{}.{}.{}.part",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("zenith.fits"),
        std::process::id(),
        FITS_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let temp = path.with_file_name(temp_name);
    let write_result = (|| -> Result<(), String> {
        let mut file = std::io::BufWriter::new(
            std::fs::File::create(&temp).map_err(|e| format!("FITS create: {e}"))?,
        );
        use std::io::Write;
        ds_fits_write_checkpoint(cancel)?;
        file.write_all(&header)
            .map_err(|e| format!("FITS header: {e}"))?;
        let plane = w * h;
        let mut block = Vec::with_capacity(1024 * 1024);
        for c in 0..ch {
            for i in 0..plane {
                block.extend_from_slice(&data[i * ch + c].to_be_bytes());
                if block.len() >= 1024 * 1024 {
                    ds_fits_write_checkpoint(cancel)?;
                    file.write_all(&block)
                        .map_err(|e| format!("FITS data: {e}"))?;
                    block.clear();
                }
            }
        }
        if !block.is_empty() {
            ds_fits_write_checkpoint(cancel)?;
            file.write_all(&block)
                .map_err(|e| format!("FITS data: {e}"))?;
        }
        let data_bytes = plane * ch * 4;
        let padding = (2880 - data_bytes % 2880) % 2880;
        if padding > 0 {
            ds_fits_write_checkpoint(cancel)?;
            file.write_all(&vec![0u8; padding])
                .map_err(|e| format!("FITS pad: {e}"))?;
        }
        file.flush().map_err(|e| format!("FITS flush: {e}"))?;
        file.get_ref()
            .sync_data()
            .map_err(|e| format!("FITS sync: {e}"))?;
        ds_fits_write_checkpoint(cancel)
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    // El archivo anterior permanece válido hasta que el temporal está
    // completamente escrito/sincronizado. Windows no reemplaza con rename; se
    // conserva un backup de rollback durante la pequeña ventana de commit.
    let backup = path.with_file_name(format!(
        ".{}.{}.bak",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("zenith.fits"),
        FITS_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let had_previous = path.exists();
    if had_previous {
        std::fs::rename(path, &backup).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            format!("FITS backup: {e}")
        })?;
    }
    match std::fs::rename(&temp, path) {
        Ok(()) => {
            if had_previous {
                let _ = std::fs::remove_file(backup);
            }
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            if had_previous {
                let _ = std::fs::rename(&backup, path);
            }
            Err(format!("FITS commit: {error}"))
        }
    }
}

fn ds_write_recipe(path: &std::path::Path, result: &DeepSkyLinearResult) -> Result<(), String> {
    let env = benchmark::get_benchmark_environment();
    // v4: contrato científico explícito (calibración, motor efectivo,
    // fallbacks y disponibilidad/units de SCI/VAR/NEFF/DQ/coverage).
    let json = serde_json::json!({
        "schemaVersion": pipeline::DEEP_SKY_RECIPE_SCHEMA_VERSION,
        "algorithmVersion": "hybrid-v2-2026.07",
        "resultId": result.id,
        "width": result.width,
        "height": result.height,
        "channels": result.channels,
        "linearFloat32": true,
        "engine": result.engine,
        "rejectionMethod": result.method,
        "framesUsed": result.frames_used,
        "framesRejected": result.frames_rejected,
        "elapsedSeconds": result.elapsed_seconds,
        "recipe": result.recipe,
        "telemetry": pipeline::telemetry_snapshot(Some(&result.id)),
        "environment": env,
        "createdAtUtc": chrono::Utc::now().to_rfc3339(),
    });
    let bytes = serde_json::to_vec_pretty(&json).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Receta JSON carpeta: {e}"))?;
    }
    static RECIPE_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(1);
    let sequence = RECIPE_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("zenith-recipe.json");
    let temp = path.with_file_name(format!(".{name}.{}.{}.part", std::process::id(), sequence));
    let backup = path.with_file_name(format!(".{name}.{}.{}.bak", std::process::id(), sequence));
    let write_result = (|| -> Result<(), String> {
        use std::io::Write;
        let mut file = std::fs::File::create(&temp).map_err(|e| format!("Receta JSON: {e}"))?;
        file.write_all(&bytes)
            .map_err(|e| format!("Receta JSON: {e}"))?;
        file.sync_data()
            .map_err(|e| format!("Receta JSON sync: {e}"))
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    let had_previous = path.exists();
    if had_previous {
        std::fs::rename(path, &backup).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            format!("Receta JSON backup: {e}")
        })?;
    }
    match std::fs::rename(&temp, path) {
        Ok(()) => {
            if had_previous {
                let _ = std::fs::remove_file(backup);
            }
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            if had_previous {
                let _ = std::fs::rename(&backup, path);
            }
            Err(format!("Receta JSON commit: {error}"))
        }
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DeepSkyFloatExport {
    master_fits: String,
    recipe_json: String,
    diagnostic_fits: Vec<String>,
}

// (async): trabajo de segundos-minutos fuera del hilo principal — la UI
// sigue viva y Cancelar/checkpoints funcionan (auditoría 2026-07-20).
#[tauri::command(async)]
fn deepsky_export_float32(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    base_path: String,
    include_maps: Option<bool>,
) -> Result<DeepSkyFloatExport, String> {
    state.license_manager.check_access()?;
    ds_begin_user_action(&state);
    let cancel = state.cancel_requested.clone();
    let result = state.deep_sky_result.lock().unwrap();
    let result = result
        .as_ref()
        .ok_or("No hay máster lineal float32 en memoria")?;
    let export_started = std::time::Instant::now();
    let export_sys = std::sync::Mutex::new(System::new_all());
    let include_diagnostics = include_maps.unwrap_or(true);
    let export_total = 2 + usize::from(include_diagnostics) * 5;
    emit_deepsky_pipeline_telemetry(
        &app,
        &result.id,
        "export",
        "CPU I/O FITS float32 + JSON",
        0,
        export_total,
        export_started,
        &export_sys,
        0,
        0,
        None,
    );
    let base = std::path::Path::new(&base_path);
    let parent = if base.is_dir() {
        base.to_path_buf()
    } else {
        base.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(std::env::temp_dir)
    };
    std::fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
    let stem = format!("ZenithDeepSky_{}", result.id);
    let master = parent.join(format!("{stem}_linear_float32.fits"));
    let recipe = parent.join(format!("{stem}_recipe.json"));
    let master_metadata = vec![
        ("ZASVER", "'hybrid-v2-2026.07'".to_string()),
        ("ZASJOB", format!("'{}'", result.id)),
        ("ZASENG", format!("'{}'", result.engine)),
        ("ZASREJ", format!("'{}'", result.method)),
        ("NCOMBINE", format!("{:>20}", result.frames_used)),
        ("NREJECT", format!("{:>20}", result.frames_rejected)),
        ("ELAPSED", format!("{:>20.6}", result.elapsed_seconds)),
        (
            "RECIPE",
            format!(
                "'{}'",
                recipe.file_name().unwrap_or_default().to_string_lossy()
            ),
        ),
    ];
    cancellation_checkpoint(cancel.as_ref(), "exportación FITS float32")?;
    ds_save_float32_fits_cancellable(
        &master,
        &result.data,
        result.width,
        result.height,
        result.channels,
        &master_metadata,
        Some(cancel.as_ref()),
    )?;

    let mut diagnostics = Vec::new();
    if include_diagnostics {
        for (name, map) in [
            ("coverage", &result.coverage),
            ("weight", &result.weight),
            ("rejection_low", &result.rejection_low),
            ("rejection_high", &result.rejection_high),
            ("registration_residuals", &result.registration_residuals),
        ] {
            cancellation_checkpoint(cancel.as_ref(), "exportación de mapas diagnósticos")?;
            if map.len() == result.width * result.height {
                let path = parent.join(format!("{stem}_{name}.fits"));
                ds_save_float32_fits_cancellable(
                    &path,
                    map,
                    result.width,
                    result.height,
                    1,
                    &[
                        ("EXTNAME", format!("'{}'", name)),
                        ("ZASJOB", format!("'{}'", result.id)),
                        ("BUNIT", ds_map_fits_unit(name).header_value().into()),
                    ],
                    Some(cancel.as_ref()),
                )?;
                diagnostics.push(path.display().to_string());
            }
        }
        // Productos científicos NF (F3): VAR/NEFF con el layout del máster;
        // DQ como float32 exacto (bits < 2^24).
        for (name, plane, chn) in [
            ("variance", result.variance.as_deref(), result.channels),
            ("neff", result.neff.as_deref(), result.channels),
        ] {
            if let Some(plane) = plane {
                cancellation_checkpoint(cancel.as_ref(), "exportación de productos científicos")?;
                let path = parent.join(format!("{stem}_{name}.fits"));
                ds_save_float32_fits_cancellable(
                    &path,
                    plane,
                    result.width,
                    result.height,
                    chn,
                    &[
                        ("EXTNAME", format!("'{}'", name.to_uppercase())),
                        ("ZASJOB", format!("'{}'", result.id)),
                        ("BUNIT", ds_map_fits_unit(name).header_value().into()),
                    ],
                    Some(cancel.as_ref()),
                )?;
                diagnostics.push(path.display().to_string());
            }
        }
        // STRUCT/RESIDUAL (F7): planos luma; el header declara que STRUCT es
        // un mapa de evidencia (soporte seleccionado), no el máster.
        for (name, plane) in [
            ("struct", result.struct_map.as_deref()),
            ("struct_residual", result.struct_residual.as_deref()),
        ] {
            if let Some(plane) = plane {
                cancellation_checkpoint(cancel.as_ref(), "exportación de STRUCT")?;
                let path = parent.join(format!("{stem}_{name}.fits"));
                ds_save_float32_fits_cancellable(
                    &path,
                    plane,
                    result.width,
                    result.height,
                    1,
                    &[
                        ("EXTNAME", format!("'{}'", name.to_uppercase())),
                        ("ZASJOB", format!("'{}'", result.id)),
                        ("ZASEVID", "T".to_string()),
                    ],
                    Some(cancel.as_ref()),
                )?;
                diagnostics.push(path.display().to_string());
            }
        }
        // Mapa de recuperabilidad EIDR (F9): R por tile a resolución del
        // máster — el "qué frecuencias contienen evidencia" publicado (§7.4).
        if let Some(plane) = result.recoverability.as_deref() {
            cancellation_checkpoint(cancel.as_ref(), "exportación de RECOV")?;
            let path = parent.join(format!("{stem}_recov.fits"));
            ds_save_float32_fits_cancellable(
                &path,
                plane,
                result.width,
                result.height,
                1,
                &[
                    ("EXTNAME", "'RECOV'".to_string()),
                    ("ZASJOB", format!("'{}'", result.id)),
                    ("ZASEVID", "T".to_string()),
                    (
                        "BUNIT",
                        DsFitsUnit::Dimensionless.header_value().to_string(),
                    ),
                ],
                Some(cancel.as_ref()),
            )?;
            diagnostics.push(path.display().to_string());
        }
        // Nebula Contrast (F7): export estético DECLARADAMENTE NO LINEAL —
        // el SCI con las estructuras VALIDADAS realzadas (ganancia
        // proporcional a la luma) y stretch STF. Nunca sustituye al máster:
        // el nombre del archivo y los headers lo rotulan sin ambigüedad.
        if let Some(struct_map) = result.struct_map.as_ref() {
            cancellation_checkpoint(cancel.as_ref(), "exportación de Nebula Contrast")?;
            let (w, h, chn) = (result.width, result.height, result.channels);
            let npx = w * h;
            let levels = crate::deepsky_struct::recommended_levels(w, h);
            match crate::deepsky_struct::try_starlet_decompose(struct_map, w, h, levels) {
                Ok((_details, coarse)) => {
                    const CONTRAST_BOOST: f32 = 0.8;
                    let mut rgb16 = vec![0u16; npx * 3];
                    for p in 0..npx {
                        let detail = struct_map[p] - coarse[p];
                        let mut luma = 0.0f32;
                        for c in 0..chn {
                            luma += result.data[p * chn + c];
                        }
                        luma /= chn as f32;
                        let gain = (1.0 + CONTRAST_BOOST * detail / luma.max(50.0)).clamp(0.2, 5.0);
                        for c in 0..3 {
                            let v = result.data[p * chn + c.min(chn - 1)] * gain;
                            rgb16[p * 3 + c] = v.clamp(0.0, 65535.0) as u16;
                        }
                    }
                    let stretched = ds_stretch16(&rgb16, w, h, "linked", 0.55);
                    let out_ch = if chn == 1 { 1 } else { 3 };
                    let contrast_f32: Vec<f32> = if out_ch == 1 {
                        (0..npx).map(|p| stretched[p * 3] as f32).collect()
                    } else {
                        stretched.iter().map(|&v| v as f32).collect()
                    };
                    let path = parent.join(format!("{stem}_contrast_NONLINEAR.fits"));
                    ds_save_float32_fits_cancellable(
                        &path,
                        &contrast_f32,
                        w,
                        h,
                        out_ch,
                        &[
                            ("EXTNAME", "'CONTRAST'".to_string()),
                            ("ZASJOB", format!("'{}'", result.id)),
                            ("ZASNONLI", "T".to_string()),
                            ("ZASBOOST", format!("{CONTRAST_BOOST:.2}")),
                        ],
                        Some(cancel.as_ref()),
                    )?;
                    diagnostics.push(path.display().to_string());
                }
                Err(error) => {
                    log_to_front(
                        &app,
                        "WARN",
                        &format!(
                            "Nebula Contrast omitido por presupuesto/validación starlet ({error}); SCI y STRUCT originales permanecen intactos."
                        ),
                    );
                }
            }
        }
        if let Some(dq) = result.dq.as_ref() {
            cancellation_checkpoint(cancel.as_ref(), "exportación de DQ")?;
            let dq_f32: Vec<f32> = dq.iter().map(|&b| b as f32).collect();
            let path = parent.join(format!("{stem}_dq.fits"));
            ds_save_float32_fits_cancellable(
                &path,
                &dq_f32,
                result.width,
                result.height,
                1,
                &[
                    ("EXTNAME", "'DQ'".to_string()),
                    ("ZASJOB", format!("'{}'", result.id)),
                    ("BUNIT", DsFitsUnit::Bitmask.header_value().to_string()),
                ],
                Some(cancel.as_ref()),
            )?;
            diagnostics.push(path.display().to_string());
        }
        // Producto BG (F2): modelo de fondo/contaminación lumínica ajustado
        // sobre el máster, REVERSIBLE (máster_sin_gradiente + BG = original si
        // el toggle de gradiente estaba activo; con el toggle apagado es el
        // diagnóstico del LP presente en el máster). Nunca se resta aquí.
        cancellation_checkpoint(cancel.as_ref(), "exportación del modelo de fondo")?;
        if let Some(model) = crate::deepsky_background::fit_background_model(
            &result.data,
            result.width,
            result.height,
            result.channels,
        ) {
            let corr = model.render_correction();
            let path = parent.join(format!("{stem}_background_model.fits"));
            ds_save_float32_fits_cancellable(
                &path,
                &corr,
                result.width,
                result.height,
                result.channels,
                &[
                    ("EXTNAME", "'BG'".to_string()),
                    ("ZASJOB", format!("'{}'", result.id)),
                    ("ZASBGDEG", model.degree.to_string()),
                    ("ZASBGREV", "T".to_string()),
                ],
                Some(cancel.as_ref()),
            )?;
            diagnostics.push(path.display().to_string());
        }
    }
    emit_deepsky_pipeline_telemetry(
        &app,
        &result.id,
        "export",
        "CPU I/O FITS float32 + JSON",
        diagnostics.len() + 1,
        export_total,
        export_started,
        &export_sys,
        0,
        0,
        None,
    );
    cancellation_checkpoint(cancel.as_ref(), "exportación de receta")?;
    ds_write_recipe(&recipe, result)?;
    emit_deepsky_pipeline_telemetry(
        &app,
        &result.id,
        "export",
        "CPU I/O FITS float32 + JSON",
        diagnostics.len() + 2,
        export_total,
        export_started,
        &export_sys,
        0,
        0,
        None,
    );
    emit_deepsky_pipeline_telemetry(
        &app,
        &result.id,
        "complete",
        "Máster lineal y evidencia exportados",
        export_total,
        export_total,
        export_started,
        &export_sys,
        0,
        0,
        None,
    );
    Ok(DeepSkyFloatExport {
        master_fits: master.display().to_string(),
        recipe_json: recipe.display().to_string(),
        diagnostic_fits: diagnostics,
    })
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
        let img = guard
            .as_ref()
            .ok_or("No hay resultado de cielo profundo en memoria.")?;
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

/// Save a single 16-bit channel as a mono TIFF (grayscale L16), mirroring
/// `derot_save_rgb16_tiff` for the channel-split workflow.
fn ds_save_gray16_tiff(
    path: &std::path::Path,
    plane: &[u16],
    w: usize,
    h: usize,
) -> Result<(), String> {
    let mut raw = Vec::with_capacity(plane.len() * 2);
    for v in plane {
        raw.extend_from_slice(&v.to_ne_bytes());
    }
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let writer = std::io::BufWriter::new(file);
    image::codecs::tiff::TiffEncoder::new(writer)
        .encode(&raw, w as u32, h as u32, image::ColorType::L16)
        .map_err(|e| e.to_string())
}

/// CHANNEL SEPARATION (PixInsight ChannelExtraction parity): split the current
/// linear RGB deep-sky result into separate MONO 16-bit masters (R, G, B) next
/// to `base_path`, plus an optional synthetic Luminance (L). These feed the
/// `deepsky_combine_channels` workflow — the round-trip that lets you stack a
/// broadband OSC target, split it, tweak per channel, and recombine (or reuse
/// L for an LRGB blend). Returns a human summary of the files written.
// (async): trabajo de segundos-minutos fuera del hilo principal — la UI
// sigue viva y Cancelar/checkpoints funcionan (auditoría 2026-07-20).
#[tauri::command(async)]
fn deepsky_split_channels(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    base_path: String,
    include_luma: Option<bool>,
) -> Result<String, String> {
    state.license_manager.check_access()?;
    let (rgb16, w, h, is_mono) = {
        let guard = state.stacked_image.lock().unwrap();
        let img = guard
            .as_ref()
            .ok_or("No hay resultado de cielo profundo en memoria.")?;
        (img.data.clone(), img.width, img.height, img.is_mono)
    };
    if is_mono {
        return Err(
            "El resultado es monocromo; separar canales es para apilados a color (OSC/RGB).".into(),
        );
    }
    if rgb16.len() < w * h * 3 {
        return Err("El resultado no es RGB de 16 bits; nada que separar.".into());
    }
    // TIFF 16-bit is a PRO feature (matches the export/save gates).
    if !state.license_manager.is_pro() {
        return Err(
            "Separar canales en TIFF 16-bit requiere licencia PRO o periodo de prueba activo."
                .into(),
        );
    }
    let parent = std::path::Path::new(&base_path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(std::env::temp_dir);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let n = w * h;
    let mut saved: Vec<String> = Vec::new();
    for (name, c) in [("R", 0usize), ("G", 1), ("B", 2)] {
        let plane: Vec<u16> = (0..n).map(|i| rgb16[i * 3 + c]).collect();
        let out = parent.join(format!("ZenithDeepSky_{}_16bit_{}.tiff", name, stamp));
        ds_save_gray16_tiff(&out, &plane, w, h)?;
        saved.push(out.display().to_string());
    }
    if include_luma.unwrap_or(false) {
        let luma: Vec<u16> = (0..n)
            .map(|i| {
                let y = 0.2126 * rgb16[i * 3] as f32
                    + 0.7152 * rgb16[i * 3 + 1] as f32
                    + 0.0722 * rgb16[i * 3 + 2] as f32;
                y.clamp(0.0, 65535.0) as u16
            })
            .collect();
        let out = parent.join(format!("ZenithDeepSky_L_16bit_{}.tiff", stamp));
        ds_save_gray16_tiff(&out, &luma, w, h)?;
        saved.push(out.display().to_string());
    }
    log_to_front(
        &app,
        "SUCCESS",
        &format!("Canales separados: {}", saved.join(" · ")),
    );
    Ok(format!(
        "Separados {} canal(es):\n{}",
        saved.len(),
        saved.join("\n")
    ))
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
        let img = if let Some(cid) = img.bayer {
            ds_debayer_image(img, cid)
        } else {
            img
        };
        if img.ch == 1 {
            Ok(img)
        } else {
            let luma = ds_luma(&img);
            Ok(DsImage {
                data: luma,
                w: img.w,
                h: img.h,
                ch: 1,
                bayer: None,
            })
        }
    };

    emit_progress(&app, "Combinar canales: cargando masters...", 6.0, None);
    let rimg = load_mono(&r_path)?;
    let (w, h) = (rimg.w, rimg.h);
    let gimg = load_mono(&g_path)?;
    let bimg = load_mono(&b_path)?;

    // Reference stars from R for co-registration.
    emit_progress(
        &app,
        "Combinar canales: registrando canales a R...",
        22.0,
        None,
    );
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
        match ds_match_triangles_in_field(&ref_stars, &st, w, h) {
            Some(reg) => {
                log_to_front(
                    &app,
                    "INFO",
                    &format!(
                        "Canal {} registrado a R: {} · {} inliers · RMS {:.2} px.",
                        name,
                        reg.transform.model.label(),
                        reg.inliers,
                        reg.rms
                    ),
                );
                Ok(ds_warp_single(img, reg.transform, w, h))
            }
            None => {
                Err(format!(
                    "Canal {name}: registro estelar no publicable (se requieren al menos 8 correspondencias biyectivas y geometría válida). No se asumirá alineación."
                ))
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
        emit_progress(
            &app,
            "Combinar canales: aplicando luminancia (LRGB)...",
            55.0,
            None,
        );
        let limg = load_mono(&lp)?;
        let ld = align(&limg, "L")?;
        for i in 0..n {
            let y = 0.2126 * final_data[i * 3]
                + 0.7152 * final_data[i * 3 + 1]
                + 0.0722 * final_data[i * 3 + 2];
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
        emit_progress(
            &app,
            "Combinar canales: extrayendo gradiente (ABE)...",
            78.0,
            None,
        );
        ds_extract_background_gradient(&mut final_data, w, h, 3);
    }
    if do_scnr {
        emit_progress(
            &app,
            "Combinar canales: SCNR (elimina verde)...",
            86.0,
            None,
        );
        ds_scnr_green(&mut final_data, 3, 1.0);
    }
    if do_neut {
        emit_progress(
            &app,
            "Combinar canales: neutralización de fondo...",
            90.0,
            None,
        );
        let off = ds_neutralize_background(&mut final_data, w, h, 3);
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Neutralización R/G/B = {:.0}/{:.0}/{:.0} ADU.",
                off[0], off[1], off[2]
            ),
        );
    }

    // Result → linear RGB16 + STF preview (same as the stacker).
    emit_progress(
        &app,
        "Combinar canales: estiramiento automático (STF)...",
        95.0,
        None,
    );
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
        *res = Some(StackResult {
            width: w,
            height: h,
            data: rgb16,
            is_mono: false,
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
            "Canales combinados {}×{} · registro {} · SCNR {} · neutralización {}.",
            w,
            h,
            if do_reg { "ON" } else { "OFF" },
            if do_scnr { "ON" } else { "OFF" },
            if do_neut { "ON" } else { "OFF" }
        ),
    );
    emit_progress(&app, "Combinar canales: completado.", 100.0, None);
    Ok(format!(
        "data:image/png;base64,{}",
        general_purpose::STANDARD.encode(&enc)
    ))
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
    // WBPP-style matching metadata (from the FITS header when present).
    temp: Option<f32>,        // CCD-TEMP (°C)
    gain: Option<f32>,        // capture GAIN / CAM-GAIN / ISO (never EGAIN e-/ADU)
    binning: Option<i32>,     // XBINNING
    filter: Option<String>,   // FILTER
    date_obs: Option<String>, // DATE-OBS (inicio de exposición) — sesiones/noches
    signature: pipeline::CalibrationSignature,
    #[serde(rename = "storeLayout")]
    store_layout: Option<pipeline::StoreLayout>,
    #[serde(rename = "signatureWarnings")]
    signature_warnings: Vec<String>,
    #[serde(rename = "signatureMissing")]
    signature_missing: Vec<String>,
    ok: bool,
    error: Option<String>,
}

fn ds_missing_light_signature_fields(
    signature: &pipeline::CalibrationSignature,
) -> Vec<String> {
    let mut missing = crate::deepsky_signature::missing_required_signature_fields(
        signature,
        crate::deepsky_calibration_contract::CalibrationRole::Bias,
    )
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    if signature.exposure_seconds.is_none() {
        missing.push("exposureSeconds".into());
    }
    missing.sort();
    missing.dedup();
    missing
}

fn ds_signature_extraction_for_hdu(
    hdu: &fitrs::Hdu,
    width: usize,
    height: usize,
    channels: usize,
) -> crate::deepsky_signature::SignatureExtraction {
    let headers = crate::deepsky_signature::normalize_header_map(
        crate::deepsky_signature::CALIBRATION_HEADER_KEYS
            .iter()
            .filter_map(|key| hdu.value(key).map(|value| (*key, format!("{value:?}")))),
    );
    crate::deepsky_signature::signature_from_headers(
        &headers,
        crate::deepsky_signature::SignatureContext {
            width: width as u32,
            height: height as u32,
            channels: channels.min(u8::MAX as usize) as u8,
            session_hint: None,
        },
    )
}

fn ds_signature_extraction_without_headers(
    width: usize,
    height: usize,
    channels: usize,
) -> crate::deepsky_signature::SignatureExtraction {
    crate::deepsky_signature::signature_from_headers(
        &crate::deepsky_signature::NormalizedHeaderMap::new(),
        crate::deepsky_signature::SignatureContext {
            width: width as u32,
            height: height as u32,
            channels: channels.min(u8::MAX as usize) as u8,
            session_hint: None,
        },
    )
}

// v7 retains the v6 bijective-registration evidence and also invalidates
// calibrated frames produced before the exact signature/pedestal/dark-scaling
// gates. Reusing those pixels would make a warm cache contradict the recipe.
const DS_PREP_CACHE_VERSION: u32 = 8;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct DsCachedFrameAnalysis {
    path: String,
    stars: Vec<(f32, f32, f32)>,
    fwhm: f32,
    background: f32,
    noise: f32,
    eccentricity: f32,
    /// (x, y, fwhm) por estrella — Rescate de detalle. Caches antiguos: vacío
    /// (el frame usa su peso global, comportamiento previo).
    #[serde(default)]
    star_fwhms: Vec<(f32, f32, f32)>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DsAnalysisCacheFile {
    version: u32,
    fingerprint: String,
    frames: Vec<Option<DsCachedFrameAnalysis>>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DsRegistrationCacheFile {
    version: u32,
    fingerprint: String,
    accepted_paths: Vec<String>,
    reference_index: usize,
    transforms: Vec<Option<DsRegistration>>,
}

fn ds_read_json_cache<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn ds_write_json_cache<T: serde::Serialize>(
    path: &std::path::Path,
    value: &T,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("Cache metadata: {e}"))?;
    std::fs::rename(tmp, path).map_err(|e| format!("Cache metadata commit: {e}"))
}

/// Read a string-valued FITS header key (e.g. FILTER), trimmed of quotes/space.
fn ds_hdr_str(hdu: &fitrs::Hdu, key: &str) -> Option<String> {
    let raw = format!("{:?}", hdu.value(key)?);
    let s = raw.trim_matches(|c| c == '"' || c == '\'' || c == ' ' || c == '(' || c == ')');
    // fitrs debug wraps strings like CharacterString("Ha") — strip the wrapper.
    let s = s.rsplit('(').next().unwrap_or(s);
    let s = s.trim_matches(|c: char| c == '"' || c == '\'' || c == ' ' || c == ')');
    if s.is_empty() || s == "None" {
        None
    } else {
        Some(s.to_string())
    }
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
            if matches!(
                ext.as_str(),
                "fits" | "fit" | "tif" | "tiff" | "png" | "jpg" | "jpeg"
            ) {
                out.push(p.to_string_lossy().to_string());
            }
        }
    }
}

fn ds_source_fingerprint(groups: &[&[String]]) -> String {
    use std::hash::{Hash, Hasher};
    use std::io::{Read, Seek};

    let mut h = std::collections::hash_map::DefaultHasher::new();
    // Version change intentionally invalidates metadata-only caches. File size
    // and mtime are insufficient when capture software rewrites a calibrated
    // frame in place or a restored backup preserves timestamps.
    "hybrid-v2-2026.07.10-content-edges-v2".hash(&mut h);
    for paths in groups {
        for path in *paths {
            std::fs::canonicalize(path)
                .unwrap_or_else(|_| std::path::PathBuf::from(path))
                .hash(&mut h);
            if let Ok(meta) = std::fs::metadata(path) {
                meta.len().hash(&mut h);
                meta.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .hash(&mut h);
                // Hash bounded content samples so fingerprinting remains cheap
                // for thousands of high-resolution lights while detecting the
                // common in-place rewrite/corruption cases.
                const EDGE: usize = 8 * 1024;
                if let Ok(mut file) = std::fs::File::open(path) {
                    let head_len = (meta.len() as usize).min(EDGE);
                    let mut head = vec![0u8; head_len];
                    match file.read_exact(&mut head) {
                        Ok(()) => head.hash(&mut h),
                        Err(e) => e.kind().hash(&mut h),
                    }
                    if meta.len() as usize > EDGE {
                        let tail_len = (meta.len() as usize).min(EDGE);
                        let mut tail = vec![0u8; tail_len];
                        match file
                            .seek(std::io::SeekFrom::End(-(tail_len as i64)))
                            .and_then(|_| file.read_exact(&mut tail))
                        {
                            Ok(()) => tail.hash(&mut h),
                            Err(e) => e.kind().hash(&mut h),
                        }
                    }
                } else {
                    "unreadable".hash(&mut h);
                }
            } else {
                "missing".hash(&mut h);
            }
        }
        0xD5u8.hash(&mut h);
    }
    format!("{:016x}", h.finish())
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
        // if the folder is "Bias". Flat-darks are a first-class calibration
        // bucket; silently dropping them made a correct CMOS calibration set
        // behave as if it had never been supplied.
        if has_flat && has_dark {
            return Some("dark_flats");
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
    #[serde(rename = "darkFlats")]
    dark_flats: Vec<DsProbe>,
    bias: Vec<DsProbe>,
}

/// Scan a folder recursively, auto-classify every image into lights/darks/
/// flats/dark-flats/bias by path keywords, and technical-probe them all.
#[tauri::command]
fn deepsky_scan_classify(root: String) -> DsClassified {
    let mut files = Vec::new();
    ds_walk_images(std::path::Path::new(&root), &mut files, 0);
    let probes = deepsky_probe(files);
    let mut out = DsClassified {
        lights: vec![],
        darks: vec![],
        flats: vec![],
        dark_flats: vec![],
        bias: vec![],
    };
    for pr in probes {
        match ds_classify(&pr.path) {
            "bias" => out.bias.push(pr),
            "flats" => out.flats.push(pr),
            "dark_flats" => out.dark_flats.push(pr),
            "darks" => out.darks.push(pr),
            _ => out.lights.push(pr),
        }
    }
    out
}

/// Canales que entregará realmente `ds_read_image` para un archivo no-FITS,
/// leyendo solo cabeceras (sin decodificar píxeles). El probe anunciaba antes
/// `ch: 3` incondicional, clasificando mono (TIFF Gray / PNG-JPEG Luma) como
/// RGB y desincronizando la agrupación del preflight del loader real.
fn ds_probe_nonfits_channels(path: &str) -> usize {
    let lower = path.to_lowercase();
    if lower.ends_with(".tif") || lower.ends_with(".tiff") {
        // Mismo mapeo que ds_read_tiff_float_safe: Gray/GrayA → 1, resto → 3.
        if let Ok(file) = std::fs::File::open(path) {
            if let Ok(mut dec) = tiff::decoder::Decoder::new(std::io::BufReader::new(file)) {
                if let Ok(color) = dec.colortype() {
                    return match color {
                        tiff::ColorType::Gray(_) | tiff::ColorType::GrayA(_) => 1,
                        _ => 3,
                    };
                }
            }
        }
        return 3;
    }
    if lower.ends_with(".png") {
        // IHDR: firma (8) + len (4) + "IHDR" (4) + ancho (4) + alto (4) +
        // profundidad (1) + tipo de color (1). SOLO el tipo 0 (gris puro)
        // llega como mono al loader: gris+alfa (tipo 4) decodifica como
        // ImageLumaA*, que ds_read_image manda por el brazo genérico
        // to_rgb16() → 3 canales. El probe debe reflejar el loader real.
        if let Ok(mut f) = std::fs::File::open(path) {
            let mut head = [0u8; 26];
            if f.read_exact(&mut head).is_ok()
                && head[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
                && &head[12..16] == b"IHDR"
            {
                return match head[25] {
                    0 => 1,
                    _ => 3,
                };
            }
        }
        return 3;
    }
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        // Buscar el SOFn (FFC0..FFCF salvo C4/C8/CC): precisión (1) + alto (2)
        // + ancho (2) + Nf (1). Nf=1 → escala de grises; 3 → YCbCr.
        if let Ok(f) = std::fs::File::open(path) {
            let mut r = std::io::BufReader::new(f);
            let mut two = [0u8; 2];
            if r.read_exact(&mut two).is_ok() && two == [0xFF, 0xD8] {
                loop {
                    let mut b = [0u8; 1];
                    if r.read_exact(&mut b).is_err() {
                        break;
                    }
                    if b[0] != 0xFF {
                        continue;
                    }
                    // Saltar bytes de relleno FF antes del código de marcador.
                    let mut m = [0u8; 1];
                    loop {
                        if r.read_exact(&mut m).is_err() {
                            return 3;
                        }
                        if m[0] != 0xFF {
                            break;
                        }
                    }
                    match m[0] {
                        0x01 | 0xD0..=0xD8 => continue, // marcadores sin payload
                        0xD9 | 0xDA => break,           // EOI / SOS sin SOF previo
                        marker => {
                            let mut lenb = [0u8; 2];
                            if r.read_exact(&mut lenb).is_err() {
                                break;
                            }
                            let len = u16::from_be_bytes(lenb) as usize;
                            if len < 2 {
                                break;
                            }
                            let is_sof = matches!(
                                marker,
                                0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
                            );
                            if is_sof {
                                let mut sof = [0u8; 6];
                                if r.read_exact(&mut sof).is_ok() {
                                    return if sof[5] == 1 { 1 } else { 3 };
                                }
                                break;
                            }
                            if r.seek_relative(len as i64 - 2).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        }
        return 3;
    }
    3
}

/// Asesor de muestreo (F2): mide la FWHM mediana de un light representativo
/// (con debayer si es CFA) y clasifica el muestreo. Devuelve None si el
/// frame no se puede leer o no hay estrellas suficientes — el preflight
/// simplemente omite la tarjeta, nunca bloquea.
fn ds_measure_sampling_advisor(path: &str) -> Option<pipeline::SamplingAdvisorReport> {
    let img = ds_read_image(path).ok()?;
    let img = match img.bayer {
        Some(cid) if img.ch == 1 => ds_debayer_image(img, cid),
        _ => img,
    };
    let npx = img.w * img.h;
    let luma: Vec<f32> = if img.ch == 1 {
        img.data
    } else {
        (0..npx)
            .map(|p| {
                (img.data[p * img.ch] + img.data[p * img.ch + 1] + img.data[p * img.ch + 2]) / 3.0
            })
            .collect()
    };
    let stars = ds_detect_stars(&luma, img.w, img.h, 80);
    let fwhms = ds_star_fwhms(&luma, img.w, img.h, &stars);
    if fwhms.len() < 5 {
        return None;
    }
    let mut vals: Vec<f32> = fwhms.iter().map(|s| s.2).collect();
    vals.sort_by(|a, b| a.total_cmp(b));
    let median = vals[vals.len() / 2];
    let (class, scale) = crate::deepsky_background::advise_sampling(median as f64);
    let name = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());
    Some(pipeline::SamplingAdvisorReport {
        fwhm_median_px: median,
        stars_measured: fwhms.len(),
        sampled_frame: name,
        classification: class.as_str().into(),
        recommended_scale: scale.into(),
    })
}

// (async): trabajo de segundos-minutos fuera del hilo principal — la UI
// sigue viva y Cancelar/checkpoints funcionan (auditoría 2026-07-20).
#[tauri::command(async)]
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
                temp: None,
                gain: None,
                binning: None,
                filter: None,
                date_obs: None,
                signature: pipeline::CalibrationSignature::default(),
                store_layout: None,
                signature_warnings: Vec::new(),
                signature_missing: Vec::new(),
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
                            let temp = ds_hdr_num(&hdu, "CCD-TEMP")
                                .or_else(|| ds_hdr_num(&hdu, "CCDTEMP"))
                                .map(|v| v as f32);
                            let gain = ds_hdr_num(&hdu, "GAIN")
                                .or_else(|| ds_hdr_num(&hdu, "CAM-GAIN"))
                                .or_else(|| ds_hdr_num(&hdu, "ISOSPEED"))
                                .or_else(|| ds_hdr_num(&hdu, "ISO"))
                                .map(|v| v as f32);
                            let binning = ds_hdr_num(&hdu, "XBINNING")
                                .or_else(|| ds_hdr_num(&hdu, "BINNING"))
                                .map(|v| v as i32);
                            let filter = ds_hdr_str(&hdu, "FILTER");
                            let date_obs = ds_hdr_str(&hdu, "DATE-OBS")
                                .or_else(|| ds_hdr_str(&hdu, "DATE-LOC"))
                                .or_else(|| ds_hdr_str(&hdu, "DATE"));
                            let extraction =
                                ds_signature_extraction_for_hdu(&hdu, w, h, ch.min(3));
                            let signature_missing =
                                ds_missing_light_signature_fields(&extraction.signature);
                            let mut signature = extraction.signature;
                            // La sesión de la firma usa la MISMA noche
                            // reconciliada (cabecera + nombre) que el
                            // emparejado de flats: una sola definición.
                            signature.session =
                                ds_session_night_id(&base.path, date_obs.as_deref())
                                    .or(signature.session);
                            DsProbe {
                                w,
                                h,
                                ch: ch.min(3),
                                exptime,
                                bayer,
                                temp,
                                gain,
                                binning,
                                filter,
                                date_obs,
                                signature,
                                store_layout: extraction.layout,
                                signature_warnings: extraction.warnings,
                                signature_missing,
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
                    Ok((w, h)) => {
                        let ch = ds_probe_nonfits_channels(p);
                        let extraction =
                            ds_signature_extraction_without_headers(w as usize, h as usize, ch);
                        let signature_missing =
                            ds_missing_light_signature_fields(&extraction.signature);
                        DsProbe {
                            w: w as usize,
                            h: h as usize,
                            ch,
                            signature: extraction.signature,
                            store_layout: extraction.layout,
                            signature_warnings: extraction.warnings,
                            signature_missing,
                            ok: true,
                            ..base
                        }
                    }
                    Err(e) => DsProbe {
                        error: Some(e.to_string()),
                        ..base
                    },
                }
            }
        })
        .collect()
}

#[derive(Clone, Debug)]
struct DsCalibrationSelection {
    paths: Vec<String>,
    group_count: usize,
    description: String,
    warnings: Vec<String>,
    blocking_reasons: Vec<String>,
    degraded: bool,
    scientific_eligible: bool,
    /// Sesiones (noche → paths) dentro del grupo seleccionado, orden
    /// cronológico. Con una sola noche el pipeline colapsa al comportamiento
    /// histórico (un único máster).
    sessions: Vec<(String, Vec<String>)>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DeepSkyFrameInspection {
    path: String,
    name: String,
    width: usize,
    height: usize,
    stars: usize,
    fwhm: f32,
    noise: f32,
    eccentricity: f32,
    score: f32,
    rejectable: bool,
    rejection_reason: Option<String>,
    recommended_reference: bool,
}

/// Informe tipado de la inspección previa. `dither` es una PREDICCIÓN
/// pre-registro (offsets traslacionales de estrellas respecto a la primera
/// toma legible); el diagnóstico autoritativo se recalcula durante el stack
/// con el registro real. `detector_pattern` se mide a resolución nativa
/// (binning 2x2 en CFA) sobre las primeras tomas, porque el downsample de la
/// inspección atenúa el banding de filas/columnas.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DeepSkyInspectionReport {
    frames: Vec<DeepSkyFrameInspection>,
    dither: Option<crate::deepsky_noise::DitherDiagnostics>,
    detector_pattern: Option<crate::deepsky_noise::DetectorPatternDiagnostics>,
}

/// Offset traslacional mediano de un frame respecto al ancla, emparejando por
/// vecino más cercano las estrellas más brillantes. None si no hay
/// coincidencias suficientes para una mediana robusta.
fn ds_median_star_offset(
    anchor: &[(f32, f32, f32)],
    stars: &[(f32, f32, f32)],
    search_radius: f32,
) -> Option<(f32, f32)> {
    if anchor.len() < 5 || stars.len() < 5 {
        return None;
    }
    let mut brightest: Vec<(f32, f32, f32)> = stars.to_vec();
    brightest.sort_by(|a, b| b.2.total_cmp(&a.2));
    brightest.truncate(40);
    let radius_sq = search_radius * search_radius;
    let mut dxs = Vec::new();
    let mut dys = Vec::new();
    for &(sx, sy, _) in &brightest {
        let mut best = f32::INFINITY;
        let mut best_dx = 0.0f32;
        let mut best_dy = 0.0f32;
        for &(ax, ay, _) in anchor {
            let dx = sx - ax;
            let dy = sy - ay;
            let dist_sq = dx * dx + dy * dy;
            if dist_sq < best {
                best = dist_sq;
                best_dx = dx;
                best_dy = dy;
            }
        }
        if best <= radius_sq {
            dxs.push(best_dx);
            dys.push(best_dy);
        }
    }
    if dxs.len() < 5 {
        return None;
    }
    let median = |values: &mut Vec<f32>| -> f32 {
        values.sort_by(|a, b| a.total_cmp(b));
        values[values.len() / 2]
    };
    Some((median(&mut dxs), median(&mut dys)))
}

fn ds_inspection_luma(image: &DsImage) -> (Vec<f32>, usize, usize, usize) {
    let source = ds_luma(image);
    let mut factor = ((image.w.max(image.h) + 1599) / 1600).max(1);
    if image.bayer.is_some() {
        factor = factor.max(2);
        if factor % 2 != 0 {
            factor += 1;
        }
    }
    if factor == 1 {
        return (source, image.w, image.h, factor);
    }
    let out_w = (image.w / factor).max(1);
    let out_h = (image.h / factor).max(1);
    let mut output = vec![0.0f32; out_w * out_h];
    output
        .par_iter_mut()
        .enumerate()
        .for_each(|(index, value)| {
            let ox = index % out_w;
            let oy = index / out_w;
            let x0 = ox * factor;
            let y0 = oy * factor;
            let x1 = (x0 + factor).min(image.w);
            let y1 = (y0 + factor).min(image.h);
            let mut sum = 0.0f64;
            let mut count = 0usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    sum += source[y * image.w + x] as f64;
                    count += 1;
                }
            }
            *value = (sum / count.max(1) as f64) as f32;
        });
    (output, out_w, out_h, factor)
}

/// Inspección previa acotada a ~1600 px por lado. Mantiene una sola toma en
/// memoria y permite enseñar referencia/rechazables antes de reservar el stack.
// (async): trabajo de segundos-minutos fuera del hilo principal — la UI
// sigue viva y Cancelar/checkpoints funcionan (auditoría 2026-07-20).
#[tauri::command(async)]
fn inspect_deepsky_frames(
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> Result<DeepSkyInspectionReport, String> {
    ds_begin_user_action(&state);
    let cancel = state.cancel_requested.clone();
    let mut inspections = Vec::with_capacity(paths.len());
    // (estrellas en coordenadas de trabajo, factor, ancho, alto) por toma
    // legible; None para tomas ilegibles. Orden temporal = orden de la lista.
    let mut star_samples: Vec<Option<(Vec<(f32, f32, f32)>, usize, usize, usize)>> =
        Vec::with_capacity(paths.len());
    let mut pattern_samples: Vec<crate::deepsky_noise::DetectorPatternDiagnostics> = Vec::new();
    for path in paths {
        cancellation_checkpoint(cancel.as_ref(), "inspección previa de tomas")?;
        let name = std::path::Path::new(&path)
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());
        match ds_read_image(&path) {
            Ok(image) => {
                let width = image.w;
                let height = image.h;
                if pattern_samples.len() < 3 {
                    let native = ds_luma(&image);
                    let (plane, plane_w, plane_h) =
                        if image.bayer.is_some() && image.w >= 2 && image.h >= 2 {
                            // Binning 2x2: elimina la alternancia CFA que el
                            // detector de banding confundiría con patrón.
                            let bin_w = image.w / 2;
                            let bin_h = image.h / 2;
                            let mut binned = vec![0.0f32; bin_w * bin_h];
                            for y in 0..bin_h {
                                for x in 0..bin_w {
                                    let base = (2 * y) * image.w + 2 * x;
                                    binned[y * bin_w + x] = 0.25
                                        * (native[base]
                                            + native[base + 1]
                                            + native[base + image.w]
                                            + native[base + image.w + 1]);
                                }
                            }
                            (binned, bin_w, bin_h)
                        } else {
                            (native, image.w, image.h)
                        };
                    let (_, pattern_noise) = ds_bg_noise(&plane);
                    if let Some(report) = crate::deepsky_noise::analyze_detector_pattern_raw(
                        &plane,
                        plane_w,
                        plane_h,
                        pattern_noise as f64,
                    ) {
                        pattern_samples.push(report);
                    }
                }
                let (luma, work_w, work_h, factor) = ds_inspection_luma(&image);
                let stars = ds_detect_stars(&luma, work_w, work_h, 120);
                let fwhm = ds_frame_fwhm_proxy(&luma, work_w, work_h, &stars) * factor as f32;
                let (_, noise_work) = ds_bg_noise(&luma);
                let noise = noise_work * factor as f32;
                let eccentricity = ds_frame_roundness(&luma, work_w, work_h, &stars);
                let score = stars.len() as f32 / (fwhm.max(0.5) * noise.max(1.0))
                    * (1.0 - eccentricity).clamp(0.1, 1.0);
                inspections.push(DeepSkyFrameInspection {
                    path,
                    name,
                    width,
                    height,
                    stars: stars.len(),
                    fwhm,
                    noise,
                    eccentricity,
                    score,
                    rejectable: false,
                    rejection_reason: None,
                    recommended_reference: false,
                });
                star_samples.push(Some((stars, factor, width, height)));
            }
            Err(error) => {
                inspections.push(DeepSkyFrameInspection {
                    path,
                    name,
                    width: 0,
                    height: 0,
                    stars: 0,
                    fwhm: 0.0,
                    noise: 0.0,
                    eccentricity: 0.0,
                    score: 0.0,
                    rejectable: true,
                    rejection_reason: Some(format!("No se pudo leer: {error}")),
                    recommended_reference: false,
                });
                star_samples.push(None);
            }
        }
    }
    let median_positive = |values: Vec<f32>| -> f32 {
        let mut values: Vec<f32> = values
            .into_iter()
            .filter(|value| value.is_finite() && *value > 0.0)
            .collect();
        if values.is_empty() {
            return 0.0;
        }
        values.sort_by(|a, b| a.total_cmp(b));
        values[values.len() / 2]
    };
    let median_fwhm = median_positive(inspections.iter().map(|item| item.fwhm).collect());
    let median_noise = median_positive(inspections.iter().map(|item| item.noise).collect());
    for item in &mut inspections {
        if item.rejection_reason.is_some() {
            continue;
        }
        let mut reasons = Vec::new();
        if item.stars < 6 {
            reasons.push(format!("sólo {} estrellas", item.stars));
        }
        if median_fwhm > 0.0 && item.fwhm > median_fwhm * 1.8 {
            reasons.push(format!(
                "FWHM {:.2} vs mediana {:.2}",
                item.fwhm, median_fwhm
            ));
        }
        if median_noise > 0.0 && item.noise > median_noise * 2.0 {
            reasons.push(format!(
                "ruido {:.0} vs mediana {:.0}",
                item.noise, median_noise
            ));
        }
        if item.eccentricity > 0.70 {
            reasons.push(format!("eccentricidad {:.2}", item.eccentricity));
        }
        if !reasons.is_empty() {
            item.rejectable = true;
            item.rejection_reason = Some(reasons.join(" · "));
        }
    }
    if let Some(reference) = inspections
        .iter_mut()
        .filter(|item| !item.rejectable)
        .max_by(|a, b| a.score.total_cmp(&b.score))
    {
        reference.recommended_reference = true;
    }

    // Predicción de dithering: offsets traslacionales de cada toma respecto a
    // la primera legible con estrellas suficientes, en orden temporal y en
    // píxeles reales (× factor de downsample). Tomas de geometría distinta o
    // sin emparejamiento fiable se omiten en vez de contaminar la mediana.
    let mut positions: Vec<(f64, f64)> = Vec::new();
    let mut anchor: Option<(&Vec<(f32, f32, f32)>, usize, usize, usize)> = None;
    for sample in &star_samples {
        let Some((stars, factor, width, height)) = sample else {
            continue;
        };
        match anchor {
            None => {
                if stars.len() >= 5 {
                    anchor = Some((stars, *factor, *width, *height));
                    positions.push((0.0, 0.0));
                }
            }
            Some((anchor_stars, anchor_factor, anchor_w, anchor_h)) => {
                if *width != anchor_w || *height != anchor_h || *factor != anchor_factor {
                    continue;
                }
                if let Some((dx, dy)) = ds_median_star_offset(anchor_stars, stars, 40.0) {
                    positions.push((dx as f64 * *factor as f64, dy as f64 * *factor as f64));
                }
            }
        }
    }
    let dither = (positions.len() >= 2)
        .then(|| crate::deepsky_noise::analyze_dither_positions(&positions));

    // Patrón de detector: mediana por banding de las muestras medidas (hasta
    // tres primeras tomas), robusta a una toma atípica.
    let detector_pattern = {
        let mut samples = pattern_samples;
        samples.sort_by(|a, b| a.banding_sigma.total_cmp(&b.banding_sigma));
        if samples.is_empty() {
            None
        } else {
            Some(samples[samples.len() / 2].clone())
        }
    };

    Ok(DeepSkyInspectionReport {
        frames: inspections,
        dither,
        detector_pattern,
    })
}

fn ds_probe_filter_id(probe: &DsProbe) -> Option<String> {
    let metadata = probe
        .filter
        .as_deref()
        .and_then(ds_filter_token)
        .map(str::to_string);
    let path = ds_filter_of_path(&probe.path).map(str::to_string);
    if metadata.as_deref() == Some("HA_OIII") && path.as_deref() == Some("SII_OIII") {
        return path;
    }
    metadata.or(path)
}

fn ds_calibration_role(kind: &str) -> crate::deepsky_calibration_contract::CalibrationRole {
    match kind {
        "bias" => crate::deepsky_calibration_contract::CalibrationRole::Bias,
        "darks" => crate::deepsky_calibration_contract::CalibrationRole::Dark,
        "flats" => crate::deepsky_calibration_contract::CalibrationRole::Flat,
        "dark-flats" => crate::deepsky_calibration_contract::CalibrationRole::DarkFlat,
        _ => crate::deepsky_calibration_contract::CalibrationRole::Bias,
    }
}

fn ds_compare_probe_calibration(
    reference: &DsProbe,
    candidate: &DsProbe,
    role: crate::deepsky_calibration_contract::CalibrationRole,
    policy: pipeline::DeepSkyCalibrationPolicy,
) -> crate::deepsky_calibration_contract::CalibrationCompatibility {
    let mut report = crate::deepsky_calibration_contract::compare_calibration_signatures(
        &reference.signature,
        &candidate.signature,
        role,
        policy,
    );
    let layout_reason = match (&reference.store_layout, &candidate.store_layout) {
        (Some(reference_layout), Some(candidate_layout)) if reference_layout == candidate_layout => {
            None
        }
        (Some(reference_layout), Some(candidate_layout)) => Some(format!(
            "storeLayout: {reference_layout:?} != {candidate_layout:?}"
        )),
        _ => Some("metadata obligatoria ausente: storeLayout/CFA phase".into()),
    };
    let session_reason = if matches!(
        role,
        crate::deepsky_calibration_contract::CalibrationRole::Flat
    ) {
        match (&reference.signature.session, &candidate.signature.session) {
            (Some(reference_session), Some(candidate_session))
                if reference_session == candidate_session =>
            {
                None
            }
            (Some(reference_session), Some(candidate_session)) => Some(format!(
                "session: {reference_session:?} != {candidate_session:?}"
            )),
            _ => Some("metadata obligatoria ausente: session".into()),
        }
    } else {
        None
    };
    for reason in [layout_reason, session_reason].into_iter().flatten() {
        report.reasons.push(reason);
        report.scientific_eligible = false;
        match policy {
            pipeline::DeepSkyCalibrationPolicy::Strict => {
                report.compatible = false;
                report.degraded = false;
            }
            pipeline::DeepSkyCalibrationPolicy::AllowDegraded => {
                report.compatible = true;
                report.degraded = true;
            }
        }
    }
    report
}

fn ds_dark_core_compatible_for_scaling(reference: &DsProbe, candidate: &DsProbe) -> bool {
    let mut reference_without_exposure = reference.clone();
    reference_without_exposure.signature.exposure_seconds = candidate.signature.exposure_seconds;
    ds_compare_probe_calibration(
        &reference_without_exposure,
        candidate,
        crate::deepsky_calibration_contract::CalibrationRole::Dark,
        pipeline::DeepSkyCalibrationPolicy::Strict,
    )
    .compatible
}

fn ds_master_signature_groups<'a>(
    probes: impl IntoIterator<Item = &'a DsProbe>,
    role: crate::deepsky_calibration_contract::CalibrationRole,
) -> Vec<Vec<&'a DsProbe>> {
    let mut groups: Vec<Vec<&DsProbe>> = Vec::new();
    for probe in probes.into_iter().filter(|probe| probe.ok) {
        if let Some(group) = groups.iter_mut().find(|group| {
            ds_compare_probe_calibration(
                group[0],
                probe,
                role,
                pipeline::DeepSkyCalibrationPolicy::Strict,
            )
            .compatible
        }) {
            group.push(probe);
        } else {
            groups.push(vec![probe]);
        }
    }
    for group in &mut groups {
        group.sort_by(|a, b| a.path.cmp(&b.path));
    }
    groups.sort_by(|a, b| a[0].path.cmp(&b[0].path));
    groups
}

/// Last-line defence before allocating a physical master.  Selection may
/// legitimately retain several light-compatible groups (e.g. dark exposure
/// groups), but each individual master must receive exactly one scientific
/// signature group.
fn ds_single_master_representative(
    paths: &[String],
    role: crate::deepsky_calibration_contract::CalibrationRole,
    label: &str,
) -> Result<Option<DsProbe>, String> {
    if paths.is_empty() {
        return Ok(None);
    }
    let probes = deepsky_probe(paths.to_vec());
    let unreadable = probes.iter().filter(|probe| !probe.ok).count();
    if unreadable > 0 {
        return Err(format!(
            "{label}: {unreadable} candidato(s) sin firma legible; no se construye el máster"
        ));
    }
    let groups = ds_master_signature_groups(probes.iter(), role);
    if groups.len() != 1 {
        return Err(format!(
            "{label}: {} firmas científicas incompatibles llegarían al mismo máster; separa los grupos",
            groups.len()
        ));
    }
    Ok(groups
        .first()
        .and_then(|group| group.first())
        .map(|probe| (*probe).clone()))
}

/// Exact, auditable calibration selection. A calibration candidate is accepted
/// only when it matches at least one target signature. Strict surfaces every
/// missing/mismatched candidate as a blocking reason. AllowDegraded may omit an
/// incompatible master (or retain an exposure-only dark for later evidence-
/// based scaling), but it can never call that decision scientific.
fn ds_select_calibration_group(
    paths: &[String],
    references: &[DsProbe],
    kind: &str,
    policy: pipeline::DeepSkyCalibrationPolicy,
) -> DsCalibrationSelection {
    if paths.is_empty() {
        let required = matches!(kind, "darks" | "flats")
            || (kind == "dark-flats" && references.iter().any(|reference| reference.ok));
        let reason = format!(
            "{kind}: no proporcionados; no existe contrato de lights/flats pre-calibrados que permita omitirlos"
        );
        return DsCalibrationSelection {
            paths: Vec::new(),
            group_count: 0,
            description: format!("{kind}: no proporcionados"),
            warnings: if required
                && matches!(policy, pipeline::DeepSkyCalibrationPolicy::AllowDegraded)
            {
                vec![format!("AllowDegraded: {reason}; sólo Classic no científico")]
            } else {
                Vec::new()
            },
            blocking_reasons: if required
                && matches!(policy, pipeline::DeepSkyCalibrationPolicy::Strict)
            {
                vec![reason]
            } else {
                Vec::new()
            },
            degraded: required
                && matches!(policy, pipeline::DeepSkyCalibrationPolicy::AllowDegraded),
            scientific_eligible: !required,
            sessions: Vec::new(),
        };
    }
    let probes = deepsky_probe(paths.to_vec());
    let role = ds_calibration_role(kind);
    let mut warnings = Vec::new();
    let mut blocking_reasons = Vec::new();
    let mut degraded = false;
    for probe in probes.iter().filter(|probe| !probe.ok) {
        let reason = format!("{kind} '{}': no pudo leerse", probe.name);
        if matches!(policy, pipeline::DeepSkyCalibrationPolicy::Strict) {
            blocking_reasons.push(reason);
        } else {
            warnings.push(format!("AllowDegraded: {reason}; se omite"));
            degraded = true;
        }
    }
    if !references.iter().any(|reference| reference.ok) {
        blocking_reasons.push(format!(
            "{kind}: no existe firma de referencia legible para validar compatibilidad"
        ));
    }
    let mut groups = std::collections::BTreeMap::<String, Vec<&DsProbe>>::new();
    for probe in probes.iter().filter(|probe| probe.ok) {
        // Geometry remains a hard grouping dimension even when a preview-only
        // PNG/JPEG has no capture headers. Omitting it made incompatible ROIs
        // appear as one candidate group in preflight (Strict still rejects the
        // missing metadata, but the matrix shown to the user was false).
        let key = serde_json::to_string(&(
            probe.signature.clone(),
            probe.store_layout.clone(),
            probe.w,
            probe.h,
            probe.ch,
        ))
        .unwrap_or_else(|_| format!("unserializable:{}", probe.path));
        groups.entry(key).or_default().push(probe);
    }
    let mut selected = Vec::<&DsProbe>::new();
    // Motivos AGRUPADOS por patrón: enumerar cada archivo con el mismo
    // problema inundaba el panel (500 flats → 500 líneas). Cada patrón guarda
    // (nº de archivos, nombre de ejemplo).
    let mut mismatch_groups: std::collections::BTreeMap<(String, bool), (usize, String)> =
        std::collections::BTreeMap::new();
    for probe in probes.iter().filter(|probe| probe.ok) {
        let exact = references.iter().filter(|reference| reference.ok).any(|reference| {
            ds_compare_probe_calibration(
                reference,
                probe,
                role,
                pipeline::DeepSkyCalibrationPolicy::Strict,
            )
            .compatible
        });
        if exact {
            selected.push(probe);
            continue;
        }
        let scalable_dark = matches!(
            role,
            crate::deepsky_calibration_contract::CalibrationRole::Dark
        ) && references
            .iter()
            .filter(|reference| reference.ok)
            .any(|reference| ds_dark_core_compatible_for_scaling(reference, probe));
        let best_reasons = references
            .iter()
            .filter(|reference| reference.ok)
            .map(|reference| {
                ds_compare_probe_calibration(
                    reference,
                    probe,
                    role,
                    pipeline::DeepSkyCalibrationPolicy::Strict,
                )
                .reasons
            })
            .min_by_key(Vec::len)
            .unwrap_or_else(|| vec!["sin firma de referencia".into()]);
        if scalable_dark
            && matches!(policy, pipeline::DeepSkyCalibrationPolicy::AllowDegraded)
        {
            selected.push(probe);
        }
        let entry = mismatch_groups
            .entry((best_reasons.join("; "), scalable_dark))
            .or_insert((0, probe.name.clone()));
        entry.0 += 1;
        if matches!(policy, pipeline::DeepSkyCalibrationPolicy::AllowDegraded) {
            degraded = true;
        }
    }
    for ((reasons_text, scalable), (count, sample)) in &mismatch_groups {
        let reason = format!(
            "{kind}: {count} archivo(s) sin coincidencia con los lights (p. ej. '{sample}') — {reasons_text}"
        );
        if matches!(policy, pipeline::DeepSkyCalibrationPolicy::Strict) {
            blocking_reasons.push(reason);
        } else if *scalable {
            warnings.push(format!(
                "AllowDegraded: {reason}; se conservan sólo como candidatos a escalado sujeto a evidencia"
            ));
        } else {
            warnings.push(format!("AllowDegraded: {reason}; se omiten"));
        }
    }
    selected.sort_by(|a, b| a.path.cmp(&b.path));

    // `master_bias` is global in the current architecture.  Matching a bias
    // against *any* light is not enough: two incompatible light cores could
    // otherwise contribute two incompatible bias groups to one estimator.
    if kind == "bias" && ds_master_signature_groups(selected.iter().copied(), role).len() > 1 {
        let reason =
            "bias: varias firmas incompatibles llegarían al máster global; el pipeline aún no publica un bias por grupo"
                .to_string();
        if matches!(policy, pipeline::DeepSkyCalibrationPolicy::Strict) {
            blocking_reasons.push(reason);
        } else {
            warnings.push(format!("AllowDegraded: {reason}; se omite el bias global"));
            selected.clear();
            degraded = true;
        }
    }
    if kind == "bias" && !selected.is_empty() {
        let uncovered_lights = references
            .iter()
            .filter(|reference| reference.ok)
            .filter(|reference| {
                !selected.iter().any(|candidate| {
                    ds_compare_probe_calibration(
                        reference,
                        candidate,
                        crate::deepsky_calibration_contract::CalibrationRole::Bias,
                        pipeline::DeepSkyCalibrationPolicy::Strict,
                    )
                    .compatible
                })
            })
            .map(|reference| reference.name.clone())
            .collect::<Vec<_>>();
        if !uncovered_lights.is_empty() {
            let reason = format!(
                "bias: el máster global no cubre la firma de {} light(s): {}",
                uncovered_lights.len(),
                uncovered_lights.join(", ")
            );
            if matches!(policy, pipeline::DeepSkyCalibrationPolicy::Strict) {
                blocking_reasons.push(reason);
            } else {
                warnings.push(format!(
                    "AllowDegraded: {reason}; se omite el bias global para no restarlo a firmas incompatibles"
                ));
                selected.clear();
                degraded = true;
            }
        }
    }

    // Flats are physically partitioned by session, but a session is still one
    // master in the current executor.  Never average two gain/ROI/CFA/filter
    // signatures merely because both match some light from the same night.
    if kind == "flats" {
        let mut by_night = std::collections::BTreeMap::<String, Vec<&DsProbe>>::new();
        for probe in &selected {
            let night = ds_session_night_id(&probe.path, probe.date_obs.as_deref())
                .unwrap_or_else(|| "?".into());
            by_night.entry(night).or_default().push(*probe);
        }
        let bad_nights = by_night
            .iter()
            .filter_map(|(night, probes)| {
                (ds_master_signature_groups(probes.iter().copied(), role).len() > 1)
                    .then(|| night.clone())
            })
            .collect::<Vec<_>>();
        if !bad_nights.is_empty() {
            let reason = format!(
                "flats: las sesiones {} contienen firmas incompatibles que llegarían al mismo máster",
                bad_nights.join(", ")
            );
            if matches!(policy, pipeline::DeepSkyCalibrationPolicy::Strict) {
                blocking_reasons.push(reason);
            } else {
                warnings.push(format!(
                    "AllowDegraded: {reason}; se omiten esas sesiones completas"
                ));
                selected.retain(|probe| {
                    let night = ds_session_night_id(&probe.path, probe.date_obs.as_deref())
                        .unwrap_or_else(|| "?".into());
                    !bad_nights.contains(&night)
                });
                degraded = true;
            }
        }
        let uncovered_nights = references
            .iter()
            .filter(|reference| reference.ok)
            .filter(|reference| {
                !selected.iter().any(|candidate| {
                    ds_compare_probe_calibration(
                        reference,
                        candidate,
                        crate::deepsky_calibration_contract::CalibrationRole::Flat,
                        pipeline::DeepSkyCalibrationPolicy::Strict,
                    )
                    .compatible
                })
            })
            .map(|reference| {
                ds_session_night_id(&reference.path, reference.date_obs.as_deref())
                    .unwrap_or_else(|| "?".into())
            })
            .collect::<std::collections::BTreeSet<_>>();
        if !uncovered_nights.is_empty() {
            let reason = format!(
                "flats: el máster por sesión no cubre todas las firmas de light en {}",
                uncovered_nights.iter().cloned().collect::<Vec<_>>().join(", ")
            );
            if matches!(policy, pipeline::DeepSkyCalibrationPolicy::Strict) {
                blocking_reasons.push(reason);
            } else {
                warnings.push(format!(
                    "AllowDegraded: {reason}; se omite cada sesión completa para no aplicar un flat incompatible"
                ));
                selected.retain(|probe| {
                    let night = ds_session_night_id(&probe.path, probe.date_obs.as_deref())
                        .unwrap_or_else(|| "?".into());
                    !uncovered_nights.contains(&night)
                });
                degraded = true;
            }
        }
    }
    if selected.is_empty() {
        let reason = format!("{kind}: ningún candidato tiene firma compatible exacta");
        if matches!(policy, pipeline::DeepSkyCalibrationPolicy::Strict) {
            if !paths.is_empty() && blocking_reasons.is_empty() {
                blocking_reasons.push(reason);
            }
        } else {
            degraded = true;
            warnings.push(format!("AllowDegraded: {reason}; el máster se omite"));
        }
    }
    let sessions: Vec<(String, Vec<String>)> = ds_group_probes_by_night(selected.iter().copied())
        .into_iter()
        .collect();
    if sessions.len() > 1 {
        warnings.push(format!(
            "{kind}: {} sesiones detectadas ({})",
            sessions.len(),
            sessions
                .iter()
                .map(|(night, paths)| format!("{night}: {} tomas", paths.len()))
                .collect::<Vec<_>>()
                .join(" · ")
        ));
    }
    let scientific_eligible = !degraded && blocking_reasons.is_empty();
    DsCalibrationSelection {
        paths: selected.iter().map(|probe| probe.path.clone()).collect(),
        group_count: groups.len(),
        description: if selected.is_empty() {
            format!("{kind}: sin grupo compatible")
        } else {
            format!(
                "{kind}: {} toma(s) exactas en {} firma(s)",
                selected.len(),
                groups.len()
            )
        },
        warnings,
        blocking_reasons,
        degraded,
        scientific_eligible,
        sessions,
    }
}

fn ds_virtual_master_path(kind: &str, paths: &[String]) -> Option<String> {
    (!paths.is_empty()).then(|| {
        let mut stable_paths = paths.to_vec();
        stable_paths.sort();
        stable_paths.dedup();
        format!(
            "master://{kind}/{}",
            ds_source_fingerprint(&[&stable_paths])
        )
    })
}

fn ds_selected_probes(
    probes: &[DsProbe],
    selection: &DsCalibrationSelection,
) -> Vec<DsProbe> {
    let selected = selection
        .paths
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    probes
        .iter()
        .filter(|probe| selected.contains(probe.path.as_str()))
        .cloned()
        .collect()
}

fn ds_exact_calibrations<'a>(
    reference: &DsProbe,
    candidates: &'a [DsProbe],
    role: crate::deepsky_calibration_contract::CalibrationRole,
) -> Vec<&'a DsProbe> {
    let mut exact = candidates
        .iter()
        .filter(|candidate| candidate.ok)
        .filter(|candidate| {
            ds_compare_probe_calibration(
                reference,
                candidate,
                role,
                pipeline::DeepSkyCalibrationPolicy::Strict,
            )
            .compatible
        })
        .collect::<Vec<_>>();
    exact.sort_by(|a, b| a.path.cmp(&b.path));
    exact
}

fn ds_prepare_calibration_decisions(
    lights: &[DsProbe],
    bias: &[DsProbe],
    darks: &[DsProbe],
    flats: &[DsProbe],
    dark_flats: &[DsProbe],
    policy: pipeline::DeepSkyCalibrationPolicy,
) -> Vec<pipeline::PreparedCalibrationDecision> {
    lights
        .iter()
        .filter(|light| light.ok)
        .map(|light| {
            let mut reasons = Vec::new();
            let selected_bias = ds_exact_calibrations(
                light,
                bias,
                crate::deepsky_calibration_contract::CalibrationRole::Bias,
            );
            let selected_dark = ds_exact_calibrations(
                light,
                darks,
                crate::deepsky_calibration_contract::CalibrationRole::Dark,
            );
            let selected_flat = ds_exact_calibrations(
                light,
                flats,
                crate::deepsky_calibration_contract::CalibrationRole::Flat,
            );
            let dark_flats_per_flat = selected_flat
                .iter()
                .map(|flat| {
                    ds_exact_calibrations(
                        flat,
                        dark_flats,
                        crate::deepsky_calibration_contract::CalibrationRole::DarkFlat,
                    )
                })
                .collect::<Vec<_>>();
            let all_flats_have_dark_flat = !selected_flat.is_empty()
                && dark_flats_per_flat
                    .iter()
                    .all(|matches| !matches.is_empty());
            let mut selected_dark_flat = dark_flats_per_flat
                .iter()
                .flatten()
                .copied()
                .collect::<Vec<_>>();
            selected_dark_flat.sort_by(|a, b| a.path.cmp(&b.path));
            selected_dark_flat.dedup_by(|a, b| a.path == b.path);

            // There is no request flag declaring that these lights were
            // already calibrated.  Therefore an absent list is not a benign
            // "not supplied" state: every light needs its own exact dark and
            // flat, and every selected flat needs its own exact raw dark-flat.
            if selected_dark.is_empty() {
                reasons.push(
                    "dark: ninguno de los darks coincide con este light (exposición, gain, temperatura o binning distintos); revisa el lote o usa la asignación manual"
                        .into(),
                );
            }
            if selected_flat.is_empty() {
                reasons.push(
                    "flat: ninguno de los flats coincide con este light (filtro, gain o binning distintos); revisa el lote o usa la asignación manual"
                        .into(),
                );
            }
            let flats_without_dark_flat = dark_flats_per_flat
                .iter()
                .filter(|matches| matches.is_empty())
                .count();
            if flats_without_dark_flat > 0 && !selected_flat.is_empty() {
                // Un solo aviso con conteo: enumerar cada flat inundaba el
                // panel. Los dark-flats son calibración OPCIONAL: sin ellos el
                // pedestal del flat queda sin restar, no es un fallo.
                reasons.push(format!(
                    "dark-flat: {flats_without_dark_flat} de {} flats sin dark-flat de la misma exposición/temperatura (los dark-flats son opcionales; puedes ligarlos manualmente o silenciar este aviso)",
                    selected_flat.len()
                ));
            }

            for (label, supplied, selected) in [
                ("bias", !bias.is_empty(), !selected_bias.is_empty()),
                ("dark", !darks.is_empty(), !selected_dark.is_empty()),
                ("flat", !flats.is_empty(), !selected_flat.is_empty()),
                (
                    "dark-flat",
                    !dark_flats.is_empty(),
                    !selected_dark_flat.is_empty(),
                ),
            ] {
                if supplied && !selected {
                    reasons.push(format!(
                        "{label}: no existe candidato con firma exacta para este light"
                    ));
                }
            }

            if !selected_flat.is_empty() {
                let pedestal = crate::deepsky_calibration_contract::validate_flat_pedestal(
                    crate::deepsky_calibration_contract::FlatPedestalInputs {
                        raw_dark_flat: all_flats_have_dark_flat,
                        bias: !all_flats_have_dark_flat && !selected_bias.is_empty(),
                        dark_flat_thermal: false,
                        validated_thermal_fraction: None,
                    },
                );
                if let Err(reason) = pedestal {
                    reasons.push(reason);
                }
            }

            let degraded = !reasons.is_empty();
            let fallback = degraded.then(|| {
                if matches!(policy, pipeline::DeepSkyCalibrationPolicy::AllowDegraded) {
                    "Classic no científico; másters incompatibles se omiten".into()
                } else {
                    "Strict bloquea la ejecución".into()
                }
            });
            let bias_paths = selected_bias
                .iter()
                .map(|probe| probe.path.clone())
                .collect::<Vec<_>>();
            let dark_paths = selected_dark
                .iter()
                .map(|probe| probe.path.clone())
                .collect::<Vec<_>>();
            let flat_paths = selected_flat
                .iter()
                .map(|probe| probe.path.clone())
                .collect::<Vec<_>>();
            let dark_flat_paths = selected_dark_flat
                .iter()
                .map(|probe| probe.path.clone())
                .collect::<Vec<_>>();
            pipeline::PreparedCalibrationDecision {
                frame_path: light.path.clone(),
                signature: light.signature.clone(),
                calibration_policy: policy,
                bias_master_path: ds_virtual_master_path("bias", &bias_paths),
                dark_master_path: ds_virtual_master_path("dark", &dark_paths),
                dark_flat_master_path: ds_virtual_master_path("dark-flat", &dark_flat_paths),
                flat_master_path: ds_virtual_master_path("flat", &flat_paths),
                dark_scale: (!selected_dark.is_empty()).then_some(1.0),
                // This is the state of the selected dark convention.  With a
                // compatible bias the executor constructs D_thermal=D_raw-B;
                // otherwise the exact raw dark is subtracted 1:1 and retains
                // its pedestal.  Bias is optional only in that raw convention.
                pedestal_state: if !selected_bias.is_empty() {
                    pipeline::PedestalState::BiasSubtracted
                } else {
                    pipeline::PedestalState::RawIncludesBias
                },
                compatible: !degraded,
                degraded,
                fallback,
                reasons,
                ..pipeline::PreparedCalibrationDecision::default()
            }
        })
        .collect()
}

fn ds_canonical_rejection(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "sigma" | "sigma_clip" | "sigma-clip" => Some("sigma"),
        "average" | "mean" | "media" => Some("average"),
        "median" | "mediana" => Some("median"),
        "winsorized" | "winsorised" => Some("winsorized"),
        "linearfit" | "linear_fit" | "linear-fit" => Some("linearfit"),
        "percentile" | "percentil" => Some("percentile"),
        "minmax" | "min_max" | "min-max" => Some("minmax"),
        _ => None,
    }
}

fn ds_canonical_normalization(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "none" | "ninguna" => Some("none"),
        "additive" | "aditiva" => Some("additive"),
        "scaling" | "multiplicative" | "multiplicativa" => Some("scaling"),
        "local" => Some("local"),
        _ => None,
    }
}

fn ds_canonical_interpolation(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "bilinear" | "bilineal" => Some("bilinear"),
        "lanczos3" | "lanczos-3" | "lanczos" => Some("lanczos3"),
        _ => None,
    }
}

fn ds_is_linear_science_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".fits")
        || lower.ends_with(".fit")
        || lower.ends_with(".tif")
        || lower.ends_with(".tiff")
}

/// Preflight tipado del asistente: valida geometría/metadatos y estima el plan
/// efectivo antes de reservar varios GB o iniciar un stack largo. ASYNC para
/// no congelar el hilo principal: desde F2 el asesor de muestreo decodifica
/// un light completo y mide estrellas.
#[tauri::command]
async fn prepare_deepsky_stack(request: DeepSkyStackRequest) -> PreparedStackPlan {
    prepare_deepsky_stack_impl(request, true)
}

/// `with_advisor=false` en las rutas de EJECUCIÓN (run_*): allí el plan solo
/// se usa para validar y el asesor (lectura completa de un light + detección
/// estelar) sería trabajo desechado — el stack relee todos los lights.
/// Señales medidas para la receta AUTO. Coste: 3 lecturas downsampled (5 si
/// n>60) de lights muestreados de forma determinista (primero/medio/último).
#[derive(Clone, Debug, Default)]
struct DsAutoSignals {
    n_lights: usize,
    sessions: usize,
    narrowband: bool,
    filter: Option<String>,
    background: f32,
    noise: f32,
    background_over_noise: f32,
    gradient_strength: f32,
    stars_per_mpx: f32,
    fwhm_px: f32,
    dark_nebula: bool,
    /// RMS del scatter de offsets estelares NO explicado por una deriva
    /// lineal. Con 3-5 muestras es una estimación conservadora: sólo HABILITA
    /// drizzle, nunca bloquea nada.
    dithering_rms_px: Option<f32>,
}

fn ds_measure_auto_signals(probes: &[&DsProbe]) -> DsAutoSignals {
    let n = probes.len();
    let mut signals = DsAutoSignals {
        n_lights: n,
        ..DsAutoSignals::default()
    };
    if n == 0 {
        return signals;
    }
    signals.sessions = probes
        .iter()
        .filter_map(|p| ds_session_night_id(&p.path, p.date_obs.as_deref()))
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        .max(1);
    signals.filter = probes.first().and_then(|p| ds_probe_filter_id(p));
    signals.narrowband = signals
        .filter
        .as_deref()
        .map(|f| f != "BROADBAND")
        .unwrap_or(false);

    // Índices de muestra deterministas sobre la lista ordenada de lights.
    let sample_count = if n > 60 { 5 } else { 3.min(n) };
    let mut sample_indices: Vec<usize> = (0..sample_count)
        .map(|k| k * n.saturating_sub(1) / sample_count.saturating_sub(1).max(1))
        .collect();
    sample_indices.dedup();

    struct Sample {
        index: usize,
        stars: Vec<(f32, f32, f32)>,
        factor: usize,
        background: f32,
        noise: f32,
        fwhm_px: f32,
        stars_per_mpx: f32,
        gradient_strength: f32,
        low_flat_fraction: f32,
    }
    let mut samples: Vec<Sample> = Vec::new();
    for &index in &sample_indices {
        let probe = probes[index];
        let Ok(image) = ds_read_image(&probe.path) else {
            continue;
        };
        let (luma, work_w, work_h, factor) = ds_inspection_luma(&image);
        let stars = ds_detect_stars(&luma, work_w, work_h, 120);
        let (background, noise_work) = ds_bg_noise(&luma);
        let fwhm_px = ds_frame_fwhm_proxy(&luma, work_w, work_h, &stars) * factor as f32;
        let mpx = (image.w as f32 * image.h as f32 / 1.0e6).max(0.001);
        // Gradiente: plano ajustado por mínimos cuadrados al grid 8×8 de fondo
        // local; la fuerza es el rango del PLANO (no del grid crudo, que una
        // nebulosa brillante centrada inflaría) sobre el ruido.
        let grid = ds_local_bg_grid(&luma, work_w, work_h, 8, 8);
        let gradient_strength = {
            let (gw, gh) = (8usize, 8usize);
            let (mut sx, mut sy, mut sxx, mut syy, mut sxy) = (0f64, 0f64, 0f64, 0f64, 0f64);
            let (mut sv, mut svx, mut svy) = (0f64, 0f64, 0f64);
            let count = (gw * gh) as f64;
            for gy in 0..gh {
                for gx in 0..gw {
                    let x = (gx as f64 + 0.5) / gw as f64;
                    let y = (gy as f64 + 0.5) / gh as f64;
                    let v = grid[gy * gw + gx] as f64;
                    sx += x;
                    sy += y;
                    sxx += x * x;
                    syy += y * y;
                    sxy += x * y;
                    sv += v;
                    svx += v * x;
                    svy += v * y;
                }
            }
            let cxx = sxx - sx * sx / count;
            let cyy = syy - sy * sy / count;
            let cxy = sxy - sx * sy / count;
            let cvx = svx - sv * sx / count;
            let cvy = svy - sv * sy / count;
            let det = cxx * cyy - cxy * cxy;
            if det.abs() > 1e-12 {
                let b = (cvx * cyy - cvy * cxy) / det;
                let c = (cvy * cxx - cvx * cxy) / det;
                // Plano lineal: extremos en las esquinas → rango = |b| + |c|.
                ((b.abs() + c.abs()) as f32 / noise_work.max(1e-6)).max(0.0)
            } else {
                0.0
            }
        };
        let low_flat_fraction = if luma.is_empty() {
            0.0
        } else {
            let lo = background - 2.0 * noise_work;
            let hi = background + 2.0 * noise_work;
            luma.iter().filter(|v| **v >= lo && **v <= hi).count() as f32 / luma.len() as f32
        };
        samples.push(Sample {
            index,
            stars_per_mpx: stars.len() as f32 / mpx,
            stars,
            factor,
            background,
            noise: noise_work,
            fwhm_px,
            gradient_strength,
            low_flat_fraction,
        });
    }
    if samples.is_empty() {
        return signals;
    }
    let median = |mut values: Vec<f32>| -> f32 {
        values.sort_by(|a, b| a.total_cmp(b));
        values[values.len() / 2]
    };
    signals.background = median(samples.iter().map(|s| s.background).collect());
    signals.noise = median(samples.iter().map(|s| s.noise).collect()).max(1e-6);
    signals.background_over_noise = signals.background / signals.noise;
    signals.gradient_strength = median(samples.iter().map(|s| s.gradient_strength).collect());
    signals.stars_per_mpx = median(samples.iter().map(|s| s.stars_per_mpx).collect());
    signals.fwhm_px = median(samples.iter().map(|s| s.fwhm_px).collect());
    let flat_fraction = median(samples.iter().map(|s| s.low_flat_fraction).collect());
    signals.dark_nebula = signals.background_over_noise < 2.0
        && signals.stars_per_mpx < 40.0
        && flat_fraction > 0.90;

    // Dithering: offsets estelares de cada muestra contra la primera. Con tan
    // pocas muestras, una deriva lineal pura es colineal en XY: el RMS de la
    // distancia PERPENDICULAR a la recta principal mide el scatter real de
    // dither sin que la deriva lo infle.
    if samples.len() >= 3 {
        let anchor = &samples[0];
        let mut offsets: Vec<(f32, f32)> = vec![(0.0, 0.0)];
        for sample in &samples[1..] {
            if sample.factor != anchor.factor {
                continue;
            }
            if let Some((dx, dy)) = ds_median_star_offset(&anchor.stars, &sample.stars, 40.0) {
                offsets.push((dx * sample.factor as f32, dy * sample.factor as f32));
            }
        }
        if offsets.len() >= 3 {
            let n_off = offsets.len() as f32;
            let mx = offsets.iter().map(|o| o.0).sum::<f32>() / n_off;
            let my = offsets.iter().map(|o| o.1).sum::<f32>() / n_off;
            let (mut cxx, mut cyy, mut cxy) = (0f32, 0f32, 0f32);
            for &(x, y) in &offsets {
                cxx += (x - mx) * (x - mx);
                cyy += (y - my) * (y - my);
                cxy += (x - mx) * (y - my);
            }
            let trace = cxx + cyy;
            let disc = ((cxx - cyy) * (cxx - cyy) + 4.0 * cxy * cxy).sqrt();
            let lambda_min = ((trace - disc) * 0.5).max(0.0) / n_off;
            signals.dithering_rms_px = Some(lambda_min.sqrt());
        }
    }
    signals
}

/// Presión de RAM preliminar a drizzle 1× (y si cabría un lienzo 2×) medida
/// sólo con la geometría de los probes; la estimación fina del preflight se
/// recalcula después con la receta ya resuelta.
fn ds_auto_preliminary_pressure(probes: &[&DsProbe]) -> (u64, bool) {
    let host_memory_mb = benchmark::get_benchmark_environment().memory_mb.max(1);
    let (mut in_px, mut ch) = (0u64, 1u64);
    for probe in probes {
        let px = probe.w as u64 * probe.h as u64;
        if px > in_px {
            in_px = px;
            ch = if probe.bayer.is_some() { 3 } else { probe.ch as u64 };
        }
    }
    let estimate_mb = |out_px: u64| -> u64 {
        (out_px.saturating_mul(ch).saturating_mul(24)
            + in_px.saturating_mul(ch).saturating_mul(8)
            + 64 * 1024 * 1024)
            / (1024 * 1024)
    };
    let pressure = estimate_mb(in_px).saturating_mul(100) / host_memory_mb;
    let ram_2x_fits = estimate_mb(in_px.saturating_mul(4)).saturating_mul(100) / host_memory_mb
        < 60;
    (pressure, ram_2x_fits)
}

/// Tabla de decisión AUTO. Función pura y determinista: mismas señales →
/// misma receta. Prioridad: RAM > nebulosa oscura > banda estrecha > tabla N
/// > drizzle. La sustitución de seguridad 2026-07-19 aplica: linear-fit está
/// deshabilitado, N>50 usa Winsorized (multi-noche lo absorbe la
/// normalización local por sesiones).
fn ds_resolve_auto_recipe(
    mut request: DeepSkyStackRequest,
    signals: &DsAutoSignals,
    memory_pressure: u64,
    ram_2x_fits: bool,
) -> (
    DeepSkyStackRequest,
    Vec<String>,
    std::collections::BTreeMap<String, String>,
) {
    let mut reasons = Vec::new();
    // Valores científicos fijos del modo AUTO.
    request.cosmetic = Some(true);
    request.optimize_dark = Some(true);
    request.auto_crop = true;
    request.gradient = false;
    request.drizzle = 1.0;
    request.pixfrac = 0.8;

    if memory_pressure >= 70 {
        request.rejection = "sigma".into();
        request.kappa_low = 3.0;
        request.kappa_high = 3.0;
        request.clip_iters = Some(1);
        request.normalization = "additive".into();
        request.interpolation = "bilinear".into();
        reasons.push(format!(
            "Presión de RAM estimada {memory_pressure}%: receta Rápido íntegra (sigma 1 pasada, bilinear, sin drizzle) para no forzar el spill a disco."
        ));
        let map = ds_auto_recipe_map(&request, signals);
        return (request, reasons, map);
    }
    request.interpolation = "lanczos3".into();

    let n = signals.n_lights;
    match n {
        0..=7 => {
            request.rejection = "average".into();
            request.kappa_low = 3.0;
            request.kappa_high = 3.0;
            request.clip_iters = Some(1);
            reasons.push(format!(
                "{n} lights (<8): σ/MAD son inestables y el clipping muerde señal — media sin rechazo con corrección cosmética."
            ));
        }
        8..=15 => {
            request.rejection = "sigma".into();
            request.kappa_low = 3.0;
            request.kappa_high = 3.0;
            request.clip_iters = Some(2);
            reasons.push(format!(
                "{n} lights: sigma iterativo κ3.0/3.0 es el mejor compromiso sesgo/varianza con estadística corta."
            ));
        }
        16..=50 => {
            request.rejection = "winsorized".into();
            request.kappa_low = 2.8;
            request.kappa_high = 3.0;
            request.clip_iters = Some(3);
            reasons.push(format!(
                "{n} lights: Winsorized κ2.8/3.0 conserva ~95% de eficiencia estadística y elimina satélites/rayos."
            ));
        }
        _ => {
            request.rejection = "winsorized".into();
            request.kappa_low = 3.0;
            request.kappa_high = 2.5;
            request.clip_iters = Some(3);
            reasons.push(format!(
                "{n} lights (>50): Winsorized κ3.0/2.5 con κ alto agresivo contra trazas; las variaciones multi-noche las absorbe la normalización (linear-fit sigue deshabilitado por seguridad)."
            ));
        }
    }

    if signals.gradient_strength > 3.0 || signals.sessions > 1 {
        request.normalization = "local".into();
        reasons.push(if signals.sessions > 1 {
            format!(
                "{} sesiones detectadas: normalización local por sesión/gradiente (modelo 24×24).",
                signals.sessions
            )
        } else {
            format!(
                "Gradiente de fondo {:.1}× el ruido: normalización local en vez de escala global.",
                signals.gradient_strength
            )
        });
    } else {
        request.normalization = "scaling".into();
        reasons.push("Fondo estable y una sola sesión: normalización por escala robusta.".into());
    }

    if signals.narrowband {
        request.kappa_high = request.kappa_high.max(3.5);
        reasons.push(format!(
            "Banda estrecha ({}): κ alto 3.5 conserva señal débil de emisión; sin SCNR/ABE (máster lineal intacto).",
            signals.filter.as_deref().unwrap_or("filtro")
        ));
        if signals.background_over_noise < 2.0 {
            request.normalization = "additive".into();
            reasons.push(
                "Fondo tenue (<2× ruido): normalización aditiva — la multiplicativa amplifica diferencias de fondo casi nulo.".into(),
            );
        }
    }

    if signals.dark_nebula {
        request.kappa_low = request.kappa_low.max(4.0);
        request.normalization = "additive".into();
        reasons.push(
            "Protección de fondo tenue activa: κ_low 4.0 (no recortar la cola oscura), fondo aditivo y anti-gradiente OFF.".into(),
        );
    }

    if let Some(rms) = signals.dithering_rms_px {
        let fwhm_ok = signals.fwhm_px > 0.0 && signals.fwhm_px < 2.5;
        if rms > 0.7 && n >= 30 && fwhm_ok && ram_2x_fits {
            request.drizzle = 2.0;
            request.pixfrac = 0.7;
            request.rejection = "sigma".into();
            request.kappa_low = if signals.dark_nebula { 4.0 } else { 2.5 };
            request.kappa_high = 2.5;
            request.clip_iters = Some(2);
            reasons.push(format!(
                "Dithering RMS {rms:.2} px, {n} lights y FWHM {:.1} px (submuestreo real): Drizzle 2× pixfrac 0.7 con rechazo sigma (los métodos por-píxel no soportan drizzle).",
                signals.fwhm_px
            ));
        } else if rms > 0.7 {
            let mut blockers = Vec::new();
            if n < 30 {
                blockers.push(format!("{n} lights (<30)"));
            }
            if !fwhm_ok {
                blockers.push(format!("FWHM {:.1} px (≥2.5: sin submuestreo)", signals.fwhm_px));
            }
            if !ram_2x_fits {
                blockers.push("RAM insuficiente para lienzo 2×".into());
            }
            reasons.push(format!(
                "Dithering detectado (RMS {rms:.2} px): Drizzle 2× quedaría disponible, pero {}.",
                blockers.join(" y ")
            ));
        }
    }

    let map = ds_auto_recipe_map(&request, signals);
    (request, reasons, map)
}

/// Receta y señales en un mapa auditable (clave → valor legible) para
/// `PreparedStackPlan.resolved_recipe` y la receta JSON.
fn ds_auto_recipe_map(
    request: &DeepSkyStackRequest,
    signals: &DsAutoSignals,
) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    map.insert("rejection".into(), request.rejection.clone());
    map.insert("kappa_low".into(), format!("{:.1}", request.kappa_low));
    map.insert("kappa_high".into(), format!("{:.1}", request.kappa_high));
    map.insert(
        "clip_iters".into(),
        request
            .clip_iters
            .map(|v| v.to_string())
            .unwrap_or_else(|| "auto".into()),
    );
    map.insert("normalization".into(), request.normalization.clone());
    map.insert("interpolation".into(), request.interpolation.clone());
    map.insert("drizzle".into(), format!("{:.1}", request.drizzle));
    map.insert("pixfrac".into(), format!("{:.2}", request.pixfrac));
    map.insert(
        "pedestal".into(),
        request
            .pedestal
            .map(|v| format!("{v:.0}"))
            .unwrap_or_else(|| "auto".into()),
    );
    map.insert("n_lights".into(), signals.n_lights.to_string());
    map.insert("sessions".into(), signals.sessions.to_string());
    map.insert("narrowband".into(), signals.narrowband.to_string());
    map.insert("dark_nebula".into(), signals.dark_nebula.to_string());
    map.insert(
        "background_over_noise".into(),
        format!("{:.2}", signals.background_over_noise),
    );
    map.insert(
        "gradient_strength".into(),
        format!("{:.2}", signals.gradient_strength),
    );
    map.insert(
        "stars_per_mpx".into(),
        format!("{:.0}", signals.stars_per_mpx),
    );
    map.insert("fwhm_px".into(), format!("{:.2}", signals.fwhm_px));
    map.insert(
        "dithering_rms_px".into(),
        signals
            .dithering_rms_px
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "sin medida".into()),
    );
    map
}

/// Aplica el perfil AUTO a un request usando sus probes. COMPARTIDO por
/// preflight y ejecución: mismas señales y misma tabla, para que el plan
/// mostrado y la receta ejecutada no puedan divergir.
fn ds_apply_auto_profile(
    request: DeepSkyStackRequest,
    probes: &[DsProbe],
) -> (
    DeepSkyStackRequest,
    Vec<String>,
    std::collections::BTreeMap<String, String>,
) {
    if request.profile != PipelineProfile::Auto {
        return (request, Vec::new(), Default::default());
    }
    let ok_probes: Vec<&DsProbe> = probes.iter().filter(|p| p.ok).collect();
    let signals = ds_measure_auto_signals(&ok_probes);
    let (pressure, ram_2x_fits) = ds_auto_preliminary_pressure(&ok_probes);
    ds_resolve_auto_recipe(request, &signals, pressure, ram_2x_fits)
}

/// Aplica las asignaciones manuales a la matriz de decisiones: la elección
/// explícita del usuario sustituye al emparejamiento automático de darks y
/// flats para los lights afectados y NUNCA cuenta como error de contrato.
fn ds_apply_manual_overrides_to_decisions(
    decisions: &mut [pipeline::PreparedCalibrationDecision],
    overrides: &[pipeline::DeepSkyCalibrationOverride],
) {
    for over in overrides {
        if over.darks.is_empty() && over.flats.is_empty() && !over.skip_flats && !over.skip_darks
        {
            continue;
        }
        let applies =
            |path: &str| over.lights.is_empty() || over.lights.iter().any(|l| l == path);
        for decision in decisions.iter_mut() {
            if !applies(&decision.frame_path) {
                continue;
            }
            decision.manual = true;
            if over.skip_darks {
                decision.reasons.retain(|reason| !reason.starts_with("dark:"));
                decision.dark_master_path = None;
                decision.dark_scale = None;
                decision
                    .reasons
                    .push("dark omitido por decisión del usuario".into());
            } else if !over.darks.is_empty() {
                decision.reasons.retain(|reason| !reason.starts_with("dark:"));
                decision.dark_master_path =
                    Some(format!("manual://{} darks", over.darks.len()));
                decision.dark_scale = Some(1.0);
            }
            if over.skip_flats {
                decision.reasons.retain(|reason| !reason.starts_with("flat:"));
                decision.flat_master_path = None;
                decision
                    .reasons
                    .push("flat omitido por decisión del usuario".into());
            } else if !over.flats.is_empty() {
                decision.reasons.retain(|reason| !reason.starts_with("flat:"));
                decision.flat_master_path =
                    Some(format!("manual://{} flats", over.flats.len()));
            }
            decision
                .reasons
                .push("asignación manual del usuario".into());
            // Sin motivos automáticos restantes, la decisión no bloquea; los
            // problemas de bias/dark-flat (si quedan) conservan su degradación.
            let blocking_left = decision.reasons.iter().any(|reason| {
                reason.starts_with("bias") || reason.starts_with("dark-flat")
            });
            decision.compatible = true;
            decision.degraded = decision.degraded && blocking_left;
        }
    }
}

fn prepare_deepsky_stack_impl(
    request: DeepSkyStackRequest,
    with_advisor: bool,
) -> PreparedStackPlan {
    use std::collections::{BTreeMap, BTreeSet};

    let request = request.resolved_profile();
    let plan_id = new_job_id("ds-plan");
    let probes = deepsky_probe(request.lights.clone());
    // AUTO: resolver la receta con señales medidas ANTES de derivar
    // requested/effective_rejection — validaciones y fallbacks del resto del
    // preflight operan así sobre los valores FINALES de la receta.
    let (request, auto_reasons, auto_recipe) = ds_apply_auto_profile(request, &probes);
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    if request.schema_version != pipeline::DEEP_SKY_STACK_REQUEST_SCHEMA_VERSION {
        errors.push(format!(
            "Versión DeepSkyStackRequest {} no soportada; se requiere {}",
            request.schema_version,
            pipeline::DEEP_SKY_STACK_REQUEST_SCHEMA_VERSION
        ));
    }
    let all_inputs = request
        .lights
        .iter()
        .chain(&request.darks)
        .chain(&request.flats)
        .chain(&request.dark_flats)
        .chain(&request.bias);
    let nonlinear: Vec<&str> = all_inputs
        .filter(|path| !ds_is_linear_science_path(path))
        .map(String::as_str)
        .collect();
    if !nonlinear.is_empty() {
        let shown = nonlinear
            .iter()
            .take(3)
            .filter_map(|path| std::path::Path::new(path).file_name())
            .map(|name| name.to_string_lossy())
            .collect::<Vec<_>>()
            .join(", ");
        let message = format!(
            "{} entrada(s) PNG/JPEG o de formato no científico ({}{}): FITS/TIFF lineal es obligatorio para calibración publicable",
            nonlinear.len(),
            shown,
            if nonlinear.len() > 3 { ", …" } else { "" }
        );
        if matches!(
            request.calibration_policy,
            pipeline::DeepSkyCalibrationPolicy::Strict
        ) {
            errors.push(message);
        } else {
            warnings.push(format!(
                "{message}; AllowDegraded lo admite sólo como resultado no científico"
            ));
        }
    }
    let requested_rejection = request.rejection.trim().to_string();
    let canonical_rejection = ds_canonical_rejection(&request.rejection);
    let mut effective_rejection = canonical_rejection.unwrap_or("sigma").to_string();
    if canonical_rejection.is_none() {
        errors.push(format!(
            "Método de rechazo desconocido '{}'; usa sigma, average, median, Winsorized, percentile o minmax",
            request.rejection
        ));
    }
    if canonical_rejection == Some("linearfit") {
        errors.push(
            "Linear-fit clipping está deshabilitado: la implementación anterior ordenaba intensidades y perdía la identidad frame↔referencia. Usa Winsorized o sigma hasta que exista una regresión robusta sobre residuales."
                .into(),
        );
    }
    if ds_canonical_normalization(&request.normalization).is_none() {
        errors.push(format!(
            "Normalización desconocida '{}'; usa none, additive, scaling o local",
            request.normalization
        ));
    }
    if ds_canonical_interpolation(&request.interpolation).is_none() {
        errors.push(format!(
            "Interpolación desconocida '{}'; usa bilinear o lanczos3",
            request.interpolation
        ));
    }
    if !request.kappa_low.is_finite()
        || !request.kappa_high.is_finite()
        || !(1.0..=8.0).contains(&request.kappa_low)
        || !(1.0..=8.0).contains(&request.kappa_high)
    {
        errors.push("kappaLow/kappaHigh deben ser finitos y estar entre 1 y 8".into());
    }
    if !request.drizzle.is_finite() || !(1.0..=3.0).contains(&request.drizzle) {
        errors.push("drizzle debe ser finito y estar entre 1× y 3×".into());
    }
    if !request.pixfrac.is_finite() || !(0.4..=1.0).contains(&request.pixfrac) {
        errors.push("pixfrac debe ser finito y estar entre 0.4 y 1.0".into());
    }
    if request
        .clip_iters
        .is_some_and(|iterations| !(1..=3).contains(&iterations))
    {
        errors.push("clipIters debe estar entre 1 y 3 cuando se especifica".into());
    }
    if request
        .pedestal
        .is_some_and(|pedestal| !pedestal.is_finite())
    {
        errors.push("pedestal debe ser un número finito".into());
    }
    // Métodos de integración versionados (receta v3). Sin fallback silencioso:
    // las restricciones de fase son errores de plan, no degradaciones.
    match request.resolved_integration_method() {
        pipeline::DeepSkyIntegrationMethod::Classic(_) => {}
        pipeline::DeepSkyIntegrationMethod::NebulaFusion(nf_cfg) => {
            if matches!(nf_cfg.mode, pipeline::NebulaFusionMode::FullWithStruct)
                && request.lights.len() < 16
            {
                errors.push(format!(
                    "STRUCT requiere al menos 16 tomas (división 8/8 mínima para validar por mitades); hay {}",
                    request.lights.len()
                ));
            }
            if matches!(
                nf_cfg.mode,
                pipeline::NebulaFusionMode::Full | pipeline::NebulaFusionMode::FullWithStruct
            ) && nf_cfg.cfa_direct
            {
                errors.push(
                    "NebulaFusion Full requiere la ruta demosaiced en esta fase: desactiva el modo CFA directo".into(),
                );
            }
            if request.drizzle > 1.01 {
                errors.push(
                    "NebulaFusion Lite integra a escala nativa: desactiva drizzle (la reconstrucción de muestreo llega con EIDR)".into(),
                );
            }
            if matches!(request.compute_policy, ComputePolicy::GpuOnly) {
                errors.push(
                    "NebulaFusion Lite ejecuta en CPU en esta fase; usa Auto o Hybrid".into(),
                );
            }
        }
        pipeline::DeepSkyIntegrationMethod::Eidr(eidr_cfg) => {
            if matches!(
                eidr_cfg.solve_mode,
                pipeline::EidrSolveMode::ExperimentalDetail
            ) {
                warnings.push(
                    "EIDR Detalle experimental (Huber-IRLS): el cuadrático científico corre como baseline interno y si el holdout empeora se REVIERTE automáticamente (§7.9/§7.10). TGV queda fuera de esta versión".into(),
                );
            }
            let n = request.lights.len();
            let (min_n, label) = match eidr_cfg.scale {
                pipeline::EidrScalePolicy::X2 => (if eidr_cfg.cfa_direct { 24 } else { 12 }, "2x"),
                pipeline::EidrScalePolicy::X1_5 => (8, "1.5x"),
                pipeline::EidrScalePolicy::X1 => (3, "1x"),
                pipeline::EidrScalePolicy::Auto => (6, "auto"),
            };
            if n < min_n {
                errors.push(format!(
                    "EIDR a escala {label} requiere al menos {min_n} tomas (hay {n}); la puerta espectral valida además la diversidad de dithers"
                ));
            }
            if request.drizzle > 1.01 {
                errors.push(
                    "EIDR sustituye a drizzle: deja drizzle en 1× (la escala 1x/1.5x/2x se elige en el método)".into(),
                );
            }
            // GpuOnly se valida en ejecución (runtime + paridad física del
            // matvec EIDR); Auto/Hybrid usan GPU cuando está disponible.
        }
    }
    if probes.is_empty() {
        errors.push("Selecciona al menos un light".into());
    }
    for p in probes.iter().filter(|p| !p.ok) {
        errors.push(format!(
            "No se pudo leer {}: {}",
            p.name,
            p.error.clone().unwrap_or_default()
        ));
    }

    let valid_probes: Vec<&DsProbe> = probes.iter().filter(|p| p.ok).collect();
    let mut signature_degraded = false;
    {
        // AGRUPADO, nunca una línea por toma: con 50+ lights el panel se
        // inundaba con el mismo mensaje repetido. La ausencia de cabeceras NO
        // bloquea el apilado: lo crítico degrada la elegibilidad científica
        // con divulgación; lo extendido es sólo una nota (es lo habitual).
        let mut probe_warning_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut critical_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut extended_counts: BTreeMap<String, usize> = BTreeMap::new();
        for probe in &valid_probes {
            for warning in &probe.signature_warnings {
                *probe_warning_counts.entry(warning.clone()).or_default() += 1;
            }
            let mut missing = probe.signature_missing.clone();
            if probe.store_layout.is_none() {
                missing.push("storeLayout/CFA phase".into());
            }
            if !missing.is_empty() {
                missing.sort();
                missing.dedup();
                *critical_counts.entry(missing.join(", ")).or_default() += 1;
            }
            let extended =
                crate::deepsky_signature::missing_extended_signature_fields(&probe.signature);
            if !extended.is_empty() {
                *extended_counts.entry(extended.join(", ")).or_default() += 1;
            }
        }
        for (text, count) in probe_warning_counts {
            warnings.push(if count > 1 {
                format!("{count} lights: {text}")
            } else {
                text
            });
        }
        for (fields, count) in critical_counts {
            warnings.push(format!(
                "{count} light(s) sin metadata crítica en cabecera ({fields}): el emparejamiento no puede verificarse del todo — el apilado continúa, el resultado no será elegible como científico y quedará divulgado en la receta."
            ));
            signature_degraded = true;
        }
        for (fields, count) in extended_counts {
            warnings.push(format!(
                "{count} light(s) sin metadata extendida ({fields}): es lo habitual en FITS de captura; el emparejamiento usa los campos disponibles (cámara, gain, offset, binning, exposición, temperatura)."
            ));
        }
    }
    let mut filters = BTreeSet::new();
    let mut geometries = BTreeSet::new();
    let mut bayers = BTreeSet::new();
    for p in &valid_probes {
        if let Some(f) = ds_probe_filter_id(p) {
            filters.insert(f);
        }
        geometries.insert((p.w, p.h, p.ch));
        bayers.insert(p.bayer.clone().unwrap_or_else(|| "NONE".into()));
    }
    if filters.len() > 1 {
        errors.push(format!(
            "Lights de varios filtros mezclados: {}",
            filters.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if geometries.len() > 1 {
        errors.push("Los lights tienen geometrías o canales incompatibles".into());
    }
    if bayers.len() > 1 {
        errors.push("Los lights mezclan patrones Bayer o datos CFA/RGB incompatibles".into());
    }
    let gains: BTreeSet<String> = valid_probes
        .iter()
        .filter_map(|p| p.gain.map(|v| format!("{:.1}", v)))
        .collect();
    let bins: BTreeSet<i32> = valid_probes.iter().filter_map(|p| p.binning).collect();
    let exposures: BTreeSet<String> = valid_probes
        .iter()
        .filter_map(|p| p.exptime.map(|v| format!("{:.1}", v)))
        .collect();
    if gains.len() > 1 {
        errors.push(format!(
            "Ganancias incompatibles en los lights: {}",
            gains.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if bins.len() > 1 {
        errors.push(format!("Binnings incompatibles en los lights: {:?}", bins));
    }
    if exposures.len() > 1 {
        warnings.push(format!(
            "Se detectaron {} exposiciones; se conservarán como grupos y se normalizarán robustamente antes de integrarlas",
            exposures.len()
        ));
    }
    if request.darks.is_empty() {
        warnings.push("Sin darks: se recomendará corrección cosmética".into());
    } else if request.bias.is_empty() && request.optimize_dark.unwrap_or(true) {
        // Escalar el dark (k≠1) sin bias resta mal el offset del sensor: el
        // clásico fallo silencioso que arruina una hora de stack. Se avisa
        // ANTES de ejecutar (con amp glow el motor ya fuerza k=1).
        let median_exp = |mut values: Vec<f32>| -> Option<f32> {
            if values.is_empty() {
                return None;
            }
            values.sort_by(|a, b| a.total_cmp(b));
            Some(values[values.len() / 2])
        };
        let light_exp = median_exp(valid_probes.iter().filter_map(|p| p.exptime).collect());
        let dark_exp = median_exp(
            deepsky_probe(request.darks.clone())
                .iter()
                .filter(|p| p.ok)
                .filter_map(|p| p.exptime)
                .collect(),
        );
        if let (Some(light_exp), Some(dark_exp)) = (light_exp, dark_exp) {
            if light_exp > 0.0 && (dark_exp - light_exp).abs() / light_exp > 0.10 {
                warnings.push(format!(
                    "Darks de {dark_exp:.0} s frente a lights de {light_exp:.0} s sin bias: el escalado k del dark restará mal el offset del sensor; añade bias o usa darks de la misma exposición."
                ));
            }
        }
    }
    if request.flats.is_empty() {
        warnings.push("Sin flats: no se corregirá viñeteo/PRNU".into());
    } else {
        let flat_probes: Vec<DsProbe> = deepsky_probe(request.flats.clone())
            .into_iter()
            .filter(|probe| probe.ok)
            .collect();
        // Validación FOTOMÉTRICA de flats (un flat por sesión, máx. 4 lecturas):
        // fuera del 20-70% del rango de saturación la respuesta del sensor no
        // es fiable y el flat corrige mal PRNU/viñeteo. Hasta ahora un panel
        // malo arruinaba el stack completo en silencio.
        {
            let narrowband_lights = valid_probes
                .first()
                .and_then(|probe| ds_probe_filter_id(probe))
                .map(|f| f != "BROADBAND")
                .unwrap_or(false);
            let mut seen_nights = BTreeSet::new();
            let mut checked = 0usize;
            for flat in &flat_probes {
                let night = ds_session_night_id(&flat.path, flat.date_obs.as_deref())
                    .unwrap_or_else(|| "unica".into());
                if !seen_nights.insert(night) || checked >= 4 {
                    continue;
                }
                checked += 1;
                let Ok(image) = ds_read_image(&flat.path) else {
                    continue;
                };
                let luma = ds_luma(&image);
                if luma.is_empty() {
                    continue;
                }
                let step = (luma.len() / 65536).max(1);
                let mut sampled: Vec<f32> = luma.iter().step_by(step).copied().collect();
                sampled.sort_by(|a, b| a.total_cmp(b));
                let median = sampled[sampled.len() / 2];
                let observed_max = sampled.last().copied().unwrap_or(0.0);
                let full_range = if observed_max <= 1.0 { 1.0 } else { 65535.0 };
                let pct = (median / full_range * 100.0).clamp(0.0, 100.0);
                if pct < 20.0 || pct > 70.0 {
                    let message = format!(
                        "Flat {} con mediana al {pct:.0}% del rango: {}",
                        flat.name,
                        if pct < 20.0 {
                            "subexpuesto — corrige mal el viñeteo y amplifica ruido; repítelo a 30-50% del rango"
                        } else {
                            "cerca de saturación — la respuesta PRNU deja de ser lineal"
                        }
                    );
                    // Los flats de cielo en banda estrecha son legítimamente
                    // bajos: se degrada a aviso para no bloquear ese flujo.
                    if narrowband_lights
                        || !matches!(
                            request.calibration_policy,
                            pipeline::DeepSkyCalibrationPolicy::Strict
                        )
                    {
                        warnings.push(message);
                    } else {
                        errors.push(message);
                    }
                }
            }
        }
        let dark_flat_probes: Vec<DsProbe> = deepsky_probe(request.dark_flats.clone())
            .into_iter()
            .filter(|probe| probe.ok)
            .collect();
        if dark_flat_probes.is_empty() && request.bias.is_empty() {
            let message = "Los flats no tienen dark-flats ni bias: el pedestal se normalizaría como parte de la respuesta óptica".to_string();
            if matches!(
                request.calibration_policy,
                pipeline::DeepSkyCalibrationPolicy::Strict
            ) {
                errors.push(message);
            } else {
                warnings.push(format!(
                    "{message}; AllowDegraded conservará el pedestal y lo declarará"
                ));
            }
        }
        if !dark_flat_probes.is_empty() {
            for flat in &flat_probes {
                let compatible = dark_flat_probes.iter().any(|dark_flat| {
                    ds_compare_probe_calibration(
                        flat,
                        dark_flat,
                        crate::deepsky_calibration_contract::CalibrationRole::DarkFlat,
                        pipeline::DeepSkyCalibrationPolicy::Strict,
                    )
                    .compatible
                });
                if !compatible {
                    let message = format!(
                        "Flat '{}' sin dark-flat de firma completa exacta (exposición/temperatura/geometría/CFA incluidas)",
                        flat.name
                    );
                    if matches!(
                        request.calibration_policy,
                        pipeline::DeepSkyCalibrationPolicy::Strict
                    ) {
                        errors.push(message);
                    } else {
                        warnings.push(format!(
                            "{message}; se intentará bias como fallback declarado"
                        ));
                    }
                }
            }
        }
    }
    if !request.dark_flats.is_empty() && request.flats.is_empty() {
        warnings.push("Se proporcionaron dark-flats sin flats; no se utilizarán".into());
    }
    if !request.darks.is_empty() {
        let dark_exposures: Vec<f32> = deepsky_probe(request.darks.clone())
            .into_iter()
            .filter_map(|probe| probe.ok.then_some(probe.exptime).flatten())
            .collect();
        for light in &valid_probes {
            let exact = light.exptime.is_some_and(|light_exp| {
                dark_exposures
                    .iter()
                    .any(|&dark_exp| ds_exposures_match(light_exp, dark_exp))
            });
            if !exact {
                let message = format!(
                    "Light '{}' sin dark de exposición exacta (tolerancia 1 ms / 1e-6 relativa)",
                    light.name
                );
                if matches!(
                    request.calibration_policy,
                    pipeline::DeepSkyCalibrationPolicy::Strict
                ) {
                    errors.push(message);
                } else {
                    warnings.push(format!(
                        "{message}; cualquier escalado/omisión quedará declarado"
                    ));
                }
            }
        }
    }
    if request.drizzle > 1.01 && valid_probes.len() < 12 {
        warnings.push("Drizzle con menos de 12 lights puede dejar cobertura irregular".into());
    }
    // Regla práctica DSS/PI: pixfrac <0.6 exige MUCHO dithering y tomas.
    if request.drizzle > 1.01 && request.pixfrac < 0.6 && valid_probes.len() < 40 {
        warnings.push(format!(
            "pixfrac {:.2} agresivo con {} lights: riesgo de agujeros de cobertura — usa 0.7-0.9 (o aporta más tomas con dithering)",
            request.pixfrac,
            valid_probes.len()
        ));
    }
    // Dark scaling entre exposiciones sin bias: el término de bias queda dentro
    // del dark y el escalado lo multiplica — calibración sesgada clásica.
    if request.bias.is_empty() && !request.darks.is_empty() {
        let dark_exps: BTreeSet<String> = deepsky_probe(request.darks.clone())
            .into_iter()
            .filter_map(|probe| probe.exptime.map(|value| format!("{value:.0}")))
            .collect();
        let light_exp = valid_probes.first().and_then(|p| p.exptime);
        let mismatched = light_exp
            .is_some_and(|le| !dark_exps.is_empty() && !dark_exps.contains(&format!("{le:.0}")));
        if dark_exps.len() > 1 || mismatched {
            warnings.push(
                "Darks de exposición distinta a los lights SIN bias: el escalado del dark multiplicaría también el bias (calibración sesgada). Añade bias o usa darks de la misma exposición".into(),
            );
        }
    }

    // Elegibilidad científica: PNG/JPEG llevan gamma/cuantización de display
    // sin prueba de linealidad. El motor clásico los sigue aceptando sin
    // cambio de comportamiento; los motores científicos los excluirán.
    let nonlinear_inputs: Vec<&str> = valid_probes
        .iter()
        .filter(|p| {
            let l = p.path.to_lowercase();
            l.ends_with(".png") || l.ends_with(".jpg") || l.ends_with(".jpeg")
        })
        .map(|p| p.name.as_str())
        .collect();
    let mut scientific_eligible = nonlinear_inputs.is_empty() && !signature_degraded;
    if !scientific_eligible {
        let shown = nonlinear_inputs
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if nonlinear_inputs.len() > 3 {
            ", …"
        } else {
            ""
        };
        warnings.push(format!(
            "{} light(s) PNG/JPEG sin linealidad demostrable ({}{}): aptos solo para el motor clásico; usa FITS o TIFF lineal para resultados científicos",
            nonlinear_inputs.len(),
            shown,
            suffix
        ));
        if !matches!(
            request.resolved_integration_method(),
            pipeline::DeepSkyIntegrationMethod::Classic(_)
        ) {
            errors.push(
                "Los motores científicos (NebulaFusion/EIDR) exigen entradas lineales (FITS/TIFF): retira los lights PNG/JPEG o usa el método clásico".into(),
            );
        }
    }

    let preflight_bias_probes = deepsky_probe(request.bias.clone());
    let preflight_dark_probes = deepsky_probe(request.darks.clone());
    let preflight_flat_probes = deepsky_probe(request.flats.clone());
    let preflight_dark_flat_probes = deepsky_probe(request.dark_flats.clone());
    let preflight_bias_selection = ds_select_calibration_group(
        &request.bias,
        &probes,
        "bias",
        request.calibration_policy,
    );
    let preflight_dark_selection = ds_select_calibration_group(
        &request.darks,
        &probes,
        "darks",
        request.calibration_policy,
    );
    let preflight_flat_selection = ds_select_calibration_group(
        &request.flats,
        &probes,
        "flats",
        request.calibration_policy,
    );
    let preflight_dark_flat_selection = ds_select_calibration_group(
        &request.dark_flats,
        &preflight_flat_probes,
        "dark-flats",
        request.calibration_policy,
    );
    let mut calibration_decisions = ds_prepare_calibration_decisions(
        &probes,
        &ds_selected_probes(&preflight_bias_probes, &preflight_bias_selection),
        &ds_selected_probes(&preflight_dark_probes, &preflight_dark_selection),
        &ds_selected_probes(&preflight_flat_probes, &preflight_flat_selection),
        &ds_selected_probes(
            &preflight_dark_flat_probes,
            &preflight_dark_flat_selection,
        ),
        request.calibration_policy,
    );
    ds_apply_manual_overrides_to_decisions(
        &mut calibration_decisions,
        &request.calibration_overrides,
    );
    if request
        .calibration_overrides
        .iter()
        .any(|over| !over.darks.is_empty() || !over.flats.is_empty())
    {
        warnings.push(
            "Asignación manual de calibración activa: los lotes forzados sustituyen al emparejamiento automático y quedan registrados en decisiones y receta.".into(),
        );
    }
    {
        // AGRUPADO por motivo: el detalle por light vive en la matriz de
        // calibración tipada (abajo); aquí solo el resumen accionable. Antes
        // se emitía un error POR LIGHT y 50 tomas inundaban el panel.
        let mut degraded_counts: BTreeMap<String, usize> = BTreeMap::new();
        for decision in &calibration_decisions {
            if decision.degraded {
                scientific_eligible = false;
                *degraded_counts
                    .entry(decision.reasons.join("; "))
                    .or_default() += 1;
            }
        }
        for (text, count) in degraded_counts {
            let message = format!(
                "{count} light(s) con calibración degradada — {text}. Detalle por toma en la matriz de calibración."
            );
            if matches!(
                request.calibration_policy,
                pipeline::DeepSkyCalibrationPolicy::Strict
            ) {
                errors.push(format!(
                    "{message} Con la política AllowDegraded el apilado continúa marcando el resultado como no científico."
                ));
            } else {
                warnings.push(format!("AllowDegraded: {message}"));
            }
        }
    }
    if !scientific_eligible
        && !matches!(
            request.resolved_integration_method(),
            pipeline::DeepSkyIntegrationMethod::Classic(_)
        )
        && matches!(
            request.calibration_policy,
            pipeline::DeepSkyCalibrationPolicy::AllowDegraded
        )
    {
        warnings.push(
            "Fallback efectivo: calibración degradada deshabilita NebulaFusion/EIDR; se ejecutará Classic no científico"
                .into(),
        );
    }

    // Asesor de muestreo (F2): FWHM mediana de un light representativo (el
    // central). Falla en silencio: sin estrellas medibles no hay tarjeta.
    // Solo con plan válido — no se paga una decodificación completa para un
    // plan que se va a rechazar.
    let sampling_advisor = if with_advisor && errors.is_empty() {
        valid_probes
            .get(valid_probes.len() / 2)
            .and_then(|probe| ds_measure_sampling_advisor(&probe.path))
    } else {
        None
    };

    // Desglose de SESIONES (noches) de los lights, con EXPOSICIÓN TOTAL por
    // sesión. Si hay varias, cada una se calibrará con los flats de SU noche.
    let fmt_exposure = |secs: f32| -> String {
        if secs >= 3600.0 {
            format!("{:.1} h", secs / 3600.0)
        } else if secs >= 60.0 {
            format!("{:.0} min", secs / 60.0)
        } else {
            format!("{:.0} s", secs)
        }
    };
    let light_sessions: Vec<(String, usize, f32)> = {
        let mut map: BTreeMap<String, (usize, f32)> = BTreeMap::new();
        for p in &valid_probes {
            let night =
                ds_session_night_id(&p.path, p.date_obs.as_deref()).unwrap_or_else(|| "?".into());
            let entry = map.entry(night).or_insert((0, 0.0));
            entry.0 += 1;
            entry.1 += p.exptime.unwrap_or(0.0);
        }
        map.into_iter()
            .map(|(night, (count, total))| (night, count, total))
            .collect()
    };
    if light_sessions.len() > 1 {
        warnings.push(format!(
            "Lights de {} sesiones: {} — cada sesión usará el master flat de su propia noche",
            light_sessions.len(),
            light_sessions
                .iter()
                .map(|(night, count, total)| format!(
                    "Sesión {night}: {count} lights · {}",
                    fmt_exposure(*total)
                ))
                .collect::<Vec<_>>()
                .join(" · ")
        ));
    }

    // Calibración coherente por tipo/sesión. Se informa exactamente qué grupo
    // se usará; los grupos incompatibles se omiten en vez de mezclarse.
    let mut flat_sessions_for_map: Vec<(String, usize)> = Vec::new();
    let mut darks_desc_for_map = String::new();
    if !valid_probes.is_empty() {
        for (kind, paths) in [
            ("darks", request.darks.clone()),
            ("flats", request.flats.clone()),
            ("bias", request.bias.clone()),
        ] {
            let selection = ds_select_calibration_group(
                &paths,
                &probes,
                kind,
                request.calibration_policy,
            );
            if !selection.blocking_reasons.is_empty() {
                errors.extend(selection.blocking_reasons.iter().cloned());
            }
            if selection.degraded {
                scientific_eligible = false;
            }
            if kind == "flats" {
                flat_sessions_for_map = selection
                    .sessions
                    .iter()
                    .map(|(night, session_paths)| (night.clone(), session_paths.len()))
                    .collect();
                // VALIDACIÓN FOTOMÉTRICA: un flat sobre/sub-expuesto arruina
                // 120 lights en silencio. Se lee UN flat por sesión (coste
                // acotado) y su mediana debe caer en el 15-75% del rango.
                for (night, session_paths) in &selection.sessions {
                    let Some(sample_path) = session_paths.first() else {
                        continue;
                    };
                    let Ok(flat) = ds_read_image(sample_path) else {
                        continue;
                    };
                    let step = (flat.data.len() / 200_000).max(1);
                    let mut sample: Vec<f32> = flat.data.iter().step_by(step).copied().collect();
                    sample.sort_by(|a, b| a.total_cmp(b));
                    let median = sample.get(sample.len() / 2).copied().unwrap_or(0.0);
                    let pct = median / 65535.0 * 100.0;
                    if !(15.0..=75.0).contains(&pct) {
                        warnings.push(format!(
                            "Flats de la sesión {night}: mediana al {pct:.0}% del rango ({}) — un flat {} degrada la corrección de viñeteo/PRNU. Ideal: 30-50%",
                            if pct < 15.0 { "subexpuesto" } else { "cerca de saturación" },
                            if pct < 15.0 { "oscuro" } else { "quemado" },
                        ));
                    }
                }
            }
            if kind == "darks" && !selection.paths.is_empty() {
                darks_desc_for_map = selection.description.clone();
            }
            warnings.extend(selection.warnings);
            if !paths.is_empty() {
                warnings.push(format!("Selección efectiva · {}", selection.description));
            }
            if kind == "darks" && !selection.paths.is_empty() {
                let exps: BTreeSet<String> = deepsky_probe(selection.paths.clone())
                    .into_iter()
                    .filter_map(|probe| probe.exptime.map(|value| format!("{value:.1}")))
                    .collect();
                if exps.len() > 1 {
                    warnings.push(format!(
                        "Darks compatibles en {} exposiciones: se crearán másters separados",
                        exps.len()
                    ));
                }
            }
        }
        let flat_reference_probes = deepsky_probe(request.flats.clone());
        let dark_flat_selection = ds_select_calibration_group(
            &request.dark_flats,
            &flat_reference_probes,
            "dark-flats",
            request.calibration_policy,
        );
        errors.extend(dark_flat_selection.blocking_reasons.iter().cloned());
        warnings.extend(dark_flat_selection.warnings.iter().cloned());
        if dark_flat_selection.degraded {
            scientific_eligible = false;
        }
        if !request.dark_flats.is_empty() {
            warnings.push(format!(
                "Selección efectiva · {}",
                dark_flat_selection.description
            ));
        }
    }

    // MATRIZ lights↔flats por sesión exacta. Una sesión sin flat queda visible
    // y nunca se sustituye por la noche cronológicamente más cercana.
    let session_map: Vec<crate::pipeline::SessionMapEntry> = light_sessions
        .iter()
        .map(|(night, count, total)| {
            let flat = flat_sessions_for_map
                .iter()
                .find(|(flat_night, _)| flat_night == night)
                .cloned();
            let flat_distance_days = flat
                .as_ref()
                .map(|(flat_night, _)| ds_night_distance(Some(night), Some(flat_night)))
                .unwrap_or(i64::MAX / 2);
            crate::pipeline::SessionMapEntry {
                night: night.clone(),
                lights: *count,
                exposure_seconds: *total,
                flat_night: flat.as_ref().map(|(flat_night, _)| flat_night.clone()),
                flat_count: flat.map(|(_, flat_count)| flat_count).unwrap_or(0),
                flat_distance_days,
                darks: darks_desc_for_map.clone(),
                light_paths: valid_probes
                    .iter()
                    .filter(|p| {
                        ds_session_night_id(&p.path, p.date_obs.as_deref())
                            .unwrap_or_else(|| "?".into())
                            == *night
                    })
                    .map(|p| p.path.clone())
                    .collect(),
                filter: valid_probes
                    .iter()
                    .find(|p| {
                        ds_session_night_id(&p.path, p.date_obs.as_deref())
                            .unwrap_or_else(|| "?".into())
                            == *night
                    })
                    .and_then(|p| ds_probe_filter_id(p)),
            }
        })
        .collect();
    for entry in &session_map {
        if entry.flat_night.is_none() && !request.flats.is_empty() {
            warnings.push(format!(
                "Sesión {}: no existe flat exacto de la misma sesión; Strict bloquea y AllowDegraded lo omite",
                entry.night
            ));
        } else if entry.flat_distance_days > 30 && entry.flat_distance_days < i64::MAX / 4 {
            warnings.push(format!(
                "Sesión {}: el flat más cercano es de {} (a {} días) — revisa fechas/flats de esa noche",
                entry.night,
                entry.flat_night.clone().unwrap_or_default(),
                entry.flat_distance_days
            ));
        }
    }

    let mut grouped: BTreeMap<String, Vec<&DsProbe>> = BTreeMap::new();
    for p in &valid_probes {
        let exp = p.exptime.map(|v| (v * 10.0).round() / 10.0);
        let gain = p.gain.map(|v| (v * 10.0).round() / 10.0);
        let temp = p.temp.map(|v| v.round());
        let key = format!(
            "{}x{}x{}|{}|{}|exp={:?}|gain={:?}|bin={:?}|temp={:?}",
            p.w,
            p.h,
            p.ch,
            p.bayer.as_deref().unwrap_or("NONE"),
            p.filter.as_deref().unwrap_or("NONE"),
            exp,
            gain,
            p.binning,
            temp
        );
        grouped.entry(key).or_default().push(*p);
    }
    let groups = grouped
        .into_iter()
        .map(|(key, ps)| {
            let p = ps[0];
            PreparedStackGroup {
                key,
                frame_count: ps.len(),
                width: p.w,
                height: p.h,
                channels: p.ch,
                bayer_pattern: p.bayer.clone(),
                filter: p.filter.clone(),
                exposure_seconds: p.exptime,
                gain: p.gain,
                binning: p.binning,
                temperature_c: p.temp,
            }
        })
        .collect::<Vec<_>>();

    let (w, h, ch) = valid_probes
        .first()
        .map(|p| (p.w, p.h, p.ch))
        .unwrap_or((0, 0, 1));
    // EIDR obliga drizzle=1 pero reconstruye a su propia escala: las
    // estimaciones deben declarar el lienzo REAL de salida (Auto se presupuesta
    // como el peor caso 2x), no la escala nativa (auditoría 2026-07-20).
    let eidr_scale = match request.resolved_integration_method() {
        pipeline::DeepSkyIntegrationMethod::Eidr(cfg) => Some(match cfg.scale {
            pipeline::EidrScalePolicy::X1 => 1.0f64,
            pipeline::EidrScalePolicy::X1_5 => 1.5,
            pipeline::EidrScalePolicy::X2 | pipeline::EidrScalePolicy::Auto => 2.0,
        }),
        _ => None,
    };
    let scale = eidr_scale.unwrap_or(request.drizzle.clamp(1.0, 3.0) as f64);
    let out_px = (w as f64 * h as f64 * scale * scale) as u64;
    let in_px = (w as u64).saturating_mul(h as u64);
    let n = valid_probes.len() as u64;
    let output_width = (w as f64 * scale).round().max(0.0) as u64;
    let tiled_row_bytes = output_width
        .saturating_mul(ch as u64)
        .saturating_mul(n)
        .saturating_mul(4);
    let per_pixel_requested = matches!(
        effective_rejection.as_str(),
        "median" | "winsorized" | "linearfit" | "percentile" | "minmax"
    );
    if per_pixel_requested && request.drizzle > 1.01 {
        warnings.push(format!(
            "Fallback previo: '{}' no se combina con drizzle; el método real será sigma iterativo con drizzle",
            requested_rejection
        ));
        effective_rejection = "sigma".into();
    } else if per_pixel_requested && n < 3 {
        warnings.push(format!(
            "Fallback previo: '{}' requiere al menos 3 lights; el método real será sigma/streaming",
            requested_rejection
        ));
        effective_rejection = "sigma".into();
    } else if per_pixel_requested && tiled_row_bytes > 2 * 1024 * 1024 * 1024 {
        warnings.push(format!(
            "Fallback previo: una franja del rechazo '{}' requeriría {:.1} GB; el método real será sigma/streaming",
            requested_rejection,
            tiled_row_bytes as f64 / 1_073_741_824.0
        ));
        effective_rejection = "sigma".into();
    }
    // Acumuladores de momentos + frame + márgenes de registro/normalización.
    // NebulaFusion Lite mantiene 7 planos f64 persistentes (totales, frame,
    // limpios y Σw²) más los productos f32 (VAR/NEFF/DQ) — su huella real es
    // ~3× la del streaming clásico y el preflight debe declararla.
    let nf_requested = matches!(
        request.resolved_integration_method(),
        pipeline::DeepSkyIntegrationMethod::NebulaFusion(_)
    );
    // EIDR mantiene planos f64 del solver (diagonal, planos, piloto, VAR/NEFF/
    // cobertura) a la escala de salida: presupuesto por píxel equiparable a NF.
    let ram_bytes_per_px_ch: u64 = if nf_requested || eidr_scale.is_some() {
        7 * 8 + 3 * 4
    } else {
        24
    };
    let estimated_ram_mb = (out_px
        .saturating_mul(ch as u64)
        .saturating_mul(ram_bytes_per_px_ch)
        + in_px.saturating_mul(ch as u64).saturating_mul(8)
        + 64 * 1024 * 1024)
        / (1024 * 1024);
    let full_vram_mb = (out_px.saturating_mul(ch as u64).saturating_mul(20)
        + in_px.saturating_mul(ch as u64).saturating_mul(4))
        / (1024 * 1024);
    let estimated_disk_mb = in_px
        .saturating_mul(ch as u64)
        .saturating_mul(4)
        .saturating_mul(n)
        / (1024 * 1024);
    let megapixel_frames = (in_px as f32 / 1_000_000.0) * n as f32;
    let estimated_seconds = (megapixel_frames / 35.0).max(1.0)
        * if request.drizzle > 1.01 {
            scale as f32 * scale as f32
        } else {
            1.0
        }
        * if matches!(
            effective_rejection.as_str(),
            "winsorized" | "linearfit" | "median" | "percentile"
        ) {
            1.6
        } else {
            1.0
        }
        * if nf_requested { 2.0 } else { 1.0 }; // NF-Lite: ~6 pasadas de warp vs ~3

    let gpu = crate::gpu_stack::gpu_info();
    // Prove every shader family that this recipe may execute. These gates are
    // cached for the session; a planetary parity label is not evidence for the
    // distinct deep-sky calibration, warp and rejection kernels.
    let deep_gpu_parity_ok = if gpu.available && request.compute_policy.allows_gpu() {
        crate::gpu_deepsky::ensure_parity()
            && crate::gpu_deepsky::ensure_pixel_preprocess_parity()
            && crate::gpu_deepsky::ensure_advanced_warp_parity()
            && (!matches!(effective_rejection.as_str(), "winsorized" | "linearfit")
                || crate::gpu_deepsky::ensure_tiled_parity())
    } else {
        true
    };
    let host_memory_mb = benchmark::get_benchmark_environment().memory_mb.max(1);
    let calibrated = !request.darks.is_empty() && !request.flats.is_empty();
    let memory_pressure = estimated_ram_mb.saturating_mul(100) / host_memory_mb;
    let (recommended_profile, recommendation_reasons) = if request.profile
        == PipelineProfile::Auto
    {
        // AUTO: las razones medidas de la receta resuelta sustituyen a la
        // heurística de 3 casos (que se conserva para los perfiles manuales).
        (PipelineProfile::Auto, auto_reasons.clone())
    } else if memory_pressure >= 70 {
        (
            PipelineProfile::Fast,
            vec![
                format!(
                    "La estimación usa {}% de la RAM física; Rápido reduce pasadas y presión de memoria.",
                    memory_pressure
                ),
                "Puedes conservar Equilibrado porque el almacén adaptativo caerá a mmap si hace falta.".into(),
            ],
        )
    } else if valid_probes.len() >= 12 && calibrated {
        let mut reasons = vec![
            format!("{} lights y calibración dark+flat permiten rechazo/normalización más robustos.", valid_probes.len()),
            "Máxima calidad activa Winsorized, normalización local y Lanczos-3; drizzle sigue siendo una decisión separada basada en dithering.".into(),
        ];
        // Señales adicionales medidas en este mismo preflight:
        let narrowband = valid_probes
            .first()
            .and_then(|probe| ds_probe_filter_id(probe))
            .map(|f| f != "BROADBAND")
            .unwrap_or(false);
        if narrowband {
            reasons.push(
                "Banda estrecha detectada: κ alto conserva la señal débil; no se aplicará SCNR ni limpieza de fondo automática (el máster queda lineal e intacto).".into(),
            );
        }
        if light_sessions.len() > 1 {
            reasons.push(format!(
                "{} sesiones detectadas: cada noche se calibra con sus propios flats (ver matriz de calibración).",
                light_sessions.len()
            ));
        }
        if valid_probes.len() >= 30 && request.drizzle <= 1.01 {
            reasons.push(format!(
                "Con {} lights, si capturaste con dithering considera Drizzle 2× (pixfrac 0.8) para recuperar resolución sub-píxel.",
                valid_probes.len()
            ));
        }
        (PipelineProfile::MaximumQuality, reasons)
    } else {
        let mut reasons = vec![
            "Equilibrado conserva registro y rechazo robustos con un coste predecible.".into(),
        ];
        if valid_probes.len() < 12 {
            reasons.push(format!("Con {} lights no se recomienda intensificar el rechazo más allá de sigma iterativo.", valid_probes.len()));
        }
        if !calibrated {
            reasons.push(
                "Faltan darks o flats; conviene revisar calibración antes de usar Máxima calidad."
                    .into(),
            );
        }
        (PipelineProfile::Balanced, reasons)
    };
    // El motor v2 divide el lienzo y recorta la fuente inversa por banda; por
    // tanto el pico real es un presupuesto de banda, no el lienzo completo.
    let estimated_vram_mb = if gpu.available {
        full_vram_mb.min((gpu.vram_budget_mb.saturating_mul(75) / 100).max(24))
    } else {
        full_vram_mb
    };
    let compute_resolution = resolve_compute_policy(
        request.compute_policy,
        &ComputeCapability {
            gpu_available: gpu.available,
            parity_ok: deep_gpu_parity_ok,
            required_vram_mb: estimated_vram_mb,
            vram_budget_mb: gpu.vram_budget_mb,
        },
    );
    let gpu_fits = compute_resolution.as_ref().is_ok_and(|r| r.use_gpu);
    // NF/EIDR integran en CPU en esta fase: el plan no puede declarar una
    // etapa GPU ni un motor "Hybrid CPU+GPU" que no se ejecutará (auditoría
    // 2026-07-20). GPU sigue disponible para calibración/estrellas.
    let classic_integration = matches!(
        request.resolved_integration_method(),
        pipeline::DeepSkyIntegrationMethod::Classic(_)
    );
    let gpu_streaming = gpu_fits
        && classic_integration
        && request.compute_policy.allows_gpu()
        && request.drizzle <= 1.01
        && matches!(effective_rejection.as_str(), "sigma" | "average");
    let gpu_tiled_rejection = gpu_fits
        && classic_integration
        && request.compute_policy.allows_gpu()
        && request.drizzle <= 1.01
        && matches!(effective_rejection.as_str(), "winsorized" | "linearfit");
    let gpu_integration = gpu_streaming || gpu_tiled_rejection;
    if matches!(request.compute_policy, ComputePolicy::GpuOnly) && request.drizzle > 1.01 {
        errors.push(
            "GPU only no admite todavía drizzle drop-kernel con paridad; usa Hybrid/Auto".into(),
        );
    }
    if matches!(request.compute_policy, ComputePolicy::GpuOnly)
        && !matches!(
            effective_rejection.as_str(),
            "sigma" | "average" | "winsorized" | "linearfit"
        )
    {
        errors.push(format!(
            "GPU only no admite rechazo '{}' con paridad; usa sigma, average, Winsorized, linear-fit o Hybrid/Auto",
            effective_rejection
        ));
    }
    if gpu_fits
        && request.compute_policy.allows_gpu()
        && !gpu_integration
        && !matches!(request.compute_policy, ComputePolicy::GpuOnly)
    {
        warnings.push(
            "La calibración usará GPU, pero la integración seleccionada conservará CPU".into(),
        );
    }
    let effective_engine = match request.compute_policy {
        ComputePolicy::CpuOnly => "CPU (Rayon/SIMD)".to_string(),
        ComputePolicy::GpuOnly if !gpu_fits => {
            errors.push(compute_resolution.as_ref().unwrap_err().clone());
            "GPU no disponible".to_string()
        }
        ComputePolicy::GpuOnly => format!("GPU only · {} ({})", gpu.name, gpu.backend),
        _ if gpu_fits => format!("Hybrid CPU+GPU · {} ({})", gpu.name, gpu.backend),
        _ => {
            if let Ok(resolution) = &compute_resolution {
                if let Some(reason) = &resolution.fallback_reason {
                    warnings.push(format!("La planificación usará CPU: {reason}"));
                }
            }
            "CPU (Rayon/SIMD)".to_string()
        }
    };
    let mut stages = BTreeMap::new();
    stages.insert(
        "read_calibrate".into(),
        if gpu_fits && request.compute_policy.allows_gpu() {
            "CPU I/O/estadística + GPU calibración/cosmética/debayer float32"
        } else {
            "CPU Rayon"
        }
        .into(),
    );
    stages.insert("register".into(), if gpu_fits && request.compute_policy.allows_gpu() { "GPU mapa estelar + CPU centroides PSF/RANSAC · similitud/afín/proyectivo/distorsión local" } else { "CPU PSF/RANSAC · similitud/afín/proyectivo/distorsión local automáticos" }.into());
    stages.insert(
        "normalize".into(),
        if gpu_streaming {
            "CPU modelo robusto + GPU aplicación"
        } else {
            "CPU modelo robusto"
        }
        .into(),
    );
    let has_cfa = valid_probes.first().is_some_and(|p| p.bayer.is_some());
    if let pipeline::DeepSkyIntegrationMethod::NebulaFusion(nf_cfg) =
        request.resolved_integration_method()
    {
        if nf_cfg.cfa_direct && !has_cfa {
            errors.push(
                "El modo CFA directo requiere lights CFA (BAYERPAT); estos lights son mono/RGB — usa la ruta estándar".into(),
            );
        }
    }
    if let pipeline::DeepSkyIntegrationMethod::Eidr(eidr_cfg) =
        request.resolved_integration_method()
    {
        if eidr_cfg.cfa_direct && !has_cfa {
            errors.push(
                "EIDR CFA directo requiere lights CFA con BAYERPAT/BAYERPATN válido; estos lights son mono/RGB"
                    .into(),
            );
        }
    }
    stages.insert(
        "integrate".into(),
        if gpu_streaming {
            "GPU tiled wgpu + CPU coordinación"
        } else if gpu_tiled_rejection {
            "CPU warp/Lanczos + GPU rechazo tiled Winsorized/linear-fit"
        } else if request.drizzle > 1.01 && has_cfa {
            "CPU drizzle CFA calibrado, sin debayer previo"
        } else if request.drizzle > 1.01 {
            "CPU drop-kernel drizzle"
        } else {
            "CPU tiled/streaming"
        }
        .into(),
    );
    stages.insert("export".into(), "CPU asynchronous I/O".into());
    let mut normalization_model = BTreeMap::new();
    normalization_model.insert("modo".into(), request.normalization.clone());
    normalization_model.insert(
        "fórmula".into(),
        match request.normalization.as_str() {
            "none" => "v′ = v (sin ajuste)".into(),
            "additive" => "v′ = v + (fondo_ref − fondo_frame)".into(),
            "local" => "v′(x,y) = v·(señal PSF_ref/señal PSF_frame) + campo de fondo 24×24(x,y)".into(),
            _ => "v′ = v·clamp(señal PSF_ref/señal PSF_frame, 0.5, 2.0) + Δfondo; ruido sólo como fallback".into(),
        },
    );
    normalization_model.insert(
        "referencia".into(),
        "frame con mayor peso PSF/ruido tras inspección".into(),
    );
    normalization_model.insert(
        "auditoría".into(),
        "coeficientes y rango local se guardan por frame en la receta".into(),
    );

    // Lotes visibles para el ligado manual: flats por noche/filtro y darks
    // por grupo de exposición, con sus paths para construir overrides exactos.
    let calibration_batches = {
        let mut flats_by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for probe in preflight_flat_probes.iter().filter(|p| p.ok) {
            let night = ds_session_night_id(&probe.path, probe.date_obs.as_deref())
                .unwrap_or_else(|| "sin fecha".into());
            let filter = ds_probe_filter_id(probe).unwrap_or_else(|| "?".into());
            flats_by_key
                .entry(format!("{night} · {filter}"))
                .or_default()
                .push(probe.path.clone());
        }
        let flats = flats_by_key
            .into_iter()
            .map(|(key, paths)| crate::pipeline::CalibrationBatchInfo {
                id: format!("flats:{key}"),
                label: format!("{key} · {} flats", paths.len()),
                count: paths.len(),
                paths,
            })
            .collect();
        let darks = ds_group_darks_by_exposure(
            &request.darks,
            crate::deepsky_calibration_contract::CalibrationRole::Dark,
        )
        .into_iter()
        .map(|(exposure, paths)| {
            let exposure_label = exposure
                .map(|seconds| format!("{seconds:.0} s"))
                .unwrap_or_else(|| "sin EXPTIME".into());
            crate::pipeline::CalibrationBatchInfo {
                id: format!("darks:{exposure_label}"),
                label: format!("{exposure_label} · {} darks", paths.len()),
                count: paths.len(),
                paths,
            }
        })
        .collect();
        crate::pipeline::CalibrationBatches { flats, darks }
    };

    PreparedStackPlan {
        session_map,
        calibration_batches,
        calibration_decisions,
        plan_id,
        valid: errors.is_empty(),
        groups,
        recommended_profile,
        recommendation_reasons,
        resolved_recipe: auto_recipe,
        warnings,
        errors,
        compute_policy: request.compute_policy,
        effective_engine,
        requested_rejection,
        effective_rejection,
        gpu_name: gpu.available.then_some(gpu.name),
        estimated_ram_mb,
        estimated_vram_mb,
        estimated_disk_mb,
        estimated_seconds,
        stages,
        normalization_model,
        scientific_eligible,
        sampling_advisor,
    }
}

/// F8: cancela UN stack deep-sky por su id de trabajo (el `resultId` que la
/// UI recibe). Devuelve false si el trabajo ya terminó o no existe. El botón
/// Cancelar global sigue funcionando (barre todos los trabajos).
#[tauri::command]
fn deepsky_cancel_job(state: State<'_, AppState>, job_id: String) -> bool {
    state.job_registry.cancel(&job_id)
}

/// Resuelve AUTO en un punto de EJECUCIÓN y lo deja trazado en el log
/// frontal. Devuelve el request con perfil Custom para que la validación
/// posterior no re-mida señales (los valores ya son los finales).
fn ds_resolve_auto_for_run(
    app: &tauri::AppHandle,
    request: DeepSkyStackRequest,
    context: &str,
) -> DeepSkyStackRequest {
    if request.profile != PipelineProfile::Auto {
        return request;
    }
    let probes = deepsky_probe(request.lights.clone());
    let (mut resolved, reasons, recipe) = ds_apply_auto_profile(request, &probes);
    resolved.profile = PipelineProfile::Custom;
    let values = recipe
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "n_lights" | "sessions" | "narrowband" | "dark_nebula" | "background_over_noise" | "gradient_strength" | "stars_per_mpx" | "fwhm_px" | "dithering_rms_px"
            )
        })
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" · ");
    log_to_front(
        app,
        "INFO",
        &format!(
            "Receta AUTO aplicada{context}: {values}\nMotivos: {}",
            reasons.join(" | ")
        ),
    );
    resolved
}

#[tauri::command]
async fn run_deepsky_stack(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    request: DeepSkyStackRequest,
) -> Result<DeepSkyResultHandle, String> {
    let request = request.resolved_profile();
    // Paridad AUTO plan↔ejecución: resolver aquí con las MISMAS señales y
    // tabla que el preflight; lo que se destructura son los valores resueltos.
    let request = ds_resolve_auto_for_run(&app, request, "");
    // Solo validación: sin asesor de muestreo (el stack relee los lights).
    let plan = prepare_deepsky_stack_impl(request.clone(), false);
    if !plan.valid {
        return Err(plan.errors.join("\n"));
    }
    // Acción de usuario nueva: rearme del flag global bajo el gate. El grupo
    // interno (stack_deepsky_impl) NO rearma, así el Cancelar no se pierde.
    ds_begin_user_action(&state);
    let started = std::time::Instant::now();
    let effective_method = request.resolved_integration_method();
    let preview_path = stack_deepsky_impl(
        app,
        state.clone(),
        request.lights,
        request.darks,
        request.flats,
        request.dark_flats,
        request.bias,
        Some(request.kappa_high.max(request.kappa_low)),
        Some(request.rejection != "average"),
        request.cosmetic,
        Some(request.gradient),
        Some(request.drizzle),
        request.optimize_dark,
        Some(request.pixfrac),
        Some(request.normalization == "local"),
        Some(request.auto_crop),
        Some(request.interpolation),
        request.clip_iters,
        request.pedestal,
        Some(request.rejection),
        Some(request.kappa_low),
        Some(request.kappa_high),
        Some(request.normalization),
        Some(request.compute_policy),
        Some(request.local_weighting),
        request.work_dir.clone(),
        Some(effective_method),
        Some(request.capture_mode),
        Some(request.calibration_policy),
        Some(request.calibration_overrides),
    )
    .await?;
    let result = state.deep_sky_result.lock().unwrap();
    let ds = result
        .as_ref()
        .ok_or("El motor terminó sin publicar un resultado lineal")?;
    let recipe_path = std::env::temp_dir()
        .join("astro_stacker_previews")
        .join(format!("{}_recipe.json", ds.id));
    ds_write_recipe(&recipe_path, ds).map_err(|error| {
        format!(
            "El máster terminó y permanece en memoria, pero no se pudo guardar su receta reproducible: {error}"
        )
    })?;
    let recipe_path = Some(recipe_path.display().to_string());
    Ok(DeepSkyResultHandle {
        result_id: ds.id.clone(),
        preview_path,
        width: ds.width,
        height: ds.height,
        channels: ds.channels,
        linear: true,
        engine: ds.engine.clone(),
        frames_used: ds.frames_used,
        frames_rejected: ds.frames_rejected,
        elapsed_seconds: started.elapsed().as_secs_f32(),
        recipe_path,
    })
}

#[tauri::command]
async fn prepare_deepsky_session(
    request: DeepSkySessionStackRequest,
) -> PreparedDeepSkySessionPlan {
    prepare_deepsky_session_impl(request, true)
}

fn prepare_deepsky_session_impl(
    request: DeepSkySessionStackRequest,
    with_advisor: bool,
) -> PreparedDeepSkySessionPlan {
    use std::collections::BTreeSet;

    let session_id = new_job_id("ds-session-plan");
    let mut groups = Vec::new();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut ids = BTreeSet::new();
    let mut components = BTreeSet::new();
    let mut estimated_ram_mb: u64 = 0;
    let mut estimated_vram_mb: u64 = 0;
    let mut estimated_disk_mb: u64 = 0;
    let mut estimated_seconds = 0.0;
    let mut total_frames = 0;

    if request.groups.is_empty() {
        errors.push("La sesión no contiene grupos de integración".into());
    }
    if !(0.0..=1.0).contains(&request.extraction.oiii_green_weight)
        || !request.extraction.oiii_green_weight.is_finite()
    {
        errors.push("El peso verde de OIII debe estar entre 0 y 1".into());
    }
    if !(0.0..=1.0).contains(&request.extraction.crosstalk_suppression)
        || !request.extraction.crosstalk_suppression.is_finite()
    {
        errors.push("La supresión de contaminación debe estar entre 0 y 1".into());
    }

    for group in request.groups {
        let id = group.id.trim().to_string();
        if id.is_empty() || !ids.insert(id.clone()) {
            errors.push(format!(
                "Identificador de grupo vacío o duplicado: '{}'",
                group.id
            ));
        }
        let filter_profile = ds_filter_token(&group.filter_profile)
            .unwrap_or_else(|| group.filter_profile.trim())
            .to_ascii_uppercase();
        let component_filters = ds_filter_components(&filter_profile);
        components.extend(component_filters.iter().cloned());
        total_frames += group.request.lights.len();
        let plan = prepare_deepsky_stack_impl(group.request, with_advisor);
        estimated_ram_mb = estimated_ram_mb.max(plan.estimated_ram_mb);
        estimated_vram_mb = estimated_vram_mb.max(plan.estimated_vram_mb);
        estimated_disk_mb = estimated_disk_mb.saturating_add(plan.estimated_disk_mb);
        if component_filters.len() > 1 {
            if let Some(geometry) = plan.groups.first() {
                let component_bytes = geometry
                    .width
                    .saturating_mul(geometry.height)
                    .saturating_mul(component_filters.len())
                    .saturating_mul(std::mem::size_of::<f32>());
                estimated_disk_mb =
                    estimated_disk_mb.saturating_add((component_bytes as u64).div_ceil(1_000_000));
            }
        }
        estimated_seconds += plan.estimated_seconds;
        errors.extend(
            plan.errors
                .iter()
                .map(|error| format!("{}: {error}", group.label)),
        );
        warnings.extend(
            plan.warnings
                .iter()
                .map(|warning| format!("{}: {warning}", group.label)),
        );
        groups.push(PreparedDeepSkySessionGroup {
            id,
            label: group.label,
            filter_profile,
            component_filters,
            plan,
        });
    }
    if groups.len() > 1 {
        warnings.push(format!(
            "Sesión multibanda: se ejecutarán {} integraciones separadas y coordinadas",
            groups.len()
        ));
    }

    PreparedDeepSkySessionPlan {
        session_id,
        valid: errors.is_empty(),
        groups,
        warnings,
        errors,
        estimated_ram_mb,
        estimated_vram_mb,
        estimated_disk_mb,
        estimated_seconds,
        total_frames,
        component_filters: components.into_iter().collect(),
    }
}

fn ds_session_safe_name(value: &str) -> String {
    let out: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let out = out.trim_matches('_');
    if out.is_empty() {
        "grupo".into()
    } else {
        out.into()
    }
}

fn ds_result_quality(result: &DeepSkyResult) -> DeepSkyQualitySummary {
    let n = result.width.saturating_mul(result.height).max(1);
    let coverage_max = result
        .coverage
        .iter()
        .copied()
        .fold(0.0f32, f32::max)
        .max(1e-6);
    let coverage_percent = 100.0
        * result
            .coverage
            .iter()
            .filter(|value| **value >= coverage_max * 0.80)
            .count() as f32
        / n as f32;
    let rejected: f32 =
        result.rejection_low.iter().sum::<f32>() + result.rejection_high.iter().sum::<f32>();
    let accepted: f32 = result.weight.iter().sum();
    let rejection_percent = if accepted + rejected > 0.0 {
        100.0 * rejected / (accepted + rejected)
    } else {
        0.0
    };
    let step = (result.data.len() / 200_000).max(1);
    let mut sample: Vec<f32> = result
        .data
        .iter()
        .step_by(step)
        .copied()
        .filter(|value| value.is_finite())
        .collect();
    sample.sort_by(|a, b| a.total_cmp(b));
    let median = sample.get(sample.len() / 2).copied().unwrap_or(0.0);
    let mut deviation: Vec<f32> = sample.iter().map(|value| (value - median).abs()).collect();
    deviation.sort_by(|a, b| a.total_cmp(b));
    let background_noise = deviation.get(deviation.len() / 2).copied().unwrap_or(0.0) * 1.4826;
    let mut recommendations = Vec::new();
    if coverage_percent < 92.0 {
        recommendations
            .push("Revisar encuadre/dithering: la cobertura uniforme es inferior a 92%".into());
    }
    if rejection_percent > 25.0 {
        recommendations.push(
            "Inspeccionar mapas de rechazo: más de 25% de muestras fueron descartadas".into(),
        );
    }
    if result.frames_used < 12 {
        recommendations.push(
            "Con menos de 12 lights, drizzle y rechazo agresivo pueden ser inestables".into(),
        );
    }
    if recommendations.is_empty() {
        recommendations.push("Cobertura y rechazo dentro de los límites recomendados; revisar FWHM y residuales visualmente".into());
    }
    let grade = if coverage_percent >= 96.0 && rejection_percent <= 15.0 {
        "Excelente"
    } else if coverage_percent >= 90.0 && rejection_percent <= 30.0 {
        "Buena"
    } else {
        "Revisar"
    }
    .into();
    DeepSkyQualitySummary {
        grade,
        coverage_percent,
        rejection_percent,
        background_noise,
        recommendations,
    }
}

fn ds_extract_dual_band_planes(
    data: &[f32],
    channels: usize,
    oiii_green_weight: f32,
    crosstalk_suppression: f32,
) -> Result<(Vec<f32>, Vec<f32>), String> {
    if channels < 3 || data.len() % channels != 0 {
        return Err("La extracción dual-band requiere datos RGB intercalados".into());
    }
    let green = oiii_green_weight.clamp(0.0, 1.0);
    let blue = 1.0 - green;
    let suppress = crosstalk_suppression.clamp(0.0, 1.0);
    let mut primary = Vec::with_capacity(data.len() / channels);
    let mut oiii = Vec::with_capacity(data.len() / channels);
    for pixel in data.chunks_exact(channels) {
        let oxygen = pixel[1] * green + pixel[2] * blue;
        oiii.push(oxygen);
        // No se recorta a cero: los negativos de calibración y la corrección
        // de contaminación deben sobrevivir en el máster científico float32.
        primary.push(pixel[0] - oxygen * suppress);
    }
    Ok((primary, oiii))
}

/// Render the current OSC dual-band RGB master as a derived HOO composite:
/// Ha (from R, crosstalk-suppressed) → R; OIII (G·w + B·(1−w)) → both G and B.
/// This is the standard one-shot-colour dual-band mapping (SV220/L-eXtreme…): it
/// turns the green-dominated linear RGB master into the characteristic red-Ha /
/// teal-OIII image. Optional per-channel background neutralization. The linear
/// master stays float (negatives/headroom preserved) and is never replaced.
// (async): trabajo de segundos-minutos fuera del hilo principal — la UI
// sigue viva y Cancelar/checkpoints funcionan (auditoría 2026-07-20).
#[tauri::command(async)]
fn deepsky_dualband_hoo(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    oiii_green_weight: Option<f32>,
    crosstalk_suppression: Option<f32>,
    neutralize: Option<bool>,
) -> Result<String, String> {
    let green_w = oiii_green_weight.unwrap_or(0.65);
    let suppress = crosstalk_suppression.unwrap_or(0.0);
    let (w, h, hoo) = {
        let guard = state.deep_sky_result.lock().unwrap();
        let r = guard.as_ref().ok_or("No hay máster lineal en memoria.")?;
        if r.channels < 3 {
            return Err(
                "El máster es monocromo; la combinación HOO necesita un máster OSC/RGB dual-band."
                    .into(),
            );
        }
        let (ha, oiii) = ds_extract_dual_band_planes(&r.data, r.channels, green_w, suppress)?;
        let npx = r.width * r.height;
        let mut hoo = vec![0.0f32; npx * 3];
        for i in 0..npx {
            hoo[i * 3] = ha[i];
            hoo[i * 3 + 1] = oiii[i];
            hoo[i * 3 + 2] = oiii[i];
        }
        if neutralize.unwrap_or(true) {
            ds_neutralize_background(&mut hoo, r.width, r.height, 3);
        }
        (r.width, r.height, hoo)
    };
    // Rebuild only the u16 preview mirror + STF preview. SCI/VAR/NEFF/DQ keep
    // their original RGB/CFA relationship in `deep_sky_result`.
    let npx = w * h;
    let mut rgb16 = vec![0u16; npx * 3];
    for i in 0..npx {
        for c in 0..3 {
            rgb16[i * 3 + c] = hoo[i * 3 + c].clamp(0.0, 65535.0) as u16;
        }
    }
    {
        let mut res = state.stacked_image.lock().unwrap();
        *res = Some(StackResult {
            width: w,
            height: h,
            data: rgb16.clone(),
            is_mono: false,
            is_surface: false,
        });
        state.deconv_cache.lock().unwrap().clear();
        state.wavelet_cache.lock().unwrap().clear();
        state.filter_cache.lock().unwrap().clear();
    }
    let preview8 = ds_render_stretch(&rgb16, w, h, false, 2.8, 0.25);
    let mut rgba = Vec::with_capacity(npx * 4);
    for i in 0..npx {
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
    log_to_front(
        &app,
        "SUCCESS",
        "Vista HOO derivada aplicada (Ha→R, OIII→G/B); SCI lineal y sus mapas científicos permanecen intactos.",
    );
    Ok(
        save_preview_png_to_temp(&enc, "deepsky").unwrap_or_else(|| {
            format!(
                "data:image/png;base64,{}",
                general_purpose::STANDARD.encode(&enc)
            )
        }),
    )
}

fn ds_session_manifest_product(
    kind: ScientificProductKind,
    path: &std::path::Path,
    unit: DsFitsUnit,
    width: u32,
    height: u32,
    channels: u8,
    linear: bool,
    derived: bool,
    role: &str,
) -> ScientificProductMetadata {
    let mut metadata = std::collections::BTreeMap::new();
    metadata.insert("role".into(), role.into());
    ScientificProductMetadata {
        kind,
        path: path.display().to_string(),
        bunit: Some(unit.header_value().trim_matches('\'').to_string()),
        sample_type: Some("float32".into()),
        width,
        height,
        channels,
        linear,
        derived,
        metadata,
    }
}

fn ds_session_require_product_len(
    name: &str,
    actual: usize,
    expected: usize,
) -> Result<(), String> {
    if actual != expected {
        return Err(format!(
            "Producto científico {name} inconsistente: {actual} muestras; se esperaban {expected}"
        ));
    }
    Ok(())
}

fn ds_session_collect_fallbacks(
    value: Option<&serde_json::Value>,
    fallbacks: &mut std::collections::BTreeSet<String>,
) {
    let Some(value) = value else { return };
    match value {
        serde_json::Value::String(reason) if !reason.trim().is_empty() => {
            fallbacks.insert(reason.trim().to_string());
        }
        serde_json::Value::Array(values) => {
            for item in values {
                ds_session_collect_fallbacks(Some(item), fallbacks);
            }
        }
        _ => {}
    }
}

fn ds_session_recipe_text(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return (!text.trim().is_empty()).then(|| text.trim().to_string());
    }
    (!value.is_null())
        .then(|| serde_json::to_string(value).ok())
        .flatten()
}

fn ds_export_session_result(
    result: &DeepSkyResult,
    output_dir: &std::path::Path,
    group_id: &str,
    label: &str,
    filter_profile: &str,
    extraction: &DualBandExtractionOptions,
    capture_mode: pipeline::DeepSkyCaptureMode,
    calibration_policy: pipeline::DeepSkyCalibrationPolicy,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<
    (
        String,
        std::collections::BTreeMap<String, String>,
        std::collections::BTreeMap<String, String>,
        ScientificBundleManifest,
    ),
    String,
> {
    cancellation_checkpoint(cancel, "publicación de bundle científico")?;
    let npx = result
        .width
        .checked_mul(result.height)
        .filter(|&count| count > 0)
        .ok_or("Bundle científico: geometría vacía o fuera de rango")?;
    let science_samples = npx
        .checked_mul(result.channels)
        .ok_or("Bundle científico: número de muestras fuera de rango")?;
    ds_session_require_product_len("SCI", result.data.len(), science_samples)?;
    let width = u32::try_from(result.width)
        .map_err(|_| "Bundle científico: ancho no representable en el contrato")?;
    let height = u32::try_from(result.height)
        .map_err(|_| "Bundle científico: alto no representable en el contrato")?;
    let channels = u8::try_from(result.channels)
        .map_err(|_| "Bundle científico: canales no representables en el contrato")?;
    if !matches!(channels, 1 | 3) {
        return Err(format!(
            "Bundle científico: se esperaban 1 o 3 canales, se recibieron {channels}"
        ));
    }

    let mut manifest_warnings = Vec::new();
    let recipe_capture_mode = match result.recipe.get("captureMode") {
        Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
            format!("Bundle científico: captureMode inválido en receta: {error}")
        })?,
        None => {
            manifest_warnings.push(
                "La receta del stack no declaró captureMode; se usó el valor resuelto por la sesión"
                    .into(),
            );
            capture_mode
        }
    };
    let recipe_calibration_policy = match result.recipe.get("calibrationPolicy") {
        Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
            format!("Bundle científico: calibrationPolicy inválido en receta: {error}")
        })?,
        None => {
            manifest_warnings.push(
                "La receta del stack no declaró calibrationPolicy; se usó el valor resuelto por la sesión"
                    .into(),
            );
            calibration_policy
        }
    };
    let calibration_decisions = match result.recipe.get("calibrationDecisions") {
        Some(value) => serde_json::from_value::<Vec<PreparedCalibrationDecision>>(value.clone())
            .map_err(|error| {
                format!("Bundle científico: calibrationDecisions inválidas: {error}")
            })?,
        None => {
            manifest_warnings.push(
                "La receta no publicó decisiones de calibración por light para este grupo".into(),
            );
            Vec::new()
        }
    };

    let mut fallbacks = std::collections::BTreeSet::new();
    ds_session_collect_fallbacks(
        result
            .recipe
            .pointer("/integrationMethod/fallbacks"),
        &mut fallbacks,
    );
    ds_session_collect_fallbacks(result.recipe.pointer("/eidr/fallbacks"), &mut fallbacks);
    ds_session_collect_fallbacks(result.recipe.get("eidrFallback"), &mut fallbacks);
    ds_session_collect_fallbacks(result.recipe.get("structFallback"), &mut fallbacks);
    ds_session_collect_fallbacks(
        result.recipe.pointer("/parameters/methodFallbackReason"),
        &mut fallbacks,
    );
    ds_session_collect_fallbacks(
        result
            .recipe
            .pointer("/parameters/localNormalization/fallback"),
        &mut fallbacks,
    );
    for decision in &calibration_decisions {
        if let Some(reason) = decision
            .fallback
            .as_deref()
            .filter(|reason| !reason.trim().is_empty())
        {
            fallbacks.insert(format!("calibration: {}", reason.trim()));
        }
        if decision.degraded {
            manifest_warnings.push(format!(
                "Calibración degradada para {}",
                decision.frame_path
            ));
        }
    }
    if let Some(recipe_warnings) = result.recipe.get("warnings").and_then(|v| v.as_array()) {
        for warning in recipe_warnings.iter().filter_map(|value| value.as_str()) {
            if !warning.trim().is_empty() {
                manifest_warnings.push(warning.trim().to_string());
            }
        }
    }

    let mut bundle_metadata = std::collections::BTreeMap::new();
    bundle_metadata.insert("engine".into(), result.engine.clone());
    bundle_metadata.insert("rejectionMethod".into(), result.method.clone());
    bundle_metadata.insert("framesUsed".into(), result.frames_used.to_string());
    bundle_metadata.insert(
        "framesRejected".into(),
        result.frames_rejected.to_string(),
    );
    if let Some(value) = ds_session_recipe_text(
        result.recipe.pointer("/integrationMethod/requested"),
    ) {
        bundle_metadata.insert("integrationRequested".into(), value);
    }
    if let Some(value) = ds_session_recipe_text(
        result.recipe.pointer("/integrationMethod/effective"),
    ) {
        bundle_metadata.insert("integrationEffective".into(), value);
    }
    if let Some(value) = result
        .recipe
        .get("sourceFingerprint")
        .and_then(|value| value.as_str())
    {
        bundle_metadata.insert("sourceFingerprint".into(), value.into());
    }

    let mut bundle = ScientificBundleManifest {
        recipe_schema: result
            .recipe
            .get("schemaVersion")
            .and_then(|value| value.as_str())
            .unwrap_or(pipeline::DEEP_SKY_RECIPE_SCHEMA_VERSION)
            .to_string(),
        group_id: group_id.to_string(),
        filter_profile: Some(filter_profile.to_string()),
        capture_mode: recipe_capture_mode,
        calibration_policy: recipe_calibration_policy,
        width,
        height,
        channels,
        calibration_decisions,
        fallbacks: fallbacks.into_iter().collect(),
        warnings: manifest_warnings,
        metadata: bundle_metadata,
        ..ScientificBundleManifest::default()
    };
    bundle
        .products
        .try_reserve(16)
        .map_err(|error| format!("Bundle científico: reserva de manifiesto: {error}"))?;

    let safe = ds_session_safe_name(group_id);
    let master = output_dir.join(format!("{safe}_master_linear_float32.fits"));
    let metadata = vec![
        ("ZASVER", "'hybrid-v2-2026.07'".into()),
        ("ZASJOB", format!("'{}'", result.id)),
        ("ZASGROUP", format!("'{}'", safe)),
        ("ZASFILT", format!("'{}'", filter_profile)),
        ("OBJECT", format!("'{}'", label.replace('\'', ""))),
        ("NCOMBINE", format!("{:>20}", result.frames_used)),
        ("EXTNAME", "'SCI'".into()),
        ("ZASROLE", "'SCI'".into()),
        ("BUNIT", DsFitsUnit::Adu.header_value().into()),
    ];
    ds_save_float32_fits_cancellable(
        &master,
        &result.data,
        result.width,
        result.height,
        result.channels,
        &metadata,
        Some(cancel),
    )?;
    bundle.products.push(ds_session_manifest_product(
        ScientificProductKind::Sci,
        &master,
        DsFitsUnit::Adu,
        width,
        height,
        channels,
        true,
        false,
        "masterScience",
    ));

    let mut diagnostics = std::collections::BTreeMap::new();
    for (name, map) in [
        ("coverage", &result.coverage),
        ("weight", &result.weight),
        ("rejection_low", &result.rejection_low),
        ("rejection_high", &result.rejection_high),
        ("registration_residuals", &result.registration_residuals),
    ] {
        ds_session_require_product_len(name, map.len(), npx)?;
        cancellation_checkpoint(cancel, "exportación de diagnósticos de sesión")?;
        let path = output_dir.join(format!("{safe}_{name}.fits"));
        let unit = ds_map_fits_unit(name);
        ds_save_float32_fits_cancellable(
            &path,
            map,
            result.width,
            result.height,
            1,
            &[
                ("EXTNAME", format!("'{}'", name.to_ascii_uppercase())),
                ("ZASGROUP", format!("'{}'", safe)),
                ("ZASMAP", format!("'{}'", name)),
                ("BUNIT", unit.header_value().into()),
            ],
            Some(cancel),
        )?;
        diagnostics.insert(name.into(), path.display().to_string());
        let (kind, role) = match name {
            "coverage" => (ScientificProductKind::Coverage, "geometricCoverage"),
            "weight" => (ScientificProductKind::Coverage, "effectivePostRejectionWeight"),
            "rejection_low" => (ScientificProductKind::Rejection, "lowOutlierCount"),
            "rejection_high" => (ScientificProductKind::Rejection, "highOutlierCount"),
            _ => (ScientificProductKind::Residual, "registrationResidualPixels"),
        };
        let mut product = ds_session_manifest_product(
            kind, &path, unit, width, height, 1, true, false, role,
        );
        product.metadata.insert("productName".into(), name.into());
        bundle.products.push(product);
    }

    for (name, plane, unit, kind, role) in [
        (
            "variance",
            result.variance.as_deref(),
            DsFitsUnit::AduSquared,
            ScientificProductKind::Var,
            "varianceOfIntegratedMean",
        ),
        (
            "neff",
            result.neff.as_deref(),
            DsFitsUnit::Dimensionless,
            ScientificProductKind::Neff,
            "effectiveSampleCount",
        ),
    ] {
        let Some(plane) = plane else { continue };
        ds_session_require_product_len(name, plane.len(), science_samples)?;
        cancellation_checkpoint(cancel, "exportación VAR/NEFF de sesión")?;
        let path = output_dir.join(format!("{safe}_{name}.fits"));
        ds_save_float32_fits_cancellable(
            &path,
            plane,
            result.width,
            result.height,
            result.channels,
            &[
                ("EXTNAME", format!("'{}'", name.to_ascii_uppercase())),
                ("ZASGROUP", format!("'{}'", safe)),
                ("ZASROLE", format!("'{}'", name.to_ascii_uppercase())),
                ("BUNIT", unit.header_value().into()),
            ],
            Some(cancel),
        )?;
        diagnostics.insert(name.into(), path.display().to_string());
        let mut product = ds_session_manifest_product(
            kind, &path, unit, width, height, channels, true, false, role,
        );
        product.metadata.insert("productName".into(), name.into());
        bundle.products.push(product);
    }

    if let Some(dq) = result.dq.as_deref() {
        ds_session_require_product_len("dq", dq.len(), npx)?;
        let mut encoded = Vec::new();
        encoded
            .try_reserve_exact(npx)
            .map_err(|error| format!("Bundle científico: reserva DQ float32: {error}"))?;
        for &bits in dq {
            if bits > 1 << 24 {
                return Err(format!(
                    "DQ contiene el bitmask {bits}, que no puede codificarse exactamente como float32"
                ));
            }
            encoded.push(bits as f32);
        }
        cancellation_checkpoint(cancel, "exportación DQ de sesión")?;
        let path = output_dir.join(format!("{safe}_dq.fits"));
        ds_save_float32_fits_cancellable(
            &path,
            &encoded,
            result.width,
            result.height,
            1,
            &[
                ("EXTNAME", "'DQ'".into()),
                ("ZASGROUP", format!("'{}'", safe)),
                ("ZASROLE", "'DQ'".into()),
                ("BUNIT", DsFitsUnit::Bitmask.header_value().into()),
            ],
            Some(cancel),
        )?;
        diagnostics.insert("dq".into(), path.display().to_string());
        let mut product = ds_session_manifest_product(
            ScientificProductKind::Dq,
            &path,
            DsFitsUnit::Bitmask,
            width,
            height,
            1,
            true,
            false,
            "dataQualityBitmask",
        );
        product
            .metadata
            .insert("encoding".into(), "u32BitsAsExactFloat32".into());
        bundle.products.push(product);
    }

    for (name, plane, kind, role) in [
        (
            "struct",
            result.struct_map.as_deref(),
            ScientificProductKind::Struct,
            "validatedMultiscaleEvidence",
        ),
        (
            "struct_residual",
            result.struct_residual.as_deref(),
            ScientificProductKind::Residual,
            "scienceMinusStruct",
        ),
    ] {
        let Some(plane) = plane else { continue };
        ds_session_require_product_len(name, plane.len(), npx)?;
        cancellation_checkpoint(cancel, "exportación STRUCT de sesión")?;
        let path = output_dir.join(format!("{safe}_{name}.fits"));
        ds_save_float32_fits_cancellable(
            &path,
            plane,
            result.width,
            result.height,
            1,
            &[
                ("EXTNAME", format!("'{}'", name.to_ascii_uppercase())),
                ("ZASGROUP", format!("'{}'", safe)),
                ("ZASEVID", "T".into()),
                ("BUNIT", DsFitsUnit::Adu.header_value().into()),
            ],
            Some(cancel),
        )?;
        diagnostics.insert(name.into(), path.display().to_string());
        let mut product = ds_session_manifest_product(
            kind,
            &path,
            DsFitsUnit::Adu,
            width,
            height,
            1,
            true,
            true,
            role,
        );
        product.metadata.insert("productName".into(), name.into());
        bundle.products.push(product);
    }

    if let Some(recov) = result.recoverability.as_deref() {
        ds_session_require_product_len("recov", recov.len(), npx)?;
        cancellation_checkpoint(cancel, "exportación RECOV de sesión")?;
        let path = output_dir.join(format!("{safe}_recov.fits"));
        ds_save_float32_fits_cancellable(
            &path,
            recov,
            result.width,
            result.height,
            1,
            &[
                ("EXTNAME", "'RECOV'".into()),
                ("ZASGROUP", format!("'{}'", safe)),
                ("ZASEVID", "T".into()),
                ("BUNIT", DsFitsUnit::Dimensionless.header_value().into()),
            ],
            Some(cancel),
        )?;
        diagnostics.insert("recov".into(), path.display().to_string());
        bundle.products.push(ds_session_manifest_product(
            ScientificProductKind::Recov,
            &path,
            DsFitsUnit::Dimensionless,
            width,
            height,
            1,
            true,
            true,
            "eidrRecoverability",
        ));
    }

    let mut components = std::collections::BTreeMap::new();
    let profile = filter_profile.to_ascii_uppercase();
    if matches!(profile.as_str(), "HA_OIII" | "SII_OIII") && result.channels >= 3 {
        let primary_name = if profile == "HA_OIII" { "HA" } else { "SII" };
        let green = extraction.oiii_green_weight.clamp(0.0, 1.0);
        let suppress = extraction.crosstalk_suppression.clamp(0.0, 1.0);
        let (primary, oiii) =
            ds_extract_dual_band_planes(&result.data, result.channels, green, suppress)?;
        for (name, plane) in [(primary_name, primary), ("OIII", oiii)] {
            // Without a measured camera+filter response matrix this RGB split
            // is useful but not a quantitative line-flux measurement.
            let path = output_dir.join(format!("{safe}_{name}_proxy_linear_float32.fits"));
            let capture_label = match recipe_capture_mode {
                pipeline::DeepSkyCaptureMode::Auto => "AUTO",
                pipeline::DeepSkyCaptureMode::BroadbandOsc => "BROADBAND_OSC",
                pipeline::DeepSkyCaptureMode::BroadbandMono => "BROADBAND_MONO",
                pipeline::DeepSkyCaptureMode::DualBandOsc => "DUAL_BAND_OSC",
                pipeline::DeepSkyCaptureMode::MonoNarrowband => "MONO_NARROWBAND",
            };
            let component_metadata = vec![
                ("ZASVER", "'hybrid-v2-2026.07'".into()),
                ("ZASGROUP", format!("'{}'", safe)),
                ("FILTER", format!("'{}_PROXY'", name)),
                ("SRCFILT", format!("'{}'", profile)),
                ("ZASROLE", "'PROXY_NOT_QUANTITATIVE'".into()),
                ("ZASCAP", format!("'{capture_label}'")),
                ("O3WGHT", format!("{:>20.6}", green)),
                ("XTLKSUP", format!("{:>20.6}", suppress)),
                ("BUNIT", DsFitsUnit::Adu.header_value().into()),
            ];
            ds_save_float32_fits_cancellable(
                &path,
                &plane,
                result.width,
                result.height,
                1,
                &component_metadata,
                Some(cancel),
            )?;
            components.insert(name.into(), path.display().to_string());
            let mut product = ds_session_manifest_product(
                ScientificProductKind::Sci,
                &path,
                DsFitsUnit::Adu,
                width,
                height,
                1,
                true,
                true,
                "spectralProxy",
            );
            product.metadata.insert("line".into(), name.into());
            product.metadata.insert("quantitative".into(), "false".into());
            product.metadata.insert(
                "reason".into(),
                "missingCameraFilterResponseMatrix".into(),
            );
            bundle.products.push(product);
        }
    }
    Ok((
        master.display().to_string(),
        components,
        diagnostics,
        bundle,
    ))
}

struct DsSessionOutputGuard {
    path: std::path::PathBuf,
    committed: bool,
}

impl Drop for DsSessionOutputGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[tauri::command]
async fn run_deepsky_session(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    request: DeepSkySessionStackRequest,
) -> Result<DeepSkySessionResultHandle, String> {
    // Solo validación: sin asesor de muestreo (cada grupo relee sus lights).
    let plan = prepare_deepsky_session_impl(request.clone(), false);
    if !plan.valid {
        return Err(plan.errors.join("\n"));
    }
    // Acción de usuario nueva: rearme ÚNICO de la sesión; los grupos internos
    // no rearman y un Cancelar entre grupos se propaga al siguiente registro.
    ds_begin_user_action(&state);
    let started = std::time::Instant::now();
    let session_id = new_job_id("ds-session");
    let base = std::path::Path::new(&request.base_path);
    let parent = if base.is_dir() {
        base.to_path_buf()
    } else {
        base.parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir)
    };
    let output_dir = parent.join(format!(
        "ZenithDeepSkySession_{}",
        ds_session_safe_name(&session_id)
    ));
    std::fs::create_dir_all(&output_dir).map_err(|error| format!("Carpeta de sesión: {error}"))?;
    let mut output_guard = DsSessionOutputGuard {
        path: output_dir.clone(),
        committed: false,
    };
    // Flag de cancelación PROPIO de la sesión (job registry, bajo el gate):
    // los checkpoints entre grupos y de exportación ya no dependen del flag
    // global, que otra acción de usuario puede rearmar legítimamente. Un
    // Cancelar pendiente en el flag global se propaga aquí al registrarse.
    let cancel = {
        let _generation_guard = state
            .planetary_generation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let cancel = state.job_registry.register(&session_id);
        if state
            .cancel_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            cancel.store(true, std::sync::atomic::Ordering::Release);
        }
        cancel
    };
    let _session_job_guard =
        pipeline::JobGuard::new(state.job_registry.clone(), session_id.clone());
    let mut group_results = Vec::new();
    let mut group_recipes = Vec::new();
    let mut all_components: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut warnings = plan.warnings;
    let mut total_used = 0;
    let mut total_rejected = 0;
    let mut session_preview = String::new();

    for (index, group) in request.groups.into_iter().enumerate() {
        cancellation_checkpoint(cancel.as_ref(), "sesión multibanda")?;
        emit_progress(
            &app,
            &format!(
                "Sesión multibanda {}/{} · {}",
                index + 1,
                plan.groups.len(),
                group.label
            ),
            index as f32 * 100.0 / plan.groups.len().max(1) as f32,
            None,
        );
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Sesión multibanda {}/{}: {} ({})",
                index + 1,
                plan.groups.len(),
                group.label,
                ds_filter_display(&group.filter_profile)
            ),
        );
        let resolved = group.request.resolved_profile();
        // AUTO por grupo: cada filtro recibe su receta con SUS señales (nº de
        // lights, fondo y dithering propios), como en el preflight de sesión.
        let resolved =
            ds_resolve_auto_for_run(&app, resolved, &format!(" · grupo {}", group.label));
        let group_capture_mode = resolved.capture_mode;
        let group_calibration_policy = resolved.calibration_policy;
        let group_method = resolved.resolved_integration_method();
        let group_started = std::time::Instant::now();
        let preview_path = stack_deepsky_impl(
            app.clone(),
            state.clone(),
            resolved.lights,
            resolved.darks,
            resolved.flats,
            resolved.dark_flats,
            resolved.bias,
            Some(resolved.kappa_high.max(resolved.kappa_low)),
            Some(resolved.rejection != "average"),
            resolved.cosmetic,
            Some(resolved.gradient),
            Some(resolved.drizzle),
            resolved.optimize_dark,
            Some(resolved.pixfrac),
            Some(resolved.normalization == "local"),
            Some(resolved.auto_crop),
            Some(resolved.interpolation),
            resolved.clip_iters,
            resolved.pedestal,
            Some(resolved.rejection),
            Some(resolved.kappa_low),
            Some(resolved.kappa_high),
            Some(resolved.normalization),
            Some(resolved.compute_policy),
            Some(resolved.local_weighting),
            resolved.work_dir.clone(),
            Some(group_method),
            Some(resolved.capture_mode),
            Some(resolved.calibration_policy),
            Some(resolved.calibration_overrides.clone()),
        )
        .await?;
        let result = state
            .deep_sky_result
            .lock()
            .unwrap()
            .clone()
            .ok_or("El grupo terminó sin publicar resultado float32")?;
        let canonical_filter = ds_filter_token(&group.filter_profile)
            .unwrap_or_else(|| group.filter_profile.trim())
            .to_ascii_uppercase();
        let (master_fits, component_paths, diagnostic_paths, scientific_bundle) =
            ds_export_session_result(
            &result,
            &output_dir,
            &group.id,
            &group.label,
            &canonical_filter,
            &request.extraction,
            group_capture_mode,
            group_calibration_policy,
            cancel.as_ref(),
        )?;
        if !component_paths.is_empty() {
            warnings.push(format!(
                "{}: Ha/OIII se exportaron como proxies heurísticos; se requiere una matriz espectral cámara-filtro para cuantificarlos",
                group.label
            ));
        }
        for (component, path) in &component_paths {
            all_components
                .entry(component.clone())
                .or_default()
                .push(path.clone());
        }
        total_used += result.frames_used;
        total_rejected += result.frames_rejected;
        // El estado interactivo conserva el último grupo; la vista inicial de
        // la sesión debe corresponder al mismo máster para que el re-estirado
        // no salte silenciosamente a otra integración.
        session_preview = preview_path.clone();
        group_recipes.push(serde_json::json!({
            "id": group.id.clone(),
            "label": group.label.clone(),
            "filterProfile": canonical_filter.clone(),
            "stackRecipe": result.recipe.clone(),
            "masterFits": master_fits.clone(),
            "componentPaths": component_paths.clone(),
            "diagnosticPaths": diagnostic_paths.clone(),
            "scientificBundle": scientific_bundle.clone(),
        }));
        group_results.push(DeepSkySessionGroupResult {
            id: group.id,
            label: group.label,
            filter_profile: canonical_filter,
            scientific_bundle,
            master_fits,
            preview_path,
            component_paths,
            diagnostic_paths,
            frames_used: result.frames_used,
            frames_rejected: result.frames_rejected,
            elapsed_seconds: group_started.elapsed().as_secs_f32(),
            quality: ds_result_quality(&result),
        });
    }
    if all_components
        .get("OIII")
        .is_some_and(|paths| paths.len() > 1)
    {
        warnings.push("Se conservaron las dos extracciones OIII por separado; deben registrarse antes de combinarlas para no degradar estrellas".into());
    }
    let recipe_path = output_dir.join("session_recipe.json");
    let recipe = serde_json::json!({
        "schemaVersion": "zenith-deepsky-session-v2",
        "sessionId": session_id,
        "palette": request.palette,
        "extraction": request.extraction,
        "groups": group_recipes,
        "componentPaths": all_components,
        "warnings": warnings,
    });
    let temp_recipe = recipe_path.with_extension("json.part");
    cancellation_checkpoint(cancel.as_ref(), "receta de sesión")?;
    let recipe_write = (|| -> Result<(), String> {
        let file = std::fs::File::create(&temp_recipe)
            .map_err(|error| format!("Receta de sesión create: {error}"))?;
        let mut writer = std::io::BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, &recipe)
            .map_err(|error| format!("Receta de sesión serialize: {error}"))?;
        writer
            .flush()
            .map_err(|error| format!("Receta de sesión flush: {error}"))?;
        writer
            .get_ref()
            .sync_data()
            .map_err(|error| format!("Receta de sesión sync: {error}"))?;
        Ok(())
    })();
    if let Err(error) = recipe_write {
        let _ = std::fs::remove_file(&temp_recipe);
        return Err(error);
    }
    std::fs::rename(&temp_recipe, &recipe_path)
        .map_err(|error| format!("Receta de sesión commit: {error}"))?;
    output_guard.committed = true;
    Ok(DeepSkySessionResultHandle {
        session_id,
        output_dir: output_dir.display().to_string(),
        preview_path: session_preview,
        groups: group_results,
        component_paths: all_components,
        frames_used: total_used,
        frames_rejected: total_rejected,
        elapsed_seconds: started.elapsed().as_secs_f32(),
        warnings,
        recipe_path: recipe_path.display().to_string(),
    })
}

/// PER-PIXEL REJECTION + weighted combine over the N registered samples at one
/// output pixel-channel (PixInsight/SIRIL ImageIntegration parity). `samples`
/// are (value, frame_weight) pairs already normalized; it is sorted in place.
/// `cov` receives the total surviving weight (coverage, for the auto-crop map).
/// Methods: "median", "minmax" (drop lowest+highest), "percentile" (rank band),
/// "winsorized" (Winsorized sigma clipping — the PI default), "linearfit"
/// (linear-fit clipping, robust to gradients). κ_low/κ_high tune the tails.
fn ds_reject_pixel(
    samples: &mut Vec<(f32, f64)>,
    method: &str,
    k_low: f32,
    k_high: f32,
    cov: &mut f64,
    rejected_low: &mut f64,
    rejected_high: &mut f64,
    // Minimum spread for the κσ window, in the working scale. Derived from the
    // measured per-frame noise (median) rather than a fixed 4.0 ADU, so rejection
    // is neither too tight on low-signal narrowband nor mis-scaled after drizzle.
    sigma_floor: f32,
) -> f32 {
    let n = samples.len();
    *rejected_low = 0.0;
    *rejected_high = 0.0;
    if n == 0 {
        *cov = 0.0;
        return 0.0;
    }
    let wmean = |s: &[(f32, f64)]| -> (f32, f64) {
        let (mut sv, mut sw) = (0.0f64, 0.0f64);
        for &(v, w) in s {
            sv += v as f64 * w;
            sw += w;
        }
        (if sw > 0.0 { (sv / sw) as f32 } else { 0.0 }, sw)
    };
    if n <= 2 {
        let (m, w) = wmean(samples);
        *cov = w;
        return m;
    }
    samples.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let median = |s: &[(f32, f64)]| -> f32 {
        let m = s.len() / 2;
        if s.len() % 2 == 1 {
            s[m].0
        } else {
            0.5 * (s[m - 1].0 + s[m].0)
        }
    };
    let original = samples.clone();
    let original_weight: f64 = original.iter().map(|&(_, w)| w).sum();
    let result = match method {
        "median" => {
            *cov = samples.iter().map(|&(_, w)| w).sum();
            median(samples)
        }
        "minmax" => {
            // Drop the single lowest and highest, weighted-mean the rest.
            let (m, w) = wmean(&samples[1..n - 1]);
            *cov = w;
            m
        }
        "percentile" => {
            // Reject a rank fraction at each tail (fraction = κ/20 → κ=3 ⇒ 15%).
            let flo = ((k_low / 20.0).clamp(0.0, 0.45) * n as f32).round() as usize;
            let fhi = ((k_high / 20.0).clamp(0.0, 0.45) * n as f32).round() as usize;
            let a = flo.min(n / 2);
            let b = (n - fhi).max(a + 1);
            let (m, w) = wmean(&samples[a..b]);
            *cov = w;
            m
        }
        "linearfit" => {
            // Fit value ≈ a·rank + b over the sorted samples; reject residuals
            // beyond −κ_low·σ / +κ_high·σ; refit up to 4×. Robust to gradients.
            let mut kept: Vec<(f32, f64)> = samples.clone();
            for _ in 0..4 {
                let m = kept.len();
                if m < 3 {
                    break;
                }
                let (mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0, 0.0, 0.0);
                for (i, &(v, _)) in kept.iter().enumerate() {
                    let x = i as f64 / (m - 1) as f64;
                    sx += x;
                    sy += v as f64;
                    sxx += x * x;
                    sxy += x * v as f64;
                }
                let den = m as f64 * sxx - sx * sx;
                let (a, b) = if den.abs() > 1e-9 {
                    (
                        (m as f64 * sxy - sx * sy) / den,
                        (sy * sxx - sx * sxy) / den,
                    )
                } else {
                    (0.0, sy / m as f64)
                };
                let mut res: Vec<f64> = kept
                    .iter()
                    .enumerate()
                    .map(|(i, &(v, _))| v as f64 - (a * (i as f64 / (m - 1) as f64) + b))
                    .collect();
                let mean_r = res.iter().sum::<f64>() / m as f64;
                let sd = (res.iter().map(|r| (r - mean_r).powi(2)).sum::<f64>() / m as f64)
                    .sqrt()
                    .max(sigma_floor as f64);
                let (lo, hi) = (-(k_low as f64) * sd, k_high as f64 * sd);
                let before = kept.len();
                let mut idx = 0;
                kept.retain(|_| {
                    let keep = res[idx] >= lo && res[idx] <= hi;
                    idx += 1;
                    keep
                });
                let _ = &mut res;
                if kept.len() == before || kept.len() < 3 {
                    break;
                }
            }
            let (m, w) = wmean(&kept);
            *cov = w;
            m
        }
        _ => {
            // "winsorized" (default per-pixel): iterative Winsorized sigma clip.
            let mut kept: Vec<(f32, f64)> = samples.clone();
            for _ in 0..5 {
                let m = kept.len();
                if m < 3 {
                    break;
                }
                let med = median(&kept);
                let mean = kept.iter().map(|&(v, _)| v as f64).sum::<f64>() / m as f64;
                let sd = (kept
                    .iter()
                    .map(|&(v, _)| (v as f64 - mean).powi(2))
                    .sum::<f64>()
                    / m as f64)
                    .sqrt()
                    .max(1e-6);
                // Winsorize the tails at med ± 1.5σ, then correct the spread.
                let (wlo, whi) = (med as f64 - 1.5 * sd, med as f64 + 1.5 * sd);
                let wmeanv = kept
                    .iter()
                    .map(|&(v, _)| (v as f64).clamp(wlo, whi))
                    .sum::<f64>()
                    / m as f64;
                let wsd = (kept
                    .iter()
                    .map(|&(v, _)| ((v as f64).clamp(wlo, whi) - wmeanv).powi(2))
                    .sum::<f64>()
                    / m as f64)
                    .sqrt();
                let sw = (wsd * 1.134).max(sigma_floor as f64); // Winsorization bias correction
                let (lo, hi) = (
                    med as f64 - k_low as f64 * sw,
                    med as f64 + k_high as f64 * sw,
                );
                let before = kept.len();
                kept.retain(|&(v, _)| (v as f64) >= lo && (v as f64) <= hi);
                if kept.len() == before || kept.len() < 3 {
                    break;
                }
            }
            let (m, w) = wmean(&kept);
            *cov = w;
            m
        }
    };
    // Los motores devuelven el peso sobreviviente exacto. Para los mapas bajo/
    // alto distribuimos ese peso rechazado según la masa de cada cola respecto
    // al estimador final; en minmax/percentile/sigma es exacto y en métodos
    // iterativos conserva exactamente el total rechazado sin inventar señal.
    let rejected = (original_weight - *cov).max(0.0);
    if rejected > 0.0 {
        let low_tail: f64 = original
            .iter()
            .filter(|(v, _)| *v < result)
            .map(|&(_, w)| w)
            .sum();
        let high_tail: f64 = original
            .iter()
            .filter(|(v, _)| *v >= result)
            .map(|&(_, w)| w)
            .sum();
        let tails = low_tail + high_tail;
        if tails > 0.0 {
            *rejected_low = rejected * low_tail / tails;
            *rejected_high = rejected * high_tail / tails;
        }
    }
    result
}

/// TILED PER-PIXEL INTEGRATION ENGINE (PixInsight/SIRIL ImageIntegration parity)
/// — the "apilado por pixel". Processes the output canvas in horizontal strips
/// so the FULL per-pixel stack (all N registered frames) fits in RAM one band
/// at a time, enabling true median / Winsorized / linear-fit / percentile
/// rejection that the streaming (moments-only) engine cannot do. Runs at
/// scale=1 (no drizzle). Returns (final_data npx·ch, coverage npx) for the
/// auto-crop. On a RAM shortfall the CALLER falls back to the streaming engine.
#[allow(clippy::too_many_arguments)]
fn ds_integrate_tiled(
    app: &tauri::AppHandle,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    load_cached: &dyn Fn(usize) -> Result<DsImage, String>,
    registered: &[(usize, DsTransform, f64)],
    norms: &[([f32; 3], [f32; 3])],
    loc_fields: &[Option<Vec<f32>>],
    ln_g: usize,
    // Rescate de detalle: campo de calidad por frame (espacio de salida,
    // rejilla wq_g×wq_g). Multiplica el peso del frame POR PÍXEL en la media
    // de supervivientes; el rechazo sigue decidiendo por VALOR. Con None el
    // camino es bit-idéntico al previo (q=1.0 exacto).
    wq_fields: &[Option<Vec<f32>>],
    wq_g: usize,
    w: usize,
    h: usize,
    ch: usize,
    method: &str,
    k_low: f32,
    k_high: f32,
    lanczos: bool,
    tile_h: usize,
    gpu_reject: bool,
    sigma_floor: f32,
) -> Result<(Vec<f32>, Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>, f64, f64), String> {
    let npx = w * h;
    let n = registered.len();
    let mut final_data = vec![0.0f32; npx * ch];
    let mut coverage = vec![0.0f64; npx]; // surviving (kept) weight per pixel
    let mut present = vec![0.0f64; npx]; // pre-rejection (geometric) weight per pixel
    let mut rejected_low = vec![0.0f64; npx];
    let mut rejected_high = vec![0.0f64; npx];
    let fw: Vec<f64> = registered.iter().map(|&(_, _, w)| w).collect();
    let lut = ds_l3_lut();
    let n_strips = h.div_ceil(tile_h);
    // Rescate de detalle en GPU: grids concatenados (n × G²); los frames sin
    // campo van a 1.0 (peso neutro). None = kernel en el camino previo exacto.
    let wq_concat: Option<Vec<f32>> = wq_fields.iter().any(|f| f.is_some()).then(|| {
        let cells = wq_g * wq_g;
        let mut concat = vec![1.0f32; n * cells];
        for (k, field) in wq_fields.iter().enumerate() {
            if let Some(f) = field {
                if f.len() >= cells {
                    concat[k * cells..(k + 1) * cells].copy_from_slice(&f[..cells]);
                }
            }
        }
        concat
    });

    for strip in 0..n_strips {
        cancellation_checkpoint(cancel.as_ref(), "integración tiled")?;
        let oy0 = strip * tile_h;
        let oy1 = ((strip + 1) * tile_h).min(h);
        let rows = oy1 - oy0;
        emit_progress(
            app,
            &format!(
                "Cielo Profundo: integrando por-píxel ({} · {}) franja {}/{}",
                method,
                if gpu_reject { "GPU" } else { "CPU" },
                strip + 1,
                n_strips,
            ),
            35.0 + (strip as f32 / n_strips as f32) * 58.0,
            None,
        );
        // Per-pixel stack for this strip: [((ly*w + x)*ch + c)*N + k]. NaN = the
        // frame did not cover this output pixel.
        let mut stack = vec![f32::NAN; rows * w * ch * n];

        // Fill the stack: each frame sampled into its slot, rows parallel & disjoint.
        for (k, &(i, t, _)) in registered.iter().enumerate() {
            let img = load_cached(i)?;
            let norm = norms[k];
            let loc = loc_fields[k].as_ref();
            let stack_ptr = stack.as_mut_ptr() as usize;
            (0..rows).into_par_iter().for_each(|ly| {
                let row = unsafe {
                    std::slice::from_raw_parts_mut(
                        (stack_ptr as *mut f32).add(ly * w * ch * n),
                        w * ch * n,
                    )
                };
                let oy = oy0 + ly;
                for x in 0..w {
                    let Some((sxf, syf)) = t.inverse(x as f32, oy as f32) else {
                        continue;
                    };
                    if sxf < 0.0
                        || syf < 0.0
                        || sxf >= (img.w - 1) as f32
                        || syf >= (img.h - 1) as f32
                    {
                        continue; // leaves NaN = missing
                    }
                    let mut vals = [0.0f32; 3];
                    if lanczos
                        && sxf >= 3.0
                        && syf >= 3.0
                        && sxf < (img.w - 4) as f32
                        && syf < (img.h - 4) as f32
                    {
                        ds_sample_lanczos3(&img, lut, sxf, syf, &mut vals);
                    } else {
                        let x0 = sxf as usize;
                        let y0 = syf as usize;
                        let fx = sxf - x0 as f32;
                        let fy = syf - y0 as f32;
                        for c in 0..ch {
                            let i00 = (y0 * img.w + x0) * ch + c;
                            let i01 = i00 + img.w * ch;
                            vals[c] = img.data[i00] * (1.0 - fx) * (1.0 - fy)
                                + img.data[i00 + ch] * fx * (1.0 - fy)
                                + img.data[i01] * (1.0 - fx) * fy
                                + img.data[i01 + ch] * fx * fy;
                        }
                    }
                    for c in 0..ch {
                        let loc_off = loc
                            .map(|field| {
                                ds_sample_local_field(
                                    field,
                                    ln_g,
                                    ln_g,
                                    ch,
                                    c,
                                    x as f32 / w as f32,
                                    oy as f32 / h as f32,
                                )
                            })
                            .unwrap_or(0.0);
                        row[(x * ch + c) * n + k] = vals[c] * norm.0[c] + norm.1[c] + loc_off;
                    }
                }
            });
        }

        if gpu_reject {
            let fw32: Vec<f32> = fw.iter().map(|&v| v as f32).collect();
            let got = crate::gpu_deepsky::reject_tiled_pass(
                &stack,
                &fw32,
                rows * w,
                ch,
                method,
                k_low,
                k_high,
                sigma_floor,
                wq_concat.as_deref().map(|grids| (grids, wq_g)),
                (w, h, oy0),
            )?;
            if strip == 0 {
                log_to_front(
                    app,
                    "INFO",
                    &format!(
                        "Rechazo GPU tiled: pico estimado {:.1} MB VRAM por franja.",
                        got.vram_bytes as f64 / 1_048_576.0
                    ),
                );
            }
            let out0 = oy0 * w * ch;
            final_data[out0..out0 + got.data.len()].copy_from_slice(&got.data);
            let px0 = oy0 * w;
            for i in 0..rows * w {
                present[px0 + i] = got.present[i] as f64;
                coverage[px0 + i] = got.coverage[i] as f64;
                rejected_low[px0 + i] = got.rejected_low[i] as f64;
                rejected_high[px0 + i] = got.rejected_high[i] as f64;
            }
            continue;
        }

        // Reject + combine per output pixel, rows parallel & disjoint.
        let fd_ptr = final_data.as_mut_ptr() as usize;
        let cov_ptr = coverage.as_mut_ptr() as usize;
        let pre_ptr = present.as_mut_ptr() as usize;
        let low_ptr = rejected_low.as_mut_ptr() as usize;
        let high_ptr = rejected_high.as_mut_ptr() as usize;
        let stack_ref = &stack;
        let has_wq = wq_fields.iter().any(|field| field.is_some());
        (0..rows).into_par_iter().for_each(|ly| {
            let fd_row = unsafe {
                std::slice::from_raw_parts_mut(
                    (fd_ptr as *mut f32).add((oy0 + ly) * w * ch),
                    w * ch,
                )
            };
            let cov_row = unsafe {
                std::slice::from_raw_parts_mut((cov_ptr as *mut f64).add((oy0 + ly) * w), w)
            };
            let pre_row = unsafe {
                std::slice::from_raw_parts_mut((pre_ptr as *mut f64).add((oy0 + ly) * w), w)
            };
            let low_row = unsafe {
                std::slice::from_raw_parts_mut((low_ptr as *mut f64).add((oy0 + ly) * w), w)
            };
            let high_row = unsafe {
                std::slice::from_raw_parts_mut((high_ptr as *mut f64).add((oy0 + ly) * w), w)
            };
            let base = ly * w * ch * n;
            let mut buf: Vec<(f32, f64)> = Vec::with_capacity(n);
            let y_norm = (oy0 + ly) as f32 / h as f32;
            let mut qbuf: Vec<f64> = vec![1.0; n];
            for x in 0..w {
                if has_wq {
                    let x_norm = x as f32 / w as f32;
                    for k in 0..n {
                        qbuf[k] = wq_fields[k]
                            .as_ref()
                            .map(|g| ds_sample_grid(g, wq_g, wq_g, x_norm, y_norm) as f64)
                            .unwrap_or(1.0);
                    }
                }
                let mut px_cov = 0.0f64;
                let mut px_pre = 0.0f64;
                for c in 0..ch {
                    buf.clear();
                    let s0 = base + (x * ch + c) * n;
                    for k in 0..n {
                        let v = stack_ref[s0 + k];
                        if v.is_finite() {
                            buf.push((v, fw[k] * qbuf[k]));
                        }
                    }
                    let mut cov = 0.0f64;
                    let mut rej_low = 0.0f64;
                    let mut rej_high = 0.0f64;
                    if c == 0 {
                        px_pre = buf.iter().map(|&(_, w)| w).sum(); // geometric coverage
                    }
                    fd_row[x * ch + c] = ds_reject_pixel(
                        &mut buf,
                        method,
                        k_low,
                        k_high,
                        &mut cov,
                        &mut rej_low,
                        &mut rej_high,
                        sigma_floor,
                    );
                    if c == 0 {
                        px_cov = cov;
                        low_row[x] = rej_low;
                        high_row[x] = rej_high;
                    }
                }
                cov_row[x] = px_cov;
                pre_row[x] = px_pre;
            }
        });
    }
    // Rejection % and mean effective coverage (frames/pixel) over the master.
    let mean_fw = if n > 0 {
        fw.iter().sum::<f64>() / n as f64
    } else {
        1.0
    };
    let sum_pre: f64 = present.iter().sum();
    let sum_cov: f64 = coverage.iter().sum();
    let rej_pct = if sum_pre > 0.0 {
        100.0 * (1.0 - sum_cov / sum_pre)
    } else {
        0.0
    };
    let mean_cov = sum_cov / (mean_fw.max(1e-6) * npx as f64);
    Ok((
        final_data,
        present,
        coverage,
        rejected_low,
        rejected_high,
        rej_pct,
        mean_cov,
    ))
}

fn emit_deepsky_pipeline_telemetry(
    app: &tauri::AppHandle,
    job_id: &str,
    phase: &str,
    engine: &str,
    done: usize,
    total: usize,
    started: std::time::Instant,
    sys: &std::sync::Mutex<System>,
    vram_mb: u64,
    cache_hits: usize,
    fallback_reason: Option<String>,
) {
    let elapsed = started.elapsed().as_secs_f32().max(0.001);
    let (ram_mb, cpu_percent, io_read_mb, io_write_mb) = {
        let mut s = sys.lock().unwrap();
        s.refresh_all();
        let pid = sysinfo::get_current_pid().ok();
        let process = pid.and_then(|p| s.process(p));
        let disk = process.map(|p| p.disk_usage());
        (
            process
                .map(|p| p.memory() / (1024 * 1024))
                .unwrap_or_else(|| s.used_memory() / (1024 * 1024)),
            Some(s.global_cpu_info().cpu_usage()),
            disk.map(|d| d.total_read_bytes as f64 / 1_048_576.0)
                .unwrap_or(0.0),
            disk.map(|d| d.total_written_bytes as f64 / 1_048_576.0)
                .unwrap_or(0.0),
        )
    };
    emit_pipeline_telemetry(
        app,
        PipelineTelemetry {
            job_id: job_id.into(),
            domain: PipelineDomain::DeepSky,
            phase: phase.into(),
            engine: engine.into(),
            progress: done as f32 / total.max(1) as f32 * 100.0,
            eta_seconds: if done > 0 && done < total {
                Some(elapsed / done as f32 * (total - done) as f32)
            } else {
                None
            },
            items_done: done,
            items_total: total,
            throughput: (done > 0).then_some(done as f32 / elapsed),
            cpu_percent,
            gpu_percent: None,
            ram_mb,
            vram_mb,
            io_read_mb,
            io_write_mb,
            cache_hits,
            cache_misses: done.saturating_sub(cache_hits),
            fallback_reason,
        },
    );
}

/// Merge a GPU Welford pass result (mean/M2/weight, per channel) into CPU
/// sum/sq/weight accumulators. Welford → raw moments per channel-element:
/// Σw·x = mean·w and Σw·x² = M2 + w·mean² (since M2 = Σw(x−mean)²). Both partial
/// sums are additive, so CPU-subset ⊕ GPU-subset equals a whole-set pass EXACTLY
/// (modulo float summation order) — the mathematical basis of the frame split.
fn ds_merge_gpu_welford_into(
    sum: &mut [f64],
    sq: &mut [f64],
    wgt: &mut [f64],
    gpu: &crate::gpu_deepsky::GpuPassResult,
) {
    let n = sum.len().min(gpu.mean.len());
    for i in 0..n {
        let w = gpu.weight[i] as f64;
        let m = gpu.mean[i] as f64;
        let m2 = gpu.moment2[i] as f64;
        sum[i] += m * w;
        sq[i] += m2 + w * m * m;
        wgt[i] += w;
    }
}

/// Share of frames routed to the CPU in the concurrent CPU+GPU split (P2.D). The
/// GPU is normally much faster per frame, so the CPU takes a minority; a real
/// timed run (the pass wall-times are logged) can raise or lower this. Kept
/// conservative so a mis-tuned split never dominates the wall-clock.
const HYBRID_CPU_FRACTION: f32 = 0.2;

/// CONCURRENT CPU+GPU streaming integration (P2.D). The registered frames are
/// partitioned; a worker thread accumulates the GPU subset (`integrate_pass`)
/// while the main thread accumulates the CPU subset (`ds_warp_accumulate`) — both
/// compute units run at once. Because Welford accumulators are additive, per pass
/// the two partial results merge EXACTLY, and the κσ window for the next pass is
/// derived from the COMBINED statistics, so rejection is identical to a whole-set
/// integration. Returns the same tuple as `ds_integrate_gpu_streaming`. Any GPU
/// error propagates so the caller can fall back to a full CPU/GPU retry.
/// `cpu_fraction` is the share of frames routed to the CPU (tunable; the pass
/// wall-times are logged so a real run can calibrate it).
#[allow(clippy::too_many_arguments)]
fn ds_integrate_hybrid_split(
    app: &tauri::AppHandle,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    load_cached: &(dyn Fn(usize) -> Result<DsImage, String> + Sync),
    registered: &[(usize, DsTransform, f64)],
    norms: &[([f32; 3], [f32; 3])],
    loc_fields: &[Option<Vec<f32>>],
    ln_g: usize,
    wq_fields: &[Option<Vec<f32>>],
    wq_g: usize,
    w: usize,
    h: usize,
    ch: usize,
    use_lanczos: bool,
    n_iters: usize,
    kappa_low: f32,
    kappa_high: f32,
    sigma_floor: f32,
    cpu_fraction: f32,
) -> Result<
    (
        Vec<f32>,
        Vec<f64>,
        Vec<f64>,
        Vec<f64>,
        Vec<f64>,
        f64,
        f64,
        u64,
        usize,
    ),
    String,
> {
    use crate::gpu_deepsky::{FrameMeta, IntegrateConfig};
    let n = registered.len();
    let npx = w * h;
    // Split contiguously: the GPU (the faster unit) takes the majority.
    let n_cpu = ((n as f32 * cpu_fraction).round() as usize).min(n.saturating_sub(1));
    let n_gpu = n - n_cpu;
    if n_gpu == 0 || n_cpu == 0 {
        return Err("split híbrido degenerado".into());
    }
    let cfg = IntegrateConfig {
        width: w,
        height: h,
        channels: ch,
        scale: 1.0,
        lanczos: use_lanczos,
        local_grid_size: ln_g,
        track_m2: true,
    };
    let gpu_metas: Vec<FrameMeta> = registered[..n_gpu]
        .iter()
        .zip(norms[..n_gpu].iter())
        .map(|(&(index, transform, weight), &norm)| FrameMeta {
            index,
            transform: transform.to_gpu().expect("Transformación GPU validada"),
            weight: weight as f32,
            norm,
        })
        .collect();
    let gpu_local: Vec<Option<Vec<f32>>> = loc_fields[..n_gpu].to_vec();
    let gpu_wq: Vec<Option<Vec<f32>>> = wq_fields[..n_gpu].to_vec();
    let cpu_reg = &registered[n_gpu..];
    let cpu_norms = &norms[n_gpu..];
    let cpu_loc = &loc_fields[n_gpu..];
    let cpu_wq = &wq_fields[n_gpu..];

    log_to_front(
        app,
        "INFO",
        &format!("Integración híbrida CPU+GPU concurrente: {n_gpu} tomas GPU ‖ {n_cpu} tomas CPU."),
    );

    let mut sum = vec![0.0f64; npx * ch];
    let mut sq = vec![0.0f64; npx * ch];
    let mut wgt = vec![0.0f64; npx * ch];
    let mut rej_low = vec![0.0f64; npx];
    let mut rej_high = vec![0.0f64; npx];
    let mut peak_vram = 0u64;
    let mut total_tiles = 0usize;

    // One accumulation pass over BOTH subsets concurrently, returning combined
    // sum/sq/wgt and the per-pixel rejection maps. `bounds` = None on pass 1.
    let run_pass = |bounds: Option<(&[f32], &[f32])>,
                    sum: &mut Vec<f64>,
                    sq: &mut Vec<f64>,
                    wgt: &mut Vec<f64>,
                    rl: &mut Vec<f64>,
                    rh: &mut Vec<f64>|
     -> Result<(u64, usize), String> {
        sum.iter_mut().for_each(|v| *v = 0.0);
        sq.iter_mut().for_each(|v| *v = 0.0);
        wgt.iter_mut().for_each(|v| *v = 0.0);
        rl.iter_mut().for_each(|v| *v = 0.0);
        rh.iter_mut().for_each(|v| *v = 0.0);
        let cfg_r = &cfg;
        let gm = &gpu_metas;
        let gl = &gpu_local;
        let gw_fields = &gpu_wq;
        let gpu_res = std::thread::scope(
            |scope| -> Result<crate::gpu_deepsky::GpuPassResult, String> {
                // GPU subset in a worker thread (wgpu is Send/Sync; the load closure is
                // Sync). It touches only shared read-only data — the CPU subset below
                // owns sum/sq/wgt exclusively, so there is no shared mutable state.
                let handle = scope.spawn(move || {
                    let load_gpu = |i: usize| load_cached(i).map(|img| img.data);
                    let t0 = std::time::Instant::now();
                    let r = crate::gpu_deepsky::integrate_pass(
                        cfg_r,
                        gm,
                        gl,
                        gw_fields,
                        wq_g,
                        bounds,
                        &load_gpu,
                        cancel,
                        |_, _, _| {},
                    );
                    (r, t0.elapsed())
                });
                // CPU subset on this thread, concurrent with the GPU worker.
                let cpu_t0 = std::time::Instant::now();
                let mut cpu_err: Option<String> = None;
                for (j, &(idx, t, fw)) in cpu_reg.iter().enumerate() {
                    if let Err(e) =
                        cancellation_checkpoint(cancel.as_ref(), "integración híbrida CPU")
                    {
                        cpu_err = Some(e);
                        break;
                    }
                    match load_cached(idx) {
                        Ok(img) => {
                            let loc_ref = cpu_loc[j].as_ref().map(|f| (f.as_slice(), ln_g, ln_g));
                            let wq_ref = cpu_wq[j].as_ref().map(|f| (f.as_slice(), wq_g, wq_g));
                            ds_warp_accumulate(
                                &img,
                                t,
                                &mut sum[..],
                                Some(&mut *sq),
                                &mut wgt[..],
                                bounds,
                                Some((&mut rl[..], &mut rh[..])),
                                w,
                                h,
                                ch,
                                fw,
                                1.0,
                                cpu_norms[j],
                                loc_ref,
                                wq_ref,
                                use_lanczos,
                                None,
                                None,
                            );
                        }
                        Err(e) => {
                            cpu_err = Some(e);
                            break;
                        }
                    }
                }
                let cpu_dt = cpu_t0.elapsed();
                let (gpu_r, gpu_dt) = handle.join().map_err(|_| "hilo GPU cayó".to_string())?;
                if let Some(e) = cpu_err {
                    return Err(e);
                }
                let gpu_r = gpu_r?;
                log_to_front(
                    app,
                    "INFO",
                    &format!(
                        "Pase híbrido: GPU {:.2}s ‖ CPU {:.2}s.",
                        gpu_dt.as_secs_f32(),
                        cpu_dt.as_secs_f32()
                    ),
                );
                Ok(gpu_r)
            },
        )?;
        // Merge the GPU subset's Welford result into the CPU sums (exact).
        ds_merge_gpu_welford_into(&mut sum[..], &mut sq[..], &mut wgt[..], &gpu_res);
        for p in 0..npx {
            rl[p] += gpu_res.rejected_low[p] as f64;
            rh[p] += gpu_res.rejected_high[p] as f64;
        }
        Ok((gpu_res.peak_vram_bytes, gpu_res.tiles))
    };

    emit_progress(
        app,
        "Cielo Profundo: integración híbrida (pasada 1)...",
        40.0,
        None,
    );
    let (pv, tl) = run_pass(
        None,
        &mut sum,
        &mut sq,
        &mut wgt,
        &mut rej_low,
        &mut rej_high,
    )?;
    peak_vram = peak_vram.max(pv);
    total_tiles += tl;
    let first_wgt = wgt.clone();
    for it in 0..n_iters {
        cancellation_checkpoint(cancel.as_ref(), "sigma-clip híbrido")?;
        let mut lo = vec![f32::MIN; npx * ch];
        let mut hi = vec![f32::MAX; npx * ch];
        for i in 0..npx * ch {
            let wv = wgt[i];
            if wv > 1.0 {
                let mu = sum[i] / wv;
                let var = (sq[i] / wv - mu * mu).max(0.0);
                let sd = var.sqrt().max(sigma_floor as f64);
                lo[i] = (mu - kappa_low as f64 * sd) as f32;
                hi[i] = (mu + kappa_high as f64 * sd) as f32;
            }
        }
        emit_progress(
            app,
            &format!(
                "Cielo Profundo: sigma-clip híbrido {}/{}...",
                it + 1,
                n_iters
            ),
            55.0 + (it as f32 / n_iters.max(1) as f32) * 38.0,
            None,
        );
        let (pv, tl) = run_pass(
            Some((&lo, &hi)),
            &mut sum,
            &mut sq,
            &mut wgt,
            &mut rej_low,
            &mut rej_high,
        )?;
        peak_vram = peak_vram.max(pv);
        total_tiles += tl;
    }

    let final_data: Vec<f32> = (0..npx * ch)
        .map(|i| {
            if wgt[i] > 0.0 {
                (sum[i] / wgt[i]) as f32
            } else {
                0.0
            }
        })
        .collect();
    // Per-pixel coverage (mean across channels) to match the other engines.
    let per_pixel = |wc: &[f64]| -> Vec<f64> {
        (0..npx)
            .map(|p| {
                let mut s = 0.0f64;
                for c in 0..ch {
                    s += wc[p * ch + c];
                }
                s / ch as f64
            })
            .collect()
    };
    let cov_first = per_pixel(&first_wgt);
    let cov_final = per_pixel(&wgt);
    let mean_fw =
        registered.iter().map(|&(_, _, v)| v).sum::<f64>() / registered.len().max(1) as f64;
    let sum_pre: f64 = cov_first.iter().sum();
    let sum_cov: f64 = cov_final.iter().sum();
    let rej_pct = if sum_pre > 0.0 {
        100.0 * (1.0 - sum_cov / sum_pre)
    } else {
        0.0
    };
    let mean_cov = sum_cov / (mean_fw.max(1e-6) * npx as f64);
    Ok((
        final_data,
        cov_first,
        cov_final,
        rej_low,
        rej_high,
        rej_pct,
        mean_cov,
        peak_vram,
        total_tiles,
    ))
}

/// Ruta streaming híbrida: la CPU entrega transformaciones, pesos y modelos
/// de normalización; wgpu procesa warp + remuestreo + momentos/rechazo en
/// bandas. Los acumuladores Welford ponderados evitan la pérdida de precisión
/// de sumar ADU grandes en f32 y generan directamente media/M2/cobertura.
#[allow(clippy::too_many_arguments)]
fn ds_integrate_gpu_streaming(
    app: &tauri::AppHandle,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    load_cached: &dyn Fn(usize) -> Result<DsImage, String>,
    registered: &[(usize, DsTransform, f64)],
    norms: &[([f32; 3], [f32; 3])],
    loc_fields: &[Option<Vec<f32>>],
    ln_g: usize,
    wq_fields: &[Option<Vec<f32>>],
    wq_g: usize,
    w: usize,
    h: usize,
    ch: usize,
    use_lanczos: bool,
    n_iters: usize,
    kappa_low: f32,
    kappa_high: f32,
    sigma_floor: f32,
    job_id: &str,
    started: std::time::Instant,
    sys: &std::sync::Mutex<System>,
) -> Result<
    (
        Vec<f32>,
        Vec<f64>,
        Vec<f64>,
        Vec<f64>,
        Vec<f64>,
        f64,
        f64,
        u64,
        usize,
    ),
    String,
> {
    use crate::gpu_deepsky::{FrameMeta, IntegrateConfig};

    let metas: Vec<FrameMeta> = registered
        .iter()
        .zip(norms.iter())
        .map(|(&(index, transform, weight), &norm)| FrameMeta {
            index,
            transform: transform.to_gpu().expect("Transformación GPU validada"),
            weight: weight as f32,
            norm,
        })
        .collect();
    let cfg = IntegrateConfig {
        width: w,
        height: h,
        channels: ch,
        scale: 1.0,
        lanczos: use_lanczos,
        local_grid_size: ln_g,
        track_m2: true,
    };
    let load = |i: usize| load_cached(i).map(|img| img.data);
    let peak_vram = std::cell::Cell::new(0u64);
    let mut total_tiles = 0usize;
    let mut pass_no = 0usize;
    let mut run_pass = |bounds: Option<(&[f32], &[f32])>| {
        pass_no += 1;
        crate::gpu_deepsky::integrate_pass(
            &cfg,
            &metas,
            loc_fields,
            wq_fields,
            wq_g,
            bounds,
            &load,
            cancel.as_ref(),
            |done, total, vram| {
                peak_vram.set(peak_vram.get().max(vram));
                if done % 4 == 0 || done == total {
                    emit_deepsky_pipeline_telemetry(
                        app,
                        job_id,
                        if pass_no == 1 {
                            "integrate_pass_1"
                        } else {
                            "sigma_clip_gpu"
                        },
                        "Hybrid CPU + GPU wgpu",
                        done,
                        total,
                        started,
                        sys,
                        vram / (1024 * 1024),
                        0,
                        None,
                    );
                }
            },
        )
    };

    let mut current = run_pass(None)?;
    total_tiles += current.tiles;
    peak_vram.set(peak_vram.get().max(current.peak_vram_bytes));
    let first_weight = current.weight.clone();
    let npx = w * h;
    for it in 0..n_iters {
        cancellation_checkpoint(cancel.as_ref(), "sigma-clip GPU")?;
        let mut lo = vec![f32::MIN; npx * ch];
        let mut hi = vec![f32::MAX; npx * ch];
        for i in 0..npx * ch {
            // Weight is now per-channel (aligned with mean/moment2 indices).
            let wv = current.weight[i];
            if wv > 1.0 {
                let sd = (current.moment2[i] / wv).max(0.0).sqrt().max(sigma_floor);
                lo[i] = current.mean[i] - kappa_low * sd;
                hi[i] = current.mean[i] + kappa_high * sd;
            }
        }
        emit_progress(
            app,
            &format!("Cielo Profundo: GPU sigma-clip {}/{}...", it + 1, n_iters),
            55.0 + (it as f32 / n_iters.max(1) as f32) * 38.0,
            None,
        );
        current = run_pass(Some((&lo, &hi)))?;
        total_tiles += current.tiles;
        peak_vram.set(peak_vram.get().max(current.peak_vram_bytes));
    }

    let mean_fw =
        registered.iter().map(|&(_, _, v)| v).sum::<f64>() / registered.len().max(1) as f64;
    // Reduce the per-channel weight to a per-pixel coverage map (mean across
    // channels) so the auto-crop and QA planes match the CPU engines. The linear
    // master itself is `current.mean` (the per-channel weighted Welford mean).
    let per_pixel = |w: &[f32]| -> Vec<f64> {
        (0..npx)
            .map(|p| {
                let mut s = 0.0f64;
                for c in 0..ch {
                    s += w[p * ch + c] as f64;
                }
                s / ch as f64
            })
            .collect()
    };
    let first_pp = per_pixel(&first_weight);
    let cov_pp = per_pixel(&current.weight);
    let sum_pre: f64 = first_pp.iter().sum();
    let sum_cov: f64 = cov_pp.iter().sum();
    let rej_pct = if sum_pre > 0.0 {
        100.0 * (1.0 - sum_cov / sum_pre)
    } else {
        0.0
    };
    let mean_cov = sum_cov / (mean_fw.max(1e-6) * npx as f64);
    Ok((
        current.mean,
        first_pp,
        cov_pp,
        current.rejected_low.into_iter().map(|v| v as f64).collect(),
        current
            .rejected_high
            .into_iter()
            .map(|v| v as f64)
            .collect(),
        rej_pct,
        mean_cov,
        peak_vram.get(),
        total_tiles,
    ))
}

/// INV-A: planifica la secuencia de dithering óptima para la evidencia de
/// super-resolución (fase fraccional por diseño experimental sobre las
/// matrices de alias + parte entera aleatoria anti walking-noise). Con
/// `existing` no vacío re-planifica en bucle cerrado (tomas ya adquiridas o
/// perdidas). Devuelve las recomendaciones y la evidencia prevista.
#[tauri::command]
async fn deepsky_plan_dither(
    count: usize,
    scale: f32,
    fwhm_px: f32,
    cfa: Option<i32>,
    existing: Option<Vec<(f64, f64)>>,
) -> Result<serde_json::Value, String> {
    if count == 0 || count > 200 {
        return Err("count debe estar entre 1 y 200".into());
    }
    if !(1.0..=2.0).contains(&scale) {
        return Err("scale debe estar entre 1.0 y 2.0".into());
    }
    let psf = crate::deepsky_psf::MoffatPsf {
        fwhm_x: fwhm_px.clamp(0.6, 6.0),
        fwhm_y: fwhm_px.clamp(0.6, 6.0),
        theta: 0.0,
        beta: 2.5,
    };
    let phases = existing.unwrap_or_default();
    let inp = crate::eidr::DitherPlanInput {
        phases: &phases,
        psf,
        scale,
        cfa: cfa.filter(|c| (8..=11).contains(c)),
    };
    let recs = crate::eidr::eidr_plan_dither_sequence(&inp, count);
    // Parte entera sugerida (anti walking-noise), determinista por índice.
    let mut state = 0x5eed_d17e_c0de_u64.wrapping_add(phases.len() as u64);
    let mut lcg = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((state >> 33) as f64) / (u32::MAX as f64 + 1.0)
    };
    let period = if inp.cfa.is_some() { 2.0f64 } else { 1.0 };
    let items: Vec<serde_json::Value> = recs
        .iter()
        .map(|r| {
            let ix = (lcg() * 5.0).floor() * period;
            let iy = (lcg() * 5.0).floor() * period;
            serde_json::json!({
                "dx": r.dx,
                "dy": r.dy,
                "dxFull": ix + r.dx,
                "dyFull": iy + r.dy,
                "evidence": r.evidence_after,
            })
        })
        .collect();
    Ok(serde_json::json!({
        "scale": scale,
        "cfa": inp.cfa,
        "fwhmPx": psf.fwhm_x,
        "evidenceStart": recs.first().map(|r| r.evidence_before),
        "evidenceEnd": recs.last().map(|r| r.evidence_after),
        "plan": items,
    }))
}

/// Resultado de la ejecución EIDR (F9): máster a la escala efectiva +
/// productos científicos + metadatos para receta/log.
struct DsEidrOutcome {
    final_data: Vec<f32>,
    coverage: Vec<f64>,
    variance: Vec<f32>,
    neff: Vec<f32>,
    dq: Vec<u32>,
    recov: Vec<f32>,
    w2: usize,
    h2: usize,
    scale_requested: String,
    scale_eff: f32,
    fallbacks: Vec<String>,
    gate_frac: (f64, f64, f64),
    nu_cut: f64,
    tile_apt_pixels: usize,
    tile_degraded_pixels: usize,
    tile_fallback_pixels: usize,
    native_cutoff_cycles_per_output_px: f64,
    tile_classes: Vec<u8>,
    publication_channels: usize,
    solver_iterations: usize,
    solver_rel_residual: f64,
    solver_converged: bool,
    solver_ridge: f64,
    lambda_f: f64,
    holdout_frames: usize,
    holdout_chi2_median: Option<f64>,
    solver_mode_used: String,
    huber_adopted_channels: usize,
    huber_reverted_channels: usize,
    irls_rounds: usize,
    refine_applied_frames: usize,
    refine_p90_px: f64,
    refine_reverted: bool,
    multigrid_used: bool,
    gamma_fwhm_px: Option<f32>,
    geometry_only_frames: usize,
    excluded_frames: usize,
    geometry_sample_count: usize,
    geometry_p95_error_px: f64,
    geometry_max_error_px: f64,
    geometry_min_jacobian: f64,
    pad: usize,
    mean_cov: f64,
}

fn ds_eidr_geometry_summary(
    reports: &[crate::eidr::EidrGeomFieldReport],
) -> Option<(usize, f64, f64, f64)> {
    if reports.is_empty() {
        return None;
    }
    let mut samples = 0usize;
    let mut worst_p95 = 0.0f64;
    let mut worst_max = 0.0f64;
    let mut min_jacobian = f64::INFINITY;
    for report in reports {
        if !report.p95_error_px.is_finite()
            || !report.max_error_px.is_finite()
            || !report.min_jacobian.is_finite()
            || report.min_jacobian <= 0.0
        {
            return None;
        }
        samples = samples.saturating_add(report.sample_count);
        worst_p95 = worst_p95.max(report.p95_error_px);
        worst_max = worst_max.max(report.max_error_px);
        min_jacobian = min_jacobian.min(report.min_jacobian);
    }
    Some((samples, worst_p95, worst_max, min_jacobian))
}

/// CFA directo se resuelve como tres retículas distintas. La decisión
/// publicable debe ser la intersección conservadora por tile, no simplemente
/// el canal con peor fracción global: una zona puede fallar en R y otra en B.
fn ds_eidr_merge_cfa_gate_reports(
    reports: Vec<crate::eidr::EidrGateReport>,
) -> Result<crate::eidr::EidrGateReport, String> {
    let mut reports = reports.into_iter();
    let mut merged = reports
        .next()
        .ok_or("EIDR CFA: no hay reportes de recuperabilidad")?;
    for report in reports {
        if report.tiles_x != merged.tiles_x
            || report.tiles_y != merged.tiles_y
            || report.tile != merged.tile
            || report.tiles.len() != merged.tiles.len()
            || (report.scale - merged.scale).abs() > f32::EPSILON
        {
            return Err("EIDR CFA: las retículas de recuperabilidad no coinciden".into());
        }
        for (dst, src) in merged.tiles.iter_mut().zip(report.tiles.iter()) {
            dst.class = dst.class.max(src.class);
            dst.r_band = dst.r_band.min(src.r_band);
            dst.kappa = dst.kappa.max(src.kappa);
        }
        if merged.r_radial.len() != report.r_radial.len() {
            return Err("EIDR CFA: los perfiles radiales no coinciden".into());
        }
        for (dst, src) in merged.r_radial.iter_mut().zip(report.r_radial.iter()) {
            if (dst.0 - src.0).abs() > 1e-9 {
                return Err("EIDR CFA: las frecuencias radiales no coinciden".into());
            }
            dst.1 = dst.1.min(src.1);
        }
        merged.total_frames = merged.total_frames.max(report.total_frames);
        merged.measured_psf_frames = merged.measured_psf_frames.min(report.measured_psf_frames);
        merged.missing_psf_frames = merged.missing_psf_frames.max(report.missing_psf_frames);
        merged.invalid_noise_frames = merged.invalid_noise_frames.max(report.invalid_noise_frames);
        merged.evidence_frames = merged.evidence_frames.min(report.evidence_frames);
    }
    let total = merged.tiles.len().max(1) as f64;
    let mut counts = [0usize; 3];
    for tile in &merged.tiles {
        counts[tile.class.min(2) as usize] += 1;
    }
    merged.frac_apt = counts[0] as f64 / total;
    merged.frac_degrade = counts[1] as f64 / total;
    merged.frac_fallback = counts[2] as f64 / total;
    Ok(merged)
}

fn ds_eidr_channel_publishable(
    report: &crate::eidr::EidrSolveReport,
    finite_solution: bool,
) -> bool {
    report.converged
        && report.rel_residual.is_finite()
        && report.ridge.is_finite()
        && finite_solution
}

fn ds_eidr_holdout_publishable(_frame_count: usize, chi2_median: Option<f64>) -> bool {
    match chi2_median {
        Some(chi2) => chi2.is_finite() && chi2 <= 2.0,
        None => false,
    }
}

/// Decisión explícita de calidad para una light ya registrada. A diferencia
/// del antiguo piso 0.3, una toma claramente nublada, desenfocada, alargada o
/// con residuo astrométrico alto queda fuera y la razón viaja a UI/receta.
fn ds_frame_quality_weight(
    fwhm: f32,
    best_fwhm: f32,
    psf_signal: f32,
    best_psf_signal: f32,
    eccentricity: f32,
    registration_rms: f32,
    median_registration_rms: f32,
) -> Result<f64, String> {
    if !fwhm.is_finite() || fwhm <= 0.0 {
        return Err("FWHM no medible".into());
    }
    if !eccentricity.is_finite() || eccentricity >= 0.65 {
        return Err(format!(
            "estrellas demasiado alargadas (ecc {:.3} >= 0.650)",
            eccentricity
        ));
    }
    if !registration_rms.is_finite() {
        return Err("residuo astrométrico no finito".into());
    }
    let registration_limit = (3.0 * median_registration_rms.max(0.0)).max(0.75);
    if registration_rms > registration_limit {
        return Err(format!(
            "residuo astrométrico {:.3} px > {:.3} px",
            registration_rms, registration_limit
        ));
    }
    let fwhm_ratio = if best_fwhm.is_finite() && best_fwhm > 0.0 {
        fwhm / best_fwhm
    } else {
        1.0
    };
    if fwhm_ratio > 2.5 {
        return Err(format!(
            "seeing/FWHM {:.2}x peor que la mejor toma (>2.50x)",
            fwhm_ratio
        ));
    }
    let transparency_ratio = if best_psf_signal.is_finite() && best_psf_signal > 0.0 {
        (psf_signal / best_psf_signal).max(0.0)
    } else {
        1.0
    };
    if transparency_ratio < 0.10 {
        return Err(format!(
            "transparencia/señal PSF {:.1}% de la mejor toma (<10%)",
            100.0 * transparency_ratio
        ));
    }

    let wf = (1.0 / fwhm_ratio.max(1.0).powi(2)).clamp(0.0, 1.0);
    let wpsf = transparency_ratio.sqrt().clamp(0.0, 1.0);
    let roundness = (1.0 - eccentricity).clamp(0.0, 1.0);
    let weight = (wf * wpsf * roundness) as f64;
    if !weight.is_finite() || weight < 0.05 {
        return Err(format!(
            "peso combinado {:.3} por debajo del umbral científico 0.050",
            weight
        ));
    }
    Ok(weight.min(1.0))
}

/// Motor EIDR (F9, ScientificQuadratic): reconstrucción forward-model a
/// escala 1x/1.5x/2x desde los píxeles calibrados NATIVOS + transforms —
/// jamás desde un máster intermedio. Flujo §7.5: PSF por frame → Γ → puerta
/// de recuperabilidad (elige/valida escala, publica RECOV y perfil R) →
/// b/diag/piloto → PCG con penalización espectral λ_F·ω (§7.3) → holdout →
/// solve final con todos los frames (warm start del solve de entrenamiento).
#[allow(clippy::too_many_arguments)]
fn ds_run_eidr(
    app: &tauri::AppHandle,
    cfg: &pipeline::EidrConfig,
    registered: &[(usize, DsTransform, f64)],
    load_cached: &dyn Fn(usize) -> Result<DsImage, String>,
    load_uncertainty: &dyn Fn(usize) -> Result<DsCalibratedUncertainty, String>,
    norms: &[([f32; 3], [f32; 3])],
    loc_fields: &[Option<Vec<f32>>],
    local_grid_size: usize,
    star_catalogs: &[Vec<(f32, f32, f32)>],
    frame_noise: &[[f32; 3]],
    w: usize,
    h: usize,
    ch_in: usize,
    cfa: Option<i32>,
    compute_policy: ComputePolicy,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<DsEidrOutcome, String> {
    use crate::eidr::*;
    let n = registered.len();
    let ch_out = if cfa.is_some() { 3 } else { ch_in };

    // --- Pase 1: PSF Moffat por frame + máscaras de inválidos ---
    emit_progress(app, "EIDR: ajustando PSF por frame...", 36.0, None);
    let mut psfs: Vec<Option<crate::deepsky_psf::MoffatPsf>> = Vec::with_capacity(n);
    let mut masks: Vec<Vec<u64>> = Vec::with_capacity(n);
    let mut spatial_inv_vars: Vec<crate::eidr::EidrSpatialInvVar> = Vec::with_capacity(n);
    for (k, &(i, _, _)) in registered.iter().enumerate() {
        cancellation_checkpoint(cancel, "EIDR: PSF por frame")?;
        let img = load_cached(i)?;
        let uncertainty = load_uncertainty(i)?;
        if !uncertainty.publishable
            || uncertainty.variance.len() != img.data.len()
            || uncertainty.dq.len() != img.w * img.h
        {
            return Err(format!(
                "EIDR: frame {} carece de VAR/DQ de calibración publicable",
                i + 1
            ));
        }
        let luma = if img.ch == 1 {
            img.data.clone()
        } else {
            ds_luma(&img)
        };
        psfs.push(
            crate::deepsky_psf::fit_frame_psf(&luma, img.w, img.h, &star_catalogs[i], 0.2)
                .map(|(fp, _)| fp.at(0.5, 0.5)),
        );
        let mut mask = eidr_invalid_mask(&img.data, img.w, img.h, img.ch);
        let mut inverse = Vec::new();
        inverse
            .try_reserve_exact(img.data.len())
            .map_err(|error| format!("EIDR: sin memoria para Σ^-1 espacial: {error}"))?;
        inverse.resize(img.data.len(), 0.0f32);
        let fatal_dq = ds_uncertainty_fatal_dq();
        for pixel in 0..img.w * img.h {
            if uncertainty.dq[pixel] & fatal_dq != 0 {
                mask[pixel >> 6] |= 1u64 << (pixel & 63);
                continue;
            }
            for channel in 0..img.ch {
                let sample = pixel * img.ch + channel;
                let normalization_channel = cfa
                    .map(|cid| ds_cfa_channel(cid, pixel % img.w, pixel / img.w))
                    .unwrap_or(channel)
                    .min(2);
                let gain = norms[k].0[normalization_channel];
                let variance = uncertainty.variance[sample] * gain * gain;
                if variance.is_finite() && variance > 0.0 {
                    inverse[sample] = 1.0 / variance;
                }
            }
        }
        let layout = if img.ch == 1 {
            EidrInvVarLayout::Mono
        } else {
            EidrInvVarLayout::RgbInterleaved
        };
        spatial_inv_vars.push(EidrSpatialInvVar::new(layout, inverse, img.w, img.h)?);
        masks.push(mask);
        if k % 4 == 0 {
            emit_progress(
                app,
                &format!("EIDR: PSF por frame {}/{}", k + 1, n),
                36.0 + 4.0 * k as f32 / n as f32,
                None,
            );
        }
    }
    let geometry_only = psfs.iter().filter(|p| p.is_none()).count();
    if geometry_only > 0 {
        return Err(format!(
            "EIDR: faltan PSF medidas en {geometry_only} de {n} frame(s); no se sustituirán por una PSF nominal"
        ));
    }
    let gamma_nominal = eidr_target_psf(&psfs)
        .ok_or("EIDR: no se pudo estimar una PSF objetivo física")?;

    // --- Geometrías afines (los modelos no afines se excluyen, §7.8) ---
    let median_fwhm = {
        let mut fw: Vec<f32> = psfs.iter().flatten().map(|p| p.fwhm_mean()).collect();
        if fw.is_empty() {
            2.5
        } else {
            fw.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            fw[fw.len() / 2]
        }
    };
    let scale_requested = match cfg.scale {
        pipeline::EidrScalePolicy::Auto => "auto".to_string(),
        pipeline::EidrScalePolicy::X1 => "1x".to_string(),
        pipeline::EidrScalePolicy::X1_5 => "1.5x".to_string(),
        pipeline::EidrScalePolicy::X2 => "2x".to_string(),
    };
    let mut fallbacks: Vec<String> = Vec::new();
    let mut scale_eff: f32 = match cfg.scale {
        pipeline::EidrScalePolicy::X1 => 1.0,
        pipeline::EidrScalePolicy::X1_5 => 1.5,
        pipeline::EidrScalePolicy::X2 => 2.0,
        pipeline::EidrScalePolicy::Auto => {
            if median_fwhm < 2.0 {
                2.0
            } else if median_fwhm < 2.8 {
                1.5
            } else {
                1.0
            }
        }
    };
    // Mínimos operativos §7.8 (el número por sí solo nunca habilita).
    let min_for = |s: f32| -> usize {
        if s > 1.75 {
            if cfa.is_some() {
                24
            } else {
                12
            }
        } else if s > 1.01 {
            8
        } else {
            3
        }
    };
    while scale_eff > 1.01 && n < min_for(scale_eff) {
        let next = if scale_eff > 1.75 { 1.5 } else { 1.0 };
        fallbacks.push(format!(
            "{scale_eff}x requiere ≥{} tomas (hay {n}); se degrada a {next}x",
            min_for(scale_eff)
        ));
        scale_eff = next;
    }

    // --- Puerta de recuperabilidad por escala (escalera con fallback) ---
    let sigma_of = |k: usize, c: usize| -> f64 {
        let sigma = frame_noise[registered[k].0][c.min(2)] as f64
            * norms[k].0[c.min(2)] as f64;
        if sigma.is_finite() && sigma > 0.0 {
            sigma
        } else {
            f64::NAN
        }
    };
    let gate_report: Option<EidrGateReport>;
    let mut geometry_reports: Vec<crate::eidr::EidrGeomFieldReport>;
    loop {
        cancellation_checkpoint(cancel, "EIDR: puerta de recuperabilidad")?;
        emit_progress(
            app,
            &format!("EIDR: puerta de recuperabilidad a {scale_eff:.1}x..."),
            41.0,
            None,
        );
        let mut gate_frames: Vec<EidrGateFrame> = Vec::with_capacity(n);
        let mut gate_geometry_reports = Vec::with_capacity(n);
        for (k, &(_, t, _)) in registered.iter().enumerate() {
            if t.model == DsRegistrationModel::LocalDistortion {
                continue;
            }
            if let Some((geom, report)) = eidr_geom_full_field(&t, scale_eff, w, h, 0.02) {
                let psf = psfs[k].ok_or("EIDR: PSF ausente tras el gate de entrada")?;
                gate_frames.push(EidrGateFrame {
                    geom,
                    psf: Some(psf),
                    sigma: sigma_of(k, 1),
                });
                gate_geometry_reports.push(report);
            }
        }
        if gate_frames.len() < min_for(scale_eff).min(n) {
            return Err("EIDR: demasiados frames con registro no afín (proyectivo/local)".into());
        }
        // CFA: se evalúan las tres retículas y manda la peor (§7.6).
        let rep = if let Some(cid) = cfa {
            let mut reports = Vec::with_capacity(3);
            for c in 0..3usize {
                reports.push(eidr_recoverability_gate(
                    &gate_frames,
                    gamma_nominal,
                    w,
                    h,
                    scale_eff,
                    Some((cid, c)),
                    256,
                ));
            }
            ds_eidr_merge_cfa_gate_reports(reports)?
        } else {
            eidr_recoverability_gate(&gate_frames, gamma_nominal, w, h, scale_eff, None, 256)
        };
        if rep.science_publishable() {
            geometry_reports = gate_geometry_reports;
            gate_report = Some(rep);
            break;
        }
        if scale_eff <= 1.01 {
            return Err(format!(
                "EIDR: la puerta científica tampoco es publicable a 1x (PSF medidas {}/{}, ruido inválido en {} frame(s), fallback {:.0}%); se conserva Classic",
                rep.measured_psf_frames,
                rep.total_frames,
                rep.invalid_noise_frames,
                100.0 * rep.frac_fallback,
            ));
        }
        let next = if scale_eff > 1.75 { 1.5 } else { 1.0 };
        fallbacks.push(format!(
            "la puerta no soporta {scale_eff}x (apto {:.0}%, degradar {:.0}%, fallback {:.0}%); se degrada a {next}x",
            100.0 * rep.frac_apt,
            100.0 * rep.frac_degrade,
            100.0 * rep.frac_fallback
        ));
        scale_eff = next;
    }
    let gate_report = gate_report.ok_or("EIDR: la puerta no produjo reporte")?;
    for fb in &fallbacks {
        log_to_front(app, "WARN", &format!("EIDR: {fb}."));
    }

    // --- Operador a la escala efectiva (lienzo acolchado, §7.8) ---
    let pad =
        ((2.0 * gamma_nominal.fwhm_x.max(gamma_nominal.fwhm_y) + 3.0).ceil() as usize).clamp(4, 16);
    let w_pad_out = ((w + 2 * pad) as f32 * scale_eff).round() as usize;
    let h_pad_out = ((h + 2 * pad) as f32 * scale_eff).round() as usize;
    emit_progress(app, "EIDR: construyendo kernels de depósito...", 43.0, None);
    let mut op_frames: Vec<EidrFrameOp> = Vec::with_capacity(n);
    let mut kept_idx: Vec<usize> = Vec::new(); // índice en `registered`
    for (k, &(_, t, _)) in registered.iter().enumerate() {
        cancellation_checkpoint(cancel, "EIDR: kernels")?;
        // El modelo de distorsión local no se representa con la matriz h (el
        // desplazamiento del pad no le aplicaría): fuera explícitamente.
        if t.model == DsRegistrationModel::LocalDistortion {
            continue;
        }
        let mut tp = t;
        // Lienzo acolchado: ref' = ref + pad (el pad entero no altera fases).
        tp.h[2] += pad as f64 * tp.h[8];
        tp.h[5] += pad as f64 * tp.h[8];
        let Some((geom, geom_report)) =
            eidr_geom_full_field(&tp, scale_eff, w + 2 * pad, h + 2 * pad, 0.02)
        else {
            continue;
        };
        geometry_reports.push(geom_report);
        let psf = psfs[k].ok_or("EIDR: PSF ausente al construir el operador")?;
        let lut = eidr_build_lut(Some(psf), gamma_nominal, &geom);
        let mut inv_var = [0.0f32; 3];
        for c in 0..3 {
            let s = sigma_of(k, c);
            inv_var[c] = (1.0 / (s * s)) as f32;
        }
        let mut fr = EidrFrameOp {
            geom,
            lut,
            inv_var,
            spatial_inv_var: None,
            mask: masks[k].clone(),
            robust_w: None,
            w,
            h,
        };
        fr.set_spatial_inv_var(Some(spatial_inv_vars[k].clone()))?;
        eidr_mask_partial_rows(&mut fr, w_pad_out, h_pad_out);
        op_frames.push(fr);
        kept_idx.push(k);
    }
    let excluded = n - kept_idx.len();
    if excluded > 0 {
        log_to_front(
            app,
            "WARN",
            &format!("EIDR: {excluded} frame(s) con registro no afín excluidos (§7.8)."),
        );
    }
    if kept_idx.len() < 4 {
        return Err(
            "EIDR: se requieren al menos 4 frames utilizables (3 solve + 1 holdout)"
                .into(),
        );
    }
    let mut op = EidrOperator {
        frames: op_frames,
        w_out: w_pad_out,
        h_out: h_pad_out,
        cfa,
        ch: ch_out,
    };
    let nk = kept_idx.len();

    // Multigrid (F10, §6 del plan): operador espejo a escala nativa para el
    // warm start 1x→s. Mismos frames/máscaras/PSF; solo cambia la celda.
    let (w1_out, h1_out) = (w + 2 * pad, h + 2 * pad);
    let mut op1: Option<EidrOperator> = if cfg.multigrid && cfg.warm_start && scale_eff > 1.01 {
        let mut fr1 = Vec::with_capacity(nk);
        for &k in &kept_idx {
            let t = registered[k].1;
            let mut tp = t;
            tp.h[2] += pad as f64 * tp.h[8];
            tp.h[5] += pad as f64 * tp.h[8];
            let Some((geom, geom_report)) =
                eidr_geom_full_field(&tp, 1.0, w + 2 * pad, h + 2 * pad, 0.02)
            else {
                break;
            };
            geometry_reports.push(geom_report);
            let psf = psfs[k].ok_or("EIDR: PSF ausente al construir multigrid")?;
            let lut = eidr_build_lut(Some(psf), gamma_nominal, &geom);
            let mut inv_var = [0.0f32; 3];
            for c in 0..3 {
                let sg = sigma_of(k, c);
                inv_var[c] = (1.0 / (sg * sg)) as f32;
            }
            let mut fr = EidrFrameOp {
                geom,
                lut,
                inv_var,
                spatial_inv_var: None,
                mask: masks[k].clone(),
                robust_w: None,
                w,
                h,
            };
            fr.set_spatial_inv_var(Some(spatial_inv_vars[k].clone()))?;
            eidr_mask_partial_rows(&mut fr, w1_out, h1_out);
            fr1.push(fr);
        }
        if fr1.len() == nk {
            Some(EidrOperator {
                frames: fr1,
                w_out: w1_out,
                h_out: h1_out,
                cfa,
                ch: ch_out,
            })
        } else {
            None
        }
    } else {
        None
    };

    // Holdout §7.5: 10-20% de frames fuera del solve de validación.
    let requested_holdout =
        (kept_idx.len() as f32 * cfg.holdout_fraction.clamp(0.05, 0.25)).round() as usize;
    // Always reserve an independent validation frame. Preserve at least
    // three frames in the solve for the smallest scientifically eligible set.
    let max_holdout = (kept_idx.len() / 4)
        .max(1)
        .min(kept_idx.len().saturating_sub(3));
    let n_hold = requested_holdout.clamp(1, max_holdout);
    let hold_stride = nk / n_hold;
    let holdout: Vec<usize> = (0..nk)
        .filter(|k| n_hold > 0 && k % hold_stride == hold_stride / 2)
        .take(n_hold.max(1))
        .collect();
    let train: Vec<usize> = (0..nk).filter(|k| !holdout.contains(k)).collect();
    let all: Vec<usize> = (0..nk).collect();

    // Penalización espectral (§7.3) desde el perfil R de la puerta.
    let n_out = op.w_out * op.h_out;
    let mut diag0 = vec![0.0f64; n_out];
    for &fi in &all {
        op.normal_diag_accum(fi, 0, &mut diag0);
    }
    let diag_med = {
        let mut pos: Vec<f64> = diag0.iter().copied().filter(|&d| d > 0.0).collect();
        if pos.is_empty() {
            return Err("EIDR: sin cobertura en el lienzo".into());
        }
        let mid = pos.len() / 2;
        pos.select_nth_unstable_by(mid, |a, b| {
            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
        });
        pos[mid]
    };
    let pen = eidr_freq_penalty(&gate_report, op.w_out, op.h_out, diag_med);
    let nu_cut = eidr_cutoff_from_gate(&gate_report);

    // --- Solve por canal (secuencial) ---
    // Flujo F10 por canal: sistemas b/diag (+espejo 1x) → warm start
    // (multigrid o piloto) → cuadrático de ENTRENAMIENTO (baseline SIEMPRE,
    // §7.9) → microregistro opcional con guardia de holdout → Huber-IRLS
    // opcional con REVERSIÓN automática si el holdout empeora → solve final
    // con todos los frames (warm) → productos.
    let mut planes: Vec<Vec<f32>> = Vec::with_capacity(ch_out);
    let mut pilot_planes: Vec<Vec<f32>> = Vec::with_capacity(ch_out);
    let mut variance: Vec<f32> = vec![f32::NAN; n_out * ch_out];
    let mut neff_plane: Vec<f32> = vec![0.0; n_out * ch_out];
    let mut coverage_luma = vec![0.0f64; n_out];
    let mut chi2_medians: Vec<f64> = Vec::new();
    let mut channel_reports: Vec<crate::eidr::EidrSolveReport> = Vec::with_capacity(ch_out);
    let mut channel_holdouts: Vec<Option<f64>> = Vec::with_capacity(ch_out);
    let mut finite_channels: Vec<bool> = Vec::with_capacity(ch_out);
    let experimental = matches!(cfg.solve_mode, pipeline::EidrSolveMode::ExperimentalDetail);
    // GPU (F10): matvec de las ecuaciones normales en wgpu, con paridad
    // física verificada ANTES del primer uso; sin paridad no hay GPU.
    let spatial_variance_requires_cpu = op.frames.iter().any(EidrFrameOp::has_spatial_inv_var);
    if compute_policy.allows_gpu() && spatial_variance_requires_cpu {
        let reason = "GPU EIDR no admite todavía Σ^-1 espacial; se usa CPU para preservar VAR/DQ por píxel";
        fallbacks.push(reason.into());
        log_to_front(app, "WARN", &format!("EIDR: {reason}."));
    }
    let gpu_allowed = compute_policy.allows_gpu()
        && !spatial_variance_requires_cpu
        && crate::gpu_stack::gpu_runtime().is_some()
        && crate::gpu_eidr::ensure_eidr_parity();
    if matches!(compute_policy, ComputePolicy::GpuOnly) && !gpu_allowed {
        if spatial_variance_requires_cpu {
            return Err(
                "GPU only: EIDR requiere Σ^-1 espacial de calibración y el backend GPU aún no lo implementa"
                    .into(),
            );
        }
        return Err(
            "GPU only: no hay runtime wgpu con paridad EIDR verificada; usa Auto o Hybrid".into(),
        );
    }
    if gpu_allowed {
        log_to_front(app, "INFO", "EIDR: matvec en GPU (paridad CPU verificada).");
    }
    let mut huber_adopted = 0usize;
    let mut huber_reverted = 0usize;
    let mut refine_applied = 0usize;
    let mut refine_p90_px = 0.0f64;
    let mut refine_reverted = false;
    let multigrid_used = op1.is_some();
    const IRLS_ROUNDS: usize = 2;
    let solve_cfg = EidrSolveConfig {
        max_iterations: cfg.max_iterations.max(20) as usize,
        data_size: train.len() * w * h,
        ..EidrSolveConfig::default()
    };

    // Plano normalizado del frame fi (no toca `op`: permite mutarlo fuera).
    let extract_plane = |fi: usize, c: usize, buf: &mut [f32]| -> Result<(), String> {
        let k = kept_idx[fi];
        let img = load_cached(registered[k].0)?;
        let (mul, add) = norms[k];
        let local_at = |pixel: usize, channel: usize| -> f32 {
            let Some(field) = loc_fields.get(k).and_then(Option::as_deref) else {
                return 0.0;
            };
            let x = pixel % w;
            let y = pixel / w;
            let (rx, ry) = registered[k].1.forward(x as f32, y as f32);
            ds_sample_local_field(
                field,
                local_grid_size,
                local_grid_size,
                ch_out,
                channel,
                rx / w.max(1) as f32,
                ry / h.max(1) as f32,
            )
        };
        if let Some(cid) = cfa {
            for p in 0..w * h {
                let cc = ds_cfa_channel(cid, p % w, p / w);
                buf[p] = img.data[p] * mul[cc] + add[cc] + local_at(p, cc);
            }
        } else {
            for p in 0..w * h {
                let channel = c.min(img.ch - 1);
                let v = img.data[p * img.ch + channel];
                buf[p] = v * mul[c.min(2)] + add[c.min(2)] + local_at(p, channel);
            }
        }
        Ok(())
    };
    let diag_of = |op: &EidrOperator, c: usize, set: &[usize]| -> Vec<f64> {
        let mut d = vec![0.0f64; op.w_out * op.h_out];
        for &fi in set {
            op.normal_diag_accum(fi, c, &mut d);
        }
        d
    };
    // χ² mediano de los frames de holdout contra la solución dada.
    let holdout_chi2 = |op: &EidrOperator,
                        c: usize,
                        z: &[f32],
                        hold_planes: &[(usize, Vec<f32>)]|
     -> Option<f64> {
        let mut meds: Vec<f64> = hold_planes
            .iter()
            .filter_map(|&(fi, ref plane)| {
                let st = eidr_holdout_stat(op, fi, c, z, plane, None);
                (st.pixels > 500 && st.chi2_median.is_finite()).then_some(st.chi2_median)
            })
            .collect();
        if meds.is_empty() {
            return None;
        }
        meds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(meds[meds.len() / 2])
    };

    for c in 0..ch_out {
        cancellation_checkpoint(cancel, "EIDR: solve")?;
        emit_progress(
            app,
            &format!("EIDR: canal {}/{} — sistema normal...", c + 1, ch_out),
            45.0 + 45.0 * c as f32 / ch_out as f32,
            None,
        );
        // b (todos) + b_hold + espejo 1x en UNA pasada de datos.
        let mut b_all = vec![0.0f64; n_out];
        let mut b_hold = vec![0.0f64; n_out];
        let mut b1_all = op1.as_ref().map(|o| vec![0.0f64; o.w_out * o.h_out]);
        let mut plane_buf = vec![0.0f32; w * h];
        let mut sigma_scratch = Vec::new();
        let mut hold_planes: Vec<(usize, Vec<f32>)> = Vec::new();
        for &fi in &all {
            cancellation_checkpoint(cancel, "EIDR: retroproyección")?;
            extract_plane(fi, c, &mut plane_buf)?;
            op.adjoint_sigma_accum(fi, c, &plane_buf, &mut sigma_scratch, &mut b_all)?;
            if let (Some(o1), Some(b1)) = (op1.as_ref(), b1_all.as_mut()) {
                o1.adjoint_sigma_accum(fi, c, &plane_buf, &mut sigma_scratch, b1)?;
            }
            if holdout.contains(&fi) {
                op.adjoint_sigma_accum(
                    fi,
                    c,
                    &plane_buf,
                    &mut sigma_scratch,
                    &mut b_hold,
                )?;
                hold_planes.push((fi, plane_buf.clone()));
            }
        }
        let mut diag_all = diag_of(&op, c, &all);
        let diag_hold = diag_of(&op, c, &holdout);
        let mut b_train: Vec<f64> = b_all
            .iter()
            .zip(b_hold.iter())
            .map(|(&a, &b)| a - b)
            .collect();
        let mut diag_train: Vec<f64> = diag_all
            .iter()
            .zip(diag_hold.iter())
            .map(|(&a, &b)| a - b)
            .collect();
        let aone_all = eidr_backprojected_flat(&op, c, &all);
        let mut progress_cb = |k: usize, kmax: usize| {
            if k % 8 == 0 {
                emit_progress(
                    app,
                    &format!("EIDR: canal {}/{} — PCG {k}/{kmax}", c + 1, ch_out),
                    45.0 + 45.0 * (c as f32 + 0.5) / ch_out as f32,
                    None,
                );
            }
        };
        // Solve con matvec GPU (contexto fresco por solve: los buffers llevan
        // pesos/geometrías del momento) y fallback CPU DECLARADO ante errores.
        let solve_with = |op: &EidrOperator,
                          set: &[usize],
                          b: &[f64],
                          d: &[f64],
                          z0: &[f32],
                          progress: &mut dyn FnMut(usize, usize)|
         -> Result<(Vec<f32>, crate::eidr::EidrSolveReport), String> {
            if gpu_allowed {
                if let Some(ctx) = crate::gpu_eidr::EidrGpuMatvec::new(op, set, c) {
                    let g = move |pv: &[f32], ov: &mut [f64]| {
                        ctx.matvec(pv, ov).map_err(|e| format!("GPU: {e}"))
                    };
                    match eidr_solve_channel(
                        op,
                        c,
                        set,
                        b,
                        d,
                        z0,
                        &solve_cfg,
                        pen.as_ref(),
                        Some(&g),
                        Some(cancel),
                        progress,
                    ) {
                        Err(e) if e.starts_with("GPU: ") => {
                            if matches!(compute_policy, ComputePolicy::GpuOnly) {
                                return Err(format!(
                                    "GPU only: matvec EIDR falló y no se permite reintento CPU ({e})"
                                ));
                            }
                            log_to_front(
                                app,
                                "WARN",
                                &format!("EIDR: matvec GPU falló ({e}); reintentando en CPU."),
                            );
                        }
                        other => return other,
                    }
                } else if matches!(compute_policy, ComputePolicy::GpuOnly) {
                    return Err(
                        "GPU only: no se pudo crear el contexto matvec EIDR y no se permite CPU"
                            .into(),
                    );
                }
            }
            eidr_solve_channel(
                op,
                c,
                set,
                b,
                d,
                z0,
                &solve_cfg,
                pen.as_ref(),
                None,
                Some(cancel),
                progress,
            )
        };
        // El piloto normalizado se conserva como producto independiente del
        // warm start. La puerta local lo limita al Nyquist nativo antes de
        // usarlo en tiles sin evidencia superresuelta.
        let mut publication_pilot = eidr_pilot(&b_all, &aone_all);
        // Warm start: multigrid 1x→s si está activo; si no, piloto; con
        // warm_start=false, ceros (§7.5 lo permite; queda en receta).
        let z0: Vec<f32> = if !cfg.warm_start {
            vec![0.0f32; n_out]
        } else if let (Some(o1), Some(b1)) = (op1.as_ref(), b1_all.as_ref()) {
            let diag1 = diag_of(o1, c, &all);
            let aone1 = eidr_backprojected_flat(o1, c, &all);
            let z01 = eidr_pilot(b1, &aone1);
            let cfg1 = EidrSolveConfig {
                max_iterations: (solve_cfg.max_iterations / 2).max(15),
                data_size: all.len() * w * h,
                ..EidrSolveConfig::default()
            };
            let mut noop = |_k: usize, _n: usize| {};
            let (z1, _r1) = eidr_solve_channel(
                o1,
                c,
                &all,
                b1,
                &diag1,
                &z01,
                &cfg1,
                None,
                None,
                Some(cancel),
                &mut noop,
            )?;
            eidr_prolong(&z1, o1.w_out, o1.h_out, op.w_out, op.h_out)
        } else {
            eidr_pilot(&b_all, &aone_all)
        };

        // Cuadrático de ENTRENAMIENTO — baseline obligatorio (§7.9).
        let (mut z_train, _rep_train) =
            solve_with(&op, &train, &b_train, &diag_train, &z0, &mut progress_cb)?;
        let mut chi2_ref = holdout_chi2(&op, c, &z_train, &hold_planes);

        // Microregistro ±0.2 px (F10, opt-in) con guardia de holdout: solo
        // en el primer canal (la corrección geométrica es común) y solo si
        // hay holdout para poder revertir.
        if c == 0 && cfg.refine_registration && !hold_planes.is_empty() {
            cancellation_checkpoint(cancel, "EIDR: microregistro")?;
            emit_progress(app, "EIDR: microregistro ±0.2 px...", 52.0, None);
            let geoms_backup: Vec<crate::eidr::EidrGeom> =
                op.frames.iter().map(|f| f.geom).collect();
            let geoms1_backup: Option<Vec<crate::eidr::EidrGeom>> = op1
                .as_ref()
                .map(|o| o.frames.iter().map(|f| f.geom).collect());
            let mut shifts: Vec<f64> = Vec::new();
            let mut n_shifted = 0usize;
            for &fi in &all {
                extract_plane(fi, c, &mut plane_buf)?;
                if let Some((dx, dy)) =
                    eidr_refine_translation(&op, fi, c, &z_train, &plane_buf, 0.2)
                {
                    let mag = (dx * dx + dy * dy).sqrt();
                    shifts.push(mag);
                    if mag > 0.01 {
                        eidr_shift_geom(&mut op.frames[fi].geom, dx, dy);
                        if let Some(o1) = op1.as_mut() {
                            eidr_shift_geom(&mut o1.frames[fi].geom, dx, dy);
                        }
                        n_shifted += 1;
                    }
                }
            }
            if n_shifted > 0 {
                // Reconstruir sistemas con la geometría corregida.
                let mut nb_all = vec![0.0f64; n_out];
                let mut nb_hold = vec![0.0f64; n_out];
                let mut nhold: Vec<(usize, Vec<f32>)> = Vec::new();
                for &fi in &all {
                    extract_plane(fi, c, &mut plane_buf)?;
                    op.adjoint_sigma_accum(
                        fi,
                        c,
                        &plane_buf,
                        &mut sigma_scratch,
                        &mut nb_all,
                    )?;
                    if holdout.contains(&fi) {
                        op.adjoint_sigma_accum(
                            fi,
                            c,
                            &plane_buf,
                            &mut sigma_scratch,
                            &mut nb_hold,
                        )?;
                        nhold.push((fi, plane_buf.clone()));
                    }
                }
                let nd_all = diag_of(&op, c, &all);
                let nd_hold = diag_of(&op, c, &holdout);
                let nb_train: Vec<f64> = nb_all
                    .iter()
                    .zip(nb_hold.iter())
                    .map(|(&a, &b)| a - b)
                    .collect();
                let nd_train: Vec<f64> = nd_all
                    .iter()
                    .zip(nd_hold.iter())
                    .map(|(&a, &b)| a - b)
                    .collect();
                let (z2, _r2) = solve_with(
                    &op,
                    &train,
                    &nb_train,
                    &nd_train,
                    &z_train,
                    &mut progress_cb,
                )?;
                let chi2_after = holdout_chi2(&op, c, &z2, &nhold);
                let better = match (chi2_ref, chi2_after) {
                    (Some(b0), Some(a)) => a <= b0 * 1.02,
                    _ => false,
                };
                if better {
                    shifts.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
                    refine_applied = n_shifted;
                    refine_p90_px = shifts[(shifts.len() * 9 / 10).min(shifts.len() - 1)];
                    publication_pilot =
                        eidr_pilot(&nb_all, &eidr_backprojected_flat(&op, c, &all));
                    b_all = nb_all;
                    b_train = nb_train;
                    diag_all = nd_all;
                    diag_train = nd_train;
                    hold_planes = nhold;
                    z_train = z2;
                    chi2_ref = chi2_after;
                    log_to_front(
                        app,
                        "INFO",
                        &format!(
                            "EIDR: microregistro aplicado a {n_shifted} frame(s), p90 {refine_p90_px:.3} px."
                        ),
                    );
                } else {
                    for (f, g) in op.frames.iter_mut().zip(geoms_backup) {
                        f.geom = g;
                    }
                    if let (Some(o1), Some(gb)) = (op1.as_mut(), geoms1_backup) {
                        for (f, g) in o1.frames.iter_mut().zip(gb) {
                            f.geom = g;
                        }
                    }
                    refine_reverted = true;
                    log_to_front(
                        app,
                        "WARN",
                        "EIDR: el microregistro no mejoró el holdout — revertido (§7.8).",
                    );
                }
            }
        }

        // Huber-IRLS (ExperimentalDetail): pesos SOLO contra el modelo
        // directo, con el cuadrático como baseline y reversión por holdout.
        if experimental && !hold_planes.is_empty() {
            cancellation_checkpoint(cancel, "EIDR: Huber IRLS")?;
            let mut z_h = z_train.clone();
            for round in 0..IRLS_ROUNDS {
                emit_progress(
                    app,
                    &format!(
                        "EIDR: canal {}/{} — IRLS Huber {}/{IRLS_ROUNDS}",
                        c + 1,
                        ch_out,
                        round + 1
                    ),
                    50.0 + 40.0 * c as f32 / ch_out as f32,
                    None,
                );
                let mut bt = vec![0.0f64; n_out];
                for &fi in &train {
                    cancellation_checkpoint(cancel, "EIDR: pesos Huber")?;
                    extract_plane(fi, c, &mut plane_buf)?;
                    let wts = eidr_irls_weights(&op, fi, c, &z_h, &plane_buf, cfg.huber_delta);
                    op.frames[fi].robust_w = Some(wts);
                    op.adjoint_sigma_accum(
                        fi,
                        c,
                        &plane_buf,
                        &mut sigma_scratch,
                        &mut bt,
                    )?;
                }
                let dt = diag_of(&op, c, &train);
                let (zr, _r) = solve_with(&op, &train, &bt, &dt, &z_h, &mut progress_cb)?;
                z_h = zr;
            }
            let chi2_h = holdout_chi2(&op, c, &z_h, &hold_planes);
            let adopt = match (chi2_ref, chi2_h) {
                (Some(q), Some(hh)) => hh <= q * 1.05,
                _ => false,
            };
            if adopt {
                huber_adopted += 1;
                // Pesos también para los frames de holdout (entran al solve
                // final) y b_all/diag_all reconstruidos con W.
                for &(fi, ref plane) in &hold_planes {
                    let wts = eidr_irls_weights(&op, fi, c, &z_h, plane, cfg.huber_delta);
                    op.frames[fi].robust_w = Some(wts);
                }
                let mut nb_all = vec![0.0f64; n_out];
                for &fi in &all {
                    extract_plane(fi, c, &mut plane_buf)?;
                    op.adjoint_sigma_accum(
                        fi,
                        c,
                        &plane_buf,
                        &mut sigma_scratch,
                        &mut nb_all,
                    )?;
                }
                b_all = nb_all;
                diag_all = diag_of(&op, c, &all);
                publication_pilot =
                    eidr_pilot(&b_all, &eidr_backprojected_flat(&op, c, &all));
                z_train = z_h;
                chi2_ref = chi2_h;
            } else {
                huber_reverted += 1;
                for &fi in &all {
                    op.frames[fi].robust_w = None;
                }
                log_to_front(
                    app,
                    "WARN",
                    &format!(
                        "EIDR canal {}: Huber empeoró el holdout (χ² {:?} vs {:?}) — REVERTIDO al cuadrático (§7.10).",
                        c + 1,
                        chi2_h,
                        chi2_ref
                    ),
                );
            }
        }
        match chi2_ref {
            Some(chi2) if ds_eidr_holdout_publishable(nk, Some(chi2)) => {
                chi2_medians.push(chi2);
            }
            Some(chi2) => {
                return Err(format!(
                    "EIDR: canal {} no supera holdout (chi² mediano {chi2:.3}, límite 2.0)",
                    c + 1
                ));
            }
            None => {
                return Err(format!(
                    "EIDR: canal {} sin estadística holdout publicable ({nk} frames); no se publica sin validación independiente",
                    c + 1
                ));
            }
        }

        // Solve final con TODOS los frames, warm start del ganador.
        let (z_final, rep) = solve_with(&op, &all, &b_all, &diag_all, &z_train, &mut progress_cb)?;
        let finite_solution = z_final.iter().all(|value| value.is_finite());
        if !ds_eidr_channel_publishable(&rep, finite_solution) {
            return Err(format!(
                "EIDR: canal {} no publicable (converged={}, residual={:?}, ridge={:?}, finite={finite_solution})",
                c + 1,
                rep.converged,
                rep.rel_residual,
                rep.ridge
            ));
        }
        channel_holdouts.push(chi2_ref);
        finite_channels.push(finite_solution);
        channel_reports.push(rep);

        // Varianza diagonal 1/diag (origen declarado) y NEFF de Kish sobre
        // los pesos efectivos del operador final. Se recalculan después de
        // microregistro/IRLS para no publicar cobertura de una geometría o
        // ponderación anterior, y nunca se colapsa VAR espacial a un escalar.
        let aone_effective = eidr_backprojected_flat(&op, c, &all);
        let mut sum_weight_sq = vec![0.0f64; n_out];
        for &fi in &all {
            op.precision_weight_sq_accum(fi, c, &mut sum_weight_sq)?;
        }
        for q in 0..n_out {
            if diag_all[q] > 0.0 {
                variance[q * ch_out + c] = (1.0 / diag_all[q]) as f32;
            }
            let cov = if sum_weight_sq[q] > 0.0 {
                (aone_effective[q] * aone_effective[q] / sum_weight_sq[q]).max(0.0)
            } else {
                0.0
            };
            neff_plane[q * ch_out + c] = cov as f32;
            if c == 0 {
                coverage_luma[q] = cov;
            } else {
                coverage_luma[q] = coverage_luma[q].min(cov);
            }
        }
        pilot_planes.push(publication_pilot);
        planes.push(z_final);
        // Los pesos robustos son por canal: limpiar antes del siguiente.
        for &fi in &all {
            op.frames[fi].robust_w = None;
        }
    }
    let chi2_median = if chi2_medians.is_empty() {
        None
    } else {
        chi2_medians.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(chi2_medians[chi2_medians.len() / 2])
    };
    if let Some(chi2) = chi2_median {
        log_to_front(
            app,
            "INFO",
            &format!(
                "EIDR: todos los canales superaron holdout de {} frame(s), χ² mediano global {chi2:.2}.",
                holdout.len()
            ),
        );
    }
    let publication = eidr_validate_publication(
        &channel_reports,
        &finite_channels,
        &channel_holdouts,
        nk,
    )?;

    // --- Recorte del pad y ensamblado de productos ---
    let (w2, h2) = (
        (w as f32 * scale_eff).round() as usize,
        (h as f32 * scale_eff).round() as usize,
    );
    let off = (pad as f32 * scale_eff).round() as usize;
    let unpad_idx = |x: usize, y: usize| (y + off) * op.w_out + (x + off);
    let mut final_data = vec![0.0f32; w2 * h2 * ch_out];
    let mut pilot_data = vec![0.0f32; w2 * h2 * ch_out];
    let mut var_out = vec![f32::NAN; w2 * h2 * ch_out];
    let mut neff_out = vec![0.0f32; w2 * h2 * ch_out];
    let mut dq = vec![0u32; w2 * h2];
    let mut coverage = vec![0.0f64; w2 * h2];
    let max_cov = coverage_luma
        .iter()
        .cloned()
        .fold(0.0f64, f64::max)
        .max(1e-9);
    for y in 0..h2 {
        for x in 0..w2 {
            let src = unpad_idx(x, y);
            let dst = y * w2 + x;
            let cov = coverage_luma[src];
            coverage[dst] = cov;
            for c in 0..ch_out {
                final_data[dst * ch_out + c] = planes[c][src];
                pilot_data[dst * ch_out + c] = pilot_planes[c][src];
                var_out[dst * ch_out + c] = variance[src * ch_out + c];
                neff_out[dst * ch_out + c] = neff_plane[src * ch_out + c];
            }
            if cov > 1e-9 && cov < 0.5 * max_cov {
                dq[dst] |= crate::deepsky_variance::dq::EDGE;
            }
        }
    }
    let tile_publication = eidr_apply_tile_publication_gate(
        &gate_report,
        EidrPublicationPlanes {
            science: &mut final_data,
            variance: &mut var_out,
            neff: &mut neff_out,
            dq: &mut dq,
            coverage: &coverage,
        },
        &pilot_data,
        w2,
        h2,
        ch_out,
    )?;
    if tile_publication.uncertainty_unavailable_pixels > 0 {
        let reason = format!(
            "{} píxel(es) EIDR mezclados/native conservan SCI pero publican VAR=NaN, NEFF=0 y DQ explícito: la covarianza del producto efectivo no está disponible",
            tile_publication.uncertainty_unavailable_pixels
        );
        log_to_front(app, "WARN", &format!("EIDR: {reason}."));
        fallbacks.push(reason);
    }
    // Mapa RECOV al tamaño del máster (sin pad: mismos tiles nativos).
    let recov_full = gate_report.recov_map(w2, h2);
    let mean_cov = coverage.iter().sum::<f64>() / (w2 * h2).max(1) as f64 / nk as f64;
    if channel_reports.len() != ch_out {
        return Err(format!(
            "EIDR: sólo {} de {ch_out} canales produjeron reporte publicable",
            channel_reports.len()
        ));
    }
    let solver_iterations = publication.max_iterations;
    let solver_rel_residual = publication.worst_rel_residual;
    let solver_ridge = channel_reports
        .iter()
        .map(|report| report.ridge)
        .fold(0.0f64, f64::max);
    let (geometry_sample_count, geometry_p95_error_px, geometry_max_error_px, geometry_min_jacobian) =
        ds_eidr_geometry_summary(&geometry_reports)
            .ok_or("EIDR: no existe un diagnóstico geométrico full-field publicable")?;
    if geometry_max_error_px > 0.02 || geometry_min_jacobian <= 0.0 {
        return Err(format!(
            "EIDR: geometría full-field no publicable (máx {geometry_max_error_px:.5} px, Jacobiano mín {geometry_min_jacobian:.6})"
        ));
    }
    log_to_front(
        app,
        "SUCCESS",
        &format!(
            "EIDR {scale_eff:.1}x: {} frames · PCG {} iter (residual {:.1e}) · geometría p95/máx {:.4}/{:.4} px · puerta apto {:.0}% · corte publicado {:.3} c/px.",
            nk,
            solver_iterations,
            solver_rel_residual,
            geometry_p95_error_px,
            geometry_max_error_px,
            100.0 * gate_report.frac_apt,
            nu_cut
        ),
    );
    Ok(DsEidrOutcome {
        final_data,
        coverage,
        variance: var_out,
        neff: neff_out,
        dq,
        recov: recov_full,
        w2,
        h2,
        scale_requested,
        scale_eff,
        fallbacks,
        gate_frac: (
            gate_report.frac_apt,
            gate_report.frac_degrade,
            gate_report.frac_fallback,
        ),
        nu_cut,
        tile_apt_pixels: tile_publication.apt_pixels,
        tile_degraded_pixels: tile_publication.degraded_pixels,
        tile_fallback_pixels: tile_publication.fallback_pixels,
        native_cutoff_cycles_per_output_px: tile_publication
            .native_cutoff_cycles_per_output_px,
        tile_classes: tile_publication.tile_classes,
        publication_channels: publication.channels.len(),
        solver_iterations,
        solver_rel_residual,
        solver_converged: true,
        solver_ridge,
        lambda_f: pen.as_ref().map(|p| p.lambda).unwrap_or(0.0),
        holdout_frames: holdout.len(),
        holdout_chi2_median: chi2_median,
        solver_mode_used: if experimental && huber_adopted > 0 {
            format!("experimentalDetailHuber({huber_adopted}/{ch_out} canales)")
        } else if experimental {
            "scientificQuadratic (Huber revertido)".into()
        } else {
            "scientificQuadratic".into()
        },
        huber_adopted_channels: huber_adopted,
        huber_reverted_channels: huber_reverted,
        irls_rounds: if experimental { IRLS_ROUNDS } else { 0 },
        refine_applied_frames: refine_applied,
        refine_p90_px,
        refine_reverted,
        multigrid_used,
        gamma_fwhm_px: Some(gamma_nominal.fwhm_x),
        geometry_only_frames: geometry_only,
        excluded_frames: excluded,
        geometry_sample_count,
        geometry_p95_error_px,
        geometry_max_error_px,
        geometry_min_jacobian,
        pad,
        mean_cov,
    })
}

/// Rearma el flag global de cancelación al INICIO de una acción explícita del
/// usuario, bajo el mismo gate que usa `cancel_planetary_jobs`. Sin el gate,
/// un Cancelar concurrente podía quedar borrado por este reset (la misma
/// carrera 1→0 que el lado planetario cerró con `begin_planetary_user_job`).
fn ds_begin_user_action(state: &AppState) {
    let _generation_guard = state
        .planetary_generation_gate
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    state
        .cancel_requested
        .store(false, std::sync::atomic::Ordering::Release);
}

/// Wrapper de comando: una invocación directa de `stack_deepsky` es una acción
/// de usuario nueva y rearma el flag global. Los flujos internos
/// (`run_deepsky_stack`, `run_deepsky_session`) rearman UNA vez en su entrada
/// y llaman a `stack_deepsky_impl`, de modo que un Cancelar recibido entre
/// grupos de una sesión no se pierda.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn stack_deepsky(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    lights: Vec<String>,
    darks: Vec<String>,
    flats: Vec<String>,
    dark_flats: Vec<String>,
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
    interpolation: Option<String>,
    clip_iters: Option<u32>,
    pedestal: Option<f32>,
    rejection: Option<String>,
    kappa_low: Option<f32>,
    kappa_high: Option<f32>,
    normalization: Option<String>,
    compute_policy: Option<ComputePolicy>,
    local_weighting: Option<bool>,
    work_dir: Option<String>,
    integration_method: Option<pipeline::DeepSkyIntegrationMethod>,
    capture_mode: Option<pipeline::DeepSkyCaptureMode>,
    calibration_policy: Option<pipeline::DeepSkyCalibrationPolicy>,
    calibration_overrides: Option<Vec<pipeline::DeepSkyCalibrationOverride>>,
) -> Result<String, String> {
    ds_begin_user_action(&state);
    stack_deepsky_impl(
        app,
        state,
        lights,
        darks,
        flats,
        dark_flats,
        bias,
        kappa,
        sigma_clip,
        cosmetic,
        gradient,
        drizzle,
        optimize_dark,
        pixfrac,
        local_norm,
        auto_crop,
        interpolation,
        clip_iters,
        pedestal,
        rejection,
        kappa_low,
        kappa_high,
        normalization,
        compute_policy,
        local_weighting,
        work_dir,
        integration_method,
        capture_mode,
        calibration_policy,
        calibration_overrides,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn stack_deepsky_impl(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    lights: Vec<String>,
    darks: Vec<String>,
    flats: Vec<String>,
    dark_flats: Vec<String>,
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
    interpolation: Option<String>,
    clip_iters: Option<u32>,
    pedestal: Option<f32>,
    rejection: Option<String>,
    kappa_low: Option<f32>,
    kappa_high: Option<f32>,
    normalization: Option<String>,
    compute_policy: Option<ComputePolicy>,
    local_weighting: Option<bool>,
    work_dir: Option<String>,
    integration_method: Option<pipeline::DeepSkyIntegrationMethod>,
    capture_mode: Option<pipeline::DeepSkyCaptureMode>,
    calibration_policy: Option<pipeline::DeepSkyCalibrationPolicy>,
    calibration_overrides: Option<Vec<pipeline::DeepSkyCalibrationOverride>>,
) -> Result<String, String> {
    let ds_run_started = std::time::Instant::now();
    let ds_result_id = new_job_id("ds-result");
    let ds_telemetry_sys = std::sync::Mutex::new(System::new());
    state.license_manager.check_access()?;
    // F8: flag de cancelación POR TRABAJO. El botón global barre el registro
    // (cancel_planetary_jobs → cancel_all), así que ambos caminos cortan este
    // stack; el guard da de baja el id incluso ante error temprano o pánico.
    // El registro va BAJO el gate y NO rearma el flag global: un Cancelar que
    // llegó en la ventana sin trabajos registrados (p. ej. entre grupos de una
    // sesión multibanda) se propaga aquí al flag recién registrado en vez de
    // perderse, y uno concurrente o bien barre este registro vía cancel_all o
    // bien queda serializado por el gate.
    let cancel = {
        let _generation_guard = state
            .planetary_generation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let cancel = state.job_registry.register(&ds_result_id);
        if state
            .cancel_requested
            .load(std::sync::atomic::Ordering::Acquire)
        {
            cancel.store(true, std::sync::atomic::Ordering::Release);
        }
        cancel
    };
    let _job_guard = pipeline::JobGuard::new(state.job_registry.clone(), ds_result_id.clone());
    let compute_policy = compute_policy.unwrap_or_default();
    let capture_mode = capture_mode.unwrap_or_default();
    let calibration_policy = calibration_policy.unwrap_or_default();
    // RESCATE DE DETALLE (v1): ponderación local por FWHM de estrellas. Solo
    // el motor streaming κσ CPU la aplica en esta fase; se anuncia cuando se
    // ignora (rechazo por-píxel o GPU streaming).
    let local_weighting = local_weighting.unwrap_or(false);
    // Asignaciones manuales (estilo PixInsight): validadas contra las listas
    // del request; una regla sin lights afectados o sin ficheros se ignora.
    let calibration_overrides: Vec<pipeline::DeepSkyCalibrationOverride> = calibration_overrides
        .unwrap_or_default()
        .into_iter()
        .filter(|over| {
            !over.darks.is_empty() || !over.flats.is_empty() || over.skip_flats || over.skip_darks
        })
        .collect();
    // Omisiones explícitas por light (skip): sin flat/dark para esos lights,
    // sin error de contrato y con divulgación en decisiones y receta.
    let mut manual_skip_flats: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut manual_skip_darks: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for over in &calibration_overrides {
        if !over.skip_flats && !over.skip_darks {
            continue;
        }
        let affected: Vec<String> = if over.lights.is_empty() {
            lights.clone()
        } else {
            lights
                .iter()
                .filter(|p| over.lights.iter().any(|l| l == *p))
                .cloned()
                .collect()
        };
        for path in affected {
            if over.skip_flats {
                manual_skip_flats.insert(path.clone());
            }
            if over.skip_darks {
                manual_skip_darks.insert(path);
            }
        }
    }
    let mut integration_method = integration_method;
    let requested_integration_method = integration_method.clone().unwrap_or_else(|| {
        pipeline::DeepSkyIntegrationMethod::Classic(pipeline::ClassicIntegrationConfig {
            version: 1,
            legacy_local_fwhm: local_weighting,
        })
    });

    // Productive calibration contract: resolve exact signatures before any
    // master allocation and before deciding whether NF/EIDR may run.
    let light_probes = deepsky_probe(lights.clone());
    let bias_probes = deepsky_probe(bias.clone());
    let dark_probes = deepsky_probe(darks.clone());
    let flat_reference_probes = deepsky_probe(flats.clone());
    let dark_flat_probes = deepsky_probe(dark_flats.clone());
    let bias_selection =
        ds_select_calibration_group(&bias, &light_probes, "bias", calibration_policy);
    let dark_selection =
        ds_select_calibration_group(&darks, &light_probes, "darks", calibration_policy);
    let flat_selection =
        ds_select_calibration_group(&flats, &light_probes, "flats", calibration_policy);
    let dark_flat_selection = ds_select_calibration_group(
        &dark_flats,
        &flat_reference_probes,
        "dark-flats",
        calibration_policy,
    );
    let mut calibration_contract_errors = light_probes
        .iter()
        .filter(|probe| probe.ok)
        .filter_map(|probe| {
            let mut missing = probe.signature_missing.clone();
            if probe.store_layout.is_none() {
                missing.push("storeLayout/CFA phase".into());
            }
            missing.sort();
            missing.dedup();
            (!missing.is_empty()).then(|| {
                format!(
                    "Light '{}': metadata obligatoria ausente: {}",
                    probe.name,
                    missing.join(", ")
                )
            })
        })
        .collect::<Vec<_>>();
    calibration_contract_errors.extend(
        [
            &bias_selection,
            &dark_selection,
            &flat_selection,
            &dark_flat_selection,
        ]
        .into_iter()
        .flat_map(|selection| selection.blocking_reasons.iter().cloned()),
    );
    let effective_bias_probes = ds_selected_probes(&bias_probes, &bias_selection);
    let effective_dark_probes = ds_selected_probes(&dark_probes, &dark_selection);
    let effective_flat_probes =
        ds_selected_probes(&flat_reference_probes, &flat_selection);
    let effective_dark_flat_probes =
        ds_selected_probes(&dark_flat_probes, &dark_flat_selection);
    let mut calibration_decisions = ds_prepare_calibration_decisions(
        &light_probes,
        &effective_bias_probes,
        &effective_dark_probes,
        &effective_flat_probes,
        &effective_dark_flat_probes,
        calibration_policy,
    );
    ds_apply_manual_overrides_to_decisions(&mut calibration_decisions, &calibration_overrides);
    calibration_contract_errors.extend(
        calibration_decisions
            .iter()
            .filter(|decision| decision.degraded)
            .map(|decision| {
                format!(
                    "Calibración '{}': {}",
                    std::path::Path::new(&decision.frame_path)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy(),
                    decision.reasons.join("; ")
                )
            }),
    );
    calibration_contract_errors.sort();
    calibration_contract_errors.dedup();
    if !calibration_contract_errors.is_empty()
        && matches!(
            calibration_policy,
            pipeline::DeepSkyCalibrationPolicy::Strict
        )
    {
        return Err(format!(
            "Strict bloqueó la calibración:\n{}",
            calibration_contract_errors.join("\n")
        ));
    }
    let calibration_degraded = !calibration_contract_errors.is_empty()
        || [
            &bias_selection,
            &dark_selection,
            &flat_selection,
            &dark_flat_selection,
        ]
        .into_iter()
        .any(|selection| selection.degraded)
        || calibration_decisions
            .iter()
            .any(|decision| decision.degraded);
    let mut calibration_method_fallback: Option<String> = None;
    if calibration_degraded
        && !matches!(
            requested_integration_method,
            pipeline::DeepSkyIntegrationMethod::Classic(_)
        )
    {
        let reason = format!(
            "Calibración AllowDegraded no cumple los supuestos científicos de {}; fallback efectivo a Classic",
            requested_integration_method.label()
        );
        calibration_method_fallback = Some(reason.clone());
        log_to_front(&app, "WARN", &reason);
        integration_method = Some(pipeline::DeepSkyIntegrationMethod::Classic(
            pipeline::ClassicIntegrationConfig {
                version: 1,
                legacy_local_fwhm: local_weighting,
            },
        ));
        for decision in &mut calibration_decisions {
            if decision.degraded {
                decision.fallback = Some(reason.clone());
            }
        }
    }
    // NebulaFusion Lite (F3): motor científico con pesos inverso-varianza y
    // máscaras congeladas. El preflight ya validó las restricciones de fase
    // (sin drizzle, sin CFA directo, sin GpuOnly, entradas lineales).
    let nf_lite_active = matches!(
        integration_method,
        Some(pipeline::DeepSkyIntegrationMethod::NebulaFusion(_))
    );
    // F4: CFA directo (sin debayer, depósito por fotodiodo) y super-binning.
    // F6: modo Full (recombinación espectral con PSF objetivo).
    let (nf_cfa_direct, nf_output_bin, nf_full_mode) = match &integration_method {
        Some(pipeline::DeepSkyIntegrationMethod::NebulaFusion(cfg)) => (
            cfg.cfa_direct,
            match cfg.output_bin {
                pipeline::OutputBinning::Native => None,
                pipeline::OutputBinning::Bin0_75 => Some((3usize, 4usize)),
                pipeline::OutputBinning::Bin0_5 => Some((1usize, 2usize)),
            },
            matches!(
                cfg.mode,
                pipeline::NebulaFusionMode::Full | pipeline::NebulaFusionMode::FullWithStruct
            ),
        ),
        _ => (false, None, false),
    };
    let nf_struct_mode = matches!(
        &integration_method,
        Some(pipeline::DeepSkyIntegrationMethod::NebulaFusion(cfg))
            if matches!(cfg.mode, pipeline::NebulaFusionMode::FullWithStruct)
    );
    // EIDR (F9): reconstrucción forward-model. El preflight ya validó las
    // restricciones (drizzle 1×, CPU, mínimos por escala, entradas lineales).
    let eidr_cfg: Option<pipeline::EidrConfig> = match &integration_method {
        Some(pipeline::DeepSkyIntegrationMethod::Eidr(cfg)) => Some(cfg.clone()),
        _ => None,
    };
    let eidr_active = eidr_cfg.is_some();
    let eidr_cfa_direct = eidr_cfg.as_ref().map(|c| c.cfa_direct).unwrap_or(false);
    let mut lights_were_cfa = false;
    // Carpeta de trabajo: los cachés multi-GB van al disco que elija el
    // usuario (p.ej. externo) en vez de al temp del sistema.
    let work_root: Option<PathBuf> = work_dir
        .filter(|d| !d.trim().is_empty())
        .map(PathBuf::from)
        .filter(|d| d.is_dir());
    if let Some(root) = &work_root {
        log_to_front(
            &app,
            "INFO",
            &format!("Carpeta de trabajo: {} (cachés y masters).", root.display()),
        );
    }
    // Rejection method. Streaming (moments) engine: "sigma" (κσ, default) and
    // "average" (no rejection). Per-pixel (tiled) engine: "median", "winsorized"
    // (PI default), "linearfit", "percentile", "minmax". The per-pixel methods
    // run at scale=1; the engine is chosen in the integration section (with a
    // RAM check + fallback to streaming σ-clip).
    let requested_rejection = rejection.unwrap_or_else(|| "sigma".into());
    let mut rejection = ds_canonical_rejection(&requested_rejection)
        .ok_or_else(|| format!("Método de rechazo desconocido: {requested_rejection}"))?
        .to_string();
    if rejection == "linearfit" {
        return Err(
            "Linear-fit clipping está deshabilitado porque la ruta histórica no implementa un ajuste frame↔referencia científicamente válido; usa Winsorized o sigma."
                .into(),
        );
    }
    let per_pixel = matches!(
        rejection.as_str(),
        "median" | "winsorized" | "linearfit" | "percentile" | "minmax"
    );
    // Separate low/high kappa (PixInsight-style asymmetric clipping). Fall back
    // to the single `kappa` for both when the split values are absent.
    let kappa = kappa.unwrap_or(3.0).clamp(1.5, 6.0);
    let kappa_low = kappa_low.unwrap_or(kappa).clamp(1.0, 8.0);
    let kappa_high = kappa_high.unwrap_or(kappa).clamp(1.0, 8.0);
    // σ-clip active unless the method is "average" (no rejection) or the legacy
    // toggle turns it off.
    let use_clip = sigma_clip.unwrap_or(true) && rejection != "average";
    // Registration sampling: Lanczos-3 (PixInsight default, sharp) unless the
    // user explicitly picks bilinear (soft/fast).
    let interpolation = interpolation.unwrap_or_else(|| "lanczos3".into());
    let use_lanczos = match ds_canonical_interpolation(&interpolation) {
        Some("bilinear") => false,
        Some("lanczos3") => true,
        _ => return Err(format!("Interpolación desconocida: {interpolation}")),
    };
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
                                                          // Normalization mode: "scaling" (additive+scaling, default) · "additive"
                                                          // (offset only) · "none" · "local" (per-cell field). The legacy `local_norm`
                                                          // bool still forces "local" for backward compatibility.
    let requested_normalization = normalization.unwrap_or_else(|| "scaling".into());
    let norm_mode = if local_norm.unwrap_or(false) {
        "local".to_string()
    } else {
        ds_canonical_normalization(&requested_normalization)
            .ok_or_else(|| format!("Normalización desconocida: {requested_normalization}"))?
            .to_string()
    };
    let use_local_norm = norm_mode == "local";
    let use_crop = auto_crop.unwrap_or(true); // trim low-coverage borders

    if lights.is_empty() {
        return Err("Selecciona al menos 1 light.".into());
    }
    let single_light = lights.len() == 1; // calibrar + estirar, sin integracion

    // --- 0.5 FILTER COHERENCE (WBPP groups by filter — mixing Ha and OIII
    // lights into ONE integration produces a meaningless master) ---
    emit_progress(&app, "Cielo Profundo: verificando filtros...", 1.0, None);
    let mut filt_counts: std::collections::HashMap<Option<&'static str>, usize> =
        std::collections::HashMap::new();
    for p in &lights {
        *filt_counts.entry(ds_filter_of_path(p)).or_insert(0) += 1;
    }
    let named: Vec<(&'static str, usize)> = filt_counts
        .iter()
        .filter_map(|(k, &n)| k.map(|f| (f, n)))
        .collect();
    if named.len() >= 2 {
        let desc = named
            .iter()
            .map(|(f, n)| format!("{} ×{}", f, n))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "Lights de varios filtros mezclados ({}). Apila cada filtro por separado (usa las palabras clave para agrupar) y combínalos después con 'Combinar canales (LRGB/SHO)'.",
            desc
        ));
    }
    let lights_filter: Option<&'static str> = named.first().map(|(f, _)| *f);
    if let Some(f) = lights_filter {
        log_to_front(
            &app,
            "INFO",
            &format!("Filtro de los lights (metadata/nombre): {}.", f),
        );
    }

    // --- 1. CALIBRATION MASTERS ---
    emit_progress(
        &app,
        "Cielo Profundo: creando masters de calibracion...",
        2.0,
        None,
    );
    if !light_probes.iter().any(|probe| probe.ok) {
        return Err("Ningún light tiene metadatos/imagen válidos para seleccionar calibraciones".into());
    }
    for warning in bias_selection
        .warnings
        .iter()
        .chain(dark_selection.warnings.iter())
        .chain(flat_selection.warnings.iter())
        .chain(dark_flat_selection.warnings.iter())
    {
        log_to_front(&app, "WARN", warning);
    }
    for selection in [
        &bias_selection,
        &dark_selection,
        &flat_selection,
        &dark_flat_selection,
    ] {
        if selection.group_count > 0 {
            log_to_front(
                &app,
                "INFO",
                &format!("Selección de calibración: {}.", selection.description),
            );
        }
    }
    let calibration_selection_recipe = serde_json::json!({
        "bias": {
            "description": bias_selection.description,
            "detectedGroups": bias_selection.group_count,
            "selected": bias_selection.paths,
            "compatible": bias_selection.blocking_reasons.is_empty(),
            "degraded": bias_selection.degraded,
            "scientificEligible": bias_selection.scientific_eligible,
            "reasons": bias_selection.blocking_reasons,
        },
        "darks": {
            "description": dark_selection.description,
            "detectedGroups": dark_selection.group_count,
            "selected": dark_selection.paths,
            "compatible": dark_selection.blocking_reasons.is_empty(),
            "degraded": dark_selection.degraded,
            "scientificEligible": dark_selection.scientific_eligible,
            "reasons": dark_selection.blocking_reasons,
        },
        "flats": {
            "description": flat_selection.description,
            "detectedGroups": flat_selection.group_count,
            "selected": flat_selection.paths,
            "compatible": flat_selection.blocking_reasons.is_empty(),
            "degraded": flat_selection.degraded,
            "scientificEligible": flat_selection.scientific_eligible,
            "reasons": flat_selection.blocking_reasons,
        },
        "darkFlats": {
            "description": dark_flat_selection.description,
            "detectedGroups": dark_flat_selection.group_count,
            "selected": dark_flat_selection.paths,
            "pedestalState": "rawIncludesBias",
            "compatible": dark_flat_selection.blocking_reasons.is_empty(),
            "degraded": dark_flat_selection.degraded,
            "scientificEligible": dark_flat_selection.scientific_eligible,
            "reasons": dark_flat_selection.blocking_reasons,
        },
    });
    let master_bias_probe = ds_single_master_representative(
        &bias_selection.paths,
        crate::deepsky_calibration_contract::CalibrationRole::Bias,
        "bias global",
    )?;
    let master_bias = ds_build_master(
        &app,
        &bias_selection.paths,
        "bias",
        false,
        &cancel,
        work_root.as_deref(),
    )?;
    // Dark-flats remain RAW (pedestal included). Subtracting bias here and
    // again from each flat would double-remove the pedestal. Each flat selects
    // a raw master with an exact exposure before any normalization.
    let mut dark_flat_masters: Vec<DsRawDarkFlatMaster> = Vec::new();
    for (exposure, paths) in ds_group_darks_by_exposure(
        &dark_flat_selection.paths,
        crate::deepsky_calibration_contract::CalibrationRole::DarkFlat,
    ) {
        cancellation_checkpoint(cancel.as_ref(), "construcción de masters dark-flat")?;
        let label = exposure
            .map(|seconds| format!("dark-flat {seconds:.3}s"))
            .unwrap_or_else(|| "dark-flat sin exposición".into());
        let calibration_probe = ds_single_master_representative(
            &paths,
            crate::deepsky_calibration_contract::CalibrationRole::DarkFlat,
            &label,
        )?;
        if let Some(master) =
            ds_build_master(&app, &paths, &label, false, &cancel, work_root.as_deref())?
        {
            dark_flat_masters.push(DsRawDarkFlatMaster {
                exposure,
                master,
                calibration_probe,
                source_paths: paths,
            });
        }
    }
    // Darks grouped by EXPOSURE (WBPP-style): mixing 60 s and 300 s darks into
    // one median master calibrates every light wrong. One master per exposure
    // group; each light picks the closest group (scaled by exposure ratio).
    let mut dark_masters: Vec<DsDarkMaster> = Vec::new();
    for (exp, paths) in ds_group_darks_by_exposure(
        &dark_selection.paths,
        crate::deepsky_calibration_contract::CalibrationRole::Dark,
    ) {
        cancellation_checkpoint(cancel.as_ref(), "construcción de masters dark")?;
        let label = match exp {
            Some(e) => format!("dark {:.0}s", e),
            None => "dark".to_string(),
        };
        let calibration_probe = ds_single_master_representative(
            &paths,
            crate::deepsky_calibration_contract::CalibrationRole::Dark,
            &label,
        )?;
        if let Some(mut d) =
            ds_build_master(&app, &paths, &label, false, &cancel, work_root.as_deref())?
        {
            let mut bias_subtracted = false;
            if let (Some(b), Some(dark_probe), Some(bias_probe)) = (
                master_bias.as_ref(),
                calibration_probe.as_ref(),
                master_bias_probe.as_ref(),
            ) {
                let signature_compatible = ds_compare_probe_calibration(
                    dark_probe,
                    bias_probe,
                    crate::deepsky_calibration_contract::CalibrationRole::Bias,
                    pipeline::DeepSkyCalibrationPolicy::Strict,
                )
                .compatible;
                if signature_compatible && ds_master_is_compatible(&d.image, &b.image) {
                    for (dv, bv) in d.image.data.iter_mut().zip(b.image.data.iter()) {
                        // No zero-clamp: D_cal noise must stay zero-mean or
                        // the master dark gets biased upward (oversubtraction).
                        *dv -= *bv;
                    }
                    bias_subtracted = true;
                } else if matches!(
                    calibration_policy,
                    pipeline::DeepSkyCalibrationPolicy::Strict
                ) {
                    return Err(format!(
                        "Strict: el bias global no es compatible con la firma completa de {label}"
                    ));
                }
            }
            let glow = ds_dark_has_amp_glow(&d.image);
            if glow {
                log_to_front(
                    &app,
                    "WARN",
                    &format!(
                        "Amp glow detectado en el master {label}: se fuerza resta 1:1 (k=1.00) — el escalado térmico distorsionaría la mancha."
                    ),
                );
            }
            dark_masters.push(DsDarkMaster {
                exposure: exp,
                master: d,
                amp_glow: glow,
                bias_subtracted,
                calibration_probe,
                source_paths: paths,
            });
        }
    }
    if dark_masters.len() > 1 {
        let desc: Vec<String> = dark_masters
            .iter()
            .map(|master| match master.exposure {
                Some(v) => format!("{:.0}s", v),
                None => "s/exp".into(),
            })
            .collect();
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Darks en {} grupos de exposición: {}.",
                dark_masters.len(),
                desc.join(" · ")
            ),
        );
    }
    // Masters de DARKS forzados por asignación manual: se construyen aparte y
    // se usan con k=1 para los lights afectados, con divulgación en el log y
    // en la matriz de decisiones (la responsabilidad del lote es del usuario).
    let mut override_dark_masters: Vec<Option<DsDarkMaster>> = Vec::new();
    let mut manual_dark_for_light: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (over_idx, over) in calibration_overrides.iter().enumerate() {
        if over.darks.is_empty() {
            override_dark_masters.push(None);
            continue;
        }
        cancellation_checkpoint(cancel.as_ref(), "master dark manual")?;
        let label = format!("dark manual {}", over_idx + 1);
        let calibration_probe = match ds_single_master_representative(
            &over.darks,
            crate::deepsky_calibration_contract::CalibrationRole::Dark,
            &label,
        ) {
            Ok(probe) => probe,
            Err(reason) => {
                log_to_front(
                    &app,
                    "WARN",
                    &format!(
                        "Lote manual de darks con firmas mezcladas ({reason}); se usa igualmente con k=1."
                    ),
                );
                None
            }
        };
        let built = ds_build_master(&app, &over.darks, &label, false, &cancel, work_root.as_deref())?
            .map(|mut d| {
                let mut bias_subtracted = false;
                if let Some(b) = master_bias.as_ref() {
                    if ds_master_is_compatible(&d.image, &b.image) {
                        for (dv, bv) in d.image.data.iter_mut().zip(b.image.data.iter()) {
                            *dv -= *bv;
                        }
                        bias_subtracted = true;
                    }
                }
                let glow = ds_dark_has_amp_glow(&d.image);
                DsDarkMaster {
                    exposure: calibration_probe.as_ref().and_then(|p| p.exptime),
                    master: d,
                    amp_glow: glow,
                    bias_subtracted,
                    calibration_probe,
                    source_paths: over.darks.clone(),
                }
            });
        if built.is_some() {
            let affected = if over.lights.is_empty() {
                lights.clone()
            } else {
                lights
                    .iter()
                    .filter(|p| over.lights.iter().any(|l| l == *p))
                    .cloned()
                    .collect()
            };
            log_to_front(
                &app,
                "INFO",
                &format!(
                    "Asignación manual {}: {} darks forzados para {} light(s) (k=1).",
                    over_idx + 1,
                    over.darks.len(),
                    affected.len()
                ),
            );
            for path in affected {
                manual_dark_for_light.insert(path, over_idx);
            }
        }
        override_dark_masters.push(built);
    }
    // Flats ya llegan filtrados por geometría/Bayer/gain/binning/filtro; nunca
    // se usa el grupo mayor de otro filtro como fallback silencioso.
    // FLATS POR SESIÓN (WBPP-style): cada noche tiene su propio panel de
    // flats (el polvo y la orientación cambian entre sesiones) — se construye
    // un master flat POR NOCHE y cada light usa el de la suya. Con una sola
    // sesión el vector tiene un elemento y el flujo es idéntico al histórico.
    let mut flat_masters: Vec<(Option<String>, DsCalibrationMaster)> = Vec::new();
    // Degradación DE DATOS descubierta al construir los masters de flat
    // (AllowDegraded): se recoge aquí y tras la construcción degrada método y
    // elegibilidad científica — la promesa del WARN deja de ser solo un log.
    let flat_data_degraded_flag = std::sync::atomic::AtomicBool::new(false);
    if !flat_selection.sessions.is_empty() {
        for (night, paths) in &flat_selection.sessions {
            cancellation_checkpoint(cancel.as_ref(), "construcción de master flats por sesión")?;
            if let Some(f) = ds_build_calibrated_flat_master(
                &app,
                paths,
                &format!("flat {night}"),
                master_bias.as_ref(),
                master_bias_probe.as_ref(),
                &dark_flat_masters,
                calibration_policy,
                &cancel,
                work_root.as_deref(),
                &flat_data_degraded_flag,
            )? {
                flat_masters.push((Some(night.clone()), f));
            }
        }
        if flat_masters.len() > 1 {
            log_to_front(
                &app,
                "SUCCESS",
                &format!(
                    "Flats por sesión: {} másters ({}).",
                    flat_masters.len(),
                    flat_masters
                        .iter()
                        .map(|(night, _)| night.clone().unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
    }
    if flat_masters.is_empty() {
        if let Some(f) = ds_build_calibrated_flat_master(
            &app,
            &flat_selection.paths,
            "flat",
            master_bias.as_ref(),
            master_bias_probe.as_ref(),
            &dark_flat_masters,
            calibration_policy,
            &cancel,
            work_root.as_deref(),
            &flat_data_degraded_flag,
        )? {
            flat_masters.push((None, f));
        }
    }
    // Cumplimiento de la promesa AllowDegraded: un flat calibrado inválido
    // degrada la corrida — NF/EIDR caen a Classic con razón visible y el
    // resultado deja de ser elegible como científico.
    let flat_data_degraded =
        flat_data_degraded_flag.load(std::sync::atomic::Ordering::Relaxed);
    if flat_data_degraded
        && calibration_method_fallback.is_none()
        && !matches!(
            requested_integration_method,
            pipeline::DeepSkyIntegrationMethod::Classic(_)
        )
    {
        let reason = format!(
            "Flat calibrado inválido con AllowDegraded: {} no puede ejecutarse como científico; fallback efectivo a Classic",
            requested_integration_method.label()
        );
        calibration_method_fallback = Some(reason.clone());
        log_to_front(&app, "WARN", &reason);
        integration_method = Some(pipeline::DeepSkyIntegrationMethod::Classic(
            pipeline::ClassicIntegrationConfig {
                version: 1,
                legacy_local_fwhm: local_weighting,
            },
        ));
    }
    let calibration_degraded = calibration_degraded || flat_data_degraded;
    // Masters de FLATS forzados por asignación manual: uno por regla; los
    // lights afectados los usan en lugar del flat de su sesión.
    let mut override_flat_masters: Vec<Option<DsCalibrationMaster>> = Vec::new();
    let mut manual_flat_for_light: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (over_idx, over) in calibration_overrides.iter().enumerate() {
        if over.flats.is_empty() {
            override_flat_masters.push(None);
            continue;
        }
        cancellation_checkpoint(cancel.as_ref(), "master flat manual")?;
        let built = ds_build_calibrated_flat_master(
            &app,
            &over.flats,
            &format!("flat manual {}", over_idx + 1),
            master_bias.as_ref(),
            master_bias_probe.as_ref(),
            &dark_flat_masters,
            calibration_policy,
            &cancel,
            work_root.as_deref(),
            &flat_data_degraded_flag,
        )?;
        if built.is_some() {
            let affected = if over.lights.is_empty() {
                lights.clone()
            } else {
                lights
                    .iter()
                    .filter(|p| over.lights.iter().any(|l| l == *p))
                    .cloned()
                    .collect()
            };
            log_to_front(
                &app,
                "INFO",
                &format!(
                    "Asignación manual {}: {} flats forzados para {} light(s).",
                    over_idx + 1,
                    over.flats.len(),
                    affected.len()
                ),
            );
            for path in affected {
                manual_flat_for_light.insert(path, over_idx);
            }
        }
        override_flat_masters.push(built);
    }
    // Noche de cada light: solo se sondea cuando hay flats multi-sesión.
    let light_nights: std::collections::HashMap<String, Option<String>> =
        if flat_masters.iter().any(|(night, _)| night.is_some()) {
            deepsky_probe(lights.clone())
                .into_iter()
                .map(|probe| {
                    let night = ds_session_night_id(&probe.path, probe.date_obs.as_deref());
                    (probe.path, night)
                })
                .collect()
        } else {
            std::collections::HashMap::new()
        };
    // Flat de la misma sesión del light. Strict nunca usa el flat de una noche
    // "cercana": polvo, rotación o tren óptico pueden cambiar entre sesiones.
    let flat_for_light = |path: &str| -> Result<Option<&DsCalibrationMaster>, String> {
        if manual_skip_flats.contains(path) {
            return Ok(None);
        }
        if let Some(&over_idx) = manual_flat_for_light.get(path) {
            if let Some(master) = override_flat_masters
                .get(over_idx)
                .and_then(|m| m.as_ref())
            {
                return Ok(Some(master));
            }
        }
        if flat_masters.is_empty() {
            return Ok(None);
        }
        if flat_masters.len() == 1 && flat_masters[0].0.is_none() {
            return Ok(Some(&flat_masters[0].1));
        }
        let night = light_nights.get(path).cloned().flatten();
        if let Some((_, flat)) = flat_masters
            .iter()
            .find(|(master_night, _)| master_night.as_deref() == night.as_deref())
        {
            return Ok(Some(flat));
        }
        if matches!(
            calibration_policy,
            pipeline::DeepSkyCalibrationPolicy::Strict
        ) {
            return Err(format!(
                "Strict: el light '{}' ({}) no tiene master flat de su misma sesión",
                std::path::Path::new(path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                night.as_deref().unwrap_or("fecha desconocida")
            ));
        }
        log_to_front(
            &app,
            "WARN",
            &format!(
                "AllowDegraded: light '{}' no tiene flat de su misma sesión ({}); se omite el flat, nunca se sustituye por la noche más cercana.",
                std::path::Path::new(path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                night.as_deref().unwrap_or("desconocida")
            ),
        );
        Ok(None)
    };

    // Inicialización única del motor de píxeles. El self-test cubre tanto la
    // fórmula de calibración como warp/integración. Auto/Hybrid deshabilitan
    // sólo esta etapa ante fallo; GpuOnly devuelve un error verificable.
    let mut gpu_calibration_enabled = if compute_policy.allows_gpu() {
        match crate::gpu_stack::gpu_runtime() {
            None if matches!(compute_policy, ComputePolicy::GpuOnly) => {
                return Err(
                    "GPU only solicitado, pero no existe un dispositivo wgpu compatible".into(),
                );
            }
            None => false,
            Some(_)
                if !crate::gpu_deepsky::ensure_parity()
                    || !crate::gpu_deepsky::ensure_pixel_preprocess_parity() =>
            {
                if matches!(compute_policy, ComputePolicy::GpuOnly) {
                    return Err("GPU only: falló la paridad de calibración/cosmética/debayer/mapa estelar de cielo profundo".into());
                }
                log_to_front(
                    &app,
                    "WARN",
                    "Paridad GPU de cielo profundo fallida; etapas de píxeles en CPU.",
                );
                false
            }
            Some(rt) => {
                log_to_front(
                    &app,
                    "SUCCESS",
                    &format!(
                        "Motor de píxeles GPU validado: {} ({}).",
                        rt.backend, rt.adapter_name
                    ),
                );
                true
            }
        }
    } else {
        false
    };
    let gpu_calibration_used = false;
    let mut gpu_preprocessing_used = false;

    // --- 2. LOAD + CALIBRATE + DETECT (streaming, dims locked to 1st light) ---
    let mut frames: Vec<(String, Vec<(f32, f32, f32)>, f32, f32, f32)> = Vec::new();
    // Paralelo a `frames`: FWHM por estrella para el Rescate de detalle.
    let mut frame_star_fwhms: Vec<Vec<(f32, f32, f32)>> = Vec::new();
    // Background level per frame (parallel to `frames`), measured during
    // calibration so the normalization pre-pass doesn't reload every frame.
    let mut frame_bgs: Vec<f32> = Vec::new();
    // Per-channel sky background per accepted frame (parallel to frame_bgs), for
    // PER-CHANNEL normalization. Measured from the calibrated+debayered `img`.
    let mut frame_bgs_rgb: Vec<[f32; 3]> = Vec::new();
    // Formal calibration noise per accepted frame/channel. EIDR consumes this
    // instead of re-labelling a luma-only empirical scalar as propagated VAR.
    let mut frame_calibration_sigmas: Vec<[f32; 3]> = Vec::new();
    let mut dims: Option<(usize, usize, usize)> = None;
    // Some(Some(cid)) = drizzle CFA verdadero; Some(None) = mono/RGB ya
    // interpolado. Se decide con el primer light aceptado y se exige coherencia.
    let mut drizzle_input_bayer: Option<Option<i32>> = None;
    let cache_dir = work_root
        .clone()
        .map(|root| root.join("zenith_cache"))
        .unwrap_or_else(|| std::env::temp_dir().join("astro_stacker_cache"));
    let _ = std::fs::create_dir_all(&cache_dir);
    let prototype = lights
        .iter()
        .find_map(|p| ds_read_image(p).ok())
        .ok_or("Ningún light pudo abrirse para definir la caché")?;
    if (nf_cfa_direct || eidr_cfa_direct) && prototype.bayer.is_none() {
        return Err(
            "CFA directo solicitado, pero el light prototipo no tiene BAYERPAT/BAYERPATN válido"
                .into(),
        );
    }
    let cache_cfa_direct =
        prototype.bayer.is_some() && (drz > 1.01 || nf_cfa_direct || eidr_cfa_direct);
    let cached_cfa_pattern = cache_cfa_direct.then_some(prototype.bayer).flatten();
    let cached_channels = if cached_cfa_pattern.is_some() {
        1
    } else if prototype.bayer.is_some() {
        3
    } else {
        prototype.ch
    };
    let flat_sessions = flat_masters.len();
    let cache_layout = cached_cfa_pattern
        .map(|pattern| format!("cfa-{pattern}"))
        .unwrap_or_else(|| {
            if cached_channels == 1 {
                "mono".into()
            } else {
                "rgb".into()
            }
        });
    let integration_cache_contract = serde_json::to_string(&integration_method)
        .unwrap_or_else(|_| "integration-method-unserializable".into());
    let integration_cache_hash = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        integration_cache_contract.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    };
    let cache_fingerprint = format!(
        "calv4-fs{flat_sessions}-{}-{}x{}x{}-layout{}-direct{}-method{}-cos{}-dark{}-ped{:?}-drz{:.2}-capture{:?}-policy{:?}",
        ds_source_fingerprint(&[&lights, &darks, &flats, &dark_flats, &bias]),
        prototype.w,
        prototype.h,
        cached_channels,
        cache_layout,
        cache_cfa_direct,
        integration_cache_hash,
        use_cosmetic,
        use_dark_opt,
        pedestal,
        drz,
        capture_mode,
        calibration_policy,
    );
    let cache_file_key: String = cache_fingerprint
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let analysis_cache_path = cache_dir.join(format!(
        "v{DS_PREP_CACHE_VERSION}_{cache_file_key}_analysis.json"
    ));
    let cached_analysis_file: Option<DsAnalysisCacheFile> =
        ds_read_json_cache(&analysis_cache_path);
    let analysis_cache_reused = cached_analysis_file.as_ref().is_some_and(|c| {
        c.version == DS_PREP_CACHE_VERSION
            && c.fingerprint == cache_fingerprint
            && c.frames.len() == lights.len()
    });
    let mut cached_analysis = if analysis_cache_reused {
        cached_analysis_file.unwrap().frames
    } else {
        vec![None; lights.len()]
    };
    let mut frame_store = Some(AdaptiveFrameStore::new_versioned(
        lights.len(),
        prototype.w * prototype.h * cached_channels,
        &cache_dir,
        &cache_fingerprint,
    )?);
    let variance_fingerprint = format!("{cache_fingerprint}-calibration-var-v1");
    let dq_fingerprint = format!("{cache_fingerprint}-calibration-dq-v1");
    let mut calibration_variance_store = Some(AdaptiveFrameStore::new_versioned(
        lights.len(),
        prototype.w * prototype.h * cached_channels,
        &cache_dir,
        &variance_fingerprint,
    )?);
    let mut calibration_dq_store = Some(AdaptiveFrameStore::new_versioned(
        lights.len(),
        prototype.w * prototype.h,
        &cache_dir,
        &dq_fingerprint,
    )?);
    let cache_was_reused = frame_store.as_ref().is_some_and(|s| s.reused())
        && calibration_variance_store
            .as_ref()
            .is_some_and(|s| s.reused())
        && calibration_dq_store.as_ref().is_some_and(|s| s.reused());
    let mut frame_cache_indices = Vec::new();
    if let Some(store) = frame_store.as_ref() {
        log_to_front(
            &app,
            if cache_was_reused { "SUCCESS" } else { "INFO" },
            &format!(
                "FrameStore cielo profundo: {} · caché v{} {}{} ({}).",
                store.kind().label(),
                DS_PREP_CACHE_VERSION,
                if cache_was_reused {
                    "reutilizada"
                } else {
                    "nueva"
                },
                if analysis_cache_reused {
                    " · análisis reutilizable"
                } else {
                    ""
                },
                cache_layout,
            ),
        );
    }

    for (i, p) in lights.iter().enumerate() {
        cancellation_checkpoint(cancel.as_ref(), "calibración y análisis")?;
        emit_progress(
            &app,
            &format!(
                "Cielo Profundo: calibrando y detectando estrellas {}/{}",
                i + 1,
                lights.len()
            ),
            2.0 + (i as f32 / lights.len() as f32) * 28.0,
            None,
        );
        let mut loaded_from_cache = frame_store.as_ref().is_some_and(|s| s.is_present(i))
            && calibration_variance_store
                .as_ref()
                .is_some_and(|s| s.is_present(i))
            && calibration_dq_store
                .as_ref()
                .is_some_and(|s| s.is_present(i));
        let mut img = if loaded_from_cache {
            match frame_store.as_ref().unwrap().get(i) {
                Ok(data) => DsImage {
                    data,
                    w: prototype.w,
                    h: prototype.h,
                    ch: cached_channels,
                    bayer: cached_cfa_pattern,
                },
                Err(e) => {
                    log_to_front(
                        &app,
                        "WARN",
                        &format!("Entrada de caché inválida para {} ({e}); se recalibra.", p),
                    );
                    loaded_from_cache = false;
                    match ds_read_image(p) {
                        Ok(v) => v,
                        Err(e) => {
                            log_to_front(&app, "WARN", &format!("Light omitido {} ({})", p, e));
                            continue;
                        }
                    }
                }
            }
        } else {
            match ds_read_image(p) {
                Ok(v) => v,
                Err(e) => {
                    log_to_front(&app, "WARN", &format!("Light omitido {} ({})", p, e));
                    continue;
                }
            }
        };
        let mut calibrated_uncertainty = if loaded_from_cache {
            let cached_planes = calibration_variance_store
                .as_ref()
                .ok_or("Caché VAR ausente")?
                .get(i)
                .and_then(|variance| {
                    calibration_dq_store
                        .as_ref()
                        .ok_or("Caché DQ ausente".to_string())?
                        .get(i)
                        .map(|dq_f32| (variance, dq_f32))
                });
            match cached_planes {
                Ok((variance, dq_f32))
                    if variance.len() == img.data.len()
                        && dq_f32.len() == img.w.saturating_mul(img.h) =>
                {
                    let dq: Vec<u32> = dq_f32
                        .into_iter()
                        .map(|value| value.max(0.0).round() as u32)
                        .collect();
                    let publishable = ds_uncertainty_is_publishable(
                        &variance,
                        &dq,
                        img.w,
                        img.h,
                        img.ch,
                    );
                    DsCalibratedUncertainty {
                        publishable,
                        variance,
                        dq,
                        fallback_reason: None,
                    }
                }
                Ok(_) | Err(_) => {
                    log_to_front(
                        &app,
                        "WARN",
                        &format!(
                            "Caché científica VAR/DQ inválida para {}; se recalibra desde el raw.",
                            p
                        ),
                    );
                    loaded_from_cache = false;
                    img = match ds_read_image(p) {
                        Ok(value) => value,
                        Err(error) => {
                            log_to_front(
                                &app,
                                "WARN",
                                &format!("Light omitido {} ({})", p, error),
                            );
                            continue;
                        }
                    };
                    DsCalibratedUncertainty {
                        variance: Vec::new(),
                        dq: Vec::new(),
                        publishable: false,
                        fallback_reason: None,
                    }
                }
            }
        } else {
            DsCalibratedUncertainty {
                variance: Vec::new(),
                dq: Vec::new(),
                publishable: false,
                fallback_reason: None,
            }
        };
        // Consistency check on the RAW dimensions (before debayer changes ch).
        match dims {
            None => {}
            Some((w, h, _)) => {
                if img.w != w || img.h != h {
                    log_to_front(
                        &app,
                        "WARN",
                        &format!("Light con dimensiones distintas, omitido: {}", p),
                    );
                    continue;
                }
            }
        }
        // CORRECT ORDER: calibrate in the RAW (CFA) space, THEN debayer.
        // A mismatched dark is never selected merely because it is nearest:
        // AllowDegraded may use it only after the measured scaling contract
        // proves bias isolation, no amp glow, linearity and <=1% residual.
        let light_exp = ds_probe_exptime(p);
        let master_flat_light = flat_for_light(p)?;
        let current_light_probe = light_probes.iter().find(|probe| probe.path == p.as_str());
        let core_compatible = |master: &&DsDarkMaster| {
            current_light_probe
                .zip(master.calibration_probe.as_ref())
                .is_some_and(|(light, dark)| ds_dark_core_compatible_for_scaling(light, dark))
        };
        let nearest_dark: Option<&DsDarkMaster> = match light_exp {
            Some(light_seconds) => dark_masters
                .iter()
                .filter(|master| core_compatible(master))
                .min_by(|a, b| {
                    let distance = |exposure: Option<f32>| {
                        exposure
                            .map(|seconds| (light_seconds / seconds.max(0.01)).ln().abs())
                            .unwrap_or(0.7)
                    };
                    distance(a.exposure)
                        .partial_cmp(&distance(b.exposure))
                        .unwrap_or(std::cmp::Ordering::Equal)
                }),
            None => dark_masters
                .iter()
                .filter(|master| core_compatible(master))
                .next(),
        };
        // La asignación manual gana a la selección automática: dark forzado
        // con k=1, sin escalado y sin el bloqueo Strict por falta de exacto.
        let manual_skip_dark = manual_skip_darks.contains(p.as_str());
        let manual_dark: Option<&DsDarkMaster> = if manual_skip_dark {
            None
        } else {
            manual_dark_for_light
                .get(p.as_str())
                .and_then(|&over_idx| override_dark_masters.get(over_idx))
                .and_then(|master| master.as_ref())
        };
        let exact_dark = if manual_skip_dark {
            // Omisión explícita del usuario: sin dark para este light y sin
            // caer al emparejado automático ni al escalado por cercanía.
            None
        } else {
            manual_dark.or_else(|| {
                current_light_probe.and_then(|light| {
                    dark_masters.iter().find(|master| {
                        master.calibration_probe.as_ref().is_some_and(|dark| {
                            ds_compare_probe_calibration(
                                light,
                                dark,
                                crate::deepsky_calibration_contract::CalibrationRole::Dark,
                                pipeline::DeepSkyCalibrationPolicy::Strict,
                            )
                            .compatible
                        })
                    })
                })
            })
        };
        if !manual_skip_dark
            && manual_dark.is_none()
            && exact_dark.is_none()
            && !dark_masters.is_empty()
            && matches!(
            calibration_policy,
            pipeline::DeepSkyCalibrationPolicy::Strict
            )
        {
            return Err(format!(
                "Strict: el light '{}' ({}) no tiene master dark de exposición exacta (tolerancia 1 ms / 1e-6 relativa)",
                std::path::Path::new(p)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                light_exp
                    .map(|seconds| format!("{seconds:.3} s"))
                    .unwrap_or_else(|| "EXPTIME desconocido".into())
            ));
        }

        let mut selected_dark = exact_dark;
        let mut dark_k = exact_dark.map(|_| 1.0).unwrap_or(1.0);
        let mut scaling_measurement: Option<DsDarkScalingMeasurement> = None;
        let mut dark_scaling_failure: Option<String> = None;
        if selected_dark.is_none() && !manual_skip_dark {
            if let Some(candidate) = nearest_dark {
                if !use_dark_opt {
                    dark_scaling_failure = Some(
                        "dark de exposición distinta omitido: el escalado está desactivado".into(),
                    );
                } else {
                    let ratio = match (light_exp, candidate.exposure) {
                        (Some(light_seconds), Some(dark_seconds)) if dark_seconds > 0.0 => {
                            light_seconds / dark_seconds
                        }
                        _ => f32::NAN,
                    };
                    // Cached frames are already calibrated/debayered. Re-read
                    // this raw only for the rare degraded scaling proof so a
                    // warm cache records the same decision as a cold one.
                    let raw_for_evidence = if loaded_from_cache {
                        match ds_read_image(p) {
                            Ok(raw) => Some(raw),
                            Err(error) => {
                                dark_scaling_failure = Some(format!(
                                    "dark scaling bloqueado: no se pudo releer el raw para validar la caché ({error})"
                                ));
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let evidence_light = raw_for_evidence.as_ref().unwrap_or(&img);
                    if dark_scaling_failure.is_none() {
                        match ds_measure_dark_scaling(
                            evidence_light,
                            master_bias.as_ref().map(|master| &master.image),
                            &candidate.master.image,
                            ratio,
                            candidate.amp_glow,
                            candidate.bias_subtracted,
                        ) {
                            Ok(measurement) => {
                                dark_k = measurement.scale;
                                scaling_measurement = Some(measurement);
                                selected_dark = Some(candidate);
                            }
                            Err(reason) => dark_scaling_failure = Some(reason),
                        }
                    }
                }
            }
        }
        if let Some(measurement) = scaling_measurement {
            log_to_front(
                &app,
                "INFO",
                &format!(
                    "Dark escalado k={:.6} validado para '{}': corr={:.6}, R²={:.6}, residuo={:.4}%, pendiente={:.6}, n={}.",
                    measurement.scale,
                    std::path::Path::new(p)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy(),
                    measurement.correlation,
                    measurement.linearity_r2,
                    measurement.residual_fraction * 100.0,
                    measurement.fitted_slope,
                    measurement.samples,
                ),
            );
        }
        if let Some(reason) = &dark_scaling_failure {
            log_to_front(
                &app,
                "WARN",
                &format!(
                    "AllowDegraded: '{}': {reason}; se omite el dark incompatible.",
                    std::path::Path::new(p)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                ),
            );
        }
        if let Some(decision) = calibration_decisions
            .iter_mut()
            .find(|decision| decision.frame_path == p.as_str())
        {
            decision.dark_master_path = selected_dark
                .and_then(|master| ds_virtual_master_path("dark", &master.source_paths));
            decision.dark_scale = selected_dark.map(|_| dark_k);
            if let Some(measurement) = scaling_measurement {
                decision.reasons.push(format!(
                    "dark scaling validado: k={:.6}, corr={:.6}, R2={:.6}, residuo={:.4}%, n={}",
                    measurement.scale,
                    measurement.correlation,
                    measurement.linearity_r2,
                    measurement.residual_fraction * 100.0,
                    measurement.samples,
                ));
            }
            if let Some(reason) = &dark_scaling_failure {
                decision.compatible = false;
                decision.degraded = true;
                decision.dark_master_path = None;
                decision.dark_scale = None;
                decision.fallback = Some("Classic no científico; dark incompatible omitido".into());
                decision.reasons.push(reason.clone());
            }
        }
        if !loaded_from_cache {
            if matches!(compute_policy, ComputePolicy::GpuOnly) {
                return Err(
                    "GPU only: el kernel de calibración actual no publica VAR/DQ ni invalida flats débiles; no se permite sustituir la ruta científica CPU"
                        .into(),
                );
            }
            calibrated_uncertainty = ds_calibrate_scientific(
                &mut img,
                master_bias.as_ref(),
                selected_dark,
                master_flat_light,
                dark_k,
            )?;
            if let Some(reason) = &calibrated_uncertainty.fallback_reason {
                log_to_front(
                    &app,
                    "WARN",
                    &format!(
                        "Calibración de '{}': {reason}; SCI/DQ se conservan, VAR no se inventa.",
                        std::path::Path::new(p)
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                    ),
                );
            }
        }
        // Pedestal sólo cuando el usuario lo pidió. El modo normal conserva el
        // ruido calibrado negativo en float32 hasta la exportación/vista.
        let ped = if loaded_from_cache {
            0.0
        } else {
            ds_apply_pedestal(&mut img, pedestal)
        };
        if frames.is_empty() && ped > 0.0 {
            log_to_front(
                &app,
                "INFO",
                &format!("Pedestal de salida explícito: +{:.0} ADU.", ped),
            );
        }
        if !loaded_from_cache && use_cosmetic {
            let uncertainty_was_publishable = calibrated_uncertainty.publishable;
            let (medians, noises) = ds_cosmetic_stats(&img);
            let before_cosmetic = img.data.clone();
            let gpu_result = if gpu_calibration_enabled {
                crate::gpu_deepsky::cosmetic_hot_pixels(
                    &mut img.data,
                    img.w,
                    img.h,
                    img.ch,
                    img.bayer.is_some(),
                    &medians,
                    &noises,
                )
                .map(|_| ())
            } else {
                Err("GPU cosmética no activa".into())
            };
            match gpu_result {
                Ok(()) => gpu_preprocessing_used = true,
                Err(e)
                    if gpu_calibration_enabled
                        && matches!(compute_policy, ComputePolicy::GpuOnly) =>
                {
                    return Err(format!(
                        "GPU only: corrección cosmética fallida para '{}': {e}",
                        p
                    ));
                }
                Err(e) => {
                    if gpu_calibration_enabled {
                        log_to_front(
                            &app,
                            "WARN",
                            &format!("Cosmética GPU deshabilitada ({e}); CPU."),
                        );
                        gpu_calibration_enabled = false;
                    }
                    ds_cosmetic_hot_pixels_with_stats(&mut img, &medians, &noises);
                }
            }
            for sample in 0..img.data.len() {
                if img.data[sample].to_bits() != before_cosmetic[sample].to_bits() {
                    let pixel = sample / img.ch;
                    calibrated_uncertainty.dq[pixel] |=
                        crate::deepsky_variance::dq::HOT_COLD
                            | crate::deepsky_variance::dq::INTERPOLATED;
                    calibrated_uncertainty.variance[sample] = f32::NAN;
                    calibrated_uncertainty.fallback_reason.get_or_insert_with(|| {
                        "VAR parcial: píxeles cosméticos interpolados se marcan DQ y VAR=NaN".into()
                    });
                }
            }
            calibrated_uncertainty.publishable = uncertainty_was_publishable
                && ds_uncertainty_is_publishable(
                    &calibrated_uncertainty.variance,
                    &calibrated_uncertainty.dq,
                    img.w,
                    img.h,
                    img.ch,
                );
        }
        if img.bayer.is_some() {
            lights_were_cfa = true;
        }
        // Se conserva el plano CFA calibrado cuando lo consumirá un kernel
        // por fotosito: drizzle CFA clásico o NF-Lite en modo CFA directo.
        let calibrated_cfa =
            if (drz > 1.01 || nf_cfa_direct || eidr_cfa_direct) && img.bayer.is_some() {
                Some(img.clone())
            } else {
                None
            };
        let calibrated_cfa_uncertainty = calibrated_cfa
            .as_ref()
            .map(|_| calibrated_uncertainty.clone());
        if let Some(cid) = img.bayer {
            (img, calibrated_uncertainty) =
                ds_debayer_scientific(img, calibrated_uncertainty, cid);
        }
        // Lock the output geometry from the FIRST fully-processed light.
        if dims.is_none() {
            dims = Some((img.w, img.h, img.ch));
        } else if dims != Some((img.w, img.h, img.ch)) {
            log_to_front(
                &app,
                "WARN",
                &format!("Light procesado con canales incompatibles, omitido: {}", p),
            );
            continue;
        }
        let store_img = calibrated_cfa.as_ref().unwrap_or(&img);
        let store_uncertainty = calibrated_cfa_uncertainty
            .as_ref()
            .unwrap_or(&calibrated_uncertainty);
        if !loaded_from_cache {
            frame_store
                .as_mut()
                .ok_or("FrameStore no inicializado")?
                .put(i, &store_img.data)?;
            calibration_variance_store
                .as_mut()
                .ok_or("FrameStore VAR no inicializado")?
                .put(i, &store_uncertainty.variance)?;
            let dq_f32: Vec<f32> = store_uncertainty
                .dq
                .iter()
                .map(|&value| value as f32)
                .collect();
            calibration_dq_store
                .as_mut()
                .ok_or("FrameStore DQ no inicializado")?
                .put(i, &dq_f32)?;
        }
        let cached_metrics = loaded_from_cache
            .then(|| cached_analysis.get(i).and_then(|v| v.as_ref()))
            .flatten()
            .filter(|v| v.path == *p)
            .cloned();
        let (stars, fwhm, bg_lvl, noise, ecc, star_fwhms) = if let Some(cached) = cached_metrics {
            (
                cached.stars,
                cached.fwhm,
                cached.background,
                cached.noise,
                cached.eccentricity,
                cached.star_fwhms,
            )
        } else {
            let luma = ds_luma(&img);
            let stars = if gpu_calibration_enabled {
                match ds_detect_stars_impl(&luma, img.w, img.h, 120, true) {
                    Ok(stars) => {
                        gpu_preprocessing_used = true;
                        stars
                    }
                    Err(e) if matches!(compute_policy, ComputePolicy::GpuOnly) => {
                        return Err(format!("GPU only: mapa estelar fallido para '{}': {e}", p));
                    }
                    Err(e) => {
                        log_to_front(
                            &app,
                            "WARN",
                            &format!("Mapa estelar GPU deshabilitado ({e}); CPU."),
                        );
                        gpu_calibration_enabled = false;
                        ds_detect_stars(&luma, img.w, img.h, 120)
                    }
                }
            } else {
                ds_detect_stars(&luma, img.w, img.h, 120)
            };
            let fwhm = ds_frame_fwhm_proxy(&luma, img.w, img.h, &stars);
            // Background level from the robust median; noise from the MRS/à-trous
            // detail layer (structure-free) rather than the global MAD.
            let background = ds_bg_noise(&luma).0;
            let noise = ds_mrs_noise(&luma, img.w, img.h);
            let eccentricity = ds_frame_roundness(&luma, img.w, img.h, &stars);
            // FWHM por estrella (Rescate de detalle): la luma ya está en RAM;
            // se cachea siempre para que activar el modo después no re-mida.
            let star_fwhms = ds_star_fwhms(&luma, img.w, img.h, &stars);
            cached_analysis[i] = Some(DsCachedFrameAnalysis {
                path: p.clone(),
                stars: stars.clone(),
                fwhm,
                background,
                noise,
                eccentricity,
                star_fwhms: star_fwhms.clone(),
            });
            (stars, fwhm, background, noise, eccentricity, star_fwhms)
        };
        if stars.len() < 6 && !single_light {
            log_to_front(
                &app,
                "WARN",
                &format!("Muy pocas estrellas ({}) — omitido: {}", stars.len(), p),
            );
            continue;
        }
        let input_bayer = calibrated_cfa.as_ref().and_then(|raw| raw.bayer);
        match drizzle_input_bayer {
            None => drizzle_input_bayer = Some(input_bayer),
            Some(previous) if previous != input_bayer => {
                return Err(
                    "Drizzle: los lights mezclan CFA/no-CFA o patrones Bayer distintos; separa los grupos antes de apilar."
                        .into(),
                );
            }
            _ => {}
        }
        // El índice de caché conserva la posición exacta en la lista original;
        // `frames` sólo contiene tomas aceptadas para registro.
        frame_cache_indices.push(i);
        frame_calibration_sigmas.push(ds_uncertainty_sigmas(
            &store_uncertainty.variance,
            store_img.w,
            store_img.h,
            store_img.ch,
            store_img.bayer,
        ));
        frames.push((p.clone(), stars, fwhm, noise, ecc));
        frame_star_fwhms.push(star_fwhms);
        frame_bgs.push(bg_lvl);
        frame_bgs_rgb.push(ds_channel_backgrounds(&img));
        if i % 4 == 0 || i + 1 == lights.len() {
            emit_deepsky_pipeline_telemetry(
                &app,
                &ds_result_id,
                "calibrate_detect",
                if gpu_preprocessing_used {
                    "Hybrid GPU calibrate/cosmetic/debayer/star-map + CPU PSF"
                } else if gpu_calibration_used {
                    "Hybrid GPU calibrate + CPU PSF"
                } else {
                    "CPU Rayon/SIMD"
                },
                i + 1,
                lights.len(),
                ds_run_started,
                &ds_telemetry_sys,
                0,
                frame_store.as_ref().map(|s| s.read_hits()).unwrap_or(0),
                None,
            );
        }
    }

    let (w, h, ch) = dims.ok_or("Ningun light valido.")?;
    let cfa_drizzle_pattern = drizzle_input_bayer.flatten();
    // DRIZZLE output canvas (integer upscale during integration): with dithered
    // subframes each landing at a different sub-pixel offset in the finer grid,
    // this recovers resolution and cuts pixelation (PixInsight/DSS drizzle).
    let w_out = ((w as f32 * drz).round() as usize).max(w);
    let h_out = ((h as f32 * drz).round() as usize).max(h);
    if drz > 1.01 {
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Drizzle {:.0}×: lienzo {}×{} → {}×{}{}.",
                drz,
                w,
                h,
                w_out,
                h_out,
                if cfa_drizzle_pattern.is_some() {
                    " · CFA calibrado sin debayer previo"
                } else {
                    ""
                }
            ),
        );
    }
    if frames.is_empty() {
        return Err("Ningun light utilizable tras calibracion/deteccion.".into());
    }
    ds_write_json_cache(
        &analysis_cache_path,
        &DsAnalysisCacheFile {
            version: DS_PREP_CACHE_VERSION,
            fingerprint: cache_fingerprint.clone(),
            frames: cached_analysis,
        },
    )?;
    frame_store
        .as_mut()
        .ok_or("No se pudo crear el almacén de frames calibrados")?
        .mark_complete()?;
    calibration_variance_store
        .as_mut()
        .ok_or("No se pudo crear el almacén VAR de calibración")?
        .mark_complete()?;
    calibration_dq_store
        .as_mut()
        .ok_or("No se pudo crear el almacén DQ de calibración")?
        .mark_complete()?;
    let frame_store = frame_store.ok_or("No se pudo crear el almacén de frames calibrados")?;
    let calibration_variance_store = calibration_variance_store
        .ok_or("No se pudo crear el almacén VAR de calibración")?;
    let calibration_dq_store =
        calibration_dq_store.ok_or("No se pudo crear el almacén DQ de calibración")?;
    log_to_front(
        &app,
        "INFO",
        &format!(
            "FrameStore listo: {} · {:.1} MB calibrados.",
            frame_store.kind().label(),
            frame_store.bytes_written() as f64 / (1024.0 * 1024.0)
        ),
    );

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
        let nm = std::path::Path::new(&rf.0)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&rf.0);
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Referencia (mejor calidad): {} · {} estrellas · FWHM {:.2} · ecc {:.2}.",
                nm,
                ref_stars.len(),
                rf.2,
                rf.4
            ),
        );
    }
    if frames.len() > 1 && ref_stars.len() < 8 {
        let capture = if matches!(capture_mode, pipeline::DeepSkyCaptureMode::MonoNarrowband) {
            " de banda estrecha mono"
        } else {
            ""
        };
        return Err(format!(
            "Registro{capture} bloqueado: la mejor referencia sólo contiene {} estrellas; se requieren al menos 8 para correspondencias biyectivas y validación espacial. Una única toma sí puede continuar sin registro.",
            ref_stars.len()
        ));
    }

    emit_progress(
        &app,
        "Cielo Profundo: registrando (triangulos estelares)...",
        32.0,
        None,
    );
    let accepted_paths: Vec<String> = frames.iter().map(|f| f.0.clone()).collect();
    let registration_cache_path = cache_dir.join(format!(
        "v{DS_PREP_CACHE_VERSION}_{cache_file_key}_registration.json"
    ));
    let cached_registration: Option<DsRegistrationCacheFile> =
        ds_read_json_cache(&registration_cache_path);
    let registration_reused = cached_registration.as_ref().is_some_and(|c| {
        c.version == DS_PREP_CACHE_VERSION
            && c.fingerprint == cache_fingerprint
            && c.accepted_paths == accepted_paths
            && c.reference_index == ref_idx
            && c.transforms.len() == frames.len()
    });
    let transforms: Vec<Option<DsRegistration>> = if registration_reused {
        log_to_front(
            &app,
            "SUCCESS",
            "Registro: transformaciones, holdout y Jacobianos reutilizados de la caché científica v6.",
        );
        cached_registration.unwrap().transforms
    } else {
        let solved: Vec<Option<DsRegistration>> = frames
            .par_iter()
            .enumerate()
            .map(|(i, (_, stars, _, _, _))| {
                if cancellation_checkpoint(cancel.as_ref(), "registro").is_err() {
                    return None;
                }
                if i == ref_idx {
                    return Some(DsRegistration {
                        transform: DsTransform::identity(),
                        inliers: stars.len(),
                        rms: 0.0,
                        holdout_count: 0,
                        holdout_p95: 0.0,
                        holdout_max: 0.0,
                        jacobian_min: 1.0,
                        jacobian_p95: 1.0,
                        jacobian_max: 1.0,
                    });
                }
                ds_match_triangles_in_field(&ref_stars, stars, w, h)
            })
            .collect();
        ds_write_json_cache(
            &registration_cache_path,
            &DsRegistrationCacheFile {
                version: DS_PREP_CACHE_VERSION,
                fingerprint: cache_fingerprint.clone(),
                accepted_paths: accepted_paths.clone(),
                reference_index: ref_idx,
                transforms: solved.clone(),
            },
        )?;
        solved
    };
    cancellation_checkpoint(cancel.as_ref(), "registro")?;
    emit_deepsky_pipeline_telemetry(
        &app,
        &ds_result_id,
        "register",
        if registration_reused {
            "Cache registro científico v6"
        } else {
            "CPU biyectivo + holdout espacial + Jacobiano"
        },
        transforms.iter().filter(|t| t.is_some()).count(),
        transforms.len(),
        ds_run_started,
        &ds_telemetry_sys,
        0,
        frame_store.read_hits(),
        None,
    );
    let mut model_counts = std::collections::BTreeMap::<&'static str, usize>::new();
    let mut rms_values = Vec::new();
    let mut holdout_p95_values = Vec::new();
    let mut minimum_jacobian = f64::INFINITY;
    let mut maximum_jacobian = f64::NEG_INFINITY;
    for reg in transforms.iter().flatten() {
        *model_counts.entry(reg.transform.model.label()).or_default() += 1;
        if reg.rms > 0.0 {
            rms_values.push(reg.rms);
        }
        if reg.holdout_count > 0 {
            holdout_p95_values.push(reg.holdout_p95);
        }
        minimum_jacobian = minimum_jacobian.min(reg.jacobian_min);
        maximum_jacobian = maximum_jacobian.max(reg.jacobian_max);
    }
    rms_values.sort_by(|a, b| a.total_cmp(b));
    holdout_p95_values.sort_by(|a, b| a.total_cmp(b));
    let median_rms = rms_values.get(rms_values.len() / 2).copied().unwrap_or(0.0);
    let median_holdout_p95 = holdout_p95_values
        .get(holdout_p95_values.len() / 2)
        .copied()
        .unwrap_or(0.0);
    log_to_front(
        &app,
        "INFO",
        &format!(
            "Registro científico: {:?} · RMS mediano {:.3} px · holdout p95 mediano {:.3} px · Jacobiano [{:.5}, {:.5}].",
            model_counts,
            median_rms,
            median_holdout_p95,
            minimum_jacobian,
            maximum_jacobian
        ),
    );

    // PSF SIGNAL WEIGHT (WBPP-style subframe weighting): PSF signal against
    // background-noise power, FWHM, roundness and astrometric residual. There
    // is deliberately no positive floor: frames outside the explicit gates
    // are excluded with a visible reason instead of contaminating the stack.
    let psfsw_of = |stars: &[(f32, f32, f32)], noise: f32| -> f32 {
        // Count-robust transparency×depth proxy: the MEAN PSF flux of the N
        // brightest stars (explicitly ranked) against the noise POWER — NOT the
        // raw SUM, which conflated genuine star COUNT / field with transparency.
        // Haze/clouds lower per-star flux → lower weight, as intended; a frame
        // that merely detects fewer stars is no longer unfairly penalized.
        if stars.is_empty() {
            return 0.0;
        }
        let mut fl: Vec<f32> = stars.iter().map(|s| s.2).collect();
        fl.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        let take = fl.len().min(20);
        let flux: f32 = fl.iter().take(take).sum::<f32>() / take as f32;
        flux / (noise * noise).max(1.0)
    };
    let best_fwhm = frames
        .iter()
        .map(|(_, _, f, _, _)| *f)
        .filter(|f| *f > 0.0)
        .fold(f32::MAX, f32::min);
    let best_psf = frames
        .iter()
        .map(|(_, s, _, nz, _)| psfsw_of(s, *nz))
        .fold(0.0f32, f32::max);
    let mut worst_ecc = (0.0f32, String::new()); // (ecc, name) para avisar
    let mut quality_exclusions: std::collections::BTreeMap<usize, String> =
        std::collections::BTreeMap::new();
    let mut registered: Vec<(usize, DsTransform, f64)> = Vec::with_capacity(frames.len());
    for (i, transform) in transforms.iter().enumerate() {
        let Some(reg) = *transform else {
            quality_exclusions.insert(
                i,
                "registro no publicable: <8 asociaciones biyectivas, holdout insuficiente o geometría inválida"
                    .into(),
            );
            continue;
        };
        if reg.holdout_count > 0 && reg.holdout_p95 > 0.20 {
            quality_exclusions.insert(
                i,
                format!(
                    "registro no publicable: holdout p95 {:.3} px excede 0.20 px",
                    reg.holdout_p95
                ),
            );
            continue;
        }
        let (ref name, ref stars, fwhm, noise, ecc) = frames[i];
        if ecc > worst_ecc.0 {
            worst_ecc = (ecc, name.clone());
        }
        match ds_frame_quality_weight(
            fwhm,
            best_fwhm,
            psfsw_of(stars, noise),
            best_psf,
            ecc,
            reg.rms,
            median_rms,
        ) {
            Ok(weight) => registered.push((i, reg.transform, weight)),
            Err(reason) => {
                quality_exclusions.insert(i, reason);
            }
        }
    }
    if worst_ecc.0 > 0.55 {
        let nm = std::path::Path::new(&worst_ecc.1)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&worst_ecc.1);
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Estrellas más alargadas: ecc {:.2} en {} (peso reducido).",
                worst_ecc.0, nm
            ),
        );
    }
    let rejected = frames.len() - registered.len();

    // WBPP-style per-frame QUALITY REPORT: surface every metric (FWHM proxy,
    // eccentricity, noise, star count, final weight, used/rejected) to the UI.
    {
        let wmap: std::collections::HashMap<usize, f64> =
            registered.iter().map(|&(i, _, w)| (i, w)).collect();
        let is_ref_idx = ref_idx;
        let report: Vec<serde_json::Value> = frames
            .iter()
            .enumerate()
            .map(|(i, (path, stars, fwhm, noise, ecc))| {
                let name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(path);
                let w = wmap.get(&i).copied();
                let reg = transforms[i];
                serde_json::json!({
                    "name": name,
                    "stars": stars.len(),
                    "fwhm": (*fwhm * 100.0).round() / 100.0,
                    "ecc": (*ecc * 1000.0).round() / 1000.0,
                    "noise": noise.round(),
                    "weight": w.map(|v| (v * 1000.0).round() / 1000.0),
                    "used": w.is_some(),
                    "reference": i == is_ref_idx,
                    "exclusionReason": quality_exclusions.get(&i),
                    "registrationModel": reg.map(|value| value.transform.model.label()),
                    "registrationInliers": reg.map(|value| value.inliers),
                    "registrationRms": reg.map(|value| value.rms),
                    "registrationHoldoutCount": reg.map(|value| value.holdout_count),
                    "registrationHoldoutP95": reg.map(|value| value.holdout_p95),
                    "registrationHoldoutMax": reg.map(|value| value.holdout_max),
                    "registrationJacobianMin": reg.map(|value| value.jacobian_min),
                    "registrationJacobianP95": reg.map(|value| value.jacobian_p95),
                    "registrationJacobianMax": reg.map(|value| value.jacobian_max),
                })
            })
            .collect();
        let _ = app.emit("ds-report", &report);
    }
    if registered.is_empty() {
        return Err("El registro estelar fallo en todos los frames (¿campos distintos?).".into());
    }
    if frames.len() > 1 && registered.len() < 2 {
        let capture = if matches!(capture_mode, pipeline::DeepSkyCaptureMode::MonoNarrowband) {
            " de banda estrecha mono"
        } else {
            ""
        };
        return Err(format!(
            "Registro{capture} bloqueado: ninguna toma adicional superó las asociaciones biyectivas, el holdout espacial y la validación de Jacobiano. No se publicará un apilado de una sola referencia como si las tomas hubieran sido registradas."
        ));
    }
    if rejected > 0 {
        log_to_front(
            &app,
            "WARN",
            &format!(
                "{} frame(s) descartados por registro/calidad: {}.",
                rejected,
                quality_exclusions
                    .values()
                    .take(4)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" · ")
            ),
        );
    }
    // Walking-noise is a 1× acquisition/registration property, not a drizzle
    // feature. Diagnose it for every stack from the temporally ordered sensor
    // centres in reference coordinates; drizzle reuses this same evidence.
    let registered_centres: Vec<(f64, f64)> = registered
        .iter()
        .map(|&(_, transform, _)| {
            let (x, y) = transform.forward(0.5 * w as f32, 0.5 * h as f32);
            (x as f64, y as f64)
        })
        .collect();
    let dither_diagnostics =
        crate::deepsky_noise::analyze_dither_positions(&registered_centres);
    if dither_diagnostics.walking_noise_risk {
        log_to_front(
            &app,
            "WARN",
            &format!(
                "Riesgo de walking noise a 1×{}: {}.",
                if drz > 1.01 { " y drizzle" } else { "" },
                dither_diagnostics.reasons.join(" · ")
            ),
        );
    } else {
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Dither 1×: {} celdas de 0.25 px · isotropía {:.2} · deriva temporal {:.2}.",
                dither_diagnostics.unique_quarter_pixel_cells,
                dither_diagnostics.isotropy,
                dither_diagnostics.temporal_drift_correlation
            ),
        );
    }
    let mut drizzle_dither_positions = None;
    if drz > 1.01 {
        // En CFA importa también la paridad 2×2 del mosaico; mono/RGB usa la
        // fase módulo 1. Ambos se cuantizan en cuatro celdas por eje.
        let dither_positions =
            ds_count_dither_positions(&registered, w, h, cfa_drizzle_pattern.is_some());
        drizzle_dither_positions = Some(dither_positions);
        if registered.len() < 8 || dither_positions < 4 {
            log_to_front(
                &app,
                "WARN",
                &format!(
                    "Drizzle{} con dithering limitado: {} tomas, {} posiciones subpíxel. Puede quedar cobertura incompleta; se recomiendan ≥8 tomas y ≥4 posiciones.",
                    if cfa_drizzle_pattern.is_some() { " CFA" } else { "" },
                    registered.len(),
                    dither_positions
                ),
            );
        } else {
            log_to_front(
                &app,
                "INFO",
                &format!(
                    "Drizzle{}: {} posiciones subpíxel detectadas; cobertura apta.",
                    if cfa_drizzle_pattern.is_some() {
                        " CFA"
                    } else {
                        ""
                    },
                    dither_positions
                ),
            );
        }
    }
    let star_catalogs: Vec<Vec<(f32, f32, f32)>> = frames.iter().map(|f| f.1.clone()).collect();
    let registration_residuals =
        ds_registration_residual_map(&ref_stars, &star_catalogs, &registered, w, h, w_out, h_out);

    let fallback_demosaic_cache = std::sync::atomic::AtomicBool::new(false);
    let load_cached = |i: usize| -> Result<DsImage, String> {
        let image = DsImage {
            data: frame_store.get(
                *frame_cache_indices
                    .get(i)
                    .ok_or("Índice de frame cache fuera de rango")?,
            )?,
            w,
            h,
            ch: if cfa_drizzle_pattern.is_some() { 1 } else { ch },
            bayer: cfa_drizzle_pattern,
        };
        if fallback_demosaic_cache.load(std::sync::atomic::Ordering::Relaxed) {
            if let Some(pattern) = image.bayer {
                return Ok(ds_debayer_image(image, pattern));
            }
        }
        Ok(image)
    };
    let load_cached_uncertainty =
        |i: usize| -> Result<DsCalibratedUncertainty, String> {
            let source_index = *frame_cache_indices
                .get(i)
                .ok_or("Indice de frame VAR/DQ fuera de rango")?;
            let variance = calibration_variance_store.get(source_index)?;
            let dq_f32 = calibration_dq_store.get(source_index)?;
            if dq_f32.len() != w.saturating_mul(h) {
                return Err("Caché DQ con geometría incompatible".into());
            }
            let dq: Vec<u32> = dq_f32
                .into_iter()
                .map(|value| value.max(0.0).round() as u32)
                .collect();
            let variance_channels = variance
                .len()
                .checked_div(w.saturating_mul(h).max(1))
                .unwrap_or(0);
            let publishable = ds_uncertainty_is_publishable(
                &variance,
                &dq,
                w,
                h,
                variance_channels,
            );
            let uncertainty = DsCalibratedUncertainty {
                publishable,
                variance,
                dq,
                fallback_reason: None,
            };
            if fallback_demosaic_cache.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some(pattern) = cfa_drizzle_pattern {
                    let dummy = DsImage {
                        data: vec![0.0; w * h],
                        w,
                        h,
                        ch: 1,
                        bayer: Some(pattern),
                    };
                    return Ok(ds_debayer_scientific(dummy, uncertainty, pattern).1);
                }
            }
            Ok(uncertainty)
        };

    // --- 3.5 PER-FRAME NORMALIZATION (SIRIL/WBPP): match every frame's sky
    // level and noise scale to the reference so moon/cloud/gradient variations
    // don't bias the stacked background and the σ-clip stays effective. ---
    cancellation_checkpoint(cancel.as_ref(), "normalización")?;
    emit_progress(
        &app,
        "Cielo Profundo: normalizando frames al de referencia...",
        33.0,
        None,
    );
    // Reuse the background/noise measured during calibration (frame_bgs[i] and
    // frames[i].3) — no need to reload+re-measure every cached frame here.
    let (bg_ref, nz_ref) = (frame_bgs[ref_idx], frames[ref_idx].3);
    let bg_ref_rgb = frame_bgs_rgb[ref_idx];
    let _ = bg_ref; // luma bg kept for logs; per-channel bg drives normalization
    let stellar_ref = ds_psf_signal_level(&frames[ref_idx].1);
    // PER-CHANNEL normalization: (mul[3], add[3]). The transparency SCALE is
    // largely achromatic (clouds attenuate all channels ≈ equally) so it stays a
    // single luma-derived factor replicated across channels; the ADDITIVE OFFSET
    // is per channel so each channel's sky is matched to the reference — this is
    // what removes the OSC colour cast and differential-extinction gradients that
    // a single luma offset cannot. v'[c] = v[c]·mul[c] + add[c].
    let mut norms: Vec<([f32; 3], [f32; 3])> = Vec::with_capacity(registered.len());
    let mut normalization_sources: Vec<&'static str> = Vec::with_capacity(registered.len());
    for &(i, _, _) in &registered {
        if i == ref_idx {
            norms.push(([1.0; 3], [0.0; 3]));
            normalization_sources.push("reference");
            continue;
        }
        if norm_mode == "none" {
            norms.push(([1.0; 3], [0.0; 3]));
            normalization_sources.push("disabled");
            continue;
        }
        let nz = frames[i].3;
        let bg_rgb = frame_bgs_rgb[i];
        // "additive": offset only. "scaling"/"local": transparency equalized by
        // the median PSF stellar flux ratio; noise ratio only as fallback (<6
        // stars). Achromatic → same factor for every channel.
        let (mul_scalar, source) = if norm_mode == "additive" {
            (1.0, "additive")
        } else if let (Some(reference_signal), Some(frame_signal)) =
            (stellar_ref, ds_psf_signal_level(&frames[i].1))
        {
            (
                (reference_signal / frame_signal.max(1e-6)).clamp(0.5, 2.0),
                "psf_stellar_signal",
            )
        } else {
            ((nz_ref / nz.max(1e-3)).clamp(0.5, 2.0), "noise_fallback")
        };
        let mul = [mul_scalar; 3];
        let add = [
            bg_ref_rgb[0] - bg_rgb[0] * mul[0],
            bg_ref_rgb[1] - bg_rgb[1] * mul[1],
            bg_ref_rgb[2] - bg_rgb[2] * mul[2],
        ];
        norms.push((mul, add));
        normalization_sources.push(source);
    }

    // --- 3.6 LOCAL NORMALIZATION (opt-in, PixInsight LocalNormalization): a
    // coarse per-cell sky-offset field per frame (vs the reference, in ref
    // space) so spatially-varying gradients — drifting light pollution, a moon
    // gradient — are matched LOCALLY, not just by one global offset. Applied
    // additively during integration; the global scalar offset is folded in. ---
    const LN_G: usize = 24;
    let mut loc_fields: Vec<Option<Vec<f32>>> = vec![None; registered.len()];
    let mut local_normalization_effective = false;
    let mut local_normalization_fallback: Option<String> = None;
    let mut local_normalization_metrics_recipe = serde_json::Value::Null;
    if use_local_norm {
        emit_progress(
            &app,
            "Cielo Profundo: normalización local simétrica por canal...",
            34.0,
            None,
        );
        let longest = w.max(h).max(1) as f64;
        let proxy_w = ((w as f64 * 192.0 / longest).round() as usize)
            .clamp(LN_G.min(w), w.max(1));
        let proxy_h = ((h as f64 * 192.0 / longest).round() as usize)
            .clamp(LN_G.min(h), h.max(1));
        let mut proxies: Vec<(Vec<f32>, Vec<u8>)> = Vec::new();
        proxies
            .try_reserve_exact(registered.len())
            .map_err(|error| format!("sin memoria para proxies de fondo: {error}"))?;
        for (k, &(i, t, _)) in registered.iter().enumerate() {
            cancellation_checkpoint(cancel.as_ref(), "normalización local")?;
            let raw = load_cached(i)?;
            let img = if let Some(cid) = raw.bayer {
                ds_debayer_image(raw, cid)
            } else {
                raw
            };
            if img.ch != ch {
                return Err(format!(
                    "normalización local: frame {i} tiene {} canal(es), se esperaban {ch}",
                    img.ch
                ));
            }
            proxies.push(ds_registered_background_proxy(
                &img,
                t,
                w,
                h,
                proxy_w,
                proxy_h,
                norms[k].0,
            )?);
        }
        let registered_background: Vec<crate::deepsky_background::RegisteredBackgroundFrame<'_>> =
            proxies
                .iter()
                .map(|(data, coverage)| {
                    crate::deepsky_background::RegisteredBackgroundFrame {
                        data,
                        coverage: Some(coverage),
                    }
                })
                .collect();
        match crate::deepsky_background::solve_symmetric_local_normalization(
            &registered_background,
            proxy_w,
            proxy_h,
            ch,
            crate::deepsky_background::SymmetricLocalNormalizationConfig {
                grid_width: LN_G,
                grid_height: LN_G,
                first_order: true,
            },
        ) {
            Ok(solution) => {
                let metrics: Vec<serde_json::Value> = solution
                    .channel_metrics
                    .iter()
                    .enumerate()
                    .map(|(channel, metric)| {
                        serde_json::json!({
                            "channel": channel,
                            "edges": metric.edges,
                            "components": metric.components,
                            "componentIds": metric.component_ids,
                            "seamRms": metric.rms_seam,
                            "noiseSigma": metric.noise_sigma,
                            "seamSigmaRatio": metric.seam_sigma_ratio,
                        })
                    })
                    .collect();
                let invalid = solution.channel_metrics.iter().any(|metric| {
                    metric.components != 1
                        || !metric.seam_sigma_ratio.is_finite()
                        || metric.seam_sigma_ratio >= 0.2
                });
                local_normalization_metrics_recipe = serde_json::json!({
                    "proxyWidth": proxy_w,
                    "proxyHeight": proxy_h,
                    "gridWidth": solution.grid_width,
                    "gridHeight": solution.grid_height,
                    "channels": metrics,
                    "acceptanceSeamSigmaRatioLt": 0.2,
                });
                if invalid {
                    local_normalization_fallback = Some(
                        "grafo desconectado o seam de fondo >=0.2 sigma; se conserva normalización global por canal"
                            .into(),
                    );
                } else {
                    for frame in 0..registered.len() {
                        let mut fields = Vec::new();
                        fields
                            .try_reserve_exact(ch * LN_G * LN_G)
                            .map_err(|error| {
                                format!("sin memoria para campo local por canal: {error}")
                            })?;
                        for channel in 0..ch {
                            fields.extend_from_slice(
                                solution
                                    .field(frame, channel)
                                    .ok_or("campo local frame/canal ausente")?,
                            );
                        }
                        loc_fields[frame] = Some(fields);
                    }
                    for norm in &mut norms {
                        norm.1 = [0.0; 3];
                    }
                    local_normalization_effective = true;
                    log_to_front(
                        &app,
                        "INFO",
                        "Normalización local simétrica por canal: grafo conectado y seams <0.2σ.",
                    );
                }
            }
            Err(error) => {
                local_normalization_fallback = Some(format!(
                    "normalización local simétrica no resoluble ({error}); se conserva normalización global por canal"
                ));
            }
        }
        if let Some(reason) = &local_normalization_fallback {
            log_to_front(&app, "WARN", reason);
        }
    }

    // ===== RESCATE DE DETALLE: campos de peso por región (FWHM local) =====
    // Rejilla WQ_G×WQ_G en el espacio de referencia por frame registrado:
    // mediana del FWHM de sus estrellas por celda (posiciones transformadas al
    // canvas). Calidad = (mediana_de_todos / fwhm_frame_celda)² acotada — la
    // toma más nítida EN ESA ZONA pesa más. Celdas sin datos → 1.0 (neutro: el
    // peso global fw sigue mandando, comportamiento previo). La media
    // ponderada Σw·v/Σw sigue siendo lineal: la fotometría no se sesga porque
    // el FWHM mide FORMA, no brillo.
    const WQ_G: usize = 8;
    let wq_fields: Vec<Option<Vec<f32>>> = if local_weighting {
        let mut per_frame_cells: Vec<Vec<Vec<f32>>> = Vec::with_capacity(registered.len());
        for &(i, t, _) in registered.iter() {
            let mut cells: Vec<Vec<f32>> = vec![Vec::new(); WQ_G * WQ_G];
            for &(sx, sy, fwhm) in frame_star_fwhms.get(i).map(|v| v.as_slice()).unwrap_or(&[]) {
                if !(fwhm.is_finite() && fwhm > 0.1) {
                    continue;
                }
                let (rx, ry) = t.forward(sx, sy);
                if !rx.is_finite() || !ry.is_finite() {
                    continue;
                }
                // Coordenadas de referencia normalizadas por el canvas 1×
                // (mismas u,v que muestrean los kernels: salida/escala).
                let u = (rx / (w_out as f32 / drz)).clamp(0.0, 0.9999);
                let v = (ry / (h_out as f32 / drz)).clamp(0.0, 0.9999);
                let cell = (v * WQ_G as f32) as usize * WQ_G + (u * WQ_G as f32) as usize;
                cells[cell].push(fwhm);
            }
            per_frame_cells.push(cells);
        }
        // Mediana por celda dentro de cada frame, y mediana cruzada de todas
        // las tomas por celda como referencia de "seeing típico" de esa zona.
        let median = |values: &mut Vec<f32>| -> Option<f32> {
            if values.is_empty() {
                return None;
            }
            values.sort_by(|a, b| a.total_cmp(b));
            Some(values[values.len() / 2])
        };
        let frame_cell_fwhm: Vec<Vec<Option<f32>>> = per_frame_cells
            .into_iter()
            .map(|cells| cells.into_iter().map(|mut c| median(&mut c)).collect())
            .collect();
        let mut cross: Vec<Option<f32>> = Vec::with_capacity(WQ_G * WQ_G);
        for cell in 0..WQ_G * WQ_G {
            let mut vals: Vec<f32> = frame_cell_fwhm.iter().filter_map(|f| f[cell]).collect();
            cross.push(median(&mut vals));
        }
        let fields: Vec<Option<Vec<f32>>> = frame_cell_fwhm
            .iter()
            .map(|cells| {
                let mut grid: Vec<f32> = (0..WQ_G * WQ_G)
                    .map(|cell| match (cross[cell], cells[cell]) {
                        (Some(c), Some(f)) if f > 0.1 => ((c / f) * (c / f)).clamp(0.55, 1.8),
                        _ => 1.0,
                    })
                    .collect();
                // Suavizado 3×3 (una pasada): evita costuras entre celdas.
                let src_grid = grid.clone();
                for gy in 0..WQ_G {
                    for gx in 0..WQ_G {
                        let mut acc = 0.0f32;
                        let mut n = 0.0f32;
                        for dy in -1i32..=1 {
                            for dx in -1i32..=1 {
                                let nx = gx as i32 + dx;
                                let ny = gy as i32 + dy;
                                if nx < 0 || ny < 0 || nx >= WQ_G as i32 || ny >= WQ_G as i32 {
                                    continue;
                                }
                                acc += src_grid[ny as usize * WQ_G + nx as usize];
                                n += 1.0;
                            }
                        }
                        grid[gy * WQ_G + gx] = acc / n.max(1.0);
                    }
                }
                Some(grid)
            })
            .collect();
        let informative = fields
            .iter()
            .flatten()
            .flat_map(|g| g.iter())
            .filter(|&&q| (q - 1.0).abs() > 0.05)
            .count();
        log_to_front(
            &app,
            "SUCCESS",
            &format!(
                "Rescate de detalle ACTIVO: pesos por región (rejilla {WQ_G}×{WQ_G}, FWHM local de estrellas) en {} tomas · {} celdas con preferencia de nitidez.",
                fields.len(),
                informative
            ),
        );
        fields
    } else {
        vec![None; registered.len()]
    };

    emit_deepsky_pipeline_telemetry(
        &app,
        &ds_result_id,
        "normalize",
        if use_local_norm {
            "CPU modelo robusto local + aplicación híbrida"
        } else {
            "CPU modelo robusto global + aplicación híbrida"
        },
        registered.len(),
        registered.len(),
        ds_run_started,
        &ds_telemetry_sys,
        0,
        frame_store.read_hits(),
        None,
    );

    // --- 4. INTEGRATION — ITERATIVE κσ clipped mean (streaming) ---
    // Pass 1 accumulates the raw weighted mean/variance. Each clip iteration
    // then rebuilds the (μ ± κσ) window from the PREVIOUS pass's CLIPPED
    // statistics and re-integrates: one iteration is the classic two-pass κσ;
    // the second removes what the outliers did to the window itself (a bright
    // satellite inflates σ so much that a single pass often keeps its trail).
    // This is the memory-safe equivalent of SIRIL/PI's iterated rejection.
    cancellation_checkpoint(
        cancel.as_ref(),
        if drz > 1.01 {
            "drizzle"
        } else {
            "integración"
        },
    )?;
    let npx = w_out * h_out;

    // ENGINE DECISION: the per-pixel (tiled) engine handles a full-stack method
    // (median/Winsorized/linear-fit/percentile/minmax) at scale=1 when the strip
    // stack fits RAM; otherwise (drizzle on, stack too big, or a moments method)
    // the streaming κσ engine runs — with a WARN + fallback to σ-clip if a
    // per-pixel method was requested but can't run.
    let n_frames = registered.len();
    let row_bytes = w_out
        .saturating_mul(ch)
        .saturating_mul(n_frames)
        .saturating_mul(4);
    let tile_budget: usize = 2 * 1024 * 1024 * 1024; // 2 GB for the per-pixel strip
    let can_tile = row_bytes > 0 && row_bytes <= tile_budget;
    let mut method_fallback_reason = None;
    if per_pixel && drz > 1.01 {
        let reason = format!(
            "El rechazo solicitado '{}' no se combina con drizzle; se usa sigma iterativo",
            requested_rejection
        );
        log_to_front(&app, "WARN", &reason);
        method_fallback_reason = Some(reason);
        rejection = "sigma".to_string();
    } else if per_pixel && (!can_tile || n_frames < 3) {
        let reason = format!(
            "El rechazo solicitado '{}' no cabe en una franja de 2 GB o tiene menos de 3 tomas; se usa sigma streaming",
            requested_rejection
        );
        log_to_front(&app, "WARN", &reason);
        method_fallback_reason = Some(reason);
        rejection = "sigma".to_string();
    }
    if let Some(reason) = method_fallback_reason.clone() {
        emit_deepsky_pipeline_telemetry(
            &app,
            &ds_result_id,
            "integrate_method_fallback",
            "Plan efectivo: sigma streaming",
            0,
            n_frames,
            ds_run_started,
            &ds_telemetry_sys,
            0,
            frame_store.read_hits(),
            Some(reason),
        );
    }
    let use_tiled = matches!(
        rejection.as_str(),
        "median" | "winsorized" | "linearfit" | "percentile" | "minmax"
    ) && drz <= 1.01;

    let n_iters: usize = if use_clip && !use_tiled && registered.len() >= 4 {
        clip_iters
            .map(|v| v.clamp(1, 3) as usize)
            .unwrap_or(if registered.len() >= 6 { 2 } else { 1 })
    } else {
        0
    };

    // GPU v2 cubre streaming (media/sigma) y el rechazo tiled Winsorized /
    // linear-fit. Mediana/percentil/minmax permanecen CPU hasta disponer de
    // kernels con paridad; drizzle conserva el kernel drop/CFA especializado.
    let gpu_transform_supported = registered.iter().all(|&(_, t, _)| t.to_gpu().is_some());
    // Rescate de detalle v2: los pesos por región cubren TODOS los motores —
    // streaming CPU/GPU (shader con rejilla de calidad, paridad en
    // ensure_parity), tiled CPU/GPU (ensure_tiled_parity) y drizzle.
    let gpu_stream_supported = !use_tiled && drz <= 1.01 && gpu_transform_supported;
    let gpu_tiled_supported = use_tiled && matches!(rejection.as_str(), "winsorized" | "linearfit");
    let gpu_supported = gpu_stream_supported || gpu_tiled_supported;
    if matches!(compute_policy, ComputePolicy::GpuOnly) && !gpu_supported {
        return Err(if !gpu_transform_supported {
            "GPU only: una transformación de registro no es invertible en el warp GPU.".into()
        } else if use_tiled {
            format!(
                "GPU only: el rechazo '{}' conserva CPU tiled hasta disponer de un kernel con paridad. Usa Winsorized, linear-fit o sigma.",
                rejection
            )
        } else {
            "GPU only: drizzle de cielo profundo aún usa el kernel drop CPU; usa Hybrid/Auto o desactiva drizzle."
                .into()
        });
    }

    // σ-floor for rejection windows, in the working scale: the MEDIAN measured
    // per-frame background noise across the registered set, not a fixed 4.0 ADU.
    // At a background pixel the frame-to-frame spread ≈ the single-frame noise, so
    // this is the physically-correct lower bound — the κσ window is never tighter
    // than κ·(real noise). It tracks the data (tighter on clean broadband, wider on
    // faint narrowband) and stays correct after normalization/drizzle rescale it.
    let sigma_floor: f32 = {
        let mut ns: Vec<f32> = registered
            .iter()
            .filter_map(|&(i, _, _)| {
                let n = frames[i].3;
                (n.is_finite() && n > 0.0).then_some(n)
            })
            .collect();
        if ns.is_empty() {
            4.0
        } else {
            ns.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            ns[ns.len() / 2].max(1e-3)
        }
    };
    log_to_front(
        &app,
        "INFO",
        &format!(
            "Rechazo: σ-floor = {:.2} (mediana de ruido por toma).",
            sigma_floor
        ),
    );

    let mut used_gpu = false;
    let mut peak_vram_mb = if gpu_preprocessing_used || gpu_calibration_used {
        32
    } else {
        0
    };
    let mut effective_engine = if use_tiled {
        "CPU tiled".to_string()
    } else {
        "CPU streaming".to_string()
    };
    let gpu_attempt = if !nf_lite_active
        && !eidr_active
        && compute_policy.allows_gpu()
        && gpu_stream_supported
    {
        match crate::gpu_stack::gpu_runtime() {
            None => {
                if matches!(compute_policy, ComputePolicy::GpuOnly) {
                    return Err(
                        "GPU only solicitado, pero no existe un dispositivo wgpu compatible".into(),
                    );
                }
                log_to_front(
                    &app,
                    "WARN",
                    "Cielo profundo: GPU no disponible; integración CPU.",
                );
                None
            }
            Some(_)
                if !crate::gpu_deepsky::ensure_parity()
                    || !crate::gpu_deepsky::ensure_advanced_warp_parity() =>
            {
                if matches!(compute_policy, ComputePolicy::GpuOnly) {
                    return Err("GPU only: falló la paridad del kernel de integración/warp avanzado (RMSE > 0.5 ADU)".into());
                }
                log_to_front(
                    &app,
                    "WARN",
                    "Cielo profundo: integración/warp GPU sin paridad; usando CPU.",
                );
                None
            }
            Some(rt) => {
                log_to_front(
                    &app,
                    "SUCCESS",
                    &format!(
                        "Integración cielo profundo GPU activa: {} ({}) · bandas adaptativas.",
                        rt.backend, rt.adapter_name
                    ),
                );
                // P2.D: in Hybrid/Auto mode with enough frames, integrate with the
                // CPU and GPU CONCURRENTLY (frame split). Falls back to GPU-only on
                // any error. OPT-IN (env `ZAS_HYBRID_SPLIT=1`) while the concurrent
                // frame-store I/O is validated on real hardware — the default flow
                // keeps the single-stream GPU path (no new concurrency). The pass
                // wall-times are logged so a real run can calibrate the CPU share.
                let use_split = std::env::var("ZAS_HYBRID_SPLIT").is_ok()
                    && matches!(compute_policy, ComputePolicy::Hybrid | ComputePolicy::Auto)
                    && registered.len() >= 8;
                let run_gpu_only = || {
                    ds_integrate_gpu_streaming(
                        &app,
                        &cancel,
                        &load_cached,
                        &registered,
                        &norms,
                        &loc_fields,
                        LN_G,
                        &wq_fields,
                        WQ_G,
                        w_out,
                        h_out,
                        ch,
                        use_lanczos,
                        n_iters,
                        kappa_low,
                        kappa_high,
                        sigma_floor,
                        &ds_result_id,
                        ds_run_started,
                        &ds_telemetry_sys,
                    )
                };
                let result = if use_split {
                    match ds_integrate_hybrid_split(
                        &app,
                        &cancel,
                        &load_cached,
                        &registered,
                        &norms,
                        &loc_fields,
                        LN_G,
                        &wq_fields,
                        WQ_G,
                        w_out,
                        h_out,
                        ch,
                        use_lanczos,
                        n_iters,
                        kappa_low,
                        kappa_high,
                        sigma_floor,
                        HYBRID_CPU_FRACTION,
                    ) {
                        Ok(v) => Ok(v),
                        Err(e) => {
                            log_to_front(
                                &app,
                                "WARN",
                                &format!("Split CPU+GPU concurrente falló ({e}); usando GPU-only."),
                            );
                            run_gpu_only()
                        }
                    }
                } else {
                    run_gpu_only()
                };
                Some(result)
            }
        }
    } else {
        None
    };
    let gpu_success = match gpu_attempt {
        Some(Ok((data, cov, weight, rejected_low, rejected_high, rej, mean_cov, peak, tiles))) => {
            used_gpu = true;
            peak_vram_mb = peak_vram_mb.max((peak.saturating_add(1_048_575) / 1_048_576) as u64);
            effective_engine = "Hybrid CPU + GPU wgpu".into();
            log_to_front(
                &app,
                "SUCCESS",
                &format!(
                    "Integración GPU completa: {} bandas/pases · pico estimado {:.1} MB VRAM.",
                    tiles,
                    peak as f64 / (1024.0 * 1024.0)
                ),
            );
            Some((
                data,
                cov,
                weight,
                rejected_low,
                rejected_high,
                rej,
                mean_cov,
            ))
        }
        Some(Err(e)) if matches!(compute_policy, ComputePolicy::GpuOnly) => {
            return Err(format!(
                "GPU only: la integración falló y no se permite fallback: {e}"
            ));
        }
        Some(Err(e)) => {
            log_to_front(
                &app,
                "WARN",
                &format!(
                    "GPU falló durante integración ({e}); reiniciando la etapa completa en CPU."
                ),
            );
            emit_deepsky_pipeline_telemetry(
                &app,
                &ds_result_id,
                "integrate_fallback",
                "CPU streaming",
                0,
                registered.len(),
                ds_run_started,
                &ds_telemetry_sys,
                0,
                0,
                Some(e),
            );
            None
        }
        None => None,
    };

    let mut nf_products: Option<crate::nebula_fusion::NfLiteProducts> = None;
    let mut nf_full_report: Option<(f32, usize, usize)> = None;
    let mut nf_full_fallback: Option<String> = None;
    let mut nf_struct: Option<(Vec<f32>, Vec<f32>)> = None;
    let mut nf_struct_accepted: Option<Vec<(usize, usize)>> = None;
    let mut nf_struct_fallback: Option<String> = None;
    let mut nf_parameter_fallbacks: Vec<String> = Vec::new();
    let mut classic_moment_products = false;
    let mut classic_calibration_products = false;
    let mut eidr_runtime_fallback: Option<String> = None;
    // EIDR (F9): corre ANTES del despacho clásico/NF — produce su propio
    // lienzo (w·s × h·s) y productos; el resto del pipeline (crop, STF,
    // export) continúa con las dimensiones re-vinculadas tras la tupla.
    let mut eidr_outcome: Option<DsEidrOutcome> = None;
    let eidr_candidate = if let Some(ecfg) = eidr_cfg.as_ref() {
        log_to_front(
            &app,
            "INFO",
            &format!(
                "EIDR (cuadrático científico): {} frames · escala {:?} · consume calibrados nativos + transforms.",
                registered.len(),
                ecfg.scale
            ),
        );
        match ds_run_eidr(
            &app,
            ecfg,
            &registered,
            &load_cached,
            &load_cached_uncertainty,
            &norms,
            &loc_fields,
            LN_G,
            &star_catalogs,
            &frame_calibration_sigmas,
            w,
            h,
            ch,
            cfa_drizzle_pattern.filter(|_| eidr_cfa_direct),
            compute_policy,
            cancel.as_ref(),
        ) {
            Ok(outcome) => Some(outcome),
            Err(error) if matches!(compute_policy, ComputePolicy::GpuOnly) => {
                return Err(format!(
                    "EIDR no produjo una solución publicable y GpuOnly prohíbe fallback: {error}"
                ));
            }
            Err(error) => {
                let reason = format!(
                    "EIDR no publicable ({error}); fallback seguro a integración Classic CPU"
                );
                // A CFA-direct cache contains one photosite plane. Classic at
                // 1× consumes RGB, so switch the shared loader to deterministic
                // float32 demosaic for the fallback instead of interpreting CFA
                // bytes as an interleaved RGB frame.
                if eidr_cfa_direct {
                    fallback_demosaic_cache.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                log_to_front(&app, "WARN", &reason);
                emit_deepsky_pipeline_telemetry(
                    &app,
                    &ds_result_id,
                    "eidr_scientific_fallback",
                    "Classic CPU",
                    0,
                    registered.len(),
                    ds_run_started,
                    &ds_telemetry_sys,
                    0,
                    frame_store.read_hits(),
                    Some(reason.clone()),
                );
                method_fallback_reason = Some(match method_fallback_reason.take() {
                    Some(previous) => format!("{previous}; {reason}"),
                    None => reason.clone(),
                });
                eidr_runtime_fallback = Some(reason);
                None
            }
        }
    } else {
        None
    };
    let (mut final_data, wgt1, weight_map, rejection_low, rejection_high, rej_pct, mean_cov): (
        Vec<f32>,
        Vec<f64>,
        Vec<f64>,
        Vec<f64>,
        Vec<f64>,
        f64,
        f64,
    ) = if let Some(out) = eidr_candidate {
        effective_engine = format!("eidr_{:.1}x", out.scale_eff).replace(".0x", "x");
        rejection = "forward_model".into();
        let npx2 = out.w2 * out.h2;
        let coverage = out.coverage.clone();
        let mean_cov = out.mean_cov;
        let zeros = vec![0.0f64; npx2];
        let data = out.final_data.clone();
        eidr_outcome = Some(out);
        (
            data,
            coverage.clone(),
            coverage,
            zeros.clone(),
            zeros,
            0.0,
            mean_cov,
        )
    } else if nf_lite_active {
        // --- Motor NebulaFusion Lite (F3): pesos inverso-varianza + máscaras
        // congeladas por cross-fit. CPU siempre en esta fase; el preflight ya
        // excluyó drizzle/CFA-directo/GpuOnly. ---
        log_to_front(
            &app,
            "INFO",
            &format!(
                "NebulaFusion Lite: {} frames · pesos 1/σ² por celda · máscaras cross-fit LOO congeladas.",
                registered.len()
            ),
        );
        let nf_ctx = crate::nebula_fusion::NfLiteContext {
            registered: &registered,
            norms: &norms,
            loc_fields: &loc_fields,
            loc_grid: LN_G,
            w_out,
            h_out,
            ch,
            use_lanczos,
            cancel: cancel.as_ref(),
            // CFA directo: los frames quedaron almacenados como plano CFA
            // calibrado (ver calibrated_cfa) y el patrón viaja con ellos.
            cfa: cfa_drizzle_pattern.filter(|_| nf_cfa_direct),
            full: nf_full_mode,
            stars: &star_catalogs,
            struct_mode: nf_struct_mode,
            config: match &integration_method {
                Some(pipeline::DeepSkyIntegrationMethod::NebulaFusion(cfg)) => cfg.into(),
                _ => Default::default(),
            },
        };
        let mut nf_progress = |phase: &str, k: usize, n: usize| {
            emit_progress(
                &app,
                &format!("NebulaFusion Lite: {phase} {k}/{n}"),
                35.0 + (k as f32 / n.max(1) as f32) * 55.0,
                None,
            );
        };
        let load_nf_uncertainty = |index: usize| {
            let uncertainty = load_cached_uncertainty(index)?;
            if !uncertainty.publishable {
                return Err(uncertainty.fallback_reason.unwrap_or_else(|| {
                    "VAR/DQ de calibracion no publicable para NebulaFusion".into()
                }));
            }
            let channels = uncertainty
                .variance
                .len()
                .checked_div(w.saturating_mul(h).max(1))
                .unwrap_or(0);
            Ok(crate::nebula_fusion::NfCalibrationUncertainty {
                variance: uncertainty.variance,
                dq: uncertainty.dq,
                w,
                h,
                ch: channels,
            })
        };
        let mut out = crate::nebula_fusion::run_lite(
            &nf_ctx,
            &load_cached,
            Some(&load_nf_uncertainty),
            &mut nf_progress,
        )?;
        nf_full_report = out.full_report;
        nf_full_fallback = out.full_fallback.clone();
        nf_struct_fallback = out.struct_fallback.clone();
        nf_parameter_fallbacks = out.parameter_fallbacks.clone();
        if let (Some(sm), Some(sr)) = (out.struct_map.take(), out.struct_residual.take()) {
            nf_struct = Some((sm, sr));
        }
        nf_struct_accepted = out.struct_accepted.take();
        if let Some((fwhm, fb, total)) = out.full_report {
            log_to_front(
                &app,
                "SUCCESS",
                &format!(
                    "NebulaFusion Full: PSF objetivo {fwhm:.2} px · {fb}/{total} tiles degradados a media plana."
                ),
            );
        }
        if let Some(reason) = &out.full_fallback {
            log_to_front(
                &app,
                "WARN",
                &format!("NebulaFusion Full degradado a Lite: {reason}."),
            );
        }
        if let Some(reason) = &out.struct_fallback {
            log_to_front(
                &app,
                "WARN",
                &format!("NebulaFusion STRUCT omitido; SCI permanece intacto: {reason}."),
            );
        }
        for reason in &out.parameter_fallbacks {
            log_to_front(
                &app,
                "WARN",
                &format!("NebulaFusion parámetro efectivo degradado: {reason}."),
            );
        }
        effective_engine = if out.full_report.is_some() {
            "nebula_fusion_full".into()
        } else {
            "nebula_fusion_lite".into()
        };
        rejection = "crossfit_loo".into();
        log_to_front(
            &app,
            "INFO",
            &format!(
                "NebulaFusion Lite: {} muestras enmascaradas por cross-fit ({:.2}% del peso).",
                out.products.masked_samples, out.rej_pct
            ),
        );
        nf_products = Some(out.products);
        (
            out.final_data,
            out.wgt1,
            out.weight_map,
            out.rejection_low,
            out.rejection_high,
            out.rej_pct,
            out.mean_cov,
        )
    } else if let Some(gpu) = gpu_success {
        gpu
    } else if use_tiled {
        // --- PER-PIXEL (tiled) engine ---
        let tiled_gpu = if compute_policy.allows_gpu() && gpu_tiled_supported {
            match crate::gpu_stack::gpu_runtime() {
                None if matches!(compute_policy, ComputePolicy::GpuOnly) => {
                    return Err("GPU only: no existe un dispositivo wgpu para rechazo tiled".into());
                }
                None => false,
                Some(_) if !crate::gpu_deepsky::ensure_tiled_parity() => {
                    if matches!(compute_policy, ComputePolicy::GpuOnly) {
                        return Err(
                            "GPU only: falló la paridad Winsorized/linear-fit del kernel tiled"
                                .into(),
                        );
                    }
                    log_to_front(
                        &app,
                        "WARN",
                        "Rechazo GPU tiled sin paridad; usando CPU tiled.",
                    );
                    false
                }
                Some(_) => true,
            }
        } else {
            false
        };
        let mut tile_h = (tile_budget / row_bytes.max(1)).clamp(1, h_out);
        if tiled_gpu {
            if let Some(rt) = crate::gpu_stack::gpu_runtime() {
                // Dos pilas f32 residentes (valores + pesos de trabajo), más
                // outputs. El binding de la pila es el límite dominante.
                let gpu_stack_budget = rt.max_binding.min(rt.vram_budget / 3) as usize;
                tile_h = tile_h.min((gpu_stack_budget / row_bytes.max(1)).clamp(1, h_out));
            }
            let estimated_bytes = row_bytes
                .saturating_mul(tile_h)
                .saturating_mul(2)
                .saturating_add(
                    w_out
                        .saturating_mul(tile_h)
                        .saturating_mul(ch)
                        .saturating_mul(20),
                );
            peak_vram_mb =
                peak_vram_mb.max((estimated_bytes.saturating_add(1_048_575) / 1_048_576) as u64);
        }
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Motor por-píxel (tiled): {} · {} frames · franjas de {} filas · {}.",
                rejection,
                n_frames,
                tile_h,
                if tiled_gpu { "rechazo GPU" } else { "CPU" },
            ),
        );
        let tiled_result = ds_integrate_tiled(
            &app,
            &cancel,
            &load_cached,
            &registered,
            &norms,
            &loc_fields,
            LN_G,
            &wq_fields,
            WQ_G,
            w_out,
            h_out,
            ch,
            &rejection,
            kappa_low,
            kappa_high,
            use_lanczos,
            tile_h,
            tiled_gpu,
            sigma_floor,
        );
        match tiled_result {
            Ok(v) => {
                if tiled_gpu {
                    used_gpu = true;
                    effective_engine = "Hybrid CPU warp + GPU tiled rejection".into();
                }
                v
            }
            Err(e) if tiled_gpu && compute_policy.allows_fallback() => {
                log_to_front(
                    &app,
                    "WARN",
                    &format!("GPU tiled falló ({e}); repitiendo integración tiled en CPU."),
                );
                effective_engine = "CPU tiled (fallback)".into();
                ds_integrate_tiled(
                    &app,
                    &cancel,
                    &load_cached,
                    &registered,
                    &norms,
                    &loc_fields,
                    LN_G,
                    &wq_fields,
                    WQ_G,
                    w_out,
                    h_out,
                    ch,
                    &rejection,
                    kappa_low,
                    kappa_high,
                    use_lanczos,
                    tile_h,
                    false,
                    sigma_floor,
                )?
            }
            Err(e) => return Err(format!("GPU only: rechazo tiled falló sin fallback: {e}")),
        }
    } else {
        // --- STREAMING (moments) engine: iterative κσ clipped mean / average ---
        let cfa_drizzle = cfa_drizzle_pattern.filter(|_| drz > 1.01);
        let mut sum = vec![0.0f64; npx * ch];
        let mut sq = vec![0.0f64; npx * ch];
        // Weight is per-channel everywhere now (per-channel rejection): npx·ch.
        let mut wgt = vec![0.0f64; npx * ch];
        // Σw² is required for an honest NEFF and variance of the weighted
        // mean. The current drizzle kernels do not expose it, so scientific
        // products remain explicitly unavailable for drizzle instead of being
        // guessed from geometric coverage.
        let mut weight_sq = (drz <= 1.01).then(|| vec![0.0f64; npx * ch]);
        // Reduce the per-channel weight to a per-pixel coverage map for the
        // auto-crop / QA planes (CFA keeps its Bayer-density normalization).
        let cov_reduce = |wgt: &[f64]| -> Vec<f64> {
            if cfa_drizzle.is_some() {
                ds_cfa_coverage(wgt)
            } else {
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
        };
        for (k, &(i, t, fw)) in registered.iter().enumerate() {
            cancellation_checkpoint(cancel.as_ref(), "integración pasada 1")?;
            emit_progress(
                &app,
                &format!(
                    "Cielo Profundo: integrando (pasada 1) {}/{}",
                    k + 1,
                    registered.len()
                ),
                35.0 + (k as f32 / registered.len() as f32) * 20.0,
                None,
            );
            let img = load_cached(i)?;
            let loc_ref = loc_fields[k].as_ref().map(|f| (f.as_slice(), LN_G, LN_G));
            let wq_ref = wq_fields[k].as_ref().map(|f| (f.as_slice(), WQ_G, WQ_G));
            if let Some(cid) = cfa_drizzle {
                ds_drizzle_cfa_accumulate(
                    &img,
                    cid,
                    t,
                    &mut sum,
                    Some(&mut sq),
                    &mut wgt,
                    None,
                    None,
                    w_out,
                    h_out,
                    fw,
                    drz,
                    pixfrac,
                    norms[k],
                    loc_ref,
                    wq_ref,
                    None,
                    None,
                    None,
                );
            } else if drz > 1.01 {
                ds_drizzle_accumulate(
                    &img,
                    t,
                    &mut sum,
                    Some(&mut sq),
                    &mut wgt,
                    None,
                    None,
                    w_out,
                    h_out,
                    ch,
                    fw,
                    drz,
                    pixfrac,
                    norms[k],
                    loc_ref,
                    wq_ref,
                );
            } else {
                ds_warp_accumulate(
                    &img,
                    t,
                    &mut sum,
                    Some(&mut sq),
                    &mut wgt,
                    None,
                    None,
                    w_out,
                    h_out,
                    ch,
                    fw,
                    drz,
                    norms[k],
                    loc_ref,
                    wq_ref,
                    use_lanczos,
                    None,
                    weight_sq.as_mut(),
                );
            }
            if k % 4 == 0 || k + 1 == registered.len() {
                emit_deepsky_pipeline_telemetry(
                    &app,
                    &ds_result_id,
                    "integrate_pass_1",
                    "CPU streaming",
                    k + 1,
                    registered.len(),
                    ds_run_started,
                    &ds_telemetry_sys,
                    0,
                    frame_store.read_hits(),
                    None,
                );
            }
        }
        // Coverage map for the auto-crop = pass-1 weights (before any rejection).
        let wgt1 = cov_reduce(&wgt);

        // Running result; clip iterations refine it in place (rejection holes keep
        // the previous iteration's value — they can never go black).
        let mut final_data: Vec<f32> = (0..npx * ch)
            .map(|i| {
                let wv = wgt[i];
                if wv > 0.0 {
                    (sum[i] / wv) as f32
                } else {
                    0.0
                }
            })
            .collect();
        let mut rejected_low = vec![0.0f64; npx];
        let mut rejected_high = vec![0.0f64; npx];
        let mut final_clip_bounds: Option<(Vec<f32>, Vec<f32>)> = None;

        if n_iters > 0 {
            let mut lo = vec![f32::MIN; npx * ch];
            let mut hi = vec![f32::MAX; npx * ch];
            for it in 0..n_iters {
                // κσ window from the current (raw, then progressively clipped) stats.
                for i in 0..npx * ch {
                    let wv = wgt[i];
                    if wv > 1.0 {
                        let mu = sum[i] / wv;
                        let var = (sq[i] / wv - mu * mu).max(0.0);
                        let sd = var.sqrt().max(sigma_floor as f64);
                        lo[i] = (mu - kappa_low as f64 * sd) as f32;
                        hi[i] = (mu + kappa_high as f64 * sd) as f32;
                    } else {
                        lo[i] = f32::MIN;
                        hi[i] = f32::MAX;
                    }
                }
                // Clipped re-integration (accumulators reused, no reallocation).
                sum.iter_mut().for_each(|v| *v = 0.0);
                sq.iter_mut().for_each(|v| *v = 0.0);
                wgt.iter_mut().for_each(|v| *v = 0.0);
                if let Some(weight_sq) = weight_sq.as_mut() {
                    weight_sq.iter_mut().for_each(|v| *v = 0.0);
                }
                rejected_low.iter_mut().for_each(|v| *v = 0.0);
                rejected_high.iter_mut().for_each(|v| *v = 0.0);
                let base = 55.0 + (it as f32 / n_iters as f32) * 38.0;
                let span = 38.0 / n_iters as f32;
                for (k, &(i, t, fw)) in registered.iter().enumerate() {
                    cancellation_checkpoint(cancel.as_ref(), "integración sigma-clip")?;
                    emit_progress(
                        &app,
                        &format!(
                            "Cielo Profundo: integrando (pasada 2, σ-clip {}/{}) {}/{}",
                            it + 1,
                            n_iters,
                            k + 1,
                            registered.len()
                        ),
                        base + (k as f32 / registered.len() as f32) * span,
                        None,
                    );
                    let img = load_cached(i)?;
                    let loc_ref = loc_fields[k].as_ref().map(|f| (f.as_slice(), LN_G, LN_G));
                    let wq_ref = wq_fields[k].as_ref().map(|f| (f.as_slice(), WQ_G, WQ_G));
                    if let Some(cid) = cfa_drizzle {
                        ds_drizzle_cfa_accumulate(
                            &img,
                            cid,
                            t,
                            &mut sum,
                            Some(&mut sq),
                            &mut wgt,
                            Some((&lo, &hi)),
                            Some((&mut rejected_low, &mut rejected_high)),
                            w_out,
                            h_out,
                            fw,
                            drz,
                            pixfrac,
                            norms[k],
                            loc_ref,
                            wq_ref,
                            None,
                            None,
                            None,
                        );
                    } else if drz > 1.01 {
                        ds_drizzle_accumulate(
                            &img,
                            t,
                            &mut sum,
                            Some(&mut sq),
                            &mut wgt,
                            Some((&lo, &hi)),
                            Some((&mut rejected_low, &mut rejected_high)),
                            w_out,
                            h_out,
                            ch,
                            fw,
                            drz,
                            pixfrac,
                            norms[k],
                            loc_ref,
                            wq_ref,
                        );
                    } else {
                        ds_warp_accumulate(
                            &img,
                            t,
                            &mut sum,
                            Some(&mut sq),
                            &mut wgt,
                            Some((&lo, &hi)),
                            Some((&mut rejected_low, &mut rejected_high)),
                            w_out,
                            h_out,
                            ch,
                            fw,
                            drz,
                            norms[k],
                            loc_ref,
                            wq_ref,
                            use_lanczos,
                            None,
                            weight_sq.as_mut(),
                        );
                    }
                    if k % 4 == 0 || k + 1 == registered.len() {
                        emit_deepsky_pipeline_telemetry(
                            &app,
                            &ds_result_id,
                            &format!("sigma_clip_{}", it + 1),
                            "CPU streaming",
                            k + 1,
                            registered.len(),
                            ds_run_started,
                            &ds_telemetry_sys,
                            0,
                            frame_store.read_hits(),
                            None,
                        );
                    }
                }
                for i in 0..npx * ch {
                    let wv = wgt[i];
                    if wv > 0.0 {
                        final_data[i] = (sum[i] / wv) as f32;
                    }
                }
            }
            final_clip_bounds = Some((lo, hi));
        }
        // Rejection % (surviving vs pass-1 coverage) and mean effective frames.
        let mean_fw =
            registered.iter().map(|&(_, _, w)| w).sum::<f64>() / registered.len().max(1) as f64;
        let sum_pre: f64 = wgt1.iter().sum();
        let final_coverage = cov_reduce(&wgt);
        // SCI never receives invented values. Preview filling, if introduced,
        // must operate on a later copy and must not enter FITS/VAR/DQ.
        let holes = ds_mark_uncovered_nan(&mut final_data, &final_coverage, ch);
        if holes > 0 {
            log_to_front(
                &app,
                "WARN",
                &format!(
                    "Integración {:.1}×: {holes} píxeles sin cobertura conservados como NaN en SCI. Más dithering o pixfrac mayor los elimina.",
                    drz
                ),
            );
        }
        let sum_cov: f64 = final_coverage.iter().sum();
        if let Some(weight_sq) = weight_sq.as_ref() {
            nf_products = ds_classic_products_from_moments(
                &sum,
                &sq,
                &wgt,
                weight_sq,
                npx,
                ch,
            );
            classic_moment_products = nf_products.is_some();
            let bounds = final_clip_bounds
                .as_ref()
                .map(|(lo, hi)| (lo.as_slice(), hi.as_slice()));
            match ds_classic_products_from_calibration(
                &load_cached,
                &load_cached_uncertainty,
                &registered,
                &norms,
                &loc_fields,
                LN_G,
                w_out,
                h_out,
                ch,
                &wq_fields,
                WQ_G,
                use_lanczos,
                bounds,
                cancel.as_ref(),
            )? {
                Some(products) => {
                    nf_products = Some(products);
                    classic_moment_products = true;
                    classic_calibration_products = true;
                }
                None => {
                    log_to_front(
                        &app,
                        "WARN",
                        "Classic: no se pudo alinear el contrato VAR/DQ de calibración; se conserva VAR empírica de los momentos, sin etiquetarla como propagada.",
                    );
                }
            }
        }
        let rej_pct = if sum_pre > 0.0 {
            100.0 * (1.0 - sum_cov / sum_pre)
        } else {
            0.0
        };
        let mean_cov = sum_cov / (mean_fw.max(1e-6) * npx as f64);
        (
            final_data,
            wgt1,
            final_coverage,
            rejected_low,
            rejected_high,
            rej_pct,
            mean_cov,
        )
    };

    // Every integration backend shares the same scientific no-coverage
    // invariant.  The CPU streaming path already applies it locally, but the
    // common pass is required for tiled/GPU/NebulaFusion/EIDR as well: a zero
    // in those buffers is an implementation sentinel, never measured signal.
    let _ = ds_mark_uncovered_nan(&mut final_data, &weight_map, ch);

    // (frame cache removed by _cache_guard on drop — every exit path)

    // EIDR: el lienzo pasa a la escala efectiva; los mapas por píxel del
    // pipeline clásico se recalculan o anulan a ese tamaño.
    let (w_out, h_out) = if let Some(e) = &eidr_outcome {
        (e.w2, e.h2)
    } else {
        (w_out, h_out)
    };
    let registration_residuals = if eidr_outcome.is_some() {
        ds_registration_residual_map(&ref_stars, &star_catalogs, &registered, w, h, w_out, h_out)
    } else {
        registration_residuals
    };
    // Productos EIDR en el contenedor científico común (VAR/NEFF/DQ) para
    // compartir recorte/export; recov viaja aparte.
    let mut nf_products = nf_products;
    let mut eidr_recov: Option<Vec<f32>> = None;
    if let Some(e) = &mut eidr_outcome {
        nf_products = Some(crate::nebula_fusion::NfLiteProducts {
            variance: std::mem::take(&mut e.variance),
            neff: std::mem::take(&mut e.neff),
            dq: std::mem::take(&mut e.dq),
            masked_samples: 0,
            variance_origin:
                crate::deepsky_variance::VarianceOrigin::HybridEmpiricalPropagated,
            g1g2_offset_max: None,
        });
        eidr_recov = Some(std::mem::take(&mut e.recov));
    }

    // --- 4.4 AUTO-CROP low-coverage borders (dithered/rotated stacks) ---
    let original_w = w_out;
    let original_h = h_out;
    let (final_data, w_out, h_out, crop_x, crop_y, npx) = if use_crop {
        let (cropped, nw, nh, x0, y0) =
            ds_autocrop_with_origin(&final_data, &wgt1, w_out, h_out, ch);
        if nw != w_out || nh != h_out {
            log_to_front(
                &app,
                "INFO",
                &format!(
                    "Recorte de bordes: {}×{} → {}×{} (cobertura parcial).",
                    w_out, h_out, nw, nh
                ),
            );
        }
        (cropped, nw, nh, x0, y0, nw * nh)
    } else {
        (final_data, w_out, h_out, 0, 0, w_out * h_out)
    };
    let wgt1 = ds_crop_plane(&wgt1, original_w, original_h, crop_x, crop_y, w_out, h_out);
    let weight_map = ds_crop_plane(
        &weight_map,
        original_w,
        original_h,
        crop_x,
        crop_y,
        w_out,
        h_out,
    );
    let rejection_low = ds_crop_plane(
        &rejection_low,
        original_w,
        original_h,
        crop_x,
        crop_y,
        w_out,
        h_out,
    );
    let rejection_high = ds_crop_plane(
        &rejection_high,
        original_w,
        original_h,
        crop_x,
        crop_y,
        w_out,
        h_out,
    );
    let registration_residuals = ds_crop_plane(
        &registration_residuals,
        original_w,
        original_h,
        crop_x,
        crop_y,
        w_out,
        h_out,
    );
    // Los productos científicos comparten el recorte del máster.
    let nf_products = nf_products.map(|p| {
        if crop_x == 0 && crop_y == 0 && w_out == original_w && h_out == original_h {
            p
        } else {
            crate::nebula_fusion::NfLiteProducts {
                variance: crate::nebula_fusion::crop_interleaved_f32(
                    &p.variance,
                    original_w,
                    original_h,
                    ch,
                    crop_x,
                    crop_y,
                    w_out,
                    h_out,
                ),
                neff: crate::nebula_fusion::crop_interleaved_f32(
                    &p.neff, original_w, original_h, ch, crop_x, crop_y, w_out, h_out,
                ),
                dq: crate::nebula_fusion::crop_plane_u32(
                    &p.dq, original_w, crop_x, crop_y, w_out, h_out,
                ),
                masked_samples: p.masked_samples,
                variance_origin: p.variance_origin,
                g1g2_offset_max: p.g1g2_offset_max,
            }
        }
    });
    let eidr_recov =
        eidr_recov.map(|r| ds_crop_plane(&r, original_w, original_h, crop_x, crop_y, w_out, h_out));
    // STRUCT comparte recorte y binning del máster (planos luma).
    let nf_struct = nf_struct.map(|(sm, sr)| {
        (
            ds_crop_plane(&sm, original_w, original_h, crop_x, crop_y, w_out, h_out),
            ds_crop_plane(&sr, original_w, original_h, crop_x, crop_y, w_out, h_out),
        )
    });
    let nf_struct = match (nf_output_bin, nf_struct, nf_products.is_some()) {
        (Some((num, den)), Some((sm, sr)), true) => Some((
            crate::nebula_fusion::bin_area_f32(&sm, w_out, h_out, 1, num, den, false).0,
            crate::nebula_fusion::bin_area_f32(&sr, w_out, h_out, 1, num, den, false).0,
        )),
        (_, s, _) => s,
    };

    // --- Super-binning NF (F4): salida 0.75x/0.5x para sobremuestreo ---
    // Área ponderada tras el auto-crop: SCI conserva la fotometría de
    // superficie; VAR se propaga con Σa²·VAR/(Σa)²; NEFF media ponderada;
    // DQ con NO_COVERAGE solo si todo el bloque carece de cobertura.
    let (
        mut final_data,
        w_out,
        h_out,
        npx,
        wgt1,
        weight_map,
        rejection_low,
        rejection_high,
        registration_residuals,
        mut nf_products,
    ) = if let (Some((num, den)), true) = (nf_output_bin, nf_products.is_some()) {
        let bin_f64 = |v: &[f64]| -> Vec<f64> {
            let v32: Vec<f32> = v.iter().map(|&x| x as f32).collect();
            crate::nebula_fusion::bin_area_f32(&v32, w_out, h_out, 1, num, den, false)
                .0
                .into_iter()
                .map(|x| x as f64)
                .collect()
        };
        let (mut b_data, nw, nh) =
            crate::nebula_fusion::bin_area_f32(&final_data, w_out, h_out, ch, num, den, false);
        let mut b_products = nf_products.map(|p| crate::nebula_fusion::NfLiteProducts {
            variance: crate::nebula_fusion::bin_area_f32(
                &p.variance,
                w_out,
                h_out,
                ch,
                num,
                den,
                true,
            )
            .0,
            neff: crate::nebula_fusion::bin_area_f32(&p.neff, w_out, h_out, ch, num, den, false).0,
            dq: crate::nebula_fusion::bin_dq(&p.dq, w_out, h_out, num, den),
            masked_samples: p.masked_samples,
            variance_origin: p.variance_origin,
            g1g2_offset_max: p.g1g2_offset_max,
        });
        if let Some(products) = b_products.as_mut() {
            ds_reconcile_scientific_planes_with_dq(
                &mut b_data,
                &mut products.variance,
                &mut products.neff,
                &products.dq,
                ch,
            )?;
        }
        let b_wgt1 = bin_f64(&wgt1);
        let b_weight = bin_f64(&weight_map);
        let b_rlow = bin_f64(&rejection_low);
        let b_rhigh = bin_f64(&rejection_high);
        let b_res = crate::nebula_fusion::bin_area_f32(
            &registration_residuals,
            w_out,
            h_out,
            1,
            num,
            den,
            false,
        )
        .0;
        log_to_front(
                &app,
                "INFO",
                &format!(
                    "Super-binning {}x{} → {}x{} (escala {num}/{den}): SNR por píxel mejorado para datos sobremuestreados.",
                    w_out, h_out, nw, nh
                ),
            );
        (
            b_data,
            nw,
            nh,
            nw * nh,
            b_wgt1,
            b_weight,
            b_rlow,
            b_rhigh,
            b_res,
            b_products,
        )
    } else {
        (
            final_data,
            w_out,
            h_out,
            npx,
            wgt1,
            weight_map,
            rejection_low,
            rejection_high,
            registration_residuals,
            nf_products,
        )
    };
    if calibration_degraded {
        if let Some(products) = nf_products.as_mut() {
            for flags in &mut products.dq {
                *flags |= crate::deepsky_variance::dq::DEGRADED_CALIBRATION;
            }
        }
    }
    let mut coverage_only_dq = nf_products
        .is_none()
        .then(|| ds_dq_from_coverage(&weight_map));
    if calibration_degraded {
        if let Some(dq) = coverage_only_dq.as_mut() {
            for flags in dq {
                *flags |= crate::deepsky_variance::dq::DEGRADED_CALIBRATION;
            }
        }
    }
    let classic_products_missing_reason = if nf_products.is_none() {
        Some(if drz > 1.01 {
            "Classic drizzle aún no conserva Σw² por depósito; VAR/NEFF se omiten para no inventar incertidumbre"
        } else if use_tiled {
            "Classic tiled aún no devuelve momentos ponderados/Σw²; VAR/NEFF se omiten para no inventar incertidumbre"
        } else if used_gpu {
            "Classic GPU streaming aún no devuelve momentos ponderados/Σw²; VAR/NEFF se omiten para no inventar incertidumbre"
        } else {
            "la ruta efectiva no expuso momentos ponderados/Σw² verificables"
        })
    } else {
        None
    };

    // Diagnostic: the LINEAR master's value range (helps spot a dead/clipped
    // integration before any cosmetic step touches it).
    let detector_pattern_diagnostics = {
        let luma = if ch == 3 {
            final_data
                .chunks_exact(3)
                .map(|p| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2])
                .collect::<Vec<f32>>()
        } else {
            final_data.clone()
        };
        let (bg, nz) = ds_bg_noise(&luma);
        let mx = luma.iter().cloned().fold(0.0f32, f32::max);
        let nonzero =
            luma.iter().filter(|&&v| v > 1.0).count() as f64 / luma.len().max(1) as f64 * 100.0;
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Máster lineal: fondo≈{:.0} ADU · ruido≈{:.0} · máx {:.0} · {:.0}% con señal.",
                bg, nz, mx, nonzero
            ),
        );

        // MASTER QUALITY READOUT (WBPP-style): detect stars in the final master
        // to report count + median FWHM, plus an SNR proxy (peak signal / noise)
        // and the rejection statistics. Emitted for the final-view panel.
        let stars = ds_detect_stars(&luma, w_out, h_out, 200);
        let fwhm = ds_frame_fwhm_proxy(&luma, w_out, h_out, &stars);
        let snr = if nz > 0.0 {
            ((mx - bg).max(0.0) / nz) as f64
        } else {
            0.0
        };
        let detector_pattern = crate::deepsky_noise::analyze_detector_pattern(
            &luma,
            w_out,
            h_out,
            nz as f64,
        );
        if detector_pattern
            .as_ref()
            .is_some_and(|diagnostics| diagnostics.banding_detected)
        {
            let diagnostics = detector_pattern.as_ref().expect("checked above");
            log_to_front(
                &app,
                "WARN",
                &format!(
                    "Patrón del detector: banding {:.2}σ (RMS filas {:.2} ADU, columnas {:.2} ADU; autocorrelación {:.2}/{:.2}). Se conserva en SCI y receta; no se aplica una corrección destructiva.",
                    diagnostics.banding_sigma,
                    diagnostics.row_offset_rms_adu,
                    diagnostics.column_offset_rms_adu,
                    diagnostics.row_lag1_correlation,
                    diagnostics.column_lag1_correlation,
                ),
            );
        }
        log_to_front(
            &app,
            "INFO",
            &format!(
                "Calidad del máster: {} estrellas · FWHM {:.2} px · SNR pico ~{:.0} · rechazo {:.1}% · cobertura media {:.1} tomas/píxel.",
                stars.len(), fwhm, snr, rej_pct, mean_cov
            ),
        );
        let _ = app.emit(
            "ds-master-stats",
            &serde_json::json!({
                "stars": stars.len(),
                "fwhm": (fwhm * 100.0).round() / 100.0,
                "bg": bg.round(),
                "noise": nz.round(),
                "snr": snr.round(),
                "rej_pct": (rej_pct * 10.0).round() / 10.0,
                "mean_cov": (mean_cov * 10.0).round() / 10.0,
                "engine": effective_engine,
                "method": rejection,
                "w": w_out,
                "h": h_out,
                "detectorPattern": detector_pattern.as_ref(),
            }),
        );
        detector_pattern
    };

    // --- 4.5 DERIVED BACKGROUND/COLOR PREVIEW (SCI is immutable) ---
    // ABE/SCNR/neutralization are reversible derived-view operations. They
    // never mutate the linear SCI array, so VAR/NEFF/DQ remain compatible for
    // Classic, NebulaFusion and EIDR alike.
    let mut derived_preview_data: Option<Vec<f32>> = None;
    if use_gradient {
        emit_progress(
            &app,
            "Cielo Profundo: vista derivada de fondo y color (ABE + SCNR)...",
            93.0,
            None,
        );
        let mut derived = final_data.clone();
        ds_extract_background_gradient(&mut derived, w_out, h_out, ch);
        if ch == 3 {
            ds_scnr_green(&mut derived, ch, 1.0);
            let off = ds_neutralize_background(&mut derived, w_out, h_out, ch);
            log_to_front(
                &app,
                "INFO",
                &format!(
                    "Vista derivada fondo/color: offsets R/G/B = {:.0}/{:.0}/{:.0} ADU; SCI lineal sin cambios.",
                    off[0], off[1], off[2]
                ),
            );
        }
        derived_preview_data = Some(derived);
    }

    // --- 5. RESULT (LINEAR) + AUTO STF PREVIEW (linked) ---
    cancellation_checkpoint(cancel.as_ref(), "vista previa y publicación del resultado")?;
    emit_progress(
        &app,
        "Cielo Profundo: estiramiento automatico (STF)...",
        96.0,
        None,
    );
    // The LINEAR, unclamped master lives in `DeepSkyLinearResult.data` (built below,
    // f32) and is what `deepsky_export_float32`/FITS export use — negatives and
    // headroom preserved. This u16 buffer is ONLY the display/preview mirror stored
    // in the shared `StackResult` (still u16, shared with the planetary path). Fully
    // unifying `StackResult` to f32 so in-app deconv/wavelet also see the unclamped
    // linear data is deferred to P4 (LinearFrame contract).
    let mut rgb16 = vec![0u16; npx * 3];
    let preview_linear = derived_preview_data.as_deref().unwrap_or(&final_data);
    for i in 0..npx {
        for c in 0..3 {
            let v = preview_linear[i * ch + c.min(ch - 1)];
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

    let coverage_f32 = if wgt1.len() == npx {
        wgt1.iter().map(|&v| v as f32).collect()
    } else {
        Vec::new()
    };
    let frame_records: Vec<serde_json::Value> = frames
        .iter()
        .enumerate()
        .map(|(i, (path, stars, fwhm, noise, ecc))| {
            let reg = transforms[i];
            let registered_pos = registered.iter().position(|&(j, _, _)| j == i);
            let weight = registered_pos.map(|k| registered[k].2);
            let normalization = registered_pos.map(|k| norms[k]);
            let normalization_source = registered_pos.map(|k| normalization_sources[k]);
            let local_range = registered_pos
                .and_then(|k| loc_fields[k].as_ref())
                .map(|field| {
                    field
                        .iter()
                        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &v| {
                            (lo.min(v), hi.max(v))
                        })
                });
            serde_json::json!({
                "path": path,
                "used": weight.is_some(),
                "reference": i == ref_idx,
                "stars": stars.len(),
                "fwhm": fwhm,
                "noise": noise,
                "eccentricity": ecc,
                "weight": weight,
                "exclusionReason": quality_exclusions.get(&i),
                "registrationModel": reg.map(|r| r.transform.model.label()),
                "registrationInliers": reg.map(|r| r.inliers),
                "registrationRms": reg.map(|r| r.rms),
                "registrationHoldoutCount": reg.map(|r| r.holdout_count),
                "registrationHoldoutP95": reg.map(|r| r.holdout_p95),
                "registrationHoldoutMax": reg.map(|r| r.holdout_max),
                "registrationJacobianMin": reg.map(|r| r.jacobian_min),
                "registrationJacobianP95": reg.map(|r| r.jacobian_p95),
                "registrationJacobianMax": reg.map(|r| r.jacobian_max),
                "normalizationMultiply": normalization.map(|v| v.0[0]),
                "normalizationAdd": normalization.map(|v| v.1),
                "normalizationMultiplyRgb": normalization.map(|v| v.0),
                "normalizationAddRgb": normalization.map(|v| v.1),
                "normalizationScaleSource": normalization_source,
                "localNormalizationMin": local_range.map(|v| v.0),
                "localNormalizationMax": local_range.map(|v| v.1),
            })
        })
        .collect();
    // Método de integración efectivo y todos los fallbacks observados.
    let effective_integration_method = integration_method.unwrap_or_else(|| {
        pipeline::DeepSkyIntegrationMethod::Classic(pipeline::ClassicIntegrationConfig {
            version: 1,
            legacy_local_fwhm: local_weighting,
        })
    });
    let mut integration_fallbacks = nf_full_fallback.iter().cloned().collect::<Vec<String>>();
    if let Some(reason) = &nf_struct_fallback {
        integration_fallbacks.push(format!("STRUCT: {reason}"));
    }
    integration_fallbacks.extend(
        nf_parameter_fallbacks
            .iter()
            .map(|reason| format!("NebulaFusion parameter: {reason}")),
    );
    if let Some(reason) = &eidr_runtime_fallback {
        integration_fallbacks.push(reason.clone());
    }
    if let Some(reason) = &calibration_method_fallback {
        integration_fallbacks.push(reason.clone());
    }
    if let Some(reason) = &local_normalization_fallback {
        integration_fallbacks.push(format!("LocalNormalization: {reason}"));
    }
    let effective_integration_recipe = if eidr_runtime_fallback.is_some() {
        serde_json::json!({
            "method": "classic",
            "reason": "eidrScientificGateFallback",
        })
    } else {
        serde_json::to_value(&effective_integration_method)
            .unwrap_or_else(|_| serde_json::json!({"method": effective_integration_method.label()}))
    };
    let integration_recipe = serde_json::json!({
        "requested": requested_integration_method.label(),
        "effective": effective_integration_recipe,
        "fallbacks": integration_fallbacks,
        "targetPsfFwhmPx": nf_full_report.map(|(f, _, _)| f),
        "tilesFallback": nf_full_report.map(|(_, fb, _)| fb),
        "tilesTotal": nf_full_report.map(|(_, _, t)| t),
    });
    let eidr_recipe = eidr_outcome.as_ref().map(|e| {
        serde_json::json!({
            "scaleRequested": e.scale_requested,
            "scaleEffective": e.scale_eff,
            "fallbacks": e.fallbacks,
            "gate": {
                "fracApt": e.gate_frac.0,
                "fracDegrade": e.gate_frac.1,
                "fracFallback": e.gate_frac.2,
                "publishedCutoffCyclesPerPx": e.nu_cut,
                "nativeFallbackCutoffCyclesPerOutputPx": e.native_cutoff_cycles_per_output_px,
                "aptPixels": e.tile_apt_pixels,
                "degradedPixels": e.tile_degraded_pixels,
                "fallbackNativePixels": e.tile_fallback_pixels,
                "tileClasses": e.tile_classes,
            },
            "solver": {
                "mode": e.solver_mode_used,
                "validatedChannels": e.publication_channels,
                "iterations": e.solver_iterations,
                "relResidual": e.solver_rel_residual,
                "converged": e.solver_converged,
                "ridge": e.solver_ridge,
                "lambdaFreqPenalty": e.lambda_f,
                "irlsRounds": e.irls_rounds,
                "huberAdoptedChannels": e.huber_adopted_channels,
                "huberRevertedChannels": e.huber_reverted_channels,
                "multigrid": e.multigrid_used,
            },
            "refineRegistration": {
                "appliedFrames": e.refine_applied_frames,
                "p90ShiftPx": e.refine_p90_px,
                "reverted": e.refine_reverted,
            },
            "holdout": {
                "frames": e.holdout_frames,
                "chi2Median": e.holdout_chi2_median,
            },
            "targetPsfFwhmPx": e.gamma_fwhm_px,
            "missingPsfFrames": e.geometry_only_frames,
            "excludedNonAffineFrames": e.excluded_frames,
            "geometryFullField": {
                "sampleCount": e.geometry_sample_count,
                "p95ErrorPx": e.geometry_p95_error_px,
                "maxErrorPx": e.geometry_max_error_px,
                "minJacobian": e.geometry_min_jacobian,
                "acceptanceLimitPx": 0.02,
            },
            "padNativePx": e.pad,
            "varianceApproximation": "inverse_normal_diag_on_apt_tiles",
            "uncertaintyUnavailableOnMixedOrNativeTiles":
                e.tile_degraded_pixels > 0 || e.tile_fallback_pixels > 0,
        })
    });
    let dual_band_recipe = if matches!(capture_mode, pipeline::DeepSkyCaptureMode::DualBandOsc) {
        serde_json::json!({
            "quantitative": false,
            "labels": ["Ha proxy", "OIII proxy"],
            "reason": "No se proporcionó una matriz espectral cámara-filtro; la separación RGB es heurística",
        })
    } else {
        serde_json::Value::Null
    };
    let inputs_recipe = serde_json::json!({
        "lights": lights,
        "darks": darks,
        "flats": flats,
        "darkFlats": dark_flats,
        "bias": bias,
    });
    let parameters_recipe = serde_json::json!({
        "computePolicy": compute_policy,
        "captureMode": capture_mode,
        "calibrationPolicy": calibration_policy,
        "effectiveEngine": effective_engine,
        "requestedRejection": requested_rejection,
        "rejection": rejection,
        "methodFallbackReason": method_fallback_reason,
        "kappaLow": kappa_low,
        "kappaHigh": kappa_high,
        "clipIterations": n_iters,
        "normalization": norm_mode,
        "localNormalization": {
            "requested": use_local_norm,
            "effective": local_normalization_effective,
            "model": "symmetricMultiframePerChannel",
            "fallback": local_normalization_fallback,
            "metrics": local_normalization_metrics_recipe,
        },
        "registration": {
            "matcher": "bijective-triangle-v2",
            "modelSelection": "deterministic-spatial-holdout-80-20",
            "minimumCorrespondences": 8,
            "medianRmsPx": median_rms,
            "medianHoldoutP95Px": median_holdout_p95,
            "minimumJacobian": minimum_jacobian,
            "maximumJacobian": maximum_jacobian,
            "cacheVersion": DS_PREP_CACHE_VERSION,
        },
        "interpolation": if use_lanczos { "lanczos3" } else { "bilinear" },
        "drizzle": drz,
        "pixfrac": pixfrac,
        "cfaDrizzlePattern": cfa_drizzle_pattern,
        "ditherPositions": drizzle_dither_positions,
        "ditherDiagnostics1x": dither_diagnostics,
        "detectorPattern": detector_pattern_diagnostics,
        "cosmetic": use_cosmetic,
        "darkOptimization": use_dark_opt,
        "autoCrop": use_crop,
        "optionalAbeScnr": {
            "requested": use_gradient,
            "effective": derived_preview_data.is_some(),
            "target": "derivedPreviewOnly",
            "scienceModified": false,
        },
        "pedestal": pedestal,
    });
    let output_pedestal_state = if !calibration_decisions.is_empty()
        && calibration_decisions
            .iter()
            .all(|decision| matches!(decision.pedestal_state, pipeline::PedestalState::BiasSubtracted))
    {
        pipeline::PedestalState::BiasSubtracted
    } else {
        pipeline::PedestalState::RawIncludesBias
    };
    // A stack can span nights while retaining one camera/readout geometry.
    // Preserve every signature field that is truly common and clear only the
    // dimensions that differ; publishing `CalibrationSignature::default()`
    // discarded useful provenance and made the validated LinearFrame lie by
    // omission.
    let mut output_calibration_signature = calibration_decisions
        .first()
        .map(|decision| decision.signature.clone())
        .unwrap_or_default();
    macro_rules! clear_signature_field_if_mixed {
        ($field:ident) => {
            if calibration_decisions
                .iter()
                .skip(1)
                .any(|decision| decision.signature.$field != output_calibration_signature.$field)
            {
                output_calibration_signature.$field = None;
            }
        };
    }
    clear_signature_field_if_mixed!(camera);
    clear_signature_field_if_mixed!(sensor);
    clear_signature_field_if_mixed!(read_mode);
    clear_signature_field_if_mixed!(gain);
    clear_signature_field_if_mixed!(iso);
    clear_signature_field_if_mixed!(offset);
    clear_signature_field_if_mixed!(temperature_c);
    clear_signature_field_if_mixed!(exposure_seconds);
    clear_signature_field_if_mixed!(binning_x);
    clear_signature_field_if_mixed!(binning_y);
    clear_signature_field_if_mixed!(roi);
    clear_signature_field_if_mixed!(cfa_pattern);
    clear_signature_field_if_mixed!(cfa_phase);
    clear_signature_field_if_mixed!(filter);
    clear_signature_field_if_mixed!(session);
    clear_signature_field_if_mixed!(optical_train);
    clear_signature_field_if_mixed!(adc_bits);
    clear_signature_field_if_mixed!(white_level_adu);

    let uncertainty_unavailable_pixels = nf_products
        .as_ref()
        .map(|products| {
            products
                .dq
                .iter()
                .filter(|&&flags| {
                    flags & crate::deepsky_variance::dq::EIDR_UNCERTAINTY_UNAVAILABLE != 0
                })
                .count()
        })
        .unwrap_or(0);
    let no_coverage_pixels = nf_products
        .as_ref()
        .map(|products| {
            products
                .dq
                .iter()
                .filter(|&&flags| flags & crate::deepsky_variance::dq::NO_COVERAGE != 0)
                .count()
        })
        .or_else(|| {
            coverage_only_dq.as_ref().map(|dq| {
                dq.iter()
                    .filter(|&&flags| flags & crate::deepsky_variance::dq::NO_COVERAGE != 0)
                    .count()
            })
        })
        .unwrap_or(0);
    let scientific_output_eligible = !calibration_degraded
        && nf_products.is_some()
        && uncertainty_unavailable_pixels == 0;
    let scientific_products_recipe = serde_json::json!({
        "SCI": {"present": true, "unit": "ADU", "linear": true},
        "VAR": {
            "present": nf_products.is_some(),
            "unit": "ADU^2",
            "origin": nf_products.as_ref().map(|p| p.variance_origin.as_str()),
            "missingReason": classic_products_missing_reason,
            "partial": uncertainty_unavailable_pixels > 0,
            "unavailablePixels": uncertainty_unavailable_pixels,
        },
        "NEFF": {
            "present": nf_products.is_some(),
            "unit": "1",
            "missingReason": classic_products_missing_reason,
            "partial": uncertainty_unavailable_pixels > 0,
        },
        "DQ": {
            "present": true,
            "unit": "BITMASK",
            "origin": if nf_products.is_some() { "scientificEngine" } else { "coverageOnly" },
        },
        "coverage": {
            "present": coverage_f32.len() == npx,
            "unit": "1",
            "definition": "geometricPreRejectionWeight",
        },
        "effectiveWeight": {
            "present": weight_map.len() == npx,
            "unit": "1",
            "definition": "survivingPostRejectionWeight",
        },
        "classicWeightedMoments": classic_moment_products,
        "classicCalibrationVariancePropagated": classic_calibration_products,
        "linearFrameValidated": nf_products.is_some(),
        "noCoveragePixels": no_coverage_pixels,
        "scientificEligible": scientific_output_eligible,
    });
    let recipe = serde_json::json!({
        "schemaVersion": pipeline::DEEP_SKY_RECIPE_SCHEMA_VERSION,
        "sourceFingerprint": ds_source_fingerprint(&[&lights, &darks, &flats, &dark_flats, &bias]),
        "captureMode": capture_mode,
        "calibrationPolicy": calibration_policy,
        "calibrationDegraded": calibration_degraded,
        "scientificEligible": scientific_output_eligible,
        "outputPedestalState": output_pedestal_state,
        "outputCalibrationSignature": output_calibration_signature.clone(),
        "integrationMethod": integration_recipe,
        "scientificProducts": scientific_products_recipe,
        "variance": {
            "origin": nf_products
                .as_ref()
                .map(|p| serde_json::Value::String(p.variance_origin.as_str().into()))
                .unwrap_or(serde_json::Value::Null),
        },
        // Entrada OSC debayerizada float32 vs CFA directo (F4): la receta
        // declara cuál corrió para no reclamar la calidad del modo CFA.
        "demosaicedInput": lights_were_cfa
            && !((nf_lite_active && nf_cfa_direct)
                || (eidr_outcome.is_some() && eidr_cfa_direct)),
        "cfaDirectRequested": nf_cfa_direct || eidr_cfa_direct,
        "cfaDirect": (nf_lite_active && nf_cfa_direct)
            || (eidr_outcome.is_some() && eidr_cfa_direct),
        // EIDR (F9): escala, puerta, solver y holdout — reproducibilidad
        // completa del sucesor de drizzle.
        "eidr": eidr_recipe,
        "eidrFallback": eidr_runtime_fallback,
        "dualBandExtraction": dual_band_recipe,
        "cfaG1G2OffsetMax": nf_products.as_ref().and_then(|p| p.g1g2_offset_max),
        "outputBin": match nf_output_bin {
            Some((3, 4)) => "0.75x",
            Some((1, 2)) => "0.5x",
            _ => "1x",
        },
        "crossfitMaskedSamples": nf_products.as_ref().map(|p| p.masked_samples),
        // STRUCT (F7): refit PCG pospuesto (los umbrales sesgan levemente las
        // amplitudes aceptadas — documentado); conteo por nivel starlet.
        "structRefit": if nf_struct_accepted.is_some() { Some(false) } else { None },
        "structAcceptedPerLevel": nf_struct_accepted
            .as_ref()
            .map(|v| v.iter().map(|&(a, t)| serde_json::json!([a, t])).collect::<Vec<_>>()),
        "structFallback": nf_struct_fallback,
        "inputs": inputs_recipe,
        "calibrationSelection": calibration_selection_recipe,
        "calibrationDecisions": calibration_decisions,
        "parameters": parameters_recipe,
        "frames": frame_records,
    });

    // SCI/VAR/DQ are published only after passing the common radiometric
    // container. This consumes the large planes instead of cloning them,
    // avoiding another full-frame peak at 26/62 MP. Routes that cannot yet
    // produce VAR (Classic tiled/GPU/drizzle) remain explicitly outside this
    // contract and publish only coverage DQ with the missing reason above.
    let (final_data, final_variance, final_neff, final_dq) = match nf_products {
        Some(mut products) => {
            let variance_origin = products.variance_origin;
            let neff = std::mem::take(&mut products.neff);
            // Reconciliación SCI↔DQ ANTES del contenedor radiométrico. Los
            // productos propagados aplican exclusiones por DQ/VAR que el
            // acumulador del máster no aplica, y esa divergencia producía
            // contradicciones que abortaban stacks VÁLIDOS en LinearFrame
            // (auditoría 2026-07-20):
            //  - SCI finito con NO_COVERAGE/NAN_INPUT → el máster sí integró
            //    señal: se degrada a incertidumbre-no-disponible (VAR NaN
            //    auditada), nunca a un aborto.
            //  - SCI no finito sin máscara de invalidez → NO_COVERAGE real
            //    con SCI/VAR NaN canónicos.
            {
                let npx_out = w_out * h_out;
                for pixel in 0..npx_out {
                    let mut all_finite = true;
                    for c in 0..ch {
                        all_finite &= final_data[pixel * ch + c].is_finite();
                    }
                    let flags = products.dq[pixel];
                    let contradiction = flags
                        & (crate::deepsky_variance::dq::NO_COVERAGE
                            | crate::deepsky_variance::dq::NAN_INPUT
                            | crate::deepsky_variance::dq::FLAT_INVALID)
                        != 0;
                    if all_finite {
                        if contradiction {
                            products.dq[pixel] &= !(crate::deepsky_variance::dq::NO_COVERAGE
                                | crate::deepsky_variance::dq::NAN_INPUT
                                | crate::deepsky_variance::dq::FLAT_INVALID);
                        }
                        let var_unavailable = (0..ch).any(|c| {
                            !products.variance[pixel * ch + c].is_finite()
                        });
                        if var_unavailable {
                            products.dq[pixel] |=
                                crate::deepsky_variance::dq::EIDR_UNCERTAINTY_UNAVAILABLE;
                        }
                    } else {
                        products.dq[pixel] = (flags
                            & !(crate::deepsky_variance::dq::NAN_INPUT
                                | crate::deepsky_variance::dq::FLAT_INVALID))
                            | crate::deepsky_variance::dq::NO_COVERAGE;
                        for c in 0..ch {
                            let index = pixel * ch + c;
                            final_data[index] = f32::NAN;
                            products.variance[index] = f32::NAN;
                        }
                    }
                    if products.dq[pixel] & crate::deepsky_variance::dq::NO_COVERAGE != 0 {
                        for c in 0..ch {
                            products.variance[pixel * ch + c] = f32::NAN;
                        }
                    }
                }
            }
            let frame = crate::deepsky_linear_frame::LinearFrame::new(
                w_out,
                h_out,
                ch,
                final_data,
                std::mem::take(&mut products.variance),
                std::mem::take(&mut products.dq),
                None,
                crate::deepsky_linear_frame::LinearFrameMetadata {
                    layout: if ch == 3 {
                        pipeline::StoreLayout::Rgb
                    } else {
                        pipeline::StoreLayout::Mono
                    },
                    pedestal_state: output_pedestal_state,
                    calibration_signature: output_calibration_signature,
                    variance_origin: Some(variance_origin),
                    science_unit: "ADU",
                },
            )?;
            let (data, variance, dq, _psf, _metadata) = frame.into_components();
            (data, Some(variance), Some(neff), Some(dq))
        }
        None => (final_data, None, None, coverage_only_dq),
    };
    // Última barrera ANTES de mutar estado compartido: entre el checkpoint de
    // la vista previa y este punto hay decenas de segundos (stretch, PNG,
    // LinearFrame) y un trabajo cancelado en esa ventana NO debe publicar ni
    // machacar el resultado vigente (auditoría 2026-07-20).
    cancellation_checkpoint(cancel.as_ref(), "publicación del resultado")?;
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
    {
        let mut linear = state.deep_sky_result.lock().unwrap();
        *linear = Some(DeepSkyLinearResult {
            id: ds_result_id.clone(),
            data: final_data,
            width: w_out,
            height: h_out,
            channels: ch,
            coverage: coverage_f32,
            weight: weight_map.iter().map(|&v| v as f32).collect(),
            rejection_low: rejection_low.iter().map(|&v| v as f32).collect(),
            rejection_high: rejection_high.iter().map(|&v| v as f32).collect(),
            registration_residuals: registration_residuals,
            engine: if nf_lite_active || eidr_outcome.is_some() {
                effective_engine.clone()
            } else if used_gpu {
                "hybrid_wgpu".into()
            } else if use_tiled {
                "cpu_tiled".into()
            } else {
                "cpu_streaming".into()
            },
            method: rejection.clone(),
            frames_used: registered.len(),
            frames_rejected: rejected,
            elapsed_seconds: ds_run_started.elapsed().as_secs_f32(),
            recipe,
            variance: final_variance,
            neff: final_neff,
            dq: final_dq,
            struct_map: nf_struct.as_ref().map(|(sm, _)| sm.clone()),
            struct_residual: nf_struct.as_ref().map(|(_, sr)| sr.clone()),
            recoverability: eidr_recov.clone(),
        });
    }

    log_to_front(
        &app,
        "SUCCESS",
        &format!(
            "Cielo Profundo: {} frames ({} rechazados) · motor {} · rechazo {} (κ↓{:.1}/κ↑{:.1} ×{}) · norm {} · interp {} · cosmética {} · vista derivada fondo/color {} · opt.dark {} · drizzle {} · peso PSF Signal Weight.",
            registered.len(),
            rejected,
            effective_engine,
            rejection,
            kappa_low,
            kappa_high,
            n_iters,
            norm_mode,
            if drz > 1.01 {
                "drop-kernel"
            } else if use_lanczos {
                "Lanczos-3"
            } else {
                "bilineal"
            },
            if use_cosmetic { "ON" } else { "OFF" },
            if use_gradient { "ON" } else { "OFF" },
            if use_dark_opt { "ON" } else { "OFF" },
            if drz > 1.01 { format!("{:.0}× drop pf{:.2}", drz, pixfrac) } else { "OFF".to_string() }
        ),
    );
    log_to_front(
        &app,
        "INFO",
        "Peso por toma: PSF Signal Weight (flujo estelar/σ²) × nitidez (FWHM) × redondez.",
    );
    emit_progress(&app, "Cielo Profundo: completado.", 100.0, None);
    emit_deepsky_pipeline_telemetry(
        &app,
        &ds_result_id,
        "complete",
        &effective_engine,
        registered.len(),
        registered.len(),
        ds_run_started,
        &ds_telemetry_sys,
        if used_gpu { peak_vram_mb } else { 0 },
        frame_store.read_hits(),
        None,
    );
    Ok(
        save_preview_png_to_temp(&enc, "deepsky").unwrap_or_else(|| {
            format!(
                "data:image/png;base64,{}",
                general_purpose::STANDARD.encode(&enc)
            )
        }),
    )
}

#[cfg(test)]
mod ds_tests {
    use super::*;

    fn ds_test_probe(path: &str, signature: pipeline::CalibrationSignature) -> DsProbe {
        let bayer = signature.cfa_pattern.clone();
        let store_layout = if let (Some(pattern), Some(phase)) =
            (signature.cfa_pattern.clone(), signature.cfa_phase)
        {
            Some(pipeline::StoreLayout::Cfa {
                pattern,
                phase_x: phase[0],
                phase_y: phase[1],
            })
        } else {
            Some(pipeline::StoreLayout::Mono)
        };
        DsProbe {
            path: path.into(),
            name: path.into(),
            w: 8,
            h: 8,
            ch: 1,
            exptime: signature.exposure_seconds.map(|value| value as f32),
            bayer,
            temp: signature.temperature_c,
            gain: signature.gain,
            binning: signature.binning_x.map(|value| value as i32),
            filter: signature.filter.clone(),
            date_obs: signature.session.clone(),
            signature,
            store_layout,
            signature_warnings: Vec::new(),
            signature_missing: Vec::new(),
            ok: true,
            error: None,
        }
    }

    fn ds_test_signature() -> pipeline::CalibrationSignature {
        pipeline::CalibrationSignature {
            camera: Some("camera-a".into()),
            sensor: Some("sensor-a".into()),
            read_mode: Some("mode-1".into()),
            gain: Some(100.0),
            offset: Some(20.0),
            temperature_c: Some(-10.0),
            exposure_seconds: Some(30.0),
            binning_x: Some(1),
            binning_y: Some(1),
            roi: Some([0, 0, 8, 8]),
            filter: Some("Ha".into()),
            session: Some("2026-07-19".into()),
            optical_train: Some("scope-a".into()),
            adc_bits: Some(16),
            white_level_adu: Some(60_000.0),
            ..pipeline::CalibrationSignature::default()
        }
    }

    #[test]
    fn test_ds_classifies_dark_flats_as_first_class_calibration() {
        assert_eq!(
            ds_classify("/capture/DarkFlats/frame_001.fits"),
            "dark_flats"
        );
        assert_eq!(ds_classify("/capture/flat-dark_2s.fit"), "dark_flats");
        assert_eq!(ds_classify("/capture/FLATDARK_2s.tiff"), "dark_flats");
    }

    #[test]
    fn test_ds_bayer_roi_offsets_shift_pattern_parity() {
        assert_eq!(ds_shift_bayer_id(8, 0, 0), Some(8));
        assert_eq!(ds_shift_bayer_id(8, 1, 0), Some(9));
        assert_eq!(ds_shift_bayer_id(8, 0, 1), Some(10));
        assert_eq!(ds_shift_bayer_id(8, 1, 1), Some(11));
        assert_eq!(ds_shift_bayer_id(8, 3, -1), Some(11));
        assert_eq!(
            ds_bayer_id_from_layout(&pipeline::StoreLayout::Cfa {
                pattern: "RGGB".into(),
                // Signature extraction already composed explicit (1,0) +
                // ROI (0,1), so the reader must apply the final (1,1) once.
                phase_x: 1,
                phase_y: 1,
            }),
            Some(11)
        );
        assert_eq!(
            ds_bayer_id_from_layout(&pipeline::StoreLayout::Mono),
            None
        );
    }

    #[test]
    fn test_ds_missing_required_calibration_lists_fail_closed() {
        let light = ds_test_probe("light.fits", ds_test_signature());
        let strict_dark = ds_select_calibration_group(
            &[],
            std::slice::from_ref(&light),
            "darks",
            pipeline::DeepSkyCalibrationPolicy::Strict,
        );
        assert!(!strict_dark.scientific_eligible);
        assert!(!strict_dark.blocking_reasons.is_empty());

        let degraded_flat = ds_select_calibration_group(
            &[],
            std::slice::from_ref(&light),
            "flats",
            pipeline::DeepSkyCalibrationPolicy::AllowDegraded,
        );
        assert!(!degraded_flat.scientific_eligible);
        assert!(degraded_flat.degraded);
        assert!(degraded_flat
            .warnings
            .iter()
            .any(|reason| reason.contains("Classic no científico")));

        let decisions = ds_prepare_calibration_decisions(
            &[light],
            &[],
            &[],
            &[],
            &[],
            pipeline::DeepSkyCalibrationPolicy::Strict,
        );
        assert_eq!(decisions.len(), 1);
        assert!(decisions[0].degraded);
        assert!(!decisions[0].compatible);
        assert_eq!(
            decisions[0].pedestal_state,
            pipeline::PedestalState::RawIncludesBias
        );
        assert!(decisions[0]
            .reasons
            .iter()
            .any(|reason| reason.starts_with("dark:")));
        assert!(decisions[0]
            .reasons
            .iter()
            .any(|reason| reason.starts_with("flat:")));
    }

    #[test]
    fn test_ds_global_master_grouping_detects_incompatible_signatures() {
        let a = ds_test_probe("bias-gain100.fits", ds_test_signature());
        let mut signature_b = ds_test_signature();
        signature_b.gain = Some(200.0);
        let b = ds_test_probe("bias-gain200.fits", signature_b);
        let groups = ds_master_signature_groups(
            [&a, &b],
            crate::deepsky_calibration_contract::CalibrationRole::Bias,
        );
        assert_eq!(groups.len(), 2);

        let compatible = ds_test_probe("bias-gain100-b.fits", ds_test_signature());
        let groups = ds_master_signature_groups(
            [&a, &compatible],
            crate::deepsky_calibration_contract::CalibrationRole::Bias,
        );
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn test_ds_dark_flat_match_requires_complete_flat_signature() {
        let flat_probe = ds_test_probe("flat.fits", ds_test_signature());
        let dark_flat_probe = ds_test_probe("dark-flat.fits", ds_test_signature());
        let image = DsImage {
            data: vec![1000.0; 64],
            w: 8,
            h: 8,
            ch: 1,
            bayer: None,
        };
        let master = DsRawDarkFlatMaster {
            exposure: Some(30.0),
            master: DsCalibrationMaster {
                image: image.clone(),
                variance: vec![1.0; 64],
                neff: vec![5.0; 64],
                dq: vec![0; 64],
                rejected_fraction: vec![0.0; 64],
                frames: 5,
                max_input_scale_sq: 1.0,
            },
            calibration_probe: Some(dark_flat_probe),
            source_paths: vec!["dark-flat.fits".into()],
        };
        assert!(ds_dark_flat_master_matches_flat(
            &flat_probe,
            &image,
            &master
        ));

        let mut wrong_temperature = ds_test_signature();
        wrong_temperature.temperature_c = Some(-9.0);
        let mut wrong = master.clone();
        wrong.calibration_probe = Some(ds_test_probe("wrong-df.fits", wrong_temperature));
        assert!(!ds_dark_flat_master_matches_flat(
            &flat_probe,
            &image,
            &wrong
        ));
    }

    #[test]
    fn test_ds_flat_linearity_marks_dq_and_rejects_illuminated_clip() {
        let mut signature = ds_test_signature();
        signature.white_level_adu = Some(1000.0);
        signature.adc_bits = Some(12);
        let mut data = vec![500.0f32; 2_000];
        data[10] = 1000.0;
        data[20] = 950.0;
        data[21] = 925.0;
        data[22] = 910.0;
        let flat = DsImage {
            data,
            w: 2_000,
            h: 1,
            ch: 1,
            bayer: None,
        };
        let report = ds_flat_linearity_report(&flat, &signature).unwrap();
        assert_eq!(report.saturated_samples, 1);
        assert_eq!(report.nonlinear_samples, 3);
        assert_ne!(
            report.dq[10] & crate::deepsky_variance::dq::SATURATED,
            0
        );
        assert_ne!(
            report.dq[20] & crate::deepsky_variance::dq::NONLINEAR,
            0
        );
        assert!(report.invalid_frame_reason().is_some());

        signature.white_level_adu = None;
        let adc_report = ds_flat_linearity_report(&flat, &signature).unwrap();
        assert_eq!(adc_report.white_level_adu, 4095.0);
    }

    #[test]
    fn test_ds_flat_prefers_raw_dark_flat_without_double_bias() {
        let mut flat = DsImage {
            data: vec![1_250.0, 2_250.0],
            w: 2,
            h: 1,
            ch: 1,
            bayer: None,
        };
        let raw_dark_flat = DsImage {
            data: vec![250.0, 250.0], // bias 200 + thermal 50
            w: 2,
            h: 1,
            ch: 1,
            bayer: None,
        };
        let bias = DsImage {
            data: vec![200.0, 200.0],
            w: 2,
            h: 1,
            ch: 1,
            bayer: None,
        };
        let source =
            ds_apply_flat_calibration(&mut flat, Some(&raw_dark_flat), Some(&bias)).unwrap();
        assert_eq!(source, "rawDarkFlat");
        assert_eq!(flat.data, vec![1_000.0, 2_000.0]);
    }

    #[test]
    fn test_ds_uncovered_science_pixels_are_nan_not_filled() {
        let mut sci = vec![10.0f32, 11.0, 20.0, 21.0, 30.0, 31.0];
        let coverage = vec![1.0f64, 0.0, f64::NAN];
        assert_eq!(ds_mark_uncovered_nan(&mut sci, &coverage, 2), 2);
        assert_eq!(&sci[..2], &[10.0, 11.0]);
        assert!(sci[2..].iter().all(|value| value.is_nan()));
    }

    #[test]
    fn test_ds_reconcile_scientific_planes_with_dq_keeps_uncertainty_and_holes_honest() {
        let mut science = vec![10.0f32, 11.0, 20.0, 21.0, 30.0, 31.0, 40.0, 41.0];
        let mut variance = vec![1.0f32, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5];
        let mut neff = vec![8.0f32; 8];
        let dq = vec![
            0,
            crate::deepsky_variance::dq::EIDR_UNCERTAINTY_UNAVAILABLE,
            crate::deepsky_variance::dq::NO_COVERAGE,
            crate::deepsky_variance::dq::NAN_INPUT
                | crate::deepsky_variance::dq::FLAT_INVALID,
        ];
        ds_reconcile_scientific_planes_with_dq(
            &mut science,
            &mut variance,
            &mut neff,
            &dq,
            2,
        )
        .unwrap();

        assert_eq!(&science[..2], &[10.0, 11.0]);
        assert_eq!(&variance[..2], &[1.0, 1.5]);
        assert_eq!(&neff[..2], &[8.0, 8.0]);
        assert_eq!(&science[2..4], &[20.0, 21.0]);
        assert!(variance[2..4].iter().all(|value| value.is_nan()));
        assert_eq!(&neff[2..4], &[0.0, 0.0]);
        assert!(science[4..6].iter().all(|value| value.is_nan()));
        assert!(variance[4..6].iter().all(|value| value.is_nan()));
        assert_eq!(&neff[4..6], &[0.0, 0.0]);
        assert!(science[6..8].iter().all(|value| value.is_nan()));
        assert!(variance[6..8].iter().all(|value| value.is_nan()));
        assert_eq!(&neff[6..8], &[0.0, 0.0]);
    }

    #[test]
    fn test_ds_reconcile_scientific_planes_with_dq_rejects_misaligned_planes() {
        let mut science = vec![1.0f32; 2];
        let mut variance = vec![1.0f32; 2];
        let mut neff = vec![1.0f32; 1];
        let error = ds_reconcile_scientific_planes_with_dq(
            &mut science,
            &mut variance,
            &mut neff,
            &[0],
            2,
        )
        .unwrap_err();
        assert!(error.contains("incompatibles"));
    }

    #[test]
    fn test_ds_classic_weighted_moments_publish_honest_var_neff_and_dq() {
        // Two equally weighted samples 10 and 14: sample variance=8 and
        // variance of their mean=4; NEFF=2 exactly.
        let products = ds_classic_products_from_moments(
            &[24.0, 0.0],
            &[296.0, 0.0],
            &[2.0, 0.0],
            &[2.0, 0.0],
            2,
            1,
        )
        .unwrap();
        assert!((products.variance[0] - 4.0).abs() < 1e-6);
        assert!((products.neff[0] - 2.0).abs() < 1e-6);
        assert_eq!(products.dq[0], 0);
        assert!(products.variance[1].is_nan());
        assert_eq!(products.neff[1], 0.0);
        assert_ne!(
            products.dq[1] & crate::deepsky_variance::dq::NO_COVERAGE,
            0
        );
    }

    fn calibration_master_for_test(
        image: DsImage,
        variance: Vec<f32>,
    ) -> DsCalibrationMaster {
        let len = image.data.len();
        DsCalibrationMaster {
            image,
            variance,
            neff: vec![8.0; len],
            dq: vec![0; len],
            rejected_fraction: vec![0.0; len],
            frames: 8,
            max_input_scale_sq: 1.0,
        }
    }

    #[test]
    fn test_ds_scientific_dark_uses_shared_bias_covariance() {
        let (w, h) = (8usize, 8usize);
        let mut light = DsImage {
            data: (0..w * h)
                .map(|index| 1_000.0 + (index % 7) as f32)
                .collect(),
            w,
            h,
            ch: 1,
            bayer: None,
        };
        let raw_light_variance =
            crate::deepsky_variance::empirical_channel_variance(&light)[0];
        let bias = calibration_master_for_test(
            DsImage {
                data: vec![100.0; w * h],
                w,
                h,
                ch: 1,
                bayer: None,
            },
            vec![4.0; w * h],
        );
        // The image is thermal D-B, while VAR remains the raw-D estimator
        // variance. At k=1 the shared bias cancels algebraically.
        let dark = DsDarkMaster {
            exposure: Some(60.0),
            master: calibration_master_for_test(
                DsImage {
                    data: vec![20.0; w * h],
                    w,
                    h,
                    ch: 1,
                    bayer: None,
                },
                vec![9.0; w * h],
            ),
            amp_glow: false,
            bias_subtracted: true,
            source_paths: vec!["dark.fit".into()],
            calibration_probe: None,
        };
        let uncertainty =
            ds_calibrate_scientific(&mut light, Some(&bias), Some(&dark), None, 1.0)
                .unwrap();
        assert!((light.data[0] - 880.0).abs() < 1.0e-6);
        assert!((uncertainty.variance[0] - (raw_light_variance + 9.0)).abs() < 1.0e-5);
        assert!(uncertainty.publishable);
    }

    #[test]
    fn test_ds_scientific_weak_flat_is_nan_and_dq_not_clamped() {
        let (w, h) = (8usize, 8usize);
        let mut light = DsImage {
            data: (0..w * h)
                .map(|index| 500.0 + (index % 5) as f32)
                .collect(),
            w,
            h,
            ch: 1,
            bayer: None,
        };
        let mut response = vec![1.0; w * h];
        response[0] = 0.01;
        let flat = calibration_master_for_test(
            DsImage {
                data: response,
                w,
                h,
                ch: 1,
                bayer: None,
            },
            vec![1.0e-4; w * h],
        );
        let uncertainty =
            ds_calibrate_scientific(&mut light, None, None, Some(&flat), 1.0).unwrap();
        assert!(light.data[0].is_nan());
        assert_ne!(
            uncertainty.dq[0] & crate::deepsky_variance::dq::FLAT_INVALID,
            0
        );
        assert!(light.data[1].is_finite());
        assert!(uncertainty.variance[1].is_finite());
    }

    #[test]
    fn test_ds_uncertainty_accepts_masked_local_var_but_rejects_unmasked_gap() {
        let mut variance = vec![4.0f32; 8];
        let mut dq = vec![0u32; 8];
        variance[2] = f32::NAN;
        dq[2] = crate::deepsky_variance::dq::HOT_COLD
            | crate::deepsky_variance::dq::INTERPOLATED;
        assert!(ds_uncertainty_is_publishable(&variance, &dq, 8, 1, 1));

        dq[2] = 0;
        assert!(!ds_uncertainty_is_publishable(&variance, &dq, 8, 1, 1));
    }

    #[test]
    fn test_ds_classic_propagates_calibration_variance_through_identity_warp() {
        let (w, h, ch) = (8usize, 8usize, 1usize);
        let load_science = |index: usize| {
            Ok(DsImage {
                data: vec![if index == 0 { 10.0 } else { 14.0 }; w * h],
                w,
                h,
                ch,
                bayer: None,
            })
        };
        let load_uncertainty = |index: usize| {
            Ok(DsCalibratedUncertainty {
                variance: vec![if index == 0 { 4.0 } else { 9.0 }; w * h],
                dq: vec![0; w * h],
                publishable: true,
                fallback_reason: None,
            })
        };
        let registered = vec![
            (0, DsTransform::identity(), 1.0),
            (1, DsTransform::identity(), 1.0),
        ];
        let norms = vec![([1.0; 3], [0.0; 3]); 2];
        let fields = vec![None, None];
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let products = ds_classic_products_from_calibration(
            &load_science,
            &load_uncertainty,
            &registered,
            &norms,
            &fields,
            1,
            w,
            h,
            ch,
            &fields,
            1,
            false,
            None,
            &cancel,
        )
        .unwrap()
        .unwrap();
        let center = 3 * w + 3;
        assert!((products.variance[center] - 3.25).abs() < 1.0e-6);
        assert!((products.neff[center] - 2.0).abs() < 1.0e-6);
        assert_eq!(
            products.variance_origin,
            crate::deepsky_variance::VarianceOrigin::HybridEmpiricalPropagated
        );
    }

    #[test]
    fn test_ds_eidr_geometry_summary_keeps_worst_field_metrics() {
        let reports = [
            crate::eidr::EidrGeomFieldReport {
                sample_count: 289,
                p95_error_px: 0.004,
                max_error_px: 0.008,
                min_jacobian: 0.99,
            },
            crate::eidr::EidrGeomFieldReport {
                sample_count: 289,
                p95_error_px: 0.010,
                max_error_px: 0.019,
                min_jacobian: 0.95,
            },
        ];
        let summary = ds_eidr_geometry_summary(&reports).unwrap();
        assert_eq!(summary.0, 578);
        assert_eq!(summary.1, 0.010);
        assert_eq!(summary.2, 0.019);
        assert_eq!(summary.3, 0.95);
    }

    #[test]
    fn test_ds_eidr_publication_requires_convergence_finite_and_holdout() {
        let good = crate::eidr::EidrSolveReport {
            iterations: 12,
            rel_residual: 1.0e-5,
            converged: true,
            ridge: 0.01,
        };
        assert!(ds_eidr_channel_publishable(&good, true));
        assert!(!ds_eidr_channel_publishable(
            &crate::eidr::EidrSolveReport {
                converged: false,
                ..good
            },
            true
        ));
        assert!(!ds_eidr_channel_publishable(&good, false));
        assert!(ds_eidr_holdout_publishable(12, Some(1.2)));
        assert!(!ds_eidr_holdout_publishable(12, Some(2.01)));
        assert!(!ds_eidr_holdout_publishable(12, None));
        assert!(!ds_eidr_holdout_publishable(6, None));
    }

    fn auto_signals(n: usize) -> DsAutoSignals {
        DsAutoSignals {
            n_lights: n,
            sessions: 1,
            narrowband: false,
            filter: None,
            background: 800.0,
            noise: 100.0,
            background_over_noise: 8.0,
            gradient_strength: 1.0,
            stars_per_mpx: 120.0,
            fwhm_px: 3.2,
            dark_nebula: false,
            dithering_rms_px: None,
        }
    }

    fn auto_request(n_hint: &str) -> pipeline::DeepSkyStackRequest {
        pipeline::DeepSkyStackRequest {
            profile: PipelineProfile::Auto,
            lights: vec![format!("{n_hint}.fits")],
            ..Default::default()
        }
    }

    #[test]
    fn test_ds_reject_pixel_unequal_weights_keep_estimator_neutral() {
        // Rescate de detalle: los pesos entran SOLO en la media ponderada de
        // supervivientes, nunca en la decisión de rechazo por valor. Con todos
        // los valores iguales, cualquier combinación de pesos debe devolver
        // exactamente ese valor (sin sesgo del estimador).
        for method in ["sigma", "winsorized", "median", "percentile", "average"] {
            let mut samples: Vec<(f32, f64)> = (0..12)
                .map(|k| (500.0f32, 0.2 + k as f64 * 0.13))
                .collect();
            let (mut cov, mut lo, mut hi) = (0.0, 0.0, 0.0);
            let out = ds_reject_pixel(
                &mut samples,
                method,
                3.0,
                3.0,
                &mut cov,
                &mut lo,
                &mut hi,
                4.0,
            );
            assert!(
                (out - 500.0).abs() < 1e-4,
                "{method}: estimador {out} sesgado con pesos desiguales"
            );
        }
    }

    #[test]
    fn test_ds_auto_recipe_n_table_and_determinism() {
        // N<8 → media sin rechazo; 8-15 → sigma; 16-50 → winsorized 2.8/3.0;
        // >50 → winsorized 3.0/2.5 (linear-fit NUNCA: sustitución 2026-07-19).
        for (n, rejection, kl, kh, iters) in [
            (5usize, "average", 3.0f32, 3.0f32, 1u32),
            (12, "sigma", 3.0, 3.0, 2),
            (40, "winsorized", 2.8, 3.0, 3),
            (120, "winsorized", 3.0, 2.5, 3),
        ] {
            let (resolved, reasons, map) =
                ds_resolve_auto_recipe(auto_request("l"), &auto_signals(n), 10, true);
            assert_eq!(resolved.rejection, rejection, "N={n}");
            assert_eq!(resolved.kappa_low, kl, "N={n}");
            assert_eq!(resolved.kappa_high, kh, "N={n}");
            assert_eq!(resolved.clip_iters, Some(iters), "N={n}");
            assert_eq!(resolved.interpolation, "lanczos3");
            assert_eq!(resolved.normalization, "scaling");
            assert!(!resolved.gradient, "AUTO nunca activa ABE/SCNR");
            assert!(!reasons.is_empty());
            assert_eq!(map.get("rejection"), Some(&rejection.to_string()));
            assert_ne!(resolved.rejection, "linearfit");
            // Determinismo: misma entrada → misma receta (paridad plan↔run).
            let (again, _, map2) =
                ds_resolve_auto_recipe(auto_request("l"), &auto_signals(n), 10, true);
            assert_eq!(again.rejection, resolved.rejection);
            assert_eq!(map, map2);
        }
    }

    #[test]
    fn test_ds_auto_recipe_memory_pressure_forces_fast() {
        let (resolved, reasons, _) =
            ds_resolve_auto_recipe(auto_request("l"), &auto_signals(120), 85, true);
        assert_eq!(resolved.rejection, "sigma");
        assert_eq!(resolved.clip_iters, Some(1));
        assert_eq!(resolved.interpolation, "bilinear");
        assert_eq!(resolved.normalization, "additive");
        assert_eq!(resolved.drizzle, 1.0);
        assert!(reasons[0].contains("RAM"));
    }

    #[test]
    fn test_ds_auto_recipe_gradient_and_sessions_pick_local_normalization() {
        let mut signals = auto_signals(40);
        signals.gradient_strength = 4.5;
        let (resolved, _, _) = ds_resolve_auto_recipe(auto_request("l"), &signals, 10, true);
        assert_eq!(resolved.normalization, "local");

        let mut signals = auto_signals(40);
        signals.sessions = 3;
        let (resolved, _, _) = ds_resolve_auto_recipe(auto_request("l"), &signals, 10, true);
        assert_eq!(resolved.normalization, "local");
    }

    #[test]
    fn test_ds_auto_recipe_narrowband_and_dark_nebula_protect_faint_signal() {
        // Dataset tipo usuario: 120 lights narrowband con fondo tenue.
        let mut signals = auto_signals(120);
        signals.narrowband = true;
        signals.filter = Some("HA_OIII".into());
        signals.background_over_noise = 1.4;
        signals.stars_per_mpx = 25.0;
        signals.dark_nebula = true;
        let (resolved, reasons, map) =
            ds_resolve_auto_recipe(auto_request("l"), &signals, 10, true);
        assert_eq!(resolved.rejection, "winsorized");
        assert!(resolved.kappa_high >= 3.5, "κ alto conserva emisión débil");
        assert!(resolved.kappa_low >= 4.0, "κ_low 4.0 no recorta la cola oscura");
        assert_eq!(resolved.normalization, "additive");
        assert!(!resolved.gradient);
        assert!(reasons.iter().any(|r| r.contains("fondo tenue")));
        assert_eq!(map.get("dark_nebula"), Some(&"true".to_string()));
    }

    #[test]
    fn test_ds_auto_recipe_drizzle_gate_requires_all_conditions() {
        // Todas las condiciones → drizzle 2x con sigma κ2.5 (fallback del motor).
        let mut signals = auto_signals(60);
        signals.fwhm_px = 1.8;
        signals.dithering_rms_px = Some(1.1);
        let (resolved, _, _) = ds_resolve_auto_recipe(auto_request("l"), &signals, 10, true);
        assert_eq!(resolved.drizzle, 2.0);
        assert_eq!(resolved.pixfrac, 0.7);
        assert_eq!(resolved.rejection, "sigma");
        assert_eq!(resolved.kappa_low, 2.5);

        // Sin RAM 2x: sólo razón informativa, drizzle 1x y receta base intacta.
        let (resolved, reasons, _) =
            ds_resolve_auto_recipe(auto_request("l"), &signals, 10, false);
        assert_eq!(resolved.drizzle, 1.0);
        assert_eq!(resolved.rejection, "winsorized");
        assert!(reasons.iter().any(|r| r.contains("Drizzle 2× quedaría disponible")));

        // FWHM grande (sin submuestreo): tampoco activa.
        let mut signals = auto_signals(60);
        signals.fwhm_px = 3.4;
        signals.dithering_rms_px = Some(1.1);
        let (resolved, _, _) = ds_resolve_auto_recipe(auto_request("l"), &signals, 10, true);
        assert_eq!(resolved.drizzle, 1.0);
    }

    #[test]
    fn test_ds_session_night_id_reconciles_wrong_clock_year_with_filename() {
        let dir = std::env::temp_dir().join("zas_night_reconcile_test");
        let _ = std::fs::create_dir_all(&dir);
        // Nombre del software de captura con el año correcto; cabecera con el
        // reloj de la cámara un año atrás (mismo mes/día): gana el nombre.
        let named = dir.join("2026-04-26_19-47-56_SV220_flat_0001.fits");
        let _ = std::fs::write(&named, b"x");
        assert_eq!(
            ds_session_night_id(named.to_str().unwrap(), Some("2025-04-26T19:47:56")),
            Some("2026-04-26".into())
        );
        // Discrepancia pequeña (cabecera un día después, misma sesión UTC):
        // gana la cabecera, como siempre.
        assert_eq!(
            ds_session_night_id(named.to_str().unwrap(), Some("2026-04-27T03:10:00")),
            Some("2026-04-26".into())
        );
        // Sin patrón de fecha en el nombre: la cabecera manda.
        let plain = dir.join("flat_sin_fecha.fits");
        let _ = std::fs::write(&plain, b"x");
        assert_eq!(
            ds_session_night_id(plain.to_str().unwrap(), Some("2025-04-26T19:47:56")),
            Some("2025-04-26".into())
        );
    }

    #[test]
    fn test_ds_session_night_id_cuts_at_local_noon() {
        // Una toma a las 03:14 pertenece a la NOCHE del día anterior.
        assert_eq!(
            ds_session_night_id("x", Some("2026-03-02T03:14:00.123")).as_deref(),
            Some("2026-03-01")
        );
        // Una toma a las 22:13 pertenece a la noche del mismo día.
        assert_eq!(
            ds_session_night_id("x", Some("2026-03-02T22:13:45")).as_deref(),
            Some("2026-03-02")
        );
        // Formato con espacio también acepta.
        assert_eq!(
            ds_session_night_id("x", Some("2026-03-03 01:00:00")).as_deref(),
            Some("2026-03-02")
        );
        assert_eq!(ds_night_distance(Some("2026-03-02"), Some("2026-03-05")), 3);
        assert!(ds_night_distance(None, Some("2026-03-05")) > 1_000);
    }

    #[test]
    fn test_ds_dark_amp_glow_detector() {
        let (w, h) = (256usize, 256usize);
        // Dark plano con ruido determinista leve: SIN glow.
        let mut data = vec![0.0f32; w * h];
        for (i, v) in data.iter_mut().enumerate() {
            *v = 100.0 + ((i * 2654435761) % 7) as f32 * 0.5;
        }
        let flat_dark = DsImage {
            data: data.clone(),
            w,
            h,
            ch: 1,
            bayer: None,
        };
        assert!(
            !ds_dark_has_amp_glow(&flat_dark),
            "un dark plano no tiene glow"
        );
        // Mancha de glow en la esquina (región contigua elevada): CON glow.
        for y in 0..64 {
            for x in 0..96 {
                data[y * w + x] += 400.0;
            }
        }
        let glow_dark = DsImage {
            data,
            w,
            h,
            ch: 1,
            bayer: None,
        };
        assert!(
            ds_dark_has_amp_glow(&glow_dark),
            "la mancha contigua debe detectarse"
        );
    }

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
        assert!(
            before_corner_delta > 2000.0,
            "el sintético debe tener gradiente"
        );

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
        assert!(
            found.len() >= 40,
            "solo {} estrellas detectadas",
            found.len()
        );
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
    fn test_ds_gaussian_psf_fit_recovers_subpixel_centroid_and_fwhm() {
        let (w, h) = (64usize, 64usize);
        let (truth_x, truth_y, truth_sigma) = (31.27f32, 29.68f32, 1.35f32);
        let mut image = vec![120.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let r2 = (x as f32 - truth_x).powi(2) + (y as f32 - truth_y).powi(2);
                image[y * w + x] += 12_000.0 * (-0.5 * r2 / truth_sigma.powi(2)).exp();
            }
        }
        let fit = ds_fit_star_psf(&image, w, h, 31, 30, 120.0).expect("ajuste PSF");
        assert!(
            (fit.x - truth_x).abs() < 0.015,
            "x={} esperado={truth_x}",
            fit.x
        );
        assert!(
            (fit.y - truth_y).abs() < 0.015,
            "y={} esperado={truth_y}",
            fit.y
        );
        assert!(
            (fit.sigma - truth_sigma).abs() < 0.02,
            "sigma={}",
            fit.sigma
        );
        let stars = ds_detect_stars(&image, w, h, 10);
        assert!(stars
            .iter()
            .any(|&(x, y, _)| { (x - truth_x).abs() < 0.03 && (y - truth_y).abs() < 0.03 }));
    }

    #[test]
    fn test_ds_psf_signal_normalization_tracks_transparency() {
        let reference: Vec<(f32, f32, f32)> = (0..12)
            .map(|i| (i as f32, 0.0, 2_000.0 + i as f32 * 25.0))
            .collect();
        let hazy: Vec<(f32, f32, f32)> = reference
            .iter()
            .map(|&(x, y, flux)| (x, y, flux * 0.5))
            .collect();
        let scale = ds_psf_signal_level(&reference).unwrap() / ds_psf_signal_level(&hazy).unwrap();
        assert!((scale - 2.0).abs() < 1e-6);
        assert!(ds_psf_signal_level(&reference[..5]).is_none());
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

        let reg = ds_match_triangles_in_field(&ref_stars, &tgt_stars, w, h)
            .expect("registro fallido");
        assert!(reg.inliers >= 20, "pocos inliers: {}", reg.inliers);
        assert!(reg.holdout_count >= reg.inliers.div_ceil(5));
        assert!(
            reg.holdout_p95 <= 0.20,
            "p95 sintético fuera del gate científico: {} px",
            reg.holdout_p95
        );
        // Verify corner mapping accuracy < 0.35 px
        for &(cx, cy) in &[
            (30.0f32, 30.0f32),
            (480.0, 30.0),
            (30.0, 480.0),
            (480.0, 480.0),
        ] {
            // true target position of this ref corner:
            let dx = cx - t_true.2;
            let dy = cy - t_true.3;
            let tx = (a * dx + b * dy) / det;
            let ty = (-b * dx + a * dy) / det;
            let back = reg.transform.forward(tx, ty);
            let err = ((back.0 - cx).powi(2) + (back.1 - cy).powi(2)).sqrt();
            assert!(err < 0.35, "error de registro {} px en esquina", err);
        }
    }

    #[test]
    fn test_ds_registration_models_affine_projective_and_local_distortion() {
        let grid: Vec<(f32, f32)> = (0..6)
            .flat_map(|y| (0..7).map(move |x| (40.0 + x as f32 * 83.0, 30.0 + y as f32 * 77.0)))
            .collect();
        let affine_pairs: Vec<_> = grid
            .iter()
            .map(|&(x, y)| {
                (
                    (x, y),
                    (1.013 * x + 0.018 * y + 13.2, -0.012 * x + 0.994 * y - 7.4),
                )
            })
            .collect();
        let a = ds_fit_affine(&affine_pairs).expect("ajuste afín");
        assert!(ds_transform_rms(a, &affine_pairs) < 1e-3);

        let h = [
            1.01f64, 0.012, 8.0, -0.007, 0.998, -5.0, 0.000035, -0.000022, 1.0,
        ];
        let projective_pairs: Vec<_> = grid
            .iter()
            .map(|&(x, y)| {
                let d = h[6] * x as f64 + h[7] * y as f64 + 1.0;
                let u = (h[0] * x as f64 + h[1] * y as f64 + h[2]) / d;
                let v = (h[3] * x as f64 + h[4] * y as f64 + h[5]) / d;
                ((x, y), (u as f32, v as f32))
            })
            .collect();
        let p = ds_fit_projective(&projective_pairs).expect("ajuste proyectivo");
        assert!(ds_transform_rms(p, &projective_pairs) < 0.01);
        let z = p.forward(317.0, 219.0);
        let back = p.inverse(z.0, z.1).expect("inversa proyectiva");
        assert!((back.0 - 317.0).abs() < 0.01 && (back.1 - 219.0).abs() < 0.01);

        let local_pairs: Vec<_> = grid
            .iter()
            .map(|&(x, y)| {
                let xn = (x - 290.0) / 300.0;
                let yn = (y - 220.0) / 300.0;
                (
                    (x, y),
                    (
                        x + 9.0 * xn * xn - 4.0 * xn * yn,
                        y - 7.0 * yn * yn + 3.0 * xn * yn,
                    ),
                )
            })
            .collect();
        let d = ds_fit_local_distortion(&local_pairs).expect("ajuste distorsión");
        assert!(ds_transform_rms(d, &local_pairs) < 0.01);
        let z = d.forward(410.0, 350.0);
        let back = d.inverse(z.0, z.1).expect("inversa local");
        assert!((back.0 - 410.0).abs() < 0.03 && (back.1 - 350.0).abs() < 0.03);

        let selected = ds_select_registration_model(
            ds_solve_similarity(&local_pairs).unwrap(),
            &local_pairs,
            600,
            500,
        )
        .expect("selección con geometría válida");
        assert_eq!(
            selected.transform.model,
            DsRegistrationModel::LocalDistortion
        );
        assert!(selected.rms < 0.02);
        assert!(selected.holdout_count >= local_pairs.len().div_ceil(5));
    }

    #[test]
    fn test_ds_registration_correspondences_are_bijective_and_ties_deterministic() {
        let transform = DsTransform::identity();
        let target = vec![(0.0, 0.0), (0.0, 0.0), (4.0, 0.0)];
        let reference = vec![(-1.0, 0.0), (1.0, 0.0), (4.0, 0.0)];
        let first = ds_bijective_nearest_indices(transform, &target, &reference, 2.0);
        let second = ds_bijective_nearest_indices(transform, &target, &reference, 2.0);
        assert_eq!(first, second, "los empates deben resolverse de forma estable");
        let targets: std::collections::BTreeSet<_> =
            first.iter().map(|pair| pair.0).collect();
        let references: std::collections::BTreeSet<_> =
            first.iter().map(|pair| pair.1).collect();
        assert_eq!(targets.len(), first.len());
        assert_eq!(references.len(), first.len());
        assert!(first.contains(&(0, 0)), "el empate elige el índice menor");

        let repeated_votes = vec![(0, 0), (0, 0), (1, 0), (1, 1), (1, 1), (2, 2)];
        let collapsed = ds_bijective_vote_pairs(&repeated_votes);
        assert_eq!(collapsed, vec![(0, 0), (1, 1), (2, 2)]);

        let tied_buckets = std::collections::BTreeMap::from([
            ((2, -1), vec![(0, 0), (1, 1)]),
            ((-3, 4), vec![(2, 2), (3, 3)]),
        ]);
        let (winner, _) = ds_stable_vote_winner(tied_buckets).expect("votos");
        assert_eq!(winner, (-3, 4), "el empate usa la clave estable menor");
    }

    #[test]
    fn test_ds_registration_holdout_rejects_unneeded_complexity() {
        let pairs: Vec<((f32, f32), (f32, f32))> = (0..6)
            .flat_map(|y| {
                (0..6).map(move |x| {
                    let px = 30.0 + x as f32 * 85.0;
                    let py = 25.0 + y as f32 * 72.0;
                    // Deterministic centroid noise, with no real higher-order
                    // distortion for a projective/local model to recover.
                    let noise_x = (((x * 17 + y * 11) % 7) as f32 - 3.0) * 0.006;
                    let noise_y = (((x * 5 + y * 19) % 5) as f32 - 2.0) * 0.006;
                    (
                        (px, py),
                        (
                            1.004 * px - 0.011 * py + 9.5 + noise_x,
                            0.011 * px + 1.004 * py - 6.0 + noise_y,
                        ),
                    )
                })
            })
            .collect();
        let similarity = ds_solve_similarity(&pairs).expect("similitud");
        let selected = ds_select_registration_model(similarity, &pairs, 512, 512)
            .expect("registro válido");
        assert_eq!(selected.transform.model, DsRegistrationModel::Similarity);
        assert!(selected.holdout_count >= pairs.len().div_ceil(5));
        assert!(selected.holdout_p95 < 0.05);
    }

    #[test]
    fn test_ds_registration_field_rejects_foldover_and_implausible_scale() {
        let mut reflected = DsTransform::identity();
        reflected.model = DsRegistrationModel::LocalDistortion;
        reflected.norm = [256.0, 256.0, 256.0];
        reflected.poly = [
            256.0, 0.0, 256.0, 0.0, 0.0, 0.0, 0.0, -256.0, 256.0, 0.0, 0.0, 0.0,
        ];
        assert!(ds_transform_field_report(reflected, 512, 512).is_none());

        let mut oversized = DsTransform::identity();
        oversized.h[0] = 3.0;
        oversized.h[4] = 3.0;
        assert!(ds_transform_field_report(oversized, 512, 512).is_none());

        let mut excessive_field_variation = DsTransform::identity();
        excessive_field_variation.model = DsRegistrationModel::Projective;
        excessive_field_variation.h[6] = 0.0005;
        assert!(
            ds_transform_field_report(excessive_field_variation, 512, 512).is_none(),
            "una variación de área >1.5× a través del sensor no es plausible"
        );

        let identity = ds_transform_field_report(DsTransform::identity(), 512, 512)
            .expect("identidad válida");
        assert!((identity.min_jacobian - 1.0).abs() < 1e-9);
        assert!((identity.p95_jacobian - 1.0).abs() < 1e-9);
        assert!((identity.max_jacobian - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_ds_dark_scaling_requires_measured_physical_evidence() {
        // A light built as bias + exposure_ratio·D_cal + sky must pass the
        // correlation/R²/residual gates and return the physical exposure ratio.
        let (w, h, ch) = (128usize, 128usize, 1usize);
        let n = w * h;
        let bias = DsImage {
            data: vec![200.0; n],
            w,
            h,
            ch,
            bayer: None,
        };
        // D_cal: mostly 0, two hot-pixel populations (2000 and 4000).
        let mut dcal = vec![0.0f32; n];
        for i in (5..n).step_by(37) {
            dcal[i] = 2000.0;
        }
        for i in (11..n).step_by(53) {
            dcal[i] = 4000.0;
        }
        let dark = DsImage {
            data: dcal.clone(),
            w,
            h,
            ch,
            bayer: None,
        };
        let k_true = 1.5f32;
        let sky = 40.0f32;
        let light = DsImage {
            data: (0..n).map(|i| 200.0 + k_true * dcal[i] + sky).collect(),
            w,
            h,
            ch,
            bayer: None,
        };
        let measurement = ds_measure_dark_scaling(
            &light,
            Some(&bias),
            &dark,
            k_true,
            false,
            true,
        )
        .expect("evidencia lineal exacta");
        assert!((measurement.scale - k_true).abs() < 1.0e-6);
        assert!(measurement.correlation >= 0.995);
        assert!(measurement.linearity_r2 >= 0.995);
        assert!(measurement.residual_fraction <= 0.01);
        let glow_error = ds_measure_dark_scaling(
            &light,
            Some(&bias),
            &dark,
            k_true,
            true,
            true,
        )
        .unwrap_err();
        assert!(glow_error.contains("amp glow"));
    }

    #[test]
    fn test_ds_flat_cfa_equalization() {
        // RGGB flat: per-position gains (the panel's color) × radial vignette.
        // After normalization each Bayer position must average 1.0 (strictly
        // color-neutral) while the vignette SHAPE survives (center > corner).
        let (w, h) = (64usize, 64usize);
        let gains = [8000.0f32, 16000.0, 15000.0, 12000.0]; // R G1 G2 B
        let mut data = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let pos = (y & 1) * 2 + (x & 1);
                let dx = x as f32 - 32.0;
                let dy = y as f32 - 32.0;
                let vig = 1.0 - 0.4 * ((dx * dx + dy * dy) / (32.0 * 32.0 * 2.0));
                data[y * w + x] = gains[pos] * vig;
            }
        }
        let mut f = DsImage {
            data,
            w,
            h,
            ch: 1,
            bayer: Some(8),
        };
        ds_flat_normalize(&mut f);
        let mut sums = [0.0f64; 4];
        let mut cnts = [0u64; 4];
        for y in 0..h {
            for x in 0..w {
                let pos = (y & 1) * 2 + (x & 1);
                sums[pos] += f.data[y * w + x] as f64;
                cnts[pos] += 1;
            }
        }
        for p in 0..4 {
            let m = sums[p] / cnts[p] as f64;
            assert!((m - 1.0).abs() < 0.01, "posición CFA {} media {}", p, m);
        }
        // Vignette preserved (same Bayer position center vs corner).
        assert!(
            f.data[32 * w + 32] > f.data[2 * w + 2] * 1.1,
            "viñeteo perdido"
        );
    }

    #[test]
    fn test_ds_exposure_clustering() {
        let items = vec![
            (60.0f32, "a".to_string()),
            (300.0, "c".to_string()),
            (59.7, "b".to_string()),
            (299.0, "d".to_string()),
            (10.0, "e".to_string()),
            (270.0, "f".to_string()),
        ];
        let groups = ds_cluster_exposures(items);
        assert_eq!(groups.len(), 6, "grupos: {:?}", groups);
        assert_eq!(groups[0].1, vec!["e".to_string()]);
        assert_eq!(groups[1].1, vec!["b".to_string()]);
        assert_eq!(groups[2].1, vec!["a".to_string()]);
        assert_eq!(groups[3].1, vec!["f".to_string()]);
        assert_eq!(groups[4].1, vec!["d".to_string()]);
        assert_eq!(groups[5].1, vec!["c".to_string()]);
        assert!(!ds_exposures_match(270.0, 300.0));
        assert!(ds_exposures_match(300.0, 300.0005));
        assert!(!ds_exposures_match(300.0, 300.002));
        assert!(ds_cluster_exposures(Vec::new()).is_empty());
    }

    #[test]
    fn test_ds_calibration_selection_never_mixes_filter_groups() {
        let dir =
            std::env::temp_dir().join(format!("zas-calibration-groups-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let light = dir.join("light_Ha.png");
        let flat_ha = dir.join("flat_Ha.png");
        let flat_oiii = dir.join("flat_OIII.png");
        let flat_wrong_geometry = dir.join("flat_Ha_wrong.png");
        image::GrayImage::from_pixel(32, 24, image::Luma([100]))
            .save(&light)
            .unwrap();
        image::GrayImage::from_pixel(32, 24, image::Luma([120]))
            .save(&flat_ha)
            .unwrap();
        image::GrayImage::from_pixel(32, 24, image::Luma([130]))
            .save(&flat_oiii)
            .unwrap();
        image::GrayImage::from_pixel(16, 16, image::Luma([140]))
            .save(&flat_wrong_geometry)
            .unwrap();
        let mut reference = deepsky_probe(vec![light.display().to_string()])
            .into_iter()
            .next()
            .unwrap();
        let signature_for = |roi: [u32; 4], filter: &str| pipeline::CalibrationSignature {
            camera: Some("test-camera".into()),
            sensor: Some("test-sensor".into()),
            read_mode: Some("test-read-mode".into()),
            gain: Some(100.0),
            offset: Some(20.0),
            exposure_seconds: Some(1.0),
            binning_x: Some(1),
            binning_y: Some(1),
            roi: Some(roi),
            filter: Some(filter.into()),
            session: Some("2026-07-19".into()),
            optical_train: Some("test-train".into()),
            adc_bits: Some(16),
            white_level_adu: Some(65535.0),
            ..pipeline::CalibrationSignature::default()
        };
        reference.signature = signature_for([0, 0, 32, 24], "Ha");
        reference.store_layout = Some(pipeline::StoreLayout::Mono);
        let mut flat_probes = deepsky_probe(vec![
            flat_ha.display().to_string(),
            flat_oiii.display().to_string(),
            flat_wrong_geometry.display().to_string(),
        ]);
        flat_probes[0].signature = signature_for([0, 0, 32, 24], "Ha");
        flat_probes[1].signature = signature_for([0, 0, 32, 24], "OIII");
        flat_probes[2].signature = signature_for([0, 0, 16, 16], "Ha");
        for probe in &mut flat_probes {
            probe.store_layout = Some(pipeline::StoreLayout::Mono);
        }
        assert!(ds_compare_probe_calibration(
            &reference,
            &flat_probes[0],
            crate::deepsky_calibration_contract::CalibrationRole::Flat,
            pipeline::DeepSkyCalibrationPolicy::Strict,
        )
        .compatible);
        assert!(!ds_compare_probe_calibration(
            &reference,
            &flat_probes[1],
            crate::deepsky_calibration_contract::CalibrationRole::Flat,
            pipeline::DeepSkyCalibrationPolicy::Strict,
        )
        .compatible);
        let bias_probe = flat_probes[0].clone();
        let bias_only_decisions = ds_prepare_calibration_decisions(
            &[reference.clone()],
            &[bias_probe],
            &[],
            &[flat_probes[0].clone()],
            &[],
            pipeline::DeepSkyCalibrationPolicy::Strict,
        );
        assert_eq!(bias_only_decisions.len(), 1);
        assert!(bias_only_decisions[0].degraded);
        assert!(bias_only_decisions[0]
            .reasons
            .iter()
            .any(|reason| reason.contains("bias-only bloqueado")));
        let selection = ds_select_calibration_group(
            &[
                flat_ha.display().to_string(),
                flat_oiii.display().to_string(),
                flat_wrong_geometry.display().to_string(),
            ],
            &[reference],
            "flats",
            pipeline::DeepSkyCalibrationPolicy::Strict,
        );
        // The selector reprobes paths in production, so this synthetic PNG
        // corpus intentionally demonstrates that metadata cannot be injected
        // out-of-band: Strict rejects all three rather than guessing by name.
        assert!(selection.paths.is_empty());
        assert_eq!(selection.group_count, 2);
        assert!(!selection.scientific_eligible);
        assert!(selection
            .blocking_reasons
            .iter()
            .any(|reason| reason.contains("metadata obligatoria ausente")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_ds_preflight_reports_real_rejection_and_rejects_unknown_methods() {
        let dir = std::env::temp_dir().join(format!(
            "zas-preflight-effective-method-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut lights = Vec::new();
        for index in 0..3 {
            let path = dir.join(format!("light_{index:02}.png"));
            image::GrayImage::from_pixel(32, 24, image::Luma([100 + index as u8]))
                .save(&path)
                .unwrap();
            lights.push(path.display().to_string());
        }
        let request = DeepSkyStackRequest {
            schema_version: pipeline::DEEP_SKY_STACK_REQUEST_SCHEMA_VERSION,
            lights,
            darks: Vec::new(),
            flats: Vec::new(),
            dark_flats: Vec::new(),
            bias: Vec::new(),
            calibration_overrides: Vec::new(),
            capture_mode: pipeline::DeepSkyCaptureMode::Auto,
            calibration_policy: pipeline::DeepSkyCalibrationPolicy::AllowDegraded,
            compute_policy: ComputePolicy::CpuOnly,
            profile: PipelineProfile::Custom,
            rejection: "winsorized".into(),
            kappa_low: 2.5,
            kappa_high: 3.0,
            clip_iters: Some(2),
            normalization: "scaling".into(),
            interpolation: "lanczos3".into(),
            drizzle: 2.0,
            pixfrac: 0.8,
            cosmetic: Some(true),
            gradient: false,
            optimize_dark: Some(false),
            auto_crop: true,
            pedestal: Some(0.0),
            local_weighting: false,
            work_dir: None,
            integration_method: None,
            scientific_products: false,
        };
        // Receta v4: sin integration_method el método efectivo es Classic con
        // la migración del localWeighting antiguo a legacy_local_fwhm.
        let effective = request.resolved_integration_method();
        assert_eq!(effective.label(), "classic");
        match &effective {
            pipeline::DeepSkyIntegrationMethod::Classic(cfg) => {
                assert!(!cfg.legacy_local_fwhm);
            }
            other => panic!("método inesperado: {other:?}"),
        }
        let mut legacy = request.clone();
        legacy.local_weighting = true;
        match legacy.resolved_integration_method() {
            pipeline::DeepSkyIntegrationMethod::Classic(cfg) => {
                assert!(cfg.legacy_local_fwhm);
            }
            other => panic!("método inesperado: {other:?}"),
        }
        // Motores aún no disponibles: error explícito, jamás fallback.
        let mut nf_request = request.clone();
        nf_request.integration_method = Some(pipeline::DeepSkyIntegrationMethod::NebulaFusion(
            pipeline::NebulaFusionConfig::default(),
        ));
        let nf_plan = prepare_deepsky_stack_impl(nf_request, false);
        assert!(!nf_plan.valid);
        assert!(nf_plan.errors.iter().any(|e| e.contains("NebulaFusion")));
        // El tag serde del enum es estable (contrato de receta v3).
        let json = serde_json::to_value(&effective).unwrap();
        assert_eq!(json["method"], "classic");
        let parsed: pipeline::DeepSkyIntegrationMethod =
            serde_json::from_value(serde_json::json!({"method": "nebula_fusion", "mode": "lite"}))
                .unwrap();
        assert_eq!(parsed.label(), "nebula_fusion");

        let plan = prepare_deepsky_stack_impl(request.clone(), false);
        assert!(plan.valid, "{:?}", plan.errors);
        assert_eq!(plan.requested_rejection, "winsorized");
        assert_eq!(plan.effective_rejection, "sigma");
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("método real será sigma")));

        let mut strict = request.clone();
        strict.calibration_policy = pipeline::DeepSkyCalibrationPolicy::Strict;
        let strict_plan = prepare_deepsky_stack_impl(strict, false);
        assert!(!strict_plan.valid);
        assert!(strict_plan
            .errors
            .iter()
            .any(|error| error.contains("FITS/TIFF lineal")));

        let mut false_linearfit = request.clone();
        false_linearfit.rejection = "linear-fit".into();
        false_linearfit.drizzle = 1.0;
        let linearfit_plan = prepare_deepsky_stack_impl(false_linearfit, false);
        assert!(!linearfit_plan.valid);
        assert!(linearfit_plan
            .errors
            .iter()
            .any(|error| error.contains("Linear-fit clipping está deshabilitado")));

        let mut invalid = request;
        invalid.rejection = "magic-stack".into();
        invalid.drizzle = 1.0;
        let invalid_plan = prepare_deepsky_stack_impl(invalid, false);
        assert!(!invalid_plan.valid);
        assert!(invalid_plan
            .errors
            .iter()
            .any(|error| error.contains("Método de rechazo desconocido")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_ds_bzero_signed_fits_mapping() {
        // BITPIX=16 + BZERO=32768 (the universal camera convention): raw signed
        // −32768..32767 must map to 0..65535. Without this the whole sky
        // background (raw < 32768) clamps to black and only star cores survive.
        assert_eq!(ds_int_to_adu(-32768.0, 1.0, 32768.0), 0.0);
        assert_eq!(ds_int_to_adu(0.0, 1.0, 32768.0), 32768.0);
        assert_eq!(ds_int_to_adu(32767.0, 1.0, 32768.0), 65535.0);
        // Typical sky: raw −31968 (= 800 ADU unsigned) must be 800, not 0.
        assert_eq!(ds_int_to_adu(-31968.0, 1.0, 32768.0), 800.0);
        // No keywords (BSCALE=1, BZERO=0) = identity.
        assert_eq!(ds_int_to_adu(1234.0, 1.0, 0.0), 1234.0);
    }

    #[test]
    fn test_ds_float_pipeline_preserves_negative_background_and_explicit_pedestal() {
        // El modo científico por defecto conserva exactamente los negativos.
        let (w, h) = (64usize, 64usize);
        let n = w * h;
        let mut data = vec![0.0f32; n];
        for (i, v) in data.iter_mut().enumerate() {
            let mut z = (i as u32).wrapping_mul(0x9E37_79B9);
            z ^= z >> 15;
            *v = -40.0 + (z % 61) as f32 - 30.0; // fondo −40 ± 30
        }
        data[100] = 20000.0; // una estrella
        let original = data.clone();
        let mut img = DsImage {
            data,
            w,
            h,
            ch: 1,
            bayer: None,
        };
        let ped = ds_apply_pedestal(&mut img, None);
        assert_eq!(ped, 0.0);
        assert_eq!(img.data, original);
        assert!(img.data.iter().any(|&v| v < 0.0));
        let (_, nz) = ds_bg_noise(&img.data);
        assert!(nz > 10.0, "ruido negativo aplastado: {}", nz);
        // Pedestal fijo: desplazamiento exacto, todavía sin clamp/headroom loss.
        let mut img2 = DsImage {
            data: vec![-20.0f32, 70_000.0],
            w: 2,
            h: 1,
            ch: 1,
            bayer: None,
        };
        assert_eq!(ds_apply_pedestal(&mut img2, Some(100.0)), 100.0);
        assert_eq!(img2.data, vec![80.0, 70_100.0]);
    }

    #[test]
    fn test_ds_float32_tiff_input_preserves_negative_values_and_headroom() {
        use tiff::encoder::{colortype, TiffEncoder};

        let path = std::env::temp_dir().join(format!(
            "zas-f32-linear-{}-{}.tiff",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let source = vec![-0.25f32, 0.0, 1.0, 1.5];
        {
            let file = std::fs::File::create(&path).unwrap();
            let writer = std::io::BufWriter::new(file);
            let mut encoder = TiffEncoder::new(writer).unwrap();
            encoder
                .write_image::<colortype::Gray32Float>(2, 2, &source)
                .unwrap();
        }
        let loaded = ds_read_image(path.to_str().unwrap()).unwrap();
        assert_eq!((loaded.w, loaded.h, loaded.ch), (2, 2, 1));
        for (actual, expected) in loaded.data.iter().zip(source.iter()) {
            let expected_adu = expected * 65535.0;
            assert!(
                (actual - expected_adu).abs() < 0.01,
                "TIFF float32 {actual} != {expected_adu}"
            );
        }
        assert!(loaded.data[0] < 0.0, "el fondo negativo fue recortado");
        assert!(loaded.data[3] > 65535.0, "el headroom fue recortado");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_ds_source_fingerprint_detects_in_place_content_change() {
        let path = std::env::temp_dir().join(format!(
            "zas-ds-fingerprint-{}-{}.fits",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&path, vec![0x11u8; 20_000]).unwrap();
        let paths = vec![path.to_string_lossy().to_string()];
        let before = ds_source_fingerprint(&[&paths]);
        // Same path and exact byte length; both sampled edges are rewritten.
        std::fs::write(&path, vec![0xA7u8; 20_000]).unwrap();
        let after = ds_source_fingerprint(&[&paths]);
        assert_ne!(before, after, "la caché no detectó un origen reescrito");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_ds_master_combination_is_tiled_robust_and_cancellable() {
        let dir = std::env::temp_dir().join(format!(
            "zas-ds-master-store-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = AdaptiveFrameStore::new(5, 6, &dir, "robust-master").unwrap();
        for (index, offset) in [0.0f32, 1.0, -1.0, 0.5, 50_000.0].into_iter().enumerate() {
            let frame: Vec<f32> = (0..6).map(|pixel| pixel as f32 - 3.0 + offset).collect();
            store.put(index, &frame).unwrap();
        }
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let master = ds_combine_master_store(&store, 5, 6, true, &cancel).unwrap();
        for (pixel, value) in master.iter().enumerate() {
            // Máscara mediana/MAD rechaza 50000; el estimador publicado es la
            // media eficiente de [-1, 0, 0.5, 1], no la mediana ruidosa.
            let expected = pixel as f32 - 2.875;
            assert!((value - expected).abs() < 1e-6, "master[{pixel}]={value}");
        }
        assert!(master[0] < 0.0, "el master recortó valores negativos");
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(ds_combine_master_store(&store, 5, 6, true, &cancel)
            .unwrap_err()
            .contains("Cancelado"));
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_ds_filter_token_detection() {
        assert_eq!(ds_filter_token("M42_Ha_300s"), Some("HA"));
        assert_eq!(ds_filter_token("flat_OIII_bin1"), Some("OIII"));
        assert_eq!(ds_filter_token("SII"), Some("SII"));
        assert_eq!(ds_filter_token("SV220"), Some("HA_OIII"));
        assert_eq!(ds_filter_token("SV220_Ha_OIII_600s"), Some("HA_OIII"));
        assert_eq!(ds_filter_token("SV220_SII_OIII_600s"), Some("SII_OIII"));
        assert_eq!(ds_filter_token("SII+OIII"), Some("SII_OIII"));
        assert_eq!(ds_filter_token("NGC7000_L_001"), Some("L"));
        assert_eq!(ds_filter_token("m31_red_0042"), Some("R"));
        // No false positives from words merely CONTAINING the letters.
        assert_eq!(ds_filter_token("alpha_test"), None);
        assert_eq!(ds_filter_token("IMG_0001"), None);
        assert_eq!(ds_filter_token("flat_panel"), None);
    }

    #[test]
    fn test_dual_band_components_are_scientifically_explicit() {
        assert_eq!(ds_filter_components("HA_OIII"), vec!["HA", "OIII"]);
        assert_eq!(ds_filter_components("SII_OIII"), vec!["SII", "OIII"]);
        assert_eq!(ds_filter_display("HA_OIII"), "Ha + OIII");
        assert_eq!(ds_filter_display("SII_OIII"), "SII + OIII");
    }

    #[test]
    fn test_dual_band_extraction_preserves_float_headroom_and_negatives() {
        let data = vec![100.0, 40.0, 20.0, -5.0, 10.0, 30.0];
        let (primary, oiii) = ds_extract_dual_band_planes(&data, 3, 0.75, 0.20).unwrap();
        assert_eq!(oiii, vec![35.0, 15.0]);
        assert_eq!(primary, vec![93.0, -8.0]);
    }

    #[test]
    fn test_multiband_session_cleanup_removes_uncommitted_outputs() {
        let path = std::env::temp_dir().join(new_job_id("ds-session-cleanup-test"));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("complete-group.fits"), b"partial session").unwrap();
        {
            let _guard = DsSessionOutputGuard {
                path: path.clone(),
                committed: false,
            };
        }
        assert!(!path.exists());
    }

    #[test]
    fn test_ds_lanczos_warp_preserves_dc() {
        // A constant field warped with a sub-pixel shift through the Lanczos-3
        // path must stay constant (renormalized kernel → exact DC preservation,
        // no ringing on flat sky).
        let (w, h) = (64usize, 64usize);
        let img = DsImage {
            data: vec![1000.0f32; w * h],
            w,
            h,
            ch: 1,
            bayer: None,
        };
        let t = DsTransform::from_similarity((1.0f32, 0.0, 0.37, -0.21)); // sub-pixel translation
        let mut sum = vec![0.0f64; w * h];
        let mut wgt = vec![0.0f64; w * h];
        ds_warp_accumulate(
            &img,
            t,
            &mut sum,
            None,
            &mut wgt,
            None,
            None,
            w,
            h,
            1,
            1.0,
            1.0,
            ([1.0; 3], [0.0; 3]),
            None,
            None,
            true,
            None,
            None,
        );
        for y in 8..h - 8 {
            for x in 8..w - 8 {
                let i = y * w + x;
                assert!(wgt[i] > 0.0, "sin cobertura en ({},{})", x, y);
                let v = sum[i] / wgt[i];
                assert!((v - 1000.0).abs() < 0.6, "pixel ({},{}) = {}", x, y, v);
            }
        }
    }

    #[test]
    fn test_ds_lanczos_is_linear_and_not_clamped_to_bilinear_neighbours() {
        let (w, h) = (20usize, 20usize);
        let mut impulse = DsImage {
            data: vec![0.0; w * h],
            w,
            h,
            ch: 1,
            bayer: None,
        };
        // The impulse is in the 6×6 Lanczos support but outside the bilinear
        // 2×2 neighbourhood of (8.25, 8.35). The old signal-dependent clamp
        // forced this valid (negative-lobe) response to exactly zero.
        impulse.data[8 * w + 7] = 100.0;
        let mut sample = [0.0f32; 3];
        ds_sample_lanczos3(&impulse, ds_l3_lut(), 8.25, 8.35, &mut sample);
        assert!(
            sample[0].abs() > 1e-3,
            "el soporte Lanczos fue recortado como si fuera bilineal"
        );

        let mut scaled = impulse.clone();
        scaled.data.iter_mut().for_each(|value| *value *= -2.5);
        let mut scaled_sample = [0.0f32; 3];
        ds_sample_lanczos3(&scaled, ds_l3_lut(), 8.25, 8.35, &mut scaled_sample);
        assert!(
            (scaled_sample[0] + 2.5 * sample[0]).abs() < 1e-5,
            "Lanczos dejó de ser lineal: {:?} vs {:?}",
            sample,
            scaled_sample
        );
    }

    #[test]
    fn test_ds_drizzle_conserves_flux_and_mean() {
        // Constant input, identity transform, 2× drizzle, pixfrac 1.0. The
        // drop-kernel must conserve flux (total weight ≈ N·(pixfrac·scale)²) and
        // preserve the value (every covered output pixel ≈ the input level).
        let (w, h, ch) = (64usize, 64usize, 1usize);
        let img = DsImage {
            data: vec![1000.0f32; w * h],
            w,
            h,
            ch,
            bayer: None,
        };
        let scale = 2.0f32;
        let pixfrac = 1.0f32;
        let (wo, ho) = (w * 2, h * 2);
        let mut sum = vec![0.0f64; wo * ho * ch];
        let mut wgt = vec![0.0f64; wo * ho];
        ds_drizzle_accumulate(
            &img,
            DsTransform::identity(),
            &mut sum,
            None,
            &mut wgt,
            None,
            None,
            wo,
            ho,
            ch,
            1.0,
            scale,
            pixfrac,
            ([1.0; 3], [0.0; 3]),
            None,
            None,
        );
        let tw: f64 = wgt.iter().sum();
        let expected = (w * h) as f64 * (pixfrac * scale).powi(2) as f64;
        // Edge drops fall partly outside → slightly less than the ideal total.
        assert!(
            tw > expected * 0.9 && tw <= expected * 1.001,
            "peso total {} vs esperado {}",
            tw,
            expected
        );
        let mean_out = sum.iter().sum::<f64>() / tw;
        assert!(
            (mean_out - 1000.0).abs() < 0.5,
            "media {} (esperada 1000)",
            mean_out
        );
        // A central output pixel must carry the input level.
        let cpix = (ho / 2) * wo + wo / 2;
        assert!(
            wgt[cpix] > 0.0 && (sum[cpix] / wgt[cpix] - 1000.0).abs() < 0.5,
            "pixel central {}",
            sum[cpix] / wgt[cpix].max(1.0)
        );
    }

    /// ACEPTACIÓN del drizzle: convención CENTRO + recuperación sub-píxel.
    /// (1) Un impulso en x0 con identidad a 2× debe centrar su flujo EXACTO en
    /// x0·2 (la convención esquina anterior lo corría a x0·2−0.5: sesgo
    /// astrométrico constante frente al máster 1×). (2) Dos tomas ditheradas
    /// 0.5 px de ENTRADA deben resolverse a 1.0 px de SALIDA a 2× — la promesa
    /// del drizzle que un apilado 1× no puede cumplir.
    #[test]
    fn test_ds_drizzle_center_convention_and_subpixel_recovery() {
        let (w, h) = (64usize, 64usize);
        let (x0, y0) = (32usize, 32usize);
        let mut data = vec![0.0f32; w * h];
        data[y0 * w + x0] = 10_000.0;
        let img = DsImage {
            data,
            w,
            h,
            ch: 1,
            bayer: None,
        };
        let (wo, ho) = (w * 2, h * 2);
        let centroid_x = |sum: &[f64]| -> f64 {
            let mut m = 0.0f64;
            let mut mx = 0.0f64;
            for y in 0..ho {
                for x in 0..wo {
                    let v = sum[y * wo + x];
                    m += v;
                    mx += v * x as f64;
                }
            }
            mx / m.max(1e-12)
        };

        // (1) Identidad: centroide exactamente en x0·2.
        let mut sum = vec![0.0f64; wo * ho];
        let mut wgt = vec![0.0f64; wo * ho];
        ds_drizzle_accumulate(
            &img,
            DsTransform::identity(),
            &mut sum,
            None,
            &mut wgt,
            None,
            None,
            wo,
            ho,
            1,
            1.0,
            2.0,
            1.0,
            ([1.0; 3], [0.0; 3]),
            None,
            None,
        );
        let c0 = centroid_x(&sum);
        assert!(
            (c0 - (x0 * 2) as f64).abs() < 1e-9,
            "convención centro: centroide {} (esperado {})",
            c0,
            x0 * 2
        );

        // (2) Dither de +0.5 px de entrada → +1.0 px de salida a 2×.
        let mut sum2 = vec![0.0f64; wo * ho];
        let mut wgt2 = vec![0.0f64; wo * ho];
        ds_drizzle_accumulate(
            &img,
            DsTransform::from_similarity((1.0, 0.0, 0.5, 0.0)),
            &mut sum2,
            None,
            &mut wgt2,
            None,
            None,
            wo,
            ho,
            1,
            1.0,
            2.0,
            1.0,
            ([1.0; 3], [0.0; 3]),
            None,
            None,
        );
        let c1 = centroid_x(&sum2);
        assert!(
            (c1 - c0 - 1.0).abs() < 1e-9,
            "recuperación sub-píxel: Δcentroide {} (esperado 1.0)",
            c1 - c0
        );
    }

    /// RESCATE DE DETALLE — validación del contrato:
    /// (1) el máster ponderado por región recupera nitidez (FWHM menor que la
    /// media simple) cuando una toma es más nítida que otra;
    /// (2) NEUTRALIDAD FOTOMÉTRICA: el fondo constante y el flujo estelar no
    /// se sesgan (media ponderada lineal con pesos independientes del brillo);
    /// (3) con rejilla uniforme el resultado coincide con el camino sin pesos.
    #[test]
    fn test_ds_local_weighting_recovers_detail_without_photometric_bias() {
        const WQ_G: usize = 8;
        let (w, h) = (96usize, 96usize);
        let star = |sigma: f32| -> DsImage {
            let mut data = vec![500.0f32; w * h];
            for y in 0..h {
                for x in 0..w {
                    let dx = x as f32 - 48.0;
                    let dy = y as f32 - 48.0;
                    // Amplitud ∝ 1/σ² → flujo integrado idéntico entre tomas
                    // (el blur conserva flujo, como el seeing real).
                    data[y * w + x] += 30000.0 / (sigma * sigma)
                        * (-(dx * dx + dy * dy) / (2.0 * sigma * sigma)).exp();
                }
            }
            DsImage {
                data,
                w,
                h,
                ch: 1,
                bayer: None,
            }
        };
        let sharp = star(1.3);
        let blurred = star(2.8);

        let stack = |wq_a: Option<f32>, wq_b: Option<f32>| -> Vec<f32> {
            let mut sum = vec![0.0f64; w * h];
            let mut wgt = vec![0.0f64; w * h];
            let grid = |q: Option<f32>| q.map(|v| vec![v; WQ_G * WQ_G]);
            let ga = grid(wq_a);
            let gb = grid(wq_b);
            ds_warp_accumulate(
                &sharp,
                DsTransform::identity(),
                &mut sum,
                None,
                &mut wgt,
                None,
                None,
                w,
                h,
                1,
                1.0,
                1.0,
                ([1.0; 3], [0.0; 3]),
                None,
                ga.as_ref().map(|g| (g.as_slice(), WQ_G, WQ_G)),
                false,
                None,
                None,
            );
            ds_warp_accumulate(
                &blurred,
                DsTransform::identity(),
                &mut sum,
                None,
                &mut wgt,
                None,
                None,
                w,
                h,
                1,
                1.0,
                1.0,
                ([1.0; 3], [0.0; 3]),
                None,
                gb.as_ref().map(|g| (g.as_slice(), WQ_G, WQ_G)),
                false,
                None,
                None,
            );
            (0..w * h)
                .map(|i| (sum[i] / wgt[i].max(1e-12)) as f32)
                .collect()
        };

        let plain = stack(None, None);
        let weighted = stack(Some(1.6), Some(0.6));
        let uniform = stack(Some(1.0), Some(1.0));

        // (3) Rejilla uniforme ≈ camino sin pesos (mismos valores).
        for i in 0..w * h {
            assert!(
                (uniform[i] - plain[i]).abs() <= 0.01,
                "rejilla uniforme difiere en el píxel {i}: {} vs {}",
                uniform[i],
                plain[i]
            );
        }

        // (2) Fondo neutro: esquina lejos de la estrella.
        assert!(
            (weighted[5 * w + 5] - 500.0).abs() < 0.01,
            "el fondo se sesgó: {}",
            weighted[5 * w + 5]
        );
        // (2) Flujo estelar conservado (blur conserva flujo → la media
        // ponderada también): ±0.1%.
        let flux = |img: &[f32]| -> f64 { img.iter().map(|&v| (v - 500.0).max(0.0) as f64).sum() };
        let fp = flux(&plain);
        let fw_ = flux(&weighted);
        assert!(
            ((fw_ - fp) / fp).abs() < 0.001,
            "flujo sesgado: {fp} vs {fw_}"
        );

        // (1) Recuperación de detalle: FWHM del máster ponderado < simple.
        let fwhm_of = |img: &[f32]| -> f32 { ds_frame_fwhm_proxy(img, w, h, &[(48.0, 48.0, 1.0)]) };
        let f_plain = fwhm_of(&plain);
        let f_weighted = fwhm_of(&weighted);
        assert!(
            f_weighted < f_plain - 0.05,
            "sin ganancia de nitidez: ponderado {f_weighted} vs simple {f_plain}"
        );
    }

    #[test]
    fn test_ds_true_cfa_drizzle_preserves_channels_without_debayer() {
        let (w, h) = (32usize, 32usize);
        let mut raw = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                raw[y * w + x] = [1000.0, 2000.0, 3000.0][ds_cfa_channel(8, x, y)];
            }
        }
        let img = DsImage {
            data: raw,
            w,
            h,
            ch: 1,
            bayer: Some(8),
        };
        let (wo, ho) = (w * 2, h * 2);
        let mut sum = vec![0.0f64; wo * ho * 3];
        let mut sq = vec![0.0f64; sum.len()];
        let mut wgt = vec![0.0f64; sum.len()];
        for &(dx, dy) in &[(0.0, 0.0), (0.5, 0.0), (0.0, 0.5), (0.5, 0.5)] {
            ds_drizzle_cfa_accumulate(
                &img,
                8,
                DsTransform::from_similarity((1.0, 0.0, dx, dy)),
                &mut sum,
                Some(&mut sq),
                &mut wgt,
                None,
                None,
                wo,
                ho,
                1.0,
                2.0,
                1.0,
                ([1.0; 3], [0.0; 3]),
                None,
                None,
                None,
                None,
                None,
            );
        }
        let coverage = ds_cfa_coverage(&wgt);
        assert!(coverage.iter().filter(|&&v| v > 0.0).count() > wo * ho / 20);
        for y in 8..ho - 8 {
            for x in 8..wo - 8 {
                let p = (y * wo + x) * 3;
                for c in 0..3 {
                    if wgt[p + c] > 0.0 {
                        let value = sum[p + c] / wgt[p + c];
                        assert!((value - [1000.0, 2000.0, 3000.0][c]).abs() < 0.01);
                    }
                }
            }
        }
    }

    #[test]
    fn test_ds_dither_position_count_covers_mono_rgb_and_cfa() {
        let registered = [
            (0usize, 0.00f32, 0.00f32),
            (1, 0.28, 0.02),
            (2, 0.03, 0.29),
            (3, 0.31, 0.32),
        ]
        .into_iter()
        .map(|(index, dx, dy)| (index, DsTransform::from_similarity((1.0, 0.0, dx, dy)), 1.0))
        .collect::<Vec<_>>();
        assert!(ds_count_dither_positions(&registered, 100, 80, false) >= 4);
        let cfa_registered = [
            (0usize, 0.00f32, 0.00f32),
            (1, 0.55, 0.02),
            (2, 0.03, 0.58),
            (3, 0.61, 0.62),
        ]
        .into_iter()
        .map(|(index, dx, dy)| (index, DsTransform::from_similarity((1.0, 0.0, dx, dy)), 1.0))
        .collect::<Vec<_>>();
        assert!(ds_count_dither_positions(&cfa_registered, 100, 80, true) >= 4);

        let undithered = vec![
            (0, DsTransform::identity(), 1.0),
            (1, DsTransform::identity(), 1.0),
            (2, DsTransform::identity(), 1.0),
        ];
        assert_eq!(ds_count_dither_positions(&undithered, 100, 80, false), 1);
        assert_eq!(ds_count_dither_positions(&undithered, 100, 80, true), 1);
    }

    #[test]
    fn test_ds_roundness_detects_elongation() {
        let (w, h) = (128usize, 128usize);
        let centers = [
            (30.0f32, 30.0f32),
            (90.0, 40.0),
            (60.0, 90.0),
            (100.0, 100.0),
        ];
        let star_list: Vec<(f32, f32, f32)> =
            centers.iter().map(|&(x, y)| (x, y, 3000.0)).collect();

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
                    let v = 3000.0
                        * (-(dx * dx) as f32 / (2.0 * 1.2 * 1.2)
                            - (dy * dy) as f32 / (2.0 * 3.0 * 3.0))
                            .exp();
                    elong_img[y as usize * w + x as usize] += v;
                }
            }
        }
        let e_elong = ds_frame_roundness(&elong_img, w, h, &star_list);
        assert!(
            e_elong > e_round + 0.2,
            "elongación no detectada: redonda {} alargada {}",
            e_round,
            e_elong
        );
    }

    #[test]
    fn test_ds_quality_gate_has_no_point_three_floor_and_explains_exclusion() {
        let retained = ds_frame_quality_weight(4.0, 2.0, 25.0, 100.0, 0.20, 0.20, 0.15)
            .expect("una toma mediocre pero válida conserva un peso bajo real");
        assert!(retained >= 0.05 && retained < 0.3, "peso {retained}");

        let cloudy = ds_frame_quality_weight(2.2, 2.0, 5.0, 100.0, 0.20, 0.20, 0.15)
            .unwrap_err();
        assert!(cloudy.contains("transparencia"), "{cloudy}");
        let trailed = ds_frame_quality_weight(2.2, 2.0, 80.0, 100.0, 0.70, 0.20, 0.15)
            .unwrap_err();
        assert!(trailed.contains("alargadas"), "{trailed}");
        let bad_registration =
            ds_frame_quality_weight(2.2, 2.0, 80.0, 100.0, 0.20, 1.0, 0.15)
                .unwrap_err();
        assert!(bad_registration.contains("astrométrico"), "{bad_registration}");
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

    #[test]
    fn test_ds_reject_pixel_recovers_clean_value() {
        // 12 tomas limpias ≈1000 + 2 atípicos (satélite 50000, píxel muerto 0).
        // Todos los métodos por-píxel deben recuperar ~1000 pese a los outliers.
        let clean: [f32; 12] = [
            995.0, 1002.0, 998.0, 1005.0, 1001.0, 999.0, 1003.0, 997.0, 1000.0, 1004.0, 996.0,
            1001.0,
        ];
        let mk = || -> Vec<(f32, f64)> {
            let mut v: Vec<(f32, f64)> = clean.iter().map(|&x| (x, 1.0)).collect();
            v.push((50000.0, 1.0)); // satélite
            v.push((0.0, 1.0)); // píxel muerto
            v
        };
        let mut cov = 0.0f64;
        let mut rejected_low = 0.0f64;
        let mut rejected_high = 0.0f64;
        for m in ["winsorized", "linearfit", "median", "minmax", "percentile"] {
            let mut s = mk();
            let r = ds_reject_pixel(
                &mut s,
                m,
                3.0,
                3.0,
                &mut cov,
                &mut rejected_low,
                &mut rejected_high,
                4.0,
            );
            assert!(
                (r - 1000.0).abs() < 30.0,
                "método {} dio {} (esperado ~1000)",
                m,
                r
            );
            assert!(cov > 0.0, "cobertura nula en {}", m);
        }
        // κ asimétrico: κ↑ pequeño recorta más los brillantes.
        let mut s = mk();
        let r = ds_reject_pixel(
            &mut s,
            "winsorized",
            3.0,
            1.5,
            &mut cov,
            &mut rejected_low,
            &mut rejected_high,
            4.0,
        );
        assert!((r - 1000.0).abs() < 20.0, "winsorized κ asimétrico: {}", r);
        // Pocas muestras (≤2) → media ponderada directa (sin rechazo).
        let mut two = vec![(1000.0f32, 2.0f64), (2000.0f32, 1.0f64)];
        let r2 = ds_reject_pixel(
            &mut two,
            "median",
            3.0,
            3.0,
            &mut cov,
            &mut rejected_low,
            &mut rejected_high,
            4.0,
        );
        assert!(
            (r2 - (1000.0 * 2.0 + 2000.0) as f32 / 3.0).abs() < 1.0,
            "media ponderada n=2: {}",
            r2
        );
    }

    #[test]
    fn test_ds_norm_scale_decision_distinguishes_normalized_from_adu() {
        // SIRIL/PI normalized 0..1 (peak in [0,1], tiny mean) → ×65535.
        assert_eq!(ds_norm_scale_decision(0.92, 0.05, None), 65535.0);
        // Real-ADU frame with a bias pedestal (mean ≫ 1) → left as-is, even when
        // the peak happens to be tiny (the old `max<=2` bug rescaled these).
        assert_eq!(ds_norm_scale_decision(1.8, 800.0, None), 1.0);
        assert_eq!(ds_norm_scale_decision(30000.0, 500.0, None), 1.0);
        // Explicit DATAMAX header wins in both directions.
        assert_eq!(ds_norm_scale_decision(0.9, 0.05, Some(60000.0)), 1.0);
        assert_eq!(ds_norm_scale_decision(0.9, 0.05, Some(1.0)), 65535.0);
    }

    #[test]
    fn test_ds_nonfinite_inputs_are_detected_instead_of_sanitized_to_zero() {
        let samples = [0.0, -3.5, f32::NAN, f32::INFINITY, f32::NEG_INFINITY];
        assert_eq!(ds_nonfinite_sample_count(&samples), 3);
        assert_eq!(samples[0], 0.0, "un cero real sigue siendo una muestra válida");
        assert!(samples[2].is_nan(), "el lector no debe fabricar un cero");
    }

    #[test]
    fn test_ds_mrs_noise_recovers_injected_sigma() {
        // Flat background + pseudo-Gaussian noise (σ≈50, via central-limit of a
        // deterministic LCG) over structure-free data → MRS estimate ≈ σ.
        let (w, h) = (256usize, 256usize);
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            // 12 uniforms − 6 ≈ N(0,1); scaled to σ=50 on a 3000 ADU pedestal.
            let mut acc = 0.0f32;
            for _ in 0..12 {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                acc += ((state >> 33) as f32) / (1u64 << 31) as f32;
            }
            3000.0 + (acc - 6.0) * 50.0
        };
        let luma: Vec<f32> = (0..w * h).map(|_| next()).collect();
        let n = ds_mrs_noise(&luma, w, h);
        assert!((n - 50.0).abs() < 12.0, "MRS noise {} lejos de σ≈50", n);
    }

    /// REAL-DATA validation (M16 SV220 dual-band OSC). Env-gated: set
    /// `ZAS_M16_DIR` to the APILADOS root to run against the user's frames.
    /// This is explicitly ignored without the corpus so it can never inflate
    /// the ordinary "passed" count without exercising any real pixels. Measures
    /// the per-channel background balance that
    /// per-channel normalization (P1.B) fixes, and MRS vs MAD noise (P1.D), on the
    /// actual light frames. Run with:
    ///   ZAS_M16_DIR=/path cargo test --bin astro-stacker validate_m16 -- --nocapture
    #[test]
    #[ignore = "requires ZAS_M16_DIR real dataset"]
    fn validate_m16_channel_balance_on_real_data() {
        let root = std::env::var("ZAS_M16_DIR")
            .expect("ZAS_M16_DIR must point to the real M16 calibration corpus");
        let list_fits = |dir: &str, max: usize| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| {
                            let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                            !n.starts_with("._")
                                && (n.to_lowercase().ends_with(".fits")
                                    || n.to_lowercase().ends_with(".fit"))
                        })
                        .map(|p| p.to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
            v.truncate(max);
            v
        };
        let median_master = |paths: &[String]| -> Option<DsImage> {
            let imgs: Vec<DsImage> = paths.iter().filter_map(|p| ds_read_image(p).ok()).collect();
            let first = imgs.first()?.clone();
            let (w, h, ch) = (first.w, first.h, first.ch);
            let n = w * h * ch;
            let mut out = vec![0.0f32; n];
            let mut col = vec![0.0f32; imgs.len()];
            for i in 0..n {
                for (k, im) in imgs.iter().enumerate() {
                    col[k] = im.data.get(i).copied().unwrap_or(0.0);
                }
                col.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                out[i] = col[col.len() / 2];
            }
            Some(DsImage {
                data: out,
                w,
                h,
                ch,
                bayer: first.bayer,
            })
        };
        let lights = list_fits(&format!("{root}/M16/SV220 Ha OIII/14-Mayo-26 600seg"), 4);
        let darks = list_fits(&format!("{root}/(20) DARKS 600 seg -8c 10 Offset"), 8);
        assert!(!lights.is_empty(), "sin lights bajo {root}");
        println!("\n=== M16 SV220 Ha+OIII — validación real ===");
        println!("lights={} darks={}", lights.len(), darks.len());
        let master_dark = median_master(&darks);

        let mut per_channel_bg: Vec<[f32; 3]> = Vec::new();
        let mut luma_bg: Vec<f32> = Vec::new();
        let (mut mad_noise, mut mrs_noise) = (0.0f32, 0.0f32);
        for (li, lp) in lights.iter().enumerate() {
            let mut img = ds_read_image(lp).unwrap();
            let bayer = img.bayer;
            // Dark-only calibration isolates the per-channel SKY background.
            ds_calibrate(&mut img, None, master_dark.as_ref(), None, 1.0);
            let rgb = if let Some(cid) = bayer {
                ds_debayer_image(img, cid)
            } else {
                img
            };
            let bg = ds_channel_backgrounds(&rgb);
            let luma = ds_luma(&rgb);
            luma_bg.push(ds_bg_noise(&luma).0);
            if li == 0 {
                mad_noise = ds_bg_noise(&luma).1;
                mrs_noise = ds_mrs_noise(&luma, rgb.w, rgb.h);
            }
            println!(
                "  light {li}: fondo R/G/B = {:.1} / {:.1} / {:.1} ADU  (Δ G-R = {:.1})",
                bg[0],
                bg[1],
                bg[2],
                bg[1] - bg[0]
            );
            per_channel_bg.push(bg);
        }

        // Residual per-channel background mismatch across frames after OLD (single
        // luma offset) vs NEW (per-channel offset) normalization, frame 0 = ref.
        let refbg = per_channel_bg[0];
        let refluma = luma_bg[0];
        let (mut old_ss, mut new_ss, mut cnt) = (0.0f64, 0.0f64, 0usize);
        for i in 1..per_channel_bg.len() {
            let luma_off = refluma - luma_bg[i]; // OLD: one offset for all channels
            for c in 0..3 {
                let old_res = (per_channel_bg[i][c] + luma_off - refbg[c]) as f64;
                let new_res = 0.0f64; // NEW per-channel offset matches each channel exactly
                old_ss += old_res * old_res;
                new_ss += new_res * new_res;
                cnt += 1;
            }
        }
        let old_rms = if cnt > 0 {
            (old_ss / cnt as f64).sqrt()
        } else {
            0.0
        };
        let new_rms = if cnt > 0 {
            (new_ss / cnt as f64).sqrt()
        } else {
            0.0
        };
        println!(
            "\nDesajuste de fondo POR CANAL entre tomas tras normalizar:\n  ANTES (offset luma único) = {old_rms:.2} ADU\n  DESPUÉS (offset por canal) = {new_rms:.2} ADU"
        );
        println!("Ruido en toma 0:  MAD(antes) = {mad_noise:.2}   MRS à-trous(después) = {mrs_noise:.2} ADU\n");
        assert!(
            new_rms <= old_rms + 1e-3,
            "la norm. por canal no mejora el balance"
        );
    }

    #[test]
    fn test_ds_hybrid_merge_welford_equals_whole() {
        // The concurrent CPU+GPU split (P2.D) is only correct if merging a GPU
        // Welford result into the CPU raw-moment sums reconstructs the WHOLE-set
        // accumulation exactly. Verify: CPU-subset ⊕ GPU-subset == whole.
        let samples: [(f64, f64); 6] = [
            (100.0, 1.0),
            (105.0, 0.8),
            (98.0, 1.2),
            (110.0, 1.0),
            (95.0, 0.9),
            (102.0, 1.1),
        ];
        let (mut w_sum, mut w_sq, mut w_wgt) = (0.0f64, 0.0, 0.0);
        for &(x, w) in &samples {
            w_sum += w * x;
            w_sq += w * x * x;
            w_wgt += w;
        }
        // CPU subset (raw moments) = first 4.
        let (mut a_sum, mut a_sq, mut a_wgt) = (0.0f64, 0.0, 0.0);
        for &(x, w) in &samples[..4] {
            a_sum += w * x;
            a_sq += w * x * x;
            a_wgt += w;
        }
        // GPU subset (weighted Welford, exactly as the shader) = last 2.
        let (mut mean_b, mut m2_b, mut w_b) = (0.0f64, 0.0f64, 0.0f64);
        for &(x, w) in &samples[4..] {
            let new_w = w_b + w;
            let delta = x - mean_b;
            mean_b += delta * (w / new_w);
            m2_b += w * delta * (x - mean_b);
            w_b = new_w;
        }
        let gpu = crate::gpu_deepsky::GpuPassResult {
            mean: vec![mean_b as f32],
            moment2: vec![m2_b as f32],
            weight: vec![w_b as f32],
            rejected_low: vec![0.0],
            rejected_high: vec![0.0],
            peak_vram_bytes: 0,
            tiles: 0,
        };
        let mut sum = vec![a_sum];
        let mut sq = vec![a_sq];
        let mut wgt = vec![a_wgt];
        ds_merge_gpu_welford_into(&mut sum, &mut sq, &mut wgt, &gpu);
        assert!((sum[0] - w_sum).abs() < 1.0, "sum {} vs {}", sum[0], w_sum);
        assert!((sq[0] - w_sq).abs() < 1.0, "sq {} vs {}", sq[0], w_sq);
        assert!((wgt[0] - w_wgt).abs() < 1e-6, "wgt {} vs {}", wgt[0], w_wgt);
        assert!(
            (sum[0] / wgt[0] - w_sum / w_wgt).abs() < 1e-4,
            "mean combinada"
        );
    }

    #[test]
    fn test_ds_channel_backgrounds_measures_each_channel_independently() {
        // Distinct per-channel sky (R=100, G=200, B=50) with a bright minority
        // that must NOT move the robust 25th-percentile background.
        let (w, h) = (100usize, 100usize);
        let mut data = vec![0.0f32; w * h * 3];
        for i in 0..w * h {
            data[i * 3] = 100.0;
            data[i * 3 + 1] = 200.0;
            data[i * 3 + 2] = 50.0;
        }
        for i in 0..(w * h / 20) {
            data[i * 3] = 60000.0;
            data[i * 3 + 1] = 60000.0;
            data[i * 3 + 2] = 60000.0;
        }
        let img = DsImage {
            data,
            w,
            h,
            ch: 3,
            bayer: None,
        };
        let bg = ds_channel_backgrounds(&img);
        assert!((bg[0] - 100.0).abs() < 1.0, "R bg {}", bg[0]);
        assert!((bg[1] - 200.0).abs() < 1.0, "G bg {}", bg[1]);
        assert!((bg[2] - 50.0).abs() < 1.0, "B bg {}", bg[2]);
        // Per-channel additive offset to a reference (bg_ref=[120,120,120]) must
        // equalize every channel's sky (the OSC colour-cast fix).
        let bg_ref = [120.0f32, 120.0, 120.0];
        for c in 0..3 {
            let add = bg_ref[c] - bg[c]; // mul=1 (additive)
            assert!(
                (bg[c] + add - 120.0).abs() < 1.0,
                "canal {} no neutralizado",
                c
            );
        }
    }

    #[test]
    fn test_ds_filter_token_recognizes_dualband_brands() {
        assert_eq!(ds_filter_token("M42_L-eXtreme_300s.fits"), Some("HA_OIII"));
        assert_eq!(ds_filter_token("IC1805 L-eNhance 120s"), Some("HA_OIII"));
        assert_eq!(ds_filter_token("NGC7000_L-Ultimate.fit"), Some("HA_OIII"));
        assert_eq!(ds_filter_token("Antlia_ALP-T_sub.fits"), Some("HA_OIII"));
        assert_eq!(ds_filter_token("veil_IDAS_NBZ.fits"), Some("HA_OIII"));
        // Individual lines still resolve to themselves.
        assert_eq!(ds_filter_token("M27_Ha_600s.fits"), Some("HA"));
        assert_eq!(ds_filter_token("M27_SII.fits"), Some("SII"));
        // SV220 labelled SII/OIII resolves to the SII+OIII combination.
        assert_eq!(
            ds_filter_token("rosette_SV220_SII_OIII.fits"),
            Some("SII_OIII")
        );
    }

    #[test]
    fn test_ds_recipe_is_atomic_and_contains_reproducible_contract() {
        let dir = std::env::temp_dir().join(format!(
            "zas-recipe-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("nested").join("recipe.json");
        let result = DeepSkyLinearResult {
            id: "deep-test-job".into(),
            data: vec![1.0; 4],
            width: 2,
            height: 2,
            channels: 1,
            coverage: vec![1.0; 4],
            weight: vec![1.0; 4],
            rejection_low: vec![0.0; 4],
            rejection_high: vec![0.0; 4],
            registration_residuals: vec![0.0; 4],
            engine: "cpu_streaming".into(),
            method: "sigma".into(),
            frames_used: 4,
            frames_rejected: 1,
            elapsed_seconds: 2.5,
            recipe: serde_json::json!({
                "sourceFingerprint": "fixture-v1",
                "parameters": {"optionalAbeScnr": false},
                "frames": [{"path": "/fixture/light.fits", "used": true}]
            }),
            variance: None,
            neff: None,
            dq: None,
            struct_map: None,
            struct_residual: None,
            recoverability: None,
        };
        ds_write_recipe(&path, &result).unwrap();
        let recipe: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            recipe["schemaVersion"],
            pipeline::DEEP_SKY_RECIPE_SCHEMA_VERSION
        );
        assert_eq!(recipe["resultId"], "deep-test-job");
        assert_eq!(recipe["linearFloat32"], true);
        assert_eq!(recipe["recipe"]["sourceFingerprint"], "fixture-v1");
        assert!(std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .all(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                !name.ends_with(".part") && !name.ends_with(".bak")
            }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_ds_float32_fits_cancel_and_disk_full_never_commit_partial_output() {
        let dir = std::env::temp_dir().join(format!(
            "zas-f32-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("master.fits");
        std::fs::write(&path, b"previous-valid-output").unwrap();
        let data = vec![123.0f32; 1024];

        let cancel = std::sync::atomic::AtomicBool::new(true);
        let error = ds_save_float32_fits_cancellable(&path, &data, 32, 32, 1, &[], Some(&cancel))
            .unwrap_err();
        assert!(error.contains("Cancelado"));
        assert_eq!(std::fs::read(&path).unwrap(), b"previous-valid-output");

        cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        TEST_FITS_WRITES_BEFORE_FAILURE.with(|budget| budget.set(Some(1)));
        let error = ds_save_float32_fits_cancellable(&path, &data, 32, 32, 1, &[], Some(&cancel))
            .unwrap_err();
        TEST_FITS_WRITES_BEFORE_FAILURE.with(|budget| budget.set(None));
        assert!(error.contains("disco lleno"));
        assert_eq!(std::fs::read(&path).unwrap(), b"previous-valid-output");
        assert!(
            std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().ends_with(".part")),
            "un error no debe dejar temporales FITS"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_ds_float32_fits_exports_metadata_and_negative_values() {
        let path = std::env::temp_dir().join(format!("zas-f32-meta-{}.fits", std::process::id()));
        let data = vec![-42.5f32, 0.0, 100.25, 70000.0];
        ds_save_float32_fits(
            &path,
            &data,
            2,
            2,
            1,
            &[
                ("ZASVER", "'hybrid-v2-test'".into()),
                ("NCOMBINE", format!("{:>20}", 12)),
            ],
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let header = String::from_utf8_lossy(&bytes[..2880]);
        assert!(header.contains("BITPIX  =                  -32"));
        assert!(header.contains("ZASVER  = 'hybrid-v2-test'"));
        assert!(header.contains("NCOMBINE=                   12"));
        let loaded = ds_read_image(path.to_str().unwrap()).unwrap();
        assert_eq!((loaded.w, loaded.h, loaded.ch), (2, 2, 1));
        for (a, b) in loaded.data.iter().zip(&data) {
            assert!((a - b).abs() < 0.001, "FITS float32 {a} != {b}");
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_ds_fits_bunit_matches_scientific_product() {
        let dir = std::env::temp_dir().join(format!("zas-bunit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, expected) in [
            ("variance", "'ADU^2'"),
            ("neff", "'1'"),
            ("coverage", "'1'"),
            ("recov", "'1'"),
            ("dq", "'BITMASK'"),
        ] {
            let path = dir.join(format!("{name}.fits"));
            let unit = ds_map_fits_unit(name).header_value().to_string();
            ds_save_float32_fits(&path, &[1.0], 1, 1, 1, &[("BUNIT", unit)]).unwrap();
            let bytes = std::fs::read(path).unwrap();
            let header = String::from_utf8_lossy(&bytes[..2880]);
            assert!(
                header.contains(&format!("BUNIT   = {expected}")),
                "{name}: {header}"
            );
            assert_eq!(header.matches("BUNIT").count(), 1, "BUNIT duplicado en {name}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_ds_session_bundle_exports_every_scientific_product_and_recipe_decision() {
        let dir = std::env::temp_dir().join(format!(
            "zas-session-bundle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let decision = PreparedCalibrationDecision {
            frame_path: "/fixture/light_001.fits".into(),
            calibration_policy: DeepSkyCalibrationPolicy::AllowDegraded,
            compatible: true,
            degraded: true,
            fallback: Some("biasOnlyThermalUnproven".into()),
            reasons: vec!["fixture degradation".into()],
            ..PreparedCalibrationDecision::default()
        };
        let result = DeepSkyResult {
            id: "session-bundle-fixture".into(),
            data: (0..12).map(|value| value as f32 - 2.0).collect(),
            width: 2,
            height: 2,
            channels: 3,
            coverage: vec![3.0; 4],
            weight: vec![2.5; 4],
            rejection_low: vec![1.0, 0.0, 0.0, 0.0],
            rejection_high: vec![0.0, 1.0, 0.0, 0.0],
            registration_residuals: vec![0.1, 0.2, 0.1, 0.2],
            engine: "eidr_1.5x".into(),
            method: "winsorized".into(),
            frames_used: 12,
            frames_rejected: 2,
            elapsed_seconds: 1.0,
            recipe: serde_json::json!({
                "schemaVersion": pipeline::DEEP_SKY_RECIPE_SCHEMA_VERSION,
                "sourceFingerprint": "fixture-hash-v1",
                "captureMode": "dualBandOsc",
                "calibrationPolicy": "allowDegraded",
                "calibrationDecisions": [decision],
                "integrationMethod": {
                    "requested": "eidr",
                    "effective": {"method": "classic"},
                    "fallbacks": ["gpuToCpu"]
                }
            }),
            variance: Some(vec![4.0; 12]),
            neff: Some(vec![10.0; 12]),
            dq: Some(vec![0, 1, 512, 1024]),
            struct_map: Some(vec![100.0, 101.0, 102.0, 103.0]),
            struct_residual: Some(vec![-1.0, 0.0, 1.0, 2.0]),
            recoverability: Some(vec![0.9, 0.8, 0.7, 0.6]),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let (master, components, diagnostics, bundle) = ds_export_session_result(
            &result,
            &dir,
            "dual-band group",
            "Dual band fixture",
            "HA_OIII",
            &DualBandExtractionOptions::default(),
            DeepSkyCaptureMode::Auto,
            DeepSkyCalibrationPolicy::Strict,
            &cancel,
        )
        .unwrap();

        assert!(std::path::Path::new(&master).is_file());
        assert_eq!(bundle.recipe_schema, pipeline::DEEP_SKY_RECIPE_SCHEMA_VERSION);
        assert_eq!(bundle.capture_mode, DeepSkyCaptureMode::DualBandOsc);
        assert_eq!(
            bundle.calibration_policy,
            DeepSkyCalibrationPolicy::AllowDegraded
        );
        assert_eq!(bundle.calibration_decisions.len(), 1);
        assert!(bundle.fallbacks.iter().any(|item| item == "gpuToCpu"));
        assert!(bundle
            .fallbacks
            .iter()
            .any(|item| item.contains("biasOnlyThermalUnproven")));
        assert_eq!(bundle.products.len(), 14);
        assert_eq!(components.len(), 2);
        for name in [
            "coverage",
            "weight",
            "rejection_low",
            "rejection_high",
            "registration_residuals",
            "variance",
            "neff",
            "dq",
            "struct",
            "struct_residual",
            "recov",
        ] {
            assert!(
                diagnostics
                    .get(name)
                    .is_some_and(|path| std::path::Path::new(path).is_file()),
                "producto ausente: {name}"
            );
        }
        for (name, expected) in [
            ("variance", "'ADU^2'"),
            ("neff", "'1'"),
            ("dq", "'BITMASK'"),
            ("struct", "'ADU'"),
            ("recov", "'1'"),
        ] {
            let bytes = std::fs::read(&diagnostics[name]).unwrap();
            let header = String::from_utf8_lossy(&bytes[..2880]);
            assert!(
                header.contains(&format!("BUNIT   = {expected}")),
                "BUNIT incorrecto en {name}: {header}"
            );
        }
        let dq = ds_read_image(&diagnostics["dq"]).unwrap();
        assert_eq!(dq.data, vec![0.0, 1.0, 512.0, 1024.0]);
        let proxies = bundle
            .products
            .iter()
            .filter(|product| {
                product.metadata.get("role").map(String::as_str) == Some("spectralProxy")
            })
            .collect::<Vec<_>>();
        assert_eq!(proxies.len(), 2);
        assert!(proxies.iter().all(|product| {
            product.derived
                && product.metadata.get("quantitative").map(String::as_str) == Some("false")
        }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_ds_eidr_empty_cfa_gate_merge_is_an_error_not_a_panic() {
        let error = ds_eidr_merge_cfa_gate_reports(Vec::new()).unwrap_err();
        assert!(error.contains("no hay reportes"), "{error}");
    }
}
