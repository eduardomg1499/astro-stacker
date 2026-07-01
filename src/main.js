import "./styles.css";
import { MosaicManager } from "./mosaic_manager.js";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-shell"; // CORRECT IMPORT
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog"; // Renamed to avoid conflict
import { mkdir } from "@tauri-apps/plugin-fs";
import { listen } from "@tauri-apps/api/event";
import { check } from '@tauri-apps/plugin-updater';
import { relaunch } from '@tauri-apps/plugin-process';
import { getVersion } from '@tauri-apps/api/app';
import { i18n } from "./i18n.js";
import { tutorialManager } from "./tutorial_manager.js";
import { getCurrentWindow, LogicalSize } from '@tauri-apps/api/window';

window.mkdir = mkdir;

let appWindow = null;
try {
    appWindow = getCurrentWindow();
} catch (err) {
    console.warn("Tauri window API unavailable in this runtime:", err);
}
const $ = (selector) => document.querySelector(selector);
const $$ = (selector) => document.querySelectorAll(selector);

function normalizeBackendText(value) {
    if (value === null || value === undefined) return "";

    return String(value)
        .replace(/Ã¡/g, "á")
        .replace(/Ã©/g, "é")
        .replace(/Ã­/g, "í")
        .replace(/Ã³/g, "ó")
        .replace(/Ãº/g, "ú")
        .replace(/Ã±/g, "ñ")
        .replace(/Ã¼/g, "ü")
        .replace(/Â¿/g, "¿")
        .replace(/Â¡/g, "¡")
        .replace(/Â°/g, "°")
        .replace(/Â/g, "");
}

function tr(key, fallback = "") {
    const value = i18n?.t?.(key);
    return value && value !== key ? value : fallback;
}

function trFormat(key, values = {}, fallback = "") {
    let text = tr(key, fallback);
    Object.entries(values).forEach(([name, value]) => {
        text = text.split(`{{${name}}}`).join(String(value));
    });
    return text;
}

function pathBaseName(value) {
    const text = String(value || "");
    return text.split(/[\\/]/).pop() || text;
}

function translateBackendProgressText(value) {
    const text = normalizeBackendText(value);
    if (i18n?.currentLang !== "en") return text;

    let out = text
        .replace(/Procesando Frame/g, "Processing frame")
        .replace(/Procesando frame/g, "Processing frame")
        .replace(/Cargando Lote/g, "Loading batch")
        .replace(/Generando Referencia Maestra/g, "Generating master reference")
        .replace(/Creando Referencia Low-Noise \(Doble Pasada\)/g, "Creating low-noise reference (double pass)")
        .replace(/Alta Precision/g, "high precision")
        .replace(/Iniciando Acumulacion Robusta/g, "Starting robust accumulation")
        .replace(/Doble Pasada: regenerando referencia desde el apilado/g, "Double pass: rebuilding reference from the stack")
        .replace(/Estimacion de PSF y Deconvolucion TV-RL/g, "PSF estimation and TV-RL deconvolution")
        .replace(/Eliminando pixeles calientes y ruido residual/g, "Removing hot pixels and residual noise")
        .replace(/Aplicando Sharpening \(Wavelet Multi-Band\)/g, "Applying sharpening (multi-band wavelet)")
        .replace(/Guardando resultado/g, "Saving result")
        .replace(/Completado/g, "Completed")
        .replace(/Listo/g, "Done");

    out = out.replace(
        /Modo Dinámico: ([^ ]+)GB RAM Libres -> Lotes de (\d+) frames \((\d+)\/(\d+) total\)/,
        "Dynamic mode: $1GB free RAM -> chunks of $2 frames ($3/$4 total)"
    );

    return out;
}

function isMacPlatform() {
    const platform = window.navigator.platform || "";
    const userAgent = window.navigator.userAgent || "";
    return /Mac|iPhone|iPad|iPod/i.test(platform) || /Mac OS|Macintosh/i.test(userAgent);
}

function applyWindowChromePlatform() {
    if (!document.body) return;
    document.body.classList.toggle("platform-macos", isMacPlatform());
}

async function enableNativeMacTitlebarIfAvailable() {
    if (!document.body || !isMacPlatform() || !appWindow) return;
    if (typeof appWindow.setDecorations !== 'function' || typeof appWindow.setTitleBarStyle !== 'function') return;

    try {
        await appWindow.setDecorations(true);
        await appWindow.setTitleBarStyle('Overlay');
        if (typeof appWindow.setTitle === 'function') {
            await appWindow.setTitle("");
        }
        document.body.classList.add("native-macos-titlebar");
    } catch (err) {
        document.body.classList.remove("native-macos-titlebar");
        try {
            await appWindow.setDecorations(false);
        } catch (_) {}
        console.warn("Native macOS titlebar unavailable, using custom fallback:", err);
    }
}

// REVEAL AS SOON AS MODULE PARSES
// Note: Permission 'core:window:allow-show' is required for this to work in Tauri v2
if (appWindow && typeof appWindow.show === 'function') {
    // Small delay ensures first paint and eliminates the "invisible frame" flicker
    setTimeout(() => {
        appWindow.show().catch(e => console.error("Tauri reveal failed:", e));
    }, 10);
}

// Custom Titlebar Logic
function initTitlebar() {
    try {
        applyWindowChromePlatform();
        enableNativeMacTitlebarIfAvailable();

        const btnMinimize = document.getElementById('titlebar-minimize');
        const btnMaximize = document.getElementById('titlebar-maximize');
        const btnClose = document.getElementById('titlebar-close');
        const runWindowAction = (actionName) => {
            if (!appWindow || typeof appWindow[actionName] !== 'function') return;
            appWindow[actionName]().catch(e => console.error(e));
        };

        if (btnMinimize) {
            btnMinimize.addEventListener('click', () => runWindowAction('minimize'));
        }
        if (btnMaximize) {
            btnMaximize.addEventListener('click', () => runWindowAction('toggleMaximize'));
        }
        if (btnClose) {
            btnClose.addEventListener('click', () => runWindowAction('close'));
        }

        // JS Draggable Fallback
        const titlebar = document.querySelector('.custom-titlebar');
        if (titlebar) {
            titlebar.addEventListener('mousedown', (e) => {
                // Ignore clicks on window controls
                if (e.target.closest('.titlebar-btn')) return;
                
                if (e.buttons === 1) { // Left click only
                    if (!appWindow || typeof appWindow.startDragging !== 'function') return;
                    appWindow.startDragging().catch(e => console.error("Drag error:", e));
                }
            });
        }
    } catch (err) {
        console.error("Error wiring up Tauri window controls:", err);
    }
}

function initCustomSelectBox(selectId, triggerId, optionsId, textId) {
    const trigger = document.getElementById(triggerId);
    const options = document.getElementById(optionsId);
    const hiddenSelect = document.getElementById(selectId);
    const textSpan = document.getElementById(textId);
    
    if (!trigger || !options || !hiddenSelect) return;
    const iconContainer = trigger.querySelector('.custom-select-selected-icon');

    // Toggle dropdown
    trigger.addEventListener('click', (e) => {
        e.stopPropagation();
        trigger.classList.toggle('active');
        options.classList.toggle('open');
    });

    // Handle option click
    const optionEls = options.querySelectorAll('.custom-option');
    optionEls.forEach(opt => {
        opt.addEventListener('click', (e) => {
            e.stopPropagation();
            const value = opt.getAttribute('data-value');
            
            // Update UI
            optionEls.forEach(o => o.classList.remove('selected'));
            opt.classList.add('selected');
            if (textSpan) textSpan.textContent = opt.querySelector('span').textContent;
            
            // Clone icon
            if (iconContainer) {
                const svgIcon = opt.querySelector('svg');
                if (svgIcon) {
                    const clone = svgIcon.cloneNode(true);
                    iconContainer.innerHTML = '';
                    iconContainer.appendChild(clone);
                }
            }

            // Update underlying select & trigger change if not identical
            if (hiddenSelect.value !== value) {
                hiddenSelect.value = value;
                hiddenSelect.dispatchEvent(new Event('change'));
            }

            // Close dropdown
            trigger.classList.remove('active');
            options.classList.remove('open');
        });
    });

    // Close when clicking outside
    document.addEventListener('click', (e) => {
        if (!trigger.contains(e.target) && !options.contains(e.target)) {
            trigger.classList.remove('active');
            options.classList.remove('open');
        }
    });

    // Sync initial state if needed
    hiddenSelect.addEventListener('change', () => {
        const val = hiddenSelect.value;
        const matchingOpt = options.querySelector(`.custom-option[data-value="${val}"]`);
        if (matchingOpt && !matchingOpt.classList.contains('selected')) {
            // Update UI
            optionEls.forEach(o => o.classList.remove('selected'));
            matchingOpt.classList.add('selected');
            if (textSpan) textSpan.textContent = matchingOpt.querySelector('span').textContent;
            
            if (iconContainer) {
                const svgIcon = matchingOpt.querySelector('svg');
                if (svgIcon) {
                    const clone = svgIcon.cloneNode(true);
                    iconContainer.innerHTML = '';
                    iconContainer.appendChild(clone);
                }
            }
        }
    });
}

function initCustomSelect() {
    initCustomSelectBox('sel-quality-method', 'custom-quality-trigger', 'custom-quality-options', 'custom-quality-text');
    initCustomSelectBox('settings-lang-select', 'custom-language-trigger', 'custom-language-options', 'custom-language-text');
    initCustomSelectBox('sel-target-category', 'trigger-target-category', 'options-target-category', 'text-target-category');
}

function normalizeLanguageCode(lang) {
    return lang === "en" ? "en" : "es";
}

function syncActivationLanguageButtons(lang) {
    const activeLang = normalizeLanguageCode(lang || i18n.currentLang);
    document.querySelectorAll(".license-lang-btn[data-lang]").forEach((btn) => {
        const isActive = btn.dataset.lang === activeLang;
        btn.classList.toggle("active", isActive);
        btn.setAttribute("aria-pressed", String(isActive));
    });
}

function initHeaderSupportButtons() {
    const socialWrapper = $("#social-links-wrapper");
    const btnSocial = $("#btn-social-links");
    const socialMenu = $("#social-links-menu");
    const btnDonation = $("#btn-donation");
    const donationModal = $("#donation-modal");
    const btnDonationClose = $("#donation-modal-close");

    const setSocialOpen = (openMenu) => {
        if (!socialMenu || !btnSocial) return;
        socialMenu.classList.toggle("open", openMenu);
        socialMenu.setAttribute("aria-hidden", String(!openMenu));
        btnSocial.classList.toggle("active", openMenu);
    };

    const openDonationModal = () => {
        if (!donationModal) return;
        donationModal.style.display = "flex";
        setSocialOpen(false);
    };

    const closeDonationModal = () => {
        if (donationModal) donationModal.style.display = "none";
    };

    const openSupportUrl = async (url) => {
        if (!url) return;
        setSocialOpen(false);
        closeDonationModal();
        if (typeof window.openBrowser === "function") {
            await window.openBrowser(url);
            return;
        }
        try {
            await open(url);
        } catch (e) {
            console.warn("Tauri Shell Open fallo, intentando window.open:", e);
            window.open(url, "_blank");
        }
    };

    if (btnSocial && socialMenu) {
        btnSocial.addEventListener("click", (event) => {
            event.stopPropagation();
            setSocialOpen(!socialMenu.classList.contains("open"));
        });
    }

    document.querySelectorAll("[data-social-url]").forEach((btn) => {
        btn.addEventListener("click", () => openSupportUrl(btn.dataset.socialUrl));
    });

    if (btnDonation) {
        btnDonation.addEventListener("click", openDonationModal);
    }

    if (btnDonationClose) {
        btnDonationClose.addEventListener("click", closeDonationModal);
    }

    if (donationModal) {
        donationModal.addEventListener("click", (event) => {
            if (event.target === donationModal) closeDonationModal();
        });
    }

    document.querySelectorAll("[data-donation-url]").forEach((btn) => {
        btn.addEventListener("click", () => openSupportUrl(btn.dataset.donationUrl));
    });

    document.addEventListener("click", (event) => {
        if (socialWrapper && !socialWrapper.contains(event.target)) {
            setSocialOpen(false);
        }
    });

    document.addEventListener("keydown", (event) => {
        if (event.key === "Escape") {
            setSocialOpen(false);
            closeDonationModal();
        }
    });
}

if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', () => {
        initTitlebar();
        initCustomSelect();
        initHeaderSupportButtons();
    });
} else {
    initTitlebar();
    initCustomSelect();
    initHeaderSupportButtons();
}


function debounce(func, wait) {
    let timeout;
    return function executedFunction(...args) {
        const later = () => {
            clearTimeout(timeout);
            func(...args);
        };
        clearTimeout(timeout);
        timeout = setTimeout(later, wait);
    };
}

// Desactivar clic derecho (menu contextual de navegador)
document.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    return false;
}, { passive: false });

// Desactivar atajos de desarrollador (F12, Ctrl+Shift+I, etc.)
document.addEventListener('keydown', function (event) {
    // F12
    if (event.key === 'F12' || event.keyCode === 123) {
        event.preventDefault();
        return false;
    }

    // Ctrl+Shift+I (Inspect), Ctrl+Shift+J (Console), Ctrl+Shift+C (Element Inspector)
    if (event.ctrlKey && event.shiftKey && (event.key === 'I' || event.key === 'J' || event.key === 'C' || event.keyCode === 73 || event.keyCode === 74 || event.keyCode === 67)) {
        event.preventDefault();
        return false;
    }

    // Ctrl+U (View Source)
    if (event.ctrlKey && (event.key === 'U' || event.keyCode === 85)) {
        event.preventDefault();
        return false;
    }

    // Ctrl+R, F5 (Prevenir Recargas accidentales)
    if ((event.ctrlKey && (event.key === 'R' || event.keyCode === 82)) || event.key === 'F5' || event.keyCode === 116) {
        event.preventDefault();
        return false;
    }
});

// =========================================================================
// GESTION DE LICENCIA (NUEVO)
// =========================================================================

let isProVersion = false;
let licenseStatus = "UNKNOWN"; // UNKNOWN, LOCKED, TRIAL, ANNUAL, PRO, FREE, EXPIRED
let licenseCheckComplete = false;
let tutorialsInitialized = false;
let lastLicenseInfo = null;
const ACTIVE_LICENSE_STATUSES = ["PRO", "ANNUAL", "TRIAL", "FREE"];

function escapeHtml(value) {
    const map = {
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&#39;"
    };
    return String(value ?? "").replace(/[&<>"']/g, char => map[char]);
}

function getLicenseStatusColor(status) {
    if (status === "PRO" || status === "ANNUAL") return "#10b981";
    if (status === "TRIAL") return "#fcd34d";
    if (status === "FREE") return "#38bdf8";
    if (status === "EXPIRED" || status === "LOCKED") return "#ef4444";
    return "#94a3b8";
}

function getLicenseStatusLabel(status) {
    const normalized = String(status || "UNKNOWN").toLowerCase();
    return tr(`license.statuses.${normalized}`, status || tr("license.statuses.unknown", "DESCONOCIDA"));
}

function valueOrFallback(value, fallback = tr("general.unavailable", "No disponible")) {
    if (value === null || value === undefined || value === "") return fallback;
    return value;
}

function getCanonicalLicenseType(value, status) {
    const raw = normalizeBackendText(value || "").toLowerCase();
    if (raw.includes("trial") || raw.includes("prueba")) return "trial";
    if (raw.includes("free") || raw.includes("gratuita") || raw.includes("gratuito")) return "free";
    if (raw.includes("annual") || raw.includes("anual") || raw.includes("yearly")) return "annual";
    if (raw.includes("pro")) return "pro";

    const statusType = String(status || "").toLowerCase();
    if (["trial", "free", "annual", "pro"].includes(statusType)) return statusType;
    return "license";
}

function getLicenseTypeLabel(info) {
    const type = getCanonicalLicenseType(info?.license_type, info?.status);
    return tr(`license.types.${type}`, valueOrFallback(info?.license_type, tr("license.types.license", "Licencia")));
}

function licenseDaysLabel(info) {
    if (info.days_remaining > 9000) return tr("general.not_applicable", "No aplica");
    return `${Math.max(0, info.days_remaining ?? 0)}`;
}

function getLicenseMessage(info) {
    const status = String(info?.status || "UNKNOWN").toUpperCase();
    const type = getCanonicalLicenseType(info?.license_type, status);
    const days = Math.max(0, info?.days_remaining ?? 0);
    const expiresAt = valueOrFallback(info?.expires_at, "");

    if (status === "LOCKED") return tr("license.messages.activation_required", "Se requiere activación.");
    if (status === "TRIAL") return trFormat("license.messages.trial_active", { days }, `Prueba gratuita activa. Quedan ${days} días.`);
    if (status === "FREE") return trFormat("license.messages.free_active", { days }, `Licencia gratuita activa. Quedan ${days} días con funciones limitadas.`);
    if (status === "ANNUAL") {
        return expiresAt
            ? trFormat("license.messages.annual_active_until", { date: expiresAt }, `Licencia anual activa hasta ${expiresAt}.`)
            : tr("license.messages.annual_active", "Licencia anual activa.");
    }
    if (status === "PRO") return tr("license.messages.pro_active", "Licencia PRO activa.");
    if (status === "EXPIRED") {
        if (type === "trial") return tr("license.messages.trial_finished", "La prueba gratuita ha finalizado.");
        if (type === "free") return tr("license.messages.free_expired", "La licencia gratuita ha expirado.");
        if (type === "annual") return tr("license.messages.annual_expired", "La licencia anual ha expirado o no fue renovada.");
    }

    return normalizeBackendText(valueOrFallback(info?.message, tr("license.fallbacks.no_message", "Sin mensaje")));
}

function getLicenseRenewalLabel(info) {
    const value = normalizeBackendText(info?.renewal_status);
    if (!value) return "";

    const dateMatch = value.match(/(\d{4}-\d{2}-\d{2}(?:\s+\d{2}:\d{2}\s+UTC)?)/);
    if (dateMatch && value.toLowerCase().includes("verific")) {
        return trFormat("license.renewal.verified_on", { date: dateMatch[1] }, value);
    }
    if (value.toLowerCase().includes("expir")) return tr("license.renewal.expired", value);
    if (value.toLowerCase().includes("no renov")) return tr("license.renewal.not_current", value);
    if (value.toLowerCase().includes("verific")) return tr("license.renewal.verified", value);
    return value;
}

function getLicenseBadgeText(info) {
    const status = String(info?.status || licenseStatus || "UNKNOWN").toUpperCase();
    const days = Math.max(0, info?.days_remaining ?? 0);
    if (status === "PRO") return tr("license.badge.pro", "PRO VERSION");
    if (status === "ANNUAL") return days < 9000
        ? trFormat("license.badge.annual_days", { days }, `ANUAL (${days} DIAS)`)
        : tr("license.badge.annual", "LICENCIA ANUAL");
    if (status === "FREE") return tr("license.badge.free", "LICENCIA GRATUITA");
    if (status === "TRIAL") return trFormat("license.badge.trial_days", { days }, `PRUEBA (${days} DIAS)`);
    if (status === "LOCKED") return tr("license.badge.required", "LICENCIA REQUERIDA");
    return tr("license.badge.expired", "EXPIRADA");
}

function renderLicenseInfoBox(info, container) {
    if (!container) return;
    lastLicenseInfo = info;

    const statusColor = getLicenseStatusColor(info.status);
    const expiryFallback = info.status === "PRO" ? tr("general.not_applicable", "No aplica") : tr("general.unavailable", "No disponible");
    const rows = [
        ["status", `<span style="color:${statusColor}">${escapeHtml(getLicenseStatusLabel(info.status))}</span>`],
        ["license_type", escapeHtml(getLicenseTypeLabel(info))],
        ["license_key", escapeHtml(valueOrFallback(info.license_key, tr("license.fallbacks.no_registered_license", "Sin licencia registrada")))],
        ["registered_at", escapeHtml(valueOrFallback(info.registered_at))],
        ["expires_at", escapeHtml(valueOrFallback(info.expires_at, expiryFallback))],
        ["days_remaining", escapeHtml(licenseDaysLabel(info))],
        ["last_checked_at", escapeHtml(valueOrFallback(info.last_checked_at))],
    ];

    if (info.renewal_status) {
        rows.push(["renewal", escapeHtml(getLicenseRenewalLabel(info))]);
    }

    rows.push(["message", escapeHtml(getLicenseMessage(info))]);

    container.innerHTML = rows.map(([key, value]) => `
        <div class="license-row${key === "message" || key === "renewal" ? " license-message" : ""}">
            <span>${escapeHtml(tr(`license.rows.${key}`, key))}</span>
            <span>${value}</span>
        </div>
    `).join("");
}

function configureSettingsLicenseButtons(info) {
    const btnActivate = $("#btn-settings-activate");
    const btnDeactivate = $("#btn-settings-deactivate");
    if (!btnActivate || !btnDeactivate) return;

    btnActivate.style.display = "none";
    btnDeactivate.style.display = "inline-block";

    if (["PRO", "ANNUAL"].includes(info.status)) {
        btnDeactivate.textContent = tr("license.actions.change", "Cambiar Licencia");
        btnDeactivate.className = "primary";
        btnDeactivate.style.background = "#3b82f6";
        btnDeactivate.style.boxShadow = "none";
        btnDeactivate.style.fontWeight = "bold";
        btnDeactivate.onclick = () => window.showActivationModal();
        return;
    }

    btnDeactivate.textContent = tr("license.actions.upgrade_annual", "MEJORAR A LICENCIA ANUAL");
    btnDeactivate.className = "success";
    btnDeactivate.style.background = "linear-gradient(135deg, #f59e0b, #d97706)";
    btnDeactivate.style.boxShadow = "0 4px 15px rgba(245, 158, 11, 0.4)";
    btnDeactivate.style.fontWeight = "bold";
    btnDeactivate.onclick = () => {
        $("#settings-modal").style.display = "none";
        window.showActivationModal();
    };
}

function canRunTutorialsAfterLicense() {
    return licenseCheckComplete && ACTIVE_LICENSE_STATUSES.includes(licenseStatus);
}

window.canRunTutorialsAfterLicense = canRunTutorialsAfterLicense;

function isModalVisibleById(id) {
    const el = document.getElementById(id);
    return !!el && window.getComputedStyle(el).display !== "none";
}

function canPresentTutorialOverlay() {
    return !isModalVisibleById("license-modal")
        && !isModalVisibleById("activation-modal")
        && !isModalVisibleById("settings-modal")
        && !isModalVisibleById("processing-overlay");
}

function scheduleTutorialAfterLicense(delay = 500) {
    if (!tutorialsInitialized || !canRunTutorialsAfterLicense()) return;
    if (!canPresentTutorialOverlay()) return;

    setTimeout(() => {
        if (!tutorialsInitialized || !canRunTutorialsAfterLicense()) return;
        if (!canPresentTutorialOverlay()) return;

        if (tutorialManager.pendingReset) {
            tutorialManager.checkPendingReset();
        } else {
            tutorialManager.checkStartup();
        }
    }, delay);
}

function maybeStartTutorialFlow(flowName, startIndex = 0, delay = 500, force = false, retries = 6) {
    if (!tutorialsInitialized || !canRunTutorialsAfterLicense()) return;
    if (!force && tutorialManager.hasSeen?.(flowName)) return;

    setTimeout(() => {
        if (!tutorialsInitialized || !canRunTutorialsAfterLicense()) return;
        if (!canPresentTutorialOverlay()) {
            if (retries > 0) maybeStartTutorialFlow(flowName, startIndex, 300, force, retries - 1);
            return;
        }
        tutorialManager.startFlow(flowName, startIndex, force);
    }, delay);
}

function restartTutorialFlowFromSettings(flowName, startIndex = 0) {
    if (!tutorialsInitialized || !canRunTutorialsAfterLicense()) return;
    if (settingsModal) settingsModal.style.display = "none";
    setTimeout(() => tutorialManager.restartFlow(flowName, startIndex), 180);
}

async function checkLicenseAtStartup() {
    try {
        const info = await invoke("check_license_status");
        isProVersion = info.is_pro; // Backend devuelve TRUE si hay prueba vigente o licencia anual/PRO activa
        licenseStatus = info.status;
        licenseCheckComplete = true;
        lastLicenseInfo = info;

        console.log("License Info:", info);
        updateProUI(info);

        if (licenseStatus === "EXPIRED" || licenseStatus === "LOCKED") {
            showLicenseModal(false); // Force open, NO upgrade mode
        }
        // Logic Update: If TRIAL, ANNUAL or PRO, we do NOT show the modal at startup automatically.
        // It should be silent. The user can open it from settings if they want to upgrade.
        // This prevents the "First Use" window from annoying users after updates.
        else if (licenseStatus === "TRIAL") {
            console.log("Trial Active - Silent Startup");
        } else if (licenseStatus === "ANNUAL") {
            console.log("Annual License Active - Silent Startup");
        }
        scheduleTutorialAfterLicense();
    } catch (e) {
        licenseCheckComplete = true;
        console.error("Error checando licencia:", e);
        showCustomAlert("Error", "No se pudo verificar la licencia. La aplicación continuará con acceso limitado.");
    }
}

function updateProUI(info) {
    lastLicenseInfo = info;
    const badge = $("#license-indicator");
    const proTags = $$(".pro-tag");
    const settingsLicenseInfo = $("#settings-license-info");

    // Actualizar Panel de Configuracion
    renderLicenseInfoBox(info, settingsLicenseInfo);
    configureSettingsLicenseButtons(info);

    // Quitar el data-i18n para que el traductor (i18n.js) no sobreescriba "PRO VERSION" de vuelta a "FREE VERSION"
    if (!badge) return;
    badge.removeAttribute("data-i18n");
    badge.style.color = "";
    badge.style.borderColor = "";

    if (licenseStatus === "PRO") {
        badge.textContent = getLicenseBadgeText(info);
        badge.className = "license-badge pro";
        badge.style.cursor = "default";
        proTags.forEach(el => el.style.display = "none");
    } else if (licenseStatus === "ANNUAL") {
        badge.textContent = getLicenseBadgeText(info);
        badge.className = "license-badge pro";
        badge.style.cursor = "default";
        proTags.forEach(el => el.style.display = "none");
    } else if (licenseStatus === "FREE") {
        badge.textContent = getLicenseBadgeText(info);
        badge.className = "license-badge";
        badge.style.color = "#38bdf8";
        badge.style.borderColor = "#38bdf8";
        badge.style.cursor = "default";
        proTags.forEach(el => el.style.display = "inline-block");
    } else if (licenseStatus === "TRIAL") {
        badge.textContent = getLicenseBadgeText(info);
        badge.className = "license-badge";
        badge.style.color = "#fcd34d";
        badge.style.borderColor = "#fcd34d";
        badge.style.cursor = "default";
        proTags.forEach(el => el.style.display = "inline-block");
    } else if (licenseStatus === "LOCKED") {
        badge.textContent = getLicenseBadgeText(info);
        badge.className = "license-badge";
        badge.style.color = "#ef4444";
        badge.style.borderColor = "#ef4444";
        badge.style.cursor = "default";
        // badge.onclick = () => showLicenseModal(false); // Disabled per user request (Info only)
    } else {
        badge.textContent = getLicenseBadgeText(info);
        badge.className = "license-badge";
        badge.style.color = "#ef4444";
        badge.style.borderColor = "#ef4444";
        badge.style.cursor = "default";
    }
}

function blockAppExpired(msg) {
    // Crear overlay de bloqueo total
    const blocker = document.createElement("div");
    blocker.id = "expired-blocker";
    blocker.style.cssText = "position:fixed; top:0; left:0; width:100%; height:100%; background:rgba(2,6,23,0.98); z-index:99999; display:flex; flex-direction:column; align-items:center; justify-content:center; backdrop-filter:blur(20px);";

    blocker.innerHTML = `
        <div style="text-align:center; max-width:500px; padding:40px; border:1px solid #ef4444; border-radius:20px; background:rgba(30,41,59,0.5);">
            <h1 style="color:#ef4444; margin-bottom:20px; font-size:2rem;">LICENCIA EXPIRADA</h1>
            <p style="color:#cbd5e1; margin-bottom:30px; font-size:1.1rem;">${msg}</p>
            <p style="color:#94a3b8; font-size:0.9rem; margin-bottom:30px;">La licencia vigente terminó o no se pudo renovar. Para continuar usando Zenith Astro Stacker, activa una licencia anual.</p>
            <button id="btn-unlock-pro" class="success" style="font-size:1.2rem; padding:15px 40px; width:auto; margin:0 auto; background: linear-gradient(135deg, #ef4444, #b91c1c);">ACTIVAR AHORA</button>
        </div>
    `;

    document.body.appendChild(blocker);

    document.getElementById("btn-unlock-pro").onclick = () => {
        showLicenseModal(false); // Locked mode
    };
}

// showLicenseModal params:
// - canClose: True if user has valid license (upgrade flow). False if locked.
// - marketingMode: True to show "Buy Now" upsell. False for standard input.
// showLicenseModal params:
// - canClose: True if user has valid license (upgrade flow). False if locked.
// - marketingMode: True to show "Buy Now" upsell. False for standard input.
function showLicenseModal(canClose = false, marketingMode = false) {
    const modal = $("#license-modal");
    const modalBox = modal.querySelector(".modal-box");
    const trialBtns = modal.querySelector(".license-actions-grid");
    const divider = modal.querySelector(".license-divider");
    const inputWrapper = modal.querySelector(".license-input-wrapper");
    const btnActivate = modal.querySelector("#btn-activate-license");
    const title = modal.querySelector(".license-title");
    const desc = modal.querySelector(".license-desc");

    // Limpieza de inyecciones previas (Upsell container, Close Btn)
    const existingUpsell = document.getElementById("upsell-container");
    if (existingUpsell) existingUpsell.remove();
    const existingClose = document.getElementById("modal-close-x");
    if (existingClose) existingClose.remove();

    // Default Visibility Reset
    inputWrapper.style.display = "block";
    btnActivate.style.display = "block";
    if (trialBtns) trialBtns.style.display = "grid";
    if (divider) divider.style.display = "block";

    // 1. Gestion de Cierre (Click Outside y Boton X)
    const staticCloseBtn = document.getElementById("license-modal-close");

    if (canClose) {
        // Click Outside
        modal.onclick = (e) => { if (e.target === modal) closeLicenseModal(); };
        modal.style.cursor = "pointer";

        // Boton X Estatico
        if (staticCloseBtn) {
            staticCloseBtn.style.display = "block";
            staticCloseBtn.onclick = (e) => {
                e.stopPropagation();
                closeLicenseModal();
            };
        }
    } else {
        modal.onclick = null;
        modal.style.cursor = "default";
        if (staticCloseBtn) staticCloseBtn.style.display = "none";
    }

    // 2. Modos de Visualizacion
    if (marketingMode) {
        // --- MODO MARKETING / UPSELL ---
        if (trialBtns) trialBtns.style.display = "none";
        if (divider) divider.style.display = "none";

        // Hide input initially
        inputWrapper.style.display = "none";
        btnActivate.style.display = "none";

        title.innerHTML = tr("activation.marketing_title", "<svg class='zas-icon'><use href='#icon-rocket'></use></svg> <span style='color:#f59e0b'>Sube de Nivel</span>");
        desc.innerHTML = tr("activation.marketing_desc", "Disfruta de potencia completa con una licencia anual activa.");

        // Inject Buy Button and Toggle
        const upsellDiv = document.createElement("div");
        upsellDiv.id = "upsell-container";
        upsellDiv.style.textAlign = "center";
        upsellDiv.style.marginTop = "20px";
        upsellDiv.innerHTML = `
            <button class="btn-premium" style="width:100%; padding:15px; font-size:1.1rem; margin-bottom:15px; cursor:pointer;"
                onclick="window.openBrowser('https://zenith-astro-stacker.lemonsqueezy.com/checkout/buy/106d0e17-92c2-4a23-8782-8f780f06ec74')">
                <svg class="zas-icon"><use href="#icon-pro"></use></svg> <b>${tr("activation.buy_btn", "COMPRAR LICENCIA ANUAL")}</b>
            </button>
            <div style="font-size:0.9rem; color:#94a3b8; margin-bottom:10px;">
                ${tr("activation.already_have_key", "¿Ya tienes tu clave?")}
            </div>
            <button class="primary" style="background:transparent; border:1px solid #334155; color:#cbd5e1; font-size:0.85rem; padding:5px 15px; cursor:pointer;" 
                 id="toggle-license-input">
                 ${tr("activation.enter_license_code", "Ingresar Código de Licencia")}
            </button>
        `;

        desc.parentNode.insertBefore(upsellDiv, inputWrapper);

        document.getElementById("toggle-license-input").onclick = () => {
            inputWrapper.style.display = "block";
            btnActivate.style.display = "block";
            upsellDiv.style.display = "none";
            title.textContent = tr("activation.enter_license_title", "Ingresa tu Licencia");
            desc.innerHTML = tr("activation.enter_license_desc", "Introduce tu código de licencia.");
        };

    } else if (canClose && !marketingMode && ["PRO", "ANNUAL"].includes(licenseStatus)) {
        // --- MODO CAMBIAR CLAVE (LICENCIA ACTIVA) ---
        if (trialBtns) trialBtns.style.display = "none";
        if (divider) divider.style.display = "none";

        title.textContent = tr("activation.update_title", "Actualizar Licencia");
        desc.innerHTML = tr("activation.update_desc", "Ingresa tu nueva clave de licencia.");

    } else {
        // --- MODO LOCKED O ACTIVACION STANDARD (Start) ---
        // Si canClose es true pero NO es PRO, cae aquí => "Activación Requerida" pero con botón cerrar (gracias al bloque 1)
        title.textContent = tr("activation.title", "Activación Requerida");
        desc.innerHTML = tr("activation.required_desc", "Desbloquea todo el potencial de <b>Zenith Astro Stacker</b>.");
    }

    modal.style.display = "flex";
}

function closeLicenseModal() {
    if (licenseStatus === "EXPIRED" || licenseStatus === "LOCKED") return;
    $("#license-modal").style.display = "none";

    // Cleanup temporary upsell elements on close
    const existingUpsell = document.getElementById("upsell-container");
    if (existingUpsell) existingUpsell.remove();

    scheduleTutorialAfterLicense();
}

async function handleActivation() {
    const input = $("#license-key-input");
    const btn = $("#btn-activate-license");
    const loader = $("#license-loading"); // Missing in original snippet view but presumed valid

    const key = input.value.trim();
    if (!key || key.length < 5) {
        showCustomAlert("Error", tr("activation.invalid_key", "Ingresa una clave válida."));
        return;
    }

    btn.style.display = "none";
    loader.style.display = "block";

    try {
        const msg = await invoke("activate_pro_license", { key: key, deviceName: "ZenithUserPC" });

        // Wait for the user to acknowledge activation before scheduling tutorials.
        await showCustomAlert(
            tr("activation.activated_title", "¡Licencia activada!"),
            tr("activation.activated_message", "Tu licencia se ha activado correctamente.\n\nBienvenido a la experiencia completa de Zenith Astro Stacker.")
        );

        // Recargar estado
        await checkLicenseAtStartup();

        // Quitar bloqueo si existia
        const blocker = document.getElementById("expired-blocker");
        if (blocker) blocker.remove();

        closeLicenseModal();
    } catch (e) {
        showCustomAlert(tr("activation.activation_error_title", "Error de Activación"), normalizeBackendText(e));
    } finally {
        btn.style.display = "block";
        loader.style.display = "none";
    }
}

async function confirmDeactivation() {
    const confirmed = await showCustomChoice(
        tr("activation.deactivate_title", "Desactivar Licencia"),
        tr("activation.deactivate_message", "¿Deseas desactivar la licencia en este equipo? Volverás al estado sin activar, o a expirado si la prueba ya terminó."),
        tr("activation.deactivate_confirm", "Sí, desactivar"),
        tr("general.cancel", "Cancelar")
    );

    if (confirmed) {
        try {
            await invoke("deactivate_license");
            await checkLicenseAtStartup();
            showCustomAlert(tr("activation.deactivated_title", "Desactivado"), tr("activation.deactivated_message", "Licencia removida de este equipo."));
        } catch (e) {
            console.error(e);
        }
    }
}

async function handleQaLicenseReset(event = null) {
    const btn = event?.currentTarget || $("#btn-reset-license-qa") || $("#btn-settings-reset-license-qa");
    const confirmed = await showCustomChoice(
        tr("activation.qa_reset_title", "Reset de pruebas"),
        tr("activation.qa_reset_message", "Esto borra el estado local de licencia y marca la prueba como no utilizada en este equipo.\n\nÚsalo solo para validar activaciones."),
        tr("activation.qa_reset_confirm", "Reiniciar"),
        tr("general.cancel", "Cancelar")
    );

    if (!confirmed) return;

    if (btn) {
        btn.disabled = true;
        btn.dataset.originalText = btn.textContent;
        btn.textContent = tr("activation.qa_reset_loading", "Reiniciando...");
    }

    try {
        await invoke("reset_license_state");
        const input = $("#license-key-input");
        if (input) input.value = "";
        await checkLicenseAtStartup();
        refreshSettingsLicenseInfo();
        await showCustomAlert(
            tr("activation.qa_reset_ready_title", "QA listo"),
            tr("activation.qa_reset_ready_message", "Estado local reiniciado. Ya puedes intentar activar otra prueba o licencia.")
        );
    } catch (e) {
        console.error("Error resetting license state:", e);
        await showCustomAlert("Error", String(e));
    } finally {
        if (btn) {
            btn.disabled = false;
            btn.textContent = btn.dataset.originalText || tr("activation.qa_reset_btn", "Permitir otra prueba");
            delete btn.dataset.originalText;
        }
    }
}

// Vinculaciones de eventos de licencia
const btnActivate = $("#btn-activate-license");
if (btnActivate) btnActivate.addEventListener("click", handleActivation);

const btnQaLicenseReset = $("#btn-reset-license-qa");
if (btnQaLicenseReset) btnQaLicenseReset.addEventListener("click", handleQaLicenseReset);

const btnSettingsQaLicenseReset = $("#btn-settings-reset-license-qa");
if (btnSettingsQaLicenseReset) btnSettingsQaLicenseReset.addEventListener("click", handleQaLicenseReset);




// Funcion auxiliar para abrir navegador
// Funcion auxiliar para abrir navegador
window.openBrowser = async (url) => {
    try {
        console.log("Intentando abrir URL:", url);
        // 1. Intentar con Tauri Shell (Metodo preferido)
        await open(url);
    } catch (e) {
        console.warn("Tauri Shell Open falló, intentando fallback:", e);
        try {
            // 2. Fallback: Window Open estandar
            const win = window.open(url, '_blank');
            if (!win) throw new Error("Popup blocked or window.open failed");
        } catch (e2) {
            console.error("Fallaron todos los metodos de apertura:", e2);
            // 3. Ultimo recurso: Mostrar alerta (solo si todo falla real)
            showCustomAlert("Acción Requerida", `Por favor abre este enlace manualmente:\n${url}`);
        }
    }
};

// --- LOGICA SETTINGS Y BOTONES NUEVOS ---
// --- LOGICA SETTINGS Y BOTONES NUEVOS ---


// (Eliminado: Inicializacion duplicada. Se maneja en window.load al final del archivo)


// --- SETTINGS MODAL LOGIC ---
const settingsModal = $("#settings-modal");
const btnSettings = $("#btn-settings");
const btnCloseSettings = $("#btn-close-settings");

if (btnSettings) {
    btnSettings.addEventListener("click", () => {
        settingsModal.style.display = "flex";

        // Refresh license info if tab is active
        const activeTab = document.querySelector(".settings-tab.active");
        if (activeTab && activeTab.dataset.tab === "license") {
            refreshSettingsLicenseInfo();
        }

        // INIT TUTORIAL SETTINGS UI
        const syncTutorialToggles = () => {
            const realChk = document.getElementById("chk-tutorial-enabled-real");
            if (toggle) toggle.checked = tutorialManager.enabled;
            if (realChk) realChk.checked = tutorialManager.enabled;
        };

        const toggle = document.getElementById("settings-tutorial-toggle");
        if (toggle) {
            toggle.checked = tutorialManager.enabled;
            const realChk = document.getElementById("chk-tutorial-enabled-real");
            if (realChk) realChk.checked = tutorialManager.enabled;

            toggle.onchange = (e) => {
                tutorialManager.toggle(e.target.checked);
                if (realChk) realChk.checked = e.target.checked;
            };
        }

        const btnResetTut = document.getElementById("btn-reset-tutorial");
        if (btnResetTut) {
            btnResetTut.onclick = () => {
                tutorialManager.resetAll({ startIntro: false });
                showCustomAlert(
                    tr("settings.general.tutorials_reset_title", "Tutoriales reiniciados"),
                    tr("settings.general.tutorials_reset_message", "Los tutoriales volverán a aparecer una vez cuando entres a cada flujo.")
                );
                syncTutorialToggles();
            };
        }

        document.querySelectorAll("[data-tutorial-start]").forEach(btn => {
            btn.onclick = () => {
                const flow = btn.getAttribute("data-tutorial-start");
                if (!flow) return;
                restartTutorialFlowFromSettings(flow, 0);
            };
        });
    });
}

if (btnCloseSettings) {
    btnCloseSettings.addEventListener("click", () => {
        settingsModal.style.display = "none";
        // Check if we need to restart tutorial after reset
        scheduleTutorialAfterLicense();
    });
}

// Tab Switching Logic
const settingsTabs = $$(".settings-tab");
settingsTabs.forEach(tab => {
    tab.addEventListener("click", () => {
        // Remove active class from all
        settingsTabs.forEach(t => t.classList.remove("active"));
        $$(".settings-content .tab-content").forEach(c => c.style.display = "none");

        // Add active to clicked
        tab.classList.add("active");

        // Show content
        const tabId = tab.dataset.tab;
        const target = $(`#tab-${tabId}`);
        if (target) target.style.display = "block";

        // Specific actions
        if (tabId === "license") {
            refreshSettingsLicenseInfo();
        }
    });
});

async function refreshSettingsLicenseInfo() {
    const container = $("#settings-license-info");

    if (!container) return;

    try {
        container.innerHTML = `<p style="color:#94a3b8; text-align:center;">${escapeHtml(tr("license.settings.loading", "Cargando información..."))}</p>`;
        const info = await invoke("check_license_status");
        lastLicenseInfo = info;

        renderLicenseInfoBox(info, container);
        configureSettingsLicenseButtons(info);

    } catch (e) {
        console.error("Error fetching license info for settings:", e);
        container.innerHTML = `<p style="color:#ef4444;">${escapeHtml(tr("license.settings.load_error", "Error al cargar información de licencia."))}</p>`;
    }
}

// Bind Settings Buttons
const btnSetActivate = $("#btn-settings-activate");
if (btnSetActivate) {
    btnSetActivate.addEventListener("click", () => {
        $("#settings-modal").style.display = "none";
        showLicenseModal();
    });
}

// (Listener eliminado: La logica de este boton ahora se maneja dinamicamente en refreshSettingsLicenseInfo)

// --- INDEPENDENT ACTIVATION MODAL LOGIC (User Requested Separation) ---

window.showActivationModal = function () {
    const modal = document.getElementById("activation-modal");
    const closeBtn = document.getElementById("activation-modal-close");
    const btnActivate = document.getElementById("btn-manual-activate");
    const input = document.getElementById("activation-key-input");

    // Reset State
    if (input) input.value = "";
    if (document.getElementById("activation-loading")) document.getElementById("activation-loading").style.display = "none";
    if (btnActivate) btnActivate.style.display = "block";

    if (modal) {
        modal.style.display = "flex";

        // Ensure Close Logic
        if (closeBtn) {
            closeBtn.onclick = window.closeActivationModal;
        }

        // Click Outside
        modal.onclick = (e) => {
            if (e.target === modal) window.closeActivationModal();
        };
    }
}

window.closeActivationModal = function () {
    const modal = document.getElementById("activation-modal");
    if (modal) modal.style.display = "none";

    scheduleTutorialAfterLicense();
}

window.handleManualActivation = async function () {
    const input = document.getElementById("activation-key-input");
    const btn = document.getElementById("btn-manual-activate");
    const loader = document.getElementById("activation-loading");

    const key = input.value.trim();
    if (!key || key.length < 5) {
        showCustomAlert("Error", tr("activation.invalid_key", "Ingresa una clave válida."));
        return;
    }

    if (btn) btn.style.display = "none";
    if (loader) loader.style.display = "block";

    try {
        // Reuse same backend command
        await invoke("activate_pro_license", { key: key, deviceName: "ZenithUserPC" });

        await showCustomAlert(
            tr("activation.activated_title", "¡Licencia activada!"),
            tr("activation.activated_message", "Tu licencia se ha activado correctamente.\n\nBienvenido a la experiencia completa de Zenith Astro Stacker.")
        );

        await checkLicenseAtStartup();
        refreshSettingsLicenseInfo(); // Refresh settings UI
        window.closeActivationModal();

    } catch (e) {
        showCustomAlert(tr("activation.activation_error_title", "Error de Activación"), normalizeBackendText(e));
    } finally {
        if (btn) btn.style.display = "block";
        if (loader) loader.style.display = "none";
    }
}

// Bind Events for Manual Activation
const btnManualActivate = document.getElementById("btn-manual-activate");
if (btnManualActivate) {
    btnManualActivate.addEventListener("click", window.handleManualActivation);
}




// Boton Trial en Modal
const btnTrialStart = $("#btn-trial-start");
if (btnTrialStart) {
    btnTrialStart.addEventListener("click", () => {
        // Boton "Continuar Trial" (Solo si status == TRIAL y dias > 0)
        if (licenseStatus === "TRIAL") {
            $("#license-modal").style.display = "none";
        } else {
            // Si trata de saltar
            showCustomAlert("Acceso Denegado", "Debes activar una licencia válida (Trial o Pro) para continuar.");
        }
    });
}

// --- NUEVA LOGICA DE TABS EN SETTINGS ---
// (Eliminado)

// =========================================================================
// ESTADO GLOBAL
// =========================================================================

let currentFilePath = "";
window.currentFilePath = "";
window.setCurrentFilePath = (path = "") => {
    currentFilePath = path || "";
    window.currentFilePath = currentFilePath;
};
window.getCurrentFilePath = () => currentFilePath;
window.setMosaicViewportMode = (active = false) => {
    window.__mosaicViewportMode = !!active;

    const resContainer = document.querySelector("#view-result .zoom-content");
    const resultImg = document.querySelector("#img-result");
    if (!resContainer || !resultImg) return;

    if (active) {
        resContainer.style.position = "absolute";
        resContainer.style.top = "0";
        resContainer.style.left = "0";
        resContainer.style.display = "block";
        resContainer.style.justifyContent = "initial";
        resContainer.style.alignItems = "initial";
        resContainer.style.transformOrigin = "0 0";
        resultImg.style.margin = "0";
    } else {
        if (resultImg.naturalWidth && resultImg.naturalHeight && window.prepareZoomSurfaceForImage) {
            window.prepareZoomSurfaceForImage(resultImg);
        } else {
            resContainer.style.position = "";
            resContainer.style.top = "";
            resContainer.style.left = "";
            resContainer.style.display = "";
            resContainer.style.justifyContent = "";
            resContainer.style.alignItems = "";
            resContainer.style.width = "";
            resContainer.style.height = "";
            resContainer.style.transformOrigin = "";
            resultImg.style.margin = "";
        }
    }
};
let chartInstance = null;
let activeAPoints = [];
let updateTimer = null;
let isLocalOperation = false;
let isCancellationRequested = false;
let pipelineRequestId = 0;
let lastProcessedParams = "";
window.resetPipelineState = () => {
    pipelineRequestId = 0;
    lastProcessedParams = "";
};
let currentAnalysisMode = "global";
let currentBestFrame = 0; // NEW: Store Ref Frame
// ANIMATION STATE
// "planetary" or "surface"
let currentGraphData = [];
let currentRecommendedPct = 20;
let currentFileMetadata = null; // NEW: Store metadata for report
let currentVideoStats = null; // NEW: Store video stats during analysis
let manualAnchorPoint = null; // NEW: Manual Anchor for Surface Mode
let isSettingManualAnchor = false; // State for interactive selection mode

// Variables de Zoom y Pan (Vista Principal)
let zoomLevel = 0.1;
let panX = 0;
let panY = 0;
let isDragging = false;
let startX = 0;
let startY = 0;
let isPanningFrameRequested = false;

function captureViewportState() {
    return {
        zoomLevel,
        panX,
        panY
    };
}

function restoreViewportState(state) {
    if (!state) return;
    zoomLevel = state.zoomLevel;
    panX = state.panX;
    panY = state.panY;
    isPanningFrameRequested = false;
    updateTransform();
}

function clearMosaicInfoOverlay() {
    document.getElementById("mosaic-info-card")?.remove();
    document.getElementById("mosaic-info-chip")?.remove();
}
window.clearMosaicInfoOverlay = clearMosaicInfoOverlay;

let activeDrizzleFactor = 1.0;

// Variables de Recorte (Vista Principal)
let isCropping = false;
let isDrawingCrop = false;
let isMovingCrop = false;
let isResizingCrop = false;
let resizeDir = "";
let cropStart = { x: 0, y: 0 };
let moveOffset = { x: 0, y: 0 };
let cropSelection = { x: 0, y: 0, w: 0, h: 0 };

// --- Estado Batch & Animacion ---
let isBatchMode = false;
let batchFiles = [];
let batchSourcePath = "";
let batchOutputFolder = "";
let batchGeneratedImages = [];
let batchResultPaths = [];
let mosaicManager = null; // Instance

const ZENITH_ULTIMATE_VALUE = "zenith_ultimate";
const ZENITH_ULTIMATE_NAME = "Zenith Presicion Ultimate";

function normalizeZenithCategory(category) {
    return category === "planet_large" ? "planet_small" : (category || "surface");
}

function getSelectedTargetCategory() {
    return normalizeZenithCategory(document.getElementById("sel-target-category")?.value || "surface");
}

function getBayerOverrideValue() {
    const value = document.getElementById("sel-bayer-override")?.value || "auto";
    return value === "auto" ? null : parseInt(value);
}

function getAnalysisModeValue(flow) {
    return flow?.analysisMode || document.getElementById("sel-analysis-mode")?.value || getZenithUltimateFlow().analysisMode;
}

function getManualAnchorOverrideValue() {
    return null;
}

function getStackingRoiOverrideValue() {
    return null;
}

function getZenithUltimateFlow(category = getSelectedTargetCategory()) {
    category = normalizeZenithCategory(category);
    const isSurface = category === "surface";
    return {
        name: ZENITH_ULTIMATE_NAME,
        category,
        // Both categories use the V3 analysis engine
        analysisMode: isSurface ? "surface_v3" : "planet_v3",
        batchMode: isSurface ? "surface_v3" : "planet_v3",
        // Planets use global CoG alignment (rigid body - no liquid warping).
        // Surface uses liquid_v3 with per-AP local warp analysis.
        alignMode: "liquid_v3",
        isSurface,
        isV3: true,
        // R11: AMBAS categorías usan análisis de warping (mapas de calidad
        // 40×40 por frame → selección por-AP). El planeta conserva además su
        // centrado global CoG; el warping local refina encima, igual que
        // Superficie. Esto le da a Disco el motor completo.
        warpingAnalysis: true,
        // BOTH categories use AP multipoint (AS!4-style): planets get local
        // refinement on top of the CoG global centering (bands, moons, GRS).
        // If the Smart Grid yields 0 points (tiny/faint disk) the flow falls
        // back to pure CoG automatically in ensureZenithUltimateAlignmentPoints.
        needsPoints: true,
        // Surface: AP 32 con malla solapada (~21px de paso) — resolución de
        // warp suficiente para seguir celdas de seeing individuales (AS!4-like).
        apSize: isSurface ? 32 : 32,
        apThreshold: isSurface ? 0.04 : 0.08,
        gridMode: isSurface ? "surface" : "planetary",
        recommendedPct: isSurface ? 15 : 12
    };
}

function isZenithUltimateSelected() {
    const selQuality = document.getElementById("sel-quality-method");
    return !selQuality || selQuality.value === ZENITH_ULTIMATE_VALUE;
}

function getActiveZenithFlow() {
    if (isZenithUltimateSelected()) return getZenithUltimateFlow();
    const alignMode = ui?.alignMode?.value || "zenith_map";
    const category = normalizeZenithCategory(getSelectedTargetCategory());
    return {
        name: "Legacy",
        category,
        analysisMode: category === "surface" ? "surface_v2" : "planet_v2",
        batchMode: category === "surface" ? "surface_v2" : "planet_v2",
        alignMode,
        isSurface: category === "surface",
        isV3: alignMode.includes("v3"),
        warpingAnalysis: alignMode === "liquid_warping" || alignMode === "liquid_v3",
        needsPoints: alignMode === "liquid_warping" || alignMode === "liquid_v3",
        apSize: parseInt(document.getElementById("ap-size")?.value || "48"),
        apThreshold: parseFloat(document.getElementById("ap-bright")?.value || "5") / 100.0,
        gridMode: category === "surface" ? "surface" : "planetary",
        recommendedPct: 20
    };
}

function applyZenithUltimateFlow() {
    const flow = getZenithUltimateFlow();
    const selTargetCategory = document.getElementById("sel-target-category");
    const selAnalysisMode = document.getElementById("sel-analysis-mode");
    const alignModeSelect = document.getElementById("align-mode");
    const apSize = document.getElementById("ap-size");
    const apBright = document.getElementById("ap-bright");
    const mpWrapper = document.getElementById("multipoint-wrapper");
    const warning = document.getElementById("liquid-warning");
    const elitePanel = document.getElementById("panel-elite-settings");

    if (selTargetCategory && selTargetCategory.value !== flow.category) selTargetCategory.value = flow.category;
    if (selAnalysisMode) selAnalysisMode.value = flow.analysisMode;
    if (alignModeSelect) alignModeSelect.value = flow.alignMode;
    if (apSize) apSize.value = String(flow.apSize);
    if (apBright) apBright.value = String(Math.round(flow.apThreshold * 100));
    if (mpWrapper) mpWrapper.style.display = flow.needsPoints ? "block" : "none";
    if (warning) warning.style.display = "none";
    if (elitePanel) elitePanel.style.display = "none";

    currentAnalysisMode = flow.analysisMode;
    if (alignModeSelect) alignModeSelect.dispatchEvent(new Event("change"));
}

window.getZenithUltimateFlow = getZenithUltimateFlow;

function applySuggestedTargetCategory(suggestedTarget) {
    const category = normalizeZenithCategory(suggestedTarget);
    const selTargetCategory = document.getElementById("sel-target-category");
    if (!selTargetCategory || selTargetCategory.value === category) return;

    selTargetCategory.value = category;
    selTargetCategory.dispatchEvent(new Event("change"));
    applyZenithUltimateFlow();

    const flow = getZenithUltimateFlow(category);
    currentAnalysisMode = flow.analysisMode;
    log("INFO", `Objetivo detectado: ${category === "planet_small" ? "Planeta / Fase Lunar" : "Superficie"}.`);
}

// Estado del Reproductor de Animacion
let animState = {
    isPlaying: false,
    frameIndex: 0,
    direction: 1,
    timerId: null,
    speedMs: 100,
    isBoomerang: false,
    images: []
};

// Estado de Filtros Visuales para Animacion (Valores flotantes)
let animFilters = {
    rotation: 0,
    brightness: 0.0,
    contrast: 1.0,
    saturation: 1.0,
    gamma: 1.0,
    hue: 0,
    levelsBlack: 0.0,
    levelsWhite: 1.0,
    colorFilter: "none",
    tint: { r: 255, g: 255, b: 255 }
};

// Estado de Overlays
let animOverlays = {
    mode: "none",
    wmText: "",
    wmOpacity: 0.5,
    frTitle: "",
    frTele: "",
    frCam: "",
    frOther: "",
    font: "Arial"
};

// Variables de Recorte (VISOR ANIMACION)
let isAnimCropping = false;
let isAnimDrawing = false;
let isAnimMoving = false;
let isAnimResizing = false;
let animResizeDir = "";
let animCropStart = { x: 0, y: 0 };
let animMoveOffset = { x: 0, y: 0 };
let animCropSelection = { x: 0, y: 0, w: 0, h: 0 };
let animCropExportRect = null;
let animCropDisplaySelection = null;

// =========================================================================
// SISTEMA DE ACTUALIZACIONES (UPDATER)
// =========================================================================

// =========================================================================
// SISTEMA DE ACTUALIZACIONES (UPDATER)
// =========================================================================

/**
 * Compara dos versiones (v1 vs v2).
 * Soporta formatos: "0.1.1", "0.1.1.1", "0.1.1-Beta", "v0.1.1.1Beta"
 * Retorna:
 *  1 si v1 > v2
 * -1 si v1 < v2
 *  0 si v1 == v2
 */
function compareVersions(v1, v2) {
    // Normalizar: quitar 'v', espacios, y convertir "Beta" a algo manejable si es necesario,
    // pero para comparacion numerica, simplemente extraemos los componentes.

    const clean = (v) => v.replace(/^v/, '').replace(/-?beta/i, '').replace(/[^0-9.]/g, '');

    // Separar por puntos
    const p1 = clean(v1).split('.').map(p => parseInt(p, 10));
    const p2 = clean(v2).split('.').map(p => parseInt(p, 10));

    // Comparar componente a componente
    const len = Math.max(p1.length, p2.length);

    for (let i = 0; i < len; i++) {
        const n1 = p1[i] || 0;
        const n2 = p2[i] || 0;

        if (n1 > n2) return 1;
        if (n1 < n2) return -1;
    }

    return 0;
}

// Fallback manual para versiones no estandar (Ej: v0.1.1.1Beta) que Tauri rechaza
async function manualUpdateCheck(currentVer) {
    try {
        // Fetch directo al JSON de GitHub (mismo endpoint que tauri.conf.json)
        // Usamos cache: no-store para evitar lecturas viejas
        const response = await fetch("https://github.com/eduardomg1499/astro-stacker/releases/latest/download/latest.json", {
            cache: "no-store",
            headers: { 'Cache-Control': 'no-cache' }
        });

        if (!response.ok) throw new Error("Fetch failed: " + response.statusText);

        const data = await response.json();
        const remoteVer = data.version;

        console.log(`Manual Check: Current=${currentVer}, Remote=${remoteVer}`);

        // Comparacion robusta
        if (compareVersions(remoteVer, currentVer) > 0) {
            return {
                available: true,
                version: remoteVer,
                body: data.notes || tr("updater.manual_notes", "Nueva versión disponible."),
                manual: true, // Flag para indicar que es deteccion manual
                url: data.platforms["windows-x86_64"]?.url || "https://github.com/eduardomg1499/astro-stacker/releases/latest"
            };
        }
    } catch (e) {
        console.warn("Manual update check failed:", e);
    }
    return null;
}

function updateStatusCard(kind, message, version = "", notesHtml = "") {
    const icon = kind === "success" ? "✓" : kind === "error" ? "×" : "↗";
    const iconClass = kind === "success" ? "" : kind;
    return `
        <div class="update-card-content">
            <div class="update-status-icon ${iconClass}">${icon}</div>
            <p class="update-card-copy">${message}</p>
            ${version ? `<div class="update-card-version">${escapeHtml(version)}</div>` : ""}
            ${notesHtml ? `<div class="update-card-notes">${notesHtml}</div>` : ""}
        </div>
    `;
}

async function checkForAppUpdates(silent = true) {
    const badge = $("#updater-badge");
    const icon = $("#updater-icon");
    const text = $("#updater-text");

    if (!badge || !icon || !text) return;

    let currentVer = "v0.0.0";
    try {
        currentVer = await getVersion();
        text.textContent = `v${currentVer}`;
    } catch (e) {
        console.error("Error obteniendo version", e);
    }

    // STARTUP CHECKS
    // (Moved to I18N section below)

    // BIND FLOW TRIGGERS
    // Individual
    const btnAnalyze = document.getElementById("btn-analyze");
    if (btnAnalyze) {
        btnAnalyze.addEventListener("click", () => {
            // No longer starting 'individual' flow here.
            // The tutorial will be advanced by specific UI events later.
        });
    }

    // Batch
    const btnBatch = document.getElementById("btn-batch-mode");
    if (btnBatch) {
        btnBatch.addEventListener("click", () => {
            // The batch tutorial starts after the folder has been scanned, so it can
            // point to the visible batch controls instead of the native file picker.
        });
    }

    // Mosaic
    const btnMosaic = document.getElementById("btn-mosaic-mode");
    if (btnMosaic) {
        btnMosaic.addEventListener("click", () => {
            // The real mosaic entry handler starts the tutorial once the panel is visible.
        });
    }

    if (!silent) {
        badge.className = "updater-badge checking";
        icon.innerHTML = "<svg class='zas-icon icon-spin'><use href='#icon-lightning'></use></svg>";
        text.textContent = tr("updater.searching", "Buscando...");
    }

    try {
        // 1. Intentar Plugin Oficial (Soporta Update Automatico)
        let update = null;
        try {
            update = await check();
        } catch (pluginErr) {
            console.warn("Plugin check failed, falling back to manual:", pluginErr);
        }

        // 2. Si el plugin no ve updates (o falló), intentar manual (Fallback para versiones "raras" o Beta)
        if (!update || !update.available) {
            const manualUpdate = await manualUpdateCheck(currentVer);
            if (manualUpdate) {
                update = manualUpdate;
            }
        }

        if (update && update.available) {
            console.log(`Actualizacion encontrada: ${update.version} (Actual: ${currentVer})`);

            badge.className = "updater-badge available";
            icon.innerHTML = "<svg class='zas-icon icon-bounce'><use href='#icon-rocket'></use></svg>";
            text.textContent = trFormat("updater.available_badge", { version: update.version }, `v${update.version} Disponible`);

            badge.onclick = async () => {
                const isManual = update.manual === true;
                const actionText = isManual
                    ? tr("updater.manual_action", "Descargar manualmente")
                    : tr("updater.install_action", "Actualizar ahora");
                
                const releaseNotes = escapeHtml(normalizeBackendText(update.body || tr("updater.default_notes", "Mejoras de rendimiento y correcciones."))).replace(/\n/g, '<br>');
                const detailsHtml = `
                    ${tr("updater.current_version", "Versión actual")}: <span style="color:#fcd34d;">${escapeHtml(currentVer)}</span><br>
                    ${tr("updater.new_version", "Nueva versión")}: <span style="color:#10b981;">v${escapeHtml(update.version)}</span><br><br>
                    ${isManual ? tr("updater.manual_note", "Nota: esta versión requiere instalación manual.") : tr("updater.install_question", "¿Deseas descargar e instalar ahora?")}
                `;

                const confirmed = await showCustomChoice(
                    "ZENITH ASTRO STACKER",
                    updateStatusCard(
                        "available",
                        tr("updater.available_title", "Nueva versión disponible"),
                        `v${update.version}`,
                        `${releaseNotes}<br><br>${detailsHtml}`
                    ),
                    actionText,
                    tr("updater.later", "Más tarde")
                );

                if (confirmed) {
                    if (isManual) {
                        // Flujo Manual (Abrir Browser)
                        await window.openBrowser(update.url);
                    } else {
                        // Flujo Automatico (Plugin)
                        try {
                            showProcessing(tr("updater.downloading", "DESCARGANDO ACTUALIZACIÓN..."));
                            let downloaded = 0;
                            let contentLength = 0;

                            await update.downloadAndInstall((event) => {
                                switch (event.event) {
                                    case 'Started':
                                        contentLength = event.data.contentLength;
                                        console.log(`Descarga iniciada. Tamaño: ${contentLength}`);
                                        break;
                                    case 'Progress':
                                        downloaded += event.data.chunkLength;
                                        if (contentLength > 0) {
                                            const pct = (downloaded / contentLength) * 100;
                                            const bar = $("#overlay-progress-fill");
                                            const msg = $("#processing-msg");
                                            if (bar) bar.style.width = pct + "%";
                                            if (msg) msg.textContent = trFormat("updater.downloading_pct", { pct: Math.round(pct) }, `DESCARGANDO: ${Math.round(pct)}%`);
                                        }
                                        break;
                                    case 'Finished':
                                        console.log('Descarga completada');
                                        break;
                                }
                            });

                            showProcessing(tr("updater.restarting", "REINICIANDO..."));
                            await relaunch();
                        } catch (e) {
                            hideProcessing();
                            showCustomAlert(
                                tr("updater.install_error_title", "Error de instalación"),
                                trFormat("updater.install_error_message", { error: normalizeBackendText(e) }, `Falló la instalación: ${normalizeBackendText(e)}\nIntenta descargarla manualmente desde GitHub.`)
                            );
                            // Fallback final a manual si falla el automatico
                            window.openBrowser("https://github.com/eduardomg1499/astro-stacker/releases/latest");
                        }
                    }
                }
            };

        } else {
            if (!silent) {
                let msg = trFormat("updater.latest_message", { version: currentVer }, `Estas utilizando la version mas reciente (v${currentVer}).`);
                if (update) {
                    console.log("Check result:", update);
                    if (update.version === currentVer) {
                        msg += `\n\n${trFormat("updater.same_version_message", { version: update.version }, `El servidor reporta la misma version (v${update.version}).`)}`;
                    }
                }
                showCustomAlert("ZENITH ASTRO STACKER", updateStatusCard("success", escapeHtml(msg).replace(/\n/g, "<br>"), `v${currentVer}`));
            }
            badge.className = "updater-badge";
            icon.innerHTML = "<svg class='zas-icon'><use href='#icon-lightning'></use></svg>";
            text.textContent = `v${currentVer}`;
            badge.onclick = () => checkForAppUpdates(false);
        }
    } catch (error) {
        console.error("Error general buscando updates:", error);

        badge.className = "updater-badge";
        icon.innerHTML = "<svg class='zas-icon'><use href='#icon-cross'></use></svg>";
        text.textContent = `v${currentVer}`;
        badge.onclick = () => checkForAppUpdates(false);

        if (!silent) {
            showCustomAlert(
                "ZENITH ASTRO STACKER",
                updateStatusCard(
                    "error",
                    escapeHtml(tr("updater.connection_error_title", "Error de conexión")),
                    "",
                    escapeHtml(tr("updater.connection_error_message", "No se pudo buscar actualizaciones. Revisa tu internet."))
                )
            );
        }
    }
}


async function checkAvx2Status() {
    try {
        // Cross-platform backend label: "AVX2 · Windows", "NEON · macOS", etc.
        // (The old check only knew x86 AVX2, so Apple Silicon wrongly showed
        // "SIMD OFF" even though NEON is always active there.)
        const label = await invoke("get_accel_label");
        const isAccel = !/escalar/i.test(label);

        // 1. Persistent label on the processing overlay (the progress screen).
        const accelEl = document.getElementById("accel-label");
        if (accelEl) {
            accelEl.innerHTML = `<svg class='zas-icon' style='width:1em;height:1em;'><use href='#icon-rocket'></use></svg> Aceleración: ${label}`;
            accelEl.style.color = isAccel ? "#10b981" : "#f59e0b";
        }

        // 2. Header badge (created once).
        const appInfoDiv = document.querySelector(".app-info");
        if (appInfoDiv && !appInfoDiv.querySelector(".accel-badge")) {
            const badge = document.createElement("span");
            badge.className = "accel-badge";
            const icon = isAccel ? "icon-rocket" : "icon-cross";
            badge.innerHTML = `<svg class='zas-icon' style='width:1em;height:1em;margin-right:2px;'><use href='#${icon}'></use></svg> ${label}`;
            badge.style.cssText = `background:${isAccel ? "#10b981" : "#f59e0b"}; color:${isAccel ? "white" : "black"}; padding:2px 6px; border-radius:4px; font-size:0.75rem; margin-left:8px; font-weight:bold; display:inline-flex; align-items:center;`;
            appInfoDiv.appendChild(badge);
        }
        console.log("SIMD backend:", label);
    } catch (e) {
        console.error("Failed to get accel label:", e);
    }
}

// =========================================================================
// UTILIDADES GRAFICAS
// =========================================================================

function setImageAndWait(imgElement, srcBase64, fitView = true) {
    return new Promise((resolve) => {
        if (!imgElement) { resolve(false); return; }
        imgElement.classList.remove("loaded");
        imgElement.onload = null; imgElement.onerror = null;

        const onReady = () => {
            if (!imgElement.naturalWidth || !imgElement.naturalHeight) {
                resolve(false);
                return;
            }
            requestAnimationFrame(() => {
                prepareZoomSurfaceForImage(imgElement);
                if (fitView) {
                    fitToScreen(imgElement);
                } else {
                    updateTransform();
                    requestAnimationFrame(() => ensureImageVisibleInViewport(imgElement));
                }

                // FIX: Ensure Crop Box stays valid and visible if resizing/updating
                if (isCropping && imgElement.naturalWidth > 0) {
                    const iw = imgElement.naturalWidth;
                    const ih = imgElement.naturalHeight;
                    // Clamp to bounds
                    if (cropSelection.x < 0) cropSelection.x = 0;
                    if (cropSelection.y < 0) cropSelection.y = 0;
                    if (cropSelection.x + cropSelection.w > iw) cropSelection.x = Math.max(0, iw - cropSelection.w);
                    if (cropSelection.y + cropSelection.h > ih) cropSelection.y = Math.max(0, ih - cropSelection.h);

                    updateCropDOM();
                }

                imgElement.classList.add("loaded");
                resolve(true);
            });
        };

        imgElement.onload = onReady;
        imgElement.onerror = () => resolve(false);
        imgElement.src = srcBase64;

        setTimeout(() => {
            if (
                imgElement.complete &&
                imgElement.naturalWidth > 0 &&
                imgElement.naturalHeight > 0 &&
                !imgElement.classList.contains("loaded")
            ) {
                onReady();
            }
        }, 100);
    });
}
window.setImageAndWait = setImageAndWait; // Expose for MosaicManager

function clearSourcePreviewSurface(width = null, height = null, clearImage = true) {
    if (clearImage && ui.imgSource) {
        ui.imgSource.onload = null;
        ui.imgSource.onerror = null;
        ui.imgSource.classList.remove("loaded", "img-reveal");
        ui.imgSource.removeAttribute("src");
    }

    if (ui.gridOverlay) {
        if (Number.isFinite(width) && Number.isFinite(height) && width > 0 && height > 0) {
            ui.gridOverlay.width = width;
            ui.gridOverlay.height = height;
        }
        const ctx = ui.gridOverlay.getContext("2d");
        ctx.clearRect(0, 0, ui.gridOverlay.width, ui.gridOverlay.height);
        ui.gridOverlay.style.position = "absolute";
        ui.gridOverlay.style.left = "0px";
        ui.gridOverlay.style.top = "0px";
        ui.gridOverlay.style.transform = "none";
        ui.gridOverlay.style.width = "auto";
        ui.gridOverlay.style.height = "auto";
    }

    const srcContainer = $("#view-source .zoom-content");
    if (srcContainer) srcContainer.style.transform = "translate(0px, 0px) scale(1)";
    activeAPoints = [];
    if (ui.apCount) ui.apCount.textContent = "0";
}

window.addEventListener("load", () => {
    console.log("Zenith: Startup content loaded.");

    // Populate the hardware-acceleration label (progress overlay + header badge).
    checkAvx2Status();

    // Double-check show if it somehow missed the module init
    if (appWindow && typeof appWindow.show === 'function') {
        appWindow.show().catch(e => {}); 
    }

    const splash = $("#splash-screen");
    const splashBar = $("#splash-progress-fill");

    if (splashBar) {
        let progress = 20;
        const splashInterval = setInterval(() => {
            progress += Math.random() * 20 + 5;
            if (progress >= 100) {
                progress = 100;
                splashBar.style.width = "100%";
                clearInterval(splashInterval);
                setTimeout(finishSplash, 200);
            } else {
                splashBar.style.width = progress + "%";
            }
        }, 120);
    } else {
        finishSplash();
    }

    async function finishSplash() {
        if (splash) {
            // PASO 1: El Banner se desvanece
            splash.style.opacity = "0"; 
            console.log("Zenith: Loading complete. Transitions initiated...");

            // PASO 2: Expansión Animada de la Ventana
            try {
                if (appWindow && typeof appWindow.setSize === 'function') {
                    // 2a. Eliminar restricciones de tamaño mínimo temporalmente
                    if (typeof appWindow.setMinSize === 'function') {
                        await appWindow.setMinSize(new LogicalSize(0, 0));
                    }

                    // 2b. Animación de expansión suave
                    // Usamos una transición controlada para evitar saltos bruscos
                    const targetWidth = 1280;
                    const targetHeight = 900;
                    
                    await animateWindowExpansion(targetWidth, targetHeight, 450);
                    
                    console.log("Zenith: Window expansion complete.");
                }
            } catch(e) { 
                console.error("Zenith: Startup Expansion failed:", e); 
                // Fallback: Salto instantáneo en caso de error
                try {
                   await appWindow.setSize(new LogicalSize(1280, 900));
                   await appWindow.center();
                } catch(e2) {}
            }

            // PASO 3: Revelar Interfaz Principal (Sincronizado)
            setTimeout(() => {
                document.body.classList.add("ready");
                // Restaurar restricciones de tamaño final para la UI principal
                if (appWindow && typeof appWindow.setMinSize === 'function') {
                    appWindow.setMinSize(new LogicalSize(1000, 700)).catch(() => {});
                }
            }, 100);

            // PASO 4: Limpieza total del splash
            setTimeout(() => { 
                splash.style.display = "none";
            }, 1500);
        } else {
            document.body.classList.add("ready"); 
        }
    }

    /**
     * Función auxiliar para animar el tamaño de la ventana de Tauri
     */
    async function animateWindowExpansion(targetW, targetH, duration) {
        const startSize = await appWindow.innerSize();
        // Convertir PhysicalSize a Logical (asumiendo DPI estándar si no se puede obtener el factor)
        // En Tauri v2, es mejor trabajar con LogicalSize consistentemente.
        const factor = await appWindow.scaleFactor();
        const startW = startSize.width / factor;
        const startH = startSize.height / factor;

        const startTime = performance.now();

        return new Promise((resolve) => {
            function step(currentTime) {
                const elapsed = currentTime - startTime;
                const progress = Math.min(elapsed / duration, 1);
                
                // Easing: easeOutCubic
                const ease = 1 - Math.pow(1 - progress, 3);
                
                const currentW = Math.round(startW + (targetW - startW) * ease);
                const currentH = Math.round(startH + (targetH - startH) * ease);

                appWindow.setSize(new LogicalSize(currentW, currentH)).catch(() => {});
                
                if (progress < 1) {
                    requestAnimationFrame(step);
                } else {
                    appWindow.center().then(resolve).catch(resolve);
                }
            }
            requestAnimationFrame(step);
        });
    }

    loadSystemFonts();
    checkLicenseAtStartup();
    checkForAppUpdates(true);
});

async function loadSystemFonts() {
    try {
        const fonts = await invoke("get_available_fonts");
        const sel = $("#sel-anim-font");
        if (sel && fonts.length > 0) {
            sel.innerHTML = "";
            fonts.forEach(f => {
                const opt = document.createElement("option");
                opt.value = f;
                opt.textContent = f;
                sel.appendChild(opt);
            });
            if (fonts.includes("Arial")) sel.value = "Arial";
            else sel.value = fonts[0];
        }
    } catch (e) {
        console.error("Fonts error", e);
    }
}

// =========================================================================
// GESTION DE MODALES PERSONALIZADOS
// =========================================================================

// =========================================================================
// GESTION DE MODALES PERSONALIZADOS
// =========================================================================

function internalShowModal(title, msg, type, labelOk = "Aceptar", labelCancel = "Cancelar") {
    return new Promise((resolve) => {
        const overlay = $("#custom-modal-overlay");
        const titleEl = $("#modal-title");
        const msgEl = $("#modal-msg");
        const btnOk = $("#btn-modal-ok");
        const btnCancel = $("#btn-modal-cancel");
        const boxEl = overlay.querySelector(".modal-box");

        if (!overlay) { resolve(true); return; }

        titleEl.innerHTML = title;
        const hasStructuredHtml = /<\/?[a-z][\s\S]*>/i.test(String(msg));
        msgEl.innerHTML = hasStructuredHtml ? msg : String(msg).replace(/\n/g, "<br>");
        if (boxEl) {
            boxEl.classList.toggle("ser-modal-box", Boolean(msgEl.querySelector(".ser-converter-modal")));
        }

        // Hide buttons if labels are explicitly null
        if (labelOk) {
            btnOk.textContent = labelOk;
            btnOk.style.display = 'block';
            btnOk.style.width = (type === 'alert' || !labelCancel) ? '100%' : 'auto';
        } else {
            btnOk.style.display = 'none';
        }

        if (labelCancel) {
            btnCancel.textContent = labelCancel;
            btnCancel.style.display = 'block';
        } else {
            btnCancel.style.display = 'none';
        }

        overlay.style.display = "flex";

        const cleanup = () => {
            btnOk.onclick = null;
            btnCancel.onclick = null;
            overlay.style.display = "none";
            if (boxEl) boxEl.classList.remove("ser-modal-box");
            // Cleaning content listeners
            const choices = msgEl.querySelectorAll('[data-modal-result]');
            choices.forEach(c => c.onclick = null);
        };

        // Standard Button Handlers
        if (btnOk.style.display !== 'none') {
            btnOk.onclick = () => { cleanup(); resolve(true); };
        }

        if (btnCancel.style.display !== 'none') {
            btnCancel.onclick = () => { cleanup(); resolve(false); };
        }

        // Custom Choice Handlers (Embedded in HTML)
        const choices = msgEl.querySelectorAll('[data-modal-result]');
        choices.forEach(choice => {
            choice.onclick = () => {
                const rawVal = choice.dataset.modalResult;
                let val = rawVal;
                if (rawVal === 'true') val = true;
                if (rawVal === 'false') val = false;

                cleanup();
                resolve(val);
            };
        });
    });
}

async function showCustomAlert(title, msg) {
    await internalShowModal(title, msg, 'alert', tr("general.acknowledge", "Entendido"), null);
}

async function showCustomChoice(title, msg, labelTrue = tr("general.confirm_yes", "Sí"), labelFalse = tr("general.no", "No")) {
    return await internalShowModal(title, msg, 'confirm', labelTrue, labelFalse);
}



// FUNCION NUEVA: Mostrar Modal de Instalacion FFmpeg con Deteccion de OS
function showFFmpegModal() {
    const modal = $("#ffmpeg-modal");
    const inputUrl = $("#ffmpeg-url-input");
    const instrText = $("#ffmpeg-instruction-os");

    const platform = window.navigator.platform.toLowerCase();
    let url = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
    let instr = "Abre el ZIP, entra a la carpeta <code>bin</code> y extrae <b>ffmpeg.exe</b> y <b>ffprobe.exe</b>.";

    if (platform.includes("mac")) {
        url = "https://evermeet.cx/ffmpeg/getrelease/zip";
        instr = "Abre el ZIP y extrae los archivos binarios <b>ffmpeg</b> y <b>ffprobe</b>.";
    } else if (platform.includes("linux")) {
        url = "https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-amd64-static.tar.xz";
        instr = "Descomprime el .tar.xz y extrae los archivos binarios <b>ffmpeg</b> y <b>ffprobe</b>.";
    }

    if (inputUrl) inputUrl.value = url;
    if (instrText) instrText.innerHTML = instr;

    if (modal) modal.style.display = "flex";
}

// Evento copiar enlace
const btnCopyLink = $("#btn-copy-link");
if (btnCopyLink) {
    btnCopyLink.addEventListener("click", () => {
        const inputUrl = $("#ffmpeg-url-input");
        if (inputUrl) {
            inputUrl.select();
            inputUrl.setSelectionRange(0, 99999); // Movil

            navigator.clipboard.writeText(inputUrl.value).then(() => {
                const originalText = btnCopyLink.textContent;
                btnCopyLink.textContent = "¡Copiado!";
                setTimeout(() => {
                    btnCopyLink.textContent = originalText;
                }, 2000);
            }).catch(err => {
                console.error("Error al copiar: ", err);
            });
        }
    });
}

// Evento cerrar modal FFmpeg
const btnCloseFfmpeg = $("#btn-close-ffmpeg");
if (btnCloseFfmpeg) {
    btnCloseFfmpeg.addEventListener("click", () => {
        const modal = $("#ffmpeg-modal");
        if (modal) modal.style.display = "none";
    });
}

// =========================================================================
// UI & OVERLAYS PROCESAMIENTO
// =========================================================================

function showProcessing(msg = "PROCESANDO...") {
    isLocalOperation = false;
    isCancellationRequested = false;
    const overlay = $("#processing-overlay");
    const txt = $("#processing-msg");
    const det = $("#processing-details");
    const bar = $("#overlay-progress-fill");
    const btnCancel = $("#btn-cancel-process");

    if (overlay && txt) {
        txt.textContent = normalizeBackendText(msg);
        txt.style.color = "";
        if (det) det.textContent = "Iniciando...";
        if (bar) bar.style.width = "0%";
        if (btnCancel) {
            btnCancel.disabled = false;
            btnCancel.style.opacity = "";
            btnCancel.style.cursor = "";
        }
        overlay.style.display = "flex";
    }
}
window.showProcessing = showProcessing;

function hideProcessing() {
    const overlay = $("#processing-overlay");
    if (overlay) overlay.style.display = "none";
    isCancellationRequested = false;
    const btnCancel = $("#btn-cancel-process");
    if (btnCancel) {
        btnCancel.disabled = false;
        btnCancel.style.opacity = "";
        btnCancel.style.cursor = "";
    }
}
window.hideProcessing = hideProcessing;

function isCancellationError(error) {
    const msg = String(error || "").toLowerCase();
    return isCancellationRequested || msg.includes("cancelad") || msg.includes("cancelled") || msg.includes("cancelling");
}

function showLocalProcessing(msg = "Calculando...") {
    isLocalOperation = true;
    const overlay = $("#local-processing-overlay");
    const txt = $("#local-msg");
    const pct = $("#local-pct");
    if (overlay && txt) {
        txt.textContent = msg;
        if (pct) pct.textContent = "0%";
        overlay.style.display = "flex";
    }
}

function hideLocalProcessing() {
    isLocalOperation = false;
    const overlay = $("#local-processing-overlay");
    if (overlay) overlay.style.display = "none";
}

const imgLoader = $("#img-loader");
function showImgLoader() { if (imgLoader) imgLoader.style.display = "flex"; }
function hideImgLoader() { if (imgLoader) imgLoader.style.display = "none"; }

function triggerStackSuccessEffect() {
    const img = $("#img-result");
    if (img) {
        img.classList.remove("img-reveal");
        void img.offsetWidth;
        img.classList.add("img-reveal");
    }
}

// =========================================================================
// REFERENCIAS DOM
// =========================================================================

const ui = {
    btnAnalyze: $("#btn-analyze"),
    btnBatchMode: $("#btn-batch-mode"),
    btnRunAnalysis: $("#btn-run-analysis"),
    btnStack: $("#btn-stack"),
    btnPlaceGrid: $("#btn-place-grid"),
    btnSmartGrid: $("#btn-smart-grid"),
    btnSavePng: $("#btn-save-png"),
    btnSaveTiff: $("#btn-save-tiff"),
    btnToggleLog: $("#btn-toggle-log"),

    analysisActions: $("#analysis-actions"),
    selectedFilename: $("#selected-filename"),
    selTargetCategory: $("#sel-target-category"),

    // Batch Panel
    panelBatch: $("#panel-batch"),
    batchSourcePath: $("#batch-source-path"),
    batchCount: $("#batch-count"),
    btnBatchTune: $("#btn-batch-tune"),
    btnBatchRun: $("#btn-batch-run"),
    selBatchType: $("#sel-batch-type"),
    selBatchTargetCategory: $("#sel-batch-target-category"),
    batchProgressText: $("#batch-progress-text"),
    batchProgressBar: $("#batch-progress-bar"),

    selBatchType: null,

    // Pasos de Batch
    batchStepAnalysis: $("#batch-step-analysis"),
    batchStepStacking: $("#batch-step-stacking"),
    batchStepExecution: $("#batch-step-execution"),

    // Anim Modal
    animModal: $("#animation-modal"),
    animPreviewImg: $("#anim-preview-img"),
    btnAnimExport: $("#btn-anim-export"),
    selAnimFormat: $("#anim-format"),
    selAnimRescale: $("#anim-rescale"),
    selAnimExportQuality: $("#anim-export-quality"),
    btnAnimCancel: $("#btn-anim-cancel"),
    btnAnimPrev: $("#btn-anim-prev"),
    btnAnimPlayPause: $("#btn-anim-playpause"),
    btnAnimNext: $("#btn-anim-next"),

    inputAnimSpeed: $("#anim-speed"),
    valAnimSpeed: $("#anim-speed-val"),
    animFrameCounter: $("#anim-frame-counter"),
    checkAnimBoomerang: $("#anim-boomerang"),
    selAnimFormat: $("#anim-format"),
    btnNormalize: $("#btn-anim-normalize"),

    // Controles Visor (Sliders + Inputs Numericos)
    btnAnimRotate: $("#btn-anim-rotate"),
    btnAnimResetFilters: $("#btn-anim-reset-filters"),

    slAnimBright: $("#sl-anim-bright"), numAnimBright: $("#num-anim-bright"),
    slAnimContrast: $("#sl-anim-contrast"), numAnimContrast: $("#num-anim-contrast"),
    slAnimSat: $("#sl-anim-sat"), numAnimSat: $("#num-anim-sat"),
    slAnimGamma: $("#sl-anim-gamma"), numAnimGamma: $("#num-anim-gamma"),
    slAnimHue: $("#sl-anim-hue"), numAnimHue: $("#num-anim-hue"),
    slAnimLvlBlack: $("#sl-anim-lvl-black"), numAnimLvlBlack: $("#num-anim-lvl-black"),
    slAnimLvlWhite: $("#sl-anim-lvl-white"), numAnimLvlWhite: $("#num-anim-lvl-white"),
    selAnimColorFilter: $("#sel-anim-color-filter"),
    slAnimTintR: $("#sl-anim-tint-r"), numAnimTintR: $("#num-anim-tint-r"),
    slAnimTintG: $("#sl-anim-tint-g"), numAnimTintG: $("#num-anim-tint-g"),
    slAnimTintB: $("#sl-anim-tint-b"), numAnimTintB: $("#num-anim-tint-b"),
    slAnimColorStrength: $("#sl-anim-color-strength"), numAnimColorStrength: $("#num-anim-color-strength"),
    slAnimHighlightProtect: $("#sl-anim-highlight-protect"), numAnimHighlightProtect: $("#num-anim-highlight-protect"),

    // Overlays
    selAnimOverlayMode: $("#sel-anim-overlay-mode"),
    selAnimFont: $("#sel-anim-font"),
    panelOverlayWatermark: $("#panel-overlay-watermark"),
    panelOverlayFrame: $("#panel-overlay-frame"),

    inputAnimWatermarkText: $("#input-anim-watermark-text"),
    slAnimWatermarkOp: $("#sl-anim-watermark-op"),
    numAnimWatermarkOp: $("#num-anim-watermark-op"),

    inputAnimFrameTitle: $("#input-anim-frame-title"),
    inputAnimFrameTele: $("#input-anim-frame-tele"),
    inputAnimFrameCam: $("#input-anim-frame-cam"),
    inputAnimFrameOther: $("#input-anim-frame-other"),

    // Animation Overlays Elements
    animOverlaysLayer: $("#anim-overlays-layer"),
    animWatermark: $("#anim-watermark"),
    animFrameTop: $("#anim-frame-top"),
    animFrameBottom: $("#anim-frame-bottom"),

    // Crop Anim
    btnAnimCropStart: $("#btn-anim-crop-start"),
    btnAnimCropConfirm: $("#btn-anim-crop-confirm"),
    btnAnimCropCancel: $("#btn-anim-crop-cancel"),
    animCropBox: $("#anim-crop-box"),
    animCropControls: $("#anim-crop-controls"),

    // Principal
    btnAutoPsf: $("#btn-auto-psf"),
    selAutoMode: $("#sel-auto-mode"),

    apCount: $("#ap-count"),
    multipointWrapper: $("#multipoint-wrapper"),
    statusText: $("#status-text"),
    pBarContainer: $("#progress-container"),
    pBarFill: $("#progress-fill"),

    panelInfo: $("#panel-info"),
    panelAnalysis: $("#panel-analysis"),
    panelWavelets: $("#panel-wavelets"),
    consolePanel: $("#console-panel"),
    logContainer: $("#log-container"),

    stackSlider: $("#stack-slider"),
    pctDisplay: $("#pct-display"),
    selAnalysisMode: $("#sel-analysis-mode"),
    alignMode: $("#align-mode"),
    drizzleScale: $("#drizzle-scale"),
    chkDoublePass: $("#chk-double-pass"),
    chkSharpened: $("#chk-sharpened"),
    sharpenIntensityContainer: $("#sharpen-intensity-container"),
    selSharpenIntensity: $("#sel-sharpen-intensity"),
    apSize: $("#ap-size"),
    apBright: $("#ap-bright"),

    // Sliders Post-Processing
    slDeconvSigma: $("#sl-deconv-sigma"), valDeconvSigma: $("#num-deconv-sigma"),
    slDeconvIter: $("#sl-deconv-iter"), valDeconvIter: $("#num-deconv-iter"),
    slVcSigma: $("#sl-vc-sigma"), valVcSigma: $("#num-vc-sigma"),
    slVcIter: $("#sl-vc-iter"), valVcIter: $("#num-vc-iter"),

    slUsmAmt: $("#sl-usm-amt"), valUsmAmt: $("#num-usm-amt"),
    slUsmRad: $("#sl-usm-rad"), valUsmRad: $("#num-usm-rad"),
    slLceAmt: $("#sl-lce-amt"), valLceAmt: $("#num-lce-amt"),
    slMasterDenoise: $("#sl-master-denoise"), numMasterDenoise: $("#num-master-denoise"),
    slDenoiseDetail: $("#sl-denoise-detail"), numDenoiseDetail: $("#num-denoise-detail"),
    slDenoiseChroma: $("#sl-denoise-chroma"), numDenoiseChroma: $("#num-denoise-chroma"),

    u1: $("#u1"), u2: $("#u2"), u3: $("#u3"), u4: $("#u4"), u5: $("#u5"),
    w1: $("#w1"), w2: $("#w2"), w3: $("#w3"), w4: $("#w4"), w5: $("#w5"), w6: $("#w6"),
    d1: $("#d1"), d2: $("#d2"), d3: $("#d3"), d4: $("#d4"), d5: $("#d5"), d6: $("#d6"),

    slCrisp: $("#sl-crisp"), valCrisp: $("#num-crisp"),
    slGamma: $("#sl-gamma"), valGamma: $("#num-gamma"),
    slSat: $("#sl-sat"), valSat: $("#num-sat"),
    slContrast: $("#sl-contrast"), numContrast: $("#num-contrast"),
    slBrightness: $("#sl-brightness"), numBrightness: $("#num-brightness"),
    slRBal: $("#sl-r-bal"), numRBal: $("#num-r-bal"),
    slBBal: $("#sl-b-bal"), numBBal: $("#num-b-bal"),

    // New Deringing Canvas
    selDrMode: $("#sel-deringing-mode"),
    panelDrManual: $("#panel-deringing-manual"),
    slDrRad: $("#sl-dr-rad"), numDrRad: $("#num-dr-rad"),
    slDrDark: $("#sl-dr-dark"), numDrDark: $("#num-dr-dark"),
    slDrLight: $("#sl-dr-light"), numDrLight: $("#num-dr-light"),
    chkDrMask: $("#chk-dr-mask"),

    rx: $("#rx"), ry: $("#ry"), bx: $("#bx"), by: $("#by"),

    blendSlider: $("#blend-slider"),
    blendDisplay: $("#blend-display"),

    viewport: $("#main-viewport"),
    viewSource: $("#view-source"),
    viewResult: $("#view-result"),
    imgSource: $("#img-source"),
    imgResult: $("#img-result"),
    gridOverlay: $("#grid-overlay"),

    iRes: $("#info-res"), iFrames: $("#info-frames"), iPat: $("#info-pat"),
    statAvgQual: $("#stat-avg-qual"), statStability: $("#stat-stability"),
    statWorst: $("#stat-worst-score"), statBest: $("#stat-best-score"),

    panelTools: $("#panel-tools"),
    toolsMainView: $("#tools-main-view"),
    cropControls: $("#crop-controls"),
    cropInstr: $("#crop-instr"),
    btnStartCrop: $("#btn-start-crop"),
    btnConfirmCrop: $("#btn-confirm-crop"),
    btnCancelCrop: $("#btn-cancel-crop"),
    btnConfirmCrop: $("#btn-confirm-crop"),
    btnCancelCrop: $("#btn-cancel-crop"),
    cropBox: $("#crop-box"),
    chartModeSwitch: $("#chart-mode-switch"),

    // Manual Tint UI
    slAnimTintR: $("#sl-anim-tint-r"), numAnimTintR: $("#num-anim-tint-r"),
    slAnimTintG: $("#sl-anim-tint-g"), numAnimTintG: $("#num-anim-tint-g"),
    slAnimTintB: $("#sl-anim-tint-b"), numAnimTintB: $("#num-anim-tint-b"),
    selBayerOverride: $("#sel-bayer-override"),
    selColorSpaceOverride: $("#sel-color-space-override"),
};

// =========================================================================
// LOGICA DE REPRODUCTOR DE ANIMACION
// =========================================================================

// Helper to Lock UI during Batch
function setBatchModeUI(isActive) {
    const cards = document.querySelectorAll(".sidebar .card");
    const header = document.querySelector("header");

    if (isActive) {
        if (header) header.classList.add("disabled-ui");
        cards.forEach(card => {
            if (card.id !== "panel-batch") {
                card.classList.add("disabled-ui");
            } else {
                card.classList.add("batch-glow-active");
            }
        });
    } else {
        if (header) header.classList.remove("disabled-ui");
        cards.forEach(card => {
            card.classList.remove("disabled-ui");
            card.classList.remove("batch-glow-active");
        });
    }
}


// Globales para Gestion de Frames
let animFullSourceFrames = [];
let animExcludedIndices = new Set();

const ANIM_FILTER_DEFAULTS = {
    rotation: 0,
    brightness: 0.0,
    contrast: 1.0,
    saturation: 1.0,
    gamma: 1.0,
    hue: 0,
    levelsBlack: 0.0,
    levelsWhite: 1.0,
    colorFilter: "none",
    colorStrength: 1.0,
    highlightProtect: 0.35,
    tint: { r: 255, g: 255, b: 255 }
};

const ANIM_SOLAR_PRESETS = {
    "solar-ha-gold": {
        brightness: 0.0,
        contrast: 1.18,
        saturation: 1.25,
        gamma: 1.08,
        hue: 0,
        levelsBlack: 0.02,
        levelsWhite: 1.0,
        colorStrength: 0.9,
        highlightProtect: 0.55,
        tint: { r: 255, g: 255, b: 255 }
    }
};

function setAnimControlPair(slider, number, value, decimals = 1) {
    if (slider) slider.value = String(value);
    if (number) number.value = Number(value).toFixed(decimals);
}

function syncAnimationFilterControls() {
    setAnimControlPair(ui.slAnimBright, ui.numAnimBright, animFilters.brightness, 1);
    setAnimControlPair(ui.slAnimContrast, ui.numAnimContrast, animFilters.contrast, 1);
    setAnimControlPair(ui.slAnimSat, ui.numAnimSat, animFilters.saturation, 1);
    setAnimControlPair(ui.slAnimGamma, ui.numAnimGamma, animFilters.gamma, 1);
    setAnimControlPair(ui.slAnimHue, ui.numAnimHue, animFilters.hue, 0);
    setAnimControlPair(ui.slAnimLvlBlack, ui.numAnimLvlBlack, animFilters.levelsBlack, 2);
    setAnimControlPair(ui.slAnimLvlWhite, ui.numAnimLvlWhite, animFilters.levelsWhite, 2);
    setAnimControlPair(ui.slAnimColorStrength, ui.numAnimColorStrength, animFilters.colorStrength ?? 1.0, 2);
    setAnimControlPair(ui.slAnimHighlightProtect, ui.numAnimHighlightProtect, animFilters.highlightProtect ?? 0.35, 2);

    if (ui.selAnimColorFilter) ui.selAnimColorFilter.value = animFilters.colorFilter || "none";

    const tint = animFilters.tint || { r: 255, g: 255, b: 255 };
    if (ui.slAnimTintR) ui.slAnimTintR.value = tint.r;
    if (ui.numAnimTintR) ui.numAnimTintR.value = tint.r;
    if (ui.slAnimTintG) ui.slAnimTintG.value = tint.g;
    if (ui.numAnimTintG) ui.numAnimTintG.value = tint.g;
    if (ui.slAnimTintB) ui.slAnimTintB.value = tint.b;
    if (ui.numAnimTintB) ui.numAnimTintB.value = tint.b;
}

function resetAnimationEditorSettings({ keepPlayback = true } = {}) {
    animFilters = JSON.parse(JSON.stringify(ANIM_FILTER_DEFAULTS));
    animOverlays = {
        mode: "none",
        wmText: "",
        wmOpacity: 0.5,
        frTitle: "",
        frTele: "",
        frCam: "",
        frOther: "",
        font: "Arial"
    };

    animCropExportRect = null;
    animCropDisplaySelection = null;
    animCropSelection = { x: 0, y: 0, w: 0, h: 0 };
    exitAnimationCropMode(keepPlayback);

    syncAnimationSpeedUI(100);
    if (ui.checkAnimBoomerang) ui.checkAnimBoomerang.checked = false;
    animState.isBoomerang = false;
    animState.direction = 1;

    syncAnimationFilterControls();

    if (ui.selAnimOverlayMode) ui.selAnimOverlayMode.value = "none";
    if (ui.panelOverlayWatermark) ui.panelOverlayWatermark.style.display = "none";
    if (ui.panelOverlayFrame) ui.panelOverlayFrame.style.display = "none";
    if (ui.inputAnimWatermarkText) ui.inputAnimWatermarkText.value = "";
    setAnimControlPair(ui.slAnimWatermarkOp, ui.numAnimWatermarkOp, 50, 0);
    if (ui.inputAnimFrameTitle) ui.inputAnimFrameTitle.value = "";
    if (ui.inputAnimFrameTele) ui.inputAnimFrameTele.value = "";
    if (ui.inputAnimFrameCam) ui.inputAnimFrameCam.value = "";
    if (ui.inputAnimFrameOther) ui.inputAnimFrameOther.value = "";
    if (ui.selAnimFont) ui.selAnimFont.value = "Arial";
    if (ui.selAnimFormat) ui.selAnimFormat.value = "mp4";
    if (ui.selAnimRescale) ui.selAnimRescale.value = "1.0";
    if (ui.selAnimExportQuality) ui.selAnimExportQuality.value = "balanced";

    updateAnimVisuals();
}

function clearAnimationTimer() {
    if (animState.timerId) {
        clearTimeout(animState.timerId);
        animState.timerId = null;
    }
}

function readAnimationSpeed() {
    const input = ui.inputAnimSpeed;
    const min = parseInt(input?.min || "20", 10);
    const max = parseInt(input?.max || "500", 10);
    let speed = parseInt(input?.value || animState.speedMs || "100", 10);
    if (Number.isNaN(speed)) speed = 100;
    return Math.max(min, Math.min(max, speed));
}

function syncAnimationSpeedUI(speed = readAnimationSpeed()) {
    animState.speedMs = speed;
    if (ui.inputAnimSpeed) ui.inputAnimSpeed.value = String(speed);
    if (ui.valAnimSpeed) {
        ui.valAnimSpeed.textContent = trFormat("animation.speed_value", { ms: speed }, `${speed}ms`);
    }
    return speed;
}

function updateAnimationPlayButton() {
    const icon = ui.btnAnimPlayPause?.querySelector("span") || ui.btnAnimPlayPause;
    if (icon) icon.textContent = animState.isPlaying ? "⏸" : "▶";
}

function setAnimationPlaying(playing) {
    animState.isPlaying = !!playing;
    updateAnimationPlayButton();
    clearAnimationTimer();
    if (animState.isPlaying && animState.images.length > 0) {
        scheduleNextFrame();
    }
}

function updateAnimationFrameCounter() {
    if (!ui.animFrameCounter) return;
    const total = animState.images.length;
    const current = total > 0 ? Math.min(animState.frameIndex + 1, total) : 0;
    ui.animFrameCounter.textContent = trFormat(
        "animation.frame_counter",
        { current, total },
        `Frame ${current} / ${total}`
    );
}

function getAcceptedAnimationFrames(sourceFrames = animFullSourceFrames) {
    return sourceFrames.filter((_, index) => !animExcludedIndices.has(index));
}

function refreshAnimationFromFullFrames(allFrames, keepFilters = true) {
    animFullSourceFrames = [...allFrames];
    const acceptedFrames = getAcceptedAnimationFrames(allFrames);
    startAnimationPlayer(acceptedFrames.length > 0 ? acceptedFrames : allFrames, keepFilters);
}

function advanceAnimationFrameIndex() {
    if (animState.images.length <= 0) return;
    if (animState.isBoomerang) {
        let nextIdx = animState.frameIndex + animState.direction;
        if (nextIdx >= animState.images.length) {
            nextIdx = animState.images.length - 2;
            animState.direction = -1;
            if (nextIdx < 0) nextIdx = 0;
        } else if (nextIdx < 0) {
            nextIdx = 1;
            animState.direction = 1;
            if (nextIdx >= animState.images.length) nextIdx = 0;
        }
        animState.frameIndex = nextIdx;
    } else {
        animState.frameIndex = (animState.frameIndex + 1) % animState.images.length;
    }
}

function startAnimationPlayer(imageDataList, keepFilters = false) {
    clearAnimationTimer();

    // 1. Capture current settings BEFORE partial resets
    let prevSpeed = 100;
    let prevBoomerang = false;
    let prevDirection = 1;
    let prevFrameIndex = 0;

    if (keepFilters) {
        // Read from DOM explicitly to ensure we look at the real current UI state
        // Bypassing 'ui' object just in case the reference is stale or mapped incorrectly
        const domSpeedInput = document.getElementById("anim-speed");
        const domBoomerang = document.getElementById("anim-boomerang");

        prevSpeed = domSpeedInput ? (parseInt(domSpeedInput.value) || 100) : (animState.speedMs || 100);
        prevBoomerang = domBoomerang ? domBoomerang.checked : ui.checkAnimBoomerang.checked;

        prevDirection = animState.direction || 1;
        // Try to keep frame index if possible, though mostly we reset to 0 on major changes
        prevFrameIndex = animState.frameIndex || 0;
    }

    // Si no se mantienen filtros (nueva carga), reiniciamos la fuente completa y exclusiones
    if (!keepFilters) {
        animFullSourceFrames = [...imageDataList];
        animExcludedIndices.clear();
    } else {
        // Fix: If keeping filters (e.g. Normalize, Realign), we MUST update the source frames reference
        // so the Frame Manager shows the new images, while preserving exclusions.
        // We assume 1-to-1 mapping if length matches.
        if (animFullSourceFrames.length === imageDataList.length) {
            animFullSourceFrames = [...imageDataList];
        }
    }

    animState.images = imageDataList.filter(Boolean).map(item => {
        if (typeof item === 'string' && item.startsWith("data:")) {
            return item;
        } else {
            return convertFileSrc(item);
        }
    });

    if (animState.images.length === 0) {
        showCustomAlert(tr("general.error", "Error"), tr("animation.errors.no_valid_images", "No se generaron imagenes validas para reproducir."));
        return;
    }

    // 2. Restore or Reset State
    animState.frameIndex = keepFilters ? (prevFrameIndex < animState.images.length ? prevFrameIndex : 0) : 0;
    animState.direction = keepFilters ? prevDirection : 1;
    animState.isPlaying = true;
    updateAnimationPlayButton();

    // Persist Speed & Boomerang
    if (keepFilters) {
        syncAnimationSpeedUI(prevSpeed);
        animState.isBoomerang = prevBoomerang;
        // Ensure UI matches state (just in case)
        if (ui.checkAnimBoomerang) ui.checkAnimBoomerang.checked = prevBoomerang;
    } else {
        syncAnimationSpeedUI();
        animState.isBoomerang = ui.checkAnimBoomerang.checked;
    }

    if (!keepFilters) {
        resetAnimationEditorSettings({ keepPlayback: false });
    } else {
        // Restore UI values from current state if keeping filters
        syncAnimationFilterControls();
    }

    updateAnimVisuals();
    ui.animModal.style.display = "flex";
    showCurrentFrame();
    scheduleNextFrame();
}

function defaultAnimationExportButtonHtml() {
    return tr(
        "animation.export.file_button",
        "<svg class='zas-icon'><use href='#icon-save'></use></svg> Exportar archivo"
    );
}

function restoreAnimationExportButtonState(fallbackHtml = null) {
    if (ui.btnAnimExport) {
        ui.btnAnimExport.disabled = false;
        ui.btnAnimExport.innerHTML = fallbackHtml || defaultAnimationExportButtonHtml();
    }
    if (ui.btnAnimCancel) ui.btnAnimCancel.disabled = false;
}

function exitAnimationCropMode(resumePlayback = true) {
    isAnimCropping = false;
    isAnimDrawing = false;
    isAnimMoving = false;
    isAnimResizing = false;
    animResizeDir = "";

    if (ui.animCropControls) ui.animCropControls.style.display = "none";
    if (ui.btnAnimCropStart) ui.btnAnimCropStart.style.display = "block";
    if (ui.animCropBox) ui.animCropBox.style.display = "none";

    const container = document.querySelector("#anim-image-container");
    if (container) container.style.cursor = "default";

    applyAnimationCropPreview();
    if (resumePlayback) setAnimationPlaying(true);
}

function clearAnimationCropPreviewStyles() {
    const container = document.querySelector("#anim-image-container");
    const img = ui.animPreviewImg;
    if (container) {
        container.classList.remove("anim-crop-preview-active");
        container.style.width = "";
        container.style.height = "";
        container.style.overflow = "";
    }
    if (img) {
        img.style.position = "";
        img.style.left = "";
        img.style.top = "";
        img.style.width = "";
        img.style.height = "";
        img.style.maxWidth = "100%";
        img.style.maxHeight = "80vh";
        img.style.objectFit = "contain";
    }
}

function applyAnimationCropPreview(forceFullImage = false) {
    const container = document.querySelector("#anim-image-container");
    const img = ui.animPreviewImg;
    const viewport = ui.animViewport || document.querySelector("#anim-viewport");

    if (!container || !img || !viewport || forceFullImage || isAnimCropping || !animCropExportRect) {
        clearAnimationCropPreviewStyles();
        return;
    }

    if (!img.naturalWidth || !img.naturalHeight) return;

    const crop = animCropExportRect;
    const cropW = Math.max(2, crop.w * img.naturalWidth);
    const cropH = Math.max(2, crop.h * img.naturalHeight);
    const aspect = cropW / cropH;
    const maxW = Math.max(120, viewport.clientWidth - 12);
    const maxH = Math.max(120, viewport.clientHeight - 12);

    let displayW = maxW;
    let displayH = displayW / aspect;
    if (displayH > maxH) {
        displayH = maxH;
        displayW = displayH * aspect;
    }

    const fullDisplayW = displayW / crop.w;
    const fullDisplayH = displayH / crop.h;

    container.classList.add("anim-crop-preview-active");
    container.style.width = `${displayW}px`;
    container.style.height = `${displayH}px`;
    container.style.overflow = "hidden";

    img.style.position = "absolute";
    img.style.maxWidth = "none";
    img.style.maxHeight = "none";
    img.style.objectFit = "fill";
    img.style.width = `${fullDisplayW}px`;
    img.style.height = `${fullDisplayH}px`;
    img.style.left = `${-crop.x * fullDisplayW}px`;
    img.style.top = `${-crop.y * fullDisplayH}px`;
}

function stopAnimationPlayer() {
    hideProcessing();
    restoreAnimationExportButtonState();
    setAnimationPlaying(false);
    exitAnimationCropMode(false);
    clearAnimationCropPreviewStyles();
    if (typeof closeFrameManager === "function") closeFrameManager();

    if (ui.animModal) ui.animModal.style.display = "none";
    if (ui.animPreviewImg) {
        ui.animPreviewImg.src = "";
        ui.animPreviewImg.style.transform = "";
        ui.animPreviewImg.style.filter = "";
    }
}

function clamp01(value) {
    return Math.max(0, Math.min(1, Number(value) || 0));
}

function solarHaGoldChannelValue(gray, channelValue, colorStrength, highlightProtect) {
    const strength = clamp01(colorStrength);
    const protect = clamp01(highlightProtect);
    const highlightWeight = clamp01((gray - 0.68) / 0.32) * protect;
    const protectedChannel = channelValue * (1 - highlightWeight) + gray * highlightWeight;
    return gray * (1 - strength) + protectedChannel * strength;
}

function solarHaGoldTables(colorStrength, highlightProtect) {
    const grayStops = [0.0, 0.25, 0.5, 0.75, 1.0];
    const base = {
        r: [0.05, 0.24, 0.58, 0.95, 1.0],
        g: [0.00, 0.06, 0.28, 0.76, 1.0],
        b: [0.00, 0.00, 0.02, 0.06, 0.55]
    };
    const toTable = channel => grayStops
        .map((gray, index) => solarHaGoldChannelValue(gray, base[channel][index], colorStrength, highlightProtect).toFixed(3))
        .join(" ");

    return {
        r: toTable("r"),
        g: toTable("g"),
        b: toTable("b")
    };
}

function updateAnimVisuals() {
    if (!ui.animPreviewImg) return;

    // A. Basic CSS Filters (Brightness, Contrast, Hue, Saturation)
    // Note: Saturation/Hue are applied via CSS filter first.
    // If we want them to interact with Tint (which makes image mono/colored), order matters.
    // Current CSS order: brightness -> contrast -> hue -> saturate -> url(svg).
    // This means Saturation applies to initial image.
    // However, if we use SVG for Tint, the output of SVG is tint.
    // If we want Saturation slider to affect the TINTED result (e.g. make the red solar filter more intense),
    // we should strictly probably move saturation to SVG or apply it after.
    // But for now, let's keep it simple as implemented in CSS.

    let totalBright = (1.0 + animFilters.brightness);
    let gamma = animFilters.gamma || 1.0;

    // Gamma as brightness multiplier (simple approximation for user preference)
    if (gamma !== 1.0) {
        totalBright = totalBright * gamma;
    }

    let filterStr = `brightness(${totalBright}) contrast(${animFilters.contrast}) hue-rotate(${animFilters.hue}deg) saturate(${animFilters.saturation})`;

    // B. Advanced SVG Filters (Tint & Levels)
    let r = 1, g = 1, b = 1;
    const colorStrength = clamp01(animFilters.colorStrength ?? 1.0);
    const highlightProtect = clamp01(animFilters.highlightProtect ?? 0.35);

    // 1. Calculate Tint Factors
    const isSolarGoldPreset = animFilters.colorFilter === "solar-ha-gold";
    if (animFilters.colorFilter !== "none") {
        switch (animFilters.colorFilter) {
            case "solar-ha-gold": r = 1.0; g = 1.0; b = 1.0; break;
            case "solar-orange": r = 1.0; g = 0.6; b = 0.2; break;
            case "solar-yellow": r = 1.0; g = 0.9; b = 0.3; break;
            case "h-alpha": r = 1.0; g = 0.2; b = 0.2; break;
            case "calcium-k": r = 0.3; g = 0.2; b = 1.0; break;
        }
    } else {
        r = (animFilters.tint?.r ?? 255) / 255.0;
        g = (animFilters.tint?.g ?? 255) / 255.0;
        b = (animFilters.tint?.b ?? 255) / 255.0;
    }

    if (!isSolarGoldPreset) {
        r = 1 + (r - 1) * colorStrength;
        g = 1 + (g - 1) * colorStrength;
        b = 1 + (b - 1) * colorStrength;
    }

    // 2. Levels Calculation
    let lvlBlack = animFilters.levelsBlack !== undefined ? animFilters.levelsBlack : 0.0;
    let lvlWhite = animFilters.levelsWhite !== undefined ? animFilters.levelsWhite : 1.0;

    // Clamp to avoid inversion or div/0
    if (lvlBlack < 0) lvlBlack = 0;
    if (lvlWhite > 1) lvlWhite = 1;
    if (lvlBlack >= lvlWhite - 0.05) lvlBlack = lvlWhite - 0.05; // Maintain gap

    let slope = 1.0;
    let intercept = 0.0;
    const range = lvlWhite - lvlBlack;
    if (range > 0.001) {
        slope = 1.0 / range;
        intercept = -lvlBlack * slope;
    }

    const svgFilter = document.querySelector("#anim-advanced-filter");
    if (svgFilter) {
        // Update Tint Matrix
        const matrixEl = svgFilter.querySelector("feColorMatrix");
        if (matrixEl) {
            const matrix = isSolarGoldPreset
                ? `0.299 0.587 0.114 0 0  0.299 0.587 0.114 0 0  0.299 0.587 0.114 0 0  0 0 0 1 0`
                : `${r} 0 0 0 0  0 ${g} 0 0 0  0 0 ${b} 0 0  0 0 0 1 0`;
            matrixEl.setAttribute("values", matrix);
        }

        // Update Levels Component Transfer
        const funcR = svgFilter.querySelector("#lvl-r");
        const funcG = svgFilter.querySelector("#lvl-g");
        const funcB = svgFilter.querySelector("#lvl-b");

        if (funcR && funcG && funcB) {
            if (isSolarGoldPreset) {
                const tables = solarHaGoldTables(colorStrength, highlightProtect);
                funcR.setAttribute("type", "table");
                funcG.setAttribute("type", "table");
                funcB.setAttribute("type", "table");
                funcR.setAttribute("tableValues", tables.r);
                funcG.setAttribute("tableValues", tables.g);
                funcB.setAttribute("tableValues", tables.b);
                [funcR, funcG, funcB].forEach(fn => {
                    fn.removeAttribute("slope");
                    fn.removeAttribute("intercept");
                });
            } else {
                [funcR, funcG, funcB].forEach(fn => {
                    fn.setAttribute("type", "linear");
                    fn.removeAttribute("tableValues");
                });
                funcR.setAttribute("slope", slope); funcR.setAttribute("intercept", intercept);
                funcG.setAttribute("slope", slope); funcG.setAttribute("intercept", intercept);
                funcB.setAttribute("slope", slope); funcB.setAttribute("intercept", intercept);
            }
        }

        // Apply URL filter if ANY advanced setting is active
        const isTintActive = (Math.abs(r - 1) > 0.01 || Math.abs(g - 1) > 0.01 || Math.abs(b - 1) > 0.01);
        const isLevelsActive = (lvlBlack > 0.01 || lvlWhite < 0.99);

        if (isSolarGoldPreset || isTintActive || isLevelsActive) {
            filterStr += ` url(#anim-advanced-filter)`;
        }
    }

    // Apply Filters and Rotation
    ui.animPreviewImg.style.transform = `rotate(${animFilters.rotation}deg)`;
    ui.animPreviewImg.style.filter = filterStr;

    // C. Overlays Updates (HTML Real-time)
    if (ui.animOverlaysLayer) {
        if (animOverlays.mode === "none") {
            ui.animOverlaysLayer.style.display = "none";
        } else {
            ui.animOverlaysLayer.style.display = "block";

            // Watermark Logic
            if (animOverlays.mode === "watermark") {
                if (ui.animWatermark) {
                    ui.animWatermark.style.display = "block";
                    ui.animWatermark.textContent = animOverlays.wmText;
                    ui.animWatermark.style.opacity = animOverlays.wmOpacity;
                    ui.animWatermark.style.fontFamily = animOverlays.font;
                }
                if (ui.animFrameTop) ui.animFrameTop.style.display = "none";
                if (ui.animFrameBottom) ui.animFrameBottom.style.display = "none";
            }
            // Frame Logic
            else if (animOverlays.mode === "frame") {
                if (ui.animWatermark) ui.animWatermark.style.display = "none";

                // Unified Footer Logic (Title + Info)
                if (ui.animFrameTop) ui.animFrameTop.style.display = "none";

                if (ui.animFrameBottom) {
                    ui.animFrameBottom.style.display = "flex";
                    // Build Frame Info String
                    let info = [];
                    if (animOverlays.frTele) info.push(`🔭 ${animOverlays.frTele}`);
                    if (animOverlays.frCam) info.push(`📷 ${animOverlays.frCam}`);
                    if (animOverlays.frOther) info.push(animOverlays.frOther);

                    let titleHtml = `<div style="font-family:${animOverlays.font}; font-weight:bold; font-size:1.4em; margin-bottom:4px;">${animOverlays.frTitle}</div>`;
                    let infoHtml = `<div style="font-family:${animOverlays.font}; font-size:1.0em;">${info.join(" | ")}</div>`;

                    ui.animFrameBottom.innerHTML = titleHtml + infoHtml;
                }
            }
        }
    }
}

function scheduleNextFrame() {
    clearAnimationTimer();
    if (!animState.isPlaying || animState.images.length === 0) return;

    const currentSpeed = syncAnimationSpeedUI();
    animState.timerId = setTimeout(() => {
        animState.timerId = null;
        if (!animState.isPlaying) return;
        advanceAnimationFrameIndex();
        showCurrentFrame();
        scheduleNextFrame();
    }, currentSpeed);
}

if (ui.inputAnimSpeed) {
    ui.inputAnimSpeed.addEventListener("input", () => {
        syncAnimationSpeedUI();
        if (animState.isPlaying) scheduleNextFrame();
    });
}
if (ui.checkAnimBoomerang) {
    ui.checkAnimBoomerang.addEventListener("change", (e) => {
        animState.isBoomerang = e.target.checked;
        if (!animState.isBoomerang) animState.direction = 1;
    });
}
if (ui.btnAnimRotate) {
    ui.btnAnimRotate.addEventListener("click", () => {
        animFilters.rotation = (animFilters.rotation + 90) % 360;
        updateAnimVisuals();
    });
}

if (ui.btnAnimResetFilters) {
    ui.btnAnimResetFilters.addEventListener("click", () => {
        resetAnimationEditorSettings({ keepPlayback: true });
    });
}

// EVENTOS DE OVERLAYS
if (ui.selAnimOverlayMode) {
    ui.selAnimOverlayMode.addEventListener("change", (e) => {
        animOverlays.mode = e.target.value;
        if (animOverlays.mode === "none") {
            ui.panelOverlayWatermark.style.display = "none";
            ui.panelOverlayFrame.style.display = "none";
        } else if (animOverlays.mode === "watermark") {
            ui.panelOverlayWatermark.style.display = "block";
            ui.panelOverlayFrame.style.display = "none";
        } else if (animOverlays.mode === "frame") {
            ui.panelOverlayWatermark.style.display = "none";
            ui.panelOverlayFrame.style.display = "block";
        }
        updateAnimVisuals();
    });
}

if (ui.inputAnimWatermarkText) {
    ui.inputAnimWatermarkText.addEventListener("input", (e) => { animOverlays.wmText = e.target.value; updateAnimVisuals(); });
}
if (ui.inputAnimFrameTitle) {
    ui.inputAnimFrameTitle.addEventListener("input", (e) => { animOverlays.frTitle = e.target.value; updateAnimVisuals(); });
}
if (ui.inputAnimFrameTele) {
    ui.inputAnimFrameTele.addEventListener("input", (e) => { animOverlays.frTele = e.target.value; updateAnimVisuals(); });
}
if (ui.inputAnimFrameCam) {
    ui.inputAnimFrameCam.addEventListener("input", (e) => { animOverlays.frCam = e.target.value; updateAnimVisuals(); });
}
if (ui.inputAnimFrameOther) {
    ui.inputAnimFrameOther.addEventListener("input", (e) => { animOverlays.frOther = e.target.value; updateAnimVisuals(); });
}
if (ui.selAnimFont) {
    ui.selAnimFont.addEventListener("change", (e) => { animOverlays.font = e.target.value; updateAnimVisuals(); });
}

// Vinculacion de controles de Visor (Slider <-> Input)
function linkAnimControl(sliderId, numberId, filterKey, decimals = 1) {
    const sl = $(sliderId);
    const num = $(numberId);
    if (!sl || !num) return;

    sl.addEventListener("input", () => {
        let val = parseFloat(sl.value);
        num.value = val.toFixed(decimals);
        if (filterKey === 'opacity') animOverlays.wmOpacity = val / 100.0;
        else animFilters[filterKey] = val;
        updateAnimVisuals();
    });

    num.addEventListener("change", () => {
        let val = parseFloat(num.value);
        if (isNaN(val)) val = 0;
        sl.value = val;
        if (filterKey === 'opacity') animOverlays.wmOpacity = val / 100.0;
        else animFilters[filterKey] = val;
        updateAnimVisuals();
    });
}

linkAnimControl("#sl-anim-bright", "#num-anim-bright", "brightness");
linkAnimControl("#sl-anim-contrast", "#num-anim-contrast", "contrast");
linkAnimControl("#sl-anim-sat", "#num-anim-sat", "saturation");
// linkAnimControl("#sl-anim-gamma", "#num-anim-gamma", "gamma");
// Custom Logic: Direct Gamma (Slider Right = Higher Number = Brighter)
// We treat "Gamma" here as a brightness multiplier for user intuition.
linkAnimControl("#sl-anim-gamma", "#num-anim-gamma", "gamma");
linkAnimControl("#sl-anim-lvl-black", "#num-anim-lvl-black", "levelsBlack", 2);
linkAnimControl("#sl-anim-lvl-white", "#num-anim-lvl-white", "levelsWhite", 2);
linkAnimControl("#sl-anim-color-strength", "#num-anim-color-strength", "colorStrength", 2);
linkAnimControl("#sl-anim-highlight-protect", "#num-anim-highlight-protect", "highlightProtect", 2);

// Deprecated Inverted Logic Removed
/*
const slAnimGamma = $("#sl-anim-gamma");
const numAnimGamma = $("#num-anim-gamma");
if (slAnimGamma && numAnimGamma) {
    // ... removed ...
}
*/

linkAnimControl("#sl-anim-hue", "#num-anim-hue", "hue");
linkAnimControl("#sl-anim-watermark-op", "#num-anim-watermark-op", "opacity");

// Manuales para Levels y Color Filter
if (ui.numAnimLvlBlack) ui.numAnimLvlBlack.addEventListener("change", (e) => { animFilters.levelsBlack = parseFloat(e.target.value); updateAnimVisuals(); });
if (ui.numAnimLvlWhite) ui.numAnimLvlWhite.addEventListener("change", (e) => { animFilters.levelsWhite = parseFloat(e.target.value); updateAnimVisuals(); });
if (ui.selAnimColorFilter) ui.selAnimColorFilter.addEventListener("change", (e) => {
    animFilters.colorFilter = e.target.value;
    const preset = ANIM_SOLAR_PRESETS[animFilters.colorFilter];
    if (preset) {
        Object.assign(animFilters, JSON.parse(JSON.stringify(preset)), {
            colorFilter: animFilters.colorFilter
        });
        syncAnimationFilterControls();
    }
    updateAnimVisuals();
});

// Manual RGB Tint Listeners
function linkTintControl(slId, numId, colorKey) {
    const sl = $(slId); const num = $(numId);
    if (!sl || !num) return;
    const upd = (val) => {
        if (!animFilters.tint) animFilters.tint = { r: 255, g: 255, b: 255 };
        animFilters.tint[colorKey] = val;
        updateAnimVisuals();
    };
    sl.addEventListener("input", () => { num.value = sl.value; upd(parseInt(sl.value)); });
    num.addEventListener("change", () => {
        let v = parseInt(num.value); if (v < 0) v = 0; if (v > 255) v = 255;
        sl.value = v; upd(v);
    });
}
linkTintControl("#sl-anim-tint-r", "#num-anim-tint-r", "r");
linkTintControl("#sl-anim-tint-g", "#num-anim-tint-g", "g");
linkTintControl("#sl-anim-tint-b", "#num-anim-tint-b", "b");


    if (ui.btnNormalize) {
    ui.btnNormalize.addEventListener("click", async () => {
        if (!batchResultPaths || batchResultPaths.length === 0) {
            showCustomAlert(tr("general.error", "Error"), tr("animation.errors.no_source_files", "No hay archivos de origen disponibles para procesar."));
            return;
        }
        const confirm = await showCustomChoice(
            i18n.t("animation.normalize_confirm_title"),
            i18n.t("animation.normalize_confirm_message"),
            i18n.t("general.confirm_yes"),
            i18n.t("general.cancel")
        );
        if (!confirm) return;

        ui.btnNormalize.disabled = true;
        setAnimationPlaying(false);
        showProcessing(tr("animation.normalize_processing", "NORMALIZANDO BRILLO..."));

        try {
            const normalizedBase64List = await invoke("normalize_batch_brightness", { paths: batchResultPaths });
            if (normalizedBase64List && normalizedBase64List.length > 0) {
                batchGeneratedImages = normalizedBase64List;
                refreshAnimationFromFullFrames(batchGeneratedImages, true);
                log("SUCCESS", i18n.t("animation.normalize_success_log"));
                showCustomAlert(i18n.t("general.ready"), i18n.t("animation.normalize_success_message"));
            }
        } catch (e) {
            log("ERROR", "Normalizacion fallida: " + e);
            showCustomAlert(tr("general.error", "Error"), trFormat("animation.normalize_error", { error: e }, "Fallo al normalizar brillo: " + e));
            setAnimationPlaying(true);
        } finally {
            hideProcessing();
            ui.btnNormalize.disabled = false;
        }
    });

    // PHASE 40: Animation Playback Controls
    ui.btnAnimPlayPause.addEventListener("click", () => {
        setAnimationPlaying(!animState.isPlaying);
    });

    ui.btnAnimPrev.addEventListener("click", () => {
        setAnimationPlaying(false);

        animState.frameIndex--;
        if (animState.frameIndex < 0) {
            animState.frameIndex = animState.images.length - 1;
        }
        showCurrentFrame();
    });

    ui.btnAnimNext.addEventListener("click", () => {
        setAnimationPlaying(false);

        animState.frameIndex++;
        if (animState.frameIndex >= animState.images.length) {
            animState.frameIndex = 0;
        }
        showCurrentFrame();
    });
}

function showCurrentFrame() {
    if (animState.images[animState.frameIndex]) {
        ui.animPreviewImg.onload = () => {
            applyAnimationCropPreview();
            updateAnimVisuals();
        };
        ui.animPreviewImg.src = animState.images[animState.frameIndex];
        if (ui.animPreviewImg.complete) {
            requestAnimationFrame(() => applyAnimationCropPreview());
        }
        // Ensure overlays update if visible
        updateAnimVisuals();
    }
    updateAnimationFrameCounter();
}

// =========================================================================
// CROP ANIMACION
// =========================================================================

function getAnimCropBounds() {
    const img = ui.animPreviewImg;
    const container = document.querySelector("#anim-image-container");
    if (!container) return { x: 0, y: 0, w: 0, h: 0 };

    const containerRect = container.getBoundingClientRect();
    const imgRect = img ? img.getBoundingClientRect() : null;
    const hasImageBounds = imgRect && imgRect.width > 1 && imgRect.height > 1;

    if (!hasImageBounds) {
        return { x: 0, y: 0, w: containerRect.width, h: containerRect.height };
    }

    return {
        x: imgRect.left - containerRect.left,
        y: imgRect.top - containerRect.top,
        w: imgRect.width,
        h: imgRect.height
    };
}

function clampAnimPointToImage(coords) {
    const bounds = getAnimCropBounds();
    return {
        x: Math.max(bounds.x, Math.min(bounds.x + bounds.w, coords.x)),
        y: Math.max(bounds.y, Math.min(bounds.y + bounds.h, coords.y))
    };
}

function normalizeAnimCropRect(rect) {
    const x = rect.w < 0 ? rect.x + rect.w : rect.x;
    const y = rect.h < 0 ? rect.y + rect.h : rect.y;
    return { x, y, w: Math.abs(rect.w), h: Math.abs(rect.h) };
}

function clampAnimCropSelection(minSize = 8) {
    const bounds = getAnimCropBounds();
    if (bounds.w <= 1 || bounds.h <= 1) return;

    const rect = normalizeAnimCropRect(animCropSelection);
    rect.w = Math.max(minSize, Math.min(rect.w, bounds.w));
    rect.h = Math.max(minSize, Math.min(rect.h, bounds.h));
    rect.x = Math.max(bounds.x, Math.min(bounds.x + bounds.w - rect.w, rect.x));
    rect.y = Math.max(bounds.y, Math.min(bounds.y + bounds.h - rect.h, rect.y));
    animCropSelection = rect;
}

function getAnimCropSelectionFromExportRect() {
    if (!animCropExportRect) return null;
    const bounds = getAnimCropBounds();
    if (bounds.w <= 1 || bounds.h <= 1) return null;
    return {
        x: bounds.x + animCropExportRect.x * bounds.w,
        y: bounds.y + animCropExportRect.y * bounds.h,
        w: animCropExportRect.w * bounds.w,
        h: animCropExportRect.h * bounds.h
    };
}

function updateAnimCropDOM() {
    if (!ui.animCropBox) return;
    clampAnimCropSelection();
    ui.animCropBox.style.left = animCropSelection.x + "px";
    ui.animCropBox.style.top = animCropSelection.y + "px";
    ui.animCropBox.style.width = animCropSelection.w + "px";
    ui.animCropBox.style.height = animCropSelection.h + "px";
    ui.animCropBox.style.display = "block";
}

function getAnimCoordinates(evt, container) {
    const rect = container.getBoundingClientRect();
    const clientX = evt.clientX - rect.left;
    const clientY = evt.clientY - rect.top;
    return { x: clientX, y: clientY };
}

function setupAnimCropInteractions() {
    const container = document.querySelector("#anim-image-container");
    if (!container) return;

    container.addEventListener("mousedown", (e) => {
        if (!isAnimCropping || e.button !== 0) return;

        if (e.target.classList.contains("crop-handle")) {
            isAnimResizing = true;
            animResizeDir = e.target.getAttribute("data-dir");
            e.stopPropagation();
            return;
        }

        const clickedBox = e.target.closest("#anim-crop-box");
        if (clickedBox) {
            isAnimMoving = true;
            const coords = getAnimCoordinates(e, container);
            animMoveOffset.x = coords.x - animCropSelection.x;
            animMoveOffset.y = coords.y - animCropSelection.y;
            container.style.cursor = "move";
            e.stopPropagation();
            return;
        }

        isAnimDrawing = true;
        const coords = clampAnimPointToImage(getAnimCoordinates(e, container));
        animCropStart = coords;
        animCropSelection = { x: coords.x, y: coords.y, w: 0, h: 0 };
        updateAnimCropDOM();
    });

    window.addEventListener("mousemove", (e) => {
        if (!isAnimCropping) return;

        if (isAnimDrawing) {
            const coords = clampAnimPointToImage(getAnimCoordinates(e, container));
            const currentX = coords.x;
            const currentY = coords.y;

            const minX = Math.min(animCropStart.x, currentX);
            const minY = Math.min(animCropStart.y, currentY);
            const w = Math.abs(currentX - animCropStart.x);
            const h = Math.abs(currentY - animCropStart.y);

            animCropSelection = { x: minX, y: minY, w: w, h: h };
            updateAnimCropDOM();
            return;
        }

        if (isAnimMoving) {
            const coords = getAnimCoordinates(e, container);
            let newX = coords.x - animMoveOffset.x;
            let newY = coords.y - animMoveOffset.y;

            animCropSelection.x = newX;
            animCropSelection.y = newY;
            updateAnimCropDOM();
            return;
        }

        if (isAnimResizing) {
            const coords = clampAnimPointToImage(getAnimCoordinates(e, container));
            const curX = coords.x;
            const curY = coords.y;

            let oldX = animCropSelection.x;
            let oldY = animCropSelection.y;
            let oldR = animCropSelection.x + animCropSelection.w;
            let oldB = animCropSelection.y + animCropSelection.h;

            if (animResizeDir.includes("n")) {
                let newTop = curY; if (newTop > oldB - 5) newTop = oldB - 5;
                animCropSelection.y = newTop; animCropSelection.h = oldB - newTop;
            }
            if (animResizeDir.includes("s")) {
                let newBottom = curY; if (newBottom < oldY + 5) newBottom = oldY + 5;
                animCropSelection.h = newBottom - oldY;
            }
            if (animResizeDir.includes("w")) {
                let newLeft = curX; if (newLeft > oldR - 5) newLeft = oldR - 5;
                animCropSelection.x = newLeft; animCropSelection.w = oldR - newLeft;
            }
            if (animResizeDir.includes("e")) {
                let newRight = curX; if (newRight < oldX + 5) newRight = oldX + 5;
                animCropSelection.w = newRight - oldX;
            }

            if (animCropSelection.w < 0) animCropSelection.w = Math.abs(animCropSelection.w);
            if (animCropSelection.h < 0) animCropSelection.h = Math.abs(animCropSelection.h);

            updateAnimCropDOM();
        }
    });

    window.addEventListener("mouseup", () => {
        if (isAnimCropping) {
            isAnimDrawing = false;
            isAnimMoving = false;
            isAnimResizing = false;
            animResizeDir = "";
            container.style.cursor = "crosshair";
        }
    });
}
setupAnimCropInteractions();

window.addEventListener("resize", () => {
    if (ui.animModal && window.getComputedStyle(ui.animModal).display !== "none") {
        applyAnimationCropPreview();
    }
});

if (ui.btnAnimCropStart) {
    ui.btnAnimCropStart.addEventListener("click", () => {
        setAnimationPlaying(false);
        applyAnimationCropPreview(true);

        isAnimCropping = true;
        ui.animCropControls.style.display = "flex";
        ui.btnAnimCropStart.style.display = "none";

        const container = document.querySelector("#anim-image-container");
        if (container) {
            container.style.cursor = "crosshair";
            ui.animCropBox.style.display = "block";

            requestAnimationFrame(() => {
                const restoredCrop = getAnimCropSelectionFromExportRect();
                if (restoredCrop && restoredCrop.w >= 10 && restoredCrop.h >= 10) {
                    animCropSelection = restoredCrop;
                } else if (animCropDisplaySelection && animCropDisplaySelection.w >= 10 && animCropDisplaySelection.h >= 10) {
                    animCropSelection = { ...animCropDisplaySelection };
                } else if (animCropSelection.w < 10) {
                    const bounds = getAnimCropBounds();
                    const cw = bounds.w * 0.5;
                    const ch = bounds.h * 0.5;
                    animCropSelection = { x: bounds.x + (bounds.w - cw) / 2, y: bounds.y + (bounds.h - ch) / 2, w: cw, h: ch };
                }
                updateAnimCropDOM();
            });
        }
    });
}

if (ui.btnAnimCropCancel) {
    ui.btnAnimCropCancel.addEventListener("click", () => {
        exitAnimationCropMode(true);
    });
}

if (ui.btnAnimCropConfirm) {
    ui.btnAnimCropConfirm.addEventListener("click", () => {
        if (!batchResultPaths || batchResultPaths.length === 0) return;

        const img = ui.animPreviewImg;
        if (!img || !img.naturalWidth || !img.naturalHeight) {
            showCustomAlert(tr("general.error", "Error"), tr("animation.crop.missing_image_size", "No se pudo leer el tamano real de la imagen."));
            return;
        }
        clampAnimCropSelection(10);

        const rect = img.getBoundingClientRect();
        const containerRect = document.querySelector("#anim-image-container").getBoundingClientRect();

        const imgLeft = rect.left - containerRect.left;
        const imgTop = rect.top - containerRect.top;

        const boxX = animCropSelection.x;
        const boxY = animCropSelection.y;

        const relX = boxX - imgLeft;
        const relY = boxY - imgTop;

        const scaleX = img.naturalWidth / rect.width;
        const scaleY = img.naturalHeight / rect.height;

        // Calculate normalized coordinates (0.0 - 1.0) relative to VISUAL image
        let normX = relX / rect.width;
        let normY = relY / rect.height;
        let normW = animCropSelection.w / rect.width;
        let normH = animCropSelection.h / rect.height;

        // Clamp to [0, 1]
        normX = Math.max(0, Math.min(1, normX));
        normY = Math.max(0, Math.min(1, normY));
        normW = Math.max(0, Math.min(1 - normX, normW));
        normH = Math.max(0, Math.min(1 - normY, normH));

        // Apply rotation transformation to map visual coords to original image coords
        const rotation = animFilters.rotation || 0;
        let finalNormX = normX;
        let finalNormY = normY;
        let finalNormW = normW;
        let finalNormH = normH;

        if (rotation === 90) {
            // 90° CW: Visual top-left becomes original top-right
            // x' = y, y' = 1 - (x + w), w' = h, h' = w
            finalNormX = normY;
            finalNormY = 1 - (normX + normW);
            finalNormW = normH;
            finalNormH = normW;
        } else if (rotation === 180) {
            // 180°: Visual top-left becomes original bottom-right
            // x' = 1 - (x + w), y' = 1 - (y + h)
            finalNormX = 1 - (normX + normW);
            finalNormY = 1 - (normY + normH);
            finalNormW = normW;
            finalNormH = normH;
        } else if (rotation === 270) {
            // 270° CW (or 90° CCW): Visual top-left becomes original bottom-left
            // x' = 1 - (y + h), y' = x, w' = h, h' = w
            finalNormX = 1 - (normY + normH);
            finalNormY = normX;
            finalNormW = normH;
            finalNormH = normW;
        }

        // Scale to original image dimensions
        let finalX = Math.round(finalNormX * img.naturalWidth);
        let finalY = Math.round(finalNormY * img.naturalHeight);
        let finalW = Math.round(finalNormW * img.naturalWidth);
        let finalH = Math.round(finalNormH * img.naturalHeight);

        // Final bounds check
        if (finalX < 0) finalX = 0;
        if (finalY < 0) finalY = 0;
        if (finalX + finalW > img.naturalWidth) finalW = img.naturalWidth - finalX;
        if (finalY + finalH > img.naturalHeight) finalH = img.naturalHeight - finalY;

        if (finalW < 10 || finalH < 10) {
            showCustomAlert(tr("general.error", "Error"), tr("animation.crop.invalid_selection", "Seleccion invalida o fuera de la imagen."));
            return;
        }

        animCropExportRect = {
            x: finalX / img.naturalWidth,
            y: finalY / img.naturalHeight,
            w: finalW / img.naturalWidth,
            h: finalH / img.naturalHeight
        };
        animCropDisplaySelection = { ...animCropSelection };
        exitAnimationCropMode(true);
        log("SUCCESS", trFormat("animation.crop.export_crop_log", { width: finalW, height: finalH }, `Recorte de exportacion definido: ${finalW}x${finalH}`));
    });
}

/*
LEGACY ANIMATION REALIGN HANDLER
Disabled with the commented animation buttons in index.html. Batch now centers
animation-ready outputs automatically by category, so this remains only as a
manual rescue path if those controls are restored later.
const btnAnimRealign = $("#btn-anim-realign");
if (btnAnimRealign) {
    btnAnimRealign.addEventListener("click", async () => {
        if (!batchResultPaths || batchResultPaths.length === 0) {
            showCustomAlert("Info", "No hay archivos originales vinculados para realinear.");
            return;
        }

        btnAnimRealign.disabled = true;
        const oldText = btnAnimRealign.textContent;
        btnAnimRealign.textContent = "⏳ Planetario...";
        showProcessing("ALINEANDO (PLANETARIO)...");

        try {
            // FORCE PLANETARY MODE
            const newImages = await invoke("realign_animation_frames", {
                paths: batchResultPaths,
                modeType: "planetary"
            });

            if (newImages && newImages.length > 0) {
                batchGeneratedImages = newImages;
                // FIX: startAnimationPlayer handles timer clearing and scheduling
                startAnimationPlayer(batchGeneratedImages, true);

                log("SUCCESS", "Realineacion Planetaria completada.");
                showCustomAlert("Exito", "Frames realineados (Planetario).");
            }
        } catch (e) {
            log("ERROR", "Realign: " + e);
            showCustomAlert("Error", "Fallo al realinear: " + e);
        } finally {
            hideProcessing();
            btnAnimRealign.disabled = false;
            btnAnimRealign.textContent = oldText;
        }
    });
}
*/

/*
const btnAnimRealignSurface = $("#btn-anim-realign-surface");
if (btnAnimRealignSurface) {
    btnAnimRealignSurface.addEventListener("click", async () => {
        if (!batchResultPaths || batchResultPaths.length === 0) {
            showCustomAlert("Info", "No hay archivos originales vinculados para realinear.");
            return;
        }

        btnAnimRealignSurface.disabled = true;
        const oldText = btnAnimRealignSurface.textContent;
        btnAnimRealignSurface.textContent = "⏳ Superficie...";
        showProcessing("ALINEANDO (SUPERFICIE)...");

        try {
            // FORCE SURFACE MODE
            const newImages = await invoke("realign_animation_frames", {
                paths: batchResultPaths,
                modeType: "surface"
            });

            if (newImages && newImages.length > 0) {
                batchGeneratedImages = newImages;

                startAnimationPlayer(batchGeneratedImages, true);

                log("SUCCESS", "Realineacion Superficie completada.");
                showCustomAlert("Exito", "Frames realineados (Superficie).");
            }
        } catch (e) {
            log("ERROR", "Realign: " + e);
            showCustomAlert("Error", "Fallo al realinear: " + e);
        } finally {
            hideProcessing();
            btnAnimRealignSurface.disabled = false;
            btnAnimRealignSurface.textContent = oldText;
        }
    });
}
*/

// =========================================================================
// LOGICA DE ZOOM / PAN
// =========================================================================

function updateTransform() {
    if (!isPanningFrameRequested) {
        requestAnimationFrame(() => {
            const zResult = zoomLevel;
            const zSource = zoomLevel * (typeof activeDrizzleFactor !== 'undefined' ? activeDrizzleFactor : 1);

            const srcContainer = $("#view-source .zoom-content");
            const resContainer = $("#view-result .zoom-content");

            const wSrc = ui.imgSource ? ui.imgSource.naturalWidth : 0;
            const hSrc = ui.imgSource ? ui.imgSource.naturalHeight : 0;

            if (srcContainer) {
                srcContainer.style.transform = `translate(${panX}px, ${panY}px) scale(${zSource})`;
            }

            if (resContainer) {
                const wRes = ui.imgResult ? ui.imgResult.naturalWidth : 0;
                const hRes = ui.imgResult ? ui.imgResult.naturalHeight : 0;

                const sourceVisible = ui.viewSource && ui.viewSource.style.display !== "none" && wSrc > 0 && hSrc > 0;
                const syncWithSource = sourceVisible && !window.__mosaicViewportMode;

                // Shift result by half the difference only when source/result are
                // intentionally synchronized. Mosaic output is a standalone canvas.
                const shiftX = syncWithSource ? (wSrc * zSource - wRes * zResult) / 2 : 0;
                const shiftY = syncWithSource ? (hSrc * zSource - hRes * zResult) / 2 : 0;

                resContainer.style.transform = `translate(${panX + shiftX}px, ${panY + shiftY}px) scale(${zResult})`;
            }

            // INSPECCION DE PIXELES (paridad con el visor de AS!4): por encima
            // del 150% de zoom desactivamos el suavizado bilinear del navegador
            // en AMBAS vistas para evaluar nitidez real pixel a pixel.
            const pixelated = zoomLevel > 1.5;
            [ui.imgSource, ui.imgResult].forEach(img => {
                if (img) img.style.imageRendering = pixelated ? "pixelated" : "auto";
            });

            isPanningFrameRequested = false;
        });
        isPanningFrameRequested = true;
    }
}

function prepareZoomSurfaceForImage(img) {
    if (!img || !img.naturalWidth || !img.naturalHeight) return false;

    const contentEl = img.closest(".zoom-content");
    if (!contentEl) return false;

    const w = img.naturalWidth;
    const h = img.naturalHeight;

    contentEl.style.position = "absolute";
    contentEl.style.top = "0";
    contentEl.style.left = "0";
    contentEl.style.display = "block";
    contentEl.style.justifyContent = "initial";
    contentEl.style.alignItems = "initial";
    contentEl.style.width = `${w}px`;
    contentEl.style.height = `${h}px`;
    contentEl.style.transformOrigin = "0 0";

    img.style.display = "block";
    img.style.width = `${w}px`;
    img.style.height = `${h}px`;
    img.style.maxWidth = "none";
    img.style.maxHeight = "none";
    img.style.margin = "0";

    return true;
}
window.prepareZoomSurfaceForImage = prepareZoomSurfaceForImage;

function ensureImageVisibleInViewport(img) {
    if (!img || !img.naturalWidth || !img.naturalHeight) return;

    const containerEl = img.closest(".zoom-target-container");
    if (!containerEl) return;

    const imgRect = img.getBoundingClientRect();
    const viewRect = containerEl.getBoundingClientRect();
    if (!imgRect.width || !imgRect.height || !viewRect.width || !viewRect.height) return;

    const intersectW = Math.max(0, Math.min(imgRect.right, viewRect.right) - Math.max(imgRect.left, viewRect.left));
    const intersectH = Math.max(0, Math.min(imgRect.bottom, viewRect.bottom) - Math.max(imgRect.top, viewRect.top));
    const visibleArea = intersectW * intersectH;
    const minArea = Math.min(imgRect.width * imgRect.height, viewRect.width * viewRect.height);

    if (visibleArea < minArea * 0.02) {
        fitToScreen(img);
    }
}
window.ensureImageVisibleInViewport = ensureImageVisibleInViewport;

function fitToScreen(targetImg = null) {
    const img = targetImg || ui.imgSource;
    if (!img || !img.naturalWidth) return;

    prepareZoomSurfaceForImage(img);

    const containerEl = img.closest('.zoom-target-container');
    if (!containerEl) return;

    const rect = containerEl.getBoundingClientRect();
    const w = img.naturalWidth;
    const h = img.naturalHeight;

    if (w === 0 || h === 0 || rect.width === 0 || rect.height === 0) return;

    const padding = 40;
    const availW = rect.width;
    const availH = rect.height;

    const scaleX = Math.max(availW - padding, 1) / w;
    const scaleY = Math.max(availH - padding, 1) / h;
    zoomLevel = Math.min(scaleX, scaleY);
    if (zoomLevel > 1.0) zoomLevel = 1.0;

    // Centering: Offset = (Container - ScaledImage) / 2
    panX = (availW - (w * zoomLevel)) / 2;
    panY = (availH - (h * zoomLevel)) / 2;

    updateTransform();

    // Fix: Sync Crop Grid immediately if active
    if (typeof isCropping !== 'undefined' && isCropping && typeof updateCropDOM === 'function') {
        updateCropDOM();
    }
    if (typeof isVideoCropping !== 'undefined' && isVideoCropping && typeof updateVideoCropDOM === 'function') {
        updateVideoCropDOM();
    }
}
window.fitImageToScreen = fitToScreen;

window.fitResultToScreen = () => {
    if (!ui.imgResult || !ui.imgResult.naturalWidth || !ui.imgResult.naturalHeight) return;
    fitToScreen(ui.imgResult);
};

function fitVisibleViewportImage() {
    if (window.__mosaicViewportMode && ui.imgResult && ui.imgResult.naturalWidth > 0) {
        if (window.fitMosaicResultViewport) window.fitMosaicResultViewport();
        return;
    }

    const sourceVisible = ui.viewSource && ui.viewSource.style.display !== "none";
    if (sourceVisible && ui.imgSource && ui.imgSource.naturalWidth > 0) {
        fitToScreen(ui.imgSource);
        return;
    }

    if (ui.imgResult && ui.imgResult.naturalWidth > 0) {
        fitToScreen(ui.imgResult);
    }
}
window.fitVisibleViewportImage = fitVisibleViewportImage;

window.fitMosaicResultViewport = () => {
    const img = ui.imgResult;
    if (!img || !img.naturalWidth || !img.naturalHeight) return;

    prepareZoomSurfaceForImage(img);

    const containerEl = img.closest(".zoom-target-container");
    const contentEl = document.querySelector("#view-result .zoom-content");
    if (!containerEl || !contentEl) return;

    const rect = containerEl.getBoundingClientRect();
    const w = img.naturalWidth;
    const h = img.naturalHeight;
    if (w === 0 || h === 0 || rect.width === 0 || rect.height === 0) return;

    const padding = 40;
    const scaleX = Math.max(rect.width - padding, 1) / w;
    const scaleY = Math.max(rect.height - padding, 1) / h;
    zoomLevel = Math.min(scaleX, scaleY, 1.0);
    panX = (rect.width - (w * zoomLevel)) / 2;
    panY = (rect.height - (h * zoomLevel)) / 2;
    isPanningFrameRequested = false;
    updateTransform();
};

function attachZoomEvents(container) {
    if (!container) return;

    container.addEventListener("wheel", (e) => {
        e.preventDefault();
        const rect = container.getBoundingClientRect();
        const mouseX = e.clientX - rect.left;
        const mouseY = e.clientY - rect.top;

        const delta = e.deltaY > 0 ? 0.9 : 1.1;
        const newZoom = Math.max(0.01, Math.min(zoomLevel * delta, 50.0));

        panX = mouseX - (mouseX - panX) * (newZoom / zoomLevel);
        panY = mouseY - (mouseY - panY) * (newZoom / zoomLevel);

        zoomLevel = newZoom;
        updateTransform();
    }, { passive: false });

    container.addEventListener("mousedown", (e) => {
        if (typeof isCropping !== 'undefined' && isCropping) return;
        if (typeof isSettingStackingRoi !== 'undefined' && isSettingStackingRoi) return;
        if (typeof isSettingManualAnchor !== 'undefined' && isSettingManualAnchor) return;

        if (e.button === 0 || e.button === 1) {
            isDragging = true;
            startX = e.clientX - panX;
            startY = e.clientY - panY;
            container.style.cursor = "grabbing";
        }
    });

    window.addEventListener("mouseup", () => {
        if (isDragging) {
            isDragging = false;
            let blocked = false;
            if (typeof isCropping !== 'undefined' && isCropping) blocked = true;
            if (typeof isSettingStackingRoi !== 'undefined' && isSettingStackingRoi) blocked = true;
            if (typeof isSettingManualAnchor !== 'undefined' && isSettingManualAnchor) blocked = true;

            if (container && !blocked) container.style.cursor = "grab";
        }
    });

    window.addEventListener("mousemove", (e) => {
        let blocked = false;
        if (typeof isCropping !== 'undefined' && isCropping) blocked = true;
        if (typeof isSettingStackingRoi !== 'undefined' && isSettingStackingRoi) blocked = true;
        if (typeof isSettingManualAnchor !== 'undefined' && isSettingManualAnchor) blocked = true;

        if (!isDragging || blocked) return;
        e.preventDefault();
        panX = e.clientX - startX;
        panY = e.clientY - startY;
        updateTransform();
    });
}

const zoomContainers = $$(".zoom-target-container");
zoomContainers.forEach(el => attachZoomEvents(el));

// Restoration of Resize Listener with Debounce
const debouncedResize = debounce(() => {
    fitVisibleViewportImage();
}, 150);

window.addEventListener("resize", debouncedResize);

function getLocalCoordinates(evt, container) {
    // FIX: Use the actual IMAGE element for coordinates Source of Truth
    // This avoids issues if the wrapper has strange offsets or size
    const img = container.querySelector("img");
    const rect = img ? img.getBoundingClientRect() : container.getBoundingClientRect();

    // Now clientX is relative to the visual start of the Image
    const clientX = evt.clientX - rect.left;
    const clientY = evt.clientY - rect.top;

    const scale = zoomLevel;

    // Convert screen pixels to internal CSS pixels
    const imgX = clientX / scale;
    const imgY = clientY / scale;

    return { x: imgX, y: imgY };
}

function updateCropDOM() {
    if (!ui.cropBox) return;

    // FIX: Account for any internal layout offset of the image
    // If img starts at 50px inside wrapper, crop box must start at 50px + x
    const img = ui.imgResult;
    const offX = img ? img.offsetLeft : 0;
    const offY = img ? img.offsetTop : 0;

    ui.cropBox.style.left = (offX + cropSelection.x) + "px";
    ui.cropBox.style.top = (offY + cropSelection.y) + "px";
    ui.cropBox.style.width = cropSelection.w + "px";
    ui.cropBox.style.height = cropSelection.h + "px";
    ui.cropBox.style.display = "block";
}

function setupCropInteractions() {
    const container = ui.viewResult.querySelector(".zoom-target-container");
    if (!container) return;

    container.addEventListener("mousedown", (e) => {
        if (!isCropping || e.button !== 0) return;

        if (e.target.classList.contains("crop-handle")) {
            isResizingCrop = true;
            resizeDir = e.target.getAttribute("data-dir");
            e.stopPropagation();
            return;
        }

        const clickedBox = e.target.closest("#crop-box");
        if (clickedBox) {
            isMovingCrop = true;
            const coords = getLocalCoordinates(e, container);
            moveOffset.x = coords.x - cropSelection.x;
            moveOffset.y = coords.y - cropSelection.y;
            container.style.cursor = "move";
            e.stopPropagation();
            return;
        }

        isDrawingCrop = true;
        const coords = getLocalCoordinates(e, container);
        cropStart = coords;
        cropSelection = { x: coords.x, y: coords.y, w: 0, h: 0 };
        updateCropDOM();
    });

    window.addEventListener("mousemove", (e) => {
        if (!isCropping) return;

        const imgW = ui.imgResult.naturalWidth;
        const imgH = ui.imgResult.naturalHeight;

        if (isDrawingCrop) {
            const coords = getLocalCoordinates(e, container);

            // FIX: Clamp drawing to image bounds
            let currentX = Math.max(0, Math.min(coords.x, imgW));
            let currentY = Math.max(0, Math.min(coords.y, imgH));

            const minX = Math.min(cropStart.x, currentX);
            const minY = Math.min(cropStart.y, currentY);
            const w = Math.abs(currentX - cropStart.x);
            const h = Math.abs(currentY - cropStart.y);

            cropSelection = { x: minX, y: minY, w: w, h: h };
            updateCropDOM();
            return;
        }

        if (isMovingCrop) {
            const coords = getLocalCoordinates(e, container);
            let newX = coords.x - moveOffset.x;
            let newY = coords.y - moveOffset.y;

            if (newX < 0) newX = 0;
            if (newY < 0) newY = 0;
            if (newX + cropSelection.w > imgW) newX = imgW - cropSelection.w;
            if (newY + cropSelection.h > imgH) newY = imgH - cropSelection.h;

            cropSelection.x = newX;
            cropSelection.y = newY;
            updateCropDOM();
            return;
        }

        if (isResizingCrop) {
            const coords = getLocalCoordinates(e, container);

            // FIX: Clamp resize to image bounds
            const curX = Math.max(0, Math.min(coords.x, imgW));
            const curY = Math.max(0, Math.min(coords.y, imgH));

            let oldX = cropSelection.x;
            let oldY = cropSelection.y;
            let oldR = cropSelection.x + cropSelection.w;
            let oldB = cropSelection.y + cropSelection.h;

            if (resizeDir.includes("n")) {
                let newTop = curY;
                if (newTop > oldB - 5) newTop = oldB - 5;
                cropSelection.y = newTop;
                cropSelection.h = oldB - newTop;
            }
            if (resizeDir.includes("s")) {
                let newBottom = curY;
                if (newBottom < oldY + 5) newBottom = oldY + 5;
                cropSelection.h = newBottom - oldY;
            }
            if (resizeDir.includes("w")) {
                let newLeft = curX;
                if (newLeft > oldR - 5) newLeft = oldR - 5;
                cropSelection.x = newLeft;
                cropSelection.w = oldR - newLeft;
            }
            if (resizeDir.includes("e")) {
                let newRight = curX;
                if (newRight < oldX + 5) newRight = oldX + 5;
                cropSelection.w = newRight - oldX;
            }

            if (cropSelection.w < 0) cropSelection.w = Math.abs(cropSelection.w);
            if (cropSelection.h < 0) cropSelection.h = Math.abs(cropSelection.h);

            updateCropDOM();
            return;
        }
    });

    window.addEventListener("mouseup", () => {
        if (isCropping) {
            isDrawingCrop = false;
            isMovingCrop = false;
            isResizingCrop = false;
            resizeDir = "";
            if (container) container.style.cursor = "crosshair";
        }
    });
}

setupCropInteractions();

if (ui.btnStartCrop) {
    ui.btnStartCrop.addEventListener("click", () => {
        if (!ui.imgResult.src || ui.imgResult.naturalWidth === 0) {
            showCustomAlert("Error", "No hay imagen para recortar.");
            return;
        }
        isCropping = true;
        ui.toolsMainView.style.display = "none";
        ui.cropControls.style.display = "flex";
        ui.cropInstr.style.display = "block";

        const container = ui.viewResult.querySelector(".zoom-target-container");
        if (container) {
            container.style.cursor = "crosshair";
            container.classList.add("crop-active");
        }

        if (cropSelection.w < 10) {
            const w = ui.imgResult.naturalWidth;
            const h = ui.imgResult.naturalHeight;
            const cw = w * 0.5;
            const ch = h * 0.5;
            cropSelection = { x: (w - cw) / 2, y: (h - ch) / 2, w: cw, h: ch };
            updateCropDOM();
        } else {
            ui.cropBox.style.display = "block";
        }
    });
}

if (ui.btnCancelCrop) {
    ui.btnCancelCrop.addEventListener("click", () => {
        isCropping = false;
        ui.cropBox.style.display = "none";
        ui.toolsMainView.style.display = "block";
        ui.cropControls.style.display = "none";
        ui.cropInstr.style.display = "none";

        const container = ui.viewResult.querySelector(".zoom-target-container");
        if (container) {
            container.style.cursor = "grab";
            container.classList.remove("crop-active");
        }
    });
}

if (ui.btnConfirmCrop) {
    ui.btnConfirmCrop.addEventListener("click", async () => {
        if (cropSelection.w < 10 || cropSelection.h < 10) {
            showCustomAlert("Atencion", "Seleccion muy pequeña");
            return;
        }

        const ix = Math.round(cropSelection.x);
        const iy = Math.round(cropSelection.y);
        const iw = Math.round(cropSelection.w);
        const ih = Math.round(cropSelection.h);

        // Guardar parámetros actuales antes del recorte
        const hadProcessing = lastProcessedParams !== null && lastProcessedParams !== "{}";

        ui.btnConfirmCrop.disabled = true;
        showProcessing("RECORTANDO...");

        try {
            const b64 = await invoke("crop_stacked_image", { x: ix, y: iy, w: iw, h: ih });
            await setImageAndWait(ui.imgResult, b64, true);

            // Solo resetear si no había procesamiento previo
            if (!hadProcessing) {
                resetProcessingParams();
            } else {
                // Reaplicar los parámetros guardados automáticamente
                log("INFO", "Reaplicando parámetros de post-procesamiento...");
                // Los parámetros ya están en los controles UI, solo trigger update
                // Forzar actualización limpiando lastProcessedParams para que detecte cambio
                lastProcessedParams = null;
                triggerUpdate();
            }

            log("SUCCESS", `Recorte aplicado: ${iw}x${ih}`);
            ui.btnCancelCrop.click();
        } catch (e) {
            log("ERROR", "Recorte: " + e);
            showCustomAlert("Error", "Error al recortar: " + e);
        } finally {
            hideProcessing();
            ui.btnConfirmCrop.disabled = false;
        }
    });
}

function linkControl(sliderId, numberId, scale = 1.0) {
    const sl = $(sliderId);
    const num = $(numberId);
    if (!sl || !num) return;

    sl.addEventListener("input", () => {
        let val = parseFloat(sl.value);
        if (scale !== 1.0) val = val / scale;
        // Clean display: remove trailing zeros (e.g. 1.00 -> 1)
        num.value = Number(val.toFixed(2)).toString();
        triggerUpdate();
    });

    num.addEventListener("change", () => {
        let val = parseFloat(num.value);
        if (isNaN(val)) val = parseFloat(sl.value) / scale;

        // Enforce limits from slider to prevent extreme values (e.g. contrast 100)
        const minVal = parseFloat(sl.min) / scale;
        const maxVal = parseFloat(sl.max) / scale;
        if (val < minVal) val = minVal;
        if (val > maxVal) val = maxVal;

        num.value = scale === 1.0 ? val : val.toFixed(2);
        sl.value = val * scale;
        triggerUpdate();
    });
}

linkControl("#sl-deconv-sigma", "#num-deconv-sigma", 10.0);
linkControl("#sl-deconv-iter", "#num-deconv-iter", 1.0);
linkControl("#sl-vc-sigma", "#num-vc-sigma", 10.0);
linkControl("#sl-vc-iter", "#num-vc-iter", 1.0);
linkControl("#sl-usm-amt", "#num-usm-amt", 10.0);
linkControl("#sl-usm-rad", "#num-usm-rad", 10.0);
linkControl("#sl-lce-amt", "#num-lce-amt", 100.0);

["u1", "u2", "u3", "u4", "u5"].forEach(id => linkControl(`#${id}`, `#num-${id}`, 10.0));
["w1", "w2", "w3", "w4", "w5", "w6"].forEach(id => linkControl(`#${id}`, `#num-${id}`, 5.0));
["d1", "d2", "d3", "d4", "d5", "d6"].forEach(id => linkControl(`#${id}`, `#num-${id}`, 10.0));

linkControl("#sl-crisp", "#num-crisp", 10.0);
linkControl("#sl-master-denoise", "#num-master-denoise", 1.0);
linkControl("#sl-denoise-detail", "#num-denoise-detail", 1.0);
linkControl("#sl-denoise-chroma", "#num-denoise-chroma", 1.0);
// linkControl("#sl-gamma", "#num-gamma", 100.0);
// Custom Inverted Gamma Logic for Post-Processing
const slGamma = $("#sl-gamma");
const numGamma = $("#num-gamma");
if (slGamma && numGamma) {
    slGamma.addEventListener("input", () => {
        let valSlider = parseFloat(slGamma.value);
        // High-res mapping: Slider 1000 -> 1.00, Slider 2000 -> 2.00
        let gamma = valSlider / 1000.0;

        numGamma.value = gamma.toFixed(2);
        triggerUpdate();
    });

    numGamma.addEventListener("change", () => {
        let gamma = parseFloat(numGamma.value);
        if (isNaN(gamma) || gamma <= 0.001) gamma = 0.01;

        // Limpieza visual: si el usuario escribe 1.00 -> 1
        numGamma.value = Number(gamma.toFixed(2)).toString();

        // Direct Mapping Inverse: Slider = Gamma * 1000
        slGamma.value = Math.round(gamma * 1000.0);
        triggerUpdate();
    });
}
linkControl("#sl-sat", "#num-sat", 1000.0);
// Gamma handled by custom logic above
linkControl("#sl-contrast", "#num-contrast", 1000.0);
linkControl("#sl-brightness", "#num-brightness", 1000.0);
linkControl("#sl-r-bal", "#num-r-bal", 1000.0);
linkControl("#sl-b-bal", "#num-b-bal", 1000.0);
linkControl("#sl-dr-rad", "#num-dr-rad", 10.0); // Threshold 0-50.0
linkControl("#sl-dr-dark", "#num-dr-dark", 1000.0); // Dark Strength 0-1.00
linkControl("#sl-dr-light", "#num-dr-light", 1000.0); // Light Strength 0-1.00

if (ui.blendSlider) {
    ui.blendSlider.addEventListener("input", () => {
        if (ui.blendDisplay) ui.blendDisplay.textContent = ui.blendSlider.value + "%";
        triggerUpdate();
    });
    ui.blendSlider.addEventListener("change", triggerUpdate);
}

[ui.rx, ui.ry, ui.bx, ui.by].forEach(el => { if (el) el.addEventListener("input", triggerUpdate); });
if (ui.chkDeringing) ui.chkDeringing.addEventListener("change", triggerUpdate);

function resetProcessingParams() {
    const zeroIds = [
        "u1", "u2", "u3", "u4", "u5",
        "w1", "w2", "w3", "w4", "w5", "w6",
        "d1", "d2", "d3", "d4", "d5", "d6"
    ];
    zeroIds.forEach(id => {
        const sl = $(`#${id}`); const num = $(`#num-${id}`);
        if (sl) sl.value = 0;
        if (num) num.value = 0;
    });

    if (ui.slDeconvSigma) ui.slDeconvSigma.value = 0;
    if (ui.valDeconvSigma) ui.valDeconvSigma.value = 0;
    if (ui.slDeconvIter) ui.slDeconvIter.value = 0;
    if (ui.valDeconvIter) ui.valDeconvIter.value = 0;

    if (ui.slVcSigma) ui.slVcSigma.value = 0;
    if (ui.valVcSigma) ui.valVcSigma.value = 0;
    if (ui.slVcIter) ui.slVcIter.value = 0;
    if (ui.valVcIter) ui.valVcIter.value = 0;

    if (ui.slUsmAmt) ui.slUsmAmt.value = 0;
    if (ui.valUsmAmt) ui.valUsmAmt.value = 0;
    if (ui.slUsmRad) ui.slUsmRad.value = 0;
    if (ui.valUsmRad) ui.valUsmRad.value = 0;

    if (ui.slLceAmt) ui.slLceAmt.value = 0;
    if (ui.valLceAmt) ui.valLceAmt.value = 0;

    if (ui.slCrisp) ui.slCrisp.value = 0;
    if (ui.valCrisp) ui.valCrisp.value = 0;

    if (ui.slMasterDenoise) ui.slMasterDenoise.value = 0;
    if (ui.numMasterDenoise) ui.numMasterDenoise.value = 0;
    if (ui.slDenoiseDetail) ui.slDenoiseDetail.value = 70;
    if (ui.numDenoiseDetail) ui.numDenoiseDetail.value = 70;
    if (ui.slDenoiseChroma) ui.slDenoiseChroma.value = 55;
    if (ui.numDenoiseChroma) ui.numDenoiseChroma.value = 55;

    if (ui.slGamma) ui.slGamma.value = 1000;
    if (ui.numGamma) ui.numGamma.value = "1";
    if (ui.slSat) ui.slSat.value = 1000;
    if (ui.numSat) ui.numSat.value = "1";
    if (ui.slContrast) ui.slContrast.value = 1000;
    if (ui.numContrast) ui.numContrast.value = "1";
    if (ui.slBrightness) ui.slBrightness.value = 0;
    if (ui.numBrightness) ui.numBrightness.value = "0";
    if (ui.slRBal) ui.slRBal.value = 0;
    if (ui.numRBal) ui.numRBal.value = "0";
    if (ui.slBBal) ui.slBBal.value = 0;
    if (ui.numBBal) ui.numBBal.value = "0";

    if (ui.rx) ui.rx.value = 0; if (ui.ry) ui.ry.value = 0;
    if (ui.bx) ui.bx.value = 0; if (ui.by) ui.by.value = 0;

    if (ui.blendSlider) ui.blendSlider.value = 100;
    if (ui.blendDisplay) ui.blendDisplay.textContent = "100%";

    if (ui.selDrMode) {
        ui.selDrMode.value = "0"; // Disabled by default
        if (ui.panelDrManual) ui.panelDrManual.style.display = "none";
    }
    if (ui.slDrRad) ui.slDrRad.value = 10;
    if (ui.numDrRad) ui.numDrRad.value = 10;
    if (ui.slDrDark) ui.slDrDark.value = 50;
    if (ui.numDrDark) ui.numDrDark.value = 50;
    if (ui.slDrLight) ui.slDrLight.value = 0;
    if (ui.numDrLight) ui.numDrLight.value = 0;
    if (ui.chkDrMask) ui.chkDrMask.checked = false;

    lastProcessedParams = JSON.stringify(getPipelineParams());
}

if (ui.selDrMode) {
    ui.selDrMode.addEventListener("change", () => {
        const mode = ui.selDrMode.value; // "0"=Off, "1"=Auto, "2"=Manual
        if (ui.panelDrManual) {
            ui.panelDrManual.style.display = (mode === "2") ? "block" : "none";
        }
        
        // Update opacity/state of manual controls to indicate if they are active
        const manualInputs = [ui.slDrRad, ui.numDrRad, ui.slDrDark, ui.numDrDark, ui.slDrLight, ui.numDrLight, ui.chkDrMask];
        manualInputs.forEach(input => {
            if (input) {
                input.disabled = (mode !== "2");
                if (input.parentElement) {
                    input.parentElement.style.opacity = (mode === "2") ? "1" : "0.5";
                }
            }
        });

        triggerUpdate();
    });
}
if (ui.chkDrMask) {
    ui.chkDrMask.addEventListener("change", triggerUpdate);
}
const selSharpenMode = document.getElementById("sel-sharpen-mode");
if (selSharpenMode) {
    selSharpenMode.addEventListener("change", () => {
        updateModeGlow();
        triggerUpdate();
    });
}

function updateModeGlow() {
    const panel = document.getElementById("panel-wavelets");
    const mode = document.getElementById("sel-sharpen-mode")?.value;
    if (!panel) return;

    panel.classList.remove("glow-luminance", "glow-rgb");
    if (mode === "luminance") {
        panel.classList.add("glow-luminance");
    } else if (mode === "rgb") {
        panel.classList.add("glow-rgb");
    }
}

function getPipelineParams() {
    const getVal = (id) => parseFloat($(`#num-${id}`)?.value) || 0;
    return {
        u: [getVal("u1"), getVal("u2"), getVal("u3"), getVal("u4"), getVal("u5")],
        w: [getVal("w1"), getVal("w2"), getVal("w3"), getVal("w4"), getVal("w5"), getVal("w6")],
        d: [getVal("d1"), getVal("d2"), getVal("d3"), getVal("d4"), getVal("d5"), getVal("d6")],
        crisp: getVal("crisp"),
        deconv: {
            s: getVal("deconv-sigma"), i: parseInt($(`#num-deconv-iter`).value) || 0,
            vs: getVal("vc-sigma"), vi: parseInt($(`#num-vc-iter`).value) || 0
        },
        usm: { a: getVal("usm-amt"), r: getVal("usm-rad") },
        lce: getVal("lce-amt"),
        masterDenoise: getVal("master-denoise"),
        denoiseDetail: getVal("denoise-detail"),
        denoiseChroma: getVal("denoise-chroma"),
        color: {
            g: getVal("gamma"),
            s: getVal("sat"),
            c: getVal("contrast"),
            b: getVal("brightness"),
            rb: getVal("r-bal"),
            bb: getVal("b-bal")
        },
        shift: {
            rx: parseFloat(ui.rx.value) || 0, ry: parseFloat(ui.ry.value) || 0,
            bx: parseFloat(ui.bx.value) || 0, by: parseFloat(ui.by.value) || 0
        },
        blend: parseFloat(ui.blendSlider.value) || 100,
        // Deringing Params
        dr: {
            mode: parseInt(ui.selDrMode?.value) || 0, // 0=Off, 1=Auto, 2=Manual
            rad: getVal("dr-rad"),
            dark: getVal("dr-dark"),
            light: getVal("dr-light"),
            mask: ui.chkDrMask?.checked || false
        },
        useRgbSharpening: (document.getElementById("sel-sharpen-mode")?.value === "rgb")
    };
}
window.getPipelineParams = getPipelineParams;

function triggerUpdate() {
    const currentParams = JSON.stringify(getPipelineParams());
    if (currentParams === lastProcessedParams) return;

    clearTimeout(updateTimer);
    updateTimer = setTimeout(() => {
        const nowParams = JSON.stringify(getPipelineParams());
        if (nowParams !== lastProcessedParams) {
            pipelineRequestId++;
            showLocalProcessing("Pendiente...");
            showImgLoader();

            const p = getPipelineParams();
            if (ui.statusText) {
                if (p.deconv.i > 10 || p.deconv.vi > 10 || p.lce > 0) {
                    ui.statusText.innerHTML = "<svg class='zas-icon icon-spin'><use href='#icon-settings'></use></svg> Procesando filtros pesados...";
                    ui.statusText.style.color = "#f59e0b";
                } else {
                    ui.statusText.textContent = "Procesando...";
                    ui.statusText.style.color = "#94a3b8";
                }
            }
            if (ui.pBarContainer) { ui.pBarContainer.style.display = "block"; ui.pBarFill.style.width = "0%"; }
            processPipeline(pipelineRequestId, nowParams);
        }
    }, 300);
}

async function processPipeline(requestId, paramsString) {
    if (!currentFilePath) { hideImgLoader(); hideLocalProcessing(); return; }
    const p = JSON.parse(paramsString);
    const msg = $("#local-msg");
    if (msg) msg.textContent = "Calculando...";

    try {
        const b64 = await invoke("apply_wavelets", {
            reqId: requestId,
            u1: p.u[0], u2: p.u[1], u3: p.u[2], u4: p.u[3], u5: p.u[4],
            w1: p.w[0], w2: p.w[1], w3: p.w[2], w4: p.w[3], w5: p.w[4], w6: p.w[5],
            d1: p.d[0], d2: p.d[1], d3: p.d[2], d4: p.d[3], d5: p.d[4], d6: p.d[5],
            gamma: p.color.g, saturation: p.color.s,
            contrast: p.color.c, brightness: p.color.b,
            rBal: p.color.rb, bBal: p.color.bb,
            rX: p.shift.rx, rY: p.shift.ry, bX: p.shift.bx, bY: p.shift.by,
            blend: p.blend / 100.0,

            // New Advanced Deringing
            deringingMode: p.dr.mode,
            deringingRadius: p.dr.rad,
            deringingDark: p.dr.dark,
            deringingLight: p.dr.light,
            deringingMask: p.dr.mask,

            crisp: p.crisp,
            deconvIter: p.deconv.i, deconvSigma: p.deconv.s,
            vcIter: p.deconv.vi, vcSigma: p.deconv.vs,
            usmAmount: p.usm.a, usmRadius: p.usm.r, lceAmount: p.lce,
            masterDenoise: p.masterDenoise,
            masterDenoiseDetail: p.denoiseDetail,
            masterDenoiseChroma: p.denoiseChroma,
            useRgbSharpening: p.useRgbSharpening
        });

        if (requestId !== pipelineRequestId) { console.log("Descartado."); return; }
        lastProcessedParams = paramsString;

        if (ui.imgResult) {
            if (msg) msg.textContent = "Renderizando...";
            const viewportBeforeRender = captureViewportState();
            await setImageAndWait(ui.imgResult, b64, false);
            restoreViewportState(viewportBeforeRender);

            if (ui.statusText) { ui.statusText.textContent = "Vista actualizada."; ui.statusText.style.color = "#94a3b8"; }
        }
    } catch (e) {
        if (!e.toString().includes("Cancelled")) { log("ERROR", "Pipeline: " + e); }
    } finally {
        if (requestId === pipelineRequestId) { hideImgLoader(); hideLocalProcessing(); }
    }
}

// =========================================================================
// LÓGICA DE APILADO Y BATCH
// =========================================================================

function resetDataAcquisitionUI() {
    console.log("Resetting Data Acquisition UI...");
    clearMosaicInfoOverlay();

    // Limpieza agresiva de memoria en el backend (soluciona el problema de ralentización entre videos)
    invoke("clear_app_memory").catch(err => console.error("Error al limpiar memoria:", err));

    // Reset de estado de análisis para liberar memoria frontend
    currentGraphData = [];
    activeAPoints = [];
    currentVideoStats = null;
    batchGeneratedImages = [];
    batchResultPaths = [];
    if (chartInstance) {
        chartInstance.destroy();
        chartInstance = null;
    }

    // 1. Hide Panels
    if (ui.panelAnalysis) ui.panelAnalysis.style.display = "none";
    if (ui.panelBatch) ui.panelBatch.style.display = "none";
    if (ui.panelTools) ui.panelTools.style.display = "none";
    if (ui.panelWavelets) ui.panelWavelets.style.display = "none";
    if (ui.selDrMode) {
        ui.selDrMode.value = "0";
        if (ui.panelDrManual) ui.panelDrManual.style.display = "none";
    }
    if (ui.analysisActions) ui.analysisActions.style.display = "none";
    if (ui.panelInfo) ui.panelInfo.style.display = "none";

    const pMosaic = document.getElementById("panel-mosaic");
    if (pMosaic) pMosaic.style.display = "none";

    // 2. Clear Viewports
    if (ui.viewResult) ui.viewResult.style.display = "none";
    
    // Al reiniciar UI, asegurar que la vista fuente (canvas base) vuelva a ser visible
    // y resetear el estado y visibilidad del botón de ocultar fuente
    if (ui.viewSource) {
        ui.viewSource.style.display = "flex";
    }
    const btnToggleSrc = document.getElementById("btn-toggle-source");
    if (btnToggleSrc) {
        btnToggleSrc.style.display = "none";
        btnToggleSrc.textContent = tr("viewer.hide_source", "Ocultar Fuente");
        btnToggleSrc.style.borderColor = "#334155";
        btnToggleSrc.style.color = "#cbd5e1";
        btnToggleSrc.style.background = "rgba(0,0,0,0.4)";
    }

    clearSourcePreviewSurface();
    if (ui.imgResult) {
        ui.imgResult.src = "";
        ui.imgResult.classList.remove("loaded");
    }

    // Clear Overlay Canvas (Grid/Points)
    if (ui.gridOverlay) {
        const ctx = ui.gridOverlay.getContext('2d');
        ctx.clearRect(0, 0, ui.gridOverlay.width, ui.gridOverlay.height);
    }

    // 3. Reset State
    currentFilePath = "";
    window.currentFilePath = "";
    window.setMosaicViewportMode(false);
    batchFiles = [];
    activeAPoints = [];
    if (ui.apCount) ui.apCount.textContent = "0";
    isBatchMode = false;
    if (ui.selBayerOverride) ui.selBayerOverride.value = "auto";
    if (ui.statusText) ui.statusText.textContent = "Esperando accion.";

    // Reset Transforms
    zoomLevel = 1.0;
    panX = 0;
    panY = 0;
    activeDrizzleFactor = 1.0;

    // FIX: Apply the reset transform to the DOM immediately
    // to prevent stale transforms if the next fitToScreen call fails or delays
    updateTransform();

    // 4. Specific Mosaic Cleanup & Overlays
    if (mosaicManager) {
        mosaicManager.toggleOverlay(false);
    }

    // 5. Reset Crop Selection
    isCropping = false;
    cropSelection = { x: 0, y: 0, w: 0, h: 0 };
    manualAnchorPoint = null; // Reset Anchor
}

function updateStackButtonState() {
    if (!ui.alignMode) return;
    const flow = getActiveZenithFlow();
    const mode = flow.alignMode || ui.alignMode.value;
    const isGlobal = !flow.needsPoints || mode === "global" || mode === "zenith_map" || mode === "zenith_v3";

    // Ocultar wrapper de puntos si es global
    const mpWrapper = $("#multipoint-wrapper");
    if (mpWrapper) {
        mpWrapper.style.display = isGlobal ? "none" : "block";
    }

    // GATING ANTI-ERROR: el apilado requiere análisis previo (el backend lee el
    // caché de análisis; sin él falla). currentFileMetadata solo existe tras un
    // análisis exitoso y se limpia al cargar otro archivo.
    const hasAnalysis = !!currentFileMetadata && !!currentFilePath;

    // Modos Multi-Punto (Liquid Warping): Solo requieren Grid (El anclaje manual es opcional)
    const hasAnchor = (manualAnchorPoint !== null);
    const hasGrid = (activeAPoints && activeAPoints.length > 0);
    const isReady = hasAnalysis && (isZenithUltimateSelected() || isGlobal || hasGrid);

    let tip = "";
    if (!isReady) {
        tip = "Configuración incompleta:";
        if (!hasAnalysis) tip = tip + " [Analiza el video primero]";
        else {
            if (!hasAnchor) tip = tip + " [Opcional: Anclaje Manual]";
            if (!hasGrid) tip = tip + " [Falta Generar Malla (Smart AP)]";
        }
    } else {
        tip = "Listo para procesar";
    }

    if (ui.btnStack) {
        ui.btnStack.disabled = !isReady;
        ui.btnStack.style.opacity = isReady ? "1" : "0.5";
        ui.btnStack.title = tip;
    }

    if (ui.btnBatchRun) {
        // En modo batch, solo habilitar si es ready Y hay archivos cargados
        const hasFiles = (batchFiles && batchFiles.length > 0);
        const canRunBatch = isReady && hasFiles;
        ui.btnBatchRun.disabled = !canRunBatch;
        ui.btnBatchRun.style.opacity = canRunBatch ? "1" : "0.5";
        ui.btnBatchRun.title = canRunBatch
            ? tr("batch.execution.ready_tooltip", "Ejecutar lote")
            : (hasFiles ? tip : tr("batch.execution.load_files_tooltip", "Carga archivos primero"));
    }
}

// Listener para cambio de modo
if (ui.alignMode) {
    ui.alignMode.addEventListener("change", updateStackButtonState);
}

// 1. Selector de Carpeta para Batch
if (ui.btnBatchMode) {
    ui.btnBatchMode.addEventListener("click", async () => {
        const folder = await openDialog({ directory: true, multiple: false });
        if (!folder) return;

        // CLEANUP
        resetDataAcquisitionUI();

        const selectedBatchTarget = normalizeZenithCategory(ui.selBatchTargetCategory?.value || getSelectedTargetCategory());

        // Sincronizar el flujo con el selector lateral de Batch.
        if (ui.selBatchTargetCategory) {
            ui.selBatchTargetCategory.value = selectedBatchTarget;
        }
        const mainTarget = document.getElementById("sel-target-category");
        if (mainTarget) {
            mainTarget.value = selectedBatchTarget;
            applyZenithUltimateFlow();
        }

        const isRecursive = await showCustomChoice(
            tr("batch.scan.title", "Configuracion de Escaneo"),
            tr("batch.scan.question", "¿Deseas buscar videos en subcarpetas (recursivo) o solo en la carpeta raiz?"),
            tr("batch.scan.recursive", "Recursivo"),
            tr("batch.scan.root_only", "Solo Raiz")
        );

        // Tutorial: Advance after Recursive selection is now handled by modal auto-advance

        showProcessing(tr("batch.scan.scanning", "ESCANEANDO..."));
        try {
            const files = await invoke("scan_directory", { path: String(folder), recursive: isRecursive });

            if (files.length === 0) {
                showCustomAlert(tr("batch.scan.empty_title", "Sin Archivos"), tr("batch.scan.empty_message", "No se encontraron videos .SER o .AVI en la carpeta."));
                return;
            }

            isBatchMode = true;
            batchSourcePath = folder;
            batchFiles = files;

            ui.panelBatch.style.display = "block";

            if (ui.batchSourcePath) ui.batchSourcePath.textContent = folder;
            if (ui.batchCount) ui.batchCount.textContent = files.length;

            log("INFO", trFormat("batch.logs.activated", { count: files.length }, `Modo Batch activado. ${files.length} archivos.`));
            updateStackButtonState();
            maybeStartTutorialFlow("batch", 1, 350);

            // Tutorial: Detect if images are loaded and skip to Step 4 if so
            if (tutorialManager && tutorialManager.currentFlowName === 'batch') {
                const first = files[0].toLowerCase();
                const isImg = first.endsWith(".tif") || first.endsWith(".tiff") || first.endsWith(".png") || first.endsWith(".jpg");
                if (isImg) {
                    setTimeout(() => tutorialManager.jumpToStep(5), 500);
                }
            }

        } catch (e) {
            log("ERROR", "Scan: " + e);
            showCustomAlert(tr("general.error", "Error"), trFormat("batch.scan.error_message", { error: e }, "Error al escanear: " + e));
        } finally {
            hideProcessing();
        }
    });

}

// 1.5. Boton Mosaico
if ($("#btn-mosaic-mode")) {
    $("#btn-mosaic-mode").addEventListener("click", () => {
        console.log("MAIN: Mosaic Mode Button Clicked");
        try {
            // Init if not exists
            if (!mosaicManager) {
                console.log("MAIN: Initializing MosaicManager...");
                mosaicManager = new MosaicManager();
            }

            // CLEANUP
            resetDataAcquisitionUI();

            // Show Mosaic Panel
            const pMosaic = document.getElementById("panel-mosaic");
            if (pMosaic) {
                pMosaic.style.display = "flex"; // Force flex to ensure layout
                pMosaic.style.flexDirection = "column";
                // Ensure it scrolls into view for users with smaller screens
                setTimeout(() => pMosaic.scrollIntoView({ behavior: 'smooth', block: 'start' }), 50);
            } else {
                console.error("MAIN: panel-mosaic not found in DOM");
            }

            // Show Overlay
            if (mosaicManager) {
                mosaicManager.toggleOverlay(true);
            }
            maybeStartTutorialFlow("mosaic", 1, 450);

            // HIDE Deringing UI in Mosaic Mode
            if (ui.selDrMode) {
                ui.selDrMode.parentElement.style.display = "none";
                if (ui.panelDrManual) ui.panelDrManual.style.display = "none";
            }

            log("INFO", "Modo Mosaico Activado.");
        } catch (e) {
            console.error("MAIN: Error entering Mosaic Mode:", e);
            log("ERROR", "Fallo modo mosaico: " + e);
        }
    });
}

// 1.6. Planetary Derotation Mode
(function () {
    const btnDerotateMode = document.getElementById("btn-derotate-mode");
    const modal = document.getElementById("derotation-modal");
    if (!btnDerotateMode || !modal) return;

    const state = {
        imagePath: "",
        preflight: null,
        diagnostics: null,
        autoDisc: null,
        disc: null,
        planet: "jupiter",
        logPath: "",
        sourceKind: "",
        usingCurrentStack: false
    };

    const img = document.getElementById("derot-preview-img");
    const placeholder = document.getElementById("derot-placeholder");
    const canvas = document.getElementById("derot-wireframe-canvas");
    const qualityText = document.getElementById("derot-quality-text");
    const qualityDot = document.getElementById("derot-quality-dot");
    const inputCapture = document.getElementById("derot-capture-time");
    const inputReference = document.getElementById("derot-reference-time");
    const cmSelect = document.getElementById("derot-cm-system");
    const limbSlider = document.getElementById("sl-derot-limb");
    const limbValue = document.getElementById("derot-limb-value");
    const fusionIntervalInput = document.getElementById("derot-fusion-interval-sec");
    const diagConfidence = document.getElementById("derot-diag-confidence");
    const diagDelta = document.getElementById("derot-diag-delta");
    const diagTimeSource = document.getElementById("derot-diag-time-source");
    const diagDisc = document.getElementById("derot-diag-disc");
    const diagContrast = document.getElementById("derot-diag-contrast");
    const diagWarnings = document.getElementById("derot-warning-list");
    const logStatus = document.getElementById("derot-log-status");
    const discInputs = {
        cx: document.getElementById("derot-disc-cx"),
        cy: document.getElementById("derot-disc-cy"),
        rx: document.getElementById("derot-disc-rx"),
        ry: document.getElementById("derot-disc-ry"),
        angle: document.getElementById("derot-disc-angle")
    };
    const b0Input = document.getElementById("derot-b0-lat");
    const discReadouts = {
        center: document.getElementById("derot-readout-center"),
        size: document.getElementById("derot-readout-size"),
        angle: document.getElementById("derot-readout-angle"),
        b0: document.getElementById("derot-readout-b0")
    };
    const planetRates = {
        jupiter: [877.9, 870.27, 870.536],
        saturn: [844.3, 812.0, 810.7938],
        mars: [350.89198507, 350.89198507, 350.89198507]
    };

    function setDerotStatus(text, tone = "ready") {
        if (qualityText) qualityText.textContent = text;
        if (!qualityDot) return;
        const colors = {
            ready: "#22c55e",
            busy: "#38bdf8",
            warn: "#f59e0b",
            error: "#ef4444"
        };
        qualityDot.style.background = colors[tone] || colors.ready;
    }

    function setActivePlanet(containerId, planet) {
        const container = document.getElementById(containerId);
        if (!container) return;
        container.querySelectorAll(".planet-icon").forEach((button) => {
            const isActive = button.dataset.planet === planet;
            button.classList.toggle("active", isActive);
            button.style.border = isActive ? "2px solid #8b5cf6" : "1px solid rgba(255,255,255,0.15)";
            button.style.background = isActive ? "rgba(139,92,246,0.2)" : "rgba(255,255,255,0.05)";
            button.style.color = isActive ? "#c4b5fd" : "#94a3b8";
        });
    }

    function formatDerotTimeSource(source) {
        if (source === "manual_log") return tr("derotation.time_source.manual_log", "TXT manual");
        if (source === "log") return tr("derotation.time_source.log", "Log");
        if (source === "file_modified") return tr("derotation.time_source.file_modified", "Archivo");
        if (source === "manual") return tr("derotation.time_source.manual", "Manual");
        return tr("derotation.time_source.unknown", "Sin dato");
    }

    function parseDerotUtcInput(value) {
        if (!value) return null;
        const [date, time = "00:00:00"] = value.split("T");
        const [year, month, day] = date.split("-").map(Number);
        const [hour = 0, minute = 0, second = 0] = time.split(":").map(Number);
        if (![year, month, day].every(Number.isFinite)) return null;
        return Date.UTC(year, (month || 1) - 1, day || 1, hour || 0, minute || 0, second || 0);
    }

    function estimateDerotDelta() {
        const capture = parseDerotUtcInput(inputCapture?.value || "");
        const reference = parseDerotUtcInput(inputReference?.value || "");
        if (capture === null || reference === null) return null;
        const system = parseInt(cmSelect?.value || "1", 10);
        const rate = (planetRates[state.planet] || planetRates.jupiter)[Math.max(0, Math.min(2, system))];
        return rate * ((capture - reference) / 86400000);
    }

    function syncDiscInputs(disc) {
        if (!disc) return;
        if (discInputs.cx) discInputs.cx.value = Number(disc.cx || 0).toFixed(1);
        if (discInputs.cy) discInputs.cy.value = Number(disc.cy || 0).toFixed(1);
        if (discInputs.rx) discInputs.rx.value = Number(disc.radius_x || 0).toFixed(1);
        if (discInputs.ry) discInputs.ry.value = Number(disc.radius_y || 0).toFixed(1);
        if (discInputs.angle) discInputs.angle.value = Number(disc.angle_deg || 0).toFixed(1);
        updateDiscReadouts();
    }

    function readDiscInputs() {
        if (!state.disc) return null;
        const cx = parseFloat(discInputs.cx?.value || state.disc.cx);
        const cy = parseFloat(discInputs.cy?.value || state.disc.cy);
        const rx = parseFloat(discInputs.rx?.value || state.disc.radius_x);
        const ry = parseFloat(discInputs.ry?.value || state.disc.radius_y);
        const angle = parseFloat(discInputs.angle?.value || state.disc.angle_deg || 0);
        if (![cx, cy, rx, ry, angle].every(Number.isFinite) || rx <= 0 || ry <= 0) return state.disc;
        return {
            ...state.disc,
            cx,
            cy,
            radius_x: rx,
            radius_y: ry,
            angle_deg: angle
        };
    }

    function getB0Value() {
        const value = parseFloat(b0Input?.value || state.preflight?.b0_deg || state.diagnostics?.b0_deg || 0);
        return Number.isFinite(value) ? Math.max(-35, Math.min(35, value)) : 0;
    }

    function updateDiscReadouts() {
        const d = state.disc || readDiscInputs();
        if (discReadouts.center) {
            discReadouts.center.textContent = d ? `${Number(d.cx || 0).toFixed(0)}, ${Number(d.cy || 0).toFixed(0)}` : "--";
        }
        if (discReadouts.size) {
            discReadouts.size.textContent = d ? `${Number(d.radius_x || 0).toFixed(0)} x ${Number(d.radius_y || 0).toFixed(0)}` : "--";
        }
        if (discReadouts.angle) {
            discReadouts.angle.textContent = d ? `${Number(d.angle_deg || 0).toFixed(1)}°` : "--";
        }
        if (discReadouts.b0) {
            discReadouts.b0.textContent = `${getB0Value().toFixed(1)}°`;
        }
    }

    function setDerotLogStatus(path = state.logPath, source = state.diagnostics?.time_source || "") {
        if (!logStatus) return;
        if (path) {
            const name = String(path).split(/[\\/]/).pop();
            logStatus.textContent = `${formatDerotTimeSource(source || "manual_log")}: ${name}`;
            logStatus.style.color = "#7dd3fc";
        } else {
            logStatus.textContent = tr(
                "derotation.log.auto_hint",
                "Sin TXT manual. Se intentará leer SharpCap/FireCapture junto a la imagen."
            );
            logStatus.style.color = "#64748b";
        }
    }

    function renderDerotDiagnostics(diagnostics = state.diagnostics, manual = false) {
        const delta = estimateDerotDelta();
        const confidence = Number(diagnostics?.confidence ?? 0);
        const tone = confidence >= 0.58 ? "ready" : confidence >= 0.35 ? "warn" : "error";
        if (diagConfidence) {
            diagConfidence.textContent = manual
                ? tr("derotation.diagnostics.manual", "Manual")
                : diagnostics
                    ? `${Math.round(confidence * 100)}% · ${diagnostics.classification || "review"}`
                    : "--";
            diagConfidence.style.color = tone === "ready" ? "#34d399" : tone === "warn" ? "#fbbf24" : "#fb7185";
        }
        if (diagDelta) {
            const value = Number.isFinite(delta) ? delta : Number(diagnostics?.delta_deg ?? NaN);
            diagDelta.textContent = Number.isFinite(value) ? `${value.toFixed(3)}°` : "--";
            diagDelta.style.color = Math.abs(value || 0) > 75 ? "#fb7185" : "#e2e8f0";
        }
        if (diagTimeSource) diagTimeSource.textContent = formatDerotTimeSource(diagnostics?.time_source || "");
        if (diagDisc) {
            const d = state.disc;
            diagDisc.textContent = d
                ? `${Math.round(d.radius_x || 0)}x${Math.round(d.radius_y || 0)} px · ${Number(d.angle_deg || 0).toFixed(1)}°`
                : "--";
        }
        if (diagContrast) {
            const ratio = Number(diagnostics?.contrast_ratio ?? NaN);
            diagContrast.textContent = Number.isFinite(ratio) ? `${ratio.toFixed(2)}x` : "--";
        }
        if (diagWarnings) {
            const warnings = [...(diagnostics?.warnings || [])];
            if (manual) warnings.unshift(tr("derotation.warnings.manual_geometry", "Geometría ajustada manualmente; verifica que la malla siga el limbo real."));
            diagWarnings.innerHTML = warnings.length
                ? warnings.slice(0, 4).map((w) => `<div>• ${String(w)}</div>`).join("")
                : `<span style="color:#34d399;">${tr("derotation.diagnostics.ready", "Geometría lista para aplicar.")}</span>`;
        }
        if (diagnostics) {
            setDerotStatus(
                diagnostics.can_apply
                    ? tr("derotation.status.geometry_ready", "Geometría lista.")
                    : tr("derotation.status.geometry_review", "Revisa la geometría."),
                tone
            );
        }
    }

    function drawDerotationWireframe() {
        if (!canvas || !img || !state.disc || !img.naturalWidth || !img.naturalHeight) return;
        const container = canvas.parentElement;
        if (!container) return;

        const width = container.clientWidth;
        const height = container.clientHeight;
        canvas.width = width;
        canvas.height = height;
        const ctx = canvas.getContext("2d");
        ctx.clearRect(0, 0, width, height);

        const scale = Math.min(width / img.naturalWidth, height / img.naturalHeight);
        const offsetX = (width - img.naturalWidth * scale) / 2;
        const offsetY = (height - img.naturalHeight * scale) / 2;
        const d = state.disc;
        const cx = offsetX + d.cx * scale;
        const cy = offsetY + d.cy * scale;
        const rx = d.radius_x * scale;
        const ry = d.radius_y * scale;

        ctx.save();
        ctx.lineWidth = 1.5;
        ctx.strokeStyle = "rgba(196,181,253,0.95)";
        ctx.setLineDash([]);
        ctx.beginPath();
        ctx.ellipse(cx, cy, rx, ry, 0, 0, Math.PI * 2);
        ctx.stroke();

        ctx.strokeStyle = "rgba(56,189,248,0.65)";
        ctx.lineWidth = 1;
        for (let i = -2; i <= 2; i++) {
            ctx.beginPath();
            ctx.ellipse(cx + (rx * i / 5), cy, Math.max(1, rx * 0.22), ry, 0, -Math.PI / 2, Math.PI / 2);
            ctx.stroke();
        }
        for (let i = -2; i <= 2; i++) {
            ctx.beginPath();
            ctx.ellipse(cx, cy + (ry * i / 5), rx, Math.max(1, ry * 0.18), 0, 0, Math.PI * 2);
            ctx.stroke();
        }
        ctx.fillStyle = "#fbbf24";
        ctx.beginPath();
        ctx.arc(cx, cy, 3, 0, Math.PI * 2);
        ctx.fill();
        ctx.restore();
    }

    async function loadDerotationResultIntoWorkspace(result, sourceLabel = "Derotation") {
        modal.style.display = "none";
        window.setCurrentFilePath?.(result.output_path || state.imagePath || "");
        currentFilePath = result.output_path || currentFilePath;
        currentFileMetadata = {
            width: result.width,
            height: result.height,
            frame_count: result.frame_count || 1,
            bpp: 3,
            color_id: 0,
            pattern_name: sourceLabel,
            file_size_mb: 0,
            is_color: true
        };
        currentVideoStats = {
            avg_quality: 100,
            quality_stability: 100,
            worst_score: 100,
            best_score: 100
        };

        if (ui.iRes) ui.iRes.textContent = `${result.width}x${result.height}`;
        if (ui.iFrames) ui.iFrames.textContent = String(result.frame_count || 1);
        if (ui.iPat) ui.iPat.textContent = sourceLabel;
        if (ui.statAvgQual) ui.statAvgQual.textContent = "100.00%";
        if (ui.statStability) ui.statStability.textContent = "100.00%";
        if (ui.statWorst) ui.statWorst.textContent = "-";
        if (ui.statBest) ui.statBest.textContent = "-";

        if (ui.panelInfo) ui.panelInfo.style.display = "block";
        if (ui.panelAnalysis) ui.panelAnalysis.style.display = "none";
        if (ui.panelTools) ui.panelTools.style.display = "block";
        if (ui.panelWavelets) ui.panelWavelets.style.display = "block";
        if (ui.viewResult) ui.viewResult.style.display = "flex";
        if (ui.imgSource && state.preflight?.preview_base64) {
            await setImageAndWait(ui.imgSource, state.preflight.preview_base64, true);
        }
        if (ui.imgResult) {
            await setImageAndWait(ui.imgResult, result.preview_base64, false);
            fitToScreen();
            triggerStackSuccessEffect();
        }

        resetProcessingParams();
        lastProcessedParams = null;
    }

    async function loadDerotationPreflight(path, logPath = state.logPath || "") {
        state.imagePath = path;
        state.logPath = logPath || "";
        state.usingCurrentStack = false;
        setDerotStatus(tr("derotation.status.inspecting", "Analizando imagen..."), "busy");
        showProcessing(tr("derotation.processing.inspecting", "ANALIZANDO DEROTACIÓN..."));
        try {
            const preflight = await invoke("get_planetary_derotation_preflight", {
                imagePath: path,
                logPath: state.logPath || null
            });
            state.preflight = preflight;
            state.diagnostics = preflight.diagnostics || null;
            state.disc = preflight.detected_disc;
            state.autoDisc = preflight.detected_disc ? { ...preflight.detected_disc } : null;
            state.planet = preflight.suggested_planet || "jupiter";
            state.sourceKind = preflight.source_kind || "file";
            syncDiscInputs(state.disc);
            if (b0Input) b0Input.value = Number(preflight.b0_deg || 0).toFixed(1);
            updateDiscReadouts();

            if (inputCapture) inputCapture.value = preflight.capture_time || "";
            if (inputReference) inputReference.value = preflight.reference_time || preflight.capture_time || "";
            if (img) {
                img.style.display = "block";
                img.onload = drawDerotationWireframe;
                img.src = preflight.preview_base64;
            }
            if (placeholder) placeholder.style.display = "none";
            setActivePlanet("derot-planet-selector", state.planet);
            setDerotLogStatus(state.logPath, state.diagnostics?.time_source);
            setDerotStatus(
                trFormat(
                    "derotation.status.ready",
                    { resolution: `${preflight.width}x${preflight.height}` },
                    `Listo · ${preflight.width}x${preflight.height}`
                ),
                "ready"
            );
            renderDerotDiagnostics(state.diagnostics);
            requestAnimationFrame(drawDerotationWireframe);
        } finally {
            hideProcessing();
        }
    }

    async function loadCurrentStackPreflight(showErrors = true, logPath = state.logPath || "") {
        state.imagePath = window.getCurrentFilePath?.() || currentFilePath || "";
        state.logPath = logPath || "";
        state.usingCurrentStack = true;
        setDerotStatus(tr("derotation.status.inspecting", "Analizando imagen..."), "busy");
        showProcessing(tr("derotation.processing.inspecting", "ANALIZANDO DEROTACIÓN..."));
        try {
            const preflight = await invoke("get_current_stacked_derotation_preflight", {
                sourcePath: state.imagePath || null,
                logPath: state.logPath || null
            });
            state.preflight = preflight;
            state.diagnostics = preflight.diagnostics || null;
            state.disc = preflight.detected_disc;
            state.autoDisc = preflight.detected_disc ? { ...preflight.detected_disc } : null;
            state.planet = preflight.suggested_planet || "jupiter";
            state.sourceKind = preflight.source_kind || "current_stack";
            syncDiscInputs(state.disc);
            if (b0Input) b0Input.value = Number(preflight.b0_deg || 0).toFixed(1);
            updateDiscReadouts();
            if (inputCapture) inputCapture.value = preflight.capture_time || "";
            if (inputReference) inputReference.value = preflight.reference_time || preflight.capture_time || "";
            if (img) {
                img.style.display = "block";
                img.onload = drawDerotationWireframe;
                img.src = preflight.preview_base64;
            }
            if (placeholder) placeholder.style.display = "none";
            setActivePlanet("derot-planet-selector", state.planet);
            setDerotLogStatus(state.logPath, state.diagnostics?.time_source);
            setDerotStatus(
                trFormat(
                    "derotation.status.ready",
                    { resolution: `${preflight.width}x${preflight.height}` },
                    `Listo · ${preflight.width}x${preflight.height}`
                ),
                "ready"
            );
            renderDerotDiagnostics(state.diagnostics);
            requestAnimationFrame(drawDerotationWireframe);
            return true;
        } catch (e) {
            if (showErrors) showCustomAlert(tr("general.info", "Info"), normalizeBackendText(e));
            setDerotStatus(tr("derotation.status.waiting", "Carga una imagen apilada planetaria."), "warn");
            return false;
        } finally {
            hideProcessing();
        }
    }

    btnDerotateMode.addEventListener("click", async () => {
        modal.style.display = "flex";
        setDerotStatus(tr("derotation.status.waiting", "Carga una imagen apilada planetaria."), "warn");
        setActivePlanet("derot-planet-selector", state.planet);
        setDerotLogStatus();
        updateDiscReadouts();
        requestAnimationFrame(drawDerotationWireframe);
        if (ui.imgResult?.src && ui.imgResult.naturalWidth > 0) {
            await loadCurrentStackPreflight(false);
        }
    });

    document.getElementById("btn-derot-close")?.addEventListener("click", () => {
        modal.style.display = "none";
    });

    document.getElementById("btn-derot-load-image")?.addEventListener("click", async () => {
        const result = await openDialog({
            multiple: false,
            filters: [{ name: "Planetary image", extensions: ["png", "tif", "tiff", "jpg", "jpeg"] }]
        });
        if (!result) return;
        const path = (typeof result === "object" && result !== null && result.path) ? result.path : result;
        if (!path) return;
        try {
            await loadDerotationPreflight(path, "");
        } catch (e) {
            setDerotStatus(tr("general.error", "Error"), "error");
            showCustomAlert(tr("general.error", "Error"), normalizeBackendText(e));
        }
    });

    document.getElementById("btn-derot-use-current")?.addEventListener("click", async () => {
        await loadCurrentStackPreflight(true);
    });

    document.getElementById("derot-planet-selector")?.querySelectorAll(".planet-icon").forEach((button) => {
        button.addEventListener("click", () => {
            state.planet = button.dataset.planet || "jupiter";
            setActivePlanet("derot-planet-selector", state.planet);
            renderDerotDiagnostics(state.diagnostics);
        });
    });

    Object.values(discInputs).forEach((input) => {
        input?.addEventListener("input", () => {
            state.disc = readDiscInputs();
            updateDiscReadouts();
            renderDerotDiagnostics(state.diagnostics, true);
            drawDerotationWireframe();
        });
    });
    b0Input?.addEventListener("input", () => {
        updateDiscReadouts();
        renderDerotDiagnostics(state.diagnostics, true);
    });

    document.querySelectorAll("[data-derot-adjust]").forEach((button) => {
        button.addEventListener("click", (event) => {
            const field = button.dataset.derotAdjust;
            const rawDelta = parseFloat(button.dataset.delta || "0");
            if (!field || !Number.isFinite(rawDelta)) return;
            state.disc = readDiscInputs() || state.disc;
            if (!state.disc) return;
            const multiplier = event.shiftKey ? 5 : (event.altKey || event.metaKey ? 0.2 : 1);
            const delta = rawDelta * multiplier;
            if (field === "cx") state.disc.cx += delta;
            if (field === "cy") state.disc.cy += delta;
            if (field === "center" && state.autoDisc) {
                state.disc.cx = state.autoDisc.cx;
                state.disc.cy = state.autoDisc.cy;
            }
            if (field === "rx") state.disc.radius_x = Math.max(1, state.disc.radius_x + delta);
            if (field === "ry") state.disc.radius_y = Math.max(1, state.disc.radius_y + delta);
            if (field === "size") {
                state.disc.radius_x = Math.max(1, state.disc.radius_x + delta);
                state.disc.radius_y = Math.max(1, state.disc.radius_y + delta);
            }
            if (field === "angle") state.disc.angle_deg += delta;
            if (field === "b0" && b0Input) {
                const next = Math.max(-35, Math.min(35, getB0Value() + delta));
                b0Input.value = next.toFixed(1);
            }
            syncDiscInputs(state.disc);
            renderDerotDiagnostics(state.diagnostics, true);
            drawDerotationWireframe();
        });
    });

    document.getElementById("btn-derot-reset-disc")?.addEventListener("click", () => {
        if (!state.autoDisc) return;
        state.disc = { ...state.autoDisc };
        syncDiscInputs(state.disc);
        if (b0Input && state.preflight) b0Input.value = Number(state.preflight.b0_deg || 0).toFixed(1);
        updateDiscReadouts();
        renderDerotDiagnostics(state.diagnostics);
        drawDerotationWireframe();
    });

    [inputCapture, inputReference, cmSelect].forEach((el) => {
        el?.addEventListener("input", () => renderDerotDiagnostics(state.diagnostics));
        el?.addEventListener("change", () => renderDerotDiagnostics(state.diagnostics));
    });

    if (limbSlider && limbValue) {
        limbSlider.addEventListener("input", () => {
            limbValue.textContent = Number(limbSlider.value || 0).toFixed(1);
        });
    }

    document.getElementById("btn-derot-parse-log")?.addEventListener("click", async () => {
        try {
            const result = await openDialog({
                multiple: false,
                filters: [{ name: "SharpCap / FireCapture TXT", extensions: ["txt", "log"] }]
            });
            if (!result) return;
            const path = (typeof result === "object" && result !== null && result.path) ? result.path : result;
            if (!path) return;
            state.logPath = path;
            setDerotLogStatus(path, "manual_log");
            if (state.usingCurrentStack || (!state.imagePath && ui.imgResult?.src && ui.imgResult.naturalWidth > 0)) {
                await loadCurrentStackPreflight(true, path);
            } else if (state.imagePath) {
                await loadDerotationPreflight(state.imagePath, path);
            } else {
                showCustomAlert(
                    tr("general.info", "Info"),
                    tr("derotation.errors.load_image_after_log", "TXT cargado. Ahora carga una imagen o usa el resultado actual para aplicar esos tiempos.")
                );
            }
        } catch (e) {
            showCustomAlert(tr("general.error", "Error"), normalizeBackendText(e));
        }
    });

    document.getElementById("btn-derot-detect")?.addEventListener("click", async () => {
        if (!state.imagePath) {
            showCustomAlert(tr("general.info", "Info"), tr("derotation.errors.load_first", "Carga primero una imagen planetaria."));
            return;
        }
        showProcessing(tr("derotation.processing.detecting", "DETECTANDO DISCO..."));
        try {
            if (state.usingCurrentStack) {
                await loadCurrentStackPreflight(true, state.logPath);
                return;
            }
            const detection = await invoke("detect_planetary_derotation_disc", {
                imagePath: state.imagePath,
                planet: state.planet,
                logPath: state.logPath || null
            });
            state.disc = detection.detected_disc;
            state.autoDisc = detection.detected_disc ? { ...detection.detected_disc } : null;
            state.diagnostics = detection.diagnostics || state.diagnostics;
            syncDiscInputs(state.disc);
            setDerotStatus(tr("derotation.status.disc_ready", "Disco detectado."), "ready");
            renderDerotDiagnostics(state.diagnostics);
            drawDerotationWireframe();
        } catch (e) {
            setDerotStatus(tr("general.error", "Error"), "error");
            showCustomAlert(tr("general.error", "Error"), normalizeBackendText(e));
        } finally {
            hideProcessing();
        }
    });

    document.getElementById("btn-derot-fusion")?.addEventListener("click", async () => {
        try {
            const result = await openDialog({
                multiple: true,
                filters: [{ name: "Planetary stacks", extensions: ["png", "tif", "tiff", "jpg", "jpeg"] }]
            });
            if (!result) return;
            const paths = (Array.isArray(result) ? result : [result])
                .map((entry) => (typeof entry === "object" && entry !== null && entry.path) ? entry.path : entry)
                .filter(Boolean);
            if (paths.length < 2) {
                showCustomAlert(tr("general.info", "Info"), "Selecciona al menos 2 stacks planetarios para fusionar.");
                return;
            }

            state.disc = readDiscInputs() || state.disc;
            const fallbackIntervalSec = parseFloat(fusionIntervalInput?.value || "0");
            showProcessing("FUSIONANDO STACKS DEROTADOS...");
            const fusion = await invoke("fuse_planetary_derotation_stacks", {
                imagePaths: paths,
                planet: state.planet,
                cmSystem: parseInt(cmSelect?.value || "1", 10),
                limbStrength: parseFloat(limbSlider?.value || "0.5"),
                fallbackIntervalSec: Number.isFinite(fallbackIntervalSec) ? fallbackIntervalSec : 0,
                subEarthLatDeg: getB0Value(),
                northAngleDeg: state.disc ? Number(state.disc.angle_deg || 0) : null,
                discOverride: state.disc || null
            });

            state.diagnostics = fusion.diagnostics || state.diagnostics;
            state.disc = fusion.detected_disc || state.disc;
            syncDiscInputs(state.disc);
            await loadDerotationResultIntoWorkspace(fusion, `Derotation Fusion ${fusion.frame_count || paths.length}x`);
            const warningText = (fusion.warnings || []).length ? `\n\nAvisos:\n${fusion.warnings.join("\n")}` : "";
            const rejectPct = Number(fusion.rejected_pixel_fraction || 0) * 100;
            const gainRows = Array.isArray(fusion.normalization_gains)
                ? fusion.normalization_gains
                    .map((g, idx) => Array.isArray(g) ? `${idx + 1}: ${g.map((v) => Number(v || 1).toFixed(2)).join("/")}` : "")
                    .filter(Boolean)
                    .slice(0, 5)
                : [];
            const gainText = gainRows.length
                ? `\nNormalización RGB: ${gainRows.join("  ")}${fusion.normalization_gains.length > gainRows.length ? " ..." : ""}`
                : "";
            const rejectionText = `\nRechazo robusto: ${rejectPct.toFixed(2)}%`;
            log(
                "SUCCESS",
                `Fusión multi-stack derotada: ${fusion.frame_count || paths.length} stacks · ${Number(fusion.time_span_sec || 0).toFixed(1)}s · rechazo ${rejectPct.toFixed(2)}% · ${fusion.output_path}`
            );
            showCustomAlert(
                "Fusión derotada completada",
                `Resultado guardado:\n${fusion.output_path}\n\nStacks: ${fusion.frame_count || paths.length}\nVentana temporal: ${Number(fusion.time_span_sec || 0).toFixed(1)}s${gainText}${rejectionText}${warningText}`
            );
        } catch (e) {
            showCustomAlert(tr("general.error", "Error"), normalizeBackendText(e));
        } finally {
            hideProcessing();
        }
    });

    document.getElementById("btn-derot-apply")?.addEventListener("click", async () => {
        if (!state.imagePath) {
            showCustomAlert(tr("general.info", "Info"), tr("derotation.errors.load_first", "Carga primero una imagen planetaria."));
            return;
        }
        const captureTime = inputCapture?.value || "";
        const referenceTime = inputReference?.value || captureTime;
        if (!captureTime || !referenceTime) {
            showCustomAlert(tr("general.error", "Error"), tr("derotation.errors.time_required", "Define tiempo de captura y tiempo de referencia."));
            return;
        }

        showProcessing(tr("derotation.processing.applying", "APLICANDO DEROTACIÓN PLANETARIA..."));
        try {
            state.disc = readDiscInputs();
            const commonPayload = {
                planet: state.planet,
                captureTime,
                referenceTime,
                cmSystem: parseInt(cmSelect?.value || "1", 10),
                limbStrength: parseFloat(limbSlider?.value || "0.5"),
                subEarthLatDeg: getB0Value(),
                discOverride: state.disc
            };
            const result = state.usingCurrentStack
                ? await invoke("apply_current_stacked_planetary_derotation", {
                    ...commonPayload,
                    sourcePath: state.imagePath || null
                })
                : await invoke("apply_planetary_derotation", {
                    ...commonPayload,
                    imagePath: state.imagePath
                });
            state.diagnostics = result.diagnostics || state.diagnostics;

            modal.style.display = "none";
            window.setCurrentFilePath?.(state.imagePath || result.output_path);
            currentFileMetadata = {
                width: result.width,
                height: result.height,
                frame_count: 1,
                bpp: 3,
                color_id: 0,
                pattern_name: `Derotation ${result.planet}`,
                file_size_mb: (state.preflight?.source_size_bytes || 0) / (1024 * 1024),
                is_color: true
            };
            currentVideoStats = {
                avg_quality: 100,
                quality_stability: 100,
                worst_score: 100,
                best_score: 100
            };

            if (ui.iRes) ui.iRes.textContent = `${result.width}x${result.height}`;
            if (ui.iFrames) ui.iFrames.textContent = "1";
            if (ui.iPat) ui.iPat.textContent = `Derotation ${result.planet}`;
            if (ui.statAvgQual) ui.statAvgQual.textContent = "100.00%";
            if (ui.statStability) ui.statStability.textContent = "100.00%";
            if (ui.statWorst) ui.statWorst.textContent = "-";
            if (ui.statBest) ui.statBest.textContent = "-";

            if (ui.panelInfo) ui.panelInfo.style.display = "block";
            if (ui.panelAnalysis) ui.panelAnalysis.style.display = "none";
            if (ui.panelTools) ui.panelTools.style.display = "block";
            if (ui.panelWavelets) ui.panelWavelets.style.display = "block";
            if (ui.viewResult) ui.viewResult.style.display = "flex";

            if (ui.imgSource && state.preflight?.preview_base64) {
                await setImageAndWait(ui.imgSource, state.preflight.preview_base64, true);
            }
            if (ui.imgResult) {
                await setImageAndWait(ui.imgResult, result.preview_base64, false);
                fitToScreen();
                triggerStackSuccessEffect();
            }

            resetProcessingParams();
            lastProcessedParams = null;
            log(
                "SUCCESS",
                trFormat(
                    "derotation.logs.completed",
                    { delta: Number(result.delta_deg || 0).toFixed(3), path: result.output_path },
                    `Derotación planetaria completada (Δ ${Number(result.delta_deg || 0).toFixed(3)}°). ${result.output_path}`
                )
            );
            showCustomAlert(
                tr("derotation.success_title", "Derotación completada"),
                trFormat(
                    "derotation.success_message",
                    { path: result.output_path, delta: Number(result.delta_deg || 0).toFixed(3) },
                    `Imagen derotada guardada:\n${result.output_path}\n\nDelta aplicado: ${Number(result.delta_deg || 0).toFixed(3)}°`
                )
            );
        } catch (e) {
            showCustomAlert(tr("general.error", "Error"), normalizeBackendText(e));
        } finally {
            hideProcessing();
        }
    });

    window.addEventListener("resize", () => {
        if (modal.style.display !== "none") requestAnimationFrame(drawDerotationWireframe);
    });
})();

// 2. Sintonizar con el Primer Video
if (ui.btnBatchTune) {
    ui.btnBatchTune.addEventListener("click", async () => {
        if (batchFiles.length === 0) return;
        currentFilePath = batchFiles[0];

        // Hide batch control during calibration
        if (ui.panelBatch) ui.panelBatch.style.display = "none";

        // Reset Batch Progress to 0/N
        if (ui.batchProgressText) ui.batchProgressText.textContent = "0 / " + batchFiles.length;
        if (ui.batchProgressBar) ui.batchProgressBar.style.width = "0%";

        showProcessing(tr("batch.logs.reference_loading", "CARGANDO REFERENCIA..."));
        setTimeout(async () => {
            try {
                activeDrizzleFactor = 1.0;
                ui.viewResult.style.display = "none";
                ui.panelWavelets.style.display = "none";

                // 1. Cargar vista previa inicial
                const bOverride = getBayerOverrideValue();
                const res = await invoke("preview_video", { path: currentFilePath, bayerOverride: bOverride });
                applySuggestedTargetCategory(res.suggested_target);
                clearSourcePreviewSurface(res.width, res.height);

                // SHARPENING MODE AUTO-MANAGEMENT
                const selSharpen = document.getElementById("sel-sharpen-mode");
                if (selSharpen) {
                    if (!res.is_color) {
                        selSharpen.value = "luminance";
                        selSharpen.options[1].disabled = true; // Disable RGB
                    } else {
                        selSharpen.options[1].disabled = false;
                    }
                }

                await setImageAndWait(ui.imgSource, res.preview_base64, true);
                clearSourcePreviewSurface(res.width, res.height, false);
                fitToScreen();

                // STOP AUTO-ANALYSIS: Show "Analyze" button instead
                if (ui.analysisActions) {
                    ui.analysisActions.style.display = "block";
                    // Sync analysis mode with batch type
                    const selAnalysisMode = $("#sel-analysis-mode");
                    if (selAnalysisMode) {
                        const batchFlow = getZenithUltimateFlow(ui.selBatchTargetCategory?.value || getSelectedTargetCategory());
                        selAnalysisMode.value = batchFlow.analysisMode;
                        if (ui.alignMode) ui.alignMode.value = batchFlow.alignMode;
                    }
                }

                // Show Analysis Step in sidebar
                if (ui.batchStepAnalysis) ui.batchStepAnalysis.style.display = "block";

                // Hide panels that depend on analysis
                ui.panelInfo.style.display = "none";
                ui.panelAnalysis.style.display = "none";

                ui.btnBatchRun.disabled = true;

                log("SUCCESS", tr("batch.logs.reference_ready", "Referencia cargada. Pulsa 'Analizar Video' para continuar."));
                if (tutorialManager?.currentFlowName === 'batch' && tutorialManager.currentStepIndex === 3) {
                    tutorialManager.nextStep();
                }

            } catch (e) {
                log("ERROR", "Tune: " + e);
                showCustomAlert(tr("general.error", "Error"), trFormat("batch.reference.error_loading", { error: e }, "Error al cargar referencia: " + e));
            } finally {
                hideProcessing();
            }
        }, 100);
    });
}

// 3. Ejecutar Lote
if (ui.btnBatchRun) {
    ui.btnBatchRun.addEventListener("click", async () => {
        if (batchFiles.length === 0) return;

        const shouldPauseBatchRunTutorial =
            tutorialManager?.currentFlowName === 'batch' && tutorialManager.currentStepIndex === 13;
        if (shouldPauseBatchRunTutorial) {
            tutorialManager.hideOverlay();
        }

        const timestamp = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
        const sep = batchSourcePath.includes("\\") ? "\\" : "/";
        batchOutputFolder = `${batchSourcePath}${sep}Animacion_${timestamp}`;
        batchGeneratedImages = [];
        batchResultPaths = [];

        try {
            await mkdir(batchOutputFolder);
        } catch (e) {
            console.warn("No se pudo crear carpeta, se intentara guardar directo.", e);
            batchOutputFolder = batchSourcePath;
        }

        ui.btnBatchRun.disabled = true;
        ui.btnBatchTune.disabled = true;
        setBatchModeUI(true);

        try {
            const p = getPipelineParams();
            const stackPct = parseFloat(ui.stackSlider.value);
            const drizzle = parseFloat(ui.drizzleScale.value);
            const batchTarget = ui.selBatchTargetCategory?.value || getSelectedTargetCategory();
            const batchFlow = getZenithUltimateFlow(batchTarget);
            const align = batchFlow.alignMode;

            // Reset the shared batch anchor once, then keep it alive for all entries.
            await invoke("clear_app_memory").catch(() => {});

            // Obtener la categoría del objetivo desde el nuevo selector o un default seguro
            const actualBatchMode = batchFlow.batchMode;

            for (let i = 0; i < batchFiles.length; i++) {
                const file = batchFiles[i];
                const displayIdx = i + 1;
                const fileName = pathBaseName(file);

                ui.batchProgressText.textContent = `${displayIdx} / ${batchFiles.length}`;
                const pct = (displayIdx / batchFiles.length) * 100;
                ui.batchProgressBar.style.width = `${pct}%`;

                log("INFO", trFormat("batch.logs.processing_file", {
                    current: displayIdx,
                    total: batchFiles.length,
                    name: fileName
                }, `[Batch ${displayIdx}/${batchFiles.length}] Procesando: ${fileName}...`));

                try {
                    const bOverride = getBayerOverrideValue();
                    const result = await invoke("process_batch_entry", {
                        filePath: file,
                        outputFolder: batchOutputFolder,
                        stackPct: stackPct,
                        drizzle: drizzle,
                        alignMode: align,
                        u1: p.u[0], u2: p.u[1], u3: p.u[2], u4: p.u[3], u5: p.u[4],
                        w1: p.w[0], w2: p.w[1], w3: p.w[2], w4: p.w[3], w5: p.w[4], w6: p.w[5],
                        d1: p.d[0], d2: p.d[1], d3: p.d[2], d4: p.d[3], d5: p.d[4], d6: p.d[5],
                        gamma: p.color.g, saturation: p.color.s,
                        contrast: p.color.c, brightness: p.color.b,
                        rBal: p.color.rb, bBal: p.color.bb,
                        rX: p.shift.rx, rY: p.shift.ry, bX: p.shift.bx, bY: p.shift.by,

                        deringingMode: 0, // Always Off in batch — prevents white blob artifacts
                        deringingRadius: p.dr.rad,
                        deringingDark: p.dr.dark,
                        deringingLight: p.dr.light,
                        deringingMask: p.dr.mask,

                        crisp: p.crisp,
                        deconvIter: p.deconv.i, deconvSigma: p.deconv.s,
                        vcIter: p.deconv.vi, vcSigma: p.deconv.vs,
                        usmAmount: p.usm.a, usmRadius: p.usm.r, lceAmount: p.lce,
                        masterDenoise: p.masterDenoise,
                        masterDenoiseDetail: p.denoiseDetail,
                        masterDenoiseChroma: p.denoiseChroma,
                        blend: p.blend / 100.0,
                        useRgbSharpening: p.useRgbSharpening,
                        batchMode: actualBatchMode,
                        targetType: batchFlow.category,
                        bayerOverride: bOverride,
                        anchorOverride: getManualAnchorOverrideValue(),
                        sharpened: document.getElementById("chk-sharpened").checked,
                        sharpenIntensity: parseFloat(ui.selSharpenIntensity?.value || "0.5"),
                        doublePass: document.getElementById("chk-double-pass").checked,
                        warpingAnalysis: batchFlow.warpingAnalysis,
                        normalizeColors: document.getElementById("chk-normalize-colors") ? document.getElementById("chk-normalize-colors").checked : true,
                        isV3: batchFlow.isV3,
                        apGridSize: batchFlow.apSize,
                        apThreshold: batchFlow.apThreshold,
                        progressPrefix: `[${displayIdx}/${batchFiles.length}]`
                    });

                    const preview = result?.preview_base64 || result?.path;
                    if (preview && result?.path) {
                        batchGeneratedImages.push(preview);
                        batchResultPaths.push(result.path);
                    } else {
                        log("WARN", trFormat("batch.logs.file_failed", {
                            name: fileName,
                            error: tr("animation.errors.no_valid_images", "No se generaron imagenes validas para reproducir.")
                        }, `Fallo en ${fileName}: salida inválida`));
                    }

                    // Cleanup per item without destroying the shared batch anchor/dimensions.
                    await invoke("clear_stack_memory").catch(() => {});

                } catch (e) {
                    log("ERROR", trFormat("batch.logs.file_failed", { name: fileName, error: e }, `Fallo en ${file}: ${e}`));
                }
            }

            if (batchGeneratedImages.length === 0) {
                showCustomAlert(tr("general.error", "Error"), tr("batch.execution.no_outputs", "El lote terminó, pero no se generaron frames válidos."));
                return;
            }

            log("SUCCESS", tr("batch.logs.completed", "Lote completado: PNGs Guardados. Iniciando modo Animacion..."));
            startAnimationPlayer(batchGeneratedImages);
            if (shouldPauseBatchRunTutorial) {
                tutorialManager.showOverlay();
                setTimeout(() => tutorialManager.nextStep(), 500);
            }
        } finally {
            if (shouldPauseBatchRunTutorial && tutorialManager?.currentFlowName === 'batch' && tutorialManager.currentStepIndex === 13) {
                tutorialManager.showOverlay();
            }
            setBatchModeUI(false);
            ui.btnBatchRun.disabled = false;
            ui.btnBatchTune.disabled = false;
        }
    });
}

if (ui.btnAutoPsf) {
    ui.btnAutoPsf.addEventListener("click", async () => {
        if (ui.panelWavelets.style.display === "none") { showCustomAlert("Aviso", "Primero debes apilar el video."); return; }
        ui.btnAutoPsf.disabled = true; ui.btnAutoPsf.textContent = i18n.t("wavelets.deconvolution.analyzing");
        try {
            const res = await invoke("analyze_psf");
            log("INFO", res.msg);
            const mode = ui.selAutoMode.value;
            const sigma = Math.max(0.6, Math.min(parseFloat(res.sigma) || 1.2, 2.2));
            const iter = Math.max(1, Math.min(parseInt(res.iterations, 10) || 2, 3));
            const vcIter = mode === "vc" ? Math.max(1, Math.min(iter, 2)) : Math.max(1, Math.min(iter - 1, 2));
            if (mode === "rl" || mode === "both") {
                ui.slDeconvSigma.value = Math.round(sigma * 10); ui.valDeconvSigma.value = sigma.toFixed(1);
                ui.slDeconvIter.value = iter; ui.valDeconvIter.value = iter;
            }
            if (mode === "vc" || mode === "both") {
                const vcSig = Math.max(0.6, sigma - 0.2);
                ui.slVcSigma.value = Math.round(vcSig * 10); ui.valVcSigma.value = vcSig.toFixed(1);
                ui.slVcIter.value = vcIter; ui.valVcIter.value = vcIter;
            }
            log("SUCCESS", `PSF conservador: Sigma=${sigma.toFixed(1)}, RL=${iter}, VC=${mode === "rl" ? 0 : vcIter}`);
            triggerUpdate();
        } catch (e) { log("ERROR", "Auto PSF: " + e); showCustomAlert("Error", "Error: " + e); }
        finally { ui.btnAutoPsf.disabled = false; ui.btnAutoPsf.innerHTML = i18n.t("wavelets.deconvolution.auto_analyze"); }
    });
}

if (ui.btnAnalyze) {
    ui.btnAnalyze.addEventListener("click", async () => {
        const file = await openDialog({ filters: [{ name: 'Astro Video', extensions: ['ser', 'avi', 'mp4', 'mov', 'mkv', 'm4v', 'wmv', 'flv', 'mts', 'm2ts'] }] });
        if (!file) return;

        // CLEANUP (Keep file reference, but clean UI)
        resetDataAcquisitionUI();
        currentFilePath = file.path || file;
        // Nuevo archivo ⇒ el análisis anterior ya no es válido: bloquear Apilar
        // hasta que el nuevo análisis termine (evita errores de usuario).
        currentFileMetadata = null;
        updateStackButtonState();

        // Comprobación de formato (Auto-Conversion a SER)
        const ext = currentFilePath.split('.').pop().toLowerCase();
        const isRaw = ["ser", "fit", "fits", "fts"].includes(ext);

        if (!isRaw) {
            log("INFO", `Formato comprimido detectado (.${ext}). Usando Aceleración de Hardware GPU...`);

            // UI Limpia sin mención de conversión a disco
            await showCustomAlert(
                "Aceleración por Caché GPU Activa <svg class='zas-icon'><use href='#icon-rocket'></use></svg>",
                "Has seleccionado un video comprimido. \n\nAstro Stacker procesará este video mediante **Aceleración Secuencial por Hardware (Caché GPU)** directamente en Memoria RAM para obtener máxima velocidad de lectura sin consumir espacio excesivo en tu disco duro."
            );

            // Note: We bypass `convert_video_to_ser_frontend` entirely
            // and keep `currentFilePath` as the original MP4/AVI.
            // The backend's FfmpegReader will handle the fast streaming.
        }

        // EXTRAER EL TARGET CATEGORY DIRECTO DE LA UI (NO MÁS MODALES)
        applyZenithUltimateFlow();
        const targetCategory = getSelectedTargetCategory();

        // AUTO CONFIGURAR EL DOMAIN DEL BACKEND
        currentAnalysisMode = getZenithUltimateFlow(targetCategory).analysisMode;

        showProcessing("CARGANDO VISTA PREVIA...");
        setTimeout(async () => {
            try {
                activeDrizzleFactor = 1.0;
                ui.btnStack.disabled = true;

                log("INFO", "Leyendo frame...");
                const bOverride = getBayerOverrideValue();
                const res = await invoke("preview_video", { path: currentFilePath, bayerOverride: bOverride });
                applySuggestedTargetCategory(res.suggested_target);
                clearSourcePreviewSurface(res.width, res.height);

                // SHARPENING MODE AUTO-MANAGEMENT
                const selSharpen = document.getElementById("sel-sharpen-mode");
                if (selSharpen) {
                    if (!res.is_color) {
                        selSharpen.value = "luminance";
                        selSharpen.options[1].disabled = true; // Disable RGB
                    } else {
                        selSharpen.options[1].disabled = false;
                    }
                }

                if (ui.imgSource) {
                    await setImageAndWait(ui.imgSource, res.preview_base64, true);
                    clearSourcePreviewSurface(res.width, res.height, false);

                    // FORCE Fit again after a short delay to ensure layout (sidebar/panels) has settled
                    // This fixes the "video load not centered" issue if the viewport size changed
                    requestAnimationFrame(() => fitToScreen());
                }
                if (ui.analysisActions) {
                    ui.analysisActions.style.display = "block";
                    // Tutorial: nextStep() call removed here to allow user to see and click the button in Step 2
                }
                if (ui.selectedFilename) ui.selectedFilename.textContent = res.filename;

                // AUTOMACION DE CONFIGURACION PARA VIDEO EN COLOR (NUEVO)
                if (res.is_color) {
                    log("INFO", "Video en color detectado. Aplicando sRGB y Bayer Auto.");
                    if (ui.selColorSpaceOverride) ui.selColorSpaceOverride.value = "srgb";
                    if (ui.selBayerOverride) ui.selBayerOverride.value = "auto";
                }

                log("SUCCESS", "Video cargado.");
                if (ui.statusText) ui.statusText.textContent = "Listo. Esperando accion.";
                maybeStartTutorialFlow('individual', 1, 250);

                // INITIATE GUIDED UI GLOW SEQUENCE
                const qmContainer = document.getElementById("quality-method-container");
                const tcContainer = document.getElementById("container-target-category");

                if (qmContainer && tcContainer) {
                    qmContainer.classList.add("guided-glow");
                    
                    const removeQmGlow = () => {
                        qmContainer.classList.remove("guided-glow");
                        qmContainer.removeEventListener("click", removeQmGlow, true);
                        
                        tcContainer.classList.add("guided-glow");
                        const removeTcGlow = () => {
                            tcContainer.classList.remove("guided-glow");
                            tcContainer.removeEventListener("click", removeTcGlow, true);
                        };
                        tcContainer.addEventListener("click", removeTcGlow, true);
                    };
                    qmContainer.addEventListener("click", removeQmGlow, true);
                }

            } catch (e) { log("ERROR", e); showCustomAlert("Error", "Error al abrir video: " + e); }
            finally { hideProcessing(); }
        }, 100);
    });
}

// -------------------------------------------------------------
// SECUENCIA FITS IMPORT
const btnAnalyzeFits = $("#btn-analyze-fits");
if (btnAnalyzeFits) {
    btnAnalyzeFits.addEventListener("click", async () => {
        const folder = await openDialog({ directory: true, multiple: false });
        if (!folder) return;

        // CLEANUP (Keep file reference, but clean UI)
        resetDataAcquisitionUI();
        currentFilePath = folder;

        const modeChoice = await showCustomChoice(
            "Modo de Analisis (FITS)",
            `¿Qué tipo de objeto vas a procesar ?
                            <div style="margin-top:8px; display:flex; flex-direction:column; gap:6px;">
                                <div class="choice-card" data-modal-result="planet_v2">
                                    <div style="display:flex; gap:10px; align-items:center;">
                                        <span style="font-size:1.4em;">🪐</span>
                                        <div>
                                            <strong style="color:#fcd34d; font-size:0.95rem; display:block; margin-bottom:0;">Planetario v2 (PPA)</strong>
                                            <div style="color:#94a3b8; font-size:0.8rem; line-height:1.2;">Recomendado para fotografías planetarias FITS.</div>
                                        </div>
                                    </div>
                                </div>
                                <div class="choice-card" data-modal-result="surface_v2">
                                    <div style="display:flex; gap:10px; align-items:center;">
                                        <span style="font-size:1.4em;">🌑</span>
                                        <div>
                                            <strong style="color:#38bdf8; font-size:0.95rem; display:block; margin-bottom:0;">Superficie v2 (Features)</strong>
                                            <div style="color:#94a3b8; font-size:0.8rem; line-height:1.2;">Recomendado. Estabilización Lunar/Solar FITS.</div>
                                        </div>
                                    </div>
                                </div>
                            </div>`, null, null
        );

        if (!modeChoice) return;
        if (ui.selAnalysisMode) ui.selAnalysisMode.value = modeChoice;
        currentAnalysisMode = modeChoice;

        ui.txtSelectedFile.textContent = "Secuencia FITS: " + currentFilePath;
        $("#analysis-actions").style.display = "block";
        $("#stack-actions").style.display = "none";

        if (currentAnalysisMode.startsWith("planet")) {
            const btnManualAnchor = $("#btn-manual-anchor");
            if (btnManualAnchor) btnManualAnchor.style.display = "none";
        } else {
            const btnManualAnchor = $("#btn-manual-anchor");
            if (btnManualAnchor) btnManualAnchor.style.display = "block";
        }

        manualAnchorPoint = null;
        isSettingManualAnchor = false;
        if (ui.btnManualAnchor) {
            ui.btnManualAnchor.textContent = "📍 Definir Anclaje Manual";
            ui.btnManualAnchor.className = "secondary";
        }

        try {
            showProcessing("CARGANDO SECUENCIA FITS...");
            const bOverride = getBayerOverrideValue();
            const res = await invoke("preview_video", { path: currentFilePath, bayerOverride: bOverride });
            applySuggestedTargetCategory(res.suggested_target);
            clearSourcePreviewSurface(res.width, res.height);

            currentVideoStats = res;

            // SHARPENING MODE AUTO-MANAGEMENT
            const selSharpen = document.getElementById("sel-sharpen-mode");
            if (selSharpen) {
                if (!res.is_color) {
                    selSharpen.value = "luminance";
                    selSharpen.options[1].disabled = true; // Disable RGB
                } else {
                    selSharpen.options[1].disabled = false;
                }
            }

            if (ui.imgSource) {
                await setImageAndWait(ui.imgSource, res.preview_base64, true);
                clearSourcePreviewSurface(res.width, res.height, false);
                requestAnimationFrame(() => fitToScreen());
            }

            ui.txtSelectedFile.innerHTML = `
                                < strong >📁 ${currentFilePath}</strong > <br>
                                    <span style="color:#38bdf8; font-size:0.8rem;">
                                        ${res.width}x${res.height} | ${res.frame_count} frames FITS | BPP: ${res.bytes_per_pixel}
                                    </span>
                                    `;

            if (res.is_color) {
                log("INFO", "Video en color detectado. Aplicando sRGB y Bayer Auto.");
                if (ui.selColorSpaceOverride) ui.selColorSpaceOverride.value = "srgb";
                if (ui.selBayerOverride) ui.selBayerOverride.value = "auto";
            }
            log("SUCCESS", `Secuencia FITS lista: ${res.width}x${res.height}, ${res.frame_count} frames`);
            maybeStartTutorialFlow('individual', 1, 250);
        } catch (e) {
            log("ERROR", e);
            showCustomAlert("Error FITS", "El directorio no contiene imágenes .fits / .fit legibles o están dañadas.");
        } finally {
            hideProcessing();
        }
    });
}
// -------------------------------------------------------------

if (ui.btnRunAnalysis) {
    ui.btnRunAnalysis.addEventListener("click", async () => {
        if (!currentFilePath) return;

        // Ensure batch panel stays hidden during analysis
        if (isBatchMode && ui.panelBatch) ui.panelBatch.style.display = "none";

        const shouldAdvanceIndividualToProcessing =
            tutorialManager?.currentFlowName === 'individual' && tutorialManager.currentStepIndex === 2;
        if (shouldAdvanceIndividualToProcessing) {
            tutorialManager.nextStep();
        }

        showProcessing("ANALIZANDO FRAMES...");

        // Start Timer (Visual Feedback like Stacking)
        startStackingTimer();

        setTimeout(async () => {
            try {
                log("INFO", "Analisis completo...");
                // ENVIAR EL MODO AL BACKEND
                const bOverride = getBayerOverrideValue();

                applyZenithUltimateFlow();
                const flow = getActiveZenithFlow();
                // LEER MODO DE ANALISIS DEL DROPDOWN (V2)
                const analysisMode = getAnalysisModeValue(flow);
                currentAnalysisMode = analysisMode; // FIX: Sync global state
                const warpingAnalysis = flow.warpingAnalysis;
                log("INFO", `Iniciando ${flow.name} con modo: ${analysisMode} (${flow.category}, Warping: ${warpingAnalysis})`);

                const res = await invoke("analyze_video", {
                    path: currentFilePath,
                    mode: analysisMode,
                    targetType: flow.category,
                    warpingAnalysis, // NEW
                    bayerOverride: bOverride,
                    anchorOverride: getManualAnchorOverrideValue()
                });

                const format = (v) => (v === undefined || v === null || isNaN(v)) ? "-" : Math.min(100, Math.max(0, v)).toFixed(2) + "%";

                if (ui.iRes) ui.iRes.textContent = `${res.metadata.width}x${res.metadata.height}`;
                if (ui.iFrames) ui.iFrames.textContent = res.metadata.frame_count;
                if (ui.iPat) ui.iPat.textContent = res.metadata.pattern_name;
                if (ui.statAvgQual) ui.statAvgQual.textContent = format(res.stats.avg_quality);
                if (ui.statStability) ui.statStability.textContent = format(res.stats.quality_stability);
                if (ui.statWorst) ui.statWorst.textContent = format(res.stats.worst_score);
                if (ui.statBest) ui.statBest.textContent = format(res.stats.best_score);
                ui.stackSlider.value = res.recommended_pct; ui.pctDisplay.textContent = res.recommended_pct.toFixed(0) + "%";
                ui.panelInfo.style.display = "block"; ui.panelAnalysis.style.display = "block";
                ui.analysisActions.style.display = "none";
                if (isBatchMode) {
                    ui.btnAnalyze.textContent = tr("general.change_reference", "📂 Cambiar Referencia");
                    // Reveal Stacking Step in Batch Sidebar
                    if (ui.batchStepStacking) ui.batchStepStacking.style.display = "block";
                } else {
                    ui.btnAnalyze.textContent = tr("general.load_another_video", "📂 Cargar Otro Video");
                }
                updateStackButtonState();

                // AUTOMACION DE CONFIGURACION PARA VIDEO EN COLOR
                if (res.metadata.is_color) {
                    log("INFO", "Video en color detectado. Aplicando sRGB y Bayer Auto.");
                    if (ui.selColorSpaceOverride) ui.selColorSpaceOverride.value = "srgb";
                    if (ui.selBayerOverride) ui.selBayerOverride.value = "auto";
                }

                // SHARPENING MODE AUTO-MANAGEMENT
                const selSharpen = document.getElementById("sel-sharpen-mode");
                if (selSharpen) {
                    if (!res.metadata.is_color) {
                        selSharpen.value = "luminance";
                        selSharpen.options[1].disabled = true; // Disable RGB
                    } else {
                        selSharpen.options[1].disabled = false;
                    }
                }

                // NORMALIZAR COLORES: sin efecto en video mono — se deshabilita
                // para evitar confusión del usuario (el backend ya lo ignora).
                const chkNorm = document.getElementById("chk-normalize-colors");
                if (chkNorm) {
                    chkNorm.disabled = !res.metadata.is_color;
                    chkNorm.checked = !!res.metadata.is_color;
                }

                const shouldAdvanceAfterAnalysis =
                    (tutorialManager?.currentFlowName === 'individual' && tutorialManager.currentStepIndex === 3)
                    || (tutorialManager?.currentFlowName === 'batch' && tutorialManager.currentStepIndex === 4);
                if (shouldAdvanceAfterAnalysis) {
                    setTimeout(() => tutorialManager.nextStep(), 500);
                }

                currentGraphData = res.quality_graph || [];
                currentRecommendedPct = res.recommended_pct || 20;
                currentBestFrame = res.best_frame_idx || 0; // NEW: Capture Ref Frame
                currentVideoStats = res.stats; // NEW: Store stats for report
                currentFileMetadata = res.metadata; // NEW: Store for report
                updateChartViz();

                if (res.preview_base64) {
                    // FIX PREVIEW: Use true to fitToScreen after new analysis.
                    // If user had zoomed into a previous stacked result, the analysis
                    // preview would appear distorted/clipped. fitToScreen resets the viewport.
                    await setImageAndWait(ui.imgSource, res.preview_base64, true);
                    requestAnimationFrame(() => fitToScreen());
                }

                log("SUCCESS", "Analisis completado.");
                if (ui.statusText) ui.statusText.textContent = "Listo.";
                const btnToggle = $("#btn-toggle-source");
                if (btnToggle) btnToggle.style.display = "block";
            } catch (e) {
                if (isCancellationError(e)) {
                    log("WARN", "Analisis cancelado por el usuario.");
                    if (ui.statusText) ui.statusText.textContent = "Analisis cancelado.";
                } else {
                    log("ERROR", "Analisis: " + e);
                    showCustomAlert("Error", "Error: " + e);
                }
                ui.analysisActions.style.display = "block";
            }
            finally {
                hideProcessing();
                stopStackingTimer(); // Stop timer
            }
        }, 100);
    });
}

if (ui.stackSlider) ui.stackSlider.addEventListener("input", (e) => {
    ui.pctDisplay.textContent = `${e.target.value}%`;
    // Sync chart cut line with slider position
    currentRecommendedPct = parseFloat(e.target.value);
    updateChartViz();
});

if (ui.chartModeSwitch) {
    ui.chartModeSwitch.addEventListener("change", updateChartViz);
}

// --- WORKFLOW RESILIENCE: Reset Analysis/Stacking state if critical settings change ---
function resetWorkflowForSettingsChange() {
    if (!currentFilePath) return; // Only reset if a video is already loaded

    console.log("Workflow Reset Triggered: Setting Changed.");

    // 1. Hide post-analysis panels
    if (ui.panelAnalysis) ui.panelAnalysis.style.display = "none";
    if (ui.panelWavelets) ui.panelWavelets.style.display = "none";
    if (ui.panelTools) ui.panelTools.style.display = "none";

    // 2. Show analysis actions again
    if (ui.analysisActions) ui.analysisActions.style.display = "block";

    // 3. Disable stack button
    if (ui.btnStack) {
        ui.btnStack.disabled = true;
        ui.btnStack.style.opacity = "0.5";
    }

    // 4. Clear points/grid
    activeAPoints = [];
    if (ui.apCount) ui.apCount.textContent = "0";
    if (ui.gridOverlay) {
        const ctx = ui.gridOverlay.getContext('2d');
        ctx.clearRect(0, 0, ui.gridOverlay.width, ui.gridOverlay.height);
    }

    log("INFO", "Ajuste cambiado: Se requiere analizar el video nuevamente para validar el cambio.");
}

// Attach listeners to critical selectors
[
    "#sel-analysis-mode",
    "#sel-batch-type",
    "#sel-batch-target-category",
    "#sel-color-space-override",
    "#sel-bayer-override",
    "#sel-quality-method",
    "#sel-target-category",
    "#align-mode"
].forEach(selector => {
    const el = document.querySelector(selector);
    if (el) {
        el.addEventListener("change", () => {
            // Special case: sel-quality-method might already be synced via other logic, 
            // but we ensure workflow reset here.
            if (selector === "#sel-batch-target-category") {
                const mainTarget = document.getElementById("sel-target-category");
                if (mainTarget) mainTarget.value = normalizeZenithCategory(el.value);
            }
            if (selector === "#sel-quality-method" || selector === "#sel-target-category") {
                applyZenithUltimateFlow();
            }
            if (selector === "#sel-batch-target-category") {
                applyZenithUltimateFlow();
            }
            resetWorkflowForSettingsChange();
        });
    }
});

if (ui.alignMode) {
    ui.alignMode.addEventListener("change", () => {
        const mode = ui.alignMode.value;
        const mpWrapper = $("#multipoint-wrapper");
        const warning = $("#liquid-warning");

        if (mode === "liquid_warping" || mode === "liquid_v3" || mode === "zenith_ultimate") {
            if (mpWrapper) mpWrapper.style.display = "block";
            if (warning) warning.style.display = isZenithUltimateSelected() ? "none" : "block";
        } else {
            if (mpWrapper) mpWrapper.style.display = "none";
            if (warning) warning.style.display = "none";
        }

        updateStackButtonState();
    });
    // Trigger once on load
    ui.alignMode.dispatchEvent(new Event('change'));
}

if (ui.btnSmartGrid) {
    ui.btnSmartGrid.addEventListener("click", async () => {
        if (!currentFilePath) return;
        const size = parseInt(ui.apSize.value) || 48;
        const thresholdVal = parseFloat(ui.apBright.value) / 100.0; // 0-100 -> 0.0-1.0

        ui.btnSmartGrid.disabled = true;
        ui.btnSmartGrid.innerHTML = `<svg class="zas-icon"><use href="#icon-magic"></use></svg> Generando...`;
        try {
            const flow = getActiveZenithFlow();
            const pts = await invoke("generate_smart_ap_grid", {
                path: currentFilePath,
                gridSize: size,  // FIX: Matches Rust argument `grid_size` (camelCase)
                threshold: thresholdVal,
                refFrameIdx: currentBestFrame,
                mode: flow.gridMode
            });
            activeAPoints = pts;
            drawGrid(activeAPoints, ui.imgSource.naturalWidth, ui.imgSource.naturalHeight);
            if (typeof drawManualAnchor === "function") drawManualAnchor();
            ui.apCount.textContent = activeAPoints.length;
            updateStackButtonState();
            log("SUCCESS", `Smart Grid: ${pts.length} puntos generados.`);

            if (tutorialManager && tutorialManager.currentFlowName === 'individual' && tutorialManager.currentStepIndex === 6) {
                tutorialManager.nextStep();
            }
            if (tutorialManager && tutorialManager.currentFlowName === 'batch' && tutorialManager.currentStepIndex === 7) {
                tutorialManager.nextStep();
            }
        } catch (e) {
            log("ERROR", "Smart Grid: " + e);
            showCustomAlert("Error", "Fallo Smart Grid: " + e);
        } finally {
            ui.btnSmartGrid.disabled = false;
            // FIX: Use localized string which already contains its own SVG
            ui.btnSmartGrid.innerHTML = i18n.t("analysis.generate_smart_points");
        }
    });
}

function getOperatingSystemLabel() {
    const platform = navigator.platform || "";
    const userAgent = navigator.userAgent || "";
    if (/Mac|Macintosh|Mac OS/i.test(platform) || /Mac OS|Macintosh/i.test(userAgent)) return "macOS";
    if (/Win/i.test(platform) || /Windows/i.test(userAgent)) return "Windows";
    if (/Linux/i.test(platform) || /Linux/i.test(userAgent)) return "Linux";
    return platform || tr("license.statuses.unknown", "DESCONOCIDA");
}

function getStackingEngineReportLabel(context = {}) {
    const flow = context.flow || getActiveZenithFlow();
    const alignMode = context.alignMode || flow?.alignMode || "global";
    if (context.isZenithUltimate || flow?.name === ZENITH_ULTIMATE_NAME) {
        const key = flow?.isSurface ? "zenith_ultimate_surface" : "zenith_ultimate_planetary";
        return tr(`report.engines.${key}`, flow?.name || ZENITH_ULTIMATE_NAME);
    }
    return tr(`report.engines.${alignMode}`, tr("report.engines.global", "Zenith Global v2"));
}

function getAlignmentPointsReportLabel(count, flow) {
    const pointCount = Math.max(0, Number(count) || 0);
    if (pointCount > 0) return trFormat("report.ap_count", { count: pointCount }, `${pointCount} puntos`);
    if (flow?.needsPoints) return tr("report.global_fallback", "Global automático");
    return tr("report.global_alignment", "Alineación global");
}

function showStackingReport(duration, stackContext = {}) {
    if (!currentFileMetadata) return;

    // Calculations
    const requestedPct = parseFloat(ui.stackSlider?.value || 20);
    const safePct = Number.isFinite(requestedPct) ? requestedPct : 20;
    const apCount = stackContext.pointsCount ?? ((typeof activeAPoints !== 'undefined') ? activeAPoints.length : 0);
    const safeDuration = Math.max(0.001, Number(duration) || 0.001);

    // Additional Metrics
    const totalFrames = Number(currentFileMetadata.frame_count || parseInt(ui.iFrames?.textContent) || 0);
    const stackedCount = totalFrames > 0 ? Math.max(1, Math.min(totalFrames, Math.round(totalFrames * (safePct / 100)))) : 0;
    const stackPct = safePct;

    const timeStr = safeDuration.toFixed(1) + "s";
    const fps = (stackedCount / safeDuration).toFixed(1);
    const fileSizeMB = currentFileMetadata.file_size_mb || 0;
    const mbps = (fileSizeMB / safeDuration).toFixed(2);

    const totalPixels = (((currentFileMetadata.width || 0) * (currentFileMetadata.height || 0) * stackedCount) / 1000000).toFixed(1);

    // NEW KPIs
    const snrGain = stackedCount > 0 ? (10 * Math.log10(stackedCount)).toFixed(1) : "0.0";
    // BUG FIX: Backend seems to return 0-100 already for quality stats
    const rawQual = currentVideoStats?.avg_quality ?? currentVideoStats?.stats?.avg_quality ?? 0;
    const avgQuality = (rawQual > 1) ? rawQual.toFixed(1) : (rawQual * 100).toFixed(1);

    const threads = navigator.hardwareConcurrency || 8;
    const osLabel = getOperatingSystemLabel();
    const engineLabel = getStackingEngineReportLabel(stackContext);
    const apLabel = getAlignmentPointsReportLabel(apCount, stackContext.flow);

    let reportHtml = `
                                    <div class="technical-report" style="text-align:left; font-family:'JetBrains Mono', 'Consolas', monospace; font-size:0.85rem; color:#94a3b8; line-height:1.2; margin:0; padding:0;">
                                        <!-- Header Group -->
                                        <div style="padding: 10px 14px; background: rgba(56, 189, 248, 0.04); border-radius: 10px; border: 1px solid rgba(56, 189, 248, 0.15); display:flex; justify-content:space-between; align-items:center; margin-bottom: 10px;">
                                            <div style="flex: 1;">
                                                <div style="color:#64748b; font-size:0.6rem; text-transform:uppercase; letter-spacing:1px; margin-bottom:2px;">${i18n.t('report.execution_time')}</div>
                                                <div style="color:#38bdf8; font-weight:bold; font-size:1.15rem;">${timeStr}</div>
                                            </div>
                                            <div style="flex: 1; text-align:right;">
                                                <div style="color:#64748b; font-size:0.6rem; text-transform:uppercase; letter-spacing:1px; margin-bottom:2px;">${i18n.t('report.snr_gain')}</div>
                                                <div style="color:#10b981; font-weight:bold; font-size:1.15rem;">+${snrGain} dB</div>
                                            </div>
                                        </div>

                                        <!-- Stats Table -->
                                        <table style="width:100%; border-collapse: collapse; font-size:0.8rem; margin: 0 auto;">
                                            <tr style="border-bottom: 1px solid rgba(255,255,255,0.03);">
                                                <td style="padding:6px 0; color:#64748b;"><span class="report-led led-blue"></span>${i18n.t('report.stacked_frames')}</td>
                                                <td style="padding:6px 0; text-align:right; color:#cbd5e1; font-weight:500;">${stackedCount} / ${totalFrames} <span style="font-size:0.7rem; opacity:0.6;">(${stackPct}%)</span></td>
                                            </tr>
                                            <tr style="border-bottom: 1px solid rgba(255,255,255,0.03);">
                                                <td style="padding:6px 0; color:#64748b;"><span class="report-led led-green"></span>${i18n.t('report.avg_quality')}</td>
                                                <td style="padding:6px 0; text-align:right; color:#cbd5e1; font-weight:500;">${avgQuality}%</td>
                                            </tr>
                                            <tr style="border-bottom: 1px solid rgba(255,255,255,0.03);">
                                                <td style="padding:6px 0; color:#64748b;"><span class="report-led led-blue"></span>${i18n.t('report.core_engine')}</td>
                                                <td style="padding:6px 0; text-align:right; color:#cbd5e1;">${escapeHtml(engineLabel)}</td>
                                            </tr>
                                            <tr style="border-bottom: 1px solid rgba(255,255,255,0.03);">
                                                <td style="padding:6px 0; color:#64748b;"><span class="report-led led-green"></span>${i18n.t('report.alignment_points')}</td>
                                                <td style="padding:6px 0; text-align:right; color:#cbd5e1;">${escapeHtml(apLabel)}</td>
                                            </tr>
                                            <tr style="border-bottom: 1px solid rgba(255,255,255,0.03);">
                                                <td style="padding:6px 0; color:#64748b;"><span class="report-led led-green"></span>${i18n.t('report.cpu_threads')}</td>
                                                <td style="padding:6px 0; text-align:right; color:#cbd5e1;">${trFormat('report.threads_active', { count: threads }, `${threads} Threads Active`)}</td>
                                            </tr>
                                            <tr style="border-bottom: 1px solid rgba(255,255,255,0.03);">
                                                <td style="padding:6px 0; color:#64748b;"><span class="report-led led-blue"></span>${i18n.t('report.precision')}</td>
                                                <td style="padding:6px 0; text-align:right; color:#cbd5e1;">${i18n.t('report.precision_subpixel')}</td>
                                            </tr>
                                            <tr style="border-bottom: 1px solid rgba(255,255,255,0.03);">
                                                <td style="padding:6px 0; color:#64748b;"><span class="report-led led-blue"></span>${i18n.t('report.ref_frame')}</td>
                                                <td style="padding:6px 0; text-align:right; color:#cbd5e1;">#${currentBestFrame}</td>
                                            </tr>
                                            <tr style="border-bottom: 1px solid rgba(255,255,255,0.03);">
                                                <td style="padding:6px 0; color:#64748b;"><span class="report-led led-green"></span>${i18n.t('report.throughput')}</td>
                                                <td style="padding:6px 0; text-align:right; color:#10b981; font-weight:bold;">${fps} FPS</td>
                                            </tr>
                                            <tr style="border-bottom: 1px solid rgba(255,255,255,0.03);">
                                                <td style="padding:6px 0; color:#64748b;"><span class="report-led led-green"></span>${i18n.t('report.bandwidth')}</td>
                                                <td style="padding:6px 0; text-align:right; color:#10b981; font-weight:bold;">${mbps} MB/s</td>
                                            </tr>
                                        </table>

                                        <!-- Bottom Info -->
                                        <div style="margin-top:10px; font-size:0.65rem; color:#475569; border-top:1px solid rgba(255,255,255,0.05); padding-top:6px; display:flex; justify-content:space-between; font-style:italic; opacity:0.8;">
                                            <span>${i18n.t('report.operating_system')}: ${escapeHtml(osLabel)} · ID: ${Math.random().toString(16).slice(2, 10).toUpperCase()}</span>
                                            <span>${i18n.t('report.system_integrity')}: ${trFormat('report.mp_processed', { mp: totalPixels }, `${totalPixels} MP PROCESSED`)}</span>
                                        </div>
                                    </div>
    `.replace(/>\s+</g, '><').trim(); // Strip intra-tag whitespace/newlines

    showCustomAlert(i18n.t("report.title"), reportHtml);
}

if (ui.chkSharpened) {
    ui.chkSharpened.addEventListener("change", () => {
        if (ui.sharpenIntensityContainer) {
            ui.sharpenIntensityContainer.style.display = ui.chkSharpened.checked ? "block" : "none";
        }
    });
}

async function ensureZenithUltimateAlignmentPoints(flow) {
    if (!flow.needsPoints || (activeAPoints && activeAPoints.length > 0) || !currentFilePath) {
        return;
    }

    // FIX VALIDATION: Ensure analysis was performed first.
    // Without a valid best frame reference, the Smart Grid cannot be positioned correctly.
    const hasValidAnalysis = (currentBestFrame !== null && currentBestFrame !== undefined);
    if (!hasValidAnalysis) {
        throw new Error(i18n ? i18n.t('errors.analyze_first') : "Por favor analiza el video antes de apilar. Haz clic en el botón Analizar primero.");
    }

    const size = parseInt(ui.apSize?.value || flow.apSize || 48);
    const threshold = parseFloat(ui.apBright?.value || Math.round(flow.apThreshold * 100)) / 100.0;
    log("INFO", `${flow.name}: generando Smart AP automatico para ${flow.category}...`);
    const pts = await invoke("generate_smart_ap_grid", {
        path: currentFilePath,
        gridSize: size,
        threshold,
        refFrameIdx: currentBestFrame,
        mode: flow.gridMode
    });
    if (!pts || pts.length === 0) {
        log("WARN", `${flow.name}: Smart Grid generó 0 puntos. Verifica el umbral de brillo o el tipo de objetivo.`);
        // For planets, we can proceed with global CoG alignment even without AP points
        if (!flow.isSurface) {
            log("INFO", "Modo planetario: continuando con alineamiento CoG global (sin puntos AP).");
            return;
        }
        throw new Error("No se generaron puntos AP. Ajusta el umbral de brillo o usa el Smart Grid manualmente.");
    }
    activeAPoints = pts;
    drawGrid(activeAPoints, ui.imgSource.naturalWidth, ui.imgSource.naturalHeight);
    if (ui.apCount) ui.apCount.textContent = activeAPoints.length;
    log("SUCCESS", `${flow.name}: ${pts.length} puntos AP listos.`);
}

if (ui.btnStack) {
    ui.btnStack.addEventListener("click", async () => {
        if (!currentFilePath) return;
        const drizzleFactor = parseFloat(ui.drizzleScale.value) || 1.0;
        const sharpenIntensity = parseFloat(ui.selSharpenIntensity?.value || "0.5");

        // CHECK FRONTEND DE LICENCIA (Permitir en TRIAL)
        if (drizzleFactor > 1.0 && !isProVersion) {
            showCustomChoice("Funcion PRO", "El Drizzle > 1x requiere licencia anual o periodo de prueba activo.\n\n¿Deseas activar tu licencia ahora?", "Si, Activar", "Usar Drizzle 1x (Gratis)")
                .then(yes => {
                    if (yes) showLicenseModal();
                    else {
                        ui.drizzleScale.value = "1.0"; // Reset a 1x
                        ui.btnStack.click(); // Reintentar con 1x
                    }
                });
            return;
        }

        if (drizzleFactor >= 4.0) {
            const confirmed = await showCustomChoice("Aviso Drizzle", `Drizzle x${drizzleFactor} consume mucha memoria. ¿Continuar?`, "Si", "No");
            if (!confirmed) return;
        }

        activeDrizzleFactor = drizzleFactor;

        ui.btnStack.disabled = true;
        if (ui.pBarContainer) { ui.pBarContainer.style.display = "block"; ui.pBarFill.style.width = "0%"; }
        showProcessing("APILANDO FRAMES...");

        // INICIO: Timer y Tips
        startStackingTimer();
        startTipsCarousel();
        const startTime = Date.now(); // Capture Start Time for Report
        const shouldPauseStackTutorial =
            (tutorialManager?.currentFlowName === 'individual' && tutorialManager.currentStepIndex === 8)
            || (tutorialManager?.currentFlowName === 'batch' && tutorialManager.currentStepIndex === 9);
        if (shouldPauseStackTutorial) {
            tutorialManager.hideOverlay();
        }

        setTimeout(async () => {
            try {
                // CLEANUP: Descartar apilado anterior de la RAM para evitar fragmentación al re-apilar
                await invoke("clear_app_memory").catch(() => {});

                applyZenithUltimateFlow();
                const flow = getActiveZenithFlow();
                await ensureZenithUltimateAlignmentPoints(flow);
                const alignModeStr = flow.alignMode || ui.alignMode.value;
                let pointsToSend = [];
                if (flow.needsPoints && activeAPoints.length > 0) {
                    pointsToSend = activeAPoints.map(p => {
                        // Handle legacy [x, y] or new {x, y, size}
                        if (Array.isArray(p)) {
                            return { x: p[0], y: p[1], size: parseInt(ui.apSize.value) || flow.apSize || 48 };
                        } else {
                            // Already an object, ensure size is present
                            return {
                                x: p.x,
                                y: p.y,
                                size: p.size || parseInt(ui.apSize.value) || flow.apSize || 48
                            };
                        }
                    });
                }

                // Determinar si es superficie para enviar flag al backend
                const isSurfaceMode = flow.isSurface;
                const warpingAnalysis = flow.warpingAnalysis;

                let b64 = "";

                // CHECK: ZENITH ULTIMATE / LIQUID WARPING DISPATCH
                // FIX DISPATCH: zenith_v3 (now used for BOTH surface and planet in Zenith Ultimate)
                // must route through stack_video_liquid_warping, not stack_video.
                // Previously "zenith_v3" fell into the else-global branch without AP points.
                if (!isZenithUltimateSelected() && alignModeStr === "elite_v4") {
                    log("INFO", "Iniciando Zenith Elite V4 (Neural-Restoration)...");
                    const eliteConfig = {
                        psf_radius: parseInt(document.getElementById("elite-psf-radius").value) || 15,
                        fwhm_pixels: parseFloat(document.getElementById("elite-fwhm").value) || 2.5,
                        airy_weight: parseFloat(document.getElementById("elite-airy").value) || 0.3,
                        auto_psf_from_limb: document.getElementById("elite-auto-psf").checked,
                        deconv_iterations: parseInt(document.getElementById("elite-iter").value) || 15,
                        tv_lambda: parseFloat(document.getElementById("elite-lambda").value) || 0.01,
                        snr_floor: parseFloat(document.getElementById("elite-snr").value) || 5.0,
                        rejection_threshold: 0.1, 
                        post_sharpen: parseFloat(document.getElementById("elite-sharpen").value) || 0.5,
                        feather_px: 20
                    };
                    b64 = await invoke("zas_stack_video_elite", {
                        path: currentFilePath,
                        percent: parseFloat(ui.stackSlider.value),
                        config: eliteConfig,
                        category: flow.category
                    });
                } else if (isZenithUltimateSelected() || alignModeStr === "liquid_warping" || alignModeStr === "liquid_v3" || alignModeStr === "zenith_v3") {
                    // Zenith Ultimate (both categories) + legacy liquid_warping modes
                    log("INFO", `Iniciando ${isZenithUltimateSelected() ? flow.name : (alignModeStr === "liquid_v3" ? "Zenith Precision V3 (Multipoint)" : "Liquid Warping V2")}...`);
                    b64 = await invoke("stack_video_liquid_warping", {
                        path: currentFilePath,
                        percent: parseFloat(ui.stackSlider.value),
                        customPoints: pointsToSend,
                        drizzle: drizzleFactor,
                        isSurface: isSurfaceMode,
                        bayerOverride: getBayerOverrideValue(),
                        apSize: parseInt(ui.apSize.value) || flow.apSize || 48,
                        sharpened: document.getElementById("chk-sharpened").checked,
                        sharpenIntensity: sharpenIntensity,
                        doublePass: document.getElementById("chk-double-pass").checked,
                        warpingAnalysis, // NEW
                        anchorOverride: getManualAnchorOverrideValue(),
                        stackingRoi: getStackingRoiOverrideValue(),
                        normalizeColors: document.getElementById("chk-normalize-colors") ? document.getElementById("chk-normalize-colors").checked : true,
                        isV3: flow.isV3 || (alignModeStr === "liquid_v3") || (alignModeStr === "zenith_v3"), // V3 flag for all these modes
                        keepFullFrame: document.getElementById("chk-keep-full-frame") ? document.getElementById("chk-keep-full-frame").checked : false,
                        targetType: flow.category
                    });
                } else {
                    // MODO GLOBAL / STANDARD
                    b64 = await invoke("stack_video", {
                        path: currentFilePath,
                        percent: parseFloat(ui.stackSlider.value),
                        mode: alignModeStr,
                        customPoints: pointsToSend,
                        drizzle: drizzleFactor,
                        isSurface: isSurfaceMode,
                        bayerOverride: getBayerOverrideValue(),
                        apSize: parseInt(ui.apSize.value) || flow.apSize || 48,
                        sharpened: document.getElementById("chk-sharpened").checked,
                        sharpenIntensity: sharpenIntensity,
                        doublePass: document.getElementById("chk-double-pass").checked,
                        warpingAnalysis, // NEW
                        anchorOverride: getManualAnchorOverrideValue(),
                        stackingRoi: getStackingRoiOverrideValue(),
                        normalizeColors: document.getElementById("chk-normalize-colors") ? document.getElementById("chk-normalize-colors").checked : true,
                        isV3: flow.isV3 || (alignModeStr === "zenith_v3"), // NEW FLAG
                        keepFullFrame: document.getElementById("chk-keep-full-frame") ? document.getElementById("chk-keep-full-frame").checked : false,
                        targetType: flow.category
                    });
                }

                // Auto-close Stacking ROI to restore canvas panning
                if (typeof isSettingStackingRoi !== 'undefined' && isSettingStackingRoi) {
                    isSettingStackingRoi = false;
                    const btnRoi = document.getElementById("btn-stacking-roi");
                    if (btnRoi) {
                        btnRoi.classList.add("secondary");
                        btnRoi.classList.remove("primary");
                        btnRoi.style.background = "rgba(16, 185, 129, 0.15)";
                        btnRoi.style.color = "#6ee7b7";
                        btnRoi.textContent = "📐 Definir Área de Apilado";
                    }
                    const roiBox = document.getElementById("stacking-roi-box");
                    if (roiBox) roiBox.style.display = "none";
                }

                if (ui.viewResult) ui.viewResult.style.display = "flex";
                if (ui.imgResult) {
                    await setImageAndWait(ui.imgResult, b64, false);
                    fitToScreen(); // Recenter the viewport symmetrically to feature both images
                }

                // PERSISTENCE: Mantener paneles de análisis e info visibles para permitir re-apilado rápido
                if (ui.panelAnalysis) ui.panelAnalysis.style.display = "block";
                if (ui.panelInfo) ui.panelInfo.style.display = "block";

                ui.gridOverlay.getContext('2d').clearRect(0, 0, ui.gridOverlay.width, ui.gridOverlay.height);
                if (ui.panelWavelets) {
                    ui.panelWavelets.style.display = "block";
                    // HIDE Mosaic Buttons in Stacking Flow
                    const btnGen = document.getElementById("btn-gen-mosaic");
                    const btnArr = document.getElementById("btn-arrange-mosaic");
                    const chkGuideWrap = document.getElementById("chk-mosaic-guided-wrapper");

                    if (btnGen) btnGen.style.display = "none";
                    if (btnArr) btnArr.style.display = "none";
                    if (chkGuideWrap) chkGuideWrap.style.display = "none";

                    // HIDE Deringing UI in Stacking Flow (if we ever reuse this for mosaic, but mostly for safety)
                    if (ui.selDrMode) ui.selDrMode.parentElement.style.display = "none";
                } else {
                    // SHOW Deringing UI in Standard Stacking Flow
                    if (ui.selDrMode) ui.selDrMode.parentElement.style.display = "block";
                }

                if (!isBatchMode && ui.panelTools) {
                    ui.panelTools.style.display = "block";
                    // Show re-analyze warning panel below the tools panel
                    const pReanalyze = document.getElementById("panel-reanalyze");
                    if (pReanalyze) pReanalyze.style.display = "block";
                }

                // Batch Mode: Refined Flow reveal
                if (isBatchMode) {
                    if (ui.panelBatch) {
                        ui.panelBatch.style.display = "block";
                        // Moving batch panel to the bottom of the sidebar
                        const sidebar = document.querySelector(".sidebar");
                        if (sidebar) sidebar.appendChild(ui.panelBatch);
                    }

                    // Mark steps 1 to 4 as completed
                    const steps = ui.panelBatch.querySelectorAll(".batch-step");
                    for (let i = 0; i < 4 && i < steps.length; i++) {
                        steps[i].classList.add("completed");
                    }

                    // Adjust button visibilities
                    const btnToggleStacking = document.getElementById("btn-batch-toggle-stacking");
                    if (btnToggleStacking) btnToggleStacking.style.display = "none";
                    if (ui.btnBatchRun) ui.btnBatchRun.disabled = false;
                    if (ui.batchStepExecution) ui.batchStepExecution.style.display = "block";

                    // Show batch re-analyze warning panel
                    const pBatchReanalyze = document.getElementById("panel-batch-reanalyze");
                    if (pBatchReanalyze) pBatchReanalyze.style.display = "block";
                }

                resetProcessingParams();

                hideProcessing();
                const totalTime = (Date.now() - startTime) / 1000;

                setTimeout(() => {
                    triggerStackSuccessEffect();
                    // SHOW REPORT
                    showStackingReport(totalTime, {
                        flow,
                        alignMode: alignModeStr,
                        pointsCount: pointsToSend.length,
                        isZenithUltimate: isZenithUltimateSelected()
                    });
                    const shouldAdvanceStackTutorial =
                        (tutorialManager?.currentFlowName === 'batch' && tutorialManager.currentStepIndex === 9)
                        || (tutorialManager?.currentFlowName === 'individual' && tutorialManager.currentStepIndex === 8);
                    if (shouldPauseStackTutorial) {
                        tutorialManager.showOverlay();
                    }
                    if (shouldAdvanceStackTutorial) {
                        tutorialManager.nextStep();
                    }
                }, 400);

                log("SUCCESS", "Apilado exitoso.");
            } catch (e) {
                if (isCancellationError(e)) {
                    log("WARN", "Apilado cancelado por el usuario.");
                    if (ui.statusText) ui.statusText.textContent = "Apilado cancelado.";
                } else {
                    log("ERROR", e);
                    showCustomAlert("Error", "Error en apilado: " + e);
                }
                if (shouldPauseStackTutorial) {
                    tutorialManager.showOverlay();
                }
                hideProcessing();
            }
            finally {
                stopStackingTimer();
                stopTipsCarousel();
                ui.btnStack.disabled = false;
            }
        }, 100);
    });
}

// =========================================================================
// RE-ANALYZE BUTTONS (Normal Mode + Batch Mode)
// Clears cached analysis file and restarts analysis from scratch
// =========================================================================
function triggerFreshReanalyze() {
    // 1. Hide the re-analyze panels
    const pReanalyze = document.getElementById("panel-reanalyze");
    const pBatchReanalyze = document.getElementById("panel-batch-reanalyze");
    if (pReanalyze) pReanalyze.style.display = "none";
    if (pBatchReanalyze) pBatchReanalyze.style.display = "none";

    // CLEANUP Backend memory to ensure fresh start (PSFs, caches, etc)
    invoke("clear_app_memory").catch(() => {});

    // 2. Reset manual anchor and stacking ROI (the main causes of blurry results)
    // Use the shared helpers if available (set by setupAnchorRoiToggles)
    if (typeof window._clearManualAnchor === "function") window._clearManualAnchor();
    else {
        manualAnchorPoint = null;
        if (typeof isSettingManualAnchor !== "undefined") isSettingManualAnchor = false;
    }
    if (typeof window._clearStackingRoi === "function") window._clearStackingRoi();
    else {
        if (typeof stackingRoiSelection !== "undefined") stackingRoiSelection = null;
        if (typeof isSettingStackingRoi !== "undefined") isSettingStackingRoi = false;
        const roiBox = document.getElementById("stacking-roi-box");
        if (roiBox) roiBox.style.display = "none";
    }

    // 3. Hide post-stack panels, show analysis UI again
    if (ui.panelTools) ui.panelTools.style.display = "none";
    if (ui.panelWavelets) ui.panelWavelets.style.display = "none";
    if (ui.viewResult) ui.viewResult.style.display = "none";
    if (ui.imgResult) { ui.imgResult.src = ""; ui.imgResult.classList.remove("loaded"); }
    if (ui.analysisActions) ui.analysisActions.style.display = "block";

    // 4. Trigger a fresh analysis (this will ignore/overwrite the cached JSON)
    // We re-activate the Run Analysis button without deleting the file —
    // the backend will overwrite it when it runs again.
    // If the user wants truly no cache, hide analysis panel and re-launch:
    if (ui.panelAnalysis) ui.panelAnalysis.style.display = "none";
    if (ui.btnStack) { ui.btnStack.disabled = true; ui.btnStack.style.opacity = "0.5"; }

    log("INFO", tr("analysis.reanalyze.log", "Reanálisis solicitado. Anclaje y área liberados. Ejecuta el análisis nuevamente."));

    // Scroll to analysis actions
    if (ui.analysisActions) ui.analysisActions.scrollIntoView({ behavior: "smooth", block: "center" });
    if (ui.btnRunAnalysis) {
        // Flash the run-analysis button to guide the user
        ui.btnRunAnalysis.style.boxShadow = "0 0 18px #7c3aed, 0 0 36px #db2777";
        setTimeout(() => { if (ui.btnRunAnalysis) ui.btnRunAnalysis.style.boxShadow = ""; }, 2500);
    }
}

const btnReanalyze = document.getElementById("btn-reanalyze");
if (btnReanalyze) btnReanalyze.addEventListener("click", triggerFreshReanalyze);

const btnBatchReanalyze = document.getElementById("btn-batch-reanalyze");
if (btnBatchReanalyze) btnBatchReanalyze.addEventListener("click", triggerFreshReanalyze);


async function fn_save(format_idx) {
    if (!currentFilePath) return;

    // CHECK FRONTEND TIFF
    if (format_idx === 1 && !isProVersion) {
        showCustomAlert(tr("export.pro_title", "Función PRO"), tr("export.tiff_requires_pro", "El guardado en TIFF 16-bit requiere licencia anual o periodo de prueba activo.\n\nPor favor activa tu licencia."));
        return;
    }

    const p = getPipelineParams();
    log("INFO", tr("general.saving", "GUARDANDO...")); showProcessing(tr("general.saving", "GUARDANDO..."));
    setTimeout(async () => {
        try {
            const msg = await invoke("save_final_image", {
                path: currentFilePath, formatIdx: format_idx,
                u1: p.u[0], u2: p.u[1], u3: p.u[2], u4: p.u[3], u5: p.u[4],
                w1: p.w[0], w2: p.w[1], w3: p.w[2], w4: p.w[3], w5: p.w[4], w6: p.w[5],
                d1: p.d[0], d2: p.d[1], d3: p.d[2], d4: p.d[3], d5: p.d[4], d6: p.d[5],
                gamma: p.color.g, saturation: p.color.s,
                contrast: p.color.c, brightness: p.color.b,
                rBal: p.color.rb, bBal: p.color.bb,
                rX: p.shift.rx, rY: p.shift.ry, bX: p.shift.bx, bY: p.shift.by,
                blend: p.blend / 100.0,
                deringingMode: p.dr.mode,
                deringingRadius: p.dr.rad,
                deringingDark: p.dr.dark,
                deringingLight: p.dr.light,
                deringingMask: p.dr.mask,

                crisp: p.crisp,
                deconvIter: p.deconv.i, deconvSigma: p.deconv.s,
                vcIter: p.deconv.vi, vcSigma: p.deconv.vs,
                usmAmount: p.usm.a, usmRadius: p.usm.r, lceAmount: p.lce,
                masterDenoise: p.masterDenoise,
                masterDenoiseDetail: p.denoiseDetail,
                masterDenoiseChroma: p.denoiseChroma,
                useRgbSharpening: p.useRgbSharpening
            });
            log("SUCCESS", normalizeBackendText(msg)); showCustomAlert(tr("general.saved", "Guardado"), normalizeBackendText(msg));
        } catch (e) { log("ERROR", "Save: " + e); showCustomAlert("Error", "Error guardando: " + e); }
        finally { hideProcessing(); }
    }, 100);
}

if (ui.btnSavePng) ui.btnSavePng.addEventListener("click", () => fn_save(0));
if (ui.btnSaveTiff) ui.btnSaveTiff.addEventListener("click", () => fn_save(1));

if (ui.btnToggleLog) ui.btnToggleLog.addEventListener("click", () => ui.consolePanel.classList.toggle("open"));

if (ui.btnAnimExport) {
    ui.btnAnimExport.addEventListener("click", async () => {
        if (!batchResultPaths || batchResultPaths.length === 0) {
            showCustomAlert(tr("general.error", "Error"), tr("animation.errors.no_frames_to_export", "No hay fotogramas para exportar."));
            return;
        }

        const format = ui.selAnimFormat.value || "mp4";
        const rescaleFactor = parseFloat(ui.selAnimRescale?.value || "1.0");
        const exportQuality = ui.selAnimExportQuality?.value || "balanced";
        const delay = syncAnimationSpeedUI();
        restoreAnimationExportButtonState();
        const oldButtonHtml = ui.btnAnimExport.innerHTML;

        try {
            // Si es MP4, verificar FFmpeg
            if (format === "mp4") {
                try {
                    const status = await invoke("check_ffmpeg_status");
                    if (!status) {
                        showFFmpegModal();
                        return;
                    }
                } catch (e) { console.error("FFmpeg check failed", e); }
            }

            // Lock UI but KEEP PLAYING
            ui.btnAnimExport.disabled = true;
            if (ui.btnAnimCancel) ui.btnAnimCancel.disabled = true;

            setAnimationPlaying(true);

            ui.btnAnimExport.innerHTML = trFormat(
                "animation.export.exporting",
                { format: format.toUpperCase() },
                `Exportando ${format.toUpperCase()}...`
            );

            showProcessing(trFormat("animation.export.processing", { format: format.toUpperCase() }, `EXPORTANDO ${format.toUpperCase()}...`));

            // 1. Filtrar Frames Activos (Respetar "Discarded Frames")
            let activeFiles = [];
            if (batchResultPaths && batchResultPaths.length > 0) {
                activeFiles = batchResultPaths.filter((_, i) => !animExcludedIndices.has(i));
            }

            if (activeFiles.length === 0) {
                showCustomAlert(tr("general.error", "Error"), tr("animation.errors.no_active_frames", "No hay frames activos para exportar."));
                return;
            }

            // Ensure we have a valid output folder
            let finalOutputFolder = batchOutputFolder;
            if (!finalOutputFolder || finalOutputFolder === "") {
                // FALLBACK: Use the directory of the first file
                if (batchResultPaths && batchResultPaths.length > 0) {
                    const firstFile = batchResultPaths[0];
                    const sep = firstFile.includes("\\") ? "\\" : "/";
                    finalOutputFolder = firstFile.substring(0, firstFile.lastIndexOf(sep));
                } else {
                    showCustomAlert(tr("general.error", "Error"), tr("animation.errors.missing_output_folder", "No se pudo determinar la carpeta de salida."));
                    return;
                }
            }

            // Use export_animation_video for all formats (it wraps create_gif_animation logic)
            const resultPath = await invoke("export_animation_video", {
                folder: finalOutputFolder,
                files: activeFiles,        // pass the filtered list
                delayMs: delay,
                boomerang: ui.checkAnimBoomerang.checked,
                format: format,
                rotation: animFilters.rotation,
                brightness: animFilters.brightness,
                contrast: animFilters.contrast,
                saturation: animFilters.saturation,

                // Nuevos Filtros
                gamma: animFilters.gamma || 1.0,
                levelsBlack: animFilters.levelsBlack || 0.0,
                levelsWhite: animFilters.levelsWhite || 1.0,
                hueShift: animFilters.hue || 0.0,
                colorFilter: animFilters.colorFilter || "none",
                colorStrength: animFilters.colorStrength ?? 1.0,
                highlightProtect: animFilters.highlightProtect ?? 0.35,

                overlayMode: animOverlays.mode,
                watermarkText: animOverlays.wmText || "",
                watermarkOpacity: animOverlays.wmOpacity || 0.5,
                frameLineTop: animOverlays.frTitle || "",
                frameLineBottom: `${animOverlays.frTele || ""} | ${animOverlays.frCam || ""} | ${animOverlays.frOther || ""}`,
                fontName: animOverlays.font || "Arial",

                // New Tint Params
                manualTintR: (animFilters.tint?.r ?? 255) / 255.0,
                manualTintG: (animFilters.tint?.g ?? 255) / 255.0,
                manualTintB: (animFilters.tint?.b ?? 255) / 255.0,

                // Rescale Param
                rescaleFactor: rescaleFactor,
                qualityPreset: exportQuality,
                cropX: animCropExportRect?.x ?? -1,
                cropY: animCropExportRect?.y ?? -1,
                cropW: animCropExportRect?.w ?? 0,
                cropH: animCropExportRect?.h ?? 0,
            });

            const openFolder = await showCustomChoice(
                tr("animation.export.saved_title", "Exportación completada"),
                trFormat("animation.export.saved_message", { path: resultPath }, `Archivo guardado en:\n${resultPath}\n\n¿Abrir carpeta?`),
                tr("animation.export.open_folder", "Sí, abrir"),
                tr("animation.close_editor", "Cerrar")
            );
            if (openFolder && window.__TAURI__) {
                // invoke shell open if possible, or just ignore
            }

            // ui.btnAnimCancel.click(); // Keep open as requested
        } catch (e) {
            log("ERROR", "Export: " + e);
            showCustomAlert(tr("general.error", "Error"), trFormat("animation.errors.export_failed", { error: e }, "Fallo al exportar: " + e));
        } finally {
            hideProcessing();
            restoreAnimationExportButtonState(oldButtonHtml);
        }
    });
}

// -------------------------------------------------------------------------
// FUNCIONES AUXILIARES DE LOG Y GRAFICAS
// -------------------------------------------------------------------------

function log(level, msg) {
    if (!ui.logContainer) return;
    msg = translateBackendProgressText(msg);

    // Cap log entries to prevent DOM-induced slowdowns (max 250)
    if (ui.logContainer.children.length > 250) {
        // Remove oldest 50 entries when limit exceeded to avoid frequent reflows
        for (let i = 0; i < 50; i++) {
            if (ui.logContainer.firstChild) ui.logContainer.removeChild(ui.logContainer.firstChild);
        }
    }

    const d = document.createElement("div");
    d.className = "log-entry"; if (level === "ERROR") d.className += " log-err";
    d.textContent = `[${new Date().toLocaleTimeString()}] [${level}] ${msg}`;
    
    ui.logContainer.appendChild(d);
    ui.logContainer.scrollTop = ui.logContainer.scrollHeight;
}
window.log = log;

function updateChartViz() {
    if (!currentGraphData || currentGraphData.length === 0) return;

    // Verificar estado del switch: Checked = Calidad (Sorted), Unchecked = Tiempo (Time)
    const isSorted = ui.chartModeSwitch ? ui.chartModeSwitch.checked : true; // Default to true (sorted)

    let dataToShow = [];

    if (isSorted) {
        // Clonar y ordenar descendente por score (indice 1)
        dataToShow = [...currentGraphData].sort((a, b) => b[1] - a[1]);
    } else {
        // Orden original (cronologico) - pero aseguramos que sea array
        dataToShow = currentGraphData;
    }

    drawChart(dataToShow, currentRecommendedPct, isSorted);
}

function drawChart(data, cutVal, isSorted) {
    const canvas = document.getElementById('qualityChart');
    if (!canvas) return;
    const ctx = canvas.getContext("2d");

    if (chartInstance) chartInstance.destroy();

    // Crear etiquetas (indices)
    const labels = data.map((_, i) => i + 1);

    // Gradient Fill
    let gradient = ctx.createLinearGradient(0, 0, 0, 400);
    gradient.addColorStop(0, 'rgba(56, 189, 248, 0.5)'); // Sky blue top
    gradient.addColorStop(1, 'rgba(56, 189, 248, 0.0)'); // Transparent bottom

    // Configuracion de Anotaciones
    let annotations = {};

    if (isSorted) {
        // En modo SORTER (Calidad), mostramos linea vertical de corte
        const cutIndex = Math.floor(data.length * (cutVal / 100));
        annotations = {
            line1: {
                type: 'line',
                xMin: cutIndex,
                xMax: cutIndex,
                borderColor: '#10b981', // Emerald for cut line
                borderWidth: 2,
                borderDash: [5, 5],
                label: {
                    content: `Stack ${cutVal.toFixed(0)}%`,
                    display: true,
                    position: 'start',
                    backgroundColor: 'rgba(6, 78, 59, 0.8)', // Darker emerald background
                    color: '#34d399', // Light emerald text
                    font: { size: 11, weight: 'bold' },
                    yAdjust: 10,
                    xAdjust: isSorted ? -10 : 0
                }
            }
        };
    }

    chartInstance = new Chart(ctx, {
        type: 'line',
        data: {
            labels: labels,
            datasets: [{
                label: isSorted ? 'Calidad (Ordenada)' : 'Calidad (Tiempo)',
                // Transformar pares [frame_idx, score] a formato compatible {x, y, frame}
                data: data.map((item, i) => ({
                    x: i + 1,
                    y: item[1],
                    frame: item[0] // Guardar frame original para tooltip
                })),
                borderColor: '#38bdf8', // Sky 400
                borderWidth: 2,
                pointRadius: 0,
                pointHoverRadius: 4,
                fill: true,
                backgroundColor: gradient,
                tension: 0.3 // Suavizado para mejor look
            }]
        },
        options: {
            responsive: true,
            maintainAspectRatio: false,
            interaction: {
                mode: 'index',
                intersect: false,
            },
            plugins: {
                legend: { display: false },
                annotation: {
                    annotations: annotations
                },
                tooltip: {
                    backgroundColor: 'rgba(15, 23, 42, 0.9)',
                    titleColor: '#e2e8f0',
                    bodyColor: '#38bdf8',
                    borderColor: 'rgba(51, 65, 85, 0.5)',
                    borderWidth: 1,
                    displayColors: false,
                    callbacks: {
                        label: function (context) {
                            // context.raw es el objeto {x, y} o el valor directo si fuera simple
                            // Pero aqui pasamos [{x: i, y: val, frame: f}, ...]
                            return `Calidad: ${context.parsed.y.toFixed(1)}%`;
                        },
                        title: function (context) {
                            const raw = context[0].raw; // {x:..., y:..., frame:...}
                            const frameStr = raw && raw.frame !== undefined ? ` | Frame: #${raw.frame}` : "";
                            const rankStr = isSorted ? `Rank #${context[0].label}` : `Frame #${context[0].label}`;
                            return `${rankStr}${frameStr}`;
                        }
                    }
                },
            },
            scales: {
                x: {
                    display: true,
                    grid: {
                        display: false,
                        color: '#334155'
                    },
                    title: {
                        display: true,
                        text: isSorted ? ' MEJOR  ⟶  PEOR ' : ' INICIO (Tiempo) ⟶  FIN ',
                        color: '#64748b',
                        font: { size: 10, weight: 'bold' },
                        padding: { top: 10 }
                    },
                    ticks: { display: false }
                },
                y: {
                    display: true,
                    position: 'right',
                    grid: {
                        color: 'rgba(51, 65, 85, 0.3)',
                        drawBorder: false
                    },
                    ticks: {
                        color: '#475569',
                        font: { size: 9 },
                        maxTicksLimit: 5
                    }
                }
            }
        }
    });

    // --- CLICK-TO-SELECT-PERCENTAGE ---
    // In sorted mode (Calidad), clicking on the chart sets the stacking percentage.
    // In time mode (Tiempo), clicking is disabled since frames aren't quality-sorted.
    canvas.onclick = function(evt) {
        if (!chartInstance || !isSorted) return;

        // Get the chart area (excludes axis labels/padding)
        const chartArea = chartInstance.chartArea;
        if (!chartArea) return;

        // Chart.js chartArea uses visible canvas pixels, so keep the click in the same coordinate space.
        const rect = canvas.getBoundingClientRect();
        const clickX = evt.clientX - rect.left;
        const chartWidth = chartArea.right - chartArea.left;
        if (chartWidth <= 0) return;

        // Check if click is within the chart area
        if (clickX < chartArea.left || clickX > chartArea.right) return;

        // Calculate percentage based on position within chart area
        const relativeX = Math.max(0, Math.min(1, (clickX - chartArea.left) / chartWidth));
        const pct = Math.max(1, Math.min(100, Math.round(relativeX * 100)));

        // Update through the slider event so the label, state, and chart redraw stay in sync.
        if (ui.stackSlider) {
            ui.stackSlider.value = String(pct);
            ui.stackSlider.dispatchEvent(new Event("input", { bubbles: true }));
            return;
        } else if (ui.pctDisplay) {
            ui.pctDisplay.textContent = pct + "%";
        }

        currentRecommendedPct = pct;
        updateChartViz();
    };

    // Visual cursor hint: pointer in sorted mode, default in time mode
    canvas.style.cursor = isSorted ? "pointer" : "default";

}

listen("log_event", (e) => log(e.payload.level, e.payload.msg));
listen("backend_panic", (e) => {
    const msg = (e && e.payload != null) ? String(e.payload) : "Error interno";
    log("ERROR", "Backend panic: " + msg);
    try {
        showCustomAlert(
            tr("general.backend_panic_title", "Error inesperado"),
            tr("general.backend_panic_body", "La operación falló y se detuvo de forma segura.") + "\n\n" + msg
        );
    } catch (_) {
        showCustomAlert("Error", "Error inesperado: " + msg);
    }
});
listen("progress", (e) => {
    const step = translateBackendProgressText(e.payload.step);
    const details = translateBackendProgressText(e.payload.details);
    const pct = Number(e.payload.pct ?? 0);

    // 1. Update Status Bar (Log side)
    if (ui.statusText) ui.statusText.textContent = `${step}: ${pct.toFixed(0)}%`;

    // 2. Update Status Bar Progress Line
    if (ui.pBarContainer) {
        ui.pBarContainer.style.display = "block";
        ui.pBarFill.style.width = `${pct}%`;
    }

    // 3. Update GLOBAL Overlay
    const overlayBar = $("#overlay-progress-fill");
    const overlayMsg = $("#processing-msg");
    const overlayDet = $("#processing-details");

    if (overlayBar) overlayBar.style.width = `${pct}%`;
    if (overlayMsg) overlayMsg.textContent = `${step} ${Math.round(pct)}%`;
    if (overlayDet && details) overlayDet.textContent = details;

    // 4. Update LOCAL Overlay (Sync check requested by user)
    // "verifica que los mensajes de la pantalla de carga esten mostrandose correctamente"
    if (isLocalOperation) {
        const localMsg = $("#local-msg");
        const localPct = $("#local-pct");
        if (localMsg) localMsg.textContent = step;
        if (localPct) localPct.textContent = `${Math.round(pct)}%`;
    }

    if (pct >= 100) {
        setTimeout(() => { if (ui.pBarContainer) ui.pBarContainer.style.display = "none"; }, 500);
    }
});

const btnToggleSource = $("#btn-toggle-source");
if (btnToggleSource) {
    // Evitar que el mousedown inicie el arrastre de la imagen (attachZoomEvents)
    btnToggleSource.addEventListener("mousedown", (e) => e.stopPropagation());

    btnToggleSource.addEventListener("click", (e) => {
        e.stopPropagation();
        const sourceView = $("#view-source");
        if (!sourceView) return;

        if (sourceView.style.display === "none") {
            // MOSTRAR
            sourceView.style.display = "flex";
            btnToggleSource.textContent = tr("viewer.hide_source", "Ocultar Fuente");
            btnToggleSource.style.borderColor = "#334155";
            btnToggleSource.style.color = "#cbd5e1";
            btnToggleSource.style.background = "rgba(0,0,0,0.4)";
        } else {
            // OCULTAR
            sourceView.style.display = "none";
            btnToggleSource.textContent = tr("viewer.show_source", "Mostrar Fuente");
            btnToggleSource.style.borderColor = "#34d399";
            btnToggleSource.style.color = "#34d399";
            btnToggleSource.style.background = "rgba(16, 185, 129, 0.1)";
        }
        btnToggleSource.style.width = "auto"; // Forzar auto width
        btnToggleSource.style.pointerEvents = "auto"; // Forzar interaccion

        // FORZAR RESIZE DEL VIEWPORT
        // fitToScreen depende de offsetWidth/Height, asi que esperamos al reflow
        requestAnimationFrame(() => {
            fitVisibleViewportImage();
            // Llama doble por si acaso la transicion tarda
            setTimeout(fitVisibleViewportImage, 100);
        });
    });
}

// Function to load fonts
async function loadFonts() {
    if (!ui.selAnimFont) return;
    try {
        const fonts = await invoke("get_available_fonts");
        if (fonts && fonts.length > 0) {
            ui.selAnimFont.innerHTML = "";
            fonts.forEach(f => {
                const opt = document.createElement("option");
                opt.value = f;
                opt.textContent = f;
                ui.selAnimFont.appendChild(opt);
            });
            // Try to set default
            if (fonts.includes("Arial")) ui.selAnimFont.value = "Arial";
        }
    } catch (e) {
        console.error("Error loading fonts:", e);
    }
}

// Initial Load
loadFonts();

// Restore Cancel Listener if missing (safety check)
if (ui.btnAnimCancel) {
    ui.btnAnimCancel.removeEventListener("click", stopAnimationPlayer);
    ui.btnAnimCancel.addEventListener("click", stopAnimationPlayer);
}

// =========================================================================
// I18N INITIALIZATION
// =========================================================================

(async () => {
    try {
        await i18n.init();

        // Sync Settings Dropdown
        const langSelect = document.getElementById("settings-lang-select");
        if (langSelect) {
            langSelect.value = i18n.currentLang;
            langSelect.dispatchEvent(new Event("change"));
            langSelect.addEventListener("change", (e) => {
                i18n.setLanguage(e.target.value);
            });
        }
        syncActivationLanguageButtons(i18n.currentLang);

        // Initialize tutorials after i18n is ready. Startup is gated by license status.
        tutorialManager.setCanRunPredicate(canRunTutorialsAfterLicense);
        tutorialManager.init();
        tutorialsInitialized = true;
        scheduleTutorialAfterLicense();

    } catch (e) {
        console.error("I18n Init Failed:", e);
    }
})();

// Listen for global language change events (from activation modal buttons or others)
document.addEventListener("changeLang", (e) => {
    const selectedLang = normalizeLanguageCode(e.detail);
    syncActivationLanguageButtons(selectedLang);
    i18n.setLanguage(selectedLang).then(() => {
        const langSelect = document.getElementById("settings-lang-select");
        if (langSelect) {
            langSelect.value = selectedLang;
            langSelect.dispatchEvent(new Event("change"));
        }
    }).catch((err) => {
        console.error("Language change failed:", err);
    });
});

window.addEventListener("languageChanged", (e) => {
    syncActivationLanguageButtons(e.detail?.lang);
    const langSelect = document.getElementById("settings-lang-select");
    if (langSelect) {
        langSelect.value = normalizeLanguageCode(e.detail?.lang);
        langSelect.dispatchEvent(new Event("change"));
    }
    if (lastLicenseInfo) {
        updateProUI(lastLicenseInfo);
    }
    syncAnimationSpeedUI();
    updateAnimationFrameCounter();
    if (divAnimFrameManager?.style.display !== "none") {
        renderFrameManager();
    }
});

// =========================================================================
// GRID DRAWING UTILS (New Implementation)
// =========================================================================

window.drawGrid = function (points, imgW, imgH) {
    const canvas = ui.gridOverlay;
    if (!canvas) return;

    canvas.width = imgW;
    canvas.height = imgH;

    const ctx = canvas.getContext('2d');
    ctx.clearRect(0, 0, imgW, imgH);

    if (!points || points.length === 0) return;


    // STRICT POSITIONING (Matches CSS Layout)
    const img = ui.imgSource;
    if (img) {
        canvas.style.position = "absolute";
        canvas.style.left = img.offsetLeft + "px";
        canvas.style.top = img.offsetTop + "px";
        canvas.style.transform = "none";
        canvas.style.width = img.offsetWidth + "px";
        canvas.style.height = img.offsetHeight + "px";
    }

    // --- ELECTRIC LIQUID AESTHETIC ---
    // Style: Cyberpunk/Data-Moshing. Deep Blue + White Hot Nodes.
    // Technique: Additive Blending ('lighter') to simulate energy accumulation.

    ctx.save();
    ctx.globalCompositeOperation = 'lighter'; // Key for the "Glowing Energy" look
    ctx.lineCap = "round";
    ctx.lineJoin = "round";

    // 1. MESH CONNECTIONS (The Energy Net)
    if (points.length > 0) {
        // Handle size from object or array
        const p0 = points[0];
        const gridSize = (typeof p0.size !== 'undefined') ? p0.size : (p0[2] || 48);

        // 1.8x grid size ensures diagonals are connected (creates triangles)
        const connectDist = gridSize * 1.8;
        const connectDistSq = connectDist * connectDist;

        ctx.beginPath();
        // Electric Blue: R=0, G=150, B=255. Low opacity allows brightness to build up where lines overlap.
        ctx.strokeStyle = "rgba(0, 160, 255, 0.3)";
        ctx.lineWidth = 1.2;

        // Shadow/Glow for lines
        ctx.shadowBlur = 15;
        ctx.shadowColor = "rgba(0, 100, 255, 0.8)";

        // Performance Limit
        if (points.length < 5000) {
            for (let i = 0; i < points.length; i++) {
                const p1 = points[i];
                const x1 = (typeof p1.x !== 'undefined') ? p1.x : p1[0];
                const y1 = (typeof p1.y !== 'undefined') ? p1.y : p1[1];

                // Check forward only
                for (let j = i + 1; j < points.length; j++) {
                    const p2 = points[j];
                    const x2 = (typeof p2.x !== 'undefined') ? p2.x : p2[0];
                    const y2 = (typeof p2.y !== 'undefined') ? p2.y : p2[1];

                    const dx = x1 - x2;
                    const dy = y1 - y2;

                    if (Math.abs(dx) > connectDist || Math.abs(dy) > connectDist) continue;

                    if ((dx * dx + dy * dy) < connectDistSq) {
                        ctx.moveTo(x1, y1);
                        ctx.lineTo(x2, y2);
                    }
                }
            }
        }
        ctx.stroke();
    }

    // 2. NODES (The Data Points)
    // White Hot Centers with Blue Halo

    // A) Blue Halo (Soft)
    ctx.shadowBlur = 8;
    ctx.shadowColor = "#00ffff"; // Cyan Glow
    ctx.fillStyle = "rgba(0, 180, 255, 0.6)";

    // Helper to get coords
    const getX = (p) => (typeof p.x !== 'undefined') ? p.x : p[0];
    const getY = (p) => (typeof p.y !== 'undefined') ? p.y : p[1];

    ctx.beginPath();
    // 2. NODES (The Data Points)
    // White Hot Centers with Blue Halo

    // A) Blue Halo (Soft)
    ctx.shadowBlur = 8;
    ctx.shadowColor = "#00ffff"; // Cyan Glow
    ctx.fillStyle = "rgba(0, 180, 255, 0.6)";

    ctx.beginPath();
    points.forEach(p => {
        const px = getX(p);
        const py = getY(p);
        // Slightly larger circle for the glow
        ctx.moveTo(px + 3, py);
        ctx.arc(px, py, 3, 0, Math.PI * 2);
    });
    ctx.fill();

    // B) White Core (Sharp)
    ctx.shadowBlur = 4;
    ctx.shadowColor = "#ffffff";
    ctx.fillStyle = "#ffffff";
    ctx.globalAlpha = 1.0;

    ctx.beginPath();
    points.forEach(p => {
        const px = getX(p);
        const py = getY(p);
        ctx.moveTo(px + 1.5, py);
        ctx.arc(px, py, 1.5, 0, Math.PI * 2);
    });
    ctx.fill();

    ctx.restore();
    console.log(`Electric Mesh drawn: ${points.length} nodes.`);
};

// =========================================================================
// GESTOR DE FRAMES (FRAME MANAGER)
// =========================================================================

const btnAnimManageFrames = $("#btn-anim-manage-frames");
const divAnimFrameManager = $("#anim-frame-manager");
const divAnimViewport = $("#anim-viewport");
const divAnimFrameGrid = $("#anim-frame-grid");
const btnAnimApplyFrames = $("#btn-anim-apply-frames");
const btnAnimCancelFrames = $("#btn-anim-cancel-frames");
const btnAnimFrameZoomIn = $("#btn-anim-frame-zoom-in");
const btnAnimFrameZoomOut = $("#btn-anim-frame-zoom-out");
const animFrameZoomValue = $("#anim-frame-zoom-value");
const animFrameManagerSummary = $("#anim-frame-manager-summary");
let animFrameThumbSize = 160;

function applyFrameManagerZoom(nextSize = animFrameThumbSize) {
    animFrameThumbSize = Math.max(96, Math.min(300, nextSize));
    if (divAnimFrameGrid) {
        divAnimFrameGrid.style.setProperty("--frame-cell-size", `${animFrameThumbSize}px`);
    }
    if (animFrameZoomValue) {
        animFrameZoomValue.textContent = `${animFrameThumbSize}px`;
    }
}

if (btnAnimFrameZoomIn) {
    btnAnimFrameZoomIn.addEventListener("click", () => applyFrameManagerZoom(animFrameThumbSize + 32));
}

if (btnAnimFrameZoomOut) {
    btnAnimFrameZoomOut.addEventListener("click", () => applyFrameManagerZoom(animFrameThumbSize - 32));
}

if (btnAnimManageFrames) {
    btnAnimManageFrames.addEventListener("click", () => {
        showFrameManager();
    });
}

function showFrameManager() {
    // Modal Mode: We do NOT hide divAnimViewport anymore, just show the modal on top
    if (divAnimFrameManager) divAnimFrameManager.style.display = "flex";

    // Dim background or ensure viewport is unresponsive if needed,
    // but the modal overlay (if styled right) covers it.

    renderFrameManager();

    // Reset Scroll
    if (divAnimFrameGrid) {
        divAnimFrameGrid.scrollTop = 0;
        divAnimFrameGrid.scrollLeft = 0;
    }
}

function renderFrameManager() {
    if (!divAnimFrameGrid) return;
    divAnimFrameGrid.innerHTML = "";
    applyFrameManagerZoom(animFrameThumbSize);

    const totalFrames = animFullSourceFrames.length;
    const activeFrames = Math.max(0, totalFrames - animExcludedIndices.size);
    if (animFrameManagerSummary) {
        animFrameManagerSummary.textContent = trFormat(
            "animation.frame_manager.summary",
            { active: activeFrames, total: totalFrames },
            `${activeFrames} / ${totalFrames}`
        );
    }

    animFullSourceFrames.forEach((src, index) => {
        const isExcluded = animExcludedIndices.has(index);

        const cell = document.createElement("div");
        cell.className = "frame-cell";
        cell.style.position = "relative";
        cell.style.aspectRatio = "1";
        cell.style.border = isExcluded ? "2px solid #ef4444" : "1px solid rgba(255,255,255,0.2)";
        cell.style.borderRadius = "8px";
        cell.style.overflow = "hidden";
        cell.style.cursor = "pointer";
        cell.style.opacity = isExcluded ? "0.5" : "1";

        const img = document.createElement("img");
        // Ensure src is valid (convert if needed) matches startAnimationPlayer logic
        let imgSrc = src;
        if (typeof src === 'string' && src.startsWith("data:")) {
            imgSrc = src;
        } else {
            imgSrc = convertFileSrc(src);
        }
        img.src = imgSrc;

        img.style.width = "100%";
        img.style.height = "100%";
        img.style.objectFit = "cover";

        const overlay = document.createElement("div");
        overlay.style.position = "absolute";
        overlay.style.inset = "0";
        overlay.style.display = "flex";
        overlay.style.justifyContent = "center";
        overlay.style.alignItems = "center";
        overlay.style.background = isExcluded ? "rgba(0,0,0,0.6)" : "rgba(0,0,0,0.0)";
        overlay.style.transition = "background 0.2s";

        // Frame Number Label (Always visible)
        const lbl = document.createElement("span");
        lbl.textContent = `#${index + 1}`;
        lbl.style.position = "absolute";
        lbl.style.top = "2px";
        lbl.style.left = "4px";
        lbl.style.fontSize = "0.75rem";
        lbl.style.fontWeight = "bold";
        lbl.style.color = "white";
        lbl.style.textShadow = "0 1px 2px black";
        lbl.style.pointerEvents = "none";
        lbl.style.zIndex = "10";

        // X Icon for excluded
        const icon = document.createElement("span");
        icon.innerHTML = "✕";
        icon.style.fontSize = "3rem";
        icon.style.color = "#ef4444";
        icon.style.fontWeight = "bold";
        icon.style.display = isExcluded ? "block" : "none";

        cell.appendChild(img);
        cell.appendChild(overlay);
        cell.appendChild(lbl);
        overlay.appendChild(icon);



        cell.onclick = () => {
            if (animExcludedIndices.has(index)) {
                animExcludedIndices.delete(index);
            } else {
                animExcludedIndices.add(index);
            }
            renderFrameManager(); // Re-render to update UI
        };

        divAnimFrameGrid.appendChild(cell);
    });
}

if (btnAnimApplyFrames) {
    btnAnimApplyFrames.addEventListener("click", () => {
        const newPlaylist = animFullSourceFrames.filter((_, i) => !animExcludedIndices.has(i));
        if (newPlaylist.length === 0) {
            showCustomAlert(tr("general.error", "Error"), tr("animation.errors.no_active_frames", "No hay frames activos para exportar."));
            return;
        }

        // Restart player with keepFilters=true to maintain visual settings, but with new playlist
        startAnimationPlayer(newPlaylist, true);

        // Also update text/stats if needed
        closeFrameManager();
    });
}

// Horizontal Scroll Listener REMOVED (Now using native Vertical Grid Scroll)
if (divAnimFrameGrid) {
    // Optional: Add custom scroll behavior if requested, but native is best for Grid.
}

if (btnAnimCancelFrames) {
    btnAnimCancelFrames.addEventListener("click", () => {
        closeFrameManager();
    });
}

function closeFrameManager() {
    if (divAnimFrameManager) divAnimFrameManager.style.display = "none";
    // No need to restore viewport as we didn't hide it
}

// =========================================================================
// LIQUID WARPING UI LOGIC
// =========================================================================

if (ui.alignMode) {
    ui.alignMode.addEventListener("change", () => {
        updateAlignModeUI();
    });
}

function updateAlignModeUI() {
    if (!ui.alignMode || !ui.multipointWrapper) return;
    const mode = ui.alignMode.value;
    const warning = document.getElementById("liquid-warning");

    const flow = getActiveZenithFlow();
    if (!flow.needsPoints || mode === "global" || mode === "zenith_map" || mode === "zenith_v3") {
        ui.multipointWrapper.style.display = "none";
        if (warning) warning.style.display = "none";
    } else {
        // "liquid" serves as advanced mode
        ui.multipointWrapper.style.display = "block";
        if (warning) warning.style.display = isZenithUltimateSelected() ? "none" : "block";
    }
}

// Inicializar estado UI
updateAlignModeUI();

// =========================================================================
// STACKING TIMER & TIPS UTILS
// =========================================================================
let stackingTimerId = null;
let stackingTipsId = null;
let stackingStartTime = 0;

const STACKING_TIPS = [
    "<svg class='zas-icon icon-pulse' style='color:#fbbf24;'><use href='#icon-lightbulb'></use></svg> Wavelets ayudan a resaltar detalles finos que el apilado descubre.",
    "<svg class='zas-icon icon-pulse' style='color:#fbbf24;'><use href='#icon-lightbulb'></use></svg> El Drizzle mejora la resolución si tienes buen seeing y muchos frames.",
    "<svg class='zas-icon icon-pulse' style='color:#fbbf24;'><use href='#icon-lightbulb'></use></svg> Usa 'Superficie' para Luna/Sol y 'Planetario' para Júpiter/Saturno.",
    "<svg class='zas-icon icon-pulse' style='color:#fbbf24;'><use href='#icon-lightbulb'></use></svg> Alineación Multipunto corrige la turbulencia atmosférica (seeing).",
    "<svg class='zas-icon icon-pulse' style='color:#fbbf24;'><use href='#icon-lightbulb'></use></svg> Puedes ajustar el brillo y contraste después de apilar sin perder datos.",
    "<svg class='zas-icon icon-pulse' style='color:#fbbf24;'><use href='#icon-lightbulb'></use></svg> Si la imagen se ve cuadriculada, prueba reducir la Nitidez (Sharpen).",
    "<svg class='zas-icon icon-pulse' style='color:#fbbf24;'><use href='#icon-lightbulb'></use></svg> Zenith Astro Stacker recuerda tus ajustes de Wavelets para el próximo video."
];

function startStackingTimer() {
    stopStackingTimer(); // clean prev
    stackingStartTime = Date.now();

    // Force recreate UI to ensure correct placement
    let timerEl = document.getElementById("stacking-timer");
    if (timerEl) timerEl.remove();

    if (ui.pBarContainer) {
        timerEl = document.createElement("div");
        timerEl.id = "stacking-timer";
        timerEl.style.marginTop = "2px";
        timerEl.style.color = "#fbbf24";
        timerEl.style.fontFamily = "monospace";
        timerEl.style.fontSize = "0.9em";
        timerEl.style.fontWeight = "bold";
        timerEl.style.textAlign = "center";

        // Insert AFTER processing-msg
        const msgEl = document.getElementById("processing-msg");
        if (msgEl && msgEl.parentNode) {
            msgEl.parentNode.insertBefore(timerEl, msgEl.nextSibling);
        }
    }

    stackingTimerId = setInterval(() => {
        const elapsed = Math.floor((Date.now() - stackingStartTime) / 1000);
        const m = Math.floor(elapsed / 60);
        const s = elapsed % 60;
        if (timerEl) timerEl.textContent = `(${m.toString().padStart(2, '0')}:${s.toString().padStart(2, '0')})`;
    }, 1000);
}

function stopStackingTimer() {
    if (stackingTimerId) clearInterval(stackingTimerId);
    const timerEl = document.getElementById("stacking-timer");
    if (timerEl) timerEl.textContent = "";
}

function startTipsCarousel() {
    stopTipsCarousel();

    // Force recreate UI to ensure correct placement
    let tipsEl = document.getElementById("stacking-tips");
    if (tipsEl) tipsEl.remove();

    if (ui.pBarContainer) {
        tipsEl = document.createElement("div");
        tipsEl.id = "stacking-tips";
        tipsEl.style.marginTop = "8px";
        tipsEl.style.color = "#60a5fa"; // blue-400
        tipsEl.style.fontSize = "0.9em";
        tipsEl.style.fontWeight = "bold";
        tipsEl.style.textAlign = "center";
        tipsEl.style.fontStyle = "italic";
        tipsEl.style.height = "20px"; // prevent layout jump
        tipsEl.style.transition = "opacity 0.5s";
        tipsEl.style.zIndex = "1000";

        // Anchor below Timer (if exists) OR Processing Message
        const anchor = document.getElementById("stacking-timer") || document.getElementById("processing-msg");
        if (anchor && anchor.parentNode) {
            anchor.parentNode.insertBefore(tipsEl, anchor.nextSibling);
        } else if (ui.pBarContainer.parentNode) {
            // Fallback: Append to overlay
            ui.pBarContainer.parentNode.appendChild(tipsEl);
        }
    }

    if (!tipsEl) return;

    let idx = 0;
    const showNextTip = () => {
        tipsEl.style.opacity = "0";
        setTimeout(() => {
            tipsEl.innerHTML = STACKING_TIPS[idx % STACKING_TIPS.length];
            tipsEl.style.opacity = "1";
            idx++;
        }, 500);
    };

    showNextTip(); // First one
    stackingTipsId = setInterval(showNextTip, 8000); // Check user request (sutil)
}

function stopTipsCarousel() {
    if (stackingTipsId) clearInterval(stackingTipsId);
    const tipsEl = document.getElementById("stacking-tips");
    if (tipsEl) {
        tipsEl.textContent = "";
    }
}

// Initial Sync
updateAlignModeUI();

// =========================================================================
// PROCESS CANCELLATION
// =========================================================================

const btnCancelProcess = document.getElementById("btn-cancel-process");
if (btnCancelProcess) {
    btnCancelProcess.addEventListener("click", async () => {
        try {
            if (isCancellationRequested) return;
            isCancellationRequested = true;
            btnCancelProcess.disabled = true;
            btnCancelProcess.style.opacity = "0.55";
            btnCancelProcess.style.cursor = "wait";
            console.warn("User requested cancellation...");
            const msgEl = document.getElementById("processing-msg");
            if (msgEl) {
                // Visual Feedback immediately
                msgEl.innerText = "CANCELANDO / CANCELLING...";
                msgEl.style.color = "#ef4444";
            }
            const detEl = document.getElementById("processing-details");
            if (detEl) detEl.textContent = "Deteniendo backend...";

            // Invoke Backend Command
            await invoke("cancel_processing");

        } catch (e) {
            console.error("Error sending cancel command:", e);
        }
    });
}

    // =========================================================================
    // INTERACTIVE ALIGNMENT ROI (SURFACE) - LEGACY DISABLED
    // =========================================================================

/*
Legacy manual surface animation realignment. Batch surface outputs are already
stabilized against a shared anchor and normalized to the same canvas, so the UI
is commented out in index.html and this handler remains commented for reference.
(function () {
    let isSelectingAnimROI = false;
    const btnAnimRealignSurface = document.getElementById("btn-anim-realign-surface");
    const animAlignRoi = document.getElementById("anim-align-roi");
    const animPreviewImg = document.getElementById("anim-preview-img");

    if (btnAnimRealignSurface && animAlignRoi && animPreviewImg) {


        // 1. Button Click Handler
        btnAnimRealignSurface.addEventListener("click", async (e) => {
            e.stopPropagation();

            if (!isSelectingAnimROI) {
                // START SELECTION MODE
                isSelectingAnimROI = true;
                animAlignRoi.style.display = "block";
                btnAnimRealignSurface.textContent = "✅ Confirmar (Start)";
                btnAnimRealignSurface.className = "success"; // Reset class to success
                btnAnimRealignSurface.style.background = "#10b981"; // Green

                // Initialize Position (Centered 33%) if not set
                if (!animAlignRoi.style.left) {
                    animAlignRoi.style.left = "33%";
                    animAlignRoi.style.top = "33%";
                    animAlignRoi.style.width = "33%";
                    animAlignRoi.style.height = "33%";
                }
            } else {
                // CONFIRM -> EXECUTE

                // 1. Capture ROI Dimensions BEFORE hiding (otherwise offsetWidth is 0)
                const boxL = animAlignRoi.offsetLeft;
                const boxT = animAlignRoi.offsetTop;
                const boxW = animAlignRoi.offsetWidth;
                const boxH = animAlignRoi.offsetHeight;

                // 2. Hide UI
                isSelectingAnimROI = false;
                animAlignRoi.style.display = "none";
                btnAnimRealignSurface.textContent = "🌑 Alinear Superficie";
                btnAnimRealignSurface.className = "secondary"; // Reset class
                btnAnimRealignSurface.style.background = ""; // Reset

                // 3. Calculate Coordinates relative to ORIGINAL image size
                // We need natural dimensions vs displayed dimensions
                const natW = animPreviewImg.naturalWidth;
                const natH = animPreviewImg.naturalHeight;
                const dispW = animPreviewImg.clientWidth;
                const dispH = animPreviewImg.clientHeight;

                if (natW === 0 || dispW === 0) {
                    showCustomAlert("Error", "Imagen no valida o dimensiones 0.");
                    return;
                }

                // Scale to Natural
                const ratioX = natW / dispW;
                const ratioY = natH / dispH;

                const finalX = Math.round(boxL * ratioX);
                const finalY = Math.round(boxT * ratioY);
                const finalW = Math.round(boxW * ratioX);
                const finalH = Math.round(boxH * ratioY);

                console.log(`Interactive ROI: Source [${finalX},${finalY} - ${finalW}x${finalH}]`);

                if (finalW < 16 || finalH < 16) {
                    showCustomAlert("Error", "La region seleccionada es muy pequeña.");
                    return;
                }

                // EXECUTE ALIGNMENT
                performSurfaceAlignment(finalX, finalY, finalW, finalH);
            }
        });

        // 2. Drag Logic for ROI Box
        let isDraggingROI = false;
        let dragStartX, dragStartY;
        let roiStartLeft, roiStartTop;

        animAlignRoi.addEventListener("mousedown", (e) => {
            isDraggingROI = true;
            dragStartX = e.clientX;
            dragStartY = e.clientY;
            roiStartLeft = animAlignRoi.offsetLeft;
            roiStartTop = animAlignRoi.offsetTop;
            animAlignRoi.style.cursor = "grabbing";
            e.preventDefault(); // Prevent text selection
            e.stopPropagation();
        });

        // Global Move/Up listeners are safer on window
        window.addEventListener("mousemove", (e) => {
            if (!isDraggingROI) return;

            const dx = e.clientX - dragStartX;
            const dy = e.clientY - dragStartY;

            let newL = roiStartLeft + dx;
            let newT = roiStartTop + dy;

            // Constrain to Image Bounds
            const containerW = animPreviewImg.clientWidth;
            const containerH = animPreviewImg.clientHeight;
            const boxW = animAlignRoi.offsetWidth;
            const boxH = animAlignRoi.offsetHeight;

            if (newL < 0) newL = 0;
            if (newT < 0) newT = 0;
            if (newL + boxW > containerW) newL = containerW - boxW;
            if (newT + boxH > containerH) newT = containerH - boxH;

            animAlignRoi.style.left = newL + "px";
            animAlignRoi.style.top = newT + "px";
        });

        window.addEventListener("mouseup", () => {
            if (isDraggingROI) {
                isDraggingROI = false;
                animAlignRoi.style.cursor = "move";
            }
        });
    }

    async function performSurfaceAlignment(roiX, roiY, roiW, roiH) {
        // Fix: Use batchResultPaths (OS Paths) instead of animFullSourceFrames (Base64/AssetUrl)
        if (!batchResultPaths || batchResultPaths.length === 0) return;

        // Filter out excluded using the same indices
        const validFrames = batchResultPaths.filter((_, i) => !animExcludedIndices.has(i));
        if (validFrames.length < 2) {
            showCustomAlert("Error", "Necesitas al menos 2 frames activos.");
            return;
        }

        const btn = document.getElementById("btn-anim-realign-surface"); // Refetch new generic btn
        if (btn) btn.disabled = true;

        showProcessing("Alineando Superficie (ROI)...");

        try {
            // NEW: Pass custom_roi (x, y, w, h)
            // Note: Rust expects Option<(usize, usize, usize, usize)>
            // In JS invoke, we pass it as `customRoi: [x, y, w, h]` or object?
            // Tauri usually maps Option<T> from null/undefined or value.
            // Tuple mapping might be tricky from JS object.
            // We pass it as array which maps to tuple in serde if configured, otherwise as object.
            // Let's try passing as array first.

            const newFrames = await invoke("realign_animation_frames", {
                paths: validFrames,
                modeType: "surface",
                customRoi: [roiX, roiY, roiW, roiH]
            });

            // Update Player
            startAnimationPlayer(newFrames, true); // Keep speed & filters
            showCustomAlert("Exito", "Alineacion de Superficie completada con exito.");

        } catch (e) {
            console.error(e);
            showCustomAlert("Error", "Fallo al alinear: " + e);
        } finally {
            hideProcessing();
            if (btn) btn.disabled = false;
        }
    }


})();
*/

// =========================================================================
// DIRECT ANIMATION LOAD LOGIC (NEW)
// =========================================================================
(function () {
    const btnLoadAnim = document.getElementById("btn-load-animation");

    if (btnLoadAnim) {
        btnLoadAnim.addEventListener("click", async () => {
            try {
                const result = await openDialog({
                    multiple: true,
                    filters: [{
                        name: 'Images',
                        extensions: ['png', 'tif', 'tiff', 'jpg', 'jpeg']
                    }]
                });

                if (result && result.length > 0) {
                    // 1. Ensure Strings
                    const paths = result.map(file => (typeof file === "object" && file !== null && file.path) ? file.path : file);
                    const validPaths = paths.filter(p => typeof p === 'string' && p.length > 0);

                    if (validPaths.length === 0) {
                        showCustomAlert(tr("general.error", "Error"), tr("animation.errors.no_valid_paths", "No se encontraron rutas de archivo validas."));
                        return;
                    }

                    // 2. Sort files naturally
                    const sorted = validPaths.sort(new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' }).compare);

                    // 3. Replicate Batch "Cache" Loading
                    // Batch logic pushes Base64 previews to batchGeneratedImages.

                    showProcessing(`CARGANDO ${sorted.length} IMAGENES...`);

                    // Allow UI to render the loading screen before blocking logic
                    setTimeout(async () => {
                        try {
                            const loadedImages = [];
                            const loadedPaths = [];

                            for (let i = 0; i < sorted.length; i++) {
                                const path = sorted[i];
                                try {
                                    // Use the same backend command as Mosaic Manager (or similar to batch).
                                    // This bypasses browser security issues with local files.
                                    const res = await invoke("load_image_thumbnail", { path: path });

                                    if (res && res.preview_base64) {
                                        const prefix = res.preview_base64.startsWith("data:") ? "" : "data:image/png;base64,";
                                        loadedImages.push(prefix + res.preview_base64);
                                        loadedPaths.push(path);
                                    } else {
                                        console.warn("No preview for", path);
                                    }
                                } catch (err) {
                                    console.error("Failed to load thumbnail for", path, err);
                                }
                            }

                            if (loadedImages.length === 0) {
                                hideProcessing();
                                showCustomAlert("Error", "No se pudieron cargar las previsualizaciones.");
                                return;
                            }

                            // 4. Update Global State
                            batchGeneratedImages = loadedImages;
                            batchResultPaths = loadedPaths;

                            // 5. Show Animation Editor
                            const animModal = document.getElementById("animation-modal");
                            if (animModal) {
                                animModal.style.display = "flex";
                                const animViewport = document.getElementById("anim-viewport");
                                if (animViewport) animViewport.style.display = "flex";
                            }

                            // 6. Start Player (Reset Filters = false)
                            startAnimationPlayer(batchGeneratedImages, false);

                            showCustomAlert("Exito", `Cargados ${loadedImages.length} frames.`);

                        } catch (e) {
                            console.error("Error in load loop:", e);
                            showCustomAlert("Error", "Fallo loop de carga: " + e);
                        } finally {
                            hideProcessing();
                        }
                    }, 100);
                }
            } catch (e) {
                console.error("Error loading animation:", e);
                showCustomAlert("Error", "Fallo al cargar imágenes: " + e);
            }
        });
    }
})();

// =========================================================================
// ANIMATION PLANETARY DEROTATION
// =========================================================================
(function () {
    const chk = document.getElementById("chk-anim-derotate");
    const options = document.getElementById("anim-derot-options");
    const btnApply = document.getElementById("btn-anim-apply-derotation");
    if (!chk || !options) return;

    let animDerotPlanet = "jupiter";

    function setActivePlanet(containerId, planet) {
        const container = document.getElementById(containerId);
        if (!container) return;
        container.querySelectorAll(".planet-icon").forEach((button) => {
            const isActive = button.dataset.planet === planet;
            button.classList.toggle("active", isActive);
            button.style.border = isActive ? "2px solid #8b5cf6" : "1px solid rgba(255,255,255,0.15)";
            button.style.background = isActive ? "rgba(139,92,246,0.2)" : "rgba(255,255,255,0.05)";
            button.style.color = isActive ? "#c4b5fd" : "#94a3b8";
        });
    }

    chk.addEventListener("change", () => {
        options.style.display = chk.checked ? "block" : "none";
        setActivePlanet("anim-derot-planet-selector", animDerotPlanet);
    });

    document.getElementById("anim-derot-planet-selector")?.querySelectorAll(".planet-icon").forEach((button) => {
        button.addEventListener("click", (event) => {
            event.preventDefault();
            animDerotPlanet = button.dataset.planet || "jupiter";
            setActivePlanet("anim-derot-planet-selector", animDerotPlanet);
        });
    });

    btnApply?.addEventListener("click", async () => {
        if (!batchResultPaths || batchResultPaths.length === 0) {
            showCustomAlert(tr("general.error", "Error"), tr("animation.errors.no_frames_to_export", "No hay fotogramas para exportar."));
            return;
        }

        const cmSystem = parseInt(document.getElementById("anim-derot-cm-system")?.value || "1", 10);
        const limbStrength = parseFloat(document.getElementById("sl-anim-derot-limb")?.value || "0.5");
        const fallbackIntervalSec = parseFloat(document.getElementById("anim-derot-interval-sec")?.value || "0");
        const oldText = btnApply.innerHTML;
        btnApply.disabled = true;
        btnApply.innerHTML = tr("animation.derotation.applying", "Aplicando...");
        showProcessing(tr("animation.derotation.processing", "DEROTANDO SECUENCIA PLANETARIA..."));

        try {
            const sequenceResult = await invoke("derotate_animation_frames", {
                paths: batchResultPaths,
                planet: animDerotPlanet,
                cmSystem,
                limbStrength,
                fallbackIntervalSec: Number.isFinite(fallbackIntervalSec) ? fallbackIntervalSec : 0,
                subEarthLatDeg: null,
                northAngleDeg: null
            });
            const resultPaths = Array.isArray(sequenceResult) ? sequenceResult : (sequenceResult.output_paths || []);
            const warnings = Array.isArray(sequenceResult) ? [] : (sequenceResult.warnings || []);

            if (!resultPaths || resultPaths.length === 0) {
                showCustomAlert(tr("general.error", "Error"), tr("animation.errors.no_valid_images", "No se generaron imagenes validas para reproducir."));
                return;
            }

            batchResultPaths = resultPaths;
            batchGeneratedImages = resultPaths;
            refreshAnimationFromFullFrames(resultPaths, true);
            chk.checked = true;
            options.style.display = "block";
            log("SUCCESS", trFormat(
                "animation.derotation.sequence_done",
                { count: resultPaths.length },
                `Secuencia derotada: ${resultPaths.length} frames.`
            ));
            showCustomAlert(
                tr("animation.derotation.sequence_title", "Secuencia derotada"),
                trFormat(
                    "animation.derotation.sequence_message",
                    { count: resultPaths.length },
                    `Se generaron ${resultPaths.length} frames derotados y se cargaron en el editor.`
                ) + (warnings.length ? `<br><br>${warnings.slice(0, 3).join("<br>")}` : "")
            );
        } catch (e) {
            showCustomAlert(tr("general.error", "Error"), normalizeBackendText(e));
        } finally {
            hideProcessing();
            btnApply.disabled = false;
            btnApply.innerHTML = oldText;
        }
    });
})();

// =========================================================================
// VIDEO CONVERTER TOOL LOGIC (NEW)
// =========================================================================
(function () {
    const btnConvert = document.getElementById("btn-tools-convert");
    if (!btnConvert) return;

    const formatBytes = (bytes) => {
        const value = Number(bytes || 0);
        if (!Number.isFinite(value) || value <= 0) return "0 MB";
        const units = ["B", "KB", "MB", "GB", "TB"];
        let size = value;
        let unitIndex = 0;
        while (size >= 1024 && unitIndex < units.length - 1) {
            size /= 1024;
            unitIndex += 1;
        }
        const decimals = unitIndex >= 3 ? 2 : 1;
        return `${size.toFixed(decimals)} ${units[unitIndex]}`;
    };

    const escapeHtml = (value) => String(value ?? "")
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;")
        .replace(/'/g, "&#39;");

    const parentFolder = (filePath) => {
        const text = String(filePath || "");
        const idx = Math.max(text.lastIndexOf("/"), text.lastIndexOf("\\"));
        return idx > 0 ? text.slice(0, idx) : text;
    };

    const translateConversionProgress = (message) => {
        const text = normalizeBackendText(message);
        if (i18n?.currentLang !== "en") return text;
        return text
            .replace("Preparando SER", "Preparing SER")
            .replace("estimado", "estimated")
            .replace("Convirtiendo frame", "Converting frame")
            .replace("Conversión SER completada", "SER conversion completed");
    };

    const estimateForProfile = (preflight, profile) =>
        (preflight?.estimates || []).find((item) => item.profile === profile) || {};

    const defaultSerPath = (inputPath) => {
        const text = String(inputPath || "");
        const dot = text.lastIndexOf(".");
        const slash = Math.max(text.lastIndexOf("/"), text.lastIndexOf("\\"));
        if (dot > slash) return `${text.slice(0, dot)}.ser`;
        return `${text}.ser`;
    };

    const ensureSerExtension = (outputPath) => {
        const text = String(outputPath || "").trim();
        if (!text) return "";
        return /\.ser$/i.test(text) ? text : `${text}.ser`;
    };

    const profileCard = (preflight, profile, title, tag, desc, toneClass) => {
        const estimate = estimateForProfile(preflight, profile);
        const isRecommended = Boolean(estimate.recommended);
        const bitDepth = estimate.bit_depth || "-";
        const colorMode = estimate.is_color ? "RGB" : "Mono";
        const size = formatBytes(estimate.estimated_size_bytes);
        const perFrame = formatBytes(estimate.bytes_per_frame);
        const badge = isRecommended
            ? `<span class="ser-profile-badge">${tr("converter.recommended_badge", "Recomendado")}</span>`
            : "";

        return `
            <button class="ser-profile-card ${toneClass}" data-modal-result="${profile}">
                <span class="ser-profile-card-top">
                    <span>
                        <strong>${title}</strong>
                        <small>${tag}</small>
                    </span>
                    ${badge}
                </span>
                <span class="ser-profile-desc">${desc}</span>
                <span class="ser-profile-metrics">
                    <span><b>${tr("converter.estimate_label", "SER estimado")}</b>${size}</span>
                    <span><b>${tr("converter.report_depth", "Profundidad")}</b>${colorMode} ${bitDepth}-bit</span>
                    <span><b>${tr("converter.report_frame_size", "Tamaño/frame")}</b>${perFrame}</span>
                </span>
            </button>
        `;
    };

    async function chooseSerConversionProfile(preflight) {
        const sourceType = preflight?.source_is_color ? "Color" : "Mono";
        const html = `
            <div class="ser-converter-modal">
                <div class="ser-converter-layout">
                    <div class="ser-source-panel">
                        <div class="ser-source-card">
                            <div>
                                <span>${tr("converter.source_file", "Video fuente")}</span>
                                <strong>${escapeHtml(preflight?.file_name || pathBaseName(preflight?.input_path))}</strong>
                            </div>
                            <div>
                                <span>${tr("converter.source_size", "Tamaño actual")}</span>
                                <strong>${formatBytes(preflight?.source_size_bytes)}</strong>
                            </div>
                            <div>
                                <span>${tr("converter.source_resolution", "Resolución")}</span>
                                <strong>${preflight?.width || "-"} x ${preflight?.height || "-"}</strong>
                            </div>
                            <div>
                                <span>${tr("converter.source_frames", "Frames")}</span>
                                <strong>${preflight?.frame_count || 0}</strong>
                            </div>
                            <div>
                                <span>${tr("converter.source_depth", "Origen")}</span>
                                <strong>${sourceType} · ${preflight?.source_bit_depth || 8}-bit · ${escapeHtml(preflight?.pix_fmt || "unknown")}</strong>
                            </div>
                        </div>
                        <p class="ser-profile-intro">
                            ${tr("converter.profile_intro", "Elige cómo crear el SER de trabajo. Para MP4/MOV el video se decodifica a frames útiles para apilado; no se aplica sharpening ni filtros destructivos.")}
                        </p>
                        <p class="ser-profile-note">${tr("converter.size_note", "SER no usa compresión: por eso el tamaño final puede ser mucho mayor que el video original.")}</p>
                    </div>
                    <div class="ser-profile-grid">
                        ${profileCard(
            preflight,
            "mono8",
            tr("converter.profile_mono8_title", "Ligero recomendado"),
            "Mono 8-bit",
            tr("converter.profile_mono8_desc", "Menor tamaño. Ideal para Luna, Sol H-alpha/mono y luminancia planetaria."),
            "is-green"
        )}
                        ${profileCard(
            preflight,
            "color8",
            tr("converter.profile_color8_title", "Conservar color"),
            "RGB 8-bit",
            tr("converter.profile_color8_desc", "Mantiene color cuando el video lo trae. Ocupa cerca de 3x frente al modo mono."),
            "is-blue"
        )}
                        ${profileCard(
            preflight,
            "autoDepth",
            tr("converter.profile_auto_title", "Alta profundidad"),
            "Auto 8/16-bit",
            tr("converter.profile_auto_desc", "Preserva 10/12/16-bit si la fuente lo permite. Mejor calidad potencial, archivo más grande."),
            "is-purple"
        )}
                    </div>
                </div>
            </div>
        `;

        return await showCustomChoice(
            tr("converter.profile_title", "Formato de conversión SER"),
            html,
            null,
            tr("general.cancel", "Cancelar")
        );
    }

    btnConvert.addEventListener("click", async () => {
        let unlisten = null;
        let conversionStarted = false;
        let cancelBtn = null;
        let previousCancelDisplay = "";
        try {
            // 1. Select File
            const result = await openDialog({
                multiple: false,
                filters: [{
                    name: 'Video Files',
                    extensions: ['mov', 'avi', 'mp4', 'mkv', 'wmv', 'flv', 'mts', 'm2ts', 'webm', 'mpg', 'mpeg', '3gp', 'ogv']
                }]
            });

            if (!result) return;
            const path = (typeof result === "object" && result !== null && result.path) ? result.path : result;
            if (!path) return;

            showProcessing(tr("converter.inspecting", "LEYENDO DATOS DEL VIDEO..."));
            const preflight = await invoke("get_ser_conversion_preflight", {
                inputPath: path
            });
            hideProcessing();

            const profile = await chooseSerConversionProfile(preflight);
            if (!profile) return;

            const selectedOutputPath = ensureSerExtension(await saveDialog({
                defaultPath: defaultSerPath(path),
                filters: [{ name: "SER Video", extensions: ["ser"] }]
            }));
            if (!selectedOutputPath) return;

            conversionStarted = true;
            btnConvert.disabled = true;
            btnConvert.style.opacity = "0.65";
            btnConvert.style.cursor = "wait";

            const startTime = Date.now();
            showProcessing(tr("converter.processing", "CONVIRTIENDO VIDEO A SER..."));
            cancelBtn = document.getElementById("btn-cancel-process");
            if (cancelBtn) {
                previousCancelDisplay = cancelBtn.style.display;
                cancelBtn.style.display = "none";
            }

            // Setup Progress Listener
            unlisten = await listen("conversion_progress", (event) => {
                const p = event.payload || {};
                const bar = document.getElementById("overlay-progress-fill");
                const det = document.getElementById("processing-details");
                const progress = Number(p.progress ?? p.percent ?? 0);
                if (bar) bar.style.width = `${Math.max(0, Math.min(100, progress)).toFixed(1)}%`;
                if (det) {
                    const estimate = p.estimatedSizeBytes ? ` | ${formatBytes(p.writtenBytes)} / ~${formatBytes(p.estimatedSizeBytes)}` : "";
                    const message = p.message
                        ? translateConversionProgress(p.message)
                        : tr("converter.processing_detail", "Convirtiendo frames...");
                    det.textContent = `${message}${estimate}`;
                }
            });

            // 2. Start Conversion (Returns Stats Struct)
            const stats = await invoke("convert_video_to_ser_frontend", {
                inputPath: path,
                profile,
                outputPath: selectedOutputPath
            });

            // 3. Report Success
            if (unlisten) {
                unlisten();
                unlisten = null;
            }
            hideProcessing();

            const totalTime = stats.duration_sec || ((Date.now() - startTime) / 1000);
            const finalSize = stats.size_bytes ?? 0;
            const estimatedSize = stats.estimated_size_bytes ?? 0;
            const frameCount = stats.frame_count ?? 0;
            const profileLabel = stats.profile || (profile === "color8" ? "Color RGB 8-bit" : profile === "autoDepth" ? "Auto 8/16-bit" : "Mono 8-bit");
            const outputPath = stats.path || "";
            const outputName = pathBaseName(outputPath);
            const sourceName = pathBaseName(path);
            const throughput = totalTime > 0 ? (frameCount / totalTime).toFixed(1) : "0.0";

            const reportHtml = `
                    <div style="text-align:left; font-family:'JetBrains Mono', 'Consolas', monospace; font-size:0.84rem; color:#94a3b8; line-height:1.45;">
                        <div style="padding: 12px; background: rgba(15,23,42,0.75); border-radius: 10px; border: 1px solid rgba(245,158,11,.28); margin-bottom: 12px;">
                            <div style="display:flex; justify-content:space-between; gap:14px; margin-bottom:6px;">
                                <span style="color:#64748b; text-transform:uppercase;">${tr("converter.report_profile", "Perfil SER")}</span>
                                <strong style="color:#fbbf24; text-align:right;">${escapeHtml(profileLabel)}</strong>
                            </div>
                            <div style="display:flex; justify-content:space-between; gap:14px;">
                                <span style="color:#64748b; text-transform:uppercase;">${tr("converter.report_time", "Tiempo")}</span>
                                <strong style="color:#fbbf24;">${totalTime.toFixed(1)}s</strong>
                            </div>
                        </div>

                        <div style="background:rgba(255,255,255,0.03); padding:10px 12px; border:1px solid rgba(255,255,255,0.08); border-radius:10px; margin-bottom:10px;">
                            <div style="display:flex; justify-content:space-between; gap:14px; margin-bottom:4px;">
                                <span style="color:#64748b;">${tr("converter.report_resolution", "Resolución")}:</span>
                                <strong style="color:#e2e8f0;">${stats.width || "-"} x ${stats.height || "-"}</strong>
                            </div>
                            <div style="display:flex; justify-content:space-between; gap:14px; margin-bottom:4px;">
                                <span style="color:#64748b;">${tr("converter.report_frames", "Frames")}:</span>
                                <strong style="color:#e2e8f0;">${frameCount}</strong>
                            </div>
                            <div style="display:flex; justify-content:space-between; gap:14px; margin-bottom:4px;">
                                <span style="color:#64748b;">${tr("converter.report_depth", "Profundidad")}:</span>
                                <strong style="color:#38bdf8;">${stats.bit_depth || "-"}-bit · CID ${stats.color_id ?? "-"}</strong>
                            </div>
                            <div style="display:flex; justify-content:space-between; gap:14px; margin-bottom:4px;">
                                <span style="color:#64748b;">${tr("converter.report_frame_size", "Tamaño/frame")}:</span>
                                <strong style="color:#a3e635;">${formatBytes(stats.bytes_per_frame)}</strong>
                            </div>
                            <div style="display:flex; justify-content:space-between; gap:14px; margin-bottom:4px;">
                                <span style="color:#64748b;">${tr("converter.report_estimated", "Estimado")}:</span>
                                <strong style="color:#fbbf24;">${formatBytes(estimatedSize)}</strong>
                            </div>
                            <div style="display:flex; justify-content:space-between; gap:14px; margin-bottom:4px;">
                                <span style="color:#64748b;">${tr("converter.report_final_size", "Tamaño final")}:</span>
                                <strong style="color:#a3e635;">${formatBytes(finalSize)}</strong>
                            </div>
                            <div style="display:flex; justify-content:space-between; gap:14px;">
                                <span style="color:#64748b;">${tr("converter.report_throughput", "Rendimiento")}:</span>
                                <strong style="color:#e2e8f0;">${throughput} FPS</strong>
                            </div>
                        </div>

                        <div style="font-size:0.72rem; color:#64748b; word-break:break-all;">
                            <div><strong style="color:#94a3b8;">${tr("converter.report_source", "Fuente")}:</strong> ${escapeHtml(sourceName)}</div>
                            <div><strong style="color:#94a3b8;">${tr("converter.report_output", "Salida")}:</strong> ${escapeHtml(outputName)}</div>
                            <div style="margin-top:4px;">${escapeHtml(outputPath)}</div>
                        </div>
                    </div>
                `;

            const openFolder = await showCustomChoice(
                tr("converter.success_title", "Conversión SER completada"),
                reportHtml,
                tr("converter.open_folder", "Abrir carpeta"),
                tr("general.acknowledge", "Entendido")
            );

            if (openFolder && outputPath) {
                try {
                    await open(parentFolder(outputPath));
                } catch (openErr) {
                    console.warn("Could not open converted SER folder:", openErr);
                }
            }

        } catch (e) {
            const errStr = typeof e === 'string' ? e : JSON.stringify(e);
            log("ERROR", "Convert: " + errStr);
            console.error(e);
            showCustomAlert(tr("converter.error_title", "Error de conversión"), errStr);
        } finally {
            if (unlisten) unlisten();
            hideProcessing();
            if (cancelBtn) {
                cancelBtn.style.display = previousCancelDisplay;
            }
            if (conversionStarted) {
                btnConvert.disabled = false;
                btnConvert.style.opacity = "";
                btnConvert.style.cursor = "";
            }
        }
    });



    // Note: showCustomChoice is used in main.js (line 352), so it is available.
})();


// =========================================================================
// MANUAL ANCHOR LOGIC (SURFACE)
// =========================================================================

function drawManualAnchor() {
    if (!ui.gridOverlay || !manualAnchorPoint || !ui.imgSource) return;

    // FIX: Sync Canvas Dimensions and Style to match visual image exactly
    // This ensures the drawing aligns with the natural image coordinates
    if (ui.gridOverlay.width !== ui.imgSource.naturalWidth || ui.gridOverlay.height !== ui.imgSource.naturalHeight) {
        ui.gridOverlay.width = ui.imgSource.naturalWidth;
        ui.gridOverlay.height = ui.imgSource.naturalHeight;
    }

    // Ensure CSS size matches internal buffer size to maintain 1:1 pixel mapping in the viewport
    ui.gridOverlay.style.width = ui.gridOverlay.width + "px";
    ui.gridOverlay.style.height = ui.gridOverlay.height + "px";
    ui.gridOverlay.style.left = ui.imgSource.offsetLeft + "px";
    ui.gridOverlay.style.top = ui.imgSource.offsetTop + "px";

    const ctx = ui.gridOverlay.getContext('2d');

    const x = manualAnchorPoint.x;
    const y = manualAnchorPoint.y;
    const w = ui.imgSource.naturalWidth;
    const h = ui.imgSource.naturalHeight;

    // Calculate Reference Box Size (Matching Backend Logic)
    // backend: 384.min(w/2).min(h/2) approximate
    const refSize = Math.min(384, w / 2, h / 2);
    const boxHalf = refSize / 2;

    ctx.save();

    // 1. Draw The "Effective" Anchor Box (Yellow dashed)
    // This shows the user exactly what area will be used for tracking
    ctx.strokeStyle = "rgba(250, 204, 21, 0.8)"; // Yellow-400
    ctx.lineWidth = 2;
    ctx.setLineDash([5, 5]);
    ctx.strokeRect(x - boxHalf, y - boxHalf, refSize, refSize);

    // 2. Draw The Center Point (Red Crosshair)
    ctx.setLineDash([]);
    ctx.strokeStyle = "#ef4444"; // Red-500
    ctx.lineWidth = 3;
    ctx.beginPath();

    const crossSize = 20;
    ctx.moveTo(x - crossSize, y);
    ctx.lineTo(x + crossSize, y);
    ctx.moveTo(x, y - crossSize);
    ctx.lineTo(x, y + crossSize);
    ctx.stroke();

    // 3. Label
    ctx.font = "bold 14px Sans-Serif";
    ctx.fillStyle = "#ef4444";
    ctx.fillText("ANCHOR", x + 12, y - 12);

    // 4. Box Label
    ctx.fillStyle = "rgba(250, 204, 21, 0.8)";
    ctx.font = "italic 12px Sans-Serif";
    ctx.fillText(`${Math.round(refSize)}px Ref`, x - boxHalf, y + boxHalf + 15);

    ctx.restore();
}

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

    // The box and the image are both inside .zoom-content, which has a CSS transform scale applied.
    // Therefore, inline CSS coordinates act as "natural" coordinates prior to the zoom scale.
    // We only need to account for any internal layout offsets the image might have inside .zoom-content.
    const offX = img.offsetLeft || 0;
    const offY = img.offsetTop || 0;

    box.style.left = (offX + stackingRoiSelection.x) + "px";
    box.style.top = (offY + stackingRoiSelection.y) + "px";
    box.style.width = stackingRoiSelection.w + "px";
    box.style.height = stackingRoiSelection.h + "px";
    box.style.display = "block";
}

function setupStackingRoiInteractions() {
    const btnRoi = document.getElementById("btn-stacking-roi");
    const container = document.getElementById("view-source").querySelector(".zoom-target-container");
    const boxDOM = document.getElementById("stacking-roi-box");

    if (!btnRoi || !container) return;

    btnRoi.addEventListener("click", () => {
        const img = document.getElementById("img-source");
        if (!img || !img.src || img.src.includes("data:image/gif;base64,R0lGODlhAQABAAD/ACwAAAAAAQABAAACADs=")) {
            showCustomAlert("Aviso", "Abre un video y reprodúcelo o analízalo para ver el cuadro base.");
            return;
        }

        isSettingStackingRoi = !isSettingStackingRoi;

        if (isSettingStackingRoi && typeof isSettingManualAnchor !== 'undefined' && isSettingManualAnchor) {
            // Turn off manual anchor visually and logically
            isSettingManualAnchor = false;
            const btnAnchor = document.getElementById("btn-manual-anchor");
            if (btnAnchor) {
                btnAnchor.classList.add("secondary");
                btnAnchor.classList.remove("primary");
                btnAnchor.style.background = "rgba(59, 130, 246, 0.15)";
                btnAnchor.style.color = "#93c5fd";
                btnAnchor.innerHTML = `<svg class="zas-icon" style="width:14px; height:14px; fill:none; stroke:currentColor;"><use href="#icon-anchor"></use></svg><span>Definir Anclaje Manual</span>`;
            }
        }

        if (isSettingStackingRoi) {
            btnRoi.classList.remove("secondary");
            btnRoi.classList.add("primary");
            btnRoi.style.background = "#10b981";
            btnRoi.style.color = "white";
            btnRoi.innerHTML = `<svg class="zas-icon" style="width:14px; height:14px; fill:none; stroke:currentColor;"><use href="#icon-check"></use></svg><span>Área Definida (Ocultar)</span>`;

            // Show the ROI clear button
            const clearRoiBtn = document.getElementById("btn-clear-roi");
            if (clearRoiBtn) clearRoiBtn.style.display = "flex";

            if (!stackingRoiSelection) {
                // Default to 100% full image size
                stackingRoiSelection = {
                    x: 0,
                    y: 0,
                    w: img.naturalWidth,
                    h: img.naturalHeight
                };
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
            btnRoi.innerHTML = `<svg class="zas-icon" style="width:14px; height:14px; fill:none; stroke:currentColor;"><use href="#icon-ruler"></use></svg><span>Definir Área de Apilado</span>`;
            boxDOM.style.display = "none";
            // Hide the clear button when ROI is deactivated via main button
            const clearRoiBtn2 = document.getElementById("btn-clear-roi");
            if (clearRoiBtn2) clearRoiBtn2.style.display = "none";
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
                if (newTop > oldB - 20) newTop = oldB - 20; // limit small size
                stackingRoiSelection.y = newTop;
                stackingRoiSelection.h = oldB - newTop;
            }
            if (stackingRoiResizeDir.includes("s")) {
                let newBottom = curY;
                if (newBottom < oldY + 20) newBottom = oldY + 20;
                // Previene extender mas alla del fondo
                stackingRoiSelection.h = Math.min(imgH - oldY, newBottom - oldY);
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
                // Previene extender mas alla del borde derecho
                stackingRoiSelection.w = Math.min(imgW - oldX, newRight - oldX);
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

    // Update DOM automatically on zoom or pan
    container.addEventListener("wheel", () => {
        if (isSettingStackingRoi) updateStackingRoiDOM();
    });
}

function setupManualAnchorInteractions() {
    const btnAnchor = document.getElementById("btn-manual-anchor");
    const container = document.getElementById("view-source");

    if (!container) return;

    // Toggle Mode Button
    if (btnAnchor) {
        btnAnchor.addEventListener("click", () => {
            // Check if image handles "click"
            if (!ui.imgSource || !ui.imgSource.src || ui.imgSource.src.includes("data:image/gif;base64,R0lGODlhAQABAAD/ACwAAAAAAQABAAACADs=")) {
                showCustomAlert("Aviso", "Carga una imagen o video primero.");
                return;
            }

            isSettingManualAnchor = !isSettingManualAnchor;

            if (isSettingManualAnchor && typeof isSettingStackingRoi !== 'undefined' && isSettingStackingRoi) {
                isSettingStackingRoi = false;
                const btnRoi = document.getElementById("btn-stacking-roi");
                if (btnRoi) {
                    btnRoi.classList.add("secondary");
                    btnRoi.classList.remove("primary");
                    btnRoi.style.background = "rgba(16, 185, 129, 0.15)";
                    btnRoi.style.color = "#6ee7b7";
                    btnRoi.innerHTML = `<svg class="zas-icon" style="width:14px; height:14px; fill:none; stroke:currentColor;"><use href="#icon-ruler"></use></svg><span>Definir Área de Apilado</span>`;
                }
                const roiBox = document.getElementById("stacking-roi-box");
                if (roiBox) roiBox.style.display = "none";
            }

            if (isSettingManualAnchor) {
                // ACTIVE
                btnAnchor.classList.remove("secondary");
                btnAnchor.classList.add("primary"); // Highlight
                btnAnchor.style.background = "#3b82f6";
                btnAnchor.style.color = "white";
                btnAnchor.innerHTML = `<svg class="zas-icon" style="width:14px; height:14px; fill:none; stroke:currentColor;"><use href="#icon-anchor"></use></svg><span>Haz Clic en la Imagen (Cancelar)</span>`;

                // Update Cursor
                const zoomContainer = container.querySelector(".zoom-target-container");
                if (zoomContainer) zoomContainer.style.cursor = "crosshair";

                showCustomAlert("Modo de Anclaje", "Haz clic sobre un rasgo prominente en la imagen (mancha, crater) para definir el punto de anclaje.");
            } else {
                // INACTIVE
                resetAnchorBtnState(btnAnchor);
                const zoomContainer = container.querySelector(".zoom-target-container");
                if (zoomContainer) zoomContainer.style.cursor = "grab";
            }
        });
    }

    function resetAnchorBtnState(btn) {
        if (!btn) return;
        isSettingManualAnchor = false;
        btn.classList.add("secondary");
        btn.classList.remove("primary");
        btn.style.background = "rgba(59, 130, 246, 0.15)";
        btn.style.color = "#93c5fd";
        btn.innerHTML = `<svg class="zas-icon" style="width:14px; height:14px; fill:none; stroke:currentColor;"><use href="#icon-anchor"></use></svg><span>Definir Anclaje Manual</span>`;
    }

    // Helper: Local Coordinates Calculation (Robust)
    function getLocalCoordinates(e, zoomContainer) {
        if (!zoomContainer) return { x: 0, y: 0 };

        // 1. Get container rect
        const rect = zoomContainer.getBoundingClientRect();

        // 2. Calculate Click Position relative to Container
        const clickX = e.clientX - rect.left;
        const clickY = e.clientY - rect.top;

        // 3. Get Image and Transforms (if available) to map back to natural coords
        // Usually, zoomContainer contains the image directly or via content div.
        // Assuming zoom-content logic:
        const img = ui.imgSource;
        if (!img) return { x: 0, y: 0 };

        // If we are using standard Zoom/Pan implementation:
        // transforms are usually applied to .zoom-content
        // But getLocalCoordinates usually implies we want coordinates on the *image element* itself.
        // If the click is on the container, we need to account for transform.

        // However, robust way:
        const imgRect = img.getBoundingClientRect();
        const relX = e.clientX - imgRect.left;
        const relY = e.clientY - imgRect.top;

        const scaleX = img.naturalWidth / imgRect.width;
        const scaleY = img.naturalHeight / imgRect.height;

        return {
            x: relX * scaleX,
            y: relY * scaleY
        };
    }

    // Image Click Handler
    // We use "mousedown" to catch it before drag logic if needed, 
    // or we can use a dedicated click handler.
    // Reusing existing listener logic pattern.
    container.addEventListener("mousedown", (e) => {
        // Condition: Either Shift+Click OR isSettingManualAnchor mode active
        const allowClick = isSettingManualAnchor && e.button === 0;

        if (allowClick) {
            // Check bounds
            if (!ui.imgSource || !ui.imgSource.naturalWidth) return;

            e.stopPropagation();
            e.preventDefault();

            const zoomContainer = container.querySelector(".zoom-target-container");
            if (!zoomContainer) return;

            const coords = getLocalCoordinates(e, zoomContainer);

            // Bounds Check
            if (coords.x < 0 || coords.y < 0 || coords.x > ui.imgSource.naturalWidth || coords.y > ui.imgSource.naturalHeight) {
                return;
            }

            manualAnchorPoint = coords;
            updateStackButtonState();
            resetWorkflowForSettingsChange(); // FIX: Force re-analysis when anchor is placed to generate proper cache

            // Show the clear-anchor button now that an anchor is set
            const clearAnchorBtn = document.getElementById("btn-clear-anchor");
            if (clearAnchorBtn) clearAnchorBtn.style.display = "flex";

            // Visual Feedback
            const ctx = ui.gridOverlay.getContext('2d');
            ctx.clearRect(0, 0, ui.gridOverlay.width, ui.gridOverlay.height);

            if (typeof drawGrid === "function" && activeAPoints && activeAPoints.length > 0) {
                drawGrid(activeAPoints, ui.imgSource.naturalWidth, ui.imgSource.naturalHeight);
            }

            drawManualAnchor();

            log("INFO", `Anclaje fijado: ${Math.round(coords.x)}, ${Math.round(coords.y)}`);

            // Confirm & Exit Mode if active
            if (isSettingManualAnchor && btnAnchor) {
                resetAnchorBtnState(btnAnchor);
                if (zoomContainer) zoomContainer.style.cursor = "grab";
                // Optional: Sound or Flash? 
            }
        }
    });

    // Escape Key Listener
    window.addEventListener("keydown", (e) => {
        if (e.key === "Escape" && isSettingManualAnchor && btnAnchor) {
            resetAnchorBtnState(btnAnchor);
            const zoomContainer = container.querySelector(".zoom-target-container");
            if (zoomContainer) zoomContainer.style.cursor = "grab";
        }
    });
}

// Init
setupStackingRoiInteractions();
setupManualAnchorInteractions();

// Sync Analysis Mode (UI Consolidation)
(function () {
    const selQuality = document.getElementById('sel-quality-method');
    const alignModeSelect = document.getElementById('align-mode');
    const mpWrapper = document.getElementById('multipoint-wrapper');

    if (selQuality && alignModeSelect) {
        const updateVisibility = () => {
            if (selQuality.value !== ZENITH_ULTIMATE_VALUE) {
                selQuality.value = ZENITH_ULTIMATE_VALUE;
            }
            applyZenithUltimateFlow();
            const flow = getActiveZenithFlow();

            if (mpWrapper) {
                mpWrapper.style.display = flow.needsPoints ? 'block' : 'none';
            }

            // Sync warning or other internal states
            alignModeSelect.dispatchEvent(new Event('change'));
        };

        alignModeSelect.addEventListener('change', () => {
            const panel = document.getElementById('panel-elite-settings');
            if (panel) {
                panel.style.display = (alignModeSelect.value === 'elite_v4') ? 'block' : 'none';
            }
        });

        // Initial sync on load or after analysis reset
        setTimeout(updateVisibility, 500);
    }
})();

// =========================================================================
// MANUAL ANCHOR + STACKING ROI — CLEAR HELPERS
// =========================================================================

/**
 * Fully clears the manual anchor:
 *   - Nulls manualAnchorPoint (backend will receive null on next stack call)
 *   - Clears the canvas gridOverlay visual drawing
 *   - Redraws any alignment grid points (without the anchor)
 *   - Resets the anchor button to its default state
 *   - Hides the clear button
 */
window._clearManualAnchor = function () {
    // 1. Clear data (backend state)
    manualAnchorPoint = null;
    if (typeof isSettingManualAnchor !== "undefined") isSettingManualAnchor = false;

    // 2. Clear canvas overlay — remove the anchor visual
    if (ui.gridOverlay) {
        const ctx = ui.gridOverlay.getContext("2d");
        ctx.clearRect(0, 0, ui.gridOverlay.width, ui.gridOverlay.height);
        // Re-draw grid points only (without anchor)
        if (typeof drawGrid === "function" && typeof activeAPoints !== "undefined" && activeAPoints && activeAPoints.length > 0 && ui.imgSource) {
            drawGrid(activeAPoints, ui.imgSource.naturalWidth, ui.imgSource.naturalHeight);
        }
    }

    // 3. Reset cursor
    const viewSrc = document.getElementById("view-source");
    if (viewSrc) {
        const zc = viewSrc.querySelector(".zoom-target-container");
        if (zc) zc.style.cursor = "grab";
    }

    // 4. Reset anchor button visual state
    const btn = document.getElementById("btn-manual-anchor");
    if (btn) {
        btn.className = "secondary";
        btn.style.removeProperty("background");
        btn.style.removeProperty("color");
        btn.style.removeProperty("border");
        btn.style.background = "rgba(59, 130, 246, 0.15)";
        btn.style.border = "1px solid rgba(59, 130, 246, 0.3)";
        btn.style.color = "#93c5fd";
        btn.innerHTML = `<svg class="zas-icon" style="width:14px; height:14px; fill:none; stroke:currentColor;"><use href="#icon-anchor"></use></svg><span>Definir Anclaje Manual</span>`;
    }

    // 5. Hide clear button
    const clearBtn = document.getElementById("btn-clear-anchor");
    if (clearBtn) clearBtn.style.display = "none";

    // 6. Invalidate backend cache so the next analysis does not use the old anchor
    if (typeof resetWorkflowForSettingsChange === "function") {
        resetWorkflowForSettingsChange();
    }

    log("INFO", "Anclaje Manual eliminado.");
};

/**
 * Fully clears the stacking ROI:
 *   - Nulls stackingRoiSelection and sets isSettingStackingRoi=false (backend receives null)
 *   - Hides the ROI selection box from DOM
 *   - Resets the ROI button state
 *   - Hides the clear button
 */
window._clearStackingRoi = function () {
    // 1. Clear data (backend state)
    if (typeof stackingRoiSelection !== "undefined") stackingRoiSelection = null;
    if (typeof isSettingStackingRoi !== "undefined") isSettingStackingRoi = false;

    // 2. Hide ROI box DOM element
    const roiBox = document.getElementById("stacking-roi-box");
    if (roiBox) roiBox.style.display = "none";

    // 3. Reset ROI button state
    const btn = document.getElementById("btn-stacking-roi");
    if (btn) {
        btn.className = "secondary";
        btn.style.removeProperty("background");
        btn.style.removeProperty("color");
        btn.style.removeProperty("border");
        btn.style.background = "rgba(16, 185, 129, 0.15)";
        btn.style.border = "1px solid rgba(16, 185, 129, 0.3)";
        btn.style.color = "#6ee7b7";
        btn.innerHTML = `<svg class="zas-icon" style="width:14px; height:14px; fill:none; stroke:currentColor;"><use href="#icon-ruler"></use></svg><span>Definir Área de Apilado</span>`;
    }

    // 4. Hide clear button
    const clearBtn = document.getElementById("btn-clear-roi");
    if (clearBtn) clearBtn.style.display = "none";

    // 5. Invalidate backend cache
    if (typeof resetWorkflowForSettingsChange === "function") {
        resetWorkflowForSettingsChange();
    }

    log("INFO", "Área de Apilado eliminada.");
};

// Wire up the dedicated clear buttons
(function wireClearButtons() {
    const btnClearAnchor = document.getElementById("btn-clear-anchor");
    if (btnClearAnchor) {
        btnClearAnchor.addEventListener("click", function (e) {
            e.stopPropagation();
            window._clearManualAnchor();
        });
    }

    const btnClearRoi = document.getElementById("btn-clear-roi");
    if (btnClearRoi) {
        btnClearRoi.addEventListener("click", function (e) {
            e.stopPropagation();
            window._clearStackingRoi();
        });
    }
})();
