# === Descarga ffmpeg.exe y ffprobe.exe (Windows x64) a src-tauri\bin ===
# Estos binarios estan ignorados por Git, asi que no llegan al clonar el repo.
# Ejecutalo UNA VEZ por maquina antes de compilar con build_installer.ps1.
#
#   powershell -ExecutionPolicy Bypass -File .\scripts\fetch-ffmpeg-windows.ps1
#
# Fuente: builds estaticos oficiales de BtbN (GitHub).

#requires -version 5
$ErrorActionPreference = "Stop"

# Raiz del proyecto (este script vive en scripts\)
$root   = Split-Path -Parent $PSScriptRoot
$binDir = Join-Path $root "src-tauri\bin"
New-Item -ItemType Directory -Force -Path $binDir | Out-Null

$ffmpeg  = Join-Path $binDir "ffmpeg.exe"
$ffprobe = Join-Path $binDir "ffprobe.exe"

if ((Test-Path $ffmpeg) -and ((Get-Item $ffmpeg).Length -gt 1MB) -and `
    (Test-Path $ffprobe) -and ((Get-Item $ffprobe).Length -gt 1MB)) {
    Write-Host "Ya existen ffmpeg.exe y ffprobe.exe en $binDir. Nada que hacer." -ForegroundColor Green
    exit 0
}

$zipUrl = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip"
$tmpZip = Join-Path $env:TEMP "zas-ffmpeg-win64.zip"
$tmpDir = Join-Path $env:TEMP "zas-ffmpeg-win64"

Write-Host "Descargando FFmpeg (Windows x64) desde BtbN..." -ForegroundColor Cyan
# La barra de progreso de Invoke-WebRequest hace la descarga ~50x mas lenta en
# PowerShell 5.1 (parece colgada). La suprimimos y preferimos curl.exe (incluido
# en Win10/11): rapido, sigue redirecciones y muestra progreso real.
$ProgressPreference = 'SilentlyContinue'
$curlExe = Get-Command curl.exe -ErrorAction SilentlyContinue
if ($curlExe) {
    & curl.exe -L --fail --retry 3 -o $tmpZip $zipUrl
    if ($LASTEXITCODE -ne 0) { throw "Fallo la descarga con curl (codigo $LASTEXITCODE)." }
} else {
    Invoke-WebRequest -Uri $zipUrl -OutFile $tmpZip -UseBasicParsing
}
if ((-not (Test-Path $tmpZip)) -or ((Get-Item $tmpZip).Length -lt 1MB)) {
    throw "El zip descargado esta vacio o incompleto."
}

Write-Host "Extrayendo..." -ForegroundColor Cyan
if (Test-Path $tmpDir) { Remove-Item $tmpDir -Recurse -Force }
Expand-Archive -Path $tmpZip -DestinationPath $tmpDir -Force

$srcFfmpeg  = Get-ChildItem -Path $tmpDir -Recurse -Filter "ffmpeg.exe"  | Select-Object -First 1
$srcFfprobe = Get-ChildItem -Path $tmpDir -Recurse -Filter "ffprobe.exe" | Select-Object -First 1
if ((-not $srcFfmpeg) -or (-not $srcFfprobe)) {
    throw "No se encontraron ffmpeg.exe/ffprobe.exe dentro del zip descargado."
}

Copy-Item $srcFfmpeg.FullName  $ffmpeg  -Force
Copy-Item $srcFfprobe.FullName $ffprobe -Force

Remove-Item $tmpZip -Force -ErrorAction SilentlyContinue
Remove-Item $tmpDir -Recurse -Force -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "Listo. Binarios colocados en $binDir :" -ForegroundColor Green
Get-ChildItem $ffmpeg, $ffprobe | Select-Object Name, @{N="MB";E={[math]::Round($_.Length/1MB,1)}}
Write-Host "Ahora puedes compilar:  powershell -ExecutionPolicy Bypass -File .\build_installer.ps1" -ForegroundColor Cyan
