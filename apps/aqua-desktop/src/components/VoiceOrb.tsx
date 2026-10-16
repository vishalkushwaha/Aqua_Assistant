import { useState, useEffect, useRef, useCallback } from 'react';
import { useCoreStatus } from '../hooks/useCoreStatus';
import { NeuralOrb } from './NeuralOrb';
import { startWakeListener } from './wakeWord';
import { runTask, stopTask, useTasks } from '../tasks';
import { subscribeActivity } from '../activity';
import { startDevicePlayer } from '../devicesPlayer';
import { withAudioLock } from '../api/speech';
import { streamTurn } from '../api/pipelineStream';
import type { ActivityMessage } from '../activity';

type Phase = 'idle' | 'listening' | 'thinking' | 'speaking';

const SAMPLE_RATE = 16000;
const SPEECH_LEVEL = 0.02; // RMS above this counts as speech
const SILENCE_MS = 1500; // stop this long after the person stops talking
const MAX_LISTEN_MS = 30000;
const DRAG_PX = 4;

type TauriInternals = { invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown> };
const tauriInvoke = (cmd: string, args?: Record<string, unknown>): void => {
  const t = (window as unknown as { __TAURI_INTERNALS__?: TauriInternals }).__TAURI_INTERNALS__;
  t?.invoke(cmd, args).catch((e) => console.warn(`[orb] ${cmd} failed`, e));
};

// Accent for the text box, matching the orb's colour for each state.
const KEEP_ALIVE_MS = 20000; // the core closes an idle voice session after 60 s, so a conversation renews it
const MAX_FAILS = 3; // consecutive failed turns before the conversation ends by itself
// What ends a conversation. The core knows the same phrases; this is the fallback if it does not report it.
const END_PHRASES = /^(?:hey aqua |aqua )?(?:bye|bye bye|goodbye|good bye|sleep|go to sleep)$/;
const isEndPhrase = (text: string) => END_PHRASES.test(text.toLowerCase().replace(/[^a-z ]/g, ' ').replace(/\s+/g, ' ').trim());

/** Open or close the core's voice session (a deliberate click replaces the wake word). Never throws. */
async function setVoiceSession(active: boolean): Promise<void> {
  try {
    await fetch('/api/v1/voice/session', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ active }),
    });
  } catch (e) {
    console.warn('[orb] voice session call failed', e);
  }
}

const REMOTE_TTL_MS = 6000; // an admin-panel task that stops reporting is treated as finished after this
const REMOTE_WORD: Record<Phase, string> = { idle: '', listening: 'Listening…', thinking: 'Working…', speaking: 'Speaking…' };
const INPUT_H = 570; // window height with the text box open
const PROMPT_H = 390; // window height while the sent text is shown under "Thinking..."
const REPLY_H = 470; // window height while an answer is shown
/** How long the orb waits for one answer before giving up on it (a little over the core's own 600 s limit for a turn). */
const TURN_ABORT_MS = 11 * 60 * 1000;

const ACCENT: Record<Phase, string> = { idle: '#ffb03c', listening: '#28c8ff', thinking: '#b060ff', speaking: '#38e88a' };

/** Recorded audio (webm/ogg) to 16 kHz mono 16-bit WAV, base64: the form /api/v1/pipeline/wav takes. */
async function blobToWavBase64(blob: Blob): Promise<string> {
  const decodeCtx = new AudioContext();
  const decoded = await decodeCtx.decodeAudioData(await blob.arrayBuffer());
  await decodeCtx.close();
  const offline = new OfflineAudioContext(1, Math.max(1, Math.ceil(decoded.duration * SAMPLE_RATE)), SAMPLE_RATE);
  const src = offline.createBufferSource();
  src.buffer = decoded;
  src.connect(offline.destination);
  src.start(0);
  const pcm = (await offline.startRendering()).getChannelData(0);

  const buf = new ArrayBuffer(44 + pcm.length * 2);
  const v = new DataView(buf);
  const str = (o: number, s: string) => { for (let i = 0; i < s.length; i++) v.setUint8(o + i, s.charCodeAt(i)); };
  str(0, 'RIFF'); v.setUint32(4, 36 + pcm.length * 2, true); str(8, 'WAVE'); str(12, 'fmt ');
  v.setUint32(16, 16, true); v.setUint16(20, 1, true); v.setUint16(22, 1, true);
  v.setUint32(24, SAMPLE_RATE, true); v.setUint32(28, SAMPLE_RATE * 2, true); v.setUint16(32, 2, true); v.setUint16(34, 16, true);
  str(36, 'data'); v.setUint32(40, pcm.length * 2, true);
  for (let i = 0; i < pcm.length; i++) {
    const s = Math.max(-1, Math.min(1, pcm[i]));
    v.setInt16(44 + i * 2, s < 0 ? s * 0x8000 : s * 0x7fff, true);
  }
  const bytes = new Uint8Array(buf);
  let bin = '';
  for (let i = 0; i < bytes.length; i += 0x8000) bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(bin);
}

export function VoiceOrb() {
  const { connected } = useCoreStatus();
  const [phase, setPhase] = useState<Phase>('idle');
  const [note, setNote] = useState<string | null>(null);
  const [textOpen, setTextOpen] = useState(false); // "Task in Text": a text box under the orb
  const [draft, setDraft] = useState('');
  const [reply, setReply] = useState<string | null>(null);
  const [submitted, setSubmitted] = useState<string | null>(null); // the text just sent, shown while the assistant works
  const [appear, setAppear] = useState(0); // bumped each time the orb is shown, to replay the grow-in

  const phaseRef = useRef<Phase>('idle');
  const stopListeningRef = useRef<(() => void) | null>(null);
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const conversationRef = useRef(false); // on until the person says bye / sleep / go to sleep (or ends it from the menu)
  const [conversing, setConversing] = useState(false);
  const [paused, setPaused] = useState(false); // minimized: the orb is hidden, so the microphone is not kept open
  const keepAliveRef = useRef<number | null>(null);
  const failsRef = useRef(0);
  const resumeRef = useRef<() => void>(() => undefined); // listen again after a turn, while the conversation lasts
  const levelRef = useRef(0); // live voice level 0..1 for the orb: the microphone, then the reply audio

  const go = (p: Phase) => { phaseRef.current = p; setPhase(p); };

  // Tasks (voice, text, and the admin panel's) run one at a time, in the order they were given: see ../tasks.ts.
  const tasks = useTasks();
  const [queuedAhead, setQueuedAhead] = useState(0); // this orb's task is waiting behind this many others

  // Speaks the device announcements the core queues (a device connected, a battery low). The core decides what and when.
  useEffect(() => startDevicePlayer(), []);

  useEffect(() => {
    document.documentElement.style.backgroundColor = 'transparent';
    document.body.style.backgroundColor = 'transparent';
    return () => { document.documentElement.style.backgroundColor = ''; document.body.style.backgroundColor = ''; };
  }, []);

  // The tray icon shows the orb window and calls this, so the orb grows in again each time.
  useEffect(() => {
    const w = window as unknown as { __orbShow?: () => void };
    w.__orbShow = () => { setAppear((n) => n + 1); setPaused(false); };
    return () => { delete w.__orbShow; };
  }, []);

  const endConversation = useCallback(() => {
    if (!conversationRef.current) return;
    conversationRef.current = false;
    setConversing(false);
    failsRef.current = 0;
    if (keepAliveRef.current !== null) { window.clearInterval(keepAliveRef.current); keepAliveRef.current = null; }
    void setVoiceSession(false);
    stopListeningRef.current?.(); // let go of the microphone
  }, []);

  const beginConversation = useCallback(async () => {
    conversationRef.current = true;
    setConversing(true);
    failsRef.current = 0;
    await setVoiceSession(true);
    if (keepAliveRef.current === null) {
      keepAliveRef.current = window.setInterval(() => void setVoiceSession(true), KEEP_ALIVE_MS);
    }
  }, []);

  // The right-click menu's "End Conversation" and "Minimize" call this.
  useEffect(() => {
    const w = window as unknown as { __orbEnd?: () => void };
    w.__orbEnd = endConversation;
    return () => { delete w.__orbEnd; };
  }, [endConversation]);
  useEffect(() => endConversation, [endConversation]);

  // Minimize: end any conversation and stop listening for the wake phrase until the orb is shown again.
  useEffect(() => {
    const w = window as unknown as { __orbMinimized?: () => void };
    w.__orbMinimized = () => { endConversation(); setPaused(true); };
    return () => { delete w.__orbMinimized; };
  }, [endConversation]);

  /**
   * Play the spoken reply (base64 WAV). The orb swells and shrinks with its loudness. Resolves when the voice has ended or
   * `signal` aborted (Stop); either way the sound and its audio resources are let go of before it resolves.
   */
  const playReply = useCallback((wavBase64: string, signal?: AbortSignal): Promise<void> => new Promise<void>((resolve) => {
    // Stop was pressed while this piece waited for the audio lock: an aborted signal never fires its listener, so it would play in full.
    if (signal?.aborted) { resolve(); return; }
    const audio = new Audio(`data:audio/wav;base64,${wavBase64}`);
    audioRef.current = audio;
    const actx = new AudioContext();
    const analyser = actx.createAnalyser();
    analyser.fftSize = 1024;
    actx.createMediaElementSource(audio).connect(analyser);
    analyser.connect(actx.destination);
    const samples = new Float32Array(analyser.fftSize);
    const meter = window.setInterval(() => {
      analyser.getFloatTimeDomainData(samples);
      let sum = 0;
      for (const v of samples) sum += v * v;
      levelRef.current = Math.min(1, Math.sqrt(sum / samples.length) * 5);
    }, 33);
    let finished = false;
    const done = () => {
      if (finished) return;
      finished = true;
      window.clearInterval(meter);
      levelRef.current = 0;
      signal?.removeEventListener('abort', done);
      audio.pause();
      audio.removeAttribute('src');
      audio.load();
      void actx.close();
      audioRef.current = null;
      resolve();
    };
    signal?.addEventListener('abort', done, { once: true });
    audio.onended = done;
    audio.onerror = done;
    go('speaking');
    void (async () => {
      // WebView2 can start the context suspended; resume it so the reply is actually heard.
      await actx.resume().catch(() => undefined);
      await audio.play().catch((e) => {
        console.error('[orb] could not play the reply', e);
        setNote('Could not play the voice reply');
        done();
      });
    })();
  }), []);

  const openText = useCallback(() => {
    setPaused(false);
    setTextOpen(true);
    tauriInvoke('orb_resize', { height: INPUT_H, editing: true });
  }, []);
  const closeText = useCallback(() => {
    setTextOpen(false);
    setReply(null);
    tauriInvoke('orb_resize', { height: 300 });
  }, []);
  const dismissReply = useCallback(() => {
    setReply(null);
    tauriInvoke('orb_resize', { height: 300 });
  }, []);
  useEffect(() => {
    const w = window as unknown as { __orbText?: () => void };
    w.__orbText = openText;
    return () => { delete w.__orbText; };
  }, [openText]);

  /** The same assistant flow as a spoken request, starting from typed text: it queues, then runs alone, and the answer is shown and spoken. */
  const sendText = useCallback(async () => {
    const text = draft.trim();
    if (!text) return;
    if (phaseRef.current !== 'idle') { setNote('Busy, try again in a moment'); return; }
    go('thinking');
    setReply(null);
    setTextOpen(false); // the box goes away; only the status text under the orb remains
    setDraft('');
    setSubmitted(text);
    tauriInvoke('orb_resize', { height: PROMPT_H });
    const outcome = await runTask('Text task', async (ctx) => {
      // The core speaks the answer piece by piece as it writes it: the first sentence is heard seconds before the rest exists. Each
      // piece's words are shown as it starts to play, and the exact text replaces them when the core has finished.
      let shown = '';
      const showReply = (answer: string) => {
        setReply(answer.trim() || '(no answer)');
        setSubmitted(null);
        tauriInvoke('orb_resize', { height: REPLY_H });
      };
      await streamTurn('/api/v1/pipeline/text/stream', { text }, ctx.signal, {
        onSpeech: async (piece) => {
          shown = shown ? `${shown} ${piece.text}` : piece.text;
          showReply(shown);
          if (piece.wav_base64) {
            ctx.setPhase('speaking');
            await withAudioLock(() => playReply(piece.wav_base64, ctx.signal));
          }
        },
        onDone: (data) => showReply(String(data.assistant_text ?? '')),
      }, { fallbackPath: '/api/v1/pipeline/text' });
    }, { maxMs: TURN_ABORT_MS, onQueued: setQueuedAhead });
    setQueuedAhead(0);
    if (outcome.stopped) {
      setNote('Stopped');
      setSubmitted(null);
      tauriInvoke('orb_resize', { height: 300 });
    } else if (outcome.error) {
      console.error('[orb] text task failed', outcome.error);
      setNote(outcome.error instanceof Error ? outcome.error.message : String(outcome.error));
      setDraft(text); // keep what was typed so it can be sent again
      setSubmitted(null);
      tauriInvoke('orb_resize', { height: 300 });
    }
    go('idle');
  }, [draft, playReply]);

  useEffect(() => {
    if (!note) return;
    const t = window.setTimeout(() => setNote(null), 4000);
    return () => window.clearTimeout(t);
  }, [note]);

  /** Send the recording through Whisper (speech to text), the assistant, and Kokoro (speech out), then play the reply. It is a task: it waits its turn, runs alone, and ends when the reply has been spoken or stopped. */
  const runPipeline = useCallback(async (blob: Blob) => {
    go('thinking');
    const outcome = await runTask('Voice request', async (ctx) => {
      const wav = await blobToWavBase64(blob);
      // The core speaks the answer piece by piece as it writes it, so the first sentence is heard seconds before the rest exists.
      const data = await streamTurn('/api/v1/pipeline/wav/stream', { wav_base64: wav, active_session: true }, ctx.signal, {
        onSpeech: async (piece) => {
          if (!piece.wav_base64) return;
          ctx.setPhase('speaking');
          await withAudioLock(() => playReply(piece.wav_base64, ctx.signal));
        },
      }, { fallbackPath: '/api/v1/pipeline/wav' });
      const r = data.result ?? data;
      failsRef.current = 0;
      // The core closes the session on a sleep command; "bye" and friends are checked here as well.
      if (r.session_action === 'sleep' || data.session_active === false || isEndPhrase(String(r.transcribed_text ?? ''))) {
        endConversation();
      }
      // Noise (wind, typing, a fan) is ignored without a word; only speech that was not understood gets a note.
      if (!r.transcribed_text && !String(r.assistant_text ?? '').startsWith('IGNORED: not speech')) setNote("Didn't catch that");
    }, { maxMs: TURN_ABORT_MS, onQueued: setQueuedAhead });
    setQueuedAhead(0);
    if (outcome.stopped) {
      setNote('Stopped'); // the person cancelled it: not a failure
    } else if (outcome.error) {
      console.error('[orb] voice pipeline failed', outcome.error);
      setNote(outcome.error instanceof Error ? outcome.error.message : String(outcome.error));
      if (++failsRef.current >= MAX_FAILS) endConversation();
    }
    go('idle');
    window.setTimeout(() => resumeRef.current(), 600); // then listens again, if the conversation is still on
  }, [playReply, endConversation]);

  /** Record from the microphone until the person goes quiet (or clicks again), then hand it to the pipeline. */
  const startListening = useCallback(async () => {
    setNote(null);
    let stream: MediaStream;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: false } });
    } catch (e) {
      setNote('Microphone is not available');
      console.error('[orb] microphone', e);
      return;
    }
    go('listening');

    let recorder: MediaRecorder;
    try {
      recorder = new MediaRecorder(stream);
    } catch (e) {
      // Without this the microphone stayed open and the orb stayed on "listening".
      stream.getTracks().forEach((t) => t.stop());
      go('idle');
      setNote('Could not start recording');
      console.error('[orb] recorder', e);
      return;
    }
    const chunks: Blob[] = [];
    recorder.ondataavailable = (ev) => { if (ev.data.size) chunks.push(ev.data); };

    const ctx = new AudioContext();
    const analyser = ctx.createAnalyser();
    analyser.fftSize = 1024;
    ctx.createMediaStreamSource(stream).connect(analyser);
    const samples = new Float32Array(analyser.fftSize);

    let heardSpeech = false;
    let loudPolls = 0;
    let lastVoice = performance.now();
    const began = lastVoice;

    const stop = () => { if (recorder.state !== 'inactive') recorder.stop(); };
    const timer = window.setInterval(() => {
      analyser.getFloatTimeDomainData(samples);
      let sum = 0;
      for (const s of samples) sum += s * s;
      const rms = Math.sqrt(sum / samples.length);
      levelRef.current = Math.min(1, rms * 8);
      const now = performance.now();
      // A click or a key press is over in a few milliseconds; a voice holds. Two polls in a row (160 ms) above the level count as speech.
      if (rms > SPEECH_LEVEL) { if (++loudPolls >= 2) { heardSpeech = true; lastVoice = now; } } else { loudPolls = 0; }
      if ((heardSpeech && now - lastVoice > SILENCE_MS) || now - began > MAX_LISTEN_MS) stop();
    }, 80);
    stopListeningRef.current = stop;

    recorder.onstop = () => {
      window.clearInterval(timer);
      stream.getTracks().forEach((t) => t.stop());
      void ctx.close();
      stopListeningRef.current = null;
      levelRef.current = 0;
      if (!conversationRef.current) { go('idle'); return; } // ended from the menu while recording: drop it
      if (!heardSpeech) {
        go('idle');
        if (conversationRef.current) resumeRef.current(); // nobody spoke yet: keep listening
        else setNote("Didn't hear anything");
        return;
      }
      void runPipeline(new Blob(chunks, { type: recorder.mimeType }));
    };
    recorder.start();
  }, [runPipeline]);

  // What the admin panel is doing (recording, working on a task, speaking): when the orb has nothing of its own going on,
  // it shows the same, including the live voice level, so the orb follows the work in real time.
  const remoteRef = useRef(new Map<string, ActivityMessage & { seen: number }>());
  const [remote, setRemote] = useState<{ phase: Phase; label: string } | null>(null);
  useEffect(() => {
    const recompute = () => {
      const now = Date.now();
      const all = remoteRef.current;
      for (const [k, v] of all) if (now - v.seen > REMOTE_TTL_MS) all.delete(k);
      // Speaking beats working beats listening; between equals the most recent report wins.
      const rank = (p: string) => (p === 'speaking' ? 3 : p === 'thinking' ? 2 : 1);
      let best: (ActivityMessage & { seen: number }) | null = null;
      for (const v of all.values()) if (!best || rank(v.phase) > rank(best.phase) || (rank(v.phase) === rank(best.phase) && v.seen > best.seen)) best = v;
      if (!best && phaseRef.current === 'idle') levelRef.current = 0;
      setRemote((prev) => {
        if (!best) return prev === null ? prev : null;
        const label = best.label ?? best.source;
        return prev && prev.phase === best.phase && prev.label === label ? prev : { phase: best.phase as Phase, label };
      });
    };
    const off = subscribeActivity((msg) => {
      if (msg.phase === 'idle') remoteRef.current.delete(msg.source);
      else remoteRef.current.set(msg.source, { ...msg, seen: Date.now() });
      if (msg.phase !== 'idle' && typeof msg.level === 'number' && phaseRef.current === 'idle') levelRef.current = msg.level;
      recompute();
    });
    const sweep = window.setInterval(recompute, 1000);
    return () => { off(); window.clearInterval(sweep); };
  }, []);
  // What "Stop" would stop right now, for the right-click menu: the task running anywhere (admin panel or orb), else this orb's own work.
  const stopKind: 'thinking' | 'speaking' | null =
    tasks.current ? (tasks.current.phase === 'speaking' ? 'speaking' : 'thinking') : phase === 'speaking' ? 'speaking' : phase === 'thinking' ? 'thinking' : null;
  useEffect(() => {
    const w = window as unknown as { __orbStop?: () => void };
    w.__orbStop = stopTask; // the menu's Stop Thinking / Stop Speaking
    return () => { delete w.__orbStop; };
  }, []);

  // While a task is running the microphone keeps listening for a spoken "stop", through the core's own lane for it (it works
  // while the core is busy). It ends the task at once, wherever the task is running.
  const taskRunning = tasks.current !== null || phase === 'thinking' || phase === 'speaking';
  useEffect(() => {
    if (!taskRunning || !connected || paused) return;
    const listener = startWakeListener({
      mode: 'stop',
      onWake: (heard) => { console.log('[orb] spoken stop heard:', heard); stopTask(); },
      onError: (e) => console.warn('[orb] stop listener', e),
    });
    return () => listener.stop();
  }, [taskRunning, connected, paused]);

  const shown: Phase = phase !== 'idle' ? phase : remote?.phase ?? 'idle'; // what the orb looks like
  const mirroring = phase === 'idle' && remote !== null;

  // Standby: while idle (and not in a conversation) the microphone stays open for the wake phrase. Saying "Hey Aqua" or "wake up"
  // anywhere in a sentence starts a conversation and goes to listening.
  // The wake-word listener also waits while the admin panel is using the microphone or the speakers.
  const standby = connected && phase === 'idle' && !conversing && !paused && !remote;
  useEffect(() => {
    if (!standby) return;
    const listener = startWakeListener({
      onWake: (heard) => {
        if (phaseRef.current !== 'idle' || conversationRef.current) return;
        console.log('[orb] wake phrase heard:', heard);
        void beginConversation().then(() => startListening());
      },
      onError: (e) => console.warn('[orb] wake listener', e),
    });
    return () => listener.stop();
  }, [standby, beginConversation, startListening]);

  useEffect(() => {
    resumeRef.current = () => {
      if (conversationRef.current && phaseRef.current === 'idle') void startListening();
    };
  }, [startListening]);

  /** Left click: start listening; click again to finish early (listening) or to stop the reply (speaking). */
  const onOrbClick = useCallback(() => {
    switch (phaseRef.current) {
      case 'idle':
        // Turning the orb on starts a conversation: it keeps listening after every reply until the person says bye.
        void beginConversation().then(() => startListening());
        break;
      case 'listening':
        stopListeningRef.current?.();
        break;
      default:
        // Thinking or speaking: stop the task (the same as Stop in the menu or saying "stop"). The conversation stays open.
        stopTask();
        break;
    }
  }, [startListening, beginConversation]);

  // Left button: a click listens; pressing and moving drags the window (Tauri's own drag region would swallow the click).
  const press = useRef<{ x: number; y: number; dragged: boolean } | null>(null);
  const onMouseDown = (e: React.MouseEvent) => {
    if (e.button === 0) press.current = { x: e.screenX, y: e.screenY, dragged: false };
  };
  const onMouseMove = (e: React.MouseEvent) => {
    const p = press.current;
    if (!p || p.dragged || !(e.buttons & 1)) return;
    if (Math.hypot(e.screenX - p.x, e.screenY - p.y) > DRAG_PX) {
      p.dragged = true;
      tauriInvoke('plugin:window|start_dragging');
    }
  };
  const onMouseUp = (e: React.MouseEvent) => {
    const p = press.current;
    press.current = null;
    if (e.button === 0 && p && !p.dragged) onOrbClick();
  };

  const label =
    note ?? (!connected ? 'Starting…' : queuedAhead > 0 ? `Waiting for ${queuedAhead === 1 ? 'the current task' : `${queuedAhead} tasks`} to finish…` : mirroring && remote ? `${REMOTE_WORD[remote.phase]} (${remote.label})` : standby && !textOpen && !reply && !submitted ? 'Say "Hey Aqua" or "wake up"' : phase === 'listening' ? (conversing ? 'Listening… say "bye" to stop' : 'Listening… click to send') : phase === 'thinking' ? 'Thinking… click to stop' : phase === 'speaking' ? 'Speaking… click to stop' : null);

  const accent = ACCENT[shown];
  const hasPanel = textOpen || !!submitted || !!reply;
  const pullUp = -30; // the orb's box has empty space under the sphere; panels tuck into it
  const busy = phase === 'thinking';

  // Only the sphere, the label and the answer panels take mouse clicks. The rest of the orb's square window lets clicks through to
  // the files and buttons underneath (the desktop shell clips the window to these shapes). While the orb is active its rings swell
  // out towards the window's edge, so the clickable circle is larger then; at rest it is just the sphere and its glow.
  const sphereRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const send = () => {
      const box = sphereRef.current?.getBoundingClientRect();
      if (!box) return;
      const r = box.width * (shown === 'idle' ? 0.36 : 0.5);
      const cx = box.left + box.width / 2;
      const cy = box.top + box.height / 2;
      const shapes: { kind: string; x: number; y: number; w: number; h: number }[] = [{ kind: 'ellipse', x: cx - r, y: cy - r, w: 2 * r, h: 2 * r }];
      document.querySelectorAll('[data-hit]').forEach((el) => {
        const b = el.getBoundingClientRect();
        if (b.width > 0 && b.height > 0) shapes.push({ kind: 'rect', x: b.left, y: b.top, w: b.width, h: b.height });
      });
      tauriInvoke('orb_hit_regions', { shapes, dpr: window.devicePixelRatio || 1 });
    };
    const t1 = window.setTimeout(send, 50);
    const t2 = window.setTimeout(send, 450); // again once the window has finished resizing
    return () => {
      window.clearTimeout(t1);
      window.clearTimeout(t2);
    };
  }, [shown, label, hasPanel, textOpen, reply, submitted]);

  return (
    <div style={{ width: '100vw', height: '100vh', display: 'flex', flexDirection: 'column', alignItems: 'center', backgroundColor: 'transparent', overflow: 'hidden' }}>
      <style>{`@keyframes orbAppear { from { transform: scale(0.1); opacity: 0; } to { transform: scale(1); opacity: 1; } }`}</style>
      <div ref={sphereRef} style={{ width: 300, height: 300, flex: 'none', position: 'relative', display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
        <NeuralOrb
          key={appear}
          phase={shown}
          connected={connected}
          levelRef={levelRef}
          onMouseDown={onMouseDown}
          onMouseMove={onMouseMove}
          onMouseUp={onMouseUp}
          onContextMenu={(e) => { e.preventDefault(); tauriInvoke('orb_menu', { stop: stopKind }); }}
        />
        {label && (
          <div data-hit="" style={{ position: 'absolute', bottom: hasPanel ? 40 : 8, padding: '2px 10px', borderRadius: 10, background: 'rgba(0,0,0,0.6)', color: '#e5f4f6', font: '12px system-ui, sans-serif', pointerEvents: 'none' }}>
            {label}
          </div>
        )}
      </div>
      {submitted && (
        <div
          data-hit=""
          style={{
            marginTop: pullUp, width: 280, flex: 'none', maxHeight: 100, overflowY: 'auto', whiteSpace: 'pre-wrap', wordBreak: 'break-word', padding: '8px 12px', borderRadius: 12,
            background: 'rgba(16,16,22,0.9)', border: `1px solid ${accent}55`, color: '#cfd6d9', font: 'italic 13px system-ui, sans-serif', userSelect: 'text',
          }}
        >
          {submitted}
        </div>
      )}
      {reply && !textOpen && (
        <div
          data-hit=""
          onClick={dismissReply}
          title="Click to dismiss"
          style={{
            marginTop: pullUp, width: 280, flex: 'none', maxHeight: 180, overflowY: 'auto', whiteSpace: 'pre-wrap', padding: '10px 12px', borderRadius: 12,
            background: 'rgba(16,16,22,0.9)', border: `1px solid ${accent}55`, color: '#e8eef0', font: '13px system-ui, sans-serif', cursor: 'pointer', userSelect: 'text',
          }}
        >
          {reply}
        </div>
      )}
      {textOpen && (
        <div
          data-hit=""
          style={{
            marginTop: pullUp, width: 280, flex: 'none', display: 'flex', flexDirection: 'column', gap: 8, padding: 12, borderRadius: 14,
            background: 'rgba(16,16,22,0.94)', border: `1px solid ${accent}55`, boxShadow: `0 0 18px ${accent}33`,
            font: '13px system-ui, sans-serif', color: '#e8eef0', transition: 'border-color 0.5s, box-shadow 0.5s',
          }}
        >
          <textarea
            autoFocus
            value={draft}
            disabled={busy}
            placeholder="Type what you want Aqua to do..."
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); void sendText(); }
              else if (e.key === 'Escape') closeText();
            }}
            rows={4}
            style={{ resize: 'none', padding: 8, borderRadius: 8, border: `1px solid ${accent}66`, background: 'rgba(0,0,0,0.45)', color: 'inherit', font: 'inherit', outline: 'none', userSelect: 'text' }}
          />
          <div style={{ display: 'flex', gap: 8, justifyContent: 'flex-end' }}>
            <button onClick={closeText} style={{ padding: '6px 12px', borderRadius: 8, border: '1px solid rgba(255,255,255,0.2)', background: 'transparent', color: 'inherit', cursor: 'pointer' }}>
              Close
            </button>
            <button
              onClick={() => void sendText()}
              disabled={busy || !draft.trim()}
              style={{ padding: '6px 16px', borderRadius: 8, border: 'none', background: accent, color: '#101014', fontWeight: 600, cursor: busy || !draft.trim() ? 'default' : 'pointer', opacity: busy || !draft.trim() ? 0.5 : 1 }}
            >
              {busy ? 'Working...' : 'Send'}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
