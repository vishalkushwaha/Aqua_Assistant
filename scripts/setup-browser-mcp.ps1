# Browser MCP setup for Aqua — idempotent, machine-readable.
# Upstream: https://github.com/BrowserMCP/mcp
# Docs:     https://docs.browsermcp.io/setup-server
#
# Usage:
#   ./scripts/setup-browser-mcp.ps1
#   ./scripts/setup-browser-mcp.ps1 -Json
#
# Does NOT install Chrome extensions (Chrome security) or capture credentials.

param(
    [switch]$Json
)

$ErrorActionPreference = "Stop"
$Package = "@browsermcp/mcp@0.1.3"
$ExtensionUrl = "https://docs.browsermcp.io/setup-extension"
$Root = Split-Path -Parent $PSScriptRoot
if (-not $Root) { $Root = (Get-Location).Path }
# The same folder Aqua uses: AQUA_WORKSPACE_ROOT when it is set (an installed copy keeps its data elsewhere), else the
# repository's data folder.
if ($env:AQUA_WORKSPACE_ROOT) { $Root = $env:AQUA_WORKSPACE_ROOT }
$StateDir = Join-Path $Root "data\browser_mcp"
$StateFile = Join-Path $StateDir "state.json"

function Write-Result {
    param([hashtable]$Obj)
    if ($Json) {
        $Obj | ConvertTo-Json -Compress -Depth 6
    } else {
        if ($Obj.ok) { Write-Host "OK: $($Obj.detail)" } else { Write-Host "FAIL: $($Obj.error)" }
        if ($Obj.user_action_required) { Write-Host "USER ACTION: $($Obj.user_action_required)" }
    }
}

$installed = $false
$configured = $false
$nodeOk = $false
$npxOk = $false
$errorMsg = $null
$userAction = $null

try {
    $node = Get-Command node -ErrorAction SilentlyContinue
    $npx = Get-Command npx -ErrorAction SilentlyContinue
    $nodeOk = [bool]$node
    $npxOk = [bool]$npx

    if (-not $nodeOk -or -not $npxOk) {
        $errorMsg = "Node.js + npx required. Install from https://nodejs.org then re-run."
        $userAction = "Install Node.js LTS and ensure node/npx are on PATH."
        Write-Result @{
            ok = $false
            installed = $false
            configured = $false
            node_available = $nodeOk
            npx_available = $npxOk
            package = $Package
            extension_url = $ExtensionUrl
            error = $errorMsg
            user_action_required = $userAction
            stage = "install"
        }
        exit 1
    }

    if (-not $Json) {
        Write-Host "==> node: $($node.Source)"
        Write-Host "==> npx:  $($npx.Source)"
        Write-Host "==> Prefetching $Package ..."
    }

    # Prefetch / verify package (idempotent — npx caches).
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & npx -y $Package --help 2>$null | Out-Null
    $npxExit = $LASTEXITCODE
    $ErrorActionPreference = $prevEap
    if ($npxExit -ne 0) {
        # Some builds return non-zero on --help; try a dry version probe via npm view.
        & npm view @browsermcp/mcp version 2>$null | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw "Failed to resolve $Package via npx/npm"
        }
    }
    $installed = $true

    New-Item -ItemType Directory -Force -Path $StateDir | Out-Null
    $state = @{
        version = 1
        package = $Package
        setup_ok = $true
        command = "npx"
        args = @("-y", $Package)
        extension_url = $ExtensionUrl
        setup_completed_at = (Get-Date).ToUniversalTime().ToString("o")
        note = "Chrome extension Connect is a manual user step; Aqua never stores browser passwords."
    }
    $state | ConvertTo-Json -Depth 5 | Set-Content -Path $StateFile -Encoding UTF8
    $configured = $true

    Write-Result @{
        ok = $true
        installed = $true
        configured = $true
        node_available = $true
        npx_available = $true
        package = $Package
        package_cached = $true
        state_file = $StateFile
        extension_url = $ExtensionUrl
        detail = "Browser MCP package ready. Start aqua-core MCP, then Connect the Chrome extension."
        user_action_required = "Install/open Browser MCP Chrome extension and click Connect: $ExtensionUrl"
        stage = "configure"
        next = @(
            "POST /api/v1/browser-mcp/ensure",
            "Or open Aqua desktop → Browser MCP → Start Browser MCP"
        )
    }
    exit 0
}
catch {
    Write-Result @{
        ok = $false
        installed = $installed
        configured = $configured
        node_available = $nodeOk
        npx_available = $npxOk
        package = $Package
        extension_url = $ExtensionUrl
        error = $_.Exception.Message
        user_action_required = "Fix Node/npx or network access, then re-run setup-browser-mcp."
        stage = "install"
    }
    exit 1
}
