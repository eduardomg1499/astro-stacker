# Protocolo A/B científico de cielo profundo

El arnés falla cerrado. Un JSON con esquema, ID o enumeración desconocidos;
una métrica obligatoria ausente o no finita; una evidencia ausente o cuyo
SHA-256 no coincida; una identidad mezclada; o una corrida incompleta invalida
el corpus. Un archivo JSON ajeno dentro del directorio tampoco se ignora.

Este arnés no autoriza por sí solo una afirmación contra WBPP, DSS o APP. Para
ello se necesita el corpus real congelado, la ejecución reproducible de cada
producto y el gate independiente `superiorityEligible`.

## Cobertura y unidad de ejecución

El reporte completo puede contener varios datasets. Cada `datasetId` declara
en `scenarioIds` qué escenarios `deep_sky` satisface de
[`dataset-matrix.json`](./dataset-matrix.json). Para ser elegible para release,
la unión debe cubrir **todos** los escenarios deep-sky de la matriz vigente y
las cuatro clases `broadbandOsc`, `broadbandMono`, `dualBandOsc` y
`monoNarrowband`. IDs desconocidos o escenarios de otro dominio se rechazan.

Para cada dataset y clase se exige:

- los mismos raws y `rawSetSha256` en todas las herramientas y estados;
- el mismo crop, escala lineal y hardware en todas las herramientas y estados;
- una única versión y un único `parametersSha256` por herramienta, sin cambios
  entre caché fría y caliente;
- exactamente cinco corridas `cold` y cinco `warm`, con `runIndex` 1–5, para
  Zenith, WBPP, DSS y APP;
- `runId` globalmente único y `completed=true`.

Las versiones y los parámetros naturalmente pueden ser diferentes entre
herramientas y datasets; lo que no puede cambiar es la configuración de una
misma herramienta al comparar sus cinco repeticiones frías y calientes.

Cada JSON debe cumplir
[`deepsky-ab-run.schema.json`](./deepsky-ab-run.schema.json), actualmente
`zenith-deepsky-ab-run-v2`. Todas las rutas de `outputs` y `evidence` son
relativas al JSON, no pueden escapar de ese directorio y sus hashes se
recalculan. Cada corrida conserva como mínimo:

- uno o más masters, con al menos uno vinculado también desde `outputs`;
- VAR y DQ;
- receta/configuración efectiva de la herramienta;
- uno o más logs.

Si una herramienta no produce VAR, DQ o receta nativos, el procedimiento de
medición debe guardar el producto evaluado o un manifiesto explícito y
content-addressed; omitir el rol no es una evidencia válida. No se acepta una
ruta meramente declarada.

Para Zenith, `decisionSha256`, `cfaSha256` y
`scientificMetricsSha256` deben ser idénticos entre las diez corridas
cold/warm de cada dataset. Los tiempos están fuera de esos hashes.

## Ejecución

```sh
npm run benchmark:deepsky-ab -- ./ruta/runs \
  --matrix benchmarks/dataset-matrix.json \
  --out ./ruta/deepsky-ab-report.json
```

La ruta y opciones existentes de la CLI se conservan. El proceso devuelve 0
cuando `releaseEligible=true`; devuelve 1 ante cualquier incumplimiento de
validación, cobertura, límite absoluto o no-regresión. Una matriz corrupta o
una invocación inválida devuelve 2.

## Gates y semántica del reporte

`releaseEligible` y `superiorityEligible` son decisiones diferentes:

- `releaseEligible` exige contrato/evidencias válidos, cobertura completa de
  la matriz, todos los límites científicos absolutos y ninguna regresión mayor
  de 3% frente a WBPP, DSS o APP en las métricas primarias.
- `superiorityEligible` exige además que, en cada clase de captura, al menos una
  métrica primaria mejore 5% o más con el límite inferior del bootstrap 95%
  por encima de cero **contra todos** los competidores y datasets de esa clase.

Por compatibilidad, `passed` es alias de `releaseEligible`; nunca debe
interpretarse como una afirmación de superioridad. El reporte separa
`validationFailures`, `releaseFailures` y `superiorityFailures`, conserva la
huella SHA-256 de la matriz y de todos los JSON de entrada, y publica el detalle
por dataset/competidor/métrica.

Las métricas absolutas incluyen calibración de flat/dark, fotometría, registro,
geometría EIDR, cobertura Monte Carlo de VAR, seams, amplificación de lectura,
incluido el límite duro total, RSS y readback GPU. Además deben ser exactamente cero las violaciones de
`coverage=0`, frecuencias artificiales, Jacobianos negativos y fallos de
asignación. GPU sólo puede declararse ofrecida cuando alcanza el speedup mínimo
de la política de la matriz.

## Reproducibilidad mínima

Antes de ejecutar, congelar y registrar versiones, parámetros efectivos,
raws, metadata, hashes, crop, escala lineal, hardware, SO y preparación de
caché. Se ejecutan cinco corridas frías y cinco calientes sin modificar ese
contrato. Se conservan masters, VAR, DQ, recetas, logs y salidas de medición.

El A/B utiliza imágenes lineales y exactamente la misma geometría. ABE, SCNR,
stretch, sharpening y proxies espectrales no cuantitativos quedan fuera de las
métricas primarias. Sin raws reales completos y trazabilidad de WBPP/DSS/APP,
un resultado sintético puede validar el arnés, pero no sustentar una publicación
de superioridad.
