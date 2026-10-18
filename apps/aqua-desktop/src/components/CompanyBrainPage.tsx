import { useEffect, useMemo, useRef, useState, type FormEvent } from 'react';
import {
  ArrowRight,
  CheckCircle2,
  Database,
  History,
  Play,
  Send,
  ShieldCheck,
  Sparkles,
} from 'lucide-react';
import { falkorApi, type CompanyBrainAnswer, type CompanyMemorySnapshot } from '../api/falkorApi';
import { PageHeader, Pill, Section } from './ui';
import { GraphVisualizer, type VisualNode, type VisualEdge } from './GraphVisualizer';
import { GraphAnalyticsChart, type EntityStat } from './GraphAnalyticsChart';

const PRESET_SCENARIOS = [
  {
    track: 'Tracks 1 & 3',
    title: 'Incident Triage & Decision Trace',
    question:
      'Who should handle the OAuth production incident, what earlier decision affects the fix, and which sources support the answer?',
    description: 'Multi-hop reasoning connecting Incident -> Project -> Owner -> Architecture Decision (DEC-014).',
  },
  {
    track: 'Track 3',
    title: 'Ownership & Dependency Mapping',
    question:
      'Who owns the Identity Platform, which team does it belong to, and what external system does it depend on?',
    description: 'Traces ownership hierarchy from Person to Team to Company and System dependencies.',
  },
  {
    track: 'Track 1 Algorithm',
    title: 'Who Knows About This System',
    question:
      'Who knows about the OAuth Gateway system and has worked on projects connected to it?',
    description: 'Exact WeMakeDevs Cypher traversal pattern matching skills and system knowledge.',
  },
];

// Initial seeded Company Brain graph structure
const SEEDED_COMPANY_NODES: VisualNode[] = [
  { id: 'company_acme', label: 'Company', title: 'Acme Cloud', properties: { active: true, domain: 'cloud.internal' }, x: 400, y: 70 },
  { id: 'team_identity', label: 'Team', title: 'Identity Engineering', properties: { active: true }, x: 400, y: 140 },
  { id: 'person_priya', label: 'Person', title: 'Priya Raman', sub: 'Staff Engineer', properties: { role: 'Staff Engineer', active: true }, x: 260, y: 190 },
  { id: 'project_identity', label: 'Project', title: 'Identity Platform', properties: { status: 'active' }, x: 400, y: 220 },
  { id: 'system_oauth_gateway', label: 'System', title: 'OAuth Gateway', properties: { status: 'active' }, x: 540, y: 190 },
  { id: 'INC-1042', label: 'Ticket', title: 'INC-1042 (P1 Incident)', sub: 'OAuth incident', properties: { priority: 'P1', status: 'open' }, x: 250, y: 310 },
  { id: 'DEC-014', label: 'Decision', title: 'DEC-014: Staged PKCE', sub: 'Architecture Decision', properties: { decided_at: '2026-10-15' }, x: 400, y: 330 },
  { id: 'SRC-ADR-014', label: 'Document', title: 'ADR-014: PKCE Migration', sub: 'company://docs/ADR-014', properties: { kind: 'document' }, x: 550, y: 290 },
  { id: 'SRC-CHAT-221', label: 'Chat', title: 'Slack #incident-221', sub: 'company://chat/identity/221', properties: { kind: 'chat' }, x: 550, y: 360 },
  { id: 'agent_aqua', label: 'Agent', title: 'Aqua Company Brain', sub: 'company_brain', properties: { role: 'company_brain' }, x: 120, y: 240 },
  { id: 'agent_incident', label: 'Agent', title: 'Incident Response Agent', sub: 'incident_response', properties: { role: 'incident_response' }, x: 120, y: 340 },
];

const SEEDED_COMPANY_EDGES: VisualEdge[] = [
  { source: 'person_priya', target: 'team_identity', label: 'MEMBER_OF' },
  { source: 'team_identity', target: 'company_acme', label: 'PART_OF' },
  { source: 'person_priya', target: 'project_identity', label: 'OWNS' },
  { source: 'INC-1042', target: 'project_identity', label: 'BELONGS_TO' },
  { source: 'project_identity', target: 'system_oauth_gateway', label: 'DEPENDS_ON' },
  { source: 'DEC-014', target: 'project_identity', label: 'AFFECTS' },
  { source: 'DEC-014', target: 'person_priya', label: 'DECIDED_BY' },
  { source: 'DEC-014', target: 'SRC-ADR-014', label: 'SUPPORTED_BY' },
  { source: 'DEC-014', target: 'SRC-CHAT-221', label: 'SUPPORTED_BY' },
  { source: 'INC-1042', target: 'SRC-CHAT-221', label: 'SUPPORTED_BY' },
  { source: 'agent_aqua', target: 'INC-1042', label: 'TRIAGED' },
  { source: 'agent_aqua', target: 'agent_incident', label: 'HANDOFF' },
];

const COMPANY_ENTITY_STATS: EntityStat[] = [
  { type: 'People & Teams', count: 2, color: '#34d399', description: 'Owner and engineering team' },
  { type: 'Projects & Systems', count: 2, color: '#a78bfa', description: 'Platform and dependency' },
  { type: 'Tickets', count: 1, color: '#fb923c', description: 'Production incident' },
  { type: 'Decisions', count: 1, color: '#facc15', description: 'Architecture decision' },
  { type: 'Sources', count: 2, color: '#f472b6', description: 'Document and chat evidence' },
  { type: 'Agents', count: 2, color: '#38bdf8', description: 'Company and incident agents' },
  { type: 'Companies', count: 1, color: '#2dd4bf', description: 'Tenant organization' },
];

export function CompanyBrainPage() {
  const [question, setQuestion] = useState(PRESET_SCENARIOS[0].question);
  const [answer, setAnswer] = useState<CompanyBrainAnswer | null>(null);
  const [busy, setBusy] = useState(false);
  const [seeding, setSeeding] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [completedAction, setCompletedAction] = useState<string | null>(null);
  const [memory, setMemory] = useState<CompanyMemorySnapshot | null>(null);
  const [activeScenarioIdx, setActiveScenarioIdx] = useState(0);
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
        const asked = await falkorApi.askCompanyBrain(PRESET_SCENARIOS[0].question);
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
      setNotice(result.message || 'Company Brain dataset is ready in FalkorDB.');
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSeeding(false);
    }
  };

  const ask = async (event?: FormEvent, queryToAsk?: string) => {
    if (event) event.preventDefault();
    const query = (queryToAsk || question).trim();
    if (!query) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    setCompletedAction(null);
    try {
      const res = await falkorApi.askCompanyBrain(query);
      setAnswer(res);
      setNotice(`Grounded answer generated via FalkorDB (${res.paths.length} multi-hop paths traversed).`);
    } catch (e) {
      setAnswer(null);
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const selectScenario = (idx: number) => {
    setActiveScenarioIdx(idx);
    const scenario = PRESET_SCENARIOS[idx];
    setQuestion(scenario.question);
    void ask(undefined, scenario.question);
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
      const mem = await falkorApi.recallCompanyMemory();
      setMemory(mem);
      setNotice('Cross-session memory successfully recalled directly from FalkorDB.');
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
        `Investigate ${answer.facts.ticketId} using ${answer.facts.decisionId} and cited graph sources.`,
      );
      setNotice(result.message);
      setMemory(await falkorApi.recallCompanyMemory());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  // Convert answer paths to high-visibility highlight paths
  const activeHighlightPaths = useMemo(() => {
    if (!answer?.paths) return [];
    return answer.paths.map((p) =>
      p.map((nodeTitle) => {
        const found = SEEDED_COMPANY_NODES.find(
          (n) => n.title.toLowerCase().includes(nodeTitle.toLowerCase()) || n.id === nodeTitle,
        );
        return found ? found.id : nodeTitle;
      }),
    );
  }, [answer]);

  return (
    <div className="page" style={{ display: 'flex', flexDirection: 'column', gap: 24, maxWidth: 1180, margin: '0 auto' }}>
      {/* Hackathon Badge & Page Header */}
      <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          <span
            style={{
              padding: '4px 10px',
              borderRadius: 6,
              background: 'rgba(60, 203, 255, 0.12)',
              border: '1px solid rgba(60, 203, 255, 0.3)',
              color: 'var(--accent-cyan)',
              fontSize: 11,
              fontWeight: 700,
              letterSpacing: '0.04em',
              textTransform: 'uppercase',
            }}
          >
            WeMakeDevs Hackathon · FalkorDB Powered
          </span>
          <span style={{ fontSize: 12, color: 'var(--text-muted)' }}>
            Track 1 (Connected Data) · Track 2 (Memory & Coordination) · Track 3 (Company Brain)
          </span>
        </div>

        <PageHeader
          title="Company Brain Agent"
          subtitle="Autonomous agent grounded in FalkorDB: connects people, teams, systems, tickets, decisions, and chat records. Every action is gated by graph relationship proofs."
          actions={
            <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
              <button
                className="btn-ghost"
                onClick={() => void seed()}
                disabled={seeding}
                style={{ display: 'flex', gap: 7, alignItems: 'center', padding: '7px 12px', fontSize: 13 }}
              >
                <Database size={15} /> {seeding ? 'Seeding Graph…' : 'Seed FalkorDB Graph'}
              </button>
              <Pill tone="ok" label="Database" value="FalkorDB Native" />
            </div>
          }
        />
      </div>

      {/* Notifications / Alerts */}
      {(error || notice) && (
        <div
          role={error ? 'alert' : 'status'}
          style={{
            padding: '14px 16px',
            borderRadius: 12,
            border: `1px solid ${error ? 'rgba(239,68,68,.35)' : 'rgba(16,185,129,.35)'}`,
            background: error ? 'rgba(239,68,68,.08)' : 'rgba(16,185,129,.08)',
            color: error ? '#f87171' : '#34d399',
            fontSize: 13.5,
            display: 'flex',
            alignItems: 'center',
            gap: 10,
          }}
        >
          <Sparkles size={16} />
          <span>{error || notice}</span>
        </div>
      )}

      {/* Quick Track Scenario Cards */}
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(280px, 1fr))', gap: 12 }}>
        {PRESET_SCENARIOS.map((sc, i) => {
          const isActive = activeScenarioIdx === i;
          return (
            <button
              key={sc.title}
              onClick={() => selectScenario(i)}
              style={{
                textAlign: 'left',
                padding: '14px 16px',
                borderRadius: 12,
                border: `1px solid ${isActive ? 'var(--accent-cyan)' : 'var(--glass-border)'}`,
                background: isActive ? 'rgba(60, 203, 255, 0.08)' : 'rgba(255, 255, 255, 0.02)',
                cursor: 'pointer',
                display: 'flex',
                flexDirection: 'column',
                gap: 6,
                transition: 'all 0.2s var(--ease-out)',
              }}
            >
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                <span
                  style={{
                    fontSize: 10.5,
                    fontWeight: 700,
                    color: isActive ? 'var(--accent-cyan)' : 'var(--text-muted)',
                    textTransform: 'uppercase',
                    letterSpacing: '0.05em',
                  }}
                >
                  {sc.track}
                </span>
                {isActive && <CheckCircle2 size={14} color="var(--accent-cyan)" />}
              </div>
              <strong style={{ fontSize: 13, color: 'var(--text-primary)' }}>{sc.title}</strong>
              <p style={{ margin: 0, fontSize: 11.5, color: 'var(--text-secondary)', lineHeight: 1.4 }}>
                {sc.description}
              </p>
            </button>
          );
        })}
      </div>

      {/* Interactive Visual Graph Canvas Component */}
      <GraphVisualizer
        nodes={SEEDED_COMPANY_NODES}
        edges={SEEDED_COMPANY_EDGES}
        highlightPaths={activeHighlightPaths}
        height={380}
        title="Live FalkorDB Organizational Knowledge Graph"
        subtitle="Nodes, teams, decisions, tickets, and multi-hop paths active in memory."
      />

      {/* Ask Question Console */}
      <Section
        title="Ask the Connected Graph Agent"
        hint="The agent determines retrieval intent, executes multi-hop traversals in FalkorDB, and generates an explainable answer."
      >
        <form onSubmit={ask} style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
          <textarea
            rows={3}
            value={question}
            onChange={(event) => setQuestion(event.target.value)}
            aria-label="Company question"
            placeholder="Ask about systems, team ownership, incident fixes, or decisions..."
            style={{
              width: '100%',
              padding: 14,
              borderRadius: 10,
              resize: 'vertical',
              color: 'var(--text-primary)',
              background: 'rgba(0,0,0,.35)',
              border: '1px solid var(--glass-border)',
              font: 'inherit',
              fontSize: 13.5,
              lineHeight: 1.6,
              outline: 'none',
            }}
          />
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: 10 }}>
            <button
              className="btn-primary"
              disabled={busy || !question.trim()}
              style={{
                display: 'flex',
                gap: 8,
                alignItems: 'center',
                padding: '9px 20px',
                background: 'linear-gradient(135deg, var(--accent-cyan), var(--accent-purple))',
                border: 'none',
                borderRadius: 8,
                color: '#fff',
                fontWeight: 600,
                fontSize: 13.5,
                cursor: 'pointer',
              }}
            >
              <Play size={15} /> {busy ? 'Traversing FalkorDB Graph…' : 'Query Company Brain'}
            </button>

            <span style={{ fontSize: 12, color: 'var(--text-muted)' }}>
              Tenant Scope: <strong style={{ color: 'var(--text-secondary)' }}>demo</strong> (Multi-graph isolated)
            </span>
          </div>
        </form>
      </Section>

      {/* Answer & Provenance Details */}
      {answer && (
        <>
          {/* Grounded Answer Card */}
          <Section
            title="Graph-Grounded Answer"
            hint={`Intent: ${answer.intent} · Interaction ID: ${answer.interactionId} (Persisted in FalkorDB)`}
            actions={<Pill tone="ok" label="Provenance" value={`${answer.sources.length} Verified Sources`} />}
          >
            <div
              style={{
                fontSize: 15,
                lineHeight: 1.75,
                color: 'var(--text-primary)',
                background: 'rgba(255, 255, 255, 0.02)',
                padding: '16px 20px',
                borderRadius: 10,
                border: '1px solid var(--glass-border)',
              }}
            >
              {answer.answer}
            </div>
          </Section>

          {/* Reasoning Paths & Source Citations */}
          <div style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1.2fr) minmax(280px, .8fr)', gap: 20 }}>
            {/* Multi-Hop Traversal Proof */}
            <Section title="Multi-Hop Relationship Proofs (Track 1 & 3)" hint="The exact relationship hops followed in FalkorDB.">
              <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
                {answer.paths.map((path, index) => (
                  <div
                    key={`${path.join('-')}-${index}`}
                    style={{
                      display: 'flex',
                      alignItems: 'center',
                      gap: 8,
                      flexWrap: 'wrap',
                      fontSize: 12.5,
                      padding: '10px 14px',
                      borderRadius: 8,
                      background: 'rgba(0,0,0,0.25)',
                      border: '1px solid var(--glass-border)',
                    }}
                  >
                    {path.map((part, partIndex) => (
                      <span key={`${part}-${partIndex}`} style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
                        {partIndex > 0 && <ArrowRight size={13} color="var(--accent-cyan)" />}
                        <span
                          style={{
                            padding: '4px 9px',
                            borderRadius: 6,
                            fontWeight: 600,
                            background: partIndex % 2 ? 'rgba(60, 203, 255, 0.12)' : 'rgba(255,255,255,0.06)',
                            color: partIndex % 2 ? 'var(--accent-cyan)' : 'var(--text-primary)',
                          }}
                        >
                          {part}
                        </span>
                      </span>
                    ))}
                  </div>
                ))}
              </div>
            </Section>

            {/* Cited Primary Sources */}
            <Section title="Primary Graph Sources" hint="Every claim anchors to an ADR document, ticket, or chat.">
              <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
                {answer.sources.map((source) => (
                  <div
                    key={source.id}
                    style={{
                      padding: '12px 14px',
                      borderRadius: 8,
                      border: '1px solid var(--glass-border)',
                      background: 'rgba(0,0,0,0.2)',
                    }}
                  >
                    <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
                      <strong style={{ fontSize: 13, color: '#fff' }}>{source.title}</strong>
                      <span
                        style={{
                          fontSize: 10,
                          fontWeight: 700,
                          textTransform: 'uppercase',
                          padding: '2px 6px',
                          borderRadius: 4,
                          background: 'rgba(244, 114, 182, 0.15)',
                          color: '#f472b6',
                        }}
                      >
                        {source.kind}
                      </span>
                    </div>
                    <div style={{ color: 'var(--text-muted)', fontSize: 11.5, marginTop: 4, fontFamily: 'var(--font-mono)' }}>
                      {source.uri}
                    </div>
                  </div>
                ))}
              </div>
            </Section>
          </div>

          {/* Action Execution Gated by Graph Preconditions (Track 1) */}
          {answer.action && (
            <Section
              title="Track 1: Act on Connected Data (Graph Precondition Verified)"
              hint="The assignment is authorized only because FalkorDB verified Priya Raman owns Identity Platform."
              actions={<Pill tone={completedAction ? 'ok' : 'attention'} label={completedAction ? 'Action Recorded' : 'Approval Pending'} />}
            >
              <div
                style={{
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'space-between',
                  gap: 16,
                  flexWrap: 'wrap',
                  padding: '14px 18px',
                  borderRadius: 10,
                  background: 'rgba(0,0,0,0.25)',
                  border: '1px solid var(--glass-border)',
                }}
              >
                <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
                  {completedAction ? (
                    <CheckCircle2 size={22} color="var(--state-ok)" />
                  ) : (
                    <ShieldCheck size={22} color="var(--state-attention)" />
                  )}
                  <div>
                    <div style={{ fontSize: 14, fontWeight: 700, color: 'var(--text-primary)' }}>
                      {answer.action.label}
                    </div>
                    <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 2 }}>
                      Precondition: <code style={{ color: 'var(--accent-emerald)' }}>MATCH (p:Person &#123;id:'{answer.action.assigneeId}'&#125;)-[:OWNS]-&gt;(:Project)&lt;-[:BELONGS_TO]-(:Ticket &#123;id:'{answer.action.ticketId}'&#125;)</code>
                    </div>
                  </div>
                </div>

                {!completedAction ? (
                  <button
                    className="btn-primary"
                    onClick={() => void executeAction()}
                    disabled={busy}
                    style={{
                      padding: '8px 18px',
                      background: 'var(--accent-emerald)',
                      color: '#022c22',
                      fontWeight: 700,
                      borderRadius: 8,
                      border: 'none',
                      cursor: 'pointer',
                    }}
                  >
                    Execute Verified Action
                  </button>
                ) : (
                  <span style={{ fontSize: 12, color: 'var(--accent-emerald)', fontWeight: 600 }}>
                    Action ID: {completedAction} written to FalkorDB
                  </span>
                )}
              </div>
            </Section>
          )}

          {/* Memory & Multi-Agent Coordination (Track 2) */}
          <Section
            title="Track 2: Agent Memory & Inter-Agent Coordination"
            hint="Cross-session persistence and multi-agent handoff through the FalkorDB tenant graph."
            actions={
              <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                <button
                  className="btn-ghost"
                  onClick={() => void recallMemory()}
                  disabled={busy}
                  style={{ display: 'flex', gap: 7, alignItems: 'center', fontSize: 12.5 }}
                >
                  <History size={14} /> Recall Cross-Session Memory
                </button>
                <button
                  className="btn-primary"
                  onClick={() => void handoff()}
                  disabled={busy}
                  style={{
                    display: 'flex',
                    gap: 7,
                    alignItems: 'center',
                    fontSize: 12.5,
                    background: 'linear-gradient(135deg, #a78bfa, #ec4899)',
                    border: 'none',
                    borderRadius: 6,
                    color: '#fff',
                  }}
                >
                  <Send size={14} /> Coordinate Incident Agent Handoff
                </button>
              </div>
            }
          >
            <span data-demo-memory style={{ position: 'absolute' }} />
            {memory ? (
              <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(220px, 1fr))', gap: 14 }}>
                <div style={{ padding: 14, borderRadius: 10, border: '1px solid var(--glass-border)', background: 'rgba(0,0,0,0.2)' }}>
                  <div style={{ fontSize: 11, color: 'var(--text-muted)', textTransform: 'uppercase' }}>Session Questions</div>
                  <strong style={{ fontSize: 16, color: '#fff', marginTop: 4, display: 'block' }}>
                    {memory.questions.length} Remembered Exchange{memory.questions.length === 1 ? '' : 's'}
                  </strong>
                  <div style={{ color: 'var(--accent-cyan)', fontSize: 12, marginTop: 4 }}>
                    {memory.questions[0]?.sourceIds.length ?? 0} grounded sources recalled
                  </div>
                </div>

                <div style={{ padding: 14, borderRadius: 10, border: '1px solid var(--glass-border)', background: 'rgba(0,0,0,0.2)' }}>
                  <div style={{ fontSize: 11, color: 'var(--text-muted)', textTransform: 'uppercase' }}>Graph Actions</div>
                  <strong style={{ fontSize: 16, color: '#fff', marginTop: 4, display: 'block' }}>
                    {memory.actions.length} Executed Action{memory.actions.length === 1 ? '' : 's'}
                  </strong>
                  <div style={{ color: 'var(--accent-emerald)', fontSize: 12, marginTop: 4 }}>
                    {memory.actions[0]?.targetId || 'Action logged to graph'}
                  </div>
                </div>

                <div style={{ padding: 14, borderRadius: 10, border: '1px solid var(--glass-border)', background: 'rgba(0,0,0,0.2)' }}>
                  <div style={{ fontSize: 11, color: 'var(--text-muted)', textTransform: 'uppercase' }}>Agent Handoffs</div>
                  <strong style={{ fontSize: 16, color: '#fff', marginTop: 4, display: 'block' }}>
                    {memory.handoffs.length} Active Handoff{memory.handoffs.length === 1 ? '' : 's'}
                  </strong>
                  <div style={{ color: '#fb7185', fontSize: 12, marginTop: 4 }}>
                    {memory.handoffs[0] ? `${memory.handoffs[0].fromAgent} → ${memory.handoffs[0].toAgent}` : 'Ready for coordination'}
                  </div>
                </div>
              </div>
            ) : (
              <div style={{ color: 'var(--text-muted)', fontSize: 13 }}>
                Click “Recall Cross-Session Memory” to prove persistent state recall directly from FalkorDB.
              </div>
            )}
          </Section>
        </>
      )}

      {/* Analytics Chart & Compliance Matrix */}
      <GraphAnalyticsChart
        nodeCount={SEEDED_COMPANY_NODES.length}
        edgeCount={SEEDED_COMPANY_EDGES.length}
        latencyMs={2}
        tenantId="demo"
        entityStats={COMPANY_ENTITY_STATS}
      />
    </div>
  );
}
