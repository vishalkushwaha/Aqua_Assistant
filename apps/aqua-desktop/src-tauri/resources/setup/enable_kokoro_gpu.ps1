<#
.SYNOPSIS
  Run Kokoro text to speech on an NVIDIA GPU (about 4x real time on a GTX 1650, against about 1x on the CPU).

.DESCRIPTION
  Installs onnxruntime-gpu and the CUDA 12.6 libraries it needs (about 2.5 GB) into their own folder,
  %LOCALAPPDATA%\Aqua\kokoro-gpu, without touching the Kokoro virtual environment. The Kokoro daemon
  (vendor\kokoro-tts\kokoro_daemon.py) finds that folder by itself, tries the GPU, measures it on a test sentence, and
  uses it only if it really is faster; otherwise it keeps using the CPU. Nothing else needs configuring.

  Needs: an NVIDIA GPU, a driver from the CUDA 12.6 era or newer (560 or higher), and about 5 GB free on the drive that
  holds %LOCALAPPDATA% (the download is unpacked twice while it installs). The libraries are pinned to 12.6 because
  newer ones make the convolution layers fail to start on that driver. pip's download cache is not used, so nothing
  extra is left behind.

  Undo with -Disable: it deletes the folder, and Kokoro goes back to the CPU.

.PARAMETER Disable
  Delete the GPU folder. Kokoro goes back to the CPU.

.PARAMETER Force
  Install even if the driver looks too old or the free space looks too small.

.EXAMPLE
  powershell -File scripts\enable_kokoro_gpu.ps1
  powershell -File scripts\enable_kokoro_gpu.ps1 -Disable
#>
param(
    [switch]$Disable,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$root   = Split-Path -Parent $PSScriptRoot
$venv   = Join-Path $root 'vendor\kokoro-tts\.venv'
$py     = Join-Path $venv 'Scripts\python.exe'
if (-not (Test-Path $py)) {
    $candidatePy = @(
        (Join-Path $root 'engines\kokoro\python\python.exe'),
        (Join-Path $root 'resources\engines\kokoro\python\python.exe'),
        (Join-Path $root 'apps\aqua-desktop\src-tauri\resources\engines\kokoro\python\python.exe'),
        (Join-Path $env:LOCALAPPDATA 'Aqua\engines\kokoro\python\python.exe')
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if ($candidatePy) { $py = $candidatePy }
}
$target = if ($env:AQUA_KOKORO_GPU_LIBS) { $env:AQUA_KOKORO_GPU_LIBS } else { Join-Path $env:LOCALAPPDATA 'Aqua\kokoro-gpu' }

# The tested set, installed without dependencies: onnxruntime-gpu needs nothing the virtual environment does not have.
$gpuPackages = @(
    'onnxruntime-gpu==1.22.0',
    'nvidia-cuda-runtime-cu12==12.6.77',
    'nvidia-cuda-nvrtc-cu12==12.6.77',
    'nvidia-cublas-cu12==12.6.4.1',
    'nvidia-cufft-cu12==11.3.0.4',
    'nvidia-curand-cu12==10.3.7.77',
    'nvidia-cudnn-cu12==9.5.1.17',
    'nvidia-nvjitlink-cu12==12.9.86'
)

function Info($t) { Write-Host "  $t" -ForegroundColor DarkGray }
function Ok($t)   { Write-Host "OK   $t" -ForegroundColor Green }
function Fail($t) { Write-Host "FAIL $t" -ForegroundColor Red; exit 1 }

# pip writes progress to stderr, which PowerShell treats as an error, so it runs through cmd
function Pip([string]$pipArgs) {
    $out = cmd.exe /c "`"$py`" -m pip $pipArgs 2>&1"
    if ($LASTEXITCODE -ne 0) { $out | Select-Object -Last 12 | ForEach-Object { Info $_ }; Fail "pip $pipArgs failed." }
}

# The Kokoro daemon holds files in the folder, so it is stopped first. The core starts it again on its own.
function Stop-KokoroDaemon {
    foreach ($c in (Get-NetTCPConnection -LocalPort 8766 -State Listen -ErrorAction SilentlyContinue)) {
        $p = Get-Process -Id $c.OwningProcess -ErrorAction SilentlyContinue
        if ($p -and $p.Path -like '*python*') { Info "stopping the Kokoro daemon (pid $($p.Id))"; Stop-Process -Id $p.Id -Force; Start-Sleep -Seconds 2 }
    }
}

if (-not (Test-Path $py)) { Fail "The Kokoro environment was not found: $py`n     Run scripts\setup_kokoro_cli.ps1 first." }

if ($Disable) {
    Write-Host 'Turning the GPU off for Kokoro' -ForegroundColor Cyan
    Stop-KokoroDaemon
    if (Test-Path $target) { Remove-Item $target -Recurse -Force; Ok "removed $target. Kokoro will use the CPU." } else { Ok 'the GPU folder was not there; Kokoro already uses the CPU.' }
    exit 0
}

Write-Host 'Enabling Kokoro on the GPU' -ForegroundColor Cyan

# 1. a GPU and a driver that can run CUDA 12.6
if (-not (Get-Command nvidia-smi -ErrorAction SilentlyContinue)) { Fail 'nvidia-smi was not found: there is no NVIDIA driver here, so there is no GPU to use. Kokoro keeps running on the CPU.' }
$gpu = (& nvidia-smi --query-gpu=name,driver_version,memory.total --format=csv,noheader 2>$null | Select-Object -First 1)
if (-not $gpu) { Fail 'nvidia-smi did not report a GPU.' }
$name, $driver, $mem = $gpu -split ',\s*'
Info "GPU: $name, driver $driver, $mem"
if ([version]$driver -lt [version]'560.0' -and -not $Force) {
    Fail "The driver ($driver) is older than 560, which the CUDA 12.6 libraries need. Update the NVIDIA driver, or use -Force to try anyway."
}
Ok 'NVIDIA GPU found'

# 2. enough room: the wheels are unpacked in the temp folder and again in the target
$drives = @($target, $env:TEMP) | ForEach-Object { (Get-Item (Split-Path $_ -Qualifier)).PSDrive } | Select-Object -Unique
foreach ($d in $drives) {
    $free = [math]::Round($d.Free / 1GB, 1)
    Info "free space on $($d.Name): $free GB"
    if ($free -lt 5 -and -not $Force) { Fail "Only $free GB is free on $($d.Name):. The install needs about 5 GB while it runs (about 2.5 GB stay). Free some space, or use -Force." }
}

# 3. install into the separate folder, with no pip cache
Stop-KokoroDaemon
if (Test-Path $target) { Remove-Item $target -Recurse -Force }
New-Item -ItemType Directory -Force $target | Out-Null
Info "installing onnxruntime-gpu and the CUDA 12.6 libraries into $target (about 2.5 GB; this takes a few minutes) ..."
try {
    Pip ("install --disable-pip-version-check --no-cache-dir --no-deps --target `"$target`" " + ($gpuPackages -join ' '))
} catch { }
if (-not (Test-Path (Join-Path $target 'onnxruntime'))) { if (Test-Path $target) { Remove-Item $target -Recurse -Force }; Fail 'The install did not finish; the GPU folder was removed again.' }
Ok 'packages installed'

# 4. prove it: load the model on the GPU the way the daemon does, and time one sentence
$check = @'
import sys, os, time
sys.path.insert(0, os.environ['AQUA_GPU_DIR'])
import onnxruntime as ort
ort.preload_dlls()
if 'CUDAExecutionProvider' not in ort.get_available_providers():
    print('NO_CUDA_PROVIDER'); sys.exit(2)
from kokoro_onnx import Kokoro
root = os.environ['AQUA_ROOT']
m = os.path.join(root, 'data', 'models', 'kokoro', 'kokoro-v1.0.onnx'); v = os.path.join(root, 'data', 'models', 'kokoro', 'voices-v1.0.bin')
sess = ort.InferenceSession(m, providers=[('CUDAExecutionProvider', {'device_id': 0}), 'CPUExecutionProvider'])
if sess.get_providers()[0] != 'CUDAExecutionProvider':
    print('CUDA_NOT_USED'); sys.exit(3)
k = Kokoro.from_session(sess, v)
text = 'The weather today is sunny with a light breeze, so it is a good day to go outside.'
k.create('Ready.', voice='af_sarah', speed=1.0, lang='en-us')
t = time.perf_counter(); s, sr = k.create(text, voice='af_sarah', speed=1.0, lang='en-us'); dt = time.perf_counter() - t
print('SPEED %.1f' % ((len(s) / sr) / dt))
'@
$env:AQUA_ROOT = $root
$env:AQUA_GPU_DIR = $target
$tmp = Join-Path $env:TEMP 'aqua_kokoro_gpu_check.py'
Set-Content -Path $tmp -Value $check -Encoding ASCII
$res = cmd.exe /c "`"$py`" `"$tmp`" 2>nul" | Select-String 'SPEED|NO_CUDA|CUDA_NOT_USED' | Select-Object -Last 1
Remove-Item $tmp -ErrorAction SilentlyContinue
if ($res -and $res.Line -like 'SPEED*') {
    $x = [double]($res.Line -replace 'SPEED\s+', '')
    Ok ("Kokoro runs on the GPU at {0:n1}x real time. Restart Aqua to use it." -f $x)
    if ($x -lt 2.0) { Write-Host '     That is slower than expected. The daemon will only use the GPU if it reaches 2.0x on its own test.' -ForegroundColor Yellow }
} else {
    $why = if ($res) { $res.Line } else { 'the check did not run' }
    Write-Host "The GPU could not be used ($why). Kokoro will keep using the CPU. The folder was left in place; remove it with: scripts\enable_kokoro_gpu.ps1 -Disable" -ForegroundColor Yellow
    exit 2
}
