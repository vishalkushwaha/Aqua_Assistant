# Start / ensure Browser MCP via aqua-core (preferred) or print setup hints.
# Usage: ./scripts/start-browser-mcp.ps1 [-Json]

param([switch]$Json)

$ErrorActionPreference = "Continue"
$EnsureUrl = "http://127.0.0.1:8767/api/v1/browser-mcp/ensure"
$StartUrl = "http://127.0.0.1:8767/api/v1/browser-mcp/start"
$ExtensionUrl = "https://docs.browsermcp.io/setup-extension"
$TokenFile = Join-Path (Split-Path -Parent $PSScriptRoot) "data\.aqua_api_token"
$ApiToken = if ($env:AQUA_API_TOKEN) { $env:AQUA_API_TOKEN } elseif (Test-Path $TokenFile) { (Get-Content $TokenFile -Raw).Trim() } else { "" }
$AuthHeaders = @{ Authorization = "Bearer $ApiToken" }

function Emit($obj, $code) {
    if ($Json) { $obj | ConvertTo-Json -Compress -Depth 8 }
    else { Write-Host ($obj | ConvertTo-Json -Depth 8) }
    exit $code
}

# Prefer full ensure (setup + start + probe).
try {
    $resp = Invoke-WebRequest -Uri $EnsureUrl -Method POST -Headers $AuthHeaders -UseBasicParsing -TimeoutSec 90
    $body = $resp.Content | ConvertFrom-Json
    $ok = [bool]$body.ready
    Emit @{
        ok = $ok
        source = "aqua-core:/api/v1/browser-mcp/ensure"
        health = $body
        detail = $body.detail
        user_action_required = $body.user_action_required
        extension_url = $ExtensionUrl
    } $(if ($ok) { 0 } else { 2 })
} catch {
    # Fallback: start only
    try {
        $resp = Invoke-WebRequest -Uri $StartUrl -Method POST -Headers $AuthHeaders -UseBasicParsing -TimeoutSec 60
        $body = $resp.Content | ConvertFrom-Json
        Emit @{
            ok = [bool]$body.initialized
            source = "aqua-core:/api/v1/browser-mcp/start"
            status = $body
            detail = "MCP process start attempted. Run check-browser-mcp for full contract."
            user_action_required = "Connect Chrome extension: $ExtensionUrl"
            extension_url = $ExtensionUrl
        } $(if ($body.initialized) { 0 } else { 2 })
    } catch {
        Emit @{
            ok = $false
            source = "local"
            error = "aqua-core not reachable at 127.0.0.1:8767. Start aqua-core, then re-run."
            hint = "cargo run -p aqua-core --release   OR   target/release/aqua-core.exe"
            user_action_required = "Start Aqua Core, then run this script again."
            extension_url = $ExtensionUrl
            stage = "server_start"
        } 1
    }
}
