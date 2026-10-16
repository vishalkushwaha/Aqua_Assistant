---
name: aqua-build
description: Build Aqua's native dependencies (llama.cpp, whisper.cpp, DeepFilterNet, Kokoro) and fetch model weights (GGUF, whisper ggml, ONNX) into data/models. Use when an engine reports "not ready"/binary missing, when adding or switching a local model, or when setting up a fresh checkout.
---

# Build engines & fetch models

All vendor code is git submodules (`.gitmodules`): llama.cpp, whisper.cpp, kokoro-tts, deepfilternet_rust, laya_rust, mcp_reference_servers. Fresh checkout: `git submodule update --init --recursive`.

## Native binaries (Windows; `.sh` twins exist for Linux/macOS)
| Engine | Script | Output the code looks for |
|---|---|---|
| llama.cpp | `scripts/build_llama_cli.ps1` | `vendor/llama.cpp/build/bin/Release/llama-cli.exe` (override `AQUA_LLAMA_CLI_BIN`) — see `resolve_cli_binary` in `core/src/modules/llm_router/llama_cli.rs` |
| whisper.cpp | `scripts/build_whisper_cli.ps1` | `vendor/whisper.cpp/build/bin/Release/whisper-cli.exe` |
| DeepFilterNet | `scripts/build_deepfilter_cli.ps1` | `vendor/deepfilternet_rust/target/release/deep-filter.exe` |
| Kokoro TTS | `scripts/setup_kokoro_cli.ps1` | venv in `vendor/kokoro-tts/.venv` (needs Python 3.11–3.12) |

Needs CMake + MSVC for llama/whisper. Builds are long — run in background.

## Model weights → `data/models/<engine>/`
| Model | Script |
|---|---|
| Qwen 0.5B chat GGUF (default `qwen-0.5b-instruct-q4_k_m.gguf`) | `scripts/fetch_llama_qwen_0_5b.ps1` |
| Qwen3-8B Q4_K_M (~5 GB, agent planning) | `scripts/fetch_llama_qwen3_8b.ps1` |
| Whisper base/small/medium en | `scripts/fetch_whisper_{base,small,medium}_en.ps1` |
| DeepFilterNet3 ONNX | `scripts/fetch_deepfilternet3_onnx.ps1` |
| Kokoro v1.0 ONNX + voices | `scripts/fetch_kokoro_v1.ps1` |
| Laya gate | downloaded by `LayaGate::bootstrap` into `data/models/laya` |

Weights (`*.gguf|*.onnx|*.bin|*.pt|*.safetensors`) are git-ignored — never `git add` them.

## Adding / switching an LLM
- Any `*.gguf` dropped in `data/models/llm/` appears in `GET /api/v1/llama/models`; select with `POST /api/v1/llama/models {"model":"<basename>"}` (persisted as setting `llm_active_gguf`).
- Respect the project's CPU-only, <200 MB active-RAM goal: before recommending a model, use the `hf-mem` skill to estimate memory and `huggingface-local-models` for quant choice (Q4_K_M default).
- Fetch scripts don't verify checksums yet; if you add one, pin the HF revision and check SHA-256.

## Verify
`GET /api/v1/status` → `llm_engine_ready`, `whisper_engine_ready`, `tts_engine_ready`, `noise_engine_ready` (see `aqua-run` for the token).
