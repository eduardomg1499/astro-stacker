const VALID_POLICIES = new Set(["auto", "hybrid", "gpu_only", "cpu_only"]);

function normalizePolicy(value, fallback = "auto") {
    const aliases = {
        gpu: "gpu_only",
        cpu: "cpu_only",
        global: fallback,
    };
    const normalized = aliases[String(value || "").toLowerCase()]
        || String(value || "").toLowerCase();
    return VALID_POLICIES.has(normalized) ? normalized : fallback;
}

/**
 * Deep-sky follows the application compute policy unless Expert explicitly
 * selects an override. Unknown/stale values fail closed to the global policy.
 */
export function resolveDeepSkyComputePolicy(
    selection,
    globalPolicy = "auto",
    expertOverrideEnabled = true,
) {
    const global = normalizePolicy(globalPolicy, "auto");
    return !expertOverrideEnabled
        || selection === "global"
        || !VALID_POLICIES.has(String(selection || ""))
        ? global
        : normalizePolicy(selection, global);
}

function productKind(product) {
    if (typeof product === "string") return product;
    return String(product?.product || product?.id || "");
}

/**
 * Conservative disclosure used only by the UI estimate. It deliberately does
 * not claim GPU support for algorithms whose scientific parity is CPU-only.
 */
export function describeDeepSkyComputePlan({
    policy = "auto",
    gpuAvailable = false,
    rejection = "sigma",
    clipIters = 1,
    drizzle = 1,
    products = ["classic"],
    cosmetic = true,
} = {}) {
    const effectivePolicy = normalizePolicy(policy, "auto");
    const gpuAllowed = gpuAvailable && effectivePolicy !== "cpu_only";
    const kinds = new Set(products.map(productKind));
    const iterations = Math.max(0, Number(clipIters) || 0);
    const scale = Math.max(1, Number(drizzle) || 1);
    const streamingWithoutIterations = rejection === "average"
        || (rejection === "sigma" && iterations === 0);
    const classicIntegrationGpu = gpuAllowed
        && kinds.has("classic")
        && streamingWithoutIterations
        && scale === 1;
    const cpuProducts = [...kinds].filter(kind => [
        "nebula_fusion_sci",
        "nebulaFusionSci",
        "struct",
        "eidr",
    ].includes(kind));

    let classicCpuReason = "";
    if (kinds.has("classic") && !classicIntegrationGpu) {
        if (!gpuAvailable) classicCpuReason = "gpu_unavailable";
        else if (effectivePolicy === "cpu_only") classicCpuReason = "cpu_policy";
        else if (scale > 1) classicCpuReason = "drizzle_cpu";
        else if (rejection === "sigma" && iterations > 0) classicCpuReason = "sigma_iterative_cpu";
        else if (["winsorized", "linearfit", "median", "percentile", "minmax", "tiled"].includes(rejection)) {
            classicCpuReason = "tiled_rejection_cpu";
        } else classicCpuReason = "scientific_cpu";
    }

    return {
        policy: effectivePolicy,
        gpuAllowed,
        gpuStageAssist: gpuAllowed,
        gpuCosmeticAssist: gpuAllowed && cosmetic,
        classicIntegrationGpu,
        classicCpuReason,
        cpuProducts,
    };
}

function backendFromEngine(engine) {
    const value = String(engine || "");
    if (/\bmetal\b/i.test(value)) return "Metal";
    if (/\b(?:dx12|directx\s*12)\b/i.test(value)) return "DX12";
    if (/\bvulkan\b/i.test(value)) return "Vulkan";
    if (/\bwgpu\b/i.test(value)) return "wgpu";
    return "";
}

/**
 * Telemetry must describe observed execution, never manufacture a GPU load.
 * A known backend marks the accelerator active unless the event explicitly
 * records an effective CPU fallback.
 */
export function deepSkyTelemetryComputeStatus(telemetry = {}) {
    const engine = String(telemetry.engine || "");
    const fallback = String(telemetry.fallback_reason || telemetry.fallbackReason || "");
    const backend = backendFromEngine(engine);
    const effectiveCpuFallback = /fallback[^\n]*cpu|cpu[^\n]*fallback|cpu[- ]?only|(?:gpu|wgpu)[^\n]*(?:unavailable|disabled|inactive)|(?:unavailable|disabled|inactive)[^\n]*(?:gpu|wgpu)/i.test(
        `${engine} ${fallback}`,
    );
    const gpuActive = Boolean(backend) && !effectiveCpuFallback;
    const rawCpuPercent = telemetry.cpu_percent ?? telemetry.cpuPercent;
    const cpuPercent = rawCpuPercent === null
        || rawCpuPercent === undefined
        || rawCpuPercent === ""
        ? null
        : Number(rawCpuPercent);
    return {
        backend,
        gpuActive,
        cpuPercent: Number.isFinite(cpuPercent)
            ? Math.max(0, Math.min(100, cpuPercent))
            : null,
        vramMb: Number.isFinite(Number(telemetry.vram_mb)) && Number(telemetry.vram_mb) > 0
            ? Math.round(Number(telemetry.vram_mb))
            : null,
        reason: gpuActive ? "" : (fallback || engine || "cpu_stage"),
    };
}
