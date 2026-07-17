# Puerta de paridad planetaria en Windows (PR-41): build release + suites de
# paridad + (opcional) tests fisicos de GPU/FFmpeg. Ejecutar en el PC Windows
# antes de medir E2E o publicar una build.
#
#   .\planetary_parity.ps1              # build + suite CPU completa
#   .\planetary_parity.ps1 -Physical    # + tests #[ignore] fisicos (GPU/FFmpeg reales)
param(
    [switch]$Physical
)
$ErrorActionPreference = "Stop"
$Root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Push-Location (Join-Path $Root "src-tauri")
try {
    Write-Host "== 1/3 cargo check --release ==" -ForegroundColor Cyan
    cargo check --release --bin astro-stacker
    if ($LASTEXITCODE -ne 0) { throw "FALLO: cargo check --release" }

    Write-Host "== 2/3 suite CPU completa ==" -ForegroundColor Cyan
    cargo test --bin astro-stacker
    if ($LASTEXITCODE -ne 0) { throw "FALLO: suite CPU" }

    if ($Physical) {
        Write-Host "== 3/3 tests fisicos (GPU DX12/Vulkan + FFmpeg reales) ==" -ForegroundColor Cyan
        $physicalTests = @(
            "gpu_detect_reports_adapter",
            "gpu_parity_matches_cpu_reference",
            "planetary_analysis_gpu_batch_parity_physical",
            "planetary_sad_batch_gpu_parity_physical",
            "ffmpeg_analysis_green_is_bit_exact_to_rgb48_green_channel",
            "ffmpeg_cancelable_select_handles_more_than_256_exact_rgb_frames"
        )
        foreach ($t in $physicalTests) {
            Write-Host ("-- " + $t) -ForegroundColor DarkCyan
            cargo test --release --bin astro-stacker $t -- --ignored --nocapture
            if ($LASTEXITCODE -ne 0) { throw "FALLO fisico: $t" }
        }
    } else {
        Write-Host "== 3/3 omitido (usa -Physical para GPU/FFmpeg reales) ==" -ForegroundColor Yellow
    }
    Write-Host "PARIDAD: PASS" -ForegroundColor Green
}
finally { Pop-Location }
