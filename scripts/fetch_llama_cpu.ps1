<#
.SYNOPSIS
  Install the official llama.cpp build for the processor, which reads prompts about 22 % faster than Aqua's own build.

.DESCRIPTION
  Aqua's own model server (vendor\llama.cpp, built with MinGW) is a plain build. The official release picks the fastest code for
  the processor at run time (AVX2, AVX-512 and so on). Measured on an 11th-gen Core i5 with Qwen3-8B Q4_K_M, four threads, the same
  prompt of 1,341 tokens, four runs each: Aqua's build reads 33.9 tokens a second and writes 4.75; the official build reads 41.4 and
  writes 4.84. Writing is limited by memory, not by the code, so only reading gains.

  Installed in data\engines\llama, it is used for the model server in place of the vendored one. It is also what Aqua falls back to
  when the graphics card cannot be used (see fetch_llama_gpu.ps1). The zip is the official release from github.com/ggml-org/llama.cpp,
  pinned by SHA-256 (19 MB, 48 MB unpacked). Undo with -Disable: it deletes the folder and Aqua goes back to its own build.

.PARAMETER Disable
  Delete the official build. Aqua uses its own build again.

.EXAMPLE
  powershell -File scripts\fetch_llama_cpu.ps1
  powershell -File scripts\fetch_llama_cpu.ps1 -Disable
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
$OutDir = Join-Path $Root "data/engines/llama"

if ($Disable) {
    Stop-EngineHolding $OutDir
    if (Test-Path -LiteralPath $OutDir) { Remove-Item -LiteralPath $OutDir -Recurse -Force; Write-Host "Removed $OutDir. Aqua uses its own build." }
    else { Write-Host "Nothing to remove: $OutDir does not exist." }
    return
}

$Build = "b11371"
$Url = "https://github.com/ggml-org/llama.cpp/releases/download/$Build/llama-$Build-bin-win-cpu-x64.zip"
# Pinned SHA-256 of the zip as downloaded from the address above (it is also the digest GitHub shows for the release asset).
$Sha256 = "085d650f1b4b0725a32c2421b970da5c5d2f769202cd16f89591ac9dcce835c6"

$Zip = Join-Path ([System.IO.Path]::GetTempPath()) "llama-$Build-cpu.zip"
Get-VerifiedFile -Url $Url -OutFile $Zip -Sha256 $Sha256

Stop-EngineHolding $OutDir
if (Test-Path -LiteralPath $OutDir) { Remove-Item -LiteralPath $OutDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
Expand-Archive -LiteralPath $Zip -DestinationPath $OutDir -Force
Remove-Item -LiteralPath $Zip -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath "$Zip.part","$Zip.mismatch" -Force -ErrorAction SilentlyContinue
$VendorBuild = Join-Path $Root "vendor/llama.cpp/build"
if (Test-Path -LiteralPath $VendorBuild) {
    Remove-Item -LiteralPath $VendorBuild -Recurse -Force
    Write-Host "Removed the old compiled tree: $VendorBuild"
}

if (-not (Test-Path -LiteralPath (Join-Path $OutDir "llama-server.exe"))) { throw "llama-server.exe is not in the unpacked folder: $OutDir" }
Write-Host "Installed to $OutDir. The next model server Aqua starts on the processor is this build."
