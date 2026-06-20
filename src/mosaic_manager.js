import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { i18n } from "./i18n.js";
import { tutorialManager } from "./tutorial_manager.js";

function t(key, fallback = "") {
    const value = i18n?.t?.(key);
    return value && value !== key ? value : fallback;
}

function tf(key, values = {}, fallback = "") {
    let text = t(key, fallback);
    Object.entries(values).forEach(([name, value]) => {
        text = text.split(`{{${name}}}`).join(String(value));
    });
    return text;
}

export class MosaicManager {
    constructor() {
        this.tiles = []; // { id, type: 'video'|'image', path, x, y, width, height, rotation, src, status }
        this.isDragging = false;
        this.draggedTileId = null;
        this.dragOffset = { x: 0, y: 0 };
        this.canvasZoom = 1.0;
        this.magnetEnabled = true;

        this.ui = {
            panel: document.getElementById("panel-mosaic"),
            dropzone: document.getElementById("mosaic-dropzone"),
            list: document.getElementById("mosaic-sources-list"),
            canvasOverlay: document.getElementById("mosaic-viewport-overlay"),
            canvas: document.getElementById("mosaic-canvas"),
            btnAuto: document.getElementById("btn-mosaic-auto"),
            btnClear: document.getElementById("btn-mosaic-clear"),
            btnGenerate: document.getElementById("btn-mosaic-gen"),
            chkMagnet: document.getElementById("chk-mosaic-magnet"),
            btnEdit: document.getElementById("btn-mosaic-edit"),
            // New Workflow Buttons
            btnAnalyzeAll: document.getElementById("btn-mosaic-analyze-all"),
            btnStackAll: document.getElementById("btn-mosaic-stack-all"),
            sliderStackPct: document.getElementById("mosaic-stack-pct"),
            lblStackPct: document.getElementById("mosaic-stack-pct-val"),
            stepAnalyze: document.getElementById("mosaic-step-analyze"),
            stepStack: document.getElementById("mosaic-step-stack"),
            stepContainer: document.getElementById("mosaic-workflow-controls"),
            stepGenerate: document.getElementById("mosaic-step-generate")
        };

        this.init();
    }

    init() {
        if (!this.ui.panel) return;

        // Force Flex Layout when shown (fix for sidebar controller setting display:block)
        const observer = new MutationObserver((mutations) => {
            mutations.forEach((mutation) => {
                if (mutation.type === "attributes" && mutation.attributeName === "style") {
                    if (this.ui.panel.style.display === "block") {
                        this.ui.panel.style.display = "flex";
                        // Ensure direction is maintained if CSS doesn't cover it
                        this.ui.panel.style.flexDirection = "column";
                    }
                }
            });
        });
        observer.observe(this.ui.panel, { attributes: true });

        // Dropzone - Click Logic Only
        this.ui.dropzone.addEventListener("click", () => this.openFileDialog());

        // Robust Drag & Drop Logic (Visual Feedback ONLY)
        const dz = this.ui.dropzone;
        dz.addEventListener("dragenter", (e) => {
            e.preventDefault();
            e.stopPropagation();
            dz.style.background = 'rgba(6, 182, 212, 0.3)';
        });
        dz.addEventListener("dragover", (e) => {
            e.preventDefault();
            e.stopPropagation();
            dz.style.background = 'rgba(6, 182, 212, 0.3)';
        });
        dz.addEventListener("dragleave", (e) => {
            e.preventDefault();
            e.stopPropagation();
            dz.style.background = '';
        });
        // Remove HTML5 Drop - We use Tauri System Drop below
        dz.addEventListener("drop", (e) => {
            e.preventDefault();
            e.stopPropagation();
            dz.style.background = '';
        });

        // TAURI SYSTEM DROP LISTENER (Fixes missing paths in Drop event)
        // TAURI SYSTEM DROP LISTENER (Fixes missing paths in Drop event)
        listen('tauri://file-drop', (event) => {
            // Check if Mosaic Panel is visible/active (using offsetParent is more robust than style.display)
            if (this.ui.panel && this.ui.panel.offsetParent !== null) {
                const files = event.payload; // Array of absolute paths
                if (files && files.length > 0) {
                    this.handleFiles(files);
                }
            } else if (this.ui.panel && this.ui.panel.style.display && this.ui.panel.style.display !== 'none') {
                // Fallback check if offsetParent fails in some contexts
                const files = event.payload;
                if (files && files.length > 0) {
                    this.handleFiles(files);
                }
            }
        });

        // Canvas Interactions
        this.ui.canvas.addEventListener("mousedown", (e) => this.handleMouseDown(e));
        window.addEventListener("mousemove", (e) => this.handleMouseMove(e));
        window.addEventListener("mouseup", () => this.handleMouseUp());

        // Global Drag Prevention (Critical for File Drop support)
        window.addEventListener("dragover", (e) => e.preventDefault());
        window.addEventListener("drop", (e) => e.preventDefault());

        // Standard Buttons
        if (this.ui.chkMagnet) {
            this.ui.chkMagnet.addEventListener("change", (e) => {
                this.magnetEnabled = e.target.checked;
            });
        }

        if (this.ui.btnClear) {
            this.ui.btnClear.addEventListener("click", () => this.clearAll());
        }

        if (this.ui.btnAuto) {
            this.ui.btnAuto.addEventListener("click", () => this.autoArrange());
        }

        if (this.ui.btnGenerate) {
            this.ui.btnGenerate.addEventListener("click", () => this.generatePanorama());
        }

        // New Result Actions Listeners
        const btnSavePng = document.getElementById("btn-mosaic-save-png");
        if (btnSavePng) btnSavePng.addEventListener("click", () => this.saveMosaicResult(0)); // 0 = PNG

        const btnSaveTiff = document.getElementById("btn-mosaic-save-tiff");
        if (btnSaveTiff) btnSaveTiff.addEventListener("click", () => this.saveMosaicResult(1)); // 1 = TIFF

        // Re-bind Edit button if it was replaced or just ensure it works
        if (this.ui.btnEdit) {
            this.ui.btnEdit.addEventListener("click", () => this.sendToWavelets());
        }

        const btnRedo = document.getElementById("btn-mosaic-redo");
        if (btnRedo) {
            btnRedo.removeEventListener("click", this.redoMosaic); // Prevent duplicates if init called multiple times?
            btnRedo.addEventListener("click", () => this.redoMosaic());
        }

        // Progress Listener for Stacking Updates
        listen("progress", (event) => {
            const { pct, step } = event.payload;

            // Only update if we have an active stacking tile
            if (this.activeStackTileId) {
                const el = document.getElementById(this.activeStackTileId);
                if (el) {
                    const badge = el.querySelector(".tile-stack-val");
                    if (badge) {
                        badge.textContent = Math.round(pct) + "%";
                        // Optional: Update text color or icon based on step
                    }
                    // Also update the overlay text if present
                    const overlayText = el.querySelector(".astro-loader + div"); // The text below spinner
                    if (overlayText) {
                        // Simplify message: "Stacking 45%"
                        overlayText.textContent = `Apilando ${Math.round(pct)}%`;
                    }
                }
            }
        });

        // Workflow Buttons
        if (this.ui.btnAnalyzeAll) this.ui.btnAnalyzeAll.addEventListener("click", () => this.analyzeAll());
        if (this.ui.btnStackAll) {
            this.ui.btnStackAll.addEventListener("click", () => {
                // Tutorial: Advance immediately to stacking progress view
                setTimeout(() => tutorialManager.nextStep(), 300);
                this.stackAll();
            });
        }
        this.ui.sliderStackPct.addEventListener("input", (e) => {
            const val = e.target.value;
            this.ui.lblStackPct.textContent = val + "%";
            // Update all tile badges
            const badges = document.querySelectorAll(".tile-stack-val");
            badges.forEach(b => b.textContent = val + "%");
        });

        // Initial Button State
        if (this.ui.stepGenerate) this.ui.stepGenerate.style.display = "none";
    }

    updateWorkflowState() {
        if (!this.ui.stepGenerate) return;

        const hasTiles = this.tiles.length > 0;
        const hasVideo = this.tiles.some(t => t.type === 'video');
        window.__mosaicTutorialHasVideo = hasVideo;

        const labelEl = document.querySelector('#mosaic-workflow-controls > label');

        if (!hasTiles) {
            this.setStepVisibility('none');
            if (labelEl) {
                labelEl.innerHTML = '2. Proceso de Apilado';
                labelEl.style.color = '';
            }
            return;
        }

        if (!hasVideo) {
            // DIRECT IMAGE WORKFLOW
            // All tiles are static images (or stacked results).
            this.setStepVisibility('generate');
            this.ui.btnGenerate.disabled = false;
            if (labelEl) {
                labelEl.innerHTML = '2. Proceso de Apilado <span style="color:#10b981; margin-left:8px; font-weight:bold;">✔ Completado</span>';
                labelEl.style.color = '#10b981';
            }
        } else {
            // VIDEO WORKFLOW
            if (labelEl) {
                labelEl.innerHTML = '2. Proceso de Apilado';
                labelEl.style.color = '';
            }
            // Check if we have videos pending analysis
            const needsAnalysis = this.tiles.some(t => t.type === 'video' && t.status === 'pending_analysis');
            const needsStacking = this.tiles.some(t => t.type === 'video' && t.status === 'analyzed');

            if (needsAnalysis) {
                this.setStepVisibility('analyze');
            } else if (needsStacking) {
                this.setStepVisibility('stack');
            } else {
                this.setStepVisibility('generate');
            }
        }
    }

    setStepVisibility(step) {
        if (this.ui.stepAnalyze) this.ui.stepAnalyze.style.display = step === 'analyze' ? 'block' : 'none';
        if (this.ui.stepStack) this.ui.stepStack.style.display = step === 'stack' ? 'block' : 'none';
        if (this.ui.stepGenerate) this.ui.stepGenerate.style.display = step === 'generate' ? 'block' : 'none';

        // Parent container if exists
        const workflowControls = document.getElementById("mosaic-workflow-controls");
        if (workflowControls) {
            workflowControls.style.display = step === 'none' ? 'none' : 'block';
        }
    }

    async openFileDialog() {
        const files = await openDialog({
            multiple: true,
            filters: [{ name: 'Media', extensions: ['ser', 'avi', 'png', 'tif', 'tiff', 'jpg'] }]
        });
        if (files) {
            this.handleFiles(Array.isArray(files) ? files : [files]);
        }
    }

    handleDrop(e) {
        e.preventDefault();
        e.stopPropagation(); // Stop bubbling to window
        this.ui.dropzone.style.background = '';

        let files = [];
        if (e.dataTransfer.files && e.dataTransfer.files.length > 0) {
            files = Array.from(e.dataTransfer.files);
        }

        if (files.length > 0) {
            this.handleFiles(files);
        }
    }

    async handleFiles(fileList) {
        if (!fileList) return;

        // UI LOCK: Prevent interactions while loading to avoid race conditions
        const lockUI = (locked) => {
            if (this.ui.btnAnalyzeAll) {
                this.ui.btnAnalyzeAll.disabled = locked;
                this.ui.btnAnalyzeAll.textContent = locked ? "⏳ Cargando..." : "🔍 Analizar Todo";
            }
            if (this.ui.btnStackAll) this.ui.btnStackAll.disabled = locked;
            if (this.ui.btnGenerate) this.ui.btnGenerate.disabled = locked;
            if (this.ui.btnClear) this.ui.btnClear.disabled = locked;

            // Show canvas overlay
            let overlay = document.getElementById('mosaic-canvas-global-loader');
            if (!overlay) {
                overlay = document.createElement('div');
                overlay.id = 'mosaic-canvas-global-loader';
                overlay.style.cssText = 'position:absolute; inset:0; background:rgba(15, 23, 42, 0.75); backdrop-filter:blur(4px); z-index:999; display:flex; flex-direction:column; align-items:center; justify-content:center; border-radius:8px;';
                overlay.innerHTML = `
                    <div class="astro-loader" style="transform:scale(0.8)">
                        <div class="astro-star"></div>
                        <div class="orbit-sys outer"><div class="orbit-ring"></div><div class="astro-planet planet-1"></div></div>
                    </div>
                    <div style="color:#38bdf8; font-size:1.1rem; font-weight:bold; margin-top:20px;">Cargando Fuentes...</div>
                    <div style="color:#94a3b8; font-size:0.8rem; margin-top:5px;">Procesando archivos, por favor espera.</div>
                `;
                if(this.ui.canvas && this.ui.canvas.parentElement) {
                    this.ui.canvas.parentElement.appendChild(overlay);
                }
            }
            overlay.style.display = locked ? 'flex' : 'none';
        };

        lockUI(true);

        try {
            // Processing files slightly off-thread so the UI can paint the loader
            await new Promise(r => setTimeout(r, 50));

            // Handle various Tauri v2 / HTML5 return types
            const files = Array.isArray(fileList) ? fileList : [fileList];

            for (const file of files) {
                let path = null;
                if (typeof file === 'string') {
                    path = file;
                } else if (file && typeof file === 'object') {
                    path = file.path || file.name; // Fallback to name if path missing
                }

                if (path) {
                    await this.addTile(path);
                } else {
                    console.error("Invalid file object encountered:", file);
                }
            }
        } catch (e) {
            console.error("Error handling files:", e);
            alert("Error al cargar archivos: " + e);
        } finally {
            // Wait for all thumbnails to finish loading their base64 (since addTile calls them without awaiting inside handleFiles array)
            const waitTime = Math.min(Array.isArray(fileList) ? fileList.length * 150 : 500, 2000);
            await new Promise(r => setTimeout(r, waitTime));

            // UI UNLOCK - Always execute even if addTile fails
            lockUI(false);
            this.updateWorkflowState();

            // Tutorial: file selection completed. The guide branches later based
            // on whether these are videos or already-stacked images.
            if (tutorialManager?.currentFlowName === 'mosaic' && tutorialManager.currentStepIndex === 1) {
                setTimeout(() => tutorialManager.nextStep(), 500);
            }
        }
    }

    async addTile(path) {
        if (!path || typeof path !== 'string') {
            console.error("addTile requires a string path, got:", path);
            return;
        }

        const ext = path.split('.').pop().toLowerCase();
        const isVideo = ['ser', 'avi'].includes(ext);
        const id = 'tile_' + Date.now() + Math.random().toString(36).substr(2, 5);

        const tile = {
            id,
            path,
            type: isVideo ? 'video' : 'image',
            x: 50 + (this.tiles.length * 30),
            y: 50 + (this.tiles.length * 30),
            width: 200, // placeholder until loaded
            height: 150,
            rotation: 0,
            status: isVideo ? 'pending_analysis' : 'ready', // Wait for analysis flow
            src: '',
            analysisData: null
        };

        this.tiles.push(tile);
        this.renderList();
        this.renderCanvas();

        if (isVideo) {
            this.loadVideoThumbnail(tile);
        } else {
            // UNIFIED: Use backend loading for images too (avoids asset protocol issues)
            console.log(`Loading Image Tile via Backend: ${path}`);
            this.loadVideoThumbnail(tile);
        }
    }

    async loadVideoThumbnail(tile) {
        try {
            console.log("Requesting preview for:", tile.path);

            // Backend now handles both via separate commands or unified?
            // I implemented 'load_image_thumbnail' for images.
            // 'preview_video' for videos.

            let res = null;
            if (tile.type === 'video') {
                res = await invoke("preview_video", { path: tile.path });
            } else {
                // Image
                res = await invoke("load_image_thumbnail", { path: tile.path });
            }

            if (res && res.preview_base64) {
                const prefix = res.preview_base64.startsWith("data:") ? "" : "data:image/png;base64,";
                tile.src = prefix + res.preview_base64;
                if (res.metadata && res.metadata.width > 0) {
                    // Update Aspect Ratio based on REAL metadata
                    const ratio = res.metadata.width / res.metadata.height;
                    tile.height = 200 / ratio;
                    tile.width = 200;

                    // Also update tile status if it was pending
                    if (tile.type === 'image') {
                        tile.status = 'ready';
                    }
                }
            } else {
                console.warn("No preview returned for", tile.path);
                tile.status = 'error';
            }
        } catch (e) {
            console.error("Thumb failed for path:", tile.path, e);
            tile.status = 'error';
        }
        this.renderCanvas();
        this.renderList();
    }

    async analyzeAll() {
        const videos = this.tiles.filter(t => t.type === 'video' && t.status === 'pending_analysis');
        if (videos.length === 0) {
            if (this.tiles.some(t => t.status === 'analyzed')) {
                this.ui.stepStack.style.display = "block";
                this.ui.stepAnalyze.style.display = "none";
            }
            return;
        }

        const btn = this.ui.btnAnalyzeAll;
        btn.disabled = true;
        btn.textContent = "⏳ Analizando...";

        let successCount = 0;

        for (const tile of videos) {
            const el = document.getElementById(tile.id);
            if (el) {
                // Centered Loader with Status Text
                el.innerHTML = `
                    <div style="position:absolute; inset:0; background:rgba(0,0,0,0.7); display:flex; flex-direction:column; align-items:center; justify-content:center; z-index:10;">
                        <div class="astro-loader mini" style="transform:scale(0.8);">
                            <div class="astro-star"></div>
                            <div class="orbit-sys outer"><div class="orbit-ring"></div><div class="astro-planet planet-1"></div></div>
                            <div class="orbit-sys inner"><div class="orbit-ring"></div><div class="astro-planet planet-2"></div></div>
                        </div>
                        <div style="color:#94a3b8; font-size:0.8rem; margin-top:10px; font-weight:bold;">Analizando...</div>
                    </div>
                `;
            }

            try {
                const displayIdx = successCount + 1;
                const categoryRaw = document.getElementById("sel-target-category")?.value || "surface";
                const category = categoryRaw === "planet_large" ? "planet_small" : categoryRaw;
                const flow = window.getZenithUltimateFlow
                    ? window.getZenithUltimateFlow(category)
                    : {
                        analysisMode: category === "surface" ? "surface_v3" : "planet_v3",
                        batchMode: category === "surface" ? "surface_v3" : "planet_v3",
                        alignMode: "liquid_v3",
                        warpingAnalysis: true,
                        isV3: true,
                        needsPoints: true,
                        apSize: 32,
                        apThreshold: category === "surface" ? 0.04 : 0.08,
                        gridMode: category === "surface" ? "surface" : "planetary",
                        category
                    };
                const mode = flow.analysisMode;
                const res = await invoke("analyze_video", {
                    path: tile.path,
                    mode: mode,
                    targetType: flow.category,
                    warpingAnalysis: flow.warpingAnalysis,
                    progressPrefix: `[Tesela ${displayIdx}/${videos.length}]`
                });
                tile.analysisData = res.stats;
                tile.status = 'analyzed';
                successCount++;
                if (el) {
                    // Success Result Badge (floating top-right)
                    // Added 'tile-stack-val' class for dynamic updates
                    const currentPct = this.ui.sliderStackPct.value;
                    el.innerHTML = `
                        <div class="tile-status" style="display:flex; gap:5px; position:absolute; top:5px; right:5px; background:#059669; padding:4px 8px; border-radius:4px; font-size:0.8rem; font-weight:bold; color:white; z-index:20; box-shadow:0 2px 4px rgba(0,0,0,0.3);">
                            <span>Q: ${Math.round(res.stats.avg_quality)}%</span>
                            <span style="opacity:0.7;">|</span>
                            <span>S: <span class="tile-stack-val">${currentPct}%</span></span>
                        </div>
                    `;
                }
            } catch (e) {
                console.error("Analysis failed", tile.path, e);
                tile.status = 'error';
                if (el) el.innerHTML = `<div class="tile-status" style="position:absolute; inset:0; display:flex; align-items:center; justify-content:center; background:rgba(239,68,68,0.8); color:white; font-weight:bold;">Error</div>`;
            }
        }

        btn.disabled = false;
        btn.textContent = "🔍 Analizar Todo";

        if (successCount === 0) {
            // If all failed, DO NOT advance. Alert user.
            alert("Error: Analysis failed for all selected videos. Please check file formats or logs.");
            return;
        }

        this.ui.stepAnalyze.style.display = "none";
        this.ui.stepStack.style.display = "block";

        if (tutorialManager?.currentFlowName === 'mosaic' && tutorialManager.currentStepIndex === 4) {
            setTimeout(() => tutorialManager.nextStep(), 500);
        }

        const analyzed = this.tiles.filter(t => t.status === 'analyzed');
        if (analyzed.length > 0) {
            const avg = analyzed.reduce((a, b) => a + (b.analysisData ? b.analysisData.avg_quality : 0), 0) / analyzed.length;
            document.getElementById("mosaic-quality-summary").textContent = `Promedio Calidad: ${Math.round(avg)}%`;
        }
    }

    async stackAll() {
        const videos = this.tiles.filter(t => t.type === 'video' && t.status === 'analyzed');
        if (videos.length === 0) {
            console.log("No videos to stack or not analyzed yet.");
            this.updateWorkflowState();
            return;
        }

        const btn = this.ui.btnStackAll;
        btn.disabled = true;
        btn.textContent = "📚 Apilando...";

        const pct = document.getElementById("mosaic-stack-pct")?.value || 20;
        const drizzle = 1.0;
        const categoryRaw = document.getElementById("sel-target-category")?.value || "surface";
        const category = categoryRaw === "planet_large" ? "planet_small" : categoryRaw;
        const flow = window.getZenithUltimateFlow
            ? window.getZenithUltimateFlow(category)
            : {
                analysisMode: category === "surface" ? "surface_v3" : "planet_v3",
                batchMode: category === "surface" ? "surface_v3" : "planet_v3",
                alignMode: "liquid_v3",
                warpingAnalysis: true,
                isV3: true,
                needsPoints: true,
                apSize: 32,
                apThreshold: category === "surface" ? 0.04 : 0.08,
                gridMode: category === "surface" ? "surface" : "planetary",
                category
            };
        const mode = flow.batchMode;

        if (window.showProcessing) window.showProcessing(`APILANDO ${videos.length} TESELAS...`);

        // Use global pipeline params but force deringing OFF
        const p = typeof window.getPipelineParams === "function" ? window.getPipelineParams() : null;

        // Output folder logic: Use the directory of the first video
        const firstVideoPath = videos[0].path;
        const sep = firstVideoPath.includes("\\") ? "\\" : "/";
        const baseDir = firstVideoPath.substring(0, firstVideoPath.lastIndexOf(sep));
        const timestamp = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
        const outputFolder = `${baseDir}${sep}Mosaic_Stacks_${timestamp}`;

        try {
            if (window.mkdir) await window.mkdir(outputFolder);
        } catch (e) {
            console.warn("Could not create mosaic output folder, using source dir.", e);
        }

        let successCount = 0;
        for (let i = 0; i < videos.length; i++) {
            const tile = videos[i];
            const displayIdx = i + 1;
            this.activeStackTileId = tile.id;

            if (window.log) window.log("INFO", `[Mosaico] Apilando ${displayIdx}/${videos.length}: ${tile.path.split(sep).pop()}...`);

            // Progress update in overlay
            const overlayDet = document.getElementById("processing-details");
            if (overlayDet) overlayDet.textContent = `Procesando tesela ${displayIdx} de ${videos.length}...`;

            const el = document.getElementById(tile.id);
            if (el) {
                el.innerHTML = `
                    <div style="position:absolute; inset:0; background:rgba(0,0,0,0.7); display:flex; flex-direction:column; align-items:center; justify-content:center; z-index:10; pointer-events:none;">
                        <div class="astro-loader mini" style="transform:scale(0.8);">
                            <div class="astro-star"></div>
                         </div>
                    </div>
                `;
            }

            try {
                const bayerValue = document.getElementById("sel-bayer-override")?.value || "auto";
                const bOverride = bayerValue === "auto" ? null : parseInt(bayerValue);
                const sharpened = document.getElementById("chk-sharpened")?.checked || false;
                const doublePass = document.getElementById("chk-double-pass")?.checked || false;

                // Call the standard batch entry command
                const result = await invoke("process_batch_entry", {
                    filePath: tile.path,
                    outputFolder: outputFolder,
                    stackPct: parseFloat(pct),
                    drizzle: drizzle,
                    alignMode: flow.alignMode,
                    u1: p?.u[0] || 0, u2: p?.u[1] || 0, u3: p?.u[2] || 0, u4: p?.u[3] || 0, u5: p?.u[4] || 0,
                    w1: p?.w[0] || 0, w2: p?.w[1] || 0, w3: p?.w[2] || 0, w4: p?.w[3] || 0, w5: p?.w[4] || 0, w6: p?.w[5] || 0,
                    d1: p?.d[0] || 0, d2: p?.d[1] || 0, d3: p?.d[2] || 0, d4: p?.d[3] || 0, d5: p?.d[4] || 0, d6: p?.d[5] || 0,
                    gamma: p?.color.g || 1.0, saturation: p?.color.s || 1.0,
                    contrast: p?.color.c || 1.0, brightness: p?.color.b || 1.0,
                    rBal: p?.color.rb || 1.0, bBal: p?.color.bb || 1.0,
                    rX: p?.shift.rx || 0, rY: p?.shift.ry || 0, bX: p?.shift.bx || 0, bY: p?.shift.by || 0,

                    deringingMode: 0,
                    deringingRadius: p?.dr.rad || 2.0,
                    deringingDark: p?.dr.dark || 0.1,
                    deringingLight: p?.dr.light || 0.1,
                    deringingMask: false,

                    crisp: p?.crisp || 0,
                    deconvIter: p?.deconv.i || 0, deconvSigma: p?.deconv.s || 1.0,
                    vcIter: p?.deconv.vi || 0, vcSigma: p?.deconv.vs || 1.0,
                    usmAmount: p?.usm.a || 0, usmRadius: p?.usm.r || 1.0, lceAmount: p?.lce || 0,
                    masterDenoise: p?.masterDenoise || 0,
                    blend: (p?.blend || 100) / 100.0,
                    useRgbSharpening: p?.useRgbSharpening || false,
                    batchMode: mode,
                    targetType: flow.category,
                    bayerOverride: bOverride,
                    anchorOverride: null,
                    sharpened: sharpened,
                    sharpenIntensity: parseFloat(document.getElementById("num-sharpen-intensity")?.value) || 0.5,
                    doublePass: doublePass,
                    warpingAnalysis: flow.warpingAnalysis,
                    normalizeColors: document.getElementById("chk-normalize-colors")?.checked || false,
                    isV3: flow.isV3,
                    apGridSize: flow.apSize,
                    apThreshold: flow.apThreshold,
                    progressPrefix: `[Mosaico ${displayIdx}/${videos.length}]`
                });

                tile.path = result.path;
                tile.type = 'image';
                tile.status = 'ready';
                tile.src = result.preview_base64;
                successCount++;

                if (el) el.innerHTML = "";

            } catch (e) {
                if (window.log) window.log("ERROR", `Fallo al apilar tesela ${tile.path}: ${e?.message || e}`);
                tile.status = 'error';
                if (el) el.innerHTML = `<div style="color:red; font-size:0.6rem;">Error</div>`;
                if (window.hideProcessing) window.hideProcessing();
            }
        }

        btn.disabled = false;
        btn.textContent = "📚 Apilar por Lotes";
        if (window.hideProcessing) window.hideProcessing();
        if (window.log) window.log("SUCCESS", `Apilado completado: ${successCount} de ${videos.length} teselas procesadas.`);
        this.updateWorkflowState();
        this.renderCanvas();
        if (tutorialManager?.currentFlowName === 'mosaic' && tutorialManager.currentStepIndex === 5) {
            setTimeout(() => tutorialManager.nextStep(), 500);
        }
    }

    updateTileDimensions(tile) {
        const img = new Image();
        img.onload = () => {
            const ratio = img.width / img.height;
            tile.width = 200;
            tile.height = 200 / ratio;
            this.renderCanvas();
        };
        img.src = tile.src;
    }

    renderList() {
        this.ui.list.innerHTML = "";
        // iterate slightly reversed so newest top? or normal
        this.tiles.forEach(tile => {
            const item = document.createElement("div");
            item.className = "tile-list-item";
            const name = tile.path.split(/[/\\]/).pop();

            let thumbHTML = "";
            if (tile.src) {
                thumbHTML = `<img src="${tile.src}" class="tile-list-thumb">`;
            } else if (tile.status === 'error') {
                thumbHTML = `
                    <div class="tile-list-thumb" style="display:flex; align-items:center; justify-content:center; background:#450a0a; color:#f87171;">
                        <svg class="zas-icon"><use href="#icon-cross"></use></svg>
                    </div>`;
            } else {
                // Loading Animation for List
                thumbHTML = `
                    <div class="tile-list-thumb" style="display:flex; align-items:center; justify-content:center; background:#1e293b; overflow:hidden;">
                        <div class="astro-loader mini" style="transform:scale(0.3); margin:0;">
                            <div class="astro-star"></div>
                            <div class="orbit-sys outer"><div class="orbit-ring"></div><div class="astro-planet planet-1"></div></div>
                            <div class="orbit-sys inner"><div class="orbit-ring"></div><div class="astro-planet planet-2"></div></div>
                        </div>
                    </div>`;
            }

            item.innerHTML = `
                ${thumbHTML}
                <span class="tile-list-name" title="${tile.path}">${name}</span>
                <span class="tile-list-remove" title="Remove">✖</span>
                `;

            item.querySelector(".tile-list-remove").addEventListener("click", (e) => {
                e.stopPropagation();
                this.removeTile(tile.id);
            });

            this.ui.list.appendChild(item);
        });
    }

    removeTile(id) {
        this.tiles = this.tiles.filter(t => t.id !== id);
        this.renderList();
        this.renderCanvas();
        this.updateWorkflowState();
    }

    clearAll() {
        this.tiles = [];
        document.getElementById("mosaic-info-card")?.remove();
        document.getElementById("mosaic-info-chip")?.remove();
        if (window.setMosaicViewportMode) window.setMosaicViewportMode(false);
        this.renderList();
        this.renderCanvas();
        this.updateWorkflowState();
    }

    renderCanvas() {
        this.ui.canvas.innerHTML = '<div id="mosaic-grid-bg" style="position: absolute; width: 100%; height: 100%; opacity: 0.1; background-image: linear-gradient(#333 1px, transparent 1px), linear-gradient(90deg, #333 1px, transparent 1px); background-size: 50px 50px;"></div>';

        this.tiles.forEach(tile => {
            const el = document.createElement("div");
            el.className = `mosaic-tile ${tile.status === 'processing' ? 'processing' : ''}`;
            el.id = tile.id;
            el.style.left = tile.x + "px";
            el.style.top = tile.y + "px";
            el.style.width = tile.width + "px";
            el.style.height = tile.height + "px";

            if (tile.src) {
                el.style.backgroundImage = `url('${tile.src}')`;
                el.style.backgroundSize = "contain";
                el.style.backgroundRepeat = "no-repeat";
                el.style.backgroundPosition = "center";
            } else if (tile.status === 'error') {
                // Error State for Canvas
                el.innerHTML = `
                    <div style="width:100%; height:100%; display:flex; flex-direction:column; align-items:center; justify-content:center; background:rgba(239, 68, 68, 0.2); border:1px solid #ef4444; color:#ef4444;">
                        <svg class="zas-icon" style="width: 2em; height: 2em;"><use href="#icon-cross"></use></svg>
                        <span style="font-size:0.8em; text-align:center;">Error</span>
                    </div>`;
            } else {
                // Loader placeholder
                el.innerHTML = `
                    <div style="width:100%; height:100%; display:flex; align-items:center; justify-content:center; background:rgba(15, 23, 42, 0.4);">
                        <div class="astro-loader mini" style="transform:scale(0.8); margin:0;">
                            <div class="astro-star"></div>
                            <div class="orbit-sys outer"><div class="orbit-ring"></div><div class="astro-planet planet-1"></div></div>
                            <div class="orbit-sys inner"><div class="orbit-ring"></div><div class="astro-planet planet-2"></div></div>
                        </div>
                    </div>
                `;
            }

            if (tile.status === 'processing') {
                // Initial processing state if set
            }

            this.ui.canvas.appendChild(el);
        });
    }

    handleMouseDown(e) {
        if (e.target.classList.contains("mosaic-tile")) {
            this.isDragging = true;
            this.draggedTileId = e.target.id;
            const tile = this.tiles.find(t => t.id === this.draggedTileId);

            // Bring to front
            this.tiles = this.tiles.filter(t => t.id !== tile.id).concat(tile);
            this.renderCanvas();

            // Refind element after re-render
            const el = document.getElementById(this.draggedTileId);
            if (el) el.classList.add("dragging");

            const rect = el.getBoundingClientRect();
            const canvasRect = this.ui.canvas.getBoundingClientRect();

            this.dragOffset.x = e.clientX - rect.left;
            this.dragOffset.y = e.clientY - rect.top;
        }
    }

    handleMouseMove(e) {
        if (!this.isDragging || !this.draggedTileId) return;

        const canvasRect = this.ui.canvas.getBoundingClientRect();
        const x = e.clientX - canvasRect.left - this.dragOffset.x;
        const y = e.clientY - canvasRect.top - this.dragOffset.y;

        const tile = this.tiles.find(t => t.id === this.draggedTileId);
        if (tile) {
            tile.x = x;
            tile.y = y;

            const el = document.getElementById(this.draggedTileId);
            if (el) {
                el.style.left = x + "px";
                el.style.top = y + "px";
            }

            if (this.magnetEnabled) {
                this.checkMagneticSnap(tile);
            }
        }
    }

    handleMouseUp() {
        if (this.isDragging) {
            const el = document.getElementById(this.draggedTileId);
            if (el) el.classList.remove("dragging");

            // Remove ghosts
            const ghosts = document.querySelectorAll(".mosaic-ghost");
            ghosts.forEach(g => g.remove());
        }
        this.isDragging = false;
        this.draggedTileId = null;
    }

    checkMagneticSnap(activeTile) {
        // Simplified frontend snap simulation
        // In real impl, we'd call backend or do pixel sampling.
        // Here we just snap bounding boxes if close.
        const snapThreshold = 20;

        this.tiles.forEach(other => {
            if (other.id === activeTile.id) return;

            // Check proximity (Right Edge of Active -> Left Edge of Other)
            if (Math.abs((activeTile.x + activeTile.width) - other.x) < snapThreshold &&
                Math.abs(activeTile.y - other.y) < snapThreshold) {

                // Show Ghost
                this.showGhost(other.x - activeTile.width, other.y, activeTile.width, activeTile.height);

                // If extremely close, snap! (MouseUp logic handles final commit, but visual cue here)
            }
        });
    }

    showGhost(x, y, w, h) {
        let ghost = document.querySelector(".mosaic-ghost");
        if (!ghost) {
            ghost = document.createElement("div");
            ghost.className = "mosaic-ghost";
            this.ui.canvas.appendChild(ghost);
        }
        ghost.style.left = x + "px";
        ghost.style.top = y + "px";
        ghost.style.width = w + "px";
        ghost.style.height = h + "px";
    }

    autoArrange() {
        // Simple grid
        let x = 50, y = 50, maxHeight = 0;
        const canvasWidth = this.ui.canvas.offsetWidth;

        this.tiles.forEach(tile => {
            if (x + tile.width > canvasWidth) {
                x = 50;
                y += maxHeight + 10;
                maxHeight = 0;
            }
            tile.x = x;
            tile.y = y;
            x += tile.width + 10;
            if (tile.height > maxHeight) maxHeight = tile.height;
        });
        this.renderCanvas();

        if (tutorialManager?.currentFlowName === 'mosaic' && tutorialManager.currentStepIndex === 2) {
            setTimeout(() => tutorialManager.nextStep(), 300);
        }
    }

    async generatePanorama() {
        if (this.tiles.length < 1) {
            return;
        }

        // Check Integration Mode (User requested "types of integration")
        // ENFORCED: Blind Stitching (Microsoft ICE style)
        const mode = document.getElementById("chk-mosaic-guided")?.checked ? "guided" : "blind";

        console.log("Starting Mosaic Generation (" + mode + ")...");
        const shouldPauseMosaicTutorial =
            tutorialManager?.currentFlowName === 'mosaic' && tutorialManager.currentStepIndex === 6;
        if (shouldPauseMosaicTutorial) {
            tutorialManager.hideOverlay();
        }

        // Show Processing Overlay
        if (window.showProcessing) window.showProcessing(t("mosaic.generation.processing", "GENERANDO MOSAICO..."));

        const btn = this.ui.btnGenerate;
        btn.disabled = true;
        btn.innerHTML = t("mosaic.generation.stitching", "<svg class='zas-icon icon-spin'><use href='#icon-settings'></use></svg> Uniendo...");

        // Call Backend
        // We pass the tile configurations and the mode
        const tiles = this.tiles.map(t => ({
            path: t.path,
            x: Math.round(t.x),
            y: Math.round(t.y),
            width: Math.round(t.width),
            height: Math.round(t.height),
            rotation: 0.0 // Future
        }));

        try {
            // Invoke Tauri Command
            // fn stitch_mosaic(tiles, mode)
            // Invoke Tauri Command
            // fn stitch_mosaic(tiles, mode)
            const res = await invoke("stitch_mosaic", {
                tiles: tiles,
                mode: mode
            });

            // Success Updates
            // Hide Generation Step
            if (this.ui.stepGenerate) this.ui.stepGenerate.style.display = 'none';

            // Show Result Actions
            const resultActions = document.getElementById("mosaic-result-actions");
            if (resultActions) resultActions.style.display = 'block';

            // Default to final result view. Source/mosaic layout remains available
            // through the toggle so the user is not dropped into a split view.
            if (this.ui.canvasOverlay) {
                this.ui.canvasOverlay.style.display = 'none';
                this.ui.canvasOverlay.style.width = '50%';
                this.ui.canvasOverlay.style.right = 'auto'; // Ensure left aligned
                this.ui.canvasOverlay.style.left = '0';
            }

            const viewSource = document.getElementById("view-source");
            const viewResult = document.getElementById("view-result");
            const btnToggleSource = document.getElementById("btn-toggle-source");

            // Source View Container (Background for Canvas)
            if (viewSource) {
                viewSource.style.display = "none";
                viewSource.style.flex = "1";
            }
            // Result View Container
            if (viewResult) {
                viewResult.style.display = "flex";
                viewResult.style.flex = "1";
            }

            // Toggle Logic
            if (btnToggleSource) {
                btnToggleSource.style.display = "block";
                btnToggleSource.textContent = t("viewer.show_source", "Mostrar Fuente");
                btnToggleSource.onclick = () => {
                    if (this.ui.canvasOverlay.style.display === "none") {
                        // SHOW Source
                        this.ui.canvasOverlay.style.display = "block";
                        if (viewSource) viewSource.style.display = "flex";
                        btnToggleSource.textContent = t("viewer.hide_source", "Ocultar Fuente");
                    } else {
                        // HIDE Source
                        this.ui.canvasOverlay.style.display = "none";
                        if (viewSource) viewSource.style.display = "none";
                        btnToggleSource.textContent = t("viewer.show_source", "Mostrar Fuente");
                    }
                    requestAnimationFrame(() => {
                        if (window.fitMosaicResultViewport) window.fitMosaicResultViewport();
                        setTimeout(() => {
                            if (window.fitMosaicResultViewport) window.fitMosaicResultViewport();
                        }, 100);
                    });
                };
            }

            // Force Image Styling to be visible and SET SOURCE
            const imgResult = document.getElementById("img-result");
            if (imgResult) {
                imgResult.style.width = 'auto';
                imgResult.style.height = 'auto';
                imgResult.style.maxWidth = 'none';
                imgResult.style.maxHeight = 'none';
                imgResult.style.objectFit = 'contain';

                const previewInfo = this.resolveMosaicPreviewSource(res);
                if (previewInfo.src) {
                    if (window.setMosaicViewportMode) window.setMosaicViewportMode(true);
                    let displayedPreviewInfo = previewInfo;
                    let loaded = false;
                    if (window.setImageAndWait) {
                        loaded = await window.setImageAndWait(imgResult, previewInfo.src, true);
                    } else {
                        imgResult.src = previewInfo.src;
                        loaded = true;
                    }

                    if (!loaded && previewInfo.path) {
                        console.warn("Mosaic native preview path failed to load. Falling back to backend thumbnail.");
                        const thumbnailSrc = await this.loadBackendPreview(previewInfo.path);
                        if (thumbnailSrc && window.setImageAndWait) {
                            loaded = await window.setImageAndWait(imgResult, thumbnailSrc, true);
                            displayedPreviewInfo = { src: thumbnailSrc, path: previewInfo.path, native: false };
                        }
                    }

                    if (!loaded && res.path) {
                        console.warn("Mosaic preview failed to load. Falling back to master result path.");
                        const fallbackSrc = convertFileSrc(res.path);
                        if (window.setImageAndWait) {
                            loaded = await window.setImageAndWait(imgResult, fallbackSrc, true);
                        } else {
                            imgResult.src = fallbackSrc;
                            loaded = true;
                        }
                    }

                    if (!loaded) {
                        imgResult.removeAttribute("src");
                        imgResult.style.display = "none";
                    } else {
                        imgResult.style.display = "block";
                        if (window.fitMosaicResultViewport) window.fitMosaicResultViewport();
                    }
                    imgResult.classList.add("img-reveal");
                    this.renderMosaicInfoCard(res, displayedPreviewInfo, mode);
                }
            } else {
                console.error("MosaicManager: FATAL - img-result NOT FOUND");
            }

            // Save the path so the buttons work
            if (window.setCurrentFilePath) {
                window.setCurrentFilePath(res.path);
            } else {
                window.currentFilePath = res.path;
            }

            console.log("Mosaic Generated. Waiting for user confirmation.");

            if (shouldPauseMosaicTutorial) {
                tutorialManager.showOverlay();
                setTimeout(() => tutorialManager.nextStep(), 500);
            }

        } catch (e) {
            console.error("STITCH_MOSAIC FAILED");
            console.error("Error Details:", e);
            console.error("Tiles Configuration Payload:", JSON.stringify(tiles, null, 2));
            btn.innerHTML = t("mosaic.generation.error_button", "<svg class='zas-icon'><use href='#icon-cross'></use></svg> Error");
            alert(tf("mosaic.generation.failed_message", { error: e }, "No se pudo unir el mosaico. Revisa los detalles.\n{{error}}"));
        } finally {
            if (shouldPauseMosaicTutorial && tutorialManager?.currentFlowName === 'mosaic' && tutorialManager.currentStepIndex === 6) {
                tutorialManager.showOverlay();
            }
            btn.disabled = false;
            if (window.hideProcessing) window.hideProcessing();
        }
    }

    async saveMosaicResult(formatIdx) {
        const currentPath = window.getCurrentFilePath?.() || window.currentFilePath || "";
        if (!currentPath) {
            alert(t("mosaic.results.no_result", "No hay mosaico generado para guardar."));
            return;
        }

        const ext = formatIdx === 0 ? "png" : "tiff";
        console.log(`Saving Mosaic as ${ext}...`);

        try {
            const result = await invoke("export_mosaic_result", {
                sourcePath: currentPath,
                formatIdx: formatIdx
            });

            alert(result || tf("mosaic.results.save_success", { ext }, `Guardado exitosamente como .${ext}`));
        } catch (e) {
            console.error("Save Failed", e);
            alert(tf("mosaic.results.save_error", { error: e }, "Error al guardar: {{error}}"));
        }
    }

    resolveMosaicPreviewSource(res) {
        const previewRef = res?.preview_base64 || "";
        if (previewRef.startsWith("file_path:")) {
            const previewPath = previewRef.slice("file_path:".length);
            return { src: convertFileSrc(previewPath), path: previewPath, native: true };
        }
        if (previewRef) {
            const prefix = previewRef.startsWith("data:") ? "" : "data:image/png;base64,";
            return { src: prefix + previewRef, path: "", native: true };
        }
        if (res?.path) {
            return { src: convertFileSrc(res.path), path: res.path, native: true };
        }
        return { src: "", path: "", native: false };
    }

    async loadBackendPreview(path) {
        if (!path) return "";
        try {
            const preview = await invoke("load_image_thumbnail", { path });
            const previewRef = preview?.preview_base64 || "";
            if (!previewRef) return "";
            return previewRef.startsWith("data:") ? previewRef : `data:image/png;base64,${previewRef}`;
        } catch (e) {
            console.warn("Backend thumbnail fallback failed:", e);
            return "";
        }
    }

    renderMosaicInfoCard(res, previewInfo, mode) {
        const viewResult = document.getElementById("view-result");
        if (!viewResult) return;

        let card = document.getElementById("mosaic-info-card");
        let chip = document.getElementById("mosaic-info-chip");
        if (!card) {
            card = document.createElement("div");
            card.id = "mosaic-info-card";
            viewResult.appendChild(card);
        }
        if (!chip) {
            chip = document.createElement("button");
            chip.id = "mosaic-info-chip";
            chip.type = "button";
            chip.textContent = t("mosaic.info.show", "Datos");
            viewResult.appendChild(chip);
        }

        const meta = res?.metadata || {};
        const width = Number(meta.width || 0);
        const height = Number(meta.height || 0);
        const megapixels = width && height ? ((width * height) / 1_000_000).toFixed(1) : "0.0";
        const tiles = Number(meta.frame_count || this.tiles.length || 0);
        const fileName = (res?.path || "").split(/[\\/]/).pop() || "Mosaic_Result.tiff";
        const modeLabel = mode === "guided"
            ? t("mosaic.info.mode_guided", "Guiado")
            : t("mosaic.info.mode_blind", "Automatico");
        const previewLabel = previewInfo?.native
            ? t("mosaic.info.preview_native", "Nativa RGB8")
            : t("mosaic.info.preview_embedded", "Embebida");

        card.style.cssText = [
            "position:fixed",
            "right:clamp(12px, 2vw, 22px)",
            "bottom:calc(env(safe-area-inset-bottom, 0px) + clamp(72px, 8vh, 110px))",
            "z-index:120",
            "width:min(340px, calc(100vw - 32px))",
            "max-height:min(46vh, 420px)",
            "overflow:auto",
            "background:rgba(15,23,42,0.92)",
            "border:1px solid rgba(56,189,248,0.45)",
            "box-shadow:0 18px 40px rgba(0,0,0,0.45)",
            "backdrop-filter:blur(12px)",
            "border-radius:8px",
            "padding:clamp(10px, 1.4vw, 14px)",
            "color:#e5eefb",
            "font-size:clamp(10px, 1.05vw, 12px)",
            "line-height:1.35",
            "pointer-events:auto"
        ].join(";");
        chip.style.cssText = [
            "position:fixed",
            "right:clamp(12px, 2vw, 22px)",
            "bottom:calc(env(safe-area-inset-bottom, 0px) + clamp(72px, 8vh, 110px))",
            "z-index:121",
            "display:none",
            "width:max-content",
            "min-width:78px",
            "max-width:160px",
            "inline-size:max-content",
            "background:rgba(15,23,42,0.9)",
            "border:1px solid rgba(56,189,248,0.55)",
            "color:#67e8f9",
            "border-radius:999px",
            "padding:8px 12px",
            "font-weight:800",
            "cursor:pointer",
            "align-items:center",
            "justify-content:center",
            "white-space:nowrap",
            "pointer-events:auto"
        ].join(";");

        card.innerHTML = `
            <div style="display:flex; align-items:center; justify-content:space-between; gap:12px; margin-bottom:10px;">
                <strong style="color:#67e8f9; letter-spacing:0.08em; text-transform:uppercase;">${t("mosaic.info.title", "Mosaico")}</strong>
                <button type="button" id="mosaic-info-hide" aria-label="Close" style="width:clamp(22px, 2.2vw, 28px); height:clamp(22px, 2.2vw, 28px); display:flex; align-items:center; justify-content:center; flex:0 0 auto; background:rgba(15,23,42,0.62); border:1px solid rgba(148,163,184,0.16); border-radius:8px; color:#cbd5e1; cursor:pointer; font-size:clamp(14px, 1.45vw, 18px); line-height:1; box-shadow:0 10px 24px rgba(15,23,42,0.32);">&times;</button>
            </div>
            <div style="display:grid; grid-template-columns:minmax(92px, auto) minmax(0, 1fr); gap:6px 14px;">
                <span style="color:#94a3b8;">${t("mosaic.info.resolution", "Resolucion")}</span><strong>${width.toLocaleString()} x ${height.toLocaleString()}</strong>
                <span style="color:#94a3b8;">${t("mosaic.info.megapixels", "Megapixeles")}</span><strong>${megapixels} MP</strong>
                <span style="color:#94a3b8;">${t("mosaic.info.tiles", "Teselas")}</span><strong>${tiles}</strong>
                <span style="color:#94a3b8;">${t("mosaic.info.mode", "Modo")}</span><strong>${modeLabel}</strong>
                <span style="color:#94a3b8;">${t("mosaic.info.preview", "Vista")}</span><strong>${previewLabel}</strong>
                <span style="color:#94a3b8;">${t("mosaic.info.export", "Exportacion")}</span><strong>PNG/TIFF 16-bit</strong>
                <span style="color:#94a3b8;">${t("mosaic.info.master", "Maestro")}</span><strong style="word-break:break-all;">${fileName}</strong>
            </div>
        `;

        card.querySelector("#mosaic-info-hide")?.addEventListener("click", () => {
            card.style.display = "none";
            chip.style.display = "inline-flex";
        });
        chip.onclick = () => {
            card.style.display = "block";
            chip.style.display = "none";
        };
    }

    redoMosaic() {
        // Reset View to Editing Mode
        console.log("MosaicManager: Redoing Mosaic...");
        if (window.setMosaicViewportMode) window.setMosaicViewportMode(false);
        document.getElementById("mosaic-info-card")?.remove();
        document.getElementById("mosaic-info-chip")?.remove();

        // Restore Overlay
        if (this.ui.canvasOverlay) {
            this.ui.canvasOverlay.style.display = 'block';
            this.ui.canvasOverlay.style.width = '100%';
        }

        // Hide Result View
        const viewResult = document.getElementById("view-result");
        if (viewResult) viewResult.style.display = 'none';

        // Show Source View Container Fully
        const viewSource = document.getElementById("view-source");
        if (viewSource) {
            viewSource.style.display = 'flex';
            viewSource.style.flex = '1';
        }

        const btnToggleSource = document.getElementById("btn-toggle-source");
        if (btnToggleSource) btnToggleSource.style.display = 'none';

        // Show Generate Step Again
        if (this.ui.stepGenerate) this.ui.stepGenerate.style.display = 'block';

        // Hide Result Actions
        const resultActions = document.getElementById("mosaic-result-actions");
        if (resultActions) resultActions.style.display = 'none';

        this.ui.btnGenerate.disabled = false;
        this.ui.btnGenerate.innerHTML = t("mosaic.generation.create_btn", "<svg class='zas-icon'><use href='#icon-mosaic'></use></svg> Crear Mosaico");

        // EXPLICIT SHOW of Mosaic Buttons
        const btnGen = document.getElementById("btn-gen-mosaic");
        const btnArr = document.getElementById("btn-arrange-mosaic");
        if (btnGen) btnGen.style.display = "block"; // Or flex/inline-block depending on CSS, but block usually fine for width:100% buttons
        if (btnArr) btnArr.style.display = "block";
    }

    sendToWavelets(path) {
        console.log("MosaicManager: Switching to Wavelets View...");
        const targetPath = path || window.getCurrentFilePath?.() || window.currentFilePath || "";
        if (targetPath) {
            if (window.setCurrentFilePath) {
                window.setCurrentFilePath(targetPath);
            } else {
                window.currentFilePath = targetPath;
            }
        }

        // Switch to Wavelets Panel with the "stitched" result
        const panelMosaic = document.getElementById("panel-mosaic");
        if (panelMosaic) panelMosaic.style.display = "none";
        document.getElementById("btn-mosaic-mode").classList.remove("active");

        // EXPLICIT HIDE of Mosaic Buttons (User Request)
        const btnGen = document.getElementById("btn-gen-mosaic");
        const btnArr = document.getElementById("btn-arrange-mosaic");
        if (btnGen) btnGen.style.display = "none";
        if (btnArr) btnArr.style.display = "none";

        const panelWavelets = document.getElementById("panel-wavelets");
        if (panelWavelets) panelWavelets.style.display = "block";

        // HIDE Deringing UI in Mosaic Flow transition
        const selDr = document.getElementById("sel-deringing-mode");
        if (selDr) {
            selDr.parentElement.style.display = "none";
            const panelDr = document.getElementById("panel-deringing-manual");
            if (panelDr) panelDr.style.display = "none";
        }

        // MANAGE VIEWPORT: Show Result, Hide Source
        const viewSource = document.getElementById("view-source");
        const viewResult = document.getElementById("view-result");
        const imgResult = document.getElementById("img-result");

        if (viewSource) {
            console.log("MosaicManager: Hiding Source View");
            // viewSource.style.display = "none"; // DEBUG: Keep it for now to see if it causes blankness?
            // Better: Just hide it.
            viewSource.style.display = "none";
        }
        if (window.setMosaicViewportMode) window.setMosaicViewportMode(false);

        if (viewResult) {
            console.log("MosaicManager: Showing Result View");
            viewResult.style.display = "flex";

            // Debug Image
            if (imgResult) {
                console.log("MosaicManager: Result Image Src Length:", imgResult.src ? imgResult.src.length : 0);
                if (!imgResult.src || imgResult.src.length < 100) {
                    console.warn("MosaicManager: Result Image seems empty!");
                }
                // Ensure image is visible in Wavelets mode too
                imgResult.style.display = 'block';
                requestAnimationFrame(() => {
                    if (window.fitResultToScreen) window.fitResultToScreen();
                    setTimeout(() => {
                        if (window.fitResultToScreen) window.fitResultToScreen();
                    }, 100);
                });
            }

            // Ensure crop box is hidden
            const cropBox = document.getElementById("crop-box");
            if (cropBox) cropBox.style.display = "none";
        } else {
            console.error("MosaicManager: FATAL - view-result not found!");
        }

        // UPDATE GLOBALS for Wavelets/Saving to work
        if (targetPath && window.setCurrentFilePath) window.setCurrentFilePath(targetPath);
        if (window.resetPipelineState) window.resetPipelineState();

        // Also hide overlay
        this.toggleOverlay(false);
    }

    toggleOverlay(show) {
        this.ui.canvasOverlay.style.display = show ? "block" : "none";
    }
}
