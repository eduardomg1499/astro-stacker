# Análisis de recursos CPU/GPU/RAM — pipeline planetario y superficie (2026-07-17)

Objetivo: radiografía completa de dónde va el tiempo en análisis y apilado
(planetario y superficie, foco en superficie) y plan priorizado para ocupar
toda la máquina sin tocar ni un bit del resultado, en macOS y Windows.
Cielo profundo queda fuera del alcance.

Basado en trazas reales (`zas-perf-trace-v1`) del M5 (10 núcleos: 4P+6E,
GPU 10c Metal 4, 24 GB) con `P1004387.MOV` (HEVC 10-bit 5888×3312, 2055
frames, 1285 apilados) y `10_26_04.ser` (2620×1532, 445/279 frames), más
lectura de código con file:line verificados. Todas las rutas bajo
`src-tauri/src/`.

---

## 1. Cómo leer las trazas (semántica de perf_trace)

- `add_ns` acumula `total_ns += elapsed` bajo un mutex global
  (perf_trace.rs:82-103). Cuando N workers registran la misma fase, el
  `total` es la SUMA de tiempos de pared de todos los hilos (≈ segundos de
  CPU si van llenos). `avg = total/count` es el coste real por ítem en un
  solo hilo.
- `total_fase / pared_del_job` = concurrencia efectiva de esa fase.
- En análisis, `score` se mide UNA vez por lote alrededor del `par_iter`
  (commands_v2_v3.rs:3255-3310) → su avg es pared/frame; los `a_*` se miden
  POR FRAME dentro de cada worker → su avg es coste real. Por eso
  `a_sad avg 628 ms > score avg 146 ms`: el cociente (0+120.6+1291.8+0)/299.9
  = **4.71× de paralelismo efectivo sobre un pool de 8**.

## 2. Trazas de referencia (mismo MOV, mismo equipo)

| Corrida | Total | Notas |
|---|---|---|
| Análisis planetario (06:55) | **61.8 s** | decode-bound (HW decode ~35 fps); score avg 12.8 ms |
| Análisis superficie (13:36) | **386.4 s** | compute-bound: a_sad 628 ms/frame, 1292 s CPU |
| Apilado planetario caché caliente (07:02) | **453.4 s** | decode_wait ≈ 0; aun así ~2.3 núcleos efectivos |
| Apilado superficie con swap 782 MB (13:52) | **928.7 s** | decode_wait 352 s; hilos/lote colapsados a 6/6 |
| Apilado SER planetario (13:58/14:00) | 22.8/27.2 s | paraleliza bien (~8 núcleos en local_shifts) |

El SER demuestra que la maquinaria paraleliza cuando el suministro y el
plan de RAM no la estrangulan: el problema es específico de la ruta
MOV 20 MP + superficie.

---

## 3. ANÁLISIS de superficie: dónde van los 386 s

Camino crítico del consumidor: `decode_wait 0.3 + gpu_preprocess 80.8 +
score 299.9 ≈ 381 s ≈ pared`. El hilo de decode (377.9 s) está BLOQUEADO
por backpressure (`decode_wait ≈ 0`): decodificar NO es el cuello; el
scoring lo es (commands_v2_v3.rs:3129-3131).

Dentro del scoring (por frame, coste real):

| Sub-fase | avg | CPU total | Qué es |
|---|---|---|---|
| a_prep | ~0 | 0 | buffers GPU ya preparados (1516-1523) |
| a_metric | 58.7 ms | 120.6 s | `score_frame_quality_v2` + normalize_lap (1556-1571) |
| **a_sad** | **628.6 ms** | **1291.8 s** | SAD global de textura (superficie) |
| a_grid | ~0 | 0 | grid 40×40 ya viene de GPU (1714-1739) |

`a_sad` en superficie (`texture_align`, 1494) = semilla gruesa GPU a ¼-res
(caja 736×414, r=32 → 4225 offsets, `search_sad_single_parallel`, 1613-1638)
+ **refino fino CPU ±16 (33×33 = 1089 offsets) sobre caja 1472×828 u16 a
media res** (`refine_best_match_sad_offset`, 1643-1655; NEON en
alignment.rs:866 con early-abort). Dos causas de los 628 ms:

1. El barrido fino CPU es memory-bound (~5 GB/frame de streaming); 8
   workers a la vez saturan el ancho de banda → paralelismo se aplana en 4.7×.
2. La semilla GPU se lanza DESDE los 8 workers en paralelo → contienden el
   mutex `SAD_ENGINE` (gpu_analysis.rs:1264) y la cola física de la GPU, y
   cada worker espera su readback con spin-poll.

Serializaciones adicionales: `gpu_preprocess` (lote) y `score` (lote) corren
SECUENCIALES en el consumidor — GPU ociosa durante el score, CPU ociosa
durante el preprocess (3236 vs 3255); `gpu_batch_len=6 < pool 8` deja 2
workers parados (2922, 591); la GPU calcula un score que luego se descarta
y se recalcula en CPU (gpu_analysis.rs:719/749 vs 1557 — el CPU rescore es
deliberado, "paridad por construcción").

Pool del análisis: `ram_aware_analysis_threads` (1759-1786) = hw−2 (ffmpeg)
= 8 en el M5; el cap por RAM usa `available_memory()` CRUDO (sin
swap/purgable de `resolve_planetary_available_memory`) y dimensiona color a
32 B/px cuando el stream real es G16 (2 B/px) → en máquinas con menos RAM
recorta hilos sin necesidad (no afectó a esta corrida, hw-capped).

Reuso: el shift global del análisis SÍ se reutiliza en el apilado
(frame_stats → render_dx, 8357-8358); el caché de análisis es `_a10`
bincode+LZ4 (374-417, 3690).

## 4. APILADO superficie/MOV: dónde van los 453-928 s

Estructura (commands_v2_v3.rs): 1 hilo prefetcher por pasada →
`sync_channel(1)` de lotes (7491-7540) → bucle consumidor por lotes
(8182) → `chunk.par_iter()` en pool acotado `stack_threads` (7754-7757,
8246) → cadena SERIAL por frame en cada worker: debayer (8371) → normalizar
/verde → enhance (8438) → sad_global (8466) → sad_ap_gpu (8525) →
local_shifts (8560) → envío a hilo GPU (accumulate belt, canal 3, 8072-8129).

Los seis despilfarros, cuantificados:

1. **La pasada 2 recomputa trabajo bit-idéntico: 525 s de CPU.**
   `enhance_for_alignment` (full-res 19.5 Mpx, mono-hilo NEON,
   alignment.rs:62-180) + `downscale_4x` + debayer producen `f_edges/f_ds`
   que dependen solo del frame decodificado y de `surface_ref_p90`,
   calculado UNA vez antes del bucle (7701) → p1 y p2 producen lo mismo.
   p2/enhance 445 s + p2/debayer 79 s tirados. Cachear `f_edges` (39 MB) +
   `f_ds` (2.4 MB) por frame ≈ 53 GB en el caché NVMe (o la fracción que
   quepa en RAM) es bit-exacto.
2. **Suministro de decode estrangulado (mata la corrida en frío): 352 s
   de starvation.** Un solo ffmpeg secuencial con `-threads cpus/3 = 3`
   (types.rs:2049-2081) produce rgb48le por swscale; `raw_to_u16_buffer`
   (NEON pero serial) corre EN el hilo prefetcher (5178/5207);
   `sync_channel(1)` = ≤2 lotes en vuelo. Suministro ~5.4 fps mientras 6
   workers miran.
3. **ref_decode 107-110 s**: `read_batch` decodifica 0..~2000 frames en una
   pasada ascendente para quedarse 20 (comentario 6897); el caché no sirve
   porque el análisis sembró G16 y la referencia pide RGB48 (clave distinta,
   4406-4408); el gate "select ≥95% del span" (4833-4836) impide el pipe
   exacto para 20 frames dispersos.
4. **Lote == hilos → cero holgura y APs en serie.** El floor
   `frames_per_batch ≥ stack_threads` (5969-5984) hace lote exacto = hilos;
   `parallel_aps = chunk.len() < stack_threads && APs ≥ 128` (8586) nunca
   se activa en lotes llenos → ~5882 APs/frame en serie (9505-9513) y un
   frame lento estanca su lote entero.
5. **debayer en MOV = memcpy de 117 MB con zero-fill previo de otros
   117 MB** (`out.resize(target_size,0)` + `copy_from_slice`,
   debayer.rs:68,83-93): ~234 MB de tráfico inútil por frame, 193 s
   sumados en las dos pasadas.
6. **p2 sin coarse GPU**: al desactivar `sad_ap_gpu` en p2 (8496) y sembrar
   de p1, los APs que saturan ±6 tras `rebuild_ref` caen a pirámide CPU
   completa → local_shifts p2 311 s > p1 217 s. Movió ~51 s de GPU a ~+93 s
   de CPU. (Tocar esto es QUALITY-RISK: la vía protege el fix del limbo
   4317de5.)

Plan de RAM (plan_planetary_ram, 5688-6013): fijo ≈ 2.9 GiB (acumulador
double-pass 1.74 + warp_map 0.45 + master 0.74) + `per_thread_cost` ≈
952 MiB (scratch 595 + 2 lotes×178.5) → en 24 GB salen 5-6 hilos y lote
5-6 para 20 MP color. Con swap >256 MB el suelo purgable se desactiva
(d737ec6) y colapsa a 6/6 (corrida de 928 s). GPU: solo warp+accumulate
(+ coarse AP en p1); enhance, SAD global, LK y conversiones, todo en CPU.

## 5. GPU: hoy vs potencial

- Runtime wgpu único compartido (gpu_stack.rs:177, Backends::PRIMARY,
  HighPerformance); presupuesto Apple unificado min(total/6, avail/3)
  clamp [512 MiB, 3 GiB] (203-239), dGPU plano 3 GiB (sin query real de
  VRAM); override `ZAS_GPU_BUDGET_MB`.
- Análisis GPU por lote: downscale→blur→lap→score_rows→(cog)→(grid) en un
  submit + un readback (gpu_analysis.rs:1504-1731). Infra SAD por puntos:
  `search_sad_points` (1236, presupuesto 600 M muestras/submit en Metal,
  200 M no-Metal) y `search_sad_single_parallel` (1149) — exactamente el
  molde para portar el SAD global de superficie.
- Apilado GPU: `accumulate_frame` warp+acumulación+sigma (m2 en p1, bounds
  en p2), submit belt en hilo aparte, chunking Metal 512 M / no-Metal 64 M
  (881-913), poll no bloqueante. No está en el camino crítico.
- La GPU del M5 pasa ociosa la mayor parte de ambos flujos.

## 6. Windows: estado y riesgos

- Paridad SIMD COMPLETA en hot loops (AVX2 y NEON en enhance, SAD rect,
  fast_sample, debayer MHC; LK unchecked auto-vec en ambas arqs)
  (alignment.rs:86-108/768/867, liquid_warping.rs:638/711, debayer.rs:575/684).
- Decode HW sondado y confirmado por fuente (`benchmark_ffmpeg_decode_route`,
  1903-2027; backends d3d11va/dxva2/cuda/qsv/videotoolbox, 1883-1896;
  `validate_hardware_route` evita runs software bajo clave GPU,
  types.rs:2381-2415). Solo videotoolbox tiene fixtures de test.
- Riesgos: TDR con presupuestos estáticos y discrepancia comentario/código
  300 M vs 200 M (gpu_stack.rs:855-872); DX12/Vulkan sin validación física
  (tests #[ignore]); sidecar ffmpeg cae en silencio a PATH si falta
  (core_utils.rs:454); `clean_windows_path` pela `\\?\` (rutas largas).

---

## 7. PLAN PRIORIZADO

Regla de oro heredada: CALIDAD PRIMERO. Nada de recortar la ventana ±16 ni
la caja del SAD por heurística (lección surface_adaptive_refine y limbo
4317de5: el SAD de superficie NO es unimodal). Todo lo marcado P0 es
bit-exacto por construcción.

### P0 — sin riesgo numérico, máxima palanca

| # | Qué | Dónde | Ganancia est. (M5) |
|---|---|---|---|
| A1 | SAD global de superficie a GPU por LOTE: GPU calcula las 1089 sumas SAD del fino ±16 (y el coarse 4225) dentro del submit del batch; la CPU hace argmin con el MISMO orden dy/dx y strict-<, subpíxel y flag saturado intactos → bit-exacto; elimina además la contención de 8 workers sobre SAD_ENGINE | 1601-1655, gpu_analysis.rs:1504/1149 | análisis 386→~90-120 s |
| A2 | Solapar gpu_preprocess(lote N+1) con score(lote N) (doble buffer en el consumidor) | 3236/3255 | −hasta 80 s análisis |
| A3 | No calcular/leer score_rows GPU que se descarta; alimentar 8 workers (≥2 lotes o lote≥pool) | 1646-1654, 2922 | menor, gratis |
| A4 | ram_aware_analysis_threads: usar resolve_planetary_available_memory y bpp real del stream (G16=2) | 1759-1786 | máquinas pequeñas/Windows |
| S1 | Cachear f_edges+f_ds de p1→p2 (RAM primero, spill a caché NVMe) y saltar debayer+enhance en p2 | 8437-8440, 8371 | −~250-400 s pared apilado |
| S2 | Suministro: threads ffmpeg adaptativos (cpus/3→cpus−2 si decode_wait alto), raw_to_u16 paralelo o en workers, canal 2-3 | types.rs:2049-2081, 5178/5207, 7495 | −gran parte de 352 s (frío) |
| S3 | Lote ≥ 2-3× hilos (cabiendo en RAM) + activar par-APs cuando haya workers ociosos (flatten frame×AP o gate nuevo) | 5969-5984, 8586 | núcleos efectivos 2.3→7-9 |
| S4 | Referencia: pipe select exacto para los 20 índices (quitar gate 95% para sets dispersos) y/o pre-calentar rgb48 del top-N al acabar el análisis | 6816-6861, 4833 | −~100 s |
| S5 | debayer RGB: eliminar zero-fill (y de paso el memcpy si el buffer puede moverse) | debayer.rs:68 | −30-60 s |
| S6 | Pre-calentado especulativo en segundo plano del caché rgb48 de los frames seleccionados mientras el usuario revisa el análisis (cancelable, respetando presupuesto de disco) | nuevo, sobre 4746+ | p1 en frío ≈ p1 en caliente |

### P1 — expansión GPU (con gates de paridad física)

- Enhance (blur separable + high-pass) en GPU por lote, entero u16 con el
  mismo redondeo; readback de f_edges barato en memoria unificada; gate
  bit-exacto vs NEON. Elimina también los ~497 s de p1/enhance.
- Si tras S2 el decode sigue limitando en frío: subir filter_threads del
  swscale o conversión P010→RGB48 propia en GPU.
- LK a GPU (PR-32) solo si la telemetría post-P0 aún muestra local_shifts
  dominante.

### P2 — con riesgo controlado (gates fuertes primero)

- p2: cuando un AP satura ±6, en vez de pirámide CPU completa, resolverlo
  con coarse GPU puntual (misma matemática de argmin). Gate: tests del
  limbo + paridad 4317de5.
- Revisar el plan de RAM con RSS medido real (fijo 2.9 GiB y scratch
  595 MiB/hilo parecen sobredimensionados) para llegar a 8-10 hilos en
  24 GB sin swap. Gate: cero swap en corridas E2E.
- a_metric: confiar en el score GPU solo si se prueba paridad bit-exacta
  de ranking (hoy el rescore CPU es deliberado).

### P3 — Windows/plataforma

- Resolver 200 M vs 300 M del submit cap y validar TDR en DX12/Vulkan
  físico; fixtures de confirmación d3d11va/dxva2/qsv; presupuesto VRAM por
  query real donde exista.
- Sidecar ffmpeg: error duro con diagnóstico si falta el bundled.
- Suite planetary_parity.ps1 tras cada bloque P0/P1.

## 8. Proyección (superficie MOV 20 MP, caché caliente, sin swap)

| Fase | Hoy | P0 | P0+P1 |
|---|---|---|---|
| Análisis | 386 s | ~90-120 s | ~65-85 s (decode-bound) |
| Apilado | ~560 s | ~230-280 s | ~150-190 s |
| E2E | ~16 min | ~5.5-6.5 min | **~3.5-4.5 min** |

Planetario: análisis ya óptimo (61 s); apilado 453 → ~200-240 s con S1+S4+S3.
En frío, S2+S6 acercan la primera pasada al régimen caliente. Los claims
frente a AutoStakkert!4 se validan con benchmarks/protocol-as4.md (5 runs,
mediana, cold/warm, CPU/GPU/Hybrid por separado).

## 9. Guardarraíles

- No asumir unimodalidad del SAD jamás; ±16 y caja intactos salvo A/B
  visual aprobado por el usuario.
- Saturación ±6 → pirámide completa se mantiene (fix del limbo) salvo P2
  con sus gates.
- Deep-sky intocable: cambios en gpu_analysis/gpu_stack solo aditivos.
- El usuario ejecuta las pruebas físicas; sesiones paralelas comparten
  rama (add solo ficheros propios).
