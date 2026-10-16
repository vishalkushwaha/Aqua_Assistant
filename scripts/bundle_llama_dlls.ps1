<#
  Copy the MinGW runtime DLLs the llama.cpp programs were built with next to them.

  llama-cli.exe and llama-server.exe need libstdc++-6, libgcc_s_seh-1, libwinpthread-1 and
  libgomp-1. Without a copy beside the program, Windows takes whichever set comes first on PATH,
  and a different (older) MinGW earlier on PATH (Git for Windows' shell has one) makes the program
  exit at once with 0xC0000139. The program's own folder is always searched first, so a copy there
  makes it start the same way from any shell, service or scheduled task.

  It works for any of the vendored CMake/MinGW builds (whisper-cli needs the same runtime):

  Usage:  powershell -File scripts\bundle_llama_dlls.ps1                     # llama.cpp (default)
          powershell -File scripts\bundle_llama_dlls.ps1 -Vendor whisper.cpp
#>
param([string]$Vendor = "llama.cpp")
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$build = Join-Path $root "vendor/$Vendor/build"
$bin = Join-Path $build "bin"
if (-not (Test-Path $bin)) { throw "$Vendor is not built yet ($bin not found). Run its build script first." }

# The compiler that built it is the one whose runtime it needs.
$cache = Join-Path $build "CMakeCache.txt"
$compilerDir = $null
if (Test-Path $cache) {
    $line = Select-String -Path $cache -Pattern "^CMAKE_CXX_COMPILER:FILEPATH=(.+)$" | Select-Object -First 1
    if ($line) { $compilerDir = Split-Path -Parent $line.Matches[0].Groups[1].Value }
}
if (-not $compilerDir -or -not (Test-Path (Join-Path $compilerDir "libstdc++-6.dll"))) {
    $found = Get-Command g++.exe -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($found) { $compilerDir = Split-Path -Parent $found.Source }
}
if (-not $compilerDir) { throw "could not find the MinGW compiler folder that built llama.cpp" }

$copied = @()
foreach ($dll in "libstdc++-6.dll", "libgcc_s_seh-1.dll", "libwinpthread-1.dll", "libgomp-1.dll") {
    $src = Join-Path $compilerDir $dll
    if (Test-Path $src) {
        Copy-Item $src -Destination $bin -Force
        $copied += $dll
    }
}
Write-Host "Copied $($copied.Count) runtime DLL(s) from $compilerDir to $bin : $($copied -join ', ')"
