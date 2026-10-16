# Set up Aqua's managed browser: pinned Playwright MCP driving your installed Edge/Chrome in an
# Aqua-only profile. Idempotent. See documentation/BROWSER.md.
# Usage: ./scripts/setup-managed-browser.ps1 [-Json]

param([switch]$Json)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$Tools = Join-Path $Root "tools\browser-mcp"
$Cli = Join-Path $Tools "node_modules\@playwright\mcp\cli.js"
$Data = Join-Path $Root "data\browser"
$Policy = Join-Path $Data "policy.json"
$DefaultPolicy = Join-Path $Data "policy.default.json"

function Finish($ok, $detail, $extra) {
    $result = @{ ok = $ok; detail = $detail } + $extra
    if ($Json) { $result | ConvertTo-Json -Compress -Depth 5 } else { Write-Host ($result | ConvertTo-Json -Depth 5) }
    if ($ok) { exit 0 } else { exit 1 }
}

$node = Get-Command node -ErrorAction SilentlyContinue
if (-not $node) {
    Finish $false "Node.js is not installed. Install Node.js LTS from https://nodejs.org and re-run." @{ stage = "install" }
}

Write-Host "==> Installing pinned Playwright MCP (npm ci)..."
Push-Location $Tools
try {
    npm ci --no-audit --no-fund --loglevel=error
    if ($LASTEXITCODE -ne 0) { throw "npm ci failed with exit code $LASTEXITCODE" }
} finally {
    Pop-Location
}
if (-not (Test-Path $Cli)) { Finish $false "Playwright MCP CLI missing after install: $Cli" @{ stage = "install" } }
$version = (& node $Cli --version) -join " "

Write-Host "==> Preparing $Data ..."
foreach ($d in @("profile", "artifacts", "uploads")) {
    New-Item -ItemType Directory -Force -Path (Join-Path $Data $d) | Out-Null
}
if (-not (Test-Path $Policy) -and (Test-Path $DefaultPolicy)) {
    Copy-Item $DefaultPolicy $Policy
    Write-Host "    Created policy.json from policy.default.json"
}

$edge = @(
    "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe",
    "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
$chrome = @(
    "$env:ProgramFiles\Google\Chrome\Application\chrome.exe",
    "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe",
    "$env:LOCALAPPDATA\Google\Chrome\Application\chrome.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1

if (-not $edge -and -not $chrome) {
    Finish $false "Neither Microsoft Edge nor Google Chrome was found. Install one of them." @{ stage = "browser"; playwright_mcp = $version }
}

Finish $true "Managed browser ready. It opens on the agent's first browser task." @{
    playwright_mcp = $version
    edge = [bool]$edge
    chrome = [bool]$chrome
    policy = $Policy
    uploads_folder = (Join-Path $Data "uploads")
}
