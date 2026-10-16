# Whisper medium.en (whisper.cpp ggml)
$ErrorActionPreference = "Stop"
# An installed copy keeps its data in %LOCALAPPDATA%\Aqua: run with AQUA_WORKSPACE_ROOT set to that folder.
$Root = if ($env:AQUA_WORKSPACE_ROOT) { $env:AQUA_WORKSPACE_ROOT } else { Split-Path -Parent $PSScriptRoot }
. (Join-Path $PSScriptRoot "_verified_download.ps1")
$OutDir = Join-Path $Root "data/models/whisper"
$OutFile = Join-Path $OutDir "ggml-medium.en.bin"
$Url = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.en.bin"
# Pinned SHA-256 (the value Hugging Face publishes for this file)
$Sha256 = "cc37e93478338ec7700281a7ac30a10128929eb8f427dda2e865faa8f6da4356"

Get-VerifiedFile -Url $Url -OutFile $OutFile -Sha256 $Sha256
