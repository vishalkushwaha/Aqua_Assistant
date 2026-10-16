# Downloads DeepFilterNet3 ONNX from the upstream DeepFilterNet models/ directory
# (used by vendor/deepfilternet_rust libDF tract). Extracts into data/models/deepfilternet/.

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "_verified_download.ps1")
# An installed copy keeps its data in %LOCALAPPDATA%\Aqua: run with AQUA_WORKSPACE_ROOT set to that folder.
$Root = if ($env:AQUA_WORKSPACE_ROOT) { $env:AQUA_WORKSPACE_ROOT } else { Split-Path -Parent $PSScriptRoot }
$DfDir = Join-Path $Root "data/models/deepfilternet"
$Tar = Join-Path $DfDir "DeepFilterNet3_onnx.tar.gz"
$OutDir = Join-Path $DfDir "DeepFilterNet3_onnx"
$Url = "https://github.com/Rikorose/DeepFilterNet/raw/main/models/DeepFilterNet3_onnx.tar.gz"

New-Item -ItemType Directory -Force -Path $DfDir | Out-Null

# Pinned SHA-256 of the archive this install was tested with (upstream publishes none to compare to).
Get-VerifiedFile -Url $Url -OutFile $Tar -Sha256 "c94d91f70911001c946e0fabb4aa9adc37045f45a03b56008cb0c8244cb63616"

if (-not (Test-Path (Join-Path $OutDir "enc.onnx"))) {
    Write-Host "Extracting to $OutDir ..."
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    tar -xzf $Tar -C $OutDir
    $Nested = Join-Path $OutDir "tmp/export"
    if (Test-Path (Join-Path $Nested "enc.onnx")) {
        Write-Host "Flattening tmp/export ONNX files into $OutDir ..."
        Copy-Item -Force (Join-Path $Nested "*") $OutDir
    }
} else {
    Write-Host "Extracted model already present under $OutDir"
}

Write-Host "Done. Verify: enc.onnx, erb_dec.onnx, df_dec.onnx, config.ini"
