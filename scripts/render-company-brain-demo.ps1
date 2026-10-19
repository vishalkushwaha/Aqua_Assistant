param(
    [ValidateSet("Showcase", "Submission", "Complete")]
    [string]$Mode = "Showcase",
    [string]$Output = "",
    [string]$Voice = "af_heart",
    [double]$SpeechSpeed = 0.96,
    [switch]$SkipCapture
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$captureMode = $Mode.ToLowerInvariant()
if (-not $Output) {
    $Output = if ($Mode -eq "Submission") {
        "demo/video/aqua-falkordb-submission-demo.mp4"
    } elseif ($Mode -eq "Complete") {
        "demo/video/aqua-complete-product-walkthrough.mp4"
    } else {
        "demo/video/aqua-company-brain-demo.mp4"
    }
}
$frames = Join-Path $repoRoot "demo/video/frames"
$videoDir = Split-Path -Parent (Join-Path $repoRoot $Output)
$workDir = Join-Path $videoDir "render-work"
$concatVideo = Join-Path $workDir "frames.txt"
$concatAudio = Join-Path $workDir "audio.txt"
$captions = Join-Path $workDir "captions.srt"
$overlays = Join-Path $workDir "overlays.ass"
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

$showcaseScenes = @(
    @{ File="01-overview.png"; Label="PRODUCT OVERVIEW"; Title="Private desktop intelligence"; Text="Meet Aqua, a privacy focused Windows desktop assistant. Its console brings voice, automation, knowledge, security, and device controls into one place." },
    @{ File="02-company-brain-answer.png"; Label="FALKORDB REASONING"; Title="Evidence before answers"; Text="Company Brain uses FalkorDB as a real reasoning layer. It traverses tickets, projects, owners, decisions, and sources to produce a grounded answer with evidence." },
    @{ File="03-company-brain-action.png"; Label="GRAPH AUTHORIZATION"; Title="Verify before acting"; Text="Before Aqua acts, it revalidates the ownership path in the graph. The approved assignment and its result are then written back as auditable company knowledge." },
    @{ File="04-company-brain-memory.png"; Label="PERSISTENT MEMORY"; Title="Context that survives sessions"; Text="Questions, answers, citations, outcomes, state revisions, and agent handoffs persist across sessions. Tenant scoped graph keys keep company data separated." },
    @{ File="05-memory-graph.png"; Label="LIVE GRAPH"; Title="Inspectable knowledge and coordination"; Text="The Memory Graph explorer makes associations and skill coordination visible, complementing Company Brain's live FalkorDB retrieval." },
    @{ File="06-capabilities.png"; Label="COMPLETE CONTROL PANEL"; Title="One assistant, every workflow"; Text="Beyond Company Brain, Aqua brings together workflows, scheduled routines, app approvals, privacy controls, private memory, preferences and learning, reviewed skills, local file search, a secure vault, and device settings." },
    @{ File="07-voice-profiles.png"; Label="ON-DEVICE VOICE"; Title="Natural offline narration"; Text="Voice Profiles selects local text to speech voices and audio styles. This demonstration itself now includes an audible offline narration track." },
    @{ File="08-voice-security.png"; Label="VOICE SECURITY"; Title="Protected assistant actions"; Text="Voice Security adds a custom wake phrase, speaker enrollment, and biometric verification for protected assistant actions." },
    @{ File="09-platform-support.png"; Label="WINDOWS DELIVERY"; Title="Validated desktop workflow"; Text="Aqua is currently presented and validated as Windows desktop software. FalkorDB can run locally in Docker or through a configured cloud endpoint. Mac OS and Linux desktop releases are not claimed in this demo." }
)

$submissionScenes = @(
    @{ File="submission-01-overview.png"; Label="AQUA | GRAPH HACKS"; Title="Graph-grounded private AI"; Text="This is Aqua Company Brain, a working Windows desktop agent built for Graph Hacks. FalkorDB is its required context, reasoning, action, and persistent memory layer." },
    @{ File="submission-02-dataset-ready.png"; Label="INPUT | TRACK 03"; Title="Build the company knowledge graph"; Text="First, Aqua prepares a repeatable synthetic company graph. People, teams, projects, incidents, decisions, documents, and chats are stored as tenant scoped nodes with explicit relationships and source provenance." },
    @{ File="submission-03-grounded-answer.png"; Label="PROCESSING TO OUTPUT | TRACKS 01 + 03"; Title="Ask once. Trace every claim."; Text="Now we ask who should handle the OAuth production incident, which earlier decision affects the fix, and which records support the answer. Aqua traverses ticket to project to owner, and decision to project to source. It identifies Priya Raman, decision D E C zero fourteen, and two exact source records." },
    @{ File="submission-04-graph-action.png"; Label="ACTION | TRACK 01"; Title="Authorization proven by relationship"; Text="The proposed assignment is not trusted blindly. Before acting, Aqua reruns the ownership path in FalkorDB. Only a person who owns the incident's project can be assigned. The completed action, selected owner, support path, and outcome are written back to the graph." },
    @{ File="submission-05-persistent-memory.png"; Label="MEMORY | TRACK 02"; Title="Recall beyond the current session"; Text="Next, Aqua recalls the interaction from a later session. FalkorDB stores the agent, session, question, grounded answer, cited sources, action, and outcome. This is persistent graph memory, not temporary browser state." },
    @{ File="submission-06-agent-handoff.png"; Label="COORDINATION | TRACK 02"; Title="Hand off without losing context"; Text="Aqua then hands the incident to a second response agent through the same tenant scoped graph. The receiving agent gets the grounded incident, decision, and sources, while the handoff itself remains auditable and available in future sessions." },
    @{ File="submission-07-live-graph.png"; Label="PROCESSING EVIDENCE | ALL TRACKS"; Title="Inspect the live graph"; Text="The live Memory Graph confirms the database is connected and populated. The graph includes company knowledge alongside agent tasks, experiences, tools, and skills, and the Cypher console can inspect those relationships directly." },
    @{ File="submission-08-falkordb-refusal.png"; Label="FAIL-CLOSED SAFETY | ALL TRACKS"; Title="No graph means no fabricated answer"; Text="FalkorDB is mandatory, not decorative. Against an unavailable database, Aqua displays the dependency error and produces no fabricated fallback answer. Removing the graph removes retrieval, authorization, memory, and handoff capability." },
    @{ File="submission-09-capabilities.png"; Label="COMPLETE PRODUCT EXPERIENCE"; Title="Beyond the hackathon workflow"; Text="Company Brain runs inside the wider Aqua Assistant: local voice interaction, workflows, scheduled routines, approval and privacy controls, inspectable learning, reviewed skills, private file search, and a secure vault." },
    @{ File="submission-10-platform.png"; Label="DELIVERY & SETUP"; Title="A reproducible Windows workflow"; Text="The validated target is Aqua for Windows desktop, with FalkorDB available through local Docker or a configured cloud endpoint. This working demonstration covers connected reasoning, sourced answers, graph controlled action, persistent memory, tenant isolation, and multi agent coordination." }
)
$completeScenes = @(
    @{ File="complete-01-overview.png"; Label="AQUA | COMPLETE WALKTHROUGH"; Title="One private assistant, end to end"; Text="Meet Aqua, a Windows desktop assistant that combines a local language model, private voice interaction, safe automation, inspectable knowledge, secure files, device controls, and a FalkorDB powered company brain. This walkthrough explains the complete product and then proves all three Graph Hacks tracks." },
    @{ File="complete-02-local-ai.png"; Label="LOCAL LLM | INPUT TO OUTPUT"; Title="Private reasoning with controlled execution"; Text="Aqua's core assistant intelligence is a local G G U F language model hosted by its Llama runtime. Typed text, or speech transcribed locally by Whisper, becomes the input. Aqua retrieves permitted memory, files, or graph evidence, streams the model response, and sends every requested action through deterministic ToolHost policy and approval checks. Kokoro can then speak the final output." },
    @{ File="complete-03-voice.png"; Label="VOICE + DESKTOP EXPERIENCE"; Title="Hands free, local, and identity aware"; Text="The voice pipeline combines a custom wake phrase, Deep Filter Net noise cleanup, Whisper speech recognition, the local language model, and Kokoro neural speech. Voice Profiles selects the delivery style. Voice Security can enroll a master speaker for protected actions. Audio Devices chooses input and output, while the floating neural orb shows listening, thinking, speaking, and screen context states." },
    @{ File="complete-04-automation.png"; Label="AUTOMATION + SAFETY"; Title="Build routines without surrendering control"; Text="Workflows combine tool steps, agent goals, and saved values into repeatable jobs. Automated Routines can be described in natural language, created from templates, scheduled, monitored, and inspected after failure. App Approvals exposes policy, tools, activity, and emergency stop controls. Permissions and Privacy manages trusted projects, browser profiles, consent, autonomy limits, and auditable approvals." },
    @{ File="complete-05-knowledge.png"; Label="PRIVATE KNOWLEDGE + LEARNING"; Title="Inspectable memory that users can change"; Text="Aqua exposes remembered facts, preferences, corrections, and verified experiences instead of hiding them. Users can tune learning and retrieval, review proposed skills, index local files, protect selected documents in the Secure Vault, and remove knowledge. The FalkorDB Memory Graph adds associative recall, graph algorithms, company knowledge, and shared agent state with tenant isolation." },
    @{ File="complete-06-company-graph.png"; Label="TRACK 03 | COMPANY BRAIN INPUT"; Title="Connect the organization before asking"; Text="For Track Three, Aqua creates a repeatable synthetic company graph. People, teams, projects, systems, incidents, decisions, documents, chats, and agents become tenant scoped nodes. Explicit relationships record ownership, team membership, dependencies, affected systems, and supporting evidence. Source hashes, observation times, freshness deadlines, and active state preserve provenance." },
    @{ File="complete-07-grounded-answer.png"; Label="TRACKS 01 + 03 | PROCESSING TO OUTPUT"; Title="Ask once and trace every claim"; Text="The input asks who should handle the OAuth production incident, which earlier decision affects the fix, and which records support the answer. Aqua executes bounded Cypher traversals from ticket to project to owner, and from decision to project to source. The output names Priya Raman, decision D E C zero fourteen, two primary sources, and every relationship hop used as proof." },
    @{ File="complete-08-graph-action.png"; Label="TRACK 01 | CONNECTED ACTION"; Title="The graph authorizes the assignment"; Text="A generated recommendation is not enough to act. Aqua independently reruns the ticket to project to owner relationship in FalkorDB. Only a person proven to own the affected project can receive the incident. After approval, the assignment, selected graph entity, supporting path, and action outcome are stored as auditable records. Invalid owners are rejected." },
    @{ File="complete-09-persistent-memory.png"; Label="TRACK 02 | PERSISTENT MEMORY"; Title="Context survives the current session"; Text="Aqua stores the agent, session, question, grounded answer, cited sources, authorized action, and outcome as connected graph records. A later session recalls the same evidence from FalkorDB rather than browser state. Tenant identifiers isolate organizations, while retention and deletion controls keep the memory inspectable and manageable." },
    @{ File="complete-10-agent-handoff.png"; Label="TRACK 02 | AGENT COORDINATION"; Title="Hand off work without losing evidence"; Text="The Company Brain publishes shared state and transfers the incident to a second response agent through FalkorDB. The receiving agent gets the incident, architectural decision, source identifiers, and prior outcome. The handoff itself remains queryable and auditable, so coordination preserves context without copying an opaque conversation transcript." },
    @{ File="complete-11-live-graph.png"; Label="ALL TRACKS | GRAPH INSPECTION"; Title="See the knowledge, memory, and coordination layer"; Text="The live Memory Graph confirms FalkorDB is connected and populated. Entity filters separate people, projects, sources, questions, answers, actions, and handoffs. The adaptive visualization handles the larger graph, while Cypher presets demonstrate multi hop expertise discovery, shortest paths, associative recall, and connected entity centrality." },
    @{ File="complete-12-falkordb-refusal.png"; Label="ALL TRACKS | FAIL CLOSED"; Title="No graph means no unsupported answer"; Text="FalkorDB is an indispensable dependency. When the database is deliberately made unavailable, Aqua displays the connection failure and produces no fabricated answer. The same loss prevents evidence retrieval, graph authorization, persistent recall, and agent handoff. Restoring the saved configuration returns the workflow to the live graph." },
    @{ File="complete-13-capabilities.png"; Label="COMPLETE AQUA CONTROL PANEL"; Title="Every capability in one desktop experience"; Text="The full panel brings together the overview, voice profiles, voice security, workflows, automated routines, app approvals, permissions and privacy, private memory, the FalkorDB graph, Company Brain, preferences and learning, reviewed extensions and skills, local file search, Secure Vault, audio devices, and display or orb settings. Loading, offline, recovery, and success states share one responsive design." },
    @{ File="complete-14-platform.png"; Label="WINDOWS DELIVERY + SETUP"; Title="Installable, local first, and reproducible"; Text="Aqua's validated target is Windows desktop. The N S I S installer bundles the desktop shell and required runtime support, while setup can add the local G G U F language model, Whisper, Kokoro, Deep Filter Net, and optional C U D A acceleration. FalkorDB can run in local Docker or at a configured secure cloud endpoint. This completes the product tour and all three hackathon tracks." }
)
$scenes = if ($Mode -eq "Submission") { $submissionScenes } elseif ($Mode -eq "Complete") { $completeScenes } else { $showcaseScenes }

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
        node scripts/capture-company-brain-demo.mjs $frames $captureMode
        if ($LASTEXITCODE -ne 0) { throw "Live browser capture failed." }
    } finally {
        Pop-Location
    }
}

$videoLines = @()
$audioLines = @()
$srtLines = @()
$assLines = @(
    "[Script Info]",
    "Title: Aqua Graph Hacks Walkthrough",
    "ScriptType: v4.00+",
    "PlayResX: 1920",
    "PlayResY: 1080",
    "ScaledBorderAndShadow: yes",
    "WrapStyle: 0",
    "",
    "[V4+ Styles]",
    "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding",
    "Style: Kicker,Segoe UI Semibold,18,&H00FFC43C,&H00FFC43C,&H00100C08,&HC0100C08,-1,0,0,0,100,100,1,0,3,1,0,7,58,58,42,1",
    "Style: Title,Segoe UI Semibold,30,&H00FFFFFF,&H00FFFFFF,&H00100C08,&HC0100C08,-1,0,0,0,100,100,0,0,3,1,0,7,58,58,76,1",
    "Style: Caption,Segoe UI,26,&H00FFFFFF,&H00FFFFFF,&H00111111,&HC8111111,0,0,0,0,100,100,0,0,3,1,0,2,76,76,34,1",
    "",
    "[Events]",
    "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text"
)
$cursor = 0.0
$index = 0

function Format-AssTime {
    param([TimeSpan]$Time)
    $centiseconds = [Math]::Floor($Time.Milliseconds / 10)
    return "{0}:{1:00}:{2:00}.{3:00}" -f [Math]::Floor($Time.TotalHours), $Time.Minutes, $Time.Seconds, $centiseconds
}

function ConvertTo-AssText {
    param([string]$Value)
    return $Value.Replace('\', '\\').Replace('{', '\{').Replace('}', '\}').Replace("`r", '').Replace("`n", '\N')
}

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

    $sceneVideo = Join-Path $workDir ("scene-{0:D2}.mp4" -f $index)
    $durationInvariant = $duration.ToString([Globalization.CultureInfo]::InvariantCulture)
    $fadeOut = [Math]::Max(0, $duration - 0.35).ToString([Globalization.CultureInfo]::InvariantCulture)
    $motionFilter = "scale=1920:1080:force_original_aspect_ratio=increase,crop=1920:1080,zoompan=z='min(zoom+0.00009,1.025)':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d=1:s=1920x1080:fps=30,fade=t=in:st=0:d=0.35,fade=t=out:st=$fadeOut`:d=0.35,format=yuv420p"
    ffmpeg -loglevel error -y -loop 1 -i $framePath -vf $motionFilter -t $durationInvariant -an -c:v libx264 -preset veryfast -crf 18 -pix_fmt yuv420p $sceneVideo
    if ($LASTEXITCODE -ne 0) { throw "Animated scene rendering failed for scene $index." }

    $videoLines += "file '$($sceneVideo.Replace("'", "''"))'"
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
    $assStart = Format-AssTime $start
    $assEnd = Format-AssTime $end
    $assLabel = ConvertTo-AssText $scene.Label
    $assTitle = ConvertTo-AssText $scene.Title
    $assCaption = ConvertTo-AssText $scene.Text
    $assLines += "Dialogue: 0,$assStart,$assEnd,Kicker,,0,0,0,,{\fad(250,250)}$assLabel"
    $assLines += "Dialogue: 0,$assStart,$assEnd,Title,,0,0,0,,{\fad(250,250)}$assTitle"
    $assLines += "Dialogue: 0,$assStart,$assEnd,Caption,,0,0,0,,{\fad(150,150)}$assCaption"
    $cursor += $duration
}

if ($daemonProcess) {
    Stop-Process -Id $daemonProcess.Id -Force -ErrorAction SilentlyContinue
    $daemonProcess = $null
}

Set-Content -LiteralPath $concatVideo -Value $videoLines -Encoding ascii
Set-Content -LiteralPath $concatAudio -Value $audioLines -Encoding ascii
Set-Content -LiteralPath $captions -Value $srtLines -Encoding utf8
Set-Content -LiteralPath $overlays -Value $assLines -Encoding utf8
$sidecarCaptions = if ($Mode -eq "Submission") {
    Join-Path $videoDir "aqua-falkordb-submission-demo.srt"
} elseif ($Mode -eq "Complete") {
    Join-Path $videoDir "aqua-complete-product-walkthrough.srt"
} else {
    Join-Path $videoDir "captions.srt"
}
Set-Content -LiteralPath $sidecarCaptions -Value $srtLines -Encoding utf8

$totalDuration = $cursor.ToString([Globalization.CultureInfo]::InvariantCulture)
$overlayPath = $overlays.Replace('\','/').Replace(':','\:')

Push-Location $repoRoot
try {
    ffmpeg -y -f concat -safe 0 -i $concatVideo -f concat -safe 0 -i $concatAudio -vf "subtitles='$overlayPath',format=yuv420p" -t $totalDuration -c:v libx264 -preset medium -crf 20 -c:a aac -ar 48000 -b:a 160k -movflags +faststart $finalVideo
    if ($LASTEXITCODE -ne 0) { throw "FFmpeg video rendering failed." }
} finally {
    Pop-Location
}

Write-Host "Created Kokoro-narrated Aqua showcase at $finalVideo ($([Math]::Round($cursor, 1)) seconds, voice $Voice)."
