//! Registro geométrico para paisajes nocturnos y Vía Láctea.
//!
//! El cielo y el suelo se resuelven como ramas independientes. El resultado
//! no contiene rásteres intermedios: sólo transformaciones source→output e
//! inversas evaluables. De este modo el consumidor compone máscara, registro y
//! encuadre antes de hacer una única interpolación por píxel publicado.

use std::{collections::BTreeMap, fmt};

pub const MILKY_WAY_REGISTRATION_SCHEMA: &str = "zenith-milky-way-registration-v1";

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point2 {
    pub x: f64,
    pub y: f64,
}

impl Point2 {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    fn finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }

    fn distance(self, other: Self) -> f64 {
        (self.x - other.x).hypot(self.y - other.y)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageGeometry {
    pub width: usize,
    pub height: usize,
}

impl ImageGeometry {
    pub fn new(width: usize, height: usize) -> Result<Self, RegistrationError> {
        let geometry = Self { width, height };
        geometry.validate()?;
        Ok(geometry)
    }

    pub fn validate(self) -> Result<(), RegistrationError> {
        if self.width < 2 || self.height < 2 {
            return Err(RegistrationError::InvalidGeometry(
                "la imagen debe medir al menos 2×2".into(),
            ));
        }
        self.width
            .checked_mul(self.height)
            .ok_or_else(|| RegistrationError::InvalidGeometry("dimensiones desbordadas".into()))?;
        Ok(())
    }

    fn corners(self) -> [Point2; 4] {
        let x = (self.width - 1) as f64;
        let y = (self.height - 1) as f64;
        [
            Point2::new(0.0, 0.0),
            Point2::new(x, 0.0),
            Point2::new(x, y),
            Point2::new(0.0, y),
        ]
    }

    fn area(self) -> f64 {
        (self.width - 1) as f64 * (self.height - 1) as f64
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RegistrationError {
    InvalidGeometry(String),
    InvalidImage(String),
    InvalidMask(String),
    InsufficientData { required: usize, available: usize },
    DegenerateModel(String),
    NoConsensus(String),
    InvalidPlan(String),
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidGeometry(message)
            | Self::InvalidImage(message)
            | Self::InvalidMask(message)
            | Self::DegenerateModel(message)
            | Self::NoConsensus(message)
            | Self::InvalidPlan(message) => formatter.write_str(message),
            Self::InsufficientData {
                required,
                available,
            } => write!(
                formatter,
                "se requieren {required} correspondencias y sólo hay {available}"
            ),
        }
    }
}

impl std::error::Error for RegistrationError {}

#[derive(Clone, Copy, Debug)]
pub struct MaskView<'a> {
    pub geometry: ImageGeometry,
    pub data: &'a [u8],
    /// Un píxel pertenece a la rama cuando su valor es mayor o igual a este
    /// umbral. Esto permite máscaras binarias o suaves ya rasterizadas.
    pub include_at_or_above: u8,
}

impl<'a> MaskView<'a> {
    pub fn validate(self) -> Result<(), RegistrationError> {
        self.geometry.validate()?;
        let expected = self.geometry.width * self.geometry.height;
        if self.data.len() != expected {
            return Err(RegistrationError::InvalidMask(format!(
                "la máscara contiene {} muestras; se esperaban {expected}",
                self.data.len()
            )));
        }
        Ok(())
    }

    #[inline]
    fn includes(self, x: usize, y: usize) -> bool {
        self.data[y * self.geometry.width + x] >= self.include_at_or_above
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StarFeature {
    pub position: Point2,
    pub flux: f64,
    pub snr: f64,
    pub fwhm_px: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StarDetectionConfig {
    pub threshold_sigma: f64,
    pub centroid_radius: usize,
    pub min_separation_px: f64,
    pub max_stars: usize,
    pub min_mask_fraction: f64,
}

impl Default for StarDetectionConfig {
    fn default() -> Self {
        Self {
            threshold_sigma: 5.0,
            centroid_radius: 2,
            min_separation_px: 4.0,
            max_stars: 600,
            min_mask_fraction: 0.65,
        }
    }
}

fn quantile(values: &mut Vec<f64>, q: f64) -> Option<f64> {
    values.retain(|value| value.is_finite());
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let position = q.clamp(0.0, 1.0) * (values.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let fraction = position - lower as f64;
    Some(values[lower] * (1.0 - fraction) + values[upper] * fraction)
}

fn masked_background_noise(
    image: &[f32],
    geometry: ImageGeometry,
    mask: MaskView<'_>,
) -> Result<(f64, f64), RegistrationError> {
    const MAX_SAMPLES: usize = 200_000;
    let included = mask
        .data
        .iter()
        .filter(|value| **value >= mask.include_at_or_above)
        .count();
    if included < 16 {
        return Err(RegistrationError::InvalidMask(
            "la máscara deja menos de 16 píxeles para estimar el fondo".into(),
        ));
    }
    let step = (included / MAX_SAMPLES).max(1);
    let mut seen = 0usize;
    let mut samples = Vec::with_capacity(included.min(MAX_SAMPLES));
    for y in 0..geometry.height {
        for x in 0..geometry.width {
            if !mask.includes(x, y) {
                continue;
            }
            if seen % step == 0 {
                let value = image[y * geometry.width + x] as f64;
                if value.is_finite() {
                    samples.push(value);
                }
            }
            seen += 1;
        }
    }
    let mut median_samples = samples.clone();
    let median = quantile(&mut median_samples, 0.5).ok_or_else(|| {
        RegistrationError::InvalidImage("la región permitida no contiene valores finitos".into())
    })?;
    let mut deviations = samples
        .iter()
        .map(|value| (value - median).abs())
        .collect::<Vec<_>>();
    let mad = quantile(&mut deviations, 0.5).unwrap_or(0.0);
    let mut spread_samples = samples;
    let p16 = quantile(&mut spread_samples, 0.16).unwrap_or(median);
    let mut spread_samples = median_samples;
    let p84 = quantile(&mut spread_samples, 0.84).unwrap_or(median);
    let robust_sigma = (mad * 1.482_602_218_505_602)
        .max((p84 - p16).abs() * 0.5)
        .max(f64::EPSILON * median.abs().max(1.0));
    Ok((median, robust_sigma))
}

/// Detecta estrellas exclusivamente dentro de `mask`. Los píxeles de suelo no
/// participan ni en el umbral robusto, ni en máximos locales, ni en centroides.
pub fn detect_stars_masked(
    image: &[f32],
    geometry: ImageGeometry,
    mask: MaskView<'_>,
    config: StarDetectionConfig,
) -> Result<Vec<StarFeature>, RegistrationError> {
    geometry.validate()?;
    mask.validate()?;
    if mask.geometry != geometry {
        return Err(RegistrationError::InvalidMask(
            "máscara e imagen tienen geometrías distintas".into(),
        ));
    }
    let expected = geometry.width * geometry.height;
    if image.len() != expected {
        return Err(RegistrationError::InvalidImage(format!(
            "la luminancia contiene {} muestras; se esperaban {expected}",
            image.len()
        )));
    }
    if !(config.threshold_sigma.is_finite() && config.threshold_sigma >= 2.0)
        || config.centroid_radius == 0
        || config.max_stars == 0
        || !config.min_separation_px.is_finite()
        || config.min_separation_px < 1.0
        || !(0.0..=1.0).contains(&config.min_mask_fraction)
    {
        return Err(RegistrationError::InvalidImage(
            "configuración de detección estelar inválida".into(),
        ));
    }

    let (background, noise) = masked_background_noise(image, geometry, mask)?;
    let threshold = background + config.threshold_sigma * noise;
    let radius = config.centroid_radius;
    let border = radius + 1;
    if geometry.width <= border * 2 || geometry.height <= border * 2 {
        return Err(RegistrationError::InvalidGeometry(
            "la imagen es demasiado pequeña para el radio de centroide".into(),
        ));
    }
    let side = radius * 2 + 1;
    let required_mask_samples = ((side * side) as f64 * config.min_mask_fraction).ceil() as usize;
    let mut candidates = Vec::new();
    for y in border..(geometry.height - border) {
        for x in border..(geometry.width - border) {
            if !mask.includes(x, y) {
                continue;
            }
            let center = image[y * geometry.width + x] as f64;
            if !center.is_finite() || center <= threshold {
                continue;
            }
            let mut local_maximum = true;
            'neighbours: for ny in (y - 1)..=(y + 1) {
                for nx in (x - 1)..=(x + 1) {
                    if nx == x && ny == y || !mask.includes(nx, ny) {
                        continue;
                    }
                    let neighbour = image[ny * geometry.width + nx] as f64;
                    if neighbour > center
                        || (neighbour == center && (ny < y || (ny == y && nx < x)))
                    {
                        local_maximum = false;
                        break 'neighbours;
                    }
                }
            }
            if !local_maximum {
                continue;
            }

            // Veto de saturación por meseta: un píxel saturado recorta el
            // perfil de la estrella en una meseta plana, y el desempate en
            // orden raster de arriba elige siempre la ESQUINA de esa meseta
            // como "pico". El centroide ponderado saldría sesgado 1-2 px
            // hacia esa esquina, y de forma distinta en cada frame (la
            // meseta cambia con el seeing y el jitter), así que las
            // estrellas más brillantes —las de mayor peso en el ajuste—
            // serían justamente las peor medidas. Una gaussiana bien
            // muestreada sólo tiene 1 píxel al >= 99.5% del pico (el propio
            // pico; 2 si el centro cae justo entre dos píxeles): 3 o más
            // delatan meseta y la estrella se EXCLUYE por completo en vez
            // de publicarse con un centroide falso.
            let plateau_level = center * 0.995;
            let mut plateau_pixels = 0usize;
            for ny in (y - radius)..=(y + radius) {
                for nx in (x - radius)..=(x + radius) {
                    if !mask.includes(nx, ny) {
                        continue;
                    }
                    let value = image[ny * geometry.width + nx] as f64;
                    if value.is_finite() && value >= plateau_level {
                        plateau_pixels += 1;
                    }
                }
            }
            if plateau_pixels >= 3 {
                continue;
            }

            let mut included = 0usize;
            let (mut flux, mut weighted_x, mut weighted_y) = (0.0, 0.0, 0.0);
            for ny in (y - radius)..=(y + radius) {
                for nx in (x - radius)..=(x + radius) {
                    if !mask.includes(nx, ny) {
                        continue;
                    }
                    included += 1;
                    let signal = (image[ny * geometry.width + nx] as f64 - background).max(0.0);
                    if signal.is_finite() {
                        flux += signal;
                        weighted_x += signal * nx as f64;
                        weighted_y += signal * ny as f64;
                    }
                }
            }
            if included < required_mask_samples || flux <= noise * 2.0 {
                continue;
            }
            let centroid = Point2::new(weighted_x / flux, weighted_y / flux);
            let (mut variance_x, mut variance_y) = (0.0, 0.0);
            for ny in (y - radius)..=(y + radius) {
                for nx in (x - radius)..=(x + radius) {
                    if !mask.includes(nx, ny) {
                        continue;
                    }
                    let signal = (image[ny * geometry.width + nx] as f64 - background).max(0.0);
                    variance_x += signal * (nx as f64 - centroid.x).powi(2);
                    variance_y += signal * (ny as f64 - centroid.y).powi(2);
                }
            }
            let sigma = ((variance_x + variance_y) / (2.0 * flux)).max(0.0).sqrt();
            candidates.push(StarFeature {
                position: centroid,
                flux,
                snr: flux / (noise * (included as f64).sqrt()).max(f64::EPSILON),
                fwhm_px: 2.354_820_045 * sigma,
            });
        }
    }
    candidates.sort_by(|left, right| {
        right
            .flux
            .total_cmp(&left.flux)
            .then_with(|| left.position.y.total_cmp(&right.position.y))
            .then_with(|| left.position.x.total_cmp(&right.position.x))
    });
    let minimum_distance_squared = config.min_separation_px * config.min_separation_px;
    let mut selected: Vec<StarFeature> = Vec::with_capacity(config.max_stars.min(candidates.len()));
    for candidate in candidates {
        if selected.iter().any(|other| {
            (other.position.x - candidate.position.x).powi(2)
                + (other.position.y - candidate.position.y).powi(2)
                < minimum_distance_squared
        }) {
            continue;
        }
        selected.push(candidate);
        if selected.len() == config.max_stars {
            break;
        }
    }
    Ok(selected)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Correspondence {
    /// Coordenada en el frame que será remuestreado.
    pub source: Point2,
    /// Coordenada homóloga en la geometría de salida/referencia.
    pub target: Point2,
    pub confidence: f64,
}

impl Correspondence {
    pub fn new(source: Point2, target: Point2, confidence: f64) -> Self {
        Self {
            source,
            target,
            confidence,
        }
    }

    fn valid(self) -> bool {
        self.source.finite()
            && self.target.finite()
            && self.confidence.is_finite()
            && self.confidence > 0.0
    }
}

/// Asociación biyectiva después de disponer de una transformación aproximada
/// (p. ej. una hipótesis triangular). Ningún target puede adjudicarse a dos
/// estrellas source.
pub fn match_stars_bijective(
    source: &[StarFeature],
    target: &[StarFeature],
    coarse_source_to_target: &TransformModel,
    max_distance_px: f64,
) -> Vec<Correspondence> {
    if !max_distance_px.is_finite() || max_distance_px <= 0.0 {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    for (source_index, star) in source.iter().enumerate() {
        let Some(projected) = coarse_source_to_target.apply(star.position) else {
            continue;
        };
        for (target_index, target_star) in target.iter().enumerate() {
            let distance = projected.distance(target_star.position);
            if distance <= max_distance_px {
                candidates.push((distance, source_index, target_index));
            }
        }
    }
    candidates.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    let mut source_used = vec![false; source.len()];
    let mut target_used = vec![false; target.len()];
    let mut matches = Vec::new();
    for (distance, source_index, target_index) in candidates {
        if source_used[source_index] || target_used[target_index] {
            continue;
        }
        source_used[source_index] = true;
        target_used[target_index] = true;
        let flux_ratio = (source[source_index].flux.max(f64::EPSILON)
            / target[target_index].flux.max(f64::EPSILON))
        .ln()
        .abs();
        matches.push(Correspondence::new(
            source[source_index].position,
            target[target_index].position,
            (-0.5 * (distance / max_distance_px).powi(2)).exp() * (-0.1 * flux_ratio).exp(),
        ));
    }
    matches
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadialDistortion {
    pub center: Point2,
    /// Radio de normalización, normalmente media diagonal del sensor. Los
    /// coeficientes k1/k2 quedan así independientes de la resolución.
    pub normalization_radius: f64,
    pub k1: f64,
    pub k2: f64,
}

impl RadialDistortion {
    pub fn validate(self) -> Result<(), RegistrationError> {
        if !self.center.finite()
            || !self.normalization_radius.is_finite()
            || self.normalization_radius <= 0.0
            || !self.k1.is_finite()
            || !self.k2.is_finite()
        {
            return Err(RegistrationError::DegenerateModel(
                "modelo radial no finito o sin radio de normalización".into(),
            ));
        }
        Ok(())
    }

    pub fn apply(self, point: Point2) -> Option<Point2> {
        self.validate().ok()?;
        let x = (point.x - self.center.x) / self.normalization_radius;
        let y = (point.y - self.center.y) / self.normalization_radius;
        let r2 = x * x + y * y;
        let factor = 1.0 + self.k1 * r2 + self.k2 * r2 * r2;
        let mapped = Point2::new(
            self.center.x + x * factor * self.normalization_radius,
            self.center.y + y * factor * self.normalization_radius,
        );
        mapped.finite().then_some(mapped)
    }

    pub fn inverse(self, distorted: Point2) -> Option<Point2> {
        self.validate().ok()?;
        let dx = (distorted.x - self.center.x) / self.normalization_radius;
        let dy = (distorted.y - self.center.y) / self.normalization_radius;
        let distorted_radius = dx.hypot(dy);
        if distorted_radius <= 1.0e-15 {
            return Some(self.center);
        }
        let mut radius = distorted_radius;
        for _ in 0..12 {
            let r2 = radius * radius;
            let r4 = r2 * r2;
            let value = radius * (1.0 + self.k1 * r2 + self.k2 * r4) - distorted_radius;
            let derivative = 1.0 + 3.0 * self.k1 * r2 + 5.0 * self.k2 * r4;
            if !derivative.is_finite() || derivative.abs() <= 1.0e-12 {
                return None;
            }
            let step = value / derivative;
            radius -= step;
            if !radius.is_finite() || radius < 0.0 {
                return None;
            }
            if step.abs() <= 1.0e-12 {
                break;
            }
        }
        let ratio = radius / distorted_radius;
        let point = Point2::new(
            self.center.x + dx * ratio * self.normalization_radius,
            self.center.y + dy * ratio * self.normalization_radius,
        );
        point.finite().then_some(point)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlanarTransform {
    /// [a, b, tx, c, d, ty]
    Affine([f64; 6]),
    /// Matriz 3×3 row-major, source→target.
    Homography([f64; 9]),
}

impl PlanarTransform {
    pub const fn identity() -> Self {
        Self::Affine([1.0, 0.0, 0.0, 0.0, 1.0, 0.0])
    }

    pub fn apply(self, point: Point2) -> Option<Point2> {
        let mapped = match self {
            Self::Affine(matrix) => Point2::new(
                matrix[0] * point.x + matrix[1] * point.y + matrix[2],
                matrix[3] * point.x + matrix[4] * point.y + matrix[5],
            ),
            Self::Homography(matrix) => {
                let denominator = matrix[6] * point.x + matrix[7] * point.y + matrix[8];
                if !denominator.is_finite() || denominator.abs() <= 1.0e-14 {
                    return None;
                }
                Point2::new(
                    (matrix[0] * point.x + matrix[1] * point.y + matrix[2]) / denominator,
                    (matrix[3] * point.x + matrix[4] * point.y + matrix[5]) / denominator,
                )
            }
        };
        mapped.finite().then_some(mapped)
    }

    pub fn inverse(self) -> Option<Self> {
        match self {
            Self::Affine(matrix) => {
                let determinant = matrix[0] * matrix[4] - matrix[1] * matrix[3];
                if !determinant.is_finite() || determinant.abs() <= 1.0e-14 {
                    return None;
                }
                let a = matrix[4] / determinant;
                let b = -matrix[1] / determinant;
                let c = -matrix[3] / determinant;
                let d = matrix[0] / determinant;
                Some(Self::Affine([
                    a,
                    b,
                    -(a * matrix[2] + b * matrix[5]),
                    c,
                    d,
                    -(c * matrix[2] + d * matrix[5]),
                ]))
            }
            Self::Homography(matrix) => invert_3x3(matrix).map(Self::Homography),
        }
    }

    /// Devuelve una afín exacta, incluso si llegó representada como
    /// homografía con tercera fila [0, 0, s].
    pub fn exact_affine(self, epsilon: f64) -> Option<[f64; 6]> {
        match self {
            Self::Affine(matrix) => Some(matrix),
            Self::Homography(matrix)
                if matrix[6].abs() <= epsilon
                    && matrix[7].abs() <= epsilon
                    && matrix[8].abs() > epsilon =>
            {
                let scale = matrix[8];
                Some([
                    matrix[0] / scale,
                    matrix[1] / scale,
                    matrix[2] / scale,
                    matrix[3] / scale,
                    matrix[4] / scale,
                    matrix[5] / scale,
                ])
            }
            Self::Homography(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransformModel {
    pub planar: PlanarTransform,
    /// Se aplica en source antes de la afín/homografía.
    pub radial: Option<RadialDistortion>,
}

impl TransformModel {
    pub const fn identity() -> Self {
        Self {
            planar: PlanarTransform::identity(),
            radial: None,
        }
    }

    pub fn apply(self, source: Point2) -> Option<Point2> {
        let source = match self.radial {
            Some(radial) => radial.apply(source)?,
            None => source,
        };
        self.planar.apply(source)
    }

    /// Output→source para que el consumidor haga inverse mapping y una sola
    /// interpolación. La inversión radial se resuelve analíticamente en radio
    /// mediante Newton; no crea un frame intermedio “undistorted”.
    pub fn inverse(self, output: Point2) -> Option<Point2> {
        let planar_inverse = self.planar.inverse()?;
        let distorted_source = planar_inverse.apply(output)?;
        match self.radial {
            Some(radial) => radial.inverse(distorted_source),
            None => Some(distorted_source),
        }
    }
}

fn matrix_multiply(left: [f64; 9], right: [f64; 9]) -> [f64; 9] {
    let mut output = [0.0; 9];
    for row in 0..3 {
        for column in 0..3 {
            output[row * 3 + column] = (0..3)
                .map(|index| left[row * 3 + index] * right[index * 3 + column])
                .sum();
        }
    }
    output
}

fn invert_3x3(matrix: [f64; 9]) -> Option<[f64; 9]> {
    let determinant = matrix[0] * (matrix[4] * matrix[8] - matrix[5] * matrix[7])
        - matrix[1] * (matrix[3] * matrix[8] - matrix[5] * matrix[6])
        + matrix[2] * (matrix[3] * matrix[7] - matrix[4] * matrix[6]);
    if !determinant.is_finite() || determinant.abs() <= 1.0e-14 {
        return None;
    }
    let inverse = [
        (matrix[4] * matrix[8] - matrix[5] * matrix[7]) / determinant,
        (matrix[2] * matrix[7] - matrix[1] * matrix[8]) / determinant,
        (matrix[1] * matrix[5] - matrix[2] * matrix[4]) / determinant,
        (matrix[5] * matrix[6] - matrix[3] * matrix[8]) / determinant,
        (matrix[0] * matrix[8] - matrix[2] * matrix[6]) / determinant,
        (matrix[2] * matrix[3] - matrix[0] * matrix[5]) / determinant,
        (matrix[3] * matrix[7] - matrix[4] * matrix[6]) / determinant,
        (matrix[1] * matrix[6] - matrix[0] * matrix[7]) / determinant,
        (matrix[0] * matrix[4] - matrix[1] * matrix[3]) / determinant,
    ];
    inverse
        .iter()
        .all(|value| value.is_finite())
        .then_some(inverse)
}

fn solve_linear_system(mut matrix: Vec<Vec<f64>>, mut rhs: Vec<f64>) -> Option<Vec<f64>> {
    let size = rhs.len();
    if matrix.len() != size || matrix.iter().any(|row| row.len() != size) {
        return None;
    }
    for column in 0..size {
        let pivot = (column..size).max_by(|left, right| {
            matrix[*left][column]
                .abs()
                .total_cmp(&matrix[*right][column].abs())
        })?;
        let column_scale = (column..size)
            .map(|row| matrix[row][column].abs())
            .fold(0.0, f64::max);
        if column_scale <= 1.0e-14 || matrix[pivot][column].abs() <= column_scale * 1.0e-12 {
            return None;
        }
        matrix.swap(column, pivot);
        rhs.swap(column, pivot);
        let divisor = matrix[column][column];
        for value in &mut matrix[column][column..] {
            *value /= divisor;
        }
        rhs[column] /= divisor;
        for row in 0..size {
            if row == column {
                continue;
            }
            let factor = matrix[row][column];
            if factor == 0.0 {
                continue;
            }
            for index in column..size {
                matrix[row][index] -= factor * matrix[column][index];
            }
            rhs[row] -= factor * rhs[column];
        }
    }
    rhs.iter().all(|value| value.is_finite()).then_some(rhs)
}

fn weighted_least_squares(rows: &[Vec<f64>], rhs: &[f64], weights: &[f64]) -> Option<Vec<f64>> {
    let parameters = rows.first()?.len();
    if rows.len() != rhs.len()
        || rows.len() != weights.len()
        || rows.iter().any(|row| row.len() != parameters)
    {
        return None;
    }
    let mut normal = vec![vec![0.0; parameters]; parameters];
    let mut projected = vec![0.0; parameters];
    for ((row, target), weight) in rows.iter().zip(rhs).zip(weights) {
        if !weight.is_finite() || *weight <= 0.0 {
            continue;
        }
        for left in 0..parameters {
            projected[left] += weight * row[left] * target;
            for right in 0..parameters {
                normal[left][right] += weight * row[left] * row[right];
            }
        }
    }
    // Regularización sólo a escala de redondeo: estabiliza normal equations
    // sin convertir configuraciones geométricamente degeneradas en válidas.
    let trace = (0..parameters)
        .map(|index| normal[index][index])
        .sum::<f64>();
    let ridge = trace.abs().max(1.0) * 1.0e-14;
    for index in 0..parameters {
        normal[index][index] += ridge;
    }
    solve_linear_system(normal, projected)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationModelSelection {
    Affine,
    Homography,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadialSearchConfig {
    pub enabled: bool,
    pub center: Point2,
    pub normalization_radius: f64,
    pub max_abs_k1: f64,
    pub max_abs_k2: f64,
    /// Se fuerza a impar y al menos 3 para que cero siempre sea candidato.
    pub grid_steps: usize,
    pub refinement_rounds: usize,
}

impl RadialSearchConfig {
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            center: Point2::new(0.0, 0.0),
            normalization_radius: 1.0,
            max_abs_k1: 0.0,
            max_abs_k2: 0.0,
            grid_steps: 3,
            refinement_rounds: 0,
        }
    }

    pub fn for_ultra_wide(source: ImageGeometry) -> Self {
        let width = source.width as f64;
        let height = source.height as f64;
        Self {
            enabled: true,
            center: Point2::new(
                (source.width - 1) as f64 * 0.5,
                (source.height - 1) as f64 * 0.5,
            ),
            normalization_radius: width.hypot(height) * 0.5,
            max_abs_k1: 0.24,
            max_abs_k2: 0.08,
            grid_steps: 5,
            refinement_rounds: 1,
        }
    }

    fn validate(self) -> Result<(), RegistrationError> {
        if !self.enabled {
            return Ok(());
        }
        if !self.center.finite()
            || !self.normalization_radius.is_finite()
            || self.normalization_radius <= 0.0
            || !self.max_abs_k1.is_finite()
            || !self.max_abs_k2.is_finite()
            || self.max_abs_k1 < 0.0
            || self.max_abs_k2 < 0.0
            || self.grid_steps < 3
        {
            return Err(RegistrationError::DegenerateModel(
                "búsqueda radial inválida".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ValidationLimits {
    pub min_inliers: usize,
    pub min_inlier_ratio: f64,
    pub max_rms_px: f64,
    pub max_p95_px: f64,
    pub min_source_coverage: f64,
    pub corner_margin_fraction: f64,
    pub min_corner_area_ratio: f64,
    pub max_corner_area_ratio: f64,
    pub min_abs_jacobian: f64,
    pub allow_reflection: bool,
}

impl Default for ValidationLimits {
    fn default() -> Self {
        Self {
            min_inliers: 10,
            min_inlier_ratio: 0.50,
            max_rms_px: 1.8,
            max_p95_px: 3.0,
            min_source_coverage: 0.08,
            corner_margin_fraction: 0.35,
            min_corner_area_ratio: 0.20,
            max_corner_area_ratio: 5.0,
            min_abs_jacobian: 1.0e-4,
            allow_reflection: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RegistrationConfig {
    pub model: RegistrationModelSelection,
    pub ransac_iterations: usize,
    pub inlier_threshold_px: f64,
    pub auto_homography_improvement: f64,
    pub radial: RadialSearchConfig,
    pub validation: ValidationLimits,
}

impl Default for RegistrationConfig {
    fn default() -> Self {
        Self {
            model: RegistrationModelSelection::Auto,
            ransac_iterations: 512,
            inlier_threshold_px: 2.5,
            auto_homography_improvement: 0.15,
            radial: RadialSearchConfig::disabled(),
            validation: ValidationLimits::default(),
        }
    }
}

impl RegistrationConfig {
    fn validate(self) -> Result<(), RegistrationError> {
        let numeric_limits = [
            self.validation.min_inlier_ratio,
            self.validation.max_rms_px,
            self.validation.max_p95_px,
            self.validation.min_source_coverage,
            self.validation.corner_margin_fraction,
            self.validation.min_corner_area_ratio,
            self.validation.max_corner_area_ratio,
            self.validation.min_abs_jacobian,
        ];
        if self.ransac_iterations == 0
            || !self.inlier_threshold_px.is_finite()
            || self.inlier_threshold_px <= 0.0
            || !self.auto_homography_improvement.is_finite()
            || !(0.0..0.75).contains(&self.auto_homography_improvement)
            || self.validation.min_inliers == 0
            || !numeric_limits.iter().all(|value| value.is_finite())
            || !(0.0..=1.0).contains(&self.validation.min_inlier_ratio)
            || self.validation.max_rms_px <= 0.0
            || self.validation.max_p95_px <= 0.0
            || !(0.0..=1.0).contains(&self.validation.min_source_coverage)
            || self.validation.corner_margin_fraction < 0.0
            || self.validation.min_corner_area_ratio <= 0.0
            || self.validation.max_corner_area_ratio < self.validation.min_corner_area_ratio
            || self.validation.min_abs_jacobian <= 0.0
        {
            return Err(RegistrationError::DegenerateModel(
                "configuración RANSAC/validación inválida".into(),
            ));
        }
        self.radial.validate()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RegistrationValidation {
    pub accepted: bool,
    pub reasons: Vec<String>,
    pub inliers: usize,
    pub inlier_ratio: f64,
    pub rms_px: f64,
    pub p95_px: f64,
    pub max_residual_px: f64,
    pub source_coverage: f64,
    pub mapped_corners: [Point2; 4],
    pub corner_area_ratio: f64,
    pub jacobian_min_abs: f64,
    pub jacobian_max_abs: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransformSolution {
    pub transform: TransformModel,
    pub inlier_indices: Vec<usize>,
    pub validation: RegistrationValidation,
}

fn fit_affine(
    correspondences: &[Correspondence],
    indices: &[usize],
    robust_weights: Option<&[f64]>,
) -> Option<PlanarTransform> {
    if indices.len() < 3 {
        return None;
    }
    let mut rows = Vec::with_capacity(indices.len() * 2);
    let mut rhs = Vec::with_capacity(indices.len() * 2);
    let mut weights = Vec::with_capacity(indices.len() * 2);
    for &index in indices {
        let pair = *correspondences.get(index)?;
        let robust = robust_weights
            .and_then(|values| values.get(index))
            .copied()
            .unwrap_or(1.0);
        let weight = pair.confidence * robust;
        rows.push(vec![pair.source.x, pair.source.y, 1.0, 0.0, 0.0, 0.0]);
        rhs.push(pair.target.x);
        weights.push(weight);
        rows.push(vec![0.0, 0.0, 0.0, pair.source.x, pair.source.y, 1.0]);
        rhs.push(pair.target.y);
        weights.push(weight);
    }
    let parameters = weighted_least_squares(&rows, &rhs, &weights)?;
    let matrix = [
        parameters[0],
        parameters[1],
        parameters[2],
        parameters[3],
        parameters[4],
        parameters[5],
    ];
    let determinant = matrix[0] * matrix[4] - matrix[1] * matrix[3];
    (matrix.iter().all(|value| value.is_finite()) && determinant.abs() > 1.0e-12)
        .then_some(PlanarTransform::Affine(matrix))
}

#[derive(Clone, Copy)]
struct PointNormalization {
    center: Point2,
    scale: f64,
}

impl PointNormalization {
    fn from_points(points: &[Point2], weights: &[f64]) -> Option<Self> {
        let total = weights.iter().copied().sum::<f64>();
        if points.len() != weights.len() || !total.is_finite() || total <= 0.0 {
            return None;
        }
        let center = Point2::new(
            points
                .iter()
                .zip(weights)
                .map(|(point, weight)| point.x * weight)
                .sum::<f64>()
                / total,
            points
                .iter()
                .zip(weights)
                .map(|(point, weight)| point.y * weight)
                .sum::<f64>()
                / total,
        );
        let mean_distance = points
            .iter()
            .zip(weights)
            .map(|(point, weight)| point.distance(center) * weight)
            .sum::<f64>()
            / total;
        if !mean_distance.is_finite() || mean_distance <= 1.0e-9 {
            return None;
        }
        Some(Self {
            center,
            scale: std::f64::consts::SQRT_2 / mean_distance,
        })
    }

    fn apply(self, point: Point2) -> Point2 {
        Point2::new(
            (point.x - self.center.x) * self.scale,
            (point.y - self.center.y) * self.scale,
        )
    }

    fn matrix(self) -> [f64; 9] {
        [
            self.scale,
            0.0,
            -self.center.x * self.scale,
            0.0,
            self.scale,
            -self.center.y * self.scale,
            0.0,
            0.0,
            1.0,
        ]
    }

    fn inverse_matrix(self) -> [f64; 9] {
        [
            1.0 / self.scale,
            0.0,
            self.center.x,
            0.0,
            1.0 / self.scale,
            self.center.y,
            0.0,
            0.0,
            1.0,
        ]
    }
}

fn fit_homography(
    correspondences: &[Correspondence],
    indices: &[usize],
    robust_weights: Option<&[f64]>,
) -> Option<PlanarTransform> {
    if indices.len() < 4 {
        return None;
    }
    let mut sources = Vec::with_capacity(indices.len());
    let mut targets = Vec::with_capacity(indices.len());
    let mut point_weights = Vec::with_capacity(indices.len());
    for &index in indices {
        let pair = *correspondences.get(index)?;
        let robust = robust_weights
            .and_then(|values| values.get(index))
            .copied()
            .unwrap_or(1.0);
        sources.push(pair.source);
        targets.push(pair.target);
        point_weights.push(pair.confidence * robust);
    }
    let source_normalization = PointNormalization::from_points(&sources, &point_weights)?;
    let target_normalization = PointNormalization::from_points(&targets, &point_weights)?;
    let mut rows = Vec::with_capacity(indices.len() * 2);
    let mut rhs = Vec::with_capacity(indices.len() * 2);
    let mut weights = Vec::with_capacity(indices.len() * 2);
    for ((source, target), weight) in sources.iter().zip(&targets).zip(&point_weights) {
        let source = source_normalization.apply(*source);
        let target = target_normalization.apply(*target);
        rows.push(vec![
            source.x,
            source.y,
            1.0,
            0.0,
            0.0,
            0.0,
            -target.x * source.x,
            -target.x * source.y,
        ]);
        rhs.push(target.x);
        weights.push(*weight);
        rows.push(vec![
            0.0,
            0.0,
            0.0,
            source.x,
            source.y,
            1.0,
            -target.y * source.x,
            -target.y * source.y,
        ]);
        rhs.push(target.y);
        weights.push(*weight);
    }
    let parameters = weighted_least_squares(&rows, &rhs, &weights)?;
    let normalized = [
        parameters[0],
        parameters[1],
        parameters[2],
        parameters[3],
        parameters[4],
        parameters[5],
        parameters[6],
        parameters[7],
        1.0,
    ];
    let denormalized = matrix_multiply(
        target_normalization.inverse_matrix(),
        matrix_multiply(normalized, source_normalization.matrix()),
    );
    if !denormalized.iter().all(|value| value.is_finite()) || denormalized[8].abs() <= 1.0e-14 {
        return None;
    }
    let scale = denormalized[8];
    let matrix = denormalized.map(|value| value / scale);
    invert_3x3(matrix).map(|_| PlanarTransform::Homography(matrix))
}

fn deterministic_sample(population: usize, count: usize, iteration: usize) -> Option<Vec<usize>> {
    if count > population || count == 0 {
        return None;
    }
    if iteration == 0 {
        return Some((0..count).collect());
    }
    let mut state = 0x9e37_79b9_7f4a_7c15u64
        ^ (iteration as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9)
        ^ (population as u64).rotate_left(17);
    let mut sample = Vec::with_capacity(count);
    let mut attempts = 0usize;
    while sample.len() < count && attempts < population * count * 8 {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
        let index = (state % population as u64) as usize;
        if !sample.contains(&index) {
            sample.push(index);
        }
        attempts += 1;
    }
    (sample.len() == count).then_some(sample)
}

fn residuals_and_inliers(
    transform: TransformModel,
    correspondences: &[Correspondence],
    threshold: f64,
) -> (Vec<f64>, Vec<usize>, f64) {
    let mut residuals = Vec::with_capacity(correspondences.len());
    let mut inliers = Vec::new();
    let mut weighted_support = 0.0;
    for (index, pair) in correspondences.iter().enumerate() {
        let residual = transform
            .apply(pair.source)
            .map(|mapped| mapped.distance(pair.target))
            .unwrap_or(f64::INFINITY);
        residuals.push(residual);
        if residual <= threshold {
            inliers.push(index);
            weighted_support += pair.confidence;
        }
    }
    (residuals, inliers, weighted_support)
}

fn rms_for_indices(residuals: &[f64], indices: &[usize]) -> f64 {
    if indices.is_empty() {
        return f64::INFINITY;
    }
    (indices
        .iter()
        .filter_map(|index| residuals.get(*index))
        .map(|value| value * value)
        .sum::<f64>()
        / indices.len() as f64)
        .sqrt()
}

fn fit_planar(
    model: RegistrationModelSelection,
    correspondences: &[Correspondence],
    indices: &[usize],
    robust_weights: Option<&[f64]>,
) -> Option<PlanarTransform> {
    match model {
        RegistrationModelSelection::Affine => fit_affine(correspondences, indices, robust_weights),
        RegistrationModelSelection::Homography => {
            fit_homography(correspondences, indices, robust_weights)
        }
        RegistrationModelSelection::Auto => None,
    }
}

fn model_minimum(model: RegistrationModelSelection) -> usize {
    match model {
        RegistrationModelSelection::Affine => 3,
        RegistrationModelSelection::Homography => 4,
        RegistrationModelSelection::Auto => 4,
    }
}

fn apply_radial_to_correspondences(
    correspondences: &[Correspondence],
    radial: Option<RadialDistortion>,
) -> Option<Vec<Correspondence>> {
    correspondences
        .iter()
        .map(|pair| {
            Some(Correspondence {
                source: match radial {
                    Some(radial) => radial.apply(pair.source)?,
                    None => pair.source,
                },
                target: pair.target,
                confidence: pair.confidence,
            })
        })
        .collect()
}

fn percentile_sorted(values: &mut [f64], quantile: f64) -> f64 {
    if values.is_empty() {
        return f64::INFINITY;
    }
    values.sort_by(f64::total_cmp);
    let index = ((values.len() - 1) as f64 * quantile.clamp(0.0, 1.0)).round() as usize;
    values[index]
}

fn polygon_signed_area(points: &[Point2; 4]) -> f64 {
    (0..4)
        .map(|index| {
            let next = (index + 1) % 4;
            points[index].x * points[next].y - points[next].x * points[index].y
        })
        .sum::<f64>()
        * 0.5
}

fn numeric_jacobian(transform: TransformModel, point: Point2) -> Option<f64> {
    let epsilon = 0.25;
    let left = transform.apply(Point2::new(point.x - epsilon, point.y))?;
    let right = transform.apply(Point2::new(point.x + epsilon, point.y))?;
    let top = transform.apply(Point2::new(point.x, point.y - epsilon))?;
    let bottom = transform.apply(Point2::new(point.x, point.y + epsilon))?;
    let du_dx = (right.x - left.x) / (2.0 * epsilon);
    let dv_dx = (right.y - left.y) / (2.0 * epsilon);
    let du_dy = (bottom.x - top.x) / (2.0 * epsilon);
    let dv_dy = (bottom.y - top.y) / (2.0 * epsilon);
    let determinant = du_dx * dv_dy - du_dy * dv_dx;
    determinant.is_finite().then_some(determinant)
}

pub fn validate_transform(
    transform: TransformModel,
    correspondences: &[Correspondence],
    inlier_indices: &[usize],
    source_geometry: ImageGeometry,
    target_geometry: ImageGeometry,
    limits: ValidationLimits,
) -> RegistrationValidation {
    let mut reasons = Vec::new();
    let mut inlier_residuals = inlier_indices
        .iter()
        .filter_map(|index| {
            let pair = correspondences.get(*index)?;
            Some(
                transform
                    .apply(pair.source)
                    .map(|point| point.distance(pair.target))
                    .unwrap_or(f64::INFINITY),
            )
        })
        .collect::<Vec<_>>();
    let rms = if inlier_residuals.is_empty() {
        f64::INFINITY
    } else {
        (inlier_residuals
            .iter()
            .map(|residual| residual * residual)
            .sum::<f64>()
            / inlier_residuals.len() as f64)
            .sqrt()
    };
    let maximum = inlier_residuals.iter().copied().fold(0.0f64, f64::max);
    let p95 = percentile_sorted(&mut inlier_residuals, 0.95);
    let ratio = inlier_indices.len() as f64 / correspondences.len().max(1) as f64;
    if inlier_indices.len() < limits.min_inliers {
        reasons.push(format!(
            "inliers insuficientes: {} < {}",
            inlier_indices.len(),
            limits.min_inliers
        ));
    }
    if ratio < limits.min_inlier_ratio {
        reasons.push(format!(
            "proporción de inliers {:.3} < {:.3}",
            ratio, limits.min_inlier_ratio
        ));
    }
    if !rms.is_finite() || rms > limits.max_rms_px {
        reasons.push(format!("RMS {rms:.3} px excede {:.3}", limits.max_rms_px));
    }
    if !p95.is_finite() || p95 > limits.max_p95_px {
        reasons.push(format!("P95 {p95:.3} px excede {:.3}", limits.max_p95_px));
    }

    let inlier_points = inlier_indices
        .iter()
        .filter_map(|index| correspondences.get(*index).map(|pair| pair.source))
        .collect::<Vec<_>>();
    let source_coverage = if inlier_points.is_empty() {
        0.0
    } else {
        let min_x = inlier_points
            .iter()
            .map(|point| point.x)
            .fold(f64::INFINITY, f64::min);
        let max_x = inlier_points
            .iter()
            .map(|point| point.x)
            .fold(f64::NEG_INFINITY, f64::max);
        let min_y = inlier_points
            .iter()
            .map(|point| point.y)
            .fold(f64::INFINITY, f64::min);
        let max_y = inlier_points
            .iter()
            .map(|point| point.y)
            .fold(f64::NEG_INFINITY, f64::max);
        ((max_x - min_x).max(0.0) * (max_y - min_y).max(0.0)) / source_geometry.area()
    };
    if !source_coverage.is_finite() || source_coverage < limits.min_source_coverage {
        reasons.push(format!(
            "cobertura espacial {:.3} < {:.3}",
            source_coverage, limits.min_source_coverage
        ));
    }

    let source_corners = source_geometry.corners();
    let mut mapped_corners = [Point2::new(f64::NAN, f64::NAN); 4];
    let mut corners_valid = true;
    for (destination, source) in mapped_corners.iter_mut().zip(source_corners) {
        if let Some(mapped) = transform.apply(source) {
            *destination = mapped;
        } else {
            corners_valid = false;
        }
    }
    let target_area = target_geometry.area().max(1.0);
    let corner_signed_area = if corners_valid {
        polygon_signed_area(&mapped_corners)
    } else {
        f64::NAN
    };
    let corner_area_ratio = corner_signed_area.abs() / target_area;
    if !corners_valid || !corner_area_ratio.is_finite() {
        reasons.push("alguna esquina no puede proyectarse".into());
    } else {
        if !limits.allow_reflection && corner_signed_area <= 0.0 {
            reasons.push("la transformación refleja la geometría".into());
        }
        if corner_area_ratio < limits.min_corner_area_ratio
            || corner_area_ratio > limits.max_corner_area_ratio
        {
            reasons.push(format!(
                "área de esquinas {:.3} fuera de [{:.3}, {:.3}]",
                corner_area_ratio, limits.min_corner_area_ratio, limits.max_corner_area_ratio
            ));
        }
        let margin_x = target_geometry.width as f64 * limits.corner_margin_fraction;
        let margin_y = target_geometry.height as f64 * limits.corner_margin_fraction;
        if mapped_corners.iter().any(|corner| {
            corner.x < -margin_x
                || corner.y < -margin_y
                || corner.x > target_geometry.width as f64 - 1.0 + margin_x
                || corner.y > target_geometry.height as f64 - 1.0 + margin_y
        }) {
            reasons.push("las esquinas extrapolan fuera del margen seguro".into());
        }
    }

    let mut sample_points = source_corners.to_vec();
    sample_points.push(Point2::new(
        (source_geometry.width - 1) as f64 * 0.5,
        (source_geometry.height - 1) as f64 * 0.5,
    ));
    let jacobians = sample_points
        .iter()
        .filter_map(|point| numeric_jacobian(transform, *point))
        .collect::<Vec<_>>();
    let jacobian_min_abs = jacobians
        .iter()
        .map(|value| value.abs())
        .fold(f64::INFINITY, f64::min);
    let jacobian_max_abs = jacobians
        .iter()
        .map(|value| value.abs())
        .fold(0.0, f64::max);
    if jacobians.len() != sample_points.len()
        || !jacobian_min_abs.is_finite()
        || jacobian_min_abs < limits.min_abs_jacobian
    {
        reasons.push("Jacobiano nulo/no finito en centro o esquinas".into());
    }
    if !limits.allow_reflection && jacobians.iter().any(|value| *value <= 0.0) {
        reasons.push("la orientación/Jacobiano cambia de signo".into());
    } else if let Some(sign) = jacobians.first().map(|value| value.signum()) {
        if jacobians.iter().any(|value| value.signum() != sign) {
            reasons.push("la transformación se pliega dentro del campo".into());
        }
    }

    RegistrationValidation {
        accepted: reasons.is_empty(),
        reasons,
        inliers: inlier_indices.len(),
        inlier_ratio: ratio,
        rms_px: rms,
        p95_px: p95,
        max_residual_px: maximum,
        source_coverage,
        mapped_corners,
        corner_area_ratio,
        jacobian_min_abs,
        jacobian_max_abs,
    }
}

#[derive(Clone)]
struct ConsensusCandidate {
    transform: TransformModel,
    inliers: Vec<usize>,
    weighted_support: f64,
    rms: f64,
}

fn consensus_better(candidate: &ConsensusCandidate, current: Option<&ConsensusCandidate>) -> bool {
    let Some(current) = current else {
        return true;
    };
    candidate
        .inliers
        .len()
        .cmp(&current.inliers.len())
        .then_with(|| {
            candidate
                .weighted_support
                .total_cmp(&current.weighted_support)
        })
        .then_with(|| current.rms.total_cmp(&candidate.rms))
        .is_gt()
}

fn estimate_consensus_for_model(
    original_correspondences: &[Correspondence],
    model: RegistrationModelSelection,
    radial: Option<RadialDistortion>,
    source_geometry: ImageGeometry,
    target_geometry: ImageGeometry,
    config: RegistrationConfig,
) -> Result<TransformSolution, RegistrationError> {
    let adjusted =
        apply_radial_to_correspondences(original_correspondences, radial).ok_or_else(|| {
            RegistrationError::DegenerateModel("la distorsión radial no es invertible".into())
        })?;
    let minimum = model_minimum(model);
    if adjusted.len() < minimum {
        return Err(RegistrationError::InsufficientData {
            required: minimum,
            available: adjusted.len(),
        });
    }
    let mut best: Option<ConsensusCandidate> = None;
    for iteration in 0..config.ransac_iterations {
        let Some(sample) = deterministic_sample(adjusted.len(), minimum, iteration) else {
            continue;
        };
        let Some(planar) = fit_planar(model, &adjusted, &sample, None) else {
            continue;
        };
        let transform = TransformModel { planar, radial };
        let (residuals, inliers, weighted_support) = residuals_and_inliers(
            transform,
            original_correspondences,
            config.inlier_threshold_px,
        );
        if inliers.len() < minimum {
            continue;
        }
        let candidate = ConsensusCandidate {
            transform,
            rms: rms_for_indices(&residuals, &inliers),
            inliers,
            weighted_support,
        };
        if consensus_better(&candidate, best.as_ref()) {
            best = Some(candidate);
        }
    }
    let mut best = best.ok_or_else(|| {
        RegistrationError::NoConsensus(format!(
            "RANSAC no encontró un consenso {}",
            match model {
                RegistrationModelSelection::Affine => "afín",
                RegistrationModelSelection::Homography => "proyectivo",
                RegistrationModelSelection::Auto => "válido",
            }
        ))
    })?;

    // IRLS determinista sobre el consenso. Se ajusta el modelo planar en las
    // coordenadas ya corregidas radialmente, pero los residuos se miden siempre
    // contra el mapping completo source→target.
    for _ in 0..3 {
        let mut robust_weights = vec![0.0; adjusted.len()];
        for &index in &best.inliers {
            let residual = best
                .transform
                .apply(original_correspondences[index].source)
                .map(|point| point.distance(original_correspondences[index].target))
                .unwrap_or(f64::INFINITY);
            let normalized = residual / (config.inlier_threshold_px * 1.5);
            robust_weights[index] = if normalized < 1.0 {
                (1.0 - normalized * normalized).powi(2)
            } else {
                0.0
            };
        }
        let Some(planar) = fit_planar(model, &adjusted, &best.inliers, Some(&robust_weights))
        else {
            break;
        };
        let transform = TransformModel { planar, radial };
        let (residuals, inliers, weighted_support) = residuals_and_inliers(
            transform,
            original_correspondences,
            config.inlier_threshold_px,
        );
        if inliers.len() < minimum {
            break;
        }
        best = ConsensusCandidate {
            transform,
            rms: rms_for_indices(&residuals, &inliers),
            inliers,
            weighted_support,
        };
    }
    let validation = validate_transform(
        best.transform,
        original_correspondences,
        &best.inliers,
        source_geometry,
        target_geometry,
        config.validation,
    );
    Ok(TransformSolution {
        transform: best.transform,
        inlier_indices: best.inliers,
        validation,
    })
}

fn radial_grid(center: f64, half_range: f64, steps: usize) -> Vec<f64> {
    if half_range <= 0.0 {
        return vec![center];
    }
    let steps = steps.max(3) | 1;
    (0..steps)
        .map(|index| center - half_range + 2.0 * half_range * index as f64 / (steps - 1) as f64)
        .collect()
}

fn solution_better(candidate: &TransformSolution, current: Option<&TransformSolution>) -> bool {
    let Some(current) = current else {
        return true;
    };
    candidate
        .validation
        .accepted
        .cmp(&current.validation.accepted)
        .then_with(|| {
            candidate
                .validation
                .inliers
                .cmp(&current.validation.inliers)
        })
        .then_with(|| {
            current
                .validation
                .rms_px
                .total_cmp(&candidate.validation.rms_px)
        })
        .then_with(|| {
            let candidate_penalty = candidate
                .transform
                .radial
                .map(|radial| radial.k1.abs() + radial.k2.abs())
                .unwrap_or(0.0);
            let current_penalty = current
                .transform
                .radial
                .map(|radial| radial.k1.abs() + radial.k2.abs())
                .unwrap_or(0.0);
            current_penalty.total_cmp(&candidate_penalty)
        })
        .is_gt()
}

fn estimate_model_with_radial_search(
    correspondences: &[Correspondence],
    model: RegistrationModelSelection,
    source_geometry: ImageGeometry,
    target_geometry: ImageGeometry,
    config: RegistrationConfig,
) -> Result<TransformSolution, RegistrationError> {
    if !config.radial.enabled {
        return estimate_consensus_for_model(
            correspondences,
            model,
            None,
            source_geometry,
            target_geometry,
            config,
        );
    }
    let mut best: Option<TransformSolution> = None;
    let mut center_k1 = 0.0;
    let mut center_k2 = 0.0;
    let mut range_k1 = config.radial.max_abs_k1;
    let mut range_k2 = config.radial.max_abs_k2;
    for round in 0..=config.radial.refinement_rounds {
        for k1 in radial_grid(center_k1, range_k1, config.radial.grid_steps) {
            for k2 in radial_grid(center_k2, range_k2, config.radial.grid_steps) {
                let radial = RadialDistortion {
                    center: config.radial.center,
                    normalization_radius: config.radial.normalization_radius,
                    k1,
                    k2,
                };
                let Ok(solution) = estimate_consensus_for_model(
                    correspondences,
                    model,
                    Some(radial),
                    source_geometry,
                    target_geometry,
                    config,
                ) else {
                    continue;
                };
                if solution_better(&solution, best.as_ref()) {
                    best = Some(solution);
                }
            }
        }
        let Some(current) = best.as_ref() else {
            continue;
        };
        let radial = current.transform.radial.unwrap_or(RadialDistortion {
            center: config.radial.center,
            normalization_radius: config.radial.normalization_radius,
            k1: 0.0,
            k2: 0.0,
        });
        center_k1 = radial.k1;
        center_k2 = radial.k2;
        if round < config.radial.refinement_rounds {
            range_k1 /= (config.radial.grid_steps - 1).max(2) as f64;
            range_k2 /= (config.radial.grid_steps - 1).max(2) as f64;
        }
    }
    best.ok_or_else(|| RegistrationError::NoConsensus("ningún modelo radial superó RANSAC".into()))
}

/// Estima una afín/homografía robusta. `Auto` conserva la afín salvo que la
/// homografía sea necesaria: evita pagar dos grados proyectivos por una mejora
/// cosmética marginal.
pub fn estimate_registration(
    correspondences: &[Correspondence],
    source_geometry: ImageGeometry,
    target_geometry: ImageGeometry,
    config: RegistrationConfig,
) -> Result<TransformSolution, RegistrationError> {
    source_geometry.validate()?;
    target_geometry.validate()?;
    config.validate()?;
    let valid = correspondences
        .iter()
        .copied()
        .filter(|pair| pair.valid())
        .collect::<Vec<_>>();
    if valid.len() != correspondences.len() {
        return Err(RegistrationError::InvalidImage(
            "hay correspondencias NaN/Inf o con confianza no positiva".into(),
        ));
    }
    match config.model {
        RegistrationModelSelection::Affine | RegistrationModelSelection::Homography => {
            estimate_model_with_radial_search(
                correspondences,
                config.model,
                source_geometry,
                target_geometry,
                config,
            )
        }
        RegistrationModelSelection::Auto => {
            let affine = estimate_model_with_radial_search(
                correspondences,
                RegistrationModelSelection::Affine,
                source_geometry,
                target_geometry,
                config,
            );
            let homography = estimate_model_with_radial_search(
                correspondences,
                RegistrationModelSelection::Homography,
                source_geometry,
                target_geometry,
                config,
            );
            match (affine, homography) {
                (Ok(affine), Ok(homography)) => {
                    let homography_materially_better = !affine.validation.accepted
                        || (homography.validation.accepted
                            && homography.validation.inliers >= affine.validation.inliers
                            && homography.validation.rms_px
                                <= affine.validation.rms_px
                                    * (1.0 - config.auto_homography_improvement));
                    Ok(if homography_materially_better {
                        homography
                    } else {
                        affine
                    })
                }
                (Ok(affine), Err(_)) => Ok(affine),
                (Err(_), Ok(homography)) => Ok(homography),
                (Err(affine_error), Err(homography_error)) => Err(RegistrationError::NoConsensus(
                    format!("afín: {affine_error}; homografía: {homography_error}"),
                )),
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SkyImageRegistrationConfig {
    pub detection: StarDetectionConfig,
    pub registration: RegistrationConfig,
    /// Sólo las estrellas más informativas participan en los descriptores
    /// invariantes. Una vez hallada la hipótesis, se vuelven a asociar todas.
    pub descriptor_stars: usize,
    pub descriptor_neighbours: usize,
    pub descriptor_tolerance: f64,
    pub rematch_radius_px: f64,
}

impl Default for SkyImageRegistrationConfig {
    fn default() -> Self {
        Self {
            detection: StarDetectionConfig::default(),
            registration: RegistrationConfig::default(),
            descriptor_stars: 80,
            descriptor_neighbours: 6,
            descriptor_tolerance: 0.075,
            rematch_radius_px: 3.5,
        }
    }
}

impl SkyImageRegistrationConfig {
    fn validate(self) -> Result<(), RegistrationError> {
        self.registration.validate()?;
        if self.descriptor_stars < 8
            || self.descriptor_neighbours < 3
            || self.descriptor_neighbours >= self.descriptor_stars
            || !self.descriptor_tolerance.is_finite()
            || self.descriptor_tolerance <= 0.0
            || !self.rematch_radius_px.is_finite()
            || self.rematch_radius_px <= 0.0
        {
            return Err(RegistrationError::DegenerateModel(
                "configuración de asociación estelar inválida".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SkyRegistrationFallback {
    None,
    /// Los descriptores no alcanzaron consenso, pero un pico de traslación
    /// permitió volver a asociar las estrellas y validar el modelo completo.
    TranslationSeed,
    /// La transformación se conserva sólo como candidata y no puede publicarse.
    RejectedCandidate {
        reason: String,
    },
    /// No hubo evidencia suficiente; identidad evita inventar movimiento.
    Identity {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct SkyRegistrationResult {
    /// Siempre transforma el frame `target_luma` hacia `reference_luma`.
    pub transform: TransformModel,
    pub detected_reference_stars: usize,
    pub detected_target_stars: usize,
    pub matched_correspondences: usize,
    pub inlier_correspondences: Vec<Correspondence>,
    pub rms_px: f64,
    pub confidence: f64,
    pub accepted: bool,
    pub validation: RegistrationValidation,
    pub fallback: SkyRegistrationFallback,
}

#[derive(Clone)]
struct LocalStarDescriptor {
    star_index: usize,
    signature: Vec<f64>,
}

fn local_star_descriptors(
    stars: &[StarFeature],
    maximum_stars: usize,
    neighbours: usize,
) -> Vec<LocalStarDescriptor> {
    let count = stars.len().min(maximum_stars);
    let stars = &stars[..count];
    if stars.len() <= neighbours {
        return Vec::new();
    }
    stars
        .iter()
        .enumerate()
        .filter_map(|(star_index, star)| {
            let mut distances = stars
                .iter()
                .enumerate()
                .filter(|(other_index, _)| *other_index != star_index)
                .map(|(_, other)| star.position.distance(other.position))
                .filter(|distance| distance.is_finite() && *distance > 1.0e-6)
                .collect::<Vec<_>>();
            distances.sort_by(f64::total_cmp);
            distances.truncate(neighbours);
            if distances.len() != neighbours {
                return None;
            }
            let scale = *distances.last()?;
            if scale <= 1.0e-6 {
                return None;
            }
            Some(LocalStarDescriptor {
                star_index,
                signature: distances
                    .into_iter()
                    .map(|distance| distance / scale)
                    .collect(),
            })
        })
        .collect()
}

/// RMS entre dos firmas de la misma longitud, sin tolerancia a omisiones.
fn signature_rms(left: &[f64], right: &[f64]) -> f64 {
    if left.len() != right.len() || left.is_empty() {
        return f64::INFINITY;
    }
    (left
        .iter()
        .zip(right)
        .map(|(left, right)| (left - right).powi(2))
        .sum::<f64>()
        / left.len() as f64)
        .sqrt()
}

/// Mejor coste de alineamiento tipo edit-distance-1 cuando el frame de
/// `shifted` perdió EXACTAMENTE un vecino respecto al de `complete`.
///
/// Física del fallo: la firma es el vector ordenado de distancias a los N
/// vecinos más próximos. Si un vecino tenue cae bajo el umbral en un frame
/// (rutinario, porque el umbral es por frame), esa firma pierde el elemento
/// correspondiente y el (N+1)-ésimo vecino entra a ocupar la ÚLTIMA plaza
/// (las distancias van ordenadas). Por tanto el alineamiento correcto omite
/// un elemento i de `complete` (el vecino perdido) y el último de `shifted`
/// (el intruso). Como el descriptor original normaliza por la distancia
/// N-ésima y la omisión cambia esa escala, cada subfirma de N-1 elementos se
/// renormaliza por su nueva mayor distancia antes del RMS. Determinista:
/// recorre todas las posiciones de omisión y se queda el mínimo.
fn best_skip_alignment(complete: &[f64], shifted: &[f64]) -> f64 {
    let count = complete.len();
    if count < 3 || shifted.len() != count {
        return f64::INFINITY;
    }
    // `shifted` sin su último elemento (el vecino intruso), renormalizado
    // por la que pasa a ser su mayor distancia.
    let shifted_scale = shifted[count - 2];
    if !(shifted_scale.is_finite() && shifted_scale > 1.0e-9) {
        return f64::INFINITY;
    }
    let truncated = shifted[..count - 1]
        .iter()
        .map(|value| value / shifted_scale)
        .collect::<Vec<_>>();
    let mut best = f64::INFINITY;
    let mut reduced: Vec<f64> = Vec::with_capacity(count - 1);
    for skip in 0..count {
        reduced.clear();
        for (index, value) in complete.iter().enumerate() {
            if index != skip {
                reduced.push(*value);
            }
        }
        let Some(scale) = reduced.last().copied() else {
            continue;
        };
        if !(scale.is_finite() && scale > 1.0e-9) {
            continue;
        }
        for value in &mut reduced {
            *value /= scale;
        }
        let cost = signature_rms(&reduced, &truncated);
        if cost < best {
            best = cost;
        }
    }
    best
}

fn descriptor_distance(left: &LocalStarDescriptor, right: &LocalStarDescriptor) -> f64 {
    if left.signature.len() != right.signature.len() || left.signature.is_empty() {
        return f64::INFINITY;
    }
    // Caso nominal: los mismos N vecinos sobrevivieron en ambos frames.
    let direct = signature_rms(&left.signature, &right.signature);
    // Tolerancia a UNA omisión: si un vecino tenue falta en uno de los dos
    // frames, la firma entera se corre y el alineamiento directo se
    // descorrela por completo (la sonda quedaría huérfana bajo mutual-best).
    // Se prueba "saltar un elemento en left" y "saltar uno en right" y se
    // conserva el mejor de los tres costes. La construcción es simétrica
    // (distance(a,b) == distance(b,a)), sin RNG, así que el criterio
    // mutual-best del resto del pipeline sigue siendo válido tal cual.
    let skip_in_left = best_skip_alignment(&left.signature, &right.signature);
    let skip_in_right = best_skip_alignment(&right.signature, &left.signature);
    direct.min(skip_in_left).min(skip_in_right)
}

fn match_stars_by_local_geometry(
    target_stars: &[StarFeature],
    reference_stars: &[StarFeature],
    config: SkyImageRegistrationConfig,
) -> Vec<Correspondence> {
    let sources = local_star_descriptors(
        target_stars,
        config.descriptor_stars,
        config.descriptor_neighbours,
    );
    let targets = local_star_descriptors(
        reference_stars,
        config.descriptor_stars,
        config.descriptor_neighbours,
    );
    if sources.is_empty() || targets.is_empty() {
        return Vec::new();
    }
    let source_best = sources
        .iter()
        .map(|source| {
            targets
                .iter()
                .enumerate()
                .map(|(index, target)| (descriptor_distance(source, target), index))
                .min_by(|left, right| left.0.total_cmp(&right.0))
        })
        .collect::<Vec<_>>();
    let target_best = targets
        .iter()
        .map(|target| {
            sources
                .iter()
                .enumerate()
                .map(|(index, source)| (descriptor_distance(source, target), index))
                .min_by(|left, right| left.0.total_cmp(&right.0))
        })
        .collect::<Vec<_>>();
    let mut matches = Vec::new();
    for (source_descriptor_index, best) in source_best.into_iter().enumerate() {
        let Some((distance, target_descriptor_index)) = best else {
            continue;
        };
        if distance > config.descriptor_tolerance
            || target_best[target_descriptor_index].map(|(_, source_index)| source_index)
                != Some(source_descriptor_index)
        {
            continue;
        }
        let source = &sources[source_descriptor_index];
        let target = &targets[target_descriptor_index];
        matches.push(Correspondence::new(
            target_stars[source.star_index].position,
            reference_stars[target.star_index].position,
            (-0.5 * (distance / config.descriptor_tolerance).powi(2))
                .exp()
                .max(0.05),
        ));
    }
    matches
}

fn translation_seed(
    target_stars: &[StarFeature],
    reference_stars: &[StarFeature],
    bin_width: f64,
) -> Option<TransformModel> {
    let mut bins: BTreeMap<(i64, i64), (usize, f64, f64)> = BTreeMap::new();
    for source in target_stars.iter().take(80) {
        for target in reference_stars.iter().take(80) {
            let dx = target.position.x - source.position.x;
            let dy = target.position.y - source.position.y;
            let key = (
                (dx / bin_width).round() as i64,
                (dy / bin_width).round() as i64,
            );
            let entry = bins.entry(key).or_insert((0, 0.0, 0.0));
            entry.0 += 1;
            entry.1 += dx;
            entry.2 += dy;
        }
    }
    let (_, (count, sum_x, sum_y)) = bins.into_iter().max_by(|left, right| {
        left.1
             .0
            .cmp(&right.1 .0)
            .then_with(|| left.0.cmp(&right.0))
    })?;
    if count < 3 {
        return None;
    }
    Some(TransformModel {
        planar: PlanarTransform::Affine([
            1.0,
            0.0,
            sum_x / count as f64,
            0.0,
            1.0,
            sum_y / count as f64,
        ]),
        radial: None,
    })
}

fn registration_confidence(validation: &RegistrationValidation) -> f64 {
    if !validation.accepted {
        return 0.0;
    }
    let residual_score = (1.0 / (1.0 + validation.rms_px.max(0.0))).clamp(0.0, 1.0);
    let coverage_score = (validation.source_coverage / 0.35).clamp(0.0, 1.0);
    (0.55 * validation.inlier_ratio + 0.30 * residual_score + 0.15 * coverage_score).clamp(0.0, 1.0)
}

fn high_level_sky_result(
    solution: TransformSolution,
    correspondences: Vec<Correspondence>,
    detected_reference_stars: usize,
    detected_target_stars: usize,
    fallback: SkyRegistrationFallback,
) -> SkyRegistrationResult {
    let inlier_correspondences = solution
        .inlier_indices
        .iter()
        .filter_map(|index| correspondences.get(*index).copied())
        .collect::<Vec<_>>();
    SkyRegistrationResult {
        transform: solution.transform,
        detected_reference_stars,
        detected_target_stars,
        matched_correspondences: correspondences.len(),
        inlier_correspondences,
        rms_px: solution.validation.rms_px,
        confidence: registration_confidence(&solution.validation),
        accepted: solution.validation.accepted,
        validation: solution.validation,
        fallback,
    }
}

/// API simple para backend. Detecta y asocia estrellas sin usar el suelo. La
/// transformación devuelta siempre es target→reference.
pub fn solve_sky_registration(
    reference_luma: &[f32],
    target_luma: &[f32],
    geometry: ImageGeometry,
    sky_mask: MaskView<'_>,
) -> Result<SkyRegistrationResult, RegistrationError> {
    solve_sky_registration_with_config(
        reference_luma,
        target_luma,
        geometry,
        sky_mask,
        SkyImageRegistrationConfig::default(),
    )
}

pub fn solve_sky_registration_with_config(
    reference_luma: &[f32],
    target_luma: &[f32],
    geometry: ImageGeometry,
    sky_mask: MaskView<'_>,
    config: SkyImageRegistrationConfig,
) -> Result<SkyRegistrationResult, RegistrationError> {
    config.validate()?;
    let reference_stars =
        detect_stars_masked(reference_luma, geometry, sky_mask, config.detection)?;
    let target_stars = detect_stars_masked(target_luma, geometry, sky_mask, config.detection)?;
    let minimum = match config.registration.model {
        RegistrationModelSelection::Affine => 3,
        RegistrationModelSelection::Homography | RegistrationModelSelection::Auto => 4,
    };
    if reference_stars.len() < minimum || target_stars.len() < minimum {
        let empty = Vec::new();
        let validation = validate_transform(
            TransformModel::identity(),
            &empty,
            &[],
            geometry,
            geometry,
            config.registration.validation,
        );
        return Ok(SkyRegistrationResult {
            transform: TransformModel::identity(),
            detected_reference_stars: reference_stars.len(),
            detected_target_stars: target_stars.len(),
            matched_correspondences: 0,
            inlier_correspondences: Vec::new(),
            rms_px: f64::INFINITY,
            confidence: 0.0,
            accepted: false,
            validation,
            fallback: SkyRegistrationFallback::Identity {
                reason: "estrellas insuficientes dentro de la máscara de cielo".into(),
            },
        });
    }

    let descriptor_pairs = match_stars_by_local_geometry(&target_stars, &reference_stars, config);
    let mut candidates: Vec<(
        TransformSolution,
        Vec<Correspondence>,
        SkyRegistrationFallback,
    )> = Vec::new();
    if descriptor_pairs.len() >= minimum {
        if let Ok(initial) =
            estimate_registration(&descriptor_pairs, geometry, geometry, config.registration)
        {
            let rematched = match_stars_bijective(
                &target_stars,
                &reference_stars,
                &initial.transform,
                config.rematch_radius_px,
            );
            let refined = if rematched.len() >= minimum {
                estimate_registration(&rematched, geometry, geometry, config.registration)
                    .ok()
                    .map(|solution| (solution, rematched))
            } else {
                None
            };
            let (solution, pairs) = refined.unwrap_or((initial, descriptor_pairs));
            candidates.push((solution, pairs, SkyRegistrationFallback::None));
        }
    }

    if let Some(seed) = translation_seed(
        &target_stars,
        &reference_stars,
        config.registration.inlier_threshold_px.max(2.0),
    ) {
        let rematched = match_stars_bijective(
            &target_stars,
            &reference_stars,
            &seed,
            config
                .rematch_radius_px
                .max(config.registration.inlier_threshold_px * 1.5),
        );
        if rematched.len() >= minimum {
            if let Ok(solution) =
                estimate_registration(&rematched, geometry, geometry, config.registration)
            {
                candidates.push((
                    solution,
                    rematched,
                    SkyRegistrationFallback::TranslationSeed,
                ));
            }
        }
    }

    candidates.sort_by(|left, right| {
        right
            .0
            .validation
            .accepted
            .cmp(&left.0.validation.accepted)
            .then_with(|| right.0.validation.inliers.cmp(&left.0.validation.inliers))
            .then_with(|| {
                left.0
                    .validation
                    .rms_px
                    .total_cmp(&right.0.validation.rms_px)
            })
    });
    if let Some((solution, pairs, fallback)) = candidates.into_iter().next() {
        let fallback = if solution.validation.accepted {
            fallback
        } else {
            SkyRegistrationFallback::RejectedCandidate {
                reason: solution.validation.reasons.join("; "),
            }
        };
        return Ok(high_level_sky_result(
            solution,
            pairs,
            reference_stars.len(),
            target_stars.len(),
            fallback,
        ));
    }

    let empty = Vec::new();
    let validation = validate_transform(
        TransformModel::identity(),
        &empty,
        &[],
        geometry,
        geometry,
        config.registration.validation,
    );
    Ok(SkyRegistrationResult {
        transform: TransformModel::identity(),
        detected_reference_stars: reference_stars.len(),
        detected_target_stars: target_stars.len(),
        matched_correspondences: 0,
        inlier_correspondences: Vec::new(),
        rms_px: f64::INFINITY,
        confidence: 0.0,
        accepted: false,
        validation,
        fallback: SkyRegistrationFallback::Identity {
            reason: "sin consenso geométrico seguro dentro de la máscara de cielo".into(),
        },
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroundImageRegistrationConfig {
    pub maximum_pyramid_levels: usize,
    pub coarse_radius_px: i32,
    pub refinement_radius_px: i32,
    pub minimum_samples: usize,
    pub maximum_samples_per_candidate: usize,
    pub minimum_ncc: f64,
    pub minimum_peak_margin: f64,
}

impl Default for GroundImageRegistrationConfig {
    fn default() -> Self {
        Self {
            maximum_pyramid_levels: 7,
            coarse_radius_px: 14,
            refinement_radius_px: 3,
            minimum_samples: 128,
            maximum_samples_per_candidate: 160_000,
            minimum_ncc: 0.55,
            minimum_peak_margin: 0.006,
        }
    }
}

impl GroundImageRegistrationConfig {
    fn validate(self) -> Result<(), RegistrationError> {
        if self.maximum_pyramid_levels == 0
            || self.coarse_radius_px < 1
            || self.refinement_radius_px < 1
            || self.minimum_samples < 16
            || self.maximum_samples_per_candidate < self.minimum_samples
            || !self.minimum_ncc.is_finite()
            || !(-1.0..=1.0).contains(&self.minimum_ncc)
            || !self.minimum_peak_margin.is_finite()
            || !(0.0..=2.0).contains(&self.minimum_peak_margin)
        {
            return Err(RegistrationError::DegenerateModel(
                "configuración NCC de suelo inválida".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum GroundRegistrationFallback {
    None,
    Identity { reason: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct GroundImageRegistrationResult {
    /// Transformación efectiva target→reference. Si la confianza no supera el
    /// contrato es identidad y `candidate_transform` conserva el diagnóstico.
    pub transform: TransformModel,
    pub candidate_transform: Option<TransformModel>,
    pub ncc_score: f64,
    pub peak_margin: f64,
    pub confidence: f64,
    pub samples: usize,
    pub accepted: bool,
    pub fallback: GroundRegistrationFallback,
}

#[derive(Clone)]
struct MaskedCorrelationPlane {
    geometry: ImageGeometry,
    values: Vec<f32>,
    valid: Vec<bool>,
}

impl MaskedCorrelationPlane {
    fn from_image(
        image: &[f32],
        geometry: ImageGeometry,
        mask: MaskView<'_>,
    ) -> Result<Self, RegistrationError> {
        geometry.validate()?;
        mask.validate()?;
        if mask.geometry != geometry {
            return Err(RegistrationError::InvalidMask(
                "máscara de suelo e imagen tienen geometrías distintas".into(),
            ));
        }
        let expected = geometry.width * geometry.height;
        if image.len() != expected {
            return Err(RegistrationError::InvalidImage(format!(
                "la luminancia de suelo contiene {} muestras; se esperaban {expected}",
                image.len()
            )));
        }
        let include_at_or_above = mask.include_at_or_above;
        let valid = image
            .iter()
            .zip(mask.data)
            .map(|(value, mask_value)| value.is_finite() && *mask_value >= include_at_or_above)
            .collect::<Vec<_>>();
        Ok(Self {
            geometry,
            values: image.to_vec(),
            valid,
        })
    }

    fn downsample_half(&self) -> Option<Self> {
        let width = self.geometry.width / 2;
        let height = self.geometry.height / 2;
        if width < 2 || height < 2 {
            return None;
        }
        let geometry = ImageGeometry { width, height };
        let mut values = vec![0.0f32; width * height];
        let mut valid = vec![false; width * height];
        for output_y in 0..height {
            for output_x in 0..width {
                let mut sum = 0.0f64;
                let mut count = 0usize;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let source_x = output_x * 2 + dx;
                        let source_y = output_y * 2 + dy;
                        let index = source_y * self.geometry.width + source_x;
                        if self.valid[index] {
                            sum += self.values[index] as f64;
                            count += 1;
                        }
                    }
                }
                if count >= 2 {
                    let index = output_y * width + output_x;
                    values[index] = (sum / count as f64) as f32;
                    valid[index] = true;
                }
            }
        }
        Some(Self {
            geometry,
            values,
            valid,
        })
    }

    fn valid_count(&self) -> usize {
        self.valid.iter().filter(|valid| **valid).count()
    }
}

#[derive(Clone, Copy, Debug)]
struct NccCandidate {
    dx: i32,
    dy: i32,
    score: f64,
    samples: usize,
}

fn masked_ncc_translation(
    reference: &MaskedCorrelationPlane,
    target: &MaskedCorrelationPlane,
    dx: i32,
    dy: i32,
    config: GroundImageRegistrationConfig,
) -> Option<NccCandidate> {
    if reference.geometry != target.geometry {
        return None;
    }
    let geometry = reference.geometry;
    let potential = reference.valid_count().min(target.valid_count());
    let stride = ((potential as f64 / config.maximum_samples_per_candidate as f64)
        .sqrt()
        .ceil() as usize)
        .max(1);
    let mut count = 0usize;
    let (mut reference_sum, mut target_sum) = (0.0, 0.0);
    let (mut reference_square, mut target_square, mut cross) = (0.0, 0.0, 0.0);
    for target_y in (0..geometry.height).step_by(stride) {
        let reference_y = target_y as i64 + dy as i64;
        if reference_y < 0 || reference_y >= geometry.height as i64 {
            continue;
        }
        for target_x in (0..geometry.width).step_by(stride) {
            let reference_x = target_x as i64 + dx as i64;
            if reference_x < 0 || reference_x >= geometry.width as i64 {
                continue;
            }
            let target_index = target_y * geometry.width + target_x;
            let reference_index = reference_y as usize * geometry.width + reference_x as usize;
            if !target.valid[target_index] || !reference.valid[reference_index] {
                continue;
            }
            let left = reference.values[reference_index] as f64;
            let right = target.values[target_index] as f64;
            reference_sum += left;
            target_sum += right;
            reference_square += left * left;
            target_square += right * right;
            cross += left * right;
            count += 1;
        }
    }
    if count < config.minimum_samples.min(potential.max(1)) {
        return None;
    }
    let count_f64 = count as f64;
    let covariance = cross - reference_sum * target_sum / count_f64;
    let reference_variance = reference_square - reference_sum * reference_sum / count_f64;
    let target_variance = target_square - target_sum * target_sum / count_f64;
    let denominator = (reference_variance.max(0.0) * target_variance.max(0.0)).sqrt();
    if !denominator.is_finite() || denominator <= f64::EPSILON {
        return None;
    }
    let score = (covariance / denominator).clamp(-1.0, 1.0);
    score.is_finite().then_some(NccCandidate {
        dx,
        dy,
        score,
        samples: count,
    })
}

fn search_ncc_window(
    reference: &MaskedCorrelationPlane,
    target: &MaskedCorrelationPlane,
    center_x: i32,
    center_y: i32,
    radius: i32,
    maximum_shift: i32,
    config: GroundImageRegistrationConfig,
) -> Vec<NccCandidate> {
    let mut candidates = Vec::new();
    for dy in (center_y - radius).max(-maximum_shift)..=(center_y + radius).min(maximum_shift) {
        for dx in (center_x - radius).max(-maximum_shift)..=(center_x + radius).min(maximum_shift) {
            if let Some(candidate) = masked_ncc_translation(reference, target, dx, dy, config) {
                candidates.push(candidate);
            }
        }
    }
    candidates.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| right.samples.cmp(&left.samples))
            .then_with(|| left.dy.cmp(&right.dy))
            .then_with(|| left.dx.cmp(&right.dx))
    });
    candidates
}

fn parabolic_peak(left: f64, center: f64, right: f64) -> f64 {
    let denominator = left - 2.0 * center + right;
    if !denominator.is_finite() || denominator.abs() <= 1.0e-12 {
        return 0.0;
    }
    (0.5 * (left - right) / denominator).clamp(-0.5, 0.5)
}

/// Registro nativo de paisaje por NCC en pirámide, restringido a la máscara.
/// Sólo estima traslación: para afín/homografía de suelo use las
/// correspondencias de `estimate_registration` y mantenga la rama separada.
pub fn solve_ground_registration(
    reference_luma: &[f32],
    target_luma: &[f32],
    geometry: ImageGeometry,
    ground_mask: MaskView<'_>,
    max_shift_px: f64,
) -> Result<GroundImageRegistrationResult, RegistrationError> {
    solve_ground_registration_with_config(
        reference_luma,
        target_luma,
        geometry,
        ground_mask,
        max_shift_px,
        GroundImageRegistrationConfig::default(),
    )
}

pub fn solve_ground_registration_with_config(
    reference_luma: &[f32],
    target_luma: &[f32],
    geometry: ImageGeometry,
    ground_mask: MaskView<'_>,
    max_shift_px: f64,
    config: GroundImageRegistrationConfig,
) -> Result<GroundImageRegistrationResult, RegistrationError> {
    config.validate()?;
    if !max_shift_px.is_finite() || max_shift_px < 0.0 {
        return Err(RegistrationError::DegenerateModel(
            "el desplazamiento máximo de suelo debe ser finito y no negativo".into(),
        ));
    }
    let reference = MaskedCorrelationPlane::from_image(reference_luma, geometry, ground_mask)?;
    let target = MaskedCorrelationPlane::from_image(target_luma, geometry, ground_mask)?;
    if reference.valid_count() < config.minimum_samples
        || target.valid_count() < config.minimum_samples
    {
        return Ok(GroundImageRegistrationResult {
            transform: TransformModel::identity(),
            candidate_transform: None,
            ncc_score: f64::NEG_INFINITY,
            peak_margin: 0.0,
            confidence: 0.0,
            samples: 0,
            accepted: false,
            fallback: GroundRegistrationFallback::Identity {
                reason: "la máscara de suelo no deja muestras suficientes".into(),
            },
        });
    }

    let mut reference_pyramid = vec![reference];
    let mut target_pyramid = vec![target];
    let mut scale = 1usize;
    while reference_pyramid.len() < config.maximum_pyramid_levels
        && max_shift_px / scale as f64 > config.coarse_radius_px as f64
    {
        let Some(reference_next) = reference_pyramid
            .last()
            .and_then(|plane| plane.downsample_half())
        else {
            break;
        };
        let Some(target_next) = target_pyramid
            .last()
            .and_then(|plane| plane.downsample_half())
        else {
            break;
        };
        if reference_next.valid_count() < config.minimum_samples
            || target_next.valid_count() < config.minimum_samples
        {
            break;
        }
        reference_pyramid.push(reference_next);
        target_pyramid.push(target_next);
        scale *= 2;
    }

    let mut best: Option<NccCandidate> = None;
    let mut final_candidates = Vec::new();
    for level in (0..reference_pyramid.len()).rev() {
        let level_scale = 1usize << level;
        let maximum_shift = (max_shift_px / level_scale as f64).ceil() as i32;
        let (center_x, center_y, radius) = match best {
            None => (0, 0, maximum_shift.min(config.coarse_radius_px)),
            Some(previous) => (
                previous.dx * 2,
                previous.dy * 2,
                config.refinement_radius_px,
            ),
        };
        let candidates = search_ncc_window(
            &reference_pyramid[level],
            &target_pyramid[level],
            center_x,
            center_y,
            radius,
            maximum_shift,
            config,
        );
        let Some(level_best) = candidates.first().copied() else {
            best = None;
            break;
        };
        best = Some(level_best);
        if level == 0 {
            final_candidates = candidates;
        }
    }

    let Some(best) = best else {
        return Ok(GroundImageRegistrationResult {
            transform: TransformModel::identity(),
            candidate_transform: None,
            ncc_score: f64::NEG_INFINITY,
            peak_margin: 0.0,
            confidence: 0.0,
            samples: 0,
            accepted: false,
            fallback: GroundRegistrationFallback::Identity {
                reason: "NCC no produjo un pico finito dentro del desplazamiento permitido".into(),
            },
        });
    };
    let runner_up = final_candidates
        .iter()
        .filter(|candidate| {
            (candidate.dx - best.dx).abs() > 1 || (candidate.dy - best.dy).abs() > 1
        })
        .map(|candidate| candidate.score)
        .fold(f64::NEG_INFINITY, f64::max);
    let peak_margin = if runner_up.is_finite() {
        (best.score - runner_up).max(0.0)
    } else {
        2.0
    };
    let score_at = |dx: i32, dy: i32| {
        final_candidates
            .iter()
            .find(|candidate| candidate.dx == dx && candidate.dy == dy)
            .map(|candidate| candidate.score)
            .unwrap_or(best.score)
    };
    let subpixel_x = parabolic_peak(
        score_at(best.dx - 1, best.dy),
        best.score,
        score_at(best.dx + 1, best.dy),
    );
    let subpixel_y = parabolic_peak(
        score_at(best.dx, best.dy - 1),
        best.score,
        score_at(best.dx, best.dy + 1),
    );
    let candidate_transform = TransformModel {
        planar: PlanarTransform::Affine([
            1.0,
            0.0,
            best.dx as f64 + subpixel_x,
            0.0,
            1.0,
            best.dy as f64 + subpixel_y,
        ]),
        radial: None,
    };
    let accepted = best.score >= config.minimum_ncc
        && peak_margin >= config.minimum_peak_margin
        && best.samples >= config.minimum_samples;
    let confidence = if accepted {
        let score: f64 = ((best.score - config.minimum_ncc)
            / (1.0 - config.minimum_ncc).max(f64::EPSILON))
        .clamp(0.0, 1.0);
        let margin: f64 =
            (peak_margin / (config.minimum_peak_margin * 4.0).max(f64::EPSILON)).clamp(0.0, 1.0);
        (0.75 * score + 0.25 * margin).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (transform, fallback) = if accepted {
        (candidate_transform, GroundRegistrationFallback::None)
    } else {
        (
            TransformModel::identity(),
            GroundRegistrationFallback::Identity {
                reason: format!(
                    "pico NCC no concluyente: score {:.3}, margen {:.4}, muestras {}",
                    best.score, peak_margin, best.samples
                ),
            },
        )
    };
    Ok(GroundImageRegistrationResult {
        transform,
        candidate_transform: Some(candidate_transform),
        ncc_score: best.score,
        peak_margin,
        confidence,
        samples: best.samples,
        accepted,
        fallback,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct LinearTanWcs {
    pub ctype1: String,
    pub ctype2: String,
    pub crval1: f64,
    pub crval2: f64,
    pub crpix1: f64,
    pub crpix2: f64,
    pub cd11: f64,
    pub cd12: f64,
    pub cd21: f64,
    pub cd22: f64,
    pub pixel_width: usize,
    pub pixel_height: usize,
}

impl LinearTanWcs {
    pub fn validate(&self) -> Result<(), RegistrationError> {
        if self.ctype1.trim().to_ascii_uppercase() != "RA---TAN"
            || self.ctype2.trim().to_ascii_uppercase() != "DEC--TAN"
        {
            return Err(RegistrationError::DegenerateModel(
                "sólo puede propagarse un WCS TAN lineal".into(),
            ));
        }
        let values = [
            self.crval1,
            self.crval2,
            self.crpix1,
            self.crpix2,
            self.cd11,
            self.cd12,
            self.cd21,
            self.cd22,
        ];
        if !values.iter().all(|value| value.is_finite())
            || !(0.0..360.0).contains(&self.crval1)
            || !(-90.0..=90.0).contains(&self.crval2)
            || self.pixel_width < 2
            || self.pixel_height < 2
        {
            return Err(RegistrationError::DegenerateModel(
                "WCS no finito, fuera de rango o sin geometría".into(),
            ));
        }
        let determinant = self.cd11 * self.cd22 - self.cd12 * self.cd21;
        if !determinant.is_finite() || determinant.abs() <= 1.0e-18 {
            return Err(RegistrationError::DegenerateModel(
                "matriz CD singular".into(),
            ));
        }
        Ok(())
    }

    pub fn fingerprint(&self) -> String {
        let canonical = format!(
            "{}x{}|{:.10}|{:.10}|{:.4}|{:.4}|{:.10E}|{:.10E}|{:.10E}|{:.10E}",
            self.pixel_width,
            self.pixel_height,
            self.crval1,
            self.crval2,
            self.crpix1,
            self.crpix2,
            self.cd11,
            self.cd12,
            self.cd21,
            self.cd22,
        );
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in canonical.bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("wcs-grid-{hash:016x}")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum WcsDisposition {
    Missing,
    Preserved {
        wcs: LinearTanWcs,
        source_fingerprint: String,
        output_fingerprint: String,
    },
    /// La semilla sigue siendo útil para resolver nuevamente contra catálogo,
    /// pero no debe escribirse como WCS de salida.
    Candidate {
        seed: LinearTanWcs,
        source_fingerprint: String,
        reason: String,
    },
    Invalidated {
        source_fingerprint: Option<String>,
        reason: String,
    },
}

pub fn propagate_wcs(
    input: Option<&LinearTanWcs>,
    sky_solution: &TransformSolution,
    source_geometry: ImageGeometry,
    output_geometry: ImageGeometry,
) -> WcsDisposition {
    let Some(input) = input else {
        return WcsDisposition::Missing;
    };
    let source_fingerprint = input.fingerprint();
    if let Err(error) = input.validate() {
        return WcsDisposition::Invalidated {
            source_fingerprint: Some(source_fingerprint),
            reason: format!("WCS fuente inválido: {error}"),
        };
    }
    if input.pixel_width != source_geometry.width || input.pixel_height != source_geometry.height {
        return WcsDisposition::Invalidated {
            source_fingerprint: Some(source_fingerprint),
            reason: format!(
                "la geometría WCS {}×{} no coincide con la fuente {}×{}",
                input.pixel_width,
                input.pixel_height,
                source_geometry.width,
                source_geometry.height
            ),
        };
    }
    if !sky_solution.validation.accepted {
        return WcsDisposition::Invalidated {
            source_fingerprint: Some(source_fingerprint),
            reason: "el registro de cielo no superó validación".into(),
        };
    }
    if sky_solution.transform.radial.is_some() {
        return WcsDisposition::Candidate {
            seed: input.clone(),
            source_fingerprint,
            reason: "la corrección radial no cabe exactamente en CD/TAN; vuelve a resolver sobre la salida"
                .into(),
        };
    }
    let Some(affine) = sky_solution.transform.planar.exact_affine(1.0e-12) else {
        return WcsDisposition::Candidate {
            seed: input.clone(),
            source_fingerprint,
            reason: "una homografía proyectiva no conserva un WCS CD/TAN lineal exacto".into(),
        };
    };
    let determinant = affine[0] * affine[4] - affine[1] * affine[3];
    if !determinant.is_finite() || determinant.abs() <= 1.0e-14 {
        return WcsDisposition::Invalidated {
            source_fingerprint: Some(source_fingerprint),
            reason: "la afín de cielo es singular".into(),
        };
    }
    // En coordenadas internas 0-based: output = A·source+t, por tanto
    // source = A⁻¹·(output-t) y CD_output = CD_source·A⁻¹. CRPIX es 1-based
    // por contrato FITS, así que se convierte antes y después de aplicar A.
    let inverse = [
        affine[4] / determinant,
        -affine[1] / determinant,
        -affine[3] / determinant,
        affine[0] / determinant,
    ];
    let input_crpix_zero_based = Point2::new(input.crpix1 - 1.0, input.crpix2 - 1.0);
    let output_crpix_zero_based = PlanarTransform::Affine(affine)
        .apply(input_crpix_zero_based)
        .expect("la afín ya fue validada como finita");
    let crpix1 = output_crpix_zero_based.x + 1.0;
    let crpix2 = output_crpix_zero_based.y + 1.0;
    let output = LinearTanWcs {
        ctype1: input.ctype1.clone(),
        ctype2: input.ctype2.clone(),
        crval1: input.crval1,
        crval2: input.crval2,
        crpix1,
        crpix2,
        cd11: input.cd11 * inverse[0] + input.cd12 * inverse[2],
        cd12: input.cd11 * inverse[1] + input.cd12 * inverse[3],
        cd21: input.cd21 * inverse[0] + input.cd22 * inverse[2],
        cd22: input.cd21 * inverse[1] + input.cd22 * inverse[3],
        pixel_width: output_geometry.width,
        pixel_height: output_geometry.height,
    };
    if let Err(error) = output.validate() {
        return WcsDisposition::Invalidated {
            source_fingerprint: Some(source_fingerprint),
            reason: format!("WCS derivado inválido: {error}"),
        };
    }
    let output_fingerprint = output.fingerprint();
    WcsDisposition::Preserved {
        wcs: output,
        source_fingerprint,
        output_fingerprint,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroundRegistrationMode {
    /// Trípode fijo: el paisaje define el encuadre y no se mueve.
    LockedIdentity,
    Affine,
    Homography,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GroundSolution {
    LockedIdentity,
    Registered(TransformSolution),
}

impl GroundSolution {
    pub fn transform(&self) -> TransformModel {
        match self {
            Self::LockedIdentity => TransformModel::identity(),
            Self::Registered(solution) => solution.transform,
        }
    }

    pub fn accepted(&self) -> bool {
        match self {
            Self::LockedIdentity => true,
            Self::Registered(solution) => solution.validation.accepted,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarpLayer {
    Sky,
    Ground,
}

/// Contrato por construcción de una sola interpolación. No admite un path a
/// un frame prewarpeado ni una cadena de resamplers: cada coordenada output se
/// retroproyecta directamente al source original de su rama.
#[derive(Clone, Debug, PartialEq)]
pub struct SingleResamplePlan {
    pub schema: &'static str,
    pub source_geometry: ImageGeometry,
    pub output_geometry: ImageGeometry,
    sky_source_to_output: TransformModel,
    ground_source_to_output: TransformModel,
    pub masks_sampled_in_source_space: bool,
}

impl SingleResamplePlan {
    fn new(
        source_geometry: ImageGeometry,
        output_geometry: ImageGeometry,
        sky_source_to_output: TransformModel,
        ground_source_to_output: TransformModel,
    ) -> Result<Self, RegistrationError> {
        source_geometry.validate()?;
        output_geometry.validate()?;
        let center = Point2::new(
            (output_geometry.width - 1) as f64 * 0.5,
            (output_geometry.height - 1) as f64 * 0.5,
        );
        if sky_source_to_output.inverse(center).is_none()
            || ground_source_to_output.inverse(center).is_none()
        {
            return Err(RegistrationError::InvalidPlan(
                "alguna rama no dispone de inverse mapping".into(),
            ));
        }
        Ok(Self {
            schema: MILKY_WAY_REGISTRATION_SCHEMA,
            source_geometry,
            output_geometry,
            sky_source_to_output,
            ground_source_to_output,
            masks_sampled_in_source_space: true,
        })
    }

    pub const fn resample_passes(&self) -> u8 {
        1
    }

    pub fn source_coordinate(&self, layer: WarpLayer, output: Point2) -> Option<Point2> {
        match layer {
            WarpLayer::Sky => self.sky_source_to_output.inverse(output),
            WarpLayer::Ground => self.ground_source_to_output.inverse(output),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DualLayerRegistration {
    pub sky: TransformSolution,
    pub ground: GroundSolution,
    pub wcs: WcsDisposition,
    pub single_resample_plan: Option<SingleResamplePlan>,
    pub publishable: bool,
}

pub struct DualLayerRequest<'a> {
    pub sky_correspondences: &'a [Correspondence],
    pub ground_correspondences: &'a [Correspondence],
    pub source_geometry: ImageGeometry,
    pub output_geometry: ImageGeometry,
    pub sky_config: RegistrationConfig,
    pub ground_mode: GroundRegistrationMode,
    pub ground_config: RegistrationConfig,
    pub input_wcs: Option<&'a LinearTanWcs>,
}

pub fn solve_dual_layer_registration(
    request: DualLayerRequest<'_>,
) -> Result<DualLayerRegistration, RegistrationError> {
    let sky = estimate_registration(
        request.sky_correspondences,
        request.source_geometry,
        request.output_geometry,
        request.sky_config,
    )?;
    let ground = match request.ground_mode {
        GroundRegistrationMode::LockedIdentity => GroundSolution::LockedIdentity,
        GroundRegistrationMode::Affine | GroundRegistrationMode::Homography => {
            let mut config = request.ground_config;
            config.model = match request.ground_mode {
                GroundRegistrationMode::Affine => RegistrationModelSelection::Affine,
                GroundRegistrationMode::Homography => RegistrationModelSelection::Homography,
                GroundRegistrationMode::LockedIdentity => unreachable!(),
            };
            config.radial = RadialSearchConfig::disabled();
            GroundSolution::Registered(estimate_registration(
                request.ground_correspondences,
                request.source_geometry,
                request.output_geometry,
                config,
            )?)
        }
    };
    let publishable = sky.validation.accepted && ground.accepted();
    let wcs = propagate_wcs(
        request.input_wcs,
        &sky,
        request.source_geometry,
        request.output_geometry,
    );
    let single_resample_plan = publishable
        .then(|| {
            SingleResamplePlan::new(
                request.source_geometry,
                request.output_geometry,
                sky.transform,
                ground.transform(),
            )
        })
        .transpose()?;
    Ok(DualLayerRegistration {
        sky,
        ground,
        wcs,
        single_resample_plan,
        publishable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} no está a ±{tolerance} de {expected}"
        );
    }

    fn next_noise(state: &mut u64) -> f64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((*state >> 11) as f64 / ((1u64 << 53) as f64)) * 2.0 - 1.0
    }

    fn add_star(image: &mut [f32], geometry: ImageGeometry, center: Point2, amplitude: f64) {
        let center_x = center.x.round() as i64;
        let center_y = center.y.round() as i64;
        for dy in -3..=3 {
            for dx in -3..=3 {
                let x = center_x + dx;
                let y = center_y + dy;
                if x < 0 || y < 0 || x >= geometry.width as i64 || y >= geometry.height as i64 {
                    continue;
                }
                let radius_squared = (x as f64 - center.x).powi(2) + (y as f64 - center.y).powi(2);
                image[y as usize * geometry.width + x as usize] +=
                    (amplitude * (-radius_squared / 2.2).exp()) as f32;
            }
        }
    }

    fn test_config(model: RegistrationModelSelection) -> RegistrationConfig {
        RegistrationConfig {
            model,
            ransac_iterations: 900,
            inlier_threshold_px: 1.2,
            auto_homography_improvement: 0.12,
            radial: RadialSearchConfig::disabled(),
            validation: ValidationLimits {
                min_inliers: 8,
                min_inlier_ratio: 0.55,
                max_rms_px: 0.8,
                max_p95_px: 1.2,
                min_source_coverage: 0.08,
                corner_margin_fraction: 0.5,
                min_corner_area_ratio: 0.15,
                max_corner_area_ratio: 6.0,
                min_abs_jacobian: 1.0e-5,
                allow_reflection: false,
            },
        }
    }

    fn grid_correspondences(
        geometry: ImageGeometry,
        transform: TransformModel,
        noise: f64,
    ) -> Vec<Correspondence> {
        let mut pairs = Vec::new();
        let mut state = 0x91c3_7731_a5b4_021du64;
        for row in 0..5 {
            for column in 0..7 {
                let source = Point2::new(
                    55.0 + column as f64 * (geometry.width as f64 - 110.0) / 6.0
                        + next_noise(&mut state) * 3.0,
                    45.0 + row as f64 * (geometry.height as f64 - 90.0) / 4.0
                        + next_noise(&mut state) * 3.0,
                );
                let mut target = transform.apply(source).unwrap();
                target.x += next_noise(&mut state) * noise;
                target.y += next_noise(&mut state) * noise;
                pairs.push(Correspondence::new(source, target, 1.0));
            }
        }
        pairs
    }

    #[test]
    fn masked_star_detection_never_uses_ground_pixels() {
        let geometry = ImageGeometry::new(64, 48).unwrap();
        let mut image = vec![0.0f32; geometry.width * geometry.height];
        let mut state = 7u64;
        for value in &mut image {
            *value = (0.01 + next_noise(&mut state) * 0.001) as f32;
        }
        let expected = [
            Point2::new(12.3, 9.8),
            Point2::new(31.1, 14.6),
            Point2::new(51.7, 22.2),
        ];
        for (index, point) in expected.iter().enumerate() {
            add_star(&mut image, geometry, *point, 0.8 + index as f64 * 0.2);
        }
        // Este máximo es más brillante, pero pertenece al suelo excluido.
        add_star(&mut image, geometry, Point2::new(28.0, 40.0), 30.0);
        let mut mask = vec![0u8; image.len()];
        for y in 0..30 {
            mask[y * geometry.width..(y + 1) * geometry.width].fill(255);
        }
        let stars = detect_stars_masked(
            &image,
            geometry,
            MaskView {
                geometry,
                data: &mask,
                include_at_or_above: 128,
            },
            StarDetectionConfig {
                threshold_sigma: 5.0,
                centroid_radius: 3,
                min_separation_px: 5.0,
                max_stars: 20,
                min_mask_fraction: 0.65,
            },
        )
        .unwrap();
        assert_eq!(stars.len(), expected.len());
        for point in expected {
            assert!(
                stars
                    .iter()
                    .any(|star| star.position.distance(point) < 0.35),
                "no se detectó la estrella {point:?}: {stars:?}"
            );
        }
        assert!(stars.iter().all(|star| star.position.y < 30.0));
    }

    #[test]
    fn affine_ransac_recovers_motion_despite_outliers() {
        let geometry = ImageGeometry::new(900, 620).unwrap();
        let expected = TransformModel {
            planar: PlanarTransform::Affine([1.002, -0.014, 8.5, 0.011, 0.997, -5.25]),
            radial: None,
        };
        let mut pairs = grid_correspondences(geometry, expected, 0.08);
        for index in 0..9 {
            pairs.push(Correspondence::new(
                Point2::new(40.0 + index as f64 * 73.0, 80.0 + index as f64 * 31.0),
                Point2::new(800.0 - index as f64 * 39.0, 40.0 + index as f64 * 53.0),
                0.8,
            ));
        }
        let solution = estimate_registration(
            &pairs,
            geometry,
            geometry,
            test_config(RegistrationModelSelection::Affine),
        )
        .unwrap();
        assert!(
            solution.validation.accepted,
            "{:?}",
            solution.validation.reasons
        );
        assert!(solution.validation.inliers >= 34);
        assert!(solution.validation.rms_px < 0.2);
        let probe = Point2::new(613.2, 327.9);
        assert!(
            solution
                .transform
                .apply(probe)
                .unwrap()
                .distance(expected.apply(probe).unwrap())
                < 0.25
        );
    }

    #[test]
    fn auto_selects_homography_only_when_projective_terms_are_material() {
        let geometry = ImageGeometry::new(1000, 700).unwrap();
        let expected = TransformModel {
            planar: PlanarTransform::Homography([
                1.01, 0.012, 6.0, -0.008, 1.006, -4.0, 0.000_19, -0.000_13, 1.0,
            ]),
            radial: None,
        };
        let pairs = grid_correspondences(geometry, expected, 0.03);
        let mut config = test_config(RegistrationModelSelection::Auto);
        config.inlier_threshold_px = 0.65;
        config.validation.max_rms_px = 0.45;
        let solution = estimate_registration(&pairs, geometry, geometry, config).unwrap();
        assert!(
            solution.validation.accepted,
            "{:?}",
            solution.validation.reasons
        );
        assert!(matches!(
            solution.transform.planar,
            PlanarTransform::Homography(_)
        ));
        assert!(solution.validation.rms_px < 0.12);
    }

    #[test]
    fn ultra_wide_radial_search_recovers_curvature_and_reduces_rms() {
        let geometry = ImageGeometry::new(1000, 700).unwrap();
        let radial = RadialDistortion {
            center: Point2::new(499.5, 349.5),
            normalization_radius: (1000.0f64).hypot(700.0) * 0.5,
            k1: 0.12,
            k2: -0.03,
        };
        let expected = TransformModel {
            planar: PlanarTransform::Affine([0.999, -0.006, 4.0, 0.007, 1.001, -3.0]),
            radial: Some(radial),
        };
        let pairs = grid_correspondences(geometry, expected, 0.01);
        let mut baseline_config = test_config(RegistrationModelSelection::Affine);
        baseline_config.inlier_threshold_px = 8.0;
        baseline_config.validation.max_rms_px = 8.0;
        baseline_config.validation.max_p95_px = 12.0;
        let baseline = estimate_registration(&pairs, geometry, geometry, baseline_config).unwrap();
        let mut radial_config = baseline_config;
        radial_config.inlier_threshold_px = 0.7;
        radial_config.validation.max_rms_px = 0.5;
        radial_config.validation.max_p95_px = 0.8;
        radial_config.radial = RadialSearchConfig {
            enabled: true,
            center: radial.center,
            normalization_radius: radial.normalization_radius,
            max_abs_k1: 0.16,
            max_abs_k2: 0.08,
            grid_steps: 5,
            refinement_rounds: 1,
        };
        let solution = estimate_registration(&pairs, geometry, geometry, radial_config).unwrap();
        assert!(
            solution.validation.accepted,
            "{:?}",
            solution.validation.reasons
        );
        assert!(solution.validation.rms_px < baseline.validation.rms_px * 0.2);
        let recovered = solution.transform.radial.unwrap();
        // k1/k2 están parcialmente correlacionados con la escala afín; se
        // valida la curva radial observable, no un coeficiente aislado.
        for radius_squared in [0.5f64, 1.0] {
            let expected_factor =
                1.0 + radial.k1 * radius_squared + radial.k2 * radius_squared.powi(2);
            let recovered_factor =
                1.0 + recovered.k1 * radius_squared + recovered.k2 * radius_squared.powi(2);
            assert_close(recovered_factor, expected_factor, 0.015);
        }
    }

    #[test]
    fn validation_rejects_reflection_even_with_perfect_residuals() {
        let geometry = ImageGeometry::new(640, 480).unwrap();
        let reflection = TransformModel {
            planar: PlanarTransform::Affine([-1.0, 0.0, 639.0, 0.0, 1.0, 0.0]),
            radial: None,
        };
        let pairs = grid_correspondences(geometry, reflection, 0.0);
        let indices = (0..pairs.len()).collect::<Vec<_>>();
        let validation = validate_transform(
            reflection,
            &pairs,
            &indices,
            geometry,
            geometry,
            test_config(RegistrationModelSelection::Affine).validation,
        );
        assert!(!validation.accepted);
        assert!(validation
            .reasons
            .iter()
            .any(|reason| reason.contains("refleja") || reason.contains("orientación")));
    }

    fn exact_solution(geometry: ImageGeometry, transform: TransformModel) -> TransformSolution {
        let pairs = grid_correspondences(geometry, transform, 0.0);
        let indices = (0..pairs.len()).collect::<Vec<_>>();
        let validation = validate_transform(
            transform,
            &pairs,
            &indices,
            geometry,
            geometry,
            test_config(RegistrationModelSelection::Affine).validation,
        );
        assert!(validation.accepted, "{:?}", validation.reasons);
        TransformSolution {
            transform,
            inlier_indices: indices,
            validation,
        }
    }

    fn sample_wcs(geometry: ImageGeometry) -> LinearTanWcs {
        LinearTanWcs {
            ctype1: "RA---TAN".into(),
            ctype2: "DEC--TAN".into(),
            crval1: 83.822,
            crval2: -5.391,
            crpix1: 450.0,
            crpix2: 300.0,
            cd11: -0.000_24,
            cd12: 0.000_002,
            cd21: 0.000_001,
            cd22: 0.000_24,
            pixel_width: geometry.width,
            pixel_height: geometry.height,
        }
    }

    #[test]
    fn wcs_is_preserved_only_for_exact_affine_and_matching_source_geometry() {
        let geometry = ImageGeometry::new(900, 620).unwrap();
        let affine = TransformModel {
            planar: PlanarTransform::Affine([1.01, -0.02, 7.0, 0.015, 0.99, -4.0]),
            radial: None,
        };
        let solution = exact_solution(geometry, affine);
        let wcs = sample_wcs(geometry);
        let disposition = propagate_wcs(Some(&wcs), &solution, geometry, geometry);
        let WcsDisposition::Preserved { wcs: output, .. } = disposition else {
            panic!("un WCS afín exacto debió preservarse")
        };
        let expected_crpix_zero_based = affine
            .apply(Point2::new(wcs.crpix1 - 1.0, wcs.crpix2 - 1.0))
            .unwrap();
        assert_close(output.crpix1, expected_crpix_zero_based.x + 1.0, 1.0e-9);
        assert_close(output.crpix2, expected_crpix_zero_based.y + 1.0, 1.0e-9);
        assert_ne!(output.fingerprint(), wcs.fingerprint());

        let wrong_geometry = ImageGeometry::new(899, 620).unwrap();
        assert!(matches!(
            propagate_wcs(Some(&wcs), &solution, wrong_geometry, geometry),
            WcsDisposition::Invalidated { .. }
        ));

        let projective = TransformModel {
            planar: PlanarTransform::Homography([
                1.0, 0.0, 2.0, 0.0, 1.0, -1.0, 0.000_02, 0.000_01, 1.0,
            ]),
            radial: None,
        };
        let projective_solution = exact_solution(geometry, projective);
        assert!(matches!(
            propagate_wcs(Some(&wcs), &projective_solution, geometry, geometry),
            WcsDisposition::Candidate { .. }
        ));
    }

    #[test]
    fn sky_image_wrapper_recovers_translation_without_ground_contamination() {
        let geometry = ImageGeometry::new(160, 110).unwrap();
        let mut reference = vec![0.01f32; geometry.width * geometry.height];
        let mut target = reference.clone();
        let mut state = 0x4433u64;
        for index in 0..24 {
            let reference_position = Point2::new(
                15.0 + (index % 6) as f64 * 24.0 + next_noise(&mut state) * 2.5,
                12.0 + (index / 6) as f64 * 21.0 + next_noise(&mut state) * 2.5,
            );
            let target_position =
                Point2::new(reference_position.x - 5.0, reference_position.y + 3.0);
            let amplitude = 0.7 + index as f64 * 0.025;
            add_star(&mut reference, geometry, reference_position, amplitude);
            add_star(&mut target, geometry, target_position, amplitude);
        }
        // Objetos brillantes de suelo, distintos entre frames, deben ignorarse.
        add_star(&mut reference, geometry, Point2::new(40.0, 100.0), 50.0);
        add_star(&mut target, geometry, Point2::new(125.0, 98.0), 50.0);
        let mut mask = vec![0u8; reference.len()];
        for y in 0..90 {
            mask[y * geometry.width..(y + 1) * geometry.width].fill(255);
        }
        let mask = MaskView {
            geometry,
            data: &mask,
            include_at_or_above: 128,
        };
        let result = solve_sky_registration(&reference, &target, geometry, mask).unwrap();
        assert!(result.accepted, "{:?}", result.validation.reasons);
        assert!(result.inlier_correspondences.len() >= 18);
        let mapped = result.transform.apply(Point2::new(70.0, 50.0)).unwrap();
        assert_close(mapped.x, 75.0, 0.4);
        assert_close(mapped.y, 47.0, 0.4);
    }

    #[test]
    fn ground_ncc_wrapper_recovers_target_to_reference_shift() {
        let geometry = ImageGeometry::new(112, 80).unwrap();
        let mut reference = vec![0.0f32; geometry.width * geometry.height];
        let mut state = 0xabc1_9872u64;
        for y in 0..geometry.height {
            for x in 0..geometry.width {
                let texture = 0.3 * (x as f64 * 0.17).sin()
                    + 0.25 * (y as f64 * 0.23).cos()
                    + 0.15 * ((x + y) as f64 * 0.11).sin()
                    + next_noise(&mut state) * 0.08;
                reference[y * geometry.width + x] = texture as f32;
            }
        }
        let expected_dx = 5i32;
        let expected_dy = -3i32;
        let mut target = vec![0.0f32; reference.len()];
        for y in 0..geometry.height {
            for x in 0..geometry.width {
                let reference_x = x as i32 + expected_dx;
                let reference_y = y as i32 + expected_dy;
                if reference_x >= 0
                    && reference_y >= 0
                    && reference_x < geometry.width as i32
                    && reference_y < geometry.height as i32
                {
                    target[y * geometry.width + x] =
                        reference[reference_y as usize * geometry.width + reference_x as usize];
                }
            }
        }
        let mask_data = vec![255u8; reference.len()];
        let result = solve_ground_registration(
            &reference,
            &target,
            geometry,
            MaskView {
                geometry,
                data: &mask_data,
                include_at_or_above: 128,
            },
            10.0,
        )
        .unwrap();
        assert!(result.accepted, "{result:?}");
        assert!(result.ncc_score > 0.95);
        let mapped = result.transform.apply(Point2::new(40.0, 30.0)).unwrap();
        assert_close(mapped.x, 45.0, 0.55);
        assert_close(mapped.y, 27.0, 0.55);
    }

    #[test]
    fn dual_layer_plan_keeps_independent_transforms_and_one_resample() {
        let geometry = ImageGeometry::new(900, 620).unwrap();
        let sky_transform = TransformModel {
            planar: PlanarTransform::Affine([1.0, -0.006, 6.0, 0.005, 1.0, -4.0]),
            radial: None,
        };
        let ground_transform = TransformModel {
            planar: PlanarTransform::Affine([1.0, 0.002, -2.0, -0.001, 1.0, 1.5]),
            radial: None,
        };
        let sky_pairs = grid_correspondences(geometry, sky_transform, 0.0);
        let ground_pairs = grid_correspondences(geometry, ground_transform, 0.0);
        let config = test_config(RegistrationModelSelection::Affine);
        let wcs = sample_wcs(geometry);
        let result = solve_dual_layer_registration(DualLayerRequest {
            sky_correspondences: &sky_pairs,
            ground_correspondences: &ground_pairs,
            source_geometry: geometry,
            output_geometry: geometry,
            sky_config: config,
            ground_mode: GroundRegistrationMode::Affine,
            ground_config: config,
            input_wcs: Some(&wcs),
        })
        .unwrap();
        assert!(result.publishable);
        assert!(matches!(result.wcs, WcsDisposition::Preserved { .. }));
        let plan = result.single_resample_plan.as_ref().unwrap();
        assert_eq!(plan.resample_passes(), 1);
        assert!(plan.masks_sampled_in_source_space);
        let source = Point2::new(321.25, 211.75);
        // El plan invierte directamente sus modelos estimados; no hay un
        // ráster intermedio ni una segunda transformación oculta.
        let sky_output = result.sky.transform.apply(source).unwrap();
        let ground_output = result.ground.transform().apply(source).unwrap();
        assert!(
            plan.source_coordinate(WarpLayer::Sky, sky_output)
                .unwrap()
                .distance(source)
                < 1.0e-7
        );
        assert!(
            plan.source_coordinate(WarpLayer::Ground, ground_output)
                .unwrap()
                .distance(source)
                < 1.0e-7
        );
    }

    #[test]
    fn bijective_matcher_never_reuses_a_reference_star() {
        let source = [
            StarFeature {
                position: Point2::new(10.0, 10.0),
                flux: 10.0,
                snr: 8.0,
                fwhm_px: 2.0,
            },
            StarFeature {
                position: Point2::new(10.4, 10.1),
                flux: 9.0,
                snr: 7.0,
                fwhm_px: 2.1,
            },
        ];
        let target = [StarFeature {
            position: Point2::new(10.1, 10.0),
            flux: 10.0,
            snr: 8.0,
            fwhm_px: 2.0,
        }];
        let matches = match_stars_bijective(&source, &target, &TransformModel::identity(), 2.0);
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn saturated_plateau_star_is_excluded_from_detection() {
        let geometry = ImageGeometry::new(64, 48).unwrap();
        let mut image = vec![0.01f32; geometry.width * geometry.height];
        // Estrella limpia (gaussiana bien muestreada): debe conservarse.
        let clean = Point2::new(15.3, 12.6);
        add_star(&mut image, geometry, clean, 0.6);
        // Estrella saturada: el sensor recorta el perfil en una meseta plana
        // 5x5. Sin el veto, el desempate raster la publicaría con el "pico"
        // en la esquina superior izquierda y un centroide sesgado.
        for y in 30..35 {
            for x in 44..49 {
                image[y * geometry.width + x] = 0.9;
            }
        }
        let mask = vec![255u8; image.len()];
        let stars = detect_stars_masked(
            &image,
            geometry,
            MaskView {
                geometry,
                data: &mask,
                include_at_or_above: 128,
            },
            StarDetectionConfig {
                threshold_sigma: 5.0,
                centroid_radius: 3,
                min_separation_px: 5.0,
                max_stars: 20,
                min_mask_fraction: 0.65,
            },
        )
        .unwrap();
        assert_eq!(
            stars.len(),
            1,
            "la meseta saturada debió excluirse y la gaussiana conservarse: {stars:?}"
        );
        assert!(
            stars[0].position.distance(clean) < 0.05,
            "el centroide de la estrella limpia se degradó: {:?}",
            stars[0].position
        );
    }

    #[test]
    fn clean_gaussian_keeps_subpixel_centroid() {
        let geometry = ImageGeometry::new(96, 64).unwrap();
        let mut image = vec![0.01f32; geometry.width * geometry.height];
        // Offsets subpíxel variados respecto a la rejilla para verificar que
        // el veto de meseta no toca el caso limpio y que el centroide
        // ponderado recupera la posición verdadera con error < 0.05 px.
        let expected = [
            Point2::new(20.3, 15.6),
            Point2::new(58.75, 22.4),
            Point2::new(41.2, 47.85),
        ];
        for (index, point) in expected.iter().enumerate() {
            add_star(&mut image, geometry, *point, 0.5 + 0.2 * index as f64);
        }
        let mask = vec![255u8; image.len()];
        let stars = detect_stars_masked(
            &image,
            geometry,
            MaskView {
                geometry,
                data: &mask,
                include_at_or_above: 128,
            },
            StarDetectionConfig {
                threshold_sigma: 5.0,
                centroid_radius: 3,
                min_separation_px: 5.0,
                max_stars: 20,
                min_mask_fraction: 0.65,
            },
        )
        .unwrap();
        assert_eq!(stars.len(), expected.len(), "{stars:?}");
        for point in expected {
            let error = stars
                .iter()
                .map(|star| star.position.distance(point))
                .fold(f64::INFINITY, f64::min);
            assert!(
                error < 0.05,
                "centroide de {point:?} con error {error} px (>= 0.05)"
            );
        }
    }

    /// 40 estrellas deterministas en rejilla 8x5 con perturbación LCG: sin
    /// simetrías exactas, con vecindarios bien poblados para el descriptor.
    fn descriptor_test_stars() -> Vec<StarFeature> {
        let mut state = 0x00c5_c5c5_1234_5678u64;
        let mut stars = Vec::with_capacity(40);
        for index in 0..40usize {
            let column = (index % 8) as f64;
            let row = (index / 8) as f64;
            let position = Point2::new(
                45.0 + column * 68.0 + next_noise(&mut state) * 14.0,
                55.0 + row * 105.0 + next_noise(&mut state) * 14.0,
            );
            stars.push(StarFeature {
                position,
                flux: 100.0 - index as f64,
                snr: 20.0,
                fwhm_px: 2.4,
            });
        }
        stars
    }

    #[test]
    fn descriptor_matching_survives_one_missing_faint_neighbour() {
        let reference = descriptor_test_stars();
        let config = SkyImageRegistrationConfig::default();
        // Sonda interior con vecindario completo.
        let probe_index = 18usize;
        let probe_position = reference[probe_index].position;
        // Se elimina el vecino MÁS PRÓXIMO de la sonda: peor caso, porque su
        // pérdida desplaza la firma radial entera desde la primera posición.
        let mut missing_index = usize::MAX;
        let mut missing_distance = f64::INFINITY;
        for (index, star) in reference.iter().enumerate() {
            if index == probe_index {
                continue;
            }
            let distance = star.position.distance(probe_position);
            if distance < missing_distance {
                missing_distance = distance;
                missing_index = index;
            }
        }
        assert!(missing_index != usize::MAX);
        // Target: mismas estrellas menos el vecino tenue, con jitter
        // determinista de hasta 0.3 px por coordenada.
        let mut state = 0x7777_1111u64;
        let mut target = Vec::new();
        for (index, star) in reference.iter().enumerate() {
            if index == missing_index {
                continue;
            }
            let mut moved = *star;
            moved.position.x += next_noise(&mut state) * 0.3;
            moved.position.y += next_noise(&mut state) * 0.3;
            target.push(moved);
        }
        let matches = match_stars_by_local_geometry(&target, &reference, config);
        let probe_match = matches
            .iter()
            .find(|correspondence| correspondence.target.distance(probe_position) < 1.0e-9);
        let Some(probe_match) = probe_match else {
            panic!(
                "la sonda perdió su emparejamiento al faltar un vecino tenue: {} matches",
                matches.len()
            );
        };
        assert!(
            probe_match.source.distance(probe_position) < 1.0,
            "la sonda emparejó con una estrella equivocada: {probe_match:?}"
        );
    }

    #[test]
    fn descriptor_matching_is_rotation_invariant_with_skip_tolerance() {
        let reference = descriptor_test_stars();
        let angle = 30.0f64.to_radians();
        let (sin, cos) = angle.sin_cos();
        let center = Point2::new(283.0, 265.0);
        let rotate = |point: Point2| {
            let dx = point.x - center.x;
            let dy = point.y - center.y;
            Point2::new(
                center.x + cos * dx - sin * dy,
                center.y + sin * dx + cos * dy,
            )
        };
        let target = reference
            .iter()
            .map(|star| StarFeature {
                position: rotate(star.position),
                ..*star
            })
            .collect::<Vec<_>>();
        let matches = match_stars_by_local_geometry(
            &target,
            &reference,
            SkyImageRegistrationConfig::default(),
        );
        // El descriptor sólo usa distancias: una rotación pura debe seguir
        // emparejando el campo casi al completo, también con la tolerancia
        // a omisiones activa.
        assert!(
            matches.len() >= 30,
            "solo {} emparejamientos tras rotar 30 grados",
            matches.len()
        );
        for correspondence in &matches {
            assert!(
                correspondence
                    .source
                    .distance(rotate(correspondence.target))
                    < 1.0e-6,
                "emparejamiento incoherente con la rotación: {correspondence:?}"
            );
        }
    }
}
