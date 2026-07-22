const DEFAULT_RULES = [
  {
    id: "no-source",
    when: (ctx) => !ctx.hasSource,
    level: "info",
    title: "Carga una fuente",
    message: "Selecciona un video, una secuencia o un resultado para comenzar.",
    target: "#btn-open-file",
  },
  {
    id: "needs-analysis",
    when: (ctx) => ctx.hasSource && !ctx.hasAnalysis && !ctx.hasResult,
    level: "next",
    title: "Analiza antes de apilar",
    message: "El análisis detecta calidad, movimiento y el mejor fotograma de referencia.",
    target: "#btn-analyze",
  },
  {
    id: "ready-to-stack",
    when: (ctx) => ctx.hasAnalysis && !ctx.hasResult,
    level: "next",
    title: "Listo para apilar",
    message: "Revisa el porcentaje y el método; después inicia el apilado.",
    target: "#btn-stack",
  },
  {
    id: "mono-color",
    when: (ctx) => ctx.hasResult && ctx.isMono,
    level: "info",
    title: "Resultado monocromo",
    message: "Los controles de color se mantienen neutros para preservar la señal mono.",
    target: "#panel-wavelets",
  },
  {
    id: "clipping",
    when: (ctx) => ctx.hasResult && (ctx.shadowClip > 0.01 || ctx.highlightClip > 0.01),
    level: "warning",
    title: "Hay recorte tonal",
    message: "Ajusta los puntos negro y blanco hasta recuperar detalle en sombras o altas luces.",
    target: "#post-histogram-card",
  },
  {
    id: "result-ready",
    when: (ctx) => ctx.hasResult,
    level: "success",
    title: "Resultado listo para revelar",
    message: "Comienza por niveles y tono; usa A/B para comprobar cada decisión.",
    target: "#panel-wavelets",
  },
];

export function evaluateGuide(context, rules = DEFAULT_RULES) {
  return rules.filter((rule) => rule.when(context)).map(({ when, ...rule }) => rule);
}

export class ZenithGuide {
  constructor({ panel, list, status, onNavigate } = {}) {
    this.panel = panel || null;
    this.list = list || null;
    this.status = status || null;
    this.onNavigate = onNavigate || ((target) => {
      const element = document.querySelector(target);
      element?.scrollIntoView?.({ behavior: "smooth", block: "center" });
      element?.focus?.({ preventScroll: true });
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
      const lead = suggestions.find((item) => item.level === "warning") || suggestions[0];
      this.status.textContent = lead?.title || "Sin sugerencias pendientes";
      this.status.dataset.level = lead?.level || "success";
    }
    if (!this.list) return;
    this.list.replaceChildren();
    suggestions.forEach((suggestion) => {
      const article = document.createElement("article");
      article.className = `guide-card guide-${suggestion.level}`;
      const heading = document.createElement("h4");
      heading.textContent = suggestion.title;
      const message = document.createElement("p");
      message.textContent = suggestion.message;
      article.append(heading, message);
      if (suggestion.target) {
        const button = document.createElement("button");
        button.type = "button";
        button.className = "guide-go-button";
        button.textContent = "Ir al control";
        button.addEventListener("click", () => this.onNavigate(suggestion.target));
        article.append(button);
      }
      this.list.append(article);
    });
  }
}

export { DEFAULT_RULES };
