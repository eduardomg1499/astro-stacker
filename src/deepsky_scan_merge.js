const SCAN_KIND_PRIORITY = Object.freeze([
  "lights",
  "flats",
  "darks",
  "bias",
  "darkFlats",
]);

function framePath(frame) {
  return typeof frame?.path === "string" ? frame.path.trim() : "";
}

/**
 * Merge one or more classified scans into the active WBPP-style session.
 *
 * A new technical probe is authoritative for its path. This matters after an
 * app update or a corrected FITS header read: a DARK that was previously kept
 * in Lights must move to Darks instead of being discarded as a duplicate.
 * Arrays are mutated in place so existing UI references remain valid.
 */
export function mergeClassifiedDeepSkyFrames(buckets, scanned = {}) {
  const incomingByPath = new Map();

  // Generic Lights are visited first and specific calibration roles last. A
  // malformed mixed response therefore fails toward the most specific role.
  for (const kind of SCAN_KIND_PRIORITY) {
    if (!Array.isArray(buckets?.[kind])) continue;
    for (const frame of scanned?.[kind] || []) {
      const path = framePath(frame);
      if (!path) continue;
      incomingByPath.set(path, { kind, frame });
    }
  }

  let total = 0;
  let duplicates = 0;
  let reclassified = 0;

  for (const [path, incoming] of incomingByPath) {
    const matches = [];
    for (const kind of SCAN_KIND_PRIORITY) {
      const frames = buckets[kind];
      for (let index = frames.length - 1; index >= 0; index -= 1) {
        if (framePath(frames[index]) === path) matches.push({ kind, index });
      }
    }

    const alreadyCorrect = matches.length === 1 && matches[0].kind === incoming.kind;
    if (alreadyCorrect) {
      // Refresh metadata (headers, signature warnings, dimensions) even when
      // the bucket is unchanged; a rescan is allowed to repair stale probes.
      buckets[incoming.kind][matches[0].index] = incoming.frame;
      duplicates += 1;
      continue;
    }

    if (matches.length > 0) {
      // Remove every stale occurrence before inserting the single canonical
      // probe. This also repairs sessions affected by an older duplicate bug.
      for (const kind of SCAN_KIND_PRIORITY) {
        const frames = buckets[kind];
        for (let index = frames.length - 1; index >= 0; index -= 1) {
          if (framePath(frames[index]) === path) frames.splice(index, 1);
        }
      }
      reclassified += 1;
    } else {
      total += 1;
    }
    buckets[incoming.kind].push(incoming.frame);
  }

  return {
    total,
    duplicates,
    reclassified,
    received: incomingByPath.size,
  };
}
