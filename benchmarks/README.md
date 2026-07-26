# Zenith benchmark suite

The benchmark contract is intentionally based on absolute paths so a run is
reproducible and never depends on the current working directory.

1. Copy `manifest.example.json` outside the repository, replace every path and
   add the remaining scenario IDs from `dataset-matrix.json`; the small example
   is illustrative and intentionally cannot authorize a competitive claim.
2. Prepare the requested cold or warm cache state, then call
   `begin_benchmark_run`. It clears prior pipeline telemetry and starts the
   end-to-end wall clock immediately before the application opens its input.
   Pass an explicit `cachePreparation` description; the command records what
   was done but never pretends it can purge the OS cache itself.
3. In Zenith call `get_benchmark_dataset_matrix` to obtain the exact matrix
   embedded in the executable, then call `validate_benchmark_manifest` before
   running comparisons. Matrix v3 binds every scenario ID, domain, claim
   threshold and evidence requirement; a copied manifest cannot silently drift
   from `dataset-matrix.json`.
4. Call `compare_linear_masters` only after output geometry, channels, crop and
   drizzle scale are identical. An incompatible comparison is rejected.
5. Record the exact engine identity, vendor, version, distribution, executable,
   invocation, timing boundary, non-empty parameter object, output contract and
   logs. Add every file to `evidence.artifacts` with absolute path, byte length
   and verified SHA-256. A product name in `comparator.engine` is not
   provenance.
6. After the linear master and recipe have been written, call
   `finish_benchmark_run` with the exact analysis/stack `jobIds`, output
   contract and non-empty parameters. The command stops the clock at entry,
   requires the final event of every requested job to be `complete/100%`,
   validates terminal phases and writes `telemetry.json` plus
   `zenith-run.json`; `zenithRun` in that record is ready to insert into the
   manifest. Planetary runs normally pass two job IDs, while deep sky passes
   its result/job ID and the exported recipe. Invalid evidence leaves the
   session active for inspection. Run artifact v2 also returns the hashed
   output, telemetry, recipe and executable plus canonical configuration hash,
   ready to merge into `manifest.evidence`. Use `abort_benchmark_run` to
   discard it.
7. Call `generate_benchmark_report` to calculate own-CPU speedups, the
   per-dataset competitor gates, quality limits and whether a competitive claim
   is publishable.
8. Instantiate every scenario in `dataset-matrix.json`. The matrix covers SER
   mono/Bayer, H.264/HEVC, surface/small planet, OSC calibration+CFA drizzle,
   mono multisession, gradients, distortion, satellite rejection and the
   high-resolution memory/failure path.

Validation is intentionally strict: every scenario needs cold and warm Zenith
runs, telemetry v2 with terminal jobs and all required phases, the current
Zenith CPU baseline and at least one comparable rival. Every string listed in
that scenario's `acceptance` and `requiredEvidence` needs exactly one
`evidence.requirements` record with `passed=true`, `evaluator=automated`, a
reproducible method and one or more content-addressed artifacts. A partial,
manual, failed, unhashed or tampered record sets `publishableClaim=false` and
is included in validation errors. A path merely present in JSON is never
considered evidence.

Every suite must include cold-cache and warm-cache timings plus a concrete
`cachePreparation` description. Publish claims only for the hardware, engine
versions, parameters and datasets present in the saved report.

Only one timed session can be active. `jobIds` are mandatory so unrelated
background telemetry cannot leak into a run. The legacy
`clear_pipeline_telemetry`/`export_pipeline_telemetry` commands remain
available for diagnostics, but a benchmark session avoids manually copying its
elapsed time or telemetry paths.

## Evidence ledger and provenance

`manifest.evidence` has three linked collections:

- `artifacts`: source recordings, outputs, logs, telemetry, recipes, test
  reports and executables. The validator re-reads each file and verifies both
  SHA-256 and `sizeBytes`; changing one byte invalidates the suite.
- `executions`: one `baseline-cpu`, `zenith` or `competitor` record per declared
  run. It references executable/output/log artifacts and binds the exact
  `parameters` object through SHA-256 of canonical JSON (sorted keys, UTF-8,
  no whitespace). `runKey` is `baseline-cpu`,
  `zenith:<mode>:cold|warm`, or
  `competitor:<normalized-engine>:<exact-version>`.
- `requirements`: one automated result for every matrix acceptance and
  required-evidence ID. Artifact IDs make the result auditable instead of
  self-certified text.

Planetary datasets require structured AutoStakkert!4 provenance: product,
vendor, exact version, distribution identity, hashed executable, hashed
configuration, hashed output/log, invocation and timing method. Merely naming
AutoStakkert in `engine` cannot satisfy the gate. `manifest.example.json` is
deliberately incomplete and uses replacement markers; copy the shape, replace
every marker, and instantiate all matrix scenarios.

`zenithRuns` distinguishes cold and warm cache explicitly. `baselineCpuSeconds`
is the same dataset on the previous Zenith CPU engine; it is required for the
own-CPU targets and regression guard. Matrix v3 defines 2× overall and
compressed-video targets plus 1.5× for SER, all against Zenith's own CPU
reference. The global competitor median is informational only: every cold/warm
run of every dataset must beat the fastest declared competitor by at least the
matrix ratio (currently 1.05×), so a fast dataset cannot hide a slow one.
Baseline, Zenith and comparator outputs must
declare the same geometry, crop, channels and drizzle scale. Deep-sky contracts
require float32; planetary contracts require uint16. Validation also opens the
actual FITS/TIFF files and rejects a declaration whose dimensions, channel
count or sample depth do not match the saved master.

For deep sky, each Zenith run must point to its exported recipe. The validator
checks `linearFloat32`, source fingerprint, geometry, exact parameters and that
`optionalAbeScnr=false`. ABE, SCNR, sharpening and stretching belong to optional
finishing and invalidate a linear-master benchmark.

`zenith-benchmark-report-v5` embeds per-run phase evidence: observed duration,
effective engines, throughput, CPU/GPU counters when available, peak RAM/VRAM,
I/O, cache hits/misses and fallback reasons. Portable `wgpu` does not expose a
reliable utilization percentage on every backend; in that case GPU use is
proven by the effective engine, backend and allocated VRAM rather than a made-up
percentage.

Quality comparison reports registered correlation, FWHM, background noise,
photometric flux, robust-range-normalized RMSE, fitted scale error, normalized
offset, residual registration and total registration correction. Matrix v3
caps these values (3% flux/scale/offset/RMSE, 0.5 px residual and 1.0 px total
correction with the current policy). Each cold/warm Zenith run is one
quality case and must pass against every comparator declared for that run. The
80% aggregate target therefore cannot hide a single competitive regression
above 3%; CPU/GPU kernel parity remains the stricter RMSE ≤1 ADU16
(planetary) / ≤0.5 ADU (deep sky) test gate.

Parity and a 1.05× speed ratio are not enough to claim better quality. Each
planetary scenario also has a binding `quality-superiority` acceptance record.
Its automated evaluator must link hashed outputs and an independent reference,
report at least one objective win, zero objective losses, and a Zenith composite
score strictly above the comparator. Missing those metrics keeps
`publishableClaim=false` even when every non-regression gate is green.

## Release verification gate

Normal CI runs the CPU reference and synthetic/failure suite on macOS and
Windows. A release additionally runs `scripts/release/verify-hybrid-v2-gate.sh`
or its PowerShell equivalent. The gate executes all ignored physical-GPU tests,
requires at least 12 of them to have actually run (zero ignored tests cannot
silently pass), confirms a real adapter, and writes logs plus
`src-tauri/target/hybrid-v2-verification/verification.json`.

Both macOS and Windows release assistants invoke this gate before packaging.
The verification record includes the commit, dirty-worktree flag, platform,
architecture, adapter and test counts. It proves kernel parity only; it does
not replace the dataset benchmark report.

The normal suite includes
`complete_synthetic_matrix_exercises_every_report_gate`: it generates small
deterministic float32 FITS and uint16 TIFF masters, terminal telemetry, a deep
recipe and a satellite mask, instantiates all required scenarios, serializes
the manifest and verifies report v5 end to end, including real SHA-256
recomputation, structured execution provenance and every scenario requirement.
This proves that CI exercises
the report machinery; synthetic data still cannot substantiate a claim against
PixInsight, DSS, Siril or another external engine.

The `deep-sky-satellite-trails` scenario additionally requires
`artifactFreeReference` (a registered, linear master without the trail) and
`artifactMask` (a FITS mask in the same geometry). Report v3 fits scale/offset
outside the mask, measures residual RMSE inside it and rejects a rejection-
quality regression above 3% against either Zenith CPU or a comparator. This is
used instead of comparing engine rejection maps whose units are not portable.

## Synthetic FFmpeg codec fixtures

Run `node scripts/benchmark/verify-ffmpeg-fixtures.mjs` to generate short H.264
and HEVC sources with a +90° display matrix. The verifier exercises the same
gray16 conversion/rotation/resize/readback shape as Zenith, checks explicit
rotation byte-for-byte against FFmpeg autorotation, rejects failed hardware
initialization logs and records whether hardware is at least 5% faster than
CPU. It writes
`src-tauri/target/hybrid-v2-verification/ffmpeg-fixtures.json`.

The script requires `ffmpeg`, `ffprobe`, `libx264` and `libx265`. It is a
reproducible codec-contract check, not a substitute for the real H.264/HEVC
surface and small-planet benchmark scenarios.
