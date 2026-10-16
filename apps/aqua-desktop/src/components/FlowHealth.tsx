import { useCallback, useEffect, useState, type CSSProperties } from 'react';
import { HeartPulse, Send } from 'lucide-react';
import { flowsApi, type N8nHealth, type AskProposal, type AskReply, type DayBucket, type ErrorGroup, type Health } from '../api/flows';
import { Pill } from './FlowRun';

const card: CSSProperties = { background: 'rgba(255,255,255,0.04)', border: '1px solid var(--glass-border)', borderRadius: 12, padding: 14 };
const muted: CSSProperties = { fontSize: 12.5, color: 'var(--text-secondary)' };
const button: CSSProperties = { padding: '6px 11px', borderRadius: 8, border: '1px solid rgba(6,182,212,0.4)', background: 'rgba(6,182,212,0.12)', color: 'var(--accent-cyan)', fontSize: 12.5, cursor: 'pointer', display: 'inline-flex', alignItems: 'center', gap: 6 };
const input: CSSProperties = { flex: 1, background: 'rgba(0,0,0,0.35)', border: '1px solid var(--glass-border)', borderRadius: 8, padding: '6px 8px', color: 'inherit', fontSize: 13, boxSizing: 'border-box', minWidth: 0 };

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

const STATE_TEXT: Record<Health['state'], string> = { idle: 'not run yet', healthy: 'healthy', degraded: 'some trouble', failing: 'failing' };
const STATE_PILL: Record<Health['state'], string> = { idle: 'queued', healthy: 'succeeded', degraded: 'waiting', failing: 'failed' };

/** Fourteen small bars, oldest to newest: green worked, lime recovered, amber unconfirmed, red failed. */
function History({ days }: { days: DayBucket[] }) {
  const max = Math.max(1, ...days.map((d) => d.success + d.recovered + d.failed + d.unverified));
  return (
    <div style={{ display: 'flex', gap: 3, alignItems: 'flex-end', height: 34 }} aria-label="Last 14 days" data-testid="history">
      {days.map((d) => {
        const total = d.success + d.recovered + d.failed + d.unverified;
        const part = (n: number, colour: string) => n > 0 && <div style={{ height: `${(n / max) * 100}%`, background: colour, minHeight: 3 }} />;
        return (
          <div key={d.day} title={`${new Date(d.starts_at * 1000).toLocaleDateString()}: ${d.success} worked, ${d.recovered} recovered, ${d.unverified} unconfirmed, ${d.failed} failed`} style={{ width: 10, height: '100%', display: 'flex', flexDirection: 'column-reverse', justifyContent: 'flex-start', background: total ? 'transparent' : 'rgba(255,255,255,0.06)', borderRadius: 2, overflow: 'hidden' }}>
            {part(d.success, '#10b981')}{part(d.recovered, '#84cc16')}{part(d.unverified, '#f59e0b')}{part(d.failed, '#f87171')}
          </div>
        );
      })}
    </div>
  );
}

export function HealthDashboard({ onOpen }: { onOpen: (flowId: number, runId?: number) => void }) {
  const [data, setData] = useState<{ flows: { health: Health; history: DayBucket[] }[]; errors: ErrorGroup[] } | null>(null);
  const load = useCallback(() => flowsApi.health().then(setData).catch(() => undefined), []);
  useEffect(() => {
    void load();
    const t = window.setInterval(() => void load(), 10000);
    return () => window.clearInterval(t);
  }, [load]);
  if (!data || (data.flows.every((f) => f.health.state === 'idle') && data.errors.length === 0)) return null;
  return (
    <div style={{ display: 'grid', gap: 8 }} data-testid="health-dashboard">
      <strong><HeartPulse size={14} /> Workflow health</strong>
      <div style={{ display: 'grid', gap: 8, gridTemplateColumns: 'repeat(auto-fill, minmax(260px, 1fr))' }}>
        {data.flows.filter((f) => f.health.state !== 'idle').map(({ health: h, history }) => (
          <div key={h.flow_id} style={{ ...card, display: 'grid', gap: 6 }} data-testid="health-card">
            <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
              <strong style={{ flex: 1, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis' }}>{h.name}</strong>
              <Pill status={STATE_PILL[h.state]} text={`${STATE_TEXT[h.state]}${h.score !== null ? ` · ${h.score}` : ''}`} />
            </div>
            <History days={history} />
            {h.reasons.map((r) => <div key={r} style={muted}>{r}</div>)}
            <div><button style={button} onClick={() => onOpen(h.flow_id)}>Open</button></div>
          </div>
        ))}
      </div>
      {data.errors.length > 0 && (
        <div style={{ ...card, display: 'grid', gap: 6 }} data-testid="error-dashboard">
          <strong>Failures in the last 14 days</strong>
          {data.errors.map((g) => (
            <div key={g.class} style={{ display: 'flex', gap: 10, alignItems: 'center' }}>
              <div style={{ flex: 1, minWidth: 0 }}>
                <div style={{ fontSize: 13 }}>{g.count} × {g.label} <span style={muted}>({g.explain}) in {g.flows.join(', ')}</span></div>
                <div style={muted}>{g.example}</div>
              </div>
              <button style={button} onClick={() => onOpen(g.last_flow_id, g.last_run_id)}>Latest</button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** Ask about your workflows in words. Answers come from the run history; changes need your approval here. */
export function StatusChat({ onChanged }: { onChanged: () => void }) {
  const [text, setText] = useState('');
  const [reply, setReply] = useState<AskReply | null>(null);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState('');
  const [err, setErr] = useState('');
  const send = (q: string) => {
    if (!q.trim()) return;
    setBusy(true); setErr(''); setNote('');
    flowsApi.askAssistant(q).then(setReply).catch((e) => setErr(message(e))).finally(() => setBusy(false));
  };
  const apply = (p: AskProposal) => flowsApi.applyRecovery(p.run_id, p.node_id, p.key)
    .then((r) => { setNote(`Applied to ${p.flow_name}: ${p.title} (now version ${r.version}). The next run shows whether it helped; you can undo it from the run.`); setReply((x) => (x ? { ...x, proposals: x.proposals.filter((y) => y !== p) } : x)); onChanged(); })
    .catch((e) => setErr(message(e)));
  return (
    <div style={{ ...card, display: 'grid', gap: 8 }} data-testid="status-chat">
      <strong>Ask about your workflows</strong>
      <div style={{ display: 'flex', gap: 6 }}>
        <input style={input} value={text} onChange={(e) => setText(e.target.value)} onKeyDown={(e) => { if (e.key === 'Enter') send(text); }} aria-label="Ask about your workflows" placeholder="Did my scheduled workflow run? · Fix the last error · Check my workflows and fix what you can" />
        <button style={button} onClick={() => send(text)} disabled={busy || !text.trim()}><Send size={13} /> Ask</button>
      </div>
      <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
        {['Did my scheduled workflow run?', 'Fix the last error', 'Check my workflows and fix any problems you can'].map((q) => <button key={q} style={{ ...button, opacity: 0.8 }} onClick={() => { setText(q); send(q); }}>{q}</button>)}
      </div>
      {reply && (
        <div style={{ display: 'grid', gap: 6 }} data-testid="status-reply">
          <div style={{ whiteSpace: 'pre-wrap', fontSize: 13 }}>{reply.reply}</div>
          {reply.proposals.map((p) => (
            <div key={`${p.run_id}:${p.node_id}:${p.key}`} style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
              <span style={{ flex: 1, fontSize: 13 }}>{p.flow_name}: {p.title}</span>
              <button style={button} onClick={() => void apply(p)}>Approve and apply</button>
            </div>
          ))}
        </div>
      )}
      {note && <div style={{ ...muted, color: '#10b981' }} data-testid="status-note">{note}</div>}
      {err && <div role="alert" style={{ color: '#f87171', fontSize: 12.5 }}>{err}</div>}
    </div>
  );
}

/** Is n8n set up and running? Shown on an n8n step, so a missing n8n is seen before the run, not in it. */
export function N8nStatus() {
  const [h, setH] = useState<N8nHealth | null>(null);
  const [err, setErr] = useState('');
  const check = useCallback(() => { setErr(''); flowsApi.n8nHealth().then(setH).catch((e) => setErr(message(e))); }, []);
  useEffect(() => { check(); }, [check]);
  const colour = h?.reachable ? '#10b981' : '#f59e0b';
  return (
    <div style={{ ...card, display: 'grid', gap: 4, borderColor: `${colour}88` }} data-testid="n8n-status">
      <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
        <strong style={{ flex: 1, fontSize: 13 }}>n8n on this computer: {h ? (h.reachable ? 'up' : h.configured ? 'not answering' : 'not set up') : 'checking…'}</strong>
        <button style={button} onClick={check}>Check again</button>
      </div>
      <div style={muted}>{err || h?.detail}</div>
    </div>
  );
}
