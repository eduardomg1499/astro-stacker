import test from "node:test";
import assert from "node:assert/strict";

import {
    BATCH_OUTPUT_POLICY_SINGLE_DIRECTORY,
    BATCH_OUTPUT_POLICY_SOURCE_ADJACENT,
    buildBatchOutputLookup,
    formatBatchOutputError,
    freezeBatchProcessingContract,
    normalizeBatchEntryResult,
    normalizeBatchOutputSettings
} from "../src/batch_output.js";

test("sourceAdjacent is the safe default when no reusable custom directory exists", () => {
    assert.deepEqual(normalizeBatchOutputSettings("unknown", ""), {
        policy: BATCH_OUTPUT_POLICY_SOURCE_ADJACENT,
        directory: ""
    });
    assert.deepEqual(normalizeBatchOutputSettings(BATCH_OUTPUT_POLICY_SINGLE_DIRECTORY, "  /Volumes/NVMe  "), {
        policy: BATCH_OUTPUT_POLICY_SINGLE_DIRECTORY,
        directory: "/Volumes/NVMe"
    });
});

test("the frozen backend plan resolves every source independently", () => {
    const files = ["/capture/a.ser", "/capture/night-2/a.ser"];
    const lookup = buildBatchOutputLookup({
        schemaVersion: 1,
        animationFolder: "/capture/Zenith_Batch_session",
        entries: [
            { sourcePath: files[0], outputFolder: "/capture/Zenith_Batch_session/a.ser" },
            { sourcePath: files[1], outputFolder: "/capture/night-2/Zenith_Batch_session/a.ser" }
        ]
    }, files);
    assert.equal(lookup.get(files[0]), "/capture/Zenith_Batch_session/a.ser");
    assert.equal(lookup.get(files[1]), "/capture/night-2/Zenith_Batch_session/a.ser");
});

test("an incomplete backend plan is rejected before processing", () => {
    assert.throws(() => buildBatchOutputLookup({
        schemaVersion: 1,
        animationFolder: "/capture/out",
        entries: []
    }, ["/capture/a.ser"]), /no contiene la fuente/);
});

test("typed backend errors preserve the useful path and message", () => {
    assert.equal(
        formatBatchOutputError({
            code: "permission_denied",
            path: "/Volumes/NVMe/session",
            message: "No se pudo comprobar la escritura"
        }),
        "No se pudo comprobar la escritura\n/Volumes/NVMe/session"
    );
    assert.equal(formatBatchOutputError("permiso denegado"), "permiso denegado");
});

test("the reference processing recipe is deeply copied and frozen", () => {
    const live = {
        wavelets: [1, 2, 3],
        color: { gamma: 1.2, saturation: 1.05 },
        qualityPolicy: "adaptive"
    };
    const frozen = freezeBatchProcessingContract(live);
    live.wavelets[0] = 99;
    live.color.gamma = 2.4;
    assert.deepEqual(frozen, {
        wavelets: [1, 2, 3],
        color: { gamma: 1.2, saturation: 1.05 },
        qualityPolicy: "adaptive"
    });
    assert.equal(Object.isFrozen(frozen), true);
    assert.equal(Object.isFrozen(frozen.wavelets), true);
    assert.equal(Object.isFrozen(frozen.color), true);
});

test("batch entry results accept both Rust and JS field casing", () => {
    assert.deepEqual(normalizeBatchEntryResult({
        path: "/out/prepared.png",
        master_path: "/out/master.tiff",
        preview_base64: ""
    }), {
        preparedPath: "/out/prepared.png",
        linearMasterPath: "/out/master.tiff",
        preview: "/out/prepared.png"
    });
    assert.deepEqual(normalizeBatchEntryResult({
        preparedPath: "/out/two.png",
        linearMasterPath: "/out/two.tiff",
        previewBase64: "data:image/png;base64,preview"
    }), {
        preparedPath: "/out/two.png",
        linearMasterPath: "/out/two.tiff",
        preview: "data:image/png;base64,preview"
    });
});

test("a half-published batch pair is rejected before animation", () => {
    assert.throws(
        () => normalizeBatchEntryResult({ path: "/out/prepared.png", master_path: "" }),
        /sin su máster lineal RGB16/
    );
    assert.throws(
        () => normalizeBatchEntryResult({ master_path: "/out/master.tiff" }),
        /no publicó la imagen preparada/
    );
});
