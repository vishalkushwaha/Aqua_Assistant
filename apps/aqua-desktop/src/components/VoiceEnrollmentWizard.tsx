import { useState, useEffect, useRef } from 'react';
import { Mic, CheckCircle2, ShieldCheck, RefreshCw, Volume2, RotateCcw, MessageSquare, UserCheck, UserX, Square } from 'lucide-react';
import { mayAdvanceByItself, saidWakePhrase } from '../api/enrollmentMatch';
import { requireJson } from '../api/jsonResponse';

interface AudioMetrics {
  rmsDb: number;
  peakDb: number;
  durationSec: number;
  clippingCount: number;
  qualityLabel: string;
}

interface RecordedStep {
  base64: string;
  blobUrl: string;
  metrics: AudioMetrics;
  sttResult?: string;
}

export function VoiceEnrollmentWizard() {
  // Settings State
  const [wakeupWord, setWakeupWord] = useState<string>("Hey Aqua");
  const [customReply, setCustomReply] = useState<string>("Yes Master, I am online and listening.");
  const [voiceSecurityEnabled, setVoiceSecurityEnabled] = useState<boolean>(true);
  const [isSavingSettings, setIsSavingSettings] = useState<boolean>(false);
  const [settingsSavedMessage, setSettingsSavedMessage] = useState<string | null>(null);

  // Calibration State
  const [isEnrolling, setIsEnrolling] = useState<boolean>(false);
  const [currentStep, setCurrentStep] = useState<number>(0);
  const [recordedSamples, setRecordedSamples] = useState<{ [key: number]: RecordedStep }>({});
  const [isRecordingStep, setIsRecordingStep] = useState<boolean>(false);
  const [isProcessingStt, setIsProcessingStt] = useState<boolean>(false);
  const [enrolledFingerprint, setEnrolledFingerprint] = useState<string | null>(null);

  // Audio Playback State
  const [activePlaybackStep, setActivePlaybackStep] = useState<number | null>(null);
  const audioRef = useRef<HTMLAudioElement | null>(null);

  // MediaRecorder refs for enrollment
  const mediaRecorderRef = useRef<MediaRecorder | null>(null);
  const audioChunksRef = useRef<Blob[]>([]);

  // Verification Testing State
  const [isRecordingTest, setIsRecordingTest] = useState<boolean>(false);
  const [isVerifying, setIsVerifying] = useState<boolean>(false);
  const [verificationResult, setVerificationResult] = useState<{
    authorized: boolean;
    similarity: number;
    speakerLabel: string;
    actionTaken: string;
    sttResult?: string;
  } | null>(null);
  const testRecorderRef = useRef<MediaRecorder | null>(null);
  const testAudioChunksRef = useRef<Blob[]>([]);

  const calibrationPhrases = [
    `"${wakeupWord}", initiate master admin voice calibration routine.`,
    "The quick brown fox jumps over the lazy sleeping hound.",
    `"${wakeupWord}", system verification complete, grant master access.`
  ];

  // Load saved settings from Rust backend on mount
  useEffect(() => {
    fetch('/api/v1/voice/settings')
      .then(res => requireJson<Record<string, any>>(res, 'Voice settings are unavailable.'))
      .then(data => {
        if (data.wakeup_word) setWakeupWord(data.wakeup_word);
        if (data.custom_reply) setCustomReply(data.custom_reply);
        if (data.voice_security_enabled !== undefined) setVoiceSecurityEnabled(data.voice_security_enabled);
        if (data.biometric_enrolled && data.enrolled_fingerprint_id) {
          setEnrolledFingerprint(data.enrolled_fingerprint_id);
        }
      })
      .catch(() => {
        // Fallback default
      });
  }, []);

  const handleSaveSettings = async () => {
    setIsSavingSettings(true);
    setSettingsSavedMessage(null);
    try {
      const res = await fetch('/api/v1/voice/settings', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ 
          wakeup_word: wakeupWord, 
          custom_reply: customReply,
          voice_security_enabled: voiceSecurityEnabled
        })
      });
      if (res.ok) {
        setSettingsSavedMessage("Wakeup word & response settings saved to backend database.");
        setTimeout(() => setSettingsSavedMessage(null), 3500);
      }
    } catch {
      // Ignore
    } finally {
      setIsSavingSettings(false);
    }
  };

  /** Process audio blob into 48kHz WAV Base64 string and calculate real audio metrics */
  const processRecordedAudioBlob = async (blob: Blob): Promise<{ base64: string; blobUrl: string; metrics: AudioMetrics }> => {
    const arrayBuffer = await blob.arrayBuffer();
    const AudioCtx = window.AudioContext || (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
    const audioCtx = new AudioCtx();
    const decodedBuffer = await audioCtx.decodeAudioData(arrayBuffer);

    // Resample to 48kHz Mono PCM
    const targetRate = 48000;
    const offlineCtx = new OfflineAudioContext(1, Math.ceil(decodedBuffer.duration * targetRate), targetRate);
    const src = offlineCtx.createBufferSource();
    src.buffer = decodedBuffer;
    src.connect(offlineCtx.destination);
    src.start(0);
    const resampled = await offlineCtx.startRendering();

    const pcm = resampled.getChannelData(0);
    const durationSec = parseFloat(decodedBuffer.duration.toFixed(2));

    // Calculate RMS, Peak, and Clipping
    let sumSq = 0;
    let peak = 0;
    let clippingCount = 0;

    for (let i = 0; i < pcm.length; i++) {
      const s = pcm[i];
      const abs = Math.abs(s);
      sumSq += s * s;
      if (abs > peak) peak = abs;
      if (abs >= 0.99) clippingCount++;
    }

    const rms = Math.sqrt(sumSq / pcm.length);
    const rmsDb = Math.round(20 * Math.log10(Math.max(rms, 1e-5)));
    const peakDb = Math.round(20 * Math.log10(Math.max(peak, 1e-5)));

    let qualityLabel = "Optimal Speech Clarity";
    if (rms < 0.005) qualityLabel = "Faint Audio / Too Quiet";
    else if (clippingCount > 50) qualityLabel = "High Distortion / Clipping Detected";
    else if (durationSec < 1.0) qualityLabel = "Audio Sample Too Short";

    // Build 16-bit PCM WAV File Header
    const dataSize = pcm.length * 2;
    const bufferSize = 44 + dataSize;
    const wavBuffer = new ArrayBuffer(bufferSize);
    const view = new DataView(wavBuffer);

    const writeStr = (off: number, str: string) => {
      for (let i = 0; i < str.length; i++) view.setUint8(off + i, str.charCodeAt(i));
    };

    writeStr(0, 'RIFF');
    view.setUint32(4, 36 + dataSize, true);
    writeStr(8, 'WAVE');
    writeStr(12, 'fmt ');
    view.setUint32(16, 16, true);
    view.setUint16(20, 1, true);
    view.setUint16(22, 1, true);
    view.setUint32(24, targetRate, true);
    view.setUint32(28, targetRate * 2, true);
    view.setUint16(32, 2, true);
    view.setUint16(34, 16, true);
    writeStr(36, 'data');
    view.setUint32(40, dataSize, true);

    let offset = 44;
    for (let i = 0; i < pcm.length; i++) {
      const s = Math.max(-1, Math.min(1, pcm[i]));
      view.setInt16(offset, s < 0 ? s * 0x8000 : s * 0x7fff, true);
      offset += 2;
    }

    const bytes = new Uint8Array(wavBuffer);
    let binary = '';
    for (let i = 0; i < bytes.byteLength; i++) binary += String.fromCharCode(bytes[i]);
    const base64 = btoa(binary);

    const wavBlob = new Blob([bytes.buffer], { type: 'audio/wav' });
    const blobUrl = URL.createObjectURL(wavBlob);

    return {
      base64,
      blobUrl,
      metrics: { rmsDb, peakDb, durationSec, clippingCount, qualityLabel }
    };
  };

  const handleStartEnrollment = () => {
    setIsEnrolling(true);
    setCurrentStep(1);
    setRecordedSamples({});
  };

  const startStepRecording = async () => {
    setIsRecordingStep(true);
    audioChunksRef.current = [];
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ 
        audio: { channelCount: 1, sampleRate: 48000, echoCancellation: false, noiseSuppression: false } 
      });
      const recorder = new MediaRecorder(stream);
      mediaRecorderRef.current = recorder;

      recorder.ondataavailable = (e) => {
        if (e.data.size > 0) audioChunksRef.current.push(e.data);
      };

      recorder.onstop = async () => {
        setIsRecordingStep(false);
        setIsProcessingStt(true);
        const audioBlob = new Blob(audioChunksRef.current, { type: recorder.mimeType || 'audio/webm' });
        const stepData = await processRecordedAudioBlob(audioBlob);
        
        let sttResult = "[No STT result]";
        try {
          const res = await fetch('/api/v1/whisper/transcribe', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ wav_base64: stepData.base64 })
          });
          const data = await requireJson<Record<string, any>>(res, 'Speech transcription is unavailable.');
          sttResult = !res.ok ? `[STT Error: ${data?.error ?? res.status}]` : data.transcription?.trim() || "[No speech detected]";
        } catch (err: any) {
          sttResult = `[STT Error: ${err.message}]`;
        }
        
        setRecordedSamples(prev => ({ ...prev, [currentStep]: { ...stepData, sttResult } }));
        setIsProcessingStt(false);
        stream.getTracks().forEach(t => t.stop());

        // Verification & auto-advance. Only on words that were really heard (an error that mentions Aqua is not the wake phrase, and an empty
        // phrase matches nothing), and never on the last step: saving replaces the enrolled voiceprint, which is the person's own click.
        if (mayAdvanceByItself(currentStep, 3, sttResult, wakeupWord)) {
          setTimeout(() => {
            const nextBtn = document.getElementById('btn-next-step');
            if (nextBtn && !nextBtn.hasAttribute('disabled')) {
              nextBtn.click();
            }
          }, 2500);
        }
      };

      recorder.start(100);
    } catch {
      setIsRecordingStep(false);
    }
  };

  const stopStepRecording = () => {
    if (mediaRecorderRef.current && isRecordingStep) {
      mediaRecorderRef.current.stop();
    }
  };

  const handleReRecordStep = () => {
    setRecordedSamples(prev => {
      const updated = { ...prev };
      delete updated[currentStep];
      return updated;
    });
  };

  const handlePlaySample = (step: number) => {
    const sample = recordedSamples[step];
    if (!sample) return;
    if (audioRef.current) {
      audioRef.current.pause();
    }
    const audio = new Audio(sample.blobUrl);
    audioRef.current = audio;
    setActivePlaybackStep(step);
    audio.play();
    audio.onended = () => setActivePlaybackStep(null);
  };

  const handleNextStep = async () => {
    if (currentStep < 3) {
      setCurrentStep(prev => prev + 1);
    } else {
      // Complete enrollment with backend API
      const wavBase64List = [
        recordedSamples[1]?.base64,
        recordedSamples[2]?.base64,
        recordedSamples[3]?.base64
      ].filter(Boolean) as string[];

      try {
        const res = await fetch('/api/v1/voice/enroll', {
          method: 'POST',
          // The dev proxy (vite.config.ts) attaches the API token.
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            wav_samples_base64: wavBase64List,
            wakeup_word: wakeupWord,
            custom_reply: customReply
          })
        });
        const data = await requireJson<Record<string, any>>(res, 'Voice enrollment could not be completed.');
        if (data.success && data.enrolled_fingerprint_id) {
          setEnrolledFingerprint(data.enrolled_fingerprint_id);
        } else {
          console.error('Voice enrollment failed:', data.message);
        }
      } catch (e) {
        // Never show "enrolled" when the request failed.
        console.error('Voice enrollment request failed:', e);
      } finally {
        setIsEnrolling(false);
        setCurrentStep(0);
      }
    }
  };

  /** Record live mic audio and verify speaker against backend master fingerprint */
  const startLiveSpeakerVerificationTest = async () => {
    setIsRecordingTest(true);
    setVerificationResult(null);
    testAudioChunksRef.current = [];

    try {
      const stream = await navigator.mediaDevices.getUserMedia({
        audio: { channelCount: 1, sampleRate: 48000, echoCancellation: false, noiseSuppression: false }
      });
      const recorder = new MediaRecorder(stream);
      testRecorderRef.current = recorder;

      recorder.ondataavailable = (e) => {
        if (e.data.size > 0) testAudioChunksRef.current.push(e.data);
      };

      recorder.onstop = async () => {
        setIsRecordingTest(false);
        setIsVerifying(true);
        const testBlob = new Blob(testAudioChunksRef.current, { type: recorder.mimeType || 'audio/webm' });
        const { base64 } = await processRecordedAudioBlob(testBlob);

        try {
          const verifyPromise = fetch('/api/v1/voice/verify', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ wav_base64: base64 })
          });

          const sttPromise = fetch('/api/v1/whisper/transcribe', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ wav_base64: base64 })
          });

          const [verifyRes, sttRes] = await Promise.all([verifyPromise, sttPromise]);
          const data = await requireJson<Record<string, any>>(verifyRes, 'Voice verification could not be completed.');
          let sttResult = "[No speech detected]";
          if (sttRes.ok) {
            const sttData = await requireJson<Record<string, any>>(sttRes, 'Speech transcription is unavailable.');
            if (sttData.transcription) sttResult = sttData.transcription.trim();
          }

          setVerificationResult({
            authorized: data.authorized,
            similarity: data.similarity_percentage,
            speakerLabel: data.speaker_label,
            actionTaken: data.action_taken,
            sttResult
          });
        } catch (err: any) {
          setVerificationResult({
            authorized: false,
            similarity: 0,
            speakerLabel: "Verification Error",
            actionTaken: `Error connecting to backend: ${err.message}`,
            sttResult: "[STT Request Failed]"
          });
        } finally {
          setIsVerifying(false);
        }
        stream.getTracks().forEach(t => t.stop());
      };

      recorder.start(100);
      // Auto-stop after 3.5 seconds
      setTimeout(() => {
        if (recorder.state === 'recording') {
          recorder.stop();
        }
      }, 3500);
    } catch {
      setIsRecordingTest(false);
    }
  };

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: '24px' }}>
      {/* Wakeup Word & Custom Response Config Panel */}
      <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px', display: 'flex', flexDirection: 'column', gap: '16px' }}>
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
            <MessageSquare color="var(--accent-cyan)" size={20} />
            <h3 style={{ fontSize: '18px', fontWeight: '700', color: 'var(--text-primary)' }}>Custom Wakeup Trigger & Voice Response Setup</h3>
          </div>
          <button
            onClick={handleSaveSettings}
            disabled={isSavingSettings}
            style={{ padding: '8px 16px', borderRadius: '8px', background: 'var(--accent-cyan)', color: '#000', fontWeight: '600', border: 'none', cursor: 'pointer' }}
          >
            {isSavingSettings ? 'Saving...' : 'Save Voice Settings'}
          </button>
        </div>

        {settingsSavedMessage && (
          <div style={{ padding: '10px 14px', borderRadius: '6px', background: 'rgba(16, 185, 129, 0.15)', color: '#10b981', fontSize: '13px', border: '1px solid rgba(16, 185, 129, 0.3)' }}>
            {settingsSavedMessage}
          </div>
        )}

        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: '16px' }}>
          <div>
            <label style={{ fontSize: '12px', color: 'var(--text-secondary)', fontWeight: '600', display: 'block', marginBottom: '6px' }}>
              Wakeup Command Phrase
            </label>
            <input 
              type="text" 
              value={wakeupWord} 
              onChange={(e) => setWakeupWord(e.target.value)}
              style={{ width: '100%', padding: '10px 14px', borderRadius: '8px', background: 'rgba(0,0,0,0.4)', border: '1px solid var(--glass-border)', color: '#fff', fontSize: '14px' }} 
              placeholder="e.g. Hey Aqua"
            />
          </div>

          <div>
            <label style={{ fontSize: '12px', color: 'var(--text-secondary)', fontWeight: '600', display: 'block', marginBottom: '6px' }}>
              Voice Response / Reply Output
            </label>
            <input 
              type="text" 
              value={customReply} 
              onChange={(e) => setCustomReply(e.target.value)}
              style={{ width: '100%', padding: '10px 14px', borderRadius: '8px', background: 'rgba(0,0,0,0.4)', border: '1px solid var(--glass-border)', color: '#fff', fontSize: '14px' }} 
              placeholder="e.g. Yes Master, I am online."
            />
          </div>
        </div>

        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '16px', borderRadius: '8px', background: 'rgba(0,0,0,0.2)', border: '1px solid var(--glass-border)' }}>
          <div>
            <div style={{ fontSize: '15px', fontWeight: '600', color: '#fff', marginBottom: '4px' }}>Voice Security / Biometrics Lock</div>
            <div style={{ fontSize: '13px', color: 'var(--text-secondary)' }}>
              {voiceSecurityEnabled 
                ? "Enabled: System will only wake up if the voice matches the master biometric profile." 
                : "Disabled: ANYONE can trigger the system using the wake-up phrase."}
            </div>
          </div>
          <button 
            onClick={() => setVoiceSecurityEnabled(!voiceSecurityEnabled)}
            style={{ 
              padding: '8px 16px', borderRadius: '30px', fontWeight: '600', fontSize: '14px', cursor: 'pointer', transition: 'all 0.2s',
              background: voiceSecurityEnabled ? 'rgba(16, 185, 129, 0.2)' : 'rgba(239, 68, 68, 0.2)',
              color: voiceSecurityEnabled ? '#10b981' : '#f87171',
              border: `1px solid ${voiceSecurityEnabled ? 'rgba(16, 185, 129, 0.4)' : 'rgba(239, 68, 68, 0.4)'}`
            }}>
            {voiceSecurityEnabled ? 'Security Enabled' : 'Security Disabled'}
          </button>
        </div>
      </div>

      {/* Voice Calibration & Recording Review Wizard */}
      <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px', display: 'flex', flexDirection: 'column', gap: '20px' }}>
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
          <div>
            <h3 style={{ fontSize: '18px', fontWeight: '700', color: 'var(--text-primary)' }}>Master User Voice Calibration & Review Wizard</h3>
            <p style={{ fontSize: '13px', color: 'var(--text-secondary)' }}>Learns your voice from the recording (it is cut into parts that must agree with each other) and keeps an encrypted voice fingerprint for the voice lock.</p>
          </div>

          {!isEnrolling && (
            <button 
              onClick={handleStartEnrollment}
              style={{ padding: '10px 20px', borderRadius: '8px', background: 'var(--accent-purple)', color: '#fff', fontWeight: '600', border: 'none', cursor: 'pointer', display: 'flex', alignItems: 'center', gap: '8px' }}>
              <Mic size={16} /> {enrolledFingerprint ? "Re-Record Master Voice" : "Start Voice Enrollment"}
            </button>
          )}
        </div>

        {enrolledFingerprint && !isEnrolling && (
          <div style={{ padding: '16px', borderRadius: '8px', background: 'rgba(16, 185, 129, 0.1)', border: '1px solid rgba(16, 185, 129, 0.25)', display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
            <div style={{ display: 'flex', alignItems: 'center', gap: '12px' }}>
              <CheckCircle2 color="#10b981" size={20} />
              <div>
                <div style={{ fontSize: '14px', fontWeight: '600', color: '#10b981' }}>Active Master Voice Profile Saved in SQLite Database</div>
                <div style={{ fontSize: '12px', color: 'var(--text-muted)' }} className="mono">Fingerprint: {enrolledFingerprint} | Trigger: "{wakeupWord}" | Reply: "{customReply}"</div>
              </div>
            </div>
            <span style={{ fontSize: '12px', color: 'var(--accent-emerald)', padding: '4px 10px', borderRadius: '12px', background: 'rgba(16, 185, 129, 0.2)', fontWeight: '600' }}>
              Single-User Lock Active
            </span>
          </div>
        )}

        {isEnrolling && (
          <div style={{ padding: '24px', borderRadius: '10px', background: 'rgba(139, 92, 246, 0.08)', border: '1px solid rgba(139, 92, 246, 0.3)', display: 'flex', flexDirection: 'column', gap: '16px' }}>
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
                <ShieldCheck color="var(--accent-purple)" size={18} />
                <span style={{ fontWeight: '600', color: 'var(--accent-purple)' }}>Calibration Step {currentStep} of 3</span>
              </div>
              <span style={{ fontSize: '12px', color: 'var(--text-secondary)' }}>Speak phrase clearly into microphone</span>
            </div>

            <div style={{ padding: '20px', background: 'rgba(0,0,0,0.4)', borderRadius: '8px', fontSize: '18px', fontWeight: '600', textAlign: 'center', color: 'var(--text-primary)', border: '1px solid var(--glass-border)' }}>
              {calibrationPhrases[currentStep - 1]}
            </div>

            {/* Audio Waveform & Recorded Sample Details */}
            {recordedSamples[currentStep] ? (
              <div style={{ padding: '16px', borderRadius: '8px', background: 'rgba(6, 182, 212, 0.1)', border: '1px solid rgba(6, 182, 212, 0.3)', display: 'flex', flexDirection: 'column', gap: '12px' }}>
                <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
                  <div style={{ display: 'flex', alignItems: 'center', gap: '12px' }}>
                    <Volume2 color="var(--accent-cyan)" size={18} />
                    <div>
                      <span style={{ fontSize: '14px', fontWeight: '600', color: 'var(--accent-cyan)' }}>Voice Sample Step {currentStep} Recorded</span>
                      <div style={{ fontSize: '12px', color: 'var(--text-muted)' }}>
                        Quality: {recordedSamples[currentStep].metrics.qualityLabel} | Duration: {recordedSamples[currentStep].metrics.durationSec}s | RMS: {recordedSamples[currentStep].metrics.rmsDb} dB | Peak: {recordedSamples[currentStep].metrics.peakDb} dB
                      </div>
                    </div>
                  </div>

                  <div style={{ display: 'flex', gap: '8px' }}>
                    <button 
                      onClick={() => handlePlaySample(currentStep)} 
                      disabled={activePlaybackStep === currentStep}
                      style={{ padding: '6px 14px', borderRadius: '6px', background: 'rgba(255,255,255,0.1)', color: '#fff', border: 'none', cursor: 'pointer', fontSize: '12px', display: 'flex', alignItems: 'center', gap: '6px' }}>
                      <Volume2 size={14} /> {activePlaybackStep === currentStep ? "Playing Audio..." : "Review Audio Recording"}
                    </button>
                    <button 
                      onClick={handleReRecordStep} 
                      style={{ padding: '6px 14px', borderRadius: '6px', background: 'rgba(239, 68, 68, 0.2)', color: '#f87171', border: '1px solid rgba(239, 68, 68, 0.4)', cursor: 'pointer', fontSize: '12px', display: 'flex', alignItems: 'center', gap: '6px' }}>
                      <RotateCcw size={14} /> Replace / Re-Record
                    </button>
                  </div>
                </div>

                <div style={{ padding: '10px 14px', background: 'rgba(0,0,0,0.3)', borderRadius: '6px', border: '1px solid rgba(255,255,255,0.05)', fontSize: '13px' }}>
                  <span style={{ color: 'var(--text-secondary)', marginRight: '6px' }}>AI Understood:</span>
                  <span style={{ color: '#fff', fontWeight: '500', fontStyle: 'italic' }}>"{recordedSamples[currentStep].sttResult}"</span>
                  {(() => {
                    if (saidWakePhrase(recordedSamples[currentStep].sttResult, wakeupWord)) {
                      return (
                        <span style={{ marginLeft: '10px', color: '#10b981', fontSize: '12px' }}>
                          {currentStep < 3 ? '(Match! Auto-advancing...)' : '(Match! Press Save & Activate Profile to keep it.)'}
                        </span>
                      );
                    }
                    return null;
                  })()}
                </div>
              </div>
            ) : (
              <div style={{ display: 'flex', flexDirection: 'column', gap: '12px', alignItems: 'center' }}>
                {!isRecordingStep && !isProcessingStt ? (
                  <button 
                    onClick={startStepRecording}
                    style={{ padding: '12px 24px', borderRadius: '8px', background: 'var(--accent-purple)', color: '#fff', fontWeight: '600', border: 'none', cursor: 'pointer', display: 'flex', alignItems: 'center', gap: '8px' }}>
                    <Mic size={16} /> Record Calibration Sample #{currentStep}
                  </button>
                ) : isProcessingStt ? (
                  <button 
                    disabled
                    style={{ padding: '12px 24px', borderRadius: '8px', background: 'var(--glass-surface)', color: 'var(--accent-cyan)', fontWeight: '700', border: '1px solid var(--accent-cyan)', cursor: 'not-allowed', display: 'flex', alignItems: 'center', gap: '8px', animation: 'pulse 1.5s infinite' }}>
                    <RefreshCw size={16} className="spin" /> Transcribing voice audio...
                  </button>
                ) : (
                  <button 
                    onClick={stopStepRecording}
                    style={{ padding: '12px 24px', borderRadius: '8px', background: '#ef4444', color: '#fff', fontWeight: '700', border: 'none', cursor: 'pointer', display: 'flex', alignItems: 'center', gap: '8px', animation: 'pulse 1s infinite' }}>
                    <Square size={16} /> Stop Recording Sample #{currentStep}
                  </button>
                )}
              </div>
            )}

            <div style={{ display: 'flex', justifyContent: 'flex-end', gap: '12px', marginTop: '10px' }}>
              <button 
                id="btn-next-step"
                onClick={handleNextStep}
                disabled={!recordedSamples[currentStep]}
                style={{ 
                  padding: '10px 24px', borderRadius: '6px', 
                  background: recordedSamples[currentStep] ? 'var(--accent-cyan)' : 'rgba(255,255,255,0.1)', 
                  color: recordedSamples[currentStep] ? '#000' : 'var(--text-muted)', 
                  fontWeight: '600', border: 'none', cursor: recordedSamples[currentStep] ? 'pointer' : 'not-allowed', 
                  display: 'flex', alignItems: 'center', gap: '8px' 
                }}>
                <RefreshCw size={16} /> {currentStep === 3 ? "Save & Activate Profile" : "Confirm & Next Step ->"}
              </button>
            </div>
          </div>
        )}
      </div>

      {/* Live Speaker Biometrics Tester Card */}
      <div className="glass-panel" style={{ padding: '24px', borderRadius: '12px', display: 'flex', flexDirection: 'column', gap: '16px' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
          <UserCheck color="var(--accent-purple)" size={20} />
          <div>
            <h3 style={{ fontSize: '18px', fontWeight: '700', color: 'var(--text-primary)' }}>Live Microphone Voice Biometrics Verification</h3>
            <p style={{ fontSize: '13px', color: 'var(--text-secondary)' }}>Records live microphone audio and checks how closely it matches your enrolled voice fingerprint.</p>
          </div>
        </div>

        <div>
          {!isRecordingTest ? (
            <button 
              onClick={startLiveSpeakerVerificationTest}
              disabled={isVerifying}
              style={{ padding: '12px 24px', borderRadius: '8px', background: 'rgba(16, 185, 129, 0.2)', border: '1px solid rgba(16, 185, 129, 0.4)', color: '#10b981', fontWeight: '700', cursor: 'pointer', display: 'flex', alignItems: 'center', gap: '8px' }}>
              <Mic size={18} /> {isVerifying ? 'Verifying Voice Profile...' : 'Record Live Voice & Verify Match'}
            </button>
          ) : (
            <div style={{ padding: '12px 18px', borderRadius: '8px', background: 'rgba(239, 68, 68, 0.2)', color: '#ef4444', fontWeight: '700', border: '1px solid rgba(239, 68, 68, 0.4)', display: 'inline-flex', alignItems: 'center', gap: '8px' }}>
              <Square size={18} /> Recording Voice Sample for Verification... (Speaking phrase)
            </div>
          )}
        </div>

        {isVerifying && (
          <div style={{ padding: '14px', borderRadius: '8px', background: 'rgba(255,255,255,0.05)', textAlign: 'center', fontSize: '14px', color: 'var(--accent-cyan)' }}>
            Comparing this recording with your enrolled voice...
          </div>
        )}

        {verificationResult && !isVerifying && (
          <div style={{ 
            padding: '18px', borderRadius: '10px', 
            background: verificationResult.authorized ? 'rgba(16, 185, 129, 0.12)' : 'rgba(239, 68, 68, 0.12)', 
            border: `1px solid ${verificationResult.authorized ? 'rgba(16, 185, 129, 0.3)' : 'rgba(239, 68, 68, 0.3)'}`,
            display: 'flex', flexDirection: 'column', gap: '8px'
          }}>
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
                {verificationResult.authorized ? <UserCheck color="#10b981" size={22} /> : <UserX color="#f87171" size={22} />}
                <span style={{ fontSize: '16px', fontWeight: '700', color: verificationResult.authorized ? '#10b981' : '#f87171' }}>
                  {verificationResult.speakerLabel}
                </span>
              </div>
              <span style={{ fontSize: '14px', fontWeight: '700', color: verificationResult.authorized ? '#10b981' : '#f87171' }} className="mono">
                Voice Match Confidence: {verificationResult.similarity}% (Required: 82%)
              </span>
            </div>

            <div style={{ fontSize: '13px', color: 'var(--text-primary)', marginTop: '4px' }}>
              <strong>System Action:</strong> {verificationResult.actionTaken}
            </div>

            {verificationResult.sttResult && (
              <div style={{ padding: '8px 12px', background: 'rgba(0,0,0,0.2)', borderRadius: '6px', border: '1px solid rgba(255,255,255,0.05)', fontSize: '13px', marginTop: '4px' }}>
                <span style={{ color: 'var(--text-secondary)', marginRight: '6px' }}>AI Understood:</span>
                <span style={{ color: '#fff', fontStyle: 'italic' }}>"{verificationResult.sttResult}"</span>
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
