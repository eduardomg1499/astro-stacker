import assert from "node:assert/strict";
import test from "node:test";

import {
    mapDeepSkyComparisonCrop,
    normalizeDeepSkyProgressViewport,
    panDeepSkyProgressViewport,
    zoomDeepSkyProgressViewport,
} from "../src/deepsky_progress_viewport.js";

const metrics = { width: 500, height: 320 };

test("the shared progress viewport starts centered and clamps zoom safely", () => {
    assert.deepEqual(normalizeDeepSkyProgressViewport({}, metrics), { zoom: 1, x: 0, y: 0 });
    assert.deepEqual(
        normalizeDeepSkyProgressViewport({ zoom: 99, x: 99_999, y: -99_999 }, metrics),
        { zoom: 8, x: 1750, y: -1120 },
    );
});

test("zoom keeps the selected scientific feature under the pointer", () => {
    const next = zoomDeepSkyProgressViewport(
        { zoom: 1, x: 0, y: 0 },
        2,
        { x: 100, y: -40 },
        metrics,
    );
    assert.deepEqual(next, { zoom: 2, x: -100, y: 40 });
    assert.equal((100 - next.x) / next.zoom, 100);
    assert.equal((-40 - next.y) / next.zoom, -40);
});

test("pan is identical for both 50/50 panes and cannot lose the image", () => {
    const panned = panDeepSkyProgressViewport(
        { zoom: 3, x: 0, y: 0 },
        { x: 10_000, y: -10_000 },
        metrics,
    );
    assert.deepEqual(panned, { zoom: 3, x: 500, y: -320 });
});

test("the reference preview is mapped to the exact crop and output aspect", () => {
    const mapped = mapDeepSkyComparisonCrop({
        sourceWidth: 8000,
        sourceHeight: 6000,
        x: 800,
        y: 600,
        widthBeforeOutputBinning: 6400,
        heightBeforeOutputBinning: 4800,
        outputWidth: 3200,
        outputHeight: 2400,
    }, { width: 1600, height: 1200 }, 1200);
    assert.deepEqual(mapped, {
        sx: 160,
        sy: 120,
        sw: 1280,
        sh: 960,
        width: 1200,
        height: 900,
    });
});

test("comparison crop fails closed for missing geometry and clamps corrupt bounds", () => {
    assert.equal(mapDeepSkyComparisonCrop({}, { width: 1600, height: 1200 }), null);
    const mapped = mapDeepSkyComparisonCrop({
        sourceWidth: 100,
        sourceHeight: 80,
        x: 500,
        y: -10,
        widthBeforeOutputBinning: 500,
        heightBeforeOutputBinning: 500,
        outputWidth: 200,
        outputHeight: 100,
    }, { width: 1000, height: 800 });
    assert.deepEqual(mapped, {
        sx: 990,
        sy: 0,
        sw: 10,
        sh: 800,
        width: 200,
        height: 100,
    });
});
