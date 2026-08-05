//! Máscaras de outlier congeladas por cross-fit (F3, NF-Lite).
//!
//! El clipping κσ clásico decide DURANTE la integración y con la estadística
//! del propio píxel, lo que confunde núcleos estelares submuestreados con
//! outliers y hace que el resultado dependa del orden de las pasadas. Aquí
//! las máscaras se construyen ANTES de la combinación final y quedan
//! CONGELADAS (§6.5 del plan técnico):
//!
//! - Piloto leave-one-out desde totales corrientes: para el frame i,
//!   `piloto_i = (S − w_i·y_i) / (W − w_i)` con S=Σw·y, W=Σw ya acumulados.
//!   El plan prescribe LOO para 5≤N<8 y folds para N≥8 por coste; con el
//!   truco de los totales el LOO cuesta lo mismo que los folds y usa N−1
//!   frames por piloto (estadísticamente mejor), así que se usa LOO para
//!   todo N≥5. Con N<5 no se enmascara (solo cosmética previa).
//! - Residual normalizado con las varianzas que implican los propios pesos
//!   inverso-varianza: Var(y_i)=1/w_i y Var(piloto)=1/(W−w_i), luego
//!   r = (y_i − piloto) / sqrt(1/w_i + 1/(W−w_i)).
//! - Semilla |r|>5σ, crecimiento a vecinos conectados con |r|>3.5σ (BFS
//!   8-conexo) y dilatación final por el soporte de interpolación (Lanczos
//!   contamina ±2 px alrededor de un píxel corrupto).
//! - El píxel enmascarado NO se sustituye: se excluye y baja la cobertura.

#![allow(dead_code)]

/// Máscara congelada de UN frame en espacio de salida (índices de píxel,
/// sparse — los outliers reales son <<1% del frame). `positive`/`negative`
/// separan el signo del residual para los mapas de rechazo alto/bajo.
pub(crate) struct FrozenFrameMask {
    pub positive: Vec<u32>,
    pub negative: Vec<u32>,
}

impl FrozenFrameMask {
    pub(crate) fn is_empty(&self) -> bool {
        self.positive.is_empty() && self.negative.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.positive.len() + self.negative.len()
    }

    /// Expande la máscara sparse a un bitset denso (u64 por 64 píxeles) para
    /// la consulta O(1) del acumulador. Transitorio: uno por frame a la vez.
    pub(crate) fn to_bitset(&self, npx: usize) -> Vec<u64> {
        let mut bits = vec![0u64; npx.div_ceil(64)];
        for &p in self.positive.iter().chain(&self.negative) {
            let p = p as usize;
            if p < npx {
                bits[p >> 6] |= 1u64 << (p & 63);
            }
        }
        bits
    }
}

/// Parámetros del cross-fit (valores del plan §6.5).
pub(crate) struct CrossFitConfig {
    /// Umbral de semilla en sigmas (5.0).
    pub seed_sigma: f64,
    /// Umbral de crecimiento para vecinos conectados (3.5).
    pub grow_sigma: f64,
    /// Dilatación final en píxeles por el soporte de interpolación (2 para
    /// Lanczos-3: los lóbulos significativos alcanzan ±2 px).
    pub dilate_px: usize,
    /// Mínimo de frames para enmascarar (5; por debajo, solo cosmética).
    pub min_frames: usize,
}

impl Default for CrossFitConfig {
    fn default() -> Self {
        Self {
            seed_sigma: 5.0,
            grow_sigma: 3.5,
            dilate_px: 2,
            min_frames: 5,
        }
    }
}

/// Construye la máscara congelada de un frame desde su plano de RESIDUAL
/// NORMALIZADO (npx, peor canal con signo, ya dividido por la σ efectiva —
/// el caller lo obtiene de la curva ruido-vs-nivel). `dilate` aplica la
/// dilatación por soporte de interpolación (solo en la ronda final).
pub(crate) fn build_frozen_mask(
    residual: &[f32],
    w: usize,
    h: usize,
    n_frames: usize,
    cfg: &CrossFitConfig,
    dilate: bool,
) -> FrozenFrameMask {
    let npx = w * h;
    let empty = FrozenFrameMask {
        positive: Vec::new(),
        negative: Vec::new(),
    };
    if n_frames < cfg.min_frames || npx == 0 || residual.len() != npx {
        return empty;
    }

    // Semillas + crecimiento BFS 8-conexo sobre |r| > grow_sigma.
    let seed = cfg.seed_sigma as f32;
    let grow = cfg.grow_sigma as f32;
    let mut masked = vec![false; npx];
    let mut queue: Vec<u32> = Vec::new();
    for p in 0..npx {
        if residual[p].abs() > seed {
            masked[p] = true;
            queue.push(p as u32);
        }
    }
    while let Some(p) = queue.pop() {
        let p = p as usize;
        let (x, y) = (p % w, p / w);
        for dy in -1i64..=1 {
            for dx in -1i64..=1 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let nx = x as i64 + dx;
                let ny = y as i64 + dy;
                if nx < 0 || ny < 0 || nx >= w as i64 || ny >= h as i64 {
                    continue;
                }
                let np = ny as usize * w + nx as usize;
                if !masked[np] && residual[np].abs() > grow {
                    masked[np] = true;
                    queue.push(np as u32);
                }
            }
        }
    }

    // Dilatación por soporte de interpolación (aplica el signo del residual
    // del píxel que la origina; en empate gana el positivo, el caso cósmico).
    if dilate && cfg.dilate_px > 0 {
        let core = masked.clone();
        let r = cfg.dilate_px as i64;
        for p in 0..npx {
            if !core[p] {
                continue;
            }
            let (x, y) = (p % w, p / w);
            for dy in -r..=r {
                for dx in -r..=r {
                    let nx = x as i64 + dx;
                    let ny = y as i64 + dy;
                    if nx < 0 || ny < 0 || nx >= w as i64 || ny >= h as i64 {
                        continue;
                    }
                    masked[ny as usize * w + nx as usize] = true;
                }
            }
        }
    }

    let mut positive = Vec::new();
    let mut negative = Vec::new();
    for p in 0..npx {
        if masked[p] {
            if residual[p] >= 0.0 {
                positive.push(p as u32);
            } else {
                negative.push(p as u32);
            }
        }
    }
    FrozenFrameMask { positive, negative }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cósmico de 60σ en un píxel: máscara = píxel + dilatación, nada más.
    #[test]
    fn test_frozen_mask_catches_cosmic_and_only_cosmic() {
        let (w, h) = (32, 32);
        let npx = w * h;
        let mut residual = vec![0.0f32; npx];
        let hot = 16 * w + 16;
        residual[hot] = 60.0;
        let cfg = CrossFitConfig::default();
        let mask = build_frozen_mask(&residual, w, h, 8, &cfg, true);
        assert!(!mask.is_empty());
        assert!(mask.positive.contains(&(hot as u32)));
        // Dilatación 2 px ⇒ como máximo un cuadrado de 5×5 = 25 píxeles.
        assert!(mask.len() <= 25, "máscara demasiado grande: {}", mask.len());
        assert!(mask.negative.is_empty());
        let bits = mask.to_bitset(npx);
        assert_eq!((bits[hot >> 6] >> (hot & 63)) & 1, 1);
    }

    #[test]
    fn test_frozen_mask_grows_along_satellite_trail() {
        // Traza a 4σ con un pico de 8σ: la semilla es solo el pico, el
        // crecimiento conectado debe recorrer la traza completa.
        let (w, h) = (64, 16);
        let mut residual = vec![0.0f32; w * h];
        let row = 8usize;
        for x in 10..54 {
            residual[row * w + x] = 4.0;
        }
        residual[row * w + 32] = 8.0;
        let cfg = CrossFitConfig::default();
        let mask = build_frozen_mask(&residual, w, h, 10, &cfg, false);
        assert!(
            mask.positive.len() >= 44,
            "solo {} píxeles de la traza",
            mask.positive.len()
        );
    }

    #[test]
    fn test_frozen_mask_empty_below_min_frames() {
        let (w, h) = (8, 8);
        let mut residual = vec![0.0f32; w * h];
        residual[0] = 1e6;
        let mask = build_frozen_mask(&residual, w, h, 4, &CrossFitConfig::default(), true);
        assert!(mask.is_empty());
    }

    #[test]
    fn test_frozen_mask_clean_noise_has_negligible_false_rejection() {
        // Residual N(0,1) puro: P(|r|>5) ≈ 5.7e-7; exigimos <1e-4.
        let (w, h) = (96, 96);
        let npx = w * h;
        let mut state = 0x12345678u64;
        let mut rng = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as f64 / (1u64 << 31) as f64) - 1.0
        };
        let mut normal = move || {
            let (mut u, mut v);
            loop {
                u = rng();
                v = rng();
                let s = u * u + v * v;
                if s > 1e-12 && s < 1.0 {
                    return u * (-2.0 * s.ln() / s).sqrt();
                }
            }
        };
        let residual: Vec<f32> = (0..npx).map(|_| normal() as f32).collect();
        let mask = build_frozen_mask(&residual, w, h, 12, &CrossFitConfig::default(), false);
        let false_rate = mask.len() as f64 / npx as f64;
        assert!(
            false_rate < 1e-4,
            "falso rechazo {false_rate:.2e} >= 1e-4 ({} px)",
            mask.len()
        );
    }

    #[test]
    fn test_frozen_mask_negative_residuals_are_tracked_separately() {
        let (w, h) = (16, 16);
        let mut residual = vec![0.0f32; w * h];
        residual[10] = -12.0;
        let cfg = CrossFitConfig::default();
        let mask = build_frozen_mask(&residual, w, h, 8, &cfg, false);
        assert!(mask.negative.contains(&10));
        assert!(mask.positive.is_empty());
    }
}
