#!/usr/bin/env node

import { createHash } from "node:crypto";
import {
  mkdtempSync,
  mkdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { spawnSync } from "node:child_process";

const ffmpeg = process.env.FFMPEG || "ffmpeg";
const ffprobe = process.env.FFPROBE || "ffprobe";
const outputPath = resolve(
  process.argv[2] ||
    "src-tauri/target/hybrid-v2-verification/ffmpeg-fixtures.json",
);
const temp = mkdtempSync(`${tmpdir()}/zenith-ffmpeg-fixtures-`);

function run(binary, args, { captureStdout = false } = {}) {
  const started = process.hrtime.bigint();
  const result = spawnSync(binary, args, {
    encoding: null,
    maxBuffer: 32 * 1024 * 1024,
    stdio: ["ignore", captureStdout ? "pipe" : "ignore", "pipe"],
  });
  return {
    ok: result.status === 0 && !result.error,
    status: result.status,
    elapsedMs: Number(process.hrtime.bigint() - started) / 1e6,
    stdout: result.stdout || Buffer.alloc(0),
    stderr: (result.stderr || Buffer.alloc(0)).toString("utf8"),
    error: result.error?.message || null,
  };
}

function requireCommand(binary) {
  const result = run(binary, ["-version"], { captureStdout: true });
  if (!result.ok) {
    throw new Error(`No se pudo ejecutar ${binary}: ${result.error || result.stderr}`);
  }
  return result.stdout.toString("utf8").split(/\r?\n/, 1)[0];
}

const failureMarkers = [
  "hwaccel initialisation returned error",
  "hwaccel initialization returned error",
  "failed setup for format",
  "hardware accelerator failed",
  "device creation failed",
  "no device available",
  "videotoolbox malfunction",
  "could not dynamically load",
  "cannot load libcuda",
  "failed to initialise",
  "failed to initialize",
  "falling back to software",
  "fallback to software",
  "using software decoding",
];
const backends = [
  ["VideoToolbox", "videotoolbox"],
  ["D3D11VA", "d3d11va"],
  ["DXVA2", "dxva2"],
  ["NVDEC/CUDA", "cuda"],
  ["NVDEC", "nvdec"],
  ["Intel QSV", "qsv"],
  ["VAAPI", "vaapi"],
  ["Vulkan Video", "vulkan"],
];

function confirmedBackend(log, processOk) {
  if (!processOk) return null;
  const lower = log.toLowerCase();
  if (failureMarkers.some((marker) => lower.includes(marker))) return null;
  return backends.find(([, marker]) => lower.includes(marker))?.[0] || null;
}

function sha256(buffer) {
  return createHash("sha256").update(buffer).digest("hex");
}

function codecFixture({ id, encoder, extension, extraEncode = [] }) {
  const base = `${temp}/${id}-base.${extension}`;
  const rotated = `${temp}/${id}-rotated.${extension}`;
  const generated = run(ffmpeg, [
    "-hide_banner",
    "-loglevel",
    "error",
    "-f",
    "lavfi",
    "-i",
    "testsrc2=size=128x96:rate=24",
    "-frames:v",
    "48",
    "-c:v",
    encoder,
    ...extraEncode,
    "-pix_fmt",
    "yuv420p",
    base,
  ]);
  if (!generated.ok) {
    return {
      id,
      passed: false,
      error: `No se pudo generar ${id} con ${encoder}: ${generated.stderr}`,
    };
  }
  const tagged = run(ffmpeg, [
    "-hide_banner",
    "-loglevel",
    "error",
    "-display_rotation",
    "90",
    "-i",
    base,
    "-c",
    "copy",
    rotated,
  ]);
  if (!tagged.ok) {
    return { id, passed: false, error: `No se pudo escribir display-matrix: ${tagged.stderr}` };
  }

  const probe = run(
    ffprobe,
    [
      "-v",
      "error",
      "-select_streams",
      "v:0",
      "-show_entries",
      "stream=codec_name,width,height:stream_side_data=rotation",
      "-of",
      "json",
      rotated,
    ],
    { captureStdout: true },
  );
  const metadata = probe.ok ? JSON.parse(probe.stdout.toString("utf8")) : null;
  const rotation = metadata?.streams?.[0]?.side_data_list?.[0]?.rotation;

  const common = [
    "-hide_banner",
    "-nostdin",
    "-loglevel",
    "verbose",
    "-benchmark",
    "-noautorotate",
    "-threads",
    "4",
    "-i",
    rotated,
    "-map",
    "0:v:0",
    "-frames:v",
    "24",
    "-an",
    "-sn",
    "-fps_mode",
    "passthrough",
    "-vf",
    "transpose=2,scale=96:128:flags=neighbor,format=gray16le",
    "-pix_fmt",
    "gray16le",
    "-f",
    "rawvideo",
    "pipe:1",
  ];
  const cpu = run(ffmpeg, common);
  const hardware = run(ffmpeg, [
    ...common.slice(0, 6),
    "-hwaccel",
    "auto",
    ...common.slice(6),
  ]);
  const backend = confirmedBackend(hardware.stderr, hardware.ok);

  const autoRotation = run(
    ffmpeg,
    [
      "-hide_banner",
      "-loglevel",
      "error",
      "-i",
      rotated,
      "-frames:v",
      "1",
      "-vf",
      "scale=96:128:flags=neighbor,format=gray",
      "-pix_fmt",
      "gray",
      "-f",
      "rawvideo",
      "pipe:1",
    ],
    { captureStdout: true },
  );
  const explicitRotation = run(
    ffmpeg,
    [
      "-hide_banner",
      "-loglevel",
      "error",
      "-noautorotate",
      "-i",
      rotated,
      "-frames:v",
      "1",
      "-vf",
      "transpose=2,scale=96:128:flags=neighbor,format=gray",
      "-pix_fmt",
      "gray",
      "-f",
      "rawvideo",
      "pipe:1",
    ],
    { captureStdout: true },
  );
  const autoHash = sha256(autoRotation.stdout);
  const explicitHash = sha256(explicitRotation.stdout);
  const rotationMatches =
    autoRotation.ok && explicitRotation.ok && autoHash === explicitHash;
  return {
    id,
    encoder,
    codec: metadata?.streams?.[0]?.codec_name || null,
    storedGeometry: metadata?.streams?.[0]
      ? `${metadata.streams[0].width}x${metadata.streams[0].height}`
      : null,
    displayRotationDegrees: rotation ?? null,
    cpuPipelineMs: Number(cpu.elapsedMs.toFixed(3)),
    hardwarePipelineMs: Number(hardware.elapsedMs.toFixed(3)),
    hardwareBackendConfirmed: backend,
    preferHardware:
      Boolean(backend) && hardware.elapsedMs <= cpu.elapsedMs * 0.95,
    rotationAutoSha256: autoHash,
    rotationExplicitSha256: explicitHash,
    rotationMatches,
    passed:
      probe.ok &&
      rotation === 90 &&
      cpu.ok &&
      hardware.ok &&
      rotationMatches,
  };
}

let report;
let exitCode = 1;
try {
  const ffmpegVersion = requireCommand(ffmpeg);
  const ffprobeVersion = requireCommand(ffprobe);
  const encoders = run(ffmpeg, ["-hide_banner", "-encoders"], {
    captureStdout: true,
  }).stdout.toString("utf8");
  const required = ["libx264", "libx265"];
  const missing = required.filter(
    (encoder) => !new RegExp(`\\b${encoder}\\b`).test(encoders),
  );
  if (missing.length) {
    throw new Error(`FFmpeg no incluye los encoders requeridos: ${missing.join(", ")}`);
  }
  const fixtures = [
    codecFixture({
      id: "h264",
      encoder: "libx264",
      extension: "mp4",
      extraEncode: ["-preset", "ultrafast"],
    }),
    codecFixture({
      id: "hevc",
      encoder: "libx265",
      extension: "mp4",
      extraEncode: ["-preset", "ultrafast", "-x265-params", "log-level=error"],
    }),
  ];
  const passed = fixtures.every((fixture) => fixture.passed);
  report = {
    schema: "zenith-ffmpeg-fixtures-v1",
    passed,
    generatedAtUtc: new Date().toISOString(),
    ffmpegVersion,
    ffprobeVersion,
    fixtures,
  };
  exitCode = passed ? 0 : 1;
} catch (error) {
  report = {
    schema: "zenith-ffmpeg-fixtures-v1",
    passed: false,
    generatedAtUtc: new Date().toISOString(),
    error: error instanceof Error ? error.message : String(error),
  };
}

mkdirSync(dirname(outputPath), { recursive: true });
writeFileSync(outputPath, `${JSON.stringify(report, null, 2)}\n`);
process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
rmSync(temp, { recursive: true, force: true });
process.exit(exitCode);
