import {
  MILKY_WAY_MODES,
  MILKY_WAY_STEPS,
  buildMilkyWayRequest,
  createMilkyWayState,
  setMilkyWayStep,
  validateMilkyWayState,
} from "./milky_way_state.js";

const FIXTURE_IMAGE = "/benchmarks/design-references/milky-way-nightscape-fixture.png";
const DEFAULT_MASK_HORIZON = [[0,.64],[.22,.61],[.5,.68],[.78,.59],[1,.63]];

function esc(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

function icon(name) {
  return `<svg class="zas-icon" aria-hidden="true"><use href="#icon-${name}"></use></svg>`;
}

function modeCard(mode, current, title, description, iconName) {
  return `<button type="button" class="mw-choice-card" data-mw-mode="${mode}" aria-pressed="${mode === current}">
    ${icon(iconName)}<span><strong>${esc(title)}</strong><small>${esc(description)}</small></span>
  </button>`;
}

function productCard(kind, title, description, state = "ready", readyLabel = "Lista", reviewLabel = "Revisar") {
  return `<article class="mw-product" data-state="${state}">
    ${icon(kind === "sky" ? "galaxy" : kind === "ground" ? "moon" : "mosaic")}
    <span><strong>${esc(title)}</strong><small>${esc(description)}</small></span>
    <b>${esc(state === "ready" ? readyLabel : reviewLabel)}</b>
  </article>`;
}

export function summarizeMilkyWayRegistrationAnalysis(analysis = {}) {
  const frames = Array.isArray(analysis?.frames) ? analysis.frames : [];
  const warnings = Array.isArray(analysis?.warnings)
    ? analysis.warnings.map(String).filter(Boolean)
    : [];
  const exclusions = [];
  for (const frame of frames) {
    for (const branch of ["sky", "ground"]) {
      const report = frame?.[branch];
      if (report?.excluded === true) {
        exclusions.push({
          path: String(frame?.path || ""),
          branch,
          reason: String(report.reason || ""),
        });
      }
    }
  }
  const messages = [...new Set([
    ...warnings,
    ...exclusions.map(item => item.reason).filter(Boolean),
  ])];
  return {
    frames,
    warnings,
    exclusions,
    excludedFrames: new Set(exclusions.map(item => item.path)).size,
    messages,
    degraded: analysis?.degraded === true || messages.length > 0,
  };
}

export function summarizeMilkyWayStackResult(stackResult = {}) {
  const registration = summarizeMilkyWayRegistrationAnalysis(stackResult);
  const warnings = Array.isArray(stackResult?.warnings)
    ? stackResult.warnings.map(String).filter(Boolean)
    : [];
  const messages = [...new Set([...warnings, ...registration.messages])];
  // The native engine is the authority for scientific eligibility.  Some
  // warnings are informational (for example, optional darks were not
  // supplied) and must stay visible without falsely relabelling a valid
  // product as non-scientific.  Explicit exclusions still make the review
  // degraded even when an older backend omitted the `scientific` flag.
  const scientific = stackResult?.scientific !== false;
  return {
    ...registration,
    warnings,
    messages,
    scientific,
    degraded: !scientific || registration.excludedFrames > 0,
  };
}

export function normalizeMilkyWayMaskDetection(detected = {}) {
  const rawPreview = String(detected?.previewPngBase64 || "").trim();
  return {
    confidence: Number.isFinite(Number(detected?.confidence))
      ? Math.max(0, Math.min(1, Number(detected.confidence)))
      : 0,
    skyFraction: Number.isFinite(Number(detected?.skyFraction))
      ? Math.max(0, Math.min(1, Number(detected.skyFraction)))
      : null,
    warnings: Array.isArray(detected?.warnings)
      ? detected.warnings.map(String).filter(Boolean)
      : [],
    preview: rawPreview
      ? rawPreview.startsWith("data:") ? rawPreview : `data:image/png;base64,${rawPreview}`
      : "",
  };
}

const MILKY_WAY_PROGRESS_PHASES = Object.freeze([
  ["preflight", /^Preflight$/i],
  ["calibration", /^Calibraci[oó]n lineal$/i],
  ["registration", /^Registro(?: cielo\/suelo)?$/i],
  ["final_registration", /^Registro final de /i],
  ["integration", /^Integraci[oó]n robusta de /i],
  ["publication", /^Publicaci[oó]n transaccional$/i],
  ["crop", /^Recorte sincronizado$/i],
  ["complete", /^Completado$/i],
]);

function milkyWayProgressPhaseCode(value) {
  const phase = String(value || "");
  return MILKY_WAY_PROGRESS_PHASES.find(([, matcher]) => matcher.test(phase))?.[0] || "processing";
}

export function normalizeMilkyWayProgress(payload = {}, { startedAtMs = 0, nowMs = Date.now() } = {}) {
  const value = Number(payload?.progress);
  const progress = Number.isFinite(value) ? Math.max(0, Math.min(100, value)) : 0;
  const doneValue = Number(payload?.itemsDone);
  const totalValue = Number(payload?.itemsTotal);
  const itemsDone = Number.isFinite(doneValue) ? Math.max(0, Math.trunc(doneValue)) : 0;
  const itemsTotal = Number.isFinite(totalValue) ? Math.max(0, Math.trunc(totalValue)) : 0;
  const fraction = itemsTotal > 0 && itemsDone > 0
    ? Math.min(1, itemsDone / itemsTotal)
    : progress > 0 ? progress / 100 : 0;
  const elapsedSeconds = startedAtMs > 0 ? Math.max(0, (nowMs - startedAtMs) / 1000) : 0;
  const etaSeconds = fraction > 0 && fraction < 1 && elapsedSeconds > 0
    ? elapsedSeconds * (1 - fraction) / fraction
    : null;
  return {
    ...payload,
    jobId: String(payload?.jobId || ""),
    phase: String(payload?.phase || ""),
    phaseCode: String(payload?.phaseCode || milkyWayProgressPhaseCode(payload?.phase)),
    detail: String(payload?.detail || ""),
    progress,
    itemsDone,
    itemsTotal,
    etaSeconds,
  };
}

export function summarizeMilkyWayPreflight(plan = {}) {
  const working = Number(plan?.estimatedWorkingBytes);
  const output = Number(plan?.estimatedOutputBytes);
  return {
    jobId: String(plan?.jobId || ""),
    width: Number.isFinite(Number(plan?.width)) ? Math.max(0, Math.trunc(Number(plan.width))) : 0,
    height: Number.isFinite(Number(plan?.height)) ? Math.max(0, Math.trunc(Number(plan.height))) : 0,
    estimatedWorkingBytes: Number.isFinite(working) ? Math.max(0, working) : 0,
    estimatedOutputBytes: Number.isFinite(output) ? Math.max(0, output) : 0,
    warnings: Array.isArray(plan?.warnings) ? plan.warnings.map(String).filter(Boolean) : [],
    products: Array.isArray(plan?.products) ? plan.products.map(String).filter(Boolean) : [],
  };
}

function formatBytes(value) {
  const bytes = Math.max(0, Number(value) || 0);
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let amount = bytes;
  let unit = -1;
  do { amount /= 1024; unit += 1; } while (amount >= 1024 && unit < units.length - 1);
  return `${amount >= 10 ? amount.toFixed(0) : amount.toFixed(1)} ${units[unit]}`;
}

function formatDuration(value) {
  if (!Number.isFinite(value) || value < 0) return "";
  const seconds = Math.max(0, Math.round(value));
  const minutes = Math.floor(seconds / 60);
  const remainder = seconds % 60;
  return minutes ? `${minutes}:${String(remainder).padStart(2, "0")}` : `${remainder}s`;
}

function fixtureState(step = 0) {
  const lights = Array.from({ length: 12 }, (_, index) => ({
    path: `/fixture/milky-way-${String(index + 1).padStart(2, "0")}.fits`,
    name: `MilkyWay_${String(index + 1).padStart(2, "0")}.fits`,
    width: 6000,
    height: 4000,
    channels: 3,
    exposureSeconds: 10,
    iso: 3200,
    timestampUnix: 1785643200 + index * 12,
  }));
  return createMilkyWayState({
    lights,
    baseFramePath: lights[5].path,
    outputDirectory: "/APILADOS/Via_Lactea_2026-08-02",
    activeStep: step,
    mask: {
      strategy: "auto",
      confidence: 0.93,
      userConfirmed: true,
      featherPx: 54,
      horizonPoints: [[0, 0.64], [0.23, 0.61], [0.51, 0.69], [0.76, 0.58], [1, 0.63]],
    },
    registration: {
      model: "radialWide",
      skySolved: true,
      groundSolved: true,
      inliers: 184,
      rmsPx: 0.72,
      cornerResidualPx: 1.18,
      singleResample: true,
    },
  });
}

export function initMilkyWayFlow({
  invoke,
  listenEvent,
  openDialog,
  openEditor,
  translate,
  translateProgress,
  bindLauncher = true,
} = {}) {
  const t = (key, fallback) => translate?.(key, fallback) || fallback;
  const localizeProgressText = value => translateProgress?.(String(value || "")) || String(value || "");
  const progressPhase = value => {
    const fallback = localizeProgressText(value?.phase) || t("milkyway.processing", "Procesando");
    return t(`milkyway.progress_phase_${value?.phaseCode || "processing"}`, fallback);
  };
  const stepLabel = (id, fallback) => t(`milkyway.step_${id}`, fallback);
  const reasonKeys = new Map([
    ["Añade al menos dos tomas del cielo nocturno.", "reason_two_lights"],
    ["Hay archivos incluidos que no se pueden leer; quítalos o desmárcalos.", "reason_unreadable_files"],
    ["Congelar suelo y rechazo robusto requieren al menos cuatro tomas.", "reason_four_lights"],
    ["Elige una toma base incluida en la sesión.", "reason_base_frame"],
    ["Las tomas incluidas no comparten geometría y canales.", "reason_geometry"],
    ["Las tomas cambian exposición o ISO/gain; activa Unificar exposición o separa el lote.", "reason_exposure"],
    ["Unificar exposición normalizará el lote y quedará registrado como concesión.", "reason_exposure_warning"],
    ["Los darks conocidos no coinciden con exposición, ISO/gain o geometría de los lights.", "reason_dark_mismatch"],
    ["Los flats conocidos no coinciden con ISO/gain, geometría o canales de los lights.", "reason_flat_mismatch"],
    ["Darks con metadatos incompletos: Strict exigirá validación nativa antes de calibrar.", "reason_dark_metadata"],
    ["Flats con metadatos incompletos: Strict exigirá validación nativa antes de calibrar.", "reason_flat_metadata"],
    ["Congelar suelo necesita una máscara cielo/suelo.", "reason_mask_required"],
    ["Confirma la máscara para proteger el horizonte y el primer plano.", "reason_mask_confirm"],
    ["La máscara automática tiene baja confianza; corrígela con horizonte o pincel.", "reason_mask_confidence"],
    ["El modelo de registro no es válido.", "reason_registration_model"],
    ["Falta validar el registro estelar del cielo.", "reason_registration_sky"],
    ["El registro no conserva suficientes estrellas de control.", "reason_registration_inliers"],
    ["El residuo del registro supera el límite configurado.", "reason_registration_rms"],
    ["Falta validar la rama fija del suelo.", "reason_registration_ground"],
    ["Las transformaciones deben componerse antes de un único remuestreo.", "reason_single_resample"],
    ["Usa rechazo sigma o Winsorized para suprimir aviones, satélites y píxeles dinámicos.", "reason_robust_integration"],
    ["AllowDegraded puede excluir tomas fallidas y marca el resultado como no científico.", "reason_allow_degraded"],
    ["La transición cielo/suelo debe estar entre 0 y 512 px.", "reason_transition_range"],
    ["Añade al menos una toma de primer plano o usa el suelo apilado/base.", "reason_foreground_frame"],
    ["Elige una carpeta de trabajo y salida.", "reason_output"],
    ["Elige al menos una salida o abre el editor.", "reason_publish"],
    ["Completa el paso anterior.", "reason_previous_step"],
  ]);
  const localizeReason = reason => {
    const key = reasonKeys.get(String(reason || ""));
    return key ? t(`milkyway.${key}`, reason) : String(reason || "");
  };
  let state = createMilkyWayState();
  let modal = null;
  let preview = "";
  let busy = false;
  let result = null;
  let inerted = [];
  let maskPointer = null;
  let maskDetection = normalizeMilkyWayMaskDetection();
  let showDetectedMask = false;
  let progress = null;
  let preflight = null;
  let activeJobId = "";
  let runStartedAtMs = 0;
  let closePrompt = false;
  let preserveOnNextOpen = false;
  let runInBackground = false;
  let removeProgressListener = null;

  const validation = () => validateMilkyWayState(state);

  function setBackgroundInert(enabled) {
    if (enabled) {
      inerted = [...document.body.children]
        .filter(element => element !== modal && element instanceof HTMLElement)
        .map(element => ({
          element,
          wasInert: element.inert,
          ariaHidden: element.getAttribute("aria-hidden"),
        }));
      inerted.forEach(({ element }) => {
        element.inert = true;
        element.setAttribute("aria-hidden", "true");
      });
    } else {
      inerted.forEach(({ element, wasInert, ariaHidden }) => {
        element.inert = wasInert;
        if (ariaHidden == null) element.removeAttribute("aria-hidden");
        else element.setAttribute("aria-hidden", ariaHidden);
      });
      inerted = [];
    }
  }

  function ensureModal() {
    if (modal) return modal;
    modal = document.createElement("div");
    modal.id = "milkyway-modal";
    modal.className = "mw-modal";
    modal.hidden = true;
    modal.setAttribute("role", "dialog");
    modal.setAttribute("aria-modal", "true");
    modal.setAttribute("aria-labelledby", "mw-title");
    document.body.appendChild(modal);
    modal.addEventListener("click", onClick);
    modal.addEventListener("change", onChange);
    modal.addEventListener("input", onInput);
    modal.addEventListener("keydown", onKeydown);
    modal.addEventListener("pointerdown", onMaskPointerDown);
    modal.addEventListener("pointermove", onMaskPointerMove);
    modal.addEventListener("pointerup", onMaskPointerUp);
    modal.addEventListener("pointercancel", onMaskPointerUp);
    return modal;
  }

  function navHtml(check) {
    return MILKY_WAY_STEPS.map(([id, label], index) => {
      const info = check.steps[index];
      const active = index === state.activeStep;
      // A regression in an upstream step must never trap the user: every step
      // already visited remains available for review, while future unreachable
      // steps are native-disabled and expose the first blocker in their name.
      const locked = !info.reachable && index > state.activeStep;
      const status = info.ready
        ? t("milkyway.state_ready", "Listo")
        : info.reachable
          ? t("milkyway.state_review", "Revisar")
          : t("milkyway.state_later", "Después");
      const blocker = locked
        ? check.steps.slice(0, index).find(step => !step.ready)?.reasons?.[0]
          || info.reasons?.[0]
          || t("milkyway.reason_previous_step", "Completa el paso anterior.")
        : "";
      const accessibleName = `${stepLabel(id, label)} · ${status}${blocker ? ` · ${localizeReason(blocker)}` : ""}`;
      return `<button type="button" class="mw-step${active ? " active" : ""}" data-mw-step="${index}"
        data-state="${info.ready ? "ready" : info.reachable ? "review" : "locked"}"
        aria-label="${esc(accessibleName)}" aria-disabled="${locked ? "true" : "false"}" ${locked ? "disabled" : ""}
        ${blocker ? `title="${esc(localizeReason(blocker))}"` : ""} ${active ? 'aria-current="step"' : ""}>
        <span>${index + 1}</span><b>${esc(stepLabel(id, label))}</b><small>${esc(status)}</small>
      </button>`;
    }).join("");
  }

  function dataPage(check) {
    const visibleMode = state.mode === MILKY_WAY_MODES.SEPARATE_LAYERS
      ? MILKY_WAY_MODES.FREEZE_GROUND
      : state.mode;
    const lightsMeta = state.mode === MILKY_WAY_MODES.SKY_ONLY
      ? t("milkyway.source_lights_meta_sky", "archivos · mínimo 2")
      : t("milkyway.source_lights_meta_robust", "archivos · mínimo robusto 4");
    const baseOptions = state.lights.filter(frame => frame.included !== false).map(frame =>
      `<option value="${esc(frame.path)}" ${frame.path === state.baseFramePath ? "selected" : ""}>${esc(frame.name)}</option>`,
    ).join("");
    const fileGroups = [
      ["lights", t("milkyway.source_lights", "Tomas nocturnas"), state.lights],
      ["darks", t("milkyway.source_darks", "Darks"), state.darks],
      ["flats", t("milkyway.source_flats", "Flats"), state.flats],
    ];
    const fileReview = fileGroups.map(([kind, label, frames]) => `<section class="mw-file-group">
      <header><b>${esc(label)}</b><span>${frames.length}</span></header>
      ${frames.length ? frames.map((frame, index) => {
        const isBase = kind === "lights" && frame.path === state.baseFramePath;
        return `<div class="mw-file-row" data-state="${frame.ok === false ? "error" : "ready"}">
          <input id="mw-${kind}-include-${index}" type="checkbox" data-mw-frame-kind="${kind}" data-mw-frame-index="${index}" ${frame.included !== false ? "checked" : ""} aria-label="${esc(t("milkyway.include_file", "Incluir archivo"))}: ${esc(frame.name)}">
          <label for="mw-${kind}-include-${index}" title="${esc(frame.error || frame.path)}"><span>${esc(frame.name)}</span>${isBase ? `<small>${esc(t("milkyway.base_badge", "Base"))}</small>` : ""}${frame.ok === false ? `<small>${esc(t("milkyway.unreadable_badge", "No legible"))}</small>` : ""}</label>
          <button id="mw-${kind}-remove-${index}" type="button" data-mw-action="remove-frame" data-mw-frame-kind="${kind}" data-mw-frame-index="${index}" aria-label="${esc(t("milkyway.remove_file", "Quitar archivo"))}: ${esc(frame.name)}">${esc(t("milkyway.remove", "Quitar"))}</button>
        </div>`;
      }).join("") : `<p>${esc(t("milkyway.no_files", "Sin archivos."))}</p>`}
    </section>`).join("");
    return `<section class="mw-page" data-page="data">
      <div class="mw-page-head"><span>${esc(t("milkyway.data_eyebrow", "01 · Fuente"))}</span><h3>${esc(t("milkyway.data_title", "¿Qué quieres conservar?"))}</h3><p>${esc(t("milkyway.data_body", "Zenith registra el cielo sin arrastrar el suelo. La opción recomendada conserva además capas separadas y una composición editable."))}</p></div>
      <div class="mw-mode-grid">
        ${modeCard(MILKY_WAY_MODES.FREEZE_GROUND, visibleMode, t("milkyway.mode_freeze_title", "Cielo + suelo nítidos"), t("milkyway.mode_freeze_body", "Compone el horizonte y conserva Cielo, Suelo, Máscara, Compuesto y mapas auditables."), "moon")}
        ${modeCard(MILKY_WAY_MODES.SKY_ONLY, visibleMode, t("milkyway.mode_sky_title", "Sólo cielo"), t("milkyway.mode_sky_body", "Todo el cuadro se registra con las estrellas; no crea una rama de suelo."), "galaxy")}
      </div>
      <div class="mw-source-grid">
        <article><span>${icon("sequence")}<b>${esc(t("milkyway.source_lights", "Tomas nocturnas"))}</b><small>${state.lights.length} ${esc(lightsMeta)}</small></span><button data-mw-action="add-lights">${esc(t("milkyway.add_lights", "Añadir tomas"))}</button></article>
        <article><span>${icon("moon")}<b>${esc(t("milkyway.source_darks", "Darks"))}</b><small>${state.darks.length} ${esc(t("milkyway.source_optional_meta", "archivos · opcional"))}</small></span><button data-mw-action="add-darks">${esc(t("milkyway.add_darks", "Añadir darks"))}</button></article>
        <article><span>${icon("lightbulb")}<b>${esc(t("milkyway.source_flats", "Flats"))}</b><small>${state.flats.length} ${esc(t("milkyway.source_flats_meta", "archivos · viñeteo/lente"))}</small></span><button data-mw-action="add-flats">${esc(t("milkyway.add_flats", "Añadir flats"))}</button></article>
      </div>
      <p class="mw-source-contract">${esc(t("milkyway.linear_input_notice", "Importa FITS/TIFF lineal. Revela los RAW de cámara fuera de Zenith hasta disponer de un lector RAW científicamente validado."))}</p>
      <details class="mw-file-review"><summary>${esc(t("milkyway.review_files", "Revisar archivos"))}<span>${state.lights.length + state.darks.length + state.flats.length}</span></summary><div class="mw-file-review-scroll">${fileReview}</div></details>
      <div class="mw-form-row"><label>${esc(t("milkyway.base_frame", "Toma base"))}<select data-mw-field="baseFramePath">${baseOptions || `<option value="">${esc(t("milkyway.add_lights_first", "Añade tomas primero"))}</option>`}</select></label>
        <label class="mw-check"><input type="checkbox" data-mw-field="unifyExposure" ${state.unifyExposure ? "checked" : ""}><span><b>${esc(t("milkyway.unify_exposure_title", "Unificar exposición"))}</b><small>${esc(t("milkyway.unify_exposure_body", "Sólo si ISO/gain o tiempo cambiaron; queda registrado."))}</small></span></label></div>
      ${issuesHtml(check.steps[0])}
    </section>`;
  }

  function maskPage(check) {
    const maskRequired = [MILKY_WAY_MODES.FREEZE_GROUND, MILKY_WAY_MODES.SEPARATE_LAYERS].includes(state.mode);
    const displayedPreview = showDetectedMask && maskDetection.preview
      ? maskDetection.preview
      : preview;
    const maskDetectionHtml = maskDetection.preview || maskDetection.warnings.length
      ? `<div class="mw-step-health" data-state="${maskDetection.warnings.length ? "warning" : "ready"}" role="status">
          <span>${icon(maskDetection.warnings.length ? "warning" : "check")} <b>${esc(t("milkyway.mask_backend_review", "Diagnóstico de máscara nativa"))}</b> · ${Math.round(maskDetection.confidence * 100)}%${Number.isFinite(maskDetection.skyFraction) ? ` · ${Math.round(maskDetection.skyFraction * 100)}% ${esc(t("milkyway.mask_sky_fraction", "de cielo"))}` : ""}</span>
          ${maskDetection.warnings.length ? `<details open><summary>${esc(t("milkyway.mask_backend_warnings", "Revisar advertencias de detección"))}</summary><ul class="mw-diagnostic-list">${maskDetection.warnings.map(message => `<li>${esc(message)}</li>`).join("")}</ul></details>` : ""}
        </div>`
      : "";
    if (!maskRequired) {
      return `<section class="mw-page" data-page="mask">
        <div class="mw-page-head"><span>${esc(t("milkyway.mask_eyebrow", "02 · Geometría visible"))}</span><h3>${esc(t("milkyway.mask_sky_only_title", "Todo el cuadro es cielo"))}</h3><p>${esc(t("milkyway.mask_sky_only_body", "Zenith registra cada píxel con las estrellas. No se solicita frontera ni confirmación de suelo en este modo."))}</p></div>
        <div class="mw-mask-stage mw-mask-stage-sky-only">
          ${preview ? `<img src="${esc(preview)}" alt="${esc(t("milkyway.mask_image_alt", "Toma base para definir cielo y suelo"))}">` : `<div class="mw-mask-empty">${esc(t("milkyway.mask_empty", "Añade tomas para crear la máscara."))}</div>`}
          <span class="mw-mask-legend"><i></i>${esc(t("milkyway.mask_sky_only_legend", "Cuadro completo · registro estelar"))}</span>
        </div>
        <div class="mw-step-health" data-state="ready" role="status"><span>${icon("galaxy")} ${esc(t("milkyway.mask_sky_only_ready", "No hay una frontera pendiente. Al volver a Cielo + suelo, Zenith pedirá detectar y confirmar una máscara nueva."))}</span></div>
        ${issuesHtml(check.steps[1])}
      </section>`;
    }
    return `<section class="mw-page" data-page="mask">
      <div class="mw-page-head"><span>${esc(t("milkyway.mask_eyebrow", "02 · Geometría visible"))}</span><h3>${esc(t("milkyway.mask_title", "Separa cielo y primer plano"))}</h3><p>${esc(t("milkyway.mask_body", "Auto propone la frontera. Corrige con horizonte o pincel y confirma: la misma máscara gobierna registro, composición, recorte y edición."))}</p></div>
      <div class="mw-mask-layout">
        <div class="mw-mask-stage" data-mw-mask-stage>
          ${displayedPreview ? `<img src="${esc(displayedPreview)}" alt="${esc(showDetectedMask ? t("milkyway.mask_native_preview_alt", "Máscara soft exacta calculada por el motor") : t("milkyway.mask_image_alt", "Toma base para definir cielo y suelo"))}">` : `<div class="mw-mask-empty">${esc(t("milkyway.mask_empty", "Añade tomas para crear la máscara."))}</div>`}
          <canvas data-mw-mask-canvas aria-label="${esc(t("milkyway.mask_canvas_aria", "Máscara editable cielo y suelo"))}"></canvas>
          <span class="mw-mask-legend"><i></i>${esc(t("milkyway.mask_legend_sky", "Cielo registrado"))} <i></i>${esc(t("milkyway.mask_legend_ground", "Suelo fijo"))}</span>
        </div>
        <div class="mw-mask-tools">
          <div class="mw-segmented" role="group" aria-label="${esc(t("milkyway.mask_method_aria", "Método de máscara"))}">
            ${[["auto",t("milkyway.mask_auto", "Auto")],["horizon",t("milkyway.mask_horizon", "Horizonte")],["brush",t("milkyway.mask_brush", "Pincel")]].map(([value,label]) => `<button type="button" data-mw-mask="${value}" aria-pressed="${state.mask.strategy === value}">${esc(label)}</button>`).join("")}
          </div>
          <div class="mw-segmented" role="group" aria-label="${esc(t("milkyway.mask_keyboard_aria", "Ajustes de máscara por teclado"))}">
            <button type="button" data-mw-action="mask-up">${esc(t("milkyway.mask_up", "Subir frontera"))}</button>
            <button type="button" data-mw-action="mask-down">${esc(t("milkyway.mask_down", "Bajar frontera"))}</button>
            <button type="button" data-mw-action="mask-undo" ${(state.mask.brushStrokes || []).length ? "" : "disabled"}>${esc(t("milkyway.mask_undo", "Deshacer trazo"))}</button>
            <button type="button" data-mw-action="mask-reset">${esc(t("milkyway.mask_reset", "Reiniciar"))}</button>
          </div>
          ${state.mask.strategy === "brush" ? `<div class="mw-segmented mw-brush-target" role="group" aria-label="${esc(t("milkyway.brush_zone_aria", "Zona que pinta el pincel"))}"><button type="button" data-mw-brush-target="sky" aria-pressed="${state.mask.brushTarget !== "ground"}">${esc(t("milkyway.brush_sky", "Pintar cielo"))}</button><button type="button" data-mw-brush-target="ground" aria-pressed="${state.mask.brushTarget === "ground"}">${esc(t("milkyway.brush_ground", "Proteger suelo"))}</button></div><label>${esc(t("milkyway.brush_radius", "Radio del pincel"))} <output>${state.mask.brushRadiusPx} px</output><input type="range" min="6" max="160" value="${state.mask.brushRadiusPx}" data-mw-field="mask.brushRadiusPx"></label>` : ""}
          <div class="mw-confidence" data-state="${state.mask.confidence >= .72 ? "ready" : "review"}"><b>${Math.round(state.mask.confidence * 100)}%</b><span>${esc(t("milkyway.mask_confidence", "Confianza de frontera"))}</span></div>
          <label>${esc(t("milkyway.mask_feather", "Suavizado del horizonte"))} <output>${state.mask.featherPx} px</output><input type="range" min="0" max="256" value="${state.mask.featherPx}" data-mw-field="mask.featherPx"></label>
          <button type="button" class="mw-secondary" data-mw-action="detect-mask">${icon("magic")} ${esc(t("milkyway.mask_detect_again", "Detectar otra vez"))}</button>
          ${maskDetection.preview ? `<button type="button" class="mw-secondary" data-mw-action="toggle-mask-preview" aria-pressed="${showDetectedMask}">${icon(showDetectedMask ? "sequence" : "mosaic")} ${esc(showDetectedMask ? t("milkyway.mask_show_frame", "Ver toma base") : t("milkyway.mask_show_native", "Ver máscara soft real"))}</button>` : ""}
          <label class="mw-check"><input type="checkbox" data-mw-field="mask.userConfirmed" ${state.mask.userConfirmed ? "checked" : ""} ${maskRequired ? "" : "disabled"}><span><b>${esc(t("milkyway.mask_confirm_title", "Confirmo la frontera"))}</b><small>${esc(t("milkyway.mask_confirm_body", "Evita dobles bordes, árboles fantasma y cielo sobre el suelo."))}</small></span></label>
        </div>
      </div>
      ${maskDetectionHtml}
      ${issuesHtml(check.steps[1])}
    </section>`;
  }

  function registrationPage(check) {
    const r = state.registration;
    const review = summarizeMilkyWayRegistrationAnalysis(state.analysis);
    const reviewHtml = review.frames.length || review.messages.length
      ? `<div class="mw-step-health" data-state="${review.degraded ? "warning" : "ready"}" role="status">
          <span>${icon(review.degraded ? "warning" : "check")} <b>${esc(review.degraded
            ? t("milkyway.registration_degraded", "Registro con exclusiones")
            : t("milkyway.registration_verified", "Registro verificado"))}</b>${review.excludedFrames
              ? ` · ${review.excludedFrames} ${esc(t("milkyway.excluded_frames", "toma(s) excluida(s)"))}`
              : ` · ${review.frames.length} ${esc(t("milkyway.frames_checked", "toma(s) revisada(s)"))}`}</span>
          ${review.messages.length ? `<details><summary>${esc(t("milkyway.registration_diagnostic", "Ver advertencias y exclusiones"))}</summary><ul class="mw-diagnostic-list">${review.messages.map(message => `<li>${esc(message)}</li>`).join("")}</ul></details>` : ""}
        </div>`
      : "";
    return `<section class="mw-page" data-page="registration">
      <div class="mw-page-head"><span>${esc(t("milkyway.registration_eyebrow", "03 · Alineación"))}</span><h3>${esc(t("milkyway.registration_title", "Una transformación, dos ramas"))}</h3><p>${esc(t("milkyway.registration_body", "El cielo usa estrellas; el suelo permanece en la toma base. Zenith compone distorsión y registro antes de remuestrear una sola vez."))}</p></div>
      <div class="mw-metric-grid">
        <article data-state="${r.skySolved ? "ready" : "review"}"><span>${icon("star")} ${esc(t("milkyway.registration_sky", "Registro cielo"))}</span><b>${r.inliers || "—"}</b><small>${esc(t("milkyway.control_stars", "estrellas de control"))}</small></article>
        <article data-state="${r.skySolved ? "ready" : "review"}"><span>${icon("anchor")} ${esc(t("milkyway.registration_rms", "Residuo RMS"))}</span><b>${Number.isFinite(r.rmsPx) ? `${r.rmsPx.toFixed(2)} px` : "—"}</b><small>${esc(t("milkyway.limit", "límite"))} ${r.maxRmsPx.toFixed(1)} px</small></article>
        <article data-state="${r.groundSolved || state.mode === MILKY_WAY_MODES.SKY_ONLY ? "ready" : "review"}"><span>${icon("moon")} ${esc(t("milkyway.product_ground", "Suelo"))}</span><b>${r.groundSolved ? esc(t("milkyway.fixed", "Fijo")) : "—"}</b><small>${esc(t("milkyway.independent_branch", "rama independiente"))}</small></article>
        <article data-state="${r.singleResample ? "ready" : "review"}"><span>${icon("mosaic")} ${esc(t("milkyway.resampling", "Remuestreo"))}</span><b>${r.singleResample ? "1×" : esc(t("milkyway.state_review", "Revisar"))}</b><small>${esc(t("milkyway.composed_transforms", "transformaciones compuestas"))}</small></article>
      </div>
      <div class="mw-form-row">
        <label>${esc(t("milkyway.model", "Modelo"))}<select data-mw-field="registration.model"><option value="auto" ${r.model === "auto" ? "selected" : ""}>${esc(t("milkyway.model_auto", "Auto medido"))}</option><option value="affine" ${r.model === "affine" ? "selected" : ""}>${esc(t("milkyway.model_affine", "Afín"))}</option><option value="homography" ${r.model === "homography" ? "selected" : ""}>${esc(t("milkyway.model_homography", "Homografía"))}</option><option value="radialWide" ${r.model === "radialWide" ? "selected" : ""}>${esc(t("milkyway.model_radial", "Ultra gran angular + radial"))}</option></select></label>
        <label>${esc(t("milkyway.distortion", "Distorsión"))}<select data-mw-field="registration.distortionCorrection"><option value="auto" ${r.distortionCorrection === "auto" ? "selected" : ""}>${esc(t("milkyway.option_auto", "Auto"))}</option><option value="lens" ${r.distortionCorrection === "lens" ? "selected" : ""}>${esc(t("milkyway.distortion_lens", "Lente simple"))}</option><option value="complex" ${r.distortionCorrection === "complex" ? "selected" : ""}>${esc(t("milkyway.distortion_complex", "Compleja / horizonte"))}</option><option value="off" ${r.distortionCorrection === "off" ? "selected" : ""} ${r.model === "radialWide" ? "disabled" : ""}>${esc(t("milkyway.option_off", "Desactivada"))}</option></select>${r.model === "radialWide" ? `<small>${esc(t("milkyway.radial_requires_distortion", "Ultra gran angular incluye corrección radial. Elige Auto, Afín u Homografía para desactivarla."))}</small>` : ""}</label>
      </div>
      <button class="mw-primary mw-inline" data-mw-action="analyze-registration">${icon("anchor")} ${esc(t("milkyway.analyze_registration", "Analizar registro"))}</button>
      ${reviewHtml}
      ${issuesHtml(check.steps[2])}
    </section>`;
  }

  function integrationPage(check) {
    const i = state.integration;
    return `<section class="mw-page" data-page="integration">
      <div class="mw-page-head"><span>${esc(t("milkyway.integration_eyebrow", "04 · Señal"))}</span><h3>${esc(t("milkyway.integration_title", "Integra sin borrar estrellas ni paisaje"))}</h3><p>${esc(t("milkyway.integration_body", "El rechazo se mide por rama. Aviones, satélites y píxeles calientes se excluyen del cielo sin convertir hojas o agua en manchas."))}</p></div>
      <div class="mw-preset-grid">
        <button data-mw-preset="auto" aria-pressed="${i.profile === "auto"}"><b>${esc(t("milkyway.preset_auto", "Auto Nightscape"))}</b><small>${esc(t("milkyway.preset_auto_body", "Winsorized adaptativo · recomendado"))}</small></button>
        <button data-mw-preset="quality" aria-pressed="${i.profile === "quality"}"><b>${esc(t("milkyway.preset_quality", "Máxima calidad"))}</b><small>${esc(t("milkyway.preset_quality_body", "Dos pasadas · bordes y ruido fino"))}</small></button>
        <button data-mw-preset="fast" aria-pressed="${i.profile === "fast"}"><b>${esc(t("milkyway.preset_fast", "Rápido"))}</b><small>${esc(t("milkyway.preset_fast_body", "Menor caché · misma geometría"))}</small></button>
      </div>
      <details class="mw-expert" ${state.presentationMode === "expert" ? "open" : ""}><summary>${esc(t("milkyway.expert_controls", "Controles expertos"))}</summary><div class="mw-form-grid">
        <label>${esc(t("milkyway.integration_label", "Integración"))}<select data-mw-field="integration.method"><option value="winsorized" ${i.method === "winsorized" ? "selected" : ""}>Winsorized sigma</option><option value="sigmaClip" ${i.method === "sigmaClip" ? "selected" : ""}>Sigma clipping</option><option value="mean" ${i.method === "mean" ? "selected" : ""}>${esc(t("milkyway.integration_mean", "Promedio sin rechazo"))}</option></select></label>
        <label>${esc(t("milkyway.normalization", "Normalización"))}<select data-mw-field="integration.normalization"><option value="robustLinear" ${i.normalization === "robustLinear" ? "selected" : ""}>${esc(t("milkyway.normalization_robust", "Lineal robusta"))}</option><option value="none" ${i.normalization === "none" ? "selected" : ""}>${esc(t("milkyway.normalization_none", "Sin normalizar"))}</option></select></label>
        <label class="mw-check"><input type="checkbox" data-mw-field="integration.dynamicHotPixels" ${i.dynamicHotPixels ? "checked" : ""}><span><b>${esc(t("milkyway.dynamic_pixels_title", "Píxeles dinámicos"))}</b><small>${esc(t("milkyway.dynamic_pixels_body", "Complementa darks, no los suplanta."))}</small></span></label>
        <label class="mw-check"><input type="checkbox" data-mw-field="integration.rejectTrails" ${i.rejectTrails ? "checked" : ""}><span><b>${esc(t("milkyway.reject_trails_title", "Rechazar trazas ajenas"))}</b><small>${esc(t("milkyway.reject_trails_body", "Aviones, satélites y meteoros opcionales."))}</small></span></label>
        ${state.presentationMode === "expert" ? `<label>${esc(t("milkyway.fallback_policy", "Política ante fallos"))}<select data-mw-field="fallbackPolicy"><option value="strict" ${state.fallbackPolicy === "strict" ? "selected" : ""}>${esc(t("milkyway.fallback_strict", "Estricto · detener"))}</option><option value="allowDegraded" ${state.fallbackPolicy === "allowDegraded" ? "selected" : ""}>${esc(t("milkyway.fallback_degraded", "Permitir degradado · excluir"))}</option></select></label>` : ""}
      </div></details>
      <div class="mw-step-health" data-state="${state.fallbackPolicy === "strict" ? "ready" : "warning"}" role="status"><span>${icon(state.fallbackPolicy === "strict" ? "check" : "warning")} <b>${esc(state.fallbackPolicy === "strict" ? t("milkyway.fallback_strict", "Estricto · detener") : t("milkyway.fallback_degraded", "Permitir degradado · excluir"))}</b> · ${esc(state.fallbackPolicy === "strict"
        ? t("milkyway.fallback_strict_body", "Una toma o rama no fiable detiene el trabajo antes de publicar.")
        : t("milkyway.fallback_degraded_body", "Zenith puede excluir tomas fallidas; cada concesión se muestra y el resultado se marca como no científico."))}</span></div>
      ${issuesHtml(check.steps[3])}
    </section>`;
  }

  function compositionPage(check) {
    const productReady = t("milkyway.product_ready", "Lista");
    const productReview = t("milkyway.state_review", "Revisar");
    const skyOnly = state.mode === MILKY_WAY_MODES.SKY_ONLY;
    return `<section class="mw-page" data-page="composition">
      <div class="mw-page-head"><span>${esc(t("milkyway.composition_eyebrow", "05 · Capas"))}</span><h3>${esc(t("milkyway.composition_title", "Compón sin costuras"))}</h3><p>${esc(t("milkyway.composition_body", "La máscara confirmada y una transición suave combinan las dos ramas. Zenith publica el compuesto junto con Cielo, Suelo y todos sus mapas para auditar o editar después."))}</p></div>
      <div class="mw-products">
        ${productCard("sky", t("milkyway.product_sky", "Cielo"), t("milkyway.product_sky_body", "Registro estelar, cobertura y rechazo"), "ready", productReady, productReview)}
        ${skyOnly ? "" : productCard("ground", t("milkyway.product_ground", "Suelo"), t("milkyway.product_ground_body", "Apilado fijo o toma base"), "ready", productReady, productReview)}
        ${skyOnly ? "" : productCard("composite", t("milkyway.product_composite", "Compuesto"), t("milkyway.product_composite_body", "Fusión lineal con borde auditable"), "ready", productReady, productReview)}
      </div>
      ${skyOnly ? `<div class="mw-step-health" data-state="ready"><span>${icon("galaxy")} ${esc(t("milkyway.sky_only_products_body", "Se publican el máster de cielo, máscara de cuadro completo, VAR, cobertura, rechazo y receta reproducible."))}</span></div>` : `<div class="mw-form-grid">
        <label>${esc(t("milkyway.ground_source", "Fuente del suelo"))}<select data-mw-field="composition.groundSource"><option value="stack" ${state.composition.groundSource === "stack" ? "selected" : ""}>${esc(t("milkyway.ground_source_stack", "Apilar suelo"))}</option><option value="base" ${state.composition.groundSource === "base" ? "selected" : ""}>${esc(t("milkyway.base_frame", "Toma base"))}</option></select></label>
        <label>${esc(t("milkyway.color_match", "Igualar color"))}<select data-mw-field="composition.colorMatch"><option value="boundaryAware" ${state.composition.colorMatch === "boundaryAware" ? "selected" : ""}>${esc(t("milkyway.color_match_boundary", "Sólo borde cielo/suelo"))}</option><option value="off" ${state.composition.colorMatch === "off" ? "selected" : ""}>${esc(t("milkyway.color_match_off", "Sin igualar"))}</option></select></label>
        <label>${esc(t("milkyway.transition", "Transición"))} <output>${state.composition.featherPx} px</output><input type="range" min="0" max="256" value="${state.composition.featherPx}" data-mw-field="composition.featherPx"></label>
      </div><div class="mw-step-health" data-state="ready"><span>${icon("mosaic")} <b>${esc(t("milkyway.keep_layers_title", "Capas científicas preservadas"))}</b> · ${esc(t("milkyway.keep_layers_body", "Cielo, Suelo, Compuesto, Máscara y mapas siempre quedan disponibles para auditar y editar."))}</span></div>`}
      ${issuesHtml(check.steps[4])}
    </section>`;
  }

  function publishPage(check) {
    const productLabels = {
      sky: t("milkyway.product_sky", "Cielo"),
      ground: t("milkyway.product_ground", "Suelo"),
      composite: t("milkyway.product_composite", "Compuesto"),
      mask: t("milkyway.product_mask", "Máscara"),
      skyVariance: t("milkyway.product_sky_variance", "Varianza cielo"),
      groundVariance: t("milkyway.product_ground_variance", "Varianza suelo"),
      skyCoverage: t("milkyway.product_sky_coverage", "Cobertura cielo"),
      groundCoverage: t("milkyway.product_ground_coverage", "Cobertura suelo"),
      skyRejection: t("milkyway.product_sky_rejection", "Rechazo cielo"),
      groundRejection: t("milkyway.product_ground_rejection", "Rechazo suelo"),
      recipe: t("milkyway.product_recipe", "Receta reproducible"),
    };
    const products = check.products.map(item => `<li>${icon(item === "sky" ? "galaxy" : item === "ground" ? "moon" : "mosaic")}<span>${esc(productLabels[item] || item)}</span></li>`).join("");
    const resultReview = result ? summarizeMilkyWayStackResult(result) : null;
    const preflightHtml = preflight ? `<div class="mw-step-health" data-state="${preflight.warnings.length ? "warning" : "ready"}" role="status">
      <span>${icon(preflight.warnings.length ? "lightbulb" : "check")} <b>${esc(t("milkyway.preflight_ready", "Preflight nativo completado"))}</b> · ${preflight.width}×${preflight.height} · RAM ${esc(formatBytes(preflight.estimatedWorkingBytes))} · ${esc(t("milkyway.preflight_output", "salidas"))} ${esc(formatBytes(preflight.estimatedOutputBytes))}</span>
      ${preflight.warnings.length ? `<details><summary>${esc(t("milkyway.preflight_warnings", "Ver advertencias del plan"))}</summary><ul class="mw-diagnostic-list">${preflight.warnings.map(message => `<li>${esc(message)}</li>`).join("")}</ul></details>` : ""}
    </div>` : "";
    const resultHtml = result ? `<div class="mw-result" data-state="${resultReview.degraded ? "degraded" : "scientific"}" role="status">
      <b>${esc(resultReview.scientific
        ? t("milkyway.stack_finished", "Apilado terminado")
        : t("milkyway.stack_degraded", "Apilado terminado con concesiones"))}</b>
      <div><strong>${esc(resultReview.scientific
        ? t("milkyway.result_scientific", "Resultado científico · sin exclusiones ocultas")
        : t("milkyway.result_not_scientific", "Resultado no científico · revisa las exclusiones"))}</strong>
        <span>${esc(result.summary || t("milkyway.result_summary", "Capas publicadas y receta preservada."))}</span>
        ${resultReview.excludedFrames ? `<span>${resultReview.excludedFrames} ${esc(t("milkyway.excluded_frames", "toma(s) excluida(s)"))}</span>` : ""}
        ${resultReview.messages.length ? `<details><summary>${esc(t("milkyway.result_warnings", "Ver advertencias registradas"))}</summary><ul class="mw-diagnostic-list">${resultReview.messages.map(message => `<li>${esc(message)}</li>`).join("")}</ul></details>` : ""}
      </div>
      <button data-mw-action="open-editor">${esc(t("milkyway.open_studio", "Abrir Vía Láctea Studio"))}</button>
    </div>` : "";
    return `<section class="mw-page" data-page="publish">
      <div class="mw-page-head"><span>${esc(t("milkyway.publish_eyebrow", "06 · Resultado"))}</span><h3>${esc(t("milkyway.publish_title", "Publica y sigue editando"))}</h3><p>${esc(t("milkyway.publish_body", "El original y las capas lineales permanecen inmutables. El editor procesa el Compuesto y conserva Cielo, Suelo y Máscara como fuentes sincronizadas para comparar."))}</p></div>
      <div class="mw-publish-summary"><ul>${products}</ul><div><b>${state.lights.filter(frame => frame.included !== false).length} ${esc(t("milkyway.frames", "tomas"))}</b><span>${state.registration.inliers || 0} ${esc(t("milkyway.stars", "estrellas"))} · RMS ${Number.isFinite(state.registration.rmsPx) ? state.registration.rmsPx.toFixed(2) : "—"} px</span><span>${esc(t("milkyway.geometry", "Geometría"))} ${esc(state.geometryId)}</span></div></div>
      <div class="mw-output-row"><label>${esc(t("milkyway.work_folder", "Carpeta de trabajo"))}<input readonly value="${esc(state.outputDirectory)}" placeholder="${esc(t("milkyway.choose_folder_placeholder", "Elige una carpeta"))}"></label><button data-mw-action="choose-output">${esc(t("milkyway.choose_folder", "Elegir carpeta"))}</button></div>
      <div class="mw-export-grid">
        <label class="mw-check"><input type="checkbox" data-mw-field="publish.exportFitsLayers" ${state.publish.exportFitsLayers ? "checked" : ""}><span><b>${esc(t("milkyway.export_fits_title", "FITS por capa"))}</b><small>${esc(t("milkyway.export_fits_body", "Datos y mapas científicos."))}</small></span></label>
        <label class="mw-check"><input type="checkbox" data-mw-field="publish.openEditor" ${state.publish.openEditor ? "checked" : ""}><span><b>${esc(t("milkyway.open_editor_title", "Abrir editor"))}</b><small>${esc(t("milkyway.open_editor_body", "Edita el Compuesto; consulta Cielo, Suelo y Máscara sin alterarlos."))}</small></span></label>
      </div>
      ${preflightHtml}
      ${resultHtml}
      ${issuesHtml(check.steps[5])}
    </section>`;
  }

  function issuesHtml(info) {
    if (!info?.reasons?.length) return `<div class="mw-step-health" data-state="ready">${esc(t("milkyway.health_ready", "Listo para continuar · no hay decisiones pendientes."))}</div>`;
    return `<div class="mw-step-health" data-state="${info.ready ? "warning" : "review"}">${info.reasons.map(reason => `<span>${icon(info.ready ? "lightbulb" : "warning")} ${esc(localizeReason(reason))}</span>`).join("")}</div>`;
  }

  function pageHtml(check) {
    return [dataPage, maskPage, registrationPage, integrationPage, compositionPage, publishPage][state.activeStep](check);
  }

  function focusedControlSelector() {
    const active = document.activeElement;
    if (!(active instanceof HTMLElement) || !modal?.contains(active)) return "";
    if (active.id) return `#${CSS.escape(active.id)}`;
    for (const attribute of [
      "data-mw-field", "data-mw-action", "data-mw-mode", "data-mw-mask",
      "data-mw-brush-target", "data-mw-experience", "data-mw-step", "data-mw-preset",
    ]) {
      const value = active.getAttribute(attribute);
      if (value != null) return `[${attribute}="${CSS.escape(value)}"]`;
    }
    return "";
  }

  function render() {
    ensureModal();
    const restoreFocus = focusedControlSelector();
    const fileReviewWasOpen = !!modal.querySelector(".mw-file-review[open]");
    const check = validation();
    const current = check.steps[state.activeStep];
    const progressItems = progress?.itemsTotal > 0
      ? `${progress.itemsDone}/${progress.itemsTotal}`
      : "";
    const progressEta = formatDuration(progress?.etaSeconds);
    const progressMeta = [progressItems, progressEta ? `ETA ${progressEta}` : ""].filter(Boolean).join(" · ");
    const closePromptHtml = closePrompt ? `<div class="mw-operation-error" role="alertdialog" aria-label="${esc(t("milkyway.active_run_title", "Apilado en curso"))}">
      <strong>${esc(t("milkyway.active_run_title", "Apilado en curso"))}</strong>
      <span>${esc(t("milkyway.active_run_body", "Cerrar no debe abandonar ni confundir el trabajo activo. Puedes mantener esta vista o dejar el proceso seguro en segundo plano."))}</span>
      <button type="button" data-mw-action="keep-open">${esc(t("milkyway.keep_open", "Mantener abierto"))}</button>
      <button type="button" data-mw-action="continue-background">${esc(t("milkyway.continue_background", "Continuar en segundo plano"))}</button>
      ${activeJobId ? `<button type="button" data-mw-action="cancel-run">${esc(t("milkyway.cancel_safely", "Cancelar con seguridad"))}</button>` : ""}
    </div>` : "";
    modal.dataset.experience = state.presentationMode;
    modal.setAttribute("aria-busy", busy ? "true" : "false");
    modal.innerHTML = `<div class="mw-dialog">
      <header class="mw-header"><div class="mw-title-mark">${icon("galaxy")}</div><div><span>${esc(t("milkyway.header_eyebrow", "PAISAJE NOCTURNO · MOTOR LINEAL"))}</span><h2 id="mw-title">${esc(t("milkyway.header_title", "Apilado de Vía Láctea"))}</h2><p>${esc(t("milkyway.header_body", "Cielo móvil, suelo fijo y capas editables."))}</p></div>
        <div class="mw-experience" role="group" aria-label="${esc(t("milkyway.experience_aria", "Nivel de detalle"))}"><button data-mw-experience="essential" aria-pressed="${state.presentationMode === "essential"}">${esc(t("milkyway.experience_essential", "Esencial"))}</button><button data-mw-experience="expert" aria-pressed="${state.presentationMode === "expert"}">${esc(t("milkyway.experience_expert", "Experto"))}</button></div>
        <button class="mw-close" data-mw-action="close" aria-label="${esc(t("milkyway.close", "Cerrar"))}">${icon("cross")}</button></header>
      <nav class="mw-steps" aria-label="${esc(t("milkyway.steps_aria", "Etapas del apilado de Vía Láctea"))}">${navHtml(check)}</nav>
      <div class="mw-assistant" data-state="${current.ready ? "ready" : "review"}">${icon(current.ready ? "star" : "warning")}<span><b>${esc(current.ready ? t("milkyway.assistant_ready", "Paso preparado") : t("milkyway.assistant_review", "Hay una decisión pendiente"))}</b><small>${esc(current.reasons[0] ? localizeReason(current.reasons[0]) : t("milkyway.assistant_body", "Zenith conservará la geometría y la receta al avanzar."))}</small></span><strong>${state.activeStep + 1} / ${MILKY_WAY_STEPS.length}</strong></div>
      ${closePromptHtml}
      ${state.runError ? `<div class="mw-operation-error" role="alert"><strong>${esc(t("milkyway.operation_unchanged", "No se aplicó ningún cambio"))}</strong><span>${esc(state.runError)}</span><button type="button" data-mw-action="dismiss-error">${esc(t("milkyway.dismiss_error", "Cerrar aviso"))}</button></div>` : ""}
      <main class="mw-content">${pageHtml(check)}</main>
      ${busy && progress ? `<section class="mw-live-progress" aria-live="polite"><div><strong>${esc(progressPhase(progress))}</strong><span>${esc(localizeProgressText(progress.detail) || t("milkyway.progress_body", "Zenith conserva las capas lineales mientras trabaja."))}${progressMeta ? ` · ${esc(progressMeta)}` : ""}</span></div><b>${Math.round(Number(progress.progress) || 0)}%</b><progress max="100" value="${Math.max(0, Math.min(100, Number(progress.progress) || 0))}"></progress>${activeJobId ? `<button type="button" data-mw-action="cancel-run">${esc(t("milkyway.cancel_safely", "Cancelar con seguridad"))}</button>` : ""}</section>` : ""}
      <footer class="mw-footer"><button data-mw-action="prev" ${state.activeStep === 0 ? "disabled" : ""}>${esc(t("milkyway.previous", "Anterior"))}</button><span>${esc(current.ready ? t("milkyway.can_continue", "Puedes continuar") : current.reasons[0] ? localizeReason(current.reasons[0]) : t("milkyway.review_step", "Revisa este paso"))}</span>
        ${state.activeStep === MILKY_WAY_STEPS.length - 1
          ? `<button class="mw-run" data-mw-action="run" ${busy || !(check.valid || document.body.dataset.mwFixture === "1") ? "disabled" : ""}>${esc(busy ? t("milkyway.processing_ellipsis", "Procesando…") : t("milkyway.run", "Apilar Vía Láctea"))}</button>`
          : `<button class="mw-next" data-mw-action="next" ${current.ready ? "" : "disabled"}>${esc(t("milkyway.continue_to", "Continuar a"))} ${esc(stepLabel(MILKY_WAY_STEPS[state.activeStep + 1][0], MILKY_WAY_STEPS[state.activeStep + 1][1]))}</button>`}
      </footer>
    </div>`;
    if (busy) {
      modal.querySelectorAll("button").forEach(button => {
        if (!["close", "cancel-run", "keep-open", "continue-background"].includes(button.dataset.mwAction || "")) button.disabled = true;
      });
    }
    requestAnimationFrame(() => {
      const fileReview = modal.querySelector(".mw-file-review");
      if (fileReview && fileReviewWasOpen) fileReview.open = true;
      drawMask();
      if (restoreFocus && !modal.hidden) {
        const control = modal.querySelector(restoreFocus);
        if (control && !control.disabled) control.focus({ preventScroll: true });
      }
    });
  }

  function drawMask() {
    const canvas = modal?.querySelector("[data-mw-mask-canvas]");
    const stage = modal?.querySelector("[data-mw-mask-stage]");
    if (!canvas || !stage) return;
    const rect = stage.getBoundingClientRect();
    const width = Math.max(1, Math.round(rect.width));
    const height = Math.max(1, Math.round(rect.height));
    canvas.width = width;
    canvas.height = height;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    const points = state.mask.horizonPoints?.length >= 2
      ? state.mask.horizonPoints
      : DEFAULT_MASK_HORIZON;
    ctx.clearRect(0, 0, width, height);
    ctx.beginPath();
    ctx.moveTo(0, 0);
    points.forEach(([x,y]) => ctx.lineTo(x * width, y * height));
    ctx.lineTo(width, 0);
    ctx.closePath();
    ctx.fillStyle = "rgba(34,211,238,.23)";
    ctx.fill();
    ctx.beginPath();
    points.forEach(([x,y], index) => index ? ctx.lineTo(x * width, y * height) : ctx.moveTo(x * width, y * height));
    ctx.strokeStyle = "rgba(196,181,253,.95)";
    ctx.lineWidth = 3;
    ctx.stroke();
    for (const stroke of state.mask.brushStrokes || []) {
      if (!Array.isArray(stroke?.points) || stroke.points.length < 2) continue;
      ctx.beginPath();
      stroke.points.forEach(([x, y], index) => index
        ? ctx.lineTo(x * width, y * height)
        : ctx.moveTo(x * width, y * height));
      ctx.strokeStyle = stroke.target === "ground"
        ? "rgba(251,191,36,.88)"
        : "rgba(34,211,238,.9)";
      ctx.lineWidth = Math.max(6, Number(state.mask.brushRadiusPx || 42) * 2 * (width / 1672));
      ctx.lineCap = "round";
      ctx.lineJoin = "round";
      ctx.stroke();
    }
  }

  function normalizedMaskPoint(event, canvas) {
    const rect = canvas.getBoundingClientRect();
    return [
      Math.max(0, Math.min(1, (event.clientX - rect.left) / Math.max(1, rect.width))),
      Math.max(0, Math.min(1, (event.clientY - rect.top) / Math.max(1, rect.height))),
    ];
  }

  function updateMaskFromPointer(event) {
    const canvas = maskPointer?.canvas;
    if (!canvas) return;
    const point = normalizedMaskPoint(event, canvas);
    if (state.mask.strategy === "horizon") {
      const points = [...(state.mask.horizonPoints || [])];
      let index = points.reduce((best, item, candidate) =>
        Math.abs(item[0] - point[0]) < Math.abs((points[best]?.[0] ?? 9) - point[0]) ? candidate : best, 0);
      if (!points.length || Math.abs((points[index]?.[0] ?? 9) - point[0]) > .12) {
        points.push(point);
        points.sort((a, b) => a[0] - b[0]);
        index = points.indexOf(point);
      } else {
        points[index] = point;
      }
      state.mask.horizonPoints = points;
    } else if (state.mask.strategy === "brush") {
      const stroke = state.mask.brushStrokes?.[maskPointer.strokeIndex];
      if (stroke) stroke.points.push(point);
    } else {
      return;
    }
    state.mask.userConfirmed = false;
    invalidateMaskDetectionReview();
    invalidateRegistration();
    drawMask();
  }

  function onMaskPointerDown(event) {
    const canvas = event.target.closest?.("[data-mw-mask-canvas]");
    if (!canvas || !["horizon", "brush"].includes(state.mask.strategy)) return;
    event.preventDefault();
    canvas.setPointerCapture?.(event.pointerId);
    maskPointer = { canvas, pointerId: event.pointerId, strokeIndex: -1 };
    if (state.mask.strategy === "brush") {
      state.mask.brushStrokes ||= [];
      state.mask.brushStrokes.push({ target: state.mask.brushTarget === "ground" ? "ground" : "sky", points: [] });
      maskPointer.strokeIndex = state.mask.brushStrokes.length - 1;
    }
    updateMaskFromPointer(event);
  }

  function onMaskPointerMove(event) {
    if (!maskPointer || event.pointerId !== maskPointer.pointerId) return;
    event.preventDefault();
    updateMaskFromPointer(event);
  }

  function onMaskPointerUp(event) {
    if (!maskPointer || event.pointerId !== maskPointer.pointerId) return;
    maskPointer.canvas.releasePointerCapture?.(event.pointerId);
    maskPointer = null;
    render();
  }

  function resetSession(seed = {}) {
    state = createMilkyWayState(seed);
    preview = "";
    result = null;
    maskPointer = null;
    maskDetection = normalizeMilkyWayMaskDetection();
    showDetectedMask = false;
    progress = null;
    preflight = null;
    activeJobId = "";
    runStartedAtMs = 0;
    closePrompt = false;
    preserveOnNextOpen = false;
    runInBackground = false;
  }

  function open(initial = {}) {
    ensureModal();
    // `ensureMilkyWayFlow()` serializa la importación, pero dos clics durante
    // esa promesa todavía llegan aquí en microtareas consecutivas. Tratar una
    // ventana ya visible como reanudación hace `open()` idempotente: no resetea
    // dos veces la sesión ni vuelve a fotografiar un fondo que ya está inert
    // (ese segundo snapshot restauraba `inert=true` al cerrar y bloqueaba la app).
    const alreadyOpen = modal.hidden === false;
    const resume = alreadyOpen || initial.resume === true || preserveOnNextOpen || busy || !!activeJobId;
    if (!resume && document.body.dataset.mwFixture !== "1") {
      resetSession(initial.state || {});
    }
    preserveOnNextOpen = false;
    runInBackground = false;
    if (initial.mode) state.mode = initial.mode;
    if (Number.isFinite(initial.step)) state.activeStep = initial.step;
    modal.hidden = false;
    if (!alreadyOpen) setBackgroundInert(true);
    render();
    requestAnimationFrame(() => modal.querySelector(".mw-close")?.focus());
  }

  function close({ force = false } = {}) {
    if (!modal) return;
    if (busy && !force) {
      closePrompt = true;
      render();
      return false;
    }
    closePrompt = false;
    modal.hidden = true;
    setBackgroundInert(false);
    document.getElementById("btn-milkyway-mode")?.focus();
    return true;
  }

  function setNested(path, value) {
    const fields = path.split(".");
    let target = state;
    while (fields.length > 1) target = target[fields.shift()];
    target[fields[0]] = value;
  }

  function invalidatePublishedResult() {
    result = null;
    preflight = null;
    state.runError = "";
  }

  function invalidateMaskDetectionReview() {
    maskDetection = normalizeMilkyWayMaskDetection();
    showDetectedMask = false;
  }

  function invalidateRegistration() {
    invalidatePublishedResult();
    state.analysis = { frames: [], warnings: [], degraded: false };
    state.registration = {
      ...state.registration,
      skySolved: false,
      groundSolved: false,
      inliers: 0,
      rmsPx: null,
      cornerResidualPx: null,
    };
  }

  function invalidateMaskForNewBase() {
    state.mask = { ...state.mask, confidence: 0, userConfirmed: false };
    invalidateMaskDetectionReview();
    invalidateRegistration();
  }

  async function refreshBasePreview() {
    if (!state.baseFramePath) { preview = ""; return; }
    if (document.body.dataset.mwFixture === "1") { preview = FIXTURE_IMAGE; return; }
    try { preview = await invoke?.("deepsky_frame_preview", { path: state.baseFramePath, maxSize: 1800 }); }
    catch (_) { preview = ""; }
  }

  async function ensureIncludedBaseFrame() {
    const previousBase = state.baseFramePath;
    const current = state.lights.find(frame => frame.path === state.baseFramePath && frame.included !== false);
    if (!current) state.baseFramePath = state.lights.find(frame => frame.included !== false)?.path || "";
    const changed = state.baseFramePath !== previousBase;
    if (changed) {
      await refreshBasePreview();
      invalidateMaskForNewBase();
    }
    return changed;
  }

  async function addFiles(kind) {
    const paths = await openDialog?.({
      multiple: true,
      directory: false,
      title: kind === "lights"
        ? t("milkyway.dialog_add_lights", "Añadir tomas de Vía Láctea")
        : kind === "darks"
          ? t("milkyway.dialog_add_darks", "Añadir darks")
          : t("milkyway.dialog_add_flats", "Añadir flats"),
      filters: [{ name: t("milkyway.linear_filter", "FITS o TIFF lineal"), extensions: ["fits","fit","fts","tif","tiff"] }],
    });
    if (!paths) return;
    const list = Array.isArray(paths) ? paths : [paths];
    const existing = new Set(state[kind].map(frame => String(frame.path)));
    let additions = list
      .map(path => String(path))
      .filter(path => path && !existing.has(path))
      .map(path => ({ path, name: path.split(/[\\/]/).pop(), included: true }));
    if (additions.length && typeof invoke === "function") {
      try {
        const probes = await invoke("deepsky_probe", { paths: additions.map(frame => frame.path) });
        if (Array.isArray(probes)) {
          const byPath = new Map(probes.map(probe => [String(probe.path), probe]));
          additions = additions.map(frame => {
            const probe = byPath.get(frame.path);
            if (!probe) return frame;
            return {
              ...frame,
              name: String(probe.name || frame.name),
              width: Number(probe.w) || null,
              height: Number(probe.h) || null,
              channels: Number(probe.ch) || null,
              exposureSeconds: probe.exptime != null && Number.isFinite(Number(probe.exptime)) ? Number(probe.exptime) : null,
              iso: probe.gain != null && Number.isFinite(Number(probe.gain)) ? Number(probe.gain) : null,
              ok: probe.ok !== false,
              error: probe.error ? String(probe.error) : null,
              included: probe.ok !== false,
            };
          });
        }
      } catch (_) {
        // El preflight Rust volverá a leer cada entrada; un fallo del sondeo
        // no convierte el archivo en inválido ni inventa metadatos.
      }
    }
    state[kind].push(...additions);
    if (additions.length) invalidateRegistration();
    if (!state.baseFramePath && kind === "lights") state.baseFramePath = state.lights[Math.floor(state.lights.length / 2)]?.path || "";
    if (kind === "lights" && state.baseFramePath) {
      try { preview = await invoke?.("deepsky_frame_preview", { path: state.baseFramePath, maxSize: 1800 }); } catch (_) { preview = ""; }
    }
    render();
  }

  async function chooseOutput() {
    const path = await openDialog?.({ directory: true, multiple: false, title: t("milkyway.dialog_output", "Carpeta de trabajo y salida de Vía Láctea") });
    if (path) { state.outputDirectory = String(path); invalidatePublishedResult(); render(); }
  }

  async function detectMask() {
    if (document.body.dataset.mwFixture === "1") {
      state.mask = { ...state.mask, strategy: "auto", confidence: .93, userConfirmed: true };
      maskDetection = {
        confidence: .93,
        skyFraction: .62,
        warnings: [],
        preview: "",
      };
      invalidateRegistration();
      render();
      return;
    }
    busy = true; render();
    try {
      const detected = await invoke?.("detect_milky_way_sky_mask", { request: buildMilkyWayRequest(state) });
      maskDetection = normalizeMilkyWayMaskDetection(detected);
      state.mask = {
        ...state.mask,
        ...(detected?.mask || detected || {}),
        // Preserve automatic provenance in the UI.  The native result exposes
        // an editable horizon, but labelling it as a user horizon would bypass
        // the automatic-confidence blocker before the user has reviewed it.
        strategy: "auto",
        confidence: maskDetection.confidence,
        userConfirmed: false,
      };
      showDetectedMask = !!maskDetection.preview;
      invalidateRegistration();
      state.runError = "";
    } catch (error) {
      state.runError = `${t("milkyway.error_mask", "No se pudo detectar la frontera")}: ${String(error)}`;
    } finally { busy = false; render(); }
  }

  async function analyzeRegistration() {
    if (document.body.dataset.mwFixture === "1") {
      state.registration = { ...state.registration, skySolved: true, groundSolved: true, inliers: 184, rmsPx: .72, cornerResidualPx: 1.18 };
      state.analysis = {
        frames: state.lights.filter(frame => frame.included !== false).map(frame => ({
          path: frame.path,
          sky: { excluded: false },
          ground: state.mode === MILKY_WAY_MODES.SKY_ONLY ? null : { excluded: false },
        })),
        warnings: [],
        degraded: false,
      };
      render();
      return;
    }
    busy = true; render();
    try {
      const analysis = await invoke?.("analyze_milky_way_registration", { request: buildMilkyWayRequest(state) });
      state.registration = { ...state.registration, ...(analysis?.registration || analysis || {}) };
      const review = summarizeMilkyWayRegistrationAnalysis(analysis);
      state.analysis = {
        frames: review.frames,
        warnings: review.warnings,
        degraded: review.degraded,
      };
      state.runError = "";
    } catch (error) {
      state.runError = `${t("milkyway.error_registration", "No se pudo validar el registro")}: ${String(error)}`;
    } finally { busy = false; render(); }
  }

  async function runStack() {
    if (busy) return;
    const check = validation();
    if (!check.valid && document.body.dataset.mwFixture !== "1") return;
    state.jobId = `milky-way-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    activeJobId = "";
    runStartedAtMs = Date.now();
    closePrompt = false;
    runInBackground = false;
    preflight = null;
    progress = normalizeMilkyWayProgress({
      phase: t("milkyway.preparing", "Preparando"),
      detail: t("milkyway.preparing_body", "Validando tomas, geometría y espacio de trabajo."),
      progress: 0,
      itemsDone: 0,
      itemsTotal: state.lights.filter(frame => frame.included !== false).length,
    }, { startedAtMs: runStartedAtMs });
    busy = true; render();
    try {
      if (document.body.dataset.mwFixture === "1") {
        result = { summary: t("milkyway.fixture_summary", "Capas, VAR, cobertura, rechazo y receta publicados por rama."), fixture: true };
      } else {
        const prepared = await invoke?.("prepare_milky_way_stack", { request: buildMilkyWayRequest(state) });
        preflight = summarizeMilkyWayPreflight(prepared);
        if (preflight.jobId) state.jobId = preflight.jobId;
        activeJobId = state.jobId;
        runStartedAtMs = Date.now();
        progress = normalizeMilkyWayProgress({
          jobId: activeJobId,
          phase: t("milkyway.preflight_ready", "Preflight nativo completado"),
          detail: `${preflight.width}×${preflight.height} · RAM ${formatBytes(preflight.estimatedWorkingBytes)} · ${t("milkyway.preflight_output", "salidas")} ${formatBytes(preflight.estimatedOutputBytes)}`,
          progress: 1,
          itemsDone: 0,
          itemsTotal: state.lights.filter(frame => frame.included !== false).length,
        }, { startedAtMs: runStartedAtMs });
        render();
        result = await invoke?.("run_milky_way_stack", { request: buildMilkyWayRequest(state) });
      }
      busy = false;
      activeJobId = "";
      progress = null;
      if (runInBackground) preserveOnNextOpen = true;
      render();
      const review = summarizeMilkyWayStackResult(result);
      // A degraded result requires an explicit human review before Studio; do
      // not make its scientific status disappear behind an automatic launch.
      if (state.publish.openEditor && !result?.fixture && review.scientific && !runInBackground) {
        close();
        await openEditor?.(result);
      }
    } catch (error) {
      busy = false;
      activeJobId = "";
      progress = null;
      state.runError = String(error);
      if (runInBackground) preserveOnNextOpen = true;
      render();
    }
  }

  async function cancelRun() {
    if (!activeJobId || document.body.dataset.mwFixture === "1") return;
    try {
      const requested = await invoke?.("cancel_milky_way_stack", { jobId: activeJobId });
      progress = {
        ...(progress || {}),
        detail: requested
          ? t("milkyway.cancel_requested", "Cancelación solicitada; cerrando el producto parcial de forma segura.")
          : t("milkyway.already_finished", "El trabajo ya había terminado."),
      };
      render();
    } catch (error) {
      state.runError = `${t("milkyway.error_cancel", "No se pudo solicitar la cancelación")}: ${String(error)}`;
      render();
    }
  }

  async function onClick(event) {
    const button = event.target.closest("button");
    if (!button || !modal.contains(button)) return;
    if (button.dataset.mwMode) {
      const previousMode = state.mode;
      const nextMode = button.dataset.mwMode;
      if (previousMode !== nextMode) invalidateRegistration();
      if (previousMode !== nextMode) invalidateMaskDetectionReview();
      state.mode = nextMode;
      if (nextMode === MILKY_WAY_MODES.SKY_ONLY) {
        state.mask = {
          ...state.mask,
          strategy: "fullSky",
          confidence: 1,
          userConfirmed: true,
          horizonPoints: [],
          brushStrokes: [],
        };
      } else if (previousMode === MILKY_WAY_MODES.SKY_ONLY) {
        state.mask = {
          ...state.mask,
          strategy: "auto",
          confidence: 0,
          userConfirmed: false,
          horizonPoints: [],
          brushStrokes: [],
        };
      }
      render(); return;
    }
    if (button.dataset.mwMask) {
      if (state.mask.strategy !== button.dataset.mwMask) {
        state.mask.userConfirmed = false;
        invalidateMaskDetectionReview();
        invalidateRegistration();
      }
      state.mask.strategy = button.dataset.mwMask;
      render(); return;
    }
    if (button.dataset.mwBrushTarget) { state.mask.brushTarget = button.dataset.mwBrushTarget; render(); return; }
    if (button.dataset.mwExperience) { state.presentationMode = button.dataset.mwExperience; render(); return; }
    if (button.dataset.mwStep != null) { const moved = setMilkyWayStep(state, Number(button.dataset.mwStep)); state = moved.state; render(); return; }
    if (button.dataset.mwPreset) {
      const preset = button.dataset.mwPreset;
      state.integration = {
        ...state.integration,
        profile: preset,
        method: preset === "fast" ? "sigmaClip" : "winsorized",
        normalization: "robustLinear",
      };
      invalidatePublishedResult();
      render(); return;
    }
    switch (button.dataset.mwAction) {
      case "close": close(); break;
      case "keep-open": closePrompt = false; render(); break;
      case "continue-background":
        closePrompt = false;
        preserveOnNextOpen = true;
        runInBackground = true;
        close({ force: true });
        break;
      case "dismiss-error": state.runError = ""; render(); break;
      case "toggle-mask-preview": showDetectedMask = !showDetectedMask; render(); break;
      case "prev": state.activeStep = Math.max(0, state.activeStep - 1); render(); break;
      case "next": { const moved = setMilkyWayStep(state, state.activeStep + 1); state = moved.state; render(); break; }
      case "add-lights": await addFiles("lights"); break;
      case "add-darks": await addFiles("darks"); break;
      case "add-flats": await addFiles("flats"); break;
      case "choose-output": await chooseOutput(); break;
      case "detect-mask": await detectMask(); break;
      case "analyze-registration": await analyzeRegistration(); break;
      case "mask-up":
      case "mask-down": {
        const delta = button.dataset.mwAction === "mask-up" ? -.02 : .02;
        const points = state.mask.horizonPoints?.length >= 2 ? state.mask.horizonPoints : DEFAULT_MASK_HORIZON;
        state.mask.horizonPoints = points.map(([x, y]) => [x, Math.max(0, Math.min(1, y + delta))]);
        state.mask.strategy = "horizon";
        state.mask.userConfirmed = false;
        invalidateMaskDetectionReview();
        invalidateRegistration();
        render();
        break;
      }
      case "mask-undo":
        state.mask.brushStrokes = (state.mask.brushStrokes || []).slice(0, -1);
        state.mask.userConfirmed = false;
        invalidateMaskDetectionReview();
        invalidateRegistration();
        render();
        break;
      case "mask-reset":
        state.mask = {
          ...state.mask,
          strategy: "auto",
          confidence: 0,
          userConfirmed: false,
          horizonPoints: [],
          brushStrokes: [],
        };
        invalidateMaskDetectionReview();
        invalidateRegistration();
        render();
        break;
      case "remove-frame": {
        const kind = button.dataset.mwFrameKind;
        const index = Number(button.dataset.mwFrameIndex);
        if (["lights", "darks", "flats"].includes(kind) && Number.isInteger(index) && state[kind]?.[index]) {
          state[kind].splice(index, 1);
          invalidateRegistration();
          await ensureIncludedBaseFrame();
          render();
        }
        break;
      }
      case "run": await runStack(); break;
      case "cancel-run": closePrompt = false; await cancelRun(); break;
      case "open-editor": close(); await openEditor?.(result); break;
      default: break;
    }
  }

  async function onChange(event) {
    const frameKind = event.target.dataset.mwFrameKind;
    const frameIndex = Number(event.target.dataset.mwFrameIndex);
    if (["lights", "darks", "flats"].includes(frameKind)
      && Number.isInteger(frameIndex)
      && state[frameKind]?.[frameIndex]) {
      state[frameKind][frameIndex].included = event.target.checked;
      invalidateRegistration();
      await ensureIncludedBaseFrame();
      render();
      return;
    }
    const field = event.target.dataset.mwField;
    if (!field) return;
    const value = event.target.type === "checkbox"
      ? event.target.checked
      : event.target.type === "range" || event.target.type === "number"
        ? Number(event.target.value)
        : event.target.value;
    setNested(field, value);
    if (field === "registration.model"
      && state.registration.model === "radialWide"
      && state.registration.distortionCorrection === "off") {
      state.registration.distortionCorrection = "auto";
    } else if (field === "registration.distortionCorrection"
      && state.registration.distortionCorrection === "off"
      && state.registration.model === "radialWide") {
      // Honour the user's explicit request to switch distortion off instead of
      // executing the native radial warper behind a contradictory control.
      state.registration.model = "auto";
    }
    if (field === "baseFramePath") {
      await refreshBasePreview();
      invalidateMaskForNewBase();
    } else if (field.startsWith("registration.") || field === "unifyExposure") {
      invalidateRegistration();
    } else if (field.startsWith("mask.")) {
      if (field !== "mask.userConfirmed") {
        invalidateMaskDetectionReview();
        invalidateRegistration();
      }
      invalidatePublishedResult();
    } else {
      invalidatePublishedResult();
    }
    render();
  }

  function onInput(event) {
    const field = event.target.dataset.mwField;
    if (!field || event.target.type !== "range") return;
    setNested(field, Number(event.target.value));
    if (field.startsWith("mask.")) {
      invalidateMaskDetectionReview();
      invalidateRegistration();
    }
    else invalidatePublishedResult();
    const output = event.target.parentElement?.querySelector("output");
    if (output) output.textContent = `${event.target.value} px`;
    if (field.startsWith("mask.")) drawMask();
  }

  function onKeydown(event) {
    if (event.key === "Escape") { event.preventDefault(); close(); return; }
    if (event.key !== "Tab") return;
    const focusable = [...modal.querySelectorAll('button:not([disabled]),input:not([disabled]),select:not([disabled]),summary,[tabindex="0"]')]
      .filter(element => !element.closest("[hidden]") && element.offsetParent !== null);
    if (!focusable.length) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
    else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
  }

  ensureModal();
  if (typeof listenEvent === "function") {
    Promise.resolve(listenEvent("milky-way-progress", event => {
      const payload = event?.payload || {};
      const eventJobId = String(payload.jobId || "");
      // Never adopt a job id from the global event bus.  More than one window
      // can listen to this channel, so only the id generated by this session is
      // authoritative for progress and cancellation.
      if (!activeJobId || !eventJobId || eventJobId !== activeJobId) return;
      progress = normalizeMilkyWayProgress(payload, { startedAtMs: runStartedAtMs });
      if (modal && !modal.hidden) render();
    })).then(unlisten => { removeProgressListener = typeof unlisten === "function" ? unlisten : null; }).catch(() => {});
  }
  if (bindLauncher) document.getElementById("btn-milkyway-mode")?.addEventListener("click", () => open());
  const params = new URLSearchParams(window.location.search);
  if (import.meta.env.DEV && params.get("ux-fixture") === "milky-way") {
    document.body.dataset.mwFixture = "1";
    const step = Math.max(0, Math.min(5, Number(params.get("step") || 1) - 1));
    state = fixtureState(step);
    state.presentationMode = params.get("experience") === "expert" ? "expert" : "essential";
    preview = FIXTURE_IMAGE;
    open({ step });
  }

  return { open, close, getState: () => createMilkyWayState(state), render, dispose: () => removeProgressListener?.() };
}
