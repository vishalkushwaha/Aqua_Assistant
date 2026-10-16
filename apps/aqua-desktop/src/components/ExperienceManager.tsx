import { useCallback, useEffect, useState, type CSSProperties } from 'react';
import { learningApi, STATUSES, TYPES, type ExperienceDetail, type ExperienceSummary } from '../api/learning';

const card: CSSProperties = { background: 'rgba(255,255,255,0.04)', border: '1px solid var(--glass-border)', borderRadius: 12, padding: 16 };
const muted: CSSProperties = { fontSize: 12.5, color: 'var(--text-secondary)' };
const input: CSSProperties = { background: 'rgba(0,0,0,0.35)', border: '1px solid var(--glass-border)', borderRadius: 8, padding: '7px 10px', color: 'inherit', fontSize: 13 };
const button: CSSProperties = { padding: '6px 11px', borderRadius: 7, border: '1px solid rgba(6,182,212,0.4)', background: 'rgba(6,182,212,0.12)', color: 'var(--accent-cyan)', fontSize: 12, cursor: 'pointer' };
const dangerButton: CSSProperties = { ...button, border: '1px solid rgba(248,113,113,0.5)', background: 'rgba(248,113,113,0.12)', color: '#f87171' };

const STATUS_COLOR: Record<string, string> = {
  CANDIDATE: '#94a3b8',
  VERIFIED: '#38bdf8',
  TRUSTED: '#10b981',
  FREQUENTLY_REUSED: '#34d399',
  FAILED: '#f87171',
  STALE: '#fbbf24',
  DEPRECATED: '#fb923c',
  INVALID: '#f87171',
};

export function StatusBadge({ status }: { status: string }) {
  const color = STATUS_COLOR[status] ?? '#94a3b8';
  return (
    <span style={{ fontSize: 11, padding: '2px 8px', borderRadius: 999, border: `1px solid ${color}`, color, whiteSpace: 'nowrap' }}>{status.replace('_', ' ')}</span>
  );
}

const pct = (v: number) => `${Math.round(v * 100)}%`;
export const when = (secs: number) => (secs > 0 ? new Date(secs * 1000).toLocaleString() : '—');

function Detail({ id, onChanged, onClose }: { id: number; onChanged: () => void; onClose: () => void }) {
  const [d, setD] = useState<ExperienceDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);

  const load = useCallback(async () => {
    try {
      setD(await learningApi.get(id));
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not load this experience');
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  const act = async (fn: () => Promise<unknown>, done: string) => {
    setBusy(true);
    setNote(null);
    setError(null);
    try {
      await fn();
      setNote(done);
      await load();
      onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'That did not work');
    } finally {
      setBusy(false);
    }
  };

  if (!d) return <div style={card}>{error ? <span style={{ color: '#f87171' }}>{error}</span> : 'Loading…'}</div>;
  const e = d.experience;
  return (
    <div style={{ ...card, display: 'flex', flexDirection: 'column', gap: 12 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', gap: 12, alignItems: 'flex-start' }}>
        <div>
          <div style={{ fontWeight: 600 }}>{e.problem_summary}</div>
          <div style={muted}>
            {e.experience_type.replace('_', ' ')} · {e.task_type || 'general'} · v{e.experience_version} · {e.scope.toLowerCase()}
            {e.project_key ? ` · ${e.project_key}` : ''}
          </div>
        </div>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          <StatusBadge status={e.status} />
          <button type="button" style={button} onClick={onClose}>Close</button>
        </div>
      </div>

      <div>
        <div style={{ ...muted, marginBottom: 4 }}>Why it is (or is not) trusted</div>
        <ul style={{ margin: 0, paddingLeft: 18, fontSize: 13, lineHeight: 1.6 }}>
          {d.why_trusted.map((w, i) => <li key={i}>{w}</li>)}
        </ul>
      </div>

      <div>
        <div style={{ ...muted, marginBottom: 4 }}>Approach</div>
        <div style={{ fontSize: 13, whiteSpace: 'pre-wrap' }}>{e.approach || e.solution_summary || '—'}</div>
        {e.important_steps.length > 0 && (
          <ol style={{ margin: '6px 0 0', paddingLeft: 20, fontSize: 13 }}>{e.important_steps.map((s, i) => <li key={i}>{s}</li>)}</ol>
        )}
      </div>

      {(e.failed_approach || e.failure_reason) && (
        <div style={{ fontSize: 13 }}>
          <div style={{ ...muted, marginBottom: 4 }}>Learned from a failure</div>
          <div>Tried: {e.failed_approach}</div>
          <div>Why it failed: {e.failure_reason}</div>
          <div>What worked: {e.correction}</div>
        </div>
      )}

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(150px, 1fr))', gap: 8, fontSize: 12.5 }}>
        <div>Tools: {e.tools_used.join(', ') || '—'}</div>
        <div>Technologies: {e.technologies.join(', ') || '—'}</div>
        <div>Verification: {e.verification_method.replace('_', ' ')}</div>
        <div>Confidence: {pct(e.confidence)}</div>
        <div>Quality: {pct(e.quality_score)}</div>
        <div>Retrieved: {e.retrieval_count}×</div>
        <div>Reused: {e.reuse_count}× ({e.successful_reuse_count} ok, {e.failed_reuse_count} failed)</div>
        <div>Last verified: {when(e.last_verified_at)}</div>
        <div>Last used: {when(e.last_used_at)}</div>
        <div>Embedding: {e.embedding_model} v{e.embedding_version}</div>
      </div>
      {e.constraints.length > 0 && <div style={{ fontSize: 12.5 }}>Constraints: {e.constraints.join(' · ')}</div>}

      {d.usage.length > 0 && (
        <div>
          <div style={{ ...muted, marginBottom: 4 }}>Reuse history</div>
          <div style={{ fontSize: 12.5, display: 'flex', flexDirection: 'column', gap: 2 }}>
            {d.usage.slice(0, 8).map((u) => (
              <div key={u.id}>
                {when(u.used_at)} · similarity {u.similarity.toFixed(2)} · {u.success === null ? 'outcome pending' : u.success ? 'succeeded' : 'failed'} · {u.task_summary}
              </div>
            ))}
          </div>
        </div>
      )}

      <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', alignItems: 'center' }}>
        <button type="button" style={button} disabled={busy || !e.verified} title={e.verified ? '' : 'Only experiences with real verification can be approved'} onClick={() => void act(() => learningApi.setStatus(id, 'approve'), 'Approved: now trusted.')}>Approve / mark trusted</button>
        <button type="button" style={button} disabled={busy} onClick={() => void act(() => learningApi.revalidate(id), 'Revalidated.')}>Revalidate</button>
        <button type="button" style={button} disabled={busy} onClick={() => void act(() => learningApi.setStatus(id, 'deprecate'), 'Deprecated.')}>Deprecate</button>
        <button type="button" style={dangerButton} disabled={busy} onClick={() => void act(() => learningApi.setStatus(id, 'reject'), 'Marked invalid.')}>Mark invalid</button>
        {confirmDelete ? (
          <>
            <button type="button" style={dangerButton} disabled={busy} onClick={() => void act(async () => { await learningApi.remove(id); onClose(); }, 'Deleted.')}>Yes, delete it</button>
            <button type="button" style={button} onClick={() => setConfirmDelete(false)}>Keep</button>
          </>
        ) : (
          <button type="button" style={dangerButton} disabled={busy} onClick={() => setConfirmDelete(true)}>Delete</button>
        )}
        {note && <span style={{ ...muted, color: '#10b981' }}>{note}</span>}
        {error && <span style={{ fontSize: 12.5, color: '#f87171' }}>{error}</span>}
      </div>
    </div>
  );
}

export function ExperienceManager({ refreshKey, onChanged }: { refreshKey: number; onChanged: () => void }) {
  const [rows, setRows] = useState<ExperienceSummary[]>([]);
  const [q, setQ] = useState('');
  const [status, setStatus] = useState('');
  const [type, setType] = useState('');
  const [selected, setSelected] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const r = await learningApi.list({ q: q.trim() || undefined, status: status || undefined, type: type || undefined });
      setRows(r.experiences);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not load experiences');
    }
  }, [q, status, type]);

  useEffect(() => {
    const t = window.setTimeout(() => void load(), 200);
    return () => window.clearTimeout(t);
  }, [load, refreshKey]);

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
      <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
        <input style={{ ...input, flex: '1 1 220px' }} placeholder="Search experiences…" value={q} onChange={(e) => setQ(e.target.value)} aria-label="Search experiences" />
        <select style={input} value={status} onChange={(e) => setStatus(e.target.value)} aria-label="Filter by status">
          <option value="">All statuses</option>
          {STATUSES.map((s) => <option key={s} value={s}>{s.replace('_', ' ')}</option>)}
        </select>
        <select style={input} value={type} onChange={(e) => setType(e.target.value)} aria-label="Filter by type">
          <option value="">All types</option>
          {TYPES.map((t) => <option key={t} value={t}>{t.replace('_', ' ')}</option>)}
        </select>
      </div>
      {error && <div style={{ color: '#f87171', fontSize: 13 }}>{error}</div>}
      {selected !== null && <Detail id={selected} onChanged={() => { void load(); onChanged(); }} onClose={() => setSelected(null)} />}
      <div style={{ ...card, padding: 0, overflow: 'hidden' }}>
        {rows.length === 0 ? (
          <div style={{ padding: 16, ...muted }}>No experiences yet. When the agent finishes a task and its answer is verified against real evidence, it can be remembered here.</div>
        ) : (
          rows.map((r) => (
            <button key={r.id} type="button" onClick={() => setSelected(r.id)} style={{ display: 'grid', gridTemplateColumns: '1fr auto', gap: 8, width: '100%', textAlign: 'left', padding: '10px 14px', border: 'none', borderBottom: '1px solid var(--glass-border)', background: selected === r.id ? 'rgba(6,182,212,0.10)' : 'transparent', color: 'inherit', cursor: 'pointer' }}>
              <div>
                <div style={{ fontSize: 13.5, fontWeight: 600 }}>{r.problem_summary}</div>
                <div style={muted}>{r.experience_type.replace('_', ' ')} · {r.approach_summary || '—'}</div>
                <div style={muted}>
                  confidence {pct(r.confidence)} · quality {pct(r.quality_score)} · reused {r.reuse_count}× ({pct(r.success_rate)}) · verified {when(r.last_verified_at)}
                </div>
              </div>
              <div style={{ alignSelf: 'center' }}><StatusBadge status={r.status} /></div>
            </button>
          ))
        )}
      </div>
    </div>
  );
}
