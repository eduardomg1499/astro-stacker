# === ASISTENTE DE CONSTRUCCIÓN ZENITH ASTRO STACKER ===
# AUTOMATIZADO: Configura tus claves abajo para no tener que escribirlas siempre.

# --- CONFIGURACIÓN DE USUARIO (EDITA ESTO UNA VEZ) ---
$SavedKey = "dW50cnVzdGVkIGNvbW1lbnQ6IHJzaWduIGVuY3J5cHRlZCBzZWNyZXQga2V5ClJXUlRZMEl5aFJHSE9iWTd5SFM5aUZOYi9lamdMdEs1a3l3WUNVaG9saS9tTzFudTBnQUFBQkFBQUFBQUFBQUFBQUlBQUFBQWxrUlkzc1h3cmMyc0dqcTdxTUU1Q1h1a3VwVkJjQ1YzNmkrNkNRdXZ0RDN3TWpwdndNeXdUZUIrY0pJUGxrSUJRWTZNRDYxNVpORC91bGV1SWNTdEZ2VUxhTHdpT3AzMGE1VSs4L1lSMFNPM25pQXNXa0c2NS9ZWUhQMW9Sd3lyRlJLMlZSUWdqZ0k9Cg==" 
$SavedPassword = "1"
# -----------------------------------------------------

Write-Host ""
Write-Host '=============================================' -ForegroundColor Cyan
Write-Host '   CONSTRUCTOR DE INSTALADOR (TAURI)         ' -ForegroundColor Cyan
Write-Host '=============================================' -ForegroundColor Cyan
Write-Host ""

# 1. Obtener Clave
$cleanKey = ""
if (-not [string]::IsNullOrWhiteSpace($SavedKey)) {
    Write-Host '-> Usando clave guardada en el script...' -ForegroundColor Green
    $cleanKey = $SavedKey.Trim()
} else {
    Write-Host 'AVISO: Para automatizar esto, edita este archivo y pega tu clave en $SavedKey' -ForegroundColor Gray
    Write-Host 'Paso 1: Pega tu PRIVATE KEY aqui:' -ForegroundColor Yellow
    $rawKey = Read-Host
    $cleanKey = $rawKey.Trim()
}

if ([string]::IsNullOrWhiteSpace($cleanKey)) {
    Write-Host 'Error: No hay clave para firmar.' -ForegroundColor Red
    exit
}

# 2. Configurar Entorno
$env:TAURI_SIGNING_PRIVATE_KEY = $cleanKey
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = $SavedPassword

# --- LIMPIEZA PREVIA ---
$targetDir = "src-tauri/target/release/bundle/nsis"
if (Test-Path $targetDir) {
    Write-Host "-> Limpiando compilaciones anteriores..." -ForegroundColor DarkGray
    Remove-Item -Path "$targetDir\*.exe" -ErrorAction SilentlyContinue
    Remove-Item -Path "$targetDir\*.sig" -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host 'Paso 3: Iniciando compilacion (npm run tauri build)...' -ForegroundColor Yellow
npm run tauri build -- --bundles nsis

if ($LASTEXITCODE -ne 0) {
    Write-Host 'ERROR DURANTE LA COMPILACION.' -ForegroundColor Red
    pause
    exit $LASTEXITCODE
}

# 5. Generar Reporte
Write-Host ""
Write-Host '=============================================' -ForegroundColor Cyan
Write-Host '   PROCESO COMPLETADO EXITOSAMENTE           ' -ForegroundColor Cyan
Write-Host '=============================================' -ForegroundColor Cyan

$basePath = "src-tauri/target/release/bundle/nsis"
$tauriConfig = Get-Content "src-tauri/tauri.conf.json" | ConvertFrom-Json
$version = $tauriConfig.version
$productName = $tauriConfig.productName
$safeName = $productName -replace " ", "-" 

# Buscar archivos
$exeFile = Get-ChildItem -Path $basePath -Filter "*$productName*$version*.exe" | Select-Object -First 1
if (-not $exeFile) {
    $exeFile = Get-ChildItem -Path $basePath -Filter "*$safeName*$version*.exe" | Select-Object -First 1
}
if (-not $exeFile) {
    $exeFile = Get-ChildItem -Path $basePath -Filter "*.exe" | Sort-Object LastWriteTime -Descending | Select-Object -First 1
}

$sigFile = $null
if ($exeFile) {
    $possibleSig = "$($exeFile.FullName).sig"
    if (Test-Path $possibleSig) {
        $sigFile = Get-Item $possibleSig
    } else {
        $sigFile = Get-ChildItem -Path $basePath -Filter "*.sig" | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    }
}

if ($exeFile -and $sigFile) {
    $urlSafeName = $exeFile.Name -replace ' ', '.'
    npm run release:latest-json
    if ($LASTEXITCODE -ne 0) {
        Write-Host "AVISO: no se pudo generar latest.json automaticamente." -ForegroundColor Yellow
        Write-Host "Ejecuta despues: npm run release:latest-json" -ForegroundColor Yellow
    } else {
        Write-Host "-> ARCHIVO 'latest.json' generado en la raiz del proyecto." -ForegroundColor Green
    }
    Write-Host ""
    
    Write-Host "REFERENCIA PARA GITHUB (v$version):" -ForegroundColor Cyan
    Write-Host "---------------------------------------------" -ForegroundColor Gray
    Write-Host "1. Tag version:  v$version" -ForegroundColor White
    Write-Host "2. Release Title: Zenith Astro Stacker v$version" -ForegroundColor White
    Write-Host "3. Assets a subir:" -ForegroundColor Yellow
    Write-Host "   - $($exeFile.Name) (GitHub lo renombrara a $urlSafeName automaticamente)"
    Write-Host "   - $($sigFile.Name)"
    Write-Host "   - latest.json"
    Write-Host "---------------------------------------------" -ForegroundColor Gray
    Write-Host ""
    Write-Host "UBICACION DE LOS ARCHIVOS:" -ForegroundColor Gray
    Write-Host "$($exeFile.FullName)"
    Write-Host "$($sigFile.FullName)"
} elseif ($exeFile) {
    Write-Host ""
    Write-Host '=============================================' -ForegroundColor Yellow
    Write-Host '   AVISO: INSTALADOR ENCONTRADO (SIN FIRMA)  ' -ForegroundColor Yellow
    Write-Host "Se encontro: $($exeFile.Name)" -ForegroundColor Green
    Write-Host 'PERO no se encontro el archivo de firma (.sig).' -ForegroundColor Red
    Write-Host ""
    Write-Host "UBICACION: $($exeFile.FullName)" -ForegroundColor Yellow
} else {
    Write-Host 'No se encontro ningun archivo .exe.' -ForegroundColor Red
}

Write-Host ""
Write-Host "Presione Enter para salir..."
pause
