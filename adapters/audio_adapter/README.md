# Audio & Voice Interaction Adapter Subsystem (`adapters/audio_adapter/`)

This crate (`aqua-audio-adapter`) implements the voice stack: noise suppression, STT, TTS, voice biometrics and the speech gate. `mic_capture.rs`, `voice_standby.rs` and `silero_vad.rs` (an energy detector named Silero) are **written, not wired**: only tests reference them (register `voice.neural_vad`, simulated). The microphone is opened by the app's page, not by this crate.

## Rust source layout (`src/`)

| Module file | Responsibility |
| :--- | :--- |
| `noise_cancellation.rs` | DeepFilterNet3 frame processing |
| `stt_whisper.rs` | Whisper STT (`data/models/whisper/`) |
| `tts_kokoro.rs` | Kokoro TTS (`data/models/kokoro/`) |
| `biometrics_ecapa.rs`, `speaker_features.rs` | Master voice verification (a WeSpeaker model; the features it reads) |
| `speech_gate.rs` | A cheap check that a person is talking, before any speech model runs |
| `deepfilter_cli.rs`, `whisper_cli.rs`, `kokoro_cli.rs`, `process.rs`, `engines.rs` | The helper programs the engines run, where they live, and how a turn is kept from waiting on one forever |
| `resample.rs`, `cpu.rs` | Audio to 16 kHz mono; the real core count |
| `mic_capture.rs` | `cpal` microphone capture (not wired) |
| `voice_standby.rs`, `silero_vad.rs` | Standby pipeline and an energy detector (not wired) |

## Design docs (target subfolders)

Detailed specs reference logical folders that may be split out later:

- [`voice_standby/README.md`](voice_standby/README.md) — wake word, VAD, AEC, hot-plug
- [`speaker_verification/README.md`](speaker_verification/README.md) — admin enrollment & adaptive drift

## Tests

```bash
cargo test -p aqua-audio-adapter
```

See [`documentation/TESTING.md`](../../documentation/TESTING.md) (some tests need the models and are ignored).

## Planned logical components (architecture)

- `wake_word/` — ONNX **"Hey Aqua"** / **"Aqua"** (<1% idle CPU)
- `vad/` — Silero VAD
- `stt/` — whisper.cpp INT8 path (vendor: `vendor/whisper.cpp`)
- `tts/` — Piper / Kokoro ONNX
