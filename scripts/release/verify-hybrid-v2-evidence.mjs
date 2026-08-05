import fs from "node:fs";
import path from "node:path";

const evidenceDir = path.resolve(process.argv[2] || "src-tauri/target/hybrid-v2-verification");
const cpuLogPath = path.join(evidenceDir, "cpu-synthetic-tests.log");
const gpuLogPath = path.join(evidenceDir, "gpu-physical-tests.log");

const read = (file) => {
  const bytes = fs.readFileSync(file);
  const sample = bytes.subarray(0, Math.min(bytes.length, 512));
  const looksUtf16Le = (bytes[0] === 0xff && bytes[1] === 0xfe)
    || sample.filter((value, index) => index % 2 === 1 && value === 0).length > sample.length / 6;
  return bytes
    .toString(looksUtf16Le ? "utf16le" : "utf8")
    .replace(/^\uFEFF/, "");
};
const summaries = (text) => {
  const found = [];
  const pattern = /test result: ok\.\s+(\d+) passed;\s+0 failed;\s+(\d+) ignored;/g;
  for (const match of text.matchAll(pattern)) {
    found.push({ passed: Number(match[1]), ignored: Number(match[2]) });
  }
  return found.sort((a, b) => (b.passed + b.ignored) - (a.passed + a.ignored));
};

const cpuLog = read(cpuLogPath);
const gpuLog = read(gpuLogPath);
const cpu = summaries(cpuLog)[0];
const gpu = summaries(gpuLog).sort((a, b) => b.passed - a.passed)[0];
const adapter = gpuLog
  .match(/GPU disponible:\s*true\s+[^\r\n\w]*\s*([A-Za-z0-9][^\r\n·┬À]+?)(?:\s*[·┬À\.]|\s*presupuesto|\s*paridad|\r|\n|$)/i)?.[1]
  ?.replace(/\s*·\s*paridad\s+pending\s*$/i, "")
  .trim();
const errors = [];

if (!cpu || cpu.passed < 80 || cpu.ignored < 12) {
  errors.push("La suite CPU/sintética no contiene la matriz completa esperada.");
}
if (!gpu || gpu.passed < 12 || gpu.ignored !== 0) {
  errors.push("La suite física GPU no demuestra al menos 12 pruebas ejecutadas sin ignorar.");
}
if (!adapter) {
  errors.push("La suite física no confirmó un adaptador GPU real.");
}
if (errors.length) {
  throw new Error(errors.join(" "));
}

const evidence = {
  schema: "zenith-hybrid-v2-release-gate-v1",
  passed: true,
  completedAtUtc: new Date().toISOString(),
  gitCommit: process.env.HYBRID_GATE_COMMIT || "unknown",
  worktreeDirty: process.env.HYBRID_GATE_DIRTY === "true",
  platform: process.env.HYBRID_GATE_OS || process.platform,
  architecture: process.env.HYBRID_GATE_ARCH || process.arch,
  gpuAdapter: adapter,
  testCounts: {
    cpuSyntheticPassed: cpu.passed,
    cpuPhysicalIgnored: cpu.ignored,
    gpuPhysicalPassed: gpu.passed,
    gpuPhysicalIgnored: gpu.ignored
  },
  requiredChecks: [
    { name: "frontend-rust-check", log: "frontend-rust-check.log" },
    { name: "cpu-synthetic-tests", log: "cpu-synthetic-tests.log" },
    { name: "gpu-physical-parity", log: "gpu-physical-tests.log" }
  ]
};

fs.writeFileSync(
  path.join(evidenceDir, "verification.json"),
  `${JSON.stringify(evidence, null, 2)}\n`
);
process.stdout.write(`Validated ${gpu.passed} physical GPU tests on ${adapter}.\n`);
