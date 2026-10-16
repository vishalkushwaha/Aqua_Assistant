<#
.SYNOPSIS
  Writes latest.json, the small file the installed app reads to learn that a newer version exists.

.DESCRIPTION
  Run after build_release.ps1. It finds the signed Windows installer (and its .sig) that `tauri build` produced and writes
  latest.json next to them. Upload the installer and latest.json to the place the app is pointed at (AQUA_UPDATE_URL or
  <data folder>\update.json). Nothing is uploaded by this script, and the private signing key is never read here: only the
  public signature file (.sig) beside the installer is.

  The file carries two dates the app enforces: pub_date (now) and expires (pub_date plus -ExpiresInDays, 30 by default, 90 at
  most). The app refuses a file after its expiry, so publish a fresh latest.json before then, even when nothing changed.
  -MinVersion names the oldest installed version that may update by itself; older ones are told to install by hand.

.PARAMETER BaseUrl
  The https address the installer will be downloaded from (the folder, without the file name).

.PARAMETER Notes
  Release notes shown to the person in the update prompt (plain text). Left out, the Unreleased section of
  documentation/CHANGELOG.md is used. The script only reads that file; it never rewrites it.

.PARAMETER ExpiresInDays
  How long the file stays valid, from the moment it is written: 1 to 90 days (default 30).

.PARAMETER MinVersion
  Optional. The oldest installed version that may update by itself, such as 0.2.0. Omitted, the field is left out.

.PARAMETER BundleDir
  Only for trying the script out: the folder that holds the installer and its .sig. Default: the folder tauri build writes.

.PARAMETER InfoDir
  Where scripts\release_smoke.ps1 wrote build-info.json and SHA256SUMS.txt (default: dist). When build-info.json is there, the installer's SHA-256 must be the one it
  records (a different installer is refused: it was not the one that was started and checked), and latest.json is added to SHA256SUMS.txt. When it is not there, a warning
  says that the release was not checked and latest.json is written as before.
#>
param(
    [Parameter(Mandatory = $true)][string]$BaseUrl,
    [string]$Notes = '',
    [ValidateRange(1, 90)][int]$ExpiresInDays = 30,
    [string]$MinVersion = '',
    [string]$BundleDir = '',
    [string]$InfoDir = ''
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$bundle = if ($BundleDir) { $BundleDir } else { Join-Path $root 'apps\aqua-desktop\src-tauri\target\release\bundle\nsis' }
$infoRoot = if ($InfoDir) { $InfoDir } else { Join-Path $root 'dist' }

if ($BaseUrl -notmatch '^https://' -and $BaseUrl -notmatch '^http://(127\.0\.0\.1|localhost)[:/]') {
    throw 'BaseUrl must be https (plain http only for this computer, to try a release out).'
}
if ($BaseUrl -match '@') { throw 'BaseUrl must not contain a user name, password or token.' }

# The versions the app can read: numbers with dots, and an optional -pre suffix. Anything else it would refuse.
if ($MinVersion -and $MinVersion -notmatch '^\d+(\.\d+){1,3}(-[0-9A-Za-z.-]+)?$') {
    throw "MinVersion '$MinVersion' is not a version the app can read (numbers with dots, such as 0.2.0)."
}

# The Unreleased section of the change log: the lines under '## Unreleased' up to the next '## ' heading.
function Get-UnreleasedNotes([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return '' }
    $inside = $false
    $kept = New-Object System.Collections.Generic.List[string]
    foreach ($line in (Get-Content -LiteralPath $Path -Encoding UTF8)) {
        if ($line -match '^##\s') {
            if ($inside) { break }
            if ($line -match '^##\s+Unreleased\s*$') { $inside = $true }
            continue
        }
        if ($inside) { $kept.Add($line.TrimEnd()) }
    }
    return ($kept -join "`n").Trim()
}

$conf = Get-Content (Join-Path $root 'apps\aqua-desktop\src-tauri\tauri.conf.json') -Raw | ConvertFrom-Json
$version = $conf.version
if ($version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$') {
    throw "The version '$version' in tauri.conf.json is not one the app can read (three numbers with dots, and an optional -pre suffix)."
}
$installer = Get-ChildItem $bundle -Filter "*_${version}_x64-setup.exe" -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $installer) { throw "No installer for version $version in $bundle. Run build_release.ps1 first." }
$sigFile = "$($installer.FullName).sig"
if (-not (Test-Path $sigFile)) { throw "The installer has no signature file ($sigFile). Was TAURI_SIGNING_PRIVATE_KEY_PATH set for the build?" }

# The release check (scripts\release_smoke.ps1) records the SHA-256 of the installer it looked at. The installer that is published must be that one.
$infoFile = Join-Path $infoRoot 'build-info.json'
$sumsFile = Join-Path $infoRoot 'SHA256SUMS.txt'
if (Test-Path -LiteralPath $infoFile -PathType Leaf) {
    $installerHash = (Get-FileHash -LiteralPath $installer.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    $info = Get-Content -LiteralPath $infoFile -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($info.installer.name -ne $installer.Name -or $info.installer.sha256 -ne $installerHash) {
        throw "The installer $($installer.Name) (sha256 $installerHash) is not the one scripts\release_smoke.ps1 checked ($($info.installer.name), sha256 $($info.installer.sha256)). Build again and run the release check again, or remove $infoFile to publish without it."
    }
    Write-Host "Checked: this is the installer that scripts\release_smoke.ps1 started and checked (sha256 $installerHash)."
} else {
    Write-Warning "No record of a release check ($infoFile): run scripts\release_smoke.ps1 after the build, so that the installer you publish is one whose core was started and checked."
}

if (-not $Notes) {
    $Notes = Get-UnreleasedNotes (Join-Path $root 'documentation\CHANGELOG.md')
    if ($Notes) { Write-Host 'Release notes: the Unreleased section of documentation/CHANGELOG.md.' }
    else { Write-Warning 'No release notes: pass -Notes, or write the changes under "## Unreleased" in documentation/CHANGELOG.md.' }
}
if ($Notes.Length -gt 4000) { Write-Warning "The release notes are $($Notes.Length) characters; the app shows the first 4000." }

$signature = (Get-Content $sigFile -Raw).Trim()
$fileName = [uri]::EscapeDataString($installer.Name)
# Invariant culture: the file is read by a program, so the date has one shape in every language and calendar.
$invariant = [System.Globalization.CultureInfo]::InvariantCulture
$published = (Get-Date).ToUniversalTime()
$manifest = [ordered]@{
    version  = $version
    notes    = $Notes
    pub_date = $published.ToString('yyyy-MM-ddTHH:mm:ssZ', $invariant)
    expires  = $published.AddDays($ExpiresInDays).ToString('yyyy-MM-ddTHH:mm:ssZ', $invariant)
}
if ($MinVersion) { $manifest['min_version'] = $MinVersion }
$manifest['platforms'] = [ordered]@{
    'windows-x86_64' = [ordered]@{
        signature = $signature
        url       = ($BaseUrl.TrimEnd('/') + '/' + $fileName)
    }
}
$out = Join-Path $bundle 'latest.json'
# UTF-8 without a byte order mark: Windows PowerShell 5.1 adds one with -Encoding utf8, and the app's JSON reader rejects it.
[System.IO.File]::WriteAllText($out, (($manifest | ConvertTo-Json -Depth 5) + "`n"), (New-Object System.Text.UTF8Encoding($false)))
Write-Host "Wrote $out"
# latest.json joins the list of checksums that the release check wrote (the entry of an earlier latest.json is replaced).
if (Test-Path -LiteralPath $sumsFile -PathType Leaf) {
    $latestHash = (Get-FileHash -LiteralPath $out -Algorithm SHA256).Hash.ToLowerInvariant()
    $kept = @(Get-Content -LiteralPath $sumsFile -Encoding UTF8 | Where-Object { $_.Trim() -and $_ -notmatch '^[0-9a-f]{64}  latest\.json$' })
    $kept += ('{0}  latest.json' -f $latestHash)
    [System.IO.File]::WriteAllText($sumsFile, (($kept -join "`n") + "`n"), (New-Object System.Text.UTF8Encoding($false)))
    Write-Host "Added latest.json to $sumsFile"
}
Write-Host "Valid until $($manifest['expires']) (UTC): the app stops using it after that, so publish a fresh latest.json before then."
if ($MinVersion) { Write-Host "Installed versions older than $MinVersion are told to install by hand." }
Write-Host "Upload '$($installer.Name)' and 'latest.json' to $BaseUrl"
