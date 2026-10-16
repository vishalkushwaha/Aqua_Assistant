import { useCallback, useEffect, useRef, useState } from 'react';
import { errorSentence } from '../api/coreRequest';
import {
  acceptedSentence,
  cautions,
  collisionSentence,
  originText,
  skillDescription,
  skillName,
  skillsInboxApi,
  skillText,
  toolsText,
  type Inbox,
  type SkillProposal,
} from '../api/skillsInbox';
import { PageHeader, Section } from './ui';
import { alertText, input, muted, noticeText, row, rowBorder, small, textBox } from './screenStyles';

type Outcome = 'accepted' | 'collision' | 'failed';

/**
 * One proposal: what the skill is, what it may use, where it came from, and the three things a person can do with it. Accepting asks first,
 * here in the card. The text of the skill opens in a read-only box and is plain text, never markup. The card holds only what is on the
 * screen (which question is open); what happens to the proposal is the core's.
 */
function ProposalCard({
  proposal,
  busy,
  accept,
  reject,
}: {
  proposal: SkillProposal;
  busy: boolean;
  accept: (proposal: SkillProposal, changes: { name?: string; replace?: boolean }) => Promise<Outcome>;
  reject: (proposal: SkillProposal) => Promise<void>;
}) {
  const [asking, setAsking] = useState(false);
  const [viewing, setViewing] = useState(false);
  // Set when the name is one of the person's own skills: the name that was tried, and whether the question about replacing it is open.
  const [clash, setClash] = useState<{ tried: string; replacing: boolean } | null>(null);
  const [nameDraft, setNameDraft] = useState(proposal.name);
  const warnings = cautions(proposal);

  const tryAccept = async (changes: { name?: string; replace?: boolean }) => {
    const outcome = await accept(proposal, changes);
    if (outcome === 'collision') {
      setAsking(false);
      setClash({ tried: changes.name ?? proposal.name, replacing: false });
    }
    if (outcome === 'failed') setAsking(false);
  };

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8, padding: '14px 0' }}>
      <div style={{ fontSize: 14.5, fontWeight: 600, overflowWrap: 'anywhere' }}>{skillName(proposal)}</div>
      <div style={{ fontSize: 13.5, lineHeight: 1.5, overflowWrap: 'anywhere' }}>{skillDescription(proposal)}</div>
      <div style={muted}>{toolsText(proposal)}</div>
      <div style={muted}>{originText(proposal, Date.now())}</div>
      {warnings.map((w, i) => (
        <div key={i} style={{ fontSize: 12.5, color: 'var(--state-attention)', lineHeight: 1.5 }}>{w}</div>
      ))}

      {viewing && (
        <textarea
          readOnly
          value={skillText(proposal)}
          aria-label={`The text of the skill ${skillName(proposal)}`}
          rows={Math.min(18, Math.max(6, skillText(proposal).split('\n').length + 1))}
          style={textBox}
        />
      )}

      {clash !== null && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
          <div role="alert" style={alertText}>{collisionSentence(clash.tried)} Accept this one under another name, or replace yours (its old text is kept as an earlier version).</div>
          <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', alignItems: 'center' }}>
            <input value={nameDraft} onChange={(e) => setNameDraft(e.target.value)} aria-label="Another name for the skill" style={{ ...input, flex: '0 1 260px' }} />
            <button type="button" className="btn-primary" style={small} disabled={busy || nameDraft.trim() === ''} onClick={() => void tryAccept({ name: nameDraft })}>Accept under this name</button>
            {clash.replacing ? (
              <>
                <span style={{ fontSize: 13 }}>Replace your skill {nameDraft.trim()}?</span>
                <button type="button" className="btn-danger" style={small} disabled={busy} onClick={() => void tryAccept({ name: nameDraft, replace: true })}>Yes, replace it</button>
                <button type="button" className="btn-ghost" style={small} disabled={busy} onClick={() => setClash({ ...clash, replacing: false })}>No</button>
              </>
            ) : (
              <button type="button" className="btn-ghost" style={small} disabled={busy || nameDraft.trim() === ''} onClick={() => setClash({ ...clash, replacing: true })}>Replace mine</button>
            )}
            <button type="button" className="btn-ghost" style={small} disabled={busy} onClick={() => setClash(null)}>Cancel</button>
          </div>
        </div>
      )}

      <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', alignItems: 'center' }}>
        {asking ? (
          <>
            <span style={{ fontSize: 13 }}>Accept the skill {skillName(proposal)}? Aqua reads it the next time it starts.</span>
            <button type="button" className="btn-primary" style={small} disabled={busy} onClick={() => void tryAccept({})}>Yes, accept it</button>
            <button type="button" className="btn-ghost" style={small} disabled={busy} onClick={() => setAsking(false)}>Not yet</button>
          </>
        ) : (
          <>
            <button type="button" className="btn-primary" style={small} disabled={busy || clash !== null} onClick={() => setAsking(true)}>Accept</button>
            <button type="button" className="btn-ghost" style={small} disabled={busy} onClick={() => void reject(proposal)}>Reject</button>
            <button type="button" className="btn-ghost" style={small} onClick={() => setViewing(!viewing)}>{viewing ? 'Hide the text' : 'Open the text'}</button>
          </>
        )}
      </div>
    </div>
  );
}

/**
 * The skills Aqua proposes after a task that went well. Nothing becomes a skill until it is accepted here, and a skill can only narrow
 * what a run may do. Everything that comes from the core (a name, a description, a body) is shown as text.
 */
export function SkillsInbox() {
  const [inbox, setInbox] = useState<Inbox | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [showDecided, setShowDecided] = useState(false);
  const seq = useRef(0);

  const load = useCallback(async () => {
    const mine = ++seq.current;
    try {
      const list = await skillsInboxApi.list();
      if (mine !== seq.current) return;
      setInbox(list);
      setLoadError(null);
    } catch (e) {
      if (mine !== seq.current) return;
      setLoadError(errorSentence(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const accept = async (p: SkillProposal, changes: { name?: string; replace?: boolean }): Promise<Outcome> => {
    setBusy(true);
    setError(null);
    setNotice(null);
    let outcome: Outcome = 'failed';
    try {
      const r = await skillsInboxApi.approve(p.id, changes);
      if (r.kind === 'accepted') {
        setNotice(acceptedSentence(r.skill, r.version));
        outcome = 'accepted';
      } else {
        outcome = 'collision';
      }
    } catch (e) {
      setError(errorSentence(e));
    } finally {
      setBusy(false);
    }
    await load();
    return outcome;
  };

  const reject = async (p: SkillProposal) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await skillsInboxApi.reject(p.id);
      setNotice(`Rejected ${skillName(p)}.`);
    } catch (e) {
      setError(errorSentence(e));
    } finally {
      setBusy(false);
    }
    await load();
  };

  const waiting = inbox?.waiting ?? [];
  const decided = inbox?.decided ?? [];

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 20, maxWidth: 980, margin: '0 auto' }}>
      <PageHeader
        title="Skills inbox"
        subtitle="After a task goes well, Aqua can offer to keep how it did it as a skill. Nothing becomes a skill until you accept it here, and a skill can only narrow what Aqua may do, never widen it."
      />

      {error && <div role="alert" style={alertText}>{error}</div>}
      {notice && <div role="status" style={noticeText}>{notice}</div>}

      <Section title="Waiting for you">
        {loadError && <div role="alert" style={alertText}>{loadError}</div>}
        {inbox === null && !loadError && <div style={muted}>Loading…</div>}
        {inbox !== null && waiting.length === 0 && <div style={muted}>Nothing is waiting. When Aqua has a skill to offer, it is shown here.</div>}
        {waiting.length > 0 && (
          <div>
            {waiting.map((p, i) => (
              <div key={p.id} style={{ borderTop: i === 0 ? 'none' : rowBorder }}>
                <ProposalCard proposal={p} busy={busy} accept={accept} reject={reject} />
              </div>
            ))}
          </div>
        )}
      </Section>

      {decided.length > 0 && (
        <Section
          title="Decided before"
          actions={
            <button type="button" className="btn-ghost" style={small} onClick={() => setShowDecided(!showDecided)}>
              {showDecided ? 'Hide' : `Show ${decided.length}`}
            </button>
          }
        >
          {showDecided && (
            <ul style={{ listStyle: 'none', margin: 0, padding: 0 }}>
              {decided.map((p, i) => (
                <li key={p.id} style={{ ...row, borderTop: i === 0 ? 'none' : rowBorder }}>
                  <div style={{ flex: '1 1 280px', minWidth: 0 }}>
                    <div style={{ fontSize: 13.5, fontWeight: 600, overflowWrap: 'anywhere' }}>{skillName(p)}</div>
                    <div style={muted}>{originText(p, Date.now())}</div>
                  </div>
                  <span style={muted}>{p.state === 'approved' ? 'accepted' : p.state === 'rejected' ? 'rejected' : p.state}</span>
                </li>
              ))}
            </ul>
          )}
        </Section>
      )}
    </div>
  );
}
