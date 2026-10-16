# Who can reach the port that Browser MCP opens (card R6-23, audit item SE-05). It starts the pinned package by itself for a few seconds, the way Aqua starts it (with the
# loader tools/browser-mcp/loopback_only.cjs required, which listens on this computer only and keeps web pages out), and asks:
#   1. on which addresses does it listen (every interface, or this computer only);
#   2. does it answer a WebSocket handshake that says it comes from a web page (an Origin of a web site), on the loopback address and on this computer's LAN address;
#   3. does it still answer a handshake that says it comes from a browser extension (the extension must get in).
# Usage: ./scripts/check-browser-mcp-exposure.ps1 [-Port 9009] [-Json] [-WithoutShim]
#   -WithoutShim  start the bare package, without the loader: shows what the package does by itself (it is open to the network and to web pages)
#   AQUA_BROWSER_MCP_EXTENSION_ID, when set, is the extension id the probe uses and the loader is told (as Aqua tells it).
#
# It will NOT run while something already listens on the port: Browser MCP gives the connection to whoever connected last, so a handshake to a running one
# (with the extension connected) would knock the extension off. Stop Aqua's Browser MCP first (POST /api/v1/browser-mcp/stop), then run this.
# Nothing is sent to the internet and nothing is typed anywhere. Exit code 0 = closed to the network and to web pages, 1 = open in some way, 2 = could not check.

param([int]$Port = 9009, [switch]$Json, [switch]$WithoutShim)

$ErrorActionPreference = "Continue"
$Package = "@browsermcp/mcp@0.1.3"   # pinned like the runtime: what the person runs is what is checked
$Shim = Join-Path (Split-Path -Parent $PSScriptRoot) 'tools\browser-mcp\loopback_only.cjs'
$ExtensionId = if ($env:AQUA_BROWSER_MCP_EXTENSION_ID -match '^[a-pA-P]{32}$') { $env:AQUA_BROWSER_MCP_EXTENSION_ID.ToLower() } else { 'abcdefghijklmnopabcdefghijklmnop' }

function Listeners($port) {
    # `netstat -ano` lists IPv4 and IPv6; a listening socket has an all-zero foreign address (the word LISTENING is translated on other Windows languages).
    netstat -ano | ForEach-Object {
        $p = ($_ -split '\s+') | Where-Object { $_ }
        if ($p.Count -ge 4 -and $p[0] -eq 'TCP' -and $p[2] -in @('0.0.0.0:0', '[::]:0') -and $p[1] -match ":$port$") { $p[1] }
    } | Select-Object -Unique
}

function Handshake($address, $origin, $port) {
    try {
        $c = New-Object System.Net.Sockets.TcpClient
        $c.ReceiveTimeout = 3000
        $c.Connect($address, $port)
        $s = $c.GetStream()
        $o = if ($origin) { "Origin: $origin`r`n" } else { "" }
        $req = "GET / HTTP/1.1`r`nHost: ${address}:$port`r`nUpgrade: websocket`r`nConnection: Upgrade`r`nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==`r`nSec-WebSocket-Version: 13`r`n${o}`r`n"
        $b = [Text.Encoding]::ASCII.GetBytes($req)
        $s.Write($b, 0, $b.Length)
        $buf = New-Object byte[] 256
        $n = $s.Read($buf, 0, 256)
        $c.Close()
        return ([Text.Encoding]::ASCII.GetString($buf, 0, $n) -split "`r`n")[0]
    } catch { return "no answer ($($_.Exception.Message))" }
}

$already = @(Listeners $Port)
if ($already.Count -gt 0) {
    Write-Host "Something already listens on port ${Port}: $($already -join ', '). Not checking: a handshake could knock the Browser MCP extension off. Stop it first." -ForegroundColor Yellow
    exit 2
}

# The package as `npx` keeps it, so nothing is downloaded here: run scripts/setup-browser-mcp.ps1 first if it is missing.
$entry = $null
Get-ChildItem "$env:LOCALAPPDATA\npm-cache\_npx" -Directory -ErrorAction SilentlyContinue | ForEach-Object {
    $pkg = Join-Path $_.FullName 'node_modules\@browsermcp\mcp'
    $pkgJson = Join-Path $pkg 'package.json'
    if ((Test-Path $pkgJson) -and ((Get-Content $pkgJson -Raw | ConvertFrom-Json).version -eq ($Package -split '@')[-1])) { $entry = Join-Path $pkg 'dist\index.js' }
}
if (-not $entry -or -not (Test-Path $entry)) {
    Write-Host "$Package is not in the npx cache. Run scripts/setup-browser-mcp.ps1 first." -ForegroundColor Yellow
    exit 2
}

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = 'node'
$psi.Arguments = "`"$entry`""
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true
$withShim = (-not $WithoutShim) -and (Test-Path $Shim)
if ($withShim) {
    # The same variables Aqua gives the server (adapters/mcp_adapters/src/browser_mcp/client.rs): forward slashes, quotes around a path with a space.
    $shimPath = $Shim.Replace('\', '/')
    $require = if ($shimPath.Contains(' ')) { "--require=`"$shimPath`"" } else { "--require=$shimPath" }
    $psi.EnvironmentVariables['NODE_OPTIONS'] = if ($env:NODE_OPTIONS) { "$($env:NODE_OPTIONS) $require" } else { $require }
    $psi.EnvironmentVariables['AQUA_LOOPBACK_ONLY_PORT'] = "$Port"
    if ($env:AQUA_BROWSER_MCP_EXTENSION_ID -match '^[a-pA-P]{32}$') { $psi.EnvironmentVariables['AQUA_BROWSER_MCP_EXTENSION_ID'] = $ExtensionId }
} elseif (-not $WithoutShim) {
    Write-Host "The loader $Shim is missing: checking the bare package." -ForegroundColor Yellow
}
$proc = [System.Diagnostics.Process]::Start($psi)
$report = [ordered]@{ package = $Package; port = $Port; loader = $withShim; listening = @(); open_to_the_network = $false; web_page_origin_loopback = $null; web_page_origin_lan = $null; extension_origin_loopback = $null; lan_address = $null }
try {
    $deadline = (Get-Date).AddSeconds(25)
    do { Start-Sleep -Milliseconds 500; $now = @(Listeners $Port) } while ($now.Count -eq 0 -and -not $proc.HasExited -and (Get-Date) -lt $deadline)
    if ($now.Count -gt 0) {
        $report.listening = $now
        $report.open_to_the_network = [bool]($now | Where-Object { $_ -notmatch '^(127\.|\[::1\]|\[::ffff:127\.)' })
        $report.web_page_origin_loopback = Handshake '127.0.0.1' 'https://web-page-check.invalid' $Port
        $report.extension_origin_loopback = Handshake '127.0.0.1' "chrome-extension://$ExtensionId" $Port
        $lan = (Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue | Where-Object { $_.IPAddress -notlike '127.*' -and $_.IPAddress -notlike '169.254.*' } | Select-Object -First 1).IPAddress
        $report.lan_address = $lan
        if ($lan) { $report.web_page_origin_lan = Handshake $lan 'https://web-page-check.invalid' $Port }
    }
} finally {
    if (-not $proc.HasExited) { $proc.Kill() }
}
if ($report.listening.Count -eq 0) { Write-Host "The package did not open port $Port." -ForegroundColor Yellow; exit 2 }

$pageGetsIn = ($report.web_page_origin_loopback -match ' 101 ') -or ($report.web_page_origin_lan -match ' 101 ')
$extensionKeptOut = $withShim -and ($report.extension_origin_loopback -notmatch ' 101 ')
if ($Json) { $report | ConvertTo-Json } else {
    Write-Host "Browser MCP $Package, port ${Port} (loader: $withShim):"
    Write-Host "  listens on:                         $($report.listening -join ', ')"
    Write-Host "  open to the network:                $($report.open_to_the_network)"
    Write-Host "  answer to a web page (loopback):    $($report.web_page_origin_loopback)"
    Write-Host "  answer to a web page (LAN $($report.lan_address)): $($report.web_page_origin_lan)"
    Write-Host "  answer to an extension (loopback):  $($report.extension_origin_loopback)"
    if ($report.open_to_the_network -or $pageGetsIn) {
        Write-Host "Open: whoever connects last takes the extension's place and receives what Aqua sends to the browser." -ForegroundColor Red
    } elseif ($extensionKeptOut) {
        Write-Host "Closed to the network and to web pages, but the extension was refused too: check AQUA_BROWSER_MCP_EXTENSION_ID." -ForegroundColor Red
    } else {
        Write-Host "Closed to the network and to web pages." -ForegroundColor Green
    }
}
if ($report.open_to_the_network -or $pageGetsIn -or $extensionKeptOut) { exit 1 } else { exit 0 }
