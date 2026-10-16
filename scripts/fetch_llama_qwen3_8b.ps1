# Qwen3-8B GGUF Q4_K_M: official Hugging Face release (llama.cpp)
$ErrorActionPreference = "Stop"
# An installed copy keeps its data in %LOCALAPPDATA%\Aqua: run with AQUA_WORKSPACE_ROOT set to that folder.
$Root = if ($env:AQUA_WORKSPACE_ROOT) { $env:AQUA_WORKSPACE_ROOT } else { Split-Path -Parent $PSScriptRoot }
. (Join-Path $PSScriptRoot "_verified_download.ps1")
$OutDir = Join-Path $Root "data/models/llm"
$OutFile = Join-Path $OutDir "Qwen3-8B-Q4_K_M.gguf"
$Url = "https://huggingface.co/Qwen/Qwen3-8B-GGUF/resolve/main/Qwen3-8B-Q4_K_M.gguf"
# Pinned SHA-256 (the value Hugging Face publishes for this file)
$Sha256 = "d98cdcbd03e17ce47681435b5150e34c1417f50b5c0019dd560e4882c5745785"

Get-VerifiedFile -Url $Url -OutFile $OutFile -Sha256 $Sha256
