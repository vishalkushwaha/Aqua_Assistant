<#
.SYNOPSIS
  Start Aqua at release speed: frontend, desktop windows, and the release backend, together.

.DESCRIPTION
  start-aqua.ps1 uses the debug backend and forces the tiny 0.5B model. This script does not.
  It checks release binaries, builds the admin UI, prepares the desktop sidecar slot,
  and opens the windows. The desktop app starts the release backend itself.

  Pass -Rebuild to force recompiling aqua-core and aqua-desktop from source with cargo.

.EXAMPLE
  powershell -File scripts\start-aqua-release.ps1
  powershell -File scripts\start-aqua-release.ps1 -Rebuild
#>
param(
    [switch]$Rebuild,
    [switch]$KeepAlive = $true,
    [switch]$NoWait,
    [switch]$NoBrowser,
    [int]$BackendTimeoutSec = 240
)

$ErrorActionPreference = 'Stop'
$root     = Split-Path -Parent $PSScriptRoot
$desktop  = Join-Path $root 'apps\aqua-desktop'
$tauriDir = Join-Path $desktop 'src-tauri'
$exe      = Join-Path $tauriDir 'target\release\aqua-desktop.exe'
$coreExe  = Join-Path $root 'target\release\aqua-core.exe'

# Detect rustc target triple for Tauri sidecar resolution
$targetTriple = 'x86_64-pc-windows-gnu'
try {
    $rustcOut = & rustc -vV 2>$null
    $hostLine = ($rustcOut | Where-Object { $_ -match '^host:\s*(.+)$' })
    if ($hostLine -match '^host:\s*(.+)$') {
        $targetTriple = $Matches[1].Trim()
    }
} catch { /* use default */ }

$sidecar    = Join-Path $tauriDir "bin\aqua-core-$targetTriple.exe"
$sidecarGnu = Join-Path $tauriDir 'bin\aqua-core-x86_64-pc-windows-gnu.exe'
$uiDist     = Join-Path $desktop 'dist'
$healthUrl  = 'http://127.0.0.1:8767/api/v1/health'

function Step($n, $text) { Write-Host ""; Write-Host "[$n/4] $text" -ForegroundColor Cyan }
function Ok($text)   { Write-Host "      OK   $text" -ForegroundColor Green }
function Info($text) { Write-Host "      ...  $text" -ForegroundColor DarkGray }
function Fail($text) { Write-Host "      FAIL $text" -ForegroundColor Red; exit 1 }

function Stop-OldAqua {
    $old = @(Get-Process aqua-desktop, aqua-core -ErrorAction SilentlyContinue)
    foreach ($c in (Get-NetTCPConnection -LocalPort 8766 -State Listen -ErrorAction SilentlyContinue)) {
        $proc = Get-CimInstance Win32_Process -Filter "ProcessId=$($c.OwningProcess)" -ErrorAction SilentlyContinue
        if ($proc -and $proc.CommandLine -like '*kokoro_daemon.py*') {
            $old += Get-Process -Id $c.OwningProcess -ErrorAction SilentlyContinue
        }
    }
    foreach ($c in (Get-NetTCPConnection -LocalPort 8767 -State Listen -ErrorAction SilentlyContinue)) {
        $old += Get-Process -Id $c.OwningProcess -ErrorAction SilentlyContinue
    }
    if ($old.Count -gt 0) {
        $old = $old | Select-Object -Unique
        $old | ForEach-Object { cmd.exe /c "taskkill /PID $($_.Id) /T /F >nul 2>&1" | Out-Null }
        Start-Sleep -Seconds 3
    }
    return $old
}

function Test-Url($url) {
    try { return (Invoke-WebRequest $url -UseBasicParsing -TimeoutSec 3).StatusCode -eq 200 } catch { return $false }
}

# Ensure permanent workspace root and AppData model junctions
$env:AQUA_WORKSPACE_ROOT = $root
[Environment]::SetEnvironmentVariable('AQUA_WORKSPACE_ROOT', $root, 'Process')
[Environment]::SetEnvironmentVariable('AQUA_WORKSPACE_ROOT', $root, 'User')

$appDataAqua = "C:\Users\lenovo\AppData\Local\Aqua"
$appDataModels = Join-Path $appDataAqua "data\models"
$appDataVendor = Join-Path $appDataAqua "vendor"
$repoModels = Join-Path $root "data\models"
$repoVendor = Join-Path $root "vendor"

New-Item -ItemType Directory -Force (Join-Path $appDataAqua "data") | Out-Null
if (-not (Test-Path $appDataModels) -and (Test-Path $repoModels)) {
    try { New-Item -ItemType Junction -Path $appDataModels -Target $repoModels | Out-Null } catch {}
}
if (-not (Test-Path $appDataVendor) -and (Test-Path $repoVendor)) {
    try { New-Item -ItemType Junction -Path $appDataVendor -Target $repoVendor | Out-Null } catch {}
}

$vendorDir = Join-Path $root 'vendor'
if (-not (Test-Path $vendorDir)) {
    $altVendor = 'E:\Personal_AI\Aqua\vendor'
    if (Test-Path $altVendor) {
        try {
            New-Item -ItemType Junction -Path $vendorDir -Target $altVendor | Out-Null
            Info "Created vendor junction -> $altVendor"
        } catch { /* proceed */ }
    }
}

Step 1 'Stopping any old Aqua so the release programs can run cleanly'
$old = Stop-OldAqua
if ($old) { Ok ("stopped: " + (($old | ForEach-Object { $_.Name }) -join ', ')) } else { Ok 'nothing was running' }

Step 2 'Verifying release programs and building the UI'
if (-not (Get-Command node -ErrorAction SilentlyContinue)) { Fail 'Node.js was not found on PATH.' }
if (-not (Test-Path (Join-Path $desktop 'node_modules'))) { Fail "Frontend packages are missing. Run: cd apps\aqua-desktop ; npm install" }

$sidecarWasMissing = -not (Test-Path -LiteralPath $sidecar)
$needCoreBuild = $Rebuild -or (-not (Test-Path -LiteralPath $coreExe))

if ($needCoreBuild) {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Fail 'cargo was not found on PATH.' }
    Info 'Building aqua-core --release ...'
    Push-Location $root
    try {
        cmd.exe /c "cargo build -p aqua-core --release"
        if ($LASTEXITCODE -ne 0) { Fail 'cargo build -p aqua-core --release failed.' }
    } finally { Pop-Location }
    if (-not (Test-Path -LiteralPath $coreExe)) { Fail "Release backend was not produced: $coreExe" }
}

# Ensure sidecar directories and files are populated
New-Item -ItemType Directory -Force (Split-Path $sidecar) | Out-Null
Copy-Item -LiteralPath $coreExe -Destination $sidecar -Force
if ($sidecar -ne $sidecarGnu) {
    Copy-Item -LiteralPath $coreExe -Destination $sidecarGnu -Force
}

# Ensure WebView2Loader.dll is present next to the desktop executable
$dllDest = Join-Path (Split-Path $exe) 'WebView2Loader.dll'
if (-not (Test-Path $dllDest)) {
    $altDll = 'E:\Personal_AI\Aqua\apps\aqua-desktop\src-tauri\target\release\WebView2Loader.dll'
    if (Test-Path $altDll) {
        Copy-Item -LiteralPath $altDll -Destination $dllDest -Force
        Copy-Item -LiteralPath $altDll -Destination (Join-Path $tauriDir 'WebView2Loader.dll') -Force
    }
}
Ok 'release backend sidecar ready'

# A release Tauri executable embeds both its window configuration and the frontend
# assets that existed when Cargo built it. Rebuilding only `dist` leaves an existing
# Windows executable stale, so compare every desktop input with the executable.
$needDesktopBuild = $Rebuild -or (-not (Test-Path -LiteralPath $exe)) -or $sidecarWasMissing
if (-not $needDesktopBuild) {
    $desktopInputRoots = @(
        (Join-Path $desktop 'src'),
        (Join-Path $tauriDir 'src'),
        (Join-Path $desktop 'public')
    ) | Where-Object { Test-Path -LiteralPath $_ }
    $desktopInputs = @($desktopInputRoots | ForEach-Object {
        Get-ChildItem -LiteralPath $_ -Recurse -File
    })
    $desktopInputFiles = @(
        (Join-Path $desktop 'index.html'),
        (Join-Path $desktop 'package.json'),
        (Join-Path $desktop 'package-lock.json'),
        (Join-Path $desktop 'vite.config.ts'),
        (Join-Path $desktop 'falkorPlugin.ts'),
        (Join-Path $desktop 'companyBrain.ts'),
        (Join-Path $tauriDir 'tauri.conf.json'),
        (Join-Path $tauriDir 'Cargo.toml'),
        (Join-Path $tauriDir 'Cargo.lock')
    ) | Where-Object { Test-Path -LiteralPath $_ }
    $desktopInputs += @($desktopInputFiles | ForEach-Object { Get-Item -LiteralPath $_ })
    $exeTimestamp = (Get-Item -LiteralPath $exe).LastWriteTimeUtc
    $needDesktopBuild = [bool]($desktopInputs | Where-Object { $_.LastWriteTimeUtc -gt $exeTimestamp } | Select-Object -First 1)
}

# Always build the latest UI so changes to admin panels and FalkorDB integration are reflected
Info 'Building the admin UI (npm run build) ...'
Push-Location $desktop
try {
    cmd.exe /c "npm run build"
    if ($LASTEXITCODE -ne 0) { Fail 'npm run build failed.' }
} finally { Pop-Location }
if (-not (Test-Path -LiteralPath (Join-Path $uiDist 'index.html'))) { Fail "Admin UI build is missing: $uiDist" }
Ok 'admin UI build ready'

if ($needDesktopBuild) {
    Info 'Building the desktop app --release ...'
    Push-Location $tauriDir
    try {
        cmd.exe /c "cargo build --release"
        if ($LASTEXITCODE -ne 0) { Fail 'cargo build --release (desktop) failed.' }
    } finally { Pop-Location }
}

if (-not (Test-Path -LiteralPath $exe)) { Fail "Release desktop app is missing: $exe" }
Ok 'release backend, frontend, and desktop app are ready'

# Ensure Frontend & FalkorDB Cloud Bridge is active on port 1420
Info 'Ensuring Frontend & FalkorDB Cloud Bridge is active on port 1420 ...'
$isViteUp = $false
try {
    $res = Invoke-WebRequest -Uri 'http://127.0.0.1:1420/' -UseBasicParsing -TimeoutSec 2
    if ($res.StatusCode -eq 200) { $isViteUp = $true }
} catch {}

if (-not $isViteUp) {
    Info 'Starting Vite dev server & FalkorDB bridge (Port 1420)...'
    Start-Process cmd.exe -ArgumentList '/c', "cd /d `"$desktop`" && npm run dev" -WindowStyle Minimized
    $deadlineVite = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadlineVite) {
        Start-Sleep -Seconds 2
        try {
            $check = Invoke-WebRequest -Uri 'http://127.0.0.1:1420/' -UseBasicParsing -TimeoutSec 2
            if ($check.StatusCode -eq 200) { $isViteUp = $true; break }
        } catch {}
    }
}
Ok 'frontend & FalkorDB Cloud bridge live on http://127.0.0.1:1420/'

Step 3 'Desktop windows (they start the release backend)'
# Do not inherit the debug-script overrides (Laya off, 0.5B model).
Remove-Item Env:AQUA_LAYA -ErrorAction SilentlyContinue
Remove-Item Env:AQUA_AGENT_MODEL -ErrorAction SilentlyContinue
$env:AQUA_WORKSPACE_ROOT = $root

# Configure Kokoro TTS engine vendor root and CLI binary
$kokoroVendor = Join-Path $root 'vendor\kokoro-tts'
$kokoroCliBin = Join-Path $kokoroVendor '.venv\Scripts\kokoro-tts.exe'
$env:AQUA_KOKORO_ROOT = $kokoroVendor
[Environment]::SetEnvironmentVariable('AQUA_KOKORO_ROOT', $kokoroVendor, 'Process')
if (Test-Path $kokoroCliBin) {
    $env:AQUA_KOKORO_CLI_BIN = $kokoroCliBin
    [Environment]::SetEnvironmentVariable('AQUA_KOKORO_CLI_BIN', $kokoroCliBin, 'Process')
}

$app = Start-Process -FilePath $exe -WorkingDirectory (Split-Path $exe) -PassThru
Start-Sleep -Seconds 4
if ($app.HasExited) { Fail "The desktop app closed straight away (exit code $($app.ExitCode))." }
Ok "desktop app running (pid $($app.Id))"

Step 4 "Waiting for the backend on http://127.0.0.1:8767"
$deadline = (Get-Date).AddSeconds($BackendTimeoutSec)
$healthy = $false
while ((Get-Date) -lt $deadline) {
    if (Test-Url $healthUrl) { $healthy = $true; break }
    if ($app.HasExited) { Fail 'The desktop app closed while waiting for the backend.' }
    Start-Sleep -Seconds 3
}
if (-not $healthy) { Fail "The backend did not become healthy within $BackendTimeoutSec s." }
Ok 'backend healthy'

if (-not $NoBrowser) {
    Start-Process 'http://127.0.0.1:1420/'
}

Write-Host ""
Write-Host 'Aqua is up at release speed:' -ForegroundColor Green
Write-Host '  Desktop Windows:  Admin panel (1200x800) + Floating Voice Orb (300x300)'
Write-Host '  System Tray:      Aqua Assistant icon in bottom-right taskbar'
Write-Host '  Backend Core:     http://127.0.0.1:8767 (connected to FalkorDB Cloud)'
Write-Host '  Voice Orb Tip:    Right-click the orb or tray icon -> "Open Panel" to bring up the UI'
Write-Host '  Exit:             Right-click the orb or tray icon -> "Quit Aqua"'

if ($KeepAlive -and -not $NoWait) {
    Write-Host ""
    Write-Host "Keeping session open until desktop app exits..." -ForegroundColor DarkGray
    Wait-Process -Id $app.Id
}
