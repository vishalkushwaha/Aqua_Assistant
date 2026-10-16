import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties, type ReactNode } from 'react';
import { ApiImage } from './ApiImage';
import { openEventStream } from '../api/stream';
import { Play, Pause, Square, RotateCcw, SkipForward, StepForward, Check, X as XIcon } from 'lucide-react';
import { flowsApi, type RecoveryRecord, type RecoveryStep, type Verdict, type FlowDef, type InboxItem, type NodeRun, type NodeStatus, type Run, type RunEvent, type RunStatus, type WaitRequest } from '../api/flows';

const card: CSSProperties = { background: 'rgba(255,255,255,0.04)', border: '1px solid var(--glass-border)', borderRadius: 12, padding: 14 };
const muted: CSSProperties = { fontSize: 12.5, color: 'var(--text-secondary)' };
const button: CSSProperties = { padding: '6px 11px', borderRadius: 8, border: '1px solid rgba(6,182,212,0.4)', background: 'rgba(6,182,212,0.12)', color: 'var(--accent-cyan)', fontSize: 12.5, cursor: 'pointer', display: 'inline-flex', alignItems: 'center', gap: 6 };
const dangerButton: CSSProperties = { ...button, border: '1px solid rgba(248,113,113,0.5)', background: 'rgba(248,113,113,0.12)', color: '#f87171' };
const input: CSSProperties = { width: '100%', background: 'rgba(0,0,0,0.35)', border: '1px solid var(--glass-border)', borderRadius: 8, padding: '6px 8px', color: 'inherit', fontSize: 13, boxSizing: 'border-box' };

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));
const clock = (t: number) => new Date(t * 1000).toLocaleTimeString();

export const STATUS_COLOUR: Record<string, string> = {
  pending: '#64748b', running: '#38bdf8', waiting: '#f59e0b', succeeded: '#10b981', failed: '#f87171', skipped: '#94a3b8',
  needs_action: '#f97316', interrupted: '#f59e0b', queued: '#64748b', paused: '#a78bfa', cancelled: '#94a3b8',
};

export const STATUS_LABEL: Record<string, string> = {
  pending: 'waiting its turn', running: 'running', waiting: 'waiting', succeeded: 'done', failed: 'failed', skipped: 'skipped',
  needs_action: 'needs you', interrupted: 'interrupted',
};

export function Pill({ status, text }: { status: string; text?: string }) {
  const c = STATUS_COLOUR[status] ?? '#64748b';
  return <span style={{ fontSize: 11.5, padding: '2px 8px', borderRadius: 999, border: `1px solid ${c}66`, color: c, background: `${c}1f`, whiteSpace: 'nowrap' }}>{text ?? status.replace('_', ' ')}</span>;
}

// ---- following a run --------------------------------------------------------------------------

/** A run's state and log, kept up to date by the server's event stream (with polling as a fallback). */
export function useRun(runId: number | null) {
  const [detail, setDetail] = useState<{ run: Run; nodes: NodeRun[] } | null>(null);
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [error, setError] = useState('');
  const last = useRef(0);
  const throttleTimer = useRef<number | null>(null);
  const pollTimer = useRef<number | null>(null);
  // Which run the page is following now. A request that was started for an earlier run must not write into the state
  // of the current one (it used to: switching from run A to run B while A's request was in flight mixed A's log into B
  // and moved the "last event seen" marker to A's numbers).
  const current = useRef<number | null>(null);

  const refresh = useCallback(async () => {
    if (runId === null) return;
    const mine = runId;
    try {
      const d = await flowsApi.runDetail(mine);
      if (current.current !== mine) return;
      setDetail(d);
      // A finished run changes no more: stop the fallback polling.
      if (['succeeded', 'failed', 'cancelled'].includes(d.run.status) && pollTimer.current !== null) {
        window.clearInterval(pollTimer.current);
        pollTimer.current = null;
      }
      const ev = await flowsApi.runEvents(mine, last.current);
      if (current.current !== mine) return;
      if (ev.events.length) {
        last.current = ev.events[ev.events.length - 1].id;
        setEvents((old) => [...old, ...ev.events]);
      }
      setError('');
    } catch (e) { if (current.current === mine) setError(message(e)); }
  }, [runId]);

  useEffect(() => {
    current.current = runId;
    setDetail(null);
    setEvents([]);
    last.current = 0;
    if (runId === null) return;
    void refresh();
    let closed = false;
    const poll = () => {
      if (pollTimer.current !== null) return; // one poller, however many times the stream reports an error
      pollTimer.current = window.setInterval(() => void refresh(), 2000);
    };
    const throttled = () => {
      if (throttleTimer.current !== null) return;
      throttleTimer.current = window.setTimeout(() => { throttleTimer.current = null; void refresh(); }, 150);
    };
    // A fetch-based stream, not EventSource: an EventSource cannot send the token the packaged app needs.
    const stream = openEventStream(flowsApi.streamUrl(runId, 0), {
      onEvent: (name) => {
        if (name === 'event' || name === 'status') throttled();
        if (name === 'end') void refresh();
      },
      onEnd: () => void refresh(),
      onError: () => { if (!closed) poll(); },
    });
    return () => {
      closed = true;
      current.current = null;
      stream.close();
      if (pollTimer.current !== null) { window.clearInterval(pollTimer.current); pollTimer.current = null; }
      if (throttleTimer.current !== null) { window.clearTimeout(throttleTimer.current); throttleTimer.current = null; }
    };
  }, [runId, refresh]);

  return { detail, events, error, refresh };
}

// ---- starting a run -----------------------------------------------------------------------------

export function RunDialog({ flowId, def, blocked, onStarted, onClose }: {
  flowId: number; def: FlowDef; /** Why it cannot be started at all (unsaved edits, problems in the flow). */ blocked: string; onStarted: (id: number) => void; onClose: () => void;
}) {
  const [inputs, setInputs] = useState<Record<string, string>>(() => Object.fromEntries(def.variables.map((v) => [v.name, v.default == null ? '' : String(v.default)])));
  const [dry, setDry] = useState(false);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const start = async () => {
    setBusy(true);
    setError('');
    try {
      const values: Record<string, unknown> = {};
      def.variables.forEach((v) => { if ((inputs[v.name] ?? '').trim() !== '') values[v.name] = v.kind === 'number' ? Number(inputs[v.name]) : v.kind === 'bool' ? inputs[v.name] === 'true' : inputs[v.name]; });
      const r = await flowsApi.startRun(flowId, values, dry);
      onStarted(r.run.id);
    } catch (e) { setError(message(e)); } finally { setBusy(false); }
  };
  return (
    <div style={{ ...card, display: 'grid', gap: 10 }} data-testid="run-dialog">
      <div style={{ display: 'flex', justifyContent: 'space-between' }}><strong>Run “{def.name}”</strong><button style={button} onClick={onClose}><XIcon size={14} /></button></div>
      {def.variables.map((v) => (
        <label key={v.name} style={{ display: 'block' }}>
          <div style={{ ...muted, marginBottom: 3 }}>{v.label || v.name}{v.required ? ' *' : ''}</div>
          {v.kind === 'choice' ? (
            <select style={input} value={inputs[v.name] ?? ''} onChange={(e) => setInputs({ ...inputs, [v.name]: e.target.value })} aria-label={v.label || v.name}>
              <option value="">(choose)</option>{v.choices.map((c) => <option key={c} value={c}>{c}</option>)}
            </select>
          ) : <input style={input} value={inputs[v.name] ?? ''} onChange={(e) => setInputs({ ...inputs, [v.name]: e.target.value })} aria-label={v.label || v.name} />}
        </label>
      ))}
      <label style={{ display: 'flex', gap: 8, alignItems: 'center', fontSize: 13 }}><input type="checkbox" checked={dry} onChange={(e) => setDry(e.target.checked)} aria-label="Dry run" /> Dry run: show what would happen, touch nothing outside Aqua</label>
      {blocked && <div style={{ ...muted, color: '#f59e0b' }}>{blocked}</div>}
      {error && <div role="alert" style={{ color: '#f87171', fontSize: 12.5 }}>{error}</div>}
      <div><button style={button} disabled={busy || !!blocked} onClick={() => void start()}><Play size={14} /> {busy ? 'Starting…' : dry ? 'Start dry run' : 'Start'}</button></div>
    </div>
  );
}

// ---- what is waiting for the person -----------------------------------------------------------------

export function WaitCard({ node, wait, name, onAnswer }: { node: string; wait: WaitRequest; name: string; onAnswer: (node: string, r: { decision?: 'approved' | 'rejected'; answer?: string; edits?: string; choice?: string }) => Promise<void> }) {
  const [text, setText] = useState(wait.kind === 'approval' ? wait.preview : '');
  const [answer, setAnswer] = useState('');
  const [error, setError] = useState('');
  const send = async (r: Parameters<typeof onAnswer>[1]) => {
    setError('');
    try { await onAnswer(node, r); } catch (e) { setError(message(e)); }
  };
  if (wait.kind === 'until') return <div style={muted}>“{name}” waits until {new Date(wait.ts * 1000).toLocaleString()}.</div>;
  return (
    <div style={{ ...card, borderColor: 'rgba(245,158,11,0.55)', display: 'grid', gap: 8 }} data-testid="wait-card">
      {wait.kind === 'approval' && (
        <>
          <strong>{wait.title}</strong>
          {wait.implicit && <div style={{ ...muted, color: '#f59e0b' }}>Nothing in this flow asks before this step, and it has an effect outside Aqua, so Aqua is asking.</div>}
          {wait.allow_edit ? <textarea style={{ ...input, minHeight: 110 }} value={text} onChange={(e) => setText(e.target.value)} aria-label="Preview (you can edit it)" /> : <pre style={{ margin: 0, whiteSpace: 'pre-wrap', fontSize: 12.5 }}>{wait.preview}</pre>}
          <div style={{ display: 'flex', gap: 8 }}>
            <button style={button} onClick={() => void send({ decision: 'approved', edits: wait.allow_edit && text !== wait.preview ? text : undefined })}><Check size={14} /> Approve</button>
            <button style={dangerButton} onClick={() => void send({ decision: 'rejected' })}><XIcon size={14} /> Reject</button>
          </div>
        </>
      )}
      {wait.kind === 'input' && (
        <>
          <strong>{wait.question}</strong>
          {wait.answer_kind === 'yes_no' ? (
            <div style={{ display: 'flex', gap: 8 }}><button style={button} onClick={() => void send({ answer: 'yes' })}>Yes</button><button style={button} onClick={() => void send({ answer: 'no' })}>No</button></div>
          ) : (
            <div style={{ display: 'flex', gap: 6 }}>
              {wait.answer_kind === 'choice' && wait.options.length ? (
                <select style={input} value={answer} onChange={(e) => setAnswer(e.target.value)} aria-label="Your answer"><option value="">(choose)</option>{wait.options.map((o) => <option key={o} value={o}>{o}</option>)}</select>
              ) : <input style={input} value={answer} onChange={(e) => setAnswer(e.target.value)} onKeyDown={(e) => { if (e.key === 'Enter' && answer.trim()) void send({ answer }); }} aria-label="Your answer" />}
              <button style={button} onClick={() => void send({ answer })} disabled={!answer.trim()}>Send</button>
            </div>
          )}
        </>
      )}
      {wait.kind === 'failure' && (
        <>
          <strong style={{ color: '#f97316' }}>{wait.message}</strong>
          {wait.choices.includes('done') && <input style={input} value={answer} onChange={(e) => setAnswer(e.target.value)} placeholder="Link to it, if there is one (optional)" aria-label="Link to the result" />}
          <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
            {wait.choices.map((c) => (
              <button key={c} style={c === 'stop' ? dangerButton : button} onClick={() => void send({ choice: c, answer: c === 'done' ? answer : undefined })}>
                {c === 'retry' ? 'Try again' : c === 'skip' ? 'Skip it' : c === 'stop' ? 'Stop the run' : 'It was done'}
              </button>
            ))}
          </div>
        </>
      )}
      {error && <div role="alert" style={{ color: '#f87171', fontSize: 12.5 }}>{error}</div>}
    </div>
  );
}

// ---- the run panel ---------------------------------------------------------------------------------------

const LEVEL_COLOUR: Record<string, string> = { info: 'var(--text-secondary)', warn: '#f59e0b', error: '#f87171', notify: '#38bdf8' };


const VERDICT_TEXT: Record<Verdict['kind'], string> = { success: 'Worked', recovered: 'Worked after recovering', failed: 'Did not work', unknown: 'Could not confirm', waiting: 'Not finished' };
const VERDICT_COLOUR: Record<Verdict['kind'], string> = { success: 'succeeded', recovered: 'succeeded', failed: 'failed', unknown: 'waiting', waiting: 'queued' };

/** What the supervisor concluded about a finished run, and what can be done about a failed step. */
export function RecoveryCard({ runId, finished, onChanged }: { runId: number; finished: boolean; onChanged: () => void }) {
  const [data, setData] = useState<{ verdict: Verdict; steps: RecoveryStep[] } | null>(null);
  const [applied, setApplied] = useState<RecoveryRecord | null>(null);
  const [err, setErr] = useState('');
  useEffect(() => { if (finished) flowsApi.runRecovery(runId).then(setData).catch((e) => setErr(message(e))); else setData(null); }, [runId, finished]);
  if (!data) return err ? <div role="alert" style={{ color: '#f87171', fontSize: 12.5 }}>{err}</div> : null;
  const { verdict, steps } = data;
  const apply = (node: string, key: string) => {
    setErr('');
    flowsApi.applyRecovery(runId, node, key).then((r) => { setApplied(r.recovery); onChanged(); }).catch((e) => setErr(message(e)));
  };
  const undo = () => { if (applied) flowsApi.rollbackRecovery(applied.id).then(() => { setApplied(null); onChanged(); }).catch((e) => setErr(message(e))); };
  return (
    <div style={{ ...card, display: 'grid', gap: 8 }} data-testid="recovery-card">
      <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}><strong>Outcome</strong><Pill status={VERDICT_COLOUR[verdict.kind]} text={VERDICT_TEXT[verdict.kind]} /></div>
      <div style={muted}>{verdict.reason}</div>
      {steps.map((s) => (
        <div key={s.node_id} style={{ display: 'grid', gap: 6 }}>
          <div style={{ fontSize: 13 }}><strong>{s.node_name}</strong>: {s.label} — {s.explain}</div>
          {s.guidance && <div style={muted}>{s.guidance}</div>}
          {s.suggestions.map((g) => (
            <div key={g.key} style={{ borderLeft: '2px solid rgba(6,182,212,0.4)', paddingLeft: 8, display: 'grid', gap: 3 }} data-testid={`fix-${g.key}`}>
              <div style={{ fontSize: 13 }}>{g.title}</div>
              <div style={muted}>{g.why}{g.changes_logic ? ' This changes what the flow does when a step fails.' : ''}</div>
              {g.changes.map((c) => <div key={c} style={muted}>Changes: {c}</div>)}
              <div style={muted}>{g.knowledge.successes + g.knowledge.failures === 0 ? 'Not tried before.' : `Helped ${g.knowledge.successes} of ${g.knowledge.successes + g.knowledge.failures} times (${g.knowledge.state}).`}{g.kind === 'temporary' ? ' Used automatically by scheduled runs once trusted.' : ''}</div>
              {g.applicable && !applied && <div><button style={button} onClick={() => apply(s.node_id, g.key)}>Approve and apply</button></div>}
            </div>
          ))}
        </div>
      ))}
      {applied && <div style={{ display: 'flex', gap: 8, alignItems: 'center' }} data-testid="fix-applied"><span style={muted}>Applied: {applied.title}. It is a new version of the flow; the next run will show whether it helped.</span><button style={button} onClick={undo}>Undo</button></div>}
      {err && <div role="alert" style={{ color: '#f87171', fontSize: 12.5 }}>{err}</div>}
    </div>
  );
}

export function RunPanel({ live, def, selected, onSelect, onOpenRun, onGone }: {
  live: ReturnType<typeof useRun>; def: FlowDef; selected: string | null; onSelect: (id: string | null) => void; onOpenRun: (id: number) => void; onGone: () => void;
}) {
  const { detail, events, error, refresh } = live;
  const [level, setLevel] = useState('all');
  const [search, setSearch] = useState('');
  const [actionError, setActionError] = useState('');
  const nameOf = useCallback((id: string) => def.nodes.find((n) => n.id === id)?.name ?? id, [def]);

  const guard = async (fn: () => Promise<unknown>) => {
    setActionError('');
    try { await fn(); await refresh(); } catch (e) { setActionError(message(e)); }
  };
  const waiting = useMemo(() => (detail?.nodes ?? []).filter((n) => n.wait && (n.status === 'waiting' || n.status === 'needs_action')), [detail]);
  const shown = useMemo(() => events.filter((e) => (level === 'all' || e.level === level) && (!search.trim() || e.message.toLowerCase().includes(search.trim().toLowerCase()))), [events, level, search]);
  const logRef = useRef<HTMLDivElement>(null);
  useEffect(() => { if (logRef.current) logRef.current.scrollTop = logRef.current.scrollHeight; }, [shown.length]);

  if (!detail) return <div style={muted}>{error || 'Loading the run…'}</div>;
  const { run, nodes } = detail;
  const st: RunStatus = run.status;
  const sel = selected ? nodes.find((n) => n.node_id === selected) : undefined;
  const canRetry = sel && ['failed', 'needs_action', 'interrupted'].includes(sel.status);
  const idle = st !== 'running' && st !== 'queued';

  return (
    <div style={{ display: 'grid', gap: 10 }} data-testid="run-panel">
      <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
        <strong>Run #{run.id}</strong><Pill status={st} />{run.dry_run && <Pill status="pending" text="dry run" />}
        <span style={{ flex: 1 }} />
        {(st === 'running' || st === 'queued') && <button style={button} onClick={() => void guard(() => flowsApi.control(run.id, 'pause'))}><Pause size={13} /> Pause</button>}
        {(st === 'paused' || st === 'interrupted') && <button style={button} onClick={() => void guard(() => flowsApi.control(run.id, 'resume'))}><Play size={13} /> Resume</button>}
        {!['succeeded', 'failed', 'cancelled'].includes(st) && <button style={dangerButton} onClick={() => void guard(() => flowsApi.control(run.id, 'stop'))}><Square size={13} /> Stop</button>}
        {['succeeded', 'failed', 'cancelled', 'interrupted', 'paused'].includes(st) && <button style={button} onClick={() => void guard(async () => { const r = await flowsApi.control(run.id, 'restart'); if (r.run) onOpenRun(r.run.id); })}><RotateCcw size={13} /> Restart</button>}
      </div>
      <div style={muted}>Version {run.flow_version}{run.from_run ? ` · reuses run #${run.from_run}` : ''} · started {run.started_at ? new Date(run.started_at * 1000).toLocaleString() : '—'}</div>
      {run.error && <div style={{ fontSize: 12.5, color: st === 'cancelled' ? 'var(--text-secondary)' : '#f87171' }}>{run.error}</div>}
      {(actionError || error) && <div role="alert" style={{ color: '#f87171', fontSize: 12.5 }}>{actionError || error}</div>}

      {['failed', 'succeeded', 'interrupted'].includes(st) && !run.dry_run && <RecoveryCard runId={run.id} finished onChanged={() => void refresh()} />}

      {waiting.map((n) => n.wait && <WaitCard key={n.node_id} node={n.node_id} name={nameOf(n.node_id)} wait={n.wait} onAnswer={async (node, r) => { await flowsApi.respond(run.id, node, r); await refresh(); }} />)}

      <div style={{ display: 'grid', gap: 4 }}>
        {def.nodes.map((n) => {
          const nr = nodes.find((x) => x.node_id === n.id);
          const status: NodeStatus = nr?.status ?? 'pending';
          return (
            <button key={n.id} data-step={n.id} onClick={() => onSelect(selected === n.id ? null : n.id)} style={{ display: 'flex', gap: 8, alignItems: 'center', padding: '5px 8px', borderRadius: 8, border: `1px solid ${selected === n.id ? 'rgba(6,182,212,0.6)' : 'transparent'}`, background: selected === n.id ? 'rgba(6,182,212,0.1)' : 'transparent', color: 'inherit', cursor: 'pointer', textAlign: 'left' }}>
              <span style={{ width: 9, height: 9, borderRadius: '50%', background: STATUS_COLOUR[status], flexShrink: 0 }} />
              <span style={{ flex: 1, fontSize: 13, opacity: status === 'skipped' ? 0.55 : 1 }}>{n.name}</span>
              <span style={{ ...muted, fontSize: 11.5 }}>{STATUS_LABEL[status]}{nr && nr.attempts > 1 ? ` · ${nr.attempts} tries` : ''}</span>
            </button>
          );
        })}
      </div>

      {sel && (
        <div style={{ ...card, display: 'grid', gap: 6 }} data-testid="step-detail">
          <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}><strong>{nameOf(sel.node_id)}</strong><Pill status={sel.status} text={STATUS_LABEL[sel.status]} /></div>
          {sel.note && <div style={muted}>{sel.note}</div>}
          {sel.error && <div style={{ fontSize: 12.5, color: '#f87171' }}>{sel.error}</div>}
          {sel.status === 'succeeded' && typeof sel.outputs.image === 'string' && /\.png$/i.test(sel.outputs.image) && (
            <ApiImage src={flowsApi.artifactUrl(run.id, sel.node_id)} alt={typeof sel.outputs.alt_text === 'string' ? sel.outputs.alt_text : 'Generated image'} data-testid="step-image" style={{ maxWidth: '100%', maxHeight: 260, objectFit: 'contain', borderRadius: 8, background: 'rgba(0,0,0,0.3)' }} />
          )}
          {Object.keys(sel.outputs).length > 0 && (
            <div style={{ display: 'grid', gap: 4 }}>
              {Object.entries(sel.outputs).map(([k, v]) => (
                <div key={k}><span style={muted}>{k}</span><pre style={{ margin: 0, whiteSpace: 'pre-wrap', fontSize: 12, maxHeight: 140, overflow: 'auto' }}>{typeof v === 'string' ? v : JSON.stringify(v, null, 2)}</pre></div>
              ))}
            </div>
          )}
          {idle && (
            <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
              {canRetry && <button style={button} onClick={() => void guard(() => flowsApi.nodeAction(run.id, sel.node_id, 'retry'))}><RotateCcw size={13} /> Retry this step</button>}
              {(canRetry || sel.status === 'waiting') && <button style={button} onClick={() => void guard(() => flowsApi.nodeAction(run.id, sel.node_id, 'skip'))}><SkipForward size={13} /> Skip it</button>}
              {['succeeded', 'failed', 'skipped', 'needs_action'].includes(sel.status) && ['succeeded', 'failed', 'cancelled'].includes(st) && (
                <button style={button} onClick={() => void guard(async () => { const r = await flowsApi.nodeAction(run.id, sel.node_id, 'run-from'); if (r.run) onOpenRun(r.run.id); })}><StepForward size={13} /> Run from here</button>
              )}
            </div>
          )}
        </div>
      )}

      <div style={{ display: 'flex', gap: 6 }}>
        <select style={{ ...input, width: 110 }} value={level} onChange={(e) => setLevel(e.target.value)} aria-label="Log level"><option value="all">all</option><option value="info">info</option><option value="warn">warnings</option><option value="error">errors</option><option value="notify">messages</option></select>
        <input style={input} value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Search the log" aria-label="Search the log" />
      </div>
      <div ref={logRef} style={{ maxHeight: 220, overflow: 'auto', display: 'grid', gap: 2, fontSize: 12, fontFamily: 'ui-monospace, Consolas, monospace' }} data-testid="run-log">
        {shown.map((e) => (
          <div key={e.id} style={{ color: LEVEL_COLOUR[e.level] }}><span style={{ opacity: 0.6 }}>{clock(e.at)}</span> {e.level === 'notify' ? '✉ ' : ''}{e.message}</div>
        ))}
        {shown.length === 0 && <div style={muted}>Nothing matches.</div>}
      </div>
      <button style={{ ...button, justifySelf: 'start' }} onClick={onGone}>Close the run</button>
    </div>
  );
}

// ---- earlier runs -------------------------------------------------------------------------------------------

export function RunHistory({ flowId, onOpen, refreshKey }: { flowId: number; onOpen: (id: number) => void; refreshKey: number }): ReactNode {
  const [runs, setRuns] = useState<Run[]>([]);
  useEffect(() => { flowsApi.runs(flowId, 15).then((r) => setRuns(r.runs)).catch(() => undefined); }, [flowId, refreshKey]);
  if (runs.length === 0) return <div style={muted}>No runs yet. Press Run.</div>;
  return (
    <div style={{ display: 'grid', gap: 6 }}>
      <div style={muted}>Earlier runs</div>
      {runs.map((r) => (
        <button key={r.id} onClick={() => onOpen(r.id)} style={{ ...card, padding: 8, textAlign: 'left', cursor: 'pointer', color: 'inherit' }}>
          <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}><strong>#{r.id}</strong><Pill status={r.status} />{r.dry_run && <Pill status="pending" text="dry run" />}</div>
          <div style={muted}>{new Date(r.created_at * 1000).toLocaleString()} · v{r.flow_version}</div>
        </button>
      ))}
    </div>
  );
}

// ---- the inbox --------------------------------------------------------------------------------------------------

export function Inbox({ onOpen }: { onOpen: (flowId: number, runId: number) => void }) {
  const [items, setItems] = useState<InboxItem[]>([]);
  useEffect(() => {
    const load = () => flowsApi.inbox().then((r) => setItems(r.items)).catch(() => undefined);
    void load();
    const t = window.setInterval(() => void load(), 5000);
    return () => window.clearInterval(t);
  }, []);
  if (items.length === 0) return null;
  return (
    <div style={{ ...card, borderColor: 'rgba(245,158,11,0.55)', display: 'grid', gap: 8 }} data-testid="inbox">
      <strong>Waiting for you ({items.length})</strong>
      {items.map((i) => (
        <div key={`${i.run_id}:${i.node_id}`} style={{ display: 'flex', gap: 10, alignItems: 'center' }}>
          <div style={{ flex: 1, minWidth: 0 }}>
            <div style={{ fontSize: 13 }}>{i.flow_name}: {i.node_name}</div>
            <div style={muted}>{i.wait.kind === 'approval' ? i.wait.title : i.wait.kind === 'input' ? i.wait.question : i.wait.kind === 'failure' ? i.wait.message : ''}</div>
          </div>
          <button style={button} onClick={() => onOpen(i.flow_id, i.run_id)}>Open</button>
        </div>
      ))}
    </div>
  );
}
