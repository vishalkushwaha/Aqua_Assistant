# Install kokoro-tts CLI into vendor/kokoro-tts/.venv (Python 3.11–3.12 per upstream README)
$ErrorActionPreference = "Stop"
$Vendor = Join-Path (Split-Path -Parent $PSScriptRoot) "vendor/kokoro-tts"
if (-not (Test-Path $Vendor)) {
    throw "Missing vendor/kokoro-tts submodule. See documentation/VENDOR_SUBMODULES.md"
}

function Test-Python312 {
    param([string]$Exe)
    & $Exe -c "import sys; sys.exit(0 if (3,11) <= sys.version_info[:2] <= (3,12) else 1)" 2>$null
    return $LASTEXITCODE -eq 0
}

$pythonExe = $null
$tryList = @("python", "py", "python3.12", "python3.11")
foreach ($t in $tryList) {
    if (Get-Command $t -ErrorAction SilentlyContinue) {
        if (Test-Python312 -Exe $t) {
            $pythonExe = $t
            break
        }
    }
}
if (-not $pythonExe) {
    throw "Python 3.11 or 3.12 required (see vendor/kokoro-tts/README.md)"
}

Push-Location $Vendor
try {
    Write-Host "Using Python: $pythonExe"
    if (-not (Test-Path ".venv")) {
        & $pythonExe -m venv .venv
    }
    $pip = Join-Path $Vendor ".venv/Scripts/pip.exe"
    # Exactly the versions that were tested (scripts/kokoro_requirements.txt), not whatever is newest on the day;
    # then the project itself without resolving or fetching anything further.
    $lock = Join-Path $PSScriptRoot "kokoro_requirements.txt"
    & $pip install -r $lock
    if ($LASTEXITCODE -ne 0) { throw "installing the pinned packages failed (exit $LASTEXITCODE)" }
    & $pip install -e . --no-deps
    if ($LASTEXITCODE -ne 0) { throw "installing kokoro-tts failed (exit $LASTEXITCODE)" }
    Write-Host "kokoro-tts installed: $Vendor/.venv/Scripts/kokoro-tts.exe"
} finally {
    Pop-Location
}
