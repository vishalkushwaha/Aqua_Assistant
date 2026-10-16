import { withAudioLock } from '../api/speech';
import { useState } from 'react';
import { Globe, Sparkles, Play, CheckCircle2, MessageSquare, AlertCircle } from 'lucide-react';
import { useCoreStatus } from '../hooks/useCoreStatus';
import { meterAudioElement, useOrbActivity } from '../activity';
import { runTask } from '../tasks';
import type { TaskContext } from '../tasks';
import { fetchTtsChunk, splitTtsText } from '../tts';
import type { SynthesizedVoiceWav } from '../tts';

export interface VoiceOption {
  id: string;
  name: string;
  language: string;
  langCode: string;
  gender: 'Female' | 'Male';
  sampleText: string;
  accent: string;
}

const TTS_PLAYBACK_TIMEOUT_MS = 120_000;

async function fetchSynthesizedVoice(text: string, voice: VoiceOption, signal: AbortSignal): Promise<SynthesizedVoiceWav> {
  return fetchTtsChunk({ text, voice: voice.id, lang: voice.langCode, signal });
}

export function TtsVoiceSelectorPanel() {
  const { status, connected } = useCoreStatus();
  const ttsReady = status?.tts_engine_ready === true;

  /** Available system voice profiles */
  const availableVoices: VoiceOption[] = [
    {
      id: 'af_sarah',
      name: 'Sarah',
      language: 'English (US)',
      langCode: 'en-us',
      gender: 'Female',
      accent: 'American',
      sampleText: 'Hello, voice synthesis is online and active.',
    },
    {
      id: 'af_heart',
      name: 'Heart',
      language: 'English (US)',
      langCode: 'en-us',
      gender: 'Female',
      accent: 'American (Warm)',
      sampleText: 'All audio and voice services are ready.',
    },
    {
      id: 'am_adam',
      name: 'Adam',
      language: 'English (US)',
      langCode: 'en-us',
      gender: 'Male',
      accent: 'American',
      sampleText: 'System diagnostics complete.',
    },
    {
      id: 'ff_siwis',
      name: 'Siwis',
      language: 'French (Français)',
      langCode: 'fr-fr',
      gender: 'Female',
      accent: 'French',
      sampleText: 'Bonjour, le système Aqua est prêt.',
    },
    {
      id: 'jf_alpha',
      name: 'Alpha',
      language: 'Japanese (日本語)',
      langCode: 'ja',
      gender: 'Female',
      accent: 'Japanese',
      sampleText: 'Aquaシステムは正常です。',
    },
  ];

  const [selectedVoiceId, setSelectedVoiceId] = useState<string>('af_sarah');
  const [synthError, setSynthError] = useState<string | null>(null);
  const [activeLanguageFilter, setActiveLanguageFilter] = useState<string>('All');
  const [isPlayingSample, setIsPlayingSample] = useState<string | null>(null);

  // Test Speech Input State
  const [customText, setCustomText] = useState<string>("Hello, how can I assist you today?");
  const [isSynthesizing, setIsSynthesizing] = useState<boolean>(false);
  const [synthesisProgress, setSynthesisProgress] = useState<{ current: number; total: number } | null>(null);
  useOrbActivity('voices', isSynthesizing ? 'thinking' : 'idle', 0, 'Voices'); // the voice orb shows it is working

  const languages = ['All', 'English (US)', 'French (Français)', 'Japanese (日本語)'];

  const filteredVoices = availableVoices.filter(voice => 
    activeLanguageFilter === 'All' || voice.language === activeLanguageFilter
  );

  const selectedVoice = availableVoices.find(v => v.id === selectedVoiceId) || availableVoices[0];

  const playWavBase64 = (b64: string, signal?: AbortSignal): Promise<void> => {
    const bin = atob(b64);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    const blob = new Blob([bytes], { type: 'audio/wav' });
    const url = URL.createObjectURL(blob);
    const audio = new Audio(url);
    meterAudioElement(audio, 'voices-speech', 'Voices'); // the orb follows the voice as it plays
    return new Promise((resolve, reject) => {
      let settled = false;
      const finish = (error?: Error) => {
        if (settled) return;
        settled = true;
        window.clearTimeout(timeout);
        signal?.removeEventListener('abort', stop);
        audio.onended = null;
        audio.onerror = null;
        URL.revokeObjectURL(url);
        if (error) reject(error);
        else resolve();
      };
      const stop = () => {
        audio.pause();
        finish();
      };
      const timeout = window.setTimeout(() => {
        audio.pause();
        finish(new Error('Audio playback timed out. Please try again.'));
      }, TTS_PLAYBACK_TIMEOUT_MS);

      // Stop pauses the voice and lets the task end.
      signal?.addEventListener('abort', stop, { once: true });
      audio.onended = () => finish();
      audio.onerror = () => finish(new Error('Audio playback failed'));
      if (signal?.aborted) {
        stop();
        return;
      }
      void audio.play().catch((error: unknown) => {
        finish(error instanceof Error ? error : new Error('Audio playback failed'));
      });
    });
  };

  // Making the speech and playing it is one task: it waits its turn behind any other task and ends when the voice has ended.
  const handleSynthesizeCustomText = async (textToSpeak: string, voiceOverride?: VoiceOption) => {
    setIsSynthesizing(true); // from the moment it is given, including while it waits
    try {
      const outcome = await runTask('Voices', (ctx) => synthesizeNow(textToSpeak, ctx, voiceOverride));
      if (outcome.error) {
        setSynthError(outcome.error instanceof Error ? outcome.error.message : 'Voice synthesis service failed');
      }
    } finally {
      setIsSynthesizing(false);
      setIsPlayingSample(null);
      setSynthesisProgress(null);
    }
  };

  const synthesizeNow = async (textToSpeak: string, ctx: TaskContext, voiceOverride?: VoiceOption) => {
    const targetVoice = voiceOverride || selectedVoice;
    const startTime = performance.now();
    setIsSynthesizing(true);
    setIsPlayingSample(targetVoice.id);
    setSynthError(null);

    const requests = new AbortController();
    const stopRequests = () => requests.abort(ctx.signal.reason);
    ctx.signal.addEventListener('abort', stopRequests, { once: true });
    let pending: Promise<SynthesizedVoiceWav> | null = null;

    if (connected) {
      try {
        const pieces = splitTtsText(textToSpeak);
        if (pieces.length === 0) return;
        let latencyMs = 0;
        let wavBytes = 0;
        let sampleRate = 24000;
        let engineReady = false;
        ctx.setPhase('speaking');
        const queueChunk = (piece: string) => {
          const job = fetchSynthesizedVoice(piece, targetVoice, requests.signal);
          // Prefetch can fail while the current audio is still playing. Mark it
          // handled immediately; awaiting `job` below still observes the error.
          void job.catch(() => undefined);
          return job;
        };
        pending = queueChunk(pieces[0]);
        for (let i = 0; i < pieces.length; i++) {
          if (ctx.signal.aborted) break;
          setSynthesisProgress({ current: i + 1, total: pieces.length });
          const data = await pending;
          pending = null;
          pending = i + 1 < pieces.length ? queueChunk(pieces[i + 1]) : null;
          if (!data) break;
          if (!latencyMs) latencyMs = data.latency_ms || Math.round(performance.now() - startTime);
          if (data.wav_base64 && data.engine_ready) {
            engineReady = true;
            sampleRate = data.sample_rate || sampleRate;
            wavBytes += Math.floor((data.wav_base64.length * 3) / 4);
            await withAudioLock(() => playWavBase64(data.wav_base64, ctx.signal));
          }
        }
        if (engineReady) {
          return;
        }
      } catch (e) {
        if (ctx.signal.aborted) {
          return;
        }
        setSynthError(e instanceof Error ? e.message : 'Voice synthesis service failed');
      } finally {
        // A prefetched next chunk must never outlive the task if playback or
        // decoding fails. Leaving it running would hold the core's speech lock.
        requests.abort();
        await pending?.catch(() => undefined);
        ctx.signal.removeEventListener('abort', stopRequests);
      }
    } else {
      requests.abort();
      ctx.signal.removeEventListener('abort', stopRequests);
    }

    // Voice did not speak (the core is offline, voice engine is not ready, or it failed).
    setSynthError((prev) => prev ?? (connected ? 'Voice service is initializing. Please wait a moment.' : 'Assistant is currently offline.'));
  };

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: '24px', maxWidth: '1200px', margin: '0 auto' }}>
      {/* Interactive Custom Text-to-Voice Playground Box */}
      <div className="glass-panel" style={{ padding: '28px', position: 'relative', overflow: 'hidden' }}>
        <div style={{ position: 'absolute', top: 0, left: 0, right: 0, height: '4px', background: 'linear-gradient(90deg, var(--accent-cyan), var(--accent-purple))' }} />

        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: '16px', marginBottom: '24px' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '16px' }}>
            <div style={{ width: '48px', height: '48px', borderRadius: '12px', background: 'rgba(192, 132, 252, 0.1)', display: 'flex', alignItems: 'center', justifyContent: 'center', border: '1px solid rgba(192, 132, 252, 0.2)' }}>
              <MessageSquare color="var(--accent-purple)" size={24} />
            </div>
            <div>
              <h3 style={{ fontSize: '24px', fontWeight: '700', letterSpacing: '-0.02em', color: 'var(--text-primary)', margin: 0 }}>
                Voice Profiles & Speech Preview
              </h3>
              <p style={{ fontSize: '14px', color: 'var(--text-secondary)', margin: '4px 0 0 0' }}>
                Preview voice pronunciation and select the default voice style for your assistant.
              </p>
            </div>
          </div>

          <div style={{ display: 'flex', gap: '12px', flexWrap: 'wrap' }}>
            <div style={{ padding: '8px 16px', borderRadius: '20px', background: 'rgba(0, 240, 255, 0.1)', border: '1px solid rgba(0, 240, 255, 0.3)', display: 'flex', alignItems: 'center', gap: '8px', boxShadow: '0 0 15px rgba(0, 240, 255, 0.1)' }}>
              <Globe size={16} color="var(--accent-cyan)" />
              <span style={{ fontSize: '13px', fontWeight: '700', color: 'var(--accent-cyan)' }}>
                {selectedVoice.name} · {selectedVoice.langCode}
              </span>
            </div>
            <div style={{ padding: '8px 16px', borderRadius: '20px', background: ttsReady ? 'rgba(0, 255, 136, 0.1)' : 'rgba(245, 158, 11, 0.1)', border: `1px solid ${ttsReady ? 'rgba(0, 255, 136, 0.3)' : 'rgba(245, 158, 11, 0.3)'}`, display: 'flex', alignItems: 'center', gap: '8px', boxShadow: `0 0 15px ${ttsReady ? 'rgba(0, 255, 136, 0.1)' : 'rgba(245, 158, 11, 0.1)'}` }}>
              <div className={ttsReady ? 'pulse-dot' : ''} style={{ width: '8px', height: '8px', borderRadius: '50%', background: ttsReady ? 'var(--accent-emerald)' : 'var(--accent-amber)' }}></div>
              <span style={{ fontSize: '13px', fontWeight: '700', color: ttsReady ? 'var(--accent-emerald)' : 'var(--accent-amber)' }}>
                {ttsReady ? 'Voice Active' : 'Initializing'}
              </span>
            </div>
          </div>
        </div>

        {synthError && (
          <div style={{ marginBottom: '20px', padding: '16px', borderRadius: '12px', background: 'rgba(255, 0, 85, 0.05)', border: '1px solid rgba(255, 0, 85, 0.3)', color: 'var(--accent-rose)', fontSize: '14px', display: 'flex', alignItems: 'center', gap: '10px' }}>
            <AlertCircle size={20} />
            <span><strong>Notice:</strong> {synthError}</span>
          </div>
        )}

        <div style={{ display: 'flex', flexDirection: 'column', gap: '16px' }}>
          <textarea 
            rows={3}
            value={customText}
            onChange={(e) => setCustomText(e.target.value)}
            placeholder="Type any phrase here to preview this voice..."
            style={{
              width: '100%',
              padding: '16px',
              borderRadius: '12px',
              background: 'rgba(0,0,0,0.4)',
              border: '1px solid rgba(192, 132, 252, 0.3)',
              color: 'var(--text-primary)',
              fontSize: '15px',
              lineHeight: '1.6',
              resize: 'vertical',
              outline: 'none',
              boxShadow: 'inset 0 2px 10px rgba(0,0,0,0.2), 0 0 10px rgba(192, 132, 252, 0.05)',
              transition: 'all 0.3s'
            }}
          />

          <div style={{ display: 'flex', justifyContent: 'flex-end', alignItems: 'center' }}>
            <button
              onClick={() => void handleSynthesizeCustomText(customText)}
              disabled={isSynthesizing || !customText.trim()}
              className="btn-primary"
              style={{
                padding: '12px 24px',
                background: isSynthesizing ? 'var(--bg-tertiary)' : 'linear-gradient(135deg, var(--accent-cyan), var(--accent-purple))',
                color: isSynthesizing ? 'var(--text-muted)' : '#fff',
                opacity: isSynthesizing || !customText.trim() ? 0.6 : 1,
                cursor: isSynthesizing || !customText.trim() ? 'not-allowed' : 'pointer',
                display: 'flex',
                alignItems: 'center',
                gap: '10px',
                fontSize: '14px',
                fontWeight: '600',
                borderRadius: '10px',
                border: isSynthesizing ? '1px solid var(--glass-border)' : 'none',
                boxShadow: isSynthesizing ? 'none' : '0 4px 20px rgba(192, 132, 252, 0.3)'
              }}>
              <Play size={16} className={isSynthesizing ? 'spin' : ''} /> 
              {isSynthesizing
                ? synthesisProgress && synthesisProgress.total > 1
                  ? `Processing ${synthesisProgress.current}/${synthesisProgress.total}...`
                  : "Synthesizing..."
                : "Preview Custom Phrase"}
            </button>
          </div>
        </div>
      </div>

      {/* Voice Selection Panel */}
      <div className="glass-panel" style={{ padding: '28px', borderRadius: '12px', display: 'flex', flexDirection: 'column', gap: '24px' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: '12px' }}>
          <Sparkles size={24} color="var(--accent-cyan)" />
          <div>
            <h3 style={{ fontSize: '20px', fontWeight: '700', color: 'var(--text-primary)', margin: 0 }}>Available Voice Profiles</h3>
            <p style={{ fontSize: '14px', color: 'var(--text-secondary)', margin: '4px 0 0 0' }}>Select any voice profile to set as active default for responses or preview default sample speech.</p>
          </div>
        </div>

        {/* Language Filter Pills */}
        <div style={{ display: 'flex', gap: '12px', overflowX: 'auto', paddingBottom: '8px' }}>
          {languages.map(lang => (
            <button
              key={lang}
              onClick={() => setActiveLanguageFilter(lang)}
              style={{
                padding: '8px 20px',
                borderRadius: '30px',
                border: activeLanguageFilter === lang ? 'none' : '1px solid var(--glass-border)',
                background: activeLanguageFilter === lang ? 'var(--accent-cyan)' : 'var(--bg-tertiary)',
                color: activeLanguageFilter === lang ? '#000' : 'var(--text-secondary)',
                fontWeight: '700',
                cursor: 'pointer',
                fontSize: '13px',
                whiteSpace: 'nowrap',
                transition: 'all 0.2s',
                boxShadow: activeLanguageFilter === lang ? '0 4px 15px rgba(0, 240, 255, 0.3)' : 'none'
              }}>
              {lang}
            </button>
          ))}
        </div>

        {/* Voice Selection Grid */}
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(340px, 1fr))', gap: '20px' }}>
          {filteredVoices.map(voice => {
            const isSelected = voice.id === selectedVoiceId;
            const isPlaying = isPlayingSample === voice.id;

            return (
              <div
                key={voice.id}
                onClick={() => setSelectedVoiceId(voice.id)}
                style={{
                  padding: '20px',
                  borderRadius: '12px',
                  background: isSelected ? 'linear-gradient(145deg, rgba(0, 240, 255, 0.05), transparent)' : 'var(--bg-tertiary)',
                  border: isSelected ? '1px solid var(--accent-cyan)' : '1px solid var(--glass-border)',
                  cursor: 'pointer',
                  display: 'flex',
                  flexDirection: 'column',
                  gap: '16px',
                  transition: 'all 0.2s',
                  boxShadow: isSelected ? '0 8px 30px rgba(0, 240, 255, 0.05)' : 'none',
                  position: 'relative',
                  overflow: 'hidden'
                }}>
                {isSelected && <div style={{ position: 'absolute', top: 0, left: 0, bottom: 0, width: '4px', background: 'var(--accent-cyan)', boxShadow: '0 0 10px var(--accent-cyan)' }} />}
                
                <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                  <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
                    <div style={{ width: '32px', height: '32px', borderRadius: '50%', background: isSelected ? 'rgba(0, 240, 255, 0.1)' : 'rgba(255, 255, 255, 0.05)', display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
                      <Globe size={16} color={isSelected ? 'var(--accent-cyan)' : 'var(--text-secondary)'} />
                    </div>
                    <span style={{ fontWeight: '700', fontSize: '16px', color: isSelected ? 'var(--accent-cyan)' : 'var(--text-primary)' }}>
                      {voice.name}
                    </span>
                  </div>
                  {isSelected && <CheckCircle2 size={22} color="var(--accent-cyan)" />}
                </div>

                <div style={{ display: 'flex', gap: '8px', fontSize: '12px', flexWrap: 'wrap' }}>
                  <span style={{ padding: '4px 10px', borderRadius: '6px', background: 'rgba(255,255,255,0.05)', color: 'var(--text-secondary)', fontWeight: '600' }}>
                    {voice.language}
                  </span>
                  <span style={{ padding: '4px 10px', borderRadius: '6px', background: 'rgba(255,255,255,0.05)', color: 'var(--text-secondary)', fontWeight: '600' }}>
                    {voice.gender}
                  </span>
                  <span style={{ padding: '4px 10px', borderRadius: '6px', background: 'rgba(255,255,255,0.05)', color: 'var(--text-secondary)', fontWeight: '600' }}>
                    {voice.accent}
                  </span>
                </div>

                <div style={{ fontSize: '14px', color: 'var(--text-muted)', fontStyle: 'italic', background: 'rgba(0,0,0,0.3)', padding: '12px', borderRadius: '8px', borderLeft: isSelected ? '2px solid rgba(0, 240, 255, 0.3)' : '2px solid transparent' }}>
                  "{voice.sampleText}"
                </div>

                <button
                  onClick={(e) => {
                    e.stopPropagation();
                    setSelectedVoiceId(voice.id);
                    void handleSynthesizeCustomText(voice.sampleText, voice);
                  }}
                  disabled={isPlaying}
                  style={{
                    padding: '8px 16px',
                    borderRadius: '8px',
                    background: isPlaying ? 'var(--accent-purple)' : 'rgba(255,255,255,0.05)',
                    color: isPlaying ? '#fff' : 'var(--text-primary)',
                    border: '1px solid var(--glass-border)',
                    fontSize: '13px',
                    fontWeight: '700',
                    cursor: 'pointer',
                    display: 'flex',
                    alignItems: 'center',
                    gap: '8px',
                    marginTop: '4px',
                    alignSelf: 'flex-start',
                    transition: 'all 0.2s'
                  }}>
                  <Play size={14} className={isPlaying ? 'pulse-dot' : ''} /> {isPlaying ? "Synthesizing Stream..." : "Preview Sample Audio"}
                </button>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}

