<#
.SYNOPSIS
  Install the official llama.cpp build for graphics cards (Vulkan), so the language model can run partly on the GPU.

.DESCRIPTION
  Aqua's own model server is a CPU program. With this build installed (data\engines\llama-gpu), Aqua starts the same model on
  your graphics card when it finds a discrete one with enough free video memory: the card keeps as many of the model's layers as
  fit and does that share of the work several times faster than the processor. If the card cannot be used (not enough memory, a
  driver error, a game using the memory) Aqua falls back to the processor build by itself, so nothing breaks.

  Measured on a GTX 1650 (4 GB) with Qwen3-8B Q4_K_M, see documentation\PIPELINE_AUDIT.md.

  The zip is the official release from github.com/ggml-org/llama.cpp, pinned by SHA-256 like the other downloads (about 33 MB,
  91 MB unpacked). Nothing is compiled. Undo with -Disable: it deletes the folder and Aqua goes back to the processor.

.PARAMETER Disable
  Delete the GPU build. Aqua goes back to running the model on the processor.

.EXAMPLE
  powershell -File scripts\fetch_llama_gpu.ps1
  powershell -File scripts\fetch_llama_gpu.ps1 -Disable
#>
param(
    [switch]$Disable
)
$ErrorActionPreference = "Stop"

function Stop-EngineHolding([string]$Dir) {
    if (-not (Test-Path -LiteralPath $Dir)) { return }
    $prefix = ((Resolve-Path -LiteralPath $Dir).Path.TrimEnd('\') + '\').ToLowerInvariant()
    $holding = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object {
        $_.ExecutablePath -and $_.ExecutablePath.ToLowerInvariant().StartsWith($prefix)
    })
    foreach ($p in $holding) {
        Write-Host "Stopping $($p.Name) (pid $($p.ProcessId)); it still has the old build open."
        & taskkill.exe /PID $p.ProcessId /T /F | Out-Null
    }
    if ($holding.Count -gt 0) { Start-Sleep -Seconds 2 }
}

# An installed copy keeps its data in %LOCALAPPDATA%\Aqua: run with AQUA_WORKSPACE_ROOT set to that folder.
$Root = if ($env:AQUA_WORKSPACE_ROOT) { $env:AQUA_WORKSPACE_ROOT } else { Split-Path -Parent $PSScriptRoot }
. (Join-Path $PSScriptRoot "_verified_download.ps1")
$OutDir = Join-Path $Root "data/engines/llama-gpu"

if ($Disable) {
    Stop-EngineHolding $OutDir
    if (Test-Path -LiteralPath $OutDir) { Remove-Item -LiteralPath $OutDir -Recurse -Force; Write-Host "Removed $OutDir. Aqua runs the model on the processor." }
    else { Write-Host "Nothing to remove: $OutDir does not exist." }
    return
}

$Build = "b11371"
$Url = "https://github.com/ggml-org/llama.cpp/releases/download/$Build/llama-$Build-bin-win-vulkan-x64.zip"
# Pinned SHA-256 of the zip as downloaded from the address above (it is also the digest GitHub shows for the release asset).
$Sha256 = "dfc666fdf3137595e7d440b994bed0fa1f1dc726099685c1a75291c4bab2013f"

# The zip is kept outside the install folder and removed once it is unpacked, so the folder holds only the program.
$Zip = Join-Path ([System.IO.Path]::GetTempPath()) "llama-$Build-vulkan.zip"
Get-VerifiedFile -Url $Url -OutFile $Zip -Sha256 $Sha256

Stop-EngineHolding $OutDir
if (Test-Path -LiteralPath $OutDir) { Remove-Item -LiteralPath $OutDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
Expand-Archive -LiteralPath $Zip -DestinationPath $OutDir -Force
Remove-Item -LiteralPath $Zip -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath "$Zip.part","$Zip.mismatch" -Force -ErrorAction SilentlyContinue
# The compiled copy under vendor is unused once this official build is installed, and it is often several GB.
$VendorBuild = Join-Path $Root "vendor/llama.cpp/build"
if (Test-Path -LiteralPath $VendorBuild) {
    Remove-Item -LiteralPath $VendorBuild -Recurse -Force
    Write-Host "Removed the old compiled tree: $VendorBuild"
}

$Server = Join-Path $OutDir "llama-server.exe"
if (-not (Test-Path -LiteralPath $Server)) { throw "llama-server.exe is not in the unpacked folder: $OutDir" }
Write-Host "Installed to $OutDir"
Write-Host "Graphics cards this build can use:"
& $Server --list-devices
Write-Host "Restart Aqua (or wait for the model server's idle stop) and the next request loads the model onto the card. The log says 'llama-server ready ... on the GPU'."
