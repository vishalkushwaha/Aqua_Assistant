param(
    [string]$Output = "demo/video/aqua-company-brain-demo.mp4",
    [string]$Voice = "af_heart",
    [double]$SpeechSpeed = 0.96,
    [switch]$SkipCapture
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$frames = Join-Path $repoRoot "demo/video/frames"
$videoDir = Split-Path -Parent (Join-Path $repoRoot $Output)
$workDir = Join-Path $videoDir "render-work"
$concatVideo = Join-Path $workDir "frames.txt"
$concatAudio = Join-Path $workDir "audio.txt"
$captions = Join-Path $workDir "captions.srt"
$finalVideo = Join-Path $repoRoot $Output
$kokoroPython = Join-Path $repoRoot "vendor/kokoro-tts/.venv/Scripts/python.exe"
$kokoroDaemon = Join-Path $repoRoot "vendor/kokoro-tts/kokoro_daemon.py"
$kokoroModel = Join-Path $repoRoot "data/models/kokoro/kokoro-v1.0.onnx"
$kokoroVoices = Join-Path $repoRoot "data/models/kokoro/voices-v1.0.bin"

foreach ($required in @($kokoroPython, $kokoroDaemon, $kokoroModel, $kokoroVoices)) {
    if (-not (Test-Path -LiteralPath $required)) {
        throw "Natural Kokoro narration dependency is missing: $required"
    }
}

$scenes = @(
    @{ File="01-overview.png"; Text="Meet Aqua, a privacy focused Windows desktop assistant. Its console brings voice, automation, knowledge, security, and device controls into one place." },
    @{ File="02-company-brain-answer.png"; Text="Company Brain uses FalkorDB as a real reasoning layer. It traverses tickets, projects, owners, decisions, and sources to produce a grounded answer with evidence." },
    @{ File="03-company-brain-action.png"; Text="Before Aqua acts, it revalidates the ownership path in the graph. The approved assignment and its result are then written back as auditable company knowledge." },
    @{ File="04-company-brain-memory.png"; Text="Questions, answers, citations, outcomes, state revisions, and agent handoffs persist across sessions. Tenant scoped graph keys keep company data separated." },
    @{ File="05-memory-graph.png"; Text="The Memory Graph explorer makes associations and skill coordination visible, complementing Company Brain's live FalkorDB retrieval." },
    @{ File="06-capabilities.png"; Text="Beyond Company Brain, Aqua brings together workflows, scheduled routines, app approvals, privacy controls, private memory, preferences and learning, reviewed skills, local file search, a secure vault, and device settings." },
    @{ File="07-voice-profiles.png"; Text="Voice Profiles selects local text to speech voices and audio styles. This demonstration itself now includes an audible offline narration track." },
    @{ File="08-voice-security.png"; Text="Voice Security adds a custom wake phrase, speaker enrollment, and biometric verification for protected assistant actions." },
    @{ File="09-platform-support.png"; Text="Aqua is currently presented and validated as Windows desktop software. FalkorDB can run locally in Docker or through a configured cloud endpoint. Mac OS and Linux desktop releases are not claimed in this demo." }
)

New-Item -ItemType Directory -Path $videoDir -Force | Out-Null
New-Item -ItemType Directory -Path $workDir -Force | Out-Null

$daemonProcess = $null
try {
    $health = Invoke-RestMethod -Uri "http://127.0.0.1:8766/health" -TimeoutSec 2
} catch {
    $daemonOut = Join-Path $workDir "kokoro-daemon.log"
    $daemonError = Join-Path $workDir "kokoro-daemon-error.log"
    $daemonProcess = Start-Process -FilePath $kokoroPython -ArgumentList @($kokoroDaemon) -WorkingDirectory (Split-Path -Parent $kokoroDaemon) -WindowStyle Hidden -RedirectStandardOutput $daemonOut -RedirectStandardError $daemonError -PassThru
    $health = $null
    for ($attempt = 0; $attempt -lt 90 -and -not $health; $attempt++) {
        Start-Sleep -Seconds 1
        try { $health = Invoke-RestMethod -Uri "http://127.0.0.1:8766/health" -TimeoutSec 2 } catch { }
    }
}
if (-not $health -or -not $health.model_ready) {
    if ($daemonProcess) { Stop-Process -Id $daemonProcess.Id -Force -ErrorAction SilentlyContinue }
    throw "The Kokoro narration service did not become ready."
}
Write-Host "Kokoro narration provider: $($health.provider) ($($health.selftest_x_realtime)x real time)"

if (-not $SkipCapture) {
    Push-Location $repoRoot
    try {
        node scripts/capture-company-brain-demo.mjs $frames
        if ($LASTEXITCODE -ne 0) { throw "Live browser capture failed." }
    } finally {
        Pop-Location
    }
}

$videoLines = @()
$audioLines = @()
$srtLines = @()
$cursor = 0.0
$index = 0

foreach ($scene in $scenes) {
    $index++
    $framePath = Join-Path $frames $scene.File
    if (-not (Test-Path -LiteralPath $framePath)) { throw "Missing captured frame: $framePath" }

    $speechText = Join-Path $workDir ("speech-{0:D2}.txt" -f $index)
    $rawSpeechWav = Join-Path $workDir ("speech-{0:D2}-kokoro.wav" -f $index)
    $speechWav = Join-Path $workDir ("speech-{0:D2}.wav" -f $index)
    Set-Content -LiteralPath $speechText -Value $scene.Text -Encoding utf8

    $payload = @{ text=$scene.Text; voice=$Voice; lang="en-us"; speed=$SpeechSpeed } | ConvertTo-Json -Compress
    Invoke-WebRequest -Uri "http://127.0.0.1:8766/synthesize" -Method Post -ContentType "application/json" -Body $payload -OutFile $rawSpeechWav -TimeoutSec 180
    if (-not (Test-Path -LiteralPath $rawSpeechWav)) {
        throw "Kokoro narration synthesis failed for scene $index."
    }

    ffmpeg -loglevel error -y -i $rawSpeechWav -af "loudnorm=I=-16:TP=-1.5:LRA=11,apad=pad_dur=1.2" -ar 48000 -ac 2 $speechWav
    if ($LASTEXITCODE -ne 0) { throw "Narration mastering failed for scene $index." }

    $durationText = ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 $speechWav
    $duration = [Math]::Ceiling(([double]::Parse($durationText.Trim(), [Globalization.CultureInfo]::InvariantCulture)) * 10) / 10
    $duration = [Math]::Max($duration, 5.0)

    $videoLines += "file '$($framePath.Replace("'", "''"))'"
    $videoLines += "duration $($duration.ToString([Globalization.CultureInfo]::InvariantCulture))"
    $audioLines += "file '$($speechWav.Replace("'", "''"))'"

    $start = [TimeSpan]::FromSeconds($cursor)
    $end = [TimeSpan]::FromSeconds($cursor + $duration)
    $formatTime = {
        param([TimeSpan]$time)
        "{0:00}:{1:00}:{2:00},{3:000}" -f [Math]::Floor($time.TotalHours), $time.Minutes, $time.Seconds, $time.Milliseconds
    }
    $srtLines += "$index"
    $srtLines += "$(& $formatTime $start) --> $(& $formatTime $end)"
    $srtLines += $scene.Text
    $srtLines += ""
    $cursor += $duration
}

if ($daemonProcess) {
    Stop-Process -Id $daemonProcess.Id -Force -ErrorAction SilentlyContinue
    $daemonProcess = $null
}

$lastFrame = Join-Path $frames $scenes[-1].File
$videoLines += "file '$($lastFrame.Replace("'", "''"))'"
Set-Content -LiteralPath $concatVideo -Value $videoLines -Encoding ascii
Set-Content -LiteralPath $concatAudio -Value $audioLines -Encoding ascii
Set-Content -LiteralPath $captions -Value $srtLines -Encoding utf8

$totalDuration = $cursor.ToString([Globalization.CultureInfo]::InvariantCulture)
$subtitlePath = $captions.Replace('\','/').Replace(':','\:')

Push-Location $repoRoot
try {
    ffmpeg -y -f concat -safe 0 -i $concatVideo -f concat -safe 0 -i $concatAudio -vf "fps=30,scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2,subtitles='$subtitlePath':force_style='FontName=Arial,FontSize=10,PrimaryColour=&H00FFFFFF,OutlineColour=&H00111111,BackColour=&HCC111111,BorderStyle=3,Outline=1,Shadow=0,MarginL=50,MarginR=50,MarginV=22,Alignment=2',format=yuv420p" -t $totalDuration -c:v libx264 -preset medium -crf 20 -c:a aac -ar 48000 -b:a 160k -movflags +faststart $finalVideo
    if ($LASTEXITCODE -ne 0) { throw "FFmpeg video rendering failed." }
} finally {
    Pop-Location
}

Write-Host "Created Kokoro-narrated Aqua showcase at $finalVideo ($([Math]::Round($cursor, 1)) seconds, voice $Voice)."
