<#
.SYNOPSIS
  Builds the self-contained Kokoro text-to-speech folder that the installer carries:
  apps\aqua-desktop\src-tauri\resources\engines\kokoro\  (python\ + kokoro_daemon.py + kokoro_tts\)

.DESCRIPTION
  The development setup uses a virtual environment (vendor\kokoro-tts\.venv) that points at the Python installed on
  the developer's machine, so it cannot be copied to another PC. This script instead takes the official embeddable
  Python 3.12, installs only what the speech daemon needs into it, and smoke-tests it on a spare port.
  The voice model (kokoro-v1.0.onnx, voices-v1.0.bin) is NOT included: it lives in the models pack
  (see scripts\package_models_pack.ps1) and the core tells the daemon where it is.

.PARAMETER Out
  Output folder (default: the installer's resources\engines\kokoro).
.PARAMETER PythonVersion
  Embeddable Python version (default 3.12.10, the one the development environment uses).
#>
param(
    [string]$Out = "",
    [string]$PythonVersion = "3.12.10"
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if (-not $Out) { $Out = Join-Path $root "apps\aqua-desktop\src-tauri\resources\engines\kokoro" }
$vendor = Join-Path $root "vendor\kokoro-tts"
$cache = Join-Path $env:TEMP "aqua-kokoro-pack"
New-Item -ItemType Directory -Force $cache | Out-Null

if (Test-Path $Out) { [System.IO.Directory]::Delete($Out, $true) }
$py = Join-Path $Out "python"
New-Item -ItemType Directory -Force $py | Out-Null

# 1. The embeddable Python.
$zip = Join-Path $cache "python-$PythonVersion-embed-amd64.zip"
if (-not (Test-Path $zip)) {
    Write-Host "Downloading Python $PythonVersion (embeddable)..."
    Invoke-WebRequest "https://www.python.org/ftp/python/$PythonVersion/python-$PythonVersion-embed-amd64.zip" -OutFile $zip
}
Expand-Archive $zip -DestinationPath $py -Force
$tag = ($PythonVersion -split '\.')[0..1] -join ''
# Search path: the standard library zip, the folder itself, the packages; and let `site` run so .pth files work.
Set-Content -Encoding ascii (Join-Path $py "python$tag._pth") "python$tag.zip`r`n.`r`nLib\site-packages`r`nimport site"
New-Item -ItemType Directory -Force (Join-Path $py "Lib\site-packages") | Out-Null

# 2. pip (only to install; removed again below).
$getpip = Join-Path $cache "get-pip.py"
if (-not (Test-Path $getpip)) { Invoke-WebRequest "https://bootstrap.pypa.io/get-pip.py" -OutFile $getpip }
$pyexe = Join-Path $py "python.exe"
& $pyexe $getpip --no-warn-script-location | Out-Host
if ($LASTEXITCODE -ne 0) { throw "get-pip failed" }

# 3. What the daemon imports: kokoro-onnx (-> librosa, numba, phonemizer, espeak), onnxruntime, numpy, soundfile.
& $pyexe -m pip install --no-warn-script-location --no-cache-dir kokoro-onnx==0.3.9 onnxruntime soundfile numpy | Out-Host
if ($LASTEXITCODE -ne 0) { throw "pip install failed" }

# 4. Not needed at run time.
foreach ($d in @("pip", "setuptools", "wheel", "_distutils_hack", "pkg_resources")) {
    $p = Join-Path $py "Lib\site-packages\$d"
    if (Test-Path $p) { [System.IO.Directory]::Delete($p, $true) }
}
Get-ChildItem (Join-Path $py "Scripts") -ErrorAction SilentlyContinue | ForEach-Object { [System.IO.File]::Delete($_.FullName) }
Get-ChildItem (Join-Path $py "Lib\site-packages") -Directory -Recurse -Filter "tests" -ErrorAction SilentlyContinue | ForEach-Object { try { [System.IO.Directory]::Delete($_.FullName, $true) } catch {} }
Get-ChildItem $py -Directory -Recurse -Filter "__pycache__" -ErrorAction SilentlyContinue | ForEach-Object { try { [System.IO.Directory]::Delete($_.FullName, $true) } catch {} }

# 5. The Visual C++ runtime onnxruntime and numpy need, beside python.exe, in case the PC lacks it.
foreach ($dll in @("msvcp140.dll", "msvcp140_1.dll", "vcruntime140.dll", "vcruntime140_1.dll", "concrt140.dll")) {
    $src = Join-Path $env:SystemRoot "System32\$dll"
    if ((Test-Path $src) -and -not (Test-Path (Join-Path $py $dll))) { Copy-Item $src $py }
}

# 6. The daemon and the command-line speaker.
Copy-Item (Join-Path $vendor "kokoro_daemon.py") $Out
$tts = Join-Path $vendor "kokoro_tts"
if (Test-Path $tts) { Copy-Item $tts (Join-Path $Out "kokoro_tts") -Recurse -Force }
Get-ChildItem $Out -Directory -Recurse -Filter "__pycache__" | ForEach-Object { try { [System.IO.Directory]::Delete($_.FullName, $true) } catch {} }

# 7. Smoke test: the daemon starts from this folder alone, loads the model and answers /health on a spare port.
$models = Join-Path $root "data\models\kokoro"
if (Test-Path (Join-Path $models "kokoro-v1.0.onnx")) {
    $env:AQUA_KOKORO_PORT = "18766"
    $env:AQUA_KOKORO_MODELS = $models
    $env:AQUA_KOKORO_PROVIDER = "cpu"
    $proc = Start-Process $pyexe -ArgumentList @((Join-Path $Out "kokoro_daemon.py")) -WorkingDirectory $Out -PassThru -WindowStyle Hidden
    $ok = $false
    for ($i = 0; $i -lt 90 -and -not $ok; $i++) {
        Start-Sleep -Milliseconds 1000
        try { $ok = (Invoke-WebRequest "http://127.0.0.1:18766/health" -UseBasicParsing -TimeoutSec 2).StatusCode -eq 200 } catch {}
        if ($proc.HasExited) { break }
    }
    if ($ok) {
        $body = '{"text":"Hello from the installer test.","voice":"af_heart","lang":"en-us"}'
        try {
            $r = Invoke-WebRequest "http://127.0.0.1:18766/synthesize" -Method Post -Body $body -ContentType "application/json" -UseBasicParsing -TimeoutSec 60
            Write-Host ("Smoke test: spoke {0} bytes of audio" -f $r.RawContentLength)
        } catch { Write-Host "Smoke test: /health ok, but synthesize failed: $($_.Exception.Message)" }
    }
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
    Remove-Item Env:AQUA_KOKORO_PORT, Env:AQUA_KOKORO_MODELS, Env:AQUA_KOKORO_PROVIDER -ErrorAction SilentlyContinue
    if (-not $ok) { throw "The packaged Kokoro daemon did not become healthy" }
} else {
    Write-Warning "No model in ${models}: skipped the smoke test."
}

$size = (Get-ChildItem $Out -Recurse -File | Measure-Object Length -Sum).Sum / 1MB
Write-Host ("Kokoro folder ready: {0} ({1:n0} MB)" -f $Out, $size)
