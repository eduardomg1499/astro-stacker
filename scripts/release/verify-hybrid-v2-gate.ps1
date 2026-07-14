#requires -Version 5.1
[CmdletBinding()]
param(
    [string]$EvidenceDir = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RootDir = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\.."))
Set-Location $RootDir

if ([string]::IsNullOrWhiteSpace($EvidenceDir)) {
    $EvidenceDir = Join-Path $RootDir "src-tauri\target\hybrid-v2-verification"
}
New-Item -ItemType Directory -Path $EvidenceDir -Force | Out-Null

function Invoke-GateCommand {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][string]$LogName
    )

    Write-Host ""
    Write-Host ("== " + $Name + " ==") -ForegroundColor Cyan
    $logPath = Join-Path $EvidenceDir $LogName
    & $FilePath @Arguments 2>&1 | Tee-Object -FilePath $logPath
    if ($LASTEXITCODE -ne 0) {
        throw "$Name fallo con codigo $LASTEXITCODE. Log: $logPath"
    }
}

Write-Host "== Hybrid v2 release gate ==" -ForegroundColor Cyan
Write-Host "Evidence: $EvidenceDir"

Invoke-GateCommand `
    -Name "Frontend and Rust checks" `
    -FilePath "npm" `
    -Arguments @("run", "check") `
    -LogName "frontend-rust-check.log"

Invoke-GateCommand `
    -Name "CPU and synthetic suite" `
    -FilePath "cargo" `
    -Arguments @("test", "--manifest-path", "src-tauri/Cargo.toml", "--no-fail-fast") `
    -LogName "cpu-synthetic-tests.log"

Invoke-GateCommand `
    -Name "Physical GPU parity suite" `
    -FilePath "cargo" `
    -Arguments @(
        "test", "--manifest-path", "src-tauri/Cargo.toml", "--no-fail-fast",
        "--", "--ignored", "--nocapture"
    ) `
    -LogName "gpu-physical-tests.log"

$commit = (& git rev-parse HEAD 2>$null | Select-Object -First 1)
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace([string]$commit)) {
    $commit = "unknown"
}
$dirty = @(& git status --porcelain 2>$null).Count -gt 0
$env:HYBRID_GATE_COMMIT = ([string]$commit).Trim()
$env:HYBRID_GATE_OS = "Windows"
$env:HYBRID_GATE_ARCH = $env:PROCESSOR_ARCHITECTURE
$env:HYBRID_GATE_DIRTY = if ($dirty) { "true" } else { "false" }
& node "scripts\release\verify-hybrid-v2-evidence.mjs" $EvidenceDir
if ($LASTEXITCODE -ne 0) {
    throw "La evidencia Hybrid v2 no demuestra que las pruebas GPU fisicas se ejecutaron."
}

Write-Host ""
Write-Host "Hybrid v2 release gate: PASS" -ForegroundColor Green
