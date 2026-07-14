# Arnés A/B planetario (F0): mide un stack candidato (métricas de limbo,
# ringing, ocupación de rango) y, si se aporta una referencia con la MISMA
# geometría (p.ej. el stack de AutoStakkert!4 del mismo vídeo y encuadre),
# añade PSNR/SSIM contra ella. Escribe el informe JSON.
#
# Uso: .\run-planetary-ab.ps1 -Candidate stack.png [-Reference as4.png] [-Out informe.json]
param(
    [Parameter(Mandatory = $true)][string]$Candidate,
    [string]$Reference = "",
    [string]$Out = "planetary-ab-report.json"
)
$ErrorActionPreference = "Stop"

$env:ZAS_AB_CANDIDATE = (Resolve-Path $Candidate).Path
if ($Reference -ne "") {
    $env:ZAS_AB_REFERENCE = (Resolve-Path $Reference).Path
}
if ([System.IO.Path]::IsPathRooted($Out)) {
    $env:ZAS_AB_OUT = $Out
} else {
    $env:ZAS_AB_OUT = Join-Path (Get-Location).Path $Out
}

Push-Location (Join-Path $PSScriptRoot "..\..\src-tauri")
try {
    cargo test --release --quiet f0_ab_compare -- --ignored --nocapture
    if ($LASTEXITCODE -ne 0) { throw "cargo test devolvió $LASTEXITCODE" }
    Write-Host "Informe: $($env:ZAS_AB_OUT)"
} finally {
    Pop-Location
}
