// IMPLEMENTACIÓN AREA DE APILADO (STACKING ROI)
let isSettingStackingRoi = false;
let isMovingStackingRoi = false;
let isResizingStackingRoi = false;
let stackingRoiResizeDir = "";
let stackingRoiSelection = null; // {x, y, w, h}
let stackingRoiMoveOffset = { x: 0, y: 0 };

function getSourceLocalCoordinates(evt, container) {
    const img = document.getElementById("img-source");
    if (!img) return { x: 0, y: 0 };

    // Calculate the actual rendered dimensions of the image vs its natural dimensions
    const rect = img.getBoundingClientRect();
    const scaleX = img.naturalWidth / rect.width;
    const scaleY = img.naturalHeight / rect.height;

    const clientX = evt.clientX - rect.left;
    const clientY = evt.clientY - rect.top;

    return { x: clientX * scaleX, y: clientY * scaleY };
}

function updateStackingRoiDOM() {
    const box = document.getElementById("stacking-roi-box");
    const img = document.getElementById("img-source");
    if (!box || !img || !stackingRoiSelection) return;

    // We need to convert from natural coordinates back to display coordinates
    const rect = img.getBoundingClientRect();
    const containerRect = img.parentElement.getBoundingClientRect();
    const scaleX = rect.width / img.naturalWidth;
    const scaleY = rect.height / img.naturalHeight;

    // Offset of the image inside its container
    const offX = rect.left - containerRect.left;
    const offY = rect.top - containerRect.top;

    box.style.left = (offX + stackingRoiSelection.x * scaleX) + "px";
    box.style.top = (offY + stackingRoiSelection.y * scaleY) + "px";
    box.style.width = (stackingRoiSelection.w * scaleX) + "px";
    box.style.height = (stackingRoiSelection.h * scaleY) + "px";
    box.style.display = "block";
}

function setupStackingRoiInteractions() {
    const btnRoi = document.getElementById("btn-stacking-roi");
    const container = document.getElementById("view-source").querySelector(".zoom-target-container");
    const boxDOM = document.getElementById("stacking-roi-box");

    if (!btnRoi || !container) return;

    btnRoi.addEventListener("click", () => {
        const img = document.getElementById("img-source");
        if (!img || !img.src || img.src.includes("data:image")) {
            showCustomAlert("Aviso", "Abre un video y reprodúcelo o analízalo para ver el cuadro base.");
            return;
        }

        isSettingStackingRoi = !isSettingStackingRoi;

        if (isSettingStackingRoi) {
            btnRoi.classList.remove("secondary");
            btnRoi.classList.add("primary");
            btnRoi.style.background = "#10b981";
            btnRoi.style.color = "white";
            btnRoi.textContent = "✅ Área Definida (Ocultar)";

            if (!stackingRoiSelection) {
                // Default to 80% center
                const w = img.naturalWidth * 0.8;
                const h = img.naturalHeight * 0.8;
                const x = (img.naturalWidth - w) / 2;
                const y = (img.naturalHeight - h) / 2;
                stackingRoiSelection = { x, y, w, h };
            }
            updateStackingRoiDOM();

            // disable anchor mode if active
            const btnAnchor = document.getElementById("btn-manual-anchor");
            if (isSettingManualAnchor && btnAnchor) btnAnchor.click();

        } else {
            btnRoi.classList.remove("primary");
            btnRoi.classList.add("secondary");
            btnRoi.style.background = "rgba(16, 185, 129, 0.15)";
            btnRoi.style.color = "#6ee7b7";
            btnRoi.textContent = "📐 Definir Área de Apilado";
            boxDOM.style.display = "none";
        }
    });

    // Handle Resize & Move
    container.addEventListener("mousedown", (e) => {
        if (!isSettingStackingRoi || e.button !== 0) return;

        if (e.target.classList.contains("crop-handle") && e.target.closest("#stacking-roi-box")) {
            isResizingStackingRoi = true;
            stackingRoiResizeDir = e.target.getAttribute("data-dir");
            e.stopPropagation();
            return;
        }

        const clickedBox = e.target.closest("#stacking-roi-box");
        if (clickedBox) {
            isMovingStackingRoi = true;
            const coords = getSourceLocalCoordinates(e, container);
            stackingRoiMoveOffset.x = coords.x - stackingRoiSelection.x;
            stackingRoiMoveOffset.y = coords.y - stackingRoiSelection.y;
            container.style.cursor = "move";
            e.stopPropagation();
            return;
        }
    });

    window.addEventListener("mousemove", (e) => {
        if (!isSettingStackingRoi) return;

        const img = document.getElementById("img-source");
        if (!img) return;
        const imgW = img.naturalWidth;
        const imgH = img.naturalHeight;

        if (isMovingStackingRoi) {
            const coords = getSourceLocalCoordinates(e, container);
            let newX = coords.x - stackingRoiMoveOffset.x;
            let newY = coords.y - stackingRoiMoveOffset.y;

            if (newX < 0) newX = 0;
            if (newY < 0) newY = 0;
            if (newX + stackingRoiSelection.w > imgW) newX = imgW - stackingRoiSelection.w;
            if (newY + stackingRoiSelection.h > imgH) newY = imgH - stackingRoiSelection.h;

            stackingRoiSelection.x = newX;
            stackingRoiSelection.y = newY;
            updateStackingRoiDOM();
            return;
        }

        if (isResizingStackingRoi) {
            const coords = getSourceLocalCoordinates(e, container);

            const curX = Math.max(0, Math.min(coords.x, imgW));
            const curY = Math.max(0, Math.min(coords.y, imgH));

            let oldX = stackingRoiSelection.x;
            let oldY = stackingRoiSelection.y;
            let oldR = stackingRoiSelection.x + stackingRoiSelection.w;
            let oldB = stackingRoiSelection.y + stackingRoiSelection.h;

            if (stackingRoiResizeDir.includes("n")) {
                let newTop = curY;
                if (newTop > oldB - 20) newTop = oldB - 20;
                stackingRoiSelection.y = newTop;
                stackingRoiSelection.h = oldB - newTop;
            }
            if (stackingRoiResizeDir.includes("s")) {
                let newBottom = curY;
                if (newBottom < oldY + 20) newBottom = oldY + 20;
                stackingRoiSelection.h = newBottom - oldY;
            }
            if (stackingRoiResizeDir.includes("w")) {
                let newLeft = curX;
                if (newLeft > oldR - 20) newLeft = oldR - 20;
                stackingRoiSelection.x = newLeft;
                stackingRoiSelection.w = oldR - newLeft;
            }
            if (stackingRoiResizeDir.includes("e")) {
                let newRight = curX;
                if (newRight < oldX + 20) newRight = oldX + 20;
                stackingRoiSelection.w = newRight - oldX;
            }
            updateStackingRoiDOM();
            return;
        }
    });

    window.addEventListener("mouseup", () => {
        if (isMovingStackingRoi) {
            isMovingStackingRoi = false;
            container.style.cursor = "";
        }
        if (isResizingStackingRoi) {
            isResizingStackingRoi = false;
            stackingRoiResizeDir = "";
        }
    });
}
