<#
.SYNOPSIS
  Removes Aqua's installed data folder (%LOCALAPPDATA%\Aqua): models, databases, tokens, logs.

.DESCRIPTION
  Nothing is deleted until you confirm. Use -WhatIf to see what would be removed. Aqua's secrets in Windows
  Credential Manager (entries named "Aqua/...") are listed but only removed if you also pass -IncludeCredentials.
  A development checkout keeps its data in the repository's own data\ folder; this script does not touch that.
#>
[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'High')]
param(
    [switch]$IncludeCredentials,
    [switch]$Force   # skip the typed confirmation (for scripts)
)
$ErrorActionPreference = 'Stop'
# -Force means no questions at all: ConfirmImpact 'High' would otherwise still ask for every action.
if ($Force) { $ConfirmPreference = 'None' }

$AppDataPath = Join-Path $env:LOCALAPPDATA 'Aqua'
if (-not (Test-Path $AppDataPath)) {
    Write-Host "No Aqua data folder at $AppDataPath. Nothing to remove."
    exit 0
}

$bytes = (Get-ChildItem $AppDataPath -Recurse -Force -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum
Write-Host ("Aqua data folder: {0}  ({1:N1} GB)" -f $AppDataPath, ($bytes / 1GB)) -ForegroundColor Yellow
Write-Host 'It holds the downloaded models, databases (including your enrolled voice profile), tokens and logs.'

$creds = @(cmdkey /list 2>$null | Select-String -Pattern 'Target:\s*(?:\w+:target=)?Aqua/' | ForEach-Object { ($_ -replace '.*Target:\s*', '').Trim() })
if ($creds.Count -gt 0) {
    Write-Host "Credential Manager entries for Aqua ($($creds.Count)):"
    $creds | ForEach-Object { Write-Host "  $_" }
    if (-not $IncludeCredentials) { Write-Host 'They will be KEPT (pass -IncludeCredentials to remove them too).' }
}

if (-not $Force -and -not $WhatIfPreference) {
    $answer = Read-Host 'Type DELETE to permanently remove the folder above'
    if ($answer -cne 'DELETE') { Write-Host 'Cancelled. Nothing was removed.'; exit 1 }
}

# Running programs keep files open: the desktop app (it would start the core again), the core, and everything the core
# started (the model server, the speech daemon...) are stopped first, and so is anything running from the data folder.
$targets = @()
foreach ($name in 'aqua-desktop', 'aqua-core') { $targets += @(Get-Process -Name $name -ErrorAction SilentlyContinue) }
$targets += @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($AppDataPath, [System.StringComparison]::OrdinalIgnoreCase) })
$targets = @($targets | Sort-Object Id -Unique)
if ($targets.Count -gt 0) {
    if ($PSCmdlet.ShouldProcess(($targets | ForEach-Object { "$($_.ProcessName) ($($_.Id))" }) -join ', ', 'Stop with its child processes')) {
        Write-Host 'Stopping Aqua and what it started...'
        foreach ($t in $targets) { taskkill /PID $t.Id /T /F 2>$null | Out-Null }
        Start-Sleep -Seconds 2
    }
}

$failed = $false
if ($PSCmdlet.ShouldProcess($AppDataPath, 'Remove recursively')) {
    try {
        Remove-Item -Path $AppDataPath -Recurse -Force -ErrorAction Stop
    } catch {
        Write-Warning "Could not remove everything: $($_.Exception.Message)"
        $failed = $true
    }
    if (Test-Path $AppDataPath) {
        $left = @(Get-ChildItem $AppDataPath -Recurse -Force -ErrorAction SilentlyContinue).Count
        Write-Warning "$left item(s) are still in $AppDataPath (probably in use). Close Aqua and run again."
        $failed = $true
    }
}

if ($IncludeCredentials -and $creds.Count -gt 0) {
    foreach ($c in $creds) {
        if ($PSCmdlet.ShouldProcess($c, 'Delete credential')) {
            cmdkey /delete:$c | Out-Null
            if ($LASTEXITCODE -ne 0) { Write-Warning "Could not delete credential $c"; $failed = $true }
        }
    }
}

if ($WhatIfPreference) { Write-Host 'Dry run only: nothing was removed.'; exit 0 }
if ($failed) { Write-Host 'Finished with problems: see the warnings above.' -ForegroundColor Red; exit 2 }
Write-Host 'Aqua data removed.' -ForegroundColor Green
