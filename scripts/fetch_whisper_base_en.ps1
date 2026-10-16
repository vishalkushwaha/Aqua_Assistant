# Whisper base.en (whisper.cpp ggml)
$ErrorActionPreference = "Stop"
# An installed copy keeps its data in %LOCALAPPDATA%\Aqua: run with AQUA_WORKSPACE_ROOT set to that folder.
$Root = if ($env:AQUA_WORKSPACE_ROOT) { $env:AQUA_WORKSPACE_ROOT } else { Split-Path -Parent $PSScriptRoot }
. (Join-Path $PSScriptRoot "_verified_download.ps1")
$OutDir = Join-Path $Root "data/models/whisper"
$OutFile = Join-Path $OutDir "ggml-base.en.bin"
$Url = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin"
# Pinned SHA-256 (the value Hugging Face publishes for this file)
$Sha256 = "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002"

Get-VerifiedFile -Url $Url -OutFile $OutFile -Sha256 $Sha256
