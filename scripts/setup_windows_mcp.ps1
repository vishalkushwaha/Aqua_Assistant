# Setup Windows-MCP for Aqua (llama.cpp agent → desktop UI)
#
# Upstream: https://github.com/CursorTouch/Windows-MCP
# Runs via PyPI: uvx windows-mcp serve  (no git clone required)

$ErrorActionPreference = "Stop"

Write-Host "==> Ensuring UV is on PATH..."
$uvDir = Join-Path $env:USERPROFILE ".local\bin"
if (Test-Path (Join-Path $uvDir "uv.exe")) {
    $env:Path = "$uvDir;$env:Path"
}

$uv = Get-Command uv -ErrorAction SilentlyContinue
if (-not $uv) {
    # Do not pipe a downloaded script into a shell. Install uv yourself, then re-run this script:
    #   winget install --id astral-sh.uv -e
    throw "uv is not installed. Run: winget install --id astral-sh.uv -e   (or see https://docs.astral.sh/uv/) and re-run."
}
Write-Host "    uv: $($uv.Source)"

# Aqua runs a pinned release, offline, with only the desktop UI tools enabled.
$pinned = "0.8.5"
Write-Host "==> Prefetching windows-mcp==$pinned from PyPI..."
& uvx --from "windows-mcp==$pinned" windows-mcp serve --help | Out-Null
Write-Host "    windows-mcp $pinned cached. Aqua launches it with --offline and --tools (UI tools only)."

Write-Host ""
Write-Host "Next steps:"
Write-Host "  1. Restart aqua-core"
Write-Host "  2. POST http://127.0.0.1:8767/api/v1/windows-mcp/start"
Write-Host "  3. Or open Browser MCP Agent tab and use Unified agent (Browser + Windows)"
Write-Host "  4. Prefer English Windows UI language for best automation results"
