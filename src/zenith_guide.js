const DEFAULT_RULES = [
  {
    id: "control-help",
    priority: 110,
    when: (ctx) => !!ctx.helpTarget,
    level: "info",
    title: (ctx) => ctx.helpTitle || "Ayuda del control",
    message: (ctx) => ctx.helpMessage || "Te llevo al control y conservo la explicación contextual.",
    target: (ctx) => ctx.helpTarget,
    actionLabel: "Volver al control",
    dismissible: true,
  },
  {
    id: "no-source",
    priority: 100,
    when: (ctx) => !ctx.hasSource && !ctx.hasResult,
    level: "info",
    title: "Carga una fuente",
    message: "Selecciona un video, una secuencia o un resultado para comenzar.",
    target: "#btn-open-file",
    actionLabel: "Elegir fuente",
    activate: true,
    dismissible: false,
  },
  {
    id: "histogram-unavailable",
    priority: 99,
    when: (ctx) => ctx.hasResult && ctx.histogramAvailable === false,
    level: "warning",
    title: "Falta el diagnóstico 16-bit",
    message: "Actualiza la medición antes de tocar niveles; no modifica la imagen.",
    target: "#btn-refresh-histogram",
    actionLabel: "Actualizar medición",
    activate: true,
    dismissible: false,
  },
  {
    id: "needs-analysis",
    priority: 95,
    when: (ctx) => ctx.hasSource && !ctx.hasAnalysis && !ctx.hasResult,
    level: "next",
    title: "Analiza antes de apilar",
    message: "El análisis detecta calidad, movimiento y el mejor fotograma de referencia.",
    target: "#btn-analyze",
    actionLabel: "Ir al análisis",
    dismissible: false,
  },
  {
    id: "ready-to-stack",
    priority: 90,
    when: (ctx) => ctx.hasAnalysis && !ctx.hasResult,
    level: "next",
    title: "Listo para apilar",
    message: "Revisa porcentaje, objetivo y método; después inicia el apilado.",
    target: "#btn-stack",
    actionLabel: "Revisar apilado",
    dismissible: false,
  },
  {
    id: "clipping",
    priority: 88,
    when: (ctx) => ctx.hasResult && (ctx.shadowClip > 0.0001 || ctx.highlightClip > 0.0001),
    level: "warning",
    title: "Protege el rango tonal",
    message: (ctx) => {
      const shadow = (Number(ctx.shadowClip || 0) * 100).toFixed(2);
      const highlight = (Number(ctx.highlightClip || 0) * 100).toFixed(2);
      return `Recorte medido: sombras ${shadow}% y luces ${highlight}%. Puedo neutralizar niveles agresivos y compensar el extremo afectado.`;
    },
    target: "#sl-level-mid",
    actionLabel: "Ver niveles",
    applyAction: "protect-range",
    applyLabel: "Aplicar corrección segura",
    dismissible: true,
  },
  {
    id: "ringing",
    priority: 84,
    when: (ctx) => ctx.hasArtifactAnalysis && ctx.ringingScore >= 8,
    level: "warning",
    title: "Reduce halos antes de afinar",
    message: (ctx) => `El análisis de imperfecciones midió halos ${Number(ctx.ringingScore).toFixed(1)}. Aplica la receta medida y comprueba con A/B.`,
    target: "#artifact-repair-card",
    actionLabel: "Abrir reparación",
    applyAction: "repair-ringing",
    applyLabel: "Aplicar sugerencia medida",
    dismissible: true,
  },
  {
    id: "colour-fringe",
    priority: 82,
    when: (ctx) => ctx.hasArtifactAnalysis && !ctx.isMono && ctx.colorFringeScore >= 6,
    level: "warning",
    title: "Revisa la alineación RGB",
    message: (ctx) => `El fringing medido es ${Number(ctx.colorFringeScore).toFixed(1)}. Zenith puede volver a estimar R y B sobre el máster 16-bit.`,
    target: ".atmospheric-module",
    actionLabel: "Ver corrección",
    applyAction: "align-rgb",
    applyLabel: "Medir y alinear",
    dismissible: true,
  },
  {
    id: "low-dynamic-range",
    priority: 72,
    when: (ctx) => ctx.hasResult && ctx.histogramAvailable && ctx.robustDynamicRange < 0.12,
    level: "next",
    title: "La señal está comprimida",
    message: (ctx) => {
      const low = Math.round(Number(ctx.percentileLow || 0) * 65535);
      const high = Math.round(Number(ctx.percentileHigh || 1) * 65535);
      return `El 99.8% útil ocupa aproximadamente ${low.toLocaleString()}–${high.toLocaleString()}. Puedo expandirlo con margen de seguridad.`;
    },
    target: "#post-tone-module",
    actionLabel: "Ver tono",
    applyAction: "auto-levels",
    applyLabel: "Expandir rango útil",
    dismissible: true,
  },
  {
    id: "dark-result",
    priority: 70,
    when: (ctx) => ctx.hasResult && ctx.histogramAvailable && ctx.medianLevel < 0.035 && ctx.shadowClip <= 0.0001,
    level: "next",
    title: "Medios tonos muy bajos",
    message: (ctx) => {
      const ev = Number(ctx.recommendedExposureEv || 0).toFixed(2);
      return `La mediana está en ${(Number(ctx.medianLevel || 0) * 100).toFixed(1)}%. Una compensación de ${ev} EV la acerca a una lectura útil sin mover el negro.`;
    },
    target: "#post-tone-module",
    actionLabel: "Ver medios tonos",
    applyAction: "lift-midtones",
    applyLabel: "Aplicar exposición calculada",
    dismissible: true,
  },
  {
    id: "solar-mono-workflow",
    priority: 68,
    when: (ctx) => ctx.hasResult && ctx.isMono && !ctx.solarActive,
    level: "next",
    title: "¿Es una captura solar mono?",
    message: (ctx) => {
      const range = Math.round(Number(ctx.robustDynamicRange || 0) * 100);
      return `La señal útil ocupa cerca del ${range}% del rango. Si corresponde al Sol, puedo crear un derivado H-alpha conservador con curva, falso color y protección de ruido.`;
    },
    target: "#solar-mono-module",
    actionLabel: "Abrir laboratorio solar",
    applyAction: "solar-auto",
    applyLabel: "Aplicar receta automática",
    dismissible: true,
  },
  {
    id: "solar-filaments",
    priority: 66,
    when: (ctx) => ctx.hasResult && ctx.isMono && ctx.solarActive && Number(ctx.solarFilamentAmount || 0) <= 0.001,
    level: "info",
    title: "Filamentos aún neutrales",
    message: "La curva solar está activa, pero la recuperación protegida de filamentos sigue en cero. Puedes medirla visualmente con A/B sin alterar el master.",
    target: "#sl-solar-filament",
    actionLabel: "Ver recuperación",
    applyAction: "solar-filaments-auto",
    applyLabel: "Aplicar realce conservador",
    dismissible: true,
  },
  {
    id: "mono-color",
    priority: 64,
    when: (ctx) => ctx.hasResult && ctx.isMono,
    level: "info",
    title: "Ruta monocroma protegida",
    message: "Colorimetría RGB y alineación atmosférica están bloqueadas. Si la captura es solar, el Laboratorio Solar puede generar falso color sin convertir ni reemplazar el master mono.",
    target: "#solar-mono-module",
    actionLabel: "Ver opciones solares",
    dismissible: true,
  },
  {
    id: "compare-ready",
    priority: 58,
    when: (ctx) => ctx.hasResult && ctx.canCompare && !ctx.compareActive,
    level: "success",
    title: "Ya puedes validar el ajuste",
    message: "Activa A/B y elige Paso anterior o Apilado original; ambas vistas conservan zoom y encuadre.",
    target: "#btn-post-compare",
    actionLabel: "Activar A/B",
    activate: true,
    dismissible: true,
  },
  {
    id: "result-ready",
    priority: 50,
    when: (ctx) => ctx.hasResult && ctx.histogramAvailable,
    level: "success",
    title: (ctx) => ctx.historyLength > 1 ? "Continúa con un cambio cada vez" : "Empieza por tono y rango",
    message: (ctx) => ctx.historyLength > 1
      ? "El historial está activo. Ajusta un módulo, compara y conserva sólo la mejora visible."
      : "Orden recomendado: niveles y curva, restauración de detalle, ruido y finalmente color.",
    target: "#post-tone-module",
    actionLabel: "Ir a tono y rango",
    dismissible: true,
  },
];

function resolveValue(value, context) {
  return typeof value === "function" ? value(context) : value;
}

export function evaluateGuide(context, rules = DEFAULT_RULES, dismissedIds = new Set()) {
  return rules
    .filter((rule) => !dismissedIds.has(rule.id) && rule.when(context))
    .sort((left, right) => (right.priority || 0) - (left.priority || 0))
    .slice(0, 4)
    .map(({ when, ...rule }) => ({
      ...rule,
      title: resolveValue(rule.title, context),
      message: resolveValue(rule.message, context),
      target: resolveValue(rule.target, context),
      actionLabel: resolveValue(rule.actionLabel, context),
      applyLabel: resolveValue(rule.applyLabel, context),
    }));
}

function appendSpriteIcon(button, iconId) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", "zas-icon");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
  use.setAttribute("href", `#${iconId}`);
  svg.append(use);
  button.append(svg);
}

export class IntelligentAssistant {
  constructor({ panel, list, status, summary, onNavigate, onApply } = {}) {
    this.panel = panel || null;
    this.list = list || null;
    this.status = status || null;
    this.summary = summary || null;
    this.onNavigate = onNavigate || (() => {});
    this.onApply = onApply || (() => {});
    this.context = {};
    this.generation = null;
    this.dismissedIds = new Set();
  }

  resetForGeneration(generation) {
    const normalized = generation ?? null;
    if (this.generation === normalized) return;
    this.generation = normalized;
    this.dismissedIds.clear();
  }

  update(context = {}) {
    this.resetForGeneration(context.generation ?? this.generation);
    this.context = { ...this.context, ...context };
    const suggestions = evaluateGuide(this.context, DEFAULT_RULES, this.dismissedIds);
    this.render(suggestions);
    return suggestions;
  }

  dismiss(id) {
    if (!id) return;
    this.dismissedIds.add(id);
    this.update({});
  }

  render(suggestions) {
    if (this.status) {
      const warnings = suggestions.filter((item) => item.level === "warning").length;
      this.status.textContent = warnings ? `Asistente · ${warnings} alerta${warnings === 1 ? "" : "s"}` : "Asistente inteligente";
      this.status.dataset.level = warnings ? "warning" : "success";
    }
    if (this.summary) {
      const flow = this.context.source === "mosaic" ? "Mosaico"
        : this.context.source === "batch" ? "Lote"
          : this.context.hasResult ? "Resultado individual" : "Preparación";
      const precision = this.context.hasResult ? "16-bit" : "sin resultado";
      const recommendationCount = suggestions.length === 1 ? "1 recomendación" : `${suggestions.length} recomendaciones`;
      this.summary.textContent = `${flow} · ${precision} · ${recommendationCount}`;
    }
    if (!this.list) return;
    this.list.replaceChildren();
    suggestions.forEach((suggestion, index) => {
      const article = document.createElement("article");
      article.className = `guide-card guide-${suggestion.level}`;
      article.dataset.suggestionId = suggestion.id;

      const rank = document.createElement("span");
      rank.className = "guide-rank";
      rank.textContent = String(index + 1).padStart(2, "0");

      const body = document.createElement("div");
      body.className = "guide-card-body";
      const heading = document.createElement("h4");
      heading.textContent = suggestion.title;
      const message = document.createElement("p");
      message.textContent = suggestion.message;
      body.append(heading, message);

      if (suggestion.dismissible !== false) {
        const dismiss = document.createElement("button");
        dismiss.type = "button";
        dismiss.className = "guide-dismiss-button";
        dismiss.setAttribute("aria-label", `Descartar recomendación: ${suggestion.title}`);
        dismiss.title = "Descartar para este apilado";
        appendSpriteIcon(dismiss, "icon-cross");
        dismiss.addEventListener("click", () => this.dismiss(suggestion.id));
        article.append(dismiss);
      }

      const actions = document.createElement("div");
      actions.className = "guide-card-actions";
      if (suggestion.target) {
        const button = document.createElement("button");
        button.type = "button";
        button.className = "guide-go-button";
        button.textContent = suggestion.actionLabel || "Ir al control";
        button.addEventListener("click", () => this.onNavigate(suggestion.target, suggestion));
        actions.append(button);
      }
      if (suggestion.applyAction) {
        const apply = document.createElement("button");
        apply.type = "button";
        apply.className = "guide-apply-button";
        apply.textContent = suggestion.applyLabel || "Aplicar recomendación";
        apply.addEventListener("click", async () => {
          apply.disabled = true;
          apply.setAttribute("aria-busy", "true");
          try {
            await this.onApply(suggestion.applyAction, suggestion, this.context);
          } finally {
            apply.disabled = false;
            apply.removeAttribute("aria-busy");
          }
        });
        actions.append(apply);
      }
      if (actions.childElementCount) body.append(actions);

      article.append(rank, body);
      this.list.append(article);
    });
  }
}

// Compatibility for external integrations that still import the old class.
export const ZenithGuide = IntelligentAssistant;
export { DEFAULT_RULES };
