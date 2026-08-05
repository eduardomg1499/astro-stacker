#![recursion_limit = "256"]
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod alignment;
mod avi;
mod benchmark;
mod benchmark_quality; // métricas de artefactos planetarios (arnés A/B F0)
mod converter;
mod deepsky_background; // F2: grafo de fondo/LP, modelo BG y asesor de muestreo
mod deepsky_calibration_contract; // firmas estrictas, dark scaling y pedestal de flats
mod deepsky_calibration_stats; // media robusta y determinista para masters
mod deepsky_linear_frame; // SCI/VAR/DQ/PSF + metadata como invariante validada
mod deepsky_masks; // F3: máscaras de outlier congeladas por cross-fit
mod deepsky_noise; // diversidad de dither y patrones detector-coordinate
mod deepsky_psf; // F5: PSF Moffat elíptica + campo espacial con holdout
mod deepsky_signature; // aliases FITS -> CalibrationSignature, ROI/fase CFA
#[cfg(any(test, feature = "deepsky-sim"))]
mod deepsky_sim; // simulador de verdad conocida (solo tests/benchmarks)
mod deepsky_struct; // F7: STRUCT — starlet B3 + validación split-half con FDR
mod deepsky_studio_layers; // Studio: restauración float32 y ramas objeto/estrellas
mod deepsky_variance; // contrato lineal F1: VAR/NEFF/DQ (motores científicos)
mod derotation;
mod eidr; // F9: reconstrucción forward-model sucesora de Drizzle
mod fits_sequence;
mod frame_source;
mod frame_store;
mod gpu_analysis; // preprocesado planetario por lotes (downscale/blur/Laplaciano)
mod gpu_deepsky; // GPU compute para calibracion/warp/integracion de cielo profundo
mod gpu_eidr; // F10: matvec del operador EIDR en wgpu (paridad CPU obligatoria)
mod gpu_stack; // GPU compute (wgpu) para la etapa de acumulacion
mod gpu_wavelet; // GPU compute (wgpu) para la descomposicion wavelet (blur separable)
mod integral_image;
mod license;
mod liquid_warping;
mod milky_way;
mod milky_way_registration;
mod nebula_fusion; // F3: motor NF-Lite (pesos 1/σ² + máscaras congeladas)
mod nebula_fusion_full; // F6: coadición GLS por frecuencia con PSF objetivo
mod perf_trace; // cronómetros por fase del pipeline planetario (volcado JSON)
mod pipeline;
mod planetary_planner; // plan adaptativo por etapa + perfiles locales de calibracion
mod planetary_quality; // contratos científicos/perf compartidos del stack planetario
#[cfg(any(test, feature = "planetary-sim"))]
mod planetary_sim; // simulador planetario de verdad conocida (solo tests/benchmarks)
mod ser;
mod smart_grid; // Added smart_grid module
use liquid_warping::*;
use pipeline::*;

use alignment::*;
use avi::AviReader;
use base64::{engine::general_purpose, Engine as _};
use image::{ColorType, Delay, DynamicImage, Frame, GenericImageView, Rgba, RgbaImage};
use imageproc::drawing::draw_text_mut;

use fits_sequence::FitsSequenceReader;
use frame_source::{FrameRoi, FrameSource, UnifiedFrameSource};
use frame_store::AdaptiveFrameStore;
use license::{AppStatus, LicenseManager};
use rayon::prelude::*;
use rusttype::{Font, Scale};
use ser::SerReader;
use serde_json;
use smart_grid::ApPoint; // Import ApPoint
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
use std::borrow::Cow;
use std::fs::{self, File};
use std::io::BufWriter;
use std::io::Cursor;
use std::io::Write;
use std::io::{BufReader, Read};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering}; // Removed AtomicU64
use std::sync::{Arc, Mutex};
use sysinfo::System;
use tauri::{Emitter, Manager, State};

include!("types.rs");

include!("core_utils.rs");

include!("alignment_helpers.rs");

include!("advanced_wavelets.rs");

include!("commands_v2_v3.rs");
include!("postprocess_io.rs");
include!("commands_core.rs");
include!("deepsky.rs");
include!("deepsky_poststack.rs");
include!("deepsky_studio_palette.rs");
include!("deepsky_annotations.rs");
include!("spcc.rs");

include!("smart_ap_generator.rs");
