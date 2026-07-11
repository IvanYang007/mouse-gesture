# Build script for mouse-gesture daemon
param(
    [switch]$Release,
    [switch]$Check,
    [switch]$Bench,
    [switch]$Clean
)

$ErrorActionPreference = "Stop"

if ($Clean) {
    cargo clean
    Write-Host "==> Clean complete" -ForegroundColor Green
    return
}

if ($Check) {
    Write-Host "==> Checking..." -ForegroundColor Cyan
    cargo check
    return
}

if ($Bench) {
    Write-Host "==> Building for benchmarks (opt-level=3)..." -ForegroundColor Cyan
    $env:RUSTFLAGS = "-C target-cpu=native"
    cargo build --release
} elseif ($Release) {
    Write-Host "==> Building release (opt-level=2, LTO)..." -ForegroundColor Cyan
    $env:RUSTFLAGS = ""
    cargo build --release
} else {
    Write-Host "==> Building debug..." -ForegroundColor Cyan
    cargo build
}

$exe = if ($Release -or $Bench) { ".\target\release\mouse-gesture.exe" } else { ".\target\debug\mouse-gesture.exe" }
if (Test-Path $exe) {
    $size = [math]::Round((Get-Item $exe).Length / 1KB, 1)
    $type = if ($Bench) { "bench" } elseif ($Release) { "release" } else { "debug" }
    Write-Host "==> Build complete ($type): $exe ($size KB)" -ForegroundColor Green
} else {
    Write-Host "Build failed" -ForegroundColor Red
}
