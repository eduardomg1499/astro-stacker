import { i18n } from './i18n.js';

export class TutorialManager {
    constructor() {
        this.enabled = true;
        this.seenFlows = []; // Array of strings: ['intro', 'individual', 'batch', 'mosaic']
        this.currentFlow = null;
        this.currentFlowName = null;
        this.currentStepIndex = 0;
        this.overlay = null;
        this.canRunPredicate = () => true;
        this.suppressStartupOnce = false;
        this._currentModalHandler = null;
        this._currentTargetHandler = null;
        this._isAdvancing = false;

        // Definition of Tutorial Flows
        this.flows = {
            intro: [
                { target: null, key: "step0", position: "center" },
                { target: "#btn-analyze", key: "step1", position: "right" },
                { target: "#btn-batch-mode", key: "step2", position: "right" },
                { target: "#btn-mosaic-mode", key: "step3", position: "right" },
                { target: "#btn-settings", key: "step4", position: "bottom" },
                { target: "#updater-badge", key: "step5", position: "bottom" }
            ],
            individual: [
                { target: "#btn-analyze", key: "step0", position: "right" },
                { target: "#container-target-category", key: "step1", position: "right", openOnRender: true, openTarget: "#trigger-target-category", scrollBlock: "center" },
                { target: "#btn-run-analysis", key: "step2", position: "right" },
                { target: "#processing-overlay", key: "step3", position: "center" },
                { target: "#panel-analysis", key: "step4", position: "right", scrollBlock: "center" },
                { target: "#stacking-quality-options", key: "step5", position: "right", scrollBlock: "center" },
                { target: "#multipoint-controls", key: "step6", position: "right", scrollBlock: "center" },
                { target: "#view-source", key: "step7", position: "right" },
                { target: "#btn-stack", key: "step8", position: "right" },
                { target: "#custom-modal-overlay .modal-box", key: "step9", position: "right", force: true },
                { target: "#view-result", key: "step10", position: "left" },
                { target: "#wavelets-signal-anchor", key: "step11", position: "bottom", prepare: "wavelets", scrollBlock: "start" },
                { target: "#save-actions", key: "step12", position: "right" }
            ],
            batch: [
                { target: "#btn-batch-mode", key: "step0", position: "right" },
                { target: "#panel-batch", key: "step1", position: "right" },
                { target: "#sel-batch-target-category", key: "step2", position: "right", openOnRender: true, scrollBlock: "center" },
                { target: "#btn-batch-tune", key: "step3", position: "right" },
                { target: "#btn-run-analysis", key: "step4", position: "right" },
                { target: "#panel-analysis", key: "step5", position: "right", scrollBlock: "center" },
                { target: "#stacking-quality-options", key: "step6", position: "right", scrollBlock: "center" },
                { target: "#multipoint-controls", key: "step7", position: "right", scrollBlock: "center" },
                { target: "#view-source", key: "step8", position: "right" },
                { target: "#btn-stack", key: "step9", position: "right" },
                { target: "#custom-modal-overlay .modal-box", key: "step10", position: "right", force: true },
                { target: "#wavelets-signal-anchor", key: "step11", position: "bottom", prepare: "wavelets", scrollBlock: "start" },
                { target: "#panel-batch", key: "step12", position: "right", scrollBlock: "center" },
                { target: "#btn-batch-run", key: "step13", position: "right" },
                { target: "#animation-modal", key: "step14", position: "left", force: true }
            ],
            mosaic: [
                { target: "#btn-mosaic-mode", key: "step0", position: "right" },
                { target: "#mosaic-dropzone", key: "step1", position: "left" },
                { target: "#btn-mosaic-auto", key: "step2", position: "left" },
                { target: "#view-source", key: "step3", position: "right" },
                { target: "#btn-mosaic-analyze-all", key: "step4", position: "left", onlyWhen: "mosaicVideo" },
                { target: "#mosaic-workflow-controls", key: "step5", position: "left", onlyWhen: "mosaicVideo", scrollBlock: "center" },
                { target: "#mosaic-step-generate", key: "step6", position: "left", scrollBlock: "center" },
                { target: "#mosaic-result-actions", key: "step7", position: "left", scrollBlock: "center" }
            ]
        };
    }

    init() {
        this.loadSettings();
        this.createOverlay();

        // Listen for language changes to update overlay UI
        window.addEventListener('languageChanged', () => {
            const skipBtn = document.getElementById('tut-btn-skip');
            const backBtn = document.getElementById('tut-btn-back');
            const nextBtn = document.getElementById('tut-btn-next');
            const hintEl = document.getElementById('tut-interactive-hint');
            if (skipBtn) skipBtn.textContent = i18n.t('tutorial.common.skip');
            if (backBtn) backBtn.textContent = i18n.t('tutorial.common.back');
            if (hintEl) hintEl.textContent = i18n.t('tutorial.common.interactive_hint');
            if (nextBtn) {
                // Update based on current step
                if (this.currentFlow && this.currentStepIndex === this.currentFlow.length - 1) {
                    nextBtn.textContent = i18n.t('tutorial.common.finish');
                } else {
                    nextBtn.textContent = i18n.t('tutorial.common.next');
                }
            }
        });

        console.log("TutorialManager Initialized");
    }

    loadSettings() {
        const savedEnabled = localStorage.getItem('tutorial_enabled');
        const savedSeen = localStorage.getItem('tutorial_seen_flows');

        this.enabled = savedEnabled !== null ? (savedEnabled === 'true') : true; // Default true
        try {
            this.seenFlows = savedSeen ? JSON.parse(savedSeen) : [];
            if (!Array.isArray(this.seenFlows)) this.seenFlows = [];
        } catch (e) {
            console.warn("Invalid tutorial settings, resetting seen flows:", e);
            this.seenFlows = [];
        }
        this.pendingReset = false;
        this.suppressStartupOnce = false;
    }

    saveSettings() {
        localStorage.setItem('tutorial_enabled', this.enabled);
        localStorage.setItem('tutorial_seen_flows', JSON.stringify(this.seenFlows));
    }

    setCanRunPredicate(predicate) {
        this.canRunPredicate = typeof predicate === 'function' ? predicate : () => true;
    }

    canRunTutorials() {
        try {
            return this.canRunPredicate();
        } catch (e) {
            console.warn("Tutorial gate failed:", e);
            return false;
        }
    }

    hasSeen(flowName) {
        return this.seenFlows.includes(flowName);
    }

    resetAll({ startIntro = false } = {}) {
        this.seenFlows = [];
        this.enabled = true;
        this.saveSettings();
        this.pendingReset = !!startIntro;
        this.suppressStartupOnce = !startIntro;
        console.log("Tutorials reset");
    }

    reset() {
        this.resetAll({ startIntro: true });
    }

    resetFlow(flowName) {
        this.seenFlows = this.seenFlows.filter(name => name !== flowName);
        this.enabled = true;
        this.saveSettings();
    }

    restartFlow(flowName, startIndex = 0) {
        if (!this.flows[flowName]) return;
        this.resetFlow(flowName);
        this.pendingReset = false;
        this.suppressStartupOnce = false;
        this.startFlow(flowName, startIndex, true);
    }

    toggle(state) {
        this.enabled = state;
        this.saveSettings();
    }

    checkStartup() {
        if (!this.enabled) return;
        if (!this.canRunTutorials()) return;
        if (this.suppressStartupOnce) {
            this.suppressStartupOnce = false;
            return;
        }

        // Check if intro has been seen
        if (!this.seenFlows.includes('intro')) {
            setTimeout(() => this.startFlow('intro'), 1000); // Slight delay for UI load
        }
    }

    checkPendingReset() {
        if (this.pendingReset && this.enabled) {
            if (!this.canRunTutorials()) return;
            this.pendingReset = false;
            setTimeout(() => this.startFlow('intro', 0, true), 500); // Force restart
        }
    }

    startFlow(flowName, startIndex = 0, force = false) {
        if (!this.enabled) return;
        if (!this.canRunTutorials()) return;
        if (!this.flows[flowName]) return;

        // Skip if already seen (unless forced from settings reset)
        if (!force && this.seenFlows.includes(flowName)) {
            console.log(`Tutorial '${flowName}' already completed, skipping.`);
            return;
        }

        const flow = this.flows[flowName];
        const safeStartIndex = Math.min(Math.max(0, startIndex), flow.length - 1);

        if (this.currentFlowName && this.currentFlowName !== flowName) {
            this.hideOverlay();
        }

        this.currentFlow = flow;
        this.currentStepIndex = safeStartIndex;
        this.currentFlowName = flowName; // Track name to save later

        this.showOverlay();
        this.renderStep();
    }

    createOverlay() {
        // Create full screen overlay div
        this.overlay = document.createElement('div');
        this.overlay.id = 'tutorial-overlay';
        this.overlay.innerHTML = `
            <div id="tutorial-highlight-box"></div>
            <div id="tutorial-tooltip">
                <h3 id="tut-title">Title</h3>
                <p id="tut-content">Content goes here.</p>
                <div id="tut-interactive-hint" style="font-size:0.78rem; line-height:1.35; color:#93c5fd; background:rgba(37,99,235,0.12); border:1px solid rgba(96,165,250,0.18); border-radius:8px; padding:8px 10px; margin-top:-6px;">
                    ${i18n.t('tutorial.common.interactive_hint')}
                </div>
                <div class="tut-footer" style="display: flex; justify-content: space-between; align-items: center; margin-top: 20px; border-top: 1px solid rgba(255,255,255,0.1); padding-top: 15px;">
                    <div style="display: flex; flex-direction: column; gap: 4px;">
                        <span id="tut-step-count" style="font-size: 0.75rem; color: #64748b; font-weight: 500;">Step 1/3</span>
                        <button id="tut-btn-skip">${i18n.t('tutorial.common.skip')}</button>
                    </div>
                    <div class="tut-actions" style="display: flex; gap: 12px; align-items: center;">
                        <button id="tut-btn-back">${i18n.t('tutorial.common.back')}</button>
                        <button id="tut-btn-next" class="primary">${i18n.t('tutorial.common.next')}</button>
                    </div>
                </div>
            </div>
        `;
        document.body.appendChild(this.overlay);

        // Bind events
        document.getElementById('tut-btn-next').addEventListener('click', () => this.nextStep());
        document.getElementById('tut-btn-back').addEventListener('click', () => this.prevStep());
        document.getElementById('tut-btn-skip').addEventListener('click', () => this.endTutorial(true));
    }

    showOverlay() {
        if (this.overlay) {
            this.overlay.classList.add('active');
            document.body.classList.add('tutorial-active');
        }
    }

    hideOverlay() {
        if (this.overlay) {
            this.overlay.classList.remove('active');
            document.body.classList.remove('tutorial-active');
        }
    }

    renderStep() {
        if (!this.currentFlow) return;
        this.clearStepHandlers();
        this.skipDisallowedSteps(1);
        if (!this.currentFlow || this.currentStepIndex >= this.currentFlow.length) {
            this.endTutorial(false);
            return;
        }

        const step = this.currentFlow[this.currentStepIndex];
        const highlightBox = document.getElementById('tutorial-highlight-box');
        const tooltip = document.getElementById('tutorial-tooltip');
        const titleEl = document.getElementById('tut-title');
        const contentEl = document.getElementById('tut-content');
        const countEl = document.getElementById('tut-step-count');
        const nextBtn = document.getElementById('tut-btn-next');
        const backBtn = document.getElementById('tut-btn-back');
        const hintEl = document.getElementById('tut-interactive-hint');

        // Text Content
        const titleKey = `tutorial.${this.currentFlowName}.${step.key}.title`;
        const contentKey = i18n.t(`tutorial.${this.currentFlowName}.${step.key}.content`);

        titleEl.textContent = i18n.t(titleKey);
        contentEl.innerHTML = contentKey;
        countEl.textContent = `${this.currentStepIndex + 1} / ${this.currentFlow.length}`;
        if (hintEl) hintEl.textContent = i18n.t('tutorial.common.interactive_hint');

        // Update Button States
        nextBtn.textContent = (this.currentStepIndex === this.currentFlow.length - 1) ? i18n.t('tutorial.common.finish') : i18n.t('tutorial.common.next');
        backBtn.style.display = (this.currentStepIndex === 0) ? 'none' : 'block';

        // Positioning
        if (step.target) {
            this.prepareStepLayout(step);
            this.waitForTarget(step.target, 25, step.scrollBlock || 'center').then(rect => {
                if (rect) {
                    // Highlight Box
                    highlightBox.style.display = 'block';
                    highlightBox.style.top = `${rect.top - 5}px`;
                    highlightBox.style.left = `${rect.left - 5}px`;
                    highlightBox.style.width = `${rect.width + 10}px`;
                    highlightBox.style.height = `${rect.height + 10}px`;

                    // Tooltip Positioning
                    this.positionTooltip(tooltip, rect, step.position, step.force);
                    this.openStepTarget(step);

                    // Auto-advance on modal button clicks
                    if (step.target && step.target.includes('modal')) {
                        this.bindModalAdvanceHandler();
                    }

                    if (step.advanceOnClick) {
                        this.bindTargetAdvanceHandler(step);
                    }
                } else {
                    highlightBox.style.display = 'none';
                    this.positionTooltip(tooltip, null, 'center');
                }
            });
        } else {
            highlightBox.style.display = 'none';
            this.positionTooltip(tooltip, null, 'center');
        }
    }

    /**
     * Waits for an element to be present and have non-zero dimensions.
     * @param {string} selector 
     * @param {number} retries 
     * @returns {Promise<DOMRect|null>}
     */
    async waitForTarget(selector, retries = 15, scrollBlock = 'center') {
        for (let i = 0; i < retries; i++) {
            const el = document.querySelector(selector);
            if (el) {
                const rect = el.getBoundingClientRect();
                if (rect.width > 0 && rect.height > 0) {
                    el.scrollIntoView({ behavior: 'auto', block: scrollBlock, inline: 'nearest' });
                    // Small delay to ensure scroll and layout settled
                    await new Promise(r => setTimeout(r, 100));
                    return el.getBoundingClientRect();
                }
            }
            await new Promise(r => setTimeout(r, 200));
        }
        return null;
    }

    clearStepHandlers() {
        if (this._currentModalHandler) {
            document.removeEventListener('click', this._currentModalHandler, { capture: true });
            this._currentModalHandler = null;
        }
        if (this._currentTargetHandler) {
            document.removeEventListener('click', this._currentTargetHandler, { capture: true });
            this._currentTargetHandler = null;
        }
        this._isAdvancing = false;
    }

    bindModalAdvanceHandler() {
        const modalClickHandler = (e) => {
            const target = e.target;
            const isButton = target.tagName === 'BUTTON' || target.closest('button');
            const isChoiceCard = target.closest('.choice-card');
            const isInModal = target.closest('.modal-box, .modal-content, [class*="modal"]');

            if ((isButton || isChoiceCard) && isInModal) {
                if (this._isAdvancing) return;
                this._isAdvancing = true;

                setTimeout(() => {
                    this.nextStep();
                }, 200);
            }
        };
        document.addEventListener('click', modalClickHandler, { capture: true });
        this._currentModalHandler = modalClickHandler;
    }

    bindTargetAdvanceHandler(step) {
        const targetClickHandler = (e) => {
            const el = document.querySelector(step.target);
            if (!el || !(e.target === el || el.contains(e.target))) return;
            if (this._isAdvancing) return;

            this._isAdvancing = true;
            setTimeout(() => {
                this.nextStep();
            }, step.advanceDelay || 350);
        };
        document.addEventListener('click', targetClickHandler, { capture: true });
        this._currentTargetHandler = targetClickHandler;
    }

    openStepTarget(step) {
        if (!step.openOnRender) return;
        const selector = step.openTarget || step.target;
        const el = document.querySelector(selector);
        if (!el) return;

        setTimeout(() => {
            try {
                el.focus?.({ preventScroll: true });
                const customOptions = el.closest('.custom-select-container')?.querySelector('.custom-select-options');
                if (customOptions && !customOptions.classList.contains('open')) {
                    el.click();
                    return;
                }
                if (el.classList.contains('custom-select-trigger')) {
                    el.click();
                    return;
                }
                if (el.tagName === 'SELECT') {
                    el.click();
                }
            } catch (e) {
                console.warn("Tutorial target open failed:", e);
            }
        }, 250);
    }

    prepareStepLayout(step) {
        if (step?.prepare !== 'wavelets') return;

        const panel = document.getElementById('panel-wavelets');
        const anchor = document.getElementById('wavelets-signal-anchor') || panel;
        const resultView = document.getElementById('view-result');
        const resultImg = document.getElementById('img-result');

        if (resultView) {
            resultView.style.display = 'flex';
            resultView.style.flex = resultView.style.flex || '1';
        }
        if (resultImg && resultImg.getAttribute('src')) {
            resultImg.style.display = 'block';
        }

        if (!panel || !anchor) return;

        const sidebar = panel.closest('.sidebar');
        if (sidebar) {
            const sidebarRect = sidebar.getBoundingClientRect();
            const anchorRect = anchor.getBoundingClientRect();
            const nextTop = sidebar.scrollTop + (anchorRect.top - sidebarRect.top) - 24;
            sidebar.scrollTo({ top: Math.max(0, nextTop), behavior: 'auto' });
            return;
        }

        anchor.scrollIntoView({ behavior: 'auto', block: 'start', inline: 'nearest' });
    }

    isStepAllowed(step) {
        if (!step?.onlyWhen) return true;
        if (step.onlyWhen === 'mosaicVideo') return !!window.__mosaicTutorialHasVideo;
        if (step.onlyWhen === 'mosaicImage') return window.__mosaicTutorialHasVideo === false;
        return true;
    }

    skipDisallowedSteps(direction = 1) {
        if (!this.currentFlow) return;
        while (this.currentStepIndex >= 0 && this.currentStepIndex < this.currentFlow.length) {
            const step = this.currentFlow[this.currentStepIndex];
            if (this.isStepAllowed(step)) return;
            this.currentStepIndex += direction;
        }
        if (this.currentStepIndex < 0) this.currentStepIndex = 0;
    }

    positionTooltip(tooltip, targetRect, preferredPos, force = false) {
        // Reset classes
        tooltip.className = '';

        if (!targetRect || preferredPos === 'center') {
            tooltip.style.top = '50%';
            tooltip.style.left = '50%';
            tooltip.style.transform = 'translate(-50%, -50%)';
            return;
        }

        const margin = 20;
        const tipW = tooltip.offsetWidth || 360;
        const tipH = tooltip.offsetHeight || 260;
        const viewportW = window.innerWidth;
        const viewportH = window.innerHeight;

        let top, left;
        let actualPos = preferredPos;

        switch (preferredPos) {
            case 'right':
                // Align to the right
                top = targetRect.top + (targetRect.height / 2) - (tipH / 2);
                left = targetRect.right + margin;

                // Check if tooltip would overflow right edge
                if (left + tipW > viewportW - 15) {
                    if (force) {
                        // Force it to stay on right, even if it cuts off a bit or we shift it left
                        // BUT we shift it left only up to the target's edge
                        left = viewportW - tipW - 15;
                        if (left < targetRect.right + 5) left = targetRect.right + 5;
                    } else {
                        // Try to flip to left
                        if (targetRect.left - tipW - margin > 15) {
                            actualPos = 'left';
                            left = targetRect.left - tipW - margin;
                        } else {
                            // If no space on left either, keep it on right but shift left UNTIL it touches the target edges
                            actualPos = 'right';
                            left = viewportW - tipW - 15;
                            // Ensure we don't overlap the highlight box if possible
                            if (left < targetRect.right + 5) {
                                // If it would overlap, fall back to bottom
                                actualPos = 'bottom';
                                top = targetRect.bottom + margin;
                                left = targetRect.left + (targetRect.width / 2) - (tipW / 2);
                            }
                        }
                    }
                }
                break;
            case 'left':
                top = targetRect.top + (targetRect.height / 2) - (tipH / 2);
                left = targetRect.left - tipW - margin;

                // Check if tooltip would overflow left edge
                if (left < 10) {
                    // Flip to right
                    actualPos = 'right';
                    left = targetRect.right + margin;
                    top = targetRect.top - 10;
                }
                break;
            case 'bottom':
                top = targetRect.bottom + margin;
                left = targetRect.left + (targetRect.width / 2) - (tipW / 2);

                // Check if tooltip would overflow bottom edge
                if (top + tipH > viewportH - 10) {
                    actualPos = 'top';
                    top = targetRect.top - tipH - margin;
                }
                break;
            case 'top':
                top = targetRect.top - tipH - margin;
                left = targetRect.left + (targetRect.width / 2) - (tipW / 2);

                // Check if tooltip would overflow top edge
                if (top < 10) {
                    actualPos = 'bottom';
                    top = targetRect.bottom + margin;
                }
                break;
            default:
                top = 50;
                left = 50;
        }

        // Final boundary checks (clamp to viewport)
        if (left < 10) left = 10;
        if (top < 10) top = 10;
        if (left + tipW > viewportW - 10) left = viewportW - tipW - 10;
        if (top + tipH > viewportH - 10) top = viewportH - tipH - 10;

        tooltip.style.transform = 'none';
        tooltip.style.top = `${top}px`;
        tooltip.style.left = `${left}px`;
    }

    nextStep() {
        if (!this.currentFlow) return;

        this.currentStepIndex++;
        this.skipDisallowedSteps(1);
        if (this.currentStepIndex >= this.currentFlow.length) {
            this.endTutorial(false);
        } else {
            this.renderStep();
        }
    }

    jumpToStep(index) {
        if (!this.currentFlow) return;
        if (index < 0 || index >= this.currentFlow.length) return;

        this.currentStepIndex = index;
        this.renderStep();
    }

    prevStep() {
        if (!this.currentFlow || this.currentStepIndex <= 0) return;

        this.currentStepIndex--;
        this.skipDisallowedSteps(-1);
        this.renderStep();
    }

    endTutorial(skipped = false) {
        this.clearStepHandlers();
        this.hideOverlay();

        // Always mark as seen if it ended (either finished or skipped)
        if (this.currentFlowName && !this.seenFlows.includes(this.currentFlowName)) {
            this.seenFlows.push(this.currentFlowName);
            this.saveSettings();
        }

        this.currentFlow = null;
        this.currentFlowName = null;
        this.currentStepIndex = 0;
    }
}

export const tutorialManager = new TutorialManager();
