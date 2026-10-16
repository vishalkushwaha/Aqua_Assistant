# Silero voice-activity model for Whisper (whisper.cpp ggml format, 0.9 MB).
# With it Whisper transcribes only what it judges to be speech, so wind, typing and fans no longer come back as invented words.
$ErrorActionPreference = "Stop"
# An installed copy keeps its data in %LOCALAPPDATA%\Aqua: run with AQUA_WORKSPACE_ROOT set to that folder.
$Root = if ($env:AQUA_WORKSPACE_ROOT) { $env:AQUA_WORKSPACE_ROOT } else { Split-Path -Parent $PSScriptRoot }
. (Join-Path $PSScriptRoot "_verified_download.ps1")
$OutDir = Join-Path $Root "data/models/whisper"
$OutFile = Join-Path $OutDir "ggml-silero-v5.1.2.bin"
$Url = "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin"
# Pinned SHA-256 of the file as downloaded from the address above
$Sha256 = "29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf"

Get-VerifiedFile -Url $Url -OutFile $OutFile -Sha256 $Sha256
