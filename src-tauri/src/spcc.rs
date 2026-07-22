// SPCC — Photometric Color Calibration for the deep-sky linear master.
//
// Included at crate root (after deepsky.rs) so it shares the ds_* primitives.
// Pipeline: (1) seed a WCS from a source-light FITS header (or user override);
// (2) query Gaia DR3 for catalog stars in the field; (3) project them to pixels
// and match to detected stars with the existing triangle matcher; (4) measure
// per-channel instrumental flux of matched stars; (5) solve a per-channel white
// balance anchored to solar-type (G2V) stars — a legitimate photometric white
// balance — and apply it to the linear master.
//
// Scope/honesty: this is a real *photometric* white balance (catalog-anchored),
// not a full spectrophotometric SPCC — the latter needs the sensor QE curves and
// Gaia low-res spectra, which are not available here. For BROADBAND OSC/RGB it
// gives a scientifically-grounded neutral balance. For pure narrowband/dual-band
// (e.g. SV220 Ha+OIII) broadband star colours don't apply — use the HOO/SHO path.

const SPCC_ARCSEC_PER_RAD: f64 = 206_264.806_247_096_36;

#[derive(Clone, Copy, Debug)]
struct SpccSeed {
    ra_deg: f64,
    dec_deg: f64,
    scale_arcsec_px: f64,
}

#[derive(Clone, Copy, Debug)]
struct GaiaStar {
    ra_deg: f64,
    dec_deg: f64,
    g_mag: f64,
    bp_rp: f64,
}

// camelCase: el frontend lee res.gainR/gainG/gainB; sin el rename el camino de
// ÉXITO de SPCC lanzaba TypeError (undefined.toFixed) y se mostraba como error.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SpccResult {
    matched: usize,
    detected: usize,
    catalog: usize,
    gain_r: f64,
    gain_g: f64,
    gain_b: f64,
    anchor_stars: usize,
    solved_scale_arcsec_px: f64,
    rms_px: f32,
    preview: String,
    note: String,
}

/// Parse an RA string in HOURS (sexagesimal "HH MM SS.s", "HH:MM:SS", or decimal
/// degrees fallback) → degrees. Returns None if unparseable.
fn spcc_parse_ra_deg(s: &str) -> Option<f64> {
    let t = s.trim().trim_matches('\'').trim();
    let parts: Vec<f64> = t
        .split(|c: char| c == ':' || c == ' ' || c == 'h' || c == 'm' || c == 's')
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse::<f64>().ok())
        .collect();
    match parts.len() {
        // Single token: if it already looks like degrees (>24) treat as degrees,
        // otherwise hours.
        1 => {
            let v = parts[0];
            Some(if v > 24.0 { v } else { v * 15.0 })
        }
        // Sexagesimal hours.
        2 => Some((parts[0] + parts[1] / 60.0) * 15.0),
        3 => Some((parts[0] + parts[1] / 60.0 + parts[2] / 3600.0) * 15.0),
        _ => None,
    }
}

/// Parse a Dec string ("+DD MM SS", "-DD:MM:SS", or decimal degrees) → degrees.
fn spcc_parse_dec_deg(s: &str) -> Option<f64> {
    let t = s.trim().trim_matches('\'').trim();
    let neg = t.starts_with('-');
    let body = t.trim_start_matches(['+', '-']);
    let parts: Vec<f64> = body
        .split(|c: char| c == ':' || c == ' ' || c == 'd' || c == 'm' || c == 's')
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse::<f64>().ok())
        .collect();
    let mag = match parts.len() {
        1 => parts[0],
        2 => parts[0] + parts[1] / 60.0,
        3 => parts[0] + parts[1] / 60.0 + parts[2] / 3600.0,
        _ => return None,
    };
    Some(if neg { -mag } else { mag })
}

/// Read a plate-solve seed from a source light FITS header. Pointing from
/// OBJCTRA/OBJCTDEC (or RA/DEC); pixel scale from CDELT/PIXSCALE, else from
/// FOCALLEN(mm)+XPIXSZ(µm) [= 206.265·µm/mm]. Overrides win when provided.
fn spcc_read_seed(
    light_path: &str,
    ra_override: Option<f64>,
    dec_override: Option<f64>,
    scale_override: Option<f64>,
) -> Option<SpccSeed> {
    let (mut ra, mut dec, mut scale) = (ra_override, dec_override, scale_override);
    if ra.is_none() || dec.is_none() || scale.is_none() {
        if let Ok(fits) = fitrs::Fits::open(light_path) {
            if let Some(hdu) = fits.iter().next() {
                if ra.is_none() {
                    ra = ds_hdr_str(&hdu, "OBJCTRA")
                        .and_then(|s| spcc_parse_ra_deg(&s))
                        .or_else(|| ds_hdr_str(&hdu, "RA").and_then(|s| spcc_parse_ra_deg(&s)))
                        .or_else(|| ds_hdr_num(&hdu, "CRVAL1"));
                }
                if dec.is_none() {
                    dec = ds_hdr_str(&hdu, "OBJCTDEC")
                        .and_then(|s| spcc_parse_dec_deg(&s))
                        .or_else(|| ds_hdr_str(&hdu, "DEC").and_then(|s| spcc_parse_dec_deg(&s)))
                        .or_else(|| ds_hdr_num(&hdu, "CRVAL2"));
                }
                if scale.is_none() {
                    // CDELT2 (deg/px) → arcsec/px; else PIXSCALE; else focal+pixsz.
                    scale = ds_hdr_num(&hdu, "PIXSCALE")
                        .or_else(|| ds_hdr_num(&hdu, "CDELT2").map(|d| d.abs() * 3600.0))
                        .or_else(|| {
                            let f = ds_hdr_num(&hdu, "FOCALLEN")?;
                            let px = ds_hdr_num(&hdu, "XPIXSZ").or_else(|| ds_hdr_num(&hdu, "PIXSIZE1"))?;
                            (f > 1.0 && px > 0.0).then(|| 206.264_806 * px / f)
                        });
                }
            }
        }
    }
    Some(SpccSeed {
        ra_deg: ra?,
        dec_deg: dec?,
        scale_arcsec_px: scale?,
    })
}

/// Gnomonic (tangent-plane) projection of a sky point about the field centre.
/// Returns standard coordinates (xi, eta) in arcseconds. `handed` flips eta to
/// try both image parities (the triangle matcher solves rotation but not mirror).
fn spcc_gnomonic(ra: f64, dec: f64, ra0: f64, dec0: f64, handed: bool) -> Option<(f64, f64)> {
    let d2r = std::f64::consts::PI / 180.0;
    let (ra, dec, ra0, dec0) = (ra * d2r, dec * d2r, ra0 * d2r, dec0 * d2r);
    let cos_c = dec0.sin() * dec.sin() + dec0.cos() * dec.cos() * (ra - ra0).cos();
    if cos_c <= 0.05 {
        return None; // more than ~87° from centre; behind the tangent plane
    }
    let xi = dec.cos() * (ra - ra0).sin() / cos_c;
    let eta = (dec0.cos() * dec.sin() - dec0.sin() * dec.cos() * (ra - ra0).cos()) / cos_c;
    let sign = if handed { -1.0 } else { 1.0 };
    Some((xi * SPCC_ARCSEC_PER_RAD, sign * eta * SPCC_ARCSEC_PER_RAD))
}

/// Query Gaia DR3 (ESA archive TAP, JSON) for stars within `radius_deg` of the
/// seed, brighter than `mag_limit`, with a valid BP-RP colour. Cached to a JSON
/// file in `cache_dir` so a re-run is offline. Blocking HTTP on a worker thread.
fn spcc_query_gaia(
    seed: &SpccSeed,
    radius_deg: f64,
    mag_limit: f64,
    cache_dir: &std::path::Path,
) -> Result<Vec<GaiaStar>, String> {
    let key = format!(
        "gaia_{:.4}_{:.4}_{:.3}_{:.1}.json",
        seed.ra_deg, seed.dec_deg, radius_deg, mag_limit
    );
    let cache = cache_dir.join(key);
    if let Ok(bytes) = std::fs::read(&cache) {
        if let Ok(v) = serde_json::from_slice::<Vec<[f64; 4]>>(&bytes) {
            return Ok(v
                .into_iter()
                .map(|a| GaiaStar { ra_deg: a[0], dec_deg: a[1], g_mag: a[2], bp_rp: a[3] })
                .collect());
        }
    }
    let adql = format!(
        "SELECT ra,dec,phot_g_mean_mag,bp_rp FROM gaiadr3.gaia_source WHERE \
         1=CONTAINS(POINT('ICRS',ra,dec),CIRCLE('ICRS',{:.6},{:.6},{:.5})) \
         AND phot_g_mean_mag<{:.2} AND bp_rp IS NOT NULL",
        seed.ra_deg, seed.dec_deg, radius_deg, mag_limit
    );
    let seed_c = *seed;
    let handle = std::thread::spawn(move || -> Result<String, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("HTTP init: {e}"))?;
        let resp = client
            .get("https://gea.esac.esa.int/tap-server/tap/sync")
            .query(&[
                ("REQUEST", "doQuery"),
                ("LANG", "ADQL"),
                ("FORMAT", "json"),
                ("QUERY", adql.as_str()),
            ])
            .send()
            .map_err(|e| format!("Gaia query ({}): {e}", seed_c.ra_deg))?;
        if !resp.status().is_success() {
            return Err(format!("Gaia HTTP {}", resp.status()));
        }
        resp.text().map_err(|e| format!("Gaia body: {e}"))
    });
    let body = handle.join().map_err(|_| "hilo Gaia cayó".to_string())??;
    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("Gaia JSON: {e}"))?;
    let rows = parsed
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or("Gaia: respuesta sin 'data'")?;
    let stars: Vec<GaiaStar> = rows
        .iter()
        .filter_map(|row| {
            let a = row.as_array()?;
            Some(GaiaStar {
                ra_deg: a.first()?.as_f64()?,
                dec_deg: a.get(1)?.as_f64()?,
                g_mag: a.get(2)?.as_f64()?,
                bp_rp: a.get(3)?.as_f64()?,
            })
        })
        .collect();
    if stars.is_empty() {
        return Err("Gaia no devolvió estrellas en el campo (revisa RA/Dec/escala).".into());
    }
    let dump: Vec<[f64; 4]> = stars
        .iter()
        .map(|s| [s.ra_deg, s.dec_deg, s.g_mag, s.bp_rp])
        .collect();
    if let Ok(bytes) = serde_json::to_vec(&dump) {
        let _ = std::fs::create_dir_all(cache_dir);
        let _ = std::fs::write(&cache, bytes);
    }
    Ok(stars)
}

/// Integrated PSF flux of one star on a single channel plane at (x,y). Returns
/// None when the fit is ill-conditioned (saturated/edge).
fn spcc_channel_flux(plane: &[f32], w: usize, h: usize, x: f32, y: f32, bg: f32) -> Option<f64> {
    let (cx, cy) = (x.round() as i64, y.round() as i64);
    if cx < 5 || cy < 5 || cx >= w as i64 - 5 || cy >= h as i64 - 5 {
        return None;
    }
    ds_fit_star_psf(plane, w, h, cx as usize, cy as usize, bg).map(|f| f.flux as f64)
}

/// Solve the photometric white balance from matched (bp_rp, R,G,B flux) stars.
/// Anchored to solar-type stars (bp_rp≈0.82, G2V): those must come out neutral,
/// so gain_c = median(G_flux / c_flux) over the solar-colour subset. Falls back
/// to a robust grey-world over all matched stars if too few solar-type stars.
/// Ajuste lineal y = a + b·x con recorte sigma iterativo (3 pasadas, 2.5σ).
/// Devuelve (a, b, rms, n_usadas).
fn spcc_fit_line_sigma_clipped(points: &[(f64, f64)]) -> Option<(f64, f64, f64, usize)> {
    let mut keep: Vec<(f64, f64)> = points
        .iter()
        .copied()
        .filter(|p| p.0.is_finite() && p.1.is_finite())
        .collect();
    if keep.len() < 8 {
        return None;
    }
    let fit = |pts: &[(f64, f64)]| -> Option<(f64, f64, f64)> {
        let n = pts.len() as f64;
        let sx: f64 = pts.iter().map(|p| p.0).sum();
        let sy: f64 = pts.iter().map(|p| p.1).sum();
        let sxx: f64 = pts.iter().map(|p| p.0 * p.0).sum();
        let sxy: f64 = pts.iter().map(|p| p.0 * p.1).sum();
        let det = n * sxx - sx * sx;
        if det.abs() < 1e-9 {
            return None;
        }
        let b = (n * sxy - sx * sy) / det;
        let a = (sy - b * sx) / n;
        let rms = (pts
            .iter()
            .map(|p| {
                let r = p.1 - (a + b * p.0);
                r * r
            })
            .sum::<f64>()
            / n)
            .sqrt();
        Some((a, b, rms))
    };
    let mut result = fit(&keep)?;
    for _ in 0..3 {
        let (a, b, rms) = result;
        let sigma = rms.max(1e-6);
        let filtered: Vec<(f64, f64)> = keep
            .iter()
            .copied()
            .filter(|p| (p.1 - (a + b * p.0)).abs() <= 2.5 * sigma)
            .collect();
        if filtered.len() == keep.len() || filtered.len() < 8 {
            break;
        }
        keep = filtered;
        result = fit(&keep)?;
    }
    let (a, b, rms) = result;
    Some((a, b, rms, keep.len()))
}

/// Diagnóstico del ajuste fotométrico por regresión.
struct SpccFitInfo {
    stars: usize,
    slope_r: f64,
    slope_b: f64,
    scatter_r_mag: f64,
    scatter_b_mag: f64,
}

/// Balance por REGRESIÓN sobre todo el locus estelar (estilo PCC/SPCC de
/// PixInsight): ajusta color instrumental (−2.5·log10 F_c/F_G) contra BP−RP
/// de Gaia con TODAS las estrellas emparejadas y evalúa la recta en el color
/// de la referencia blanca. Mucho más estable que anclarse a las ~solares
/// (que pueden escasear) y sin el sesgo del color medio del campo.
fn spcc_solve_gains_regression(
    samples: &[(f64, f64, f64, f64)],
    reference_bp_rp: f64,
) -> Option<(f64, f64, f64, SpccFitInfo)> {
    let points_r: Vec<(f64, f64)> = samples
        .iter()
        .filter(|s| s.1 > 0.0 && s.2 > 0.0)
        .map(|s| (s.0, -2.5 * (s.1 / s.2).log10()))
        .collect();
    let points_b: Vec<(f64, f64)> = samples
        .iter()
        .filter(|s| s.3 > 0.0 && s.2 > 0.0)
        .map(|s| (s.0, -2.5 * (s.3 / s.2).log10()))
        .collect();
    if points_r.len() < 12 || points_b.len() < 12 {
        return None;
    }
    let (ar, br, rms_r, n_r) = spcc_fit_line_sigma_clipped(&points_r)?;
    let (ab, bb, rms_b, n_b) = spcc_fit_line_sigma_clipped(&points_b)?;
    // Color instrumental predicho de una estrella con el color de la
    // referencia blanca; neutralizarla define las ganancias.
    let color_r = ar + br * reference_bp_rp;
    let color_b = ab + bb * reference_bp_rp;
    let gain_r = 10f64.powf(0.4 * color_r);
    let gain_b = 10f64.powf(0.4 * color_b);
    let (gain_r, gain_g, gain_b) = (gain_r, 1.0f64, gain_b);
    let geo = (gain_r * gain_g * gain_b).cbrt().max(1e-9);
    Some((
        gain_r / geo,
        gain_g / geo,
        gain_b / geo,
        SpccFitInfo {
            stars: n_r.min(n_b),
            slope_r: br,
            slope_b: bb,
            scatter_r_mag: rms_r,
            scatter_b_mag: rms_b,
        },
    ))
}

fn spcc_solve_gains(samples: &[(f64, f64, f64, f64)]) -> Option<(f64, f64, f64, usize)> {
    // samples: (bp_rp, flux_r, flux_g, flux_b)
    let solar: Vec<&(f64, f64, f64, f64)> = samples
        .iter()
        .filter(|s| s.0 >= 0.70 && s.0 <= 0.95 && s.1 > 0.0 && s.2 > 0.0 && s.3 > 0.0)
        .collect();
    let anchor: Vec<&(f64, f64, f64, f64)> = if solar.len() >= 8 {
        solar
    } else {
        samples
            .iter()
            .filter(|s| s.1 > 0.0 && s.2 > 0.0 && s.3 > 0.0)
            .collect()
    };
    if anchor.len() < 3 {
        return None;
    }
    let median = |mut v: Vec<f64>| -> f64 {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[v.len() / 2]
    };
    let kr = median(anchor.iter().map(|s| s.2 / s.1).collect());
    let kb = median(anchor.iter().map(|s| s.2 / s.3).collect());
    // Normalize so the geometric mean of the three gains is 1 (preserve overall
    // brightness; only the RELATIVE channel balance changes).
    let (kr, kg, kb) = (kr, 1.0, kb);
    let geo = (kr * kg * kb).cbrt().max(1e-9);
    Some((kr / geo, kg / geo, kb / geo, anchor.len()))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpccRequest {
    ra_deg: Option<f64>,
    dec_deg: Option<f64>,
    /// Sexagesimal or decimal RA/Dec as typed by the user (parsed server-side).
    ra: Option<String>,
    dec: Option<String>,
    scale_arcsec_px: Option<f64>,
    mag_limit: Option<f64>,
    /// Referencia blanca: "averageSpiral" (default, ~galaxia espiral promedio)
    /// o "g2v" (estrella solar). Con la regresión de color, la referencia es
    /// el color BP-RP en el que se evalúa la recta ajustada.
    white_reference: Option<String>,
    work_dir: Option<String>,
}

/// SPCC command: photometric white-balance the current linear master against
/// Gaia DR3. Reads a WCS seed from the first source light (or the overrides),
/// queries Gaia, plate-solves by matching to detected stars, measures per-channel
/// flux, solves and applies a solar-anchored white balance IN PLACE.
#[tauri::command]
async fn spcc_calibrate(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    req: SpccRequest,
) -> Result<SpccResult, String> {
    // --- snapshot the master + a source light path + geometry (no long lock) ---
    let (mut data, w, h, ch, first_light) = {
        let guard = state.deep_sky_result.lock().unwrap();
        let r = guard.as_ref().ok_or("No hay máster lineal en memoria.")?;
        if r.channels < 3 {
            return Err("SPCC necesita un máster RGB (el máster es monocromo).".into());
        }
        let first_light = r
            .recipe
            .get("recipe")
            .and_then(|v| v.get("inputs"))
            .and_then(|v| v.get("lights"))
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        (r.data.clone(), r.width, r.height, r.channels, first_light)
    };
    let npx = w * h;

    // --- 1. WCS seed ---
    let ra_ov = req.ra_deg.or_else(|| req.ra.as_deref().and_then(spcc_parse_ra_deg));
    let dec_ov = req.dec_deg.or_else(|| req.dec.as_deref().and_then(spcc_parse_dec_deg));
    let light = first_light.unwrap_or_default();
    let seed = spcc_read_seed(&light, ra_ov, dec_ov, req.scale_arcsec_px)
        .ok_or("No se pudo obtener RA/Dec/escala (ni de la cabecera FITS ni de los parámetros). Indica RA, Dec y escala (arcsec/píxel).")?;
    let mag_limit = req.mag_limit.unwrap_or(16.0);

    // --- 2. Gaia catalog ---
    let cache_dir = req
        .work_dir
        .as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("zenith_spcc_cache");
    // Field radius from the diagonal FOV (arcsec → deg) with a small margin.
    let diag_px = ((w * w + h * h) as f64).sqrt();
    let radius_deg = (diag_px * seed.scale_arcsec_px / 3600.0) * 0.6;
    let gaia = spcc_query_gaia(&seed, radius_deg.clamp(0.05, 5.0), mag_limit, &cache_dir)?;

    // --- 3. Detect stars on the master luma and match to the catalog ---
    let luma: Vec<f32> = (0..npx)
        .map(|i| (data[i * ch] + data[i * ch + 1] + data[i * ch + 2]) / 3.0)
        .collect();
    let detected = ds_detect_stars(&luma, w, h, 200);
    if detected.len() < 12 {
        return Err(format!("Muy pocas estrellas detectadas ({}) para plate-solve.", detected.len()));
    }
    // Project the catalog to pixels (image centre = field centre). Try both
    // parities; keep the transform with the most inliers.
    let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
    let mut best: Option<(DsTransform, usize, f32, bool)> = None;
    for handed in [false, true] {
        let cat_px: Vec<(f32, f32, f32)> = gaia
            .iter()
            .filter_map(|s| {
                let (xi, eta) = spcc_gnomonic(s.ra_deg, s.dec_deg, seed.ra_deg, seed.dec_deg, handed)?;
                let px = cx + xi / seed.scale_arcsec_px;
                let py = cy + eta / seed.scale_arcsec_px;
                (px > 0.0 && py > 0.0 && px < w as f64 && py < h as f64)
                    .then_some((px as f32, py as f32, 10.0f32 - s.g_mag as f32))
            })
            .collect();
        if cat_px.len() < 12 {
            continue;
        }
        // Match detected (ref) ↔ catalog-projected (tgt): transform maps catalog
        // pixel → detected pixel, refining the rough seed projection.
        if let Some(reg) = ds_match_triangles(&detected, &cat_px) {
            let better = best.as_ref().map(|b| reg.inliers > b.1).unwrap_or(true);
            if better {
                best = Some((reg.transform, reg.inliers, reg.rms, handed));
            }
        }
    }
    let (transform, inliers, rms, handed) =
        best.ok_or("Plate-solve falló: no se alinearon estrellas con el catálogo (revisa RA/Dec/escala/espejo).")?;

    // --- 4. Cross-match + per-channel photometry ---
    // Background per channel for the PSF fit.
    let bg = |c: usize| -> f32 {
        let plane: Vec<f32> = (0..npx).map(|i| data[i * ch + c]).collect();
        ds_bg_noise(&plane).0
    };
    let (bgr, bgg, bgb) = (bg(0), bg(1), bg(2));
    let plane_r: Vec<f32> = (0..npx).map(|i| data[i * ch]).collect();
    let plane_g: Vec<f32> = (0..npx).map(|i| data[i * ch + 1]).collect();
    let plane_b: Vec<f32> = (0..npx).map(|i| data[i * ch + 2]).collect();
    let mut samples: Vec<(f64, f64, f64, f64)> = Vec::new();
    for s in &gaia {
        let Some((xi, eta)) = spcc_gnomonic(s.ra_deg, s.dec_deg, seed.ra_deg, seed.dec_deg, handed) else { continue; };
        let seed_px = (cx as f32 + (xi / seed.scale_arcsec_px) as f32, cy as f32 + (eta / seed.scale_arcsec_px) as f32);
        // catalog-seed pixel → detected pixel via the solved transform
        let (dx, dy) = transform.forward(seed_px.0, seed_px.1);
        if !dx.is_finite() || !dy.is_finite() || dx < 6.0 || dy < 6.0 || dx >= (w - 6) as f32 || dy >= (h - 6) as f32 {
            continue;
        }
        let (Some(fr), Some(fg), Some(fb)) = (
            spcc_channel_flux(&plane_r, w, h, dx, dy, bgr),
            spcc_channel_flux(&plane_g, w, h, dx, dy, bgg),
            spcc_channel_flux(&plane_b, w, h, dx, dy, bgb),
        ) else { continue; };
        if fr > 0.0 && fg > 0.0 && fb > 0.0 {
            samples.push((s.bp_rp, fr, fg, fb));
        }
    }
    let white_reference = req.white_reference.as_deref().unwrap_or("averageSpiral");
    let (reference_bp_rp, reference_label) = match white_reference {
        "g2v" | "sun" => (0.82, "G2V (estrella solar)"),
        // Aproximación del color efectivo BP-RP de la referencia "galaxia
        // espiral promedio" de PixInsight (no su espectro completo).
        _ => (0.88, "Galaxia espiral promedio"),
    };
    let regression = spcc_solve_gains_regression(&samples, reference_bp_rp);
    let (gain_r, gain_g, gain_b, anchor, fit_note) = match regression {
        Some((gr, gg, gb, fit)) => {
            let note = format!(
                "regresión de color con {} estrellas (pendientes R {:+.3}, B {:+.3} mag/mag; dispersión {:.0}/{:.0} mmag)",
                fit.stars,
                fit.slope_r,
                fit.slope_b,
                fit.scatter_r_mag * 1000.0,
                fit.scatter_b_mag * 1000.0
            );
            (gr, gg, gb, fit.stars, note)
        }
        None => {
            let (gr, gg, gb, anchor) = spcc_solve_gains(&samples)
                .ok_or("SPCC: muy pocas estrellas catalogadas medidas para resolver el balance.")?;
            (
                gr,
                gg,
                gb,
                anchor,
                format!("mediana anclada a {anchor} estrellas cuasi-solares (pocas muestras para regresión)"),
            )
        }
    };

    // --- 5. Apply the per-channel gains to the linear master (in place) ---
    for i in 0..npx {
        data[i * ch] = (data[i * ch] as f64 * gain_r) as f32;
        data[i * ch + 1] = (data[i * ch + 1] as f64 * gain_g) as f32;
        data[i * ch + 2] = (data[i * ch + 2] as f64 * gain_b) as f32;
    }
    let preview = {
        {
            let mut guard = state.deep_sky_result.lock().unwrap();
            let r = guard.as_mut().ok_or("El máster desapareció durante SPCC.")?;
            r.data = data.clone();
            r.method = format!("{} + SPCC", r.method);
            // WCS de la solución (TAN aproximada por similitud): persiste en la
            // receta y el export FITS float32 la escribe como keywords para que
            // PixInsight/Siril puedan anotar/reproyectar el máster.
            let sign = if handed { -1.0f64 } else { 1.0f64 };
            let s = seed.scale_arcsec_px;
            // M = L · diag(1/s, sign/s): (ξ,η) arcsec → Δpíxel imagen.
            let m11 = transform.h[0] / s;
            let m12 = transform.h[1] * sign / s;
            let m21 = transform.h[3] / s;
            let m22 = transform.h[4] * sign / s;
            let det = m11 * m22 - m12 * m21;
            if det.abs() > 1e-12 {
                // CD = M⁻¹/3600 (deg/píxel).
                let cd11 = m22 / det / 3600.0;
                let cd12 = -m12 / det / 3600.0;
                let cd21 = -m21 / det / 3600.0;
                let cd22 = m11 / det / 3600.0;
                let (px0, py0) = transform.forward(cx as f32, cy as f32);
                if let Some(recipe) = r.recipe.as_object_mut() {
                    recipe.insert(
                        "wcs".into(),
                        serde_json::json!({
                            "ctype": "TAN",
                            "crval1": seed.ra_deg,
                            "crval2": seed.dec_deg,
                            "crpix1": px0 as f64 + 1.0,
                            "crpix2": py0 as f64 + 1.0,
                            "cd11": cd11,
                            "cd12": cd12,
                            "cd21": cd21,
                            "cd22": cd22,
                            "rmsPx": rms,
                            "source": "spcc-gaia-similarity",
                        }),
                    );
                }
            }
        }
        // Rebuild the u16 mirror + STF preview so the UI reflects the calibration.
        let mut rgb16 = vec![0u16; npx * 3];
        for i in 0..npx {
            for c in 0..3 {
                rgb16[i * 3 + c] = data[i * ch + c.min(ch - 1)].clamp(0.0, 65535.0) as u16;
            }
        }
        {
            let mut res = state.stacked_image.lock().unwrap();
            *res = Some(StackResult { width: w, height: h, data: rgb16.clone(), is_mono: false, is_surface: false });
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
        save_preview_png_to_temp(&enc, "deepsky")
            .unwrap_or_else(|| format!("data:image/png;base64,{}", general_purpose::STANDARD.encode(&enc)))
    };

    log_to_front(
        &app,
        "SUCCESS",
        &format!(
            "SPCC: {} estrellas Gaia emparejadas (inliers {}, RMS {:.2} px) · {} · referencia blanca: {} · ganancias R/G/B = {:.3}/{:.3}/{:.3} · WCS guardada en la receta.",
            samples.len(), inliers, rms, fit_note, reference_label, gain_r, gain_g, gain_b
        ),
    );
    Ok(SpccResult {
        matched: samples.len(),
        detected: detected.len(),
        catalog: gaia.len(),
        gain_r,
        gain_g,
        gain_b,
        anchor_stars: anchor,
        solved_scale_arcsec_px: seed.scale_arcsec_px,
        rms_px: rms,
        preview,
        note: format!(
            "Balance fotométrico Gaia por {fit_note}. Referencia blanca: {reference_label}. WCS TAN aproximada guardada (se escribe en el FITS float32 exportado). Para banda estrecha/dual-band usa HOO/SHO."
        ),
    })
}

#[cfg(test)]
mod spcc_tests {
    use super::*;

    #[test]
    fn test_spcc_sexagesimal_parsers() {
        // M16 ≈ RA 18h18m48s, Dec −13°49′.
        let ra = spcc_parse_ra_deg("18 18 48").unwrap();
        assert!((ra - 274.7).abs() < 0.1, "RA {ra}");
        let dec = spcc_parse_dec_deg("-13 49 00").unwrap();
        assert!((dec + 13.8167).abs() < 0.01, "Dec {dec}");
        // Decimal fallbacks.
        assert!((spcc_parse_ra_deg("274.70").unwrap() - 274.70).abs() < 1e-6);
        assert!((spcc_parse_dec_deg("+41.5").unwrap() - 41.5).abs() < 1e-6);
        assert!((spcc_parse_ra_deg("10:45:03.6").unwrap() - 161.265).abs() < 0.01);
    }

    #[test]
    fn test_spcc_gnomonic_center_and_offset() {
        // The centre projects to (0,0); a star 1° east at the celestial equator is
        // ≈ +3600 arcsec in xi.
        let (xi, eta) = spcc_gnomonic(180.0, 0.0, 180.0, 0.0, false).unwrap();
        assert!(xi.abs() < 1e-6 && eta.abs() < 1e-6);
        let (xi, _) = spcc_gnomonic(181.0, 0.0, 180.0, 0.0, false).unwrap();
        assert!((xi - 3600.0).abs() < 5.0, "xi {xi}");
    }

    #[test]
    fn test_spcc_regression_recovers_known_color_law_with_outliers() {
        // Ley sintética conocida: color_R = -0.30 + 0.50·(BP-RP), color_B =
        // 0.20 - 0.40·(BP-RP) (en mag). Estrellas de todo el locus 0.2..2.2 +
        // 10% de outliers groseros. La regresión debe recuperar la recta y
        // neutralizar la referencia dentro del 1%.
        let mut samples = Vec::new();
        for k in 0..60 {
            let x = 0.2 + 2.0 * (k as f64) / 59.0;
            let color_r = -0.30 + 0.50 * x;
            let color_b = 0.20 - 0.40 * x;
            let fg = 2000.0 + (k as f64) * 13.0;
            let fr = fg * 10f64.powf(-0.4 * color_r);
            let fb = fg * 10f64.powf(-0.4 * color_b);
            samples.push((x, fr, fg, fb));
        }
        for k in 0..6 {
            // Outliers: flujo R multiplicado por 3 (p. ej. estrella variable).
            let x = 0.4 + (k as f64) * 0.3;
            samples.push((x, 9000.0, 2500.0, 2500.0));
        }
        let reference = 0.88;
        let (gr, gg, gb, fit) = spcc_solve_gains_regression(&samples, reference).unwrap();
        assert!(fit.stars >= 55, "clip debe conservar el locus ({})", fit.stars);
        assert!((fit.slope_r - 0.50).abs() < 0.02, "pendiente R {}", fit.slope_r);
        assert!((fit.slope_b + 0.40).abs() < 0.02, "pendiente B {}", fit.slope_b);
        // Una estrella exactamente en la referencia debe quedar neutra.
        let color_r = -0.30 + 0.50 * reference;
        let color_b = 0.20 - 0.40 * reference;
        let fg = 3000.0;
        let fr = fg * 10f64.powf(-0.4 * color_r);
        let fb = fg * 10f64.powf(-0.4 * color_b);
        let (r, g, b) = (fr * gr, fg * gg, fb * gb);
        assert!((r / g - 1.0).abs() < 0.01, "R/G {}", r / g);
        assert!((b / g - 1.0).abs() < 0.01, "B/G {}", b / g);
    }

    #[test]
    fn test_spcc_solve_gains_neutralizes_solar_stars() {
        // Synthetic solar-type stars with a green-biased instrument (G flux 2× R,
        // 1.5× B). The solved gains must neutralize them (G/R≈2 → gain_r≈2, etc.),
        // normalized to unit geometric mean.
        let mut samples = Vec::new();
        for k in 0..20 {
            let base = 1000.0 + k as f64 * 50.0;
            samples.push((0.82, base, base * 2.0, base * 2.0 / 1.5));
        }
        let (kr, kg, kb, anchor) = spcc_solve_gains(&samples).unwrap();
        assert_eq!(anchor, 20);
        // After gains, R,G,B fluxes should be equal for a solar star.
        let (r, g, b) = (1000.0 * kr, 2000.0 * kg, (2000.0 / 1.5) * kb);
        assert!((r - g).abs() / g < 0.02, "R{r} G{g}");
        assert!((b - g).abs() / g < 0.02, "B{b} G{g}");
        assert!(((kr * kg * kb).cbrt() - 1.0).abs() < 1e-6, "geo-mean unit");
    }
}
