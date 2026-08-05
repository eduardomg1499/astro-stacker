//! Extracción normalizada de CalibrationSignature desde cabeceras FITS.
//!
//! El lector concreto (fitrs) entrega aquí un mapa clave/valor; esta capa
//! concentra aliases, fase CFA/ROI y diagnóstico de metadata obligatoria.

#![allow(dead_code)]

use std::collections::BTreeMap;

use crate::deepsky_calibration_contract::CalibrationRole;
use crate::pipeline::{CalibrationSignature, StoreLayout};

pub(crate) type NormalizedHeaderMap = BTreeMap<String, String>;

/// Lista acotada que el adapter fitrs debe consultar. Mantenerla aquí evita
/// que la extracción de firma y el lector concreto diverjan en sus aliases.
pub(crate) const CALIBRATION_HEADER_KEYS: &[&str] = &[
    "INSTRUME",
    "CAMERA",
    "CAMMODEL",
    "CAMERAMOD",
    "SENSOR",
    "DETECTOR",
    "SENSORMOD",
    "READMODE",
    "READOUTM",
    "READOUT",
    "CAMMODE",
    "GAIN",
    "EGAIN",
    "CAM-GAIN",
    "ISOSPEED",
    "ISO",
    "ISO_SPEED",
    "OFFSET",
    "BLKLEVEL",
    "BLACKLEV",
    "CAM-OFFS",
    "CCD-TEMP",
    "CCDTEMP",
    "SENSOR_TEMP",
    "SET-TEMP",
    "EXPTIME",
    "EXPOSURE",
    "EXP_TIME",
    "XBINNING",
    "BINX",
    "XBIN",
    "YBINNING",
    "BINY",
    "YBIN",
    "BINNING",
    "CCDBIN",
    "XORGSUBF",
    "XORIGIN",
    "ROI_X",
    "STARTX",
    "YORGSUBF",
    "YORIGIN",
    "ROI_Y",
    "STARTY",
    "BAYERPAT",
    "BAYERPATN",
    "CFAPAT",
    "CFA",
    "XBAYROFF",
    "BAYOFFX",
    "CFAOFFX",
    "YBAYROFF",
    "BAYOFFY",
    "CFAOFFY",
    "FILTER",
    "FILTERID",
    "FILTNAME",
    "DATE-OBS",
    "DATE-LOC",
    "DATE",
    "TELESCOP",
    "TELESCOPE",
    "LENS",
    "FOCALLEN",
    "FOCLEN",
    "FOCAL_LENGTH",
    "FOCRATIO",
    "F_RATIO",
    "APTDIA",
    "BITDEPTH",
    "ADC_BITS",
    "SATURATE",
    "SATLEVEL",
    "WHITELEV",
    "WHITE_LEVEL",
];

#[derive(Clone, Debug)]
pub(crate) struct SignatureContext {
    pub width: u32,
    pub height: u32,
    pub channels: u8,
    pub session_hint: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct SignatureExtraction {
    pub signature: CalibrationSignature,
    /// None means the stored layout cannot be proven from the metadata.
    pub layout: Option<StoreLayout>,
    pub warnings: Vec<String>,
}

pub(crate) fn normalize_header_map<I, K, V>(entries: I) -> NormalizedHeaderMap
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    entries
        .into_iter()
        .map(|(key, value)| {
            (
                key.as_ref().trim().to_ascii_uppercase(),
                clean_header_value(value.as_ref()),
            )
        })
        .filter(|(key, value)| !key.is_empty() && !value.is_empty())
        .collect()
}

fn clean_header_value(value: &str) -> String {
    let mut value = value.trim();
    // fitrs expone HeaderValue y su representación Debug conserva el nombre
    // de variante: Float(300.0), Integer(1), CharacterString("Ha").
    // Quitar wrappers conocidos antes del parseo mantiene el adapter liviano.
    for _ in 0..3 {
        let Some(open) = value.find('(') else {
            break;
        };
        if !value.ends_with(')')
            || !value[..open]
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            break;
        }
        value = &value[open + 1..value.len() - 1];
        value = value.trim();
    }
    let without_comment = value.split(" / ").next().unwrap_or(value).trim();
    without_comment
        .trim_matches(|character| matches!(character, '\'' | '"' | '(' | ')' | '[' | ']'))
        .trim()
        .to_string()
}

fn first<'a>(headers: &'a NormalizedHeaderMap, aliases: &[&str]) -> Option<&'a str> {
    aliases
        .iter()
        .find_map(|alias| headers.get(*alias).map(String::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn first_string(headers: &NormalizedHeaderMap, aliases: &[&str]) -> Option<String> {
    first(headers, aliases).map(ToOwned::to_owned)
}

fn numeric_prefix(value: &str) -> Option<f64> {
    let token: String = value
        .trim()
        .chars()
        .take_while(|character| {
            character.is_ascii_digit() || matches!(character, '.' | '-' | '+' | 'e' | 'E')
        })
        .collect();
    (!token.is_empty())
        .then(|| token.parse::<f64>().ok())
        .flatten()
}

fn first_number(headers: &NormalizedHeaderMap, aliases: &[&str]) -> Option<f64> {
    first(headers, aliases).and_then(numeric_prefix)
}

fn first_u32(headers: &NormalizedHeaderMap, aliases: &[&str]) -> Option<u32> {
    let value = first_number(headers, aliases)?;
    (value.is_finite() && value >= 0.0 && value <= u32::MAX as f64).then_some(value.round() as u32)
}

fn parse_binning_pair(value: &str) -> Option<(u32, u32)> {
    let normalized = value
        .to_ascii_lowercase()
        .replace(['×', '*', ',', ' '], "x");
    let mut values = normalized
        .split('x')
        .filter(|value| !value.is_empty())
        .filter_map(|value| value.parse::<u32>().ok());
    let x = values.next()?;
    let y = values.next().unwrap_or(x);
    (x > 0 && y > 0).then_some((x, y))
}

fn normalize_cfa_pattern(value: &str) -> Option<String> {
    let pattern: String = value
        .chars()
        .filter(|character| matches!(character.to_ascii_uppercase(), 'R' | 'G' | 'B'))
        .map(|character| character.to_ascii_uppercase())
        .collect();
    matches!(pattern.as_str(), "RGGB" | "GRBG" | "GBRG" | "BGGR").then_some(pattern)
}

/// Identidad de sesión desde DATE-OBS con corte a MEDIODÍA: una toma de las
/// 02:00 pertenece a la noche anterior. Debe coincidir con la definición de
/// `deepsky::ds_session_night_id`, que agrupa los másters de flat por noche —
/// con dos definiciones distintas (fecha calendario aquí, noche allí), los
/// lights posteriores a medianoche perdían sus flats de la misma noche en
/// política Strict (auditoría 2026-07-20). Sin hora, se conserva la fecha.
fn date_session(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let mut parts = trimmed.splitn(2, ['T', ' ']);
    let date = parts.next()?.trim();
    if date.len() < 8 {
        return None;
    }
    let hour = parts
        .next()
        .and_then(|time| time.trim().split(':').next())
        .and_then(|h| h.parse::<u8>().ok());
    if hour.is_some_and(|h| h < 12) {
        if let Ok(parsed) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
            return Some(
                (parsed - chrono::Duration::days(1))
                    .format("%Y-%m-%d")
                    .to_string(),
            );
        }
    }
    Some(date.to_string())
}

fn optical_train(headers: &NormalizedHeaderMap) -> Option<String> {
    let telescope = first_string(headers, &["TELESCOP", "TELESCOPE", "LENS"]);
    let focal_length = first_number(headers, &["FOCALLEN", "FOCLEN", "FOCAL_LENGTH"]);
    // APTDIA is the clear-aperture diameter in millimetres, not an f/ratio.
    // Treating it as a ratio made two different optical trains appear equal
    // (and generated absurd values such as f/106 for an FSQ-106).  When both
    // focal length and aperture are present we may derive the ratio, but keep
    // the measured aperture in the identity as well.
    let aperture_mm = first_number(headers, &["APTDIA"]);
    let focal_ratio = first_number(headers, &["FOCRATIO", "F_RATIO"]).or_else(|| {
        let focal_length = focal_length?;
        let aperture = aperture_mm?;
        (focal_length.is_finite() && aperture.is_finite() && aperture > 0.0)
            .then_some(focal_length / aperture)
    });
    if telescope.is_none()
        && focal_length.is_none()
        && aperture_mm.is_none()
        && focal_ratio.is_none()
    {
        return None;
    }
    let mut parts = Vec::new();
    if let Some(value) = telescope {
        parts.push(value);
    }
    if let Some(value) = focal_length {
        parts.push(format!("f={value:.3}mm"));
    }
    if let Some(value) = aperture_mm {
        parts.push(format!("aperture={value:.3}mm"));
    }
    if let Some(value) = focal_ratio {
        parts.push(format!("ratio={value:.4}"));
    }
    Some(parts.join("|"))
}

pub(crate) fn signature_from_headers(
    headers: &NormalizedHeaderMap,
    context: SignatureContext,
) -> SignatureExtraction {
    let mut warnings = Vec::new();
    let camera = first_string(headers, &["INSTRUME", "CAMERA", "CAMMODEL", "CAMERAMOD"]);
    let sensor = first_string(headers, &["SENSOR", "DETECTOR", "SENSORMOD"]);
    let read_mode = first_string(headers, &["READMODE", "READOUTM", "READOUT", "CAMMODE"]);
    // EGAIN is normally the detector conversion gain in e-/ADU.  It is not
    // the user-selected camera gain and therefore cannot be used to decide
    // whether two calibration frames share a capture setting.
    let gain = first_number(headers, &["GAIN", "CAM-GAIN"]).map(|v| v as f32);
    if gain.is_none() && headers.contains_key("EGAIN") {
        warnings.push(
            "EGAIN describe e-/ADU y no sustituye el gain de captura; falta GAIN/CAM-GAIN".into(),
        );
    }
    let iso = first_u32(headers, &["ISOSPEED", "ISO", "ISO_SPEED"]);
    let offset =
        first_number(headers, &["OFFSET", "BLKLEVEL", "BLACKLEV", "CAM-OFFS"]).map(|v| v as f32);
    let temperature_c = first_number(headers, &["CCD-TEMP", "CCDTEMP", "SENSOR_TEMP", "SET-TEMP"])
        .map(|v| v as f32);
    let exposure_seconds = first_number(headers, &["EXPTIME", "EXPOSURE", "EXP_TIME"]);

    let pair = first(headers, &["BINNING", "CCDBIN"]).and_then(parse_binning_pair);
    let binning_x = first_u32(headers, &["XBINNING", "BINX", "XBIN"]).or(pair.map(|v| v.0));
    let binning_y = first_u32(headers, &["YBINNING", "BINY", "YBIN"]).or(pair.map(|v| v.1));

    let roi_x = first_u32(headers, &["XORGSUBF", "XORIGIN", "ROI_X", "STARTX"]);
    let roi_y = first_u32(headers, &["YORGSUBF", "YORIGIN", "ROI_Y", "STARTY"]);
    let roi = match (roi_x, roi_y) {
        (Some(x), Some(y)) if context.width > 0 && context.height > 0 => {
            Some([x, y, context.width, context.height])
        }
        (None, None) => None,
        _ => {
            warnings.push("ROI incompleto: faltan X o Y de origen".into());
            None
        }
    };

    let cfa_pattern =
        first(headers, &["BAYERPAT", "BAYERPATN", "CFAPAT", "CFA"]).and_then(normalize_cfa_pattern);
    let cfa_header_present = first(headers, &["BAYERPAT", "BAYERPATN", "CFAPAT", "CFA"]).is_some();
    if cfa_header_present && cfa_pattern.is_none() {
        warnings.push("patrón CFA no reconocido; sólo RGGB/GRBG/GBRG/BGGR son válidos".into());
    }
    let phase_header_x = first_u32(headers, &["XBAYROFF", "BAYOFFX", "CFAOFFX"]);
    let phase_header_y = first_u32(headers, &["YBAYROFF", "BAYOFFY", "CFAOFFY"]);
    let cfa_phase = cfa_pattern.as_ref().and_then(|_| {
        match (phase_header_x, phase_header_y, roi_x, roi_y) {
            // Explicit CFA offsets are sufficient even when the capture
            // omitted full-sensor ROI origins.  If both are present, ROI
            // parity composes with the explicit detector phase.
            (Some(px), Some(py), Some(x), Some(y)) => {
                Some([((px + x) & 1) as u8, ((py + y) & 1) as u8])
            }
            (Some(px), Some(py), None, None) => Some([(px & 1) as u8, (py & 1) as u8]),
            (None, None, Some(x), Some(y)) => Some([(x & 1) as u8, (y & 1) as u8]),
            _ => {
                warnings.push(
                    "CFA sin fase X/Y completa ni origen ROI completo; no se puede demostrar la fase"
                        .into(),
                );
                None
            }
        }
    });

    let layout = match (&cfa_pattern, cfa_phase, context.channels) {
        (Some(pattern), Some(phase), 1) => Some(StoreLayout::Cfa {
            pattern: pattern.clone(),
            phase_x: phase[0],
            phase_y: phase[1],
        }),
        (Some(_), None, 1) => None,
        (None, _, 1) => Some(StoreLayout::Mono),
        (None, _, 3) => Some(StoreLayout::Rgb),
        (Some(_), _, channels) => {
            warnings.push(format!(
                "cabecera CFA incompatible con {channels} canales almacenados"
            ));
            None
        }
        (_, _, channels) => {
            warnings.push(format!("número de canales no soportado: {channels}"));
            None
        }
    };

    let date_obs = first(headers, &["DATE-OBS", "DATE-LOC", "DATE"]);
    let session = context
        .session_hint
        .or_else(|| date_obs.and_then(date_session));
    let bit_depth = first_number(headers, &["BITDEPTH", "ADC_BITS"])
        .and_then(|value| (value.is_finite() && value > 0.0).then_some(value as u8));
    let signature = CalibrationSignature {
        camera,
        sensor,
        read_mode,
        gain,
        iso,
        offset,
        temperature_c,
        exposure_seconds,
        binning_x,
        binning_y,
        roi,
        cfa_pattern,
        cfa_phase,
        filter: first_string(headers, &["FILTER", "FILTERID", "FILTNAME"]),
        session,
        optical_train: optical_train(headers),
        adc_bits: bit_depth,
        white_level_adu: first_number(
            headers,
            &["SATURATE", "SATLEVEL", "WHITELEV", "WHITE_LEVEL"],
        )
        .map(|value| value as f32),
        ..CalibrationSignature::default()
    };
    SignatureExtraction {
        signature,
        layout,
        warnings,
    }
}

/// Campos FÍSICOS cuya ausencia impide verificar el emparejamiento de
/// calibración. Su falta degrada la elegibilidad científica (con divulgación)
/// pero NO bloquea el apilado. La identidad extendida (sensor, readMode, roi,
/// adcBits, whiteLevelAdu, opticalTrain) vive en
/// `missing_extended_signature_fields`: su ausencia es lo habitual en FITS de
/// captura y sólo se anota.
pub(crate) fn missing_required_signature_fields(
    signature: &CalibrationSignature,
    role: CalibrationRole,
) -> Vec<&'static str> {
    let mut missing = Vec::new();
    let mut required = |present: bool, label| {
        if !present {
            missing.push(label);
        }
    };
    required(
        signature.gain.is_some() || signature.iso.is_some(),
        "gainOrIso",
    );
    required(signature.offset.is_some(), "offset");
    required(signature.binning_x.is_some(), "binningX");
    required(signature.binning_y.is_some(), "binningY");
    if signature.cfa_pattern.is_some() {
        required(signature.cfa_phase.is_some(), "cfaPhase");
    }
    match role {
        CalibrationRole::Bias => {}
        CalibrationRole::Dark | CalibrationRole::DarkFlat => {
            required(signature.temperature_c.is_some(), "temperatureC");
            required(signature.exposure_seconds.is_some(), "exposureSeconds");
        }
        CalibrationRole::Flat => {
            required(signature.filter.is_some(), "filter");
        }
    }
    missing
}

/// Identidad extendida ausente: no bloquea ni degrada; se agrupa en un único
/// aviso informativo y queda registrada en receta/decisiones.
pub(crate) fn missing_extended_signature_fields(
    signature: &CalibrationSignature,
) -> Vec<&'static str> {
    let mut missing = Vec::new();
    let mut note = |present: bool, label| {
        if !present {
            missing.push(label);
        }
    };
    note(signature.camera.is_some(), "camera");
    note(signature.sensor.is_some(), "sensor");
    note(signature.read_mode.is_some(), "readMode");
    note(signature.roi.is_some(), "roi");
    note(signature.adc_bits.is_some(), "adcBits");
    note(signature.white_level_adu.is_some(), "whiteLevelAdu");
    note(signature.optical_train.is_some(), "opticalTrain");
    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_headers() -> NormalizedHeaderMap {
        normalize_header_map([
            ("INSTRUME", "ASI2600MC"),
            ("SENSOR", "IMX571"),
            ("READMODE", "HighGain"),
            ("GAIN", "100"),
            ("OFFSET", "50"),
            ("CCD-TEMP", "-10.0"),
            ("EXPTIME", "300.000"),
            ("XBINNING", "1"),
            ("YBINNING", "1"),
            ("XORGSUBF", "0"),
            ("YORGSUBF", "0"),
            ("FILTER", "L"),
            ("DATE-OBS", "2026-07-19T03:04:05"),
            ("TELESCOP", "RC8"),
            ("FOCALLEN", "1625.0"),
            ("FOCRATIO", "8.0"),
            ("BITDEPTH", "16"),
            ("SATURATE", "65535"),
        ])
    }

    #[test]
    fn extracts_complete_mono_signature_and_aliases() {
        let headers = base_headers();
        let out = signature_from_headers(
            &headers,
            SignatureContext {
                width: 6248,
                height: 4176,
                channels: 1,
                session_hint: None,
            },
        );
        assert_eq!(out.signature.camera.as_deref(), Some("ASI2600MC"));
        assert_eq!(out.signature.roi, Some([0, 0, 6248, 4176]));
        // 03:04 con corte a mediodía = noche del 18 (coincide con
        // ds_session_night_id, que agrupa los flats por noche).
        assert_eq!(out.signature.session.as_deref(), Some("2026-07-18"));
        assert!(matches!(out.layout, Some(StoreLayout::Mono)));
        assert!(
            missing_required_signature_fields(&out.signature, CalibrationRole::Dark).is_empty()
        );
    }

    #[test]
    fn session_uses_noon_cutover_so_midnight_crossing_keeps_one_night() {
        // Lights de 23:50 y 00:20 de la misma noche de observación DEBEN
        // compartir sesión; con la fecha calendario cruda el light posterior
        // a medianoche perdía los flats de su noche en política Strict.
        assert_eq!(
            date_session("2026-07-19T23:50:00").as_deref(),
            Some("2026-07-19")
        );
        assert_eq!(
            date_session("2026-07-20T00:20:00").as_deref(),
            Some("2026-07-19")
        );
        assert_eq!(
            date_session("2026-07-20T12:00:00").as_deref(),
            Some("2026-07-20")
        );
        // Sin hora no se puede inferir la noche: se conserva la fecha.
        assert_eq!(date_session("2026-07-20").as_deref(), Some("2026-07-20"));
    }

    #[test]
    fn recognizes_all_four_bayer_patterns_and_odd_roi_phase() {
        for pattern in ["RGGB", "GRBG", "GBRG", "BGGR"] {
            let mut headers = base_headers();
            headers.insert("BAYERPAT".into(), pattern.into());
            headers.insert("XORGSUBF".into(), "1".into());
            headers.insert("YORGSUBF".into(), "3".into());
            let out = signature_from_headers(
                &headers,
                SignatureContext {
                    width: 3000,
                    height: 2000,
                    channels: 1,
                    session_hint: None,
                },
            );
            assert_eq!(out.signature.cfa_pattern.as_deref(), Some(pattern));
            assert_eq!(out.signature.cfa_phase, Some([1, 1]));
            assert!(matches!(
                out.layout,
                Some(StoreLayout::Cfa {
                    phase_x: 1,
                    phase_y: 1,
                    ..
                })
            ));
        }
    }

    #[test]
    fn dslr_iso_is_preserved_when_gain_is_absent() {
        let mut headers = base_headers();
        headers.remove("GAIN");
        headers.insert("ISOSPEED".into(), "800".into());
        let out = signature_from_headers(
            &headers,
            SignatureContext {
                width: 6000,
                height: 4000,
                channels: 3,
                session_hint: Some("night-1".into()),
            },
        );
        assert_eq!(out.signature.gain, None);
        assert_eq!(out.signature.iso, Some(800));
        assert_eq!(out.signature.session.as_deref(), Some("night-1"));
        assert!(matches!(out.layout, Some(StoreLayout::Rgb)));
    }

    #[test]
    fn missing_metadata_is_enumerated_not_fabricated() {
        let headers = normalize_header_map([("EXPTIME", "120")]);
        let out = signature_from_headers(
            &headers,
            SignatureContext {
                width: 100,
                height: 100,
                channels: 1,
                session_hint: None,
            },
        );
        let missing = missing_required_signature_fields(&out.signature, CalibrationRole::Dark);
        // Crítico (física): sin fabricar nada, se enumera lo que falta.
        assert!(missing.contains(&"offset"));
        assert!(missing.contains(&"temperatureC"));
        assert!(missing.contains(&"gainOrIso"));
        // Identidad extendida: ausente es lo habitual; se anota aparte y no
        // bloquea ni degrada por sí sola.
        assert!(!missing.contains(&"camera"));
        assert!(!missing.contains(&"roi"));
        let extended = missing_extended_signature_fields(&out.signature);
        assert!(extended.contains(&"camera"));
        assert!(extended.contains(&"sensor"));
        assert!(extended.contains(&"roi"));
    }

    #[test]
    fn normalizes_black_level_binning_and_white_level_aliases() {
        let headers = normalize_header_map([
            ("CAMERA", "'QHY600M' / capture device"),
            ("DETECTOR", "IMX455"),
            ("READOUTM", "Mode 1"),
            ("EGAIN", "0.8"),
            ("BLKLEVEL", "30"),
            ("SENSOR_TEMP", "-5"),
            ("EXPOSURE", "60"),
            ("BINNING", "2x2"),
            ("STARTX", "20"),
            ("STARTY", "10"),
            ("FILTERID", "Ha"),
            ("LENS", "FSQ106"),
            ("ADC_BITS", "16"),
            ("WHITELEV", "60000"),
        ]);
        let out = signature_from_headers(
            &headers,
            SignatureContext {
                width: 2000,
                height: 1500,
                channels: 1,
                session_hint: Some("s1".into()),
            },
        );
        assert_eq!(out.signature.binning_x, Some(2));
        assert_eq!(out.signature.binning_y, Some(2));
        assert_eq!(out.signature.offset, Some(30.0));
        assert_eq!(out.signature.roi, Some([20, 10, 2000, 1500]));
        assert_eq!(out.signature.white_level_adu, Some(60000.0));
        assert_eq!(out.signature.gain, None);
        assert!(out.warnings.iter().any(|warning| warning.contains("EGAIN")));
        assert!(
            missing_required_signature_fields(&out.signature, CalibrationRole::Dark)
                .contains(&"gainOrIso")
        );
    }

    #[test]
    fn explicit_cfa_offsets_prove_phase_without_roi_origin() {
        let mut headers = base_headers();
        headers.remove("XORGSUBF");
        headers.remove("YORGSUBF");
        headers.insert("BAYERPAT".into(), "RGGB".into());
        headers.insert("XBAYROFF".into(), "3".into());
        headers.insert("YBAYROFF".into(), "2".into());
        let out = signature_from_headers(
            &headers,
            SignatureContext {
                width: 3000,
                height: 2000,
                channels: 1,
                session_hint: None,
            },
        );
        assert_eq!(out.signature.cfa_phase, Some([1, 0]));
        assert!(matches!(
            out.layout,
            Some(StoreLayout::Cfa {
                phase_x: 1,
                phase_y: 0,
                ..
            })
        ));
    }

    #[test]
    fn aperture_diameter_is_not_mislabelled_as_focal_ratio() {
        let headers = normalize_header_map([
            ("TELESCOP", "FSQ106"),
            ("FOCALLEN", "530"),
            ("APTDIA", "106"),
        ]);
        let identity = optical_train(&headers).unwrap();
        assert!(identity.contains("aperture=106.000mm"), "{identity}");
        assert!(identity.contains("ratio=5.0000"), "{identity}");
        assert!(!identity.contains("ratio=106"), "{identity}");
    }

    #[test]
    fn unwraps_fitrs_debug_header_variants() {
        let headers = normalize_header_map([
            ("INSTRUME", "CharacterString(\"ASI533MM\")"),
            ("GAIN", "Float(100.0)"),
            ("OFFSET", "Integer(20)"),
            ("EXPTIME", "Some(Float(60.0))"),
        ]);
        assert_eq!(headers["INSTRUME"], "ASI533MM");
        assert_eq!(first_number(&headers, &["GAIN"]), Some(100.0));
        assert_eq!(first_number(&headers, &["OFFSET"]), Some(20.0));
        assert_eq!(first_number(&headers, &["EXPTIME"]), Some(60.0));
    }
}
