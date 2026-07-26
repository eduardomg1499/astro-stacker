# Hybrid v2 implementation status

Date: 2026-07-15

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
  mean/sigma/Winsorized integration and CPU tiled fallback methods. The former
  rank-based "linear-fit" approximation is now blocked in UI, preflight and
  runtime until a real robust frame-to-reference regression is implemented,
  mono/RGB/CFA drizzle, direct scientific float32 TIFF/FITS input, and
  scientific diagnostic maps.
- Storage/failure handling: adaptive RAM/mmap/LZ4 frame store, versioned source
  fingerprints (planetary analysis a10 plus bounded deep-sky content samples),
  planetary analysis envelope `ZACv10` with CRC32 and a 1 GiB decompression
  bound, plus a hashed private-temp fallback for read-only capture media.
  FFmpeg decode-cache algorithm/key v6 (payload `ZDCFv5`) is keyed by source,
  geometry, codec, color/CFA, rotation, explicit decoder backend and FFmpeg
  runtime identity; CRC32, shared budget and attempt-level atomic commit keep
  failed/cancelled hardware routes out of stable entries,
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
  artifact-mask rejection metric, matrix-v3 quality/speed/superiority gates,
  SHA-256 evidence ledger, structured execution provenance, and report v5. The
  validator inspects actual FITS/TIFF geometry, channels and sample depth rather
  than trusting JSON declarations. Missing or malformed evidence makes
  `publishableClaim=false`.
- The executable embeds `dataset-matrix.json` as its authoritative scenario
  matrix and exposes it through `get_benchmark_dataset_matrix`. Validation binds
  every required ID to the expected domain and requires automated evidence for
  each `acceptance`/`requiredEvidence`. Competitive quality is evaluated
  per cold/warm run against every declared comparator, so an 80% aggregate can
  no longer mask one comparison that exceeds the 3% regression guard. Every
  planetary dataset additionally requires at least one objective win, zero
  objective losses and a higher composite score against a hashed independent
  reference; parity alone cannot authorize `publishableClaim`.
- Planetary publication and media safety: final PNG/TIFF/FITS, batch frames,
  animation video/GIF, mosaics and derotation outputs are encoded to sibling
  staging, flushed/synced and published only while their generation token is
  current. AVI native input is limited to structurally verified raw layouts;
  compressed/OpenDML variants route to FFmpeg. Brightness normalization writes
  non-destructive PNG copies while preserving channel layout and 8/16-bit data,
  and mosaics accept only already-stacked PNG/TIFF masters.
- Timed benchmark sessions: `begin_benchmark_run` starts the wall clock and
  clears old telemetry; `finish_benchmark_run` requires explicit job IDs,
  requires the final event for every job to be `complete/100%`, validates
  output/parameters/recipe, and atomically writes a combined telemetry export
  plus a manifest-ready `zenithRun`, file hashes and execution provenance.
  Concurrent runs are rejected and invalid
  evidence cannot silently close a session.

## Local acceptance evidence

- `cargo test --manifest-path src-tauri/Cargo.toml --no-fail-fast`:
  336 passed, 0 failed, 20 environment/physical tests intentionally ignored. This
  includes an end-to-end synthetic manifest/report test for every scenario in the matrix.
- `cargo test --manifest-path src-tauri/Cargo.toml -- --ignored --nocapture`:
  all 19 locally runnable tests passed (15 physical-GPU tests on Apple M5 / Metal,
  three real-FFmpeg exact-index tests and one debayer microbenchmark). The one
  remaining ignored gate, `f0_ab_compare`, deliberately refuses to pass without
  `ZAS_AB_CANDIDATE` and the user's real AutoStakkert!4 comparison artifacts.
- A planetary-only optimized rerun on 2026-07-15 passed all 15/15 applicable
  Metal/FFmpeg/debayer tests. Hybrid analysis measured 26.7 ms/frame at
  4144×2822, 12.0 ms/frame at 4K and 6.4 ms/frame at 1080p on Apple M5; MHC
  debayer reached 3.39× with four threads versus the scalar reference.
- The 2026-07-11 planetary regression gate additionally validates native SER
  batch index/ROI/mono16 preservation, explicit analysis `decode_gpu` versus
  `compute_gpu` telemetry, and the single-conversion preview asset contract;
  evidence and screenshot audit live in
  `benchmarks/ux-audit-2026-07-11-planetary-regressions/`.
- Worst planetary accumulation parity observed: 0.0030 ADU16 RMSE; zero pixels
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

- Instantiate every scenario from `dataset-matrix.json` with real absolute
  paths, exact recipes, current Zenith CPU baselines, cold/warm telemetry, and
  external-engine masters/logs.
- Run the release matrix on Apple M1 or newer and available Windows NVIDIA, AMD,
  and Intel systems. This workstation only proves Apple Metal behavior.
- Generate `zenith-benchmark-report-v5` and require all speed, quality,
  regression, matrix, hash, provenance and scenario-evidence gates to pass.
  The 2×/1.5× medians are against Zenith CPU; every cold/warm run in every
  dataset must independently beat its fastest competitor by the matrix ratio.
  Competitive quality must also satisfy the photometric scale/offset/RMSE and
  total-registration limits, the satellite-mask rejection guard must remain
  within 3%, and every planetary dataset must prove a positive objective
  quality advantage against an independent, content-addressed reference.
- Keep Hybrid v2 experimental and keep `Auto` non-default until that report is
  complete. Do not publish a “beats competitors” claim from the local parity
  results above.
