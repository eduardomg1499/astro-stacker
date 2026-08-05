# Auditoría de implementación — Apilado de Vía Láctea

Fecha: 2026-08-02  
Rama inspeccionada: `codex/Auditoria-DS`  
Objetivo: incorporar un flujo nativo de paisaje nocturno sin alterar los contratos científicos de Cielo Profundo ni Cometa.

## Resultado

Se implementó un asistente de seis etapas con dos modos de presentación —Esencial y Experto— que serializan la misma receta. El motor publica ramas lineales independientes de cielo y suelo, un compuesto, una máscara cielo/suelo y mapas de varianza, cobertura y rechazo. Los originales permanecen inmutables y el editor crea únicamente revisiones derivadas.

La primera versión admite dos recorridos reales:

- `Cielo y suelo`: registro estelar para el cielo y registro fijo para el terreno, con máscara confirmada.
- `Sólo cielo`: integra el cuadro completo sin inventar una rama terrestre.

`Star Trails`, RAW de cámara y tomas de primer plano capturadas por separado quedan bloqueados con errores tipados hasta disponer de motores científicamente validados. No se presenta una opción visual que el backend no pueda ejecutar. La publicación científica actual es FITS float32 por capa; TIFF lineal de salida no se etiqueta como disponible.

## Recorrido UX/UI y contrato backend

| Etapa | Decisión visible | Bloqueo verificable | Trabajo nativo |
|---|---|---|---|
| 1. Datos | Lights, toma base, darks, flats y carpeta | Mínimo de 4 tomas en doble rama o 2 en sólo cielo; archivos ilegibles y formatos no científicos bloquean | Lectura FITS/TIFF lineal, firma de geometría/CFA/canales, calibración Strict por defecto |
| 2. Cielo y suelo | Modo, máscara automática, horizonte o pincel; confirmación explícita | No se registra ni integra doble rama sin máscara confirmada | Máscara suave reproducible y geometría común |
| 3. Registro | Auto, afín, homografía o radial; inliers y RMS visibles | Falla si no supera estrellas/inliers/RMS o si una rama queda sin solución | RANSAC, transformaciones por rama, propagación WCS sólo cuando es válida, un único remuestreo |
| 4. Integración | Rechazo robusto, exposición y distorsión; diagnóstico experto | Rechazo no robusto, exposición inconsistente sin confirmación o calibración incompatible bloquean | Integración robusta, VAR, cobertura, rechazo, DQ y exclusiones registradas |
| 5. Composición | Suelo apilado o toma base, color sólo en el borde y transición | Geometrías diferentes o composición no soportada bloquean | Compuesto lineal; siempre conserva cielo, suelo, máscara y mapas por separado |
| 6. Editar y exportar | Inventario completo, carpeta, FITS por capa y apertura del Studio | No permite terminar sin salida; muestra concesiones y exclusiones | Publicación con nombres legibles, receta reproducible, progreso/cancelación y apertura del editor |

En ancho estrecho, el modal usa la altura completa, evita desbordamiento horizontal y mantiene accesibles cabecera, navegación, decisiones y acción principal. El fondo queda inerte mientras el diálogo está abierto, Escape/cierre restauran el foco y los pasos futuros usan `disabled` y `aria-disabled`, no sólo color.

## Seguridad científica

- `Strict` es el valor inicial. `AllowDegraded` sólo aparece en Experto, excluye calibraciones incompatibles y marca el resultado como no científico.
- Dark y flat se validan contra todas las lights y entre sí; una exposición desconocida no se convierte en coincidencia exacta.
- Las transformaciones de cielo y suelo se componen antes de remuestrear, evitando una segunda interpolación.
- Un recorte en Studio se aplica atómicamente a cielo, suelo, compuesto, máscara, varianzas, coberturas y rechazos. Si falta un mapa requerido, no genera una geometría parcial.
- La solución WCS queda asociada al `geometryId`; las anotaciones rechazan una solución perteneciente a otro recorte.
- El estado científico reportado por Rust es autoritativo. Las advertencias informativas no degradan por sí solas, pero una exclusión o fallback sí queda visible.

## Editor reforzado

Vía Láctea Studio se abre al terminar y también conserva el lanzador externo del Editor de Cielo Profundo. Publica diez pestañas sincronizadas: Compuesto, Cielo, Suelo, Máscara, dos varianzas, dos coberturas y dos rechazos.

El recorrido de edición contiene:

1. Recortar
2. Fondo y gradiente
3. Astrometría
4. Canales y color
5. Restaurar PSF
6. Ruido lineal
7. Capas estelares
8. Estirar
9. Curvas y color
10. Realzar detalle
11. Acabado
12. Anotar y exportar

Gradiente, PSF, ruido y separación estelar siguen disponibles manualmente, pero se omiten en la edición rápida de Vía Láctea mientras no consuman realmente la máscara de horizonte. La UI lo declara antes de aplicar; no finge protección. Color, estirado, curvas, detalle y acabado trabajan sobre una derivación del compuesto. Undo/redo, reset, A/B y reapertura conservan receta, geometría y raíces inmutables.

## Comparación funcional de referencia

Se tomaron como referencia pública los recorridos de Sequator y Starry Landscape Stacker:

- Selección de toma base, lights y calibraciones opcionales.
- Máscara de cielo con horizonte irregular y posibilidad de corrección manual.
- Alineación estelar y terreno congelado como ramas distintas.
- Unificación de exposición, rechazo de trazas y corrección de distorsión.
- Salida lineal para edición posterior.

Zenith añade contratos explícitos por etapa, mapas científicos por rama, geometría atómica, receta reproducible, modos Esencial/Experto equivalentes y un editor no destructivo. Esto es una diferencia funcional implementada, no una afirmación de superioridad de calidad.

Referencias:

- Sequator manual: https://sites.google.com/view/sequator/manual
- Starry Landscape Stacker: https://sites.google.com/site/starrylandscapestacker/home
- Starry Landscape Stacker manual: https://sites.google.com/site/starrylandscapestacker/Version-1-3-1

## QA ejecutada

- Node: 118/118 pruebas aprobadas.
- Contratos Cielo Profundo: 50/50.
- A/B Cielo Profundo: 15/15.
- Rust Vía Láctea: 34/34.
- Rust Cometa: 5/5.
- Rust completo: 745 aprobadas, 0 fallidas, 29 ignoradas.
- `cargo check`: aprobado.
- `npm run build`: aprobado; Vía Láctea se carga como chunk independiente de ~55 KB.
- `git diff --check` y `node --check src/main.js`: aprobados.
- Recorrido visual web en 467 × 987: máscara, invalidación/reconfirmación, registro, política Expert, composición, resultado, apertura de Studio, capas y aviso de herramientas sensibles al horizonte.

Las capturas web usan un fixture determinista para acreditar UX; no acreditan calibración ni calidad de píxel real. Las pruebas Rust acreditan contratos y ejecución sintética/local. Aún se requiere un corpus representativo de paisaje nocturno FITS/TIFF y, cuando corresponda, GPU física para medir artefactos, PSF, horizonte, tiempo, memoria y paridad visual antes de cualquier afirmación de superioridad.

## Evidencia visual

- `10-final-data-1440x900.png`
- `11-final-mask-467x987.png`
- `12-final-registration-467x987.png`
- `13-final-composition-467x987.png`
- `14-final-editor-entry-467x987.png`
- `22-final-studio-467x987.png`
