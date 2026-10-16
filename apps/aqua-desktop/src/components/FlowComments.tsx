import { useCallback, useEffect, useState, type CSSProperties } from 'react';
import { MessageSquare } from 'lucide-react';
import { flowsApi, type CommentDraft } from '../api/flows';

const card: CSSProperties = { background: 'rgba(255,255,255,0.04)', border: '1px solid var(--glass-border)', borderRadius: 12, padding: 14 };
const muted: CSSProperties = { fontSize: 12.5, color: 'var(--text-secondary)' };
const button: CSSProperties = { padding: '6px 11px', borderRadius: 8, border: '1px solid rgba(6,182,212,0.4)', background: 'rgba(6,182,212,0.12)', color: 'var(--accent-cyan)', fontSize: 12.5, cursor: 'pointer' };
const quiet: CSSProperties = { ...button, border: '1px solid var(--glass-border)', background: 'transparent', color: 'var(--text-secondary)' };
const MAX = 1250;

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

function Item({ d, onChanged, onOpen }: { d: CommentDraft; onChanged: () => void; onOpen: (flowId: number, runId: number) => void }) {
  const [text, setText] = useState(d.reply);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const act = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    setError('');
    try {
      await fn();
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(false);
      onChanged();
    }
  };
  const waiting = d.status === 'pending';
  const doubtful = d.status === 'unknown' || d.status === 'failed';
  return (
    <div style={{ ...card, display: 'grid', gap: 8 }} data-testid="comment-draft" data-status={d.status}>
      <div style={{ display: 'flex', gap: 8, alignItems: 'baseline', flexWrap: 'wrap' }}>
        <strong>{d.author}</strong>
        <span style={muted}>{d.category}</span>
        <button style={{ ...quiet, padding: '2px 8px' }} onClick={() => onOpen(d.flow_id, d.run_id)}>open run</button>
        <span style={{ ...muted, marginLeft: 'auto' }} data-testid="draft-status">{d.status}</span>
      </div>
      <blockquote style={{ margin: 0, padding: '4px 10px', borderLeft: '3px solid var(--glass-border)', fontSize: 13 }}>{d.comment_text}</blockquote>
      {d.note && <div style={{ ...muted, color: '#f59e0b' }}>{d.note}</div>}
      {waiting ? (
        <>
          <textarea aria-label={`Reply to ${d.author}`} style={{ background: 'rgba(0,0,0,0.35)', border: '1px solid var(--glass-border)', borderRadius: 8, padding: 8, color: 'inherit', fontSize: 13, minHeight: 70, boxSizing: 'border-box', width: '100%' }}
            value={text} maxLength={MAX} onChange={(e) => setText(e.target.value)} />
          <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
            <button style={button} disabled={busy || !text.trim()} onClick={() => void act(() => flowsApi.approveComment(d.id, text))}>
              {busy ? 'Posting…' : 'Approve and post'}
            </button>
            <button style={quiet} disabled={busy} onClick={() => void act(() => flowsApi.skipComment(d.id))}>Skip</button>
            <span style={muted}>Posts publicly as you, exactly as written here. {text.length}/{MAX}</span>
          </div>
        </>
      ) : (
        <div style={muted}>{d.status === 'posted' ? `Posted: “${d.sent_text}”` : d.sent_text ? `Reply: “${d.sent_text}”` : `Draft: “${d.reply}”`}</div>
      )}
      {d.error && <div role="alert" style={{ color: '#f87171', fontSize: 12.5 }}>{d.error}</div>}
      {doubtful && (
        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          <span style={muted}>Check the post yourself, then:</span>
          <button style={button} disabled={busy} onClick={() => void act(() => flowsApi.resolveComment(d.id, 'posted'))}>It was posted</button>
          <button style={quiet} disabled={busy} onClick={() => void act(() => flowsApi.resolveComment(d.id, 'retry'))}>Try again</button>
        </div>
      )}
      {error && <div role="alert" style={{ color: '#f87171', fontSize: 12.5 }}>{error}</div>}
    </div>
  );
}

/** Replies drafted for comments on your posts. Nothing is posted until you press "Approve and post". */
export function CommentInbox({ onOpen }: { onOpen: (flowId: number, runId: number) => void }) {
  const [data, setData] = useState<{ drafts: CommentDraft[]; counts: Record<string, number> } | null>(null);
  const load = useCallback(() => flowsApi.commentDrafts().then(setData).catch(() => undefined), []);
  useEffect(() => {
    void load();
    const t = window.setInterval(() => void load(), 10000);
    return () => window.clearInterval(t);
  }, [load]);
  if (!data || data.drafts.length === 0) return null;
  const open = data.drafts.filter((d) => ['pending', 'posting', 'unknown', 'failed'].includes(d.status));
  const closed = data.drafts.filter((d) => !open.includes(d)).slice(0, 5);
  return (
    <div style={{ ...card, display: 'grid', gap: 10 }} data-testid="comment-inbox">
      <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
        <MessageSquare size={16} />
        <strong>Comment replies</strong>
        <span style={muted} data-testid="pending-count">{data.counts.pending ?? 0} waiting for you</span>
      </div>
      {open.map((d) => <Item key={`${d.id}-${d.status}`} d={d} onChanged={() => void load()} onOpen={onOpen} />)}
      {closed.length > 0 && <div style={muted}>Recently done: {closed.map((d) => `${d.author} (${d.status})`).join(', ')}</div>}
    </div>
  );
}
