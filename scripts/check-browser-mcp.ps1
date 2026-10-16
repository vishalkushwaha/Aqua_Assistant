# Check Browser MCP prerequisites (install/config). Live connect/action probes run in aqua-core.
# Usage: ./scripts/check-browser-mcp.ps1 [-Json]

param([switch]$Json)

$ErrorActionPreference = "Continue"
$Package = "@browsermcp/mcp@0.1.3"   # pinned like the runtime and the setup script: never what `npx` finds newest
$ExtensionUrl = "https://docs.browsermcp.io/setup-extension"
$Root = Split-Path -Parent $PSScriptRoot
$StateFile = Join-Path $Root "data\browser_mcp\state.json"
$CoreHealth = "http://127.0.0.1:8767/api/v1/browser-mcp/health"
$TokenFile = Join-Path $Root "data\.aqua_api_token"
$ApiToken = if ($env:AQUA_API_TOKEN) { $env:AQUA_API_TOKEN } elseif (Test-Path $TokenFile) { (Get-Content $TokenFile -Raw).Trim() } else { "" }
$AuthHeaders = @{ Authorization = "Bearer $ApiToken" }

$node = Get-Command node -ErrorAction SilentlyContinue
$npx = Get-Command npx -ErrorAction SilentlyContinue
$nodeOk = [bool]$node
$npxOk = [bool]$npx
$configured = $false
if (Test-Path $StateFile) {
    try {
        $st = Get-Content $StateFile -Raw | ConvertFrom-Json
        $configured = [bool]$st.setup_ok
    } catch { $configured = $false }
}

$serverRunning = $null
$browserConnected = $null
$browserAction = $null
$ready = $false
$coreDetail = $null

try {
    $resp = Invoke-WebRequest -Uri $CoreHealth -Headers $AuthHeaders -UseBasicParsing -TimeoutSec 3
    if ($resp.StatusCode -eq 200) {
        $h = $resp.Content | ConvertFrom-Json
        $serverRunning = [bool]$h.server_running
        $browserConnected = [bool]$h.browser_connected
        $browserAction = [bool]$h.browser_action_available
        $ready = [bool]$h.ready
        $coreDetail = $h.detail
        if ($null -ne $h.configured) { $configured = [bool]$h.configured }
        if ($null -ne $h.installed) { $installedCore = [bool]$h.installed } else { $installedCore = $nodeOk -and $npxOk }
    }
} catch {
    $installedCore = $nodeOk -and $npxOk
}

$installed = $nodeOk -and $npxOk
if (-not $installed) { $stage = "install" }
elseif (-not $configured) { $stage = "configure" }
elseif ($serverRunning -eq $false) { $stage = "server_start" }
elseif ($browserConnected -eq $false) { $stage = "browser_connect" }
elseif ($browserAction -eq $false) { $stage = "action_probe" }
elseif ($ready) { $stage = "none" }
else { $stage = "configure" }

$result = @{
    ok = $ready
    installed = $installed
    configured = $configured
    node_available = $nodeOk
    npx_available = $npxOk
    server_running = $serverRunning
    browser_connected = $browserConnected
    browser_action_available = $browserAction
    ready = $ready
    package = $Package
    extension_url = $ExtensionUrl
    state_file = $StateFile
    stage = $stage
    detail = $(if ($coreDetail) { $coreDetail } elseif ($installed) { "Prerequisites OK; start aqua-core and Connect extension for full ready." } else { "Node/npx missing." })
    user_action_required = $(if (-not $installed) {
        "Install Node.js LTS from https://nodejs.org"
    } elseif ($browserConnected -eq $false -and $serverRunning -eq $true) {
        "Connect Browser MCP Chrome extension: $ExtensionUrl"
    } elseif (-not $configured) {
        "Run scripts/setup-browser-mcp.ps1"
    } else { $null })
}

if ($Json) {
    $result | ConvertTo-Json -Compress -Depth 6
} else {
    Write-Host ($result | ConvertTo-Json -Depth 6)
}

if ($ready) { exit 0 } else { exit 2 }
