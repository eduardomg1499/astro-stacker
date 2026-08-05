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
const SPCC_GAIA_MAX_ROWS: usize = 5_000;
const SPCC_GAIA_MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const SPCC_GAIA_CONNECT_TIMEOUT_SECS: u64 = 7;
const SPCC_GAIA_TOTAL_TIMEOUT_SECS: u64 = 24;

#[derive(Clone, Copy, Debug)]
struct SpccSeed {
    ra_deg: f64,
    dec_deg: f64,
    scale_arcsec_px: f64,
    /// Geometría de la toma que originó la escala. Se lee sólo de NAXIS1/2;
    /// nunca se decodifica el raster para obtenerla.
    source_width: Option<usize>,
    source_height: Option<usize>,
    /// `true` únicamente para un WCS ya resuelto sobre el máster actual. Una
    /// escala de cabecera/usuario todavía pertenece a la toma fuente y debe
    /// transformarse a la malla efectiva Drizzle/EIDR.
    scale_is_output: bool,
}

#[derive(Clone, Copy, Debug)]
struct GaiaStar {
    ra_deg: f64,
    dec_deg: f64,
    g_mag: f64,
    bp_rp: f64,
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
    let (mut source_width, mut source_height) = (None, None);
    if let Ok(fits) = fitrs::Fits::open(light_path) {
        if let Some(hdu) = fits.iter().next() {
            source_width = ds_hdr_num(&hdu, "NAXIS1")
                .filter(|value| value.is_finite() && *value >= 1.0)
                .map(|value| value as usize);
            source_height = ds_hdr_num(&hdu, "NAXIS2")
                .filter(|value| value.is_finite() && *value >= 1.0)
                .map(|value| value as usize);
            if ra.is_none() || dec.is_none() || scale.is_none() {
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
                            let px = ds_hdr_num(&hdu, "XPIXSZ")
                                .or_else(|| ds_hdr_num(&hdu, "PIXSIZE1"))?;
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
        source_width,
        source_height,
        scale_is_output: false,
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

fn spcc_gaia_adql(seed: &SpccSeed, radius_deg: f64, mag_limit: f64) -> String {
    // TOP bounds both server work and local memory. The explicit order makes
    // equal requests reproducible instead of depending on a TAP query plan.
    format!(
        "SELECT TOP {SPCC_GAIA_MAX_ROWS} ra,dec,phot_g_mean_mag,bp_rp \
         FROM gaiadr3.gaia_source WHERE \
         1=CONTAINS(POINT('ICRS',ra,dec),CIRCLE('ICRS',{:.6},{:.6},{:.5})) \
         AND phot_g_mean_mag<{:.2} AND bp_rp IS NOT NULL \
         ORDER BY phot_g_mean_mag ASC, source_id ASC",
        seed.ra_deg, seed.dec_deg, radius_deg, mag_limit
    )
}

fn spcc_decode_gaia_catalog(bytes: &[u8]) -> Option<Vec<GaiaStar>> {
    if bytes.len() as u64 > SPCC_GAIA_MAX_RESPONSE_BYTES {
        return None;
    }
    let values = serde_json::from_slice::<Vec<[f64; 4]>>(bytes).ok()?;
    let mut stars = values
        .into_iter()
        .filter(|value| value.iter().all(|number| number.is_finite()))
        .map(|value| GaiaStar {
            ra_deg: value[0],
            dec_deg: value[1],
            g_mag: value[2],
            bp_rp: value[3],
        })
        .collect::<Vec<_>>();
    stars.sort_by(|left, right| {
        left.g_mag
            .total_cmp(&right.g_mag)
            .then_with(|| left.ra_deg.total_cmp(&right.ra_deg))
            .then_with(|| left.dec_deg.total_cmp(&right.dec_deg))
            .then_with(|| left.bp_rp.total_cmp(&right.bp_rp))
    });
    stars.truncate(SPCC_GAIA_MAX_ROWS);
    (!stars.is_empty()).then_some(stars)
}

fn spcc_atomic_write_cache(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or("La caché Gaia no tiene directorio padre")?;
    std::fs::create_dir_all(parent).map_err(|error| format!("Caché Gaia carpeta: {error}"))?;
    static CACHE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let sequence = CACHE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("gaia.json");
    let temporary = parent.join(format!(".{name}.{}.{}.part", std::process::id(), sequence));
    let write_result = (|| -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| format!("Caché Gaia temporal: {error}"))?;
        file.write_all(bytes)
            .map_err(|error| format!("Caché Gaia: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Caché Gaia sync: {error}"))
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(_error) if path.exists() && spcc_read_catalog_file(path).is_some() => {
            // Dos consultas idénticas pueden terminar a la vez. Si la ganadora
            // ya publicó una entrada válida, descartar nuestro temporal es
            // seguro y conserva atomicidad.
            let _ = std::fs::remove_file(&temporary);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(format!("Caché Gaia commit: {error}"))
        }
    }
}

fn spcc_read_bounded_cache(path: &std::path::Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    if file
        .metadata()
        .ok()
        .is_some_and(|metadata| metadata.len() > SPCC_GAIA_MAX_RESPONSE_BYTES)
    {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(SPCC_GAIA_MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= SPCC_GAIA_MAX_RESPONSE_BYTES).then_some(bytes)
}

/// Query Gaia DR3 (ESA archive TAP, JSON) for stars within `radius_deg` of the
/// seed, brighter than `mag_limit`, with a valid BP-RP colour. Cached atomically
/// so a re-run is offline. Blocking HTTP runs on a bounded worker thread.
fn spcc_query_gaia(
    seed: &SpccSeed,
    radius_deg: f64,
    mag_limit: f64,
    cache_dir: &std::path::Path,
) -> Result<Vec<GaiaStar>, String> {
    let key = spcc_gaia_cache_key(seed, radius_deg, mag_limit);
    let cache = cache_dir.join(key);
    if let Some(bytes) = spcc_read_bounded_cache(&cache) {
        if let Some(stars) = spcc_decode_gaia_catalog(&bytes) {
            return Ok(stars);
        }
    }
    let adql = spcc_gaia_adql(seed, radius_deg, mag_limit);
    let seed_c = *seed;
    let handle = std::thread::spawn(move || -> Result<Vec<u8>, String> {
        use std::io::Read;
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(
                SPCC_GAIA_CONNECT_TIMEOUT_SECS,
            ))
            .timeout(std::time::Duration::from_secs(SPCC_GAIA_TOTAL_TIMEOUT_SECS))
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
            .map_err(|e| {
                if e.is_timeout() {
                    format!("Gaia no respondió dentro de {SPCC_GAIA_TOTAL_TIMEOUT_SECS} s")
                } else if e.is_connect() {
                    "No se pudo conectar con Gaia".to_string()
                } else {
                    format!("Consulta Gaia ({:.4}°): {e}", seed_c.ra_deg)
                }
            })?;
        if !resp.status().is_success() {
            return Err(format!("Gaia HTTP {}", resp.status()));
        }
        if resp
            .content_length()
            .is_some_and(|length| length > SPCC_GAIA_MAX_RESPONSE_BYTES)
        {
            return Err(format!(
                "Gaia devolvió más de {} MiB; reduce el campo o el límite de magnitud",
                SPCC_GAIA_MAX_RESPONSE_BYTES / 1024 / 1024
            ));
        }
        let mut limited = resp.take(SPCC_GAIA_MAX_RESPONSE_BYTES + 1);
        let mut body = Vec::new();
        limited.read_to_end(&mut body).map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut {
                "Gaia interrumpió la descarga por tiempo agotado".to_string()
            } else {
                format!("No se pudo leer la respuesta de Gaia: {error}")
            }
        })?;
        if body.len() as u64 > SPCC_GAIA_MAX_RESPONSE_BYTES {
            return Err(format!(
                "Gaia excedió el límite local de {} MiB; reduce el campo o el límite de magnitud",
                SPCC_GAIA_MAX_RESPONSE_BYTES / 1024 / 1024
            ));
        }
        Ok(body)
    });
    let body = handle.join().map_err(|_| "hilo Gaia cayó".to_string())??;
    let parsed: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| format!("Gaia JSON: {e}"))?;
    let rows = parsed
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or("Gaia: respuesta sin 'data'")?;
    let mut stars: Vec<GaiaStar> = rows
        .iter()
        .filter_map(|row| {
            let a = row.as_array()?;
            let star = GaiaStar {
                ra_deg: a.first()?.as_f64()?,
                dec_deg: a.get(1)?.as_f64()?,
                g_mag: a.get(2)?.as_f64()?,
                bp_rp: a.get(3)?.as_f64()?,
            };
            (star.ra_deg.is_finite()
                && star.dec_deg.is_finite()
                && star.g_mag.is_finite()
                && star.bp_rp.is_finite())
            .then_some(star)
        })
        .collect();
    stars.sort_by(|left, right| {
        left.g_mag
            .total_cmp(&right.g_mag)
            .then_with(|| left.ra_deg.total_cmp(&right.ra_deg))
            .then_with(|| left.dec_deg.total_cmp(&right.dec_deg))
            .then_with(|| left.bp_rp.total_cmp(&right.bp_rp))
    });
    stars.truncate(SPCC_GAIA_MAX_ROWS);
    if stars.is_empty() {
        return Err("Gaia no devolvió estrellas en el campo (revisa RA/Dec/escala).".into());
    }
    let dump: Vec<[f64; 4]> = stars
        .iter()
        .map(|s| [s.ra_deg, s.dec_deg, s.g_mag, s.bp_rp])
        .collect();
    if let Ok(bytes) = serde_json::to_vec(&dump) {
        let _ = spcc_atomic_write_cache(&cache, &bytes);
    }
    Ok(stars)
}

fn spcc_gaia_cache_key(seed: &SpccSeed, radius_deg: f64, mag_limit: f64) -> String {
    format!(
        "gaia_v2_top{SPCC_GAIA_MAX_ROWS}_{:.4}_{:.4}_{:.3}_{:.1}.json",
        seed.ra_deg, seed.dec_deg, radius_deg, mag_limit
    )
}

fn spcc_gaia_legacy_cache_key(seed: &SpccSeed, radius_deg: f64, mag_limit: f64) -> String {
    format!(
        "gaia_{:.4}_{:.4}_{:.3}_{:.1}.json",
        seed.ra_deg, seed.dec_deg, radius_deg, mag_limit
    )
}

/// Robust channel background without materializing a full 60 MP plane.
/// `ds_bg_noise` already caps its population at 200k values; sampling the
/// interleaved master at that same deterministic stride avoids three transient
/// `w*h` allocations while preserving the estimator.
fn spcc_channel_background(data: &[f32], npx: usize, channels: usize, channel: usize) -> f32 {
    if npx == 0 || channels == 0 || channel >= channels {
        return 0.0;
    }
    let step = (npx / 200_000).max(1);
    let samples = (0..npx)
        .step_by(step)
        .filter_map(|pixel| data.get(pixel * channels + channel).copied())
        .collect::<Vec<_>>();
    ds_bg_noise(&samples).0
}

/// Integrated PSF flux of one star directly from an interleaved master.
/// The fitter only consumes a 9×9 neighbourhood, so extracting that bounded
/// patch avoids retaining three full RGB channel planes (≈720 MB at 60 MP).
/// Devuelve None solo cerca del borde o si el ajuste Gauss-Newton diverge.
/// OJO: NO rechaza saturación. El amortiguamiento de Levenberg mantiene bien
/// condicionado el sistema sobre un núcleo recortado, así que una estrella
/// saturada devuelve un flujo finito pero SESGADO: los fotones por encima del
/// full-well no están en los datos y el fit los compensa mal (recorte somero:
/// σ se infla y 2π·amp·σ² SOBRE-estima; recorte profundo: σ clampa, amp queda
/// en la meseta y SUB-estima). El veto de saturación debe hacerse ANTES, por
/// meseta de la ventana: ver `spcc_star_plateau_clipped` / `spcc_measure_star`.
fn spcc_channel_flux(
    data: &[f32],
    w: usize,
    h: usize,
    channels: usize,
    channel: usize,
    x: f32,
    y: f32,
    bg: f32,
) -> Option<f64> {
    const SIDE: usize = 9;
    const RADIUS: i64 = 4;
    if channels == 0 || channel >= channels {
        return None;
    }
    let (cx, cy) = (x.round() as i64, y.round() as i64);
    if cx < 5 || cy < 5 || cx >= w as i64 - 5 || cy >= h as i64 - 5 {
        return None;
    }
    let mut patch = [0.0f32; SIDE * SIDE];
    for patch_y in 0..SIDE {
        for patch_x in 0..SIDE {
            let source_x = (cx - RADIUS + patch_x as i64) as usize;
            let source_y = (cy - RADIUS + patch_y as i64) as usize;
            patch[patch_y * SIDE + patch_x] =
                *data.get((source_y * w + source_x) * channels + channel)?;
        }
    }
    ds_fit_star_psf(&patch, SIDE, SIDE, 4, 4, bg).map(|fit| fit.flux as f64)
}

/// Píxeles empatados con el pico que delatan un núcleo recortado (meseta).
const SPCC_PLATEAU_MIN_TIES: usize = 3;
/// Umbral relativo del empate: al 99,5 % del pico de la ventana. Relativo y no
/// absoluto porque el máster llega con escala arbitraria (float 0..1, ADU
/// reescalados, ganancias previas) y a esta altura del pipeline no existe un
/// nivel de full-well fiable.
const SPCC_PLATEAU_TIE_RATIO: f32 = 0.995;

/// Veto de saturación por meseta, libre de escala, sobre la MISMA ventana 9×9
/// que usa la fotometría. Física: un sensor lineal recortado no redondea el
/// núcleo de la PSF, lo aplana en el nivel de full-well/ADC, así que varios
/// píxeles quedan EMPATADOS con el pico local. Un máximo único (más el ligero
/// suavizado del debayer) es lo normal en una PSF bien muestreada; >= 3
/// píxeles al >= 99,5 % del pico delatan recorte. Devuelve true si CUALQUIER
/// canal presenta meseta: el sesgo fotométrico de la saturación es sistemático
/// por canal (el flujo ajustado no mide los fotones perdidos; su signo depende
/// de la profundidad del recorte), no disperso, así que el sigma-clip de la
/// regresión no lo elimina y hay que excluir la estrella completa.
fn spcc_star_plateau_clipped(
    data: &[f32],
    w: usize,
    h: usize,
    channels: usize,
    x: f32,
    y: f32,
) -> bool {
    const SIDE: usize = 9;
    const RADIUS: i64 = 4;
    if channels == 0 {
        return false;
    }
    let (cx, cy) = (x.round() as i64, y.round() as i64);
    if cx < 5 || cy < 5 || cx >= w as i64 - 5 || cy >= h as i64 - 5 {
        // Fuera de la ventana medible: la fotometría ya la descarta; el veto
        // no opina para no contarla como "saturada" en el diagnóstico.
        return false;
    }
    for channel in 0..channels.min(3) {
        let mut window = [0.0f32; SIDE * SIDE];
        let mut peak = f32::NEG_INFINITY;
        for window_y in 0..SIDE {
            for window_x in 0..SIDE {
                let source_x = (cx - RADIUS + window_x as i64) as usize;
                let source_y = (cy - RADIUS + window_y as i64) as usize;
                let Some(value) = data.get((source_y * w + source_x) * channels + channel) else {
                    return false;
                };
                window[window_y * SIDE + window_x] = *value;
                peak = peak.max(*value);
            }
        }
        // Con pico <= 0 no hay estrella que saturar (ventana vacía/negativa).
        if peak > 0.0 {
            let tie_level = SPCC_PLATEAU_TIE_RATIO * peak;
            let ties = window.iter().filter(|value| **value >= tie_level).count();
            if ties >= SPCC_PLATEAU_MIN_TIES {
                return true;
            }
        }
    }
    false
}

/// Resultado de la fotometría por estrella para la regresión de color.
enum SpccStarMeasure {
    /// Núcleo recortado en algún canal: excluida y CONTADA en el diagnóstico.
    Saturated,
    /// Borde, fit divergente o flujo no positivo: ignorada sin contar.
    Unusable,
    /// Flujos R/G/B válidos, aptos para la regresión.
    Flux(f64, f64, f64),
}

/// Fotometría RGB de una estrella con veto de saturación previo. El veto va
/// ANTES del ajuste porque una estrella recortada produce un flujo finito
/// pero sistemáticamente sesgado en el canal saturado (ver doc de
/// `spcc_channel_flux`): dejarla pasar sesgaría gain_r/gain_b con un color
/// cast sistemático que el sigma-clip de la regresión no puede corregir.
fn spcc_measure_star(
    data: &[f32],
    w: usize,
    h: usize,
    channels: usize,
    x: f32,
    y: f32,
    bg_r: f32,
    bg_g: f32,
    bg_b: f32,
) -> SpccStarMeasure {
    if spcc_star_plateau_clipped(data, w, h, channels, x, y) {
        return SpccStarMeasure::Saturated;
    }
    let (Some(flux_r), Some(flux_g), Some(flux_b)) = (
        spcc_channel_flux(data, w, h, channels, 0, x, y, bg_r),
        spcc_channel_flux(data, w, h, channels, 1, x, y, bg_g),
        spcc_channel_flux(data, w, h, channels, 2, x, y, bg_b),
    ) else {
        return SpccStarMeasure::Unusable;
    };
    if flux_r > 0.0 && flux_g > 0.0 && flux_b > 0.0 {
        SpccStarMeasure::Flux(flux_r, flux_g, flux_b)
    } else {
        SpccStarMeasure::Unusable
    }
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

#[derive(Clone, serde::Deserialize)]
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
    /// El acceso de red es siempre una decisión explícita. El apilado y el
    /// primer intento del editor usan índice/caché local únicamente.
    allow_online: Option<bool>,
    /// Reutiliza una solución Zenith previamente validada (inliers/RMS
    /// presentes) y vuelve a insertarla sin consultar el catálogo.
    prefer_existing: Option<bool>,
}

struct SpccFieldSolution {
    seed: SpccSeed,
    gaia: Vec<GaiaStar>,
    detected: Vec<(f32, f32, f32)>,
    transform: DsTransform,
    inliers: usize,
    rms: f32,
    handed: bool,
    solution: AstrometrySolution,
}

fn spcc_first_light(recipe: &serde_json::Value) -> Option<String> {
    recipe
        .get("inputs")
        .or_else(|| recipe.get("recipe").and_then(|value| value.get("inputs")))
        .and_then(|value| value.get("lights"))
        .and_then(|value| value.as_array())
        .and_then(|values| values.first())
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// PCC based on broadband stellar colours is invalid for line/dual-band
/// acquisitions. The gate consumes explicit recipe metadata first and only
/// falls back to the authoritative FITS FILTER/path classifier for legacy
/// recipes. Unknown metadata remains allowed because ordinary OSC broadband
/// captures frequently have no FILTER keyword.
fn spcc_pcc_block_reason(recipe: &serde_json::Value, first_light: Option<&str>) -> Option<String> {
    let capture_mode = recipe
        .get("captureMode")
        .or_else(|| recipe.pointer("/parameters/captureMode"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        capture_mode.as_str(),
        "dualbandosc" | "dual_band_osc" | "mononarrowband" | "mono_narrowband"
    ) {
        return Some(format!(
            "PCC Gaia está bloqueado: captureMode '{capture_mode}' no representa banda ancha. Usa HOO/SHO o una combinación narrowband explícita."
        ));
    }

    let combination_mode = recipe
        .get("combinationMode")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(combination_mode.as_str(), "sho" | "hoo" | "narrowband") {
        return Some(format!(
            "PCC Gaia está bloqueado para la combinación {combination_mode}: los colores estelares BP−RP no describen una paleta de líneas de emisión."
        ));
    }

    let explicit_filter = recipe
        .get("filterProfile")
        .and_then(serde_json::Value::as_str)
        .and_then(ds_filter_token);
    let source_filter = explicit_filter.or_else(|| first_light.and_then(ds_filter_of_path));
    if matches!(
        source_filter,
        Some("HA" | "OIII" | "SII" | "HA_OIII" | "SII_OIII")
    ) {
        return Some(format!(
            "PCC Gaia está bloqueado: el filtro {} es narrowband/dual-band. Usa HOO/SHO y neutralización específica.",
            source_filter.unwrap_or("narrowband")
        ));
    }
    None
}

fn spcc_seed_from_recipe(recipe: &serde_json::Value) -> Option<SpccSeed> {
    // Un WCS externo sin fingerprint no es una solución resuelta, pero sí es
    // una semilla útil para contrastarlo contra estrellas. Mantenerlo separado
    // evita que se exporte como astrometría validada.
    let wcs = recipe.get("wcs").or_else(|| recipe.get("wcsCandidate"))?;
    let cd11 = wcs.get("cd11")?.as_f64()?;
    let cd12 = wcs.get("cd12")?.as_f64()?;
    let cd21 = wcs.get("cd21")?.as_f64()?;
    let cd22 = wcs.get("cd22")?.as_f64()?;
    Some(SpccSeed {
        ra_deg: wcs.get("crval1")?.as_f64()?,
        dec_deg: wcs.get("crval2")?.as_f64()?,
        scale_arcsec_px: spcc_cd_scale_arcsec_px(cd11, cd12, cd21, cd22)?,
        source_width: None,
        source_height: None,
        scale_is_output: true,
    })
}

fn spcc_recipe_pointer<'a>(
    recipe: &'a serde_json::Value,
    pointer: &str,
) -> Option<&'a serde_json::Value> {
    recipe.pointer(pointer).or_else(|| {
        let nested = format!("/recipe{pointer}");
        recipe.pointer(&nested)
    })
}

/// Pixels de salida por píxel de la toma fuente. El recorte no cambia la
/// escala; Drizzle/EIDR sí, y el super-binning posterior la vuelve a cambiar.
/// La receta publica las tres geometrías necesarias para separarlos.
fn spcc_output_sampling_ratio(
    recipe: &serde_json::Value,
    source_width: usize,
    source_height: usize,
    output_width: usize,
    output_height: usize,
) -> Result<f64, String> {
    let crop = spcc_recipe_pointer(recipe, "/parameters/crop");
    if let Some(crop) = crop {
        let positive = |key: &str| {
            crop.get(key)
                .and_then(serde_json::Value::as_u64)
                .map(|value| value as usize)
                .filter(|value| *value > 0)
        };
        let pre_crop_width = positive("sourceWidth");
        let pre_crop_height = positive("sourceHeight");
        let before_bin_width = positive("widthBeforeOutputBinning");
        let before_bin_height = positive("heightBeforeOutputBinning");
        let recipe_output_width = positive("outputWidth");
        let recipe_output_height = positive("outputHeight");
        if let (
            Some(pre_crop_width),
            Some(pre_crop_height),
            Some(before_bin_width),
            Some(before_bin_height),
            Some(recipe_output_width),
            Some(recipe_output_height),
        ) = (
            pre_crop_width,
            pre_crop_height,
            before_bin_width,
            before_bin_height,
            recipe_output_width,
            recipe_output_height,
        ) {
            if recipe_output_width != output_width || recipe_output_height != output_height {
                return Err(format!(
                    "WCS_GEOMETRY_GATE: la receta describe {}×{} pero el máster actual es {}×{}; vuelve a resolver astrometría",
                    recipe_output_width, recipe_output_height, output_width, output_height
                ));
            }
            let ratio_x = pre_crop_width as f64 / source_width as f64 * recipe_output_width as f64
                / before_bin_width as f64;
            let ratio_y = pre_crop_height as f64 / source_height as f64
                * recipe_output_height as f64
                / before_bin_height as f64;
            if !ratio_x.is_finite() || !ratio_y.is_finite() || ratio_x <= 0.0 || ratio_y <= 0.0 {
                return Err(
                    "WCS_GEOMETRY_GATE: la escala de salida no es finita ni positiva".into(),
                );
            }
            let disagreement = (ratio_x - ratio_y).abs() / ratio_x.max(ratio_y);
            if disagreement > 0.01 {
                return Err(format!(
                    "WCS_GEOMETRY_GATE: la malla final no tiene una escala isotrópica verificable ({ratio_x:.6}× frente a {ratio_y:.6}×)"
                ));
            }
            return Ok((ratio_x * ratio_y).sqrt());
        }
    }

    let crop_disabled = spcc_recipe_pointer(recipe, "/parameters/autoCrop")
        .and_then(serde_json::Value::as_bool)
        == Some(false);
    if source_width == output_width && source_height == output_height {
        return Ok(1.0);
    }
    if crop_disabled {
        let ratio_x = output_width as f64 / source_width as f64;
        let ratio_y = output_height as f64 / source_height as f64;
        if (ratio_x - ratio_y).abs() / ratio_x.max(ratio_y) <= 0.01 {
            return Ok((ratio_x * ratio_y).sqrt());
        }
    }
    Err(
        "WCS_GEOMETRY_GATE: no se puede separar recorte de remuestreo con la metadata disponible; indica una escala del máster o vuelve a apilar con receta v5"
            .into(),
    )
}

fn spcc_refine_seed_for_output(
    mut seed: SpccSeed,
    recipe: &serde_json::Value,
    output_width: usize,
    output_height: usize,
) -> Result<SpccSeed, String> {
    if seed.scale_is_output {
        return Ok(seed);
    }
    let (Some(source_width), Some(source_height)) = (seed.source_width, seed.source_height) else {
        let scaled_product = spcc_recipe_pointer(recipe, "/parameters/drizzle")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|scale| (scale - 1.0).abs() > 1.0e-6)
            || spcc_recipe_pointer(recipe, "/eidr/scaleEffective")
                .and_then(serde_json::Value::as_f64)
                .is_some_and(|scale| (scale - 1.0).abs() > 1.0e-6)
            || spcc_recipe_pointer(recipe, "/outputBin")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|bin| bin != "1x");
        if scaled_product {
            return Err(
                "WCS_GEOMETRY_GATE: Drizzle/EIDR/binning activo pero la toma semilla no expone NAXIS1/2; no se inventará una escala"
                    .into(),
            );
        }
        seed.scale_is_output = true;
        return Ok(seed);
    };
    let ratio = spcc_output_sampling_ratio(
        recipe,
        source_width,
        source_height,
        output_width,
        output_height,
    )?;
    seed.scale_arcsec_px /= ratio;
    seed.scale_is_output = true;
    Ok(seed)
}

fn spcc_cd_scale_arcsec_px(cd11: f64, cd12: f64, cd21: f64, cd22: f64) -> Option<f64> {
    let x = cd11.hypot(cd21);
    let y = cd12.hypot(cd22);
    (x.is_finite() && y.is_finite() && x > 0.0 && y > 0.0).then(|| (x * y).sqrt() * 3600.0)
}

fn spcc_wcs_geometry_fingerprint(
    solution: &AstrometrySolution,
    width: usize,
    height: usize,
) -> String {
    // FNV-1a sobre la representación que realmente cabe en nuestras tarjetas
    // FITS. Hashear bits f64 exactos hacía imposible verificar una exportación:
    // CRPIX/CD se redondean al serializar la cabecera y ya no conservan esos
    // bits. La cuantización coincide con `ds_wcs_fits_metadata`.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let canonical = format!(
        "{width}x{height}|{:.10}|{:.10}|{:.4}|{:.4}|{:.10E}|{:.10E}|{:.10E}|{:.10E}",
        solution.crval1,
        solution.crval2,
        solution.crpix1,
        solution.crpix2,
        solution.cd11,
        solution.cd12,
        solution.cd21,
        solution.cd22,
    );
    for byte in canonical.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("wcs-grid-{hash:016x}")
}

/// Derivación exacta para un recorte puro: los ejes celestes y la matriz CD no
/// cambian; sólo se traslada el píxel de referencia. El llamador de crop debe
/// usar este helper o registrar el error/fingerprint devuelto como invalidación.
pub(crate) fn spcc_crop_astrometry_solution(
    solution: &AstrometrySolution,
    source_width: usize,
    source_height: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Result<AstrometrySolution, String> {
    let fingerprint = spcc_wcs_geometry_fingerprint(solution, source_width, source_height);
    let finite = [
        solution.crval1,
        solution.crval2,
        solution.crpix1,
        solution.crpix2,
        solution.cd11,
        solution.cd12,
        solution.cd21,
        solution.cd22,
        solution.scale_arcsec_px,
    ]
    .iter()
    .all(|value| value.is_finite());
    if !finite
        || width == 0
        || height == 0
        || x.saturating_add(width) > source_width
        || y.saturating_add(height) > source_height
    {
        return Err(format!(
            "WCS_INVALIDATED|{fingerprint}|el recorte o la solución WCS no son válidos para esta geometría"
        ));
    }
    let mut derived = solution.clone();
    derived.crpix1 -= x as f64;
    derived.crpix2 -= y as f64;
    derived.source = format!("{}+analyticCrop", solution.source);
    derived.cached = false;
    Ok(derived)
}

fn spcc_solution_from_recipe(recipe: &serde_json::Value) -> Option<AstrometrySolution> {
    let wcs = recipe.get("wcs")?;
    let cd11 = wcs.get("cd11")?.as_f64()?;
    let cd12 = wcs.get("cd12")?.as_f64()?;
    let cd21 = wcs.get("cd21")?.as_f64()?;
    let cd22 = wcs.get("cd22")?.as_f64()?;
    // CD es la autoridad geométrica. `scaleArcsecPx` de recetas antiguas podía
    // conservar la escala de la toma fuente aun cuando SCI era Drizzle/EIDR 2×.
    let scale_arcsec_px = spcc_cd_scale_arcsec_px(cd11, cd12, cd21, cd22)?;
    let solution = AstrometrySolution {
        ctype: wcs
            .get("ctype")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("TAN")
            .to_string(),
        crval1: wcs.get("crval1")?.as_f64()?,
        crval2: wcs.get("crval2")?.as_f64()?,
        crpix1: wcs.get("crpix1")?.as_f64()?,
        crpix2: wcs.get("crpix2")?.as_f64()?,
        cd11,
        cd12,
        cd21,
        cd22,
        rms_px: wcs.get("rmsPx")?.as_f64()? as f32,
        inliers: wcs.get("inliers")?.as_u64()? as usize,
        handedness: wcs
            .get("handedness")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        scale_arcsec_px,
        source: wcs
            .get("source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("embeddedValidated")
            .to_string(),
        cached: true,
    };
    let measured_quality =
        solution.inliers >= 12 && solution.rms_px.is_finite() && solution.rms_px > 0.0;
    let embedded_integrity = solution.source == "embeddedFingerprintVerified"
        && wcs
            .get("pixelWidth")
            .and_then(serde_json::Value::as_u64)
            .zip(wcs.get("pixelHeight").and_then(serde_json::Value::as_u64))
            .zip(
                wcs.get("geometryFingerprint")
                    .and_then(serde_json::Value::as_str),
            )
            .is_some_and(|((width, height), fingerprint)| {
                fingerprint.eq_ignore_ascii_case(&spcc_wcs_geometry_fingerprint(
                    &solution,
                    width as usize,
                    height as usize,
                ))
            });
    ((measured_quality || embedded_integrity)
        && solution.scale_arcsec_px.is_finite()
        && solution.scale_arcsec_px > 0.0)
        .then_some(solution)
}

fn spcc_set_astrometry_status(
    result: &mut DeepSkyResult,
    state: &str,
    message: &str,
    catalog_key: Option<&str>,
    local_directory: Option<&std::path::Path>,
) {
    if let Some(recipe) = result.recipe.as_object_mut() {
        recipe.insert(
            "astrometryStatus".into(),
            serde_json::json!({
                "state": state,
                "message": message,
                "catalogKey": catalog_key,
                "localDirectory": local_directory.map(|path| path.display().to_string()),
                "onlineAvailable": true,
            }),
        );
    }
}

fn spcc_stamp_wcs_geometry(result: &mut DeepSkyResult) {
    let Some(solution) = result.astrometry_solution.as_ref() else {
        return;
    };
    let fingerprint = spcc_wcs_geometry_fingerprint(solution, result.width, result.height);
    if let Some(wcs) = result
        .recipe
        .get_mut("wcs")
        .and_then(serde_json::Value::as_object_mut)
    {
        wcs.insert("pixelWidth".into(), serde_json::json!(result.width));
        wcs.insert("pixelHeight".into(), serde_json::json!(result.height));
        wcs.insert(
            "geometryFingerprint".into(),
            serde_json::Value::String(fingerprint),
        );
    }
}

fn spcc_catalog_error_parts(error: &str) -> (String, Option<String>, Option<std::path::PathBuf>) {
    let mut parts = error.splitn(4, '|');
    let kind = parts.next().unwrap_or_default();
    if kind == "ASTROMETRY_CATALOG_REQUIRED" || kind == "ASTROMETRY_NETWORK" {
        let key = parts
            .next()
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let directory = parts
            .next()
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from);
        let detail = parts.next().unwrap_or_default();
        let message = if kind == "ASTROMETRY_NETWORK" {
            if detail.is_empty() {
                "Gaia en línea no respondió. Puedes reintentar o instalar el mosaico local."
                    .to_string()
            } else {
                format!("{detail}. Puedes reintentar o instalar el mosaico local.")
            }
        } else {
            "No hay un mosaico Gaia local para este campo. El apilado quedó correcto; sólo falta resolver el WCS."
                .to_string()
        };
        return (message, key, directory);
    }
    (error.to_string(), None, None)
}

fn spcc_read_catalog_file(path: &std::path::Path) -> Option<Vec<GaiaStar>> {
    let bytes = spcc_read_bounded_cache(path)?;
    spcc_decode_gaia_catalog(&bytes)
}

fn spcc_solve_field(
    data: &[f32],
    width: usize,
    height: usize,
    channels: usize,
    first_light: Option<&str>,
    existing_seed: Option<SpccSeed>,
    recipe: &serde_json::Value,
    req: &SpccRequest,
) -> Result<SpccFieldSolution, String> {
    if channels == 0 || data.len() < width.saturating_mul(height).saturating_mul(channels) {
        return Err("El máster lineal tiene una forma inválida".into());
    }
    let ra_override = req
        .ra_deg
        .or_else(|| req.ra.as_deref().and_then(spcc_parse_ra_deg));
    let dec_override = req
        .dec_deg
        .or_else(|| req.dec.as_deref().and_then(spcc_parse_dec_deg));
    let seed_from_light = spcc_read_seed(
        first_light.unwrap_or_default(),
        ra_override.or(existing_seed.map(|seed| seed.ra_deg)),
        dec_override.or(existing_seed.map(|seed| seed.dec_deg)),
        req.scale_arcsec_px,
    );
    let seed = seed_from_light.or(existing_seed).ok_or(
        "No se pudo obtener RA/Dec/escala. Indica RA, Dec y escala (arcsec/píxel) o añade índices locales.",
    )?;
    let seed = spcc_refine_seed_for_output(seed, recipe, width, height)?;
    let magnitude_limit = req.mag_limit.unwrap_or(16.0);
    let diagonal_px = ((width * width + height * height) as f64).sqrt();
    let radius_deg = (diagonal_px * seed.scale_arcsec_px / 3600.0 * 0.6).clamp(0.05, 5.0);
    let root = req
        .work_dir
        .as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let key = spcc_gaia_cache_key(&seed, radius_deg, magnitude_limit);
    let legacy_key = spcc_gaia_legacy_cache_key(&seed, radius_deg, magnitude_limit);
    let local_index = root.join("zenith_astrometry_index").join(&key);
    let legacy_local_index = root.join("zenith_astrometry_index").join(&legacy_key);
    let local_cache_dir = root.join("zenith_astrometry_cache");
    let local_cache = local_cache_dir.join(&key);
    let legacy_local_cache = local_cache_dir.join(&legacy_key);
    let (gaia, source, cached) = if let Some(stars) = spcc_read_catalog_file(&local_index) {
        (stars, "localIndex".to_string(), true)
    } else if let Some(stars) = spcc_read_catalog_file(&legacy_local_index) {
        (stars, "localIndexLegacy".to_string(), true)
    } else if let Some(stars) = spcc_read_catalog_file(&local_cache) {
        (stars, "localCache".to_string(), true)
    } else if let Some(stars) = spcc_read_catalog_file(&legacy_local_cache) {
        (stars, "localCacheLegacy".to_string(), true)
    } else if !req.allow_online.unwrap_or(false) {
        return Err(format!(
            "ASTROMETRY_CATALOG_REQUIRED|{}|{}|",
            key,
            local_index.parent().unwrap_or(&root).display()
        ));
    } else {
        let stars = spcc_query_gaia(&seed, radius_deg, magnitude_limit, &local_cache_dir).map_err(
            |error| {
                format!(
                    "ASTROMETRY_NETWORK|{}|{}|{}",
                    key,
                    local_index.parent().unwrap_or(&root).display(),
                    error
                )
            },
        )?;
        (stars, "gaiaOnline".to_string(), false)
    };

    let pixels = width * height;
    let luma = (0..pixels)
        .map(|pixel| {
            let mut sum = 0.0f32;
            for channel in 0..channels.min(3) {
                sum += data[pixel * channels + channel];
            }
            sum / channels.min(3).max(1) as f32
        })
        .collect::<Vec<_>>();
    let detected = ds_detect_stars(&luma, width, height, 200);
    if detected.len() < 12 {
        return Err(format!(
            "Muy pocas estrellas detectadas ({}) para resolver astrometría.",
            detected.len()
        ));
    }
    let (center_x, center_y) = (width as f64 / 2.0, height as f64 / 2.0);
    let mut best: Option<(DsTransform, usize, f32, bool)> = None;
    for handed in [false, true] {
        let catalog_pixels = gaia
            .iter()
            .filter_map(|star| {
                let (xi, eta) =
                    spcc_gnomonic(star.ra_deg, star.dec_deg, seed.ra_deg, seed.dec_deg, handed)?;
                let x = center_x + xi / seed.scale_arcsec_px;
                let y = center_y + eta / seed.scale_arcsec_px;
                (x > 0.0 && y > 0.0 && x < width as f64 && y < height as f64).then_some((
                    x as f32,
                    y as f32,
                    10.0f32 - star.g_mag as f32,
                ))
            })
            .collect::<Vec<_>>();
        if catalog_pixels.len() < 12 {
            continue;
        }
        if let Some(registration) = ds_match_triangles(&detected, &catalog_pixels) {
            let better = best
                .as_ref()
                .map(|current| registration.inliers > current.1)
                .unwrap_or(true);
            if better {
                best = Some((
                    registration.transform,
                    registration.inliers,
                    registration.rms,
                    handed,
                ));
            }
        }
    }
    let (transform, inliers, rms, handed) = best.ok_or(
        "Astrometría falló: no se alinearon estrellas con Gaia (revisa RA/Dec/escala/paridad).",
    )?;
    let sign = if handed { -1.0f64 } else { 1.0f64 };
    let scale = seed.scale_arcsec_px;
    let m11 = transform.h[0] / scale;
    let m12 = transform.h[1] * sign / scale;
    let m21 = transform.h[3] / scale;
    let m22 = transform.h[4] * sign / scale;
    let determinant = m11 * m22 - m12 * m21;
    if determinant.abs() <= 1e-12 {
        return Err("La solución astrométrica es singular".into());
    }
    let (reference_x, reference_y) = transform.forward(center_x as f32, center_y as f32);
    let cd11 = m22 / determinant / 3600.0;
    let cd12 = -m12 / determinant / 3600.0;
    let cd21 = -m21 / determinant / 3600.0;
    let cd22 = m11 / determinant / 3600.0;
    let solved_scale = spcc_cd_scale_arcsec_px(cd11, cd12, cd21, cd22)
        .ok_or("La solución astrométrica no produjo una escala WCS válida")?;
    let solution = AstrometrySolution {
        ctype: "TAN".into(),
        crval1: seed.ra_deg,
        crval2: seed.dec_deg,
        crpix1: reference_x as f64 + 1.0,
        crpix2: reference_y as f64 + 1.0,
        cd11,
        cd12,
        cd21,
        cd22,
        rms_px: rms,
        inliers,
        handedness: if handed { "mirrored" } else { "normal" }.into(),
        scale_arcsec_px: solved_scale,
        source,
        cached,
    };
    Ok(SpccFieldSolution {
        seed,
        gaia,
        detected,
        transform,
        inliers,
        rms,
        handed,
        solution,
    })
}

#[tauri::command]
async fn solve_deepsky_astrometry(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    req: SpccRequest,
) -> Result<AstrometrySolution, String> {
    let (data, width, height, channels, first_light, existing_seed, embedded_solution, recipe) = {
        let guard = state.deep_sky_result.lock().unwrap();
        let result = guard.as_ref().ok_or("No hay máster lineal en memoria.")?;
        (
            result.data.clone(),
            result.width,
            result.height,
            result.channels,
            spcc_first_light(&result.recipe),
            spcc_seed_from_recipe(&result.recipe),
            spcc_solution_from_recipe(&result.recipe),
            result.recipe.clone(),
        )
    };
    if req.prefer_existing.unwrap_or(true) {
        if let Some(solution) = embedded_solution {
            let mut guard = state.deep_sky_result.lock().unwrap();
            let result = guard
                .as_mut()
                .ok_or("El máster desapareció durante la validación WCS")?;
            ds_poststack_set_operation(
                result,
                PostStackOperation::Astrometry {
                    solution: solution.clone(),
                },
            );
            spcc_set_astrometry_status(
                result,
                "validated",
                "WCS Zenith previamente validado e insertado de nuevo.",
                None,
                None,
            );
            ds_poststack_recompute(result)?;
            spcc_stamp_wcs_geometry(result);
            drop(guard);
            let _ = ds_poststack_publish(&state)?;
            return Ok(solution);
        }
    }
    let field = match spcc_solve_field(
        &data,
        width,
        height,
        channels,
        first_light.as_deref(),
        existing_seed,
        &recipe,
        &req,
    ) {
        Ok(field) => field,
        Err(error) => {
            let (message, key, directory) = spcc_catalog_error_parts(&error);
            if let Ok(mut guard) = state.deep_sky_result.lock() {
                if let Some(result) = guard.as_mut() {
                    spcc_set_astrometry_status(
                        result,
                        if error.starts_with("ASTROMETRY_NETWORK") {
                            "networkError"
                        } else if error.starts_with("ASTROMETRY_CATALOG_REQUIRED") {
                            "catalogRequired"
                        } else {
                            "needsInput"
                        },
                        &message,
                        key.as_deref(),
                        directory.as_deref(),
                    );
                }
            }
            return Err(error);
        }
    };
    {
        let mut guard = state.deep_sky_result.lock().unwrap();
        let result = guard
            .as_mut()
            .ok_or("El máster desapareció durante astrometría")?;
        ds_poststack_set_operation(
            result,
            PostStackOperation::Astrometry {
                solution: field.solution.clone(),
            },
        );
        spcc_set_astrometry_status(
            result,
            "solved",
            "WCS resuelto, validado e insertado en la receta y en la exportación FITS.",
            None,
            None,
        );
        ds_poststack_recompute(result)?;
        spcc_stamp_wcs_geometry(result);
    }
    let _ = ds_poststack_publish(&state)?;
    log_to_front(
        &app,
        "SUCCESS",
        &format!(
            "Astrometría: {} inliers · RMS {:.2} px · {:.3} arcsec/px · fuente {}.",
            field.inliers, field.rms, field.solution.scale_arcsec_px, field.solution.source
        ),
    );
    Ok(field.solution)
}

/// Intento automático no bloqueante que se ejecuta al finalizar el apilado.
/// Sólo consume un WCS Zenith ya validado o catálogo local/cache; jamás abre la
/// red ni convierte la astrometría en requisito para conservar el máster.
fn spcc_try_autosolve_result(
    result: &mut DeepSkyResult,
    work_dir: Option<String>,
) -> Result<AstrometrySolution, String> {
    if let Some(solution) = spcc_solution_from_recipe(&result.recipe) {
        ds_poststack_set_operation(
            result,
            PostStackOperation::Astrometry {
                solution: solution.clone(),
            },
        );
        spcc_set_astrometry_status(
            result,
            "validated",
            "WCS Zenith existente validado e insertado automáticamente.",
            None,
            None,
        );
        ds_poststack_recompute(result)?;
        spcc_stamp_wcs_geometry(result);
        return Ok(solution);
    }
    let first_light = spcc_first_light(&result.recipe);
    let existing_seed = spcc_seed_from_recipe(&result.recipe);
    let request = SpccRequest {
        ra_deg: None,
        dec_deg: None,
        ra: None,
        dec: None,
        scale_arcsec_px: None,
        mag_limit: None,
        white_reference: None,
        work_dir,
        allow_online: Some(false),
        prefer_existing: Some(true),
    };
    let field = match spcc_solve_field(
        &result.data,
        result.width,
        result.height,
        result.channels,
        first_light.as_deref(),
        existing_seed,
        &result.recipe,
        &request,
    ) {
        Ok(field) => field,
        Err(error) => {
            let (message, key, directory) = spcc_catalog_error_parts(&error);
            spcc_set_astrometry_status(
                result,
                if error.starts_with("ASTROMETRY_CATALOG_REQUIRED") {
                    "catalogRequired"
                } else {
                    "needsInput"
                },
                &message,
                key.as_deref(),
                directory.as_deref(),
            );
            return Err(error);
        }
    };
    ds_poststack_set_operation(
        result,
        PostStackOperation::Astrometry {
            solution: field.solution.clone(),
        },
    );
    spcc_set_astrometry_status(
        result,
        "solved",
        "WCS resuelto automáticamente con catálogo local e insertado en la exportación FITS.",
        None,
        None,
    );
    ds_poststack_recompute(result)?;
    spcc_stamp_wcs_geometry(result);
    Ok(field.solution)
}

/// Compatibilidad temporal: el nombre del comando se conserva, pero la
/// operación publicada es PCC Gaia. No se etiqueta como SPCC porque no consume
/// Gaia XP ni curvas QE/filtro/óptica/atmósfera.
#[tauri::command]
async fn spcc_calibrate(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    req: SpccRequest,
) -> Result<PhotometricColorCalibrationResult, String> {
    pcc_gaia_calibrate(app, state, req).await
}

#[tauri::command]
async fn pcc_gaia_calibrate(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    req: SpccRequest,
) -> Result<PhotometricColorCalibrationResult, String> {
    // Analizar siempre la revisión inmediatamente anterior a PCC. Así una
    // segunda aplicación mide los mismos fotones, reemplaza la operación y no
    // oscila ni acumula ganancias.
    let (data, w, h, ch, first_light, existing_seed, recipe) = {
        let guard = state.deep_sky_result.lock().unwrap();
        let r = guard.as_ref().ok_or("No hay máster lineal en memoria.")?;
        if r.channels < 3 {
            return Err("PCC Gaia necesita un máster RGB; el máster actual es monocromo.".into());
        }
        let first_light = spcc_first_light(&r.recipe);
        if let Some(reason) = spcc_pcc_block_reason(&r.recipe, first_light.as_deref()) {
            return Err(reason);
        }
        (
            ds_poststack_data_before_pcc(r)?,
            r.width,
            r.height,
            r.channels,
            first_light,
            spcc_seed_from_recipe(&r.recipe),
            r.recipe.clone(),
        )
    };
    let npx = w * h;
    let field = spcc_solve_field(
        &data,
        w,
        h,
        ch,
        first_light.as_deref(),
        existing_seed,
        &recipe,
        &req,
    )?;
    let seed = field.seed;
    let solved_scale_arcsec_px = field.solution.scale_arcsec_px;
    let gaia = &field.gaia;
    let detected = &field.detected;
    let transform = field.transform;
    let inliers = field.inliers;
    let rms = field.rms;
    let handed = field.handed;
    let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);

    // --- 4. Cross-match + per-channel photometry ---
    // Deterministic bounded samples + 9×9 local patches keep PCC usable on
    // 60 MP masters without allocating three full channel planes.
    let (bgr, bgg, bgb) = (
        spcc_channel_background(&data, npx, ch, 0),
        spcc_channel_background(&data, npx, ch, 1),
        spcc_channel_background(&data, npx, ch, 2),
    );
    let mut samples: Vec<(f64, f64, f64, f64)> = Vec::new();
    // Estrellas excluidas por meseta de saturación: se cuentan aparte porque
    // su ausencia es información (muchas vetadas => exposición larga o campo
    // brillante; conviene que el usuario lo vea en el diagnóstico).
    let mut saturated_vetoed = 0usize;
    for s in gaia {
        let Some((xi, eta)) = spcc_gnomonic(s.ra_deg, s.dec_deg, seed.ra_deg, seed.dec_deg, handed)
        else {
            continue;
        };
        let seed_px = (
            cx as f32 + (xi / seed.scale_arcsec_px) as f32,
            cy as f32 + (eta / seed.scale_arcsec_px) as f32,
        );
        // catalog-seed pixel → detected pixel via the solved transform
        let (dx, dy) = transform.forward(seed_px.0, seed_px.1);
        if !dx.is_finite()
            || !dy.is_finite()
            || dx < 6.0
            || dy < 6.0
            || dx >= (w - 6) as f32
            || dy >= (h - 6) as f32
        {
            continue;
        }
        match spcc_measure_star(&data, w, h, ch, dx, dy, bgr, bgg, bgb) {
            // El recorte sesga el flujo del canal saturado de forma
            // SISTEMÁTICA (no es ruido: todos los recortes de un canal
            // empujan el color en la misma dirección): la estrella entera
            // queda fuera de la regresión, no solo el canal recortado.
            SpccStarMeasure::Saturated => saturated_vetoed += 1,
            SpccStarMeasure::Unusable => {}
            SpccStarMeasure::Flux(fr, fg, fb) => samples.push((s.bp_rp, fr, fg, fb)),
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
            let (gr, gg, gb, anchor) = spcc_solve_gains(&samples).ok_or(
                "PCC Gaia: muy pocas estrellas catalogadas medidas para resolver el balance.",
            )?;
            (
                gr,
                gg,
                gb,
                anchor,
                format!("mediana anclada a {anchor} estrellas cuasi-solares (pocas muestras para regresión)"),
            )
        }
    };
    // Diagnóstico del veto de saturación: en fotometría, una estrella vetada
    // es dato descartado a propósito y el usuario debe poder auditarlo.
    let saturation_note = if saturated_vetoed > 0 {
        format!(" Veto de saturación: {saturated_vetoed} estrellas con meseta de recorte excluidas de la fotometría.")
    } else {
        String::new()
    };

    // Publicar astrometría y PCC como revisiones separadas. El recomputador
    // parte de source_data y aplica cada ganancia una sola vez.
    {
        let mut guard = state.deep_sky_result.lock().unwrap();
        let result = guard
            .as_mut()
            .ok_or("El máster desapareció durante PCC Gaia.")?;
        ds_poststack_set_operation(
            result,
            PostStackOperation::Astrometry {
                solution: field.solution.clone(),
            },
        );
        ds_poststack_set_operation(
            result,
            PostStackOperation::GaiaPcc {
                gain_r,
                gain_g,
                gain_b,
                reference: white_reference.to_string(),
                matched_stars: samples.len(),
                rms_px: rms,
            },
        );
        ds_poststack_recompute(result)?;
        spcc_stamp_wcs_geometry(result);
    }
    let post_stack = ds_poststack_publish(&state)?;
    let preview = post_stack.preview.clone();

    log_to_front(
        &app,
        "SUCCESS",
        &format!(
            "PCC Gaia: {} estrellas emparejadas (inliers {}, RMS {:.2} px) · {} · {} vetadas por saturación · referencia blanca: {} · ganancias R/G/B = {:.3}/{:.3}/{:.3} · revisión idempotente.",
            samples.len(), inliers, rms, fit_note, saturated_vetoed, reference_label, gain_r, gain_g, gain_b
        ),
    );
    Ok(PhotometricColorCalibrationResult {
        matched: samples.len(),
        detected: detected.len(),
        catalog: gaia.len(),
        gain_r,
        gain_g,
        gain_b,
        anchor_stars: anchor,
        solved_scale_arcsec_px,
        rms_px: rms,
        preview,
        note: format!(
            "PCC Gaia por {fit_note}.{saturation_note} Referencia blanca: {reference_label}. Es PCC, no SPCC: no usa Gaia XP ni el perfil espectral del instrumento. Para banda estrecha/dual-band usa HOO/SHO."
        ),
        method: "gaiaPccBpRp".into(),
        astrometry: field.solution,
        post_stack,
    })
}

#[cfg(test)]
mod spcc_tests {
    use super::*;

    fn test_solution() -> AstrometrySolution {
        AstrometrySolution {
            ctype: "TAN".into(),
            crval1: 274.7,
            crval2: -13.8,
            crpix1: 1001.5,
            crpix2: 801.5,
            cd11: -1.0 / 3600.0,
            cd12: 0.0,
            cd21: 0.0,
            cd22: 1.0 / 3600.0,
            rms_px: 0.4,
            inliers: 42,
            handedness: "normal".into(),
            scale_arcsec_px: 1.0,
            source: "localIndex".into(),
            cached: true,
        }
    }

    #[test]
    fn gaia_query_is_bounded_ordered_and_cache_publication_is_atomic() {
        let seed = SpccSeed {
            ra_deg: 274.7,
            dec_deg: -13.8,
            scale_arcsec_px: 1.0,
            source_width: Some(4_144),
            source_height: Some(2_822),
            scale_is_output: false,
        };
        let query = spcc_gaia_adql(&seed, 1.25, 16.0);
        assert!(query.starts_with(&format!("SELECT TOP {SPCC_GAIA_MAX_ROWS} ")));
        assert!(query.contains("ORDER BY phot_g_mean_mag ASC, source_id ASC"));

        let dir = std::env::temp_dir().join(format!(
            "zas-gaia-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("field.json");
        let bytes =
            serde_json::to_vec(&vec![[10.0, -2.0, 15.0, 0.8], [11.0, -2.0, 8.0, 0.6]]).unwrap();
        spcc_atomic_write_cache(&path, &bytes).unwrap();
        let decoded = spcc_read_catalog_file(&path).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].g_mag, 8.0, "la caché se normaliza por magnitud");
        assert!(std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .all(|entry| !entry.file_name().to_string_lossy().ends_with(".part")));
        let oversized = dir.join("oversized.json");
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len(SPCC_GAIA_MAX_RESPONSE_BYTES + 1)
            .unwrap();
        assert!(
            spcc_read_bounded_cache(&oversized).is_none(),
            "una caché sobredimensionada debe rechazarse antes de cargarla"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn drizzle_and_eidr_scale_are_derived_from_effective_geometry() {
        let seed = SpccSeed {
            ra_deg: 10.0,
            dec_deg: 20.0,
            scale_arcsec_px: 2.0,
            source_width: Some(1000),
            source_height: Some(800),
            scale_is_output: false,
        };
        for (label, pre_crop_width, pre_crop_height, expected_scale) in [
            ("drizzle2", 2000usize, 1600usize, 1.0f64),
            ("eidr1_5", 1500usize, 1200usize, 2.0 / 1.5),
        ] {
            let output_width = pre_crop_width - 120;
            let output_height = pre_crop_height - 80;
            let recipe = serde_json::json!({
                "parameters": {
                    "crop": {
                        "sourceWidth": pre_crop_width,
                        "sourceHeight": pre_crop_height,
                        "widthBeforeOutputBinning": output_width,
                        "heightBeforeOutputBinning": output_height,
                        "outputWidth": output_width,
                        "outputHeight": output_height
                    }
                }
            });
            let refined =
                spcc_refine_seed_for_output(seed, &recipe, output_width, output_height).unwrap();
            assert!(
                (refined.scale_arcsec_px - expected_scale).abs() < 1.0e-9,
                "{label}: {} != {expected_scale}",
                refined.scale_arcsec_px
            );
        }

        let embedded = serde_json::json!({
            "wcs": {
                "ctype": "TAN",
                "crval1": 10.0,
                "crval2": 20.0,
                "crpix1": 500.5,
                "crpix2": 400.5,
                "cd11": -0.5 / 3600.0,
                "cd12": 0.0,
                "cd21": 0.0,
                "cd22": 0.5 / 3600.0,
                "rmsPx": 0.4,
                "inliers": 30,
                "handedness": "normal",
                "scaleArcsecPx": 1.0,
                "source": "legacyDrizzleRecipe"
            }
        });
        let solution = spcc_solution_from_recipe(&embedded).unwrap();
        assert!(
            (solution.scale_arcsec_px - 0.5).abs() < 1.0e-9,
            "CD debe corregir scaleArcsecPx heredado: {}",
            solution.scale_arcsec_px
        );
    }

    #[test]
    fn crop_derives_crpix_without_changing_the_celestial_transform() {
        let solution = test_solution();
        let cropped =
            spcc_crop_astrometry_solution(&solution, 2000, 1600, 123, 45, 1200, 900).unwrap();
        assert_eq!(cropped.crpix1, solution.crpix1 - 123.0);
        assert_eq!(cropped.crpix2, solution.crpix2 - 45.0);
        assert_eq!(cropped.crval1, solution.crval1);
        assert_eq!(cropped.crval2, solution.crval2);
        assert_eq!(cropped.cd11, solution.cd11);
        assert_eq!(cropped.cd22, solution.cd22);
        assert!(cropped.source.ends_with("+analyticCrop"));

        let error =
            spcc_crop_astrometry_solution(&solution, 2000, 1600, 1900, 0, 200, 100).unwrap_err();
        assert!(error.starts_with("WCS_INVALIDATED|wcs-grid-"));
    }

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
    fn pcc_blocks_explicit_narrowband_recipes_but_allows_broadband() {
        for recipe in [
            serde_json::json!({"captureMode": "dualBandOsc"}),
            serde_json::json!({"captureMode": "monoNarrowband"}),
            serde_json::json!({"operation": "linearChannelCombination", "combinationMode": "sho"}),
            serde_json::json!({"filterProfile": "HA_OIII"}),
        ] {
            assert!(
                spcc_pcc_block_reason(&recipe, None).is_some(),
                "debió bloquear {recipe}"
            );
        }
        assert!(
            spcc_pcc_block_reason(&serde_json::json!({"captureMode": "broadbandOsc"}), None)
                .is_none()
        );
        assert!(spcc_pcc_block_reason(
            &serde_json::json!({
                "operation": "linearChannelCombination",
                "combinationMode": "rgb"
            }),
            None
        )
        .is_none());
    }

    #[test]
    fn interleaved_pcc_photometry_matches_a_direct_channel_plane() {
        let (w, h, channels) = (25usize, 25usize, 3usize);
        let mut data = vec![100.0f32; w * h * channels];
        let (cx, cy, sigma) = (12.0f32, 12.0f32, 1.4f32);
        for y in 0..h {
            for x in 0..w {
                let radius2 = (x as f32 - cx).powi(2) + (y as f32 - cy).powi(2);
                for channel in 0..channels {
                    data[(y * w + x) * channels + channel] = 100.0
                        + (800.0 + channel as f32 * 200.0) * (-0.5 * radius2 / sigma.powi(2)).exp();
                }
            }
        }
        let plane = (0..w * h)
            .map(|pixel| data[pixel * channels + 1])
            .collect::<Vec<_>>();
        let direct = ds_fit_star_psf(&plane, w, h, 12, 12, 100.0)
            .expect("flujo directo")
            .flux as f64;
        let interleaved = spcc_channel_flux(&data, w, h, channels, 1, 12.0, 12.0, 100.0)
            .expect("flujo interleaved");
        assert!(
            (interleaved - direct).abs() / direct < 1e-5,
            "interleaved {interleaved} != plano {direct}"
        );
        assert!((spcc_channel_background(&data, w * h, channels, 0) - 100.0).abs() < 1.0);
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
        assert!(
            fit.stars >= 55,
            "clip debe conservar el locus ({})",
            fit.stars
        );
        assert!(
            (fit.slope_r - 0.50).abs() < 0.02,
            "pendiente R {}",
            fit.slope_r
        );
        assert!(
            (fit.slope_b + 0.40).abs() < 0.02,
            "pendiente B {}",
            fit.slope_b
        );
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

    /// Pinta una estrella gaussiana circular (fondo + amplitud por canal)
    /// sobre un búfer interleaved, con recorte opcional por canal (simula la
    /// meseta de full-well de un sensor lineal: min(valor, nivel)).
    fn paint_star(
        data: &mut [f32],
        w: usize,
        h: usize,
        channels: usize,
        cx: usize,
        cy: usize,
        sigma: f32,
        bg: f32,
        amps: [f32; 3],
        sat: [Option<f32>; 3],
    ) {
        let radius = 7i64;
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let x = cx as i64 + dx;
                let y = cy as i64 + dy;
                if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
                    continue;
                }
                let r2 = (dx * dx + dy * dy) as f32;
                let falloff = (-0.5 * r2 / (sigma * sigma)).exp();
                for channel in 0..channels.min(3) {
                    let mut value = bg + amps[channel] * falloff;
                    if let Some(level) = sat[channel] {
                        value = value.min(level);
                    }
                    data[(y as usize * w + x as usize) * channels + channel] = value;
                }
            }
        }
    }

    #[test]
    fn pcc_clean_gaussian_star_is_measured_and_accepted() {
        // (a) Estrella gaussiana limpia: sin meseta, el veto no dispara y la
        // fotometría entrega el flujo integrado 2π·amp·σ² de cada canal.
        let (w, h, ch) = (25usize, 25usize, 3usize);
        let sigma = 1.4f32;
        let mut data = vec![100.0f32; w * h * ch];
        paint_star(
            &mut data,
            w,
            h,
            ch,
            12,
            12,
            sigma,
            100.0,
            [700.0, 900.0, 500.0],
            [None; 3],
        );
        assert!(
            !spcc_star_plateau_clipped(&data, w, h, ch, 12.0, 12.0),
            "una PSF bien muestreada tiene máximo único: no hay meseta"
        );
        match spcc_measure_star(&data, w, h, ch, 12.0, 12.0, 100.0, 100.0, 100.0) {
            SpccStarMeasure::Flux(fr, fg, fb) => {
                let expected =
                    |amp: f64| amp * 2.0 * std::f64::consts::PI * (sigma as f64).powi(2);
                assert!((fr / expected(700.0) - 1.0).abs() < 0.05, "flujo R {fr}");
                assert!((fg / expected(900.0) - 1.0).abs() < 0.05, "flujo G {fg}");
                assert!((fb / expected(500.0) - 1.0).abs() < 0.05, "flujo B {fb}");
            }
            _ => panic!("la estrella limpia debió medirse y aceptarse"),
        }
    }

    #[test]
    fn pcc_clipped_core_is_vetoed_even_though_the_fit_returns_finite_flux() {
        // (b) La misma estrella con el núcleo G recortado al nivel del anillo
        // r=1: quedan 5 px EMPATADOS al 100% del pico de la ventana (firma
        // inequívoca de recorte en un sensor lineal). El veto debe excluirla,
        // y el test documenta POR QUÉ es necesario: el fit amortiguado no
        // devuelve None, devuelve un flujo finito sub-estimado.
        let (w, h, ch) = (25usize, 25usize, 3usize);
        let sigma = 1.4f32;
        let amp_g = 900.0f32;
        let ring = 100.0 + amp_g * (-0.5 / (sigma * sigma)).exp();
        let amps = [700.0, amp_g, 500.0];
        let mut clean = vec![100.0f32; w * h * ch];
        paint_star(&mut clean, w, h, ch, 12, 12, sigma, 100.0, amps, [None; 3]);
        let mut clipped = vec![100.0f32; w * h * ch];
        paint_star(
            &mut clipped,
            w,
            h,
            ch,
            12,
            12,
            sigma,
            100.0,
            amps,
            [None, Some(ring), None],
        );
        let ties = (0..w * h)
            .filter(|pixel| (clipped[pixel * ch + 1] - ring).abs() < 1e-3)
            .count();
        assert_eq!(ties, 5, "el recorte debe dejar 5 px al nivel de saturación");
        assert!(
            spcc_star_plateau_clipped(&clipped, w, h, ch, 12.0, 12.0),
            "5 px empatados al pico deben disparar el veto"
        );
        assert!(
            matches!(
                spcc_measure_star(&clipped, w, h, ch, 12.0, 12.0, 100.0, 100.0, 100.0),
                SpccStarMeasure::Saturated
            ),
            "la estrella recortada debe quedar excluida COMPLETA"
        );
        let flux_clean = spcc_channel_flux(&clean, w, h, ch, 1, 12.0, 12.0, 100.0)
            .expect("flujo limpio de referencia");
        let flux_clipped = spcc_channel_flux(&clipped, w, h, ch, 1, 12.0, 12.0, 100.0)
            .expect("Levenberg mantiene el fit bien condicionado: NO devuelve None con saturación");
        assert!(flux_clipped.is_finite() && flux_clipped > 0.0);
        // Sesgo medible (>1 %): con este recorte somero σ se infla y el flujo
        // SOBRE-estima; con recorte profundo sub-estimaría. En ambos casos el
        // valor es erróneo de forma sistemática: por eso el veto y no un
        // intento de "corregir" la fotometría de una estrella recortada.
        assert!(
            (flux_clipped / flux_clean - 1.0).abs() > 0.01,
            "el flujo recortado debe salir sesgado: {flux_clipped} vs {flux_clean}"
        );
    }

    #[test]
    fn pcc_regression_saturation_veto_removes_systematic_green_bias() {
        // (c) Campo sintético: 20 estrellas limpias que siguen una ley de
        // color conocida + 3 brillantes con el canal G recortado. Sin veto,
        // el G sub-estimado infla R/G y B/G en esas 3, el sigma-clip NO las
        // elimina (el sesgo es sistemático y comparable a la dispersión
        // fotométrica, no un outlier grosero) y las ganancias salen sesgadas.
        // Con veto, la regresión coincide exactamente con el fit solo-limpias.
        let (w, h, ch) = (340usize, 24usize, 3usize);
        let sigma = 1.4f32;
        let bg = 100.0f32;
        let color_r = |x: f64| -0.30 + 0.50 * x;
        let color_b = |x: f64| 0.20 - 0.40 * x;
        let mut stars: Vec<(usize, usize, f64, bool)> = Vec::new();
        for k in 0..20usize {
            stars.push((12 + 14 * k, 12, 0.2 + 2.0 * k as f64 / 19.0, false));
        }
        for (i, bp_rp) in [0.5, 1.0, 1.5].iter().enumerate() {
            stars.push((12 + 14 * (20 + i), 12, *bp_rp, true));
        }
        let mut data = vec![bg; w * h * ch];
        for (i, &(cx, cy, bp_rp, clipped)) in stars.iter().enumerate() {
            let base_g: f64 = if clipped { 1500.0 } else { 600.0 + 20.0 * i as f64 };
            // Dispersión fotométrica determinista (±2 %) solo en las limpias:
            // sin ella el sigma-clip identificaría las recortadas como
            // outliers perfectos sobre una recta exacta y el test no
            // exhibiría el mecanismo real del sesgo (en datos reales el
            // recorte moderado queda DENTRO de la dispersión del locus).
            let jitter = |channel: usize| -> f64 {
                if clipped {
                    1.0
                } else {
                    1.0 + 0.02 * (1.7 * i as f64 + 2.1 * channel as f64).sin()
                }
            };
            let amp_r = (base_g * 10f64.powf(-0.4 * color_r(bp_rp)) * jitter(0)) as f32;
            let amp_g = (base_g * jitter(1)) as f32;
            let amp_b = (base_g * 10f64.powf(-0.4 * color_b(bp_rp)) * jitter(2)) as f32;
            // Recorte al nivel del anillo r=1 en G: meseta de 5 px al pico.
            let sat_g = bg + amp_g * (-0.5 / (sigma * sigma)).exp();
            let sat = if clipped {
                [None, Some(sat_g), None]
            } else {
                [None; 3]
            };
            paint_star(&mut data, w, h, ch, cx, cy, sigma, bg, [amp_r, amp_g, amp_b], sat);
        }
        let mut with_veto: Vec<(f64, f64, f64, f64)> = Vec::new();
        let mut without_veto: Vec<(f64, f64, f64, f64)> = Vec::new();
        let mut vetoed: Vec<usize> = Vec::new();
        for (i, &(cx, cy, bp_rp, _)) in stars.iter().enumerate() {
            let (x, y) = (cx as f32, cy as f32);
            // Fotometría "cruda" sin veto: lo que hacía el código antes.
            if let (Some(fr), Some(fg), Some(fb)) = (
                spcc_channel_flux(&data, w, h, ch, 0, x, y, bg),
                spcc_channel_flux(&data, w, h, ch, 1, x, y, bg),
                spcc_channel_flux(&data, w, h, ch, 2, x, y, bg),
            ) {
                if fr > 0.0 && fg > 0.0 && fb > 0.0 {
                    without_veto.push((bp_rp, fr, fg, fb));
                }
            }
            match spcc_measure_star(&data, w, h, ch, x, y, bg, bg, bg) {
                SpccStarMeasure::Saturated => vetoed.push(i),
                SpccStarMeasure::Flux(fr, fg, fb) => with_veto.push((bp_rp, fr, fg, fb)),
                SpccStarMeasure::Unusable => {}
            }
        }
        assert_eq!(vetoed, vec![20, 21, 22], "el veto debe excluir EXACTAMENTE las 3 recortadas");
        assert_eq!(with_veto.len(), 20);
        assert_eq!(without_veto.len(), 23);

        let reference = 0.88;
        let (gr_v, gg_v, gb_v, fit_v) =
            spcc_solve_gains_regression(&with_veto, reference).expect("fit con veto");
        let (gr_a, gg_a, gb_a, fit_a) =
            spcc_solve_gains_regression(&without_veto, reference).expect("fit sin veto");
        // Con veto el fit clava la ley sintética.
        assert!((fit_v.slope_r - 0.50).abs() < 0.05, "pendiente R {}", fit_v.slope_r);
        assert!((fit_v.slope_b + 0.40).abs() < 0.05, "pendiente B {}", fit_v.slope_b);
        // Con veto == fit solo-limpias: mismo conjunto de muestras, mismas
        // ganancias (esta es la tolerancia pedida: cero, por construcción).
        let clean_only: Vec<_> = without_veto.iter().take(20).copied().collect();
        let (gr_c, _, gb_c, _) =
            spcc_solve_gains_regression(&clean_only, reference).expect("fit solo-limpias");
        assert!(
            (gr_v - gr_c).abs() < 1e-12 && (gb_v - gb_c).abs() < 1e-12,
            "con veto debe coincidir con el fit solo-limpias"
        );
        // Sin veto, el sigma-clip conserva las recortadas (sesgo sistemático
        // dentro de la dispersión, no outlier disperso)...
        assert!(
            fit_a.stars > fit_v.stars,
            "las recortadas deben sobrevivir al sigma-clip: {} vs {}",
            fit_a.stars,
            fit_v.stars
        );
        // ...y sesga las ganancias de forma medible y COHERENTE: el recorte
        // somero infla σ y sobre-estima el flujo G de las 3 recortadas, así
        // que sus colores R-G y B-G instrumentales suben, la recta sube y
        // gain_r/gain_b salen inflados respecto al fit limpio (con recorte
        // profundo el signo se invierte hacia la dominante verde; en ambos
        // casos es un cast sistemático que solo el veto elimina).
        let ratio_r = (gr_a / gg_a) / (gr_v / gg_v);
        let ratio_b = (gb_a / gg_a) / (gb_v / gg_v);
        assert!(
            ratio_r > 1.0 + 0.002 && ratio_b > 1.0 + 0.002,
            "el sesgo sin veto debe ser medible y coherente en R y B: R {ratio_r:.5}, B {ratio_b:.5}"
        );
    }
}
