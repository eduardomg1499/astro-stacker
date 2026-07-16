//! Primitivas planetarias puras y testeables.
//!
//! Mantener aquí los contratos numéricos evita que preview, CPU y GPU terminen
//! reimplementando heurísticas distintas dentro del enorme `include!` de
//! commands_v2_v3.rs.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

const MIN_FRAME_WEIGHT: f32 = 0.10;

// `HashMap<frame, Vec<bool>>` bit-packs every individual Vec, but still creates
// one heap allocation per selected frame and carries allocator/capacity
// overhead thousands of times.  This matrix owns one contiguous allocation
// and one fallible frame->row index.  Untouched rows deliberately remain
// distinguishable from rows whose AP bits are all false: callers historically
// interpret a missing row as "no local gate".
const ACCEPTANCE_ROW_INDEX_ESTIMATE_BYTES: usize = 48;

#[derive(Debug)]
pub struct CompactFrameAcceptance {
    ap_count: usize,
    words_per_row: usize,
    frame_ids: Vec<usize>,
    row_by_frame: HashMap<usize, usize>,
    touched: Vec<u8>,
    bits: Vec<u64>,
    estimated_bytes: usize,
}

impl CompactFrameAcceptance {
    pub fn try_new<I>(
        frame_ids: I,
        frame_count: usize,
        ap_count: usize,
        memory_budget_bytes: usize,
    ) -> Result<Self, String>
    where
        I: IntoIterator<Item = usize>,
    {
        let words_per_row = ap_count
            .checked_add(63)
            .ok_or("La cantidad de APs desborda la matriz de aceptación")?
            / 64;
        let bit_words = frame_count
            .checked_mul(words_per_row)
            .ok_or("La matriz de aceptación AP excede el espacio direccionable")?;
        let bit_bytes = bit_words
            .checked_mul(std::mem::size_of::<u64>())
            .ok_or("La matriz de aceptación AP excede el espacio direccionable")?;
        let row_metadata = frame_count
            .checked_mul(
                std::mem::size_of::<usize>()
                    + std::mem::size_of::<u8>()
                    + ACCEPTANCE_ROW_INDEX_ESTIMATE_BYTES,
            )
            .ok_or("Los índices de la matriz AP exceden el espacio direccionable")?;
        let estimated_bytes = bit_bytes
            .checked_add(row_metadata)
            .ok_or("El presupuesto de la matriz AP se desbordó")?;
        if estimated_bytes > memory_budget_bytes {
            return Err(format!(
                "La selección local necesita aproximadamente {} MB para su matriz AP compacta, pero el presupuesto seguro actual es {} MB. Reduce puntos AP/frames o libera RAM.",
                estimated_bytes.div_ceil(1024 * 1024),
                memory_budget_bytes / (1024 * 1024),
            ));
        }

        let mut stored_ids = Vec::new();
        stored_ids.try_reserve_exact(frame_count).map_err(|error| {
            format!("No se pudo reservar el índice de frames de la matriz AP: {error}")
        })?;
        let mut row_by_frame = HashMap::new();
        row_by_frame.try_reserve(frame_count).map_err(|error| {
            format!("No se pudo reservar el mapa de frames de la matriz AP: {error}")
        })?;
        for frame_id in frame_ids {
            let row = stored_ids.len();
            if row >= frame_count {
                return Err("El análisis contiene más filas de frame que las declaradas".into());
            }
            if row_by_frame.insert(frame_id, row).is_some() {
                return Err(format!(
                    "El análisis contiene el frame {frame_id} duplicado; vuelve a analizar el video"
                ));
            }
            stored_ids.push(frame_id);
        }
        if stored_ids.len() != frame_count {
            return Err(format!(
                "El análisis declara {frame_count} frames pero contiene {} índices",
                stored_ids.len()
            ));
        }

        let mut touched = Vec::new();
        touched
            .try_reserve_exact(frame_count)
            .map_err(|error| format!("No se pudo reservar el estado de la matriz AP: {error}"))?;
        touched.resize(frame_count, 0);
        let mut bits = Vec::new();
        bits.try_reserve_exact(bit_words).map_err(|error| {
            format!(
                "No se pudo reservar la matriz AP compacta de {} MB: {error}",
                bit_bytes.div_ceil(1024 * 1024)
            )
        })?;
        bits.resize(bit_words, 0);

        Ok(Self {
            ap_count,
            words_per_row,
            frame_ids: stored_ids,
            row_by_frame,
            touched,
            bits,
            estimated_bytes,
        })
    }

    #[inline]
    pub fn ap_count(&self) -> usize {
        self.ap_count
    }

    #[inline]
    pub fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    #[cfg(test)]
    #[inline]
    pub fn touched_len(&self) -> usize {
        self.touched
            .iter()
            .map(|&value| usize::from(value != 0))
            .sum()
    }

    #[inline]
    fn row(&self, frame_id: usize) -> Option<usize> {
        self.row_by_frame.get(&frame_id).copied()
    }

    pub fn set_accepted(&mut self, frame_id: usize, ap: usize) -> Result<(), String> {
        let row = self
            .row(frame_id)
            .ok_or_else(|| format!("El frame {frame_id} no existe en la matriz AP"))?;
        if ap >= self.ap_count {
            return Err(format!(
                "El AP {ap} excede los {} puntos de la matriz",
                self.ap_count
            ));
        }
        self.touched[row] = 1;
        self.bits[row * self.words_per_row + ap / 64] |= 1u64 << (ap % 64);
        Ok(())
    }

    pub fn set_all_accepted(&mut self, frame_id: usize) -> Result<(), String> {
        let row = self
            .row(frame_id)
            .ok_or_else(|| format!("El frame {frame_id} no existe en la matriz AP"))?;
        self.touched[row] = 1;
        if self.words_per_row == 0 {
            return Ok(());
        }
        let start = row * self.words_per_row;
        self.bits[start..start + self.words_per_row].fill(u64::MAX);
        let remainder = self.ap_count % 64;
        if remainder != 0 {
            self.bits[start + self.words_per_row - 1] = (1u64 << remainder) - 1;
        }
        Ok(())
    }

    /// `None` preserves the legacy meaning of an absent frame mask.
    #[inline]
    pub fn accepts(&self, frame_id: usize, ap: usize) -> Option<bool> {
        let row = self.row(frame_id)?;
        if self.touched[row] == 0 {
            return None;
        }
        if ap >= self.ap_count {
            return Some(false);
        }
        Some(self.bits[row * self.words_per_row + ap / 64] & (1u64 << (ap % 64)) != 0)
    }

    fn for_each_accepted(&self, frame_id: usize, visit: &mut dyn FnMut(usize)) {
        let Some(row) = self.row(frame_id) else {
            return;
        };
        if self.touched[row] == 0 {
            return;
        }
        let start = row * self.words_per_row;
        for (word_index, &stored_word) in self.bits[start..start + self.words_per_row]
            .iter()
            .enumerate()
        {
            let mut word = stored_word;
            while word != 0 {
                let bit = word.trailing_zeros() as usize;
                let ap = word_index * 64 + bit;
                if ap < self.ap_count {
                    visit(ap);
                }
                word &= word - 1;
            }
        }
    }

    /// Compacta las columnas en el mismo buffer. Sólo usa una fila temporal,
    /// de modo que filtrar APs de cielo no duplica una matriz potencialmente
    /// grande en el pico de RAM.
    pub fn remap_aps_in_place(&mut self, keep: &[usize]) -> Result<(), String> {
        let mut previous = None;
        for &old_ap in keep {
            if old_ap >= self.ap_count {
                return Err(format!(
                    "No se puede conservar el AP {old_ap}; la matriz sólo tiene {}",
                    self.ap_count
                ));
            }
            if previous.is_some_and(|value| old_ap <= value) {
                return Err("Los APs a conservar deben ser únicos y estar ordenados".into());
            }
            previous = Some(old_ap);
        }
        if keep.len() == self.ap_count {
            return Ok(());
        }

        let old_words = self.words_per_row;
        let new_ap_count = keep.len();
        let new_words = new_ap_count.div_ceil(64);
        if new_words == 0 {
            self.bits.clear();
            self.ap_count = 0;
            self.words_per_row = 0;
            self.estimated_bytes = self.frame_ids.len()
                * (std::mem::size_of::<usize>()
                    + std::mem::size_of::<u8>()
                    + ACCEPTANCE_ROW_INDEX_ESTIMATE_BYTES);
            return Ok(());
        }

        let mut row_scratch = Vec::new();
        row_scratch.try_reserve_exact(new_words).map_err(|error| {
            format!("No se pudo reservar el scratch para compactar APs: {error}")
        })?;
        row_scratch.resize(new_words, 0u64);
        for row in 0..self.frame_ids.len() {
            row_scratch.fill(0);
            if self.touched[row] != 0 {
                let source_start = row * old_words;
                for (new_ap, &old_ap) in keep.iter().enumerate() {
                    if self.bits[source_start + old_ap / 64] & (1u64 << (old_ap % 64)) != 0 {
                        row_scratch[new_ap / 64] |= 1u64 << (new_ap % 64);
                    }
                }
            }
            let destination_start = row * new_words;
            self.bits[destination_start..destination_start + new_words]
                .copy_from_slice(&row_scratch);
        }
        self.bits.truncate(self.frame_ids.len() * new_words);
        self.ap_count = new_ap_count;
        self.words_per_row = new_words;
        self.estimated_bytes = self.bits.len() * std::mem::size_of::<u64>()
            + self.frame_ids.len()
                * (std::mem::size_of::<usize>()
                    + std::mem::size_of::<u8>()
                    + ACCEPTANCE_ROW_INDEX_ESTIMATE_BYTES);
        Ok(())
    }
}

#[inline]
fn logistic(x: f32) -> f32 {
    1.0 / (1.0 + (-x.clamp(-60.0, 60.0)).exp())
}

/// Peso sigmoidal basado en la CALIDAD medida, no en la posición ordinal.
/// Dos frames con el mismo score reciben exactamente el mismo peso; pequeñas
/// permutaciones del ranking ya no fuerzan artificialmente 1.0 frente a 0.1.
pub fn sigmoidal_frame_weight(
    score: u64,
    reference_score: u64,
    center_ratio: f32,
    steepness: f32,
) -> f32 {
    if reference_score == 0 {
        return 1.0;
    }
    let q = (score as f64 / reference_score as f64).clamp(0.0, 1.0) as f32;
    let center = center_ratio.clamp(0.50, 0.99);
    let k = steepness.clamp(1.0, 30.0);
    let raw = logistic(k * (q - center));
    let best = logistic(k * (1.0 - center)).max(1e-6);
    let normalized = (raw / best).clamp(0.0, 1.0);
    MIN_FRAME_WEIGHT + (1.0 - MIN_FRAME_WEIGHT) * normalized
}

/// Referencia superior robusta para la ponderacion sigmoidal.
///
/// El maximo de una serie puede ser un unico frame corrupto (hot pixels,
/// compresion o una nube con borde duro) y, usado como denominador, empuja el
/// peso de todos los frames sanos hacia el minimo. Esta referencia calcula el
/// promedio de la cola P95..P100 despues de winsorizarla en P99. Los
/// percentiles usan rango inferior deliberadamente: con una sola observacion
/// extrema, P99 sigue siendo el mejor valor *no* atipico, incluso en lotes
/// pequenos. Los scores por encima de la referencia simplemente saturan a 1 en
/// `sigmoidal_frame_weight`.
pub fn robust_quality_reference(scores: &[u64]) -> u64 {
    if scores.is_empty() {
        return 0;
    }
    if scores.len() == 1 {
        return scores[0];
    }

    let mut sorted = scores.to_vec();
    sorted.sort_unstable();
    let last = sorted.len() - 1;
    let p95_idx = ((last as f64) * 0.95).floor() as usize;
    let p99_idx = ((last as f64) * 0.99).floor() as usize;
    let cap = sorted[p99_idx];
    let tail = &sorted[p95_idx..];
    let sum: u128 = tail.iter().map(|&score| score.min(cap) as u128).sum();
    let count = tail.len() as u128;
    ((sum + count / 2) / count).min(u64::MAX as u128) as u64
}

/// Recorta la union global de frames sin dejar ningun punto de alineacion por
/// debajo de su cobertura minima.
///
/// Primero construye un nucleo de cobertura mediante set-multicover voraz: en
/// cada paso elige el frame que satisface mas APs aun incompletos y desempata
/// por score. Esto evita el fallo del simple `truncate(score)`: un frame que
/// cubre muchos APs puede ser mas importante que varios frames globalmente
/// mejores pero redundantes. Una vez cubierto cada AP, rellena hasta
/// `max_frames` con los scores globales mas altos. Si el limite es
/// matematicamente incompatible con `min_keep_per_ap`, se conserva la cobertura
/// y se devuelve el nucleo minimo encontrado aunque exceda el limite.
pub fn coverage_aware_frame_trim(
    scored_frames: &[(usize, u64)],
    acceptance_masks: &HashMap<usize, Vec<bool>>,
    max_frames: usize,
    min_keep_per_ap: usize,
) -> HashSet<usize> {
    if scored_frames.is_empty() {
        return HashSet::new();
    }

    // Defensive deduplication: a frame index is an identity, and duplicated
    // cache rows must not count twice toward AP coverage.
    let mut score_by_id = HashMap::<usize, u64>::with_capacity(scored_frames.len());
    for &(id, score) in scored_frames {
        score_by_id
            .entry(id)
            .and_modify(|stored| *stored = (*stored).max(score))
            .or_insert(score);
    }
    let mut candidates: Vec<(usize, u64)> = score_by_id.into_iter().collect();
    candidates.sort_unstable_by_key(|&(id, _)| id);

    let ap_count = candidates
        .iter()
        .filter_map(|(id, _)| acceptance_masks.get(id).map(Vec::len))
        .max()
        .unwrap_or(0);
    let mut available = vec![0usize; ap_count];
    for &(id, _) in &candidates {
        if let Some(mask) = acceptance_masks.get(&id) {
            for (ap, &covered) in mask.iter().take(ap_count).enumerate() {
                available[ap] += covered as usize;
            }
        }
    }
    // Si un AP ya llego con menos de min_keep desde la seleccion local, el
    // recorte no puede inventar cobertura: preserva todo lo que ese AP tenia.
    let mut unmet: Vec<usize> = available
        .iter()
        .map(|&count| count.min(min_keep_per_ap))
        .collect();
    let mut unmet_total: usize = unmet.iter().sum();

    // (ganancia de cobertura, score, id estable, indice de candidato).
    // Las ganancias obsoletas se recalculan al extraerlas (priority queue lazy),
    // evitando el O(frames²*APs) de volver a puntuar todos los frames por paso.
    let mut heap = BinaryHeap::<(usize, u64, Reverse<usize>, usize)>::new();
    for (candidate_idx, &(id, score)) in candidates.iter().enumerate() {
        let gain = acceptance_masks
            .get(&id)
            .map(|mask| {
                mask.iter()
                    .zip(unmet.iter())
                    .filter(|(covered, need)| **covered && **need > 0)
                    .count()
            })
            .unwrap_or(0);
        heap.push((gain, score, Reverse(id), candidate_idx));
    }

    let mut selected = vec![false; candidates.len()];
    let mut selected_count = 0usize;
    while unmet_total > 0 {
        let Some((cached_gain, score, stable_id, candidate_idx)) = heap.pop() else {
            break;
        };
        if selected[candidate_idx] {
            continue;
        }
        let id = candidates[candidate_idx].0;
        let current_gain = acceptance_masks
            .get(&id)
            .map(|mask| {
                mask.iter()
                    .zip(unmet.iter())
                    .filter(|(covered, need)| **covered && **need > 0)
                    .count()
            })
            .unwrap_or(0);
        if current_gain != cached_gain {
            heap.push((current_gain, score, stable_id, candidate_idx));
            continue;
        }
        if current_gain == 0 {
            break;
        }

        selected[candidate_idx] = true;
        selected_count += 1;
        if let Some(mask) = acceptance_masks.get(&id) {
            for (ap, &covered) in mask.iter().take(ap_count).enumerate() {
                if covered && unmet[ap] > 0 {
                    unmet[ap] -= 1;
                    unmet_total -= 1;
                }
            }
        }
    }

    // El greedy puede cubrir de rebote un AP ya satisfecho al completar otro.
    // Retira esos frames redundantes (peor score primero) antes de rellenar el
    // cap; el nucleo resultante es inclusion-minimo y libera mas slots para los
    // mejores frames globales sin perder una sola toma requerida por AP.
    let required: Vec<usize> = available
        .iter()
        .map(|&count| count.min(min_keep_per_ap))
        .collect();
    let mut selected_coverage = vec![0usize; ap_count];
    for (candidate_idx, &(id, _)) in candidates.iter().enumerate() {
        if !selected[candidate_idx] {
            continue;
        }
        if let Some(mask) = acceptance_masks.get(&id) {
            for (ap, &covered) in mask.iter().take(ap_count).enumerate() {
                selected_coverage[ap] += covered as usize;
            }
        }
    }
    let mut removal_order: Vec<usize> =
        (0..candidates.len()).filter(|&idx| selected[idx]).collect();
    removal_order.sort_unstable_by(|&a, &b| {
        candidates[a]
            .1
            .cmp(&candidates[b].1)
            .then_with(|| candidates[b].0.cmp(&candidates[a].0))
    });
    for candidate_idx in removal_order {
        let id = candidates[candidate_idx].0;
        let removable = acceptance_masks.get(&id).is_none_or(|mask| {
            mask.iter()
                .zip(selected_coverage.iter().zip(required.iter()))
                .all(|(covered, (have, need))| !*covered || *have > *need)
        });
        if removable {
            selected[candidate_idx] = false;
            selected_count -= 1;
            if let Some(mask) = acceptance_masks.get(&id) {
                for (ap, &covered) in mask.iter().take(ap_count).enumerate() {
                    selected_coverage[ap] -= covered as usize;
                }
            }
        }
    }

    // La cobertura tiene prioridad sobre el cap. Si el nucleo cabe, los slots
    // restantes recuperan la seleccion por calidad global.
    let target = max_frames.max(selected_count).min(candidates.len());
    let mut quality_order: Vec<usize> = (0..candidates.len()).collect();
    quality_order.sort_unstable_by(|&a, &b| {
        candidates[b]
            .1
            .cmp(&candidates[a].1)
            .then_with(|| candidates[a].0.cmp(&candidates[b].0))
    });
    for candidate_idx in quality_order {
        if selected_count >= target {
            break;
        }
        if !selected[candidate_idx] {
            selected[candidate_idx] = true;
            selected_count += 1;
        }
    }

    candidates
        .iter()
        .enumerate()
        .filter_map(|(idx, &(id, _))| selected[idx].then_some(id))
        .collect()
}

fn try_filled<T: Clone>(len: usize, value: T, label: &str) -> Result<Vec<T>, String> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(len)
        .map_err(|error| format!("No se pudo reservar {label}: {error}"))?;
    values.resize(len, value);
    Ok(values)
}

/// Variante de producción para la matriz compacta. Mantiene la misma política
/// multicoverage que `coverage_aware_frame_trim`, pero recorre únicamente los
/// bits activos, hace fallibles todas las reservas proporcionales a frames/APs
/// y permite cancelar incluso durante el greedy.
pub fn coverage_aware_frame_trim_compact<C>(
    scored_frames: &[(usize, u64)],
    acceptance: &CompactFrameAcceptance,
    max_frames: usize,
    min_keep_per_ap: usize,
    cancelled: C,
) -> Result<HashSet<usize>, String>
where
    C: Fn() -> bool,
{
    if scored_frames.is_empty() {
        return Ok(HashSet::new());
    }
    if cancelled() {
        return Err("Selección local cancelada o sustituida".into());
    }

    let mut score_by_id = HashMap::<usize, u64>::new();
    score_by_id
        .try_reserve(scored_frames.len())
        .map_err(|error| {
            format!("No se pudo reservar el índice del recorte por cobertura: {error}")
        })?;
    for (position, &(id, score)) in scored_frames.iter().enumerate() {
        if position & 0x3ff == 0 && cancelled() {
            return Err("Selección local cancelada o sustituida".into());
        }
        score_by_id
            .entry(id)
            .and_modify(|stored| *stored = (*stored).max(score))
            .or_insert(score);
    }
    let mut candidates = Vec::new();
    candidates
        .try_reserve_exact(score_by_id.len())
        .map_err(|error| format!("No se pudo reservar la lista de cobertura AP: {error}"))?;
    candidates.extend(score_by_id);
    candidates.sort_unstable_by_key(|&(id, _)| id);

    let ap_count = acceptance.ap_count();
    let mut available = try_filled(ap_count, 0usize, "cobertura disponible por AP")?;
    for (position, &(id, _)) in candidates.iter().enumerate() {
        if position & 0xff == 0 && cancelled() {
            return Err("Selección local cancelada o sustituida".into());
        }
        acceptance.for_each_accepted(id, &mut |ap| available[ap] += 1);
    }
    let mut unmet = Vec::new();
    unmet
        .try_reserve_exact(ap_count)
        .map_err(|error| format!("No se pudo reservar la cobertura mínima AP: {error}"))?;
    unmet.extend(available.iter().map(|&count| count.min(min_keep_per_ap)));
    let mut unmet_total: usize = unmet.iter().sum();

    let mut heap = BinaryHeap::<(usize, u64, Reverse<usize>, usize)>::new();
    heap.try_reserve(candidates.len())
        .map_err(|error| format!("No se pudo reservar la cola de cobertura AP: {error}"))?;
    for (candidate_idx, &(id, score)) in candidates.iter().enumerate() {
        if candidate_idx & 0xff == 0 && cancelled() {
            return Err("Selección local cancelada o sustituida".into());
        }
        let mut gain = 0usize;
        acceptance.for_each_accepted(id, &mut |ap| gain += usize::from(unmet[ap] > 0));
        heap.push((gain, score, Reverse(id), candidate_idx));
    }

    let mut selected = try_filled(candidates.len(), false, "selección multicoverage")?;
    let mut selected_count = 0usize;
    let mut heap_pops = 0usize;
    while unmet_total > 0 {
        heap_pops += 1;
        if heap_pops & 0xff == 0 && cancelled() {
            return Err("Selección local cancelada o sustituida".into());
        }
        let Some((cached_gain, score, stable_id, candidate_idx)) = heap.pop() else {
            break;
        };
        if selected[candidate_idx] {
            continue;
        }
        let id = candidates[candidate_idx].0;
        let mut current_gain = 0usize;
        acceptance.for_each_accepted(id, &mut |ap| current_gain += usize::from(unmet[ap] > 0));
        if current_gain != cached_gain {
            heap.push((current_gain, score, stable_id, candidate_idx));
            continue;
        }
        if current_gain == 0 {
            break;
        }

        selected[candidate_idx] = true;
        selected_count += 1;
        acceptance.for_each_accepted(id, &mut |ap| {
            if unmet[ap] > 0 {
                unmet[ap] -= 1;
                unmet_total -= 1;
            }
        });
    }

    let mut required = Vec::new();
    required
        .try_reserve_exact(ap_count)
        .map_err(|error| format!("No se pudo reservar el mínimo requerido por AP: {error}"))?;
    required.extend(available.iter().map(|&count| count.min(min_keep_per_ap)));
    let mut selected_coverage = try_filled(ap_count, 0usize, "cobertura AP seleccionada")?;
    for (candidate_idx, &(id, _)) in candidates.iter().enumerate() {
        if candidate_idx & 0xff == 0 && cancelled() {
            return Err("Selección local cancelada o sustituida".into());
        }
        if selected[candidate_idx] {
            acceptance.for_each_accepted(id, &mut |ap| selected_coverage[ap] += 1);
        }
    }
    let mut removal_order = Vec::new();
    removal_order
        .try_reserve_exact(selected_count)
        .map_err(|error| format!("No se pudo reservar el orden de cobertura AP: {error}"))?;
    removal_order.extend((0..candidates.len()).filter(|&idx| selected[idx]));
    removal_order.sort_unstable_by(|&a, &b| {
        candidates[a]
            .1
            .cmp(&candidates[b].1)
            .then_with(|| candidates[b].0.cmp(&candidates[a].0))
    });
    for (position, candidate_idx) in removal_order.into_iter().enumerate() {
        if position & 0xff == 0 && cancelled() {
            return Err("Selección local cancelada o sustituida".into());
        }
        let id = candidates[candidate_idx].0;
        let mut removable = true;
        acceptance.for_each_accepted(id, &mut |ap| {
            if selected_coverage[ap] <= required[ap] {
                removable = false;
            }
        });
        if removable {
            selected[candidate_idx] = false;
            selected_count -= 1;
            acceptance.for_each_accepted(id, &mut |ap| selected_coverage[ap] -= 1);
        }
    }

    let target = max_frames.max(selected_count).min(candidates.len());
    let mut quality_order = Vec::new();
    quality_order
        .try_reserve_exact(candidates.len())
        .map_err(|error| format!("No se pudo reservar el ranking global final: {error}"))?;
    quality_order.extend(0..candidates.len());
    quality_order.sort_unstable_by(|&a, &b| {
        candidates[b]
            .1
            .cmp(&candidates[a].1)
            .then_with(|| candidates[a].0.cmp(&candidates[b].0))
    });
    for (position, candidate_idx) in quality_order.into_iter().enumerate() {
        if position & 0x3ff == 0 && cancelled() {
            return Err("Selección local cancelada o sustituida".into());
        }
        if selected_count >= target {
            break;
        }
        if !selected[candidate_idx] {
            selected[candidate_idx] = true;
            selected_count += 1;
        }
    }

    let mut result = HashSet::new();
    result.try_reserve(selected_count).map_err(|error| {
        format!("No se pudo reservar el resultado del recorte por cobertura: {error}")
    })?;
    for (idx, &(id, _)) in candidates.iter().enumerate() {
        if selected[idx] {
            result.insert(id);
        }
    }
    Ok(result)
}

/// Factor que expande una muestra almacenada en su rango nativo (9..15 bits)
/// al rango ADU16. No debe usarse para buffers que el decoder ya normalizó a
/// 16 bit (u8×257, FFmpeg gray16/rgb48, etc.).
pub fn native_sample_to_u16_gain(sample_bits: usize) -> f32 {
    if !(9..16).contains(&sample_bits) {
        return 1.0;
    }
    let max_native = ((1u32 << sample_bits) - 1) as f32;
    65_535.0 / max_native
}

/// Transformada de distancia euclídea 1-D de Felzenszwalb/Huttenlocher.
/// `f` contiene costes cuadrados; `cap_sq` hace la versión truncada robusta
/// incluso cuando no existe ningún píxel de fondo.
fn edt_1d(f: &[f32], out: &mut [f32], cap_sq: f32, v: &mut [usize], z: &mut [f32]) {
    let n = f.len();
    if n == 0 {
        return;
    }
    debug_assert!(v.len() >= n && z.len() > n);
    let mut k = 0usize;
    v[0] = 0;
    z[0] = f32::NEG_INFINITY;
    z[1] = f32::INFINITY;

    for q in 1..n {
        let mut s;
        loop {
            let p = v[k];
            let qf = q as f32;
            let pf = p as f32;
            s = ((f[q] + qf * qf) - (f[p] + pf * pf)) / (2.0 * (qf - pf));
            if s > z[k] || k == 0 {
                break;
            }
            k -= 1;
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f32::INFINITY;
    }

    k = 0;
    for q in 0..n {
        let qf = q as f32;
        while z[k + 1] < qf {
            k += 1;
        }
        let delta = qf - v[k] as f32;
        out[q] = (delta * delta + f[v[k]]).min(cap_sq);
    }
}

/// Distancia euclídea exacta desde cada píxel de señal (`mask >= 0.5`) al
/// fondo más cercano, truncada a `cap`. Complejidad O(w·h), frente al barrido
/// anterior O(w·h·33²). Los píxeles de fondo devuelven cero.
pub fn distance_transform_truncated(mask: &[f32], w: usize, h: usize, cap: f32) -> Vec<f32> {
    if w == 0 || h == 0 || mask.len() < w.saturating_mul(h) {
        return Vec::new();
    }
    let cap = cap.max(0.0);
    let cap_sq = cap * cap;
    let mut horizontal = vec![0.0f32; w * h];
    let mut input = vec![0.0f32; w.max(h)];
    let mut output = vec![0.0f32; w.max(h)];
    // Un único scratch para todas las filas/columnas: evita w+h asignaciones
    // pequeñas en máscaras lunares 8K, además del salto algorítmico a O(N).
    let mut sites = vec![0usize; w.max(h)];
    let mut intersections = vec![0.0f32; w.max(h) + 1];

    for y in 0..h {
        for x in 0..w {
            input[x] = if mask[y * w + x] < 0.5 { 0.0 } else { cap_sq };
        }
        edt_1d(
            &input[..w],
            &mut output[..w],
            cap_sq,
            &mut sites[..w],
            &mut intersections[..=w],
        );
        horizontal[y * w..(y + 1) * w].copy_from_slice(&output[..w]);
    }

    let mut squared = vec![0.0f32; w * h];
    for x in 0..w {
        for y in 0..h {
            input[y] = horizontal[y * w + x];
        }
        edt_1d(
            &input[..h],
            &mut output[..h],
            cap_sq,
            &mut sites[..h],
            &mut intersections[..=h],
        );
        for y in 0..h {
            squared[y * w + x] = output[y];
        }
    }
    squared.into_iter().map(f32::sqrt).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_quality_frames_keep_equal_weights() {
        let a = sigmoidal_frame_weight(999_000, 1_000_000, 0.80, 15.0);
        let b = sigmoidal_frame_weight(999_000, 1_000_000, 0.80, 15.0);
        assert_eq!(a, b);
        assert!(
            a > 0.98,
            "frames casi iguales no deben separarse por rango: {a}"
        );
        assert_eq!(sigmoidal_frame_weight(42, 42, 0.80, 15.0), 1.0);
    }

    #[test]
    fn sigmoidal_weight_is_monotonic_and_bounded() {
        let mut previous = 0.0;
        for score in (0..=1000).step_by(10) {
            let weight = sigmoidal_frame_weight(score, 1000, 0.75, 12.0);
            assert!((MIN_FRAME_WEIGHT..=1.0).contains(&weight));
            assert!(weight >= previous);
            previous = weight;
        }
    }

    #[test]
    fn robust_reference_ignores_a_single_extreme_frame() {
        let mut scores = vec![1_000u64; 100];
        scores[99] = 1_000_000_000;
        let reference = robust_quality_reference(&scores);
        assert_eq!(reference, 1_000, "un unico outlier no debe fijar la escala");

        let healthy_weight = sigmoidal_frame_weight(1_000, reference, 0.80, 15.0);
        assert_eq!(healthy_weight, 1.0);
    }

    #[test]
    fn robust_reference_is_a_winsorized_p95_p99_tail() {
        let scores: Vec<u64> = (1..=101).collect();
        let reference = robust_quality_reference(&scores);
        assert!(
            (96..=100).contains(&reference),
            "referencia inesperada: {reference}"
        );
        assert!(reference < 101, "el maximo unico no debe ser la referencia");
        assert_eq!(robust_quality_reference(&[]), 0);
        assert_eq!(robust_quality_reference(&[42]), 42);
    }

    #[test]
    fn coverage_trim_keeps_a_shared_low_score_frame_when_it_is_essential() {
        let scored = vec![(1, 100), (2, 90), (3, 1)];
        let masks = HashMap::from([
            (1usize, vec![true, false]),
            (2usize, vec![false, true]),
            (3usize, vec![true, true]),
        ]);
        let kept = coverage_aware_frame_trim(&scored, &masks, 1, 1);
        assert_eq!(kept, HashSet::from([3]));
    }

    #[test]
    fn coverage_trim_preserves_minimum_even_when_the_cap_is_impossible() {
        let scored = vec![(1, 100), (2, 90), (3, 80), (4, 70)];
        let masks = HashMap::from([
            (1usize, vec![true, false]),
            (2usize, vec![true, false]),
            (3usize, vec![false, true]),
            (4usize, vec![false, true]),
        ]);
        let kept = coverage_aware_frame_trim(&scored, &masks, 3, 2);
        assert_eq!(
            kept.len(),
            4,
            "la cobertura tiene prioridad sobre un cap imposible"
        );
        for ap in 0..2 {
            let coverage = kept
                .iter()
                .filter(|id| masks.get(id).is_some_and(|mask| mask[ap]))
                .count();
            assert_eq!(coverage, 2);
        }
    }

    #[test]
    fn coverage_trim_fills_free_slots_by_global_quality() {
        let scored = vec![(1, 100), (2, 90), (3, 1)];
        let masks = HashMap::from([
            (1usize, vec![true, false]),
            (2usize, vec![false, true]),
            (3usize, vec![true, true]),
        ]);
        let kept = coverage_aware_frame_trim(&scored, &masks, 2, 1);
        assert_eq!(kept, HashSet::from([1, 3]));
    }

    #[test]
    fn compact_acceptance_matches_legacy_multicoverage() {
        let scored = vec![(10, 100), (20, 90), (30, 80), (40, 1)];
        let masks = HashMap::from([
            (10usize, vec![true, false, false]),
            (20usize, vec![false, true, false]),
            (30usize, vec![false, false, true]),
            (40usize, vec![true, true, true]),
        ]);
        let mut compact = CompactFrameAcceptance::try_new(
            scored.iter().map(|&(id, _)| id),
            scored.len(),
            3,
            1024 * 1024,
        )
        .unwrap();
        for (&frame, mask) in &masks {
            for (ap, &accepted) in mask.iter().enumerate() {
                if accepted {
                    compact.set_accepted(frame, ap).unwrap();
                }
            }
        }

        let legacy = coverage_aware_frame_trim(&scored, &masks, 2, 1);
        let packed = coverage_aware_frame_trim_compact(&scored, &compact, 2, 1, || false).unwrap();
        assert_eq!(packed, legacy);
        assert_eq!(compact.touched_len(), scored.len());
    }

    #[test]
    fn compact_acceptance_is_budgeted_and_remaps_in_place() {
        let rejected = CompactFrameAcceptance::try_new(0..100usize, 100, 10_000, 1024);
        assert!(
            rejected.unwrap_err().contains("presupuesto seguro"),
            "una matriz fuera de presupuesto debe fallar antes de reservar"
        );

        let mut compact =
            CompactFrameAcceptance::try_new([7usize, 11], 2, 130, 1024 * 1024).unwrap();
        compact.set_accepted(7, 2).unwrap();
        compact.set_accepted(7, 65).unwrap();
        compact.set_accepted(11, 129).unwrap();
        let bytes_before = compact.estimated_bytes();
        compact.remap_aps_in_place(&[2, 65, 129]).unwrap();
        assert_eq!(compact.ap_count(), 3);
        assert_eq!(compact.accepts(7, 0), Some(true));
        assert_eq!(compact.accepts(7, 1), Some(true));
        assert_eq!(compact.accepts(7, 2), Some(false));
        assert_eq!(compact.accepts(11, 2), Some(true));
        assert!(compact.estimated_bytes() < bytes_before);
    }

    #[test]
    fn compact_coverage_trim_observes_cancellation() {
        let scored = vec![(1usize, 100u64), (2, 90)];
        let compact = CompactFrameAcceptance::try_new(
            scored.iter().map(|&(id, _)| id),
            scored.len(),
            2,
            1024 * 1024,
        )
        .unwrap();
        let error =
            coverage_aware_frame_trim_compact(&scored, &compact, 1, 1, || true).unwrap_err();
        assert!(error.contains("cancelada"));
    }

    #[test]
    fn native_bit_gain_uses_declared_range() {
        assert_eq!(native_sample_to_u16_gain(8), 1.0);
        assert!((native_sample_to_u16_gain(10) - 64.061_58).abs() < 1e-4);
        assert!((native_sample_to_u16_gain(12) - 16.003_662).abs() < 1e-4);
        assert!((native_sample_to_u16_gain(14) - 4.000_183).abs() < 1e-4);
        assert_eq!(native_sample_to_u16_gain(16), 1.0);
    }

    fn brute_force(mask: &[f32], w: usize, h: usize, cap: f32) -> Vec<f32> {
        let zeros: Vec<(isize, isize)> = (0..h)
            .flat_map(|y| {
                (0..w).filter_map(move |x| {
                    (mask[y * w + x] < 0.5).then_some((x as isize, y as isize))
                })
            })
            .collect();
        let mut out = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                if mask[y * w + x] < 0.5 {
                    continue;
                }
                out[y * w + x] = zeros
                    .iter()
                    .map(|&(zx, zy)| {
                        let dx = x as isize - zx;
                        let dy = y as isize - zy;
                        ((dx * dx + dy * dy) as f32).sqrt()
                    })
                    .fold(cap, f32::min);
            }
        }
        out
    }

    #[test]
    fn linear_edt_matches_exact_bruteforce() {
        let (w, h) = (37usize, 29usize);
        let mut mask = vec![1.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                if (x * 17 + y * 31 + x * y) % 23 == 0 {
                    mask[y * w + x] = 0.0;
                }
            }
        }
        let fast = distance_transform_truncated(&mask, w, h, 32.0);
        let exact = brute_force(&mask, w, h, 32.0);
        assert!(fast
            .iter()
            .zip(exact.iter())
            .all(|(a, b)| (a - b).abs() < 1e-5));

        let full = distance_transform_truncated(&vec![1.0; w * h], w, h, 32.0);
        assert!(full.iter().all(|&distance| distance == 32.0));
    }
}
