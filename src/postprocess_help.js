const HELP_BY_ID = {
  "sl-level-black": ["Punto negro", "Define qué valor de 16 bits pasa a negro.", "Aumentarlo profundiza el fondo, pero puede recortar detalle oscuro.", "Súbelo sólo hasta antes de perder señal; confirma el porcentaje de sombras en el histograma."],
  "sl-level-mid": ["Medios tonos", "Redistribuye el brillo intermedio sin mover directamente los extremos.", "Valores mayores levantan medios; valores menores los oscurecen.", "Es el control más seguro para aclarar una imagen bien expuesta."],
  "sl-level-white": ["Punto blanco", "Define qué valor de 16 bits se representa como blanco.", "Bajarlo expande contraste, pero puede recortar prominencias o zonas brillantes.", "Vigila el indicador de luces y deja margen antes de 65,535."],
  "sl-deconv-sigma": ["Radio PSF Richardson–Lucy", "Estima el tamaño del desenfoque que la deconvolución intenta revertir.", "Un radio mayor actúa sobre detalle más grueso.", "Ajusta primero el radio con pocas iteraciones; un valor excesivo hincha estructuras."],
  "sl-deconv-iter": ["Iteraciones Richardson–Lucy", "Controla cuánto se repite la restauración adaptativa.", "Más iteraciones aumentan detalle y tiempo, pero también el riesgo de ruido o ringing.", "Sube en pasos cortos y valida con A/B al 100%."],
  "sl-vc-sigma": ["Radio de proyección estructural", "Selecciona la escala de la segunda restauración.", "Complementa Richardson–Lucy sobre estructura estable.", "Úsalo con poca intensidad después de ajustar la restauración principal."],
  "sl-vc-iter": ["Iteraciones de proyección", "Controla la fuerza de la restauración estructural secundaria.", "Más iteraciones hacen más visible la estructura y sus defectos.", "Normalmente bastan pocas iteraciones."],
  "sl-edge-strength": ["Intensidad edge-aware", "Extiende la protección de bordes a más escalas wavelet.", "Reduce halos del limbo al permitir realce local más selectivo.", "Valores muy altos pueden suavizar textura legítima."],
  "sl-auto-mask": ["Auto-máscara", "Detecta estructura probable y reduce el realce sobre ruido plano.", "Aumentarla protege fondos y regiones de bajo SNR.", "No sustituye una reducción de ruido; úsala para moderar el sharpening."],
  "sl-master-denoise": ["Master Denoise", "Reduce ruido antes del acabado fino.", "Afecta luminancia y, en color, puede tratar crominancia por separado.", "Evita borrar grano solar coherente; aumenta junto a Preservar detalle."],
  "sl-denoise-detail": ["Preservar detalle", "Decide cuánto detalle local se conserva durante el denoise.", "Más alto protege filamentos y bordes; más bajo suaviza con mayor fuerza.", "Comprueba zonas de textura fina al 100%."],
  "sl-denoise-chroma": ["Ruido de color", "Controla la reducción selectiva de variación cromática.", "No modifica el master mono y se deshabilita cuando no hay color.", "Úsalo antes de elevar saturación."],
  "sl-crisp": ["High Pass", "Aumenta contraste de detalle fino mediante una banda pasa-altas.", "Hace más nítidos bordes pequeños, incluyendo ruido.", "Mantén valores bajos y combina con Auto-máscara."],
  "sl-usm-amt": ["Smart Sharpen", "Amplifica detalle respecto a una versión suavizada.", "Es un acabado posterior a deconvolución y wavelets.", "Para el realce principal prefiere deconvolución; usa USM como toque final."],
  "sl-usm-rad": ["Radio Smart Sharpen", "Elige el tamaño espacial del detalle que se realza.", "Cero permite selección automática; radios mayores afectan estructura más gruesa.", "Relaciona el radio con el muestreo y la escala final de publicación."],
  "sl-adaptive-usm-min": ["Fuerza mínima adaptativa", "Define el USM aplicado a zonas oscuras del master sin procesar.", "Puede incluso suavizar el fondo si queda por debajo de la fuerza neutra.", "Protege prominencias débiles y fondos ruidosos."],
  "sl-adaptive-usm-max": ["Fuerza máxima adaptativa", "Define el USM aplicado a la señal brillante.", "Permite realzar el disco sin aplicar la misma fuerza al fondo.", "Aumenta sólo si la estructura sigue limpia."],
  "sl-adaptive-usm-threshold": ["Umbral adaptativo", "Marca la luminancia donde empieza la transición entre las dos fuerzas.", "Desplaza la zona protegida hacia señal más oscura o brillante.", "Se mide sobre el master original, no sobre la curva tonal."],
  "sl-adaptive-usm-transition": ["Transición adaptativa", "Controla la anchura de la mezcla entre fuerza mínima y máxima.", "Una transición amplia evita bordes visibles entre regiones.", "Prefiere transiciones suaves en discos solares completos."],
  "sl-lce-amt": ["Contraste local LCE", "Aumenta diferencias de luminancia dentro de vecindades locales.", "Hace visibles estructuras medias sin mover tanto el contraste global.", "El exceso produce aspecto duro y halos; compáralo con Claridad."],
  "sl-gamma": ["Gamma", "Redistribuye medios tonos mediante una curva de potencia.", "No debe confundirse con brillo: preserva los extremos normalizados.", "Usa Tono profesional para ajustes más selectivos."],
  "sl-sat": ["Saturación", "Escala la distancia de cada color respecto a su luminancia.", "No aplica a una fuente mono salvo que uses el módulo Solar mono.", "Evita saturar canales después del falso color."],
  "sl-contrast": ["Contraste", "Aplica una curva S alrededor de la luminancia pivote.", "Separa luces y sombras sin ser un simple aumento de brillo.", "Vigila el recorte en ambos extremos."],
  "sl-brightness": ["Brillo", "Suma o resta luminancia de forma global.", "Desplaza todo el rango y puede recortar rápidamente.", "Para una imagen oscura suele ser mejor Exposición o Medios tonos."],
  "sl-r-bal": ["Ganancia R", "Ajusta técnicamente el canal rojo.", "Cambia el balance lineal antes del acabado avanzado.", "En uso normal prefiere Temperatura, Matiz o el cuentagotas."],
  "sl-b-bal": ["Ganancia B", "Ajusta técnicamente el canal azul.", "Cambia el balance lineal antes del acabado avanzado.", "En uso normal prefiere Temperatura, Matiz o el cuentagotas."],
  "blend-slider": ["Mezcla de restauración", "Mezcla la base con wavelets, enfoque, denoise, LCE y deringing.", "Cero conserva la base; cien aplica toda la restauración calculada.", "Tono, curvas y color se aplican después de esta mezcla."],
  "sl-dr-rad": ["Sensibilidad de artefactos", "Define la escala usada para identificar anillos y halos.", "Valores mayores alcanzan defectos más anchos.", "Usa primero el análisis automático."],
  "sl-dr-dark": ["Anillos oscuros", "Reduce halos oscuros alrededor de transiciones fuertes.", "Una fuerza alta puede aplanar detalle real próximo al limbo.", "Aplícalo sólo donde el diagnóstico mida ringing."],
  "sl-dr-light": ["Anillos claros", "Reduce halos luminosos alrededor de bordes contrastados.", "Una fuerza alta puede apagar prominencias finas.", "Comprueba el limbo con A/B."],
  "sl-solar-filament": ["Recuperación de filamentos", "Refuerza estructura solar fina que supera el piso de ruido medido.", "Aumenta fibrillas claras y oscuras sin tocar el master original.", "No prueba detalle nuevo: usa A/B y baja la fuerza si aparecen patrones de ruido."],
  "sl-solar-radius": ["Escala de filamentos", "Selecciona el radio espacial de la estructura solar que se recupera.", "Valores pequeños buscan fibrillas finas; grandes, estructura más ancha.", "Ajusta según el muestreo de la captura."],
  "sl-solar-noise-guard": ["Protección de ruido solar", "Eleva la confianza exigida antes de realzar una estructura.", "Más alto reduce ruido con riesgo de omitir filamentos débiles.", "Sube este control antes de reducir la fuerza global."],
  "sl-solar-color-strength": ["Fuerza de falso color", "Mezcla la luminancia mono con el mapa cromático solar.", "Cero conserva gris; cien usa completamente la paleta elegida.", "El color es interpretativo y no altera el master científico mono."],
  "sl-solar-highlight-protect": ["Protección de altas luces", "Devuelve gradualmente las luces intensas hacia una luminancia neutra.", "Evita bordes amarillos saturados y conserva detalle brillante.", "Auméntalo para prominencias o limbos muy luminosos."],
};

const HELP_BY_ADVANCED = {
  exposure: ["Exposición", "Multiplica la luminancia en pasos EV.", "Un paso duplica o divide por dos la señal representada.", "Úsala antes de sombras y luces."],
  shadows: ["Sombras", "Ajusta regiones oscuras sin mover el negro puro.", "Valores positivos recuperan textura oscura.", "Evita levantar ruido de fondo."],
  highlights: ["Altas luces", "Ajusta zonas brillantes conservando mejor los medios.", "Valores negativos recuperan estructura clara.", "No reemplaza un punto blanco bien colocado."],
  whites: ["Blancos", "Modifica el extremo superior del rango tonal.", "Da presencia o recupera margen cerca del blanco.", "Vigila el porcentaje de recorte de luces."],
  blacks: ["Negros", "Modifica el extremo inferior sin ser un corte duro.", "Aporta profundidad o recupera señal baja.", "Vigila el porcentaje de recorte de sombras."],
  vibrance: ["Intensidad", "Aumenta primero los colores menos saturados.", "Es más selectiva que Saturación global.", "No aplica al master mono."],
  temperature: ["Temperatura", "Desplaza el balance entre azul y cálido.", "Corrige dominantes o define una intención cromática.", "Para neutralidad usa el cuentagotas."],
  tint: ["Matiz", "Equilibra el eje verde–magenta.", "Complementa Temperatura en el balance de blancos.", "No aplica al master mono."],
  texture: ["Textura", "Realza o suaviza estructura fina con protección de señal.", "Actúa por debajo de Claridad y por encima del ruido más fino.", "En solar mono compárala con Recuperación de filamentos."],
  clarity: ["Claridad local", "Ajusta estructura media mediante contraste local protegido.", "Aporta separación visual sin una curva global más fuerte.", "El exceso genera apariencia dura."],
  scnrGreen: ["Neutralizar verde", "Reduce sólo el exceso de verde respecto a rojo y azul.", "No elimina verde legítimo que no sobresale de la referencia.", "No aplica a datos mono."],
};

const MODULE_HELP = [
  ["#post-histogram-card", "Histograma y niveles", "Mide la vista procesada en 16 bits y controla negro, medios y blanco. Actualizar vuelve a medir; no cambia la imagen."],
  ["#sl-deconv-sigma", "Deconvolución Zenith", "Restaura desenfoque estimado mediante Richardson–Lucy y una proyección estructural opcional. Ajusta radio antes de fuerza."],
  ["#u1", "Detalles de alta frecuencia", "Distribuye realce subpíxel entre las bandas más finas. Puede recuperar microdetalle, pero también amplifica ruido."],
  ["#w1", "Wavelets multiescala", "Separa la imagen por tamaños para ajustar detalle y denoise de cada escala. Las capas enlazadas propagan una receta decreciente."],
  ["#post-detail-module", "Restauración y detalle fino", "Combina denoise, High Pass, USM, contraste local, textura y claridad. Trabaja de lo grueso a lo fino y compara cada paso."],
  ["#post-tone-module", "Tono profesional", "Controla exposición, sombras, luces, blancos y negros sobre luminancia normalizada de 16 bits."],
  ["#solar-mono-module", "Laboratorio Solar mono", "Convierte un master solar mono en un derivado tonal o de falso color con curva editable, inversión y recuperación protegida de filamentos."],
  [".color-module", "Colorimetría", "Ajusta balance, HSL y grading en fuentes a color. En mono se sustituye por el Laboratorio Solar de falso color."],
  [".atmospheric-module", "Corrección atmosférica", "Alinea R y B respecto a verde para reducir bordes de color. No aplica a una captura mono."],
  ["#artifact-repair-card", "Reducción de artefactos", "Mide halos, fringing y píxeles defectuosos antes de proponer una corrección reversible."],
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
  panel.setAttribute("aria-hidden", "true");
  panel.setAttribute("aria-label", "Ayuda del control");
  panel.innerHTML = `
    <header>
      <div><small>GUÍA CONTEXTUAL</small><h3 id="postprocess-help-title"></h3></div>
      <button type="button" class="post-help-close" aria-label="Cerrar ayuda"></button>
    </header>
    <p id="postprocess-help-summary"></p>
    <dl>
      <div><dt>Qué cambia</dt><dd id="postprocess-help-effect"></dd></div>
      <div><dt>Úsalo así</dt><dd id="postprocess-help-caution"></dd></div>
    </dl>
    <button type="button" class="post-help-assistant">
      <span>Consultar al Asistente inteligente</span>
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
      panel.querySelector(".post-help-close")?.focus?.({ preventScroll: true });
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
    label.append(button);
  });

  MODULE_HELP.forEach(([selector, title, summary]) => {
    const anchor = root.querySelector(selector);
    const module = selector.startsWith("#sl-") || selector === "#u1" || selector === "#w1"
      ? anchor?.closest(".control-group")
      : anchor;
    const heading = module?.querySelector(":scope > .group-header, :scope > .post-module-heading, :scope > summary, :scope > label");
    if (!module || !heading || heading.querySelector(".post-module-help")) return;
    const info = {
      title,
      summary,
      effect: "Todos sus cambios pertenecen al historial reversible del resultado activo.",
      caution: "Empieza por un preset o un valor pequeño y confirma la mejora con A/B al tamaño de salida.",
    };
    const button = helpButton(title);
    button.classList.add("post-module-help");
    button.addEventListener("pointerdown", (event) => event.stopPropagation());
    button.addEventListener("click", (event) => {
      event.preventDefault();
      event.stopPropagation();
      panel.open({ control: module, info }, button);
    });
    heading.append(button);
  });
  return panel;
}
