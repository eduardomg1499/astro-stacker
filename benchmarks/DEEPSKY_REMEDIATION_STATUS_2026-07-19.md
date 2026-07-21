# Estado técnico de la remediación de cielo profundo

**Fecha de corte:** 2026-07-19 · **Actualización de cierre:** 2026-07-20  
**Rama:** `codex/planetary-world-class`  
**Base auditada:** `3f1082023e48ed29e893452035430a437e1fd8db`

## Cierre 2026-07-20 (sesión de terminación + auditoría)

La rama compila y la suite completa pasa: **567 tests, 0 fallos, 29 ignorados**
(GPU física/datasets), más **4/4 tests físicos GPU Metal** (paridad streaming y
tiled, ambos con y sin grids de calidad) y los contratos Node
(deepsky-contracts 15/15, planetary 24/24, build Vite). Trabajo cerrado:

- **Compilación**: fix del borrow en `nebula_fusion.rs` (`effective_config`
  construido antes de mover `struct_accepted`); el test de estacionariedad de
  NF-Full corregido (franjas de 8 px — el fixture de mitades dejaba 6 tiles de
  borde legítimamente uniformes por reflexión especular).
- **Auditoría adversarial (multi-agente) con fixes aplicados**:
  1. Carrera del `cancel_requested` global: rearme SOLO en fronteras de acción
     de usuario bajo `planetary_generation_gate`; el registro por-trabajo
     propaga un Cancelar pendiente; la sesión multibanda registra su propio
     flag en el job registry. Checkpoint final antes de PUBLICAR el resultado
     (un stack cancelado ya no machaca el estado compartido).
  2. Comandos síncronos pesados (`deepsky_export`, `deepsky_export_float32`,
     `inspect_deepsky_frames`, `deepsky_probe`, `deepsky_split_channels`,
     `deepsky_dualband_hoo`) anotados `#[tauri::command(async)]`: la UI ya no
     se congela minutos y Cancelar vuelve a funcionar durante exports.
  3. SPCC: `SpccResult` en camelCase — el camino de ÉXITO lanzaba TypeError.
  4. `FrameStore` mmap: escritura por `pwrite` en unix — disco lleno devuelve
     ENOSPC limpio en vez de matar el proceso con SIGBUS (archivo sparse).
  5. **CRÍTICO** — dos definiciones de "sesión" incompatibles: la firma usaba
     la fecha calendario y el emparejado de flats la noche con corte a
     mediodía; cualquier sesión cruzando la medianoche perdía sus flats en
     Strict. `date_session` ahora usa el mismo corte a mediodía (test nuevo).
  6. Productos científicos: reconciliación SCI↔DQ en la publicación — SCI
     finito con NO_COVERAGE/NAN_INPUT pasa a incertidumbre-no-disponible
     auditada y SCI no finito sin máscara se degrada a NO_COVERAGE canónico;
     `NEFF≤1` marca `EIDR_UNCERTAINTY_UNAVAILABLE`. Antes estos casos
     ABORTABAN stacks válidos en `LinearFrame::validate`.
  7. VAR de lights CFA: el ruido MRS se mide por subplano Bayer (el suavizado
     B3 anula señales de periodo 2 y confundía la alternancia R/G/B con ruido,
     inflando VAR ~×250). Test sintético nuevo.
  8. Preflight: la escala EIDR (Auto→2x peor caso) entra en las estimaciones
     de RAM/VRAM/presión; NF/EIDR ya no declaran etapa GPU/"Hybrid" que no se
     ejecutará; validación fotométrica de flats (20-70% del rango, WARNING en
     banda estrecha) y aviso de darks escalados sin bias.
  9. UI: descartar toda una banda ya no invalida la sesión multibanda (el
     grupo vacío se omite); el request se construye tras validar el plan (no
     una instantánea vieja); Ejecutar se deshabilita al descartar todos los
     lights; los controles EIDR/rescate refrescan el preflight; el centinela
     de distancia de flats ya no se filtra a la matriz.
- **Receta AUTO completa** (plan RESCATE-DETALLE-Y-AUTO, parches 0-11 con la
  sustitución de seguridad linear-fit→Winsorized): `PipelineProfile::Auto`,
  señales medidas (`ds_measure_auto_signals`), tabla determinista
  (`ds_resolve_auto_recipe`, tests por regla), `resolved_recipe` en el plan,
  resolver COMPARTIDO plan↔ejecución (log frontal de la receta aplicada),
  AUTO por grupo en sesiones multibanda, y preset Auto por DEFECTO en la UI
  con la receta resuelta y sus motivos visibles.
- **Rescate de detalle completo**: activado por Máxima Calidad (backend+UI);
  pesos locales en TODOS los motores — streaming CPU/GPU (shader WGSL con
  rejilla de calidad), tiled CPU/GPU (kernel de rechazo con grids por frame) y
  drizzle; paridad CPU/GPU extendida con grids no triviales y validada en
  Metal físico; eliminado el gate que forzaba CPU. El término de gradiente G
  sigue diferido (exige bump de caché de análisis).
- **Diagnósticos en inspección**: `inspect_deepsky_frames` devuelve un informe
  tipado con predicción de dithering/walking-noise (offsets estelares
  pre-registro) y patrón de detector medido a resolución nativa (binning 2×2
  en CFA); tarjetas en el paso 1 y eco en la revisión.
- **Coherencia backend↔frontend**: gating de motores experimentales por
  `scientificEligible`, manifiesto científico tipado renderizado por grupo,
  preset Equilibrado sincerado (Winsorized), i18n reconciliada (+22/−15
  claves), GPU/receta visibles, iconos del sprite en marcas de estado.

### Adenda 2026-07-21 (arranque + consistencia)

- **Splash colgado al abrir la app (crítico)**: `index.html` cargaba Chart.js
  y su plugin de anotaciones desde cdn.jsdelivr.net con `<script defer>`; si
  la petición al CDN se estancaba (DNS/firewall/sin red), `window.load` no
  disparaba nunca y el splash quedaba congelado al 30% (diagnóstico con
  beacons de arranque en el webview real). Fix doble: Chart.js y el plugin
  ahora van EMPAQUETADOS localmente (npm + import; la app arranca sin red) y
  la secuencia de arranque tiene fallback anti-cuelgue (DOMContentLoaded+4 s)
  para que ningún recurso estancado pueda volver a congelar el splash.
- **AllowDegraded cumple su promesa**: un flat calibrado inválido (>0.1% de
  muestras no válidas) ya no es solo un WARN — degrada la corrida de verdad:
  NF/EIDR caen a Classic con razón visible y `calibration_degraded` /
  elegibilidad científica lo reflejan (era el hallazgo medium pendiente de la
  auditoría del 20).
- Emoji `⭐` de la etiqueta de la gráfica de calidad sustituido por el glifo
  monocromo `★` (regla del proyecto).

### Adenda 2026-07-21 (2ª tanda: UX de calibración con datos reales)

Primer contacto del flujo con el dataset real del usuario (53 lights SV220
multi-noche). Cambios:

- **La ausencia de cabecera dejó de ser incompatibilidad**: el contrato ahora
  distingue física sin verificar (gain/offset/binning/CFA/exposición/
  temperatura → degrada elegibilidad científica con divulgación, no bloquea)
  de identidad extendida ausente (sensor, readMode, roi, adcBits,
  whiteLevelAdu, opticalTrain → nota informativa; es lo habitual en FITS de
  captura). Un mismatch de valores PRESENTES conserva el fail-closed.
- **Fin de la inundación de mensajes**: metadata y decisiones degradadas se
  agrupan por motivo con contador (backend) y los mensajes idénticos salvo el
  nombre de fichero se pliegan con detalle desplegable (frontend). El detalle
  por toma vive en la matriz de calibración, que para eso existe.
- **Botón "Continuar en modo degradado"** cuando Strict bloquea: cambia la
  política, re-prepara y el usuario decide; cada concesión queda registrada.
- **Asignación manual estilo PixInsight** (`calibrationOverrides` en el
  contrato v4): lotes de darks/flats forzados por regla (lights vacío = todos)
  con masters propios (k=1 en darks), estado "Manual" en la matriz y registro
  en receta. UI v1: selects "Forzar todos los darks/flats cargados" en el
  grupo Calibración; el contrato ya soporta subconjuntos por light para una
  UI por sesión futura.
- **Banding sin falso positivo en inspección**: la variante para tomas crudas
  aplica doble high-pass con recorte de bordes a los perfiles fila/columna —
  el viñeteo/gradiente (que disparaba "banding detectado" con lag-1≈1.0) ya
  no se confunde con patrón del detector; test sintético viñeteo vs banding.
- Con esto, datasets FITS reales con cabeceras normales vuelven a ser
  elegibles científicamente y **NebulaFusion/EIDR quedan seleccionables**.

### Backlog que este cierre NO resuelve (además de la lista original)

- Migrar la ruta GPU deep-sky del `take_gpu_error` destructivo a la API por
  época (documentado; no alcanzable desde la UI actual).
- Token de propiedad para `state.deep_sky_result` (slot last-writer-wins;
  serializado hoy por el flujo de la UI).
- Convención celda-centro vs nodo en los grids loc/wq (±media celda en
  bordes; afecta a ambos por igual y exige revalidar el gate de seams).
- Pedestal automático (parche 7): requiere extraer la selección de masters
  del bucle por-light para calibrar el prototipo antes del fingerprint.
- VAR/NEFF propagados excluyen muestras que el máster sí integró cuando la
  exclusión por DQ es parcial (limitación auditada en código).
- El dominio de auditoría "gates NF/EIDR" quedó sin finder dedicado por
  límites de sesión; los gates se revisaron sólo alrededor del fix de
  `NfEffectiveConfig` y del dispatch de fallbacks.

## Resumen técnico

La remediación ya incorpora una base de calibración mucho más estricta y
trazable: contrato de solicitud y receta v4, dark-flats como entrada de primera
clase, firma completa de calibración, selección por compatibilidad, orden
correcto de calibración de flats, límites físicos para escalar darks, registro
con holdout espacial, normalización local simétrica por canal y publicación
explícita de fallbacks. Classic vuelve a ser la ruta predeterminada; EIDR y
NebulaFusion permanecen experimentales y no se habilitan implícitamente por
`Auto`.

Los módulos focales disponen de pruebas sintéticas que cubren sus invariantes,
pero el trabajo **no está aceptado para release científico ni para una
comparación competitiva**. Falta ejecutar la suite integrada sobre el estado
final, GPU física, el corpus FITS/TIFF real completo y las salidas reproducibles
de WBPP, DSS y APP. En consecuencia, este documento no afirma que Zenith
elimine más ruido, supere el seeing ni sea superior a esos apiladores.

Los conteos de pruebas indicados abajo son cortes focales confirmados durante la
implementación. Deben repetirse después de cerrar todas las ediciones
concurrentes; no sustituyen una corrida limpia de la suite completa.

## Criterio de estado

| Estado | Interpretación |
|---|---|
| Implementado y verificado localmente | Existe una ruta o contrato productivo y pasó una prueba focal sintética o una comprobación de compilación confirmada. |
| Implementado, validación externa pendiente | El código está conectado, pero faltan raws reales, hardware físico o comparación externa para evaluar corrección y rendimiento. |
| Pendiente o limitado | El contrato está incompleto, la ruta aún degrada explícitamente o falta conectar evidencia científica al producto. |

## Implementado y verificado localmente

### Contrato científico v4 y preflight estricto

- `DeepSkyStackRequest` usa esquema 4 y la receta usa
  `zenith-deepsky-recipe-v4`.
- La solicitud acepta `darkFlats`, `captureMode`, `calibrationPolicy` y
  `scientificProducts`; `Strict` es la política predeterminada.
- Los modos tipados son `Auto`, `BroadbandOsc`, `BroadbandMono`,
  `DualBandOsc` y `MonoNarrowband`. `Auto` clasifica la captura, pero no activa
  EIDR/NebulaFusion.
- `CalibrationSignature` incluye cámara, sensor, read mode, gain/ISO, offset,
  temperatura, exposición, binning X/Y, ROI, patrón/fase CFA, filtro, sesión,
  tren óptico, profundidad ADC y nivel blanco.
- `PedestalState` diferencia `RawIncludesBias` de `BiasSubtracted`.
- `StoreLayout` diferencia CFA con patrón/fase, mono y RGB. La caché preparada
  se invalidó a v7 para no reutilizar calibraciones o decisiones anteriores.
- `PreparedCalibrationDecision` registra por light los masters, escala de dark,
  compatibilidad, degradación, fallback y razones. El preflight presenta esas
  decisiones en una matriz tipada.
- `ScientificBundleManifest` define SCI, VAR, NEFF, DQ, cobertura, rechazo,
  PSF/MTF/PSD, fondo, STRUCT, RECOV y residuales. Cada grupo multibanda publica
  el manifiesto y conserva sus productos aunque el estado interactivo avance.
- FITS/TIFF son entradas científicas. PNG/JPEG o formatos no lineales bloquean
  `Strict` y sólo pueden continuar como resultado no científico con
  `AllowDegraded`.

### Dark-flats, flats, bias y darks

- Dark-flats se reconocen en backend, UI, clasificador, preflight, ejecución y
  receta.
- Cada flat se calibra **antes** de cualquier normalización de exposición. Se
  resta exactamente una convención de pedestal: dark-flat crudo, o bias cuando
  el contrato lo permite; nunca ambos a la vez.
- Flats CFA se normalizan por las cuatro poblaciones Bayer, RGB por canal y mono
  globalmente. Los masters usan media robusta con máscara mediana/MAD para
  cinco o más tomas, evitando la penalización de ruido de una mediana habitual.
- La exposición exacta usa tolerancia de cabecera de 1 ms o `1e-6` relativa.
  Los darks requieren firma compatible y temperatura dentro de 1 °C; los
  dark-flats requieren exposición y geometría/CFA exactas.
- `Strict` no usa el flat de la noche “más cercana”. `AllowDegraded` omite el
  flat incompatible y registra la razón.
- El escalado de dark sólo se admite sobre señal bias-subtracted, sin amp glow,
  razón de exposición entre 0.25 y 4, correlación y R² de al menos 0.995, y
  residuo no mayor de 1 %. Si falla, el dark se omite y la decisión queda
  degradada; NF/EIDR regresan a Classic.
- Bias-only para flats se bloquea en `Strict` mientras no exista evidencia de
  que la contribución térmica sea como máximo 0.1 % del nivel del flat.

### Classic, registro y normalización

- Se eliminó el peso mínimo 0.3. Una toma puede excluirse por seeing/FWHM,
  excentricidad, transparencia/señal PSF o peso científico insuficiente, con
  razón en la receta.
- El antiguo “linear-fit clipping” por rangos está bloqueado en UI, preflight y
  runtime hasta implementar una regresión frame↔referencia sobre residuales.
- El remuestreo Lanczos ya no aplica un clamp dependiente de señal contra
  vecinos bilineales.
- El registro usa correspondencias biyectivas y deterministas, selección con
  holdout espacial 80/20, p95 máximo de 0.20 px y validación del Jacobiano en el
  campo. Un registro no publicable bloquea el stack en vez de presentar una
  única referencia como integración.
- La normalización local usa un grafo simétrico multiframe por canal, conserva
  componentes de gauge y exige seams menores de 0.2 sigma; si falla, vuelve a
  la normalización global por canal y registra el fallback.
- Se diagnostica riesgo de walking noise a 1× y con drizzle a partir de dither,
  isotropía y deriva temporal.
- En píxeles sin cobertura, SCI queda como `NaN` y DQ incluye
  `NO_COVERAGE`; el relleno no se publica como señal científica.
- ABE, SCNR, neutralización y HOO dual-band operan sobre una vista derivada. No
  mutan el SCI lineal ni invalidan VAR/NEFF/DQ.
- Classic streaming CPU a 1× conserva momentos ponderados para VAR y NEFF. DQ
  siempre existe, aunque en rutas sin momentos científicos sólo representa
  cobertura.

### EIDR y NebulaFusion

- EIDR rechaza frames sin PSF medida; no sustituye una PSF nominal. La puerta de
  recuperabilidad exige ruido finito y evidencia por tile, degrada escala de
  forma explícita y usa un piloto limitado al Nyquist nativo en tiles de
  fallback.
- EIDR integra los campos locales de fondo en los datos de entrada del operador,
  valida geometría en todo el campo a 0.02 px, exige convergencia y finitud de
  todos los canales, y valida holdout antes de publicar. DQ marca
  `EIDR_FALLBACK_NATIVE` y RECOV conserva la recuperabilidad publicada.
- EIDR retorna a Classic con razón visible si falla cualquier gate científico.
  No se interpreta el aumento de escala como recuperación de frecuencias no
  capturadas.
- NebulaFusion Full asigna peso cero a huecos, outliers y no finitos; no cuenta
  el piloto como observación. Usa PSF y PSD por frame/canal y mantiene
  VAR/NEFF/DQ.
- El operador Full sólo se declara válido para traslaciones representables. Una
  rotación, escala, homografía o distorsión local degrada a Lite con razón, en
  vez de publicar una PSF de salida incorrecta.
- STRUCT se produce como evidencia separada; no sustituye ni modifica SCI.

### Memoria, I/O y trazabilidad

- `FrameStore` usa spill LZ4 v4 por bloques, índice de rangos y decodifica sólo
  los bloques solicitados. Verifica checksums, conserva telemetría de bytes y
  bloques, y publica de forma atómica con sincronización y rename.
- STRUCT se calcula nivel por nivel con tiles y halos, reservas fallibles y un
  presupuesto de memoria explícito. Si no cabe, se omite con una razón en vez
  de abortar el proceso.
- La exportación FITS usa unidades diferenciadas: SCI `ADU`, VAR `ADU^2`,
  NEFF/cobertura/RECOV adimensionales y DQ `BITMASK`.
- La receta de sesión v2 conserva un `ScientificBundleManifest` por grupo con
  SCI, VAR, NEFF, DQ, STRUCT/residual, RECOV, cobertura, peso, rechazos,
  decisiones de calibración y fallbacks.
- La receta registra método solicitado y efectivo, fallbacks, decisión CFA,
  registro, normalización, productos presentes/ausentes y razones.

### Arnés A/B reproducible

- `scripts/benchmark/deepsky-ab-report.mjs` y
  `benchmarks/deepsky-ab-run.schema.json` implementan un evaluador fail-closed.
- Exige los mismos raws, SHA-256, crop, escala lineal y hardware; fija versión y
  parámetros; y requiere exactamente cinco corridas frías y cinco calientes de
  Zenith, WBPP, DSS y APP.
- La caché fría/caliente debe conservar los mismos hashes de decisiones, CFA y
  métricas científicas.
- La publicación exige todos los límites absolutos, ninguna regresión primaria
  mayor de 3 % y, para una afirmación de superioridad, una mejora de al menos
  5 % con intervalo bootstrap del 95 % favorable en cada clase evaluada.
- El arnés está probado con fixtures sintéticos; no se ha ejecutado todavía con
  el corpus competitivo real.

## Cobertura por clase de captura

| Clase | Implementación actual | Evidencia que falta |
|---|---|---|
| Broadband OSC | Modo tipado, calibración CFA antes de debayer, cuatro patrones Bayer/fase en el contrato, Classic y motores experimentales con gates. | Corpus real multi-noche, cuatro Bayer/ROI, fotometría y ruido frente a WBPP/DSS/APP. |
| Broadband mono | Modo tipado y flujo mono lineal con la misma selección estricta, registro, normalización y productos. | Corpus mono real con bias/dark-flat, amp glow, gradientes y hardware CPU/GPU. |
| Dual-band OSC | SCI RGB/CFA permanece válido; HOO, Ha y OIII se exportan como productos derivados. Sin matriz cámara-filtro se etiquetan `Ha proxy`/`OIII proxy`, no como flujo cuantitativo. | Matriz espectral cámara-filtro, covarianza, validación fotométrica Ha/OIII y comparación real. |
| Mono narrowband Ha/OIII/SII | Sesiones agrupan filtros y apilan cada grupo por separado; no mezclan raws de líneas distintas y conservan un bundle científico por filtro. | Registro entre masters, combinación espectral final y corpus multi-sesión real. |

## Evidencia local confirmada

| Comprobación focal | Último resultado confirmado | Alcance y cautela |
|---|---:|---|
| `cargo check --bin astro-stacker` | Pasa | Confirmado durante la integración focal; debe repetirse sobre el árbol final. |
| `frame_store::tests` | 9 pasan | Rango LZ4, reúso, persistencia, corrupción e I/O sintético. |
| `deepsky_background` | 11 pasan | Grafo simétrico, RGB por canal, solape, componentes y seam sintético. |
| `eidr` core | 25 pasan, 1 ignorada | Solver, PSF/ruido, recuperabilidad, publicación por tile y holdout; GPU física ignorada. |
| NebulaFusion | 18 pasan | Lite/Full, datos ausentes, PSF/PSD y fallbacks sintéticos. |
| `gpu_deepsky` no físico | 4 pasan, 4 ignoradas | Contratos/paridad sin ejecutar el dispositivo físico en las ignoradas. |
| Bundle multibanda + contrato | 3 pasan | Exporta SCI y 11 productos/diagnósticos más dos proxies, con unidades y DQ exacto. |
| `scripts/deepsky-contracts.test.mjs` | 6 pasan | Contrato/UI/preflight de cielo profundo. |
| `scripts/benchmark/deepsky-ab-report.test.mjs` | 3 pasan | Aceptación sintética y dos fallos cerrados. |

No se registra aquí un total de suite completa: aún debe ejecutarse después del
cierre de integración. Tampoco se considera “pasada” una prueba física que esté
ignorada o condicionada por una variable de entorno ausente.

## Implementado, pero sin validación de corpus o GPU física

- La calibración estricta, media robusta, VAR/DQ sintéticos y gates de flat/dark
  no se han medido todavía con un dataset completo de lights, darks, flats,
  dark-flats y bias que reproduzca el ruido y los artefactos reportados.
- La equivalencia entre caché fría y caliente está contratada, pero falta medir
  decisiones y píxeles sobre raws reales de 26/62 MP y 20/100/300 tomas.
- Registro, normalización local, dither/walking-noise y límites de tile se han
  probado con escenas sintéticas pequeñas; faltan gradientes lunares, campo
  pobre en estrellas, distorsión, satélites, cosmics y seeing variable reales.
- EIDR y NebulaFusion no están habilitados para `Auto` ni aceptados como
  científicos de release. Requieren PSF/PSD, holdout, geometría, VAR/DQ y
  recuperabilidad con raws reales por cada clase de captura.
- El rendimiento de GPU, readback, RAM/VRAM, NVMe interno/externo y aceleración
  mínima de 1.2× no se ha cerrado con telemetría física reproducible.
- El arnés A/B aún no contiene versiones fijadas, logs, masters y cinco corridas
  frías/calientes reales de WBPP, DSS y APP.

## Pendientes y limitaciones activas

1. **BLANK/NaN/Inf de entrada:** FITS/TIFF ya no los convierten silenciosamente
   en cero; el archivo se bloquea con una razón visible. Falta el refinamiento
   que permita conservar el resto del frame mediante DQ `NAN_INPUT` por píxel.
2. **`LinearFrame` aún no gobierna toda la ruta:** el contrato valida SCI/VAR/DQ,
   layout, pedestal y metadata, pero el resultado lineal productivo todavía no
   se construye obligatoriamente a través de él.
3. **VAR/DQ de masters de calibración:** existen helpers robustos con
   VAR/NEFF/DQ, pero `ds_build_master` publica principalmente SCI; la
   incertidumbre de bias/dark/flat no se propaga de extremo a extremo.
4. **Classic incompleto por backend:** VAR/NEFF son honestos en streaming CPU
   1×. Classic tiled, GPU y drizzle los omiten explícitamente porque aún no
   conservan `sum(w²)`/momentos por depósito. Su DQ es sólo de cobertura.
5. **Drizzle científico:** la regla de no cobertura está corregida, pero falta
   la propagación completa de varianza y rechazo por depósito en Classic.
6. **Normalización/registro de casos extremos:** falta un fallback WCS o phase
   correlation para estrecha con pocas estrellas; los modelos no afines se
   excluyen actualmente de EIDR.
7. **NebulaFusion Full general:** `warpᵢ × PSFᵢ` sólo está representado de forma
   defendible para traslación. Los demás warps degradan a Lite.
8. **Dual-band cuantitativo:** no existe aún una matriz de respuesta
   cámara-filtro en la API productiva; Ha/OIII siguen siendo proxies.
9. **Memoria extrema:** STRUCT reduce su huella mediante tiles, pero las
    entradas split-half RGB y los productos simultáneos de NF/EIDR aún requieren
    pruebas de 60 MP y degradación segura bajo presión real.
10. **Detector pattern:** el diagnóstico de banding/autocorrelación existe como
    módulo, pero falta conectar todas sus métricas al preflight, UI y receta.
11. **Aceptación externa:** no existe todavía el corpus real congelado ni una
    corrida competitiva; por tanto no hay evidencia de superioridad, reducción
    de ruido frente a competidores ni recuperación más allá de la banda
    capturada.

## Gates que deben pasar antes de release

### Comandos de integración

```bash
cargo check --manifest-path src-tauri/Cargo.toml --bin astro-stacker
cargo test --manifest-path src-tauri/Cargo.toml --bin astro-stacker frame_store::tests -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --bin astro-stacker deepsky_background -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --bin astro-stacker eidr -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --bin astro-stacker nebula_fusion -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --bin astro-stacker gpu_deepsky -- --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --no-fail-fast
npm run test:deepsky-contracts
npm run build
git diff --check
```

Las pruebas ignoradas de GPU y corpus real deben ejecutarse explícitamente en
hardware objetivo y con las variables/datasets requeridos; una prueba omitida
no cuenta como aceptación.

### Gates científicos absolutos

| Gate | Límite de aceptación |
|---|---:|
| Residuo de flat | ≤ 0.5 % |
| Residuo de patrón dark/amp glow | ≤ 1 % del original, o bloqueo explícito |
| Sesgo fotométrico sintético / real | ≤ 0.5 % / ≤ 1 % |
| Registro Classic p95 | ≤ 0.20 px, sin Jacobiano negativo |
| Geometría EIDR | ≤ 0.02 px |
| Cobertura Monte Carlo VAR 68/95 % | error dentro de ±5 puntos porcentuales |
| Seam de tiles/fondo | < 0.2 sigma |
| Píxeles sin cobertura | `SCI` no finito + `DQ::NO_COVERAGE` |
| Amplificación de lectura | ≤ 1.5× por pasada; máximo duro 2× |
| RSS | ≤ 70 % de RAM disponible, sin aborto por asignación |
| GPU ofrecida como acelerada | speedup ≥ 1.2× y readback < 10 % del tiempo GPU |
| Caché fría/caliente | decisiones, CFA y métricas equivalentes |

### Gate A/B competitivo

```bash
npm run benchmark:deepsky-ab -- /ruta/al/corpus-de-corridas \
  --matrix benchmarks/dataset-matrix.json \
  --out /ruta/al/deepsky-ab-report.json
```

Cada clase —broadband OSC, broadband mono, dual-band OSC y mono narrowband— se
evalúa por separado con los mismos raws, crop, escala lineal y hardware. Sólo después de
cumplir todos los límites absolutos, no quedar más de 3 % por debajo del mejor
competidor en una métrica primaria y demostrar la mejora bootstrap exigida
podrá discutirse una afirmación de superioridad.

## Decisión de liberación

**Estado actual: no liberable como flujo científico superior y no elegible para
una afirmación competitiva.** La base Classic está sustancialmente remediada y
los motores experimentales fallan de forma más segura, pero quedan pendientes
la propagación completa de incertidumbre, el DQ por píxel para BLANK, la suite
integrada, GPU física y el corpus A/B real.
