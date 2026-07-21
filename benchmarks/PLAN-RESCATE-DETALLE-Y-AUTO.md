# Plan de implementación pendiente — Deepsky élite

> **CERRADO 2026-07-20.** La receta AUTO (parches 0-11, con N>50 →
> Winsorized por la nota de seguridad) y el rescate de detalle (incluidos los
> parches GPU 10-11 con paridad física Metal y la activación en Máxima
> Calidad) están implementados; ver
> `DEEPSKY_REMEDIATION_STATUS_2026-07-19.md` § "Cierre 2026-07-20".
> Quedan diferidos: término de gradiente G (bump de caché) y pedestal
> automático (parche 7 del bloque AUTO).

> **Nota de seguridad (2026-07-19):** las propuestas que seleccionan
> `linearfit` quedan reemplazadas por Winsorized/sigma. La implementación
> histórica ordenaba intensidades y no ajustaba frame contra referencia; por
> ello está deshabilitada en UI, preflight y runtime hasta disponer de una
> regresión robusta real y rechazo sobre residuales.

Diseños completos generados el 2026-07-12 por auditoría multi-agente.
Los parches de drizzle (activación en perfiles, convención centro, agujeros,
avisos) YA están aplicados; este documento conserva los DOS bloques grandes
que requieren sesión dedicada con validación de paridad GPU.

## Receta AUTO completa

AUDITORÍA + DISEÑO "RECETA AUTO" (perfil auto real, 100% automático)

== 1. QUÉ MIDE HOY EL PREFLIGHT (prepare_deepsky_stack, deepsky.rs:4685) ==
Señales YA medidas: nº de lights válidos (probes), geometría WxHxCh, patrón Bayer, filtro con id estable narrowband (ds_probe_filter_id → HA/OIII/SII/HA_OIII/SII_OIII, deepsky.rs:720-800), clusters de exposición, gain/binning/temperatura, sesiones por noche + matriz lights↔flats (session_map), presión de RAM (estimated_ram_mb vs host), capacidad/paridad GPU, y fallbacks de rechazo (drizzle>1 o n<3 o franja>2GB → sigma, deepsky.rs:4949-4968).
La recomendación actual (deepsky.rs:4998-5028) usa SOLO 3 señales: memory_pressure>=70 → Fast; n>=12 && darks+flats → MaximumQuality; resto → Balanced. NO usa filtro, dithering, gradiente, densidad estelar ni histograma.
Medido barato pero NO consumido por el preflight: inspect_deepsky_frames (deepsky.rs:4372) ya calcula por frame en luma <=1600px: nº estrellas, FWHM, ruido MAD, excentricidad y fondo (ds_bg_noise:1206); ds_local_bg_grid (1233) ya modela el fondo espacial; el detector de amp glow (ds_dark_has_amp_glow:1065) solo corre en stack, no en preflight; los offsets de registro (DsRegistration:1903, caché DsRegistrationCacheFile:4023) nunca se usan para detectar dithering.
NO medido aún y barato (3-5 lights muestreados con ds_inspection_luma): (a) dithering = RMS del offset residual entre centroides de first/middle/last tras quitar deriva lineal; (b) gradiente = ajuste de plano al grid 8x8 de fondo, fuerza=(max-min)/ruido; (c) densidad estelar = estrellas/Mpx; (d) nebulosa oscura/fondo tenue = mediana del fondo < 2·ruido y >90% de píxeles en [bg±2σ] con <40 estrellas/Mpx; (e) mediana de flats (validez 20-70% del rango); (f) % de píxeles negativos tras calibrar el prototipo.

== 2. TABLA DE DECISIÓN AUTO (umbrales concretos) ==
RECHAZO por nº de lights N (por grupo/filtro):
- N<8 → rejection="average", cosmetic=true, clip_iters=1. Justificación: con <8 muestras σ/MAD son inestables y el clipping muerde señal; la cosmética CFA cubre hot pixels (además n<3 ya cae a sigma por fallback existente).
- 8<=N<16 → "sigma", κ=3.0/3.0, iters=2. σ iterativo es el mejor compromiso sesgo/varianza con estadística corta.
- 16<=N<=50 → "winsorized", κ_low=2.8, κ_high=3.0, iters=3. Winsorized conserva ~95% de eficiencia estadística y elimina satélites/rayos.
- N>50 → "linearfit", κ_low=5.0, κ_high=2.5, iters=3. Con >50 muestras el ajuste lineal por píxel absorbe variaciones de transparencia/gradiente entre noches (dataset del usuario: 120 lights multi-noche). Restricción existente: si drizzle>1 el motor cae a sigma → en AUTO nunca combinar linearfit/winsorized con drizzle.
NORMALIZACIÓN:
- gradiente detectado (rango plano > 3·ruido) O >=2 sesiones → "local" (el modelo 24x24 ya existe).
- fondo estable, 1 sesión → "scaling".
- NEBULOSA OSCURA/fondo tenue (bg<2·ruido) → "additive" (el escalado multiplicativo amplifica diferencias de fondo casi nulo) + κ_low=max(κ_low,4.0) para NO recortar la cola baja + gradient=false forzado + aviso "protección de fondo tenue activa".
BANDA ESTRECHA (filtro ∈ {HA,OIII,SII,HA_OIII,SII_OIII}): gradient=false (sin SCNR/ABE, ya es el default científico), κ_high=3.5 (conservar señal débil de emisión), normalización additive si fondo tenue o local si multi-sesión; nunca scaling puro con fondo <2·ruido.
DRIZZLE: dithering RMS>0.7px + N>=30 + FWHM<2.5px (submuestreo real) + estimated_ram_mb·(2·2)<60% RAM → drizzle=2.0, pixfrac=0.7 y rechazo sigma κ=2.5/2.5 (por el fallback obligatorio); si no se cumple todo, drizzle=1 y solo razón informativa "dithering detectado: drizzle 2x disponible".
INTERPOLACIÓN: lanczos3 siempre salvo memory_pressure>=70 → bilinear + resto de receta Fast.
PRIORIDAD de reglas: presión RAM > robustez (pedestal/flats) > nebulosa oscura > banda estrecha > tabla N > drizzle.

== 3. DÓNDE ENCHUFAR SIN ROMPER CUSTOM ==
Nuevo variante PipelineProfile::Auto (serde "auto"). resolved_profile NO lo toca (no tiene datos); una nueva ds_resolve_auto_recipe(request, señales) se aplica en prepare_deepsky_stack y en run_deepsky_stack ANTES de construir el plan, convirtiendo el request a los valores de la tabla y devolviendo las razones, que se publican en recommendation_reasons + nuevo campo resolved_recipe del PreparedStackPlan (la UI ya pinta effective_rejection/normalization_model, así que la receta queda visible sin tocar el render). Custom sigue intacto: el brazo Custom de resolved_profile no cambia y ds_resolve_auto_recipe solo corre con profile==Auto. Frontend: botón "Auto (recomendado)" como preset por defecto que envía profile:"auto"; los 4 presets actuales quedan igual.

== 4. ROBUSTEZ ==
- Pedestal automático: calibrar el prototipo (ya se lee en deepsky.rs:6522) y si fracción de negativos>45% o mediana<0 (sobre-resta: lo esperable es ~50% del RUIDO de fondo negativo, no del frame), pedestal=ceil(-p0.5·1.05) clamp 50..1000 ADU, resuelto ANTES de cache_fingerprint (que ya incluye ped{:?}) para no invalidar caché entre runs.
- Flats inválidos: en preflight, leer 1 flat por sesión y si mediana fuera del 20-70% del rango de saturación → error bloqueante "flats sobre/sub-expuestos" (hoy no hay ninguna validación fotométrica de flats).
- Bias faltante con darks escalables: si bias vacío && optimize_dark && exposición darks != lights (>10%) → warning "el escalado k del dark sin bias sesga la resta; añade bias o usa darks de la misma exposición" (el flujo con amp glow ya fuerza k=1 y no lo necesita).
NOTA de coordinación: deepsky.rs se está editando en OTRA sesión paralela (memoria del proyecto) — este es un PLAN, no se ha tocado código; aplicar los parches de deepsky.rs cuando esa sesión libere el archivo.

### Parche 0: Añadir el perfil AUTO al contrato tipado (serde 'auto') sin alterar Fast/Balanced/Max/Custom
- Archivo: `src-tauri/src/pipeline.rs`:114
- Ganancia: Un único punto de contrato para CLI/API/UI; 'auto' viaja igual que los demás perfiles
- Riesgo: Bajo: enum exhaustivo — el compilador señala todos los match a actualizar; serde snake_case genera 'auto' automáticamente. Riesgo real: algún match sobre PipelineProfile en commands_* que haya que cubrir (el build lo detecta).
- Ancla:
```rust
pub enum PipelineProfile {
    Fast,
    #[default]
    Balanced,
    MaximumQuality,
    Custom,
}
```
- Cambio:
```
Insertar variante `Auto,` tras `Custom`. En los tres resolved_profile del archivo, añadir Auto a los brazos vacíos: en PlanetaryAnalysisRequest y PlanetaryStackRequest cambiar `PipelineProfile::Balanced | PipelineProfile::Custom => {}` por `PipelineProfile::Balanced | PipelineProfile::Custom | PipelineProfile::Auto => {}` (en planetario Auto==Balanced por ahora); en DeepSkyStackRequest añadir brazo `PipelineProfile::Auto => {} // se resuelve con señales medidas en ds_resolve_auto_recipe` junto a `PipelineProfile::Custom => {}`. Añadir test: Auto no sobrescribe nada en resolved_profile (paridad con Custom).
```

### Parche 1: Exponer la receta resuelta del modo AUTO en el plan de preflight
- Archivo: `src-tauri/src/pipeline.rs`:482
- Ganancia: Razones y valores AUTO auditables en la UI y en la receta JSON sin cambiar el render existente
- Riesgo: Muy bajo: campo aditivo serializable; la UI antigua lo ignora (camelCase resolvedRecipe).
- Ancla:
```rust
    pub recommended_profile: PipelineProfile,
    pub recommendation_reasons: Vec<String>,
```
- Cambio:
```
Añadir a PreparedStackPlan: `pub resolved_recipe: BTreeMap<String, String>,` (claves: rejection, kappa_low, kappa_high, clip_iters, normalization, interpolation, drizzle, pixfrac, pedestal, señales medidas: dithering_rms, gradient_strength, background_over_noise, star_density, narrowband, dark_nebula). Vacío cuando profile != Auto.
```

### Parche 2: Medidor barato de señales AUTO (dithering, gradiente, densidad estelar, fondo tenue, banda estrecha)
- Archivo: `src-tauri/src/deepsky.rs`:4682
- Ganancia: Todas las señales de la tabla AUTO medidas con lecturas ya existentes, sin pasada extra completa
- Riesgo: Medio: dithering con solo 3-5 muestras es una estimación (deriva no lineal puede inflar RMS) — usar umbral conservador 0.7 px y tratarlo solo como habilitador de drizzle, nunca bloqueante. Nebulosa oscura con luna/gradiente fuerte puede dar falso negativo (seguro: solo endurece κ_low cuando SÍ se detecta). ATENCIÓN: deepsky.rs se edita en sesión paralela — coordinar antes de aplicar.
- Ancla:
```rust
/// Preflight tipado del asistente: valida geometría/metadatos y estima el plan
/// efectivo antes de reservar varios GB o iniciar un stack largo.
```
- Cambio:
```
Insertar antes: `struct DsAutoSignals { n_lights: usize, narrowband: bool, filter: Option<String>, background: f32, noise: f32, background_over_noise: f32, gradient_strength: f32, stars_per_mpx: f32, fwhm: f32, dark_nebula: bool, dithering_rms: Option<f32>, sessions: usize }` y `fn ds_measure_auto_signals(probes: &[&DsProbe]) -> DsAutoSignals`. Implementación: muestrear first/middle/last (3 lights, 5 si n>60) con ds_read_image + ds_inspection_luma (ya limita a 1600px); por muestra: ds_detect_stars(luma,w,h,200), ds_bg_noise, ds_frame_fwhm_proxy; gradiente: ds_local_bg_grid(luma,w,h,8,8) + ajuste de plano vía ds_least_squares (ya existe, línea 1929), gradient_strength=(max-min del plano)/ruido; dark_nebula = mediana(bg)<2·ruido && stars_per_mpx<40·factor² ajustado && fracción de píxeles en [bg±2σ]>0.90; dithering: emparejar por vecino más cercano las estrellas de cada muestra contra la primera, offset mediano por muestra, quitar deriva lineal (regresión sobre índice temporal) y RMS residual ·factor → dithering_rms en px reales; narrowband = ds_probe_filter_id(primer probe) ∈ {HA,OIII,SII,HA_OIII,SII_OIII}. Coste: 3-5 lecturas downscaled (~1-2 s), aceptable en preflight.
```

### Parche 3: Función de decisión AUTO: tabla completa con umbrales
- Archivo: `src-tauri/src/deepsky.rs`:4682
- Ganancia: Tabla de decisión única, determinista, con razones visibles — el corazón del 'preparado para mí'
- Riesgo: Bajo en sí misma (función pura, testeable). El riesgo es de criterio: κ y umbrales son opinables — quedan en UN sitio y salen en resolved_recipe para poder auditarlos y ajustar.
- Ancla:
```rust
fn prepare_deepsky_stack(request: DeepSkyStackRequest) -> PreparedStackPlan {
```
- Cambio:
```
Insertar encima: `fn ds_resolve_auto_recipe(mut request: DeepSkyStackRequest, signals: &DsAutoSignals, memory_pressure: u64) -> (DeepSkyStackRequest, Vec<String>)`. Orden de reglas (cada una empuja su razón textual): (1) memory_pressure>=70 → receta Fast íntegra + razón y RETURN temprano. (2) Base por N=signals.n_lights: N<8 → average/cosmetic/iters1; 8..16 → sigma κ3.0/3.0 iters2; 16..=50 → winsorized κ2.8/3.0 iters3; >50 → linearfit κ5.0/2.5 iters3. (3) Normalización: gradient_strength>3.0 || sessions>1 → local; si no → scaling. (4) narrowband → gradient=false, kappa_high=κ_high.max(3.5), y si background_over_noise<2.0 → normalization=additive. (5) dark_nebula → kappa_low=κ_low.max(4.0), normalization=additive, gradient=false, razón 'protección de fondo tenue: κ_low 4.0, fondo aditivo, anti-gradiente OFF'. (6) drizzle: dithering_rms>0.7 && N>=30 && fwhm<2.5 && RAM 2x cabe (<60% host) → drizzle=2.0, pixfrac=0.7, rejection='sigma', κ2.5/2.5 (fallback obligatorio del motor); si dithering pero falta otra condición → solo razón informativa. (7) interpolation='lanczos3', cosmetic=Some(true), optimize_dark=Some(true), auto_crop=true siempre (salvo regla 1). Devuelve (request_modificado, reasons). Test unitario por regla con DsAutoSignals sintéticos (120 lights narrowband+dark_nebula del dataset del usuario: linearfit κ5.0/3.5→κ_low4.0, additive, sin SCNR).
```

### Parche 4: Enchufar AUTO en el preflight y en la recomendación
- Archivo: `src-tauri/src/deepsky.rs`:4688
- Ganancia: El plan que ve el usuario ES la receta que se ejecutará, con razones medidas, cero clics
- Riesgo: Medio: hay que reordenar con cuidado — la validación de kappa/drizzle (líneas 4714-4732) debe correr DESPUÉS de resolver AUTO para validar los valores finales, y los fallbacks existentes de rechazo (4949) deben seguir aplicando sobre la receta AUTO. Cubrir con test de integración del preflight.
- Ancla:
```rust
    let request = request.resolved_profile();
    let plan_id = new_job_id("ds-plan");
    let probes = deepsky_probe(request.lights.clone());
```
- Cambio:
```
Tras obtener valid_probes y estimaciones de RAM (mover el cálculo de memory_pressure arriba o resolver AUTO justo antes del bloque recommended_profile): si request.profile==PipelineProfile::Auto → let signals=ds_measure_auto_signals(&valid_probes); let (request, auto_reasons)=ds_resolve_auto_recipe(request, &signals, memory_pressure); rehacer requested/effective_rejection con la receta resuelta y rellenar plan.resolved_recipe. En el bloque `let (recommended_profile, recommendation_reasons) = if memory_pressure >= 70 {` (línea 4998): cuando el perfil es Auto, recommended_profile=PipelineProfile::Auto y recommendation_reasons=auto_reasons (las señales medidas sustituyen a la heurística de 3 casos, que se conserva para los perfiles manuales).
```

### Parche 5: Paridad plan↔ejecución: resolver AUTO también en run_deepsky_stack
- Archivo: `src-tauri/src/deepsky.rs`:5150
- Ganancia: Imposible que el stack ejecute una receta distinta de la mostrada en el plan
- Riesgo: Medio: si prepare y run midieran señales por separado podrían divergir (muestras distintas por archivos cambiados) — por eso el helper compartido y el fingerprint de fuentes ya existente protegen la coherencia.
- Ancla:
```rust
    let request = request.resolved_profile();
    let plan = prepare_deepsky_stack(request.clone());
    if !plan.valid {
```
- Cambio:
```
Si request.profile==Auto: resolver señales+receta con las MISMAS funciones antes de llamar a prepare (o mejor: hacer que prepare devuelva el request resuelto en el plan — añadir método privado `fn ds_resolved_request(request) -> (DeepSkyStackRequest, Option<(DsAutoSignals, Vec<String>)>)` compartido por ambos comandos para que no puedan divergir). Registrar en el log frontal la receta AUTO aplicada (log_to_front INFO con resolved_recipe).
```

### Parche 6: Detección de flats inválidos (sobre/sub-expuestos) en preflight
- Archivo: `src-tauri/src/deepsky.rs`:4778
- Ganancia: Hoy NINGUNA validación fotométrica de flats: un panel malo arruina 120 lights en silencio
- Riesgo: Bajo-medio: coste de 1 lectura de flat por sesión en preflight (asumible); falsos positivos con flats sky de banda estrecha legítimamente bajos → considerar degradar a WARNING (no error) cuando narrowband=true.
- Ancla:
```rust
    if request.flats.is_empty() {
        warnings.push("Sin flats: no se corregirá viñeteo/PRNU".into());
    }
```
- Cambio:
```
Añadir después: si hay flats, leer UN flat por sesión (flat_sessions_for_map ya da la lista; ds_read_image + mediana muestreada estilo ds_cosmetic_stats). Rango de saturación: máximo teórico del formato (65535 para 16-bit; si el máximo observado <=1.0 tratar como normalizado y usar 1.0). Si mediana<20% del rango → error 'Flats subexpuestos (mediana {p}% del rango): corrigen mal el viñeteo y amplifican ruido; repítelos a 30-50%'. Si mediana>70% → error 'Flats cerca de saturación (mediana {p}%): la PRNU queda no lineal'. Umbral 20-70%: fuera de ahí la respuesta del sensor no es fiable (práctica WBPP/SIRIL).
```

### Parche 7: Pedestal automático cuando la calibración deja el frame mayoritariamente negativo
- Archivo: `src-tauri/src/deepsky.rs`:6535
- Ganancia: Robustez WBPP-parity ante darks calientes/bias mal restado sin intervención del usuario
- Riesgo: Medio: una calibración extra del prototipo (~1 frame, ya en RAM); si el prototipo es atípico (avión/nube) el pedestal puede sobrar — inofensivo: solo desplaza el cero en float32 sin recortar (ds_apply_pedestal no clampa la señal).
- Ancla:
```rust
    let cache_fingerprint = format!(
        "calv2-fs{flat_sessions}-{}-{}x{}x{}-cos{}-dark{}-ped{:?}-drz{:.2}",
```
- Cambio:
```
Antes de construir cache_fingerprint: si pedestal.is_none() (o modo AUTO), calibrar una copia del `prototype` (ya cargado en línea 6522) con ds_calibrate + masters ya construidos; medir fracción de negativos y percentil 0.5% muestreado. Si neg_frac>0.45 o mediana<0.0 → pedestal_auto = (−p0.5·1.05).ceil().clamp(50.0,1000.0); usar `let pedestal = pedestal.or(pedestal_auto);` ANTES del fingerprint (que ya incluye ped{:?}, así la caché es coherente) y log_to_front WARN 'Pedestal automático +{n} ADU: la calibración dejaba {p}% de píxeles negativos (posible sobre-resta de dark/bias)'. La aplicación en línea 6743 (`ds_apply_pedestal(&mut img, pedestal)`) no cambia. Umbral 45%: ~50% del FONDO negativo es legítimo (ruido centrado en cero), pero >45% del FRAME entero implica que también la señal quedó bajo cero.
```

### Parche 8: Aviso de bias faltante cuando el dark se escala
- Archivo: `src-tauri/src/deepsky.rs`:4775
- Ganancia: Explica un fallo de calibración clásico ANTES de gastar una hora de stack
- Riesgo: Muy bajo: solo un warning; la información (exposiciones de darks) ya se sondea en el mismo preflight.
- Ancla:
```rust
    if request.darks.is_empty() {
        warnings.push("Sin darks: se recomendará corrección cosmética".into());
    }
```
- Cambio:
```
Añadir después: si !request.darks.is_empty() && request.bias.is_empty() && request.optimize_dark.unwrap_or(true) → probar exposiciones de darks (deepsky_probe ya se llama para la selección en 4848) y si difieren de la de los lights en >10% → warning 'Darks de exposición distinta sin bias: el escalado k del dark restará mal el offset del sensor; añade bias o usa darks de la misma exposición (con amp glow ya se fuerza k=1)'.
```

### Parche 9: Preset AUTO en la UI como opción por defecto
- Archivo: `index.html`:1424
- Ganancia: El flujo por defecto pasa a ser 'no elijas nada': un clic menos y sin decisiones para el usuario
- Riesgo: Muy bajo: markup aditivo; verificar que dsSyncWizard/clases .recommended siguen funcionando con 5 botones.
- Ancla:
```rust
                    <div class="ds-preset-btns">
                        <button type="button" class="ds-preset" data-preset="fast" data-i18n="deepsky.preset_fast">Rápido</button>
```
- Cambio:
```
Insertar como primer botón: `<button type="button" class="ds-preset active" data-preset="auto" data-i18n="deepsky.preset_auto">Auto</button>` y quitar `active` del botón balanced. Añadir nota en ds-profile-help: '<b>Auto</b>Mide tus datos (nº de lights, filtro, dithering, gradiente, fondo) y fija la receta óptima; verás cada decisión y su motivo en el plan.' Claves i18n deepsky.preset_auto / deepsky.profile_auto_note en src/locales/es.json y en.json (bloque "deepsky" existente, es.json:975). Sin emojis; iconos solo del sprite zas-icon (regla del proyecto).
```

### Parche 10: Frontend: enviar profile:'auto' y no pisar la receta con los controles del formulario
- Archivo: `src/main.js`:8885
- Ganancia: Cero intervención: abrir módulo → plan AUTO con receta y razones ya rellenas
- Riesgo: Bajo: si dsActivePreset='auto' y el backend antiguo no conoce 'auto', serde fallaría — desplegar backend y frontend en el mismo release (mismo repo/branch, sin riesgo real).
- Ancla:
```rust
    const profile = ({ fast: "fast", balanced: "balanced", max: "maximum_quality" })[dsActivePreset] || "custom";
```
- Cambio:
```
Cambiar el mapa a `({ auto: "auto", fast: "fast", balanced: "balanced", max: "maximum_quality" })[dsActivePreset] || "custom"` e inicializar dsActivePreset="auto". El resto del request puede seguir enviando los valores del formulario: el backend los ignora para todo perfil != custom (resolved_profile + ds_resolve_auto_recipe mandan). En dsApplyPreparedPlan (main.js:9041), cuando plan.resolvedRecipe exista, reflejar los valores resueltos en los controles avanzados en modo solo-lectura (o chip-resumen) para que el usuario VEA la receta elegida, y mantener plan.recommendationReasons como ya se pinta (dsFormatPreflight:9014).
```

### Parche 11: Sesión multibanda: AUTO por grupo de filtro (Ha+OIII y SII+OIII pueden recibir recetas distintas)
- Archivo: `src-tauri/src/deepsky.rs`:5248
- Ganancia: El dataset real del usuario (dual-band por noches) queda cubierto grupo a grupo sin código extra
- Riesgo: Muy bajo: comportamiento emergente del diseño; el test lo fija por contrato.
- Ancla:
```rust
        let plan = prepare_deepsky_stack(group.request);
```
- Cambio:
```
Ningún cambio estructural necesario: al resolver AUTO dentro de prepare_deepsky_stack, cada grupo de la sesión multibanda (dual-band del usuario) obtiene su receta con SUS señales (nº lights, fondo y dithering propios). Añadir test de sesión: dos grupos con N distinto (p.ej. 70 Ha+OIII vs 50 SII+OIII) producen rechazos distintos (linearfit vs winsorized) y ambos aparecen en sus resolved_recipe.
```

## Rescate de detalle (algoritmo estrella)

ELECCIÓN: Familia A — PONDERACIÓN LOCAL DE CALIDAD ("Rescate de detalle", lucky-DSO). Es la mejor de las tres para este codebase: (1) los tres motores de integración ya son medias ponderadas lineales con peso escalar por frame `fw∈[0.3,1]` (registered: Vec<(usize, DsTransform, f64)>, deepsky.rs:7073), así que convertir el peso en un campo espacial w_k(x,y)=fw_k·q_k(x,y) reutiliza TODO el plumbing existente — en particular el patrón loc_fields + ds_sample_grid (deepsky.rs:1263) que ya se muestrea por píxel en los 3 motores CPU y en los shaders WGSL; (2) mantiene linealidad y fotometría exactas: la media ponderada Σw_k·v_k/Σw_k es insesgada si w es independiente de v, y q se mide solo de FORMA (FWHM local de estrellas) y de estructura normalizada (energía de gradiente tras normalización mul/add), nunca del brillo; (3) es el diferenciador real: PixInsight/Siril/DSS solo ponderan POR FRAME — nadie en ese segmento pondera por región. B (drizzle iterativo IBP+TV) se descarta como núcleo: solo beneficia a usuarios con dithering denso, rompe la garantía "máster = media lineal" y arriesga ringing fotométrico; A además MEJORA el drizzle existente gratis (los pesos locales entran en el kernel drop). C ya existe a medias (gpu_wavelet::gpu_richardson_lucy) y es no-lineal: se propone como paso OPCIONAL "Detalle+" post-máster en fase 2, midiendo la PSF del máster con ds_fit_star_psf — no requiere diseño nuevo, solo wiring, y queda fuera de este plan.

MATEMÁTICA CONCRETA. Rejilla de medición por frame en espacio del frame: tiles de 256 px → para 4144×2822: 17×12 celdas. Por tile t y frame k: F_k(t) = mediana de FWHM de las estrellas PSF-ajustadas dentro del tile dilatado ±½ tile (reutiliza ds_fit_star_psf; NaN si <3 estrellas); G_k(t) = media de |∇L|² (diferencias centrales sobre la luma) debiased por ruido: G'_k(t)=max(G_k−c·σ_k²,ε) con c calibrado para el estimador (var del gradiente central de ruido puro = σ²), y corregida por transparencia G''_k = G'_k·mul_k² (la normalización de flujo ya calculada en norms[k] cancela la dependencia del brillo). Tras medir TODOS los frames: F_ref(t)=percentil 20 inter-frame de F_k(t), G_ref(t)=mediana inter-frame de G''_k(t). Calidad: q_k(t) = clamp((F_ref(t)/F_k(t))^2, 0.15, 1) · clamp((G''_k(t)/G_ref(t))^0.5, 0.15, 1.25), renormalizada por tile para que max_k q_k(t)=1, suavizada 3×3 y con celdas sin métrica = 1 (neutras). El campo se remuestrea UNA vez a espacio de referencia (nodo de rejilla de salida 24×24 → t.inverse → coordenada frame → muestreo bilineal del grid frame-space) produciendo wq_fields: Vec<Option<Vec<f32>>> paralelo a loc_fields; interpolación bilineal en integración vía ds_sample_grid (sin costuras). NORMALIZACIÓN POR PÍXEL: automática — los 3 motores ya dividen por Σw acumulado (wgt_row, coverage, weight[pix]), así que Σ_k ŵ_k(x)=1 implícito. PARIDAD: wq=None recorre EXACTAMENTE el código actual (bit-idéntico, sin multiplicación extra); además la fase 3.7 colapsa a None cualquier campo con max|q−1|<1e-3. ORDEN rechazo→pesos: intacto — en ds_reject_pixel el rechazo decide por VALORES y los pesos solo entran en la media ponderada de supervivientes; en κσ streaming las ventanas ±κσ siguen calculándose de los momentos ponderados como hoy.

DÓNDE SE MIDE: en el bucle de calibración/detección (deepsky.rs:6853) donde luma+stars ya están en RAM — coste marginal ~10-20 ms/frame; se persiste en DsAnalysisCacheFile (bump DS_PREP_CACHE_VERSION 3→4). COSTE 120×11.7Mpx: RAM +120·(17·12+24·24)·4B ≈ 0.4 MB (nada); medición +~2 s total; integración tiled/streaming +1 muestreo de grid y 1 multiplicación por píxel·frame ≈ +3-7 % CPU, insignificante en GPU (el shader ya muestrea el grid de local-norm); drizzle idéntico (multiplica el área del drop). VRAM extra en M5: 120 grids f32 ≈ 0.3 MB dentro del budget 2457 MB.

TEST DE VALIDACIÓN (sintético, unit test sin AppHandle, patrón del test drop-kernel existente): 8 frames 256×256 con rejilla 4×4 de estrellas gaussianas σ=1.2 px + nebulosa sinusoidal + ruido; frames 0-3 nítidos en mitad izquierda y blur σ=2.5 en la derecha, frames 4-7 al revés. Integrar con ds_warp_accumulate identidad dos veces: uniforme (wq=None) vs ponderado. Asserts: (1) FWHM del máster ponderado (ds_frame_fwhm_proxy) < FWHM del uniforme con margen ≥5 %; (2) flujo de apertura de cada estrella (r=6, fondo anular restado) |Δ|<0.1 % y nivel de fondo |Δ|<0.1 % → sin sesgo fotométrico; (3) paridad: wq=None produce buffers bit-idénticos al baseline y grid≈1 colapsa a None. Paridad CPU/GPU: extender ensure_parity y ensure_tiled_parity con un caso de grids no triviales (RMSE<0.5 ADU, criterio ya usado).

EXPOSICIÓN: flag `local_weighting` en DeepSkyStackRequest (default false; ON en perfil MaximumQuality), parámetro opcional en el comando stack_deepsky, toggle en UI (icono del sprite zas-icon, NUNCA emoji), q media por frame añadida al ds-report y al recipe. NOTA DE COORDINACIÓN: la memoria del proyecto indica que deepsky.rs se está editando en OTRA sesión paralela — este plan usa anclas verificadas hoy sobre el working tree actual; validar que sigan presentes antes de aplicar.

### Parche 0: Añadir el flag local_weighting al request de cielo profundo (backend-resolved, como el resto de opciones)
- Archivo: `src-tauri/src/pipeline.rs`:365
- Ganancia: Un único punto de verdad para CLI/API/UI del nuevo modo.
- Riesgo: Bajo: serde(default) mantiene compatibilidad con recipes/CLI existentes.
- Ancla:
```rust
    #[serde(default)]
    pub pedestal: Option<f32>,
}
```
- Cambio:
```
Añadir campo `#[serde(default)] pub local_weighting: bool,` antes del cierre de DeepSkyStackRequest. Default false = comportamiento actual.
```

### Parche 1: Activar Rescate de detalle en el perfil MaximumQuality (los perfiles se resuelven en backend)
- Archivo: `src-tauri/src/pipeline.rs`:425
- Ganancia: El modo diferenciador llega a usuarios sin tocar controles avanzados.
- Riesgo: Medio-bajo: cambia el resultado del perfil MaximumQuality (deliberado); Custom no se toca.
- Ancla:
```rust
            PipelineProfile::MaximumQuality => {
                self.rejection = "winsorized".into();
```
- Cambio:
```
Dentro del brazo MaximumQuality añadir `self.local_weighting = true;`; en Fast y Balanced añadir `self.local_weighting = false;` explícito para que el plan resuelto sea determinista. Actualizar el test deepsky_profiles_resolve_in_backend_and_custom_is_lossless (pipeline.rs:772) con el campo nuevo.
```

### Parche 2: Aceptar el parámetro en el comando Tauri stack_deepsky
- Archivo: `src-tauri/src/deepsky.rs`:6224
- Ganancia: Punto de entrada único del feature en el pipeline.
- Riesgo: Bajo: parámetro opcional, invoke antiguo sigue funcionando.
- Ancla:
```rust
    compute_policy: Option<ComputePolicy>,
) -> Result<String, String> {
```
- Cambio:
```
Añadir `local_weighting: Option<bool>,` antes de compute_policy y resolver `let use_local_weighting = local_weighting.unwrap_or(false) && !single_light;` junto al bloque de opciones (tras `let use_crop = auto_crop.unwrap_or(true);`, línea 6286). Registrar en el recipe JSON y en el log INFO inicial.
```

### Parche 3: Nuevas funciones de métrica local: FWHM por estrella con posición y rejilla de calidad por tile
- Archivo: `src-tauri/src/deepsky.rs`:1620
- Ganancia: Métrica de nitidez local medible, independiente del brillo (FWHM = forma; gradiente = debiased y luego ratio inter-frame).
- Riesgo: Bajo: funciones puras nuevas; el refactor de ds_frame_fwhm_proxy debe conservar la mediana idéntica (cubierto por test_ds_gaussian_psf_fit_recovers_subpixel_centroid_and_fwhm, línea 8144).
- Ancla:
```rust
fn ds_frame_fwhm_proxy(luma: &[f32], w: usize, h: usize, stars: &[(f32, f32, f32)]) -> f32 {
```
- Cambio:
```
Insertar ANTES dos funciones: (1) `fn ds_star_fwhm_samples(luma, w, h, stars, max_n) -> Vec<(f32, f32, f32)>` — refactor del cuerpo de ds_frame_fwhm_proxy que devuelve (x, y, fwhm) por estrella (ds_fit_star_psf + fallback de momentos, hasta 120 estrellas en vez de 20); ds_frame_fwhm_proxy pasa a ser la mediana de esa lista (misma salida, un solo fit). (2) `fn ds_local_quality_metrics(luma, w, h, star_fwhms, noise) -> (Vec<f32>, Vec<f32>, usize, usize)` — rejilla de tiles de 256 px (gw=ceil(w/256), gh=ceil(h/256)); por tile: F(t)=mediana de FWHM de estrellas en el tile dilatado ±½ tile (NaN si <3), G(t)=media de ((L[x+1]-L[x-1])²+(L[y+1]-L[y-1])²)/4 con debias max(G−σ²,1e-6). SIMD-friendly (bucle plano por filas, rayon opcional).
```

### Parche 4: Persistir las métricas locales en la caché de análisis v4
- Archivo: `src-tauri/src/deepsky.rs`:4006
- Ganancia: Re-apilados con parámetros distintos no re-miden nada.
- Riesgo: Bajo: el bump fuerza recalibración una vez (coste conocido y comunicado por el log de FrameStore).
- Ancla:
```rust
struct DsCachedFrameAnalysis {
    path: String,
    stars: Vec<(f32, f32, f32)>,
    fwhm: f32,
    background: f32,
    noise: f32,
    eccentricity: f32,
}
```
- Cambio:
```
Añadir campos `#[serde(default)] local_fwhm: Vec<f32>, #[serde(default)] local_grad: Vec<f32>, #[serde(default)] local_gw: usize, #[serde(default)] local_gh: usize` (crudos, SIN combinar inter-frame — la combinación depende del conjunto de frames y se hace en 3.7). Bump `const DS_PREP_CACHE_VERSION: u32 = 3;` (línea 4003) a 4 para invalidar cachés v3 sin las métricas.
```

### Parche 5: Medir las métricas locales en el bucle de calibración (luma y estrellas ya en RAM)
- Archivo: `src-tauri/src/deepsky.rs`:6853
- Ganancia: Cero relecturas de disco: la medición viaja con la calibración existente.
- Riesgo: Medio-bajo: toca el bucle caliente de calibración; +~15 ms/frame medidos solo cuando no hay caché. Mantener el skip si !use_local_weighting para coste cero cuando está OFF (guardando vectores vacíos y recalculando si luego se activa — documentar en el fingerprint NO incluir el flag para no duplicar cachés).
- Ancla:
```rust
            let fwhm = ds_frame_fwhm_proxy(&luma, img.w, img.h, &stars);
```
- Cambio:
```
Sustituir por: `let star_fwhms = ds_star_fwhm_samples(&luma, img.w, img.h, &stars, 120); let fwhm = median(star_fwhms);` y tras ds_bg_noise añadir `let (local_fwhm, local_grad, lgw, lgh) = ds_local_quality_metrics(&luma, img.w, img.h, &star_fwhms, noise);` guardándolo en el DsCachedFrameAnalysis (línea 6856) y en un vector `frame_local_metrics: Vec<(Vec<f32>, Vec<f32>, usize, usize)>` paralelo a `frames` (push junto a frame_bgs, línea 6885). En la rama de caché (línea 6831) leer los campos nuevos.
```

### Parche 6: Fase 3.7: combinar métricas inter-frame y construir wq_fields en espacio de referencia
- Archivo: `src-tauri/src/deepsky.rs`:7287
- Ganancia: Campo de pesos listo con el mismo contrato de muestreo que local-norm: cero código nuevo de interpolación en los motores.
- Riesgo: Medio: la combinación inter-frame debe excluir frames no registrados (usar solo índices de `registered`); el percentil 20 con <5 frames degenera al mínimo — clamp inferior 0.15 lo hace seguro.
- Ancla:
```rust
    // --- 4. INTEGRATION — ITERATIVE κσ clipped mean (streaming) ---
```
- Cambio:
```
Insertar ANTES la sección '3.7 LOCAL QUALITY WEIGHTS (lucky-DSO)': (1) para cada tile t: F_ref(t)=percentil 20 de F_k(t) sobre frames registrados, G_ref(t)=mediana de G_k(t)·norms[k].0²; (2) q_k(t)=clamp((F_ref/F_k)^2,0.15,1)·clamp((G_k·mul²/G_ref)^0.5,0.15,1.25), celdas NaN→1, renormalizar por tile a max_k=1, suavizado box 3×3; (3) remuestrear a rejilla 24×24 en espacio de SALIDA: nodo (u,v)→px salida→/drz→t.inverse→coord frame→bilineal sobre el grid frame-space (reutiliza la convención de loc_fields/ds_sample_grid); (4) `let wq_fields: Vec<Option<Vec<f32>>>` paralelo a registered; None si !use_local_weighting o si max|q−1|<1e-3 (garantía de paridad). Log INFO con la q media/mín por frame y añadir mean_q al ds-report (línea 7110) y al recipe.
```

### Parche 7: Peso por píxel en el motor streaming CPU (ds_warp_accumulate)
- Archivo: `src-tauri/src/deepsky.rs`:2457
- Ganancia: Motor streaming + κσ iterativo ponderado localmente; media ponderada lineal intacta (se divide por wgt acumulado).
- Riesgo: Bajo: cambio mecánico; las ventanas κσ de las pasadas siguientes usan los mismos momentos ponderados (estadística consistente). Verificar los ~3 call sites (línea 7567 y tests).
- Ancla:
```rust
            for c in 0..ch {
                sum_row[x * ch + c] += vals[c] as f64 * frame_w;
                if let Some(sq) = sq_row.as_mut() {
                    sq[x * ch + c] += (vals[c] as f64) * (vals[c] as f64) * frame_w;
                }
            }
            wgt_row[x] += frame_w;
```
- Cambio:
```
Añadir parámetro `wq: Option<(&[f32], usize, usize)>` a ds_warp_accumulate (firma línea 2354, mismo patrón que `loc`). Tras calcular loc_off: `let wpx = wq.map(|(g, gw, gh)| frame_w * ds_sample_grid(g, gw, gh, x as f32 / w as f32, y as f32 / h as f32) as f64).unwrap_or(frame_w);` y usar `wpx` en sum/sq/wgt y en los pesos de rechazo de las líneas 2450-2451 (low_weight/high_weight). Con wq=None el binario emite el camino actual exacto.
```

### Parche 8: Peso por píxel en los dos kernels drizzle (drop y CFA)
- Archivo: `src-tauri/src/deepsky.rs`:2638
- Ganancia: Opción B parcialmente lograda gratis: drizzle 1-3× toma más señal de las tomas/regiones nítidas — super-resolución con mejor kernel efectivo.
- Riesgo: Medio-bajo: el test de conservación de flujo del drop-kernel (línea 8636) debe seguir pasando con wq=None; añadir variante con q constante 0.5 (flujo escala por q, la MEDIA no cambia).
- Ancla:
```rust
                                let rejected_weight = area * frame_w;
```
- Cambio:
```
Añadir el mismo parámetro `wq` a ds_drizzle_accumulate (línea 2507) y ds_drizzle_cfa_accumulate (línea 2683); calcular `let wpx = frame_w * q(x_out, y_out)` una vez por píxel de salida y sustituir `frame_w` por `wpx` en la acumulación del drop (sum += v·area·wpx, wgt += area·wpx) y en este anchor de rechazo (`area * wpx`). El muestreo usa coordenadas de salida normalizadas (x/w_out, y/h_out) — coherente con la construcción de wq_fields en espacio de salida.
```

### Parche 9: Peso por píxel en el motor tiled CPU (rechazo primero, pesos después)
- Archivo: `src-tauri/src/deepsky.rs`:5982
- Ganancia: Winsorized/linear-fit/mediana/percentile heredan la ponderación local sin alterar su estadística de rechazo.
- Riesgo: Bajo: con wq None por frame se reconstruye bit-idéntico buf.push((v, fw[k])). El muestreo del grid por (x,y,k) añade ~5 % al bucle; cachear el índice de celda por fila si el perf lo pide.
- Ancla:
```rust
                        let v = stack_ref[s0 + k];
                        if v.is_finite() {
                            buf.push((v, fw[k]));
                        }
```
- Cambio:
```
Añadir `wq_fields: &[Option<Vec<f32>>], wq_g: usize` a la firma de ds_integrate_tiled (línea 5834). En el bucle de rechazo: `let q = wq_fields[k].as_ref().map(|g| ds_sample_grid(g, wq_g, wq_g, x as f32 / w as f32, (oy0 + ly) as f32 / h as f32) as f64).unwrap_or(1.0); buf.push((v, fw[k] * q));`. NO tocar ds_reject_pixel: sus decisiones de rechazo son por VALOR (mediana/σ/ajuste) y los pesos solo entran en la media ponderada final `wmean` de los supervivientes — exactamente rechazo→pesos. present/coverage/rejected ya suman los pesos que reciben, así que los mapas científicos reflejan el peso efectivo sin cambios.
```

### Parche 10: Peso por píxel en el kernel WGSL de rechazo tiled GPU
- Archivo: `src-tauri/src/gpu_deepsky.rs`:285
- Ganancia: El modo MaximumQuality (winsorized) conserva su aceleración GPU en Metal/DX12 con pesos locales.
- Riesgo: Medio: es el parche más delicado — la paridad CPU/GPU del muestreo bilineal exige replicar exactamente el clamp de bordes de ds_sample_grid; el test de paridad extendido lo bloquea si difiere (fallback a CPU tiled ya existente).
- Ancla:
```rust
            let w = frame_weights[k];
```
- Cambio:
```
En reject_tiled_pass (línea 515): añadir parámetros `quality_grids: Option<(&[f32], usize)>` (grids concatenados n·G² + G) y `strip: (usize, usize, usize, usize)` (w, h_total, oy0, ch ya existe); nuevo binding read-only y campos en RejectParams (línea 249: grid_size, oy0, out_w, has_quality con padding a 16B). En el shader, derivar `let x = pixel % P.out_w; let y = P.oy0 + pixel / P.out_w;` y sustituir este anchor por `var w = frame_weights[k]; if (P.has_quality == 1u) { w = w * sample_grid(k, x, y); }` con sample_grid = bilineal idéntica a ds_sample_grid (misma convención de bordes). Con has_quality=0 se bindea un buffer dummy de 4 bytes y el camino es el actual. Actualizar el caller (deepsky.rs:5929) y extender ensure_tiled_parity con un caso de grids no triviales comparando contra el CPU tiled (criterio RMSE < 0.5 ADU ya usado).
```

### Parche 11: Peso por píxel en el kernel WGSL streaming (Welford ponderado)
- Archivo: `src-tauri/src/gpu_deepsky.rs`:816
- Ganancia: El camino GPU streaming (el por defecto en M5 Metal, 120 frames) ejecuta el algoritmo estrella sin coste medible (1 fetch de grid ya amortizado por el local-norm).
- Riesgo: Medio-bajo: cambio localizado en Params + 6 líneas de shader; el harness de paridad existente detecta cualquier desviación >0.5 ADU y degrada a CPU.
- Ancla:
```rust
    let old_w = weight[pix];
    let new_w = old_w + P.frame_weight;
```
- Cambio:
```
integrate_pass (línea 1485) ya recibe loc_fields y los sube como binding `grids` muestreado por píxel en espacio de salida: añadir un parámetro paralelo `wq_fields: &[Option<Vec<f32>>]` y un segundo grid por frame (o duplicar el buffer grids a 2 canales: [offset, quality]). En el shader calcular `let fw = select(P.frame_weight, P.frame_weight * quality(pix), P.has_quality == 1u);` y sustituir P.frame_weight por fw en: acumulación Welford (este anchor y las líneas 822-828: new_mean con fw/new_w, moment2 += fw·δ·δ') y en rejected_low/high (líneas 806-811). El Welford ponderado admite pesos arbitrarios sin cambios estructurales. Extender ensure_parity (línea 1677) con FrameMeta + grids de calidad no uniformes contra el acumulador CPU de referencia (líneas 1735-1736, multiplicando por el mismo q).
```

### Parche 12: Cablear wq_fields en los tres call sites de integración de stack_deepsky
- Archivo: `src-tauri/src/deepsky.rs`:7517
- Ganancia: Los 3 motores + drizzle quedan cubiertos con una sola fuente de pesos; trazabilidad completa en recipe/report.
- Riesgo: Bajo: wiring mecánico; el compilador fuerza actualizar todas las firmas (ventaja de Rust).
- Ancla:
```rust
        let tiled_result = ds_integrate_tiled(
            &app, &cancel, &load_cached, &registered, &norms, &loc_fields, LN_G,
```
- Cambio:
```
Pasar `&wq_fields, WQ_G` a: (1) ds_integrate_tiled aquí y en el retry CPU (línea 7536); (2) ds_integrate_gpu_streaming (línea 7400) → integrate_pass; (3) los tres accumulate del motor streaming CPU (líneas 7560-7567), construyendo `let wq_ref = wq_fields[k].as_ref().map(|f| (f.as_slice(), WQ_G, WQ_G));` como se hace con loc_ref. Añadir a effective_engine el sufijo ' · pesos locales' cuando algún campo es Some, y 'localWeighting': {enabled, meanQ, tiles} al recipe (ds_write_recipe, línea 3471).
```

### Parche 13: Test sintético de recuperación de detalle + neutralidad fotométrica + paridad
- Archivo: `src-tauri/src/deepsky.rs`:7985
- Ganancia: Demuestra cuantitativamente la promesa del feature: FWHM menor que la media simple con sesgo fotométrico <0.1 %, y blinda la paridad del camino uniforme.
- Riesgo: Bajo: test puro sin AppHandle (mismo patrón que el test drop-kernel de la línea 8636). NOTA: la memoria del repo pide no ejecutar `cargo test` completo en esta sesión paralela — correr `cargo test test_ds_local_quality -- --exact` acotado.
- Ancla:
```rust
    fn test_ds_session_night_id_cuts_at_local_noon() {
```
- Cambio:
```
Insertar antes: `fn test_ds_local_quality_weights_recover_detail_without_photometric_bias()`. Genera 8 frames 256×256 mono: rejilla 4×4 de estrellas gaussianas (σ=1.2 px, flujo 5000) + nebulosa sin(x/20)·sin(y/24)·80 + fondo 500 + ruido gaussiano σ=8; frames 0-3 convolucionados con σ_blur=2.5 SOLO en x>128, frames 4-7 solo en x<128. Integra dos veces con ds_warp_accumulate (transform identidad, norms (1,0), lanczos=false): A con wq=None, B con wq_fields de q_k(t) calculados por ds_local_quality_metrics+combinación 3.7. Asserts: (1) ds_frame_fwhm_proxy(master_B) < 0.95·ds_frame_fwhm_proxy(master_A); (2) por cada estrella, flujo de apertura r=6 con fondo anular r∈[8,12]: |flux_B/flux_A − 1| < 0.001, y mediana global |bg_B − bg_A|/bg_A < 0.001; (3) paridad: integración con wq=Some(grids todo-1.0) pasa por el colapso a None de la fase 3.7 → buffers bit-idénticos a A (assert_eq! sobre los f32 crudos). Segundo test corto: ds_reject_pixel con pesos desiguales conserva el estimador cuando todos los valores son iguales (sin sesgo del rechazo).
```

### Parche 14: Toggle de UI y payload del invoke (sin emojis, icono del sprite zas-icon)
- Archivo: `src/main.js`:8900
- Ganancia: Feature visible y explicado; el reporte por toma enseña al usuario QUÉ regiones aportaron más — el gancho diferenciador frente a PixInsight/Siril/DSS.
- Riesgo: Bajo: aditivo; con el checkbox ausente el payload emite false.
- Ancla:
```rust
        pixfrac: parseFloat(value("sel-ds-pixfrac", "0.8")) || 0.8,
```
- Cambio:
```
Añadir al payload `localWeighting: document.getElementById("chk-ds-local-weighting")?.checked || false,` (Tauri camelCase → local_weighting). En index.html añadir el checkbox junto a los controles avanzados de rechazo con icono del sprite zas-icon existente (nunca emoji) y en locales/es.json + en.json las claves ds.localWeighting.label ('Rescate de detalle (pesos locales)' / 'Detail rescue (local weights)') y .hint explicando: 'Pondera cada región de cada toma por su nitidez medida (FWHM local y detalle del fondo). Máster lineal y fotometría intactos.' Mostrar la q media por frame en la tabla del ds-report (columna 'Q local').
```
