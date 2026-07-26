# Cuaderno de invención — INV-A: Planificador de dithering en bucle cerrado

**CONFIDENCIAL — no divulgar (release, foro, changelog, vídeo) antes de consultar con agente de la propiedad industrial. La novedad europea es absoluta y sin período de gracia.**

- **Fecha de concepción y primera reducción a la práctica:** 17 de julio de 2026
- **Inventor:** Eduardo Martínez (Zenith Astro Stacker)
- **Implementación de referencia:** `src-tauri/src/eidr.rs` (funciones `eidr_plan_next_dither`, `eidr_plan_dither_sequence`, `build_plan_channels`, `plan_evidence`), comando de aplicación `deepsky_plan_dither` (`deepsky.rs` + `commands_core.rs`), gates reproducibles `gate_inva_planner_beats_random_mono`, `gate_inva_planner_beats_random_cfa`, `inva_planner_beats_naive_phase` (suite `cargo test`, commit en rama `codex/planetary-world-class`).

---

## 1. Problema técnico

La reconstrucción super-resuelta de imágenes astronómicas a partir de múltiples exposiciones submuestreadas (Drizzle y sucesores forward-model) requiere **diversidad de fases subpíxel** entre las exposiciones: sin fases que separen las réplicas de alias de cada banda de frecuencia, el grid fino es interpolación, no evidencia. En la práctica universal actual (PHD2, N.I.N.A., ASIAIR, secuenciadores comerciales), el desplazamiento entre tomas ("dither") se genera **aleatoriamente** o en espiral, con un propósito distinto (decorrelar el ruido de patrón y el walking noise). La cobertura de las clases de fase necesarias para super-resolución queda al azar: sesiones enteras pueden no alcanzar la evidencia necesaria para 2x, o alcanzarla con muchas más tomas de las necesarias (problema del cupón-coleccionista), sin que el usuario lo sepa hasta el procesado.

## 2. Estado de la técnica conocido

- Dither aleatorio/espiral en software de guiado y secuenciación (PHD2, N.I.N.A., ASIAIR): sin relación con la recuperabilidad espectral.
- Análisis de condicionamiento/límites de la super-resolución multi-frame (Baker & Kanade; Lin & Shum; literatura IMCOM/coadición): analizan *a posteriori* si un conjunto dado soporta reconstrucción; no controlan la adquisición.
- Diseño experimental óptimo (D/E-óptimo) en estadística: teoría general, no aplicada al control del dithering astronómico.

No se conoce (a fecha de este cuaderno) software o método publicado que **elija activamente la fase subpíxel de la siguiente exposición** para maximizar la evidencia de separación de alias del conjunto adquirido.

## 3. La invención (mecanismo)

Método implementado por ordenador para controlar el dithering de un sistema de captura astronómica:

1. **Modelo de evidencia:** para cada banda de frecuencia objetivo del anillo super-resuelto (rejilla de radios × ángulos entre el Nyquist nativo y el de salida) y cada canal de la retícula del sensor (mono: una; CFA: tres, con el verde quincunx aportando dos filas por exposición), se mantiene la matriz de réplicas aliasadas Q(i,ℓ) = a_ℓ·e^{−2πi ν_ℓ·(Δ_i+o)} construida con las fases Δ_i de las exposiciones **ya adquiridas** (a_ℓ = MTF de la PSF × apertura del fotosito en la réplica ℓ; o = paridad de la retícula).
2. **Métrica:** evidencia del modo objetivo t como precisión marginal s̃ = (1/√[(QᴴQ)⁻¹]_tt)/‖columna DC‖, transformada saturante R = s̃²/(s̃²+η), **promediada** sobre bandas y canales. La saturación por banda hace que maximizar la media reparta evidencia hacia las bandas peor servidas (maximin suave) y, a diferencia del maximin puro, ordena candidatas desde la primera exposición (cuando todos los mínimos son cero).
3. **Planificación:** se evalúa una rejilla de fases candidatas para la SIGUIENTE exposición sobre el período de la retícula (mono [0,1)²; CFA [0,2)², porque la paridad del entero selecciona el fotosito), seguida de refinamiento local; se recomienda la candidata de máxima evidencia.
4. **Composición del desplazamiento real:** parte entera pseudoaleatoria de varios píxeles (se conserva íntegro el beneficio anti walking-noise del dither clásico) + la **fase fraccional planificada** (la invención controla solo la fase, que es lo que la super-resolución necesita).
5. **Bucle cerrado:** el planificador re-computa con el conjunto realmente adquirido — tomas perdidas, rechazadas por nubes/ráfagas o descartadas por calidad se compensan automáticamente en la siguiente recomendación.
6. **Variante a priori:** aplicación golosa del mismo criterio para emitir una secuencia completa antes de la sesión (`eidr_plan_dither_sequence`).
7. **Acoplamiento con la puerta de recuperabilidad** (F9 §7.4 del plan técnico): la misma métrica que decide a posteriori si el 2x es apto se usa a priori para predecirlo y alcanzarlo con mínimas tomas; el sistema puede anunciar "con la próxima toma pasarás del 41% al 58% de banda con evidencia".

## 4. Resultados medidos (gates reproducibles, 17-07-2026)

Árbitro: la puerta de recuperabilidad real (`eidr_recoverability_gate`), no el propio planificador. Criterio: nº de tomas hasta que la escala 2x queda soportada. 5 semillas por estrategia; las 2 primeras tomas son aleatorias comunes.

| Escenario | Dither aleatorio | Dither planificado | Mejora |
|---|---|---|---|
| Mono, FWHM 1.3 px, 2x | 4,4,4,5,5 (mediana 4) | 4,4,4,4,4 (mediana 4) | consistencia 100% (elimina la cola) |
| **CFA RGGB, FWHM 1.4 px, 2x (los 3 canales aptos)** | 19,21,21,23,**31** (mediana 21) | 16,16,17,17,**17** (mediana 17) | **mediana −19%, peor caso −45%** |
| Fase óptima tras 1 toma en (0,0) | — | (0.500, 0.500) auto-descubierta | ≥ heurística ingenua |

Interpretación técnica: en mono el mínimo teórico es tan bajo que ambos lo rozan (el planificador aporta determinismo); en CFA — el caso mayoritario del mercado (cámaras a color) — el azar debe cubrir clases de fase por canal y su cola mala es larga (hasta 31 tomas); el planificador la elimina y garantiza el resultado en ~16-17. Traducción a producto: **menos tiempo de telescopio para el mismo máster 2x, y garantía en lugar de lotería.**

## 5. Reivindicaciones esbozadas (para el agente)

1. Método implementado por ordenador de control del desplazamiento entre exposiciones astronómicas que comprende: mantener, para una pluralidad de bandas de frecuencia y canales de retícula del sensor, matrices de réplicas aliasadas de las exposiciones adquiridas; evaluar fases candidatas de una exposición siguiente mediante la precisión marginal del modo objetivo de dichas matrices; y ordenar la captura con la fase seleccionada.
2. El método de 1, donde la fase seleccionada se compone con un desplazamiento entero pseudoaleatorio que preserva la decorrelación de ruido de patrón.
3. El método de 1, donde la métrica por banda es saturante y el criterio agregado reparte evidencia hacia las bandas peor servidas.
4. El método de 1 en bucle cerrado sobre las exposiciones efectivamente aceptadas.
5. El método de 1 aplicado por canal de una retícula de filtros de color, incluyendo la retícula quincunx del canal verde.
6. Sistema (aparato de captura + software) y producto de programa que ejecutan 1-5.
7. El método de 1, donde la misma métrica genera un mapa/predicción de recuperabilidad publicado al usuario antes y durante la adquisición.

## 6. Diferenciadores frente al estado de la técnica

- El dithering deja de ser ruido de proceso y pasa a ser **diseño experimental activo** del sistema de alias — un cambio de categoría, no una mejora incremental de un dither aleatorio.
- El control es **por fase de retícula y por canal CFA** (nadie modela el quincunx del verde en la planificación de dithers).
- La composición entero-aleatorio + fracción-planificada conserva la función clásica del dither (novedad no destructiva: compatible con todos los flujos actuales).
- Efecto técnico cuantificado con árbitro independiente (la puerta), reproducible en la suite.

## 7. Trabajo pendiente antes de la solicitud

- Búsqueda profesional de anterioridades (agente de la propiedad industrial): dithering controlado en microscopía/escáneres/satélites (TDI), pixel-shift de cámaras fotográficas (Olympus/Sony/Pentax: fases FIJAS predefinidas, sin bucle cerrado ni evidencia — diferenciar), diseño experimental en MRI (compressed sensing: muestreo k-space adaptativo — campo distinto, delimitar reivindicaciones al dominio).
- Extensión natural (misma solicitud): planificación conjunta para la separación cielo/contaminación (INV-B, doble anclaje) — la diversidad geométrica óptima sirve a ambos.
- Integración de captura real (plugin N.I.N.A./API) como realización preferente adicional.
