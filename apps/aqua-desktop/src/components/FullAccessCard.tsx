import { useCallback, useEffect, useState } from 'react';
import { ShieldCheck } from 'lucide-react';
import { Pill, type Tone } from './ui';
import { permissionsApi, PermissionError, type FullAccessStatus, type LayerState } from '../api/permissions';

const TONE: Record<LayerState, Tone> = { on: 'ok', off: 'neutral', unavailable: 'attention' };

/** One list of what full access does, for one of the three groups. */
function Group({ title, items }: { title: string; items: string[] }) {
  return (
    <div style={{ flex: '1 1 240px', minWidth: 0 }}>
      <h4 style={{ fontSize: 13.5, fontWeight: 600, margin: '0 0 6px' }}>{title}</h4>
      <ul style={{ margin: 0, paddingLeft: 18, display: 'flex', flexDirection: 'column', gap: 4, fontSize: 13, color: 'var(--text-secondary)', lineHeight: 1.45 }}>
        {items.map((t) => (
          <li key={t}>{t}</li>
        ))}
      </ul>
    </div>
  );
}

function since(ms: number | null): string {
  if (!ms) return '';
  try {
    return ` since ${new Date(ms).toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' })}`;
  } catch {
    return '';
  }
}

/**
 * The permission that lets Aqua work in this project without asking at every step. Off by default. The switch changes it at
 * once and it stays on across restarts, for this project folder only; the lists say what it covers, what still asks, and what
 * is never allowed.
 */
export function FullAccessCard() {
  const [status, setStatus] = useState<FullAccessStatus | null>(null);
  const [unreachable, setUnreachable] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<{ tone: 'ok' | 'error'; text: string } | null>(null);

  const refresh = useCallback(async () => {
    try {
      setStatus(await permissionsApi.fullAccess());
      setUnreachable(false);
    } catch {
      setUnreachable(true);
    }
  }, []);

  useEffect(() => {
    void refresh();
    // Cheap (one small JSON answer); keeps the card honest if the setting changes elsewhere.
    const id = window.setInterval(() => void refresh(), 5000);
    return () => window.clearInterval(id);
  }, [refresh]);

  const change = async (enabled: boolean) => {
    setBusy(true);
    setMessage(null);
    try {
      const next = await permissionsApi.setFullAccess(enabled);
      setStatus(next);
      setUnreachable(false);
      setConfirming(false);
      setMessage({ tone: 'ok', text: enabled ? 'Full access is on. Aqua will not ask about ordinary work in this project.' : 'Full access is off. Aqua asks as the policy mode says, and earlier "approve for this session" answers are forgotten.' });
    } catch (e) {
      setMessage({ tone: 'error', text: e instanceof PermissionError ? e.message : 'Could not reach aqua-core. Nothing was changed.' });
    } finally {
      setBusy(false);
    }
  };

  const enabled = status?.enabled ?? false;
  const onToggle = () => {
    if (busy || !status) return;
    if (enabled) void change(false);
    else setConfirming((c) => !c);
  };

  return (
    <section className="glass-panel" aria-labelledby="full-access-title" style={{ padding: 22, borderRadius: 12, display: 'flex', flexDirection: 'column', gap: 14 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', gap: 16, flexWrap: 'wrap' }}>
        <div style={{ display: 'flex', alignItems: 'flex-start', gap: 12, minWidth: 0 }}>
          <ShieldCheck size={22} color={enabled ? 'var(--state-ok)' : 'var(--text-muted)'} style={{ marginTop: 2, flexShrink: 0 }} />
          <div style={{ minWidth: 0 }}>
            <h3 id="full-access-title" style={{ fontSize: 18, fontWeight: 700, margin: 0 }}>Full access for this project</h3>
            <p style={{ fontSize: 13.5, color: 'var(--text-secondary)', margin: '4px 0 0', lineHeight: 1.5, maxWidth: '70ch' }}>
              {enabled
                ? `On${since(status?.since_unix_ms ?? null)}. Aqua works in ${status?.project_root ?? 'the project folder'} and uses the browser and desktop tools without asking about ordinary work.`
                : 'Off. Aqua asks before changing files, running programs or opening new sites. Turn it on to stop the repeated prompts for ordinary work in this project.'}
            </p>
          </div>
        </div>
        <button
          id="full-access-toggle"
          type="button"
          role="switch"
          aria-checked={enabled}
          aria-label="Enable full access"
          disabled={busy || !status}
          onClick={onToggle}
          style={{
            display: 'inline-flex', alignItems: 'center', gap: 10, padding: '8px 14px 8px 10px', borderRadius: 999, cursor: busy || !status ? 'default' : 'pointer',
            border: `1px solid ${enabled ? 'var(--state-ok)' : 'var(--border-subtle, rgba(255,255,255,0.14))'}`,
            background: enabled ? 'rgba(16,185,129,0.14)' : 'rgba(255,255,255,0.04)', color: 'var(--text-primary)', fontSize: 14, fontWeight: 600, opacity: busy ? 0.7 : 1,
          }}
        >
          <span aria-hidden style={{ width: 34, height: 20, borderRadius: 999, background: enabled ? 'var(--state-ok)' : 'rgba(255,255,255,0.18)', position: 'relative', transition: 'background 0.2s', flexShrink: 0 }}>
            <span style={{ position: 'absolute', top: 2, left: enabled ? 16 : 2, width: 16, height: 16, borderRadius: '50%', background: '#fff', transition: 'left 0.2s' }} />
          </span>
          {busy ? 'Working…' : enabled ? 'Enabled' : 'Enable'}
        </button>
      </div>

      {unreachable && !status && (
        <p role="status" style={{ margin: 0, fontSize: 13, color: '#fbbf24' }}>Waiting for aqua-core… The setting cannot be read or changed until it answers.</p>
      )}
      {unreachable && status && (
        <p role="status" style={{ margin: 0, fontSize: 13, color: '#fbbf24' }}>aqua-core is not answering. The state below is the last one seen.</p>
      )}

      {status && (
        <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }} aria-label="Where full access applies">
          {status.layers.map((l) => (
            <Pill key={l.id} tone={TONE[l.state]} label={l.label} value={l.state === 'on' ? 'on' : l.state === 'off' ? 'off' : 'unavailable'} title={l.note} />
          ))}
        </div>
      )}

      {status && status.problems.length > 0 && (
        <ul role="alert" style={{ margin: 0, padding: '10px 12px 10px 28px', borderRadius: 8, background: 'rgba(245,158,11,0.1)', fontSize: 13, color: '#fbbf24', display: 'flex', flexDirection: 'column', gap: 4 }}>
          {status.problems.map((p) => (
            <li key={p}>{p}</li>
          ))}
        </ul>
      )}
      {status?.enabled && status.layers.filter((l) => l.state === 'unavailable').map((l) => (
        <p key={l.id} role="status" style={{ margin: 0, fontSize: 13, color: '#fbbf24' }}>{l.label}: {l.note}</p>
      ))}

      {confirming && status && !enabled && (
        <div role="group" aria-label="Confirm full access" style={{ padding: 16, borderRadius: 10, border: '1px solid rgba(245,158,11,0.4)', background: 'rgba(245,158,11,0.06)', display: 'flex', flexDirection: 'column', gap: 14 }}>
          <p style={{ margin: 0, fontSize: 14, lineHeight: 1.5 }}>
            Let Aqua work in <strong>{status.project_root ?? 'this project'}</strong> without asking about ordinary work? You can turn it off here at any time, and it only covers this folder.
          </p>
          <div style={{ display: 'flex', gap: 24, flexWrap: 'wrap' }}>
            <Group title="Runs without asking" items={status.runs_without_asking} />
            <Group title="Still asks" items={status.still_asks} />
            <Group title="Never allowed" items={status.never_allowed} />
          </div>
          <div style={{ display: 'flex', gap: 10 }}>
            <button id="full-access-confirm" type="button" disabled={busy} onClick={() => void change(true)}
              style={{ padding: '8px 16px', borderRadius: 8, border: 'none', cursor: 'pointer', fontWeight: 600, background: 'var(--accent-cyan)', color: '#04121a' }}>
              Turn on full access
            </button>
            <button id="full-access-cancel" type="button" disabled={busy} onClick={() => setConfirming(false)}
              style={{ padding: '8px 16px', borderRadius: 8, border: '1px solid var(--border-subtle, rgba(255,255,255,0.14))', cursor: 'pointer', background: 'transparent', color: 'var(--text-primary)' }}>
              Cancel
            </button>
          </div>
        </div>
      )}

      {status && enabled && (
        <details>
          <summary style={{ cursor: 'pointer', fontSize: 13.5, color: 'var(--text-secondary)' }}>What full access covers</summary>
          <div style={{ display: 'flex', gap: 24, flexWrap: 'wrap', marginTop: 12 }}>
            <Group title="Runs without asking" items={status.runs_without_asking} />
            <Group title="Still asks" items={status.still_asks} />
            <Group title="Never allowed" items={status.never_allowed} />
          </div>
        </details>
      )}

      <div aria-live="polite" style={{ minHeight: 0 }}>
        {message && <p style={{ margin: 0, fontSize: 13, color: message.tone === 'ok' ? 'var(--state-ok)' : '#fca5a5' }}>{message.text}</p>}
      </div>
    </section>
  );
}
