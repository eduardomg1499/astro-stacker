# Protocolo A/B contra AutoStakkert!4 (planetario)

Objetivo: una comparación **controlada, reproducible y honesta** de tiempo y
calidad entre Zenith y una versión FIJADA de AutoStakkert!4. AS!4 es el
comparador, **no** el ground truth.

## Condiciones fijas (mismas para ambos)

- Misma captura (idealmente SER; para MOV, documentar que AS!4 exige
  conversión previa y **contar esa conversión dentro de su tiempo total**).
- Mismo equipo, mismo SSD, sin otras cargas; anotar estado térmico.
- Mismo encuadre/crop, mismo porcentaje de selección, mismo drizzle,
  nº de APs equivalente (documentar el AP size de cada uno).
- Salida lineal en ambos (sin sharpening) para las métricas.

## Medición de tiempo

- **E2E**: desde abrir el archivo hasta el máster lineal guardado.
- 5 ejecuciones válidas por herramienta; reportar **mediana**, p95 e IC95
  (bootstrap; `scripts/benchmark/planetary-e2e-report.mjs` lo calcula para
  Zenith desde las trazas de `ZAS_PERF_TRACE_DIR`).
- Cold vs warm cache por separado (borrar `ZAS_DECODE_CACHE_DIR` para cold).
- Zenith: correr en CPU-only, GPU-only e Hybrid; reportar cada modo.

## Métricas de calidad (pre-registradas; `benchmark_quality.rs` / arnés F0)

- LSF-FWHM de limbo, ringing por no-monotonicidad, seams AP (referencia
  local), ocupación p99.99, PSNR/SSIM contra referencia independiente
  (stack supersampleado o consenso), registro residual.
- Regla: **cero pérdidas objetivas** y al menos una mejora material por
  escenario. La paridad sola no autoriza claim.

## Criterio para afirmar superioridad

- Tiempo: mediana ≥1.10–1.15× más rápido **y** límite inferior del IC95 >1.0.
- Calidad: regla de cero pérdidas anterior contra referencia independiente.
- Publicar claims acotados: por competidor+versión, escenario y plataforma.

## Procedimiento Zenith (por sesión)

```bash
# macOS
export ZAS_DECODE_CACHE_DIR="/Volumes/<SSD-con-espacio>/zas-decode-cache"
scripts/benchmark/planetary_e2e.sh run <etiqueta>     # abre la app instrumentada
# ... 5 análisis+apilados con los MISMOS ajustes ...
scripts/benchmark/planetary_e2e.sh report <etiqueta> \
  --save-baseline benchmarks/baselines/planetary-timing-$(hostname).json
```

En Windows: `scripts\benchmark\planetary_e2e.ps1` (mismos pasos) y
`planetary_parity.ps1` para la puerta de paridad/build previa.

## Registro

Guardar por corrida: versión exacta de ambas herramientas, hash del vídeo,
ajustes completos, JSON de trazas, capturas de los ajustes de AS!4, y los
másters lineales de ambos con hash SHA-256.
