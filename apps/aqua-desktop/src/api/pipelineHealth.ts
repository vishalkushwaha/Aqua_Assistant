export interface StageStatus {
  id: string;
  label: string;
  model_present: boolean;
  engine_ready: boolean;
}

export interface PipelineStageHealth {
  stages: StageStatus[];
  pipeline_ready: boolean;
  text_pipeline_ready: boolean;
}

export interface PipelineTextResult {
  input_text: string;
  assistant_text: string;
  tts_sample_count: number;
  tts_wav_base64: string;
  tts_sample_rate: number;
  llm_engine_ready: boolean;
  tts_engine_ready: boolean;
  latency_ms: number;
}

export async function fetchPipelineHealth(): Promise<PipelineStageHealth | null> {
  try {
    const res = await fetch('/api/v1/pipeline/health', { headers: { Accept: 'application/json' } });
    if (!res.ok) return null;
    return (await res.json()) as PipelineStageHealth;
  } catch {
    return null;
  }
}

export async function runTextPipeline(
  text: string,
  voice = 'af_sarah',
  lang = 'en-us',
): Promise<PipelineTextResult | null> {
  try {
    const res = await fetch('/api/v1/pipeline/text', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
      body: JSON.stringify({ text, voice, lang }),
    });
    if (!res.ok) return null;
    const raw = (await res.json()) as PipelineTextResult & { latency_ms: number };
    return raw;
  } catch {
    return null;
  }
}
