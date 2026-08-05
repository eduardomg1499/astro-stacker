//! Simulador de verdad conocida PLANETARIO (arnés F0 del plan planetario).
//!
//! Genera escenas de disco planetario con detalle procedural conocido,
//! renderizadas en float64 con supersampling PROPIO (independiente del
//! Lanczos/warp de producción, para evitar el *inverse crime*): banda
//! ecuatorial + óvalos + detalle fino de alta frecuencia, oscurecimiento de
//! limbo, desplazamientos subpíxel exactos, blur gaussiano por-frame de
//! secuencia conocida (verdad del ranking de calidad), mosaico CFA, píxeles
//! calientes y estelas de satélite (verdad de la rejection), y paneles
//! solapados con delta de ganancia conocido (verdad del mosaico).
//!
//! Decisiones de diseño:
//! - Todo en ADU de 16 bits; `disc_adu` es el nivel del disco SIN textura y
//!   `background_adu` el cielo. La textura modula ±~30 % alrededor de 1.0.
//! - El desplazamiento (dx, dy) mueve el DISCO (objeto), no el fondo: la
//!   escena se evalúa analíticamente en coordenadas desplazadas, sin
//!   interpolación, así el ground truth de registro es exacto.
//! - El blur se aplica DESPUÉS del render ideal y ANTES del ruido, con un
//!   kernel gaussiano separable (radio 4σ, bordes reflejados): la secuencia
//!   de sigmas es la verdad conocida del ranking de nitidez.
//! - Ruido gaussiano de lectura determinista (StdRng + Box-Muller propio,
//!   misma disciplina que deepsky_sim): misma semilla ⇒ mismo frame bit a
//!   bit en todas las plataformas.
//! - El writer SER produce el header estándar de 178 bytes little-endian
//!   (mismo contrato que el test dorado de ser.rs) para que los ficheros
//!   sintéticos pasen por el MISMO lector de producción.
//! - CFA: mismos color_id que SER (8=RGGB, 9=GRBG, 10=GBRG, 11=BGGR).
//!
//! Solo se compila para tests o con la feature `planetary-sim`; nunca forma
//! parte del binario de producción.

#![allow(dead_code)]

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

// ---------------------------------------------------------------------------
// Escena
// ---------------------------------------------------------------------------

/// Disco planetario sintético con textura procedural conocida.
#[derive(Clone)]
pub(crate) struct PlanetScene {
    pub width: usize,
    pub height: usize,
    /// Centro del disco en coordenadas de píxel (el centro del píxel (0,0)
    /// está en (0.0, 0.0)).
    pub cx: f64,
    pub cy: f64,
    pub radius_px: f64,
    /// Nivel del disco sin textura (ADU).
    pub disc_adu: f64,
    /// Nivel del cielo (ADU).
    pub background_adu: f64,
    /// Coeficiente de oscurecimiento de limbo u ∈ [0, 1]: I(μ) = 1 − u(1−μ).
    pub limb_darkening: f64,
    /// Tinte por canal RGB (1.0 = neutro); para mono se usa el canal G.
    pub tint: [f64; 3],
}

impl PlanetScene {
    /// Escena estándar tipo Júpiter para tests: disco centrado, bandas +
    /// óvalos + detalle fino.
    pub fn jupiter_like(width: usize, height: usize) -> Self {
        PlanetScene {
            width,
            height,
            cx: width as f64 * 0.5,
            cy: height as f64 * 0.5,
            radius_px: width.min(height) as f64 * 0.32,
            disc_adu: 42_000.0,
            background_adu: 350.0,
            limb_darkening: 0.55,
            tint: [1.0, 1.0, 1.0],
        }
    }
}

/// Textura procedural del disco en coordenadas normalizadas del disco
/// (nx, ny ∈ [−1, 1]). Devuelve un factor multiplicativo ~[0.6, 1.4]:
/// bandas latitudinales tipo Júpiter, dos óvalos y detalle fino de alta
/// frecuencia (la componente que el blur destruye primero y que el ranking
/// de calidad debe detectar).
fn surface_texture(nx: f64, ny: f64) -> f64 {
    let bands = 0.20 * (ny * 9.0 + 0.6 * (nx * 3.0).sin()).sin();
    let oval_a =
        -0.28 * (-(((nx - 0.35).powi(2) + (ny + 0.22).powi(2)) / (2.0 * 0.08f64.powi(2)))).exp();
    let oval_b =
        0.18 * (-(((nx + 0.30).powi(2) + (ny - 0.28).powi(2)) / (2.0 * 0.12f64.powi(2)))).exp();
    let fine = 0.07 * (nx * 40.0).sin() * (ny * 37.0).cos();
    1.0 + bands + oval_a + oval_b + fine
}

/// Renderiza la escena ideal (sin blur ni ruido) con el disco desplazado
/// (dx, dy) píxeles. Supersampling ss×ss por la regla del punto medio.
pub(crate) fn render_ideal(scene: &PlanetScene, dx: f64, dy: f64, ss: usize) -> Vec<f64> {
    render_ideal_channel(scene, dx, dy, ss, 1)
}

/// Igual que `render_ideal` pero aplicando el tinte del canal `c` (0=R,
/// 1=G, 2=B). El detalle procedural es idéntico entre canales: solo cambia
/// la ganancia, como una dominante de color real.
pub(crate) fn render_ideal_channel(
    scene: &PlanetScene,
    dx: f64,
    dy: f64,
    ss: usize,
    c: usize,
) -> Vec<f64> {
    let ss = ss.max(1);
    let inv_ss = 1.0 / ss as f64;
    let gain = scene.tint[c.min(2)];
    let mut out = vec![0.0f64; scene.width * scene.height];
    let cx = scene.cx + dx;
    let cy = scene.cy + dy;
    let inv_r = 1.0 / scene.radius_px;
    for y in 0..scene.height {
        for x in 0..scene.width {
            let mut acc = 0.0f64;
            for sy in 0..ss {
                for sx in 0..ss {
                    // Punto medio del subpíxel, relativo al centro del disco.
                    let px = x as f64 + (sx as f64 + 0.5) * inv_ss - 0.5 - cx;
                    let py = y as f64 + (sy as f64 + 0.5) * inv_ss - 0.5 - cy;
                    let nx = px * inv_r;
                    let ny = py * inv_r;
                    let r2 = nx * nx + ny * ny;
                    if r2 < 1.0 {
                        let mu = (1.0 - r2).sqrt();
                        let limb = 1.0 - scene.limb_darkening * (1.0 - mu);
                        acc += scene.disc_adu * gain * limb * surface_texture(nx, ny);
                    } else {
                        acc += scene.background_adu;
                    }
                }
            }
            out[y * scene.width + x] = acc * inv_ss * inv_ss;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Blur y ruido
// ---------------------------------------------------------------------------

/// Blur gaussiano separable en f64, radio 4σ, bordes reflejados. σ ≤ 0 es
/// identidad. Es el "seeing" sintético: la secuencia de sigmas por frame es
/// la verdad conocida del ranking de calidad.
pub(crate) fn gaussian_blur_f64(img: &[f64], w: usize, h: usize, sigma: f64) -> Vec<f64> {
    if sigma <= 0.0 {
        return img.to_vec();
    }
    let radius = (sigma * 4.0).ceil() as isize;
    let mut kernel = Vec::with_capacity((2 * radius + 1) as usize);
    let inv_2s2 = 1.0 / (2.0 * sigma * sigma);
    let mut sum = 0.0;
    for k in -radius..=radius {
        let v = (-(k * k) as f64 * inv_2s2).exp();
        kernel.push(v);
        sum += v;
    }
    for v in kernel.iter_mut() {
        *v /= sum;
    }
    let reflect = |i: isize, n: isize| -> usize {
        let mut i = i;
        if i < 0 {
            i = -i - 1;
        }
        if i >= n {
            i = 2 * n - 1 - i;
        }
        i.clamp(0, n - 1) as usize
    };
    // Pasada horizontal.
    let mut tmp = vec![0.0f64; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (ki, kv) in kernel.iter().enumerate() {
                let sx = reflect(x as isize + ki as isize - radius, w as isize);
                acc += img[y * w + sx] * kv;
            }
            tmp[y * w + x] = acc;
        }
    }
    // Pasada vertical.
    let mut out = vec![0.0f64; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (ki, kv) in kernel.iter().enumerate() {
                let sy = reflect(y as isize + ki as isize - radius, h as isize);
                acc += tmp[sy * w + x] * kv;
            }
            out[y * w + x] = acc;
        }
    }
    out
}

/// Normal(0,1) determinista por Box-Muller (misma disciplina que
/// deepsky_sim: rand 0.8 sin rand_distr).
fn normal01(rng: &mut StdRng) -> f64 {
    let u1: f64 = rng.gen_range(f64::MIN_POSITIVE..1.0);
    let u2: f64 = rng.gen_range(0.0..1.0);
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

/// Cuantiza a u16 añadiendo ruido gaussiano de lectura (ADU) determinista.
pub(crate) fn quantize_u16(img: &[f64], noise_adu: f64, seed: u64) -> Vec<u16> {
    let mut rng = StdRng::seed_from_u64(seed);
    img.iter()
        .map(|&v| {
            let n = if noise_adu > 0.0 {
                normal01(&mut rng) * noise_adu
            } else {
                0.0
            };
            (v + n).round().clamp(0.0, 65535.0) as u16
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// Parámetros de un frame sintético (verdad conocida por campo).
#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameSpec {
    /// Desplazamiento global exacto del disco (px).
    pub dx: f64,
    pub dy: f64,
    /// Sigma del blur de "seeing" (px); menor = frame más nítido.
    pub blur_sigma: f64,
    /// Ruido de lectura (ADU, 1σ).
    pub noise_adu: f64,
    pub seed: u64,
}

/// Frame mono16: ideal → blur → ruido → u16.
pub(crate) fn make_frame_mono16(scene: &PlanetScene, spec: &FrameSpec) -> Vec<u16> {
    let ideal = render_ideal(scene, spec.dx, spec.dy, 4);
    let blurred = gaussian_blur_f64(&ideal, scene.width, scene.height, spec.blur_sigma);
    quantize_u16(&blurred, spec.noise_adu, spec.seed)
}

/// Canal CFA que corresponde al fotosito (x, y) para un color_id SER
/// (8=RGGB, 9=GRBG, 10=GBRG, 11=BGGR). Devuelve 0=R, 1=G, 2=B.
pub(crate) fn cfa_channel(color_id: i32, x: usize, y: usize) -> usize {
    let (ex, ey) = (x & 1, y & 1);
    match color_id {
        8 => [[0, 1], [1, 2]][ey][ex],  // RGGB
        9 => [[1, 0], [2, 1]][ey][ex],  // GRBG
        10 => [[1, 2], [0, 1]][ey][ex], // GBRG
        11 => [[2, 1], [1, 0]][ey][ex], // BGGR
        _ => 1,
    }
}

/// Frame Bayer16 (mosaico CFA): cada fotosito muestrea el canal que le toca
/// del render RGB ideal (blur y ruido aplicados por canal, misma semilla
/// derivada por canal para determinismo).
pub(crate) fn make_frame_bayer16(scene: &PlanetScene, spec: &FrameSpec, color_id: i32) -> Vec<u16> {
    let mut chans: Vec<Vec<u16>> = Vec::with_capacity(3);
    for c in 0..3 {
        let ideal = render_ideal_channel(scene, spec.dx, spec.dy, 4, c);
        let blurred = gaussian_blur_f64(&ideal, scene.width, scene.height, spec.blur_sigma);
        chans.push(quantize_u16(
            &blurred,
            spec.noise_adu,
            spec.seed.wrapping_add(c as u64),
        ));
    }
    let mut out = vec![0u16; scene.width * scene.height];
    for y in 0..scene.height {
        for x in 0..scene.width {
            let c = cfa_channel(color_id, x, y);
            out[y * scene.width + x] = chans[c][y * scene.width + x];
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Artefactos transitorios (verdad de la rejection)
// ---------------------------------------------------------------------------

/// Suma `add_adu` a los píxeles indicados (saturando), p.ej. píxeles
/// calientes residuales.
pub(crate) fn add_hot_pixels(frame: &mut [u16], w: usize, pixels: &[(usize, usize, u16)]) {
    for &(x, y, adu) in pixels {
        let i = y * w + x;
        if i < frame.len() {
            frame[i] = frame[i].saturating_add(adu);
        }
    }
}

/// Estela de satélite/avión: segmento (x0,y0)→(x1,y1) con grosor dado,
/// sumando `add_adu` (saturante). Determinista.
pub(crate) fn add_satellite_trail(
    frame: &mut [u16],
    w: usize,
    h: usize,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    width_px: f64,
    add_adu: u16,
) {
    let len = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt().max(1.0);
    let steps = (len * 2.0).ceil() as usize;
    let hw = (width_px * 0.5).max(0.5);
    let r = hw.ceil() as isize;
    let mut stamped = vec![false; w * h];
    for s in 0..=steps {
        let t = s as f64 / steps as f64;
        let cx = x0 + (x1 - x0) * t;
        let cy = y0 + (y1 - y0) * t;
        for oy in -r..=r {
            for ox in -r..=r {
                let px = cx + ox as f64;
                let py = cy + oy as f64;
                if px < 0.0 || py < 0.0 {
                    continue;
                }
                let (xi, yi) = (px as usize, py as usize);
                if xi >= w || yi >= h {
                    continue;
                }
                let d2 = (px - cx).powi(2) + (py - cy).powi(2);
                let i = yi * w + xi;
                if d2 <= hw * hw && !stamped[i] {
                    stamped[i] = true;
                    frame[i] = frame[i].saturating_add(add_adu);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Paneles de mosaico (verdad del stitching)
// ---------------------------------------------------------------------------

/// Dos paneles solapados recortados de la MISMA escena ideal, con ganancias
/// distintas (transparencia variable entre capturas). Devuelve
/// (panel_a, panel_b, panel_w, panel_h, solape_px). El panel B está
/// desplazado `shift_x` px a la derecha en la escena.
pub(crate) fn make_gain_panels(
    scene: &PlanetScene,
    panel_w: usize,
    panel_h: usize,
    shift_x: usize,
    gain_a: f64,
    gain_b: f64,
    noise_adu: f64,
    seed: u64,
) -> (Vec<u16>, Vec<u16>, usize) {
    assert!(panel_w + shift_x <= scene.width && panel_h <= scene.height);
    let ideal = render_ideal(scene, 0.0, 0.0, 4);
    let crop = |x0: usize, gain: f64, seed: u64| -> Vec<u16> {
        let mut sub = vec![0.0f64; panel_w * panel_h];
        for y in 0..panel_h {
            for x in 0..panel_w {
                sub[y * panel_w + x] = ideal[y * scene.width + (x + x0)] * gain;
            }
        }
        quantize_u16(&sub, noise_adu, seed)
    };
    let a = crop(0, gain_a, seed);
    let b = crop(shift_x, gain_b, seed.wrapping_add(1));
    let overlap = panel_w - shift_x;
    (a, b, overlap)
}

// ---------------------------------------------------------------------------
// Writer SER (header estándar 178 bytes, little-endian)
// ---------------------------------------------------------------------------

/// Escribe un SER de 16 bits little-endian con el header estándar de 178
/// bytes (mismo contrato que el test dorado de ser.rs). `color_id` 0=MONO,
/// 8..11=Bayer. Los frames deben medir width*height.
pub(crate) fn write_ser_16(
    path: &std::path::Path,
    width: usize,
    height: usize,
    color_id: i32,
    frames: &[Vec<u16>],
) -> std::io::Result<()> {
    let mut data = Vec::with_capacity(178 + frames.len() * width * height * 2);
    data.extend_from_slice(b"LUCAM-RECORDER"); // FileID (14 bytes)
    let put_i32 = |buf: &mut Vec<u8>, v: i32| buf.extend_from_slice(&v.to_le_bytes());
    put_i32(&mut data, 0); // LuID
    put_i32(&mut data, color_id); // ColorID (offset 18)
    put_i32(&mut data, 0); // LittleEndian flag (offset 22)
    put_i32(&mut data, width as i32); // Width (offset 26)
    put_i32(&mut data, height as i32); // Height
    put_i32(&mut data, 16); // PixelDepth
    put_i32(&mut data, frames.len() as i32); // FrameCount
    data.resize(178, 0); // Observer/Instrument/Telescope + DateTime
    for frame in frames {
        assert_eq!(frame.len(), width * height, "frame con geometría errónea");
        for &v in frame {
            data.extend_from_slice(&v.to_le_bytes());
        }
    }
    std::fs::write(path, data)
}

// ---------------------------------------------------------------------------
// Tests del propio simulador
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_frames_are_deterministic() {
        let scene = PlanetScene::jupiter_like(96, 96);
        let spec = FrameSpec {
            dx: 0.37,
            dy: -0.81,
            blur_sigma: 1.2,
            noise_adu: 90.0,
            seed: 7,
        };
        let a = make_frame_mono16(&scene, &spec);
        let b = make_frame_mono16(&scene, &spec);
        assert_eq!(a, b, "misma semilla debe dar el mismo frame bit a bit");
        let spec2 = FrameSpec { seed: 8, ..spec };
        assert_ne!(
            a,
            make_frame_mono16(&scene, &spec2),
            "semillas distintas deben diferir"
        );
    }

    #[test]
    fn sim_blur_reduces_gradient_energy_monotonically() {
        let scene = PlanetScene::jupiter_like(128, 128);
        let energy = |sigma: f64| -> f64 {
            let f = make_frame_mono16(
                &scene,
                &FrameSpec {
                    dx: 0.0,
                    dy: 0.0,
                    blur_sigma: sigma,
                    noise_adu: 0.0,
                    seed: 1,
                },
            );
            let mut e = 0.0f64;
            for y in 1..127usize {
                for x in 1..127usize {
                    let gx = f[y * 128 + x + 1] as f64 - f[y * 128 + x - 1] as f64;
                    let gy = f[(y + 1) * 128 + x] as f64 - f[(y - 1) * 128 + x] as f64;
                    e += gx * gx + gy * gy;
                }
            }
            e
        };
        let e0 = energy(0.0);
        let e1 = energy(1.0);
        let e2 = energy(2.5);
        assert!(
            e0 > e1 && e1 > e2,
            "el blur debe degradar la energía de gradiente: {e0} {e1} {e2}"
        );
    }

    #[test]
    fn sim_ser_roundtrip_through_production_reader() {
        let scene = PlanetScene::jupiter_like(64, 64);
        let frames: Vec<Vec<u16>> = (0..3)
            .map(|i| {
                make_frame_mono16(
                    &scene,
                    &FrameSpec {
                        dx: i as f64 * 0.5,
                        dy: 0.0,
                        blur_sigma: 0.8,
                        noise_adu: 60.0,
                        seed: i as u64,
                    },
                )
            })
            .collect();
        let path =
            std::env::temp_dir().join(format!("zas_planetary_sim_{}.ser", std::process::id()));
        write_ser_16(&path, 64, 64, 0, &frames).unwrap();
        let reader = crate::ser::SerReader::new(&path).expect("SER sintético legible");
        assert_eq!(reader.info.width, 64);
        assert_eq!(reader.info.height, 64);
        assert_eq!(reader.info.frame_count, 3);
        assert_eq!(reader.info.color_id, 0);
        assert_eq!(reader.info.sample_bits, 16);
        let f1 = reader.get_frame(1, 0);
        let via_reader = crate::raw_to_u16_buffer(&f1, 64, 64, 2);
        assert_eq!(
            via_reader, frames[1],
            "los píxeles deben sobrevivir el viaje por el lector de producción"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sim_cfa_channels_follow_pattern() {
        // RGGB: (0,0)=R (1,0)=G (0,1)=G (1,1)=B
        assert_eq!(cfa_channel(8, 0, 0), 0);
        assert_eq!(cfa_channel(8, 1, 0), 1);
        assert_eq!(cfa_channel(8, 0, 1), 1);
        assert_eq!(cfa_channel(8, 1, 1), 2);
        // BGGR invierte RGGB.
        assert_eq!(cfa_channel(11, 0, 0), 2);
        assert_eq!(cfa_channel(11, 1, 1), 0);
        // El mosaico refleja el tinte del canal: R fuerte en fotositos R.
        let mut scene = PlanetScene::jupiter_like(64, 64);
        scene.tint = [1.3, 1.0, 0.7];
        let bayer = make_frame_bayer16(
            &scene,
            &FrameSpec {
                dx: 0.0,
                dy: 0.0,
                blur_sigma: 0.0,
                noise_adu: 0.0,
                seed: 3,
            },
            8,
        );
        // Media de fotositos R vs B dentro del disco (evitar fondo).
        let (mut sr, mut nr, mut sb, mut nb) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for y in 24..40usize {
            for x in 24..40usize {
                let v = bayer[y * 64 + x] as f64;
                match cfa_channel(8, x, y) {
                    0 => {
                        sr += v;
                        nr += 1.0;
                    }
                    2 => {
                        sb += v;
                        nb += 1.0;
                    }
                    _ => {}
                }
            }
        }
        assert!(
            sr / nr > (sb / nb) * 1.5,
            "el tinte R>B debe verse en el mosaico"
        );
    }

    #[test]
    fn sim_trail_and_hot_pixels_hit_expected_pixels() {
        let w = 64usize;
        let mut frame = vec![100u16; w * w];
        add_hot_pixels(&mut frame, w, &[(10, 12, 30_000)]);
        assert_eq!(frame[12 * w + 10], 30_100);
        add_satellite_trail(&mut frame, w, w, 0.0, 0.0, 63.0, 63.0, 2.0, 5_000);
        assert!(
            frame[32 * w + 32] >= 5_100,
            "la diagonal debe llevar estela"
        );
        assert_eq!(
            frame[5 * w + 55],
            100,
            "lejos de la estela no debe cambiar nada"
        );
    }

    #[test]
    fn sim_gain_panels_share_geometry_with_known_gain_delta() {
        let scene = PlanetScene::jupiter_like(200, 120);
        let (a, b, overlap) = make_gain_panels(&scene, 120, 120, 80, 1.0, 1.12, 0.0, 5);
        assert_eq!(overlap, 40);
        // En el solape, B/A debe ser ~1.12 en píxeles con señal.
        let mut ratios = Vec::new();
        for y in 40..80usize {
            for x in 0..overlap {
                let va = a[y * 120 + (x + 80)] as f64; // columna x+80 de A
                let vb = b[y * 120 + x] as f64; //        = columna x de B
                if va > 5_000.0 {
                    ratios.push(vb / va);
                }
            }
        }
        assert!(!ratios.is_empty());
        let mean: f64 = ratios.iter().sum::<f64>() / ratios.len() as f64;
        assert!(
            (mean - 1.12).abs() < 0.01,
            "delta de ganancia conocido: {mean}"
        );
    }
}
