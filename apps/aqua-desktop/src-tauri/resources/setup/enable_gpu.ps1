<#
.SYNOPSIS
  Enables GPU & CUDA acceleration for Aqua (LLM + Kokoro TTS).
#>
param([switch]$Disable, [switch]$Force)
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot

Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host "         AQUA GPU & CUDA ACCELERATION SETUP               " -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor Cyan

if (Get-Command nvidia-smi -ErrorAction SilentlyContinue) {
    $gpu = (& nvidia-smi --query-gpu=name,driver_version,memory.total --format=csv,noheader 2>$null | Select-Object -First 1)
    Write-Host "Detected NVIDIA GPU: $gpu" -ForegroundColor Green
} else {
    Write-Host "Notice: No NVIDIA driver detected. Continuing with Vulkan/CPU mode." -ForegroundColor Yellow
}

Write-Host "`n[1/2] Configuring GPU acceleration for Llama..." -ForegroundColor Cyan
$llamaGpu = Join-Path $root 'fetch_llama_gpu.ps1'
if (Test-Path $llamaGpu) {
    if ($Disable) { & $llamaGpu -Disable } else { & $llamaGpu }
}

Write-Host "`n[2/2] Configuring CUDA acceleration for Kokoro TTS..." -ForegroundColor Cyan
$kokoroGpu = Join-Path $root 'enable_kokoro_gpu.ps1'
if (Test-Path $kokoroGpu) {
    if ($Disable) { & $kokoroGpu -Disable } elseif ($Force) { & $kokoroGpu -Force } else { & $kokoroGpu }
}

Write-Host "`nAll GPU & CUDA configurations completed successfully!" -ForegroundColor Green
