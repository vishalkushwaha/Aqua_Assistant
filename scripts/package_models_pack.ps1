<#
.SYNOPSIS
  Zips the AI models a tester needs next to the Aqua installer: Aqua-models-pack.zip.

.DESCRIPTION
  The installer (an NSIS .exe) cannot hold the models: the language model alone is 5 GB and the installer format stops at
  2 GB. The pack holds the same folders as data\models, and its install_models.cmd copies them to
  %LOCALAPPDATA%\Aqua\data\models, where an installed Aqua looks for them.

  Included:  whisper (base, small, voice-activity model), kokoro (voice), llm (Qwen3-8B), deepfilternet, biometrics, laya (safety gate).
  Left out:  whisper medium (1.5 GB; the app works without it, the Speech panel offers it when present), the 0.5B test model,
             and anything personal (the Laya "learning" folder, databases, tokens, the enrolled voice).

.PARAMETER SkipLaya
  Leave out the 1.5 GB Laya safety gate (Aqua then uses its built-in rules only).
.PARAMETER IncludeMedium
  Also include the Whisper medium model.
.PARAMETER Out
  Output zip (default: dist\Aqua-models-pack.zip).
#>
param(
    [switch]$SkipLaya,
    [switch]$IncludeMedium,
    [string]$Out = ""
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$models = Join-Path $root "data\models"
if (-not $Out) { $Out = Join-Path $root "dist\Aqua-models-pack.zip" }
New-Item -ItemType Directory -Force (Split-Path $Out) | Out-Null

$stage = Join-Path (Split-Path $Out) "_models_stage"   # beside the output so the hard links stay on one drive
if (Test-Path $stage) { [System.IO.Directory]::Delete($stage, $true) }
$m = Join-Path $stage "models"
New-Item -ItemType Directory -Force $m | Out-Null

# Hard links keep the staging folder from copying 7 GB a second time (same drive); fall back to a copy.
function Put([string]$rel) {
    $src = Join-Path $models $rel
    if (-not (Test-Path $src)) { Write-Warning "Not in the pack (missing): $rel"; return }
    $dst = Join-Path $m $rel
    New-Item -ItemType Directory -Force (Split-Path $dst) | Out-Null
    if ((Get-Item $src).PSIsContainer) {
        Get-ChildItem $src -Recurse -File | ForEach-Object {
            $rest = $_.FullName.Substring($src.Length).TrimStart('\')
            $to = Join-Path $dst $rest
            New-Item -ItemType Directory -Force (Split-Path $to) | Out-Null
            try { New-Item -ItemType HardLink -Path $to -Target $_.FullName | Out-Null } catch { Copy-Item $_.FullName $to }
        }
    } else {
        try { New-Item -ItemType HardLink -Path $dst -Target $src | Out-Null } catch { Copy-Item $src $dst }
    }
}

Put "whisper\ggml-base.en.bin"
Put "whisper\ggml-small.en.bin"
Put "whisper\ggml-silero-v5.1.2.bin"
if ($IncludeMedium) { Put "whisper\ggml-medium.en.bin" }
Put "kokoro\kokoro-v1.0.onnx"
Put "kokoro\voices-v1.0.bin"
Put "llm\Qwen3-8B-Q4_K_M.gguf"
Put "deepfilternet\DeepFilterNet3_onnx"
Put "biometrics\speaker_wespeaker_resnet34_lm.onnx"
if (-not $SkipLaya) { Put "laya\models" }

@'
@echo off
rem Copies the models next to this file into the folder an installed Aqua reads them from.
setlocal
set "DEST=%LOCALAPPDATA%\Aqua\data\models"
echo Copying Aqua's models to %DEST% ...
if not exist "%DEST%" mkdir "%DEST%"
robocopy "%~dp0models" "%DEST%" /E /NFL /NDL /NJH /NP /R:1 /W:1
if errorlevel 8 (
  echo Copying failed. Check that there is enough free disk space ^(about 8 GB^).
  pause
  exit /b 1
)
echo Done. Start Aqua from the Start menu.
pause
'@ | Set-Content -Encoding ascii (Join-Path $stage "install_models.cmd")

if (Test-Path $Out) { [System.IO.File]::Delete($Out) }
Write-Host "Zipping to $Out (the language model does not compress, so this takes a few minutes)..."
Push-Location $stage
try { & (Join-Path $env:SystemRoot "System32\tar.exe") -a -c -f $Out "install_models.cmd" "models" } finally { Pop-Location }
if ($LASTEXITCODE -ne 0) { throw "tar failed" }
[System.IO.Directory]::Delete($stage, $true)
Write-Host ("Models pack ready: {0} ({1:n2} GB)" -f $Out, ((Get-Item $Out).Length / 1GB))
