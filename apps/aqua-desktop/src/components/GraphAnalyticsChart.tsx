import {
  CheckCircle2,
  Sparkles,
} from 'lucide-react';

export interface EntityStat {
  type: string;
  count: number;
  color: string;
  description: string;
}

interface Props {
  nodeCount: number;
  edgeCount: number;
  latencyMs?: number;
  tenantId?: string;
  entityStats?: EntityStat[];
}

const DEFAULT_ENTITY_STATS: EntityStat[] = [
  { type: 'People & Teams', count: 2, color: '#34d399', description: 'Engineers, Owners, Members' },
  { type: 'Projects & Systems', count: 2, color: '#a78bfa', description: 'Identity Platform, OAuth Gateway' },
  { type: 'Tickets & Incidents', count: 1, color: '#fb923c', description: 'INC-1042 Production Incident' },
  { type: 'Decisions & ADRs', count: 2, color: '#facc15', description: 'DEC-014 PKCE Architecture Decision' },
  { type: 'Sources & Chats', count: 2, color: '#f472b6', description: 'ADR-014 Doc, Slack Incident Handoff' },
  { type: 'Memory & Tasks', count: 4, color: '#38bdf8', description: 'Cross-session Questions & Answers' },
];

export function GraphAnalyticsChart({
  nodeCount,
  edgeCount,
  latencyMs = 2,
  tenantId = 'demo',
  entityStats = DEFAULT_ENTITY_STATS,
}: Props) {
  const maxCount = Math.max(...entityStats.map((s) => s.count), 1);

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
      {/* 3 Hackathon Tracks Compliance Banner */}
      <div
        className="glass-panel"
        style={{
          padding: '16px 20px',
          borderRadius: 14,
          background: 'linear-gradient(135deg, rgba(6, 182, 212, 0.08) 0%, rgba(139, 92, 246, 0.08) 100%)',
          border: '1px solid rgba(60, 203, 255, 0.2)',
          display: 'flex',
          flexDirection: 'column',
          gap: 12,
        }}
      >
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', flexWrap: 'wrap', gap: 8 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <Sparkles size={16} color="var(--accent-cyan)" />
            <span style={{ fontSize: 13, fontWeight: 700, color: 'var(--text-primary)', letterSpacing: '0.02em' }}>
              WeMakeDevs & FalkorDB Hackathon: 3-Track Compliance Architecture
            </span>
          </div>
          <span
            style={{
              padding: '3px 10px',
              borderRadius: 20,
              fontSize: 11,
              fontWeight: 700,
              background: 'rgba(52, 211, 153, 0.15)',
              border: '1px solid rgba(52, 211, 153, 0.4)',
              color: '#34d399',
            }}
          >
            All 3 Tracks Qualified
          </span>
        </div>

        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(240px, 1fr))', gap: 12 }}>
          {/* Track 1 */}
          <div
            style={{
              padding: '12px 14px',
              borderRadius: 10,
              background: 'rgba(0,0,0,0.3)',
              border: '1px solid var(--glass-border)',
              display: 'flex',
              flexDirection: 'column',
              gap: 6,
            }}
          >
            <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
              <span style={{ fontSize: 14 }}>🕵️</span>
              <strong style={{ fontSize: 12.5, color: '#3ccbff' }}>Track 1: Acts on Connected Data</strong>
            </div>
            <p style={{ margin: 0, fontSize: 11.5, color: 'var(--text-secondary)', lineHeight: 1.4 }}>
              Multi-hop Cypher reasoning over interconnected systems and graph-verified ticket action execution.
            </p>
          </div>

          {/* Track 2 */}
          <div
            style={{
              padding: '12px 14px',
              borderRadius: 10,
              background: 'rgba(0,0,0,0.3)',
              border: '1px solid var(--glass-border)',
              display: 'flex',
              flexDirection: 'column',
              gap: 6,
            }}
          >
            <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
              <span style={{ fontSize: 14 }}>🤖</span>
              <strong style={{ fontSize: 12.5, color: '#a78bfa' }}>Track 2: Memory & Coordination</strong>
            </div>
            <p style={{ margin: 0, fontSize: 11.5, color: 'var(--text-secondary)', lineHeight: 1.4 }}>
              Persistent cross-session memory with tenant isolation (<code style={{ color: '#38bdf8' }}>tenantId: {tenantId}</code>) and seamless agent-to-agent handoffs.
            </p>
          </div>

          {/* Track 3 */}
          <div
            style={{
              padding: '12px 14px',
              borderRadius: 10,
              background: 'rgba(0,0,0,0.3)',
              border: '1px solid var(--glass-border)',
              display: 'flex',
              flexDirection: 'column',
              gap: 6,
            }}
          >
            <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
              <span style={{ fontSize: 14 }}>🧠</span>
              <strong style={{ fontSize: 12.5, color: '#facc15' }}>Track 3: Company Brain</strong>
            </div>
            <p style={{ margin: 0, fontSize: 11.5, color: 'var(--text-secondary)', lineHeight: 1.4 }}>
              Unified organizational context tracing ownership, decisions (ADR-014), tickets, and chat handoffs.
            </p>
          </div>
        </div>
      </div>

      {/* Realtime Graph Metrics & Entity Breakdown */}
      <div style={{ display: 'grid', gridTemplateColumns: 'minmax(280px, 1fr) minmax(320px, 1.4fr)', gap: 16 }}>
        {/* Core Metrics Card */}
        <div
          className="glass-panel"
          style={{
            padding: 18,
            borderRadius: 14,
            border: '1px solid var(--glass-border)',
            display: 'flex',
            flexDirection: 'column',
            justifyContent: 'space-between',
            gap: 14,
          }}
        >
          <div>
            <div style={{ fontSize: 12, fontWeight: 700, color: 'var(--text-secondary)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>
              FalkorDB Context Engine
            </div>
            <div style={{ fontSize: 18, fontWeight: 700, color: '#fff', marginTop: 4 }}>
              Multi-Tenant Graph Topology
            </div>
          </div>

          <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 10 }}>
            <div style={{ padding: 12, borderRadius: 8, background: 'rgba(0,0,0,0.25)', border: '1px solid var(--glass-border)' }}>
              <div style={{ fontSize: 11, color: 'var(--text-muted)' }}>Graph Nodes</div>
              <div style={{ fontSize: 20, fontWeight: 800, color: 'var(--accent-cyan)', marginTop: 2 }}>
                {nodeCount}
              </div>
            </div>
            <div style={{ padding: 12, borderRadius: 8, background: 'rgba(0,0,0,0.25)', border: '1px solid var(--glass-border)' }}>
              <div style={{ fontSize: 11, color: 'var(--text-muted)' }}>Relationships</div>
              <div style={{ fontSize: 20, fontWeight: 800, color: 'var(--accent-purple)', marginTop: 2 }}>
                {edgeCount}
              </div>
            </div>
            <div style={{ padding: 12, borderRadius: 8, background: 'rgba(0,0,0,0.25)', border: '1px solid var(--glass-border)' }}>
              <div style={{ fontSize: 11, color: 'var(--text-muted)' }}>Cypher Latency</div>
              <div style={{ fontSize: 20, fontWeight: 800, color: 'var(--accent-emerald)', marginTop: 2 }}>
                {latencyMs} <span style={{ fontSize: 12, fontWeight: 500 }}>ms</span>
              </div>
            </div>
            <div style={{ padding: 12, borderRadius: 8, background: 'rgba(0,0,0,0.25)', border: '1px solid var(--glass-border)' }}>
              <div style={{ fontSize: 11, color: 'var(--text-muted)' }}>Tenant Scope</div>
              <div style={{ fontSize: 16, fontWeight: 800, color: '#fb923c', marginTop: 4, fontFamily: 'var(--font-mono)' }}>
                {tenantId}
              </div>
            </div>
          </div>

          <div style={{ fontSize: 11.5, color: 'var(--text-muted)', display: 'flex', alignItems: 'center', gap: 6 }}>
            <CheckCircle2 size={13} color="var(--accent-emerald)" />
            Zero synthetic hallucination: every action is gated by Cypher path proofs.
          </div>
        </div>

        {/* Entity Distribution Bar Chart */}
        <div
          className="glass-panel"
          style={{
            padding: 18,
            borderRadius: 14,
            border: '1px solid var(--glass-border)',
            display: 'flex',
            flexDirection: 'column',
            gap: 12,
          }}
        >
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
            <div style={{ fontSize: 12, fontWeight: 700, color: 'var(--text-secondary)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>
              Connected Entity Distribution
            </div>
            <span style={{ fontSize: 11, color: 'var(--text-muted)', fontFamily: 'var(--font-mono)' }}>
              Total: {nodeCount} nodes
            </span>
          </div>

          <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
            {entityStats.map((stat) => {
              const pct = Math.max(12, Math.round((stat.count / maxCount) * 100));
              return (
                <div key={stat.type} style={{ display: 'flex', flexDirection: 'column', gap: 3 }}>
                  <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 11.5 }}>
                    <span style={{ fontWeight: 600, color: 'var(--text-primary)' }}>{stat.type}</span>
                    <span style={{ color: stat.color, fontWeight: 700, fontFamily: 'var(--font-mono)' }}>
                      {stat.count} ({stat.description})
                    </span>
                  </div>
                  <div
                    style={{
                      height: 8,
                      width: '100%',
                      background: 'rgba(255, 255, 255, 0.05)',
                      borderRadius: 4,
                      overflow: 'hidden',
                    }}
                  >
                    <div
                      style={{
                        height: '100%',
                        width: `${pct}%`,
                        background: `linear-gradient(90deg, ${stat.color}88 0%, ${stat.color} 100%)`,
                        borderRadius: 4,
                        transition: 'width 0.6s var(--ease-out)',
                      }}
                    />
                  </div>
                </div>
              );
            })}
          </div>
        </div>
      </div>
    </div>
  );
}
