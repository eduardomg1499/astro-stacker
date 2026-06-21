
// =========================================================================
// LOGICA DE RECORTE DE VIDEO (PRE-CONVERSION)
// =========================================================================

let isVideoCropping = false;
let isDrawingVideoCrop = false;
let isMovingVideoCrop = false;
let isResizingVideoCrop = false;
let videoCropSelection = { x: 0, y: 0, w: 0, h: 0 };
let videoCropStart = { x: 0, y: 0 };
let videoResizeDir = "";
let videoMoveOffset = { x: 0, y: 0 };

function updateVideoCropDOM() {
    const box = document.getElementById("video-crop-box");
    if (!box) return;

    const img = ui.imgSource;
    const offX = img ? img.offsetLeft : 0;
    const offY = img ? img.offsetTop : 0;

    box.style.left = (offX + videoCropSelection.x) + "px";
    box.style.top = (offY + videoCropSelection.y) + "px";
    box.style.width = videoCropSelection.w + "px";
    box.style.height = videoCropSelection.h + "px";
    box.style.display = "block";
}

function setupVideoCropInteractions() {
    const container = ui.viewSource.querySelector(".zoom-target-container");
    if (!container) return;

    container.addEventListener("mousedown", (e) => {
        if (!isVideoCropping || e.button !== 0) return;

        if (e.target.classList.contains("crop-handle")) {
            isResizingVideoCrop = true;
            videoResizeDir = e.target.getAttribute("data-dir");
            e.stopPropagation();
            return;
        }

        const clickedBox = e.target.closest("#video-crop-box");
        if (clickedBox) {
            isMovingVideoCrop = true;
            const coords = getLocalCoordinates(e, container);
            videoMoveOffset.x = coords.x - videoCropSelection.x;
            videoMoveOffset.y = coords.y - videoCropSelection.y;
            container.style.cursor = "move";
            e.stopPropagation();
            return;
        }

        isDrawingVideoCrop = true;
        const coords = getLocalCoordinates(e, container);
        videoCropStart = coords;
        videoCropSelection = { x: coords.x, y: coords.y, w: 0, h: 0 };
        updateVideoCropDOM();
    });

    window.addEventListener("mousemove", (e) => {
        if (!isVideoCropping) return;

        const img = ui.imgSource;
        const imgW = img.naturalWidth;
        const imgH = img.naturalHeight;

        // Safety check if implementation is missing naturalWidth
        if (!imgW || !imgH) return;

        if (isDrawingVideoCrop) {
            const coords = getLocalCoordinates(e, container);
            let currentX = Math.max(0, Math.min(coords.x, imgW));
            let currentY = Math.max(0, Math.min(coords.y, imgH));

            const minX = Math.min(videoCropStart.x, currentX);
            const minY = Math.min(videoCropStart.y, currentY);
            const w = Math.abs(currentX - videoCropStart.x);
            const h = Math.abs(currentY - videoCropStart.y);

            videoCropSelection = { x: minX, y: minY, w: w, h: h };
            updateVideoCropDOM();
            return;
        }

        if (isMovingVideoCrop) {
            const coords = getLocalCoordinates(e, container);
            let newX = coords.x - videoMoveOffset.x;
            let newY = coords.y - videoMoveOffset.y;

            if (newX < 0) newX = 0;
            if (newY < 0) newY = 0;
            if (newX + videoCropSelection.w > imgW) newX = imgW - videoCropSelection.w;
            if (newY + videoCropSelection.h > imgH) newY = imgH - videoCropSelection.h;

            videoCropSelection.x = newX;
            videoCropSelection.y = newY;
            updateVideoCropDOM();
            return;
        }

        if (isResizingVideoCrop) {
            const coords = getLocalCoordinates(e, container);
            const curX = Math.max(0, Math.min(coords.x, imgW));
            const curY = Math.max(0, Math.min(coords.y, imgH));

            let oldX = videoCropSelection.x;
            let oldY = videoCropSelection.y;
            let oldR = videoCropSelection.x + videoCropSelection.w;
            let oldB = videoCropSelection.y + videoCropSelection.h;

            if (videoResizeDir.includes("n")) {
                let newTop = curY; if (newTop > oldB - 5) newTop = oldB - 5;
                videoCropSelection.y = newTop; videoCropSelection.h = oldB - newTop;
            }
            if (videoResizeDir.includes("s")) {
                let newBottom = curY; if (newBottom < oldY + 5) newBottom = oldY + 5;
                videoCropSelection.h = newBottom - oldY;
            }
            if (videoResizeDir.includes("w")) {
                let newLeft = curX; if (newLeft > oldR - 5) newLeft = oldR - 5;
                videoCropSelection.x = newLeft; videoCropSelection.w = oldR - newLeft;
            }
            if (videoResizeDir.includes("e")) {
                let newRight = curX; if (newRight < oldX + 5) newRight = oldX + 5;
                videoCropSelection.w = newRight - oldX;
            }

            if (videoCropSelection.w < 0) videoCropSelection.w = Math.abs(videoCropSelection.w);
            if (videoCropSelection.h < 0) videoCropSelection.h = Math.abs(videoCropSelection.h);

            updateVideoCropDOM();
            return;
        }
    });

    window.addEventListener("mouseup", () => {
        if (isVideoCropping) {
            isDrawingVideoCrop = false;
            isMovingVideoCrop = false;
            isResizingVideoCrop = false;
            videoResizeDir = "";
            if (container) container.style.cursor = "crosshair";
        }
    });
}
setupVideoCropInteractions();
