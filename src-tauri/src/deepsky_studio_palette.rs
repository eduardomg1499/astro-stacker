// DeepSky Studio — galería y aplicación no destructiva de paletas multibanda.
//
// Este módulo trabaja exclusivamente con másteres lineales MONO ya integrados
// (o con los proxies explícitos de un máster OSC dual-band activo). Los archivos
// de entrada nunca se reescriben. Una aplicación crea un `DeepSkyResult`
// derivado y conserva el producto activo anterior en el almacén lossless.

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeepSkyStudioPaletteRequest {
    #[serde(default)]
    ha_paths: Vec<String>,
    #[serde(default)]
    oiii_paths: Vec<String>,
    #[serde(default)]
    sii_paths: Vec<String>,
    #[serde(default)]
    instrument_profile: Option<String>,
    #[serde(default)]
    palette_ids: Vec<String>,
    #[serde(default = "studio_palette_default_register")]
    register: bool,
    #[serde(default = "studio_palette_default_oiii_green")]
    oiii_green_weight: f32,
    #[serde(default)]
    crosstalk_suppression: f32,
    #[serde(default = "studio_palette_default_preview_edge")]
    preview_max_edge: usize,
}

impl Default for DeepSkyStudioPaletteRequest {
    fn default() -> Self {
        Self {
            ha_paths: Vec::new(),
            oiii_paths: Vec::new(),
            sii_paths: Vec::new(),
            instrument_profile: None,
            palette_ids: Vec::new(),
            register: true,
            oiii_green_weight: studio_palette_default_oiii_green(),
            crosstalk_suppression: 0.0,
            preview_max_edge: studio_palette_default_preview_edge(),
        }
    }
}

fn studio_palette_default_register() -> bool {
    true
}

fn studio_palette_default_oiii_green() -> f32 {
    0.65
}

fn studio_palette_default_preview_edge() -> usize {
    1120
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeepSkyStudioApplyPaletteRequest {
    #[serde(flatten)]
    sources: DeepSkyStudioPaletteRequest,
    palette_id: String,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DeepSkyStudioRegistrationEvidence {
    path: String,
    reference: bool,
    registered: bool,
    model: String,
    inliers: usize,
    rms_px: f32,
    coverage_percent: f32,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DeepSkyStudioNormalizationEvidence {
    path: String,
    scale: f32,
    offset_adu: f32,
    noise_sigma_adu: f32,
    normalized_weight: f32,
    overlap_percent: f32,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DeepSkyStudioLineReconciliation {
    line: String,
    input_count: usize,
    method: String,
    combined_noise_sigma_adu: f32,
    coverage_percent: f32,
    inputs: Vec<DeepSkyStudioNormalizationEvidence>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DeepSkyStudioPaletteCandidate {
    id: String,
    label: String,
    eligible: bool,
    reason: Option<String>,
    preview: Option<String>,
    scientific_class: String,
    mapping: String,
    required_lines: Vec<String>,
    score: Option<f32>,
    score_reasons: Vec<String>,
    intent: String,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DeepSkyStudioPreviewContract {
    linked: bool,
    shadow_sigma: f32,
    target_midtone: f32,
    max_edge: usize,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DeepSkyStudioPaletteGallery {
    route: String,
    width: usize,
    height: usize,
    masters_immutable: bool,
    recommended_palette_id: Option<String>,
    registrations: Vec<DeepSkyStudioRegistrationEvidence>,
    oiii_reconciliation: Option<DeepSkyStudioLineReconciliation>,
    line_reconciliations: Vec<DeepSkyStudioLineReconciliation>,
    candidates: Vec<DeepSkyStudioPaletteCandidate>,
    limitations: Vec<String>,
    preview_contract: DeepSkyStudioPreviewContract,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DeepSkyStudioPaletteDescriptor {
    product_id: String,
    palette_id: String,
    route: String,
    scientific_class: String,
    quantitative_line_flux: bool,
    input_files_immutable: bool,
    preserved_product_id: Option<String>,
    limitations: Vec<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DeepSkyStudioPaletteApplyResult {
    state: PostStackRevisionState,
    preview: String,
    descriptor: DeepSkyStudioPaletteDescriptor,
    recipe: serde_json::Value,
}

#[derive(Clone, Debug)]
struct StudioAlignedPlane {
    path: String,
    data: Vec<f32>,
    coverage: Vec<f32>,
}

#[derive(Clone, Debug)]
struct StudioLinePlane {
    data: Vec<f32>,
    coverage: Vec<f32>,
    reconciliation: DeepSkyStudioLineReconciliation,
}

#[derive(Clone, Debug, Default)]
struct StudioPreparedComponents {
    width: usize,
    height: usize,
    ha: Option<StudioLinePlane>,
    oiii: Option<StudioLinePlane>,
    sii: Option<StudioLinePlane>,
    registrations: Vec<DeepSkyStudioRegistrationEvidence>,
    route: String,
    scientific_class: String,
    limitations: Vec<String>,
    source_paths: std::collections::BTreeMap<String, Vec<String>>,
    astrometry_solution: Option<AstrometrySolution>,
}

#[derive(Clone, Copy)]
struct StudioPaletteDefinition {
    id: &'static str,
    label: &'static str,
    required: &'static [&'static str],
    mapping: &'static str,
    intent: &'static str,
}

const STUDIO_PALETTE_DEFINITIONS: &[StudioPaletteDefinition] = &[
    StudioPaletteDefinition {
        id: "hooNatural",
        label: "HOO natural",
        required: &["HA", "OIII"],
        mapping: "R=Ha · G=0.35Ha+0.65OIII · B=OIII",
        intent: "Color natural de dos líneas con transición suave entre Ha y OIII",
    },
    StudioPaletteDefinition {
        id: "hooTeal",
        label: "HOO cyan",
        required: &["HA", "OIII"],
        mapping: "R=Ha · G=OIII · B=OIII",
        intent: "Contraste cromático alto entre emisión Ha y OIII",
    },
    StudioPaletteDefinition {
        id: "hooSoft",
        label: "HOO suave",
        required: &["HA", "OIII"],
        mapping: "R=Ha · G=0.50Ha+0.50OIII · B=0.18Ha+0.82OIII",
        intent: "Transiciones contenidas y estrellas menos cian",
    },
    StudioPaletteDefinition {
        id: "hooGold",
        label: "HOO dorada",
        required: &["HA", "OIII"],
        mapping: "R=Ha · G=0.62Ha+0.38OIII · B=0.88OIII",
        intent: "Realza el volumen de Ha conservando OIII como contrapunto frío",
    },
    StudioPaletteDefinition {
        id: "sho",
        label: "SHO · Hubble",
        required: &["SII", "HA", "OIII"],
        mapping: "R=SII · G=Ha · B=OIII",
        intent: "Asignación SHO clásica para separar SII, Ha y OIII",
    },
    StudioPaletteDefinition {
        id: "shoBalanced",
        label: "SHO equilibrada",
        required: &["SII", "HA", "OIII"],
        mapping: "R=0.82SII+0.18Ha · G=Ha · B=OIII",
        intent: "SHO con transición SII/Ha más continua",
    },
    StudioPaletteDefinition {
        id: "hso",
        label: "HSO",
        required: &["HA", "SII", "OIII"],
        mapping: "R=Ha · G=SII · B=OIII",
        intent: "Alternativa tricromática que conserva Ha en rojo",
    },
    StudioPaletteDefinition {
        id: "soo",
        label: "SOO",
        required: &["SII", "OIII"],
        mapping: "R=SII · G=OIII · B=OIII",
        intent: "Bicolor SII/OIII cuando Ha no está disponible",
    },
];

fn studio_palette_canonical_id(id: &str) -> Option<&'static str> {
    match id.trim().to_ascii_lowercase().as_str() {
        "hoo" | "hoonatural" | "hoo-natural" | "natural" => Some("hooNatural"),
        "hooteal" | "hoo-teal" | "teal" => Some("hooTeal"),
        "hoosoft" | "hoo-soft" | "soft" => Some("hooSoft"),
        "hoogold" | "hoo-gold" | "gold" => Some("hooGold"),
        "sho" | "hubble" => Some("sho"),
        "shobalanced" | "sho-balanced" => Some("shoBalanced"),
        "hso" => Some("hso"),
        "soo" => Some("soo"),
        _ => None,
    }
}

fn studio_palette_definition(id: &str) -> Option<StudioPaletteDefinition> {
    let canonical = studio_palette_canonical_id(id)?;
    STUDIO_PALETTE_DEFINITIONS
        .iter()
        .copied()
        .find(|definition| definition.id == canonical)
}

fn studio_clean_paths(paths: &[String]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    paths
        .iter()
        .map(|path| path.trim())
        .filter(|path| !path.is_empty())
        .filter_map(|path| seen.insert(path.to_string()).then(|| path.to_string()))
        .collect()
}

fn studio_validate_disjoint_sources(
    ha: &[String],
    oiii: &[String],
    sii: &[String],
) -> Result<(), String> {
    let mut assigned = std::collections::BTreeMap::<&str, &'static str>::new();
    for (line, paths) in [("HA", ha), ("OIII", oiii), ("SII", sii)] {
        for path in paths {
            if let Some(previous) = assigned.insert(path.as_str(), line) {
                return Err(format!(
                    "El mismo máster '{}' está asignado a {} y {}; cada archivo MONO debe representar una sola línea.",
                    path, previous, line
                ));
            }
        }
    }
    Ok(())
}

fn studio_finite_coverage(data: &[f32], pixels: usize) -> Vec<f32> {
    (0..pixels)
        .map(|index| {
            data.get(index)
                .copied()
                .is_some_and(f32::is_finite)
                .then_some(1.0)
                .unwrap_or(0.0)
        })
        .collect()
}

fn studio_coverage_percent(coverage: &[f32]) -> f32 {
    if coverage.is_empty() {
        return 0.0;
    }
    100.0
        * coverage
            .iter()
            .filter(|value| value.is_finite() && **value > 0.0)
            .count() as f32
        / coverage.len() as f32
}

fn studio_quantile(values: &mut [f32], q: f32) -> f32 {
    if values.is_empty() {
        return f32::NAN;
    }
    values.sort_by(f32::total_cmp);
    let position = (q.clamp(0.0, 1.0) * (values.len() - 1) as f32).round() as usize;
    values[position.min(values.len() - 1)]
}

fn studio_noise_sigma(data: &[f32], coverage: &[f32], width: usize, height: usize) -> f32 {
    if width < 2 || height < 2 || data.len() < width.saturating_mul(height) {
        return 1.0;
    }
    let pixels = width * height;
    let stride = (pixels / 240_000).max(1);
    let mut differences = Vec::with_capacity((pixels / stride).min(480_000));
    for index in (0..pixels).step_by(stride) {
        if coverage.get(index).copied().unwrap_or(0.0) <= 0.0 || !data[index].is_finite() {
            continue;
        }
        let x = index % width;
        let y = index / width;
        if x + 1 < width {
            let other = index + 1;
            if coverage.get(other).copied().unwrap_or(0.0) > 0.0 && data[other].is_finite() {
                differences.push((data[index] - data[other]).abs());
            }
        }
        if y + 1 < height {
            let other = index + width;
            if coverage.get(other).copied().unwrap_or(0.0) > 0.0 && data[other].is_finite() {
                differences.push((data[index] - data[other]).abs());
            }
        }
    }
    if differences.len() < 32 {
        return 1.0;
    }
    // Para ruido gaussiano independiente, median(|x_i-x_j|)=0.953872·sigma.
    let median = studio_quantile(&mut differences, 0.5);
    (median / 0.953_872).max(1.0e-4)
}

fn studio_pair_normalization(
    reference: &StudioAlignedPlane,
    candidate: &StudioAlignedPlane,
) -> Result<(f32, f32, f32), String> {
    if reference.data.len() != candidate.data.len()
        || reference.coverage.len() != candidate.coverage.len()
    {
        return Err("Los másteres reconciliados no comparten geometría".into());
    }
    let pixels = reference.data.len();
    let stride = (pixels / 240_000).max(1);
    let mut reference_samples = Vec::new();
    let mut candidate_samples = Vec::new();
    for index in (0..pixels).step_by(stride) {
        let a = reference.data[index];
        let b = candidate.data[index];
        if reference.coverage[index] > 0.0
            && candidate.coverage[index] > 0.0
            && a.is_finite()
            && b.is_finite()
        {
            reference_samples.push(a);
            candidate_samples.push(b);
        }
    }
    if reference_samples.len() < 64 {
        return Err(format!(
            "Sólo hay {} muestras superpuestas; no alcanza para normalizar los másteres.",
            reference_samples.len()
        ));
    }
    let overlap_percent =
        100.0 * reference_samples.len() as f32 / ((pixels + stride - 1) / stride).max(1) as f32;
    let mut ref_q16 = reference_samples.clone();
    let mut ref_q50 = reference_samples.clone();
    let mut ref_q84 = reference_samples;
    let mut cand_q16 = candidate_samples.clone();
    let mut cand_q50 = candidate_samples.clone();
    let mut cand_q84 = candidate_samples;
    let r16 = studio_quantile(&mut ref_q16, 0.16);
    let r50 = studio_quantile(&mut ref_q50, 0.50);
    let r84 = studio_quantile(&mut ref_q84, 0.84);
    let c16 = studio_quantile(&mut cand_q16, 0.16);
    let c50 = studio_quantile(&mut cand_q50, 0.50);
    let c84 = studio_quantile(&mut cand_q84, 0.84);
    let reference_span = (r84 - r16).abs();
    let candidate_span = (c84 - c16).abs();
    let scale = if reference_span.is_finite()
        && candidate_span.is_finite()
        && reference_span > 1.0e-6
        && candidate_span > 1.0e-6
    {
        (reference_span / candidate_span).clamp(0.05, 20.0)
    } else {
        1.0
    };
    let offset = r50 - scale * c50;
    Ok((scale, offset, overlap_percent.clamp(0.0, 100.0)))
}

fn studio_reconcile_line(
    line: &'static str,
    planes: Vec<StudioAlignedPlane>,
    width: usize,
    height: usize,
) -> Result<StudioLinePlane, String> {
    if planes.is_empty() {
        return Err(format!("No hay másteres {line}"));
    }
    let pixels = width
        .checked_mul(height)
        .ok_or("Geometría de paleta fuera de rango")?;
    if planes
        .iter()
        .any(|plane| plane.data.len() != pixels || plane.coverage.len() != pixels)
    {
        return Err(format!("Un máster {line} no coincide con {width}×{height}"));
    }
    let reference = &planes[0];
    let mut normalized = Vec::with_capacity(planes.len());
    let mut evidences = Vec::with_capacity(planes.len());
    let mut raw_weights = Vec::with_capacity(planes.len());
    for (index, plane) in planes.iter().enumerate() {
        let (scale, offset, overlap_percent) = if index == 0 {
            (1.0, 0.0, studio_coverage_percent(&plane.coverage))
        } else {
            studio_pair_normalization(reference, plane)?
        };
        let mut data = Vec::with_capacity(pixels);
        for (&value, &coverage) in plane.data.iter().zip(&plane.coverage) {
            data.push(if coverage > 0.0 && value.is_finite() {
                value * scale + offset
            } else {
                f32::NAN
            });
        }
        let sigma = studio_noise_sigma(&data, &plane.coverage, width, height);
        let weight = 1.0 / (sigma.max(1.0e-4) as f64).powi(2);
        raw_weights.push(weight);
        normalized.push((data, plane.coverage.clone()));
        evidences.push(DeepSkyStudioNormalizationEvidence {
            path: plane.path.clone(),
            scale,
            offset_adu: offset,
            noise_sigma_adu: sigma,
            normalized_weight: 0.0,
            overlap_percent,
        });
    }
    let total_weight: f64 = raw_weights.iter().sum::<f64>().max(f64::EPSILON);
    for (evidence, weight) in evidences.iter_mut().zip(&raw_weights) {
        evidence.normalized_weight = (*weight / total_weight) as f32;
    }
    let mut data = vec![f32::NAN; pixels];
    let mut coverage = vec![0.0f32; pixels];
    for pixel in 0..pixels {
        let mut weighted_sum = 0.0f64;
        let mut surviving_weight = 0.0f64;
        for ((plane, plane_coverage), &weight) in normalized.iter().zip(&raw_weights) {
            let value = plane[pixel];
            let cov = plane_coverage[pixel].clamp(0.0, 1.0) as f64;
            if cov > 0.0 && value.is_finite() {
                weighted_sum += value as f64 * weight * cov;
                surviving_weight += weight * cov;
            }
        }
        if surviving_weight > 0.0 {
            data[pixel] = (weighted_sum / surviving_weight) as f32;
            coverage[pixel] = (surviving_weight / total_weight).clamp(0.0, 1.0) as f32;
        }
    }
    let combined_noise = (1.0 / total_weight.sqrt()) as f32;
    let reconciliation = DeepSkyStudioLineReconciliation {
        line: line.to_string(),
        input_count: planes.len(),
        method: if planes.len() == 1 {
            "singleMasterNoReconciliation".into()
        } else {
            "robustQ16Q50Q84Normalization+inverseNoiseVarianceWeighting".into()
        },
        combined_noise_sigma_adu: combined_noise,
        coverage_percent: studio_coverage_percent(&coverage),
        inputs: evidences,
    };
    Ok(StudioLinePlane {
        data,
        coverage,
        reconciliation,
    })
}

fn studio_load_mono(path: &str) -> Result<DsImage, String> {
    ds_require_mono_channel_master(ds_read_image(path)?, path)
}

fn studio_align_path(
    path: &str,
    reference_path: &str,
    reference: &DsImage,
    reference_stars: &[(f32, f32, f32)],
    register: bool,
) -> Result<(StudioAlignedPlane, DeepSkyStudioRegistrationEvidence), String> {
    let pixels = reference.w * reference.h;
    if path == reference_path {
        let coverage = studio_finite_coverage(&reference.data, pixels);
        return Ok((
            StudioAlignedPlane {
                path: path.into(),
                data: reference.data.clone(),
                coverage: coverage.clone(),
            },
            DeepSkyStudioRegistrationEvidence {
                path: path.into(),
                reference: true,
                registered: true,
                model: "identityReference".into(),
                inliers: reference_stars.len(),
                rms_px: 0.0,
                coverage_percent: studio_coverage_percent(&coverage),
            },
        ));
    }
    let image = studio_load_mono(path)?;
    if !register {
        if image.w != reference.w || image.h != reference.h {
            return Err(format!(
                "'{}' es {}×{} y la referencia es {}×{}; activa el registro estelar.",
                path, image.w, image.h, reference.w, reference.h
            ));
        }
        let coverage = studio_finite_coverage(&image.data, pixels);
        return Ok((
            StudioAlignedPlane {
                path: path.into(),
                data: image.data,
                coverage: coverage.clone(),
            },
            DeepSkyStudioRegistrationEvidence {
                path: path.into(),
                reference: false,
                registered: false,
                model: "identityUserConfirmed".into(),
                inliers: 0,
                rms_px: 0.0,
                coverage_percent: studio_coverage_percent(&coverage),
            },
        ));
    }
    let stars = ds_detect_stars(&image.data, image.w, image.h, 160);
    let registration =
        ds_match_triangles_in_field(reference_stars, &stars, reference.w, reference.h)
            .ok_or_else(|| {
                format!(
                    "No se pudo registrar '{}' a la referencia: se requieren al menos 8 correspondencias estelares validadas.",
                    path
                )
            })?;
    let (data, coverage) = ds_warp_single(&image, registration.transform, reference.w, reference.h);
    Ok((
        StudioAlignedPlane {
            path: path.into(),
            data,
            coverage: coverage.clone(),
        },
        DeepSkyStudioRegistrationEvidence {
            path: path.into(),
            reference: false,
            registered: true,
            model: registration.transform.model.label().into(),
            inliers: registration.inliers,
            rms_px: registration.rms,
            coverage_percent: studio_coverage_percent(&coverage),
        },
    ))
}

fn studio_prepare_file_components(
    request: &DeepSkyStudioPaletteRequest,
) -> Result<StudioPreparedComponents, String> {
    let ha_paths = studio_clean_paths(&request.ha_paths);
    let oiii_paths = studio_clean_paths(&request.oiii_paths);
    let sii_paths = studio_clean_paths(&request.sii_paths);
    studio_validate_disjoint_sources(&ha_paths, &oiii_paths, &sii_paths)?;
    let reference_path = ha_paths
        .first()
        .or_else(|| sii_paths.first())
        .or_else(|| oiii_paths.first())
        .ok_or("Selecciona al menos dos másteres de línea para construir una paleta")?
        .clone();
    let reference = studio_load_mono(&reference_path)?;
    let reference_stars = if request.register {
        let stars = ds_detect_stars(&reference.data, reference.w, reference.h, 160);
        if stars.len() < 8 {
            return Err(
                "La referencia no contiene 8 estrellas publicables; elige otra referencia o confirma geometría idéntica desactivando registro."
                    .into(),
            );
        }
        stars
    } else {
        Vec::new()
    };
    let mut prepared = StudioPreparedComponents {
        width: reference.w,
        height: reference.h,
        scientific_class: "relativeNarrowbandComposite".into(),
        limitations: vec![
            "Las paletas son combinaciones lineales relativas; no sustituyen fotometría espectral ni miden flujo absoluto de línea.".into(),
            "El registro y la reconciliación OIII se aplican a copias en memoria; los másteres FITS/TIFF de entrada permanecen intactos.".into(),
            "La solución WCS de archivos externos se valida de nuevo sobre el compuesto; no se copia a ciegas desde un solo canal.".into(),
            "El compuesto conserva cobertura, pero no inventa VAR/NEFF/DQ sin mapas de incertidumbre compatibles por canal.".into(),
        ],
        ..StudioPreparedComponents::default()
    };
    if !request.register {
        prepared.limitations.push(
            "Registro estelar desactivado por el usuario: se exige geometría idéntica y la coincidencia subpíxel queda bajo su responsabilidad.".into(),
        );
    }
    if let Some(profile) = request
        .instrument_profile
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        prepared.limitations.push(format!(
            "Perfil instrumental '{profile}' registrado como procedencia; esta base aún no aplica una matriz de respuesta espectral cuantitativa."
        ));
    } else {
        prepared.limitations.push(
            "Sin perfil cámara+filtro no se corrigen throughput ni contaminación cruzada entre líneas.".into(),
        );
    }
    let groups: [(&'static str, &[String]); 3] = [
        ("HA", ha_paths.as_slice()),
        ("OIII", oiii_paths.as_slice()),
        ("SII", sii_paths.as_slice()),
    ];
    for (line, paths) in groups {
        if paths.is_empty() {
            continue;
        }
        let mut aligned = Vec::with_capacity(paths.len());
        for path in paths {
            let (plane, evidence) = studio_align_path(
                path,
                &reference_path,
                &reference,
                &reference_stars,
                request.register,
            )?;
            aligned.push(plane);
            prepared.registrations.push(evidence);
        }
        let reconciled = studio_reconcile_line(line, aligned, reference.w, reference.h)?;
        prepared.source_paths.insert(line.into(), paths.to_vec());
        match line {
            "HA" => prepared.ha = Some(reconciled),
            "OIII" => prepared.oiii = Some(reconciled),
            "SII" => prepared.sii = Some(reconciled),
            _ => unreachable!(),
        }
    }
    prepared.route = studio_route_for_components(&prepared).into();
    Ok(prepared)
}

fn studio_active_filter_profile(recipe: &serde_json::Value) -> Option<String> {
    recipe
        .get("filterProfile")
        .or_else(|| recipe.pointer("/parameters/filterProfile"))
        .and_then(serde_json::Value::as_str)
        .and_then(ds_filter_token)
        .map(str::to_string)
        .or_else(|| {
            recipe
                .get("inputs")
                .or_else(|| recipe.get("recipe").and_then(|value| value.get("inputs")))
                .and_then(|value| value.get("lights"))
                .and_then(serde_json::Value::as_array)
                .and_then(|values| values.first())
                .and_then(serde_json::Value::as_str)
                .and_then(ds_filter_of_path)
                .map(str::to_string)
        })
}

fn studio_prepare_active_components(
    state: &AppState,
    request: &DeepSkyStudioPaletteRequest,
) -> Result<StudioPreparedComponents, String> {
    let guard = state
        .deep_sky_result
        .lock()
        .map_err(|_| "No se pudo bloquear el máster activo")?;
    let result = guard
        .as_ref()
        .ok_or("No hay un máster activo en Cielo profundo Studio")?;
    if result.channels < 3 {
        return Err(
            "El máster activo es MONO; asigna másteres Ha/OIII/SII explícitos para crear la galería."
                .into(),
        );
    }
    let profile = studio_active_filter_profile(&result.recipe)
        .ok_or("El máster activo no declara un perfil dual-band Ha+OIII o SII+OIII")?;
    if !matches!(profile.as_str(), "HA_OIII" | "SII_OIII") {
        return Err(format!(
            "El perfil activo '{profile}' no es dual-band; usa másteres de línea explícitos."
        ));
    }
    let (primary, oiii) = ds_extract_dual_band_planes(
        &result.data,
        result.channels,
        request.oiii_green_weight.clamp(0.0, 1.0),
        request.crosstalk_suppression.clamp(0.0, 1.0),
    )?;
    let pixels = result.width * result.height;
    let coverage = if result.coverage.len() == pixels {
        result.coverage.clone()
    } else {
        studio_finite_coverage(&primary, pixels)
    };
    let make_line = |line: &'static str, data: Vec<f32>| {
        let noise = studio_noise_sigma(&data, &coverage, result.width, result.height);
        StudioLinePlane {
            data,
            coverage: coverage.clone(),
            reconciliation: DeepSkyStudioLineReconciliation {
                line: line.into(),
                input_count: 1,
                method: "oscDualBandSpectralProxy".into(),
                combined_noise_sigma_adu: noise,
                coverage_percent: studio_coverage_percent(&coverage),
                inputs: vec![DeepSkyStudioNormalizationEvidence {
                    path: format!("active://{}", result.id),
                    scale: 1.0,
                    offset_adu: 0.0,
                    noise_sigma_adu: noise,
                    normalized_weight: 1.0,
                    overlap_percent: studio_coverage_percent(&coverage),
                }],
            },
        }
    };
    let mut prepared = StudioPreparedComponents {
        width: result.width,
        height: result.height,
        oiii: Some(make_line("OIII", oiii)),
        route: if profile == "HA_OIII" {
            "activeDualBandHaOiiiProxy".into()
        } else {
            "activeDualBandSiiOiiiProxy".into()
        },
        scientific_class: "heuristicSpectralProxyComposite".into(),
        limitations: vec![
            "La separación OSC dual-band es un proxy heurístico RGB, no una medición cuantitativa de flujo Ha/OIII/SII.".into(),
            "Se requiere una matriz cámara+filtro medida para descontaminar las líneas con validez espectrofotométrica.".into(),
            "El máster activo y sus mapas SCI/VAR/NEFF/DQ no se modifican al previsualizar.".into(),
            "La derivación conserva cobertura, pero no reutiliza VAR/NEFF/DQ como si los proxies fueran líneas independientes.".into(),
        ],
        astrometry_solution: result.astrometry_solution.clone(),
        ..StudioPreparedComponents::default()
    };
    if profile == "HA_OIII" {
        prepared.ha = Some(make_line("HA", primary));
        prepared
            .source_paths
            .insert("HA".into(), vec![format!("active://{}", result.id)]);
    } else {
        prepared.sii = Some(make_line("SII", primary));
        prepared
            .source_paths
            .insert("SII".into(), vec![format!("active://{}", result.id)]);
    }
    prepared
        .source_paths
        .insert("OIII".into(), vec![format!("active://{}", result.id)]);
    Ok(prepared)
}

fn studio_has_line(components: &StudioPreparedComponents, line: &str) -> bool {
    match line {
        "HA" => components.ha.is_some(),
        "OIII" => components.oiii.is_some(),
        "SII" => components.sii.is_some(),
        _ => false,
    }
}

fn studio_line<'a>(
    components: &'a StudioPreparedComponents,
    line: &str,
) -> Option<&'a StudioLinePlane> {
    match line {
        "HA" => components.ha.as_ref(),
        "OIII" => components.oiii.as_ref(),
        "SII" => components.sii.as_ref(),
        _ => None,
    }
}

fn studio_route_for_components(components: &StudioPreparedComponents) -> &'static str {
    match (
        components.ha.is_some(),
        components.oiii.is_some(),
        components.sii.is_some(),
    ) {
        (true, true, true) => "monoNarrowbandThreeLine",
        (true, true, false) => "monoNarrowbandHaOiii",
        (false, true, true) => "monoNarrowbandSiiOiii",
        _ => "incompleteSpectralSet",
    }
}

fn studio_recommended_palette(components: &StudioPreparedComponents) -> Option<String> {
    if components.ha.is_some() && components.oiii.is_some() && components.sii.is_some() {
        Some("sho".into())
    } else if components.ha.is_some() && components.oiii.is_some() {
        Some("hooNatural".into())
    } else if components.sii.is_some() && components.oiii.is_some() {
        Some("soo".into())
    } else {
        None
    }
}

fn studio_compose_palette(
    components: &StudioPreparedComponents,
    definition: StudioPaletteDefinition,
) -> Result<(Vec<f32>, Vec<f32>), String> {
    let missing = definition
        .required
        .iter()
        .copied()
        .filter(|line| !studio_has_line(components, line))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "{} requiere {}; faltan {}.",
            definition.label,
            definition.required.join("+"),
            missing.join("+")
        ));
    }
    let pixels = components
        .width
        .checked_mul(components.height)
        .ok_or("Geometría de paleta fuera de rango")?;
    let mut output = vec![f32::NAN; pixels * 3];
    let mut coverage = vec![0.0f32; pixels];
    for pixel in 0..pixels {
        let required_coverage = definition
            .required
            .iter()
            .filter_map(|line| studio_line(components, line))
            .map(|line| line.coverage[pixel])
            .fold(1.0f32, f32::min);
        if required_coverage <= 0.0 {
            continue;
        }
        let value = |line: &str| {
            studio_line(components, line)
                .map(|plane| plane.data[pixel])
                .unwrap_or(f32::NAN)
        };
        let ha = value("HA");
        let oiii = value("OIII");
        let sii = value("SII");
        let rgb = match definition.id {
            "hooNatural" => [ha, 0.35 * ha + 0.65 * oiii, oiii],
            "hooTeal" => [ha, oiii, oiii],
            "hooSoft" => [ha, 0.50 * ha + 0.50 * oiii, 0.18 * ha + 0.82 * oiii],
            "hooGold" => [ha, 0.62 * ha + 0.38 * oiii, 0.88 * oiii],
            "sho" => [sii, ha, oiii],
            "shoBalanced" => [0.82 * sii + 0.18 * ha, ha, oiii],
            "hso" => [ha, sii, oiii],
            "soo" => [sii, oiii, oiii],
            _ => return Err(format!("Paleta '{}' no implementada", definition.id)),
        };
        if rgb.iter().all(|value| value.is_finite()) {
            output[pixel * 3..pixel * 3 + 3].copy_from_slice(&rgb);
            coverage[pixel] = required_coverage.clamp(0.0, 1.0);
        }
    }
    Ok((output, coverage))
}

fn studio_palette_quality(data: &[f32]) -> (f32, Vec<String>) {
    let pixels = data.len() / 3;
    if pixels == 0 {
        return (0.0, vec!["Sin píxeles válidos".into()]);
    }
    let stride = (pixels / 8192).max(1);
    let mut channels = [Vec::<f32>::new(), Vec::<f32>::new(), Vec::<f32>::new()];
    let mut finite_pixels = 0usize;
    let mut non_negative = 0usize;
    let mut sampled = 0usize;
    for pixel in (0..pixels).step_by(stride) {
        sampled += 1;
        let rgb = &data[pixel * 3..pixel * 3 + 3];
        if rgb.iter().all(|value| value.is_finite()) {
            finite_pixels += 1;
            if rgb.iter().all(|value| *value >= 0.0) {
                non_negative += 1;
            }
            for channel in 0..3 {
                channels[channel].push(rgb[channel]);
            }
        }
    }
    if finite_pixels < 8 {
        return (0.0, vec!["Cobertura finita insuficiente".into()]);
    }
    let percentile = |values: &mut Vec<f32>, fraction: f32| {
        values.sort_by(|a, b| a.total_cmp(b));
        let index = ((values.len() - 1) as f32 * fraction).round() as usize;
        values[index]
    };
    let mut signals = [0.0f32; 3];
    for channel in 0..3 {
        let p50 = percentile(&mut channels[channel], 0.50);
        let p995 = percentile(&mut channels[channel], 0.995);
        signals[channel] = (p995 - p50).max(0.0);
    }
    let max_signal = signals.iter().copied().fold(0.0f32, f32::max);
    let min_signal = signals.iter().copied().fold(f32::INFINITY, f32::min);
    let balance = if max_signal > f32::EPSILON {
        (min_signal / max_signal).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let finite_ratio = finite_pixels as f32 / sampled.max(1) as f32;
    let non_negative_ratio = non_negative as f32 / finite_pixels.max(1) as f32;
    let score =
        (55.0 * finite_ratio + 30.0 * balance.sqrt() + 15.0 * non_negative_ratio).clamp(0.0, 100.0);
    let mut reasons = vec![
        format!("Cobertura finita {:.0}%", finite_ratio * 100.0),
        format!("Equilibrio de señal RGB {:.0}%", balance * 100.0),
    ];
    if non_negative_ratio < 0.995 {
        reasons.push(format!(
            "{:.1}% de muestras requieren tratamiento de pedestal",
            (1.0 - non_negative_ratio) * 100.0
        ));
    } else {
        reasons.push("Sin recorte negativo relevante en la muestra".into());
    }
    (score, reasons)
}

fn studio_render_palette_preview(
    data: &[f32],
    width: usize,
    height: usize,
    max_edge: usize,
    tag: &str,
) -> Result<String, String> {
    let max_edge = max_edge.clamp(320, 1600);
    let factor = ((width.max(height) + max_edge - 1) / max_edge).max(1);
    let preview_width = ((width + factor - 1) / factor).max(1);
    let preview_height = ((height + factor - 1) / factor).max(1);
    let mut rgb16 = vec![0u16; preview_width * preview_height * 3];
    rgb16
        .par_chunks_mut(3)
        .enumerate()
        .for_each(|(preview_pixel, output)| {
            let px = preview_pixel % preview_width;
            let py = preview_pixel / preview_width;
            let x0 = px * factor;
            let y0 = py * factor;
            let x1 = (x0 + factor).min(width);
            let y1 = (y0 + factor).min(height);
            for channel in 0..3 {
                let mut sum = 0.0f64;
                let mut count = 0usize;
                for y in y0..y1 {
                    for x in x0..x1 {
                        let value = data[(y * width + x) * 3 + channel];
                        if value.is_finite() {
                            sum += value as f64;
                            count += 1;
                        }
                    }
                }
                output[channel] = if count > 0 {
                    (sum / count as f64).clamp(0.0, 65535.0) as u16
                } else {
                    0
                };
            }
        });
    // Todas las tarjetas usan exactamente el mismo contrato STF ligado. Así
    // la paleta, no un balance por canal distinto, explica la comparación.
    let stretched = ds_render_stretch(&rgb16, preview_width, preview_height, false, 2.8, 0.25);
    let mut rgba = Vec::with_capacity(preview_width * preview_height * 4);
    for pixel in 0..preview_width * preview_height {
        rgba.extend_from_slice(&[
            stretched[pixel * 3],
            stretched[pixel * 3 + 1],
            stretched[pixel * 3 + 2],
            255,
        ]);
    }
    let image = RgbaImage::from_raw(preview_width as u32, preview_height as u32, rgba)
        .ok_or("No se pudo construir la vista de paleta")?;
    let mut encoded = Vec::new();
    DynamicImage::ImageRgba8(image)
        .write_to(&mut Cursor::new(&mut encoded), image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(save_preview_png_to_temp(&encoded, tag).unwrap_or_else(|| {
        format!(
            "data:image/png;base64,{}",
            general_purpose::STANDARD.encode(&encoded)
        )
    }))
}

fn studio_requested_definitions(
    requested: &[String],
) -> Result<Vec<StudioPaletteDefinition>, String> {
    if requested.is_empty() {
        return Ok(STUDIO_PALETTE_DEFINITIONS.to_vec());
    }
    let mut definitions = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for id in requested {
        let definition =
            studio_palette_definition(id).ok_or_else(|| format!("Paleta desconocida '{id}'"))?;
        if seen.insert(definition.id) {
            definitions.push(definition);
        }
    }
    Ok(definitions)
}

fn studio_gallery_from_components(
    components: StudioPreparedComponents,
    requested: &[String],
    preview_max_edge: usize,
) -> Result<DeepSkyStudioPaletteGallery, String> {
    let definitions = studio_requested_definitions(requested)?;
    let mut candidates = Vec::with_capacity(definitions.len());
    for definition in definitions {
        let missing = definition
            .required
            .iter()
            .copied()
            .filter(|line| !studio_has_line(&components, line))
            .collect::<Vec<_>>();
        let (eligible, reason, preview, score, score_reasons) = if missing.is_empty() {
            let (data, _) = studio_compose_palette(&components, definition)?;
            let (score, score_reasons) = studio_palette_quality(&data);
            let preview = studio_render_palette_preview(
                &data,
                components.width,
                components.height,
                preview_max_edge,
                &format!("deepsky-studio-{}", definition.id),
            )?;
            (true, None, Some(preview), Some(score), score_reasons)
        } else {
            (
                false,
                Some(format!("Faltan {}", missing.join("+"))),
                None,
                None,
                vec![format!("No evaluada: faltan {}", missing.join("+"))],
            )
        };
        candidates.push(DeepSkyStudioPaletteCandidate {
            id: definition.id.into(),
            label: definition.label.into(),
            eligible,
            reason,
            preview,
            scientific_class: components.scientific_class.clone(),
            mapping: definition.mapping.into(),
            required_lines: definition
                .required
                .iter()
                .map(|line| line.to_string())
                .collect(),
            score,
            score_reasons,
            intent: definition.intent.into(),
        });
    }
    let recommended_palette_id = candidates
        .iter()
        .filter(|candidate| candidate.eligible)
        .filter_map(|candidate| candidate.score.map(|score| (candidate.id.clone(), score)))
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(id, _)| id)
        .or_else(|| studio_recommended_palette(&components));
    let reconciliations = [
        components.ha.as_ref(),
        components.oiii.as_ref(),
        components.sii.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(|line| line.reconciliation.clone())
    .collect::<Vec<_>>();
    let oiii_reconciliation = components
        .oiii
        .as_ref()
        .map(|line| line.reconciliation.clone());
    Ok(DeepSkyStudioPaletteGallery {
        route: components.route.clone(),
        width: components.width,
        height: components.height,
        masters_immutable: true,
        recommended_palette_id,
        registrations: components.registrations.clone(),
        oiii_reconciliation,
        line_reconciliations: reconciliations,
        candidates,
        limitations: components.limitations.clone(),
        preview_contract: DeepSkyStudioPreviewContract {
            linked: true,
            shadow_sigma: 2.8,
            target_midtone: 0.25,
            max_edge: preview_max_edge.clamp(320, 1600),
        },
    })
}

#[tauri::command(async)]
fn deepsky_studio_palette_gallery(
    request: DeepSkyStudioPaletteRequest,
) -> Result<DeepSkyStudioPaletteGallery, String> {
    let components = studio_prepare_file_components(&request)?;
    studio_gallery_from_components(components, &request.palette_ids, request.preview_max_edge)
}

#[tauri::command(async)]
fn deepsky_studio_active_dualband_gallery(
    state: State<'_, AppState>,
    request: Option<DeepSkyStudioPaletteRequest>,
) -> Result<DeepSkyStudioPaletteGallery, String> {
    let request = request.unwrap_or_default();
    let components = studio_prepare_active_components(&state, &request)?;
    studio_gallery_from_components(components, &request.palette_ids, request.preview_max_edge)
}

fn studio_result_recipe(
    components: &StudioPreparedComponents,
    definition: StudioPaletteDefinition,
    request: &DeepSkyStudioPaletteRequest,
) -> serde_json::Value {
    let reconciliations = [
        components.ha.as_ref(),
        components.oiii.as_ref(),
        components.sii.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(|line| line.reconciliation.clone())
    .collect::<Vec<_>>();
    serde_json::json!({
        "schemaVersion": "zenith-deepsky-studio-palette-v1",
        "operation": "linearNarrowbandPalette",
        "palette": {
            "id": definition.id,
            "label": definition.label,
            "mapping": definition.mapping,
            "requiredLines": definition.required,
        },
        "route": components.route,
        "scientificClass": components.scientific_class,
        "quantitativeLineFlux": false,
        "inputFilesImmutable": true,
        "sourcePaths": components.source_paths,
        "instrumentProfile": request.instrument_profile,
        "registrationEnabled": request.register,
        "astrometryPreserved": components.astrometry_solution.is_some(),
        "registrations": components.registrations,
        "lineReconciliations": reconciliations,
        "dualBandExtraction": {
            "oiiiGreenWeight": request.oiii_green_weight.clamp(0.0, 1.0),
            "crosstalkSuppression": request.crosstalk_suppression.clamp(0.0, 1.0),
        },
        "limitations": components.limitations,
    })
}

fn studio_install_derived_result(
    state: &AppState,
    derived: DeepSkyResult,
    stacked: StackResult,
) -> Result<Option<String>, String> {
    // Mismo orden de locks que `deepsky_select_product`: active → products →
    // result → preview. Así un cambio concurrente no puede crear un interbloqueo
    // ni hacer desaparecer el producto fuente.
    let mut active = state
        .deep_sky_active_product
        .lock()
        .map_err(|_| "No se pudo bloquear el producto activo")?;
    let mut products = state
        .deep_sky_products
        .lock()
        .map_err(|_| "No se pudo bloquear el almacén de productos")?;
    let mut current = state
        .deep_sky_result
        .lock()
        .map_err(|_| "No se pudo bloquear el máster lineal")?;
    let mut current_preview = state
        .stacked_image
        .lock()
        .map_err(|_| "No se pudo bloquear la vista activa")?;
    let preserved_id = if let Some(previous) = current.as_ref() {
        let id = active
            .as_ref()
            .cloned()
            .unwrap_or_else(|| format!("studio-source:{}", previous.id));
        if products.contains_key(&id) {
            return Err(format!(
                "El producto fuente '{id}' ya existe en el almacén; se bloqueó la paleta para no sobrescribirlo."
            ));
        }
        let cache_root = std::env::temp_dir().join("zenith-deepsky-studio");
        let entry = ds_spill_product_to_disk(&cache_root, &id, previous)?;
        products.insert(id.clone(), entry);
        Some(id)
    } else {
        None
    };
    let derived_id = derived.id.clone();
    *current = Some(derived);
    *current_preview = Some(stacked);
    *active = Some(derived_id);
    drop(current_preview);
    drop(current);
    drop(products);
    drop(active);
    *state
        .processed_image
        .lock()
        .map_err(|_| "No se pudo invalidar la vista procesada")? = None;
    state.deconv_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    state.wavelet_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    state.filter_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    Ok(preserved_id)
}

#[tauri::command(async)]
fn deepsky_studio_apply_palette(
    state: State<'_, AppState>,
    request: DeepSkyStudioApplyPaletteRequest,
) -> Result<DeepSkyStudioPaletteApplyResult, String> {
    let definition = studio_palette_definition(&request.palette_id)
        .ok_or_else(|| format!("Paleta desconocida '{}'", request.palette_id))?;
    let has_file_sources = !request.sources.ha_paths.is_empty()
        || !request.sources.oiii_paths.is_empty()
        || !request.sources.sii_paths.is_empty();
    let components = if has_file_sources {
        studio_prepare_file_components(&request.sources)?
    } else {
        studio_prepare_active_components(&state, &request.sources)?
    };
    let (data, coverage) = studio_compose_palette(&components, definition)?;
    let route = components.route.clone();
    let scientific_class = components.scientific_class.clone();
    let limitations = components.limitations.clone();
    let astrometry_solution = components.astrometry_solution.clone();
    let pixels = components.width * components.height;
    let (width, height) = (components.width, components.height);
    let product_id = new_job_id(&format!(
        "studio-palette-{}",
        definition.id.to_ascii_lowercase()
    ));
    let recipe = studio_result_recipe(&components, definition, &request.sources);
    // Los planos Ha/OIII/SII ya no son necesarios después de componer. Soltar
    // estas copias antes de respaldar/publicar el producto activo evita sumar
    // tres másteres MONO al pico de RAM de una sesión de 60 MP.
    drop(components);
    let result = DeepSkyResult {
        id: product_id.clone(),
        data,
        width,
        height,
        channels: 3,
        coverage: coverage.clone(),
        weight: coverage,
        rejection_low: vec![0.0; pixels],
        rejection_high: vec![0.0; pixels],
        registration_residuals: vec![0.0; pixels],
        engine: "DeepSky Studio Palette".into(),
        method: definition.id.into(),
        frames_used: 0,
        frames_rejected: 0,
        elapsed_seconds: 0.0,
        recipe: recipe.clone(),
        variance: None,
        neff: None,
        dq: None,
        struct_map: None,
        struct_residual: None,
        recoverability: None,
        source_data: None,
        source_variance: None,
        source_layout: None,
        post_stack_recipe: PostStackRecipe::default(),
        astrometry_solution,
    };
    let (stacked, preview) = ds_preview_for_linear_result(&result, "deepsky-studio-palette")?;
    let state_result = ds_poststack_state_for(&result, preview.clone());
    let preserved_product_id = studio_install_derived_result(&state, result, stacked)?;
    Ok(DeepSkyStudioPaletteApplyResult {
        state: state_result,
        preview: preview.clone(),
        descriptor: DeepSkyStudioPaletteDescriptor {
            product_id,
            palette_id: definition.id.into(),
            route,
            scientific_class,
            quantitative_line_flux: false,
            input_files_immutable: true,
            preserved_product_id,
            limitations,
        },
        recipe,
    })
}

#[cfg(test)]
mod deepsky_studio_palette_tests {
    use super::*;

    fn synthetic_signal(width: usize, height: usize) -> Vec<f32> {
        (0..width * height)
            .map(|index| {
                let x = (index % width) as f32;
                let y = (index / width) as f32;
                800.0
                    + 2.5 * x
                    + 1.75 * y
                    + if (x - 19.0).powi(2) + (y - 23.0).powi(2) < 30.0 {
                        1400.0
                    } else {
                        0.0
                    }
            })
            .collect()
    }

    fn synthetic_plane(
        path: &str,
        base: &[f32],
        scale: f32,
        offset: f32,
        noise: f32,
    ) -> StudioAlignedPlane {
        let data = base
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let deterministic = match index % 4 {
                    0 => -1.0,
                    1 => 0.5,
                    2 => 1.0,
                    _ => -0.5,
                };
                value * scale + offset + noise * deterministic
            })
            .collect::<Vec<_>>();
        StudioAlignedPlane {
            path: path.into(),
            coverage: vec![1.0; base.len()],
            data,
        }
    }

    #[test]
    fn duplicate_oiii_is_robustly_normalized_and_inverse_noise_weighted() {
        let (width, height) = (64, 48);
        let base = synthetic_signal(width, height);
        let quiet = synthetic_plane("oiii-quiet.fits", &base, 1.0, 0.0, 2.0);
        let noisy_scaled = synthetic_plane("oiii-noisy.fits", &base, 1.8, 310.0, 18.0);
        let reconciled =
            studio_reconcile_line("OIII", vec![quiet, noisy_scaled], width, height).unwrap();
        assert_eq!(reconciled.reconciliation.input_count, 2);
        assert_eq!(
            reconciled.reconciliation.method,
            "robustQ16Q50Q84Normalization+inverseNoiseVarianceWeighting"
        );
        let quiet_weight = reconciled.reconciliation.inputs[0].normalized_weight;
        let noisy_weight = reconciled.reconciliation.inputs[1].normalized_weight;
        assert!(
            quiet_weight > noisy_weight,
            "la entrada menos ruidosa debe pesar más: {quiet_weight} vs {noisy_weight}"
        );
        let mean_error = reconciled
            .data
            .iter()
            .zip(&base)
            .map(|(actual, expected)| (actual - expected).abs())
            .sum::<f32>()
            / base.len() as f32;
        assert!(mean_error < 6.0, "error medio inesperado: {mean_error}");
        assert!(reconciled.coverage.iter().all(|value| *value > 0.99));
    }

    fn component(line: &'static str, value: f32, pixels: usize) -> StudioLinePlane {
        StudioLinePlane {
            data: vec![value; pixels],
            coverage: vec![1.0; pixels],
            reconciliation: DeepSkyStudioLineReconciliation {
                line: line.into(),
                input_count: 1,
                method: "synthetic".into(),
                combined_noise_sigma_adu: 1.0,
                coverage_percent: 100.0,
                inputs: Vec::new(),
            },
        }
    }

    #[test]
    fn palette_matrices_are_linear_and_do_not_mutate_sources() {
        let mut components = StudioPreparedComponents {
            width: 4,
            height: 3,
            ha: Some(component("HA", 10.0, 12)),
            oiii: Some(component("OIII", 20.0, 12)),
            sii: Some(component("SII", 30.0, 12)),
            ..StudioPreparedComponents::default()
        };
        let original_ha = components.ha.as_ref().unwrap().data.clone();
        let original_oiii = components.oiii.as_ref().unwrap().data.clone();
        let original_sii = components.sii.as_ref().unwrap().data.clone();

        let sho = studio_palette_definition("sho").unwrap();
        let (data, coverage) = studio_compose_palette(&components, sho).unwrap();
        assert_eq!(&data[..3], &[30.0, 10.0, 20.0]);
        assert!(coverage.iter().all(|value| *value == 1.0));

        let hoo = studio_palette_definition("hooNatural").unwrap();
        let (data, _) = studio_compose_palette(&components, hoo).unwrap();
        assert_eq!(data[0], 10.0);
        assert!((data[1] - 16.5).abs() < 1.0e-6);
        assert_eq!(data[2], 20.0);
        assert_eq!(components.ha.take().unwrap().data, original_ha);
        assert_eq!(components.oiii.take().unwrap().data, original_oiii);
        assert_eq!(components.sii.take().unwrap().data, original_sii);
    }

    #[test]
    fn unavailable_palettes_fail_closed_and_foraxx_is_not_aliased() {
        let components = StudioPreparedComponents {
            width: 2,
            height: 2,
            ha: Some(component("HA", 10.0, 4)),
            oiii: Some(component("OIII", 20.0, 4)),
            ..StudioPreparedComponents::default()
        };
        assert!(
            studio_compose_palette(&components, studio_palette_definition("sho").unwrap()).is_err()
        );
        assert!(studio_palette_definition("foraxx").is_none());
        assert_eq!(
            studio_recommended_palette(&components).as_deref(),
            Some("hooNatural")
        );
    }
}
