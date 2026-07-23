const HELP_BY_ID = {
  "sl-level-black": ["Punto negro", "Fija dónde empieza el negro.", "Al subirlo oscureces el fondo y puedes perder señal débil.", "Detente antes de aumentar el recorte de sombras."],
  "sl-level-mid": ["Medios tonos", "Aclara u oscurece la zona media.", "Cambia el brillo central sin mover directamente negro y blanco.", "Úsalo antes que Brillo para una corrección suave."],
  "sl-level-white": ["Punto blanco", "Fija dónde empieza el blanco.", "Al bajarlo ganas contraste, pero puedes quemar zonas brillantes.", "Conserva margen si aumenta el recorte de luces."],
  "sl-deconv-sigma": ["Radio PSF", "Define el tamaño del desenfoque a corregir.", "Más radio actúa sobre estructuras más grandes.", "Ajusta el radio antes de subir las iteraciones."],
  "sl-deconv-iter": ["Iteraciones de deconvolución", "Define la fuerza acumulada de la restauración.", "Más iteraciones revelan detalle, pero también ruido y halos.", "Súbelas poco a poco y compara al 100%."],
  "sl-vc-sigma": ["Radio estructural", "Elige la escala de la restauración secundaria.", "Refuerza estructura estable alrededor del tamaño elegido.", "Úsalo después de ajustar Richardson–Lucy."],
  "sl-vc-iter": ["Fuerza estructural", "Controla cuánto actúa la restauración secundaria.", "Valores altos hacen más visibles estructura y defectos.", "Normalmente bastan pocas iteraciones."],
  "sl-edge-strength": ["Protección de bordes", "Extiende el tratamiento anti-halo a más escalas.", "Protege limbos y transiciones de alto contraste.", "Demasiada protección puede suavizar textura real."],
  "sl-auto-mask": ["Auto-máscara", "Limita el enfoque a zonas con estructura probable.", "Al subirla protege fondos y señal con poco SNR.", "Auméntala si el ruido empieza a parecer detalle."],
  "sl-master-denoise": ["Reducción de ruido", "Suaviza ruido antes del acabado fino.", "Más fuerza limpia la imagen, pero puede borrar textura.", "Compénsalo con Preservar detalle."],
  "sl-denoise-detail": ["Preservar detalle", "Decide cuánto detalle sobrevive al denoise.", "Más alto conserva bordes y filamentos.", "Revísalo al 100% en la zona más fina."],
  "sl-denoise-chroma": ["Ruido de color", "Reduce manchas cromáticas sin suavizar igual la luminancia.", "Sólo actúa sobre fuentes a color.", "Aplícalo antes de aumentar saturación."],
  "sl-crisp": ["High Pass", "Refuerza bordes y textura muy fina.", "También puede amplificar ruido pequeño.", "Usa poca fuerza y apóyate en Auto-máscara."],
  "sl-usm-amt": ["Smart Sharpen", "Añade un enfoque final controlado.", "Amplifica detalle y ruido según el radio.", "Déjalo para después de deconvolución y wavelets."],
  "sl-usm-rad": ["Radio Smart Sharpen", "Elige el tamaño del detalle a enfocar.", "Cero usa el radio automático; más radio actúa más grueso.", "Evalúalo al tamaño final de salida."],
  "sl-adaptive-usm-min": ["Fuerza en sombras", "Define el USM de las zonas oscuras.", "Puede suavizar fondos o realzar prominencias débiles.", "Bájala si el fondo es ruidoso."],
  "sl-adaptive-usm-max": ["Fuerza en luces", "Define el USM de las zonas brillantes.", "Permite enfocar el disco sin castigar el fondo.", "Súbela sólo mientras el detalle siga limpio."],
  "sl-adaptive-usm-threshold": ["Umbral adaptativo", "Marca dónde cambia la fuerza entre sombras y luces.", "Desplaza la zona protegida por luminancia.", "Se calcula sobre el máster original."],
  "sl-adaptive-usm-transition": ["Transición adaptativa", "Suaviza el paso entre ambas fuerzas.", "Más anchura evita cambios visibles entre regiones.", "Usa una transición amplia en discos completos."],
  "sl-lce-amt": ["Contraste local", "Separa estructuras de tamaño medio.", "Da relieve sin cambiar tanto el contraste global.", "Bájalo si aparecen halos o un aspecto duro."],
  "sl-gamma": ["Gamma", "Aclara u oscurece principalmente los medios.", "Mantiene mejor los extremos que Brillo.", "Para más control usa Tono profesional."],
  "sl-sat": ["Saturación", "Aumenta o reduce la intensidad de todos los colores.", "No actúa sobre un máster mono sin falso color.", "Evita llevar algún canal al recorte."],
  "sl-contrast": ["Contraste", "Separa luces y sombras alrededor del punto medio.", "No es un aumento de brillo.", "Vigila recorte en ambos extremos."],
  "sl-brightness": ["Brillo", "Desplaza toda la luminancia por igual.", "Puede recortar negro o blanco con rapidez.", "Prefiere Exposición o Medios tonos cuando sea posible."],
  "sl-r-bal": ["Ganancia roja", "Aumenta o reduce el canal rojo.", "Cambia el balance lineal del color.", "Para uso normal prefiere Temperatura o el cuentagotas."],
  "sl-b-bal": ["Ganancia azul", "Aumenta o reduce el canal azul.", "Cambia el balance lineal del color.", "Para uso normal prefiere Temperatura o el cuentagotas."],
  "blend-slider": ["Mezcla de restauración", "Dosifica el bloque de detalle y reducción de ruido.", "0% conserva la base; 100% usa toda la restauración.", "Tono y color se aplican después."],
  "sl-dr-rad": ["Tamaño de artefacto", "Define el ancho de los halos a detectar.", "Más valor alcanza anillos más grandes.", "Empieza con el análisis automático."],
  "sl-dr-dark": ["Anillos oscuros", "Reduce halos oscuros junto a bordes fuertes.", "Demasiada fuerza puede aplanar detalle real.", "Actívalo sólo si el diagnóstico detecta ringing."],
  "sl-dr-light": ["Anillos claros", "Reduce halos brillantes junto a bordes fuertes.", "Demasiada fuerza puede apagar prominencias.", "Comprueba el limbo con A/B."],
  "sl-solar-filament": ["Recuperar filamentos", "Refuerza estructura solar que supera el ruido medido.", "Aumenta fibrillas claras y oscuras sin tocar el máster.", "Bájalo si el ruido forma patrones."],
  "sl-solar-radius": ["Escala de filamentos", "Elige el grosor de la estructura solar.", "Menos radio busca fibrillas finas; más radio, formas anchas.", "Ajústalo al muestreo de la captura."],
  "sl-solar-noise-guard": ["Protección de ruido", "Exige más confianza antes de realzar.", "Al subirla evita ruido, pero puede omitir filamentos débiles.", "Súbela antes de reducir la fuerza global."],
  "sl-solar-color-strength": ["Fuerza de falso color", "Mezcla gris y la paleta solar elegida.", "0 conserva gris; 100 usa todo el color.", "Es interpretativo y no cambia el máster mono."],
  "sl-solar-highlight-protect": ["Proteger altas luces", "Reduce color intenso en las zonas más brillantes.", "Conserva detalle del limbo y prominencias.", "Súbelo si el amarillo se satura."],
};

const HELP_BY_ADVANCED = {
  exposure: ["Exposición", "Aclara u oscurece en pasos EV.", "Un paso duplica o divide la luminancia.", "Ajústala antes de sombras y luces."],
  shadows: ["Sombras", "Recupera o profundiza zonas oscuras.", "Respeta mejor el negro que Brillo.", "No levantes también el ruido de fondo."],
  highlights: ["Altas luces", "Recupera o refuerza zonas brillantes.", "Afecta menos a los medios tonos.", "No sustituye un punto blanco correcto."],
  whites: ["Blancos", "Ajusta el extremo claro.", "Da presencia o recupera margen cerca del blanco.", "Vigila el recorte de luces."],
  blacks: ["Negros", "Ajusta el extremo oscuro.", "Da profundidad o recupera señal baja.", "Vigila el recorte de sombras."],
  vibrance: ["Intensidad", "Satura primero los colores apagados.", "Es más selectiva que Saturación.", "Sólo actúa sobre fuentes a color."],
  temperature: ["Temperatura", "Mueve el balance entre azul y cálido.", "Corrige dominantes o cambia la intención de color.", "Para neutralidad usa el cuentagotas."],
  tint: ["Matiz", "Equilibra verde y magenta.", "Completa el ajuste de Temperatura.", "Sólo actúa sobre fuentes a color."],
  texture: ["Textura", "Realza o suaviza detalle fino protegido.", "Trabaja una escala menor que Claridad.", "En solar compárala con Recuperar filamentos."],
  clarity: ["Claridad local", "Ajusta estructura de tamaño medio.", "Da separación sin una curva global fuerte.", "Bájala si el resultado se ve duro."],
  scnrGreen: ["Neutralizar verde", "Reduce únicamente el verde que sobresale.", "Conserva el verde que coincide con rojo y azul.", "Sólo actúa sobre fuentes a color."],
};

const MODULE_HELP = [
  ["#post-histogram-card", "Histograma y niveles", "Mide el resultado y ajusta negro, medios y blanco sin alterar el máster."],
  ["#sl-deconv-sigma", "Deconvolución Zenith", "Corrige desenfoque con Richardson–Lucy y una restauración estructural opcional."],
  ["#u1", "Detalles de alta frecuencia", "Ajusta las escalas más finas; puede reforzar detalle y también ruido."],
  ["#w1", "Wavelets multiescala", "Separa la imagen por tamaños para equilibrar detalle y ruido."],
  ["#post-detail-module", "Restauración y detalle fino", "Agrupa reducción de ruido, enfoque y contraste local."],
  ["#post-tone-module", "Tono profesional", "Controla el rango y la luminancia con sliders y una curva libre."],
  ["#tone-curve-free", "Curva tonal libre", "Permite mover puntos para ajustar sombras, medios y luces con precisión."],
  ["#solar-mono-module", "Laboratorio Solar mono", "Crea un derivado solar mono o coloreado con curva y protección de ruido."],
  [".color-module", "Colorimetría", "Ajusta balance, HSL y grading únicamente en fuentes a color."],
  [".atmospheric-module", "Corrección atmosférica", "Alinea rojo y azul con verde para reducir bordes de color."],
  ["#artifact-repair-card", "Reducción de artefactos", "Mide halos y defectos antes de aplicar una corrección reversible."],
];

function cleanLabelText(label, fallback) {
  const explicit = label?.querySelector(":scope > span")?.textContent?.trim();
  const raw = explicit || label?.textContent || fallback || "Control";
  return raw.replace(/\s+[-+]?\d+(?:[.,]\d+)?(?:\s?EV|%)?\s*$/, "").trim();
}

export function resolvePostprocessHelp(control) {
  if (!control) return null;
  const idHelp = HELP_BY_ID[control.id];
  if (idHelp) return { title: idHelp[0], summary: idHelp[1], effect: idHelp[2], caution: idHelp[3] };
  const advanced = HELP_BY_ADVANCED[control.dataset?.advancedControl];
  if (advanced) return { title: advanced[0], summary: advanced[1], effect: advanced[2], caution: advanced[3] };
  if (control.dataset?.hslComponent) {
    const component = control.dataset.hslComponent;
    return {
      title: `HSL · ${component === "hue" ? "Matiz" : component === "luminance" ? "Luminancia" : "Saturación"}`,
      summary: `Ajusta ${component === "hue" ? "el matiz" : component === "luminance" ? "la luminosidad" : "la saturación"} del sector cromático indicado sin afectar por igual a los demás colores.`,
      effect: "La transición entre colores vecinos es suave para evitar cortes visibles.",
      caution: "No aplica al master mono; usa falso color solar para ese flujo.",
    };
  }
  if (control.dataset?.gradeAmount !== undefined) {
    return {
      title: "Color grading por rango",
      summary: "Mezcla el color elegido sólo en sombras, medios o luces.",
      effect: "Aumentar la cantidad da una intención cromática localizada.",
      caution: "Úsalo después del balance y el HSL.",
    };
  }
  const id = control.id || "";
  const wavelet = id.match(/^([uwd])(\d)$/);
  if (wavelet) {
    const family = wavelet[1];
    const layer = Number(wavelet[2]);
    const names = { u: "Alta frecuencia", w: "Intensidad wavelet", d: "Reducción de ruido wavelet" };
    return {
      title: `${names[family]} · capa ${layer}`,
      summary: family === "d"
        ? `Reduce ruido en la escala ${layer}; las primeras capas corresponden al detalle más fino.`
        : `Ajusta la escala ${layer}; las primeras capas trabajan detalle más fino y las últimas estructura más ancha.`,
      effect: family === "d" ? "Un valor mayor filtra más esa banda." : "Un valor mayor amplifica el detalle de esa banda.",
      caution: "Mueve una escala por vez y valida textura real frente a ruido con A/B.",
    };
  }
  const label = control.closest("label") || control.closest(".control-row")?.querySelector("label");
  const title = cleanLabelText(label, control.getAttribute("aria-label") || control.id);
  return {
    title,
    summary: `Ajusta ${title.toLowerCase()} en la receta de postprocesado activa.`,
    effect: "La vista se recalcula sin modificar el master apilado.",
    caution: "Haz cambios pequeños, usa A/B y haz doble clic para volver al valor inicial.",
  };
}

function spriteIcon(id) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", "zas-icon");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
  use.setAttribute("href", `#${id}`);
  svg.append(use);
  return svg;
}

function createHelpPanel(onAskAssistant) {
  const panel = document.createElement("aside");
  panel.id = "postprocess-help-panel";
  panel.className = "postprocess-help-panel";
  panel.hidden = true;
  panel.tabIndex = -1;
  panel.setAttribute("role", "dialog");
  panel.setAttribute("aria-hidden", "true");
  panel.setAttribute("aria-labelledby", "postprocess-help-title");
  panel.innerHTML = `
    <header>
      <div class="post-help-heading-copy">
        <small>GUÍA CONTEXTUAL</small>
        <h3 id="postprocess-help-title"></h3>
      </div>
      <button type="button" class="post-help-close floating-panel-close" aria-label="Cerrar ayuda" title="Cerrar"></button>
    </header>
    <p id="postprocess-help-summary"></p>
    <dl>
      <div><dt>Efecto</dt><dd id="postprocess-help-effect"></dd></div>
      <div><dt>Consejo</dt><dd id="postprocess-help-caution"></dd></div>
    </dl>
    <button type="button" class="post-help-assistant">
      <span>Abrir en Asistente inteligente</span>
    </button>`;
  panel.querySelector(".post-help-close")?.append(spriteIcon("icon-cross"));
  panel.querySelector(".post-help-assistant")?.prepend(spriteIcon("icon-lightbulb"));
  document.body.append(panel);

  let active = null;
  let returnFocus = null;
  const close = () => {
    panel.hidden = true;
    panel.classList.remove("open");
    panel.setAttribute("aria-hidden", "true");
    returnFocus?.focus?.({ preventScroll: true });
  };
  panel.querySelector(".post-help-close")?.addEventListener("click", close);
  panel.querySelector(".post-help-assistant")?.addEventListener("click", () => {
    if (active) onAskAssistant?.(active);
    close();
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !panel.hidden) close();
  });

  return {
    open(payload, trigger) {
      active = payload;
      returnFocus = trigger;
      panel.querySelector("#postprocess-help-title").textContent = payload.info.title;
      panel.querySelector("#postprocess-help-summary").textContent = payload.info.summary;
      panel.querySelector("#postprocess-help-effect").textContent = payload.info.effect;
      panel.querySelector("#postprocess-help-caution").textContent = payload.info.caution;
      panel.hidden = false;
      panel.setAttribute("aria-hidden", "false");
      requestAnimationFrame(() => panel.classList.add("open"));
      panel.focus?.({ preventScroll: true });
    },
  };
}

function helpButton(label) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "post-control-help";
  button.setAttribute("aria-label", `Qué hace: ${label}`);
  button.title = `Qué hace ${label}`;
  button.append(spriteIcon("icon-lightbulb"));
  return button;
}

function placeControlHelpButton(label, button) {
  label.classList.add("post-help-label");
  const output = label.querySelector?.(":scope > output");
  if (output) {
    label.insertBefore(button, output);
  } else {
    label.append(button);
  }
}

function placeModuleHelpButton(heading, button) {
  if (heading.matches(".group-header")) {
    const title = heading.querySelector(":scope > h3");
    if (title) {
      const cluster = document.createElement("div");
      cluster.className = "post-help-title-cluster";
      heading.insertBefore(cluster, title);
      cluster.append(title, button);
      return;
    }
  }
  if (heading.matches("summary")) {
    const title = heading.querySelector(":scope > span");
    if (title) {
      const cluster = document.createElement("span");
      cluster.className = "post-help-title-cluster";
      heading.insertBefore(cluster, title);
      cluster.append(title, button);
      return;
    }
  }
  if (heading.matches(".post-module-heading, .tone-curve-heading")) {
    const title = heading.querySelector(":scope > div:first-child, :scope > strong");
    if (title) {
      const cluster = document.createElement("div");
      cluster.className = "post-help-title-cluster";
      heading.insertBefore(cluster, title);
      cluster.append(title, button);
      return;
    }
  }
  heading.classList.add("post-help-title-label");
  heading.append(button);
}

export function installPostprocessHelp({ root, onAskAssistant } = {}) {
  if (!root || root.dataset.helpInstalled === "1") return null;
  root.dataset.helpInstalled = "1";
  const panel = createHelpPanel(onAskAssistant);

  root.querySelectorAll('input[type="range"]').forEach((control) => {
    const info = resolvePostprocessHelp(control);
    if (!info) return;
    control.title = `${info.summary} ${info.caution}`;
    control.setAttribute("aria-label", control.getAttribute("aria-label") || info.title);
    // Dense HSL/grading grids keep one module-level button; every slider still
    // exposes its own explanation on hover/focus through title/aria-label.
    if (control.dataset?.hslIndex !== undefined || control.dataset?.gradeAmount !== undefined) return;
    const label = control.id
      ? root.querySelector(`label[for="${control.id}"]`)
      : control.closest("label") || control.closest(".control-row")?.querySelector("label")
        || control.closest(".v-slider")?.querySelector("span");
    if (!label || label.querySelector?.(".post-control-help")) return;
    const button = helpButton(info.title);
    button.dataset.helpFor = control.id || info.title;
    button.addEventListener("pointerdown", (event) => event.stopPropagation());
    button.addEventListener("click", (event) => {
      event.preventDefault();
      event.stopPropagation();
      panel.open({ control, info }, button);
    });
    placeControlHelpButton(label, button);
  });

  MODULE_HELP.forEach(([selector, title, summary]) => {
    const anchor = root.querySelector(selector);
    const module = selector.startsWith("#sl-") || selector === "#u1" || selector === "#w1"
      ? anchor?.closest(".control-group")
      : anchor;
    const heading = module?.querySelector(":scope > .group-header, :scope > .post-module-heading, :scope > .tone-curve-heading, :scope > summary, :scope > label");
    if (!module || !heading || heading.querySelector(".post-module-help")) return;
    const info = {
      title,
      summary,
      effect: "Sus cambios quedan guardados en el historial del resultado.",
      caution: "Empieza suave y confirma la mejora con A/B.",
    };
    const button = helpButton(title);
    button.classList.add("post-module-help");
    button.addEventListener("pointerdown", (event) => event.stopPropagation());
    button.addEventListener("click", (event) => {
      event.preventDefault();
      event.stopPropagation();
      panel.open({ control: module, info }, button);
    });
    placeModuleHelpButton(heading, button);
  });
  return panel;
}
