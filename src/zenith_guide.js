const DEFAULT_RULES = [
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
  },
  {
    id: "histogram-unavailable",
    priority: 99,
    when: (ctx) => ctx.hasResult && ctx.histogramAvailable === false,
    level: "warning",
    title: "Falta el diagnóstico 16-bit",
    message: "Actualiza el histograma antes de tocar niveles; así evitas decidir sobre una vista incompleta.",
    target: "#btn-refresh-histogram",
    actionLabel: "Actualizar diagnóstico",
    activate: true,
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
      return `Recorte medido: sombras ${shadow}% y luces ${highlight}%. Ajusta niveles mirando el histograma.`;
    },
    target: "#post-histogram-card",
    actionLabel: "Corregir niveles",
  },
  {
    id: "ringing",
    priority: 84,
    when: (ctx) => ctx.hasArtifactAnalysis && ctx.ringingScore >= 8,
    level: "warning",
    title: "Reduce halos antes de afinar",
    message: (ctx) => `El análisis de imperfecciones midió halos ${Number(ctx.ringingScore).toFixed(1)}. Aplica deringing moderado y comprueba con A/B.`,
    target: "#artifact-repair-card",
    actionLabel: "Abrir reparación",
  },
  {
    id: "colour-fringe",
    priority: 82,
    when: (ctx) => ctx.hasArtifactAnalysis && !ctx.isMono && ctx.colorFringeScore >= 6,
    level: "warning",
    title: "Revisa la alineación RGB",
    message: (ctx) => `El fringing medido es ${Number(ctx.colorFringeScore).toFixed(1)}. Mide los canales antes de aumentar saturación.`,
    target: ".atmospheric-module",
    actionLabel: "Alinear canales",
  },
  {
    id: "low-dynamic-range",
    priority: 72,
    when: (ctx) => ctx.hasResult && ctx.histogramAvailable && ctx.dynamicRange < 0.12,
    level: "next",
    title: "La señal está comprimida",
    message: "Amplía el rango con niveles o una curva suave antes de aplicar detalle fino.",
    target: "#post-tone-module",
    actionLabel: "Abrir tono",
  },
  {
    id: "dark-result",
    priority: 70,
    when: (ctx) => ctx.hasResult && ctx.histogramAvailable && ctx.medianLevel < 0.035 && ctx.shadowClip <= 0.0001,
    level: "next",
    title: "Medios tonos muy bajos",
    message: "Sube exposición o medios de forma gradual; no muevas primero el punto negro.",
    target: "#post-tone-module",
    actionLabel: "Ajustar medios",
  },
  {
    id: "mono-color",
    priority: 64,
    when: (ctx) => ctx.hasResult && ctx.isMono,
    level: "info",
    title: "Señal monocroma protegida",
    message: "Los controles cromáticos están bloqueados; tono, deconvolución, wavelets y detalle siguen disponibles.",
    target: "#post-detail-module",
    actionLabel: "Trabajar detalle",
  },
  {
    id: "compare-ready",
    priority: 58,
    when: (ctx) => ctx.hasResult && ctx.canCompare,
    level: "success",
    title: "Ya puedes validar el ajuste",
    message: "Activa A/B para comparar la versión anterior u original con el mismo zoom y encuadre.",
    target: "#btn-post-compare",
    actionLabel: "Abrir A/B",
    activate: true,
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
    target: "#post-histogram-card",
    actionLabel: "Ir al primer paso",
  },
];

function resolveValue(value, context) {
  return typeof value === "function" ? value(context) : value;
}

export function evaluateGuide(context, rules = DEFAULT_RULES) {
  return rules
    .filter((rule) => rule.when(context))
    .sort((left, right) => (right.priority || 0) - (left.priority || 0))
    .slice(0, 4)
    .map(({ when, ...rule }) => ({
      ...rule,
      title: resolveValue(rule.title, context),
      message: resolveValue(rule.message, context),
    }));
}

export class IntelligentAssistant {
  constructor({ panel, list, status, summary, onNavigate } = {}) {
    this.panel = panel || null;
    this.list = list || null;
    this.status = status || null;
    this.summary = summary || null;
    this.onNavigate = onNavigate || ((target, suggestion = {}) => {
      const element = document.querySelector(target);
      const details = element?.closest?.("details");
      if (details) details.open = true;
      element?.scrollIntoView?.({ behavior: "smooth", block: "center" });
      element?.focus?.({ preventScroll: true });
      if (suggestion.activate && element && !element.disabled) element.click();
    });
    this.context = {};
  }

  update(context = {}) {
    this.context = { ...this.context, ...context };
    const suggestions = evaluateGuide(this.context);
    this.render(suggestions);
    return suggestions;
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
      const heading = document.createElement("h4");
      heading.textContent = suggestion.title;
      const message = document.createElement("p");
      message.textContent = suggestion.message;
      body.append(heading, message);
      article.append(rank, body);
      if (suggestion.target) {
        const button = document.createElement("button");
        button.type = "button";
        button.className = "guide-go-button";
        button.textContent = suggestion.actionLabel || "Ir al control";
        button.addEventListener("click", () => this.onNavigate(suggestion.target, suggestion));
        body.append(button);
      }
      this.list.append(article);
    });
  }
}

// Compatibility for external integrations that still import the old class.
export const ZenithGuide = IntelligentAssistant;
export { DEFAULT_RULES };
