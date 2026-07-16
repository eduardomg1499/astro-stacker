use fitrs::{Fits, Hdu, HeaderValue};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug)]
pub struct FitsSequenceReader {
    pub folder_path: String,
    pub files: Vec<PathBuf>,
    pub width: usize,
    pub height: usize,
    pub bytes_per_pixel: usize,
    pub sample_bits: usize,
    pub frame_count: usize,
    pub color_id: i32,
    pub is_color: bool,
    pub fps: f64,
}

impl FitsSequenceReader {
    pub fn new(path: &str) -> Result<Self, String> {
        let root = Path::new(path);
        if !root.exists() {
            return Err(format!("La ruta FITS no existe: {}", root.display()));
        }

        // Elegir un FITS desde el selector de archivos significa abrir ESE
        // frame, no todos los FITS que por casualidad estén en su carpeta.
        // Una carpeta explícita sí representa una secuencia.
        let (folder, mut files) = if root.is_file() {
            if !is_fits_path(root) {
                return Err(format!("El archivo no es FIT/FITS: {}", root.display()));
            }
            (
                root.parent().unwrap_or_else(|| Path::new(".")),
                vec![root.to_path_buf()],
            )
        } else if root.is_dir() {
            let mut files = Vec::new();
            let entries = fs::read_dir(root)
                .map_err(|e| format!("No se pudo leer la carpeta FITS {}: {e}", root.display()))?;
            for entry in entries {
                let entry = entry.map_err(|e| {
                    format!(
                        "Entrada ilegible en la carpeta FITS {}: {e}",
                        root.display()
                    )
                })?;
                let candidate = entry.path();
                if candidate.is_file() && is_fits_path(&candidate) {
                    files.push(candidate);
                }
            }
            (root, files)
        } else {
            return Err(format!(
                "La ruta no es un archivo ni una carpeta FITS: {}",
                root.display()
            ));
        };

        if files.is_empty() {
            return Err(format!(
                "No se encontraron archivos FIT/FITS en {}",
                folder.display()
            ));
        }

        // Orden natural estable y determinista: frame2 precede a frame10.
        files.sort_by(|a, b| natural_path_cmp(a, b));

        let first_meta = Self::read_fits_metadata(&files[0])?;
        for file in files.iter().skip(1) {
            let meta = Self::read_fits_metadata(file)?;
            if meta != first_meta {
                return Err(format!(
                    "Secuencia FITS heterogénea: {} tiene {:?}, pero el primer frame {} tiene {:?}",
                    file.display(),
                    meta,
                    files[0].display(),
                    first_meta
                ));
            }
        }

        // FITS float no define si los valores representan una fracción [0,1]
        // o ADU/unidades físicas. Resolverlo una sola vez para TODA la
        // secuencia evita que un cosmic ray cambie la exposición de un frame.
        if first_meta.stored_bitpix < 0 {
            initialize_float_sequence_policy(&files, first_meta.stored_bitpix)?;
        }

        let frame_count = files.len();
        Ok(Self {
            folder_path: folder.to_string_lossy().into_owned(),
            files,
            width: first_meta.width,
            height: first_meta.height,
            bytes_per_pixel: first_meta.bytes_per_pixel,
            sample_bits: first_meta.sample_bits,
            frame_count,
            color_id: first_meta.color_id,
            is_color: first_meta.is_color,
            fps: 30.0,
        })
    }

    fn read_fits_metadata(path: &Path) -> Result<FitsMeta, String> {
        let fits = Fits::open(path)
            .map_err(|e| format!("Error abriendo FITS {}: {e:?}", path.display()))?;
        let hdu = fits
            .get(0)
            .ok_or_else(|| format!("FITS sin HDU primario: {}", path.display()))?;
        let meta = Self::metadata_from_hdu(path, &hdu)?;
        // Un header válido no garantiza que el payload haya terminado de
        // escribirse. Esta cota inferior barata detecta capturas truncadas al
        // abrir la secuencia, sin decodificar todos los frames en RAM.
        let raw_bytes_per_sample = meta.stored_bitpix.unsigned_abs() as usize / 8;
        let raw_bytes = meta
            .width
            .checked_mul(meta.height)
            .and_then(|n| n.checked_mul(meta.planes))
            .and_then(|n| n.checked_mul(raw_bytes_per_sample))
            .ok_or_else(|| format!("Payload FITS desborda tamaño en {}", path.display()))?;
        let min_file_bytes = 2_880usize
            .checked_add(raw_bytes)
            .ok_or_else(|| format!("Archivo FITS desborda tamaño en {}", path.display()))?;
        let file_bytes = fs::metadata(path)
            .map_err(|e| format!("No se pudo medir {}: {e}", path.display()))?
            .len();
        if file_bytes < min_file_bytes as u64 {
            return Err(format!(
                "Payload FITS truncado en {}: {} bytes, mínimo {} para {:?}",
                path.display(),
                file_bytes,
                min_file_bytes,
                meta
            ));
        }
        Ok(meta)
    }

    fn metadata_from_hdu(path: &Path, hdu: &Hdu) -> Result<FitsMeta, String> {
        let naxis = required_header_i64(hdu, "NAXIS", path)?;
        let width_i = required_header_i64(hdu, "NAXIS1", path)?;
        let height_i = required_header_i64(hdu, "NAXIS2", path)?;
        if width_i <= 0 || height_i <= 0 {
            return Err(format!(
                "Geometría FITS inválida en {}: {}x{}",
                path.display(),
                width_i,
                height_i
            ));
        }
        let width = usize::try_from(width_i)
            .map_err(|_| format!("NAXIS1 fuera de rango en {}", path.display()))?;
        let height = usize::try_from(height_i)
            .map_err(|_| format!("NAXIS2 fuera de rango en {}", path.display()))?;

        let planes = match naxis {
            2 => 1usize,
            3 => match required_header_i64(hdu, "NAXIS3", path)? {
                1 => 1,
                3 => 3,
                other => {
                    return Err(format!(
                        "NAXIS3={} no soportado en {}: sólo se admiten mono/CFA (1 plano) o RGB (3 planos)",
                        other,
                        path.display()
                    ))
                }
            },
            other => {
                return Err(format!(
                    "NAXIS={} no soportado en {}: se requiere una imagen 2D o 3 planos RGB",
                    other,
                    path.display()
                ))
            }
        };

        let stored_bitpix = required_header_i64(hdu, "BITPIX", path)?;
        let bytes_per_sample = match stored_bitpix {
            8 => 1usize,
            16 | 32 | -32 | -64 => 2usize, // dominio de trabajo u16 LE
            other => {
                return Err(format!(
                    "BITPIX={} no soportado en {} (admitidos: 8, 16, 32, -32, -64)",
                    other,
                    path.display()
                ))
            }
        };

        width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(planes))
            .and_then(|n| n.checked_mul(bytes_per_sample))
            .ok_or_else(|| format!("Geometría FITS desborda memoria en {}", path.display()))?;

        let bayer = match header_string(hdu, "BAYERPAT", path)? {
            Some(pattern) => Some(pattern),
            None => header_string(hdu, "BAYERPATN", path)?,
        };
        let bayer_id = if let Some(pattern) = bayer {
            if planes != 1 {
                return Err(format!(
                    "{} declara BAYERPAT pero también contiene {} planos; CFA debe ser un solo plano",
                    path.display(),
                    planes
                ));
            }
            let mut normalized = pattern.to_ascii_uppercase();
            normalized.retain(|c| c.is_ascii_alphabetic());
            let base = match normalized.as_str() {
                "RGGB" => 8,
                "GRBG" => 9,
                "GBRG" => 10,
                "BGGR" => 11,
                _ => {
                    return Err(format!(
                        "BAYERPAT='{}' no soportado en {} (RGGB/GRBG/GBRG/BGGR)",
                        pattern,
                        path.display()
                    ))
                }
            };
            let x_offset = first_header_offset(hdu, &["XBAYROFF", "BAYOFFX", "BAYROFFX"], path)?;
            let y_offset = first_header_offset(hdu, &["YBAYROFF", "BAYOFFY", "BAYROFFY"], path)?;
            Some(shift_bayer_id(base, x_offset, y_offset))
        } else {
            None
        };

        let color_id = if planes == 3 {
            100
        } else {
            bayer_id.unwrap_or(0)
        };
        Ok(FitsMeta {
            width,
            height,
            planes,
            bytes_per_pixel: bytes_per_sample * planes,
            sample_bits: if stored_bitpix == 8 { 8 } else { 16 },
            stored_bitpix,
            color_id,
            is_color: color_id != 0,
        })
    }

    pub fn get_frame(&self, index: usize) -> Vec<u8> {
        let Some(file_path) = self.files.get(index) else {
            return Vec::new();
        };
        match self.read_fits_data(file_path) {
            Ok(data) => data,
            Err(e) => {
                eprintln!("FITS decode error: {e}");
                Vec::new()
            }
        }
    }

    fn read_fits_data(&self, path: &Path) -> Result<Vec<u8>, String> {
        let fits = Fits::open(path)
            .map_err(|e| format!("Error abriendo FITS {}: {e:?}", path.display()))?;
        let hdu = fits
            .get(0)
            .ok_or_else(|| format!("FITS sin HDU primario: {}", path.display()))?;
        let expected = Self::metadata_from_hdu(path, &hdu)?;
        if expected.width != self.width
            || expected.height != self.height
            || expected.bytes_per_pixel != self.bytes_per_pixel
            || expected.sample_bits != self.sample_bits
            || expected.color_id != self.color_id
            || expected.is_color != self.is_color
        {
            return Err(format!(
                "El frame FITS cambió después de abrir la secuencia: {} ahora es {:?}; el lector espera {}x{}, {} B/px, {} bits, ColorID {}",
                path.display(),
                expected,
                self.width,
                self.height,
                self.bytes_per_pixel,
                self.sample_bits,
                self.color_id
            ));
        }

        let expected_samples = expected
            .width
            .checked_mul(expected.height)
            .and_then(|n| n.checked_mul(expected.planes))
            .ok_or_else(|| format!("Overflow de muestras FITS en {}", path.display()))?;
        let expected_size = expected_samples
            .checked_mul(expected.bytes_per_pixel / expected.planes)
            .ok_or_else(|| format!("Overflow de buffer FITS en {}", path.display()))?;

        let (bzero, bscale) = fits_physical_transform(&hdu, path)?;
        let phys = |raw: f64| raw * bscale + bzero;
        let float_gain = if expected.stored_bitpix < 0 {
            float_sequence_policy(&self.files, expected.stored_bitpix)?.gain()
        } else {
            1.0
        };
        // Escribir directamente en layout canónico evita un segundo buffer
        // completo al convertir RGB planar → interleaved. En Luna/Sol de alta
        // resolución esto ahorra 6 bytes por píxel de pico de memoria.
        let mut out = vec![0u8; expected_size];
        let write_u16 = |dst: &mut [u8], value: f64| {
            let finite = if value.is_finite() { value } else { 0.0 };
            dst.copy_from_slice(&(finite.round().clamp(0.0, 65_535.0) as u16).to_le_bytes());
        };

        // fitrs 0.5 hace panic ante un payload truncado. Convertimos ese
        // comportamiento de una dependencia en un error recuperable.
        let data = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hdu.read_data()))
            .map_err(|_| format!("Payload FITS truncado o corrupto en {}", path.display()))?;

        match (expected.stored_bitpix, data) {
            (8, fitrs::FitsData::Characters(arr)) => {
                ensure_sample_count(path, expected_samples, arr.data.len())?;
                fill_decoded_samples(&mut out, &arr.data, &expected, |raw, dst| {
                    let value = phys(*raw as u8 as f64);
                    let finite = if value.is_finite() { value } else { 0.0 };
                    dst[0] = finite.round().clamp(0.0, 255.0) as u8;
                });
            }
            (16 | 32, fitrs::FitsData::IntegersI32(arr)) => {
                ensure_sample_count(path, expected_samples, arr.data.len())?;
                fill_decoded_samples(&mut out, &arr.data, &expected, |raw, dst| {
                    write_u16(dst, phys(raw.unwrap_or(0) as f64));
                });
            }
            (16 | 32, fitrs::FitsData::IntegersU32(arr)) => {
                ensure_sample_count(path, expected_samples, arr.data.len())?;
                fill_decoded_samples(&mut out, &arr.data, &expected, |raw, dst| {
                    write_u16(dst, phys(raw.unwrap_or(0) as f64));
                });
            }
            (-32, fitrs::FitsData::FloatingPoint32(arr)) => {
                ensure_sample_count(path, expected_samples, arr.data.len())?;
                fill_decoded_samples(&mut out, &arr.data, &expected, |raw, dst| {
                    write_u16(dst, phys(*raw as f64) * float_gain);
                });
            }
            (-64, fitrs::FitsData::FloatingPoint64(arr)) => {
                ensure_sample_count(path, expected_samples, arr.data.len())?;
                fill_decoded_samples(&mut out, &arr.data, &expected, |raw, dst| {
                    write_u16(dst, phys(*raw) * float_gain);
                });
            }
            (bitpix, _) => {
                return Err(format!(
                    "Tipo de payload incompatible con BITPIX={} en {}",
                    bitpix,
                    path.display()
                ))
            }
        }

        Ok(out)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FitsMeta {
    width: usize,
    height: usize,
    planes: usize,
    bytes_per_pixel: usize,
    sample_bits: usize,
    stored_bitpix: i64,
    color_id: i32,
    is_color: bool,
}

fn fill_decoded_samples<T, F>(out: &mut [u8], samples: &[T], meta: &FitsMeta, mut encode: F)
where
    F: FnMut(&T, &mut [u8]),
{
    let bytes_per_sample = meta.bytes_per_pixel / meta.planes;
    let pixels = meta.width * meta.height;
    if meta.planes == 1 {
        for (sample, dst) in samples.iter().zip(out.chunks_exact_mut(bytes_per_sample)) {
            encode(sample, dst);
        }
        return;
    }

    debug_assert_eq!(meta.planes, 3);
    // FITS entrega [plano R][plano G][plano B]; el contrato planetario es
    // RGBRGB... Escribir cada plano con stride evita una copia posterior.
    for channel in 0..3 {
        let source = &samples[channel * pixels..(channel + 1) * pixels];
        for (pixel, sample) in source.iter().enumerate() {
            let dst = (pixel * 3 + channel) * bytes_per_sample;
            encode(sample, &mut out[dst..dst + bytes_per_sample]);
        }
    }
}

fn is_fits_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("fit" | "fits")
    )
}

fn natural_path_cmp(a: &Path, b: &Path) -> Ordering {
    let a_name = a.file_name().unwrap_or_default().to_string_lossy();
    let b_name = b.file_name().unwrap_or_default().to_string_lossy();
    natural_cmp(&a_name, &b_name)
}

fn natural_cmp(a: &str, b: &str) -> Ordering {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let (mut ai, mut bi) = (0usize, 0usize);
    while ai < a_bytes.len() && bi < b_bytes.len() {
        if a_bytes[ai].is_ascii_digit() && b_bytes[bi].is_ascii_digit() {
            let (a_start, b_start) = (ai, bi);
            while ai < a_bytes.len() && a_bytes[ai].is_ascii_digit() {
                ai += 1;
            }
            while bi < b_bytes.len() && b_bytes[bi].is_ascii_digit() {
                bi += 1;
            }
            let mut a_sig = a_start;
            let mut b_sig = b_start;
            while a_sig < ai && a_bytes[a_sig] == b'0' {
                a_sig += 1;
            }
            while b_sig < bi && b_bytes[b_sig] == b'0' {
                b_sig += 1;
            }
            let a_len = ai - a_sig;
            let b_len = bi - b_sig;
            match a_len.cmp(&b_len) {
                Ordering::Equal => {}
                other => return other,
            }
            match a_bytes[a_sig..ai].cmp(&b_bytes[b_sig..bi]) {
                Ordering::Equal => {}
                other => return other,
            }
            // Números equivalentes: el menos rellenado con ceros va primero.
            match (ai - a_start).cmp(&(bi - b_start)) {
                Ordering::Equal => {}
                other => return other,
            }
        } else {
            let ac = a_bytes[ai].to_ascii_lowercase();
            let bc = b_bytes[bi].to_ascii_lowercase();
            match ac.cmp(&bc) {
                Ordering::Equal => {
                    ai += 1;
                    bi += 1;
                }
                other => return other,
            }
        }
    }
    a_bytes
        .len()
        .cmp(&b_bytes.len())
        .then_with(|| a_bytes.cmp(b_bytes))
}

fn required_header_i64(hdu: &Hdu, key: &str, path: &Path) -> Result<i64, String> {
    header_i64(hdu, key)?.ok_or_else(|| {
        format!(
            "Keyword FITS obligatorio {} ausente en {}",
            key,
            path.display()
        )
    })
}

fn header_i64(hdu: &Hdu, key: &str) -> Result<Option<i64>, String> {
    let Some(value) = hdu.value(key) else {
        return Ok(None);
    };
    let parsed = match value {
        HeaderValue::IntegerNumber(v) => Some(*v as i64),
        HeaderValue::RealFloatingNumber(v)
            if v.is_finite()
                && v.fract() == 0.0
                && *v >= i64::MIN as f64
                && *v <= i64::MAX as f64 =>
        {
            Some(*v as i64)
        }
        HeaderValue::CharacterString(v) => v.trim().parse::<i64>().ok(),
        _ => None,
    };
    parsed
        .map(Some)
        .ok_or_else(|| format!("Keyword FITS {} no es un entero válido: {:?}", key, value))
}

fn header_f64(hdu: &Hdu, key: &str) -> Option<f64> {
    match hdu.value(key) {
        Some(HeaderValue::RealFloatingNumber(v)) => Some(*v),
        Some(HeaderValue::IntegerNumber(v)) => Some(*v as f64),
        Some(HeaderValue::CharacterString(v)) => v.trim().parse().ok(),
        _ => None,
    }
}

fn fits_physical_transform(hdu: &Hdu, path: &Path) -> Result<(f64, f64), String> {
    let bzero = header_f64(hdu, "BZERO").unwrap_or(0.0);
    let bscale = header_f64(hdu, "BSCALE").unwrap_or(1.0);
    if !bzero.is_finite() || !bscale.is_finite() {
        return Err(format!("BZERO/BSCALE no finito en {}", path.display()));
    }
    Ok((bzero, bscale))
}

fn header_string(hdu: &Hdu, key: &str, path: &Path) -> Result<Option<String>, String> {
    match hdu.value(key) {
        Some(HeaderValue::CharacterString(v)) => Ok(Some(v.trim().to_string())),
        Some(other) => Err(format!(
            "Keyword FITS {} debe ser texto en {}: {:?}",
            key,
            path.display(),
            other
        )),
        None => Ok(None),
    }
}

fn first_header_offset(hdu: &Hdu, keys: &[&str], path: &Path) -> Result<i64, String> {
    for key in keys {
        if hdu.value(key).is_some() {
            return header_i64(hdu, key)?
                .ok_or_else(|| format!("Offset Bayer {} inválido en {}", key, path.display()));
        }
    }
    Ok(0)
}

fn shift_bayer_id(base: i32, x_offset: i64, y_offset: i64) -> i32 {
    let (red_x, red_y) = match base {
        8 => (0i64, 0i64),
        9 => (1, 0),
        10 => (0, 1),
        11 => (1, 1),
        _ => return base,
    };
    match (
        (red_x + x_offset).rem_euclid(2),
        (red_y + y_offset).rem_euclid(2),
    ) {
        (0, 0) => 8,
        (1, 0) => 9,
        (0, 1) => 10,
        (1, 1) => 11,
        _ => unreachable!(),
    }
}

fn ensure_sample_count(path: &Path, expected: usize, actual: usize) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "Número de muestras inválido en {}: {}, esperadas {}",
            path.display(),
            actual,
            expected
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FloatScalePolicy {
    NormalizedUnit,
    Physical,
}

impl FloatScalePolicy {
    fn gain(self) -> f64 {
        match self {
            Self::NormalizedUnit => 65_535.0,
            Self::Physical => 1.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FloatSequenceKey {
    first: PathBuf,
    last: PathBuf,
    frame_count: usize,
    stored_bitpix: i64,
}

impl FloatSequenceKey {
    fn new(files: &[PathBuf], stored_bitpix: i64) -> Option<Self> {
        Some(Self {
            first: files.first()?.clone(),
            last: files.last()?.clone(),
            frame_count: files.len(),
            stored_bitpix,
        })
    }
}

const FLOAT_POLICY_SAMPLE_FRAMES: usize = 5;
const FLOAT_POLICY_CACHE_ENTRIES: usize = 64;
static FLOAT_SEQUENCE_POLICIES: OnceLock<Mutex<HashMap<FloatSequenceKey, FloatScalePolicy>>> =
    OnceLock::new();

fn float_policy_cache() -> &'static Mutex<HashMap<FloatSequenceKey, FloatScalePolicy>> {
    FLOAT_SEQUENCE_POLICIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_float_sequence_policy(key: FloatSequenceKey, policy: FloatScalePolicy) {
    let mut cache = float_policy_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if cache.len() >= FLOAT_POLICY_CACHE_ENTRIES && !cache.contains_key(&key) {
        // El lector no puede añadir estado sin romper su contrato público. Este
        // registro pequeño conserva la política entre clones/frames y se puede
        // reconstruir de forma determinista si se supera el límite.
        cache.clear();
    }
    cache.insert(key, policy);
}

fn initialize_float_sequence_policy(
    files: &[PathBuf],
    stored_bitpix: i64,
) -> Result<FloatScalePolicy, String> {
    let key = FloatSequenceKey::new(files, stored_bitpix)
        .ok_or_else(|| "Secuencia FITS float vacía".to_string())?;
    let policy = infer_float_sequence_policy(files, stored_bitpix)?;
    cache_float_sequence_policy(key, policy);
    Ok(policy)
}

fn float_sequence_policy(
    files: &[PathBuf],
    stored_bitpix: i64,
) -> Result<FloatScalePolicy, String> {
    let key = FloatSequenceKey::new(files, stored_bitpix)
        .ok_or_else(|| "Secuencia FITS float vacía".to_string())?;
    if let Some(policy) = float_policy_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
        .copied()
    {
        return Ok(policy);
    }
    let policy = infer_float_sequence_policy(files, stored_bitpix)?;
    cache_float_sequence_policy(key, policy);
    Ok(policy)
}

fn representative_frame_indices(frame_count: usize) -> Vec<usize> {
    if frame_count <= FLOAT_POLICY_SAMPLE_FRAMES {
        return (0..frame_count).collect();
    }
    let last = frame_count - 1;
    (0..FLOAT_POLICY_SAMPLE_FRAMES)
        .map(|slot| slot * last / (FLOAT_POLICY_SAMPLE_FRAMES - 1))
        .collect()
}

fn float_unit_hint(hdu: &Hdu) -> Option<FloatScalePolicy> {
    let HeaderValue::CharacterString(unit) = hdu.value("BUNIT")? else {
        return None;
    };
    let unit = unit.trim().to_ascii_uppercase();
    if unit.contains("NORM") || unit.contains("FRACTION") || unit.contains("UNIT INTERVAL") {
        Some(FloatScalePolicy::NormalizedUnit)
    } else if unit.contains("ADU")
        || unit.contains("ELECTRON")
        || unit.contains("COUNT")
        || unit == "DN"
    {
        Some(FloatScalePolicy::Physical)
    } else {
        None
    }
}

fn classify_float_frame<I>(values: I) -> Option<FloatScalePolicy>
where
    I: IntoIterator<Item = f64>,
{
    let (mut finite, mut above_unit_range, mut inlier_count) = (0usize, 0usize, 0usize);
    let mut inlier_sum = 0.0f64;
    for value in values.into_iter().filter(|value| value.is_finite()) {
        finite += 1;
        let magnitude = value.abs();
        if magnitude > 2.0 {
            above_unit_range += 1;
        } else {
            inlier_sum += magnitude;
            inlier_count += 1;
        }
    }
    if finite == 0 {
        return None;
    }

    // Se tolera al menos una muestra extrema por frame y una adicional por
    // megapíxel. Es suficiente para cosmic rays/hot pixels aislados, pero no
    // oculta un disco o superficie realmente expresado en ADU.
    let outlier_allowance = (finite / 1_000_000).max(1);
    let inlier_mean = if inlier_count == 0 {
        f64::INFINITY
    } else {
        inlier_sum / inlier_count as f64
    };
    if above_unit_range <= outlier_allowance && inlier_mean <= 1.0 {
        Some(FloatScalePolicy::NormalizedUnit)
    } else {
        Some(FloatScalePolicy::Physical)
    }
}

fn median_finite(values: &mut Vec<f64>) -> Option<f64> {
    values.retain(|value| value.is_finite());
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    Some(if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) * 0.5
    } else {
        values[middle]
    })
}

fn infer_float_sequence_policy(
    files: &[PathBuf],
    stored_bitpix: i64,
) -> Result<FloatScalePolicy, String> {
    let indices = representative_frame_indices(files.len());
    if indices.is_empty() {
        return Err("Secuencia FITS float vacía".to_string());
    }

    let mut normalized_votes = 0usize;
    let mut physical_votes = 0usize;
    let mut normalized_unit_hints = 0usize;
    let mut physical_unit_hints = 0usize;
    let mut physical_extents = Vec::new();

    for index in indices.iter().copied() {
        let path = &files[index];
        let fits = Fits::open(path)
            .map_err(|e| format!("Error abriendo FITS {}: {e:?}", path.display()))?;
        let hdu = fits
            .get(0)
            .ok_or_else(|| format!("FITS sin HDU primario: {}", path.display()))?;
        let meta = FitsSequenceReader::metadata_from_hdu(path, &hdu)?;
        if meta.stored_bitpix != stored_bitpix {
            return Err(format!(
                "Secuencia FITS float cambió BITPIX en {}: {} frente a {}",
                path.display(),
                meta.stored_bitpix,
                stored_bitpix
            ));
        }
        let expected_samples = meta
            .width
            .checked_mul(meta.height)
            .and_then(|count| count.checked_mul(meta.planes))
            .ok_or_else(|| format!("Overflow de muestras FITS en {}", path.display()))?;
        let (bzero, bscale) = fits_physical_transform(&hdu, path)?;
        let phys = |raw: f64| raw * bscale + bzero;

        match float_unit_hint(&hdu) {
            Some(FloatScalePolicy::NormalizedUnit) => normalized_unit_hints += 1,
            Some(FloatScalePolicy::Physical) => physical_unit_hints += 1,
            None => {}
        }
        let datamin = header_f64(&hdu, "DATAMIN").filter(|value| value.is_finite());
        let datamax = header_f64(&hdu, "DATAMAX").filter(|value| value.is_finite());
        if datamin.is_some() || datamax.is_some() {
            physical_extents.push(
                datamin
                    .into_iter()
                    .chain(datamax)
                    .map(f64::abs)
                    .fold(0.0f64, f64::max),
            );
        }

        let data = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hdu.read_data()))
            .map_err(|_| format!("Payload FITS truncado o corrupto en {}", path.display()))?;
        let evidence = match (stored_bitpix, data) {
            (-32, fitrs::FitsData::FloatingPoint32(arr)) => {
                ensure_sample_count(path, expected_samples, arr.data.len())?;
                classify_float_frame(arr.data.iter().map(|&value| phys(value as f64)))
            }
            (-64, fitrs::FitsData::FloatingPoint64(arr)) => {
                ensure_sample_count(path, expected_samples, arr.data.len())?;
                classify_float_frame(arr.data.iter().copied().map(phys))
            }
            _ => {
                return Err(format!(
                    "Tipo de payload incompatible con BITPIX={} en {}",
                    stored_bitpix,
                    path.display()
                ))
            }
        };
        match evidence {
            Some(FloatScalePolicy::NormalizedUnit) => normalized_votes += 1,
            Some(FloatScalePolicy::Physical) => physical_votes += 1,
            None => {}
        }
    }

    let sampled_frames = indices.len();
    if normalized_unit_hints * 2 > sampled_frames {
        return Ok(FloatScalePolicy::NormalizedUnit);
    }
    if physical_unit_hints * 2 > sampled_frames {
        return Ok(FloatScalePolicy::Physical);
    }

    // DATAMIN/DATAMAX son metadatos físicos. Exigirlos en una mayoría y usar
    // la mediana impide que el máximo de un único frame imponga la política.
    let extent_frames = physical_extents.len();
    if extent_frames * 2 > sampled_frames {
        if let Some(extent) = median_finite(&mut physical_extents) {
            return Ok(if extent <= 1.5 {
                FloatScalePolicy::NormalizedUnit
            } else {
                FloatScalePolicy::Physical
            });
        }
    }

    if normalized_votes > physical_votes {
        Ok(FloatScalePolicy::NormalizedUnit)
    } else {
        // Ante evidencia vacía o empate se conserva la interpretación física:
        // amplificar ADU por 65535 sería una pérdida irreversible por clipping.
        Ok(FloatScalePolicy::Physical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "zas_fits_sequence_{}_{}_{}",
            tag,
            std::process::id(),
            NEXT_DIR.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_i32_fits(path: &Path, shape: &[usize], bayer: Option<(&str, i32, i32)>) {
        let samples: usize = shape.iter().product();
        let mut hdu = Hdu::new(shape, (0..samples as i32).collect::<Vec<_>>());
        if let Some((pattern, x, y)) = bayer {
            hdu.insert("BAYERPAT", pattern);
            hdu.insert("XBAYROFF", x);
            hdu.insert("YBAYROFF", y);
        }
        Fits::create(path, hdu).unwrap();
    }

    fn write_f32_fits(path: &Path, shape: &[usize]) {
        let samples: usize = shape.iter().product();
        write_f32_values(path, shape, vec![0.25f32; samples], None);
    }

    fn write_f32_values(
        path: &Path,
        shape: &[usize],
        values: Vec<f32>,
        physical_scale: Option<(f64, f64)>,
    ) {
        assert_eq!(values.len(), shape.iter().product::<usize>());
        let mut hdu = Hdu::new(shape, values);
        if let Some((bzero, bscale)) = physical_scale {
            hdu.insert("BZERO", bzero);
            hdu.insert("BSCALE", bscale);
        }
        Fits::create(path, hdu).unwrap();
    }

    fn decoded_u16(frame: &[u8]) -> Vec<u16> {
        frame
            .chunks_exact(2)
            .map(|sample| u16::from_le_bytes([sample[0], sample[1]]))
            .collect()
    }

    #[test]
    fn natural_sequence_order_places_frame2_before_frame10() {
        let dir = temp_dir("natural");
        for name in ["frame10.fits", "frame2.fits", "frame1.fits"] {
            write_i32_fits(&dir.join(name), &[8, 6], None);
        }
        let reader = FitsSequenceReader::new(dir.to_str().unwrap()).unwrap();
        let names: Vec<_> = reader
            .files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["frame1.fits", "frame2.fits", "frame10.fits"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn selecting_one_fits_does_not_import_siblings() {
        let dir = temp_dir("single");
        let selected = dir.join("selected.fits");
        write_i32_fits(&selected, &[8, 6], None);
        write_i32_fits(&dir.join("unrelated.fits"), &[8, 6], None);
        let reader = FitsSequenceReader::new(selected.to_str().unwrap()).unwrap();
        assert_eq!(reader.frame_count, 1);
        assert_eq!(reader.files, vec![selected]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn heterogeneous_sequence_and_unsupported_plane_count_are_rejected() {
        let mixed = temp_dir("mixed");
        write_i32_fits(&mixed.join("frame1.fits"), &[8, 6], None);
        write_i32_fits(&mixed.join("frame2.fits"), &[9, 6], None);
        let err = FitsSequenceReader::new(mixed.to_str().unwrap()).unwrap_err();
        assert!(
            err.contains("heterogénea") && err.contains("frame2.fits"),
            "{err}"
        );
        let _ = fs::remove_dir_all(mixed);

        let mixed_storage = temp_dir("mixed_storage");
        write_i32_fits(&mixed_storage.join("frame1.fits"), &[8, 6], None);
        write_f32_fits(&mixed_storage.join("frame2.fits"), &[8, 6]);
        let err = FitsSequenceReader::new(mixed_storage.to_str().unwrap()).unwrap_err();
        assert!(
            err.contains("heterogénea") && err.contains("stored_bitpix"),
            "{err}"
        );
        let _ = fs::remove_dir_all(mixed_storage);

        let invalid = temp_dir("planes2");
        let path = invalid.join("two_planes.fits");
        write_i32_fits(&path, &[8, 6, 2], None);
        let err = FitsSequenceReader::new(path.to_str().unwrap()).unwrap_err();
        assert!(err.contains("NAXIS3=2"), "{err}");
        let _ = fs::remove_dir_all(invalid);

        let truncated = temp_dir("truncated");
        let good = truncated.join("frame1.fits");
        let bad = truncated.join("frame2.fits");
        write_i32_fits(&good, &[8, 6], None);
        write_i32_fits(&bad, &[8, 6], None);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&bad)
            .unwrap()
            .set_len((2_880 + 8 * 6 * 4 - 1) as u64)
            .unwrap();
        let err = FitsSequenceReader::new(truncated.to_str().unwrap()).unwrap_err();
        assert!(
            err.contains("truncado") && err.contains("frame2.fits"),
            "{err}"
        );
        let _ = fs::remove_dir_all(truncated);
    }

    #[test]
    fn bayer_pattern_and_crop_offsets_map_to_effective_cfa() {
        let cases = [
            ("rggb", "RGGB", 0, 0, 8),
            ("x", "RGGB", 1, 0, 9),
            ("y", "RGGB", 0, 1, 10),
            ("xy", "RGGB", 1, 1, 11),
            ("negative", "BGGR", -1, -1, 8),
        ];
        for (tag, pattern, x, y, expected_id) in cases {
            let dir = temp_dir(tag);
            let path = dir.join("cfa.fits");
            write_i32_fits(&path, &[8, 6], Some((pattern, x, y)));
            let reader = FitsSequenceReader::new(path.to_str().unwrap()).unwrap();
            assert_eq!(reader.color_id, expected_id, "caso {tag}");
            assert!(reader.is_color, "CFA debe activar la ruta de color");
            assert_eq!(reader.bytes_per_pixel, 2);
            assert_eq!(reader.get_frame(0).len(), 8 * 6 * 2);
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn rgb_planes_are_interleaved_directly_without_a_second_frame_buffer() {
        let dir = temp_dir("rgb_planar");
        let path = dir.join("rgb.fits");
        let (width, height) = (8usize, 6usize);
        let pixels = width * height;
        write_i32_fits(&path, &[width, height, 3], None);
        let reader = FitsSequenceReader::new(path.to_str().unwrap()).unwrap();
        assert_eq!(reader.color_id, 100);
        assert_eq!(reader.bytes_per_pixel, 6);
        let frame = reader.get_frame(0);
        assert_eq!(frame.len(), pixels * 6);
        let decoded: Vec<u16> = frame
            .chunks_exact(2)
            .map(|sample| u16::from_le_bytes([sample[0], sample[1]]))
            .collect();
        for pixel in 0..pixels {
            assert_eq!(decoded[pixel * 3], pixel as u16);
            assert_eq!(decoded[pixel * 3 + 1], (pixels + pixel) as u16);
            assert_eq!(decoded[pixel * 3 + 2], (2 * pixels + pixel) as u16);
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn normalized_float_sequence_uses_one_gain_despite_single_frame_outlier() {
        let dir = temp_dir("float_sequence_outlier");
        let shape = [8usize, 6usize];
        let samples: usize = shape.iter().product();
        for frame in 0..3 {
            let mut values = vec![0.25f32; samples];
            if frame == 1 {
                values[0] = 100.0; // cosmic ray: no debe convertir este frame a ADU
            }
            write_f32_values(
                &dir.join(format!("frame{}.fits", frame + 1)),
                &shape,
                values,
                None,
            );
        }

        let reader = FitsSequenceReader::new(dir.to_str().unwrap()).unwrap();
        for frame in 0..reader.frame_count {
            let decoded = decoded_u16(&reader.get_frame(frame));
            assert_eq!(decoded.len(), samples);
            assert_eq!(
                decoded[1], 16_384,
                "el frame {frame} debe compartir la ganancia normalizada de la secuencia"
            );
        }
        assert_eq!(decoded_u16(&reader.get_frame(1))[0], 65_535);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn physical_float_adu_keeps_bzero_and_bscale_without_unit_amplification() {
        let dir = temp_dir("float_physical_scale");
        let path = dir.join("physical.fits");
        let shape = [8usize, 6usize];
        write_f32_values(
            &path,
            &shape,
            vec![100.0f32; shape.iter().product()],
            Some((10.0, 2.0)),
        );

        let reader = FitsSequenceReader::new(path.to_str().unwrap()).unwrap();
        let decoded = decoded_u16(&reader.get_frame(0));
        assert!(decoded.iter().all(|&sample| sample == 210));
        let _ = fs::remove_dir_all(dir);
    }
}
