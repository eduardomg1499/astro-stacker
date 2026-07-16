# Hybrid v2 requirement audit

Audit date: 2026-07-15

This matrix treats source code, generated artifacts and executed tests as
evidence. “Locally verified” does not mean that a competitive performance claim
is authorized. The latter requires the external datasets, masters, logs and
Windows hardware listed at the end.

## Common architecture and benchmark evidence

| Requirement | Authoritative local evidence | Status |
|---|---|---|
| Preserve the existing Rust/Rayon/wgpu codebase | The planetary work evolves the existing Rust/Rayon/wgpu engine and UI without modifying `deepsky*.rs`, `gpu_deepsky.rs` or `spcc.rs`; no reset/restart was performed | Locally verified |
| Typed CPU/GPU policy and profiles | `pipeline.rs`: `ComputePolicy`, `PipelineProfile`, capability resolution and fallback tests | Locally verified |
| Common frame source for SER/AVI/FITS/FFmpeg | `frame_source.rs`: `FrameSource`, `FrameBatch`, exact out-of-order/duplicate index tests, ROI and Bayer-vs-RGB descriptor metadata | Locally verified |
| Adaptive RAM → mmap → LZ4 store | `frame_store.rs`; ranged tiled reads, roundtrip, reuse, corruption and injected disk-full tests; calibration masters use the same adaptive store | Locally verified |
| Cache versioned by algorithm, geometry and source | Planetary analysis schema/fingerprint/envelope a10 (`ZACv10`) binds source content, CFA, ROI, anchor and target with CRC32/1 GiB bound and private-temp fallback. FFmpeg decode algorithm/key v6 (payload `ZDCFv5`) adds codec, route/backend, FFmpeg identity, CFA/color and rotation; a complete attempt must validate before commit | Locally verified, including overwrite invalidation, corrupt-entry repair, route isolation and rollback |
| Reproducible cold/warm suite and per-phase resources | `benchmark.rs`, embedded/exposed matrix v3, timed benchmark sessions and telemetry v2 phase summaries | Locally verified; all twenty scenarios also exercised end to end with deterministic synthetic artifacts |
| Scenario matrix cannot drift from the executable | Manifest IDs/domains, quality thresholds, own-CPU targets, per-dataset competitor ratio and per-requirement evidence are bound to the embedded matrix | Locally verified |
| Reject incompatible or falsely declared outputs | Manifest validation checks absolute files, actual dimensions/channels and FITS BITPIX/TIFF depth against `processing` | Locally verified |
| Publishability gated by complete evidence | Report v5 recomputes SHA-256, binds configurations/provenance and requires automated artifact-backed evidence per requirement. Every run must beat its fastest rival; every planetary dataset must additionally show ≥1 objective win, zero losses and a higher composite score against an independent reference | Locally verified contract; real A/B evidence pending |

## Planetary engine

| Requirement | Authoritative local evidence | Status |
|---|---|---|
| GPU luma/CFA, pyramid, Laplacian, quality, CoG and SAD/AP work | `gpu_analysis.rs` and the batched analysis coordinator in `commands_v2_v3.rs` | Locally verified; physical Metal parity passed |
| Overlap decode/preparation N+1 with GPU N | Bounded FFmpeg/native producer-consumer pipelines and reusable GPU batch slots | Locally verified |
| Probe CPU vs hardware FFmpeg decode and confirm backend | The bounded/cancellable probe measures gray16/RGB48 conversion, rotation, resize and readback, requires ≥5% advantage, then the real stream forces that explicit backend and validates its log before cache commit; fallback restarts the full pass on CPU | Locally verified for synthetic H.264+HEVC on FFmpeg 8.1.1/VideoToolbox; Windows codec matrix still external |
| Preserve FFmpeg rotation, CFA/mono layout and exact indices | Explicit display-matrix filter with `-noautorotate`, single-plane mono/CFA contract, and sequential absolute-index fallback tests | Locally verified; +90° fixture matched FFmpeg autorotation byte-for-byte |
| GPU accumulation with pass-level fallback and final readback | `gpu_stack.rs` and full-pass CPU restart on device loss/OOM | Locally verified; physical Metal parity passed |
| Equivalent CPU route and strict GPU-only | Analysis/stack policy gates; GPU-only checks device, VRAM and parity before work or cache use | Locally verified |
| Legacy commands wrap the unified engine | `analyze_video_v2`, `stack_video`, and retired `zas_stack_video_elite` wrapper; no registered command reaches the former all-in-RAM prototype | Locally verified |
| No black/dummy warp in retained experimental code | Historical warp now has identity/affine/dense Lanczos-3 implementation and regression test | Locally verified |

## Deep-sky engine

| Requirement | Authoritative local evidence | Status |
|---|---|---|
| Float32 linear lights/masters with negatives and headroom | `DeepSkyResult`, float calibration/integration, direct TIFF float32 decoder and FITS/TIFF negative/headroom tests | Locally verified |
| Calibration grouping without incompatible session mixing | Probe/group selection by geometry, Bayer, filter, exposure, gain, binning and temperature; selection test | Locally verified |
| Robust, bounded and cancellable calibration masters | Bias/dark/flat median uses tiled `AdaptiveFrameStore` instead of retaining the full calibration stack in RAM; robustness/cancel test | Locally verified |
| GPU calibration, cosmetic, debayer/star map, warp and integration | `gpu_deepsky.rs`; session gates cover pixels, similarity, projective/local distortion and tiled rejection before preflight accepts GPU | Locally verified; physical Metal parity passed |
| PSF centroids, RANSAC and automatic model selection | Gaussian PSF fit plus similarity/affine/projective/local-distortion tests | Locally verified |
| Robust additive/multiplicative/local normalization | PSF signal scaling, noise fallback and local 24×24 background model stored per frame in recipe | Locally verified |
| Mean/sigma/Winsorized/linear-fit GPU; remaining tiled CPU methods | Engine selection and parity gates in `deepsky.rs`/`gpu_deepsky.rs` | Locally verified |
| True mono/RGB/CFA drizzle and dithering warning | Drop-kernel/CFA accumulation tests; runtime counts subpixel positions for every drizzle type | Locally verified |
| Requested vs effective rejection visible before execution | `PreparedStackPlan.requestedRejection/effectiveRejection`, UI method display and fallback telemetry | Locally verified |
| Master plus rejection/weight/coverage/residual maps | `DeepSkyResult`, result views and float32 diagnostic FITS export | Locally verified |
| FITS float32, TIFF/PNG and reproducible JSON recipe | `deepsky_export_float32`, `deepsky_export` and FITS metadata tests | Locally verified |
| Cancellation/disk-full cannot publish partial scientific artifacts | FITS block checkpoints, synchronized temporary files, rollback commit and injected mid-write failure test; recipe uses the same atomic commit pattern | Locally verified |
| ABE/SCNR excluded from the scientific benchmark | Optional flag defaults off; recipe and manifest validator reject enabled finishing | Locally verified |

## Deep-sky experience

| Requirement | Authoritative local evidence | Status |
|---|---|---|
| Four fixed-shell steps | `index.html`/`main.js`: Data, inspection, recipe, review/run | Locally verified at 1280×720 and 1000×700 |
| Discoverable deep-sky entry | Deep Sky is adjacent to Import Video and visible in the initial 1280×720 viewport | Locally verified |
| Basic profiles and advanced sections | Fast/Balanced/Maximum Quality/Custom controls; preset no longer self-switches to Custom | Locally verified |
| Groups, incompatibilities, reference, rejectable frames, real method and budgets before run | Typed preflight plus PSF/FWHM/noise/eccentricity inspection and review panels | Locally verified |
| Live phase, ETA, throughput, engine, resources, fallback and cancellation | `pipeline_telemetry` listener and deep-sky progress overlay | Locally verified |
| Master/maps/frame table and integration-only repeat | Result selector, frame report and reintegration using frame/analysis/registration caches | Locally verified |
| Keyboard, focus, labels and contrast | Native dialog names, contextual folder/remove/reclassify labels, circular focus trap, visible 2 px outline, improved small-text contrast and current viewport captures | Locally verified; full screen-reader session remains external |

## Interfaces, tests and rollout

| Requirement | Authoritative local evidence | Status |
|---|---|---|
| `prepare_deepsky_stack` and `run_deepsky_stack` | Registered Tauri commands and typed result handle | Locally verified |
| `prepare_deepsky_session` and `run_deepsky_session` | Typed coordinated multiband plan/result, float32 masters and Ha/SII/OIII component exports | Locally verified; real-filter spectral validation pending |
| Separate float32 deep-sky and uint16 planetary result types | `pipeline.rs` and `AppState` | Locally verified |
| Unified telemetry and terminal job evidence | `PipelineTelemetry`; benchmark now requires the last event of every job to be `complete/100%` | Locally verified |
| CPU/GPU parity, benchmark-contract and failure tests | 336 normal tests; 19/19 locally runnable ignored tests passed, including 15 physical-GPU tests; the planetary-only optimized rerun passed 15/15 applicable tests; the real external A/B gate remains blocked without its dataset environment | Locally verified on Apple Metal; Windows matrix and real competitor dataset still missing |
| CPU tests in CI and mandatory GPU release gate | `.github/workflows/ci.yml`; macOS/Windows release assistants call `verify-hybrid-v2-gate`, which validates executed counts and adapter evidence | Locally verified on Apple Metal; Windows execution pending |
| Hybrid v2 experimental, Auto not default | Rust default and both UI selectors use Hybrid v2 experimental | Locally verified |
| FITS/TIFF input and FITS/TIFF/PNG output scope | Deep-sky picker/backend/export paths; no XISF/DSLR RAW claim | Locally verified |

## Evidence that is still external

The goal cannot be declared competitively accepted until all twenty real scenarios
in `dataset-matrix.json` have cold and warm Zenith runs, current Zenith CPU
baselines, rival masters/logs and matching output contracts. The release matrix
must also run on Apple M1-or-newer and available Windows NVIDIA, AMD and Intel
systems. Only a valid `zenith-benchmark-report-v5` may prove the 2×/1.5×
own-CPU targets, the 80% quality target, the photometric/registration limits
and a positive independent-reference quality advantage for every planetary
dataset in addition to the fastest-competitor speed gate.
