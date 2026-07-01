#requires -Version 5.1
[CmdletBinding()]
param([string]$DefaultProjectPath = "C:\Users\picis\Desktop\Proyecto-Zenith-Astro-Stacker")

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$CodeRepoUrl = "https://github.com/eduardomg1499/Proyecto-Zenith-Astro-Stacker.git"
$ReleaseRepo = "eduardomg1499/astro-stacker"
$ExpectedBranch = "main"

function Write-Title {
    param([string]$Text)
    Write-Host ""
    Write-Host ("=" * 68) -ForegroundColor DarkCyan
    Write-Host ("  " + $Text) -ForegroundColor Cyan
    Write-Host ("=" * 68) -ForegroundColor DarkCyan
}
function Write-Step {
    param([string]$Text)
    Write-Host ""
    Write-Host ("-> " + $Text) -ForegroundColor Cyan
}
function Stop-Assistant {
    param([string]$Message)
    Write-Host ""
    Write-Host ("ERROR: " + $Message) -ForegroundColor Red
    Write-Host "No se ha subido ningun asset de Release." -ForegroundColor Yellow
    throw $Message
}
function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$Arguments = @(),
        [string]$ErrorMessage = "El comando externo fallo."
    )
    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        Stop-Assistant "$ErrorMessage Codigo de salida: $LASTEXITCODE"
    }
}
function Invoke-NativeCapture {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$Arguments = @(),
        [string]$ErrorMessage = "El comando externo fallo."
    )
    $output = & $FilePath @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        $detail = ($output | Out-String).Trim()
        Stop-Assistant "$ErrorMessage $detail"
    }
    return (($output | ForEach-Object { "$_" }) -join [Environment]::NewLine).Trim()
}
function Require-Command {
    param([string]$Name, [string]$InstallHint)
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        Stop-Assistant "Falta '$Name'. $InstallHint"
    }
}
function Read-MenuChoice {
    param([string]$Prompt, [string[]]$ValidChoices, [string]$DefaultChoice = "")
    while ($true) {
        $answer = (Read-Host $Prompt).Trim()
        if ([string]::IsNullOrWhiteSpace($answer) -and $DefaultChoice) {
            return $DefaultChoice
        }
        if ($ValidChoices -contains $answer) {
            return $answer
        }
        Write-Host "Opcion no valida. Usa: $($ValidChoices -join ', ')" -ForegroundColor Yellow
    }
}
function Ensure-GhAuthentication {
    & gh auth status *> $null
    if ($LASTEXITCODE -ne 0) {
        Write-Host "GitHub CLI no esta autenticado. Se abrira gh auth login." -ForegroundColor Yellow
        Invoke-Native "gh" @("auth", "login") "No fue posible autenticar GitHub CLI."
    }
    & gh auth status *> $null
    if ($LASTEXITCODE -ne 0) {
        Stop-Assistant "GitHub CLI sigue sin autenticar."
    }
}
function Get-ProjectVersion {
    param([string]$Root)
    $configPath = Join-Path $Root "src-tauri\tauri.conf.json"
    if (-not (Test-Path $configPath -PathType Leaf)) {
        Stop-Assistant "No existe $configPath"
    }
    $config = Get-Content $configPath -Raw | ConvertFrom-Json
    if ([string]::IsNullOrWhiteSpace([string]$config.version)) {
        Stop-Assistant "tauri.conf.json no contiene una version valida."
    }
    return [string]$config.version
}
function Test-ReleaseExists {
    param([string]$Tag)
    & gh release view $Tag --repo $ReleaseRepo *> $null
    return ($LASTEXITCODE -eq 0)
}
function Download-LatestJson {
    param([string]$Tag, [string]$Destination)
    $latestPath = Join-Path $Destination "latest.json"
    if (Test-Path $latestPath) {
        Remove-Item $latestPath -Force
    }
    Invoke-Native "gh" @(
        "release", "download", $Tag,
        "--repo", $ReleaseRepo,
        "--pattern", "latest.json",
        "--clobber",
        "--dir", $Destination
    ) "No fue posible descargar latest.json del Release $Tag."
    if (-not (Test-Path $latestPath -PathType Leaf)) {
        Stop-Assistant "GitHub no entrego latest.json para $Tag."
    }
    return $latestPath
}
function Assert-LatestJson {
    param(
        [string]$Path,
        [string]$Version,
        [string]$Tag,
        [bool]$RequireWindows
    )
    if (-not (Test-Path $Path -PathType Leaf)) {
        Stop-Assistant "No existe latest.json en $Path"
    }
    try {
        $manifest = Get-Content $Path -Raw | ConvertFrom-Json
    } catch {
        Stop-Assistant "latest.json no es JSON valido: $($_.Exception.Message)"
    }
    if ([string]$manifest.version -ne $Version) {
        Stop-Assistant "latest.json declara version '$($manifest.version)' y se esperaba '$Version'."
    }
    $required = @("darwin-aarch64", "darwin-x86_64")
    if ($RequireWindows) {
        $required += "windows-x86_64"
    }
    foreach ($platformName in $required) {
        $property = $manifest.platforms.PSObject.Properties[$platformName]
        if ($null -eq $property) {
            Stop-Assistant "latest.json no contiene la plataforma $platformName."
        }
        $platform = $property.Value
        if ([string]::IsNullOrWhiteSpace([string]$platform.signature)) {
            Stop-Assistant "latest.json no contiene firma para $platformName."
        }
        $expectedFragment = "/releases/download/$Tag/"
        if (-not ([string]$platform.url).Contains($expectedFragment)) {
            Stop-Assistant "La URL de $platformName no apunta al tag $Tag."
        }
    }
    return $manifest
}
function Get-ReleaseInfo {
    param([string]$Tag)
    $json = Invoke-NativeCapture "gh" @(
        "release", "view", $Tag,
        "--repo", $ReleaseRepo,
        "--json", "url,tagName,assets"
    ) "No fue posible consultar el Release $Tag."
    try {
        return ($json | ConvertFrom-Json)
    } catch {
        Stop-Assistant "La respuesta de GitHub para el Release no es JSON valido."
    }
}
function Assert-MacAssetsRemain {
    param([object]$ReleaseInfo, [string]$Version)
    $names = @($ReleaseInfo.assets | ForEach-Object { [string]$_.name })
    $requiredMacAssets = @(
        ("Zenith.Astro.Stacker_" + $Version + "_aarch64.app.tar.gz"),
        ("Zenith.Astro.Stacker_" + $Version + "_aarch64.app.tar.gz.sig"),
        ("Zenith.Astro.Stacker_" + $Version + "_aarch64_styled.dmg"),
        ("Zenith.Astro.Stacker_" + $Version + "_x86_64.app.tar.gz"),
        ("Zenith.Astro.Stacker_" + $Version + "_x86_64.app.tar.gz.sig"),
        ("Zenith.Astro.Stacker_" + $Version + "_x86_64_styled.dmg")
    )
    foreach ($asset in $requiredMacAssets) {
        if ($names -notcontains $asset) {
            Stop-Assistant "El Release objetivo no contiene el asset macOS requerido: $asset"
        }
    }
}
function Assert-UploadedDigest {
    param([object]$ReleaseInfo, [string]$AssetName, [string]$LocalPath)
    $asset = @($ReleaseInfo.assets | Where-Object { [string]$_.name -eq $AssetName }) |
        Select-Object -First 1
    if ($null -eq $asset) {
        Stop-Assistant "GitHub no muestra el asset subido: $AssetName"
    }
    $localDigest = "sha256:" + (Get-FileHash $LocalPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $digestProperty = $asset.PSObject.Properties["digest"]
    if ($null -ne $digestProperty -and -not [string]::IsNullOrWhiteSpace([string]$digestProperty.Value)) {
        if ([string]$digestProperty.Value -ne $localDigest) {
            Stop-Assistant "El SHA256 remoto de $AssetName no coincide con el archivo local."
        }
    }
}

Write-Title "ZENITH ASTRO STACKER - ASISTENTE WINDOWS"

Require-Command "git" "Instala Git for Windows."
Require-Command "gh" "Instala GitHub CLI y vuelve a ejecutar este asistente."
Ensure-GhAuthentication

Write-Host ""
Write-Host "Selecciona el tipo de configuracion:" -ForegroundColor White
Write-Host "  1) Configuracion desde el inicio (clonar repositorio)" -ForegroundColor White
Write-Host "  2) Repositorio ya descargado (actualizar)" -ForegroundColor White
$mode = Read-MenuChoice "Opcion [1/2]" @("1", "2")

$pathAnswer = (Read-Host "Ruta del proyecto [$DefaultProjectPath]").Trim()
$ProjectRoot = if ($pathAnswer) { $pathAnswer } else { $DefaultProjectPath }
$ProjectRoot = [System.IO.Path]::GetFullPath($ProjectRoot)

if ($mode -eq "1") {
    Write-Step "Configuracion inicial en $ProjectRoot"
    if (Test-Path (Join-Path $ProjectRoot ".git")) {
        Write-Host "La ruta ya es un repositorio Git. Se utilizara el flujo de actualizacion." -ForegroundColor Yellow
    } else {
        if (Test-Path $ProjectRoot) {
            $items = @(Get-ChildItem $ProjectRoot -Force -ErrorAction SilentlyContinue)
            if ($items.Count -gt 0) {
                Stop-Assistant "La ruta existe y no esta vacia. Ejecuta el script desde fuera del proyecto y elige una carpeta vacia o inexistente."
            }
        } else {
            $parent = Split-Path $ProjectRoot -Parent
            if (-not (Test-Path $parent)) {
                New-Item -ItemType Directory -Path $parent -Force | Out-Null
            }
        }
        Invoke-Native "git" @("clone", $CodeRepoUrl, $ProjectRoot) "No fue posible clonar el repositorio privado."
    }
} else {
    Write-Step "Actualizacion del repositorio existente en $ProjectRoot"
    if (-not (Test-Path (Join-Path $ProjectRoot ".git"))) {
        Stop-Assistant "La ruta elegida no contiene un repositorio Git. Usa la opcion 1."
    }
}

Set-Location $ProjectRoot
$originUrl = ""
& git remote get-url origin *> $null
if ($LASTEXITCODE -eq 0) {
    $originUrl = (Invoke-NativeCapture "git" @("remote", "get-url", "origin")).Trim()
}
if (-not $originUrl) {
    Invoke-Native "git" @("remote", "add", "origin", $CodeRepoUrl) "No fue posible crear origin."
} elseif (-not $originUrl.Contains("eduardomg1499/Proyecto-Zenith-Astro-Stacker")) {
    Write-Host "origin apuntaba a otro repositorio; se corregira al repositorio privado." -ForegroundColor Yellow
    Invoke-Native "git" @("remote", "set-url", "origin", $CodeRepoUrl) "No fue posible corregir origin."
}

$dirtyLines = @(& git status --porcelain)
if ($LASTEXITCODE -ne 0) {
    Stop-Assistant "No fue posible revisar git status."
}
# Permite descargar manualmente este mismo asistente dentro de scripts\ antes
# de actualizar un checkout antiguo. Se mueve temporalmente fuera del repo;
# git pull recuperara la copia oficial y versionada del mismo archivo.
$assistantRelative = "scripts/windows-release-assistant.ps1"
$assistantExpectedPath = [System.IO.Path]::GetFullPath((Join-Path $ProjectRoot $assistantRelative))
$runningScriptPath = [System.IO.Path]::GetFullPath($PSCommandPath)
$onlyManualAssistant = (
    $dirtyLines.Count -eq 1 -and
    $dirtyLines[0] -match '^\?\? scripts[/\\]windows-release-assistant\.ps1$' -and
    $runningScriptPath -eq $assistantExpectedPath
)
if ($onlyManualAssistant) {
    $bootstrapCopy = Join-Path $env:TEMP "zas-windows-release-assistant-bootstrap.ps1"
    Move-Item $assistantExpectedPath $bootstrapCopy -Force
    $dirtyLines = @()
    Write-Host "El asistente manual se movio temporalmente; git pull instalara la copia oficial." -ForegroundColor Yellow
}
if ($dirtyLines.Count -gt 0) {
    Stop-Assistant "Hay cambios locales sin guardar. Haz commit o git stash antes de continuar. Estado: $($dirtyLines -join '; ')"
}

Write-Step "Descargando el codigo mas reciente"
Invoke-Native "git" @("fetch", "origin") "git fetch fallo."
Invoke-Native "git" @("checkout", $ExpectedBranch) "No fue posible cambiar a main."
Invoke-Native "git" @("pull", "--ff-only", "origin", $ExpectedBranch) "git pull --ff-only fallo."
$head = (Invoke-NativeCapture "git" @("rev-parse", "HEAD")).Trim()
Write-Host "Commit descargado: $head" -ForegroundColor Green

Require-Command "node" "Instala Node.js 20 o superior."
Require-Command "npm" "Instala Node.js 20 o superior."
Require-Command "rustup" "Instala Rust mediante rustup."
Require-Command "cargo" "Instala Rust con toolchain MSVC."
Require-Command "powershell.exe" "Windows PowerShell es necesario."

$nodeText = (Invoke-NativeCapture "node" @("--version")).Trim().TrimStart("v")
$nodeMajor = 0
if (-not [int]::TryParse(($nodeText.Split(".")[0]), [ref]$nodeMajor) -or $nodeMajor -lt 20) {
    Stop-Assistant "Node.js 20 o superior es obligatorio. Detectado: $nodeText"
}

Write-Step "Preparando Rust, npm y FFmpeg"
Invoke-Native "rustup" @("default", "stable-x86_64-pc-windows-msvc") "No fue posible seleccionar Rust MSVC."
Invoke-Native "rustup" @("target", "add", "x86_64-pc-windows-msvc") "No fue posible instalar el target Rust."
Invoke-Native "npm" @("ci") "npm ci fallo."
$ffmpegScript = Join-Path $ProjectRoot "scripts\fetch-ffmpeg-windows.ps1"
Invoke-Native "powershell.exe" @(
    "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $ffmpegScript
) "No fue posible preparar FFmpeg."

$Version = Get-ProjectVersion $ProjectRoot
$DefaultTag = "v$Version-Beta"
$LatestPath = Join-Path $ProjectRoot "latest.json"

Write-Title "PREPARACION DEL BUILD WINDOWS $Version"
if (-not (Test-ReleaseExists $DefaultTag)) {
    Stop-Assistant "El Release macOS $DefaultTag aun no existe. Publica primero macOS."
}
$defaultRelease = Get-ReleaseInfo $DefaultTag
Assert-MacAssetsRemain $defaultRelease $Version
Download-LatestJson $DefaultTag $ProjectRoot | Out-Null
Assert-LatestJson $LatestPath $Version $DefaultTag $false | Out-Null

$env:GITHUB_RELEASE_TAG = $DefaultTag
Write-Step "Ejecutando build_installer.ps1"
$buildScript = Join-Path $ProjectRoot "build_installer.ps1"
Invoke-Native "powershell.exe" @(
    "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $buildScript
) "build_installer.ps1 fallo."
Assert-LatestJson $LatestPath $Version $DefaultTag $true | Out-Null

$nsisDir = Join-Path $ProjectRoot "src-tauri\target\release\bundle\nsis"
$exe = Get-ChildItem $nsisDir -Filter "*$Version*-setup.exe" |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
if ($null -eq $exe) {
    Stop-Assistant "No se encontro el instalador NSIS $Version."
}
$sigPath = $exe.FullName + ".sig"
if (-not (Test-Path $sigPath -PathType Leaf)) {
    Stop-Assistant "No se encontro la firma: $sigPath"
}

Write-Host ""
Write-Host "Build Windows listo:" -ForegroundColor Green
Write-Host "  $($exe.FullName)"
Write-Host "  $sigPath"
Write-Host "  $LatestPath"

$uploadChoice = Read-MenuChoice "Deseas subir ahora los assets? [S/n]" @("S", "s", "N", "n") "S"
if ($uploadChoice -match "^[Nn]$") {
    Write-Host "No se subieron assets; quedaron listos localmente." -ForegroundColor Yellow
    exit 0
}

Write-Host ""
Write-Host "Tag de destino:" -ForegroundColor White
Write-Host "  1) Predefinido: $DefaultTag" -ForegroundColor White
Write-Host "  2) Tag especifico ya existente" -ForegroundColor White
$tagMode = Read-MenuChoice "Opcion [1/2, Enter=1]" @("1", "2") "1"
$TargetTag = $DefaultTag
if ($tagMode -eq "2") {
    $TargetTag = (Read-Host "Tag exacto del Release").Trim()
    if (-not $TargetTag) {
        Stop-Assistant "El tag personalizado esta vacio."
    }
}
if (-not (Test-ReleaseExists $TargetTag)) {
    Stop-Assistant "El Release $TargetTag no existe. No se crea uno sin macOS."
}
$targetRelease = Get-ReleaseInfo $TargetTag
Assert-MacAssetsRemain $targetRelease $Version

if ($TargetTag -ne $DefaultTag) {
    Download-LatestJson $TargetTag $ProjectRoot | Out-Null
    Assert-LatestJson $LatestPath $Version $TargetTag $false | Out-Null
    $env:GITHUB_RELEASE_TAG = $TargetTag
    Invoke-Native "npm" @("run", "release:latest-json") "No se pudo regenerar latest.json."
}
Assert-LatestJson $LatestPath $Version $TargetTag $true | Out-Null

$uploadDir = Join-Path $env:TEMP "zas-$Version-windows-release"
if (Test-Path $uploadDir) {
    Remove-Item $uploadDir -Recurse -Force
}
New-Item -ItemType Directory -Path $uploadDir | Out-Null
$assetExeName = $exe.Name -replace " ", "."
$assetSigName = $assetExeName + ".sig"
$uploadExe = Join-Path $uploadDir $assetExeName
$uploadSig = Join-Path $uploadDir $assetSigName
$uploadLatest = Join-Path $uploadDir "latest.json"
Copy-Item $exe.FullName $uploadExe -Force
Copy-Item $sigPath $uploadSig -Force
Copy-Item $LatestPath $uploadLatest -Force

Write-Title "SUBIDA A GITHUB RELEASE $TargetTag"
Write-Host "Solo se subiran estos tres archivos; macOS no se toca:" -ForegroundColor Yellow
Write-Host "  $assetExeName"
Write-Host "  $assetSigName"
Write-Host "  latest.json"
$confirm = Read-MenuChoice "Confirmar subida? [S/n]" @("S", "s", "N", "n") "S"
if ($confirm -match "^[Nn]$") {
    Write-Host "Subida cancelada." -ForegroundColor Yellow
    exit 0
}

Invoke-Native "gh" @(
    "release", "upload", $TargetTag,
    $uploadExe, $uploadSig, $uploadLatest,
    "--repo", $ReleaseRepo, "--clobber"
) "La subida al Release fallo."

Write-Step "Validando GitHub"
$remoteRelease = Get-ReleaseInfo $TargetTag
Assert-MacAssetsRemain $remoteRelease $Version
Assert-UploadedDigest $remoteRelease $assetExeName $uploadExe
Assert-UploadedDigest $remoteRelease $assetSigName $uploadSig
Assert-UploadedDigest $remoteRelease "latest.json" $uploadLatest

$verifyDir = Join-Path $env:TEMP "zas-$Version-release-verify"
if (Test-Path $verifyDir) {
    Remove-Item $verifyDir -Recurse -Force
}
New-Item -ItemType Directory -Path $verifyDir | Out-Null
$remoteLatestPath = Download-LatestJson $TargetTag $verifyDir
Assert-LatestJson $remoteLatestPath $Version $TargetTag $true | Out-Null

Write-Title "PROCESO COMPLETADO Y VALIDADO"
Write-Host "Version: $Version" -ForegroundColor Green
Write-Host "Tag: $TargetTag" -ForegroundColor Green
Write-Host "Commit: $head" -ForegroundColor Green
Write-Host "Release: $($remoteRelease.url)" -ForegroundColor Green
Write-Host "macOS aarch64/x86_64: conservados" -ForegroundColor Green
Write-Host "Windows x86_64: subido y SHA256 validado" -ForegroundColor Green
Write-Host "latest.json: tres plataformas y URLs correctas" -ForegroundColor Green
Write-Host ""
Write-Host "Todo esta correcto." -ForegroundColor Cyan
