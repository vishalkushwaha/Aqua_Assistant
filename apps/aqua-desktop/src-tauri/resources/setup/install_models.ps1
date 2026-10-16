<#
.SYNOPSIS
  Downloads the AI models Aqua needs, from their public sources, into the data folder. Safe to run again: files that are
  already complete and verified are skipped, and an interrupted download carries on where it stopped.

.DESCRIPTION
  The installer runs this after copying the program. It is also on the Start menu ("Download Aqua models") and works in a
  development checkout. It uses the same pinned, SHA-256-checked fetch scripts as the rest of the project, so a damaged or
  partial download is never left in place as if it were a model.

  Models go to  %LOCALAPPDATA%\Aqua\data\models  (or <AQUA_WORKSPACE_ROOT>\data\models). About 6.5 GB in all.
  The Laya safety gate (about 1.5 GB) is fetched by Aqua itself the first time it runs; it needs the internet then.
#>
param([switch]$NoPause)
$ErrorActionPreference = "Stop"

if (-not $env:AQUA_WORKSPACE_ROOT) {
    # In a development checkout (scripts\ beside Cargo.toml) use it; an installed copy keeps its data in %LOCALAPPDATA%\Aqua.
    $repo = Split-Path -Parent $PSScriptRoot
    $env:AQUA_WORKSPACE_ROOT = if ((Split-Path -Leaf $PSScriptRoot) -eq "scripts" -and (Test-Path (Join-Path $repo "Cargo.toml"))) { $repo } else { Join-Path $env:LOCALAPPDATA "Aqua" }
}
$models = Join-Path $env:AQUA_WORKSPACE_ROOT "data\models"
New-Item -ItemType Directory -Force $models | Out-Null

function Finish([int]$code) {
    if (-not $NoPause) { Write-Host ""; [void](Read-Host "Press Enter to close") }
    exit $code
}

Write-Host ""
Write-Host "  Aqua: downloading the speech and language models" -ForegroundColor Cyan
Write-Host "  Into: $models"
Write-Host "  About 6.5 GB. You can close this window and run it again later; it carries on where it stopped."
Write-Host ""

# Enough room? (what is still missing, roughly)
$drive = Get-PSDrive -Name ((Split-Path -Qualifier $models).TrimEnd(':')) -ErrorAction SilentlyContinue
if ($drive -and $drive.Free -lt 8GB -and -not (Test-Path (Join-Path $models "llm\Qwen3-8B-Q4_K_M.gguf"))) {
    Write-Host ("  Only {0:n1} GB free on that drive; about 8 GB is needed. Free some space and run this again." -f ($drive.Free / 1GB)) -ForegroundColor Yellow
    Finish 1
}

# Can the sources be reached?
curl.exe -sS -I -L --max-time 15 -o NUL https://huggingface.co 2>$null
if ($LASTEXITCODE -ne 0) {
    Write-Host "  Cannot reach huggingface.co. Check the internet connection (or a proxy/firewall) and run this again." -ForegroundColor Yellow
    Finish 1
}

$steps = @(
    @{ Name = "Noise filter (DeepFilterNet3, 8 MB)";        Script = "fetch_deepfilternet3_onnx.ps1" },
    @{ Name = "Voice lock model (WeSpeaker, 26 MB)";        Script = "fetch_speaker_model.ps1" },
    @{ Name = "Voice-activity model (Silero, 1 MB)";        Script = "fetch_whisper_vad.ps1" },
    @{ Name = "Voice (Kokoro, 350 MB)";                     Script = "fetch_kokoro_v1.ps1" },
    @{ Name = "Listening, idle (Whisper base, 150 MB)";     Script = "fetch_whisper_base_en.ps1" },
    @{ Name = "Listening, after wake-up (Whisper small, 490 MB)"; Script = "fetch_whisper_small_en.ps1" },
    @{ Name = "Language model (Qwen3 8B, 5 GB)";            Script = "fetch_llama_qwen3_8b.ps1" }
)
$failed = @()
$n = 0
foreach ($s in $steps) {
    $n++
    Write-Host ("[{0}/{1}] {2}" -f $n, $steps.Count, $s.Name) -ForegroundColor Cyan
    $path = Join-Path $PSScriptRoot $s.Script
    if (-not (Test-Path $path)) { Write-Host "  missing script $($s.Script)" -ForegroundColor Red; $failed += $s.Name; continue }
    try { & $path } catch { Write-Host "  FAILED: $($_.Exception.Message)" -ForegroundColor Red; $failed += $s.Name }
    Write-Host ""
}

if ($failed.Count -eq 0) {
    Write-Host "  All models are in place. Start Aqua from the Start menu." -ForegroundColor Green
    Finish 0
} else {
    Write-Host "  These did not finish:" -ForegroundColor Yellow
    $failed | ForEach-Object { Write-Host "    - $_" -ForegroundColor Yellow }
    Write-Host "  Check the connection and run 'Download Aqua models' from the Start menu again; finished files are kept."
    Finish 1
}
