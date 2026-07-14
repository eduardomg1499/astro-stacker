use memmap2::MmapMut;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const FRAME_CACHE_VERSION: u32 = 3;

#[cfg(test)]
thread_local! {
    static TEST_DISK_FULL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn storage_write_checkpoint() -> Result<(), String> {
    #[cfg(test)]
    if TEST_DISK_FULL.with(|flag| flag.get()) {
        return Err("FrameStore: disco lleno (fallo inyectado)".into());
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FrameStoreManifest {
    version: u32,
    capacity: usize,
    frame_len: usize,
    complete: bool,
    present: Vec<bool>,
    checksums: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameStoreKind {
    Ram,
    Mmap,
    Lz4,
}

impl FrameStoreKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ram => "RAM float32",
            Self::Mmap => "mmap float32",
            Self::Lz4 => "LZ4 spill",
        }
    }
}

enum FrameStoreBackend {
    Ram(Vec<Option<Vec<f32>>>),
    Mmap { map: MmapMut, file: File, path: PathBuf },
    Lz4 { dir: PathBuf, prefix: String },
}

/// Almacén de frames calibrados con geometría fija. Prioriza RAM, luego mmap
/// sin compresión para pasadas repetidas y usa LZ4 únicamente si el archivo
/// mapeado no puede crearse.
pub struct AdaptiveFrameStore {
    backend: FrameStoreBackend,
    frame_len: usize,
    capacity: usize,
    present: Vec<bool>,
    checksums: Vec<u64>,
    /// Checksum ya verificado en ESTA sesión: las pasadas 2 y 3 de la
    /// integración releían el mismo frame y re-hasheaban 140 MB cada vez
    /// (~34 GB de SipHash por run). La integridad se garantiza en la primera
    /// lectura tras abrir el store; una escritura nueva la resetea.
    verified: Vec<std::sync::atomic::AtomicBool>,
    bytes_written: u64,
    read_hits: AtomicUsize,
    persistent: bool,
    manifest_path: Option<PathBuf>,
    reused: bool,
}

impl AdaptiveFrameStore {
    pub fn new(
        capacity: usize,
        frame_len: usize,
        cache_dir: &Path,
        tag: &str,
    ) -> Result<Self, String> {
        Self::open(capacity, frame_len, cache_dir, tag, false)
    }

    /// Caché persistente versionada por algoritmo/geometría/fingerprint. Un
    /// manifest corrupto, incompleto o de otra versión se invalida y recrea.
    pub fn new_versioned(
        capacity: usize,
        frame_len: usize,
        cache_dir: &Path,
        fingerprint: &str,
    ) -> Result<Self, String> {
        let safe: String = fingerprint.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        Self::open(capacity, frame_len, cache_dir, &format!("v{FRAME_CACHE_VERSION}_{safe}"), true)
    }

    fn open(
        capacity: usize,
        frame_len: usize,
        cache_dir: &Path,
        tag: &str,
        persistent: bool,
    ) -> Result<Self, String> {
        if capacity == 0 || frame_len == 0 {
            return Err("FrameStore con geometría vacía".into());
        }
        let total_bytes = (capacity as u64)
            .saturating_mul(frame_len as u64)
            .saturating_mul(4);
        let mut sys = sysinfo::System::new_all();
        sys.refresh_memory();
        let ram_budget = sys.available_memory().saturating_mul(30) / 100;
        let manifest_path = persistent.then(|| cache_dir.join(format!("{tag}.manifest.json")));
        let data_path = cache_dir.join(format!("{tag}.f32cache"));
        let cached_manifest = manifest_path.as_ref().and_then(|path| {
            let bytes = std::fs::read(path).ok()?;
            let m: FrameStoreManifest = serde_json::from_slice(&bytes).ok()?;
            (m.version == FRAME_CACHE_VERSION
                && m.capacity == capacity
                && m.frame_len == frame_len
                && m.complete
                && m.present.len() == capacity
                && m.checksums.len() == capacity
                && std::fs::metadata(&data_path).ok()?.len() == total_bytes)
                .then_some(m)
        });
        let reuse_disk = cached_manifest.is_some();
        // Una caché persistente usa mmap incluso si cabría en RAM, para poder
        // sobrevivir entre reintegraciones/sesiones sin duplicar varios GB.
        let backend = if !persistent && total_bytes <= ram_budget && total_bytes <= 2 * 1024 * 1024 * 1024 {
            FrameStoreBackend::Ram((0..capacity).map(|_| None).collect())
        } else {
            std::fs::create_dir_all(cache_dir).map_err(|e| format!("Cache dir: {e}"))?;
            let mut opts = OpenOptions::new();
            opts.read(true).write(true).create(true);
            if !reuse_disk { opts.truncate(true); }
            match opts.open(&data_path) {
                Ok(file) => {
                    if reuse_disk || file.set_len(total_bytes).is_ok() {
                        // SAFETY: the file is exclusively owned by this store, sized to
                        // the complete mapping and never resized while the map lives.
                        match unsafe { MmapMut::map_mut(&file) } {
                            Ok(map) => FrameStoreBackend::Mmap { map, file, path: data_path.clone() },
                            Err(_) => FrameStoreBackend::Lz4 { dir: cache_dir.to_path_buf(), prefix: tag.into() },
                        }
                    } else {
                        FrameStoreBackend::Lz4 { dir: cache_dir.to_path_buf(), prefix: tag.into() }
                    }
                }
                Err(_) => FrameStoreBackend::Lz4 { dir: cache_dir.to_path_buf(), prefix: tag.into() },
            }
        };
        Ok(Self {
            backend,
            frame_len,
            capacity,
            present: cached_manifest.as_ref().map(|m| m.present.clone()).unwrap_or_else(|| vec![false; capacity]),
            checksums: cached_manifest.map(|m| m.checksums).unwrap_or_else(|| vec![0; capacity]),
            verified: (0..capacity).map(|_| std::sync::atomic::AtomicBool::new(false)).collect(),
            bytes_written: 0,
            read_hits: AtomicUsize::new(0),
            persistent,
            manifest_path,
            reused: reuse_disk,
        })
    }

    pub fn kind(&self) -> FrameStoreKind {
        match self.backend {
            FrameStoreBackend::Ram(_) => FrameStoreKind::Ram,
            FrameStoreBackend::Mmap { .. } => FrameStoreKind::Mmap,
            FrameStoreBackend::Lz4 { .. } => FrameStoreKind::Lz4,
        }
    }

    pub fn put(&mut self, index: usize, data: &[f32]) -> Result<(), String> {
        storage_write_checkpoint()?;
        if index >= self.capacity || data.len() != self.frame_len {
            return Err(format!("FrameStore put inválido: idx={index}, len={}", data.len()));
        }
        match &mut self.backend {
            FrameStoreBackend::Ram(frames) => frames[index] = Some(data.to_vec()),
            FrameStoreBackend::Mmap { map, .. } => {
                let off = index * self.frame_len * 4;
                let bytes: &[u8] = bytemuck::cast_slice(data);
                map[off..off + bytes.len()].copy_from_slice(bytes);
            }
            FrameStoreBackend::Lz4 { dir, prefix } => {
                let bytes: &[u8] = bytemuck::cast_slice(data);
                let compressed = lz4_flex::compress_prepend_size(bytes);
                std::fs::write(dir.join(format!("{prefix}_{index}.lz4")), compressed)
                    .map_err(|e| format!("LZ4 spill: {e}"))?;
            }
        }
        self.present[index] = true;
        self.checksums[index] = frame_checksum(data);
        self.verified[index].store(false, Ordering::Relaxed);
        self.bytes_written = self.bytes_written.saturating_add((data.len() * 4) as u64);
        Ok(())
    }

    pub fn get(&self, index: usize) -> Result<Vec<f32>, String> {
        if index >= self.capacity || !self.present[index] {
            return Err(format!("FrameStore: frame {index} no disponible"));
        }
        let read_once = || -> Result<Vec<f32>, String> {
            match &self.backend {
                FrameStoreBackend::Ram(frames) => frames[index].clone().ok_or_else(|| "Frame RAM ausente".into()),
                FrameStoreBackend::Mmap { map, .. } => {
                    let off = index * self.frame_len * 4;
                    let bytes = &map[off..off + self.frame_len * 4];
                    let vals: &[f32] = bytemuck::try_cast_slice(bytes)
                        .map_err(|_| "Frame mmap desalineado".to_string())?;
                    Ok(vals.to_vec())
                }
                FrameStoreBackend::Lz4 { dir, prefix } => {
                    let raw = std::fs::read(dir.join(format!("{prefix}_{index}.lz4")))
                        .map_err(|e| format!("LZ4 read: {e}"))?;
                    let bytes = lz4_flex::decompress_size_prepended(&raw)
                        .map_err(|e| format!("LZ4 decode: {e}"))?;
                    let vals: &[f32] = bytemuck::try_cast_slice(&bytes)
                        .map_err(|_| "LZ4 float32 inválido".to_string())?;
                    if vals.len() != self.frame_len {
                        return Err("LZ4 frame con tamaño inesperado".into());
                    }
                    Ok(vals.to_vec())
                }
            }
        };
        let skip_checksum =
            self.checksums[index] == 0 || self.verified[index].load(Ordering::Relaxed);
        // Retry the read+checksum a few times before failing: a slow/flaky spill
        // drive (external disk, network mount) can return a transient bad read
        // that a re-read recovers. A genuinely corrupt cache still fails after the
        // retries, and the abort is honest rather than silently using bad pixels.
        let mut last_err = String::new();
        for _ in 0..3 {
            let data = read_once()?;
            if skip_checksum || frame_checksum(&data) == self.checksums[index] {
                self.verified[index].store(true, Ordering::Relaxed);
                self.read_hits.fetch_add(1, Ordering::Relaxed);
                return Ok(data);
            }
            last_err = format!("FrameStore: checksum corrupto en frame {index}");
        }
        Err(last_err)
    }

    /// Read only a tile of a frame. Non-persistent RAM/mmap stores can expose
    /// the requested range without copying or decompressing the complete
    /// frame on every master-combination tile. Persistent caches retain the
    /// full-frame checksum path; LZ4 remains the last-resort spill backend and
    /// necessarily decompresses its independent frame blob.
    pub fn get_range(&self, index: usize, range: std::ops::Range<usize>) -> Result<Vec<f32>, String> {
        if index >= self.capacity || !self.present.get(index).copied().unwrap_or(false) {
            return Err(format!("FrameStore: frame {index} no disponible"));
        }
        if range.start > range.end || range.end > self.frame_len {
            return Err(format!(
                "FrameStore range inválido: {}..{} de {}",
                range.start, range.end, self.frame_len
            ));
        }
        if self.persistent || matches!(&self.backend, FrameStoreBackend::Lz4 { .. }) {
            let full = self.get(index)?;
            return Ok(full[range].to_vec());
        }
        let result = match &self.backend {
            FrameStoreBackend::Ram(frames) => frames[index]
                .as_ref()
                .map(|frame| frame[range].to_vec())
                .ok_or_else(|| "Frame RAM ausente".to_string()),
            FrameStoreBackend::Mmap { map, .. } => {
                let start = (index * self.frame_len + range.start) * 4;
                let end = (index * self.frame_len + range.end) * 4;
                let values: &[f32] = bytemuck::try_cast_slice(&map[start..end])
                    .map_err(|_| "Frame mmap desalineado".to_string())?;
                Ok(values.to_vec())
            }
            FrameStoreBackend::Lz4 { .. } => unreachable!(),
        };
        if result.is_ok() {
            self.read_hits.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    pub fn bytes_written(&self) -> u64 { self.bytes_written }
    pub fn read_hits(&self) -> usize { self.read_hits.load(Ordering::Relaxed) }
    pub fn is_present(&self, index: usize) -> bool { self.present.get(index).copied().unwrap_or(false) }
    pub fn reused(&self) -> bool { self.reused }

    pub fn mark_complete(&mut self) -> Result<(), String> {
        storage_write_checkpoint()?;
        if !self.persistent { return Ok(()); }
        if let FrameStoreBackend::Mmap { map, file, .. } = &mut self.backend {
            map.flush().map_err(|e| format!("Cache mmap flush: {e}"))?;
            file.sync_data().map_err(|e| format!("Cache sync: {e}"))?;
        }
        let Some(path) = &self.manifest_path else { return Ok(()); };
        let manifest = FrameStoreManifest {
            version: FRAME_CACHE_VERSION,
            capacity: self.capacity,
            frame_len: self.frame_len,
            complete: true,
            present: self.present.clone(),
            checksums: self.checksums.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes).map_err(|e| format!("Cache manifest: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("Cache manifest commit: {e}"))
    }
}

fn frame_checksum(data: &[f32]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytemuck::cast_slice::<f32, u8>(data).hash(&mut h);
    h.finish()
}

impl Drop for AdaptiveFrameStore {
    fn drop(&mut self) {
        match &mut self.backend {
            FrameStoreBackend::Ram(_) => {}
            FrameStoreBackend::Mmap { map, file, path } => {
                let _ = map.flush();
                let _ = file.sync_data();
                if self.persistent { return; }
                let path = path.clone();
                // Windows no permite borrar mientras el mapping está vivo; el
                // archivo se intenta limpiar ahora y también en el próximo run.
                let _ = std::fs::remove_file(path);
            }
            FrameStoreBackend::Lz4 { dir, prefix } => {
                if let Ok(rd) = std::fs::read_dir(dir) {
                    for entry in rd.flatten() {
                        if entry.file_name().to_string_lossy().starts_with(prefix.as_str()) {
                            let _ = std::fs::remove_file(entry.path());
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_store_roundtrip_float32() {
        let dir = std::env::temp_dir().join(format!("zas-frame-store-test-{}", std::process::id()));
        let mut store = AdaptiveFrameStore::new(3, 8, &dir, "roundtrip").unwrap();
        let frame: Vec<f32> = (0..8).map(|i| i as f32 - 3.5).collect();
        store.put(1, &frame).unwrap();
        assert_eq!(store.get(1).unwrap(), frame);
        assert_eq!(store.get_range(1, 2..6).unwrap(), frame[2..6]);
        assert!(store.get(0).is_err());
        assert!(store.get_range(1, 7..9).is_err());
    }

    #[test]
    fn versioned_cache_reuses_and_detects_corruption() {
        use std::io::{Seek, SeekFrom, Write};
        let dir = std::env::temp_dir().join(format!("zas-frame-store-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let frame: Vec<f32> = (0..16).map(|i| i as f32 - 8.0).collect();
        {
            let mut store = AdaptiveFrameStore::new_versioned(2, 16, &dir, "source-fingerprint").unwrap();
            store.put(0, &frame).unwrap();
            store.mark_complete().unwrap();
        }
        {
            let store = AdaptiveFrameStore::new_versioned(2, 16, &dir, "source-fingerprint").unwrap();
            assert!(store.reused());
            assert_eq!(store.get(0).unwrap(), frame);
        }
        let data_path = std::fs::read_dir(&dir).unwrap().flatten()
            .map(|e| e.path()).find(|p| p.extension().and_then(|s| s.to_str()) == Some("f32cache")).unwrap();
        let mut file = OpenOptions::new().write(true).open(data_path).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&12345.0f32.to_ne_bytes()).unwrap();
        file.flush().unwrap();
        let store = AdaptiveFrameStore::new_versioned(2, 16, &dir, "source-fingerprint").unwrap();
        assert!(store.get(0).unwrap_err().contains("checksum"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_full_is_reported_without_committing_a_partial_frame() {
        let dir = std::env::temp_dir().join(format!("zas-frame-store-disk-full-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = AdaptiveFrameStore::new_versioned(2, 8, &dir, "disk-full").unwrap();
        TEST_DISK_FULL.with(|flag| flag.set(true));
        let err = store.put(0, &[1.0; 8]).unwrap_err();
        TEST_DISK_FULL.with(|flag| flag.set(false));
        assert!(err.contains("disco lleno"));
        assert!(!store.is_present(0), "un write fallido no debe marcar el frame como válido");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
