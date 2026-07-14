# Hybrid v2 implementation status

Date: 2026-07-11

This file separates implementation evidence from competitive acceptance. A
green build or a synthetic parity test must never be presented as proof that
Zenith is faster or better than a named external engine.

## Implemented and locally verified

- Common typed API: `ComputePolicy`, `PipelineProfile`, planetary analysis/
  stack requests, `prepare_deepsky_stack`, `run_deepsky_stack`, float32
  `DeepSkyResult`, and unified `PipelineTelemetry`.
- Multiband deep-sky API: `prepare_deepsky_session` and
  `run_deepsky_session` validate and execute independent filter integrations
  under one recipe. SV220 Ha+OIII and SII+OIII are canonical profiles; each
  produces a float32 master plus line components without mixing raw samples.
- Planetary pipeline: unified SER/AVI/FITS/FFmpeg frame source, exact source
  metadata, bounded decode/preparation overlap, reusable multi-frame GPU
  staging, luma/CFA/pyramid/Laplacian/quality/CoG/SAD kernels, AP accumulation,
  shared caches, CPU parity path, and strict `GpuOnly` behavior. FFmpeg now
  keeps mono/CFA as one plane, distinguishes Bayer from RGB/YUV metadata,
  normalizes display-matrix rotation explicitly with `-noautorotate`, and the
  fallback `FrameSource` decodes from frame zero to preserve absolute indices
  even for VFR/GOP input.
- Deep-sky pipeline: float32 calibration with negatives/headroom, compatible
  calibration-group selection, GPU cosmetic/debayer/star maps, Gaussian PSF
  centroids, RANSAC model selection and second refinement, similarity/affine/
  projective/local-distortion warps, PSF-signal and local normalization, GPU
  mean/sigma/Winsorized/linear-fit integration, CPU tiled fallback methods,
  mono/RGB/CFA drizzle, direct scientific float32 TIFF/FITS input, and
  scientific diagnostic maps.
- Storage/failure handling: adaptive RAM/mmap/LZ4 frame store, versioned source
  fingerprints (planetary analysis a6 plus bounded deep-sky content samples),
  FFmpeg decode-cache v4 keyed by content/geometry/codec/color/rotation,
  tiled/cancellable calibration-master construction, corruption detection,
  disk-full behavior, truncated FFmpeg, insufficient VRAM, absent GPU, parity
  failure, device loss, and cancellation checkpoints through export.
- Export: float32 FITS with metadata, linear/stretched TIFF, PNG preview, JSON
  recipe, telemetry, frame decisions, calibration selection, rejection maps,
  weight, coverage, and registration residuals. Scientific FITS and recipe
  files use synchronized temporary files plus rollback/commit; cancellation or
  an injected disk-full failure cannot publish a truncated final artifact.
- Deep-sky UX: four fixed-shell steps, preflight groups/fallbacks/resources,
  dataset-derived profile recommendation, pre-stack PSF/FWHM/noise/eccentricity
  inspection, basic/advanced recipes, effective CPU/GPU stages, live resources,
  clean cancellation, result maps/table, scientific export, and reintegration.
  Dialog semantics, contextual control labels, circular keyboard focus,
  keyboard reclassification and contrast were verified in the live UI.
- Multiband UX: dual-band detection, coordinated session plan, OIII G/B mix,
  optional cross-talk suppression, SHO/HOO recommendation, post-stack quality
  cards and automatic channel-slot population. Evidence at 1280×720 and
  1000×700 lives in `benchmarks/ux-audit-2026-07-11-multiband/`.
- Benchmark contract: strict absolute-path manifest, hardware identity, exact
  parameters, cold/warm cache evidence, telemetry v2 validation, recipe and
  ABE/SCNR exclusion, output geometry/crop/drizzle contract, clean-reference +
  artifact-mask rejection metric, quality/speed gates, and report v3. The
  validator inspects actual FITS/TIFF geometry, channels and sample depth rather
  than trusting JSON declarations. Missing or malformed evidence makes
  `publishableClaim=false`.
- The executable embeds `dataset-matrix.json` as its authoritative scenario
  matrix and exposes it through `get_benchmark_dataset_matrix`. Validation binds
  every required ID to the expected domain. Competitive quality is evaluated
  per cold/warm run against every declared comparator, so an 80% aggregate can
  no longer mask one comparison that exceeds the 3% regression guard.
- Timed benchmark sessions: `begin_benchmark_run` starts the wall clock and
  clears old telemetry; `finish_benchmark_run` requires explicit job IDs,
  requires the final event for every job to be `complete/100%`, validates
  output/parameters/recipe, and atomically writes a combined telemetry export
  plus a manifest-ready `zenithRun`. Concurrent runs are rejected and invalid
  evidence cannot silently close a session.

## Local acceptance evidence

- `cargo test --manifest-path src-tauri/Cargo.toml --no-fail-fast`:
  103 passed, 0 failed, 12 physical-GPU tests intentionally ignored. This
  includes an end-to-end synthetic ten-scenario manifest/report test.
- `cargo test --manifest-path src-tauri/Cargo.toml -- --ignored --nocapture`:
  12/12 passed on Apple M5 / Metal.
- The 2026-07-11 planetary regression gate additionally validates native SER
  batch index/ROI/mono16 preservation, explicit analysis `decode_gpu` versus
  `compute_gpu` telemetry, and the single-conversion preview asset contract;
  evidence and screenshot audit live in
  `benchmarks/ux-audit-2026-07-11-planetary-regressions/`.
- Worst planetary accumulation parity observed: 0.0029 ADU16 RMSE; zero pixels
  above 1 ADU in the diagnostic case.
- Deep-sky physical tests passed calibration/integration photometric+RMSE gate,
  advanced warp, tiled rejection, cosmetic correction, float32 debayer, and
  GPU star-map/CPU PSF parity.
- A generated H.264 display-matrix fixture on FFmpeg 8.1.1 confirmed that the
  explicit +90° filter is byte-identical to FFmpeg autorotation. A captured
  successful-process/software-fallback log (`VideoToolbox malfunction`) is now
  rejected as hardware evidence instead of being mislabeled GPU. The timed
  probe measures the actual `gray16le`/`rgb48le` conversion, rotation, resize
  and GPU-to-CPU readback path, and selects hardware only with at least a 5%
  wall-clock advantage.
- `scripts/benchmark/verify-ffmpeg-fixtures.mjs` reproduced the complete check
  for both H.264 and HEVC. On this Apple M5, VideoToolbox was confirmed but CPU
  won the short real-output path (about 39/37 ms CPU versus 132/111 ms HW), so
  the correct effective decision was CPU for both fixtures. Rotation hashes
  matched byte-for-byte; evidence is saved beside the release-gate record.
- `npm run check` passed.
- `cargo build --release --manifest-path src-tauri/Cargo.toml` passed.
- Visual QA passed at 1280×720 and 1000×700: fixed header/footer/action,
  center-only scrolling, no horizontal overflow, dialog/label semantics,
  circular visible keyboard focus and a contextual channel-combine return path.
  The permanent evidence and combined audit live in
  `benchmarks/ux-audit-2026-07-10/`. Cielo Profundo was promoted beside Importar
  Video so it is visible without scrolling at 1280×720; file-picker/result
  states still require a real Tauri dataset run.
- CI now runs CPU/synthetic tests on macOS and Windows. macOS and Windows
  release assistants run a mandatory physical-GPU gate, validate that at least
  12 GPU tests truly executed, confirm the adapter and save machine-readable
  evidence plus logs under `src-tauri/target/hybrid-v2-verification`.

## Evidence still required before enabling Auto or publishing superiority

- Instantiate all ten scenarios from `dataset-matrix.json` with real absolute
  paths, exact recipes, current Zenith CPU baselines, cold/warm telemetry, and
  external-engine masters/logs.
- Run the release matrix on Apple M1 or newer and available Windows NVIDIA, AMD,
  and Intel systems. This workstation only proves Apple Metal behavior.
- Generate `zenith-benchmark-report-v3` and require all speed, quality,
  regression, matrix, and evidence gates to pass. In particular, no dataset may
  be slower by more than 10%, median speedups must reach 2×/1.5×, competitive
  quality pass rate must reach 80%, and the satellite-mask rejection guard must
  remain within 3%.
- Keep Hybrid v2 experimental and keep `Auto` non-default until that report is
  complete. Do not publish a “beats competitors” claim from the local parity
  results above.
