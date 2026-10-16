import { useCallback, useEffect, useRef, useState, type CSSProperties } from 'react';
import { createPortal } from 'react-dom';
import { refusalToShow } from '../api/updateRefusal';

/**
 * Asks the packaged app whether a newer version exists and, if so, shows a prompt. Nothing is downloaded or installed until
 * "Install and restart" is clicked. "Later" hides the prompt until the next check; "Skip this version" hides that version
 * for good (a still newer version is offered again). When no update address is set up this renders nothing at all.
 * The release notes are shown as plain text, never as markup.
 *
 * When the update information was refused (it has expired, it is older than one already accepted, ...) the shell says why in
 * a plain sentence. That sentence is shown as text in a calm note, and kept until a later check that gets an answer clears it:
 * under the prompt when one is open, otherwise in a small note in the corner. The note has no button; the person decides
 * everything, as before.
 */

type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;

interface UpdateInfo { available: boolean; current: string; version: string | null; notes: string | null; date: string | null; refusal?: string | null }
interface UpdateStatus { configured: boolean; host: string | null; current: string; problem: string | null; refusal?: string | null }
interface UpdateProgress { downloaded: number; total: number; installing: boolean }

const SKIP_KEY = 'aqua.update.skipped';
const FIRST_CHECK_MS = 20_000;
const CHECK_EVERY_MS = 6 * 60 * 60 * 1000;

function tauriInvoke(): Invoke | null {
  const internals = (window as unknown as { __TAURI_INTERNALS__?: { invoke?: Invoke } }).__TAURI_INTERNALS__;
  return internals?.invoke ?? null;
}

function skipped(): string | null {
  try { return localStorage.getItem(SKIP_KEY); } catch { return null; }
}

function megabytes(n: number): string {
  return (n / (1024 * 1024)).toFixed(1);
}

const refusalBox: CSSProperties = { padding: '9px 12px', borderRadius: 8, background: '#1a160a', border: '1px solid rgba(251, 191, 36, 0.35)', fontSize: 13, lineHeight: 1.45, color: '#cbd5e1' };

/** Why the update information was not used. `text` is shown as a text child, so markup in it stays visible text and runs nothing. */
function RefusalNote({ text, style }: { text: string; style?: CSSProperties }) {
  return (
    <div role="status" style={{ ...refusalBox, ...style }}>
      <strong style={{ display: 'block', marginBottom: 2, color: '#fbbf24', fontWeight: 600 }}>The latest update information was not used.</strong>
      <span style={{ overflowWrap: 'anywhere' }}>{text}</span>
    </div>
  );
}

export function UpdatePrompt() {
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState<UpdateProgress | null>(null);
  const [error, setError] = useState('');
  const [refusal, setRefusal] = useState<string | null>(null);
  const installing = useRef(false);
  const dialogRef = useRef<HTMLDivElement>(null);
  const laterRef = useRef<HTMLButtonElement>(null);

  const check = useCallback(async () => {
    const invoke = tauriInvoke();
    if (!invoke || installing.current) return;
    try {
      const status = (await invoke('update_status')) as UpdateStatus;
      if (!status.configured) {
        setRefusal(null);
        return;
      }
      let found: UpdateInfo | undefined;
      try {
        found = (await invoke('check_for_update')) as UpdateInfo;
      } catch {
        // The check itself failed (no network, say). It is not worth interrupting anyone for, and a refusal that is shown stays.
      }
      setRefusal((shown) => refusalToShow(shown, status.refusal, found));
      if (found && found.available && found.version && found.version !== skipped()) setInfo(found);
    } catch {
      // A failed check is not worth interrupting anyone for; the next check tries again.
    }
  }, []);

  useEffect(() => {
    const first = window.setTimeout(check, FIRST_CHECK_MS);
    const every = window.setInterval(check, CHECK_EVERY_MS);
    return () => { window.clearTimeout(first); window.clearInterval(every); };
  }, [check]);

  useEffect(() => {
    if (!busy) return;
    const invoke = tauriInvoke();
    if (!invoke) return;
    const id = window.setInterval(async () => {
      try { setProgress((await invoke('update_progress')) as UpdateProgress); } catch { /* the app may already be closing */ }
    }, 500);
    return () => window.clearInterval(id);
  }, [busy]);

  const install = async () => {
    const invoke = tauriInvoke();
    if (!invoke) return;
    setError('');
    setBusy(true);
    installing.current = true;
    try {
      await invoke('install_update');
    } catch (e) {
      setError(String(e));
      setBusy(false);
      installing.current = false;
    }
  };

  const skip = () => {
    if (info?.version) {
      try { localStorage.setItem(SKIP_KEY, info.version); } catch { /* the prompt just comes back */ }
    }
    setInfo(null);
  };

  // A dialog that asks for a decision: keyboard focus moves into it (on the safe choice, "Later"), Tab stays inside it, Escape
  // means "Later", and the page behind it cannot be reached while it is open.
  const open = info !== null;
  useEffect(() => {
    if (!open) return;
    const previous = document.activeElement as HTMLElement | null;
    laterRef.current?.focus();
    const root = document.getElementById('root');
    root?.setAttribute('inert', '');
    return () => {
      root?.removeAttribute('inert');
      previous?.focus?.();
    };
  }, [open]);

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Escape' && !busy) {
      e.stopPropagation();
      setInfo(null);
      return;
    }
    if (e.key !== 'Tab') return;
    const nodes = dialogRef.current?.querySelectorAll<HTMLElement>('button:not([disabled])');
    if (!nodes || nodes.length === 0) { e.preventDefault(); return; }
    const first = nodes[0];
    const last = nodes[nodes.length - 1];
    if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
    else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
  };

  if (!info) {
    // No prompt is open: a refused update gets a small note in the corner. It never takes the clicks of the page behind it.
    return refusal ? (
      <div style={{ position: 'fixed', right: 16, bottom: 16, zIndex: 9000, width: 'min(380px, calc(100vw - 32px))', pointerEvents: 'none', fontFamily: 'system-ui' }}>
        <RefusalNote text={refusal} />
      </div>
    ) : null;
  }

  const pct = progress && progress.total > 0 ? Math.min(100, Math.round((progress.downloaded / progress.total) * 100)) : null;
  // Rendered on document.body, outside #root, so that #root can be made inert without disabling the dialog itself.
  return createPortal(
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="aqua-update-title"
      aria-describedby="aqua-update-body"
      onKeyDown={onKeyDown}
      style={{ position: 'fixed', inset: 0, zIndex: 10000, display: 'flex', alignItems: 'center', justifyContent: 'center', background: 'rgba(0,0,0,0.55)', fontFamily: 'system-ui' }}
    >
      <div ref={dialogRef} style={{ width: 'min(460px, 92vw)', maxHeight: '80vh', overflow: 'auto', padding: 22, borderRadius: 14, background: '#111827', color: '#e5e7eb', border: '1px solid #374151' }}>
        <h2 id="aqua-update-title" style={{ margin: '0 0 6px', fontSize: 18 }}>A new version of Aqua is ready</h2>
        <p id="aqua-update-body" style={{ margin: '0 0 12px', color: '#9ca3af', fontSize: 14 }}>
          Version {info.version} (you have {info.current}). Installing closes Aqua for a moment and opens it again. Nothing is installed until you choose to.
        </p>
        {info.notes && (
          <pre style={{ margin: '0 0 14px', padding: 10, maxHeight: 180, overflow: 'auto', whiteSpace: 'pre-wrap', borderRadius: 8, background: '#0b1220', color: '#cbd5e1', fontSize: 13, fontFamily: 'inherit' }}>
            {info.notes}
          </pre>
        )}
        {busy && (
          <p role="status" style={{ margin: '0 0 12px', fontSize: 13, color: '#9ca3af' }}>
            {progress?.installing && progress.total > 0 && progress.downloaded >= progress.total
              ? 'Installing…'
              : pct !== null
                ? `Downloading… ${pct}% (${megabytes(progress!.downloaded)} of ${megabytes(progress!.total)} MB)`
                : 'Downloading…'}
          </p>
        )}
        {error && <p role="alert" style={{ margin: '0 0 12px', fontSize: 13, color: '#f87171' }}>{error}</p>}
        {refusal && <RefusalNote text={refusal} style={{ margin: '0 0 14px' }} />}
        <div style={{ display: 'flex', gap: 8, justifyContent: 'flex-end', flexWrap: 'wrap' }}>
          <button onClick={skip} disabled={busy} style={{ padding: '8px 14px', borderRadius: 8, cursor: busy ? 'default' : 'pointer' }}>Skip this version</button>
          <button ref={laterRef} onClick={() => setInfo(null)} disabled={busy} style={{ padding: '8px 14px', borderRadius: 8, cursor: busy ? 'default' : 'pointer' }}>Later</button>
          <button onClick={install} disabled={busy} style={{ padding: '8px 16px', borderRadius: 8, cursor: busy ? 'default' : 'pointer', fontWeight: 600 }}>
            {busy ? 'Working…' : 'Install and restart'}
          </button>
        </div>
      </div>
    </div>,
    document.body,
  );
}
