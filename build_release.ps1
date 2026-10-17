# Builds the release core, copies it in as the desktop app's sidecar, and builds the installer.
# Every step must succeed: a failed build must never be followed by packaging an older core.
#
#   .\build_release.ps1            the full build: every installer format, signed for the in-app updater
#   .\build_release.ps1 -Tester    one NSIS installer (.exe) for handing to someone to try: no signing key needed, no updater files.
#                                  Models are not inside it; build them with scripts\package_models_pack.ps1 (see documentation/TESTER_GUIDE.md).
param([switch]$Tester)
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
Set-Location $root

function Step([string]$what, [scriptblock]$run) {
    Write-Host $what
    & $run
    if ($LASTEXITCODE -ne 0) { throw "$what failed (exit code $LASTEXITCODE)" }
}

$core = Join-Path $root 'target\release\aqua-core.exe'
if (-not (Test-Path $core)) {
    Step 'Building Aqua Core (backend) in release mode...' { cargo build --release -p aqua-core }
} else {
    Write-Host "Using verified release core: $core"
}
$BinDir = Join-Path $root 'apps\aqua-desktop\src-tauri\bin'
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

# Tauri looks for the sidecar named with the Rust host target (this machine may use gnu, not msvc).
$triple = ((rustc -vV | Select-String '^host:').ToString() -split '\s+')[1]
if (-not $triple) { throw 'Could not read the Rust host target from rustc -vV' }
# The tauri CLI names the sidecar it wants with ITS target triple, which is msvc here even though rustc's host is gnu, so the
# core goes in under both names (the installer once shipped a stale core under the other name).
$triples = @($triple, 'x86_64-pc-windows-msvc') | Select-Object -Unique
$sidecars = @{}
# Kept on the repository's drive: the system drive may be short of space and NSIS needs it for its temporary files.
$devKeep = Join-Path $root 'dist\_dev_sidecars'
New-Item -ItemType Directory -Force -Path $devKeep | Out-Null
foreach ($t in $triples) {
    $sc = Join-Path $BinDir "aqua-core-$t.exe"
    # `tauri dev` runs whatever sits here, so keep the development core and put it back when the installer is built.
    $keep = $null
    if ((Test-Path $sc) -and ((Get-Item $sc).Length -ne (Get-Item $core).Length)) { $keep = Join-Path $devKeep "aqua-dev-sidecar-$t.exe"; Copy-Item $sc $keep -Force }
    $sidecars[$sc] = $keep
    Write-Host "Copying the core in as the desktop sidecar ($t)..."
    Copy-Item $core -Destination $sc -Force
}

# The helper programs are found next to the installed app (see adapters/audio_adapter/src/engines.rs): copy them
# into engines\ beside the sidecar when they have been built. Missing ones are reported, not hidden.
$engines = Join-Path $root 'apps\aqua-desktop\src-tauri\resources\engines'
# tauri refuses a missing resource folder, so it always exists (empty if nothing was built to put in it).
New-Item -ItemType Directory -Force -Path $engines | Out-Null
$wanted = @(
    @{ From = 'vendor\whisper.cpp\build\bin\whisper-cli.exe'; To = 'whisper\whisper-cli.exe' },
    @{ From = 'vendor\deepfilternet_rust\target\release\deep-filter.exe'; To = 'deep-filter\deep-filter.exe' },
    @{ From = 'vendor\llama.cpp\build\bin\llama-cli.exe'; To = 'llama\llama-cli.exe' },
    @{ From = 'vendor\llama.cpp\build\bin\llama-server.exe'; To = 'llama\llama-server.exe' }
)
foreach ($e in $wanted) {
    $src = Join-Path $root $e.From
    if (Test-Path $src) {
        $dest = Join-Path $engines $e.To
        New-Item -ItemType Directory -Force -Path (Split-Path $dest) | Out-Null
        Copy-Item $src -Destination $dest -Force
        # the program's own DLLs travel with it
        Get-ChildItem (Split-Path $src) -Filter '*.dll' -ErrorAction SilentlyContinue | Copy-Item -Destination (Split-Path $dest) -Force
    } else {
        Write-Warning "Not bundled (not built): $($e.From)"
    }
}

# The scripts that download the models after installing (installer-hooks.nsh runs install_models.ps1 from here).
$setup = Join-Path $root 'apps\aqua-desktop\src-tauri\resources\setup'
New-Item -ItemType Directory -Force -Path $setup | Out-Null
foreach ($f in @('install_models.ps1', '_verified_download.ps1', 'fetch_deepfilternet3_onnx.ps1', 'fetch_speaker_model.ps1', 'fetch_whisper_vad.ps1', 'fetch_kokoro_v1.ps1', 'fetch_whisper_base_en.ps1', 'fetch_whisper_small_en.ps1', 'fetch_llama_qwen_4b.ps1', 'fetch_llama_qwen3_8b.ps1', 'fetch_llama_gpu.ps1', 'enable_kokoro_gpu.ps1', 'enable_gpu.ps1')) {
    Copy-Item (Join-Path $root "scripts\$f") -Destination $setup -Force
}

# Kokoro (speech) travels as its own Python plus the daemon: the development virtual environment points at this machine's Python.
if (-not (Test-Path (Join-Path $engines 'kokoro\python\python.exe'))) {
    Write-Host 'Packaging Kokoro (portable Python)...'
    & (Join-Path $root 'scripts\package_kokoro_portable.ps1')
}

Set-Location (Join-Path $root 'apps\aqua-desktop')
if ($Tester) {
    try {
        Step 'Building the tester installer (NSIS)...' { npx tauri build --bundles nsis --config src-tauri\tauri.tester.conf.json }
    } finally {
        foreach ($sc in $sidecars.Keys) { if ($sidecars[$sc] -and (Test-Path $sidecars[$sc])) { Copy-Item $sidecars[$sc] $sc -Force } }
        Write-Host 'Restored the development cores as the sidecars.'
    }
    Write-Host 'Installer: apps\aqua-desktop\src-tauri\target\release\bundle\nsis'
    Write-Host 'Done.'
    return
}
# The installer is signed for the in-app updater (the app only installs updates signed with this key). The private key stays
# on this computer, outside the repository: TAURI_SIGNING_PRIVATE_KEY_PATH, or ~\.aqua-signing\aqua-updater.key. It is only
# ever handed to the build as a path, never printed or put in a file in the repository. See documentation/UPDATES.md.
if (-not $env:TAURI_SIGNING_PRIVATE_KEY -and -not $env:TAURI_SIGNING_PRIVATE_KEY_PATH) {
    $defaultKey = Join-Path $HOME '.aqua-signing\aqua-updater.key'
    if (-not (Test-Path $defaultKey)) { throw "No updater signing key found. Set TAURI_SIGNING_PRIVATE_KEY_PATH or create $defaultKey (see documentation/UPDATES.md)." }
    $env:TAURI_SIGNING_PRIVATE_KEY_PATH = $defaultKey
}
Step 'Building the Tauri installer...' { npx tauri build }
Write-Host 'Update files (installer + .sig) are next to the installers under src-tauri\target\release\bundle. To publish: scripts\make_update_manifest.ps1'
Write-Host 'Done.'
