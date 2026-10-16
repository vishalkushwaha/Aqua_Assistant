import { useCallback, useEffect, useRef, useState, type CSSProperties } from 'react';
import { RefreshCw } from 'lucide-react';
import { Section } from './ui';
import { ALREADY_LAPSED, consentsApi, denialFacts, grantFacts, TAKEN_BACK, type Consents } from '../api/consents';
import { errorSentence } from '../api/plain';
import { useVisiblePolling } from '../hooks/useVisiblePolling';

const muted: CSSProperties = { fontSize: 12.5, color: 'var(--text-secondary)' };
const small: CSSProperties = { padding: '3px 10px' };
const heading: CSSProperties = { margin: 0, fontSize: 14, fontWeight: 600, color: 'var(--text-primary)' };
const rowStyle = (first: boolean): CSSProperties => ({
  display: 'flex', gap: 12, alignItems: 'center', justifyContent: 'space-between', flexWrap: 'wrap', padding: '10px 0', borderTop: first ? 'none' : '1px solid var(--glass-border)',
});

interface RowProps {
  /** What goes into DELETE /api/v1/consents/{id}; null: the core sent none, so the row has no button. */
  id: string | null;
  title: string;
  detail?: string;
  facts?: string;
  first: boolean;
  question: string;
  confirming: string | null;
  busy: string | null;
  onAsk: (id: string) => void;
  onCancel: () => void;
  onConfirm: (id: string) => void;
}

/** One yes that can be taken back. The question is asked here, in the panel, before anything is sent. */
function YesRow({ id, title, detail, facts, first, question, confirming, busy, onAsk, onCancel, onConfirm }: RowProps) {
  return (
    <li style={rowStyle(first)}>
      <div style={{ flex: '1 1 280px', minWidth: 0 }}>
        <div style={{ fontSize: 13.5, fontWeight: 600, overflowWrap: 'anywhere' }}>{title}</div>
        {detail && <div style={{ ...muted, overflowWrap: 'anywhere' }}>{detail}</div>}
        {facts && <div style={muted}>{facts}</div>}
      </div>
      {id !== null && (
        <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
          {confirming === id ? (
            <>
              <span style={{ fontSize: 13 }}>{question}</span>
              <button type="button" className="btn-danger" style={small} disabled={busy !== null} onClick={() => onConfirm(id)}>Yes, take it back</button>
              <button type="button" className="btn-ghost" style={small} disabled={busy !== null} onClick={onCancel}>Keep it</button>
            </>
          ) : (
            <button type="button" className="btn-ghost" style={small} disabled={busy !== null} aria-label={`Take back: ${title}`} onClick={() => onAsk(id)}>Take back</button>
          )}
        </div>
      )}
    </li>
  );
}

/**
 * What the person said yes to: the "approve for this session" answers that still apply (each lapses by itself), the sites allowed for
 * good, and the calls refused lately. Taking a yes back asks first, here in the panel; the next call of that kind asks again.
 * Everything that comes from the core (a tool, a target, a site, a reason) is a text child, never markup.
 */
export function ConsentsPanel({ refreshKey = 0 }: { refreshKey?: number }) {
  const [data, setData] = useState<Consents | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  // Only the newest answer is used: a slow list must not overwrite the list read after a yes was taken back.
  const latest = useRef(0);

  const load = useCallback(async () => {
    const mine = ++latest.current;
    try {
      const list = await consentsApi.list();
      if (mine !== latest.current) return;
      setData(list);
      setError(null);
    } catch (e) {
      if (mine !== latest.current) return;
      setError(errorSentence(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load, refreshKey]);
  // A yes for a session lapses by itself; the list follows.
  useVisiblePolling(() => void load(), 15_000);

  const takeBack = async (id: string) => {
    setBusy(id);
    setError(null);
    setNotice(null);
    try {
      const { takenBack } = await consentsApi.takeBack(id);
      setNotice(takenBack ? TAKEN_BACK : ALREADY_LAPSED);
      await load();
    } catch (e) {
      setError(errorSentence(e));
    } finally {
      setBusy(null);
      setConfirming(null);
    }
  };

  const now = Date.now() / 1000;
  const common = { confirming, busy, onAsk: (id: string) => { setNotice(null); setConfirming(id); }, onCancel: () => setConfirming(null), onConfirm: (id: string) => void takeBack(id) };

  return (
    <Section
      title="What you have said yes to"
      hint="Answers you gave to Aqua's questions, so that you can take them back. A yes for a session lapses by itself; a site stays allowed until you take it back. Taking a yes back changes nothing else: the next call of that kind asks again."
      actions={
        <button type="button" className="btn-ghost" style={{ ...small, display: 'inline-flex', alignItems: 'center', gap: 6 }} onClick={() => void load()}>
          <RefreshCw size={14} /> Refresh
        </button>
      }
    >
      {error && <div role="alert" style={{ color: 'var(--state-problem)', fontSize: 13 }}>{error}</div>}
      {notice && <div role="status" style={{ color: 'var(--state-ok)', fontSize: 13 }}>{notice}</div>}
      {data === null && !error && <div style={muted}>Loading…</div>}
      {data !== null && (
        <>
          {data.note && <div style={muted}>{data.note}</div>}

          <div>
            <h4 style={heading}>Yes for this session</h4>
            {data.grants.length === 0 ? (
              <div style={{ ...muted, marginTop: 6 }}>Nothing is allowed for this session. When you approve a tool call "for this session", it is listed here until it lapses.</div>
            ) : (
              <ul style={{ listStyle: 'none', margin: 0, padding: 0 }}>
                {data.grants.map((g, i) => (
                  <YesRow key={g.id ?? `grant-${i}`} id={g.id} title={g.tool || 'A tool'} detail={g.target} facts={grantFacts(g)} first={i === 0}
                    question="Take back this yes? The next call of that kind asks again." {...common} />
                ))}
              </ul>
            )}
          </div>

          <div>
            <h4 style={heading}>Sites allowed for good</h4>
            {data.sites.length === 0 ? (
              <div style={{ ...muted, marginTop: 6 }}>No site is allowed for good. When you choose "Always allow" for a site, it is listed here.</div>
            ) : (
              <ul style={{ listStyle: 'none', margin: 0, padding: 0 }}>
                {data.sites.map((s, i) => (
                  <YesRow key={s.id ?? `site-${i}`} id={s.id} title={s.site} first={i === 0} question={`Stop allowing ${s.site}? Aqua asks before it uses the site again.`} {...common} />
                ))}
              </ul>
            )}
          </div>

          <div>
            <h4 style={heading}>Refused lately</h4>
            {data.denials.length === 0 ? (
              <div style={{ ...muted, marginTop: 6 }}>Nothing was refused lately.</div>
            ) : (
              <ul style={{ listStyle: 'none', margin: 0, padding: 0 }}>
                {data.denials.map((d, i) => (
                  <li key={`${d.atMs ?? 'x'}-${i}`} style={rowStyle(i === 0)}>
                    <div style={{ flex: '1 1 280px', minWidth: 0 }}>
                      <div style={{ fontSize: 13.5, fontWeight: 600, overflowWrap: 'anywhere' }}>{d.tool || 'A tool'}</div>
                      {d.summary && <div style={{ ...muted, overflowWrap: 'anywhere' }}>{d.summary}</div>}
                      <div style={muted}>{denialFacts(d, now)}</div>
                      {d.reason && <div style={{ ...muted, overflowWrap: 'anywhere' }}>{d.reason}</div>}
                    </div>
                  </li>
                ))}
              </ul>
            )}
          </div>
        </>
      )}
    </Section>
  );
}
