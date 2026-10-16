import React, { useCallback, useEffect, useState } from 'react';
import ReactDOM from 'react-dom/client';
import App from './App';
import './index.css';
import { installGatewayBridge } from './api/gateway';
import { VoiceOrb } from './components/VoiceOrb';
import { UpdatePrompt } from './components/UpdatePrompt';
import { ErrorBoundary } from './components/ErrorBoundary';

const isOverlay = window.location.search.includes('overlay=true');
// The orb window is transparent: the page behind the orb must paint nothing at all. (The admin theme gives `body` a tinted
// background and two soft radial glows; clearing only the background colour left the glows as a faint box.)
if (isOverlay) document.documentElement.classList.add('overlay');

/**
 * Packaged app: the gateway address and token are fetched first, so no early request goes out without them. The core can
 * take a while to start (its speech engine is checked first), so the window says it is starting and for how long, and a
 * failure is a message with a Retry button, not a dead page.
 */
function Boot() {
  const [phase, setPhase] = useState<'starting' | 'ready' | 'failed'>('starting');
  const [error, setError] = useState('');
  const [seconds, setSeconds] = useState(0);
  const [attempt, setAttempt] = useState(0);

  const start = useCallback(() => {
    setPhase('starting');
    setSeconds(0);
    setAttempt((a) => a + 1);
  }, []);

  useEffect(() => {
    let cancelled = false;
    installGatewayBridge().then(
      () => { if (!cancelled) setPhase('ready'); },
      (e: unknown) => { if (!cancelled) { setError(String(e)); setPhase('failed'); } },
    );
    return () => { cancelled = true; };
  }, [attempt]);

  useEffect(() => {
    if (phase !== 'starting') return;
    const id = window.setInterval(() => setSeconds((s) => s + 1), 1000);
    return () => window.clearInterval(id);
  }, [phase, attempt]);

  if (phase === 'ready') return isOverlay ? <VoiceOrb /> : <><App /><UpdatePrompt /></>;
  if (isOverlay) return null; // the orb window stays empty until there is something to show
  if (phase === 'failed') {
    return (
      <div role="alert" style={{ padding: 24, fontFamily: 'system-ui', color: '#f87171', maxWidth: 560 }}>
        <p style={{ marginTop: 0 }}>{error}</p>
        <p style={{ color: '#9ca3af', fontSize: 13 }}>
          Aqua's core writes what it is doing to <code>%LOCALAPPDATA%\Aqua\logs\core.log</code>. If another copy of Aqua is
          already running, close it first.
        </p>
        <button onClick={start} style={{ padding: '8px 18px', borderRadius: 8, cursor: 'pointer' }}>Try again</button>
      </div>
    );
  }
  return (
    <div role="status" style={{ padding: 24, fontFamily: 'system-ui', color: '#9ca3af' }}>
      Starting Aqua… {seconds}s
      {seconds > 20 && <div style={{ fontSize: 13, marginTop: 8 }}>The first start loads the speech and language models and can take a minute.</div>}
    </div>
  );
}

ReactDOM.createRoot(document.getElementById('root') as HTMLElement).render(
  <React.StrictMode>
    <ErrorBoundary>
      <Boot />
    </ErrorBoundary>
  </React.StrictMode>
);
