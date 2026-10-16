import { useCallback, useEffect, useRef, useState, type CSSProperties } from 'react';
import { RefreshCw } from 'lucide-react';
import { PartGuard } from './PartGuard';
import { Section } from './ui';
import { errorSentence } from '../api/plain';
import {
  autonomyApi, CAP_IDS, CAP_INFO, capValueText, draftFrom, draftProblems, isDirty, levelMismatch, numberText, planSave, runLine, savedText, splitSentence, stopLine, withDefaultCaps,
  type AutonomyView, type CapId, type Draft,
} from '../api/autonomy';
import { useVisiblePolling } from '../hooks/useVisiblePolling';
import { alertText, input, muted, noticeText, small } from './screenStyles';

/** How often the runs that are going are read again while this page is open (the least the app allows is once a second). */
const POLL_MS = 5000;

const legend: CSSProperties = { fontSize: 14, fontWeight: 600, padding: 0, marginBottom: 6 };
const fieldset: CSSProperties = { border: 'none', margin: 0, padding: 0, minWidth: 0, display: 'flex', flexDirection: 'column', gap: 8 };

/**
 * How much Aqua asks before it acts on its own, and how much one run may do before it is stopped. Both can only make Aqua ask more, or stop a
 * run sooner, than it does today. The choices are saved with the button, and the core's answer (or its error) is shown as it comes. The runs
 * that are going are read every few seconds. Everything that comes from the core is a text child, never markup.
 */
function AutonomyCardBody() {
  const [view, setView] = useState<AutonomyView | null>(null);
  // What the person has changed and not saved yet; null while nothing is changed, so that a fresh reading of the setting is shown as it is.
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<{ tone: 'ok' | 'error'; text: string } | null>(null);
  const [busy, setBusy] = useState(false);
  // Only the newest answer is used: a slow reading must not overwrite the setting that was just saved.
  const latest = useRef(0);

  const load = useCallback(async () => {
    const mine = ++latest.current;
    try {
      const next = await autonomyApi.get();
      if (mine !== latest.current) return;
      setView(next);
      setError(null);
    } catch (e) {
      if (mine !== latest.current) return;
      setError(errorSentence(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);
  useVisiblePolling(() => void load(), POLL_MS);

  const shown: Draft | null = view ? draft ?? draftFrom(view) : null;
  const problems = view && shown ? draftProblems(view, shown) : {};
  const dirty = view && shown ? isDirty(view, shown) : false;

  const edit = (change: (d: Draft) => Draft) => {
    if (!view) return;
    setMessage(null);
    setDraft((previous) => change(previous ?? draftFrom(view)));
  };

  const save = async () => {
    if (!view || !shown) return;
    const plan = planSave(view, shown);
    if (!plan.ok) {
      setMessage({ tone: 'error', text: plan.problems[0] });
      return;
    }
    setBusy(true);
    setMessage(null);
    latest.current += 1;
    try {
      const next = await autonomyApi.save(plan.body);
      latest.current += 1;
      setDraft(null);
      setMessage({ tone: 'ok', text: savedText(plan.levelChanged) });
      if (next) {
        setView(next);
        setError(null);
      } else {
        await load();
      }
    } catch (e) {
      setMessage({ tone: 'error', text: errorSentence(e) });
    } finally {
      setBusy(false);
    }
  };

  const mismatch = view ? levelMismatch(view) : '';
  const now = Date.now();
  return (
    <Section
      title="How much Aqua asks, and how much one run may do"
      hint="Choose how often Aqua asks before it acts on its own, and how much one run may do before it is stopped. Nothing here lets Aqua do more than today's rules allow: a level can only make Aqua ask more, and a limit can only stop a run sooner. They apply to Aqua's own agent, scripts, voice requests, replayed recipes and outside tools. What you do yourself in the app is not asked about again."
      actions={
        <button type="button" className="btn-ghost" style={{ ...small, display: 'inline-flex', alignItems: 'center', gap: 6 }} disabled={busy} onClick={() => void load()}>
          <RefreshCw size={14} /> Refresh
        </button>
      }
    >
      {error && <div role="alert" style={alertText}>{error}</div>}
      {view === null && !error && <div style={muted}>Loading…</div>}
      {view !== null && shown !== null && (
        <>
          {mismatch !== '' && <div role="alert" style={{ ...alertText, color: 'var(--state-attention)' }}>{mismatch}</div>}
          {!view.saved && <div style={muted}>Nothing has been saved yet, so the defaults are in use.</div>}

          <fieldset style={fieldset} disabled={busy}>
            <legend style={legend}>How much Aqua asks</legend>
            {view.levels.map((l) => {
              const words = splitSentence(l.what);
              const checked = shown.level === l.id;
              return (
                <div key={l.id} style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
                  <label style={{ display: 'flex', gap: 10, alignItems: 'flex-start', fontSize: 14, cursor: busy ? 'default' : 'pointer' }}>
                    <input
                      type="radio"
                      name="autonomy-level"
                      value={l.id}
                      checked={checked}
                      onChange={() => edit((d) => ({ ...d, level: l.id }))}
                      style={{ accentColor: 'var(--accent-cyan)', marginTop: 3 }}
                    />
                    <span>
                      <strong>{l.name}</strong>
                      {l.id === view.level && <span style={{ ...muted, marginLeft: 8 }}>(saved)</span>}
                      {words.first !== '' && <span style={{ display: 'block', ...muted, maxWidth: '70ch' }}>{words.first}</span>}
                      {words.rest !== '' && <span style={{ display: 'block', ...muted, maxWidth: '70ch' }}>{words.rest}</span>}
                    </span>
                  </label>
                </div>
              );
            })}
          </fieldset>

          <fieldset style={fieldset} disabled={busy}>
            <legend style={legend}>What one run may do before it is stopped</legend>
            <div style={muted}>A run that reaches a limit is not given the call that would go past it, and is told what was done. The defaults are above what a run does today.</div>
            {CAP_IDS.map((cap: CapId) => {
              const problem = problems[cap];
              const id = `autonomy-cap-${cap}`;
              return (
                <div key={cap} style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
                  <div style={{ display: 'flex', gap: 10, alignItems: 'center', flexWrap: 'wrap' }}>
                    <label htmlFor={id} style={{ fontSize: 14, minWidth: 120 }}>{CAP_INFO[cap].label}</label>
                    <input
                      id={id}
                      type="text"
                      inputMode="numeric"
                      autoComplete="off"
                      value={shown.caps[cap]}
                      aria-invalid={problem ? true : undefined}
                      onChange={(e) => edit((d) => ({ ...d, caps: { ...d.caps, [cap]: e.target.value } }))}
                      style={{ ...input, flex: '0 1 200px', fontFamily: 'var(--font-mono)' }}
                    />
                    <span style={muted}>{CAP_INFO[cap].unit}</span>
                    <span style={muted}>Default: {capValueText(cap, view.defaults[cap])}</span>
                  </div>
                  <div style={{ ...muted, marginLeft: 130 }}>{CAP_INFO[cap].counts} At most {numberText(view.most[cap])}.</div>
                  {problem && <div role="alert" style={{ ...alertText, marginLeft: 130 }}>{problem}</div>}
                </div>
              );
            })}
            <div>
              <button type="button" className="btn-ghost" style={small} disabled={busy} onClick={() => edit((d) => withDefaultCaps(d, view))}>
                Put the limits back to the defaults
              </button>
            </div>
          </fieldset>

          <div style={{ display: 'flex', gap: 10, alignItems: 'center', flexWrap: 'wrap' }}>
            <button type="button" className="btn-primary" disabled={busy || !dirty || Object.keys(problems).length > 0} onClick={() => void save()}>
              {busy ? 'Saving…' : 'Save'}
            </button>
            {dirty && (
              <button type="button" className="btn-ghost" style={small} disabled={busy} onClick={() => { setMessage(null); setDraft(null); }}>
                Undo my changes
              </button>
            )}
            {dirty && !busy && <span style={muted}>Not saved yet.</span>}
          </div>
          <div aria-live="polite">
            {message && <div role={message.tone === 'error' ? 'alert' : 'status'} style={message.tone === 'ok' ? noticeText : alertText}>{message.text}</div>}
          </div>

          <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
            <h4 style={{ fontSize: 14, fontWeight: 600, margin: 0 }}>Runs going now</h4>
            {view.running.length === 0 && <div style={muted}>No run is going now.</div>}
            {view.running.length > 0 && (
              <ul style={{ listStyle: 'none', margin: 0, padding: 0, display: 'flex', flexDirection: 'column', gap: 6 }}>
                {view.running.map((r, i) => (
                  <li key={`${r.runId}-${i}`} style={{ fontSize: 13, overflowWrap: 'anywhere' }}>
                    <code>{r.runId === '' ? '(no id)' : r.runId}</code>: {runLine(r)}
                  </li>
                ))}
              </ul>
            )}
            {view.recentStops.length > 0 && (
              <details>
                <summary style={{ cursor: 'pointer', fontSize: 13, color: 'var(--text-secondary)' }}>Runs a limit stopped lately ({view.recentStops.length})</summary>
                <ul style={{ margin: '6px 0 0', paddingLeft: 18, display: 'flex', flexDirection: 'column', gap: 4 }}>
                  {view.recentStops.map((s, i) => (
                    <li key={`${s.runId}-${i}`} style={{ fontSize: 12.5, overflowWrap: 'anywhere' }}>
                      <code>{s.runId === '' ? '(no id)' : s.runId}</code>: {stopLine(s, now)}
                    </li>
                  ))}
                </ul>
              </details>
            )}
          </div>
          {view.notBuilt.length > 0 && <div style={muted}>Not built: {view.notBuilt.join('; ')}.</div>}
        </>
      )}
    </Section>
  );
}

/** The card with its own guard, so that a value it cannot draw does not hide the rest of the Permissions page. */
export function AutonomyCard() {
  return (
    <PartGuard name="How much Aqua asks, and how much one run may do">
      <AutonomyCardBody />
    </PartGuard>
  );
}
