import assert from "node:assert/strict";
import test from "node:test";

import {
    deepSkyTelemetryComputeStatus,
    describeDeepSkyComputePlan,
    resolveDeepSkyComputePolicy,
} from "../src/deepsky_compute_policy.js";

test("deep-sky follows global compute policy unless Expert overrides it", () => {
    assert.equal(resolveDeepSkyComputePolicy("global", "gpu_only"), "gpu_only");
    assert.equal(resolveDeepSkyComputePolicy("global", "cpu_only"), "cpu_only");
    assert.equal(resolveDeepSkyComputePolicy("hybrid", "cpu_only"), "hybrid");
    assert.equal(resolveDeepSkyComputePolicy("stale-value", "hybrid"), "hybrid");
    assert.equal(
        resolveDeepSkyComputePolicy("gpu_only", "cpu_only", false),
        "cpu_only",
        "Essential must ignore the stored Expert override",
    );
    assert.equal(
        resolveDeepSkyComputePolicy("gpu_only", "cpu_only", true),
        "gpu_only",
        "returning to Expert must reactivate the preserved override",
    );
});

test("GPU integration disclosure is limited to non-iterative Classic at 1x", () => {
    const eligible = describeDeepSkyComputePlan({
        policy: "auto", gpuAvailable: true, rejection: "average", clipIters: 0,
        drizzle: 1, products: ["classic"],
    });
    assert.equal(eligible.classicIntegrationGpu, true);

    for (const input of [
        { rejection: "sigma", clipIters: 2, drizzle: 1 },
        { rejection: "winsorized", clipIters: 1, drizzle: 1 },
        { rejection: "linearfit", clipIters: 1, drizzle: 1 },
        { rejection: "average", clipIters: 0, drizzle: 2 },
    ]) {
        const plan = describeDeepSkyComputePlan({
            policy: "hybrid", gpuAvailable: true, products: ["classic"], ...input,
        });
        assert.equal(plan.classicIntegrationGpu, false, JSON.stringify(input));
    }
});

test("NebulaFusion, STRUCT and EIDR remain disclosed as scientific CPU products", () => {
    const plan = describeDeepSkyComputePlan({
        policy: "hybrid",
        gpuAvailable: true,
        rejection: "average",
        clipIters: 0,
        drizzle: 1,
        products: ["classic", "nebula_fusion_sci", "struct", "eidr"],
    });
    assert.equal(plan.classicIntegrationGpu, true);
    assert.deepEqual(plan.cpuProducts, ["nebula_fusion_sci", "struct", "eidr"]);
    assert.equal(plan.gpuStageAssist, true);
});

test("telemetry reports observed backend activity without a fictitious GPU percentage", () => {
    assert.deepEqual(
        deepSkyTelemetryComputeStatus({
            engine: "Hybrid CPU+GPU · Apple M5 (Metal)",
            cpu_percent: 42,
            gpu_percent: 91,
            vram_mb: 2048,
        }),
        {
            backend: "Metal",
            gpuActive: true,
            cpuPercent: 42,
            vramMb: 2048,
            reason: "",
        },
    );
    const cpu = deepSkyTelemetryComputeStatus({
        engine: "EIDR CPU forward-model + PCG",
        cpu_percent: 72,
        gpu_percent: 99,
    });
    assert.equal(cpu.gpuActive, false);
    assert.equal(cpu.reason, "EIDR CPU forward-model + PCG");
    assert.equal(deepSkyTelemetryComputeStatus({
        engine: "CPU tiled · wgpu unavailable",
    }).gpuActive, false);
    assert.equal(deepSkyTelemetryComputeStatus({ cpu_percent: null }).cpuPercent, null);
    assert.equal(deepSkyTelemetryComputeStatus({ cpu_percent: undefined }).cpuPercent, null);
    assert.equal(deepSkyTelemetryComputeStatus({ cpu_percent: "" }).cpuPercent, null);
    assert.equal(deepSkyTelemetryComputeStatus({ cpu_percent: 0 }).cpuPercent, 0);
    assert.equal(deepSkyTelemetryComputeStatus({ cpuPercent: 37.5 }).cpuPercent, 37.5);
    assert.equal(deepSkyTelemetryComputeStatus({
        engine: "CPU calibración + GPU cosmética/mapa estelar · Metal + CPU PSF",
        cpu_percent: null,
    }).gpuActive, true);
});
