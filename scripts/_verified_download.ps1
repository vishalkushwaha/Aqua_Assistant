# Shared by the fetch_*.ps1 scripts: download a file so that a bad or partial copy can never be left in place.
#
#   * The file is downloaded to "<name>.part" and renamed only after its SHA-256 matches. An interrupted download
#     leaves a .part file (curl resumes it), never a truncated model that later scripts mistake for the real one.
#   * A file already in place is checked against the hash too; if it does not match it is set aside, not trusted.
#   * A download with no expected hash is refused unless -AllowUnpinned is given, in which case the hash that was
#     computed is printed so it can be pinned.

function Get-Sha256Hex([string]$Path) {
    (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Get-VerifiedFile {
    param(
        [Parameter(Mandatory)][string]$Url,
        [Parameter(Mandatory)][string]$OutFile,
        [string]$Sha256 = '',
        [switch]$AllowUnpinned
    )
    $ErrorActionPreference = 'Stop'
    $expected = $Sha256.Trim().ToLowerInvariant()
    if (-not $expected -and -not $AllowUnpinned) {
        throw "No expected SHA-256 is pinned for $OutFile; refusing to install an unchecked download (use -AllowUnpinned to see its hash)."
    }

    if (Test-Path -LiteralPath $OutFile) {
        if (-not $expected) { Write-Host "Already present (not verified): $OutFile"; return }
        if ((Get-Sha256Hex $OutFile) -eq $expected) { Write-Host "Already present and verified: $OutFile"; return }
        $aside = "$OutFile.mismatch"
        Write-Warning "$OutFile does not match the expected SHA-256; setting it aside as $aside and downloading again."
        Move-Item -LiteralPath $OutFile -Destination $aside -Force
    }

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $OutFile) | Out-Null
    $part = "$OutFile.part"
    Write-Host "Downloading $Url"
    # --fail: an HTML error page is a failure, not a file. -C - resumes a previous .part.
    # curl writes its progress meter to stderr; Windows PowerShell 5.1 turns native stderr into a terminating error
    # when it is redirected, so only the exit code is trusted here.
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    # A progress bar only in an interactive console; otherwise stay quiet apart from errors.
    $quiet = if ($Host.Name -eq 'ConsoleHost' -and -not [Console]::IsOutputRedirected -and -not [Console]::IsErrorRedirected) { '-#' } else { '-sS' }
    try { curl.exe $quiet -L --fail --retry 5 --retry-delay 3 -C - -o $part $Url } finally { $ErrorActionPreference = $previous }
    if ($LASTEXITCODE -ne 0) { throw "Download failed (curl exit code $LASTEXITCODE). The partial file $part is kept so it can resume." }

    $actual = Get-Sha256Hex $part
    if ($expected -and $actual -ne $expected) {
        Remove-Item -LiteralPath $part -Force
        throw "SHA-256 mismatch for $Url`n  expected $expected`n  got      $actual`nThe download was discarded and nothing was installed."
    }
    Move-Item -LiteralPath $part -Destination $OutFile -Force
    Write-Host "Saved to $OutFile  (sha256 $actual)$(if (-not $expected) { '  NOT VERIFIED' })"
}
