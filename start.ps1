<#
.SYNOPSIS
  The Universal Single-Command Launcher for Aqua AI Assistant.
  Starts:
    1. Aqua Backend Core (Port 8767)
    2. Aqua Frontend & FalkorDB Cloud Graph Bridge (Port 1420)
    3. Aqua Windows Desktop UI & Voice Orb (aqua-desktop.exe)
    4. Automatically launches the browser to http://127.0.0.1:1420/
#>
param(
    [switch]$NoBrowser,
    [int]$BackendTimeoutSec = 240
)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
if (-not $root) { $root = Get-Location }

Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host "           LAUNCHING AQUA ALL-IN-ONE ASSISTANT            " -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host "Workspace: $root" -ForegroundColor DarkGray

# 1. Environment and AppData Junctions
$env:AQUA_WORKSPACE_ROOT = $root
[Environment]::SetEnvironmentVariable('AQUA_WORKSPACE_ROOT', $root, 'Process')
[Environment]::SetEnvironmentVariable('AQUA_WORKSPACE_ROOT', $root, 'User')

$kokoroVendor = Join-Path $root 'vendor\kokoro-tts'
$kokoroCliBin = Join-Path $kokoroVendor '.venv\Scripts\kokoro-tts.exe'
$env:AQUA_KOKORO_ROOT = $kokoroVendor
[Environment]::SetEnvironmentVariable('AQUA_KOKORO_ROOT', $kokoroVendor, 'Process')
[Environment]::SetEnvironmentVariable('AQUA_KOKORO_ROOT', $kokoroVendor, 'User')
if (Test-Path $kokoroCliBin) {
    $env:AQUA_KOKORO_CLI_BIN = $kokoroCliBin
    [Environment]::SetEnvironmentVariable('AQUA_KOKORO_CLI_BIN', $kokoroCliBin, 'Process')
    [Environment]::SetEnvironmentVariable('AQUA_KOKORO_CLI_BIN', $kokoroCliBin, 'User')
}

$appDataAqua = "C:\Users\lenovo\AppData\Local\Aqua"
$appDataModels = Join-Path $appDataAqua "data\models"
$appDataVendor = Join-Path $appDataAqua "vendor"
$repoModels = Join-Path $root "data\models"
$repoVendor = Join-Path $root "vendor"

New-Item -ItemType Directory -Force (Join-Path $appDataAqua "data") | Out-Null
if (-not (Test-Path $appDataModels)) {
    try { New-Item -ItemType Junction -Path $appDataModels -Target $repoModels | Out-Null } catch {}
}
if (-not (Test-Path $appDataVendor)) {
    try { New-Item -ItemType Junction -Path $appDataVendor -Target $repoVendor | Out-Null } catch {}
}

# 2. Stop old processes cleanly
Write-Host "`n[1/4] Stopping stale processes..." -ForegroundColor Yellow
$old = @(Get-Process aqua-desktop, aqua-core -ErrorAction SilentlyContinue)
foreach ($port in @(8767, 8766)) {
    foreach ($c in (Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue)) {
        $old += Get-Process -Id $c.OwningProcess -ErrorAction SilentlyContinue
    }
}
if ($old.Count -gt 0) {
    $old | Select-Object -Unique | ForEach-Object {
        try { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue } catch {}
    }
    Start-Sleep -Seconds 2
}
Write-Host "      Clean state ready." -ForegroundColor Green

# 3. Ensure Frontend is serving
Write-Host "`n[2/4] Starting Frontend & FalkorDB Bridge (Port 1420)..." -ForegroundColor Yellow
$desktopDir = Join-Path $root 'apps\aqua-desktop'
$isViteUp = $false
try {
    $res = Invoke-WebRequest -Uri 'http://127.0.0.1:1420/' -UseBasicParsing -TimeoutSec 2
    if ($res.StatusCode -eq 200) { $isViteUp = $true }
} catch {}

if (-not $isViteUp) {
    Start-Process cmd.exe -ArgumentList '/c', "cd /d `"$desktopDir`" && npm run dev" -WindowStyle Minimized
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 2
        try {
            $check = Invoke-WebRequest -Uri 'http://127.0.0.1:1420/' -UseBasicParsing -TimeoutSec 2
            if ($check.StatusCode -eq 200) { $isViteUp = $true; break }
        } catch {}
    }
}
Write-Host "      Frontend & FalkorDB Cloud Bridge live on http://127.0.0.1:1420/" -ForegroundColor Green

# 4. Launch Desktop UI and Release Backend
Write-Host "`n[3/4] Launching Windows Desktop UI & Backend Core..." -ForegroundColor Yellow
$exe = Join-Path $desktopDir 'src-tauri\target\release\aqua-desktop.exe'
$workDir = Split-Path $exe

$app = Start-Process -FilePath $exe -WorkingDirectory $workDir -PassThru
Start-Sleep -Seconds 4
if ($app.HasExited) {
    Write-Host "      Desktop app exited with code $($app.ExitCode)." -ForegroundColor Red
    exit 1
}
Write-Host "      Desktop App running (PID $($app.Id))." -ForegroundColor Green

# 5. Wait for Backend Gateway
Write-Host "`n[4/4] Waiting for Backend Gateway on http://127.0.0.1:8767..." -ForegroundColor Yellow
$healthy = $false
$deadline = (Get-Date).AddSeconds($BackendTimeoutSec)
while ((Get-Date) -lt $deadline) {
    try {
        $h = Invoke-WebRequest -Uri 'http://127.0.0.1:8767/api/v1/health' -UseBasicParsing -TimeoutSec 2
        if ($h.StatusCode -eq 200) { $healthy = $true; break }
    } catch {}
    if ($app.HasExited) {
        Write-Host "      Desktop app closed during backend startup." -ForegroundColor Red
        exit 1
    }
    Start-Sleep -Seconds 3
}

if (-not $healthy) {
    Write-Host "      Backend did not become healthy in $BackendTimeoutSec s." -ForegroundColor Red
    exit 1
}
Write-Host "      Backend Core is healthy and ready!" -ForegroundColor Green

# 6. Open Browser
if (-not $NoBrowser) {
    Start-Process 'http://127.0.0.1:1420/'
}

Write-Host "`n==========================================================" -ForegroundColor Green
Write-Host "            AQUA IS UP AND READY TO USE!                  " -ForegroundColor Green
Write-Host "==========================================================" -ForegroundColor Green
Write-Host "  1. Desktop Voice Orb:   Floating 300x300 circular orb on screen"
Write-Host "  2. Desktop Admin Panel: 1200x800 Management window"
Write-Host "  3. Browser Web Console: http://127.0.0.1:1420/ (opened in browser)"
Write-Host "  4. FalkorDB Memory:     Connected to cloud cluster (port 62769)"
Write-Host "  5. System Tray:         Aqua icon next to the system clock"
Write-Host "==========================================================" -ForegroundColor Green
Write-Host "`nPress Ctrl+C to stop or close this window when done.`n"

# Keep process alive
Wait-Process -Id $app.Id -ErrorAction SilentlyContinue
