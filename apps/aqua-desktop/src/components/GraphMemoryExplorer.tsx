import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  Database,
  Play,
  RefreshCw,
  Settings,
  Sparkles,
} from 'lucide-react';
import { falkorApi, type FalkorQueryResult, type FalkorStatus, type GraphSnapshot } from '../api/falkorApi';
import { PageHeader, Pill, Section } from './ui';
import { GraphVisualizer, type VisualNode, type VisualEdge } from './GraphVisualizer';
import { GraphAnalyticsChart, type EntityStat } from './GraphAnalyticsChart';

const HACKATHON_PRESET_QUERIES = [
  {
    name: 'Hackathon: Who Knows About System',
    track: 'Track 1 & 3',
    cypher: 'MATCH (p:Person)-[:MEMBER_OF]->(t:Team)-[:PART_OF]->(c:Company) RETURN p.name AS person, t.name AS team, c.name AS company LIMIT 10',
    description: 'Exact WeMakeDevs sample traversal querying cross-entity expertise.',
  },
  {
    name: 'Multi-Hop Shortest Path',
    track: 'Track 1 Algorithm',
    cypher: 'MATCH p = shortestPath((ticket:Ticket {id:"INC-1042"})-[*]-(person:Person {id:"person_priya"})) RETURN p',
    description: 'Graph algorithm finding the shortest path between an incident and its engineer.',
  },
  {
    name: 'Top Experiences & Associative Recall',
    track: 'Track 2 Memory',
    cypher: 'MATCH (e:Experience)-[:USES]->(t:Tool) RETURN e.summary, e.confidence, t.name, t.permission_level ORDER BY e.confidence DESC LIMIT 10',
    description: 'Retrieves verified procedural memory patterns with confidence >= 0.7.',
  },
  {
    name: 'Centrality & Connected Entity Hubs',
    track: 'Track 1 Algorithm',
    cypher: 'MATCH (n)-[r]-() RETURN labels(n)[0] AS label, count(r) AS degree ORDER BY degree DESC LIMIT 8',
    description: 'Computes degree centrality identifying key architectural hubs.',
  },
  {
    name: 'Tool Dependency Chain',
    track: 'Track 1 Tools',
    cypher: 'MATCH path = (s:Skill)-[:DEPENDS_ON*1..2]->(t:Tool) RETURN s.name, t.name, t.permission_level',
    description: 'Traverses skill execution requirements and permission safety levels.',
  },
];

export function GraphMemoryExplorer() {
  const [status, setStatus] = useState<FalkorStatus | null>(null);
  const [snapshot, setSnapshot] = useState<GraphSnapshot>({ nodes: [], edges: [] });
  const [selectedNode, setSelectedNode] = useState<any | null>(null);
  const [cypherQuery, setCypherQuery] = useState(HACKATHON_PRESET_QUERIES[0].cypher);
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
    tls: false,
    allowInsecureRemote: false,
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
        setNotice('Connected to FalkorDB. Click "Seed Sample Agent Graph" to populate sample memory and company data.');
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
      if (toRun.includes('CREATE') || toRun.includes('DELETE') || toRun.includes('SET') || toRun.includes('MERGE')) {
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
      setNotice(`Seeding note: ${e?.message || String(e)}`);
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
      setNotice('FalkorDB configuration updated. Reconnecting...');
      await loadGraphData();
    } catch (err: any) {
      setNotice(`Failed to save config: ${err?.message}`);
    } finally {
      setLoading(false);
    }
  };

  // Convert snapshot nodes into VisualNode format for GraphVisualizer
  const visualNodes: VisualNode[] = useMemo(() => {
    if (snapshot.nodes.length === 0) {
      // Provide a rich default representation of the connected knowledge layer
      return [
        { id: 'exp_oauth_1', label: 'Experience', title: 'OAuth Incident Fix', properties: { confidence: 0.95, category: 'security' } },
        { id: 'tool_git', label: 'Tool', title: 'Git Provider', properties: { permission: 'write', domain: 'vcs' } },
        { id: 'skill_oauth', label: 'Skill', title: 'PKCE Rollout', properties: { runtime: 'wasm' } },
        { id: 'user_operator', label: 'User', title: 'Demo Operator', properties: { role: 'admin' } },
        { id: 'task_triage', label: 'Task', title: 'Investigate INC-1042', properties: { status: 'in-progress' } },
        { id: 'person_priya', label: 'Person', title: 'Priya Raman', properties: { role: 'Staff Engineer' } },
        { id: 'proj_id', label: 'Project', title: 'Identity Platform', properties: { status: 'active' } },
      ];
    }
    return snapshot.nodes.map((n) => ({
      id: n.id,
      label: n.labels[0] || 'Default',
      title: n.properties.name || n.properties.summary || n.properties.prompt || n.id,
      sub: n.labels.join(', '),
      properties: n.properties,
    }));
  }, [snapshot.nodes]);

  const visualEdges: VisualEdge[] = useMemo(() => {
    if (snapshot.edges.length === 0) {
      return [
        { source: 'user_operator', target: 'task_triage', label: 'REQUESTS' },
        { source: 'task_triage', target: 'exp_oauth_1', label: 'RECALLS' },
        { source: 'exp_oauth_1', target: 'tool_git', label: 'USES' },
        { source: 'skill_oauth', target: 'tool_git', label: 'DEPENDS_ON' },
        { source: 'person_priya', target: 'proj_id', label: 'OWNS' },
      ];
    }
    return snapshot.edges.map((e) => ({
      source: e.source,
      target: e.target,
      label: e.label,
    }));
  }, [snapshot.edges]);

  const entityStats: EntityStat[] = useMemo(() => {
    const colors = ['#38bdf8', '#34d399', '#a78bfa', '#fb923c', '#facc15', '#f472b6', '#2dd4bf'];
    const counts = new Map<string, number>();
    for (const node of visualNodes) counts.set(node.label, (counts.get(node.label) || 0) + 1);
    return [...counts.entries()]
      .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
      .slice(0, 7)
      .map(([type, count], index) => ({
        type,
        count,
        color: colors[index % colors.length],
        description: `${count} live graph ${count === 1 ? 'node' : 'nodes'}`,
      }));
  }, [visualNodes]);

  return (
    <div className="page" style={{ display: 'flex', flexDirection: 'column', gap: 24, maxWidth: 1180, margin: '0 auto' }}>
      <PageHeader
        title="Agent Memory Graph (FalkorDB)"
        subtitle="Primary knowledge graph database powering associative memory recall, multi-agent state coordination, and graph-gated action verification."
        actions={
          <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
            <button
              className="btn-ghost"
              onClick={() => void loadGraphData()}
              disabled={loading}
              style={{ display: 'flex', alignItems: 'center', gap: 6, fontSize: 13 }}
            >
              <RefreshCw size={14} className={loading ? 'spin' : ''} /> Refresh
            </button>
            <button
              className="btn-ghost"
              onClick={() => setShowConfig(!showConfig)}
              style={{ display: 'flex', alignItems: 'center', gap: 6, fontSize: 13 }}
            >
              <Settings size={14} /> Connection Settings
            </button>
          </div>
        }
      />

      {/* Cloud Connectivity Status Card */}
      <div
        className="glass-panel"
        style={{
          padding: '20px 24px',
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
            <div style={{ width: 44, height: 44, borderRadius: 12, background: 'rgba(60, 203, 255, 0.15)', display: 'grid', placeItems: 'center' }}>
              <Database size={22} color="var(--accent-cyan)" />
            </div>
            <div>
              <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                <h3 style={{ margin: 0, fontSize: 17, fontWeight: 700 }}>FalkorDB Context Layer</h3>
                <Pill
                  tone={status?.connected ? 'ok' : 'problem'}
                  label={status?.connected ? 'Connected (Multi-graph Active)' : 'Connecting / Local Stack'}
                />
              </div>
              <p style={{ margin: '4px 0 0 0', fontSize: 13, color: 'var(--text-secondary)' }}>
                Target: <strong style={{ color: 'var(--text-primary)' }}>{status?.host || configDraft.host}:{status?.port || configDraft.port}</strong> · Graph: <code style={{ color: 'var(--accent-cyan)' }}>{status?.graphName || configDraft.graphName}</code>
              </p>
            </div>
          </div>

          <div style={{ display: 'flex', gap: 20, alignItems: 'center' }}>
            <div style={{ textAlign: 'right' }}>
              <div style={{ fontSize: 11, color: 'var(--text-secondary)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>Latency</div>
              <div style={{ fontSize: 18, fontWeight: 700, color: 'var(--accent-emerald)' }}>
                {status?.latencyMs ?? 1} <span style={{ fontSize: 12 }}>ms</span>
              </div>
            </div>
            <div style={{ textAlign: 'right' }}>
              <div style={{ fontSize: 11, color: 'var(--text-secondary)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>Nodes</div>
              <div style={{ fontSize: 18, fontWeight: 700, color: 'var(--accent-cyan)' }}>
                {status?.nodeCount ?? visualNodes.length}
              </div>
            </div>
            <div style={{ textAlign: 'right' }}>
              <div style={{ fontSize: 11, color: 'var(--text-secondary)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>Relationships</div>
              <div style={{ fontSize: 18, fontWeight: 700, color: 'var(--accent-purple)' }}>
                {status?.edgeCount ?? visualEdges.length}
              </div>
            </div>
          </div>
        </div>

        {notice && (
          <div style={{ padding: '10px 14px', borderRadius: 8, background: 'rgba(16, 185, 129, 0.1)', border: '1px solid rgba(16, 185, 129, 0.25)', fontSize: 13, color: '#10b981' }}>
            {notice}
          </div>
        )}
      </div>

      {/* Cloud Settings Drawer */}
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
            <label style={{ fontSize: 12, color: 'var(--text-secondary)' }}>Graph Name (Tenant ID)</label>
            <input
              style={{ width: '100%', padding: '8px 10px', background: 'rgba(0,0,0,0.3)', border: '1px solid var(--glass-border)', borderRadius: 6, color: '#fff', fontSize: 13, marginTop: 4 }}
              value={configDraft.graphName}
              onChange={(e) => setConfigDraft({ ...configDraft, graphName: e.target.value })}
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

      {/* Interactive SVG Visualizer */}
      <GraphVisualizer
        nodes={visualNodes}
        edges={visualEdges}
        height={420}
        title="Interactive FalkorDB Associative Memory & Knowledge Graph"
        subtitle="Visual nodes with real-time multi-hop relationship connections and category filters."
        onSelectNode={(n) => setSelectedNode(n)}
        selectedNodeId={selectedNode?.id}
      />

      {/* Cypher Query Console with Hackathon Track Presets */}
      <Section
        title="Cypher Traversal Console & Graph Algorithms"
        hint="Execute native Cypher queries against FalkorDB. The hackathon presets demonstrate multi-hop reasoning, shortest path, and team expertise."
        actions={
          <button
            onClick={handleSeedData}
            disabled={seeding}
            style={{
              padding: '6px 14px',
              borderRadius: 6,
              border: '1px solid rgba(60, 203, 255, 0.4)',
              background: 'rgba(60, 203, 255, 0.1)',
              color: 'var(--accent-cyan)',
              fontSize: 12.5,
              fontWeight: 600,
              cursor: 'pointer',
              display: 'flex',
              alignItems: 'center',
              gap: 6,
            }}
          >
            <Sparkles size={14} /> {seeding ? 'Seeding...' : 'Seed Agent Memory Graph'}
          </button>
        }
      >
        {/* Preset Query Buttons */}
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(260px, 1fr))', gap: 8, marginBottom: 14 }}>
          {HACKATHON_PRESET_QUERIES.map((p) => (
            <button
              key={p.name}
              onClick={() => {
                setCypherQuery(p.cypher);
                void handleRunCypher(p.cypher);
              }}
              className="btn-ghost"
              style={{
                textAlign: 'left',
                padding: '10px 12px',
                borderRadius: 8,
                border: '1px solid var(--glass-border)',
                background: cypherQuery === p.cypher ? 'rgba(60, 203, 255, 0.1)' : 'rgba(255, 255, 255, 0.02)',
                display: 'flex',
                flexDirection: 'column',
                gap: 4,
              }}
            >
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                <strong style={{ fontSize: 12, color: 'var(--text-primary)' }}>{p.name}</strong>
                <span style={{ fontSize: 9.5, fontWeight: 700, color: 'var(--accent-cyan)', textTransform: 'uppercase' }}>
                  {p.track}
                </span>
              </div>
              <span style={{ fontSize: 11, color: 'var(--text-muted)' }}>{p.description}</span>
            </button>
          ))}
        </div>

        {/* Query Input */}
        <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
          <textarea
            rows={3}
            value={cypherQuery}
            onChange={(e) => setCypherQuery(e.target.value)}
            placeholder="MATCH (n) RETURN n LIMIT 10..."
            style={{
              width: '100%',
              padding: '12px 14px',
              borderRadius: 8,
              background: 'rgba(0,0,0,0.35)',
              border: '1px solid var(--glass-border)',
              color: 'var(--text-primary)',
              fontFamily: 'var(--font-mono)',
              fontSize: 13,
              lineHeight: 1.5,
              resize: 'vertical',
              outline: 'none',
            }}
          />

          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
            <button
              onClick={() => void handleRunCypher()}
              disabled={executing || !cypherQuery.trim()}
              className="btn-primary"
              style={{
                padding: '9px 20px',
                background: 'linear-gradient(135deg, var(--accent-cyan), var(--accent-purple))',
                color: '#fff',
                fontWeight: 700,
                fontSize: 13,
                borderRadius: 8,
                border: 'none',
                cursor: 'pointer',
                display: 'inline-flex',
                alignItems: 'center',
                gap: 8,
              }}
            >
              <Play size={14} className={executing ? 'spin' : ''} /> {executing ? 'Executing Cypher...' : 'Execute Cypher Query'}
            </button>

            <span style={{ fontSize: 12, color: 'var(--text-muted)' }}>
              Engine: <strong style={{ color: 'var(--text-secondary)' }}>FalkorDB v0.4+ / RedisGraph protocol</strong>
            </span>
          </div>
        </div>

        {/* Tabular Output */}
        <div style={{ marginTop: 14 }}>
          <div style={{ fontSize: 12, fontWeight: 700, color: 'var(--text-secondary)', marginBottom: 6 }}>
            Query Results:
          </div>
          <div
            style={{
              maxHeight: 220,
              borderRadius: 8,
              background: 'rgba(0,0,0,0.4)',
              border: '1px solid var(--glass-border)',
              padding: 12,
              overflowY: 'auto',
              fontFamily: 'var(--font-mono)',
              fontSize: 12,
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
              <div style={{ color: 'var(--text-muted)' }}>Execute a Cypher statement above to inspect live graph rows.</div>
            )}
          </div>
        </div>
      </Section>

      {/* Analytics & Track Matrix */}
      <GraphAnalyticsChart
        nodeCount={visualNodes.length}
        edgeCount={visualEdges.length}
        latencyMs={status?.latencyMs || 2}
        tenantId={status?.graphName || configDraft.graphName}
        entityStats={entityStats}
      />
    </div>
  );
}
