import assert from "node:assert/strict";
import test from "node:test";

import { mergeClassifiedDeepSkyFrames } from "../src/deepsky_scan_merge.js";

function emptyBuckets() {
  return { lights: [], darks: [], flats: [], darkFlats: [], bias: [] };
}

test("a corrected FITS role moves a stale light into darks", () => {
  const buckets = emptyBuckets();
  buckets.lights.push({
    path: "/session/DARKS/dark-600s.fits",
    frameType: null,
    name: "dark-600s.fits",
  });

  const result = mergeClassifiedDeepSkyFrames(buckets, {
    darks: [{
      path: "/session/DARKS/dark-600s.fits",
      frameType: "DARK",
      name: "dark-600s.fits",
    }],
  });

  assert.equal(result.reclassified, 1);
  assert.equal(result.total, 0);
  assert.equal(buckets.lights.length, 0);
  assert.equal(buckets.darks.length, 1);
  assert.equal(buckets.darks[0].frameType, "DARK");
});

test("rescanning refreshes metadata without duplicating a frame", () => {
  const buckets = emptyBuckets();
  buckets.darks.push({ path: "/session/dark.fits", frameType: "DARK", gain: null });

  const result = mergeClassifiedDeepSkyFrames(buckets, {
    darks: [{ path: "/session/dark.fits", frameType: "DARK", gain: 160 }],
  });

  assert.equal(result.duplicates, 1);
  assert.equal(result.reclassified, 0);
  assert.equal(buckets.darks.length, 1);
  assert.equal(buckets.darks[0].gain, 160);
});

test("specific calibration evidence wins over a generic light response", () => {
  const buckets = emptyBuckets();
  const path = "/session/DARK FLAT/FlatWizard.fits";

  const result = mergeClassifiedDeepSkyFrames(buckets, {
    lights: [{ path, frameType: null }],
    darkFlats: [{ path, frameType: "DARK", object: "FlatWizard" }],
  });

  assert.equal(result.total, 1);
  assert.equal(result.received, 1);
  assert.equal(buckets.lights.length, 0);
  assert.equal(buckets.darkFlats.length, 1);
});
