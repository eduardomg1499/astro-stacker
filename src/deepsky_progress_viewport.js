export const DS_PROGRESS_VIEWPORT_MIN_ZOOM = 1;
export const DS_PROGRESS_VIEWPORT_MAX_ZOOM = 8;

const finite = (value, fallback = 0) => Number.isFinite(Number(value))
    ? Number(value)
    : fallback;

const clamp = (value, min, max) => Math.min(max, Math.max(min, value));

export function normalizeDeepSkyProgressViewport(viewport = {}, metrics = {}) {
    const width = Math.max(0, finite(metrics.width));
    const height = Math.max(0, finite(metrics.height));
    const zoom = clamp(
        finite(viewport.zoom, DS_PROGRESS_VIEWPORT_MIN_ZOOM),
        DS_PROGRESS_VIEWPORT_MIN_ZOOM,
        DS_PROGRESS_VIEWPORT_MAX_ZOOM,
    );
    const maxX = width * (zoom - 1) * 0.5;
    const maxY = height * (zoom - 1) * 0.5;
    return {
        zoom,
        x: clamp(finite(viewport.x), -maxX, maxX),
        y: clamp(finite(viewport.y), -maxY, maxY),
    };
}

export function zoomDeepSkyProgressViewport(viewport, factor, focus = {}, metrics = {}) {
    const current = normalizeDeepSkyProgressViewport(viewport, metrics);
    const nextZoom = clamp(
        current.zoom * finite(factor, 1),
        DS_PROGRESS_VIEWPORT_MIN_ZOOM,
        DS_PROGRESS_VIEWPORT_MAX_ZOOM,
    );
    if (nextZoom === current.zoom) return current;
    const ratio = nextZoom / current.zoom;
    const focusX = finite(focus.x);
    const focusY = finite(focus.y);
    return normalizeDeepSkyProgressViewport({
        zoom: nextZoom,
        x: focusX - ((focusX - current.x) * ratio),
        y: focusY - ((focusY - current.y) * ratio),
    }, metrics);
}

export function panDeepSkyProgressViewport(viewport, delta = {}, metrics = {}) {
    const current = normalizeDeepSkyProgressViewport(viewport, metrics);
    return normalizeDeepSkyProgressViewport({
        ...current,
        x: current.x + finite(delta.x),
        y: current.y + finite(delta.y),
    }, metrics);
}

/**
 * Maps the scientific crop recorded by the stack recipe onto the downsampled
 * reference preview. The returned source rectangle uses the preview's natural
 * pixels, while the destination keeps the exact output aspect ratio. This is
 * what makes a Drizzle/bin/cropped master comparable to its reference frame
 * instead of merely applying the same CSS transform to two unrelated boxes.
 */
export function mapDeepSkyComparisonCrop(
    geometry = {},
    imageMetrics = {},
    maxOutputDimension = 1600,
) {
    const naturalWidth = Math.max(0, finite(imageMetrics.width));
    const naturalHeight = Math.max(0, finite(imageMetrics.height));
    const sourceWidth = Math.max(0, finite(geometry.sourceWidth));
    const sourceHeight = Math.max(0, finite(geometry.sourceHeight));
    if (!naturalWidth || !naturalHeight || !sourceWidth || !sourceHeight) return null;

    const x = clamp(finite(geometry.x), 0, sourceWidth - 1);
    const y = clamp(finite(geometry.y), 0, sourceHeight - 1);
    const cropWidth = clamp(
        finite(geometry.widthBeforeOutputBinning, sourceWidth - x),
        1,
        sourceWidth - x,
    );
    const cropHeight = clamp(
        finite(geometry.heightBeforeOutputBinning, sourceHeight - y),
        1,
        sourceHeight - y,
    );
    const outputWidth = Math.max(1, finite(geometry.outputWidth, cropWidth));
    const outputHeight = Math.max(1, finite(geometry.outputHeight, cropHeight));
    const limit = Math.max(1, finite(maxOutputDimension, 1600));
    const outputScale = Math.min(1, limit / Math.max(outputWidth, outputHeight));

    return {
        sx: x / sourceWidth * naturalWidth,
        sy: y / sourceHeight * naturalHeight,
        sw: cropWidth / sourceWidth * naturalWidth,
        sh: cropHeight / sourceHeight * naturalHeight,
        width: Math.max(1, Math.round(outputWidth * outputScale)),
        height: Math.max(1, Math.round(outputHeight * outputScale)),
    };
}
