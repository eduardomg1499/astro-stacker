#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod alignment;
mod avi;
mod converter;
mod derotation;
mod fits_sequence;
mod frame_source;
mod frame_store;
mod gpu_analysis; // preprocesado planetario por lotes (downscale/blur/Laplaciano)
mod gpu_deepsky; // GPU compute para calibracion/warp/integracion de cielo profundo
mod gpu_stack; // GPU compute (wgpu) para la etapa de acumulacion
mod gpu_wavelet; // GPU compute (wgpu) para la descomposicion wavelet (blur separable)
mod integral_image;
mod license;
mod liquid_warping;
mod pipeline;
mod benchmark;
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
include!("commands_core.rs");
include!("deepsky.rs");
include!("spcc.rs");

include!("smart_ap_generator.rs");
