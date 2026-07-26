# Auditoría UX — sesión multibanda de cielo profundo

Fecha: 2026-07-11  
Vista validada: asistente `Datos → Inspección de tomas → Receta → Revisar y apilar`  
Viewports: 1280×720 y 1000×700

## Recorrido y salud

1. **Datos — saludable.** La entrada conserva el patrón existente, la acción principal permanece fija y el contenido central es el único elemento desplazable.
2. **Inspección de tomas — corregido.** El estado anterior clasificaba Ha/SII como una mezcla inválida y mostraba claves técnicas sin jerarquía. Ahora reconoce `Ha + OIII` y `SII + OIII`, muestra calibración por grupo y presenta la sesión multibanda como un plan válido.
3. **Receta — saludable y ampliado.** Los perfiles básicos siguen visibles; la personalización multibanda permite controlar mezcla G/B de OIII, supresión de contaminación y paleta sugerida sin abrir todas las opciones expertas.
4. **Revisar y apilar — implementado, pendiente de evidencia científica real.** El backend devuelve resultados por grupo, masters/componentes float32, receta común y métricas de cobertura, rechazo y ruido. FWHM/fotometría finales se validarán con los datasets del usuario.

## Hallazgos corregidos

- Título y etapas recortados por una cabecera demasiado estrecha.
- Tipografía de preflight inferior a un tamaño cómodo de lectura.
- Cadenas técnicas largas sin `overflow-wrap` ni agrupación semántica.
- Clasificación incorrecta de `SV220` como Ha puro.
- La variante `SV220 SII OIII` quedaba eclipsada por una cabecera FITS genérica `FILTER=SV220`.
- Varios filtros se trataban como error y obligaban a lanzar apilados manuales independientes.
- Ausencia de extracción lineal de Ha/SII/OIII y de un resultado de sesión común.
- Ausencia de resumen de calidad posterior al apilado.

## Implementación resultante

- Perfiles canónicos `HA_OIII` y `SII_OIII`, con prioridad a la variante específica del nombre cuando la cabecera FITS sólo contiene `SV220`.
- `prepare_deepsky_session` prepara y valida todas las integraciones antes de reservar recursos.
- `run_deepsky_session` ejecuta cada perfil por separado, exporta masters y mapas diagnósticos float32, extrae Ha/SII y OIII, conserva las dos fuentes OIII sin combinarlas sin registro y escribe `session_recipe.json` con cada receta de integración completa.
- Control de extracción OIII G/B y supresión opcional de contaminación, conservando negativos y headroom float32.
- Calidad por master: cobertura uniforme, porcentaje de rechazo, ruido de fondo robusto, calificación y recomendaciones.
- Los componentes resultantes rellenan automáticamente la combinación SHO/HOO cuando la geometría científica está disponible.

## Evidencia

- `01-inspection-current.jpg`: referencia proporcionada por el usuario.
- `03-multiband-inspection-1280x720.png`: inspección corregida a 1280×720.
- `04-multiband-inspection-1000x700.png`: inspección corregida a 1000×700.
- `05-multiband-recipe-1000x700.png`: personalización multibanda.
- `06-before-after-comparison.jpg`: comparación directa de referencia y resultado.

## Pendiente para aceptación científica

- Ejecutar la sesión con los FITS reales y confirmar el mapeo espectral del sensor/filtro/cámara.
- Medir separación de bandas y contaminación cruzada con estrellas y nebulosidad reales.
- Registrar y ponderar las dos fuentes OIII antes de producir un OIII combinado.
- Comparar masters lineales, rechazo, FWHM, fotometría y tiempos contra PixInsight/DSS/Siril con la misma geometría y receta.
