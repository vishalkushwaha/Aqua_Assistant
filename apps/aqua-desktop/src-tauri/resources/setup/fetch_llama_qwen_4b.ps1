# Qwen 4B / Fast Tier GGUF Q4_K_M: official Hugging Face release (llama.cpp)
$ErrorActionPreference = "Stop"
# An installed copy keeps its data in %LOCALAPPDATA%\Aqua: run with AQUA_WORKSPACE_ROOT set to that folder.
$Root = if ($env:AQUA_WORKSPACE_ROOT) { $env:AQUA_WORKSPACE_ROOT } else { Split-Path -Parent $PSScriptRoot }
. (Join-Path $PSScriptRoot "_verified_download.ps1")
$OutDir = Join-Path $Root "data/models/llm"

# 1. First check if Qwen3.5-4B-Q4_K_M.gguf is already present
$Local4B = Join-Path $OutDir "Qwen3.5-4B-Q4_K_M.gguf"
$LocalHash = "00fe7986ff5f6b463e62455821146049db6f9313603938a70800d1fb69ef11a4"

if (Test-Path $Local4B) {
    if ((Get-Sha256Hex $Local4B) -eq $LocalHash) {
        Write-Host "Already present and verified: $Local4B"
        return
    }
}

# 2. Otherwise download Qwen 3B/4B instruction GGUF from official Hugging Face
$OutFile = Join-Path $OutDir "qwen2.5-3b-instruct-q4_k_m.gguf"
$Url = "https://huggingface.co/Qwen/Qwen2.5-3B-Instruct-GGUF/resolve/main/qwen2.5-3b-instruct-q4_k_m.gguf"
$Sha256 = "626b4a6678b86442240e33df819e00132d3ba7dddfe1cdc4fbb18e0a9615c62d"

Get-VerifiedFile -Url $Url -OutFile $OutFile -Sha256 $Sha256
