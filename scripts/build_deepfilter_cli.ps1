# Builds libDF deep-filter CLI inside vendor/deepfilternet_rust (isolated from Aqua workspace).
$ErrorActionPreference = "Stop"
$VendorRoot = Join-Path (Split-Path -Parent $PSScriptRoot) "vendor/deepfilternet_rust"
Push-Location $VendorRoot
try {
    Write-Host "Building deep-filter (tract) in vendor submodule..."
    cargo build --manifest-path libDF/Cargo.toml --release --features "tract,bin,wav-utils,transforms" --bin deep-filter
    Write-Host "Binary: $VendorRoot/target/release/deep-filter.exe"
} finally {
    Pop-Location
}
