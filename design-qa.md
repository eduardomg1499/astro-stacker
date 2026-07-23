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
