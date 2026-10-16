# Qwen1.5 0.5B Chat GGUF Q4_K_M: official Hugging Face release (used with llama.cpp)
$ErrorActionPreference = "Stop"
# An installed copy keeps its data in %LOCALAPPDATA%\Aqua: run with AQUA_WORKSPACE_ROOT set to that folder.
$Root = if ($env:AQUA_WORKSPACE_ROOT) { $env:AQUA_WORKSPACE_ROOT } else { Split-Path -Parent $PSScriptRoot }
. (Join-Path $PSScriptRoot "_verified_download.ps1")
$OutDir = Join-Path $Root "data/models/llm"
$OutFile = Join-Path $OutDir "qwen-0.5b-instruct-q4_k_m.gguf"
$Url = "https://huggingface.co/Qwen/Qwen1.5-0.5B-Chat-GGUF/resolve/main/qwen1_5-0_5b-chat-q4_k_m.gguf"
# Pinned SHA-256 (the value Hugging Face publishes for this file)
$Sha256 = "92916b71d32f5afea48fb7383e3b48c5b1c111f5a59f0b83c764ea1d07fe1a3a"

Get-VerifiedFile -Url $Url -OutFile $OutFile -Sha256 $Sha256
