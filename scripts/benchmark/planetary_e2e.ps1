# Benchmark E2E planetario (Windows): corre la APP REAL con perf_trace
# activado y agrega las trazas (mediana + IC95 por fase).
#
#   .\planetary_e2e.ps1 run [etiqueta]
#   .\planetary_e2e.ps1 report [etiqueta] [-Baseline <json>] [-SaveBaseline <json>]
param(
    [Parameter(Mandatory = $true, Position = 0)][ValidateSet("run", "report")] [string]$Cmd,
    [Parameter(Position = 1)][string]$Label = "session",
    [string]$Baseline,
    [string]$SaveBaseline
)
$ErrorActionPreference = "Stop"
$Root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$TraceDir = Join-Path $Root "benchmarks\telemetry\$Label"

switch ($Cmd) {
    "run" {
        New-Item -ItemType Directory -Force -Path $TraceDir | Out-Null
        Write-Host "Trazas en: $TraceDir"
        Write-Host "1) Se abrira la app con el tracing activo."
        Write-Host "2) Ejecuta Analisis + Apilado sobre tu video 5 veces (mismos ajustes)."
        Write-Host "3) Cierra la app y corre: .\planetary_e2e.ps1 report $Label"
        $env:ZAS_PERF_TRACE_DIR = $TraceDir
        Push-Location $Root
        try { npm run tauri dev } finally { Pop-Location; Remove-Item Env:ZAS_PERF_TRACE_DIR -ErrorAction SilentlyContinue }
    }
    "report" {
        $reportArgs = @((Join-Path $Root "scripts\benchmark\planetary-e2e-report.mjs"), $TraceDir, "--out", (Join-Path $TraceDir "report.json"))
        if ($Baseline) { $reportArgs += @("--baseline", $Baseline) }
        if ($SaveBaseline) { $reportArgs += @("--save-baseline", $SaveBaseline) }
        node @reportArgs
    }
}
