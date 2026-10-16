# Official Kokoro v1.0 release assets (nazdridoy/kokoro-tts README)
$ErrorActionPreference = "Stop"
# An installed copy keeps its data in %LOCALAPPDATA%\Aqua: run with AQUA_WORKSPACE_ROOT set to that folder.
$Root = if ($env:AQUA_WORKSPACE_ROOT) { $env:AQUA_WORKSPACE_ROOT } else { Split-Path -Parent $PSScriptRoot }
. (Join-Path $PSScriptRoot "_verified_download.ps1")
$OutDir = Join-Path $Root "data/models/kokoro"
$Base = "https://github.com/nazdridoy/kokoro-tts/releases/download/v1.0.0"
# Pinned SHA-256 of the copies this install was tested with (the release publishes no checksum to compare to).
$Files = @(
    @{ Name = "kokoro-v1.0.onnx"; Sha256 = "7d5df8ecf7d4b1878015a32686053fd0eebe2bc377234608764cc0ef3636a6c5" },
    @{ Name = "voices-v1.0.bin";  Sha256 = "d19762d46cf0e6648cb28a7711df1637aad15818185d13f4ff840d57f2f6dfed" }
)
foreach ($f in $Files) {
    Get-VerifiedFile -Url "$Base/$($f.Name)" -OutFile (Join-Path $OutDir $f.Name) -Sha256 $f.Sha256
}
