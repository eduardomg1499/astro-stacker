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

## Extensión 2026-07-30 — Deconvolución y capas estelares

### Alcance implementado

- El recorrido guiado pasa a once operaciones científicamente ordenadas:
  Recortar, Fondo y gradiente, Astrometría, Canales y color, Restaurar PSF,
  Ruido lineal, Capas estelares, Estirar, Realzar detalle, Acabado y Exportar.
- La separación nativa crea ramas aditivas de Objeto y Estrellas, además de
  máscara y residual firmado. El Combinado permanece intacto hasta una
  recombinación explícita.
- Objeto y Estrellas pueden abrir de nuevo restauración PSF, reducción de
  ruido, estirado adaptativo, detalle y acabado. Volver a deconvolución después
  de un estirado reordena y recalcula la receta antes del dominio no lineal.
- La recombinación se bloquea mientras las ramas estén en dominios distintos y
  vuelve a habilitarse al reconciliarlas; un fallo restaura la última revisión
  válida.
- Se añadió un acceso persistente a Capas en el encabezado para evitar que una
  herramienta larga deje al usuario atrapado.

### QA interactiva del fixture

- URL verificada:
  `http://127.0.0.1:5173/?ux-fixture=poststack-editor&source=multiband&step=7`.
- Se creó la separación, se alternó Combinado → Objeto → Estrellas, se abrió
  Estirar en cada rama y se volvió al laboratorio mediante el acceso
  persistente.
- Con sólo Objeto procesado, “Crear combinado” quedó deshabilitado y explicó
  que los dominios eran distintos.
- Tras procesar también Estrellas, la recombinación se habilitó y publicó
  “Recombinación derivada actualizada”, conservando ambas ramas.
- Capturas del estado previo, edición de Objeto y combinado final:
  `benchmarks/design-qa-2026-07-30/deepsky-studio-star-layers-before-595x987.png`,
  `benchmarks/design-qa-2026-07-30/deepsky-studio-object-stretch-595x987.png` y
  `benchmarks/design-qa-2026-07-30/deepsky-studio-star-layers-recombined-595x987.png`.
- La inspección corresponde al fixture web Vite en el navegador integrado. No
  acredita todavía calidad de separación sobre FITS reales, el bundle Tauri ni
  hardware físico.

### Validación técnica

- Contratos deep-sky: 50/50.
- Prueba Rust de reentrada de deconvolución: 1/1.
- `cargo check`, `cargo fmt --check`, `node --check`, localizaciones JSON y
  `npm run build`: aprobados.
- El build conserva un aviso no bloqueante: el chunk principal minificado es
  mayor de 500 kB y debe separarse por carga diferida en una fase posterior.

### Límite científico explícito

El separador es un motor nativo determinista PSF/multiescala con máscara y
cierre numérico verificables; no se presenta como “perfecto”. La restauración
actual usa una PSF global circularizada. Antes de una afirmación de calidad
final debe validarse con un corpus FITS real que mida fotometría, halos,
residuo, estrellas saturadas, nebulosidad tenue y campos variables.

final result: passed

---

# Design QA — comparador 50/50 sincronizado de Cielo Profundo

Fecha: 2026-08-03

## Evidencia normalizada

- Fuente visual: `/var/folders/6q/3jzj3gmn19g0x_78vytvp_nh0000gn/T/TemporaryItems/NSIRD_screencaptureui_uFdCkJ/Captura de pantalla 2026-08-03 a la(s) 2.30.46 p.m..png`.
- Fuente: 1676 × 1028 px; implementación: 1678 × 1028 px; viewport CSS de la implementación: 1678 × 1028 a densidad 1.
- Implementación en el mismo estado de disponibilidad (referencia lista, máster aún pendiente): `benchmarks/design-qa-2026-08-03/deepsky-progress-waiting-split-1678x1028.png`.
- Implementación con ambas salidas publicadas: `benchmarks/design-qa-2026-08-03/deepsky-progress-split-1678x1028.png`.
- Comparación conjunta, con la fuente ajustada únicamente 2 px en horizontal para igualar dimensiones: `benchmarks/design-qa-2026-08-03/source-vs-waiting-split-3356x1028.jpg`.
- Estado compacto comprobado: viewport 720 × 900; el comparador midió 684 × 240 y no produjo desbordamiento horizontal.
- No fue necesario otro recorte enfocado: el comparador ocupa la región superior completa y etiquetas, divisor, imagen y control central se leen a tamaño original en la captura conjunta.

## Historial de comparación y correcciones

1. **[P1] La vista no eran dos paneles.** La fuente superponía dos imágenes al tamaño del lienzo y recortaba una con `clip-path`; una toma vertical o con otra proporción parecía más grande que el máster. Se sustituyó por dos viewports `minmax(0, 1fr)` reales, cada uno con `object-fit: contain`, centro 50/50 fijo y la misma transformación CSS.
2. **[P2] Primer pase con altura excesiva.** El primer render corregido conservaba demasiada banda negra por encima y debajo de imágenes 16:9. Se cambió el área a una relación total 2.85:1, con límites responsive, para que cada mitad se aproxime a la proporción de un frame astronómico sin recortar datos.
3. **Pass final.** La geometría medida fue 820 × 287.72 px, con paneles idénticos de 409 × 285.72 px y divisor a 409 px. No quedan diferencias P0, P1 o P2 atribuibles al comparador.

## Interacciones verificadas

- La rueda llevó ambas vistas de 100% a 156.8% con matrices CSS idénticas.
- Arrastrar desplazó ambas imágenes exactamente `55 px / 28.09 px`.
- Doble clic restauró zoom, paneo y anuncio accesible a 100% centrado.
- `+` llevó el zoom a 125% y `Home` lo restableció a 100%; flechas y `Shift+flechas` comparten el mismo controlador de paneo.
- El estado sin máster conserva la mitad derecha como espera explícita; al publicarse una salida no cambia la geometría ni el encuadre de la referencia.

## Superficies obligatorias

- **Tipografía y copy:** conserva jerarquía Zenith y etiquetas ANTES/ACTUAL; las instrucciones completas quedan disponibles al lector de pantalla sin añadir densidad visual.
- **Espaciado y layout:** dos columnas exactamente iguales, divisor central fijo y panel compacto sin huecos dominantes.
- **Color y tokens:** se reutilizan fondo, borde, foco cian y estados púrpura/verde existentes.
- **Calidad de imagen:** ninguna imagen se recorta; ambas usan `contain`, centro 50/50 y zoom/paneo compartidos.
- **Accesibilidad:** grupo enfocable, ayuda vinculada con `aria-describedby`, estado `aria-live`, botón de restablecimiento etiquetado y equivalentes completos por teclado.

## Consola y límites

El fixture Vite sólo registró los errores conocidos por ausencia del puente Tauri (`invoke`, `listen`, fuentes y updater); no apareció una excepción del comparador ni de su módulo geométrico. La evidencia valida el frontend web y sus interacciones. No sustituye una prueba del bundle Tauri, de FITS reales ni de GPU física.

## Findings

- No quedan hallazgos P0, P1 o P2 en los estados inspeccionados.

## Follow-up Polish

- [P3] En una validación nativa futura puede medirse la comodidad del gesto con trackpad de alta resolución sobre tomas de 60 MP; el límite actual 1×–8× ya impide perder por completo el campo.

final result: passed

---

# Design QA — Cielo Profundo Studio adaptativo

Fecha: 2026-07-30

## Fuente y comparación conjunta

- Diseño aprobado:
  `/Users/edumg/.codex/generated_images/019fa6cb-14f2-7d22-91a2-c055ea6189ef/call_5xLV9b11AgXAMHiFKp5VgO1c.png`.
- Implementación inspeccionada:
  `/tmp/astro-stacker-studio-final.png`.
- Comparación conjunta a la misma altura:
  `/tmp/astro-stacker-studio-reference-vs-implementation.png`.
- Estado multibanda Ha+OIII + SII+OIII:
  `/tmp/astro-stacker-studio-multiband.png`.
- Viewport y captura: 1280 × 720 px.

## Flujo científico validado

1. Recortar conserva juntos SCI, VAR, NEFF, DQ, cobertura y mapas y deja el
   máster fuente intacto.
2. Fondo y gradiente es una sola operación robusta: Automático o Muestras por
   puntos editables, protección de señal extensa, modelo y residual
   verificables. No existe una segunda extracción que reste dos veces la
   nebulosa.
3. Astrometría valida primero WCS/índice local, permite reinsertarlo y deja
   Gaia en línea como autorización explícita.
4. Canales y color es condicional: PCC para RGB broadband con WCS; mono no
   inventa canales; dual-band y narrowband abren la galería adecuada.
5. Ruido trabaja antes del estirado sobre float32 lineal.
6. El estirado GHS adaptativo es opcional y reversible.
7. Detalle y acabado permanecen bloqueados hasta existir una rama estirada.
8. Exportar está siempre alcanzable para conservar una salida lineal aunque
   se omitan los pasos no lineales.

## Sesiones y paletas

- El fixture multibanda detectó Ha+OIII + SII+OIII sin ofrecer PCC.
- Antes de crear paletas reconcilió las dos señales OIII y mostró ese estado
  explícitamente.
- La galería ofreció HOO natural, HOO teal, SHO, HSO y SOO con previews bajo
  el mismo STF.
- Foraxx no se anuncia porque todavía no existe una implementación científica
  validada.
- Elegir una tarjeta sólo cambia la preview; Aplicar crea una revisión
  derivada; Volver al producto fuente reaparece sólo mientras existe una
  paleta aplicada.

## Interacciones recorridas

- Los nueve pasos abren su panel y Exportar no queda oculto ni interceptado.
- Esencial y Experto alternan sin cambiar la receta científica.
- Se generaron 140 muestras de fondo, se aplicó el modelo y se alternaron
  Correcto/Modelo/Residual.
- Se resolvió WCS local y PCC quedó habilitado; aplicarlo dos veces mantuvo el
  mismo número de operaciones.
- Se aplicaron ruido, GHS, detalle y acabado en orden.
- Deshacer redujo la receta de 8 a 7 operaciones; Rehacer restauró 8.
- A/B, Studio completo y panel acoplado respondieron.
- Cerrar y abrir desde `Editor de Cielo Profundo` restauró la misma sesión y
  el paso activo.
- El diálogo `Entendido` se cerró y devolvió inmediatamente la interacción al
  Studio.

## Hallazgos corregidos durante QA

1. **[P1] botón Exportar interceptado.** El panel de diagnóstico global tenía
   una capa superior invisible sobre el último paso. El Studio y sus overlays
   científicos ahora tienen una prioridad de capa propia por debajo de
   alertas críticas y por encima de asistentes/diagnósticos.
2. **[P2] riel de pasos convertido en mosaico.** Reglas compactas históricas
   embebidas en `index.html` vencían al layout nuevo. El modo Studio fuerza
   ahora una cronología vertical estable; el modo móvil conserva nueve accesos
   compactos.
3. **[P2] CTA inferior fuera del viewport.** Un estilo global expandía
   `Aplicar recomendación` más allá del lienzo. Inspector, explicación y CTA
   tienen ahora límites flexibles y no producen overflow.
4. **[P2] retorno de paleta fantasma.** `Volver al producto fuente` se
   mostraba en el fixture aun sin paleta aplicada. Ahora aparece y desaparece
   según el estado real.
5. **[P2] copy obsoleto.** La traducción aún describía una malla fija. Ahora
   explica correctamente la única operación de fondo, sus dos modos y el
   bloqueo de modelos inestables.
6. **[P1] mono aislado tratado como varios canales.** Una etiqueta Ha+OIII en
   un único máster mono ya no fabrica planos espectrales. La galería permanece
   bloqueada hasta añadir másteres Ha/OIII/SII realmente distintos; reutilizar
   el mismo archivo con dos nombres también se rechaza.
7. **[P1] preview de paleta obsoleta.** Cambiar perfil instrumental, peso OIII
   o contaminación Ha→OIII invalida inmediatamente galería y botón Aplicar;
   hay que recalcular antes de publicar la revisión.
8. **[P1] exclusión de fondo no monótona.** La protección de nebulosa ahora
   suma sus exclusiones a DQ/cobertura en vez de reemplazarlas. Median/MAD
   ignora píxeles ya excluidos y una geometría DQ/cobertura incoherente falla
   cerrada.
9. **[P2] sesión fantasma/reemplazo accidental.** Si el backend ya no conserva
   el máster, se limpia el estado web antes de abrir otro. Cambiar de FITS/TIFF
   con una sesión activa requiere confirmación y aclara que ningún archivo se
   elimina ni sobrescribe.
10. **[P2] identidad de productos derivada.** Paleta Studio y Fuente
    preservada tienen tipos, descripciones y etiquetas propias; ya no aparecen
    como Classic. Aplicar o restaurar deja exactamente un producto Principal y
    sincroniza ambos selectores. Inspeccionar otro producto sólo cambia
    “Activo”, no reescribe el Principal elegido por la receta.

## Hardening automatizado posterior a la captura

- 76/76 pruebas Node.
- 49/49 contratos deep-sky y 15/15 contratos A/B.
- 677/677 pruebas Rust ejecutadas, 0 fallidas y 29 ignoradas; incluye pruebas
  específicas para unión DQ/cobertura/objetos y fallo cerrado de geometría de
  soporte.
- `node --check`, `cargo check`, build de producción y `git diff --check`
  aprobados.

## Consola y límite de evidencia

La navegación no produjo excepciones propias del Studio. El fixture Vite
conserva errores conocidos de inicialización porque no existe el puente Tauri
para `invoke/listen`, fuentes, updater y GPU. La evidencia visual valida el
frontend y estados simulados; las pruebas Rust validan contratos sintéticos.
No acredita por sí sola un FITS real, GPU física ni superioridad frente a
PixInsight/APP.

## Findings

- No quedan hallazgos P0, P1 o P2 en los estados inspeccionados.

final result: passed

---

# Corrección de estados EIDR e iconografía · 2026-07-29

## Evidencia y normalización

- Fuente visual:
  `/var/folders/6q/3jzj3gmn19g0x_78vytvp_nh0000gn/T/TemporaryItems/NSIRD_screencaptureui_rIFEr5/Captura de pantalla 2026-07-29 a la(s) 6.02.42 p.m..png`.
- Fuente original: 2388 × 1856 px; normalizada a 1194 × 928 px para
  comparar la captura nativa @2x con un viewport CSS 1194 × 928 a densidad 1.
- Implementación:
  `/Users/edumg/Desktop/SOFTWARE by EMG/astro-stacker PROYECTO/astro-stacker/benchmarks/design-qa-2026-07-29/deepsky-progress-eidr-final-1194x928.png`.
- Comparación conjunta:
  `/Users/edumg/Desktop/SOFTWARE by EMG/astro-stacker PROYECTO/astro-stacker/benchmarks/design-qa-2026-07-29/source-eidr-vs-final-2388x928.png`.
- Región enfocada de la cronología e iconos:
  `/Users/edumg/Desktop/SOFTWARE by EMG/astro-stacker PROYECTO/astro-stacker/benchmarks/design-qa-2026-07-29/source-vs-final-eidr-steps-focus-728x560.png`.
- Estado: Classic Drizzle ×2, NebulaFusion y STRUCT preservados; EIDR activo
  en canal 1/3. La fuente mostraba `6/6` y 45% por el defecto auditado; la
  implementación muestra intencionalmente `3/6`, Integración activa y 86%
  global tras ponderar la cuarta rama.

## Historial de comparación

1. **Pass 1 — [P1] proceso global declarado terminado durante EIDR.** La
   fuente mostraba las seis etapas en verde aunque EIDR seguía activo. Se
   separó el `complete` interno de cada producto del cierre global; sólo el
   retorno del coordinador publica `6/6`.
2. **Pass 1 — [P2] iconos con desplazamiento óptico.** El margen global de
   `.zas-icon` desplazaba checks y pendientes. Se anuló dentro de la cronología
   y se reemplazó el marcador pendiente malformado por un reloj de arena
   consistente. La región enfocada confirma centros coincidentes.
3. **Pass 1 — [P2] referencia animada recortada.** La vista usaba `cover` y el
   cielo CSS sólo poblaba una franja de 92 px. La referencia real ahora usa
   `contain`, aparece completa en el lado de espera con una animación tenue y
   la tarjeta se desplaza al borde inferior sin cubrir el encuadre principal.
4. **Pass 2.** No quedan hallazgos P0, P1 o P2 en el estado inspeccionado.

## Superficies obligatorias

- **Tipografía:** jerarquía, pesos, truncados y cifras monoespaciadas se
  conservan; no cambió la densidad del panel.
- **Espaciado y layout:** los centros de icono y círculo coinciden; no existe
  overflow horizontal (`scrollWidth` = `clientWidth` = 1194).
- **Color:** verde queda reservado a etapas/productos terminados; púrpura
  identifica exclusivamente la etapa y rama activas.
- **Imagen:** la referencia 1672 × 941 se renderiza con `object-fit: contain`;
  no se recorta ni se presenta como máster.
- **Copy:** `Calculando…` sustituye el ETA ficticio `00:00`; GPU sin muestra
  aparece como `—`.
- **Accesibilidad y movimiento:** estados textuales acompañan el color y las
  animaciones se desactivan con `prefers-reduced-motion`.

## Interacciones y consola

- Se verificaron el fixture `?ux-fixture=progress&state=eidr`, la progresión
  global, la rama activa y las métricas de recursos.
- Las excepciones de consola observadas proceden del fixture web sin puente
  Tauri (`listen/transformCallback`) y del updater bloqueado por CORS; no se
  registró una excepción originada por la cronología o la referencia.

## Findings

- No quedan hallazgos P0, P1 o P2.

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

---

# Deep-sky design QA

## Scope and source

- Source reference: `/Users/edumg/Library/Containers/com.edumg.IslandPro/Data/Library/Caches/com.apple.SwiftUI.Drag-2EF6E43A-9820-4B02-9340-AB248E850CE6/Captura de pantalla 2026-07-27 a la(s) 9.29.55 p.m..png`
- Source dimensions: 1892 × 238 px.
- Visual intent retained: dark pill navigation, amber step numbers, nine-step progression and low visual noise.
- Implementation state: Vite-only `?ux-fixture=poststack-editor`, step 2 “Quitar gradiente”, with no fabricated FITS result or native backend response.
- Primary viewport: 1600 × 900 CSS px. The in-app browser capture surface produced a 1600 × 761 px screenshot.
- Responsive viewport: 480 × 800 CSS px. The focused content capture is 480 × 270 px.

## Evidence

- Full implementation: `/tmp/astro-stacker-deepsky-implementation-2026-07-27/poststack-editor-1600x900.png`
- Focused nine-step editor: `/tmp/astro-stacker-deepsky-implementation-2026-07-27/poststack-editor-focused-1600.png`
- Mobile editor: `/tmp/astro-stacker-deepsky-implementation-2026-07-27/poststack-editor-480x800-final.png`
- Focused source/implementation comparison: `/tmp/astro-stacker-deepsky-implementation-2026-07-27/comparison-focused-final.png`
- Full source/implementation comparison: `/tmp/astro-stacker-deepsky-implementation-2026-07-27/comparison-full-final.png`
- Multi-night reuse state: `/tmp/astro-stacker-deepsky-implementation-2026-07-27/multinight-board-reuse-1600x900-final.png`

## Iteration history

1. Replaced the single overflowing row with responsive pill navigation matching the supplied reference.
2. Fixed step buttons inheriting full-width global button styles.
3. Restored keyboard focus after every editor re-render.
4. Prevented long select labels from widening the 480 px layout.
5. Increased small labels and help text without returning to the original information density.
6. Fixed blocked wizard navigation so the active tab, page, requirement and footer always indicate the same step.
7. Replaced the one-day flat-reuse UI gate with full-signature preliminary matching; the backend now confirms normalized batch stability.

P0, P1 and P2 visual findings found during this pass were fixed. No unresolved visual blocker remains in the inspected states.

## Interaction and accessibility checks

- All nine editor steps respond to click.
- Arrow Left/Right, Home and End move through steps and preserve focus.
- Undo, redo, reset and A/B expose deterministic disabled/enabled state from the revision cursor.
- The empty deep-sky wizard cannot advance or run without lights.
- Opening the deep-sky dialog sets the background `inert` and `aria-hidden`; focus is trapped and restored on close.
- Essential is the default; Expert persists through `localStorage`; both use the same request builder.
- Classic, NebulaFusion SCI, STRUCT and EIDR can be selected together or independently. STRUCT-only keeps its hidden NebulaFusion dependency without selecting SCI as a visible output.
- Comet mode exposes automatic detection, first/middle/last confirmation and separate layer retention.
- A 480-flat fixture exposes every file through pagination and search; no item disappears after index 120.
- The multi-night board assigned the March Ha+OIII flat batch to the compatible May group in one action and displayed “Reutilizable validado”.
- Responsive layout had no document-level horizontal overflow at 480, 760, 1280 or 1892 px.
- States use text and icons in addition to colour.

## Console and environment

The web fixture logs expected missing-Tauri-bridge messages for window metadata, native invoke/listen, bundled fonts, GPU and updater services. They are harness-only limitations: the fixture intentionally does not emulate native commands. No JavaScript error was produced by editor navigation, product selection, comet controls, pagination/search or the multi-night assignment interaction. Native Rust checks and tests are recorded separately from this visual QA.

## Final result

passed

---

# Segunda auditoría — cielo profundo, combinación LRGB/SHO y cometas

Fecha: 2026-07-28

## Alcance y evidencia

- Checkout: `codex/Auditoria-DS`, con los cambios de esta unidad aún sin publicar.
- Referencias del usuario:
  - `/Users/edumg/.codex/attachments/1afc41e5-a42c-4ae1-be69-d47468f7796a/image-1.png`
  - `/Users/edumg/.codex/attachments/1afc41e5-a42c-4ae1-be69-d47468f7796a/image-2.png`
- Cabecera corregida: `/tmp/astro-stacker-deepsky-audit-2026-07-28/01-wizard-header-fixed-1280x720.png`.
- Combinador corregido: `/tmp/astro-stacker-deepsky-audit-2026-07-28/04-channel-combine-sprite-fixed-1280x720.png`.
- Banda ancha: PCC habilitado y HOO bloqueado: `/tmp/astro-stacker-deepsky-audit-2026-07-28/05-pcc-spcc-gate-1280x720.png`.
- Dual-band/narrowband: PCC bloqueado y HOO habilitado: `/tmp/astro-stacker-deepsky-audit-2026-07-28/06-pcc-narrowband-blocked-1280x720.png`.
- Entorno visual: fixture Vite de QA a 1280×720. Valida DOM, foco, teclado, distribución y estados; no acredita lectura o combinación real de FITS ni el comportamiento de una GPU física.

## Recorrido auditado

1. **Datos.** Bloquea el avance sin lights y conserva acceso a todos los archivos mediante búsqueda y paginación de 120 elementos.
2. **Calibraciones.** Una noche distinta ya no autoriza un flat por proximidad temporal. La reutilización exige asignación explícita, firma completa y fingerprint estable; un forzado sigue marcado como no seguro.
3. **Calidad.** Esencial mantiene una conclusión y acción principal; Experto expone el diagnóstico ampliado sin cambiar el request científico.
4. **Método.** Classic, NebulaFusion SCI, STRUCT y EIDR son seleccionables en cualquier subconjunto. STRUCT declara su dependencia de NebulaFusion Full.
5. **Apilar.** La confirmación permanece bloqueada sin lights y toda incompatibilidad forzada elimina la elegibilidad científica de NF/EIDR.
6. **Cometas.** Exige `DATE-OBS`, `EXPTIME` e instante medio; confirma tres observaciones y proyecta la trayectoria a la cuadrícula efectiva del stack antes de componer una sola transformación.
7. **Resultado.** El máster lineal fuente es inmutable; gradiente, astrometría y PCC Gaia se publican como revisiones reemplazables con undo/redo/reset y A/B.

## Correcciones verificadas

- El diálogo anidado LRGB/SHO ya no hereda el estado `inert` del asistente. Recibe foco, confina la navegación, responde al cierre y devuelve el foco al botón que lo abrió.
- El fondo y el asistente quedan fuera del árbol de accesibilidad mientras el combinador está activo.
- La cabecera separa título, resumen, conmutador Esencial/Experto y cinco pasos; ya no mezcla iconos, textos ni botones en la misma línea estrecha.
- El combinador acepta únicamente másteres mono lineales FITS/TIFF. Rechaza RGB y CFA en backend en lugar de convertirlos silenciosamente a luminancia.
- La combinación LRGB usa una transferencia lineal firmada `RGB' = RGB + (L − Y)`; no recorta los lóbulos negativos ni aplica gradiente, neutralización o SCNR por defecto.
- El registro entre másteres de canal usa Lanczos-3 en el interior y bilineal sólo en el borde; la receta declara `lanczos3_with_bilinear_border` y conserva cobertura/DQ.
- El resultado combinado publica un `DeepSkyResult` float32 lineal y una vista STF temporal; no inventa `VAR/NEFF` cuando la covarianza de los canales no está disponible.
- Elegir STRUCT muestra ahora el plano STRUCT real; el SCI de NebulaFusion permanece como dependencia lineal disponible y no vuelve a etiquetarse como STRUCT.
- Recalcular el gradiente vuelve a ajustar contra el máster fuente inmutable. Ya no ajusta el segundo modelo sobre el residual anterior ni restaura accidentalmente el gradiente original.
- PCC Gaia queda deshabilitado en UI y bloqueado de nuevo en backend cuando `captureMode`, filtro o paleta indican narrowband/dual-band. Las paletas SHO/HOO se registran en la receta de combinación.
- HOO queda deshabilitado en UI y rechazado de nuevo en backend para RGB de banda ancha, mono narrowband, combinaciones SII+OIII y másteres que ya son paletas combinadas. Sólo se habilita con contrato OSC Ha+OIII/dual-band explícito.
- La fotometría PCC mide parches PSF 9×9 directamente en el máster intercalado y fondos sobre muestras acotadas; evita tres planos RGB temporales completos (aproximadamente 720 MB para 60 MP).
- Los residuos cometarios permanecen firmados, los píxeles sin cobertura son `NaN + DQ`, y las capas cometa/combinada ya no heredan una varianza falsa del stack estelar.
- La receta de cometas declara el método real `comet-cross-trajectory-residual` y el estado `experimental`; no lo etiqueta como sustracción completa de modelo estelar.

## Salud actual

### Fortalezas

- Contratos científicos explícitos para `SCI/VAR/NEFF/DQ`, cobertura, degradaciones y receta reproducible.
- Rechazo temprano de combinaciones de calibración inseguras y de canales no mono.
- Cachés versionados de calibración y registro reutilizables entre productos.
- Historial post-stack reconstruido desde una fuente inmutable; PCC repetida no acumula ganancias.

### Riesgos que permanecen

- **P0 de publicación científica:** la separación de cometas sigue siendo experimental. Usa rechazo cruzado por trayectoria y una máscara de coma, no una sustracción estelar completa; falta validar colas extensas, tránsito sobre estrellas, rotación de campo y movimiento no lineal con verdad conocida y corpus real.
- **P1 de rendimiento:** `run_deepsky_stack` invoca el integrador una vez por producto. Los cachés evitan parte de la lectura/calibración/registro, pero NF Full + STRUCT puede repetir una integración científicamente costosa.
- **P1 de escalabilidad UI:** todos los archivos son localizables, pero el control es paginación fija, no virtualización de filas. Falta el ensayo objetivo de 5.000 tomas y 60 MP.
- **P1 de mantenibilidad/carga:** `src/main.js` tiene 19.013 líneas, `deepsky.rs` 24.902 y el chunk principal construido ocupa 590,15 KB. Faltan división por módulos y carga bajo demanda.
- **P1 de interacción cometaria:** la confirmación muestra coordenadas, pero aún necesita marcador registrado sobre la imagen y corrección directa por clic/arrastre.
- **P1 color:** el módulo actual es PCC Gaia, correctamente rotulado; SPCC real sigue bloqueado hasta disponer de Gaia DR3 XP y perfil instrumental espectral completo.
- **P1 competitivo:** no existe todavía un corpus A/B igualado contra PixInsight WBPP y APP con cinco corridas frías/calientes. No se puede afirmar superioridad ni perfección.

## Regresión automatizada

- Node: 62 aprobadas, 0 fallidas.
- Contratos deep-sky: 36 aprobadas, 0 fallidas.
- Puerta A/B: 15 aprobadas, 0 fallidas.
- Rust completo: 645 aprobadas, 0 fallidas, 29 ignoradas.
- Rust cometas dirigido: 5 aprobadas, 0 fallidas.
- Rust PCC/gradiente/registro/HOO dirigido: 5 correcciones nuevas aprobadas, 0 fallidas.
- `cargo check --bin astro-stacker`: aprobado.
- `npm run build`: aprobado; conserva advertencia por chunk principal mayor de 500 KB.
- `git diff --check`: aprobado.

Las 29 pruebas Rust ignoradas incluyen datasets reales, FFmpeg y GPU física. Por tanto, la regresión automatizada demuestra consistencia de contratos y casos sintéticos, no paridad de hardware ni superioridad científica externa.

## Resultado

passed with documented P0/P1 validation gates

---

# Seguimiento — calibraciones multi-noche y coherencia del asistente

Fecha: 2026-07-28

## Evidencia visual

- Selector de flats, darks, dark-flats y bias, con comprobación científica acotada:
  `/tmp/astro-stacker-deepsky-audit-2026-07-28-followup/18-next-night-user-verified.png`.
- Calidad con decisión reversible por light y CTA exacto:
  `/tmp/astro-stacker-deepsky-audit-2026-07-28-followup/15-quality-actionable-guide.png`.
- Método Esencial sin “Personalizado”:
  `/tmp/astro-stacker-deepsky-audit-2026-07-28-followup/13-essential-method-clean.png`.
- Classic, NebulaFusion SCI, STRUCT y EIDR seleccionados en paralelo:
  `/tmp/astro-stacker-deepsky-audit-2026-07-28-followup/14-all-products-struct-primary.png`.

## Recorrido verificado

1. Cada grupo de lights expone los cuatro roles de calibración. Los candidatos se
   agrupan por sesión y firma; no existe un lote global ambiguo de cientos de
   archivos.
2. Un flat de otra noche puede quedar “Comprobado por ti” sólo cuando no existe
   una incompatibilidad física conocida, el usuario confirma que el equipo no
   cambió y el backend obtiene un fingerprint estable de al menos tres flats.
3. Filtro, geometría, CFA, gain/ISO, binning u offset incompatibles continúan
   bloqueados; el check no los convierte en científicos.
4. Dark-flat y bias seleccionados se reflejan en la decisión efectiva y en el
   resumen por sesión. Omitir una calibración requerida fuerza Classic aunque la
   política original fuera Strict.
5. La guía abre la etapa exacta y ofrece “Seleccionar o descartar lights”; cada
   light dudoso tiene una acción reversible. No se elimina el archivo fuente.
6. Esencial oculta “Personalizado”. Una receta avanzada guardada se resume y
   remite a Experto para editarla; ambos modos construyen el mismo request.
7. Mientras el diálogo está abierto, los 23 hermanos HTML del modal tienen
   `inert` y `aria-hidden`; no queda ningún control enfocable fuera. El asistente
   global se oculta y el asistente interno permanece visible.

## Regresión

- Node: 62 aprobadas, 0 fallidas.
- Contratos deep-sky: 37 aprobadas, 0 fallidas.
- Puerta A/B: 15 aprobadas, 0 fallidas.
- Rust: 647 aprobadas, 0 fallidas, 29 ignoradas.
- `cargo check`, build de producción, JSON de idiomas y `git diff --check`:
  aprobados.

## Límite de la evidencia

El fixture Vite valida distribución, estados, foco y construcción de requests.
Las pruebas Rust validan contratos y casos sintéticos. No acreditan un corpus FITS
real, GPU física ni superioridad frente a PixInsight/APP. El bundle conserva un
chunk principal de 602,15 kB que requiere división posterior.

final result: passed with real-data and physical-GPU validation pending

---

# Design QA — centro de apilado y relevo al editor de cielo profundo

Fecha: 2026-07-29

## Fuente visual y estado comparable

- Fuente visual elegida (opción 2):
  `/Users/edumg/.codex/generated_images/019fa6cb-14f2-7d22-91a2-c055ea6189ef/call_yhYpiQONVzVqkwgTfpwSqd7p.png`.
- Dimensiones de la fuente: 1610 × 977 px.
- Implementación comparable: fixture Vite `?ux-fixture=progress`, apilado en
  curso al 62%, cuatro productos y Classic Drizzle ×2.
- Captura final comparable:
  `/Users/edumg/Desktop/SOFTWARE by EMG/astro-stacker PROYECTO/astro-stacker/benchmarks/design-qa-2026-07-29/deepsky-progress-running-1600x1000-pass2.png`.
- Viewport CSS: 1600 × 1000 px; captura: 1600 × 1000 px;
  `devicePixelRatio: 1`.
- Normalización: la fuente se ajustó por `contain` a un lienzo 1600 × 1000,
  sin recorte ni deformación; la implementación se conservó 1:1.
- Comparación conjunta final:
  `/Users/edumg/Desktop/SOFTWARE by EMG/astro-stacker PROYECTO/astro-stacker/benchmarks/design-qa-2026-07-29/reference-vs-running-pass2.png`.

## Evidencia adicional

- Comparación conjunta inicial:
  `/Users/edumg/Desktop/SOFTWARE by EMG/astro-stacker PROYECTO/astro-stacker/benchmarks/design-qa-2026-07-29/reference-vs-running-pass1.png`.
- Región enfocada de productos, recursos, diagnóstico y salida:
  `/Users/edumg/Desktop/SOFTWARE by EMG/astro-stacker PROYECTO/astro-stacker/benchmarks/design-qa-2026-07-29/focus-products-resources-pass2.png`.
- Resultado terminado con los nueve accesos al editor:
  `/Users/edumg/Desktop/SOFTWARE by EMG/astro-stacker PROYECTO/astro-stacker/benchmarks/design-qa-2026-07-29/deepsky-progress-complete-1600x1000-pass2.png`.
- Editor con comparación original/revisión:
  `/Users/edumg/Desktop/SOFTWARE by EMG/astro-stacker PROYECTO/astro-stacker/benchmarks/design-qa-2026-07-29/deepsky-poststack-editor-comparator-1600x1000-pass2.png`.
- Estado compacto final, viewport y captura 700 × 900 px:
  `/Users/edumg/Desktop/SOFTWARE by EMG/astro-stacker PROYECTO/astro-stacker/benchmarks/design-qa-2026-07-29/deepsky-progress-mobile-700x900-pass2.png`.

## Historial de comparación y correcciones

1. **Pass 1 — [P2] tipografía técnica demasiado pequeña.** Los textos
   secundarios de productos, recursos y diagnóstico bajaban hasta 8,6 px y
   perdían legibilidad frente a la fuente. Se elevaron los tamaños ópticos y se
   recapturó el mismo estado a 1600 × 1000.
2. **Pass 1 — [P2] salida no persistente al desplazarse.** En el estado
   terminado, los nuevos módulos alargaban el diálogo y podían dejar las
   acciones finales fuera del viewport. El pie se hizo `sticky`, de modo que
   Cancelar durante el apilado y Ver/Procesar máster al terminar permanecen
   disponibles.
3. **Pass 1 — [P2] encabezado bajo la barra superior en ancho compacto.** A
   700 × 900 el diálogo llenaba toda la altura y comenzaba bajo la barra de
   ventana. Se reservó la franja superior, se ajustó la altura máxima y se
   elevaron los tres objetivos de SCI/Cobertura/Rechazo de 28 a 36 px.
4. **Pass 2.** La comparación conjunta y la región enfocada no muestran
   hallazgos P0, P1 o P2 pendientes. No existe desbordamiento horizontal a
   700, 980 o 1600 px; el encabezado y la salida persistente permanecen
   visibles.

## Revisión de superficies obligatorias

- **Tipografía:** jerarquía y pesos siguen el lenguaje Zenith; los textos
  técnicos ya conservan un mínimo legible y las cadenas largas usan truncado
  sólo donde existe `title` o contexto contiguo.
- **Espaciado y ritmo:** se mantiene la composición de la fuente — comparación
  dominante, cronología lateral, productos, recursos, diagnóstico y salida —
  con radii, bordes y separación coherentes.
- **Color y tokens:** fondo azul-negro, púrpura de proceso, verde de salida
  preservada, ámbar de aviso y cian de foco conservan significado semántico y
  contraste.
- **Calidad de imagen:** producción carga una toma real mediante
  `deepsky_frame_preview` y sólo presenta un máster o mapa publicado por el
  backend. La imagen generada del fixture está rotulada como demostración y no
  se usa como resultado científico.
- **Copy:** “ANTES”, “ACTUAL”, “Original protegido”, las seis etapas y los
  cuatro productos explican qué se está viendo sin prometer una salida aún no
  publicada.
- **Iconos:** se reutiliza el sprite vectorial de Zenith; no se sustituyeron
  iconos de la referencia con emoji o arte CSS.
- **Accesibilidad:** diálogo modal con `aria-modal`, fondo `inert` y
  `aria-hidden`, foco confinado, comparadores etiquetados, controles por
  teclado, estados textuales además del color y animación anulable con
  `prefers-reduced-motion`.

## Interacciones verificadas

1. El comparador del apilado se movió de 50% a 72% y actualizó la división.
2. SCI, Cobertura y Rechazo exponen estados seleccionados/deshabilitados
   coherentes con la disponibilidad del producto.
3. El estado terminado conserva la ventana, publica 6/6 y muestra los nueve
   módulos.
4. `Procesar máster` abre Recortar con ambas imágenes del comparador cargadas.
5. `2 Quitar gradiente` abre directamente el módulo 2.
6. El comparador del editor se movió de 50% a 74%.
7. A 700, 980 y 1600 px no existe overflow horizontal; a 700 px ninguna acción
   visible mide menos de 36 px de alto.

## Consola y límite de evidencia

La consola fue revisada. El fixture web registra los mensajes ya conocidos por
ausencia del puente Tauri (ventana, `invoke/listen`, fuentes, GPU y updater);
no apareció una excepción propia de la ventana de progreso, sus comparadores o
el relevo al editor. La QA visual acredita el navegador Vite y sus
interacciones. Las pruebas Rust acreditan contratos sintéticos por separado;
esta evidencia no sustituye un apilado FITS real, un bundle nativo o una GPU
física.

## Findings

- No quedan hallazgos P0, P1 o P2 en los estados inspeccionados.

## Open Questions

- La fuente elegida sólo define el estado “en proceso”. El estado “resultado
  listo” y el relevo a los nueve módulos son una extensión intencional validada
  contra el sistema visual existente, no una copia literal de otra pantalla.

## Implementation Checklist

- [x] Comparación real durante el apilado.
- [x] Cronología y productos paralelos.
- [x] Recursos y diagnóstico técnico plegable.
- [x] Estado terminado persistente.
- [x] Acceso directo a los nueve módulos.
- [x] Comparación original/revisión dentro del editor.
- [x] Modalidad, teclado y responsive.

## Follow-up Polish

- [P3] Los recursos priorizan valores reales y densidad compacta; en una
  iteración futura podrían añadir minigráficas sólo cuando exista una serie
  temporal auténtica, sin inventar barras a partir de una única muestra.

final result: passed
