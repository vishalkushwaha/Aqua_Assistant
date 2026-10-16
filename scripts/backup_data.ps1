<#
  Back up Aqua's state before an upgrade: settings, recipes, learning, tool configuration and the
  audit log. Not included, on purpose:
    * .aqua_api_token / .aqua_ui_token  (secrets; Aqua makes new ones)
    * models\                            (large and re-downloadable)
    * browser\, browser_mcp\             (caches and browser profiles)
    * backups\                           (earlier backups: a backup must not contain the backups before it)
  Secrets in Windows Credential Manager are not files and are not touched. That includes the key that encrypts the
  enrolled voice: a backup restored on another computer or Windows account keeps the (encrypted) voice data but
  cannot read it, so enrol the voice again there.

  Aqua must be stopped: its databases are open files, and a copy taken while they are being written can be torn.
  The script refuses to run while aqua-core is running (-AllowRunning overrides, at your own risk).

  Where: the data folder is found the way Aqua finds it (AQUA_WORKSPACE_ROOT\data, else the repository's data\,
  else %LOCALAPPDATA%\Aqua\data). Backups go to <data>\backups, which git ignores.

  Usage:  powershell -File scripts\backup_data.ps1 [-Data <folder>] [-Out <folder>] [-AllowRunning]
  Restore: stop Aqua, delete any *.db-wal and *.db-shm files in the data folder (a restored database next to the write-ahead
           log of a different copy can be corrupted), then
           Expand-Archive <zip> -DestinationPath <data folder> -Force
#>
param(
    [string]$Data = "",
    [string]$Out = "",
    [switch]$AllowRunning
)
$ErrorActionPreference = "Stop"

if (-not $Data) {
    $repoData = Join-Path (Split-Path -Parent $PSScriptRoot) "data"
    if ($env:AQUA_WORKSPACE_ROOT) { $Data = Join-Path $env:AQUA_WORKSPACE_ROOT "data" }
    elseif (Test-Path $repoData) { $Data = $repoData }
    else { $Data = Join-Path $env:LOCALAPPDATA "Aqua\data" }
}
if (-not (Test-Path $Data)) { throw "data folder '$Data' not found" }
if (-not $Out) { $Out = Join-Path $Data "backups" }

$running = Get-Process -Name "aqua-core" -ErrorAction SilentlyContinue
if ($running -and -not $AllowRunning) {
    throw "Aqua is running (aqua-core, pid $($running[0].Id)). Stop it first so the databases are not copied half-written, or pass -AllowRunning."
}

New-Item -ItemType Directory -Force -Path $Out | Out-Null
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$zip = Join-Path $Out "aqua-data-$stamp.zip"
$stage = Join-Path ([System.IO.Path]::GetTempPath()) "aqua-backup-$stamp"
New-Item -ItemType Directory -Force -Path $stage | Out-Null
try {
    # robocopy exit codes 0-7 mean success.
    robocopy $Data $stage /E /XD models browser browser_mcp backups /XF .aqua_api_token .aqua_ui_token /NFL /NDL /NJH /NJS /NP | Out-Null
    if ($LASTEXITCODE -ge 8) { throw "copy failed (robocopy exit $LASTEXITCODE)" }
    Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip -Force
}
finally {
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
}
$size = [math]::Round((Get-Item $zip).Length / 1MB, 1)
Write-Host "Backup written: $zip ($size MB)"
