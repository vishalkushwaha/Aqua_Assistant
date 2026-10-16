# Quick checks for Project Aqua voice stack (core must be running on :8767)
$ErrorActionPreference = "Stop"
$Base = "http://127.0.0.1:8767"
$TokenFile = Join-Path (Split-Path -Parent $PSScriptRoot) "data\.aqua_api_token"
$ApiToken = if ($env:AQUA_API_TOKEN) { $env:AQUA_API_TOKEN } elseif (Test-Path $TokenFile) { (Get-Content $TokenFile -Raw).Trim() } else { throw "No API token: start aqua-core once to create $TokenFile" }
$AuthHeaders = @{ Authorization = "Bearer $ApiToken" }

function Test-JsonGet($path) {
    Write-Host "`n== GET $path =="
    Invoke-RestMethod -Uri "$Base$path" -Method Get -Headers $AuthHeaders | ConvertTo-Json -Depth 6
}

function Test-JsonPost($path, $body) {
    Write-Host "`n== POST $path =="
    Invoke-RestMethod -Uri "$Base$path" -Method Post -Headers $AuthHeaders -ContentType "application/json" -Body ($body | ConvertTo-Json) | ConvertTo-Json -Depth 4
}

Write-Host "Aqua local stack verification (start: cargo run -p aqua-core)"
Test-JsonGet "/api/v1/status"
Test-JsonGet "/api/v1/pipeline/health"
Test-JsonPost "/api/v1/pipeline/text" @{ text = "Say hello in one sentence."; voice = "af_sarah"; lang = "en-us" }

$jfk = Join-Path (Split-Path -Parent $PSScriptRoot) "vendor/whisper.cpp/samples/jfk.wav"
if (Test-Path $jfk) {
    $bytes = [IO.File]::ReadAllBytes($jfk)
    $b64 = [Convert]::ToBase64String($bytes)
    Test-JsonPost "/api/v1/pipeline/wav" @{ wav_base64 = $b64; voice = "af_sarah"; lang = "en-us" }
} else {
    Write-Host "`n(skip POST /api/v1/pipeline/wav - jfk.wav not found at $jfk)"
}

Write-Host "`nDone. For full engines, run fetch/build scripts in scripts/ and per-model READMEs under data/models/."
