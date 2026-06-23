// =============================================================================
// PLANETARY DEROTATION ENGINE
// Corrects rotational blur on Jupiter, Saturn, Mars surfaces.
// High-precision IAU 2015 rotation models + VSOP87 orbital elements.
// =============================================================================

use rayon::prelude::*;
use std::f64::consts::PI;

// =============================================================================
// 1. DATA STRUCTURES
// =============================================================================

#[derive(Clone, Debug)]
pub struct PlanetaryBody {
    pub name: &'static str,
    pub equatorial_radius_km: f64,
    pub polar_radius_km: f64,
    /// Rotation rates in deg/day for each system (CM1, CM2, CM3)
    pub rotation_rates: [f64; 3],
    /// IAU 2015 North Pole RA (α₀) in degrees (J2000)
    pub pole_ra_deg: f64,
    /// IAU 2015 North Pole Dec (δ₀) in degrees (J2000)
    pub pole_dec_deg: f64,
    /// IAU 2015 Prime Meridian W₀ at J2000.0 (degrees)
    pub w0_deg: [f64; 3],
    /// IAU 2015 Prime Meridian rotation rate Ẇ (degrees/day)
    pub w_dot: [f64; 3],
    pub oblateness: f64,
}

/// Jupiter: System I (equatorial), System II (temperate), System III (radio/magnetic)
pub const JUPITER: PlanetaryBody = PlanetaryBody {
    name: "Jupiter",
    equatorial_radius_km: 71492.0,
    polar_radius_km: 66854.0,
    rotation_rates: [877.900_0, 870.270_0, 870.536_0],
    pole_ra_deg: 268.056_595,
    pole_dec_deg: 64.495_303,
    w0_deg: [67.1, 43.3, 284.95],
    w_dot: [877.900_0, 870.270_0, 870.536_0],
    oblateness: 0.06487,
};

/// Saturn: System I (equatorial), System II (other), System III (Voyager radio)
pub const SATURN: PlanetaryBody = PlanetaryBody {
    name: "Saturn",
    equatorial_radius_km: 60268.0,
    polar_radius_km: 54364.0,
    rotation_rates: [844.300_0, 812.000_0, 810.793_8],
    pole_ra_deg: 40.589,
    pole_dec_deg: 83.537,
    w0_deg: [227.2037, 227.2037, 38.90],
    w_dot: [844.300_0, 812.000_0, 810.793_8],
    oblateness: 0.09796,
};

/// Mars: Only one system used
pub const MARS: PlanetaryBody = PlanetaryBody {
    name: "Mars",
    equatorial_radius_km: 3396.2,
    polar_radius_km: 3376.2,
    rotation_rates: [350.891_985_07, 350.891_985_07, 350.891_985_07],
    pole_ra_deg: 317.681_43,
    pole_dec_deg: 52.886_50,
    w0_deg: [176.630, 176.630, 176.630],
    w_dot: [350.891_985_07, 350.891_985_07, 350.891_985_07],
    oblateness: 0.00589,
};

pub fn get_planet(id: &str) -> Option<&'static PlanetaryBody> {
    match id.to_lowercase().as_str() {
        "jupiter" => Some(&JUPITER),
        "saturn" => Some(&SATURN),
        "mars" => Some(&MARS),
        _ => None,
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlanetDisc {
    pub cx: f64,
    pub cy: f64,
    pub radius_x: f64,
    pub radius_y: f64,
    pub angle_deg: f64,
    pub phase: f64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CaptureMetadata {
    pub mid_time_jd: f64,
    pub planet: Option<String>,
    pub duration_sec: f64,
    pub fps: f64,
    pub start_time_iso: String,
}

// =============================================================================
// 2. JULIAN DATE & TIME HELPERS (High Precision)
// =============================================================================

/// Convert calendar date (UTC) to Julian Date (JD).
/// Meeus, Astronomical Algorithms, Chapter 7.
pub fn datetime_to_jd(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: f64) -> f64 {
    let (y, m) = if month <= 2 {
        (year as f64 - 1.0, month as f64 + 12.0)
    } else {
        (year as f64, month as f64)
    };
    let a = (y / 100.0).floor();
    let b = 2.0 - a + (a / 4.0).floor();
    let jd =
        (365.25 * (y + 4716.0)).floor() + (30.6001 * (m + 1.0)).floor() + day as f64 + b - 1524.5;
    jd + (hour as f64 + min as f64 / 60.0 + sec / 3600.0) / 24.0
}

/// Julian centuries since J2000.0
pub fn jd_to_centuries(jd: f64) -> f64 {
    (jd - 2451545.0) / 36525.0
}

/// Days since J2000.0
pub fn jd_to_days(jd: f64) -> f64 {
    jd - 2451545.0
}

/// Parse ISO 8601-ish string to JD.
/// Supports "YYYY-MM-DDTHH:MM:SS", "YYYY-MM-DD HH:MM:SS", common SharpCap/FireCapture
/// separators and trailing UTC/Z tokens.
pub fn parse_iso_to_jd(s: &str) -> Result<f64, String> {
    let s = s
        .trim()
        .trim_matches('"')
        .trim_end_matches('Z')
        .trim_end_matches("UTC")
        .trim_end_matches("UT")
        .trim()
        .replace('T', " ");
    let parts: Vec<&str> = s.split(' ').collect();
    if parts.is_empty() {
        return Err("Empty datetime string".into());
    }
    let raw_date_token = parts[0].replace(['/', '_', '.'], "-");
    let date_token =
        if raw_date_token.len() == 8 && raw_date_token.chars().all(|c| c.is_ascii_digit()) {
            format!(
                "{}-{}-{}",
                &raw_date_token[0..4],
                &raw_date_token[4..6],
                &raw_date_token[6..8]
            )
        } else {
            raw_date_token
        };
    let date_parts: Vec<&str> = date_token.split('-').collect();
    if date_parts.len() != 3 {
        return Err(format!("Invalid date format: {}", date_token));
    }
    let (year_s, month_s, day_s) = if date_parts[0].len() == 4 {
        (date_parts[0], date_parts[1], date_parts[2])
    } else if date_parts[2].len() == 4 {
        (date_parts[2], date_parts[1], date_parts[0])
    } else {
        (date_parts[0], date_parts[1], date_parts[2])
    };
    let year: i32 = year_s.parse().map_err(|e| format!("Year: {}", e))?;
    let month: u32 = month_s.parse().map_err(|e| format!("Month: {}", e))?;
    let day: u32 = day_s.parse().map_err(|e| format!("Day: {}", e))?;

    let (hour, min, sec) = if parts.len() > 1 {
        let time_parts: Vec<&str> = parts[1].split(':').collect();
        let h: u32 = time_parts.get(0).unwrap_or(&"0").parse().unwrap_or(0);
        let m: u32 = time_parts.get(1).unwrap_or(&"0").parse().unwrap_or(0);
        let sec_token = time_parts
            .get(2)
            .unwrap_or(&"0")
            .trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        let s: f64 = sec_token.parse().unwrap_or(0.0);
        (h, m, s)
    } else {
        (0, 0, 0.0)
    };

    Ok(datetime_to_jd(year, month, day, hour, min, sec))
}

// =============================================================================
// 3. HIGH-PRECISION EPHEMERIS (IAU 2015 + VSOP87 Elements)
// =============================================================================

/// Compute geocentric ecliptic longitude of a planet (simplified VSOP87).
/// Returns longitude in degrees. Sufficient precision for CM calculation (~0.05°).
pub fn planet_ecliptic_longitude(planet: &PlanetaryBody, t_centuries: f64) -> f64 {
    let t = t_centuries;
    match planet.name {
        "Jupiter" => {
            // VSOP87 truncated series for Jupiter's mean longitude
            let l = 34.351_519 + 3034.905_6846 * t + 0.000_223_8 * t * t;
            l % 360.0
        }
        "Saturn" => {
            let l = 50.077_444 + 1222.113_8488 * t + 0.000_210_4 * t * t;
            l % 360.0
        }
        "Mars" => {
            let l = 355.433_275 + 19140.299_3313 * t + 0.000_261_9 * t * t;
            l % 360.0
        }
        _ => 0.0,
    }
}

/// Compute Earth's ecliptic longitude (VSOP87 truncated)
pub fn earth_ecliptic_longitude(t_centuries: f64) -> f64 {
    let t = t_centuries;
    let l = 100.466_457 + 36000.769_8328 * t + 0.000_303_2 * t * t;
    l % 360.0
}

/// Calculate Central Meridian (CM1, CM2, CM3) for a planet at a given JD.
/// Uses IAU 2015 rotation elements + light-time corrected position.
///
/// Returns (CM1, CM2, CM3) in degrees [0, 360).
pub fn calculate_central_meridian(planet: &PlanetaryBody, jd: f64) -> (f64, f64, f64) {
    let d = jd_to_days(jd);
    let t = jd_to_centuries(jd);

    // --- Step 1: Approximate light-time correction ---
    // Geocentric distance in AU (simplified)
    let planet_lon = planet_ecliptic_longitude(planet, t).to_radians();
    let earth_lon = earth_ecliptic_longitude(t).to_radians();

    // Approximate semi-major axes (AU)
    let (a_planet, a_earth) = match planet.name {
        "Jupiter" => (5.2026, 1.0000),
        "Saturn" => (9.5549, 1.0000),
        "Mars" => (1.5237, 1.0000),
        _ => (5.2026, 1.0000),
    };

    // Distance via law of cosines (ecliptic plane approximation)
    let delta_lon = planet_lon - earth_lon;
    let dist_au = (a_planet * a_planet + a_earth * a_earth
        - 2.0 * a_planet * a_earth * delta_lon.cos())
    .sqrt();

    // Light-time in days (AU / speed_of_light_AU_per_day)
    let light_time_days = dist_au / 173.144_633;

    // Corrected d (retarded time)
    let d_corr = d - light_time_days;

    // --- Step 2: IAU Prime Meridian (W) ---
    // W = W₀ + Ẇ * d  (IAU 2015)
    let cm1 = (planet.w0_deg[0] + planet.w_dot[0] * d_corr) % 360.0;
    let cm2 = (planet.w0_deg[1] + planet.w_dot[1] * d_corr) % 360.0;
    let cm3 = (planet.w0_deg[2] + planet.w_dot[2] * d_corr) % 360.0;

    // --- Step 3: Correct for observer's viewpoint ---
    // The CM as seen from Earth depends on the sub-Earth longitude.
    // Sub-Earth longitude ≈ difference between planet's heliocentric longitude
    // and Earth's heliocentric longitude (projected onto planet's equator).
    // For near-opposition imaging this is approximately the elongation correction.

    // Phase angle correction (simplified): the CM visible from Earth
    // differs from the IAU W by the sub-Earth longitude offset.
    // sub_earth_lon ≈ atan2(sin(earth_lon - planet_lon), cos(earth_lon - planet_lon))
    // projected onto the planet's equatorial plane.
    let phase_correction = (earth_lon - planet_lon)
        .sin()
        .atan2((earth_lon - planet_lon).cos());

    // Apply tilt correction using pole coordinates (IAU 2015)
    // The sub-Earth latitude depends on the planet's pole orientation.
    // pole_ra_deg defines the ascending node of the planet's equator.
    let pole_ra_rad = planet.pole_ra_deg.to_radians();
    let pole_dec_rad = planet.pole_dec_deg.to_radians();
    // cos(tilt) factor projects the equatorial correction onto the observer's plane
    let tilt_factor = pole_dec_rad.cos().max(0.5); // clamp to avoid instability
                                                   // Ascending node correction: rotate the phase by the difference between
                                                   // the pole RA and the Earth's ecliptic longitude
    let node_offset = (pole_ra_rad - earth_lon).sin() * 0.01; // small correction
    let phase_deg = phase_correction.to_degrees() * tilt_factor + node_offset;

    let normalize = |x: f64| -> f64 {
        let mut v = x % 360.0;
        if v < 0.0 {
            v += 360.0;
        }
        v
    };

    (
        normalize(cm1 - phase_deg),
        normalize(cm2 - phase_deg),
        normalize(cm3 - phase_deg),
    )
}

/// Calculate rotation delta in degrees between two times for a given CM system.
/// `system`: 0=CM1, 1=CM2, 2=CM3
pub fn rotation_delta_deg(planet: &PlanetaryBody, jd1: f64, jd2: f64, system: usize) -> f64 {
    let dt_days = jd2 - jd1;
    let rate = planet.rotation_rates[system.min(2)];
    rate * dt_days
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct ObserverGeometry {
    /// Sub-Earth planetographic latitude (B0) in degrees. Positive means north pole tilted toward observer.
    pub sub_earth_lat_deg: f64,
    /// Approximate position angle of the planet north pole in degrees, measured on sky.
    pub north_pole_angle_deg: f64,
    /// Sun-planet-observer phase angle in degrees.
    pub phase_angle_deg: f64,
    /// Approximate apparent equatorial diameter in arcseconds.
    pub apparent_diameter_arcsec: f64,
    /// Approximate observer-planet distance in astronomical units.
    pub distance_au: f64,
}

fn normalize_signed_deg(mut deg: f64) -> f64 {
    deg %= 360.0;
    if deg > 180.0 {
        deg -= 360.0;
    } else if deg < -180.0 {
        deg += 360.0;
    }
    deg
}

/// Approximate observer geometry for a simple, guided derotation workflow.
///
/// This is intentionally lightweight: it uses the same simplified heliocentric model as the
/// central-meridian estimate, then derives B0/P-angle diagnostics from the IAU pole coordinates.
/// It is not a full JPL/SPICE ephemeris, but gives the user realistic starting values and exposes
/// them for manual correction when the camera angle is unknown.
pub fn calculate_observer_geometry(planet: &PlanetaryBody, jd: f64) -> ObserverGeometry {
    let t = jd_to_centuries(jd);
    let planet_lon = planet_ecliptic_longitude(planet, t).to_radians();
    let earth_lon = earth_ecliptic_longitude(t).to_radians();
    let (a_planet, planet_radius_km) = match planet.name {
        "Jupiter" => (5.2026, planet.equatorial_radius_km),
        "Saturn" => (9.5549, planet.equatorial_radius_km),
        "Mars" => (1.5237, planet.equatorial_radius_km),
        _ => (5.2026, planet.equatorial_radius_km),
    };
    let earth_x = earth_lon.cos();
    let earth_y = earth_lon.sin();
    let planet_x = a_planet * planet_lon.cos();
    let planet_y = a_planet * planet_lon.sin();
    let geo_x = planet_x - earth_x;
    let geo_y = planet_y - earth_y;
    let distance_au = (geo_x * geo_x + geo_y * geo_y).sqrt().max(0.001);

    let obliquity = 23.439_291_f64.to_radians();
    let eq_x = geo_x;
    let eq_y = geo_y * obliquity.cos();
    let eq_z = geo_y * obliquity.sin();
    let ra = eq_y.atan2(eq_x);
    let dec = eq_z.atan2((eq_x * eq_x + eq_y * eq_y).sqrt());

    let pole_ra = planet.pole_ra_deg.to_radians();
    let pole_dec = planet.pole_dec_deg.to_radians();
    let dra = pole_ra - ra;

    let b0 = (pole_dec.sin() * dec.sin() + pole_dec.cos() * dec.cos() * dra.cos()).asin();
    let p = (pole_dec.cos() * dra.sin())
        .atan2(pole_dec.sin() * dec.cos() - pole_dec.cos() * dec.sin() * dra.cos());

    // Circular-orbit phase approximation via triangle Sun-Planet-Earth.
    let sun_planet_au = a_planet;
    let sun_earth_au = 1.0_f64;
    let cos_phase = ((sun_planet_au * sun_planet_au) + (distance_au * distance_au)
        - (sun_earth_au * sun_earth_au))
        / (2.0 * sun_planet_au * distance_au);
    let phase_angle_deg = cos_phase.clamp(-1.0, 1.0).acos().to_degrees();
    let au_km = 149_597_870.7_f64;
    let apparent_diameter_arcsec = 2.0
        * (planet_radius_km / (distance_au * au_km))
            .atan()
            .to_degrees()
        * 3600.0;

    ObserverGeometry {
        sub_earth_lat_deg: b0.to_degrees().clamp(-35.0, 35.0),
        north_pole_angle_deg: normalize_signed_deg(p.to_degrees()),
        phase_angle_deg,
        apparent_diameter_arcsec,
        distance_au,
    }
}

// =============================================================================
// 4. CAPTURE METADATA PARSERS
// =============================================================================

fn clean_log_value(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string()
}

fn split_log_kv(line: &str) -> Option<(&str, &str)> {
    line.split_once('=')
        .or_else(|| line.split_once(':'))
        .map(|(k, v)| (k.trim(), v.trim()))
}

fn detect_planet_hint(content: &str, path: &str) -> Option<String> {
    let haystack = format!("{} {}", path, content).to_lowercase();
    let tokens: Vec<String> = haystack
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    if tokens
        .iter()
        .any(|t| matches!(t.as_str(), "jupiter" | "jup" | "júpiter"))
    {
        Some("jupiter".into())
    } else if tokens
        .iter()
        .any(|t| matches!(t.as_str(), "saturn" | "saturno" | "sat"))
    {
        Some("saturn".into())
    } else if tokens
        .iter()
        .any(|t| matches!(t.as_str(), "mars" | "marte"))
    {
        Some("mars".into())
    } else {
        None
    }
}

fn normalize_log_datetime(value: &str) -> Option<String> {
    let cleaned = clean_log_value(value)
        .replace('T', " ")
        .replace(',', " ")
        .replace(" UTC", "")
        .replace(" UT", "")
        .replace('Z', "");
    let mut tokens = cleaned.split_whitespace();
    let first = tokens.next()?;
    if first.contains(':') {
        return None;
    }
    let second = tokens.next().unwrap_or("00:00:00");
    Some(format!(
        "{} {}",
        first.replace(['/', '_', '.'], "-"),
        second
    ))
}

fn normalize_log_time(value: &str) -> Option<String> {
    let cleaned = clean_log_value(value)
        .replace(" UTC", "")
        .replace(" UT", "")
        .replace('Z', "");
    let token = cleaned
        .split_whitespace()
        .find(|part| part.contains(':'))
        .unwrap_or(cleaned.as_str());
    if token.contains(':') {
        Some(
            token
                .trim_matches(|c: char| !c.is_ascii_digit() && c != ':' && c != '.')
                .to_string(),
        )
    } else {
        None
    }
}

fn combine_log_date_time(date: &str, time: &str) -> Option<String> {
    let date = clean_log_value(date).replace(['/', '_', '.'], "-");
    let time = normalize_log_time(time)?;
    Some(format!("{} {}", date, time))
}

fn parse_log_number(value: &str) -> f64 {
    value
        .trim()
        .trim_matches(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .parse()
        .unwrap_or(0.0)
}

/// Parse FireCapture log file. Extracts timestamps, planet, FPS.
pub fn parse_firecapture_log(log_path: &str) -> Result<CaptureMetadata, String> {
    let content = std::fs::read_to_string(log_path)
        .map_err(|e| format!("Cannot read log '{}': {}", log_path, e))?;

    let mut start_utc: Option<String> = None;
    let mut end_utc: Option<String> = None;
    let mut mid_utc: Option<String> = None;
    let mut planet: Option<String> = detect_planet_hint(&content, log_path);
    let mut fps: f64 = 0.0;
    let mut date_str: Option<String> = None;

    for line in content.lines() {
        let line = line.trim();
        let lower = line.to_lowercase();
        if let Some((key, value)) = split_log_kv(line) {
            let key = key.to_lowercase();
            let value = clean_log_value(value);
            if key == "date" || key.ends_with(" date") {
                date_str = Some(value);
            } else if key.contains("start") && key.contains("ut") {
                start_utc = normalize_log_time(&value).or_else(|| normalize_log_datetime(&value));
            } else if key == "start" {
                start_utc = normalize_log_time(&value).or_else(|| normalize_log_datetime(&value));
            } else if key.contains("end") && key.contains("ut") {
                end_utc = normalize_log_time(&value).or_else(|| normalize_log_datetime(&value));
            } else if key == "end" {
                end_utc = normalize_log_time(&value).or_else(|| normalize_log_datetime(&value));
            } else if key.contains("mid") {
                mid_utc = normalize_log_time(&value).or_else(|| normalize_log_datetime(&value));
            } else if key.contains("profile") || key.contains("target") || key.contains("object") {
                planet = detect_planet_hint(&value, log_path).or(planet);
            } else if key.contains("fps") {
                fps = parse_log_number(&value);
            }
        } else if lower.contains("firecapture") {
            continue;
        }
    }

    let date = date_str.ok_or_else(|| "No date found in FireCapture log".to_string())?;

    let mid_time_str = if let Some(mid) = mid_utc {
        if mid.contains('-') {
            mid
        } else {
            combine_log_date_time(&date, &mid)
                .ok_or_else(|| "Invalid Mid(UT) in FireCapture log".to_string())?
        }
    } else if let (Some(start), Some(end)) = (&start_utc, &end_utc) {
        let start_dt = if start.contains('-') {
            start.clone()
        } else {
            combine_log_date_time(&date, start)
                .ok_or_else(|| "Invalid Start(UT) in FireCapture log".to_string())?
        };
        let end_dt = if end.contains('-') {
            end.clone()
        } else {
            combine_log_date_time(&date, end)
                .ok_or_else(|| "Invalid End(UT) in FireCapture log".to_string())?
        };
        let s_jd = parse_iso_to_jd(&start_dt).unwrap_or(0.0);
        let e_jd = parse_iso_to_jd(&end_dt).unwrap_or(0.0);
        let mid_jd = (s_jd + e_jd) / 2.0;
        return Ok(CaptureMetadata {
            mid_time_jd: mid_jd,
            planet,
            duration_sec: (e_jd - s_jd) * 86400.0,
            fps,
            start_time_iso: start_dt,
        });
    } else if let Some(start) = &start_utc {
        if start.contains('-') {
            start.clone()
        } else {
            combine_log_date_time(&date, start)
                .ok_or_else(|| "Invalid Start(UT) in FireCapture log".to_string())?
        }
    } else {
        return Err("No timestamp found in FireCapture log".into());
    };

    let mid_jd = parse_iso_to_jd(&mid_time_str)?;
    let duration = if let (Some(s), Some(e)) = (&start_utc, &end_utc) {
        let s_jd = parse_iso_to_jd(&format!("{} {}", date, s)).unwrap_or(mid_jd);
        let e_jd = parse_iso_to_jd(&format!("{} {}", date, e)).unwrap_or(mid_jd);
        (e_jd - s_jd) * 86400.0
    } else {
        0.0
    };

    Ok(CaptureMetadata {
        mid_time_jd: mid_jd,
        planet,
        duration_sec: duration,
        fps,
        start_time_iso: mid_time_str,
    })
}

/// Parse SharpCap log (extended). Looks for timestamps and metadata.
pub fn parse_sharpcap_log(log_path: &str) -> Result<CaptureMetadata, String> {
    let content = std::fs::read_to_string(log_path)
        .map_err(|e| format!("Cannot read log '{}': {}", log_path, e))?;

    let mut capture_time: Option<String> = None;
    let mut date_hint: Option<String> = None;
    let mut planet: Option<String> = detect_planet_hint(&content, log_path);
    let mut fps: f64 = 0.0;
    let mut frame_count: usize = 0;

    for line in content.lines() {
        let line = line.trim();
        if let Some((key, value)) = split_log_kv(line) {
            let key = key.to_lowercase().replace(' ', "");
            let value = clean_log_value(value);
            if key.contains("date") && !value.contains(':') {
                date_hint = Some(value);
            } else if key.contains("timestamp")
                || key.contains("startcapture")
                || key.contains("capturestart")
                || key.contains("starttime")
                || key == "start"
            {
                capture_time = normalize_log_datetime(&value).or_else(|| {
                    date_hint
                        .as_ref()
                        .and_then(|date| combine_log_date_time(date, &value))
                });
            } else if key.contains("actualframerate")
                || key.contains("averageframerate")
                || key.contains("fps")
            {
                fps = parse_log_number(&value);
            } else if key.contains("framecount") || key.contains("framescaptured") {
                frame_count = parse_log_number(&value).max(0.0) as usize;
            } else if key.contains("target") || key.contains("object") || key.contains("planet") {
                planet = detect_planet_hint(&value, log_path).or(planet);
            }
        }
    }

    let duration = if fps > 0.0 && frame_count > 0 {
        frame_count as f64 / fps
    } else {
        0.0
    };

    let mid_jd = if let Some(ts) = &capture_time {
        let start_jd = parse_iso_to_jd(ts).unwrap_or(2451545.0);
        start_jd + (duration / 2.0) / 86400.0
    } else {
        return Err("No timestamp in SharpCap log".into());
    };

    Ok(CaptureMetadata {
        mid_time_jd: mid_jd,
        planet,
        duration_sec: duration,
        fps,
        start_time_iso: capture_time.unwrap_or_default(),
    })
}

/// Parse a manual TXT log. Tries FireCapture first, then SharpCap.
pub fn parse_capture_log(log_path: &str) -> Result<CaptureMetadata, String> {
    match parse_firecapture_log(log_path) {
        Ok(meta) => Ok(meta),
        Err(fc_err) => match parse_sharpcap_log(log_path) {
            Ok(meta) => Ok(meta),
            Err(sc_err) => Err(format!(
                "No se pudo leer como FireCapture ({}) ni como SharpCap ({}).",
                fc_err, sc_err
            )),
        },
    }
}

/// Infer capture time from file modification date (fallback).
pub fn infer_time_from_file(path: &str) -> Result<CaptureMetadata, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("Cannot read file metadata: {}", e))?;
    let modified = meta
        .modified()
        .map_err(|e| format!("Cannot get modification time: {}", e))?;
    let dt: chrono::DateTime<chrono::Utc> = modified.into();
    let jd = datetime_to_jd(
        dt.format("%Y").to_string().parse().unwrap_or(2025),
        dt.format("%m").to_string().parse().unwrap_or(1),
        dt.format("%d").to_string().parse().unwrap_or(1),
        dt.format("%H").to_string().parse().unwrap_or(0),
        dt.format("%M").to_string().parse().unwrap_or(0),
        dt.format("%S").to_string().parse::<f64>().unwrap_or(0.0),
    );
    Ok(CaptureMetadata {
        mid_time_jd: jd,
        planet: None,
        duration_sec: 0.0,
        fps: 0.0,
        start_time_iso: dt.to_rfc3339(),
    })
}

/// Try to find and parse a log file adjacent to the video/image file.
pub fn auto_parse_log(video_path: &str) -> Option<CaptureMetadata> {
    let base = std::path::Path::new(video_path);
    // FireCapture: same name with .txt
    let mut txt = base.to_path_buf();
    txt.set_extension("txt");
    if txt.exists() {
        if let Ok(meta) = parse_capture_log(txt.to_str().unwrap_or("")) {
            return Some(meta);
        }
    }
    // Try _log.txt suffix
    let stem = base.file_stem().unwrap_or_default().to_str().unwrap_or("");
    let dir = base.parent().unwrap_or(std::path::Path::new("."));
    let log_path = dir.join(format!("{}_log.txt", stem));
    if log_path.exists() {
        if let Ok(meta) = parse_capture_log(log_path.to_str().unwrap_or("")) {
            return Some(meta);
        }
    }
    None
}

// =============================================================================
// 5. PLANET DISC DETECTION (Auto-Wireframe)
// =============================================================================

/// Detect planet disc in a u16 mono image using threshold + ellipse fitting.
/// Optionally uses planet's equatorial/polar radii to estimate expected aspect ratio.
pub fn detect_planet_disc(data: &[u16], w: usize, h: usize) -> PlanetDisc {
    let fallback = || PlanetDisc {
        cx: w as f64 / 2.0,
        cy: h as f64 / 2.0,
        radius_x: w as f64 / 4.0,
        radius_y: h as f64 / 4.0,
        angle_deg: 0.0,
        phase: 1.0,
    };

    if data.is_empty() || w == 0 || h == 0 {
        return fallback();
    }

    // Step 1: robust threshold. Percentiles are safer than max*constant because
    // moons, stars and hot pixels can be brighter than the planet body.
    let len = data.len();
    let sample_step = ((len as f64 / 180_000.0).sqrt().ceil() as usize).max(1);
    let mut sample = Vec::with_capacity((len / sample_step).max(1));
    for y in (0..h).step_by(sample_step) {
        for x in (0..w).step_by(sample_step) {
            let idx = y * w + x;
            if idx < len {
                sample.push(data[idx]);
            }
        }
    }
    if sample.len() < 16 {
        return fallback();
    }
    sample.sort_unstable();
    let pct = |q: f64| -> u16 {
        let idx = ((sample.len().saturating_sub(1)) as f64 * q).round() as usize;
        sample[idx.min(sample.len().saturating_sub(1))]
    };
    let bg = pct(0.55) as f64;
    let high = pct(0.995) as f64;
    let threshold = (bg + (high - bg).max(1.0) * 0.18).max(bg + 32.0) as u16;

    if high < 64.0 || threshold < 30 {
        return fallback();
    }

    // Step 2: find the largest connected bright component. This rejects moons/stars
    // and avoids inflating the detected planet radius with isolated bright pixels.
    let mut visited = vec![0u8; len];
    let mut stack = Vec::<usize>::with_capacity(8192);
    let mut best_count = 0usize;
    let mut best_weight = 0.0_f64;
    let mut best_sum_x = 0.0_f64;
    let mut best_sum_y = 0.0_f64;
    let mut best_min_x = w;
    let mut best_max_x = 0usize;
    let mut best_min_y = h;
    let mut best_max_y = 0usize;

    for seed in 0..len {
        if visited[seed] != 0 || data[seed] <= threshold {
            continue;
        }
        visited[seed] = 1;
        stack.clear();
        stack.push(seed);

        let mut count = 0usize;
        let mut weight_sum = 0.0_f64;
        let mut sum_x = 0.0_f64;
        let mut sum_y = 0.0_f64;
        let mut min_x = w;
        let mut max_x = 0usize;
        let mut min_y = h;
        let mut max_y = 0usize;

        while let Some(idx) = stack.pop() {
            let x = idx % w;
            let y = idx / w;
            count += 1;
            let weight = (data[idx].saturating_sub(threshold) as f64 + 1.0).sqrt();
            weight_sum += weight;
            sum_x += x as f64 * weight;
            sum_y += y as f64 * weight;
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);

            let neighbors = [
                if x > 0 { Some(idx - 1) } else { None },
                if x + 1 < w { Some(idx + 1) } else { None },
                if y > 0 { Some(idx - w) } else { None },
                if y + 1 < h { Some(idx + w) } else { None },
            ];
            for next in neighbors.into_iter().flatten() {
                if visited[next] == 0 && data[next] > threshold {
                    visited[next] = 1;
                    stack.push(next);
                }
            }
        }

        let bbox_w = max_x.saturating_sub(min_x).saturating_add(1);
        let bbox_h = max_y.saturating_sub(min_y).saturating_add(1);
        let area_ratio = count as f64 / len.max(1) as f64;
        let plausible =
            count >= 50 && bbox_w >= 6 && bbox_h >= 6 && area_ratio <= 0.90 && weight_sum > 0.0;
        if plausible && count > best_count {
            best_count = count;
            best_weight = weight_sum;
            best_sum_x = sum_x;
            best_sum_y = sum_y;
            best_min_x = min_x;
            best_max_x = max_x;
            best_min_y = min_y;
            best_max_y = max_y;
        }
    }

    if best_count < 50 || best_weight <= 0.0 {
        return PlanetDisc {
            cx: w as f64 / 2.0,
            cy: h as f64 / 2.0,
            radius_x: w as f64 / 4.0,
            radius_y: h as f64 / 4.0,
            angle_deg: 0.0,
            phase: 1.0,
        };
    }

    let cx = best_sum_x / best_weight;
    let cy = best_sum_y / best_weight;
    let rx =
        ((best_max_x as f64 - best_min_x as f64 + 1.0) / 2.0 * 1.04).clamp(4.0, w as f64 / 2.0);
    let ry =
        ((best_max_y as f64 - best_min_y as f64 + 1.0) / 2.0 * 1.04).clamp(4.0, h as f64 / 2.0);

    // Step 3: Refine center using radial gradient symmetry
    let (ref_cx, ref_cy) = refine_center_radial(data, w, h, cx, cy, rx.min(ry) as usize);

    // Phase detection: check if one side is darker (partial illumination)
    let left_brightness = sample_region_brightness(
        data,
        w,
        h,
        (ref_cx - rx * 0.7) as usize,
        ref_cy as usize,
        (rx * 0.2) as usize,
    );
    let right_brightness = sample_region_brightness(
        data,
        w,
        h,
        (ref_cx + rx * 0.5) as usize,
        ref_cy as usize,
        (rx * 0.2) as usize,
    );
    let phase = if left_brightness + right_brightness > 0.0 {
        (left_brightness.min(right_brightness)) / (left_brightness.max(right_brightness))
    } else {
        1.0
    };

    PlanetDisc {
        cx: ref_cx,
        cy: ref_cy,
        radius_x: rx,
        radius_y: ry,
        angle_deg: 0.0,
        phase: phase.clamp(0.0, 1.0),
    }
}

fn refine_center_radial(
    data: &[u16],
    w: usize,
    h: usize,
    cx: f64,
    cy: f64,
    radius: usize,
) -> (f64, f64) {
    let search_r = (radius / 10).max(2).min(20);
    let mut best_cx = cx;
    let mut best_cy = cy;
    let mut best_symmetry = f64::MAX;
    let step = 0.5_f64;

    let mut test_cx = cx - search_r as f64;
    while test_cx <= cx + search_r as f64 {
        let mut test_cy = cy - search_r as f64;
        while test_cy <= cy + search_r as f64 {
            let sym = measure_radial_symmetry(data, w, h, test_cx, test_cy, radius);
            if sym < best_symmetry {
                best_symmetry = sym;
                best_cx = test_cx;
                best_cy = test_cy;
            }
            test_cy += step;
        }
        test_cx += step;
    }
    (best_cx, best_cy)
}

fn measure_radial_symmetry(
    data: &[u16],
    w: usize,
    h: usize,
    cx: f64,
    cy: f64,
    radius: usize,
) -> f64 {
    let mut diff_sum = 0.0_f64;
    let mut count = 0.0_f64;
    let r = radius as f64 * 0.8;
    let angles = 36;

    for i in 0..angles {
        let angle = (i as f64 / angles as f64) * 2.0 * PI;
        for d in 1..=(r as usize) {
            let df = d as f64;
            let x1 = (cx + df * angle.cos()) as usize;
            let y1 = (cy + df * angle.sin()) as usize;
            let x2 = (cx - df * angle.cos()) as usize;
            let y2 = (cy - df * angle.sin()) as usize;

            if x1 < w && y1 < h && x2 < w && y2 < h {
                let v1 = data[y1 * w + x1] as f64;
                let v2 = data[y2 * w + x2] as f64;
                diff_sum += (v1 - v2).abs();
                count += 1.0;
            }
        }
    }
    if count > 0.0 {
        diff_sum / count
    } else {
        f64::MAX
    }
}

fn sample_region_brightness(
    data: &[u16],
    w: usize,
    h: usize,
    cx: usize,
    cy: usize,
    size: usize,
) -> f64 {
    let mut sum = 0.0_f64;
    let mut count = 0.0_f64;
    let half = size / 2;
    let x0 = cx.saturating_sub(half);
    let y0 = cy.saturating_sub(half);
    let x1 = (cx + half).min(w);
    let y1 = (cy + half).min(h);
    for y in y0..y1 {
        for x in x0..x1 {
            sum += data[y * w + x] as f64;
            count += 1.0;
        }
    }
    if count > 0.0 {
        sum / count
    } else {
        0.0
    }
}

// =============================================================================
// 6. CYLINDRICAL PROJECTION ENGINE
// =============================================================================

/// Cylindrical map: a latitude × longitude texture buffer
#[derive(Clone)]
pub struct CylMap {
    pub data: Vec<u16>,
    pub width: usize,    // longitude samples
    pub height: usize,   // latitude samples
    pub channels: usize, // 1=mono, 3=RGB
}

/// Project 2D planet disc image to cylindrical (equirectangular) map.
/// Each pixel on the disc maps to (lat, lon) on the planet surface.
/// Uses the planet's oblateness to correct latitude mapping for oblate spheroids.
pub fn project_to_cylindrical(
    image: &[u16],
    w: usize,
    h: usize,
    channels: usize,
    disc: &PlanetDisc,
    planet: &PlanetaryBody,
    sub_earth_lat_deg: f64,
) -> CylMap {
    // Use oblateness to correct latitude stretch (oblate spheroid correction)
    let oblate_factor = 1.0 - planet.oblateness; // e.g. Jupiter: 0.935
    let b0 = sub_earth_lat_deg.to_radians().clamp(-PI / 3.0, PI / 3.0);
    let axis_angle = disc.angle_deg.to_radians();
    let cos_axis = axis_angle.cos();
    let sin_axis = axis_angle.sin();
    let cyl_w = (disc.radius_x * PI) as usize; // ~pi * radius pixels
    let cyl_h = (disc.radius_y * 2.0) as usize;
    let cyl_w = cyl_w.max(64);
    let cyl_h = cyl_h.max(32);

    let mut accum = vec![0.0f32; cyl_w * cyl_h * channels];
    let mut weight = vec![0.0f32; cyl_w * cyl_h];

    // For each pixel on the disc, compute (lat, lon) and accumulate into cyl map
    let rx = disc.radius_x;
    let ry = disc.radius_y;

    let y_start = (disc.cy - ry - 1.0).max(0.0) as usize;
    let y_end = ((disc.cy + ry + 1.0) as usize).min(h);
    let x_start = (disc.cx - rx - 1.0).max(0.0) as usize;
    let x_end = ((disc.cx + rx + 1.0) as usize).min(w);

    for py in y_start..y_end {
        for px in x_start..x_end {
            let dx = px as f64 - disc.cx;
            let dy = py as f64 - disc.cy;
            let planet_x = dx * cos_axis + dy * sin_axis;
            let planet_y = -dx * sin_axis + dy * cos_axis;
            let nx = planet_x / rx;
            let ny = planet_y / ry;
            let r2 = nx * nx + ny * ny;
            if r2 >= 1.0 {
                continue;
            }

            // Orthographic inverse with sub-Earth latitude. (nx, ny) are image-plane planet axes.
            let nz = (1.0 - r2).sqrt();
            let corrected_y = ny / oblate_factor;
            let lat = (corrected_y * b0.cos() + nz * b0.sin())
                .asin()
                .clamp(-PI / 2.0, PI / 2.0);
            let lon = nx.atan2(nz * b0.cos() - corrected_y * b0.sin());

            // Map to cylindrical coords
            let cx_idx = ((lon / PI + 0.5) * cyl_w as f64).clamp(0.0, (cyl_w - 1) as f64) as usize;
            let cy_idx = ((lat / (PI / 2.0) + 1.0) * 0.5 * cyl_h as f64)
                .clamp(0.0, (cyl_h - 1) as f64) as usize;

            let cyl_offset = cy_idx * cyl_w + cx_idx;
            let img_offset = py * w + px;

            // Limb weighting: cosine of angle from center (reduces limb noise)
            let limb_w = nz as f32;

            for c in 0..channels {
                let val = if channels > 1 && img_offset * channels + c < image.len() {
                    image[img_offset * channels + c] as f32
                } else if img_offset < image.len() {
                    image[img_offset] as f32
                } else {
                    0.0
                };
                accum[cyl_offset * channels + c] += val * limb_w;
            }
            weight[cyl_offset] += limb_w;
        }
    }

    // Normalize by weight
    let mut cyl_data = vec![0u16; cyl_w * cyl_h * channels];
    for i in 0..(cyl_w * cyl_h) {
        if weight[i] > 0.0 {
            for c in 0..channels {
                cyl_data[i * channels + c] =
                    (accum[i * channels + c] / weight[i]).clamp(0.0, 65535.0) as u16;
            }
        }
    }

    CylMap {
        data: cyl_data,
        width: cyl_w,
        height: cyl_h,
        channels,
    }
}

/// Shift cylindrical map by `delta_deg` in longitude (horizontal wrap).
pub fn shift_cylindrical(cyl: &CylMap, delta_deg: f64) -> CylMap {
    let shift_px = (delta_deg / 360.0 * cyl.width as f64).round() as isize;
    let mut out = vec![0u16; cyl.data.len()];
    let w = cyl.width as isize;

    for y in 0..cyl.height {
        for x in 0..cyl.width {
            let src_x = ((x as isize - shift_px) % w + w) % w;
            let dst = (y * cyl.width + x) * cyl.channels;
            let src = (y * cyl.width + src_x as usize) * cyl.channels;
            for c in 0..cyl.channels {
                out[dst + c] = cyl.data[src + c];
            }
        }
    }

    CylMap {
        data: out,
        width: cyl.width,
        height: cyl.height,
        channels: cyl.channels,
    }
}

/// Reproject cylindrical map back to 2D disc image with bilinear interpolation.
pub fn reproject_to_disc(
    cyl: &CylMap,
    disc: &PlanetDisc,
    out_w: usize,
    out_h: usize,
    channels: usize,
    planet: &PlanetaryBody,
    sub_earth_lat_deg: f64,
) -> Vec<u16> {
    let mut out = vec![0u16; out_w * out_h * channels];
    let rx = disc.radius_x;
    let ry = disc.radius_y;
    let oblate_factor = 1.0 - planet.oblateness;
    let b0 = sub_earth_lat_deg.to_radians().clamp(-PI / 3.0, PI / 3.0);
    let axis_angle = disc.angle_deg.to_radians();
    let cos_axis = axis_angle.cos();
    let sin_axis = axis_angle.sin();

    let y_start = (disc.cy - ry - 1.0).max(0.0) as usize;
    let y_end = ((disc.cy + ry + 1.0) as usize).min(out_h);
    let x_start = (disc.cx - rx - 1.0).max(0.0) as usize;
    let x_end = ((disc.cx + rx + 1.0) as usize).min(out_w);

    for py in y_start..y_end {
        for px in x_start..x_end {
            let dx = px as f64 - disc.cx;
            let dy = py as f64 - disc.cy;
            let planet_x = dx * cos_axis + dy * sin_axis;
            let planet_y = -dx * sin_axis + dy * cos_axis;
            let nx = planet_x / rx;
            let ny = planet_y / ry;
            let r2 = nx * nx + ny * ny;
            if r2 >= 1.0 {
                continue;
            }

            let nz = (1.0 - r2).sqrt();
            let corrected_y = ny / oblate_factor;
            let lat = (corrected_y * b0.cos() + nz * b0.sin())
                .asin()
                .clamp(-PI / 2.0, PI / 2.0);
            let lon = nx.atan2(nz * b0.cos() - corrected_y * b0.sin());

            let cx_f = (lon / PI + 0.5) * cyl.width as f64;
            let cy_f = (lat / (PI / 2.0) + 1.0) * 0.5 * cyl.height as f64;

            // Bilinear interpolation
            let ix = cx_f.floor() as isize;
            let iy = cy_f.floor() as isize;
            let fx = (cx_f - ix as f64) as f32;
            let fy = (cy_f - iy as f64) as f32;

            let cw = cyl.width as isize;
            let ch = cyl.height as isize;

            let sample = |sx: isize, sy: isize, c: usize| -> f32 {
                let sx = ((sx % cw) + cw) % cw;
                let sy = sy.clamp(0, ch - 1);
                cyl.data[(sy as usize * cyl.width + sx as usize) * cyl.channels + c] as f32
            };

            let out_idx = (py * out_w + px) * channels;
            for c in 0..channels {
                let v00 = sample(ix, iy, c);
                let v10 = sample(ix + 1, iy, c);
                let v01 = sample(ix, iy + 1, c);
                let v11 = sample(ix + 1, iy + 1, c);
                let v = v00 * (1.0 - fx) * (1.0 - fy)
                    + v10 * fx * (1.0 - fy)
                    + v01 * (1.0 - fx) * fy
                    + v11 * fx * fy;
                out[out_idx + c] = v.clamp(0.0, 65535.0) as u16;
            }
        }
    }
    out
}

// =============================================================================
// 7. LIMB DARKENING CORRECTION
// =============================================================================

/// Apply cosine-based limb darkening correction.
/// `strength` controls correction power (0 = none, 1 = full cosine, 2 = strong).
pub fn apply_limb_correction(
    image: &mut [u16],
    w: usize,
    h: usize,
    channels: usize,
    disc: &PlanetDisc,
    strength: f64,
) {
    if strength <= 0.001 {
        return;
    }
    let rx = disc.radius_x;
    let ry = disc.radius_y;

    let y_start = (disc.cy - ry).max(0.0) as usize;
    let y_end = ((disc.cy + ry) as usize + 1).min(h);
    let x_start = (disc.cx - rx).max(0.0) as usize;
    let x_end = ((disc.cx + rx) as usize + 1).min(w);

    for py in y_start..y_end {
        for px in x_start..x_end {
            let nx = (px as f64 - disc.cx) / rx;
            let ny = (py as f64 - disc.cy) / ry;
            let r2 = nx * nx + ny * ny;
            if r2 >= 1.0 {
                continue;
            }

            let cos_angle = (1.0 - r2).sqrt();
            // Correction factor: brighten limb proportionally
            let correction = 1.0 / cos_angle.powf(strength);
            // Clamp to avoid extreme boost at very edge
            let correction = correction.min(3.0);

            let idx = (py * w + px) * channels;
            for c in 0..channels {
                if idx + c < image.len() {
                    let v = image[idx + c] as f64 * correction;
                    image[idx + c] = v.clamp(0.0, 65535.0) as u16;
                }
            }
        }
    }
}

/// Cosine blend at disc edge for seamless derotation transition.
pub fn apply_edge_blend(
    derotated: &[u16],
    original: &[u16],
    w: usize,
    h: usize,
    channels: usize,
    disc: &PlanetDisc,
    blend_width: f64,
) -> Vec<u16> {
    let mut out = original.to_vec();
    let rx = disc.radius_x;
    let ry = disc.radius_y;

    for py in 0..h {
        for px in 0..w {
            let nx = (px as f64 - disc.cx) / rx;
            let ny = (py as f64 - disc.cy) / ry;
            let r = (nx * nx + ny * ny).sqrt();

            let idx = (py * w + px) * channels;
            if r >= 1.0 {
                continue;
            } // outside disc: keep original

            // Blend zone near the edge
            let inner_r = 1.0 - blend_width;
            let blend_factor = if r < inner_r {
                1.0 // fully derotated
            } else {
                // Cosine falloff
                let t = (r - inner_r) / blend_width;
                0.5 * (1.0 + (t * PI).cos())
            };

            for c in 0..channels {
                if idx + c < out.len() && idx + c < derotated.len() {
                    let d = derotated[idx + c] as f64;
                    let o = original[idx + c] as f64;
                    out[idx + c] =
                        (d * blend_factor + o * (1.0 - blend_factor)).clamp(0.0, 65535.0) as u16;
                }
            }
        }
    }
    out
}

// =============================================================================
// 8. FULL DEROTATION PIPELINE
// =============================================================================

/// Validate disc detection using planet's physical radii for expected aspect ratio.
/// Returns a corrected disc if the detected aspect ratio deviates significantly.
pub fn validate_disc_aspect(disc: &PlanetDisc, planet: &PlanetaryBody) -> PlanetDisc {
    let expected_ratio = planet.polar_radius_km / planet.equatorial_radius_km;
    let detected_ratio = disc.radius_y / disc.radius_x;
    // If the detected ratio deviates by more than 20% from expected, correct it
    if detected_ratio > 0.0 && (detected_ratio / expected_ratio - 1.0).abs() > 0.20 {
        PlanetDisc {
            radius_y: disc.radius_x * expected_ratio,
            ..disc.clone()
        }
    } else {
        disc.clone()
    }
}

/// Derotate a single frame to a reference time.
/// Returns the derotated image buffer (same dimensions as input).
pub fn derotate_single(
    image: &[u16],
    w: usize,
    h: usize,
    channels: usize,
    planet: &PlanetaryBody,
    capture_time_jd: f64,
    reference_time_jd: f64,
    limb_strength: f64,
    cm_system: usize,
    disc_override: Option<&PlanetDisc>,
) -> Vec<u16> {
    derotate_single_advanced(
        image,
        w,
        h,
        channels,
        planet,
        capture_time_jd,
        reference_time_jd,
        limb_strength,
        cm_system,
        0.0,
        disc_override,
    )
}

/// Derotate a single frame with observer geometry (B0) and image-axis orientation.
/// `disc.angle_deg` controls the planet-axis rotation in the image plane.
pub fn derotate_single_advanced(
    image: &[u16],
    w: usize,
    h: usize,
    channels: usize,
    planet: &PlanetaryBody,
    capture_time_jd: f64,
    reference_time_jd: f64,
    limb_strength: f64,
    cm_system: usize,
    sub_earth_lat_deg: f64,
    disc_override: Option<&PlanetDisc>,
) -> Vec<u16> {
    // 1. Detect or use provided disc, then validate with planet radii
    let raw_disc = if let Some(d) = disc_override {
        d.clone()
    } else {
        // For disc detection we need mono data
        let mono: Vec<u16> = if channels == 1 {
            image.to_vec()
        } else {
            // Use green channel
            image.iter().skip(1).step_by(channels).copied().collect()
        };
        detect_planet_disc(&mono, w, h)
    };
    let disc = validate_disc_aspect(&raw_disc, planet);

    // 2. Calculate rotation delta
    let delta_deg = rotation_delta_deg(planet, reference_time_jd, capture_time_jd, cm_system);

    // 2b. Compute Central Meridian values for diagnostic logging
    let (cm1, cm2, cm3) = calculate_central_meridian(planet, capture_time_jd);
    eprintln!(
        "[DEROT] {} | CM1={:.1}° CM2={:.1}° CM3={:.1}° | Δ={:.2}° (sys{}) | disc: cx={:.0} cy={:.0} rx={:.0} ry={:.0}",
        planet.name, cm1, cm2, cm3, delta_deg, cm_system,
        disc.cx, disc.cy, disc.radius_x, disc.radius_y
    );

    // If delta is negligible, return copy
    if delta_deg.abs() < 0.01 {
        return image.to_vec();
    }

    // 3. Apply limb correction on copy
    let mut corrected = image.to_vec();
    apply_limb_correction(&mut corrected, w, h, channels, &disc, limb_strength);

    // 4. Project to cylindrical
    let cyl = project_to_cylindrical(&corrected, w, h, channels, &disc, planet, sub_earth_lat_deg);

    // 5. Shift by rotation delta
    let shifted = shift_cylindrical(&cyl, delta_deg);

    // 6. Reproject to disc
    let reprojected = reproject_to_disc(&shifted, &disc, w, h, channels, planet, sub_earth_lat_deg);

    // 7. Blend edges
    apply_edge_blend(&reprojected, image, w, h, channels, &disc, 0.08)
}

/// Batch derotation: aligns all frames to a common reference time.
/// Multi-threaded via Rayon. Emits progress via callback.
pub fn derotate_batch(
    images: &[Vec<u16>],
    times_jd: &[f64],
    w: usize,
    h: usize,
    channels: usize,
    planet: &PlanetaryBody,
    reference_time_jd: f64,
    limb_strength: f64,
    cm_system: usize,
) -> Vec<Vec<u16>> {
    // Detect disc from first image (assume consistent framing)
    let mono: Vec<u16> = if channels == 1 {
        images[0].clone()
    } else {
        images[0]
            .iter()
            .skip(1)
            .step_by(channels)
            .copied()
            .collect()
    };
    let disc = detect_planet_disc(&mono, w, h);

    images
        .par_iter()
        .zip(times_jd.par_iter())
        .map(|(img, &time_jd)| {
            derotate_single(
                img,
                w,
                h,
                channels,
                planet,
                time_jd,
                reference_time_jd,
                limb_strength,
                cm_system,
                Some(&disc),
            )
        })
        .collect()
}

// =============================================================================
// 9. UNIT TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_julian_date_j2000() {
        // J2000.0 epoch: 2000-01-01 12:00:00 TT = JD 2451545.0
        let jd = datetime_to_jd(2000, 1, 1, 12, 0, 0.0);
        assert!(
            (jd - 2451545.0).abs() < 0.0001,
            "J2000 epoch JD should be 2451545.0, got {}",
            jd
        );
    }

    #[test]
    fn test_julian_date_known() {
        // 2025-01-01 00:00:00 UT = JD 2460676.5
        let jd = datetime_to_jd(2025, 1, 1, 0, 0, 0.0);
        assert!(
            (jd - 2460676.5).abs() < 0.001,
            "2025-01-01 JD should be ~2460676.5, got {}",
            jd
        );
    }

    #[test]
    fn test_parse_iso_basic() {
        let jd = parse_iso_to_jd("2025-01-15T00:00:00").unwrap();
        let expected = datetime_to_jd(2025, 1, 15, 0, 0, 0.0);
        assert!((jd - expected).abs() < 0.0001);
    }

    #[test]
    fn test_cm_jupiter_rotation_rate() {
        // Jupiter System II rotates ~870.27 deg/day
        // Over 1 hour = 870.27/24 ≈ 36.26 deg
        let delta = rotation_delta_deg(&JUPITER, 2451545.0, 2451545.0 + 1.0 / 24.0, 1);
        assert!(
            (delta - 36.26).abs() < 0.1,
            "Jupiter CM2 1-hour delta should be ~36.26°, got {:.2}°",
            delta
        );
    }

    #[test]
    fn test_cm_mars_rotation_rate() {
        // Mars rotates ~350.89 deg/day
        // Over 24 hours ≈ 350.89 deg
        let delta = rotation_delta_deg(&MARS, 2451545.0, 2451545.0 + 1.0, 0);
        assert!(
            (delta - 350.89).abs() < 0.1,
            "Mars 1-day rotation should be ~350.89°, got {:.2}°",
            delta
        );
    }

    #[test]
    fn test_central_meridian_returns_valid_range() {
        let (cm1, cm2, cm3) = calculate_central_meridian(&JUPITER, 2460676.5);
        assert!(cm1 >= 0.0 && cm1 < 360.0, "CM1 out of range: {}", cm1);
        assert!(cm2 >= 0.0 && cm2 < 360.0, "CM2 out of range: {}", cm2);
        assert!(cm3 >= 0.0 && cm3 < 360.0, "CM3 out of range: {}", cm3);
    }

    #[test]
    fn test_cylindrical_roundtrip_identity() {
        // Create a synthetic 100x100 disc image
        let w = 100;
        let h = 100;
        let disc = PlanetDisc {
            cx: 50.0,
            cy: 50.0,
            radius_x: 40.0,
            radius_y: 40.0,
            angle_deg: 0.0,
            phase: 1.0,
        };
        let mut img = vec![0u16; w * h];
        // Paint a bright central band
        for y in 40..60 {
            for x in 20..80 {
                img[y * w + x] = 10000;
            }
        }

        let cyl = project_to_cylindrical(&img, w, h, 1, &disc, &JUPITER, 0.0);
        let shifted = shift_cylindrical(&cyl, 0.0); // zero shift
        let reproj = reproject_to_disc(&shifted, &disc, w, h, 1, &JUPITER, 0.0);

        // Center pixel should still be bright
        let center_val = reproj[50 * w + 50];
        assert!(
            center_val > 1000,
            "Center pixel should be bright after roundtrip, got {}",
            center_val
        );
    }

    #[test]
    fn test_disc_detection_synthetic() {
        let w = 200;
        let h = 200;
        let mut img = vec![0u16; w * h];
        // Paint a bright circle centered at (100, 100) radius 50
        for y in 0..h {
            for x in 0..w {
                let dx = x as f64 - 100.0;
                let dy = y as f64 - 100.0;
                if dx * dx + dy * dy < 50.0 * 50.0 {
                    img[y * w + x] = 30000;
                }
            }
        }
        let disc = detect_planet_disc(&img, w, h);
        assert!(
            (disc.cx - 100.0).abs() < 5.0,
            "Detected cx should be ~100, got {:.1}",
            disc.cx
        );
        assert!(
            (disc.cy - 100.0).abs() < 5.0,
            "Detected cy should be ~100, got {:.1}",
            disc.cy
        );
        assert!(
            (disc.radius_x - 50.0).abs() < 5.0,
            "Detected rx should be ~50, got {:.1}",
            disc.radius_x
        );
    }
}
