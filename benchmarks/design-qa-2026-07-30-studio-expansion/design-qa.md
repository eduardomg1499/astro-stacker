# QA — expansión de Cielo Profundo Studio

Fecha: 2026-07-30  
Rama: `codex/Auditoria-DS`  
Alcance: editor posterior al apilado, curvas/color, paletas, geometría compartida, WCS y anotaciones.

## Resultado

El recorrido principal quedó operativo en el fixture web reproducible y sus contratos nativos compilan. La validación visual no sustituye una corrida Tauri con FITS reales; cada evidencia de fixture está identificada como tal en la interfaz.

## Recorrido interactivo

1. Se abrió el Studio con una sesión Ha+OIII + SII+OIII.
2. La galería se generó automáticamente con ocho propuestas comparables y una recomendación cuantificada.
3. Se seleccionó `HOO natural` y se creó una revisión lineal derivada.
4. Se aplicó GHS adaptativo y luego se abrió Curvas y color.
5. Se cambió al canal R, se aplicó el preset Nebulosa tenue y se creó una nueva revisión.
6. Se resolvió el WCS del fixture (186 inliers, RMS 0.42 px).
7. Se generó Atlas Zenith, se cambió a Minimal y se regeneró la capa.
8. Se comprobó el flujo mono: sólo aparecen K y L; RGB, HSL y saturación cromática no se ofrecen.
9. Se repitió el recorrido a 1024 px y 720 px. A 720 px la barra de vistas queda horizontal, la imagen no es tapada por los pasos y el panel activo continúa debajo.

## Evidencia visual

- `03-palette-auto.png`: galería automática multibanda.
- `04-curves-color.png`: curvas K/L/R/G/B/S y HSL en color.
- `05-annotations-atlas.png`: primer mapa Atlas Zenith.
- `06-curves-mono.png`: contrato mono K/L.
- `07-curves-tablet-1024.png`: distribución intermedia.
- `08-curves-narrow-720.png`: distribución estrecha corregida.
- `09-final-annotations.png`: estado final de anotaciones.

## Contratos verificados

- Curvas float32 por Combined/Object/Stars; identidad exacta y NaN preservado.
- Mono rechaza ajustes RGB/HSL en backend y no los muestra en UI.
- El recorte es global y transaccional: SCI, VAR, NEFF, DQ, cobertura, Objeto, Estrellas, máscara y residual usan un único `geometryId`.
- Una recombinación con geometrías distintas queda bloqueada.
- El WCS usa la geometría efectiva en Classic/Drizzle/NebulaFusion/STRUCT/EIDR y se invalida si su fingerprint no corresponde.
- Las anotaciones se generan como overlay independiente; exportar la capa o el compuesto no muta el máster ni su WCS.
- La consulta web de catálogo es explícita, acotada, cache-first y recuperable; sin red el máster permanece utilizable.

## Pruebas ejecutadas

- `node --check src/main.js`: aprobado.
- `npm test`: 85/85.
- `npm run build`: aprobado.
- `cargo check --manifest-path src-tauri/Cargo.toml`: aprobado.
- `cargo test ... deepsky_annotations_tests`: 7/7.
- Backend poststack: 28/28.
- Astrometría/PCC: 9/9 más prueba WCS SCI/mapas 1/1.
- Importación WCS embebida: 3/3 (fingerprint coincidente, candidato sin fingerprint y matriz PC+CDELT).
- `rustfmt --check` de los módulos nuevos: aprobado.
- `git diff --check`: aprobado.

## Consola y límites

En Vite aparecen avisos esperados porque no existe el puente Tauri (`invoke`, eventos de ventana y fuentes nativas). No se observó una excepción del recorrido nuevo de paletas, curvas, WCS o anotaciones. La validación científica definitiva aún requiere ejecutar FITS reales en el bundle nativo y comparar la proyección con un catálogo de referencia.

La primera versión del anotador incluye objetos de cielo profundo y retícula ecuatorial. Fronteras de constelaciones y nomenclatura estelar extensa quedan para un catálogo local posterior; no se fabrican cuando faltan.
