<#
.SYNOPSIS
  Start all of Aqua in order: frontend, desktop windows, backend.
  Delegates directly to start-aqua-release.ps1 for reliable release-speed execution.
#>
param(
    [switch]$Build,
    [switch]$KeepAlive,
    [int]$BackendTimeoutSec = 240
)

$PSScript = Join-Path $PSScriptRoot 'start-aqua-release.ps1'
& $PSScript -BackendTimeoutSec $BackendTimeoutSec -KeepAlive:$KeepAlive
