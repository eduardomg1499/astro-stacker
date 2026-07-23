# Verificacion visual: ajustes planetarios

- Fecha: 2026-07-18
- Estado probado: modal Configuracion General, idioma espanol, seccion Computo planetario.
- Viewport: 868 x 738 px.
- Referencia del fallo: `/var/folders/6q/3jzj3gmn19g0x_78vytvp_nh0000gn/T/TemporaryItems/NSIRD_screencaptureui_Z342iE/Captura de pantalla 2026-07-18 a la(s) 4.21.42 p.m..png`
- Captura corregida: `tmp/ui-settings-layout-after.png`
- Comparacion lado a lado: `tmp/ui-settings-layout-comparison.png`

## Revision

- La etiqueta y la ayuda de rigor AP ya no colapsan en una columna de una palabra.
- Los selectores de modo, calidad y decode permanecen dentro del panel sin solaparse.
- El texto conserva jerarquia, espaciado y colores del sistema visual existente.
- La fila pasa a una sola columna cuando el contenedor es estrecho.
- No se observaron recortes ni desbordamientos en el estado probado.

## Resultado final

PASSED

---

# Design QA — postprocesado 16-bit

Fecha: 2026-07-22

## Comparación visual en el mismo estado

- Estado: apilado planetario 960×612, preset `Detalle fino`, A/B disponible, ventana 1224×768.
- Antes: barra A/B recortada y controles superpuestos.
- Después: título y botón de fuente en la primera fila; deshacer, rehacer, A/B, referencia e historial en una segunda fila compacta y adaptable.
- Comparación conjunta: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/ab-toolbar-comparison.jpg`.
- Estado A/B activo final: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/ab-active-final.jpg`.

## Recorrido funcional validado

1. Importar y analizar una muestra planetaria color.
2. Generar 958 AP y apilar 12/24 fotogramas.
3. Aplicar `Detalle fino`: RL 18× / σ 1.05 y VC 3× / σ 0.8.
4. Activar A/B: referencia A a tamaño completo, división móvil y B actual.
5. Deshacer deja deconvolución en cero; rehacer restaura los cuatro parámetros.
6. Aplicar recomendación tonal: navega a Histograma y elimina el recorte medido.
7. Abrir, mover y ocultar Diagnóstico visual; los botones se leen como `Gráficas` y `Actualizar`.
8. Arrastrar el nodo R de corrección atmosférica: cambia R X/Y y la imagen en vivo; `Centrar` vuelve a 0.
9. Analizar AVI mono: `Normalizar colores` y `Alineación RGB` quedan apagados, deshabilitados y atenuados.
10. Configuración: rigor de validación AP visible como `Maximum (predeterminado)`.

## Evidencia adicional

- Asistente: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/assistant-intelligent.jpg`.
- Gráficas móviles: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/scopes-movable.jpg`.
- Adaptación mono: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/mono-controls-disabled.jpg`.
- Corrección atmosférica: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/atmospheric-drag.jpg`.

final result: passed

---

# Design QA — alineación responsive y precisión de curvas

Fecha: 2026-07-23

## Estado reproducido

- Aplicación nativa Tauri, ventana 1093×768.
- Fuente mono 960×612, 24 fotogramas, 977 AP y apilado 12/24.
- Barra lateral estrecha, resultado 16-bit, historial y A/B activos.

## Comparación visual conjunta

- Histograma, acciones, niveles y encabezado del módulo: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/layout-icons-histogram-comparison.png`.
- Sliders del Laboratorio Solar mono: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/layout-icons-solar-comparison.png`.
- Tarjetas y cierres del Asistente inteligente: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/assistant-cards-comparison.png`.

## Recorrido funcional validado

1. `Histograma RGB` conserva título y origen en una fila; `Gráficas` y `Actualizar` ocupan una segunda fila sin invadir el texto.
2. Negro, Medios, Blanco, tono avanzado y controles solares reservan columnas separadas para etiqueta, ayuda y valor; el track usa una fila completa.
3. Las bombillas SVG tienen un contenedor exacto de 24×24 px, sin el margen heredado del icono global y con el glifo centrado.
4. Los controles clásicos de deconvolución, detalle y tono muestran etiqueta y valor arriba y el deslizable debajo, sin colisiones al envolver texto.
5. En la curva tonal estrecha se pulsó en `(150, 625)` y el nuevo nodo quedó bajo el cursor en esa misma coordenada; la evidencia es `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/tone-curve-pointer-aligned-after.png`.
6. La edición creó `2/2 · Curva tonal · personalizada` y habilitó A/B, confirmando que el clic no fue sólo visual.
7. El cierre de cada recomendación ocupa una tercera columna de 30×30 px; al pulsarlo la tarjeta se elimina y las restantes se renumeran sin salto ni superposición.

## Revisión visual

- No se observan iconos sobre texto, valores sobre etiquetas, botones recortados ni cierres flotando sobre el contenido.
- Los textos largos envuelven dentro de su zona y los controles mantienen un objetivo cómodo para mouse o trackpad.
- La geometría de la curva usa el área visible real, incluidos sus bordes, tanto para dibujar como para convertir coordenadas del puntero.

final result: passed

---

# Design QA — iconos de ayuda, guía contextual y curva tonal

Fecha: 2026-07-23

## Comparación visual en el mismo componente

- Estado: apilado mono 960×612, 12/24 fotogramas, 977 AP, ventana nativa 1288×768.
- Iconos: la referencia mostraba la bombilla separada del título y del distintivo; la versión corregida agrupa título e icono y conserva `SUB-PIXEL` en una columna estable.
- Comparación conjunta de iconos: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/help-icons-comparison.png`.
- Guía: título y cierre ocupan columnas independientes; el texto se reduce a resumen, efecto y consejo.
- Comparación conjunta de la guía: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/context-help-comparison.png`.

## Recorrido funcional validado

1. Los iconos de Deconvolución, Detalles de alta frecuencia, Wavelets y módulos plegables aparecen junto al título que explican.
2. La guía contextual abre sin recortar el título ni desplazar el cierre; el botón de cierre conserva un objetivo de 32×32 px.
3. `Abrir en Asistente inteligente` cierra la guía y crea primero una recomendación del control consultado con `Volver al control`.
4. La nueva curva tonal libre muestra histograma, puntos editables, reset lineal y cuatro presets: Lineal, Contraste suave, Recuperar sombras y Proteger luces.
5. `Contraste suave` modifica la curva y el resultado, crea `2/2 · Curva tonal · Contraste suave` y habilita A/B con `Anterior` y `Actual`.
6. Deshacer restaura la línea neutra y el apilado original; rehacer recupera la curva y el resultado procesado.
7. La ruta mono mantiene Colorimetría y Corrección atmosférica deshabilitadas.

## Evidencia adicional

- Encabezados corregidos: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/help-icons-aligned.png`.
- Guía final: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/context-help-final.png`.
- Curva y A/B: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/tone-curve-ab-final.png`.
- Deshacer frente a rehacer: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/tone-curve-history-comparison.png`.

## Revisión visual

- No se observan bombillas aisladas, solapamientos ni distintivos fuera de su encabezado.
- Los textos extensos rompen línea dentro de su propia columna y no invaden el botón de cierre.
- Los iconos mantienen forma, tamaño y color del sprite SVG de Zenith.
- La nueva curva añade control profesional sin crear otra tarjeta desconectada del historial.

final result: passed

---

# Design QA — Laboratorio Solar mono y ayuda contextual

Fecha: 2026-07-22

## Comparación funcional con la referencia

- Referencia: curva tonal editable de ImPPG suministrada por el usuario.
- Implementación: Laboratorio Solar mono dentro del sistema visual de Zenith, ventana nativa 1093×768.
- Comparación conjunta: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/solar-reference-comparison.png`.
- La implementación conserva la interacción esencial de la referencia: histograma de fondo, puntos añadibles y movibles, eliminación con clic derecho, curva continua y reset lineal.
- Zenith añade presets, inversión, falso color, protección de luces, recuperación de filamentos y A/B sin copiar el estilo visual de otra aplicación.

## Recorrido funcional validado

1. Importar y analizar `lunar-mono.avi`; la ruta se detecta como `MONO`.
2. Generar 977 AP y apilar 12/24 fotogramas a 960×612.
3. Abrir la ayuda del módulo y la de `Recuperación de filamentos`; ambas muestran propósito, efecto, cautela y acceso al Asistente inteligente.
4. Consultar al Asistente: aparece primero el control consultado y después la receta solar calculada con el histograma activo.
5. Aplicar la receta automática: se activan curva, falso color y recuperación conservadora; la vista procesada cambia en vivo.
6. Arrastrar un punto de curva: la gráfica, la imagen, A/B y el historial avanzan a `Solar · curva personalizada`.
7. Aplicar `H-alpha invertido`, `Filamentos mono` y `Neutral`: los tres estados son visibles y Neutral devuelve curva lineal, fuerza cero y módulo inactivo.
8. A/B compara correctamente el paso anterior con el actual y conserva la división móvil.

## Evidencia

- Laboratorio y A/B: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/solar-lab-ab.png`.
- Ayuda por deslizable: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/solar-slider-help.png`.
- Preset invertido: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/solar-inverted-preset.png`.
- Recuperación mono: `/Users/edumg/.codex/visualizations/2026/07/22/019f898e-9b5e-7232-a050-44e36004925a/solar-filaments-mono.png`.

## Revisión visual

- No se observan solapamientos, cortes ni desbordamientos en el panel de 330 px.
- Los objetivos táctiles, sliders y puntos de curva funcionan con mouse y trackpad.
- Los estados activo, inactivo y deshabilitado se distinguen sin depender sólo del color.
- La ayuda flotante no tapa el control consultado y mantiene un CTA claro.
- La curva lineal es visual y matemáticamente neutra.

final result: passed
