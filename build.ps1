# Build script for mouse-gesture daemon
param(
    [switch]$Release,
    [switch]$Check
)

$ErrorActionPreference = "Stop"

if ($Check) {
    Write-Host "==> Checking..." -ForegroundColor Cyan
    cargo check
} elseif ($Release) {
    Write-Host "==> Building release..." -ForegroundColor Cyan
    cargo build --release
    $exe = ".\target\release\mouse-gesture.exe"
    if (Test-Path $exe) {
        $size = [math]::Round((Get-Item $exe).Length / 1KB, 1)
        Write-Host "==> Build complete: $exe ($size KB)" -ForegroundColor Green
    }
} else {
    Write-Host "==> Building debug..." -ForegroundColor Cyan
    cargo build
}

Write-Host "Done." -ForegroundColor Green
