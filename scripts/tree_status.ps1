<#
.SYNOPSIS
    A read-only report of this working tree and of what is running around it, for the start of a task (/pickup, step 3).

.DESCRIPTION
    Several agents edit one tree, so before touching anything it helps to see in one place:
      - git lock files (.git/*.lock, and in .git/refs and .git/modules/*): a lock that a running git holds is normal for a moment; an old one is left over and blocks every
        git command that writes. The script only lists them (with their age) and removes nothing.
      - an operation in progress (merge, rebase, cherry-pick, revert, bisect);
      - ahead and behind against origin/main, as of the last fetch (nothing is fetched here, so this is never a network call);
      - the dirty paths in groups (staged, modified, new, and conflicts when there are any), each with its count and the first 40 paths;
      - the live processes that matter to a person editing here (cargo, rustc, llama-server, aqua-core), with a shortened command line in which anything that looks like a
        secret is replaced by <masked> (an environment is never printed);
      - which of the TCP ports 8767 (the user's core), 8790 (a second core) and 1420 (the UI) are listening. The list of listeners is read from the operating system; no
        port is connected to.

    It never prints the remote URL, never fetches, never writes into the repository (git is told not to take optional locks), and always exits 0: it is a report.

    -SelfTest   builds a throw-away repository in a temp folder (a commit, an origin, a local commit, a staged, a modified and a new file, lock files) and exits 0 only if the
                report names all of them, shows the right counts and ahead/behind, and leaves the origin path out; also checks the command-line masking and the port check
    -Root       the working tree to report on (default: the repository this script is in)
    -Ports      the ports to look at (default 8767, 8790, 1420)
    -Show       how many paths to list in each group (default 40)

.EXAMPLE
    powershell -NoProfile -File scripts/tree_status.ps1
    powershell -NoProfile -File scripts/tree_status.ps1 -SelfTest
#>
param(
    [switch]$SelfTest,
    [string]$Root = '',
    [int[]]$Ports = @(8767, 8790, 1420),
    [int]$Show = 40
)

$ErrorActionPreference = 'Stop'
# git status would otherwise refresh the index, which is a write; this keeps the report read-only.
$env:GIT_OPTIONAL_LOCKS = '0'

# Runs a program, waits for it, and returns its exit code and what it wrote to stdout and stderr (no 2>&1: Windows PowerShell 5.1 turns that into errors).
function Invoke-Capture([string]$Exe, [string[]]$Arguments, [string]$WorkDir, [int]$Seconds = 60) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = ($Arguments | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }) -join ' '
    $psi.WorkingDirectory = $WorkDir
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
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

function Get-Git([string]$Dir, [string[]]$GitArgs) {
    return Invoke-Capture 'git' (@('-c', 'core.quotepath=off', '-C', $Dir) + $GitArgs) $Dir
}

function Get-GitDir([string]$Dir) {
    $r = Get-Git $Dir @('rev-parse', '--absolute-git-dir')
    if ($r.Code -ne 0) { return $null }
    return ($r.Out.Trim() -replace '/', '\')
}

# Lock files of git: the top of the git folder, its refs, and every module. Object folders are not walked (they hold tens of thousands of files).
function Find-GitLocks([string]$GitDir) {
    $found = New-Object System.Collections.Generic.List[object]
    $dirs = New-Object System.Collections.Generic.Stack[string]
    foreach ($d in @($GitDir, (Join-Path $GitDir 'refs'), (Join-Path $GitDir 'modules'))) {
        if (Test-Path -LiteralPath $d) { $dirs.Push($d) }
    }
    while ($dirs.Count -gt 0) {
        $d = $dirs.Pop()
        foreach ($item in @(Get-ChildItem -LiteralPath $d -Force -ErrorAction SilentlyContinue)) {
            if ($item.PSIsContainer) {
                if ($d -eq $GitDir) { continue }
                if (@('objects', 'lfs', 'logs') -contains $item.Name) { continue }
                $dirs.Push($item.FullName)
            } elseif ($item.Name -like '*.lock') {
                $rel = $item.FullName.Substring($GitDir.Length).TrimStart('\', '/') -replace '\\', '/'
                $found.Add([pscustomobject]@{ Rel = ".git/$rel"; AgeMin = [int]((Get-Date) - $item.LastWriteTime).TotalMinutes })
            }
        }
    }
    return $found.ToArray()
}

# The words of a command line that are safe to show: the program's file name, the arguments, and <masked> in place of whatever looks like a secret (the value after a flag such as
# --api-key or --token, a login before a host, any long run of letters and digits), cut to $Max characters.
function Get-SafeCommandLine([string]$Cmd, [int]$Max = 110) {
    if (-not $Cmd) { return '' }
    $exe = ''
    $rest = ''
    if ($Cmd -match '^\s*"([^"]+)"\s*(.*)$') {
        $exe = Split-Path -Leaf $Matches[1]
        $rest = $Matches[2]
    } elseif ($Cmd -match '^\s*(\S+)\s*(.*)$') {
        $exe = Split-Path -Leaf $Matches[1]
        $rest = $Matches[2]
    }
    $rest = [regex]::Replace($rest, '(?i)(--?(?:api[-_]?key|token|password|passwd|secret|auth|authorization|key)[=\s]+)("[^"]*"|\S+)', '$1<masked>')
    $rest = [regex]::Replace($rest, '://[^/@\s:]+:[^/@\s]+@', '://<masked>@')
    $rest = [regex]::Replace($rest, '\b[A-Za-z0-9_\-]{32,}\b', '<masked>')
    $line = ("$exe $rest").Trim()
    if ($line.Length -gt $Max) { $line = $line.Substring(0, $Max - 3) + '...' }
    return $line
}

# From rows with Name, ProcessId and CommandLine, the ones that matter to a person editing here.
function Select-WatchedProcess([object[]]$Rows) {
    return @($Rows | Where-Object { $_.Name -match '^(cargo|rustc|llama-server|aqua-core)(\.exe)?$' } | Sort-Object Name, ProcessId)
}

# The ports something is listening on, read from the operating system's table of listeners (no port is connected to).
function Get-ListeningPorts {
    $set = @{}
    foreach ($ep in [System.Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners()) { $set[[int]$ep.Port] = $true }
    return $set
}

# "pid 1234 aqua-core" for the process that listens on a port, or '' when that cannot be told.
function Get-PortOwner([int]$Port) {
    try {
        $c = @(Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction Stop | Select-Object -First 1)
        if ($c.Count -eq 0) { return '' }
        $name = ''
        try { $name = (Get-Process -Id $c[0].OwningProcess -ErrorAction Stop).ProcessName } catch { }
        return ("pid $($c[0].OwningProcess) $name").Trim()
    } catch {
        return ''
    }
}

function New-TreeReport([string]$Dir, [int[]]$PortList, [int]$ShowMax) {
    $out = New-Object System.Collections.Generic.List[string]
    $top = Get-Git $Dir @('rev-parse', '--show-toplevel')
    if ($top.Code -ne 0) {
        $out.Add("not a git working tree: $Dir")
        return $out.ToArray()
    }
    $out.Add("tree status of $($top.Out.Trim())")

    $head = Get-Git $Dir @('rev-parse', '--abbrev-ref', 'HEAD')
    $sha = Get-Git $Dir @('rev-parse', '--short', 'HEAD')
    if ($head.Code -eq 0 -and $sha.Code -eq 0) { $out.Add("branch: $($head.Out.Trim()) at $($sha.Out.Trim())") } else { $out.Add('branch: no commit yet') }

    $gitDir = Get-GitDir $Dir
    if ($gitDir) {
        $busy = @('MERGE_HEAD', 'CHERRY_PICK_HEAD', 'REVERT_HEAD', 'BISECT_LOG', 'rebase-merge', 'rebase-apply') | Where-Object { Test-Path -LiteralPath (Join-Path $gitDir $_) }
        if (@($busy).Count -gt 0) { $out.Add("operation in progress: $((@($busy) -join ', ')) (finish or abort it before anything else)") }
        $locks = @(Find-GitLocks $gitDir)
        if ($locks.Count -eq 0) {
            $out.Add('git locks: none')
        } else {
            $out.Add("git locks: $($locks.Count) (a running git holds one for a moment; an old one is left over and blocks git; this script removes nothing)")
            foreach ($l in $locks) { $out.Add("  $($l.Rel)  age $($l.AgeMin) min") }
        }
    }

    $has = Get-Git $Dir @('rev-parse', '--verify', '--quiet', 'refs/remotes/origin/main')
    if ($has.Code -ne 0) {
        $out.Add('against origin/main: there is no origin/main here')
    } else {
        $cnt = Get-Git $Dir @('rev-list', '--left-right', '--count', 'origin/main...HEAD')
        $parts = @($cnt.Out.Trim() -split '\s+')
        if ($cnt.Code -eq 0 -and $parts.Count -ge 2) {
            $when = ', no fetch recorded'
            if ($gitDir -and (Test-Path -LiteralPath (Join-Path $gitDir 'FETCH_HEAD'))) {
                $when = ', fetched ' + (Get-Item -LiteralPath (Join-Path $gitDir 'FETCH_HEAD')).LastWriteTime.ToString('yyyy-MM-dd HH:mm')
            }
            $out.Add("against origin/main: ahead $($parts[1]), behind $($parts[0]) (as of the last fetch$when; nothing was fetched now)")
        } else {
            $out.Add('against origin/main: could not be counted')
        }
    }

    $st = Get-Git $Dir @('status', '--porcelain=v1', '--untracked-files=normal')
    $staged = New-Object System.Collections.Generic.List[string]
    $modified = New-Object System.Collections.Generic.List[string]
    $new = New-Object System.Collections.Generic.List[string]
    $conflict = New-Object System.Collections.Generic.List[string]
    foreach ($l in ($st.Out -split "`r?`n")) {
        if ($l.Length -lt 4) { continue }
        $x = $l.Substring(0, 1)
        $y = $l.Substring(1, 1)
        $p = $l.Substring(3)
        if ($x -eq '?' -and $y -eq '?') { $new.Add($p); continue }
        if ($x -eq '!') { continue }
        if (@('UU', 'AA', 'DD', 'AU', 'UA', 'DU', 'UD') -contains ($x + $y)) { $conflict.Add($p); continue }
        if ($x -ne ' ') { $staged.Add("$x  $p") }
        if ($y -ne ' ') { $modified.Add("$y  $p") }
    }
    $total = $staged.Count + $modified.Count + $new.Count + $conflict.Count
    $out.Add("dirty paths: $total (a file that is both staged and changed again is in both groups)")
    $groups = @(@('staged', $staged), @('modified', $modified), @('new', $new))
    if ($conflict.Count -gt 0) { $groups = @(@('conflicts', $conflict)) + $groups }
    foreach ($g in $groups) {
        $list = $g[1]
        $out.Add("  $($g[0]) ($($list.Count)):")
        $i = 0
        foreach ($item in $list) {
            if ($i -ge $ShowMax) { $out.Add("    ... and $($list.Count - $ShowMax) more"); break }
            $out.Add("    $item")
            $i++
        }
    }

    $out.Add('running (cargo, rustc, llama-server, aqua-core):')
    try {
        $rows = Select-WatchedProcess @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)
        if ($rows.Count -eq 0) { $out.Add('  none') }
        foreach ($r in $rows) { $out.Add("  pid $($r.ProcessId)  $(Get-SafeCommandLine $r.CommandLine)") }
    } catch {
        $out.Add("  the process list could not be read ($($_.Exception.Message))")
    }

    $listening = Get-ListeningPorts
    $out.Add('ports:')
    foreach ($port in $PortList) {
        if ($listening.ContainsKey([int]$port)) {
            $owner = Get-PortOwner $port
            $out.Add("  $port listening$(if ($owner) { ', ' + $owner })")
        } else {
            $out.Add("  $port not listening")
        }
    }
    return $out.ToArray()
}

if ($SelfTest) {
    $base = Join-Path ([System.IO.Path]::GetTempPath()) ('tree_status_selftest_' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $base | Out-Null
    $failures = New-Object System.Collections.Generic.List[string]
    $open = @()
    try {
        $repo = Join-Path $base 'work'
        $origin = Join-Path $base 'origin.git'
        New-Item -ItemType Directory -Path $repo | Out-Null
        $git = { param($dir, [string[]]$a) $r = Get-Git $dir $a; if ($r.Code -ne 0) { throw "git $($a -join ' ') failed: $($r.Err)" } }
        $ident = @('-c', 'user.name=selftest', '-c', 'user.email=selftest@example.invalid', '-c', 'commit.gpgsign=false')
        & $git $repo @('init', '-q')
        & $git $repo @('symbolic-ref', 'HEAD', 'refs/heads/main')
        Set-Content -LiteralPath (Join-Path $repo 'tracked.txt') -Value 'one' -Encoding ASCII
        Set-Content -LiteralPath (Join-Path $repo 'staged.txt') -Value 'one' -Encoding ASCII
        & $git $repo @('add', 'tracked.txt', 'staged.txt')
        & $git $repo ($ident + @('commit', '-q', '-m', 'base'))
        New-Item -ItemType Directory -Path $origin | Out-Null
        & $git $origin @('init', '-q', '--bare')
        & $git $repo @('remote', 'add', 'origin', $origin)
        & $git $repo @('push', '-q', 'origin', 'main:main')
        & $git $repo @('fetch', '-q', 'origin')
        Set-Content -LiteralPath (Join-Path $repo 'local.txt') -Value 'two' -Encoding ASCII
        & $git $repo @('add', 'local.txt')
        & $git $repo ($ident + @('commit', '-q', '-m', 'local'))
        # One staged, one modified, one new file. The lock files come last: with one in place git cannot write the index.
        Set-Content -LiteralPath (Join-Path $repo 'staged.txt') -Value 'changed' -Encoding ASCII
        & $git $repo @('add', 'staged.txt')
        Set-Content -LiteralPath (Join-Path $repo 'tracked.txt') -Value 'changed' -Encoding ASCII
        Set-Content -LiteralPath (Join-Path $repo 'untracked.txt') -Value 'new' -Encoding ASCII
        $gd = Join-Path $repo '.git'
        New-Item -ItemType File -Path (Join-Path $gd 'index.lock') | Out-Null
        New-Item -ItemType Directory -Path (Join-Path $gd 'modules\sub') | Out-Null
        New-Item -ItemType File -Path (Join-Path $gd 'modules\sub\config.lock') | Out-Null

        # One port that is listening, one that is not.
        $a = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0)
        $b = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0)
        $a.Start(); $b.Start()
        $open += $a
        $portOn = $a.LocalEndpoint.Port
        $portOff = $b.LocalEndpoint.Port
        $b.Stop()

        $text = (New-TreeReport $repo @($portOn, $portOff) 40) -join "`n"
        $expect = @(
            @('index.lock', 'the top-level lock file'),
            @('modules/sub/config.lock', 'the lock file of a module'),
            @('ahead 1, behind 0', 'ahead and behind'),
            @('as of the last fetch', 'the note that nothing was fetched'),
            @('dirty paths: 3', 'the total of dirty paths'),
            @('staged (1):', 'the staged count'),
            @('modified (1):', 'the modified count'),
            @('new (1):', 'the new count'),
            @('staged.txt', 'the staged file'),
            @('tracked.txt', 'the modified file'),
            @('untracked.txt', 'the new file'),
            @("$portOn listening", 'a listening port'),
            @("$portOff not listening", 'a port nobody listens on'),
            @('running (cargo, rustc, llama-server, aqua-core):', 'the process section')
        )
        foreach ($e in $expect) { if ($text -notmatch [regex]::Escape($e[0])) { $failures.Add("the report does not show $($e[1]) ('$($e[0])')") } }
        if ($text -match [regex]::Escape($origin) -or $text -match 'origin\.git') { $failures.Add('the report shows where origin is (the remote must never be printed)') }

        # A limit on the list: five new files, show two.
        1..5 | ForEach-Object { Set-Content -LiteralPath (Join-Path $repo "many$_.txt") -Value 'x' -Encoding ASCII }
        $short = (New-TreeReport $repo @() 2) -join "`n"
        if ($short -notmatch 'new \(6\):' -or $short -notmatch '\.\.\. and 4 more') { $failures.Add('a group longer than the limit is not cut with a count of the rest') }

        # The report is a report: the script itself ends with exit code 0 and shows the same.
        $host_ = (Get-Process -Id $PID).Path
        $run = Invoke-Capture $host_ @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $PSCommandPath, '-Root', $repo, '-Ports', "$portOn") $repo 120
        if ($run.Code -ne 0) { $failures.Add("the script exited with $($run.Code), not 0 ($($run.Err.Trim()))") }
        if ($run.Out -notmatch 'untracked\.txt') { $failures.Add('a run of the script does not list the new file') }

        # A folder that is not a working tree is said so and still exits 0.
        $plain = Join-Path $base 'plain'
        New-Item -ItemType Directory -Path $plain | Out-Null
        if (((New-TreeReport $plain @() 40) -join "`n") -notmatch 'not a git working tree') { $failures.Add('a folder outside any repository is not reported as such') }

        # Command lines: the program's name and the arguments stay, a secret goes.
        $fakeKey = 'abcdefghijklmnopqrstuvwxyz012345' # secret-scan: ignore
        $line = Get-SafeCommandLine ('"C:\tools\llama-server.exe" -m model.gguf --api-key ' + $fakeKey + ' --port 8080')
        if ($line -notmatch 'llama-server\.exe -m model\.gguf' -or $line -notmatch '--port 8080') { $failures.Add("a command line lost its program or its arguments: $line") }
        if ($line -match [regex]::Escape($fakeKey) -or $line -notmatch '<masked>') { $failures.Add("a key after --api-key was not masked: $line") }
        $line = Get-SafeCommandLine ('tool --url https://user:hunter2@example.invalid/x --note ' + $fakeKey) # secret-scan: ignore
        if ($line -match 'hunter2' -or $line -match [regex]::Escape($fakeKey)) { $failures.Add("a login in a URL or a long token was not masked: $line") }
        $line = Get-SafeCommandLine '"C:\Users\x\.cargo\bin\cargo.exe" check -p aqua-core --tests'
        if ($line -ne 'cargo.exe check -p aqua-core --tests') { $failures.Add("a plain cargo command line was changed: $line") }
        if ((Get-SafeCommandLine ('prog ' + ('a ' * 200))).Length -gt 110) { $failures.Add('a long command line was not shortened') }
        $rows = @(
            [pscustomobject]@{ Name = 'cargo.exe'; ProcessId = 1; CommandLine = 'x' },
            [pscustomobject]@{ Name = 'notepad.exe'; ProcessId = 2; CommandLine = 'y' },
            [pscustomobject]@{ Name = 'Rustc.EXE'; ProcessId = 3; CommandLine = 'z' },
            [pscustomobject]@{ Name = 'llama-server.exe'; ProcessId = 4; CommandLine = 'w' },
            [pscustomobject]@{ Name = 'aqua-core.exe'; ProcessId = 5; CommandLine = 'v' },
            [pscustomobject]@{ Name = 'cargo-watch.exe'; ProcessId = 6; CommandLine = 'u' }
        )
        if ((Select-WatchedProcess $rows).Count -ne 4) { $failures.Add('the process filter does not pick exactly cargo, rustc, llama-server and aqua-core') }
    } catch {
        $failures.Add("the self-test could not run: $($_.Exception.Message)")
    } finally {
        foreach ($l in $open) { try { $l.Stop() } catch { } }
        # The temp folder holds only what this test made (no junction), so a recursive delete is safe here.
        Remove-Item -LiteralPath $base -Recurse -Force -ErrorAction SilentlyContinue
    }
    foreach ($f in $failures) { Write-Host "SELF-TEST FAILED: $f" -ForegroundColor Red }
    if ($failures.Count -eq 0) { Write-Host 'self-test passed (locks, ahead/behind, dirty groups, ports, masking and the exit code are right, and the remote is not shown).' -ForegroundColor Green }
    exit ($(if ($failures.Count -eq 0) { 0 } else { 1 }))
}

$target = if ($Root) { $Root } else { Split-Path -Parent $PSScriptRoot }
try {
    foreach ($l in @(New-TreeReport $target $Ports $Show)) { Write-Output $l }
} catch {
    Write-Output "tree status could not finish: $($_.Exception.Message)"
}
exit 0
