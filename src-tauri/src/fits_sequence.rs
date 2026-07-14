use fitrs::Fits;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

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

        // If the path is a single .fits file, use its parent directory as the sequence root.
        let folder = if root.is_file() {
            root.parent().unwrap_or(root)
        } else {
            root
        };

        if !folder.exists() || !folder.is_dir() {
            return Err("Ruta invalida o no es un directorio FITS.".into());
        }

        // 1. Scan and Sort all .fit or .fits files
        let mut files = Vec::new();
        if let Ok(entries) = fs::read_dir(folder) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    let ext = path
                        .extension()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if ext == "fits" || ext == "fit" {
                        files.push(path);
                    }
                }
            }
        }

        if files.is_empty() {
            return Err("No se encontraron archivos FITS en la carpeta.".into());
        }

        // Sort alphabetically/numerically based on filename
        files.sort_by(|a, b| {
            let name_a = a.file_name().unwrap_or_default().to_string_lossy();
            let name_b = b.file_name().unwrap_or_default().to_string_lossy();
            name_a.cmp(&name_b)
        });

        let frame_count = files.len();

        // 2. Read first file to establish Sequence parameters (Dimensions, Bit Depth)
        let first_file = &files[0];
        let meta = Self::read_fits_metadata(first_file.to_str().unwrap())?;

        Ok(Self {
            folder_path: folder.to_string_lossy().into_owned(),
            files,
            width: meta.width,
            height: meta.height,
            bytes_per_pixel: meta.bytes_per_pixel,
            sample_bits: meta.sample_bits,
            frame_count,
            color_id: meta.color_id,
            is_color: meta.is_color,
            fps: 30.0, // Fits doesn't usually store FPS, assuming typical video rate or 30 for calculations
        })
    }

    fn read_fits_metadata(path: &str) -> Result<FitsMeta, String> {
        let fits =
            Fits::open(path).map_err(|e| format!("Error abriendo FITS {}: {:?}", path, e))?;
        let hdu = fits.get(0).ok_or("No Primary HDU in FITS")?;

        let width_str = match hdu.value("NAXIS1") {
            Some(v) => format!("{:?}", v),
            None => "".to_string(),
        };
        let height_str = match hdu.value("NAXIS2") {
            Some(v) => format!("{:?}", v),
            None => "".to_string(),
        };
        let bitpix_str = match hdu.value("BITPIX") {
            Some(v) => format!("{:?}", v),
            None => "".to_string(),
        };

        let naxis3_str = match hdu.value("NAXIS3") {
            Some(v) => format!("{:?}", v),
            None => "".to_string(),
        };

        let width = Self::parse_header_int(&width_str).unwrap_or(0) as usize;
        let height = Self::parse_header_int(&height_str).unwrap_or(0) as usize;
        let bitpix = Self::parse_header_int(&bitpix_str).unwrap_or(8);
        let naxis3 = Self::parse_header_int(&naxis3_str).unwrap_or(1);

        let is_color = naxis3 >= 3;

        let bytes_per_pixel = match bitpix {
            8 => 1,
            16 => 2,
            -32 => 4, // 32-bit float
            _ => 2,   // Default fallback to 16
        };

        // Determine color ID based on BPP and RGB
        let color_id = if is_color {
            if bytes_per_pixel == 1 {
                2
            } else {
                8
            } // RGB 8-bit or RGB 16-bit
        } else {
            if bytes_per_pixel == 1 {
                0
            } else {
                12
            } // Mono 8-bit or Mono 16-bit
        };

        let total_bpp = if is_color {
            bytes_per_pixel * 3
        } else {
            bytes_per_pixel
        };

        Ok(FitsMeta {
            width,
            height,
            bytes_per_pixel: total_bpp,
            sample_bits: bitpix.unsigned_abs() as usize,
            is_color,
            color_id,
        })
    }

    fn parse_header_int(val: &str) -> Option<i64> {
        let clean = val
            .replace("Int(", "")
            .replace("Float(", "")
            .replace("String(", "")
            .replace(")", "")
            .trim()
            .to_string();
        clean.parse::<i64>().ok()
    }

    pub fn get_frame(&self, index: usize) -> Vec<u8> {
        if index >= self.files.len() {
            return Vec::new(); // Drop safely
        }

        let file_path = &self.files[index];
        match Self::read_fits_data(
            file_path.to_str().unwrap(),
            self.width,
            self.height,
            self.bytes_per_pixel,
            false,
        ) {
            Ok(data) => data,
            Err(e) => {
                println!("FITS Decode Error: {}", e);
                Vec::new()
            }
        }
    }

    fn read_fits_data(
        path: &str,
        width: usize,
        height: usize,
        expected_bpp: usize,
        _is_color: bool,
    ) -> Result<Vec<u8>, String> {
        let fits = Fits::open(path).map_err(|e| format!("FITS open error: {:?}", e))?;
        let hdu = fits.iter().next().ok_or("No HDU")?;

        let expected_size = width * height * expected_bpp;
        let mut out_buffer = Vec::with_capacity(expected_size);

        // We extract the data. fitrs returns `Result<FitsData>`
        match hdu.read_data() {
            fitrs::FitsData::Characters(arr) => {
                let flat = arr.data;
                for c in flat {
                    out_buffer.push(c as u8);
                }
            }
            fitrs::FitsData::IntegersI32(arr) => {
                let flat: Vec<Option<i32>> = arr.data;
                for val_opt in flat {
                    let val = val_opt.unwrap_or(0);
                    // Usually astronomy images are 16-bit. If fits returned them as i32, we crush it to u16.
                    let u16_val = val.max(0).min(65535) as u16;
                    out_buffer.push((u16_val & 0xFF) as u8);
                    out_buffer.push(((u16_val >> 8) & 0xFF) as u8);
                }
            }
            fitrs::FitsData::IntegersU32(arr) => {
                let flat: Vec<Option<u32>> = arr.data;
                for val_opt in flat {
                    let val = val_opt.unwrap_or(0);
                    let u16_val = val.min(65535) as u16;
                    out_buffer.push((u16_val & 0xFF) as u8);
                    out_buffer.push(((u16_val >> 8) & 0xFF) as u8);
                }
            }
            fitrs::FitsData::FloatingPoint32(arr) => {
                let flat: Vec<f32> = arr.data;
                for val in flat {
                    let u16_val = (val * 65535.0).max(0.0).min(65535.0) as u16;
                    out_buffer.push((u16_val & 0xFF) as u8);
                    out_buffer.push(((u16_val >> 8) & 0xFF) as u8);
                }
            }
            fitrs::FitsData::FloatingPoint64(arr) => {
                let flat: Vec<f64> = arr.data;
                for val in flat {
                    let u16_val = (val * 65535.0).max(0.0).min(65535.0) as u16;
                    out_buffer.push((u16_val & 0xFF) as u8);
                    out_buffer.push(((u16_val >> 8) & 0xFF) as u8);
                }
            }
        }

        // Pad if unexpected size
        if out_buffer.len() < expected_size {
            out_buffer.resize(expected_size, 0);
        } else if out_buffer.len() > expected_size {
            out_buffer.truncate(expected_size);
        }

        Ok(out_buffer)
    }

    pub fn get_frames_batch(&self, indices: &[usize]) -> Result<HashMap<usize, Vec<u8>>, String> {
        use rayon::prelude::*;

        let mut map = HashMap::with_capacity(indices.len());

        let frames: Vec<(usize, Vec<u8>)> = indices
            .par_iter()
            .filter_map(|&idx| {
                if idx < self.files.len() {
                    let frame = self.get_frame(idx);
                    if !frame.is_empty() {
                        return Some((idx, frame));
                    }
                }
                None
            })
            .collect();

        for (idx, frame) in frames {
            map.insert(idx, frame);
        }

        Ok(map)
    }
}

struct FitsMeta {
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    sample_bits: usize,
    is_color: bool,
    color_id: i32,
}
