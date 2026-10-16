<#
.SYNOPSIS
    Checks that every script in the repository still parses, without running any of them.

.DESCRIPTION
    .ps1  parsed by the PowerShell parser that would run it. This finds what Windows PowerShell 5.1 does with a UTF-8 file that has no byte order mark: it reads the bytes as
          ANSI, and the bytes of a long dash or a smart quote contain a character it takes for a string delimiter, so the script breaks half way through a run and not at the start.
    .py   compiled in memory by Python (nothing is written, so no __pycache__ appears).
    .sh   `bash -n`, which reads the script and runs nothing. A CRLF line ending committed in one is reported too (bash on Linux cannot run it; the bash of Windows may hide it).

    Which files: tracked ones, plus new ones that git does not ignore (so a script is checked before `git add`). vendor/, node_modules/ and fixtures/ are left out: a fixture
    is data and may be wrong on purpose. A tool that is not installed (python, a working bash) is reported as skipped, not as a failure; the CI runner has both.

    Exit code 0 when every script that could be checked parses, 1 when one does not.

    -SelfTest   writes good and broken scripts of each kind in a throw-away folder and exits 0 only if the good ones pass and every broken one is found
    -Root       the folder to check (default: the repository this script is in)

.EXAMPLE
    powershell -NoProfile -File scripts/check_scripts.ps1
    powershell -NoProfile -File scripts/check_scripts.ps1 -SelfTest
#>
param(
    [switch]$SelfTest,
    [string]$Root = ''
)

$ErrorActionPreference = 'Stop'
$ExcludePattern = '^(vendor|node_modules|target)/|/node_modules/|/fixtures/|^\.git/'

# Runs a program, waits for it, and returns its exit code and what it wrote to stdout and stderr (no 2>&1: Windows PowerShell 5.1 turns that into errors).
function Invoke-Capture([string]$Exe, [string[]]$Arguments, [string]$WorkDir, [int]$Seconds = 60) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = ($Arguments | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }) -join ' '
    $psi.WorkingDirectory = $WorkDir
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    $out = $p.StandardOutput.ReadToEndAsync()
    $err = $p.StandardError.ReadToEndAsync()
    if (-not $p.WaitForExit($Seconds * 1000)) {
        try { $p.Kill() } catch { }
        return @{ Code = -1; Out = ''; Err = "did not finish in $Seconds seconds" }
    }
    return @{ Code = $p.ExitCode; Out = $out.Result; Err = $err.Result }
}

# The script files under $Dir, as repo-relative paths with forward slashes.
function Get-ScriptFiles([string]$Dir) {
    $rel = @()
    if (Test-Path -LiteralPath (Join-Path $Dir '.git')) {
        $lines = Invoke-Capture 'git' @('-c', 'core.quotepath=off', 'ls-files', '--cached', '--others', '--exclude-standard') $Dir
        if ($lines.Code -eq 0) { $rel = @($lines.Out -split "`r?`n" | Where-Object { $_ }) }
    }
    if ($rel.Count -eq 0) {
        $full = (Resolve-Path -LiteralPath $Dir).Path.TrimEnd('\')
        $rel = @(Get-ChildItem -LiteralPath $Dir -Recurse -File -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -notmatch '\\(\.git|node_modules|target|vendor)\\' } |
            ForEach-Object { $_.FullName.Substring($full.Length + 1).Replace('\', '/') })
    }
    return @($rel | Where-Object { $_ -match '\.(ps1|py|sh)$' -and $_ -notmatch $ExcludePattern -and (Test-Path -LiteralPath (Join-Path $Dir $_)) } | Sort-Object)
}

# A bash that works. `bash` on a Windows PATH can be the launcher of WSL, which fails when no distribution is installed, so each candidate must answer a test first.
function Find-Bash([string]$Dir) {
    $candidates = @()
    $candidates += @(Get-Command bash -All -ErrorAction SilentlyContinue | ForEach-Object { $_.Source })
    foreach ($base in @($env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:LOCALAPPDATA)) {
        if ($base) { $candidates += (Join-Path $base 'Git\bin\bash.exe'), (Join-Path $base 'Programs\Git\bin\bash.exe') }
    }
    foreach ($c in ($candidates | Where-Object { $_ -and (Test-Path -LiteralPath $_) } | Select-Object -Unique)) {
        try {
            $r = Invoke-Capture $c @('-c', 'echo ok') $Dir 20
            if ($r.Code -eq 0 -and $r.Out.Trim() -eq 'ok') { return $c }
        } catch { }
    }
    return $null
}

function Find-Python([string]$Dir) {
    foreach ($name in @('python', 'python3')) {
        foreach ($c in @(Get-Command $name -All -ErrorAction SilentlyContinue | ForEach-Object { $_.Source })) {
            try {
                $r = Invoke-Capture $c @('-I', '-c', 'print(1)') $Dir 20
                if ($r.Code -eq 0 -and $r.Out.Trim() -eq '1') { return $c }
            } catch { }
        }
    }
    return $null
}

# Returns @{ Checked = <number per kind>; Problems = <lines>; Skipped = <lines> }.
function Test-Scripts([string]$Dir) {
    $files = Get-ScriptFiles $Dir
    $problems = New-Object System.Collections.Generic.List[string]
    $skipped = New-Object System.Collections.Generic.List[string]
    $count = @{ ps1 = 0; py = 0; sh = 0 }

    foreach ($f in ($files | Where-Object { $_ -like '*.ps1' })) {
        $tokens = $null; $errors = $null
        [void][System.Management.Automation.Language.Parser]::ParseFile((Join-Path $Dir $f), [ref]$tokens, [ref]$errors)
        $count.ps1++
        foreach ($e in @($errors | Select-Object -First 3)) { $problems.Add(("{0}:{1}: {2}" -f $f, $e.Extent.StartLineNumber, $e.Message)) }
    }

    $py = @($files | Where-Object { $_ -like '*.py' })
    if ($py.Count -gt 0) {
        $python = Find-Python $Dir
        if (-not $python) {
            $skipped.Add("$($py.Count) .py file(s) not checked: no working python on the PATH")
        } else {
            # The checker is a file in the temp folder (not an argument): quoting a program text on a Windows command line is not reliable.
            $helper = Join-Path ([System.IO.Path]::GetTempPath()) ('check_scripts_' + [guid]::NewGuid().ToString('N') + '.py')
            try {
                Set-Content -LiteralPath $helper -Encoding ASCII -Value @(
                    'import ast, sys',
                    'for p in sys.argv[1:]:',
                    '    try:',
                    '        ast.parse(open(p, "rb").read(), p)',
                    '    except SyntaxError as e:',
                    '        print("%s:%s: %s" % (p, e.lineno, e.msg))',
                    '    except Exception as e:',
                    '        print("%s: %s" % (p, e))'
                )
                $r = Invoke-Capture $python (@('-I', $helper) + $py) $Dir 120
                $count.py = $py.Count
                foreach ($line in ($r.Out -split "`r?`n" | Where-Object { $_ })) { $problems.Add($line) }
                if ($r.Code -ne 0 -and -not $r.Out.Trim()) { $problems.Add("python could not run the check: $($r.Err.Trim())") }
            } finally {
                Remove-Item -LiteralPath $helper -Force -ErrorAction SilentlyContinue
            }
        }
    }

    $sh = @($files | Where-Object { $_ -like '*.sh' })
    if ($sh.Count -gt 0) {
        # A CRLF line ending in a shell script is a syntax error to bash on Linux, but not always to the bash of Windows (it can read the file in text mode and drop the CR), so this is
        # checked on its own. In a git repository what counts is the committed line ending (the working tree of a Windows checkout is CRLF with core.autocrlf, and git stores LF).
        $crlf = @()
        if (Test-Path -LiteralPath (Join-Path $Dir '.git')) {
            $eol = Invoke-Capture 'git' (@('ls-files', '--eol', '--') + $sh) $Dir
            foreach ($line in ($eol.Out -split "`r?`n" | Where-Object { $_ -match '^i/(crlf|mixed)\s' })) { $crlf += ($line -split "`t")[-1].Trim() }
        } else {
            foreach ($f in $sh) { if ([System.IO.File]::ReadAllText((Join-Path $Dir $f)).Contains("`r`n")) { $crlf += $f } }
        }
        foreach ($f in $crlf) { $problems.Add("${f}: has CRLF line endings; bash on Linux cannot run it (keep shell scripts LF)") }
        $bash = Find-Bash $Dir
        if (-not $bash) {
            $skipped.Add("$($sh.Count) .sh file(s) not checked: no working bash on this computer")
        } else {
            foreach ($f in $sh) {
                $r = Invoke-Capture $bash @('-n', $f) $Dir 30
                $count.sh++
                if ($r.Code -ne 0) {
                    $why = (($r.Err + ' ' + $r.Out).Trim() -split "`r?`n" | Select-Object -First 1)
                    $problems.Add("${f}: bash -n failed: $why")
                }
            }
        }
    }
    return @{ Count = $count; Problems = $problems; Skipped = $skipped }
}

function Write-Result($Result, [string]$Label) {
    foreach ($s in $Result.Skipped) { Write-Host "note: $s" -ForegroundColor Yellow }
    foreach ($p in $Result.Problems) { Write-Host $p -ForegroundColor Red }
    $c = $Result.Count
    if ($Result.Problems.Count -gt 0) {
        Write-Host "script check FAILED: $($Result.Problems.Count) problem(s) ($($c.ps1) .ps1, $($c.py) .py, $($c.sh) .sh checked)." -ForegroundColor Red
    } else {
        Write-Host "script check passed: $($c.ps1) .ps1, $($c.py) .py, $($c.sh) .sh parse$Label." -ForegroundColor Green
    }
}

if ($SelfTest) {
    $dir = Join-Path ([System.IO.Path]::GetTempPath()) ('check_scripts_selftest_' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $dir | Out-Null
    try {
        $utf8 = New-Object System.Text.UTF8Encoding($false)
        $write = { param($name, $text) [System.IO.File]::WriteAllText((Join-Path $dir $name), $text, $utf8) }
        & $write 'good.ps1' "Write-Output 'ok'`n"
        & $write 'bad.ps1' "if (1 -eq 1) {`n  Write-Output 'x'`n"
        # The long dash is written as its bytes, so that this file stays plain ASCII. Windows PowerShell 5.1 reads these bytes as ANSI and finds a string delimiter in them.
        $dash = [System.Text.Encoding]::UTF8.GetString([byte[]](0xE2, 0x80, 0x94))
        & $write 'dash.ps1' ("Write-Output `"a $dash b`"`n")
        & $write 'good.py' "print('ok')`n"
        & $write 'bad.py' "def f(:`n    pass`n"
        & $write 'good.sh' "echo ok`n"
        & $write 'bad.sh' "if [ 1 ]; then`n  echo x`n"
        & $write 'crlf.sh' "if true; then`r`n  echo ok`r`nfi`r`n"
        $r = Test-Scripts $dir
        $failures = @()
        $text = ($r.Problems -join "`n")
        foreach ($name in @('bad.ps1', 'bad.py')) { if ($text -notmatch [regex]::Escape($name)) { $failures += "$name was not found" } }
        foreach ($name in @('good.ps1', 'good.py')) { if ($text -match [regex]::Escape($name)) { $failures += "$name was reported but is fine" } }
        if ($text -notmatch 'crlf\.sh') { $failures += 'crlf.sh was not found' }
        if ($r.Skipped | Where-Object { $_ -like '*.sh*' }) {
            Write-Host 'note: no working bash, so the syntax case of .sh was skipped.' -ForegroundColor Yellow
        } elseif ($text -notmatch 'bad\.sh') {
            $failures += 'bad.sh was not found'
        }
        if ($text -match 'good\.sh') { $failures += 'good.sh was reported but is fine' }
        # A long dash inside double quotes, in a UTF-8 file with no byte order mark, breaks under Windows PowerShell 5.1 (its last byte is read as a closing quote) and is fine under PowerShell 7.
        if ($PSVersionTable.PSVersion.Major -lt 6 -and $text -notmatch 'dash\.ps1') { $failures += 'dash.ps1 (a long dash in double quotes) was not found' }
        foreach ($f in $failures) { Write-Host "SELF-TEST FAILED: $f" -ForegroundColor Red }
        if ($failures.Count -eq 0) { Write-Host 'self-test passed (broken .ps1, .py and .sh files are found, good ones stay quiet).' -ForegroundColor Green }
        exit ($(if ($failures.Count -eq 0) { 0 } else { 1 }))
    } finally {
        Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

$target = if ($Root) { $Root } else { Split-Path -Parent $PSScriptRoot }
$result = Test-Scripts $target
Write-Result $result ''
exit ($(if ($result.Problems.Count -eq 0) { 0 } else { 1 }))
