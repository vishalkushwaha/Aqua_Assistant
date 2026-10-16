# A small static embedding model for ranking facts (card R3-06): minishlab/potion-base-8M, 256 numbers per word piece, about 30 MB, MIT licence.
# It is used only when AQUA_FACT_RANK=on (off by default); without it Aqua ranks facts as before.
# Source: Hugging Face, the publisher's own repository, pinned to one revision so the files cannot change under the hashes below.
$ErrorActionPreference = "Stop"
# An installed copy keeps its data in %LOCALAPPDATA%\Aqua: run with AQUA_WORKSPACE_ROOT set to that folder.
$Root = if ($env:AQUA_WORKSPACE_ROOT) { $env:AQUA_WORKSPACE_ROOT } else { Split-Path -Parent $PSScriptRoot }
. (Join-Path $PSScriptRoot "_verified_download.ps1")
$OutDir = Join-Path $Root "data/models/embedding"
$Revision = "bf8b056651a2c21b8d2565580b8569da283cab23"
$Base = "https://huggingface.co/minishlab/potion-base-8M/resolve/$Revision"
# SHA-256 of the copy this install was tested with. The one of model.safetensors is the one Hugging Face lists for the file; the other two were computed on 2026-10-08.
Get-VerifiedFile -Url "$Base/model.safetensors" -OutFile (Join-Path $OutDir "model.safetensors") -Sha256 "f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2"
Get-VerifiedFile -Url "$Base/tokenizer.json" -OutFile (Join-Path $OutDir "tokenizer.json") -Sha256 "e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50"
Get-VerifiedFile -Url "$Base/config.json" -OutFile (Join-Path $OutDir "config.json") -Sha256 "2a6ac0e9aaa356a68a5688070db78fc3a464fefe85d2f06a1905ce3718687553"
Write-Host "Done. Set AQUA_FACT_RANK=on and restart Aqua to use it (see documentation/CONFIGURATION.md)."
