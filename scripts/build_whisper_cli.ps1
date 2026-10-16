$ErrorActionPreference = "Stop"
$Vendor = Join-Path (Split-Path -Parent $PSScriptRoot) "vendor/whisper.cpp"
Push-Location $Vendor
try {
    Write-Host "Configuring whisper.cpp (Release)..."
    cmake -S . -B build
    Write-Host "Building whisper-cli..."
    cmake --build build --config Release --target whisper-cli
    # Put the MinGW runtime DLLs beside the program so it starts the same way from any shell.
    & (Join-Path $PSScriptRoot "bundle_llama_dlls.ps1") -Vendor whisper.cpp
    Write-Host "Expected binary: $Vendor/build/bin/Release/whisper-cli.exe"
} finally {
    Pop-Location
}
