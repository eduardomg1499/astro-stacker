use memmap2::MmapMut;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// v4 invalida el blob LZ4 monolítico de v3. El spill nuevo tiene header e
/// índice de bloques explícitos y nunca intenta interpretar el formato antiguo.
const FRAME_CACHE_VERSION: u32 = 4;
const LZ4_BLOCK_MAGIC: &[u8; 8] = b"ZASLZ4B4";
const LZ4_HEADER_LEN: usize = 48;
const LZ4_INDEX_ENTRY_LEN: usize = 24;
// FNV-1a 64 es determinista entre builds/plataformas; `DefaultHasher` no
// garantiza estabilidad y no debe formar parte de un formato persistente.
const CACHE_CHECKSUM_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const CACHE_CHECKSUM_PRIME: u64 = 0x0000_0100_0000_01b3;
/// 256 KiB sin comprimir: suficientemente grande para buen ratio y pequeño
/// para que una franja no materialice un frame de decenas/cientos de MiB.
const LZ4_BLOCK_ELEMS: usize = 64 * 1024;

struct Lz4ReadScratch {
    compressed: Vec<u8>,
    decoded: Vec<f32>,
}

thread_local! {
    /// Scratch por hilo: evita reservas de bloques comprimidos y decodificados
    /// en cada franja sin serializar lectores concurrentes del store.
    static LZ4_READ_SCRATCH: std::cell::RefCell<Lz4ReadScratch> = const {
        std::cell::RefCell::new(Lz4ReadScratch {
            compressed: Vec::new(),
            decoded: Vec::new(),
        })
    };
}

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
    storage_checksums: Vec<u64>,
    backend: FrameStoreManifestBackend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum FrameStoreManifestBackend {
    Mmap,
    Lz4Blocks,
}

#[derive(Clone, Debug)]
struct Lz4BlockEntry {
    offset: u64,
    compressed_len: u32,
    uncompressed_elems: u32,
    uncompressed_checksum: u64,
}

#[derive(Clone, Debug)]
struct Lz4FrameIndex {
    block_elems: usize,
    frame_len: usize,
    blocks: Vec<Lz4BlockEntry>,
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
    Mmap {
        map: MmapMut,
        file: File,
        path: PathBuf,
    },
    Lz4 {
        dir: PathBuf,
        prefix: String,
    },
}

fn lz4_frame_path(dir: &Path, prefix: &str, index: usize) -> PathBuf {
    dir.join(format!("{prefix}_{index}.lz4b4"))
}

fn try_zeroed_bytes(len: usize, context: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| format!("FrameStore: memoria insuficiente para {context} ({len} bytes)"))?;
    bytes.resize(len, 0);
    Ok(bytes)
}

fn write_u32_le(dst: &mut [u8], offset: usize, value: u32) {
    dst[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64_le(dst: &mut [u8], offset: usize, value: u64) {
    dst[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_u32_le(src: &[u8], offset: usize) -> Result<u32, String> {
    let bytes: [u8; 4] = src
        .get(offset..offset + 4)
        .ok_or_else(|| "FrameStore: header LZ4 truncado".to_string())?
        .try_into()
        .map_err(|_| "FrameStore: campo u32 LZ4 inválido".to_string())?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64_le(src: &[u8], offset: usize) -> Result<u64, String> {
    let bytes: [u8; 8] = src
        .get(offset..offset + 8)
        .ok_or_else(|| "FrameStore: header LZ4 truncado".to_string())?
        .try_into()
        .map_err(|_| "FrameStore: campo u64 LZ4 inválido".to_string())?;
    Ok(u64::from_le_bytes(bytes))
}

fn storage_checksum_reader(reader: &mut File) -> Result<(u64, u64), String> {
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("LZ4 seek checksum: {error}"))?;
    let mut checksum = CACHE_CHECKSUM_OFFSET;
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("LZ4 read checksum: {error}"))?;
        if count == 0 {
            break;
        }
        checksum_update(&mut checksum, &buffer[..count]);
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| "FrameStore: conteo de lectura LZ4 desborda".to_string())?;
    }
    Ok((checksum, total))
}

fn encode_lz4_index(entries: &[Lz4BlockEntry]) -> Result<Vec<u8>, String> {
    let len = entries
        .len()
        .checked_mul(LZ4_INDEX_ENTRY_LEN)
        .ok_or_else(|| "FrameStore: índice LZ4 demasiado grande".to_string())?;
    let mut bytes = try_zeroed_bytes(len, "índice LZ4")?;
    for (block, entry) in entries.iter().enumerate() {
        let offset = block * LZ4_INDEX_ENTRY_LEN;
        write_u64_le(&mut bytes, offset, entry.offset);
        write_u32_le(&mut bytes, offset + 8, entry.compressed_len);
        write_u32_le(&mut bytes, offset + 12, entry.uncompressed_elems);
        write_u64_le(&mut bytes, offset + 16, entry.uncompressed_checksum);
    }
    Ok(bytes)
}

fn write_lz4_block_frame(
    dir: &Path,
    prefix: &str,
    index: usize,
    data: &[f32],
    logical_checksum: u64,
) -> Result<u64, String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("LZ4 spill dir: {error}"))?;
    let block_count = data.len().div_ceil(LZ4_BLOCK_ELEMS);
    let index_len = block_count
        .checked_mul(LZ4_INDEX_ENTRY_LEN)
        .ok_or_else(|| "FrameStore: índice LZ4 desborda".to_string())?;
    let data_start = LZ4_HEADER_LEN
        .checked_add(index_len)
        .ok_or_else(|| "FrameStore: header LZ4 desborda".to_string())?;
    let final_path = lz4_frame_path(dir, prefix, index);
    let temp_path = dir.join(format!("{prefix}_{index}.lz4b4.tmp"));
    let result = (|| -> Result<u64, String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp_path)
            .map_err(|error| format!("LZ4 spill create: {error}"))?;
        file.seek(SeekFrom::Start(data_start as u64))
            .map_err(|error| format!("LZ4 spill reserve header: {error}"))?;

        let max_input_bytes = LZ4_BLOCK_ELEMS
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| "FrameStore: bloque LZ4 desborda".to_string())?;
        let max_compressed = lz4_flex::block::get_maximum_output_size(max_input_bytes);
        let mut compressed = Vec::new();
        compressed.try_reserve_exact(max_compressed).map_err(|_| {
            "FrameStore: memoria insuficiente para comprimir bloque LZ4".to_string()
        })?;
        compressed.resize(max_compressed, 0);
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(block_count)
            .map_err(|_| "FrameStore: memoria insuficiente para índice LZ4".to_string())?;

        for block in data.chunks(LZ4_BLOCK_ELEMS) {
            let input: &[u8] = bytemuck::cast_slice(block);
            let compressed_len = lz4_flex::block::compress_into(input, &mut compressed)
                .map_err(|error| format!("LZ4 encode block: {error}"))?;
            let offset = file
                .stream_position()
                .map_err(|error| format!("LZ4 spill position: {error}"))?;
            file.write_all(&compressed[..compressed_len])
                .map_err(|error| format!("LZ4 spill block: {error}"))?;
            entries.push(Lz4BlockEntry {
                offset,
                compressed_len: u32::try_from(compressed_len)
                    .map_err(|_| "FrameStore: bloque LZ4 excede u32".to_string())?,
                uncompressed_elems: u32::try_from(block.len())
                    .map_err(|_| "FrameStore: bloque LZ4 excede u32".to_string())?,
                uncompressed_checksum: frame_checksum(block),
            });
        }

        let index_bytes = encode_lz4_index(&entries)?;
        let index_checksum = frame_checksum_bytes(&index_bytes);
        let mut header = [0u8; LZ4_HEADER_LEN];
        header[..8].copy_from_slice(LZ4_BLOCK_MAGIC);
        write_u32_le(&mut header, 8, FRAME_CACHE_VERSION);
        write_u32_le(&mut header, 12, LZ4_HEADER_LEN as u32);
        write_u64_le(&mut header, 16, data.len() as u64);
        write_u32_le(&mut header, 24, LZ4_BLOCK_ELEMS as u32);
        write_u32_le(
            &mut header,
            28,
            u32::try_from(entries.len())
                .map_err(|_| "FrameStore: demasiados bloques LZ4".to_string())?,
        );
        write_u64_le(&mut header, 32, logical_checksum);
        write_u64_le(&mut header, 40, index_checksum);
        file.seek(SeekFrom::Start(0))
            .map_err(|error| format!("LZ4 header seek: {error}"))?;
        file.write_all(&header)
            .and_then(|_| file.write_all(&index_bytes))
            .map_err(|error| format!("LZ4 header/index write: {error}"))?;
        file.flush()
            .and_then(|_| file.sync_data())
            .map_err(|error| format!("LZ4 spill sync: {error}"))?;
        let (storage_checksum, _) = storage_checksum_reader(&mut file)?;
        drop(file);
        std::fs::rename(&temp_path, &final_path)
            .map_err(|error| format!("LZ4 spill commit: {error}"))?;
        if let Ok(directory) = File::open(dir) {
            directory
                .sync_all()
                .map_err(|error| format!("LZ4 spill directory sync: {error}"))?;
        }
        Ok(storage_checksum)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
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
    /// Checksum de los bytes físicos del backing store. Para LZ4 permite
    /// detectar corrupción en un bloque no solicitado leyendo el archivo una
    /// vez, sin descomprimir el frame completo.
    storage_checksums: Vec<u64>,
    /// Checksum ya verificado en ESTA sesión: las pasadas 2 y 3 de la
    /// integración releían el mismo frame y re-hasheaban 140 MB cada vez
    /// (~34 GB de SipHash por run). La integridad se garantiza en la primera
    /// lectura tras abrir el store; una escritura nueva la resetea.
    verified: Vec<std::sync::atomic::AtomicBool>,
    verify_locks: Vec<Mutex<()>>,
    /// Índice LZ4 cargado una sola vez por frame; Arc permite soltar el lock
    /// antes de I/O y descompresión.
    lz4_indices: Vec<Mutex<Option<Arc<Lz4FrameIndex>>>>,
    bytes_written: u64,
    read_hits: AtomicUsize,
    storage_bytes_read: AtomicU64,
    lz4_decoded_bytes: AtomicU64,
    lz4_blocks_decoded: AtomicUsize,
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
        let safe: String = fingerprint
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        Self::open(
            capacity,
            frame_len,
            cache_dir,
            &format!("v{FRAME_CACHE_VERSION}_{safe}"),
            true,
        )
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
            .checked_mul(frame_len as u64)
            .and_then(|value| value.checked_mul(std::mem::size_of::<f32>() as u64))
            .ok_or_else(|| "FrameStore demasiado grande para direccionar".to_string())?;
        let mut sys = sysinfo::System::new_all();
        sys.refresh_memory();
        let ram_budget = sys.available_memory().saturating_mul(30) / 100;
        let manifest_path = persistent.then(|| cache_dir.join(format!("{tag}.manifest.json")));
        let data_path = cache_dir.join(format!("{tag}.f32cache"));
        let mut cached_manifest = manifest_path.as_ref().and_then(|path| {
            let bytes = std::fs::read(path).ok()?;
            let m: FrameStoreManifest = serde_json::from_slice(&bytes).ok()?;
            let base_valid = m.version == FRAME_CACHE_VERSION
                && m.capacity == capacity
                && m.frame_len == frame_len
                && m.complete
                && m.present.len() == capacity
                && m.checksums.len() == capacity
                && m.storage_checksums.len() == capacity;
            if !base_valid {
                return None;
            }
            let storage_valid = match m.backend {
                FrameStoreManifestBackend::Mmap => std::fs::metadata(&data_path)
                    .ok()
                    .is_some_and(|metadata| metadata.len() == total_bytes),
                FrameStoreManifestBackend::Lz4Blocks => {
                    m.present.iter().enumerate().all(|(i, p)| {
                        !*p || lz4_frame_path(cache_dir, tag, i)
                            .metadata()
                            .ok()
                            .is_some_and(|metadata| metadata.len() >= LZ4_HEADER_LEN as u64)
                    })
                }
            };
            storage_valid.then_some(m)
        });
        let mut reuse_disk = cached_manifest.is_some();
        // Una caché persistente usa mmap incluso si cabría en RAM, para poder
        // sobrevivir entre reintegraciones/sesiones sin duplicar varios GB.
        let backend = if matches!(
            cached_manifest.as_ref().map(|manifest| manifest.backend),
            Some(FrameStoreManifestBackend::Lz4Blocks)
        ) {
            FrameStoreBackend::Lz4 {
                dir: cache_dir.to_path_buf(),
                prefix: tag.into(),
            }
        } else if !persistent && total_bytes <= ram_budget && total_bytes <= 2 * 1024 * 1024 * 1024
        {
            FrameStoreBackend::Ram((0..capacity).map(|_| None).collect())
        } else {
            std::fs::create_dir_all(cache_dir).map_err(|e| format!("Cache dir: {e}"))?;
            let mut opts = OpenOptions::new();
            opts.read(true).write(true).create(true);
            if !reuse_disk {
                opts.truncate(true);
            }
            let mapped = opts.open(&data_path).ok().and_then(|file| {
                if !reuse_disk && file.set_len(total_bytes).is_err() {
                    return None;
                }
                // SAFETY: el archivo es propiedad exclusiva del store, ya
                // tiene el tamaño completo y no se redimensiona con el map vivo.
                unsafe { MmapMut::map_mut(&file) }
                    .ok()
                    .map(|map| (map, file))
            });
            match mapped {
                Some((map, file)) => FrameStoreBackend::Mmap {
                    map,
                    file,
                    path: data_path.clone(),
                },
                None => {
                    // Si una caché mmap existente ya no puede abrirse, no se
                    // conservan sus bits `present`: el fallback LZ4 empieza
                    // vacío y nunca declara reutilización falsa.
                    if reuse_disk {
                        cached_manifest = None;
                        reuse_disk = false;
                    }
                    FrameStoreBackend::Lz4 {
                        dir: cache_dir.to_path_buf(),
                        prefix: tag.into(),
                    }
                }
            }
        };
        Ok(Self {
            backend,
            frame_len,
            capacity,
            present: cached_manifest
                .as_ref()
                .map(|m| m.present.clone())
                .unwrap_or_else(|| vec![false; capacity]),
            checksums: cached_manifest
                .as_ref()
                .map(|m| m.checksums.clone())
                .unwrap_or_else(|| vec![0; capacity]),
            storage_checksums: cached_manifest
                .map(|m| m.storage_checksums)
                .unwrap_or_else(|| vec![0; capacity]),
            verified: (0..capacity)
                .map(|_| std::sync::atomic::AtomicBool::new(false))
                .collect(),
            verify_locks: (0..capacity).map(|_| Mutex::new(())).collect(),
            lz4_indices: (0..capacity).map(|_| Mutex::new(None)).collect(),
            bytes_written: 0,
            read_hits: AtomicUsize::new(0),
            storage_bytes_read: AtomicU64::new(0),
            lz4_decoded_bytes: AtomicU64::new(0),
            lz4_blocks_decoded: AtomicUsize::new(0),
            persistent,
            manifest_path,
            reused: reuse_disk,
        })
    }

    #[cfg(test)]
    fn new_lz4_for_test(
        capacity: usize,
        frame_len: usize,
        cache_dir: &Path,
        tag: &str,
        persistent: bool,
    ) -> Result<Self, String> {
        if capacity == 0 || frame_len == 0 {
            return Err("FrameStore LZ4 de prueba con geometría vacía".into());
        }
        std::fs::create_dir_all(cache_dir).map_err(|error| error.to_string())?;
        Ok(Self {
            backend: FrameStoreBackend::Lz4 {
                dir: cache_dir.to_path_buf(),
                prefix: tag.to_string(),
            },
            frame_len,
            capacity,
            present: vec![false; capacity],
            checksums: vec![0; capacity],
            storage_checksums: vec![0; capacity],
            verified: (0..capacity)
                .map(|_| std::sync::atomic::AtomicBool::new(false))
                .collect(),
            verify_locks: (0..capacity).map(|_| Mutex::new(())).collect(),
            lz4_indices: (0..capacity).map(|_| Mutex::new(None)).collect(),
            bytes_written: 0,
            read_hits: AtomicUsize::new(0),
            storage_bytes_read: AtomicU64::new(0),
            lz4_decoded_bytes: AtomicU64::new(0),
            lz4_blocks_decoded: AtomicUsize::new(0),
            persistent,
            manifest_path: persistent.then(|| cache_dir.join(format!("{tag}.manifest.json"))),
            reused: false,
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
            return Err(format!(
                "FrameStore put inválido: idx={index}, len={}",
                data.len()
            ));
        }
        // Resolver cualquier lock envenenado antes de modificar el backing
        // store; después del commit físico no debe quedar un error fallible que
        // conserve metadata anterior apuntando al archivo nuevo.
        *self.lz4_indices[index]
            .get_mut()
            .map_err(|_| "FrameStore: lock de índice LZ4 envenenado".to_string())? = None;
        let logical_checksum = frame_checksum(data);
        let storage_checksum = match &mut self.backend {
            FrameStoreBackend::Ram(frames) => {
                let mut owned = Vec::new();
                owned.try_reserve_exact(data.len()).map_err(|_| {
                    "FrameStore: memoria insuficiente para copiar frame a RAM".to_string()
                })?;
                owned.extend_from_slice(data);
                frames[index] = Some(owned);
                logical_checksum
            }
            FrameStoreBackend::Mmap { map, file, .. } => {
                let off = index
                    .checked_mul(self.frame_len)
                    .and_then(|value| value.checked_mul(std::mem::size_of::<f32>()))
                    .ok_or_else(|| "FrameStore: offset mmap de escritura desborda".to_string())?;
                let bytes: &[u8] = bytemuck::cast_slice(data);
                let end = off
                    .checked_add(bytes.len())
                    .ok_or_else(|| "FrameStore: final mmap de escritura desborda".to_string())?;
                if end > map.len() {
                    return Err("FrameStore: mmap truncado al escribir".to_string());
                }
                #[cfg(unix)]
                {
                    // `set_len` deja el archivo sparse en APFS/ext4: escribir
                    // por el mapping sobre una página sin bloque asignado con
                    // el disco lleno mata el proceso con SIGBUS, sin pasar por
                    // este Result. `pwrite` materializa la página y convierte
                    // disco-lleno en un ENOSPC limpio; lectura por el mapping
                    // y escritura por el fd son coherentes (caché unificada).
                    use std::os::unix::fs::FileExt;
                    file.write_all_at(bytes, off as u64).map_err(|error| {
                        format!("FrameStore: escritura a disco fallida (¿disco lleno?): {error}")
                    })?;
                }
                #[cfg(not(unix))]
                {
                    // En Windows `set_len` reserva clusters reales (disco lleno
                    // falla al crear el store) y WriteFile no es coherente con
                    // las vistas mapeadas: se conserva la escritura por el map.
                    let _ = &file;
                    map.get_mut(off..end)
                        .ok_or_else(|| "FrameStore: mmap truncado al escribir".to_string())?
                        .copy_from_slice(bytes);
                }
                logical_checksum
            }
            FrameStoreBackend::Lz4 { dir, prefix } => {
                write_lz4_block_frame(dir, prefix, index, data, logical_checksum)?
            }
        };
        self.present[index] = true;
        self.checksums[index] = logical_checksum;
        self.storage_checksums[index] = storage_checksum;
        self.verified[index].store(false, Ordering::Relaxed);
        self.bytes_written = self
            .bytes_written
            .saturating_add((data.len() as u64).saturating_mul(4));
        Ok(())
    }

    /// Verifica la integridad del frame directamente sobre su almacenamiento,
    /// sin materializar una copia `Vec<f32>`. El resultado se conserva sólo
    /// durante la sesión actual y una escritura posterior lo invalida.
    fn verify_in_place_once(&self, index: usize) -> Result<(), String> {
        if self.verified[index].load(Ordering::Acquire) {
            return Ok(());
        }
        let _guard = self.verify_locks[index]
            .lock()
            .map_err(|_| "FrameStore: lock de verificación envenenado".to_string())?;
        if self.verified[index].load(Ordering::Acquire) {
            return Ok(());
        }
        let mut last_err = String::new();
        for _ in 0..3 {
            let checksum_ok = match &self.backend {
                FrameStoreBackend::Ram(frames) => {
                    let frame = frames[index]
                        .as_deref()
                        .ok_or_else(|| "Frame RAM ausente".to_string())?;
                    frame_checksum(frame) == self.checksums[index]
                }
                FrameStoreBackend::Mmap { map, .. } => {
                    let off = index
                        .checked_mul(self.frame_len)
                        .and_then(|v| v.checked_mul(std::mem::size_of::<f32>()))
                        .ok_or_else(|| "FrameStore: offset mmap fuera de rango".to_string())?;
                    let byte_len = self
                        .frame_len
                        .checked_mul(std::mem::size_of::<f32>())
                        .ok_or_else(|| "FrameStore: longitud mmap fuera de rango".to_string())?;
                    let end = off
                        .checked_add(byte_len)
                        .ok_or_else(|| "FrameStore: final mmap fuera de rango".to_string())?;
                    let bytes = map
                        .get(off..end)
                        .ok_or_else(|| "FrameStore: frame mmap truncado".to_string())?;
                    frame_checksum_bytes(bytes) == self.checksums[index]
                }
                FrameStoreBackend::Lz4 { dir, prefix } => {
                    let expected_storage = self.storage_checksums[index];
                    let path = lz4_frame_path(dir, prefix, index);
                    let mut file = File::open(&path)
                        .map_err(|error| format!("LZ4 read {}: {error}", path.display()))?;
                    let (storage_checksum, bytes_read) = storage_checksum_reader(&mut file)?;
                    self.storage_bytes_read
                        .fetch_add(bytes_read, Ordering::Relaxed);
                    storage_checksum == expected_storage
                }
            };
            if checksum_ok {
                self.verified[index].store(true, Ordering::Release);
                return Ok(());
            }
            last_err = format!("FrameStore: checksum corrupto en frame {index}");
        }
        Err(last_err)
    }

    fn load_lz4_index(&self, index: usize) -> Result<Arc<Lz4FrameIndex>, String> {
        let (dir, prefix) = match &self.backend {
            FrameStoreBackend::Lz4 { dir, prefix } => (dir, prefix),
            _ => return Err("FrameStore: índice LZ4 solicitado a otro backend".into()),
        };
        let mut cached = self.lz4_indices[index]
            .lock()
            .map_err(|_| "FrameStore: lock de índice LZ4 envenenado".to_string())?;
        if let Some(index) = cached.as_ref() {
            return Ok(Arc::clone(index));
        }

        let path = lz4_frame_path(dir, prefix, index);
        let mut file = File::open(&path)
            .map_err(|error| format!("LZ4 index open {}: {error}", path.display()))?;
        let mut header = [0u8; LZ4_HEADER_LEN];
        file.read_exact(&mut header)
            .map_err(|error| format!("LZ4 header read: {error}"))?;
        self.storage_bytes_read
            .fetch_add(LZ4_HEADER_LEN as u64, Ordering::Relaxed);
        if &header[..8] != LZ4_BLOCK_MAGIC {
            return Err(
                "FrameStore: formato LZ4 antiguo o magic inválido; caché v4 requerida".into(),
            );
        }
        let version = read_u32_le(&header, 8)?;
        let header_len = read_u32_le(&header, 12)? as usize;
        let frame_len = usize::try_from(read_u64_le(&header, 16)?)
            .map_err(|_| "FrameStore: frame_len LZ4 excede usize".to_string())?;
        let block_elems = read_u32_le(&header, 24)? as usize;
        let block_count = read_u32_le(&header, 28)? as usize;
        let logical_checksum = read_u64_le(&header, 32)?;
        let index_checksum = read_u64_le(&header, 40)?;
        if version != FRAME_CACHE_VERSION
            || header_len != LZ4_HEADER_LEN
            || frame_len != self.frame_len
            || block_elems != LZ4_BLOCK_ELEMS
            || block_count != frame_len.div_ceil(block_elems)
            || logical_checksum != self.checksums[index]
        {
            return Err(format!(
                "FrameStore: header LZ4 v4 incompatible en frame {index}"
            ));
        }
        let index_len = block_count
            .checked_mul(LZ4_INDEX_ENTRY_LEN)
            .ok_or_else(|| "FrameStore: índice LZ4 desborda".to_string())?;
        let mut index_bytes = try_zeroed_bytes(index_len, "lectura de índice LZ4")?;
        file.read_exact(&mut index_bytes)
            .map_err(|error| format!("LZ4 index read: {error}"))?;
        self.storage_bytes_read
            .fetch_add(index_len as u64, Ordering::Relaxed);
        if frame_checksum_bytes(&index_bytes) != index_checksum {
            return Err(format!(
                "FrameStore: checksum de índice LZ4 corrupto en frame {index}"
            ));
        }
        let file_len = file
            .metadata()
            .map_err(|error| format!("LZ4 metadata: {error}"))?
            .len();
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(block_count)
            .map_err(|_| "FrameStore: memoria insuficiente para índice LZ4".to_string())?;
        let mut expected_offset = (LZ4_HEADER_LEN + index_len) as u64;
        let mut total_elems = 0usize;
        for block in 0..block_count {
            let offset = block * LZ4_INDEX_ENTRY_LEN;
            let entry = Lz4BlockEntry {
                offset: read_u64_le(&index_bytes, offset)?,
                compressed_len: read_u32_le(&index_bytes, offset + 8)?,
                uncompressed_elems: read_u32_le(&index_bytes, offset + 12)?,
                uncompressed_checksum: read_u64_le(&index_bytes, offset + 16)?,
            };
            let expected_elems = (frame_len - total_elems).min(block_elems);
            let end = entry
                .offset
                .checked_add(entry.compressed_len as u64)
                .ok_or_else(|| "FrameStore: bloque LZ4 desborda offset".to_string())?;
            if entry.offset != expected_offset
                || entry.compressed_len == 0
                || entry.uncompressed_elems as usize != expected_elems
                || end > file_len
            {
                return Err(format!("FrameStore: índice LZ4 inválido, bloque {block}"));
            }
            expected_offset = end;
            total_elems = total_elems
                .checked_add(expected_elems)
                .ok_or_else(|| "FrameStore: suma de bloques LZ4 desborda".to_string())?;
            blocks.push(entry);
        }
        if total_elems != frame_len || expected_offset != file_len {
            return Err("FrameStore: longitud física/lógica LZ4 inconsistente".into());
        }
        let parsed = Arc::new(Lz4FrameIndex {
            block_elems,
            frame_len,
            blocks,
        });
        *cached = Some(Arc::clone(&parsed));
        Ok(parsed)
    }

    fn read_lz4_ranges(
        &self,
        index: usize,
        ranges: &[std::ops::Range<usize>],
    ) -> Result<Vec<Vec<f32>>, String> {
        self.verify_in_place_once(index)?;
        let frame_index = self.load_lz4_index(index)?;
        let (dir, prefix) = match &self.backend {
            FrameStoreBackend::Lz4 { dir, prefix } => (dir, prefix),
            _ => return Err("FrameStore: lectura LZ4 solicitada a otro backend".into()),
        };
        let mut outputs = Vec::new();
        outputs
            .try_reserve_exact(ranges.len())
            .map_err(|_| "FrameStore: memoria insuficiente para rangos LZ4".to_string())?;
        let mut needed = std::collections::BTreeSet::new();
        for range in ranges {
            if range.start > range.end || range.end > frame_index.frame_len {
                return Err(format!(
                    "FrameStore range inválido: {}..{} de {}",
                    range.start, range.end, frame_index.frame_len
                ));
            }
            let mut output = Vec::new();
            output.try_reserve_exact(range.len()).map_err(|_| {
                "FrameStore: memoria insuficiente para salida de rango LZ4".to_string()
            })?;
            output.resize(range.len(), 0.0);
            outputs.push(output);
            if !range.is_empty() {
                let first = range.start / frame_index.block_elems;
                let last = (range.end - 1) / frame_index.block_elems;
                needed.extend(first..=last);
            }
        }
        if needed.is_empty() {
            return Ok(outputs);
        }

        let path = lz4_frame_path(dir, prefix, index);
        let mut file = File::open(&path)
            .map_err(|error| format!("LZ4 range open {}: {error}", path.display()))?;
        let max_compressed = needed
            .iter()
            .filter_map(|&block| frame_index.blocks.get(block))
            .map(|entry| entry.compressed_len as usize)
            .max()
            .unwrap_or(0);
        LZ4_READ_SCRATCH.with(|scratch| -> Result<(), String> {
            let mut scratch = scratch
                .try_borrow_mut()
                .map_err(|_| "FrameStore: scratch LZ4 reentrante".to_string())?;
            if scratch.compressed.capacity() < max_compressed {
                let additional = max_compressed.saturating_sub(scratch.compressed.len());
                scratch
                    .compressed
                    .try_reserve_exact(additional)
                    .map_err(|_| {
                        "FrameStore: memoria insuficiente para bloque LZ4 comprimido".to_string()
                    })?;
            }
            scratch.compressed.resize(max_compressed, 0);
            let Lz4ReadScratch {
                compressed,
                decoded: decoded_buffer,
            } = &mut *scratch;
            for block in needed {
                let entry = frame_index
                    .blocks
                    .get(block)
                    .ok_or_else(|| format!("FrameStore: bloque LZ4 {block} fuera de rango"))?;
                let elems = entry.uncompressed_elems as usize;
                if decoded_buffer.capacity() < elems {
                    let additional = elems.saturating_sub(decoded_buffer.len());
                    decoded_buffer.try_reserve_exact(additional).map_err(|_| {
                        "FrameStore: memoria insuficiente para scratch LZ4".to_string()
                    })?;
                }
                decoded_buffer.resize(elems, 0.0);
                file.seek(SeekFrom::Start(entry.offset))
                    .and_then(|_| file.read_exact(&mut compressed[..entry.compressed_len as usize]))
                    .map_err(|error| format!("LZ4 block {block} read: {error}"))?;
                self.storage_bytes_read
                    .fetch_add(entry.compressed_len as u64, Ordering::Relaxed);
                let expected_bytes = elems
                    .checked_mul(std::mem::size_of::<f32>())
                    .ok_or_else(|| "FrameStore: salida LZ4 desborda".to_string())?;
                let decoded_bytes = {
                    let output_bytes: &mut [u8] =
                        bytemuck::cast_slice_mut(&mut decoded_buffer[..elems]);
                    lz4_flex::block::decompress_into(
                        &compressed[..entry.compressed_len as usize],
                        output_bytes,
                    )
                    .map_err(|error| format!("LZ4 block {block} decode: {error}"))?
                };
                if decoded_bytes != expected_bytes
                    || frame_checksum(&decoded_buffer[..elems]) != entry.uncompressed_checksum
                {
                    return Err(format!(
                        "FrameStore: checksum corrupto en frame {index}, bloque {block}"
                    ));
                }
                self.lz4_decoded_bytes
                    .fetch_add(decoded_bytes as u64, Ordering::Relaxed);
                self.lz4_blocks_decoded.fetch_add(1, Ordering::Relaxed);
                let block_start = block * frame_index.block_elems;
                let block_end = block_start + elems;
                for (range, output) in ranges.iter().zip(outputs.iter_mut()) {
                    let overlap_start = range.start.max(block_start);
                    let overlap_end = range.end.min(block_end);
                    if overlap_start < overlap_end {
                        let src_start = overlap_start - block_start;
                        let dst_start = overlap_start - range.start;
                        let len = overlap_end - overlap_start;
                        output[dst_start..dst_start + len]
                            .copy_from_slice(&decoded_buffer[src_start..src_start + len]);
                    }
                }
            }
            Ok(())
        })?;
        Ok(outputs)
    }

    fn read_ranges(
        &self,
        index: usize,
        ranges: &[std::ops::Range<usize>],
    ) -> Result<Vec<Vec<f32>>, String> {
        if index >= self.capacity || !self.present.get(index).copied().unwrap_or(false) {
            return Err(format!("FrameStore: frame {index} no disponible"));
        }
        for range in ranges {
            if range.start > range.end || range.end > self.frame_len {
                return Err(format!(
                    "FrameStore range inválido: {}..{} de {}",
                    range.start, range.end, self.frame_len
                ));
            }
        }
        if matches!(&self.backend, FrameStoreBackend::Lz4 { .. }) {
            return self.read_lz4_ranges(index, ranges);
        }
        self.verify_in_place_once(index)?;
        let mut outputs = Vec::new();
        outputs
            .try_reserve_exact(ranges.len())
            .map_err(|_| "FrameStore: memoria insuficiente para rangos".to_string())?;
        for range in ranges {
            let values = match &self.backend {
                FrameStoreBackend::Ram(frames) => frames[index]
                    .as_ref()
                    .map(|frame| &frame[range.clone()])
                    .ok_or_else(|| "Frame RAM ausente".to_string())?,
                FrameStoreBackend::Mmap { map, .. } => {
                    let frame_start = index
                        .checked_mul(self.frame_len)
                        .ok_or_else(|| "FrameStore: offset mmap fuera de rango".to_string())?;
                    let start = frame_start
                        .checked_add(range.start)
                        .and_then(|value| value.checked_mul(std::mem::size_of::<f32>()))
                        .ok_or_else(|| "FrameStore: inicio mmap fuera de rango".to_string())?;
                    let end = frame_start
                        .checked_add(range.end)
                        .and_then(|value| value.checked_mul(std::mem::size_of::<f32>()))
                        .ok_or_else(|| "FrameStore: final mmap fuera de rango".to_string())?;
                    bytemuck::try_cast_slice(
                        map.get(start..end)
                            .ok_or_else(|| "FrameStore: rango mmap truncado".to_string())?,
                    )
                    .map_err(|_| "Frame mmap desalineado".to_string())?
                }
                FrameStoreBackend::Lz4 { .. } => unreachable!(),
            };
            let mut output = Vec::new();
            output
                .try_reserve_exact(values.len())
                .map_err(|_| "FrameStore: memoria insuficiente para rango".to_string())?;
            output.extend_from_slice(values);
            outputs.push(output);
        }
        Ok(outputs)
    }

    pub fn get(&self, index: usize) -> Result<Vec<f32>, String> {
        let mut frames = self.read_ranges(index, &[0..self.frame_len])?;
        let data = frames
            .pop()
            .ok_or_else(|| "FrameStore: lectura completa vacía".to_string())?;
        self.read_hits.fetch_add(1, Ordering::Relaxed);
        Ok(data)
    }

    /// Lee sólo un rango. LZ4 abre el índice v4 y descomprime exclusivamente
    /// los bloques que intersectan el rango; nunca materializa el frame entero.
    pub fn get_range(
        &self,
        index: usize,
        range: std::ops::Range<usize>,
    ) -> Result<Vec<f32>, String> {
        let mut ranges = self.read_ranges(index, &[range])?;
        self.read_hits.fetch_add(1, Ordering::Relaxed);
        ranges
            .pop()
            .ok_or_else(|| "FrameStore: rango ausente".to_string())
    }

    /// Lectura rectangular interleaved. En LZ4 se calcula la unión de bloques
    /// de todas las filas y cada bloque se descomprime como máximo una vez.
    pub fn get_rect(
        &self,
        index: usize,
        width: usize,
        height: usize,
        channels: usize,
        x: std::ops::Range<usize>,
        y: std::ops::Range<usize>,
    ) -> Result<Vec<f32>, String> {
        if width == 0
            || height == 0
            || channels == 0
            || x.start > x.end
            || x.end > width
            || y.start > y.end
            || y.end > height
            || width
                .checked_mul(height)
                .and_then(|value| value.checked_mul(channels))
                != Some(self.frame_len)
        {
            return Err("FrameStore: geometría/rango rectangular inválido".into());
        }
        let mut row_ranges = Vec::new();
        row_ranges
            .try_reserve_exact(y.len())
            .map_err(|_| "FrameStore: memoria insuficiente para filas rectangulares".to_string())?;
        for row in y.clone() {
            let row_start = row
                .checked_mul(width)
                .and_then(|value| value.checked_add(x.start))
                .and_then(|value| value.checked_mul(channels))
                .ok_or_else(|| "FrameStore: inicio rectangular desborda".to_string())?;
            let row_end = row
                .checked_mul(width)
                .and_then(|value| value.checked_add(x.end))
                .and_then(|value| value.checked_mul(channels))
                .ok_or_else(|| "FrameStore: final rectangular desborda".to_string())?;
            row_ranges.push(row_start..row_end);
        }
        let rows = self.read_ranges(index, &row_ranges)?;
        let output_len = x
            .len()
            .checked_mul(y.len())
            .and_then(|value| value.checked_mul(channels))
            .ok_or_else(|| "FrameStore: salida rectangular desborda".to_string())?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(output_len)
            .map_err(|_| "FrameStore: memoria insuficiente para salida rectangular".to_string())?;
        for row in rows {
            output.extend_from_slice(&row);
        }
        self.read_hits.fetch_add(1, Ordering::Relaxed);
        Ok(output)
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
    pub fn read_hits(&self) -> usize {
        self.read_hits.load(Ordering::Relaxed)
    }
    /// Bytes físicos leídos del spill LZ4 (incluye la verificación completa
    /// única y luego sólo header/índice/bloques solicitados).
    pub fn storage_bytes_read(&self) -> u64 {
        self.storage_bytes_read.load(Ordering::Relaxed)
    }
    /// Bytes sin comprimir realmente decodificados. Permite demostrar que una
    /// franja no materializó el frame completo.
    pub fn lz4_decoded_bytes(&self) -> u64 {
        self.lz4_decoded_bytes.load(Ordering::Relaxed)
    }
    pub fn lz4_blocks_decoded(&self) -> usize {
        self.lz4_blocks_decoded.load(Ordering::Relaxed)
    }
    pub fn is_present(&self, index: usize) -> bool {
        self.present.get(index).copied().unwrap_or(false)
    }
    pub fn reused(&self) -> bool {
        self.reused
    }

    pub fn mark_complete(&mut self) -> Result<(), String> {
        storage_write_checkpoint()?;
        if !self.persistent {
            return Ok(());
        }
        if let FrameStoreBackend::Mmap { map, file, .. } = &mut self.backend {
            map.flush().map_err(|e| format!("Cache mmap flush: {e}"))?;
            file.sync_data().map_err(|e| format!("Cache sync: {e}"))?;
        }
        let Some(path) = &self.manifest_path else {
            return Ok(());
        };
        let manifest = FrameStoreManifest {
            version: FRAME_CACHE_VERSION,
            capacity: self.capacity,
            frame_len: self.frame_len,
            complete: true,
            present: self.present.clone(),
            checksums: self.checksums.clone(),
            storage_checksums: self.storage_checksums.clone(),
            backend: match &self.backend {
                FrameStoreBackend::Mmap { .. } => FrameStoreManifestBackend::Mmap,
                FrameStoreBackend::Lz4 { .. } => FrameStoreManifestBackend::Lz4Blocks,
                FrameStoreBackend::Ram(_) => {
                    return Err("Cache persistente no puede publicar backend RAM".into())
                }
            },
        };
        let bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|e| format!("Cache manifest create: {e}"))?;
        file.write_all(&bytes)
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_data())
            .map_err(|e| format!("Cache manifest sync: {e}"))?;
        drop(file);
        std::fs::rename(&tmp, path).map_err(|e| format!("Cache manifest commit: {e}"))?;
        if let Some(parent) = path.parent() {
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }
}

fn checksum_update(checksum: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *checksum ^= byte as u64;
        *checksum = checksum.wrapping_mul(CACHE_CHECKSUM_PRIME);
    }
}

fn frame_checksum_bytes(bytes: &[u8]) -> u64 {
    let mut checksum = CACHE_CHECKSUM_OFFSET;
    checksum_update(&mut checksum, bytes);
    checksum
}

fn frame_checksum(data: &[f32]) -> u64 {
    frame_checksum_bytes(bytemuck::cast_slice::<f32, u8>(data))
}

impl Drop for AdaptiveFrameStore {
    fn drop(&mut self) {
        match &mut self.backend {
            FrameStoreBackend::Ram(_) => {}
            FrameStoreBackend::Mmap { map, file, path } => {
                let _ = map.flush();
                let _ = file.sync_data();
                if self.persistent {
                    return;
                }
                let path = path.clone();
                // Windows no permite borrar mientras el mapping está vivo; el
                // archivo se intenta limpiar ahora y también en el próximo run.
                let _ = std::fs::remove_file(path);
            }
            FrameStoreBackend::Lz4 { dir, prefix } => {
                if self.persistent {
                    return;
                }
                if let Ok(rd) = std::fs::read_dir(dir) {
                    for entry in rd.flatten() {
                        if entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(prefix.as_str())
                        {
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
        let dir =
            std::env::temp_dir().join(format!("zas-frame-store-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let frame: Vec<f32> = (0..16).map(|i| i as f32 - 8.0).collect();
        {
            let mut store =
                AdaptiveFrameStore::new_versioned(2, 16, &dir, "source-fingerprint").unwrap();
            store.put(0, &frame).unwrap();
            store.mark_complete().unwrap();
        }
        {
            let store =
                AdaptiveFrameStore::new_versioned(2, 16, &dir, "source-fingerprint").unwrap();
            assert!(store.reused());
            assert_eq!(store.get_range(0, 3..7).unwrap(), frame[3..7]);
            assert!(store.verified[0].load(Ordering::Acquire));
        }
        let data_path = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|s| s.to_str()) == Some("f32cache"))
            .unwrap();
        let mut file = OpenOptions::new().write(true).open(data_path).unwrap();
        // Corromper FUERA del rango que se solicitará prueba que get_range
        // valida el frame completo, pero sin construir una copia completa.
        file.seek(SeekFrom::Start(15 * 4)).unwrap();
        file.write_all(&12345.0f32.to_ne_bytes()).unwrap();
        file.flush().unwrap();
        let store = AdaptiveFrameStore::new_versioned(2, 16, &dir, "source-fingerprint").unwrap();
        assert!(store.get_range(0, 0..2).unwrap_err().contains("checksum"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_full_is_reported_without_committing_a_partial_frame() {
        let dir =
            std::env::temp_dir().join(format!("zas-frame-store-disk-full-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = AdaptiveFrameStore::new_versioned(2, 8, &dir, "disk-full").unwrap();
        TEST_DISK_FULL.with(|flag| flag.set(true));
        let err = store.put(0, &[1.0; 8]).unwrap_err();
        TEST_DISK_FULL.with(|flag| flag.set(false));
        assert!(err.contains("disco lleno"));
        assert!(
            !store.is_present(0),
            "un write fallido no debe marcar el frame como válido"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn patterned_frame(len: usize) -> Vec<f32> {
        (0..len)
            .map(|index| {
                let x = index as u64;
                ((x.wrapping_mul(1_103_515_245).wrapping_add(12_345) >> 8) & 0xffff) as f32 / 257.0
                    - 90.0
            })
            .collect()
    }

    #[test]
    fn lz4_v4_range_crosses_blocks_without_full_frame_decode() {
        let dir = std::env::temp_dir().join(format!(
            "zas-frame-store-lz4-block-range-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // Cinco bloques hacen que el gate frío sea estable incluso con datos
        // casi incompresibles: checksum físico (~1×) + 2/5 bloques (<0.5×).
        let frame_len = 5 * LZ4_BLOCK_ELEMS + 17;
        let frame = patterned_frame(frame_len);
        let mut store =
            AdaptiveFrameStore::new_lz4_for_test(1, frame_len, &dir, "block-range", false).unwrap();
        store.put(0, &frame).unwrap();

        let range = LZ4_BLOCK_ELEMS - 7..LZ4_BLOCK_ELEMS + 11;
        assert_eq!(store.get_range(0, range.clone()).unwrap(), frame[range]);
        assert_eq!(store.lz4_blocks_decoded(), 2);
        assert_eq!(
            store.lz4_decoded_bytes(),
            (2 * LZ4_BLOCK_ELEMS * std::mem::size_of::<f32>()) as u64
        );
        assert!(
            store.lz4_decoded_bytes() < (frame_len * std::mem::size_of::<f32>()) as u64,
            "get_range no debe descomprimir el frame completo"
        );
        let logical_bytes = (frame_len * std::mem::size_of::<f32>()) as u64;
        assert!(
            store.storage_bytes_read() * 2 <= logical_bytes * 3,
            "verificación fría + rango debe quedar ≤1.5× del frame lógico: {} vs {}",
            store.storage_bytes_read(),
            logical_bytes
        );

        let before_read = store.storage_bytes_read();
        let before_decoded = store.lz4_decoded_bytes();
        let tail = 5 * LZ4_BLOCK_ELEMS..frame_len;
        assert_eq!(store.get_range(0, tail.clone()).unwrap(), frame[tail]);
        let read_delta = store.storage_bytes_read() - before_read;
        let decoded_delta = store.lz4_decoded_bytes() - before_decoded;
        assert_eq!(decoded_delta, (17 * std::mem::size_of::<f32>()) as u64);
        assert!(
            read_delta < (frame_len * std::mem::size_of::<f32>()) as u64 / 4,
            "una lectura caliente sólo debe leer el bloque solicitado: {read_delta} bytes"
        );

        // `get()` también usa el índice v4, pero naturalmente solicita todos
        // los bloques y conserva paridad float32 exacta.
        assert_eq!(store.get(0).unwrap(), frame);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lz4_v4_detects_corruption_outside_requested_range_without_full_decode() {
        let dir = std::env::temp_dir().join(format!(
            "zas-frame-store-lz4-corrupt-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let frame_len = 2 * LZ4_BLOCK_ELEMS + 31;
        let frame = patterned_frame(frame_len);
        let mut store =
            AdaptiveFrameStore::new_lz4_for_test(1, frame_len, &dir, "corrupt", false).unwrap();
        store.put(0, &frame).unwrap();
        let parsed = store.load_lz4_index(0).unwrap();
        let last = parsed.blocks.last().unwrap();
        let path = lz4_frame_path(&dir, "corrupt", 0);
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(last.offset)).unwrap();
        file.write_all(&[0x5a]).unwrap();
        file.flush().unwrap();
        drop(file);

        let error = store.get_range(0, 0..16).unwrap_err();
        assert!(error.contains("checksum"), "error inesperado: {error}");
        assert_eq!(
            store.lz4_decoded_bytes(),
            0,
            "la verificación física debe fallar antes de descomprimir"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lz4_v4_block_checksum_catches_corruption_after_session_verification() {
        let dir = std::env::temp_dir().join(format!(
            "zas-frame-store-lz4-hot-corrupt-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let frame_len = 2 * LZ4_BLOCK_ELEMS + 19;
        let frame = patterned_frame(frame_len);
        let mut store =
            AdaptiveFrameStore::new_lz4_for_test(1, frame_len, &dir, "hot-corrupt", false).unwrap();
        store.put(0, &frame).unwrap();
        assert_eq!(store.get_range(0, 0..8).unwrap(), frame[0..8]);
        assert!(store.verified[0].load(Ordering::Acquire));

        let parsed = store.load_lz4_index(0).unwrap();
        let target = &parsed.blocks[1];
        let path = lz4_frame_path(&dir, "hot-corrupt", 0);
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(
            target.offset + target.compressed_len as u64 / 2,
        ))
        .unwrap();
        file.write_all(&[0xa5]).unwrap();
        file.flush().unwrap();
        drop(file);
        let error = store
            .get_range(0, LZ4_BLOCK_ELEMS..LZ4_BLOCK_ELEMS + 8)
            .unwrap_err();
        assert!(
            error.contains("decode") || error.contains("checksum"),
            "error inesperado: {error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lz4_v4_rectangular_read_has_exact_parity_and_unique_block_decode() {
        let dir =
            std::env::temp_dir().join(format!("zas-frame-store-lz4-rect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (width, height, channels) = (513usize, 100usize, 3usize);
        let frame_len = width * height * channels;
        let frame = patterned_frame(frame_len);
        let mut store =
            AdaptiveFrameStore::new_lz4_for_test(1, frame_len, &dir, "rect", false).unwrap();
        store.put(0, &frame).unwrap();
        let (x, y) = (500..513, 40..48);
        let rect = store
            .get_rect(0, width, height, channels, x.clone(), y.clone())
            .unwrap();
        let mut expected = Vec::new();
        for row in y {
            let start = (row * width + x.start) * channels;
            let end = (row * width + x.end) * channels;
            expected.extend_from_slice(&frame[start..end]);
        }
        assert_eq!(rect, expected);
        // Las ocho filas caen alrededor del límite del primer bloque; la
        // unión necesita dos bloques, no ocho descompresiones independientes.
        assert!(store.lz4_blocks_decoded() <= 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_lz4_v4_survives_drop_and_reuses_manifest() {
        let dir = std::env::temp_dir().join(format!(
            "zas-frame-store-lz4-persistent-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let fingerprint = "lz4-persistent";
        let tag = format!("v{FRAME_CACHE_VERSION}_{fingerprint}");
        let frame_len = LZ4_BLOCK_ELEMS + 23;
        let frame = patterned_frame(frame_len);
        {
            let mut store =
                AdaptiveFrameStore::new_lz4_for_test(1, frame_len, &dir, &tag, true).unwrap();
            store.put(0, &frame).unwrap();
            store.mark_complete().unwrap();
        }
        let path = lz4_frame_path(&dir, &tag, 0);
        assert!(
            path.exists(),
            "Drop no debe borrar una caché LZ4 persistente"
        );
        {
            let store = AdaptiveFrameStore::new_versioned(1, frame_len, &dir, fingerprint).unwrap();
            assert!(store.reused());
            assert_eq!(store.kind(), FrameStoreKind::Lz4);
            assert_eq!(store.get_range(0, 9..37).unwrap(), frame[9..37]);
        }
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_lz4_blob_is_rejected_by_explicit_v4_magic() {
        let dir =
            std::env::temp_dir().join(format!("zas-frame-store-lz4-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let frame_len = LZ4_BLOCK_ELEMS + 7;
        let frame = patterned_frame(frame_len);
        let mut store =
            AdaptiveFrameStore::new_lz4_for_test(1, frame_len, &dir, "legacy", false).unwrap();
        store.put(0, &frame).unwrap();
        let path = lz4_frame_path(&dir, "legacy", 0);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b"ZASLZ4V3").unwrap();
        file.flush().unwrap();
        let (new_storage_checksum, _) = storage_checksum_reader(&mut file).unwrap();
        store.storage_checksums[0] = new_storage_checksum;
        store.verified[0].store(false, Ordering::Release);
        *store.lz4_indices[0].lock().unwrap() = None;
        drop(file);
        let error = store.get_range(0, 0..4).unwrap_err();
        assert!(
            error.contains("formato LZ4 antiguo") || error.contains("v4"),
            "error inesperado: {error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
