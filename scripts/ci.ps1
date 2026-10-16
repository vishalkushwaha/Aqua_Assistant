<#
.SYNOPSIS
    The local gate: the checks CI runs, in the same order, on this computer.

.DESCRIPTION
    Steps (run all by default; pick some with -Steps):
      fmt-check   cargo fmt --check. Informational until the code base has been formatted once; -StrictFmt makes it fail the gate.
      clippy      cargo clippy over the workspace. Informational; -StrictClippy makes it fail the gate.
      tests       per crate, every crate of the workspace: aqua-tool-host, aqua-native-tools, aqua-mcp-adapters, aqua-audio-adapter, aqua-vision-adapter, aqua-device-monitor,
                  aqua-orb-placement, aqua-cli, aqua-core --lib, aqua-core --tests; then the benchmark harness (python). Ignored tests (the ones that need models or a running
                  core) are not run.
      props       the property and fuzz tests of card OA13-01 (a group of the tests step, to run on their own in a minute or two): the case loop (tool host), the path guard, the edit
                  engine and the diff reader (native tools), the reader of a server's messages and the schemas (mcp adapters), the splitter, the templates, the sources and the
                  planner's reply reader (core). Seeded and with a fixed number of cases; -PropCases n runs n cases of each, -PropSeed s another seed (a failure prints both).
      stress      the concurrency tests of card OA13-01 (a group of the tests step): the tool host with a crowd of callers, the approval broker of the browser, the queue, the flow store
                  and the run store.
      serial      the tests that change the environment or a static of the process (the module locks of the agent loop, the model server, the planner, the doctor, the queue...), run one
                  at a time (--test-threads=1). Not part of the default list: it is the answer to a test that fails in a full parallel run and passes alone. See documentation/TESTING.md, section 11.
      tests-shell the desktop shell crate (apps/aqua-desktop/src-tauri): not a workspace member, its own Cargo.lock, so it builds in a target folder of its own (<target>\desktop-shell).
                  Its build script needs the built UI (apps/aqua-desktop/dist), the core as a sidecar (src-tauri/bin) and the resource folders named in tauri.conf.json: a clean
                  checkout has none of them, and the step then names what is missing.
      ui          npm ci (only when node_modules is missing), npm run build, npm test, in apps/aqua-desktop.
      ignored     scripts/check_ignored.py (and its self-test): every #[ignore] on a Rust test has a reason string that says what the test needs. Runs nothing, no build.
      docs-check  scripts/check_docs.py (and its self-test): front matter, links, referenced paths, the index, scripts named, skills, the hard rules of the agent files,
                  command shapes that are always wrong. Then the secret scan's own self-test. Instant, no build.
      scripts     scripts/check_scripts.ps1 (and its self-test): every .ps1, .py and .sh in the repository still parses. Runs nothing, no build. Then the unit tests of
                  the helper scripts (scripts/tests: the tracked-data check and the pre-commit hook's path rule), which build nothing either.
      tracked-data  scripts/check_tracked_data.py: no runtime data (databases, the file index, logs, device state) is tracked by git. REPORT ONLY for now: it lists the
                  offenders and the gate still passes (the files that are tracked today are the owner's to take out with `git rm --cached`); -StrictData makes it fail the gate.
      tree-clean  what the tests left in data\ and logs\ (task QA-04): `git status --porcelain --untracked-files=all --ignored -- data logs` (with the size and time of each file listed)
                  before the tests step and after it, and the difference. REPORT ONLY for now (a test that reaches the real data folder is also listed, while it runs, by the
                  data-folder tripwire: AQUA_TEST_STRICT_DATA=1 makes that test fail); -StrictTree makes this step fail the gate. Needs the tests step in the same run.
      docs        the generated-documents guard (docs_gen) and the capability register check.

    The disk of this machine is small, so when the free space gets short (below twice -MinFreeGB) the test executables and debug symbols
    are deleted between crates (they are linked again on the next run; -AlwaysClean does it every time) and so is the incremental compilation
    folder, target\debug\incremental, the biggest part of what is left (the next build of a crate is slower, nothing else is lost). The gate stops
    with a clear message when the free space falls below -MinFreeGB instead of failing in the middle of a link. -NoIncremental builds without
    that folder at all (CARGO_INCREMENTAL=0): slower builds, a smaller target. A -DryRun deletes nothing. -SelfTest proves this clean-up on a throw-away
    folder (the executables and the incremental folder go when the space is short, only the executables when it is not, a junction inside is removed and
    not followed, a dry run deletes nothing, the gate stops with its message); the scripts step runs it.

.EXAMPLE
    powershell -File scripts/ci.ps1
    powershell -File scripts/ci.ps1 -Steps tests,docs
    powershell -File scripts/ci.ps1 -Steps ignored
    powershell -File scripts/ci.ps1 -TargetDir D:\aqua-target -MinFreeGB 10
    powershell -File scripts/ci.ps1 -DryRun

.NOTES
    Never builds or touches target\release, so a running Aqua core is not disturbed. Exit code 0 only when every step that counts passed.
#>
param(
    [string[]]$Steps = @('fmt-check', 'clippy', 'tests', 'tests-shell', 'tree-clean', 'ui', 'docs-check', 'scripts', 'ignored', 'tracked-data', 'docs'),
    # CARGO_TARGET_DIR: a drive with room (default: target\ in the repository).
    [string]$TargetDir = '',
    [double]$MinFreeGB = 6,
    [switch]$StrictFmt,
    [switch]$StrictClippy,
    # Make the tracked-data step fail the gate (it only reports until the owner has taken the runtime files out of git).
    [switch]$StrictData,
    # Make the tree-clean step fail the gate (it only reports while the tests that write to the real data folder are being found and fixed).
    [switch]$StrictTree,
    # Delete the test executables after every crate, not only when the disk is getting full (slower: they are linked again next time).
    [switch]$AlwaysClean,
    # Build without incremental compilation data (CARGO_INCREMENTAL=0): slower builds, but target\debug\incremental (the biggest folder of the debug tree) is never made.
    [switch]$NoIncremental,
    # Cases for every property test (AQUA_PROP_CASES) and the seed of their dice (AQUA_PROP_SEED): a longer hunt than the default, which is a fixed number of cases and a fixed seed.
    [int]$PropCases = 0,
    [string]$PropSeed = '',
    # Prove the disk clean-up on a throw-away folder and exit (no cargo, no test is run).
    [switch]$SelfTest,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

# `-File` hands over "tests,docs" as one string.
$Steps = @($Steps | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ })
$known = @('fmt-check', 'clippy', 'tests', 'props', 'stress', 'serial', 'tests-shell', 'tree-clean', 'ui', 'docs-check', 'scripts', 'ignored', 'tracked-data', 'docs')
foreach ($s in $Steps) {
    if ($known -notcontains $s) { Write-Error "Unknown step '$s'. Steps: $($known -join ', ')"; exit 2 }
}

if ($TargetDir) { $env:CARGO_TARGET_DIR = (New-Item -ItemType Directory -Force -Path $TargetDir).FullName }
if ($NoIncremental) { $env:CARGO_INCREMENTAL = '0' }
if ($PropCases -gt 0) { $env:AQUA_PROP_CASES = "$PropCases" }
if ($PropSeed) { $env:AQUA_PROP_SEED = $PropSeed }
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo 'target' }
# The desktop shell crate is not a workspace member and has its own Cargo.lock, so it never shares a build folder with the others.
$shellTarget = Join-Path $target 'desktop-shell'
$results = New-Object System.Collections.Generic.List[object]

function Get-FreeGB([string]$path) {
    $full = [System.IO.Path]::GetFullPath($path)
    $root = [System.IO.Path]::GetPathRoot($full)
    return [math]::Round(([System.IO.DriveInfo]$root).AvailableFreeSpace / 1GB, 1)
}

# The junctions and links of directories below a folder, found without going into them.
function Find-LinksInside([string]$dir, $found) {
    foreach ($sub in [System.IO.Directory]::EnumerateDirectories($dir)) {
        if (([System.IO.File]::GetAttributes($sub) -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) { $found.Add($sub) } else { Find-LinksInside $sub $found }
    }
}

# Removes a build folder without following a junction or a link inside it (CLAUDE.md: a folder that may hold junctions is never deleted recursively as it is). Each link is removed on its
# own first, which takes the link and not what it points at; neither Remove-Item -Recurse of Windows PowerShell 5.1 nor [IO.Directory]::Delete(path, $true) of .NET Framework can be trusted with one.
function Remove-BuildFolder([string]$path) {
    if (-not (Test-Path -LiteralPath $path)) { return }
    try {
        if (([System.IO.File]::GetAttributes($path) -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) { [System.IO.Directory]::Delete($path); return }
        $links = New-Object System.Collections.Generic.List[string]
        Find-LinksInside $path $links
        foreach ($link in $links) { [System.IO.Directory]::Delete($link) }
        [System.IO.Directory]::Delete($path, $true)
    } catch { Write-Host "  could not remove ${path}: $($_.Exception.Message)" -ForegroundColor Yellow }
}

# Test executables and symbols are the bulk of target\debug; they are rebuilt when needed. The incremental compilation folder is most of the rest
# (2.3 GB of the 3.7 GB debug tree, measured by the OpenCode audit), so it goes too when the free space is below twice -MinFreeGB: the next build of
# a crate is slower, nothing else is lost. Both build folders are cleaned: the workspace's and the desktop shell crate's.
function Clear-TestBinaries {
    foreach ($root in @($target, $shellTarget)) {
        $deps = Join-Path $root 'debug\deps'
        if (Test-Path $deps) {
            Get-ChildItem -Path $deps -File -ErrorAction SilentlyContinue | Where-Object { $_.Extension -in '.exe', '.pdb' } | Remove-Item -Force -ErrorAction SilentlyContinue
        }
        if ((Test-Path $root) -and ((Get-FreeGB $root) -lt ($MinFreeGB * 2))) { Remove-BuildFolder (Join-Path $root 'debug\incremental') }
    }
}

# After a crate: linking a test executable takes the room, so they are cleared when space is getting short (or always, with -AlwaysClean).
function Clear-AfterCrate {
    if (-not $DryRun -and ($AlwaysClean -or (Get-FreeGB $target) -lt ($MinFreeGB * 2))) { Clear-TestBinaries }
}

function Assert-Disk([string]$before) {
    # A dry run builds nothing and must delete nothing.
    if ($DryRun) { return }
    $free = Get-FreeGB $target
    if ($free -lt $MinFreeGB) {
        Clear-TestBinaries
        $free = Get-FreeGB $target
    }
    if ($free -lt $MinFreeGB) {
        throw "Only $free GB is free on the drive that holds '$target' (the gate needs $MinFreeGB GB before '$before', after clearing the test executables and the incremental folder). Free some space, use -TargetDir on a drive with room, or -NoIncremental."
    }
}

# What is in data\ and logs\ as git sees it (changed, new and ignored files), each with its size and time, so that a file changed twice shows too.
function Get-RuntimeTreeState {
    $state = @{}
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { $lines = @(& git status --porcelain --untracked-files=all --ignored -- data logs 2>$null) } catch { $lines = @() } finally { $ErrorActionPreference = $previous }
    foreach ($line in $lines) {
        if (-not $line -or $line.Length -lt 4) { continue }
        $path = $line.Substring(3).Trim('"')
        $stamp = ''
        $full = Join-Path $repo $path
        if (Test-Path -LiteralPath $full -PathType Leaf) {
            $file = Get-Item -LiteralPath $full
            $stamp = "$($file.Length) bytes, $($file.LastWriteTimeUtc.ToString('o'))"
        } elseif (Test-Path -LiteralPath $full -PathType Container) {
            # An ignored folder is listed as one line: its own time changes when a file is added to it or taken out.
            $stamp = "folder, $((Get-Item -LiteralPath $full).LastWriteTimeUtc.ToString('o'))"
        }
        $state[$path] = "$($line.Substring(0, 2)) $stamp"
    }
    return $state
}

# The lines that say what differs between two states: a file that is new, gone, or not what it was.
function Compare-RuntimeTreeState($before, $after) {
    $out = New-Object System.Collections.Generic.List[string]
    foreach ($path in ($after.Keys | Sort-Object)) {
        if (-not $before.ContainsKey($path)) { $out.Add("new      $path") }
        elseif ($before[$path] -ne $after[$path]) { $out.Add("changed  $path") }
    }
    foreach ($path in ($before.Keys | Sort-Object)) {
        if (-not $after.ContainsKey($path)) { $out.Add("gone     $path") }
    }
    return ,$out
}

# The paths tauri.conf.json names that the Tauri build script must find (the built UI, the core as a sidecar named with the Rust host target, the resource folders), as the ones that are not there.
function Get-ShellPrerequisitesMissing {
    $missing = New-Object System.Collections.Generic.List[string]
    try {
        $tauri = Join-Path $repo 'apps\aqua-desktop\src-tauri'
        $conf = Get-Content -Raw -LiteralPath (Join-Path $tauri 'tauri.conf.json') | ConvertFrom-Json
        if ($conf.build.frontendDist -and -not (Test-Path -LiteralPath (Join-Path $tauri $conf.build.frontendDist))) { $missing.Add([string]$conf.build.frontendDist) }
        $triple = $null
        $previous = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try { $triple = ((& rustc -vV 2>$null | Select-String '^host:').ToString() -split '\s+')[1] } catch { $triple = $null } finally { $ErrorActionPreference = $previous }
        foreach ($bin in @($conf.bundle.externalBin)) {
            if ($bin -and $triple -and -not (Test-Path -LiteralPath (Join-Path $tauri "$bin-$triple.exe"))) { $missing.Add("$bin-$triple.exe") }
        }
        if ($conf.bundle.resources) {
            foreach ($name in $conf.bundle.resources.PSObject.Properties.Name) {
                if (-not (Test-Path -LiteralPath (Join-Path $tauri $name))) { $missing.Add($name) }
            }
        }
    } catch { }
    return $missing.ToArray()
}

# Runs one command, shows its output, and returns whether it succeeded.
function Invoke-Native([string]$exe, [string[]]$arguments) {
    $line = "$exe $($arguments -join ' ')"
    if ($DryRun) { Write-Host "  [dry run] $line"; return $true }
    Write-Host "  > $line"
    # Out-Host keeps the command's output on the screen and out of this function's return value.
    & $exe @arguments | Out-Host
    return ($LASTEXITCODE -eq 0)
}

# One step: it counts toward the result unless `-Informational`.
function Invoke-Step([string]$name, [scriptblock]$body, [switch]$Informational) {
    Write-Host ""
    Write-Host "== $name ==" -ForegroundColor Cyan
    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    $ok = $false
    $note = ''
    try { $ok = [bool](& $body) } catch { $ok = $false; $note = $_.Exception.Message; Write-Host "  $note" -ForegroundColor Red }
    $clock.Stop()
    $status = if ($ok) { 'passed' } elseif ($Informational) { 'failed (informational)' } else { 'FAILED' }
    $results.Add([pscustomobject]@{ Step = $name; Status = $status; Seconds = [math]::Round($clock.Elapsed.TotalSeconds, 0); Counts = (-not $Informational) })
    $colour = if ($ok) { 'Green' } elseif ($Informational) { 'Yellow' } else { 'Red' }
    Write-Host "== $name $status ($([math]::Round($clock.Elapsed.TotalSeconds, 0)) s) ==" -ForegroundColor $colour
}

if ($SelfTest) {
    # The disk clean-up on throw-away target folders under the temp folder: nothing else is touched, no cargo runs.
    $failures = New-Object System.Collections.Generic.List[string]
    $work = Join-Path ([System.IO.Path]::GetTempPath()) ('aqua_ci_selftest_' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    function Expect([string]$what, [bool]$ok) { if (-not $ok) { $failures.Add($what) } }
    function New-FakeTarget([string]$name, [bool]$withJunction) {
        $t = Join-Path $work $name
        foreach ($d in 'debug\deps', 'debug\incremental\crate-1\session', 'debug\build') { New-Item -ItemType Directory -Force -Path (Join-Path $t $d) | Out-Null }
        foreach ($f in 'debug\deps\a.exe', 'debug\deps\a.pdb', 'debug\deps\liba.rlib', 'debug\incremental\crate-1\session\dep-graph.bin', 'debug\build\keep.txt') { Set-Content -LiteralPath (Join-Path $t $f) -Value 'x' }
        if ($withJunction) { New-Item -ItemType Junction -Path (Join-Path $t 'debug\incremental\crate-1\link') -Target (Join-Path $work 'decoy') | Out-Null }
        return $t
    }
    try {
        New-Item -ItemType Directory -Force -Path (Join-Path $work 'decoy') | Out-Null
        Set-Content -LiteralPath (Join-Path $work 'decoy\keep.txt') -Value 'must survive'

        # The space is short: the executables, the symbols and the incremental folder of both targets go (a junction inside is removed, its target stays); the rest stays.
        $target = New-FakeTarget 'short' $true
        $shellTarget = New-FakeTarget 'short-shell' $false
        $MinFreeGB = 1000000
        Clear-TestBinaries
        foreach ($t in @($target, $shellTarget)) {
            Expect "short: $t executable removed" (-not (Test-Path (Join-Path $t 'debug\deps\a.exe')))
            Expect "short: $t symbols removed" (-not (Test-Path (Join-Path $t 'debug\deps\a.pdb')))
            Expect "short: $t incremental folder removed" (-not (Test-Path (Join-Path $t 'debug\incremental')))
            Expect "short: $t library kept" (Test-Path (Join-Path $t 'debug\deps\liba.rlib'))
            Expect "short: $t build folder kept" (Test-Path (Join-Path $t 'debug\build\keep.txt'))
        }
        Expect 'short: what a junction pointed at is kept' (Test-Path (Join-Path $work 'decoy\keep.txt'))

        # The space is plentiful: only the executables and symbols go.
        $target = New-FakeTarget 'plenty' $false
        $shellTarget = New-FakeTarget 'plenty-shell' $false
        $MinFreeGB = 0
        Clear-TestBinaries
        Expect 'plenty: executable removed' (-not (Test-Path (Join-Path $target 'debug\deps\a.exe')))
        Expect 'plenty: incremental folder kept' (Test-Path (Join-Path $target 'debug\incremental\crate-1\session\dep-graph.bin'))
        Expect 'plenty: shell incremental folder kept' (Test-Path (Join-Path $shellTarget 'debug\incremental\crate-1\session\dep-graph.bin'))

        # A dry run deletes nothing, even when asked to clean always and the space is short, and does not stop.
        $target = New-FakeTarget 'dry' $false
        $shellTarget = New-FakeTarget 'dry-shell' $false
        $MinFreeGB = 1000000
        $DryRun = $true
        $AlwaysClean = $true
        Clear-AfterCrate
        $dryStopped = $false
        try { Assert-Disk 'a crate' } catch { $dryStopped = $true }
        Expect 'dry run: nothing deleted' ((Test-Path (Join-Path $target 'debug\deps\a.exe')) -and (Test-Path (Join-Path $target 'debug\incremental\crate-1\session\dep-graph.bin')))
        Expect 'dry run: the disk check does not stop it' (-not $dryStopped)
        $DryRun = $false
        $AlwaysClean = $false

        # Short of space after the clean-up: the gate stops with its message, and cleaned first.
        $target = New-FakeTarget 'stop' $false
        $shellTarget = New-FakeTarget 'stop-shell' $false
        $message = ''
        try { Assert-Disk 'aqua-core --lib' } catch { $message = $_.Exception.Message }
        Expect 'stop: the gate stops with a message that names the crate and the way out' (($message -like '*Only * GB is free*') -and ($message -like '*aqua-core --lib*') -and ($message -like '*-NoIncremental*'))
        Expect 'stop: the executables were cleared first' (-not (Test-Path (Join-Path $target 'debug\deps\a.exe')))
    } catch {
        $failures.Add("the self-test itself broke: $($_.Exception.Message)")
    } finally {
        # Junctions first, so that nothing is ever followed out of the throw-away folder.
        foreach ($link in @(Get-ChildItem -LiteralPath $work -Recurse -Force -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Attributes -band [System.IO.FileAttributes]::ReparsePoint })) {
            try { [System.IO.Directory]::Delete($link.FullName) } catch { }
        }
        try { [System.IO.Directory]::Delete($work, $true) } catch { }
    }
    foreach ($f in $failures) { Write-Host "SELF-TEST FAILED: $f" -ForegroundColor Red }
    if ($failures.Count -eq 0) { Write-Host 'self-test passed (the disk clean-up: short space, plenty of space, a junction inside, a dry run, and the message when the gate stops).' -ForegroundColor Green }
    exit $(if ($failures.Count -eq 0) { 0 } else { 1 })
}

if ($Steps -contains 'fmt-check') {
    Invoke-Step 'fmt-check' { Invoke-Native 'cargo' @('fmt', '--all', '--', '--check') } -Informational:(-not $StrictFmt)
}

if ($Steps -contains 'clippy') {
    Invoke-Step 'clippy' { Assert-Disk 'clippy'; Invoke-Native 'cargo' @('clippy', '--workspace', '--all-targets') } -Informational:(-not $StrictClippy)
}

# The state of data\ and logs\ before the tests run, for the tree-clean step below.
$treeBefore = $null
if (($Steps -contains 'tests') -and ($Steps -contains 'tree-clean') -and -not $DryRun) { $treeBefore = Get-RuntimeTreeState }

if ($Steps -contains 'tests') {
    # One crate at a time: the linker needs the room, and a failure then names its crate.
    $suites = @(
        @{ Name = 'aqua-tool-host'; Args = @('test', '-p', 'aqua-tool-host') },
        @{ Name = 'aqua-native-tools'; Args = @('test', '-p', 'aqua-native-tools') },
        @{ Name = 'aqua-mcp-adapters'; Args = @('test', '-p', 'aqua-mcp-adapters') },
        @{ Name = 'aqua-audio-adapter'; Args = @('test', '-p', 'aqua-audio-adapter') },
        # The small crates (the audit found these four outside the gate): cheap, so a failure in one shows before the long aqua-core builds.
        @{ Name = 'aqua-vision-adapter'; Args = @('test', '-p', 'aqua-vision-adapter') },
        @{ Name = 'aqua-device-monitor'; Args = @('test', '-p', 'aqua-device-monitor') },
        @{ Name = 'aqua-orb-placement'; Args = @('test', '-p', 'aqua-orb-placement') },
        # aqua-cli has no test of its own yet, but the run builds it, so a change that breaks the command line is found by the gate.
        @{ Name = 'aqua-cli'; Args = @('test', '-p', 'aqua-cli') },
        @{ Name = 'aqua-core --lib'; Args = @('test', '-p', 'aqua-core', '--lib') },
        # With the test-support feature, so that the shared test support (core/src/test_support) is built for the integration tests too and cannot rot.
        @{ Name = 'aqua-core --tests'; Args = @('test', '-p', 'aqua-core', '--tests', '--features', 'test-support') },
        # AquaBench's own tests: a fake core, no model, nothing outside a temp folder.
        @{ Name = 'eval harness'; Exe = 'python'; Args = @('-m', 'unittest', 'discover', '-s', 'scripts/eval/tests') },
        # The measurement of the tool-call interface (card OA2-03): the validator, the variants and a run against a fake model server, no model.
        @{ Name = 'bench scripts'; Exe = 'python'; Args = @('-m', 'unittest', 'discover', '-s', 'scripts/bench/tests') }
    )
    foreach ($suite in $suites) {
        $s = $suite
        Invoke-Step "tests: $($s.Name)" {
            Assert-Disk $s.Name
            $exe = if ($s.Exe) { $s.Exe } else { 'cargo' }
            $r = Invoke-Native $exe $s.Args
            Clear-AfterCrate
            $r
        }
    }
}

# Groups of the tests, to run on their own (card OA13-01, tasks QA-05 and QA-12). Nothing is moved out of the `tests` step and no test is changed: these are the same tests, named, so that
# the property and fuzz tests, the concurrency tests, and the tests that touch the environment can be run alone and quickly. The list of what is in each group is in
# documentation/TESTING.md, section 11. `-PropCases` and `-PropSeed` (above) reach the property tests of all of them.
$groups = [ordered]@{
    props  = @(
        @{ Name = 'aqua-tool-host: the case loop'; Args = @('test', '-p', 'aqua-tool-host', '--test', 'prop_kit_test') },
        @{ Name = 'aqua-native-tools: path guard, edit engine, diff reader'; Args = @('test', '-p', 'aqua-native-tools', '--test', 'fs_properties') },
        @{ Name = 'aqua-native-tools: patch that fails at a write'; Args = @('test', '-p', 'aqua-native-tools', '--lib', 'fs::provider::properties') },
        @{ Name = 'aqua-mcp-adapters: schemas, names, server file'; Args = @('test', '-p', 'aqua-mcp-adapters', '--test', 'frame_properties') },
        @{ Name = 'aqua-mcp-adapters: the reader of messages'; Args = @('test', '-p', 'aqua-mcp-adapters', '--lib', 'frame_property_tests') },
        @{ Name = 'aqua-core: text'; Args = @('test', '-p', 'aqua-core', '--features', 'test-support', '--test', 'text_properties') },
        @{ Name = 'aqua-core: the planner reply reader'; Args = @('test', '-p', 'aqua-core', '--lib', 'parse_properties_tests') }
    )
    stress = @(
        @{ Name = 'aqua-tool-host: a crowd of callers'; Args = @('test', '-p', 'aqua-tool-host', '--test', 'concurrency') },
        @{ Name = 'aqua-mcp-adapters: the approval broker of the browser'; Args = @('test', '-p', 'aqua-mcp-adapters', '--test', 'broker_concurrency') },
        @{ Name = 'aqua-core: queue, flow store, run store'; Args = @('test', '-p', 'aqua-core', '--features', 'test-support', '--test', 'concurrency_test') }
    )
    serial = @(
        # The modules whose tests set an environment variable or a static of the process (each also has a lock of its own; here they run one at a time as well).
        @{ Name = 'aqua-core --lib: the env-coupled modules, one test at a time'; Args = @('test', '-p', 'aqua-core', '--lib', '--', '--test-threads=1', 'modules::browser_agent::', 'modules::llm_router::', 'modules::coding_session::', 'modules::coding_runner::', 'modules::planner::', 'modules::context::', 'flows::run_tests', 'laya_validator::', 'doctor::', 'tool_host::', 'follow_up::', 'queue::', 'pipeline::', 'skills_inbox::', 'reminders::', 'live_weather::', 'vault::') },
        @{ Name = 'aqua-mcp-adapters: the listener and protocol tests, one at a time'; Args = @('test', '-p', 'aqua-mcp-adapters', '--test', 'listener_check', '--test', 'mcp_protocol', '--test', 'user_servers', '--', '--test-threads=1') }
    )
}
foreach ($group in $groups.Keys) {
    if ($Steps -contains $group) {
        $suitesOfGroup = $groups[$group]
        Invoke-Step $group {
            $allPassed = $true
            foreach ($s in $suitesOfGroup) {
                Assert-Disk $s.Name
                Write-Host "  -- $($s.Name)"
                if (-not (Invoke-Native 'cargo' $s.Args)) { $allPassed = $false }
                Clear-AfterCrate
            }
            $allPassed
        }
    }
}

if ($Steps -contains 'tests-shell') {
    # The desktop shell crate (apps/aqua-desktop/src-tauri): not a workspace member, its own Cargo.lock and its own target folder. Its build script (tauri-build) refuses to
    # build when the built UI, the core as a sidecar or a resource folder named in tauri.conf.json is missing, and a clean checkout has none of them: say which, then let cargo try.
    Invoke-Step 'tests-shell' {
        Assert-Disk 'tests-shell'
        $missing = @(Get-ShellPrerequisitesMissing)
        if ($missing.Count -gt 0) {
            Write-Host "  note: the Tauri build script of the shell crate needs these paths, and they are not there ($($missing -join '; ')). Build the UI (npm run build in apps\aqua-desktop) and the core sidecar (build_release.ps1) first."
        }
        $r = Invoke-Native 'cargo' @('test', '--manifest-path', 'apps/aqua-desktop/src-tauri/Cargo.toml', '--target-dir', $shellTarget)
        Clear-AfterCrate
        $r
    }
}

if ($Steps -contains 'tree-clean') {
    # Report only until the tests that write to the real data folder have been found (the data-folder tripwire lists them) and fixed.
    Invoke-Step 'tree-clean' {
        if ($DryRun) { Write-Host "  [dry run] compare data\ and logs\ before and after the tests step"; return $true }
        if ($null -eq $treeBefore) { Write-Host "  no snapshot from before the tests (the tests step did not run in this call): nothing to compare"; return $true }
        $diff = Compare-RuntimeTreeState $treeBefore (Get-RuntimeTreeState)
        if ($diff.Count -eq 0) { Write-Host "  the tests left data\ and logs\ as they found them"; return $true }
        Write-Host "  the tests changed $($diff.Count) path(s) in data\ or logs\ (a test reached the real data folder, or a running Aqua wrote there meanwhile):"
        $diff | ForEach-Object { Write-Host "    $_" }
        return $false
    } -Informational:(-not $StrictTree)
}

if ($Steps -contains 'ui') {
    Invoke-Step 'ui' {
        $ui = Join-Path $repo 'apps\aqua-desktop'
        Push-Location $ui
        try {
            if (-not (Test-Path 'node_modules')) { if (-not (Invoke-Native 'npm' @('ci'))) { return $false } }
            if (-not (Invoke-Native 'npm' @('run', 'build'))) { return $false }
            return (Invoke-Native 'npm' @('test'))
        } finally { Pop-Location }
    }
}

if ($Steps -contains 'docs-check') {
    Invoke-Step 'docs-check' {
        # The self-test first: it proves the check still catches what it should, so a pass below means something.
        if (-not (Invoke-Native 'python' @('scripts/check_docs.py', '--self-test'))) { return $false }
        if (-not (Invoke-Native 'python' @('scripts/check_docs.py'))) { return $false }
        # The secret scan that runs before a commit proves on synthetic lines that it still finds each kind of credential and leaves ordinary text alone.
        return (Invoke-Native 'powershell' @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'scripts/hooks/pre-commit.ps1', '-SelfTest'))
    }
}

if ($Steps -contains 'scripts') {
    Invoke-Step 'scripts' {
        # The disk clean-up of this gate, on throw-away folders.
        if (-not (Invoke-Native 'powershell' @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'scripts/ci.ps1', '-SelfTest'))) { return $false }
        if (-not (Invoke-Native 'powershell' @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'scripts/check_scripts.ps1', '-SelfTest'))) { return $false }
        if (-not (Invoke-Native 'powershell' @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'scripts/check_scripts.ps1'))) { return $false }
        # The two helper scripts that move or report things prove on throw-away folders that they still do what they say.
        if (-not (Invoke-Native 'powershell' @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'scripts/tree_status.ps1', '-SelfTest'))) { return $false }
        if (-not (Invoke-Native 'powershell' @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'scripts/model_swap_smoke.ps1', '-SelfTest'))) { return $false }
        # The release check runs its whole flow against a fake core and a fake doctor (no build, no real core, one free port).
        if (-not (Invoke-Native 'powershell' @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'scripts/release_smoke.ps1', '-SelfTest'))) { return $false }
        # The size and build-time profile of the test executables (card OA13-01, QA-12): the reading of cargo's JSON and the sums, on made-up lines and throw-away files (nothing is built).
        if (-not (Invoke-Native 'powershell' @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'scripts/profile_tests.ps1', '-SelfTest'))) { return $false }
        # The tracked-data check and the hook's path rule, on throw-away git repositories (nothing is installed, nothing outside a temp folder is touched).
        return (Invoke-Native 'python' @('-m', 'unittest', 'discover', '-s', 'scripts/tests'))
    }
}

if ($Steps -contains 'ignored') {
    # An ignored test without a reason is a test nobody knows how to run, or one that was switched off because it failed: the gate fails on it.
    Invoke-Step 'ignored' {
        # The self-test first: it proves the check still finds each kind of bare #[ignore], so a pass below means something.
        if (-not (Invoke-Native 'python' @('scripts/check_ignored.py', '--self-test'))) { return $false }
        return (Invoke-Native 'python' @('scripts/check_ignored.py'))
    }
}

if ($Steps -contains 'tracked-data') {
    # Report only until the owner has run `git rm --cached` for the runtime files that are tracked today: the offenders are listed, the gate does not fail (-StrictData makes it).
    Invoke-Step 'tracked-data' { Invoke-Native 'python' @('scripts/check_tracked_data.py') } -Informational:(-not $StrictData)
}

if ($Steps -contains 'docs') {
    Invoke-Step 'docs' {
        Assert-Disk 'docs'
        # The generated documents must match the code (regenerate with: set AQUA_UPDATE_DOCS=1, then run this test).
        if (-not (Invoke-Native 'cargo' @('test', '-p', 'aqua-core', '--lib', 'docs_gen'))) { return $false }
        return (Invoke-Native 'cargo' @('test', '-p', 'aqua-core', '--lib', 'capabilities'))
    }
}

# The commit hook (the secret scan) is in no step and is off in a fresh clone: say so, without failing the gate. Not on a CI runner, where nobody commits.
if (-not $DryRun -and -not $env:CI) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { $hooksPath = (& git config --local --get core.hooksPath 2>$null) } catch { $hooksPath = $null } finally { $ErrorActionPreference = $previous }
    if (-not $hooksPath) {
        Write-Host ""
        Write-Host "Note: the commit hook (the secret scan) is not installed in this clone. Install it once: powershell -File scripts/install_hooks.ps1" -ForegroundColor Yellow
    }
}

Write-Host ""
Write-Host "Summary" -ForegroundColor Cyan
$results | Format-Table Step, Status, Seconds -AutoSize | Out-String | Write-Host
$failed = @($results | Where-Object { $_.Counts -and $_.Status -ne 'passed' })
if ($failed.Count -gt 0) {
    Write-Host "The gate FAILED: $($failed.Step -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host "The gate passed." -ForegroundColor Green
exit 0
