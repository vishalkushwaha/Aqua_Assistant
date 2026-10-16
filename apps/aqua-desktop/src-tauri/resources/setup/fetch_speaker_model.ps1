# Speaker model for the voice lock: WeSpeaker ResNet34-LM, a 26.5 MB model that turns a few seconds of speech into a
# 256-number voice fingerprint (trained on VoxCeleb; the code is Apache-2.0, check the dataset terms for commercial use).
# Source: the speaker-recognition models published with sherpa-onnx.
$ErrorActionPreference = "Stop"
# An installed copy keeps its data in %LOCALAPPDATA%\Aqua: run with AQUA_WORKSPACE_ROOT set to that folder.
$Root = if ($env:AQUA_WORKSPACE_ROOT) { $env:AQUA_WORKSPACE_ROOT } else { Split-Path -Parent $PSScriptRoot }
. (Join-Path $PSScriptRoot "_verified_download.ps1")
$OutDir = Join-Path $Root "data/models/biometrics"
$Url = "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/wespeaker_en_voxceleb_resnet34_LM.onnx"
# Pinned SHA-256 of the copy this install was tested with.
Get-VerifiedFile -Url $Url -OutFile (Join-Path $OutDir "speaker_wespeaker_resnet34_lm.onnx") -Sha256 "e9848563da86f263117134dfd7ad63c92355b37de492b55e325400c9d9c39012"
Write-Host "Done. Restart Aqua, then enrol your voice in the app (Voice security) so the lock has a voice to check."
