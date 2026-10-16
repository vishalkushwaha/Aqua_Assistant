import { useEffect, useRef, useState, type FormEvent } from 'react';
import { ArrowRight, Building2, CheckCircle2, Database, History, Play, Send, ShieldCheck } from 'lucide-react';
import { falkorApi, type CompanyBrainAnswer, type CompanyMemorySnapshot } from '../api/falkorApi';
import { PageHeader, Pill, Section } from './ui';

const DEMO_QUESTION = 'Who should handle the OAuth production incident, what earlier decision affects the fix, and which sources support the answer?';

export function CompanyBrainPage() {
  const [question, setQuestion] = useState(DEMO_QUESTION);
  const [answer, setAnswer] = useState<CompanyBrainAnswer | null>(null);
  const [busy, setBusy] = useState(false);
  const [seeding, setSeeding] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [completedAction, setCompletedAction] = useState<string | null>(null);
  const [memory, setMemory] = useState<CompanyMemorySnapshot | null>(null);
  const automatedDemoStarted = useRef(false);

  useEffect(() => {
    const stage = new URLSearchParams(window.location.search).get('demo');
    if (!stage || automatedDemoStarted.current) return;
    automatedDemoStarted.current = true;
    void (async () => {
      try {
        setSeeding(true);
        const seeded = await falkorApi.seedCompanyBrain();
        setNotice(seeded.message || 'Company Brain dataset is ready.');
        setSeeding(false);
        if (stage === 'seed') return;

        setBusy(true);
        const asked = await falkorApi.askCompanyBrain(DEMO_QUESTION);
        setAnswer(asked);
        if (stage === 'answer') return;

        const acted = await falkorApi.executeCompanyAction(asked.action!);
        setCompletedAction(acted.actionId);
        setNotice(acted.message);
        if (stage === 'action') return;

        setMemory(await falkorApi.recallCompanyMemory());
        if (stage === 'memory') return;

        const handedOff = await falkorApi.createCompanyHandoff(
          `Investigate ${asked.facts.ticketId} using ${asked.facts.decisionId} and the cited graph sources.`,
        );
        setNotice(handedOff.message);
        setMemory(await falkorApi.recallCompanyMemory());
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setSeeding(false);
        setBusy(false);
        if (stage === 'memory' || stage === 'handoff') {
          window.setTimeout(() => document.querySelector('[data-demo-memory]')?.scrollIntoView({ block: 'start' }), 400);
        }
      }
    })();
  }, []);

  const seed = async () => {
    setSeeding(true);
    setError(null);
    setNotice(null);
    try {
      const result = await falkorApi.seedCompanyBrain();
      setNotice(result.message || 'Company Brain dataset is ready.');
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSeeding(false);
    }
  };

  const ask = async (event: FormEvent) => {
    event.preventDefault();
    if (!question.trim()) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    setCompletedAction(null);
    try {
      setAnswer(await falkorApi.askCompanyBrain(question.trim()));
    } catch (e) {
      setAnswer(null);
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const executeAction = async () => {
    if (!answer?.action) return;
    setBusy(true);
    setError(null);
    try {
      const result = await falkorApi.executeCompanyAction(answer.action);
      setCompletedAction(result.actionId);
      setNotice(result.message);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const recallMemory = async () => {
    setBusy(true);
    setError(null);
    try {
      setMemory(await falkorApi.recallCompanyMemory());
      setNotice('Persistent session memory recalled directly from FalkorDB.');
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const handoff = async () => {
    if (!answer) return;
    setBusy(true);
    setError(null);
    try {
      const result = await falkorApi.createCompanyHandoff(
        `Investigate ${answer.facts.ticketId} using ${answer.facts.decisionId} and the cited graph sources.`,
      );
      setNotice(result.message);
      setMemory(await falkorApi.recallCompanyMemory());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="page" style={{ display: 'flex', flexDirection: 'column', gap: 20, maxWidth: 1100, margin: '0 auto' }}>
      <PageHeader
        title="Company Brain Agent"
        subtitle="Ask across people, projects, tickets, decisions, documents, and chats. Every answer and proposed action must come from a connected FalkorDB path."
        actions={<Pill tone="ok" label="Context layer" value="FalkorDB required" />}
      />

      {(error || notice) && (
        <div role={error ? 'alert' : 'status'} style={{
          padding: '12px 14px', borderRadius: 9,
          border: `1px solid ${error ? 'rgba(239,68,68,.35)' : 'rgba(16,185,129,.35)'}`,
          background: error ? 'rgba(239,68,68,.08)' : 'rgba(16,185,129,.08)',
          color: error ? '#f87171' : '#34d399',
        }}>
          {error || notice}
        </div>
      )}

      <Section
        title="1. Prepare connected company knowledge"
        hint="The repeatable seed creates a tenant-scoped demo company without deleting any existing graph data."
        actions={
          <button className="btn-ghost" onClick={() => void seed()} disabled={seeding} style={{ display: 'flex', gap: 7, alignItems: 'center' }}>
            <Database size={15} /> {seeding ? 'Preparing…' : 'Prepare demo company'}
          </button>
        }
      >
        <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
          {['Person', 'Team', 'Project', 'Ticket', 'Decision', 'Document', 'Chat'].map((label) => (
            <span key={label} className="pill">{label}</span>
          ))}
        </div>
      </Section>

      <Section title="2. Ask the graph-grounded agent" hint="The agent selects a retrieval intent, traverses the graph, and refuses to answer when FalkorDB is unavailable.">
        <form onSubmit={ask} style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
          <textarea
            rows={3}
            value={question}
            onChange={(event) => setQuestion(event.target.value)}
            aria-label="Company question"
            style={{
              width: '100%', padding: 12, borderRadius: 9, resize: 'vertical',
              color: 'var(--text-primary)', background: 'rgba(0,0,0,.28)',
              border: '1px solid var(--glass-border)', font: 'inherit', lineHeight: 1.5,
            }}
          />
          <button className="btn-primary" disabled={busy || !question.trim()} style={{ alignSelf: 'flex-start', display: 'flex', gap: 7, alignItems: 'center' }}>
            <Play size={15} /> {busy ? 'Following graph paths…' : 'Ask Company Brain'}
          </button>
        </form>
      </Section>

      {answer && (
        <>
          <Section
            title="Grounded answer"
            hint={`Retrieval intent: ${answer.intent}. Interaction ${answer.interactionId} was persisted for later sessions.`}
            actions={<Pill tone="ok" label="Graph grounded" value={`${answer.sources.length} sources`} />}
          >
            <p style={{ fontSize: 16, lineHeight: 1.7 }}>{answer.answer}</p>
          </Section>

          <div style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1.2fr) minmax(280px, .8fr)', gap: 20 }}>
            <Section title="Why this answer" hint="These are the exact relationships FalkorDB followed.">
              <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
                {answer.paths.map((path, index) => (
                  <div key={`${path.join('-')}-${index}`} style={{ display: 'flex', alignItems: 'center', gap: 7, flexWrap: 'wrap', fontSize: 13 }}>
                    {path.map((part, partIndex) => (
                      <span key={`${part}-${partIndex}`} style={{ display: 'contents' }}>
                        {partIndex > 0 && <ArrowRight size={13} color="var(--text-muted)" />}
                        <span style={{ padding: '5px 8px', borderRadius: 6, background: partIndex % 2 ? 'rgba(6,182,212,.1)' : 'rgba(255,255,255,.05)' }}>{part}</span>
                      </span>
                    ))}
                  </div>
                ))}
              </div>
            </Section>

            <Section title="Sources" hint="Every factual answer points back to company records.">
              <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
                {answer.sources.map((source) => (
                  <div key={source.id} style={{ padding: 10, borderRadius: 8, border: '1px solid var(--glass-border)' }}>
                    <div style={{ fontWeight: 600, fontSize: 13.5 }}>{source.title}</div>
                    <div style={{ color: 'var(--text-secondary)', fontSize: 12, marginTop: 3 }}>{source.kind} · {source.uri}</div>
                  </div>
                ))}
              </div>
            </Section>
          </div>

          {answer.action && (
            <Section
              title="3. Act on connected data"
              hint="The assignment is allowed only if the proposed person owns the ticket's project. The action and its supporting ownership path are written back to FalkorDB."
              actions={<Pill tone={completedAction ? 'ok' : 'attention'} label={completedAction ? 'Completed' : 'Approval required'} />}
            >
              <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 16, flexWrap: 'wrap' }}>
                <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
                  {completedAction ? <CheckCircle2 size={20} color="var(--state-ok)" /> : <ShieldCheck size={20} color="var(--state-attention)" />}
                  <span>{answer.action.label}</span>
                </div>
                {!completedAction && (
                  <button className="btn-primary" onClick={() => void executeAction()} disabled={busy}>Approve graph-backed action</button>
                )}
              </div>
            </Section>
          )}

          <Section
            title="4. Remember and coordinate"
            hint="Recall the persisted question, grounded answer, action, and sources, then hand the incident to a second agent through the same tenant-scoped graph."
            actions={
              <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                <button className="btn-ghost" onClick={() => void recallMemory()} disabled={busy} style={{ display: 'flex', gap: 7, alignItems: 'center' }}>
                  <History size={15} /> Recall session
                </button>
                <button className="btn-primary" onClick={() => void handoff()} disabled={busy} style={{ display: 'flex', gap: 7, alignItems: 'center' }}>
                  <Send size={15} /> Hand off to incident agent
                </button>
              </div>
            }
          >
            <span data-demo-memory style={{ position: 'absolute' }} />
            {memory ? (
              <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(220px, 1fr))', gap: 12 }}>
                <div style={{ padding: 12, borderRadius: 8, border: '1px solid var(--glass-border)' }}>
                  <strong>{memory.questions.length} remembered exchange{memory.questions.length === 1 ? '' : 's'}</strong>
                  <div style={{ color: 'var(--text-secondary)', fontSize: 12, marginTop: 5 }}>
                    {memory.questions[0]?.sourceIds.length ?? 0} grounded sources in the latest answer
                  </div>
                </div>
                <div style={{ padding: 12, borderRadius: 8, border: '1px solid var(--glass-border)' }}>
                  <strong>{memory.actions.length} graph-backed action{memory.actions.length === 1 ? '' : 's'}</strong>
                  <div style={{ color: 'var(--text-secondary)', fontSize: 12, marginTop: 5 }}>
                    {memory.actions[0]?.targetId || 'No action recorded yet'}
                  </div>
                </div>
                <div style={{ padding: 12, borderRadius: 8, border: '1px solid var(--glass-border)' }}>
                  <strong>{memory.handoffs.length} agent handoff{memory.handoffs.length === 1 ? '' : 's'}</strong>
                  <div style={{ color: 'var(--text-secondary)', fontSize: 12, marginTop: 5 }}>
                    {memory.handoffs[0] ? `${memory.handoffs[0].fromAgent} → ${memory.handoffs[0].toAgent}` : 'Ready to coordinate'}
                  </div>
                </div>
              </div>
            ) : (
              <div style={{ color: 'var(--text-muted)' }}>Select “Recall session” to prove that the exchange is available from FalkorDB.</div>
            )}
          </Section>
        </>
      )}

      {!answer && !busy && (
        <div style={{ display: 'grid', placeItems: 'center', minHeight: 90, color: 'var(--text-muted)', gap: 8 }}>
          <Building2 size={24} /> Prepare the demo company, then ask the example question.
        </div>
      )}
    </div>
  );
}
