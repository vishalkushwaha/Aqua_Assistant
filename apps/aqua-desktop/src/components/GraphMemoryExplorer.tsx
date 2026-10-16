import { useCallback, useEffect, useState } from 'react';
import {
  Database,
  Play,
  RefreshCw,
  Settings,
  Sparkles,
} from 'lucide-react';
import { falkorApi, type FalkorQueryResult, type FalkorStatus, type GraphSnapshot } from '../api/falkorApi';
import { PageHeader, Pill, Section } from './ui';

const PRESET_QUERIES = [
  {
    name: 'Recall Top Experiences',
    cypher: 'MATCH (e:Experience)-[:USES]->(t:Tool) RETURN e.summary, e.confidence, t.name, t.permission_level ORDER BY e.confidence DESC LIMIT 10',
  },
  {
    name: 'Tool Prerequisite Chain',
    cypher: 'MATCH path = (s:Skill)-[:DEPENDS_ON*1..2]->(t:Tool) RETURN s.name, t.name, t.permission_level',
  },
  {
    name: 'Complete Memory Paths',
    cypher: 'MATCH path = (u:User)-[:REQUESTS]->(t:Task)-[:RECALLS]->(e:Experience)-[:USES]->(tool:Tool) RETURN path LIMIT 25',
  },
  {
    name: 'Count Entities by Label',
    cypher: 'MATCH (n) RETURN labels(n)[0] AS label, count(n) AS total ORDER BY total DESC',
  },
];

const NODE_COLORS: Record<string, string> = {
  User: '#3b82f6',
  Task: '#06b6d4',
  Experience: '#10b981',
  Tool: '#a855f7',
  Skill: '#f59e0b',
  Company: '#0ea5e9',
  Person: '#22c55e',
  Team: '#14b8a6',
  Project: '#8b5cf6',
  Ticket: '#f97316',
  Decision: '#eab308',
  Source: '#ec4899',
  Document: '#ec4899',
  Chat: '#f43f5e',
  Default: '#64748b',
};

export function GraphMemoryExplorer() {
  const [status, setStatus] = useState<FalkorStatus | null>(null);
  const [snapshot, setSnapshot] = useState<GraphSnapshot>({ nodes: [], edges: [] });
  const [selectedNode, setSelectedNode] = useState<any | null>(null);
  const [cypherQuery, setCypherQuery] = useState(PRESET_QUERIES[0].cypher);
  const [queryResult, setQueryResult] = useState<FalkorQueryResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [executing, setExecuting] = useState(false);
  const [seeding, setSeeding] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [showConfig, setShowConfig] = useState(false);
  const [configDraft, setConfigDraft] = useState({
    host: '127.0.0.1',
    port: 6379,
    username: '',
    password: '',
    graphName: 'AquaCompanyBrain',
  });

  const loadGraphData = useCallback(async () => {
    setLoading(true);
    try {
      const [s, snap] = await Promise.all([
        falkorApi.status().catch((e) => ({
          connected: false,
          host: configDraft.host,
          port: configDraft.port,
          username: configDraft.username,
          graphName: configDraft.graphName,
          nodeCount: 0,
          edgeCount: 0,
          latencyMs: 0,
          error: e?.message || String(e),
        })),
        falkorApi.getSnapshot().catch(() => ({ nodes: [], edges: [] })),
      ]);
      setStatus(s);
      setSnapshot(snap);
      if (s.connected && snap.nodes.length === 0) {
        setNotice('Connected to FalkorDB Cloud. Click "Seed Agent Graph" to populate sample memory data.');
      }
    } finally {
      setLoading(false);
    }
  }, [configDraft.host, configDraft.port, configDraft.username, configDraft.graphName]);

  useEffect(() => {
    void loadGraphData();
  }, [loadGraphData]);

  const handleRunCypher = async (queryText?: string) => {
    const toRun = queryText || cypherQuery;
    if (!toRun.trim()) return;
    setExecuting(true);
    setNotice(null);
    try {
      const res = await falkorApi.query(toRun);
      setQueryResult(res);
      if (toRun.includes('CREATE') || toRun.includes('DELETE') || toRun.includes('SET')) {
        await loadGraphData();
      }
    } catch (e: any) {
      setQueryResult({ success: false, data: [], error: e?.message || String(e) });
    } finally {
      setExecuting(false);
    }
  };

  const handleSeedData = async () => {
    setSeeding(true);
    setNotice(null);
    try {
      await falkorApi.seedDemoData();
      setNotice('Agent Memory Graph seeded with interconnected Tasks, Experiences, Skills, and Tools!');
      await loadGraphData();
    } catch (e: any) {
      setNotice(`Seeding failed: ${e?.message || String(e)}`);
    } finally {
      setSeeding(false);
    }
  };

  const handleSaveConfig = async (e: React.FormEvent) => {
    e.preventDefault();
    setLoading(true);
    try {
      await falkorApi.updateConfig(configDraft);
      setShowConfig(false);
      setNotice('FalkorDB Cloud configuration updated. Reconnecting...');
      await loadGraphData();
    } catch (err: any) {
      setNotice(`Failed to save config: ${err?.message}`);
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="page" style={{ display: 'flex', flexDirection: 'column', gap: 24 }}>
      <PageHeader
        title="Agent Memory Graph (FalkorDB)"
        subtitle="Primary knowledge graph database powering associative memory recall and skill coordination."
        actions={
          <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
            <button
              className="btn-ghost"
              onClick={() => void loadGraphData()}
              disabled={loading}
              style={{ display: 'flex', alignItems: 'center', gap: 6 }}
            >
              <RefreshCw size={14} className={loading ? 'spin' : ''} /> Refresh
            </button>
            <button
              className="btn-ghost"
              onClick={() => setShowConfig(!showConfig)}
              style={{ display: 'flex', alignItems: 'center', gap: 6 }}
            >
              <Settings size={14} /> Cloud Settings
            </button>
          </div>
        }
      />

      {/* Cloud Connectivity Status Card */}
      <div
        className="glass-panel"
        style={{
          padding: '24px 28px',
          borderRadius: 16,
          background: status?.connected
            ? 'linear-gradient(135deg, rgba(16, 185, 129, 0.08) 0%, rgba(6, 182, 212, 0.08) 100%)'
            : 'linear-gradient(135deg, rgba(239, 68, 68, 0.08) 0%, rgba(245, 158, 11, 0.08) 100%)',
          border: `1px solid ${status?.connected ? 'rgba(16, 185, 129, 0.25)' : 'rgba(239, 68, 68, 0.3)'}`,
          display: 'flex',
          flexDirection: 'column',
          gap: 16,
        }}
      >
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: 12 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
            <div style={{ width: 42, height: 42, borderRadius: 12, background: 'rgba(6, 182, 212, 0.15)', display: 'grid', placeItems: 'center' }}>
              <Database size={22} color="var(--accent-cyan)" />
            </div>
            <div>
              <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                <h3 style={{ margin: 0, fontSize: 18, fontWeight: 700 }}>FalkorDB Cloud Instance</h3>
                <Pill
                  tone={status?.connected ? 'ok' : 'problem'}
                  label={status?.connected ? 'Active & Connected' : status ? 'Disconnected' : 'Connecting'}
                />
              </div>
              <p style={{ margin: '4px 0 0 0', fontSize: 13, color: 'var(--text-secondary)' }}>
                Target: <strong style={{ color: 'var(--text-primary)' }}>{status?.host || configDraft.host}</strong> ({status?.graphName || configDraft.graphName})
              </p>
            </div>
          </div>

          <div style={{ display: 'flex', gap: 16, alignItems: 'center' }}>
            <div style={{ textAlign: 'right' }}>
              <div style={{ fontSize: 11, color: 'var(--text-secondary)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>Latency</div>
              <div style={{ fontSize: 18, fontWeight: 700, color: 'var(--accent-emerald)' }}>
                {status?.latencyMs ?? 0} <span style={{ fontSize: 12 }}>ms</span>
              </div>
            </div>
            <div style={{ textAlign: 'right' }}>
              <div style={{ fontSize: 11, color: 'var(--text-secondary)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>Graph Nodes</div>
              <div style={{ fontSize: 18, fontWeight: 700, color: 'var(--accent-cyan)' }}>
                {status?.nodeCount ?? snapshot.nodes.length}
              </div>
            </div>
            <div style={{ textAlign: 'right' }}>
              <div style={{ fontSize: 11, color: 'var(--text-secondary)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>Relationships</div>
              <div style={{ fontSize: 18, fontWeight: 700, color: 'var(--accent-purple)' }}>
                {status?.edgeCount ?? snapshot.edges.length}
              </div>
            </div>
          </div>
        </div>

        {status?.error && (
          <div style={{ padding: '10px 14px', borderRadius: 8, background: 'rgba(239, 68, 68, 0.1)', border: '1px solid rgba(239, 68, 68, 0.25)', fontSize: 13, color: '#f87171' }}>
            <strong>Connection note:</strong> {status.error}. Double check your FalkorDB Cloud endpoint in settings.
          </div>
        )}

        {notice && (
          <div style={{ padding: '10px 14px', borderRadius: 8, background: 'rgba(16, 185, 129, 0.1)', border: '1px solid rgba(16, 185, 129, 0.25)', fontSize: 13, color: '#10b981' }}>
            {notice}
          </div>
        )}
      </div>

      {/* Cloud Config Drawer */}
      {showConfig && (
        <form onSubmit={handleSaveConfig} className="glass-panel" style={{ padding: 20, borderRadius: 12, display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(200px, 1fr))', gap: 12 }}>
          <div>
            <label style={{ fontSize: 12, color: 'var(--text-secondary)' }}>Host Endpoint</label>
            <input
              style={{ width: '100%', padding: '8px 10px', background: 'rgba(0,0,0,0.3)', border: '1px solid var(--glass-border)', borderRadius: 6, color: '#fff', fontSize: 13, marginTop: 4 }}
              value={configDraft.host}
              onChange={(e) => setConfigDraft({ ...configDraft, host: e.target.value })}
            />
          </div>
          <div>
            <label style={{ fontSize: 12, color: 'var(--text-secondary)' }}>Port</label>
            <input
              type="number"
              style={{ width: '100%', padding: '8px 10px', background: 'rgba(0,0,0,0.3)', border: '1px solid var(--glass-border)', borderRadius: 6, color: '#fff', fontSize: 13, marginTop: 4 }}
              value={configDraft.port}
              onChange={(e) => setConfigDraft({ ...configDraft, port: Number(e.target.value) })}
            />
          </div>
          <div>
            <label style={{ fontSize: 12, color: 'var(--text-secondary)' }}>Username</label>
            <input
              style={{ width: '100%', padding: '8px 10px', background: 'rgba(0,0,0,0.3)', border: '1px solid var(--glass-border)', borderRadius: 6, color: '#fff', fontSize: 13, marginTop: 4 }}
              value={configDraft.username}
              onChange={(e) => setConfigDraft({ ...configDraft, username: e.target.value })}
            />
          </div>
          <div>
            <label style={{ fontSize: 12, color: 'var(--text-secondary)' }}>Password</label>
            <input
              type="password"
              style={{ width: '100%', padding: '8px 10px', background: 'rgba(0,0,0,0.3)', border: '1px solid var(--glass-border)', borderRadius: 6, color: '#fff', fontSize: 13, marginTop: 4 }}
              value={configDraft.password}
              onChange={(e) => setConfigDraft({ ...configDraft, password: e.target.value })}
            />
          </div>
          <div style={{ gridColumn: '1 / -1', display: 'flex', gap: 8, justifyContent: 'flex-end', marginTop: 8 }}>
            <button type="button" className="btn-ghost" onClick={() => setShowConfig(false)}>Cancel</button>
            <button type="submit" className="btn-primary" style={{ padding: '8px 18px', background: 'var(--accent-cyan)', color: '#000', fontWeight: 600, borderRadius: 6, border: 'none' }}>
              Save & Reconnect
            </button>
          </div>
        </form>
      )}

      {/* Main Grid: Visual Graph Canvas & Cypher Playground */}
      <div style={{ display: 'grid', gridTemplateColumns: '1.2fr 1fr', gap: 20 }}>
        {/* Visual Graph Canvas Card */}
        <Section
          title="Interactive Graph Canvas"
          hint="Nodes and relationships stored live in FalkorDB Cloud."
        >
          <div style={{ display: 'flex', gap: 8, marginBottom: 12, flexWrap: 'wrap' }}>
            {Object.entries(NODE_COLORS).map(([label, color]) => (
              <span key={label} style={{ display: 'inline-flex', alignItems: 'center', gap: 6, fontSize: 12, padding: '3px 8px', borderRadius: 6, background: 'rgba(255,255,255,0.03)', border: '1px solid var(--glass-border)' }}>
                <span style={{ width: 8, height: 8, borderRadius: '50%', background: color }} />
                <span>{label}</span>
              </span>
            ))}
            <button
              onClick={handleSeedData}
              disabled={seeding}
              style={{
                marginLeft: 'auto',
                padding: '4px 12px',
                borderRadius: 6,
                border: '1px solid rgba(6, 182, 212, 0.4)',
                background: 'rgba(6, 182, 212, 0.1)',
                color: 'var(--accent-cyan)',
                fontSize: 12,
                cursor: 'pointer',
                display: 'flex',
                alignItems: 'center',
                gap: 6,
              }}
            >
              <Sparkles size={13} /> {seeding ? 'Seeding...' : 'Seed Sample Agent Graph'}
            </button>
          </div>

          <div
            style={{
              height: 380,
              borderRadius: 12,
              background: 'rgba(0,0,0,0.4)',
              border: '1px solid var(--glass-border)',
              overflowY: 'auto',
              padding: 16,
              display: 'flex',
              flexDirection: 'column',
              gap: 12,
            }}
          >
            {snapshot.nodes.length === 0 ? (
              <div style={{ display: 'grid', placeItems: 'center', height: '100%', color: 'var(--text-muted)', fontSize: 13 }}>
                <div>No graph nodes loaded yet. Click "Seed Sample Agent Graph" above to populate!</div>
              </div>
            ) : (
              <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(180px, 1fr))', gap: 10 }}>
                {snapshot.nodes.map((n) => {
                  const label = n.labels[0] || 'Default';
                  const color = NODE_COLORS[label] || NODE_COLORS.Default;
                  const isSelected = selectedNode?.id === n.id;
                  const title = n.properties.name || n.properties.summary || n.properties.prompt || n.id;

                  return (
                    <div
                      key={n.id}
                      onClick={() => setSelectedNode(n)}
                      style={{
                        padding: 12,
                        borderRadius: 8,
                        background: isSelected ? 'rgba(6, 182, 212, 0.15)' : 'rgba(255,255,255,0.03)',
                        border: `1px solid ${isSelected ? 'var(--accent-cyan)' : 'var(--glass-border)'}`,
                        cursor: 'pointer',
                        display: 'flex',
                        flexDirection: 'column',
                        gap: 4,
                        transition: 'border-color 0.2s',
                      }}
                    >
                      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
                        <span style={{ fontSize: 10.5, fontWeight: 700, color, textTransform: 'uppercase', letterSpacing: '0.05em' }}>
                          {label}
                        </span>
                        <span style={{ width: 6, height: 6, borderRadius: '50%', background: color }} />
                      </div>
                      <div style={{ fontSize: 13, fontWeight: 600, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                        {title}
                      </div>
                    </div>
                  );
                })}
              </div>
            )}
          </div>

          {selectedNode && (
            <div style={{ marginTop: 12, padding: 12, borderRadius: 8, background: 'rgba(0,0,0,0.3)', border: '1px solid var(--glass-border)', fontSize: 12 }}>
              <div style={{ fontWeight: 600, color: 'var(--accent-cyan)', marginBottom: 6 }}>
                Selected Node: {selectedNode.labels.join(', ')} ({selectedNode.id})
              </div>
              <pre style={{ margin: 0, fontFamily: 'var(--font-mono)', fontSize: 11.5, color: 'var(--text-secondary)', overflowX: 'auto' }}>
                {JSON.stringify(selectedNode.properties, null, 2)}
              </pre>
            </div>
          )}
        </Section>

        {/* Cypher Playground & Live Queries Card */}
        <Section
          title="Cypher Query Console"
          hint="Execute direct Cypher statements against FalkorDB Cloud."
        >
          <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap', marginBottom: 10 }}>
            {PRESET_QUERIES.map((p) => (
              <button
                key={p.name}
                onClick={() => {
                  setCypherQuery(p.cypher);
                  void handleRunCypher(p.cypher);
                }}
                className="btn-ghost"
                style={{ fontSize: 11.5, padding: '4px 8px' }}
              >
                {p.name}
              </button>
            ))}
          </div>

          <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
            <textarea
              rows={4}
              value={cypherQuery}
              onChange={(e) => setCypherQuery(e.target.value)}
              placeholder="MATCH (n) RETURN n LIMIT 10..."
              style={{
                width: '100%',
                padding: '10px 12px',
                borderRadius: 8,
                background: 'rgba(0,0,0,0.35)',
                border: '1px solid var(--glass-border)',
                color: 'var(--text-primary)',
                fontFamily: 'var(--font-mono)',
                fontSize: 12.5,
                lineHeight: 1.5,
                resize: 'vertical',
                outline: 'none',
              }}
            />

            <button
              onClick={() => void handleRunCypher()}
              disabled={executing || !cypherQuery.trim()}
              className="btn-primary"
              style={{
                padding: '8px 16px',
                background: 'linear-gradient(135deg, var(--accent-cyan), var(--accent-purple))',
                color: '#fff',
                fontWeight: 600,
                fontSize: 13,
                borderRadius: 8,
                border: 'none',
                cursor: 'pointer',
                display: 'inline-flex',
                alignItems: 'center',
                gap: 8,
                alignSelf: 'flex-start',
              }}
            >
              <Play size={14} className={executing ? 'spin' : ''} /> {executing ? 'Executing...' : 'Run Cypher'}
            </button>
          </div>

          {/* Query Results View */}
          <div style={{ marginTop: 14 }}>
            <div style={{ fontSize: 12, fontWeight: 600, color: 'var(--text-secondary)', marginBottom: 6 }}>
              Query Output:
            </div>
            <div
              style={{
                height: 200,
                borderRadius: 8,
                background: 'rgba(0,0,0,0.4)',
                border: '1px solid var(--glass-border)',
                padding: 10,
                overflowY: 'auto',
                fontFamily: 'var(--font-mono)',
                fontSize: 11.5,
              }}
            >
              {queryResult ? (
                queryResult.error ? (
                  <div style={{ color: '#f87171' }}>Error: {queryResult.error}</div>
                ) : queryResult.data.length === 0 ? (
                  <div style={{ color: 'var(--text-muted)' }}>Query returned 0 rows.</div>
                ) : (
                  <pre style={{ margin: 0, color: 'var(--text-primary)' }}>
                    {JSON.stringify(queryResult.data, null, 2)}
                  </pre>
                )
              ) : (
                <div style={{ color: 'var(--text-muted)' }}>Run a query to inspect live FalkorDB tabular records.</div>
              )}
            </div>
          </div>
        </Section>
      </div>
    </div>
  );
}
