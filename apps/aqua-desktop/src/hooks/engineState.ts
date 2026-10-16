import type { CoreStatusResponse } from '../api/coreStatus';

export type EngineState = 'ok' | 'attention' | 'problem' | 'loading';

export type EngineId = 'deepfilternet' | 'whisper' | 'llama' | 'kokoro';

/**
 * Whether a voice engine is ready: ready to run, installed but not loaded, or missing.
 * Read from /api/v1/status, which the page already polls: it reports the engine and its model file together.
 */
export function engineState(status: CoreStatusResponse | null, id: string): EngineState {
  if (!status) return 'loading'; // not known yet, or the core could not be asked: say so rather than guess
  const m = status.models;
  const [ready, present] =
    id === 'deepfilternet' ? [status.noise_engine_ready, m.deepfilternet_onnx]
    : id === 'whisper' ? [status.whisper_engine_ready, m.whisper_ggml]
    : id === 'llama' ? [status.llm_engine_ready, m.llm_gguf]
    : id === 'kokoro' ? [status.tts_engine_ready, m.kokoro_onnx]
    : [undefined, false];
  if (ready) return 'ok';
  return present ? 'attention' : 'problem';
}
