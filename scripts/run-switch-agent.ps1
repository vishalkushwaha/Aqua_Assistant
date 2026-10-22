# Run Aqua Company Brain Switch Agent
param(
  [switch]$RegisterFirst
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$WorkspaceRoot = Split-Path -Parent $ScriptDir
$AppDir = Join-Path $WorkspaceRoot "apps\aqua-desktop"
$EnvFile = Join-Path $AppDir ".env"
$EnvExample = Join-Path $AppDir ".env.example"
$CredsFile = Join-Path $AppDir ".switch-agent.json"

Write-Host "========================================================" -ForegroundColor Cyan
Write-Host "  Aqua Company Brain -> Switch Team Chat Connector" -ForegroundColor Cyan
Write-Host "========================================================" -ForegroundColor Cyan
Write-Host "Workspace: $WorkspaceRoot"
Write-Host "App Dir:   $AppDir"

if (-not (Test-Path $EnvFile)) {
  if (Test-Path $EnvExample) {
    Write-Host "[INFO] Copying .env.example to .env..." -ForegroundColor Yellow
    Copy-Item $EnvExample $EnvFile
    Write-Host "[WARN] Please verify your .env file in apps/aqua-desktop and set SWITCH_ROOM_IDS." -ForegroundColor Yellow
  } else {
    Write-Host "[ERROR] Missing .env file in $AppDir" -ForegroundColor Red
    exit 1
  }
}

Push-Location $AppDir
try {
  if ($RegisterFirst -or (-not (Test-Path $CredsFile) -and $env:SWITCH_REGISTRATION_TOKEN)) {
    Write-Host "[INFO] Registering Aqua agent with Switch Bridge..." -ForegroundColor Cyan
    npm run switch:register
  }

  Write-Host "[INFO] Starting Aqua Switch Agent listener..." -ForegroundColor Green
  npm run switch:agent
} finally {
  Pop-Location
}
