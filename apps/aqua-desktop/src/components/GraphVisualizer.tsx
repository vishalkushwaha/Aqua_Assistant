import { useMemo, useState } from 'react';
import {
  Filter,
  Network,
} from 'lucide-react';

export interface VisualNode {
  id: string;
  label: string;
  title: string;
  sub?: string;
  properties?: Record<string, any>;
  x?: number;
  y?: number;
}

export interface VisualEdge {
  source: string;
  target: string;
  label: string;
}

interface Props {
  nodes: VisualNode[];
  edges: VisualEdge[];
  highlightPaths?: string[][];
  onSelectNode?: (node: VisualNode | null) => void;
  selectedNodeId?: string | null;
  height?: number;
  title?: string;
  subtitle?: string;
}

const TYPE_CONFIG: Record<
  string,
  { color: string; bg: string; border: string }
> = {
  Person: { color: '#34d399', bg: 'rgba(52, 211, 153, 0.15)', border: '#059669' },
  Team: { color: '#2dd4bf', bg: 'rgba(45, 212, 191, 0.15)', border: '#0d9488' },
  Company: { color: '#38bdf8', bg: 'rgba(56, 189, 248, 0.15)', border: '#0284c7' },
  Project: { color: '#a78bfa', bg: 'rgba(167, 139, 250, 0.15)', border: '#7c3aed' },
  System: { color: '#818cf8', bg: 'rgba(129, 140, 248, 0.15)', border: '#4f46e5' },
  Ticket: { color: '#fb923c', bg: 'rgba(251, 146, 60, 0.15)', border: '#ea580c' },
  Decision: { color: '#facc15', bg: 'rgba(250, 204, 21, 0.15)', border: '#ca8a04' },
  Document: { color: '#f472b6', bg: 'rgba(244, 114, 182, 0.15)', border: '#db2777' },
  Chat: { color: '#fb7185', bg: 'rgba(251, 113, 133, 0.15)', border: '#e11d48' },
  Source: { color: '#f472b6', bg: 'rgba(244, 114, 182, 0.15)', border: '#db2777' },
  User: { color: '#60a5fa', bg: 'rgba(96, 165, 250, 0.15)', border: '#2563eb' },
  Task: { color: '#38bdf8', bg: 'rgba(56, 189, 248, 0.15)', border: '#0284c7' },
  Experience: { color: '#34d399', bg: 'rgba(52, 211, 153, 0.15)', border: '#059669' },
  Tool: { color: '#c084fc', bg: 'rgba(192, 132, 252, 0.15)', border: '#9333ea' },
  Skill: { color: '#fbbf24', bg: 'rgba(251, 191, 36, 0.15)', border: '#d97706' },
  Agent: { color: '#38bdf8', bg: 'rgba(56, 189, 248, 0.15)', border: '#0284c7' },
  Handoff: { color: '#fb7185', bg: 'rgba(251, 113, 133, 0.15)', border: '#e11d48' },
  Session: { color: '#a855f7', bg: 'rgba(168, 85, 247, 0.15)', border: '#9333ea' },
  Question: { color: '#38bdf8', bg: 'rgba(56, 189, 248, 0.15)', border: '#0284c7' },
  Answer: { color: '#10b981', bg: 'rgba(16, 185, 129, 0.15)', border: '#059669' },
  Action: { color: '#f59e0b', bg: 'rgba(245, 158, 11, 0.15)', border: '#d97706' },
  Room: { color: '#ec4899', bg: 'rgba(236, 72, 153, 0.15)', border: '#db2777' },
  Default: { color: '#94a3b8', bg: 'rgba(148, 163, 184, 0.15)', border: '#64748b' },
};

export function GraphVisualizer({
  nodes,
  edges,
  highlightPaths = [],
  onSelectNode,
  selectedNodeId,
  height = 420,
  title,
  subtitle,
}: Props) {
  const [filterType, setFilterType] = useState<string>('all');
  const [internalSelected, setInternalSelected] = useState<VisualNode | null>(null);

  const activeSelected = selectedNodeId
    ? nodes.find((n) => n.id === selectedNodeId) || internalSelected
    : internalSelected;

  // Flatten highlighted edges for quick lookup
  const highlightedEdgeSet = useMemo(() => {
    const set = new Set<string>();
    for (const path of highlightPaths) {
      for (let i = 0; i < path.length - 1; i++) {
        set.add(`${path[i]}->${path[i + 1]}`);
        set.add(`${path[i + 1]}->${path[i]}`);
      }
    }
    return set;
  }, [highlightPaths]);

  const highlightedNodeSet = useMemo(() => {
    const set = new Set<string>();
    for (const path of highlightPaths) {
      for (const node of path) {
        set.add(node);
      }
    }
    return set;
  }, [highlightPaths]);

  const uniqueTypes = useMemo(() => {
    const types = new Set<string>();
    for (const n of nodes) {
      types.add(n.label);
    }
    return Array.from(types);
  }, [nodes]);

  const hasSwitchNodes = useMemo(
    () => nodes.some((n) => n.id.toLowerCase().includes('switch')),
    [nodes],
  );

  const filteredNodes = useMemo(() => {
    if (filterType === 'all') return nodes;
    if (filterType === 'switch') return nodes.filter((n) => n.id.toLowerCase().includes('switch'));
    return nodes.filter((n) => n.label === filterType);
  }, [nodes, filterType]);

  const dense = filteredNodes.length > 36;
  const compact = filteredNodes.length > 20;
  const nodeRadius = dense ? 8 : compact ? 13 : 19;
  const showNodeLabels = filteredNodes.length <= 36;
  const showEdgeLabels = filteredNodes.length <= 24;

  // Layout coordinates in a balanced, visual radial-organic layout
  const positionedNodes = useMemo(() => {
    const width = 800;
    const canvasHeight = height;
    const centerX = width / 2;
    const centerY = canvasHeight / 2;

    const count = filteredNodes.length;
    if (count === 0) return [];

    // A single ring collapses a growing memory graph into an unreadable knot.
    // This deterministic sunflower layout spreads dense snapshots across the
    // entire canvas while keeping every node available to filters and inspection.
    if (count > 36) {
      const goldenAngle = Math.PI * (3 - Math.sqrt(5));
      return [...filteredNodes]
        .sort((a, b) => `${a.label}:${a.id}`.localeCompare(`${b.label}:${b.id}`))
        .map((node, index) => {
          const radius = Math.sqrt((index + 0.75) / count);
          const angle = index * goldenAngle;
          return {
            ...node,
            x: centerX + Math.cos(angle) * 350 * radius,
            y: centerY + Math.sin(angle) * 174 * radius,
          };
        });
    }

    // Group nodes by label
    const byType: Record<string, VisualNode[]> = {};
    for (const node of filteredNodes) {
      byType[node.label] = byType[node.label] || [];
      byType[node.label].push(node);
    }

    const labels = Object.keys(byType);
    const result: Array<VisualNode & { x: number; y: number }> = [];

    labels.forEach((label, labelIdx) => {
      const groupNodes = byType[label];
      const radius = count > 12 ? (labelIdx % 2 === 0 ? 170 : 250) : 160;
      const angleStep = (2 * Math.PI) / count;

      groupNodes.forEach((node, nodeIdx) => {
        // If node has preset coordinates, respect them
        if (typeof node.x === 'number' && typeof node.y === 'number') {
          result.push({ ...node, x: node.x, y: node.y });
          return;
        }

        const globalIndex = result.length;
        const angle = globalIndex * angleStep - Math.PI / 2;
        const jitter = (nodeIdx % 3) * 12;
        const x = centerX + (radius + jitter) * Math.cos(angle);
        const y = centerY + (radius * 0.75 + jitter) * Math.sin(angle);
        result.push({ ...node, x, y });
      });
    });

    return result;
  }, [filteredNodes, height]);

  const nodeMap = useMemo(() => {
    const map = new Map<string, VisualNode & { x: number; y: number }>();
    for (const n of positionedNodes) {
      map.set(n.id, n);
      map.set(n.title, n);
    }
    return map;
  }, [positionedNodes]);

  const positionedEdges = useMemo(() => {
    return edges
      .map((e) => {
        const sourceNode = nodeMap.get(e.source);
        const targetNode = nodeMap.get(e.target);
        if (!sourceNode || !targetNode) return null;
        const isHighlighted =
          highlightedEdgeSet.has(`${e.source}->${e.target}`) ||
          highlightedEdgeSet.has(`${sourceNode.title}->${targetNode.title}`) ||
          highlightedEdgeSet.has(`${sourceNode.id}->${targetNode.id}`);

        return {
          ...e,
          x1: sourceNode.x,
          y1: sourceNode.y,
          x2: targetNode.x,
          y2: targetNode.y,
          isHighlighted,
        };
      })
      .filter(Boolean) as Array<
      VisualEdge & { x1: number; y1: number; x2: number; y2: number; isHighlighted: boolean }
    >;
  }, [edges, nodeMap, highlightedEdgeSet]);

  const handleNodeClick = (node: VisualNode) => {
    setInternalSelected(node);
    if (onSelectNode) onSelectNode(node);
  };

  return (
    <div
      className="glass-panel"
      style={{
        display: 'flex',
        flexDirection: 'column',
        borderRadius: 16,
        border: '1px solid var(--glass-border)',
        overflow: 'hidden',
        background: 'linear-gradient(180deg, rgba(14, 20, 32, 0.75) 0%, rgba(6, 10, 18, 0.9) 100%)',
        position: 'relative',
      }}
    >
      {/* Visual Header & Controls */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          padding: '12px 18px',
          borderBottom: '1px solid var(--glass-border)',
          flexWrap: 'wrap',
          gap: 10,
        }}
      >
        <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
          <div
            style={{
              width: 32,
              height: 32,
              borderRadius: 8,
              background: 'rgba(60, 203, 255, 0.12)',
              display: 'grid',
              placeItems: 'center',
            }}
          >
            <Network size={16} color="var(--accent-cyan)" />
          </div>
          <div>
            <div style={{ fontSize: 13.5, fontWeight: 700, color: 'var(--text-primary)' }}>
              {title || 'FalkorDB Graph Topology'}
            </div>
            <div style={{ fontSize: 11.5, color: 'var(--text-muted)' }}>
              {subtitle || `${nodes.length} nodes · ${edges.length} relationships · Multi-hop verified`}
            </div>
          </div>
        </div>

        {/* Filter Pills */}
        <div style={{ display: 'flex', alignItems: 'center', gap: 6, flexWrap: 'wrap' }}>
          <span style={{ fontSize: 11, color: 'var(--text-muted)', display: 'flex', alignItems: 'center', gap: 4 }}>
            <Filter size={12} /> Filter:
          </span>
          <button
            onClick={() => setFilterType('all')}
            style={{
              padding: '3px 8px',
              borderRadius: 6,
              fontSize: 11,
              fontWeight: 600,
              cursor: 'pointer',
              border: '1px solid var(--glass-border)',
              background: filterType === 'all' ? 'rgba(60, 203, 255, 0.2)' : 'transparent',
              color: filterType === 'all' ? 'var(--accent-cyan)' : 'var(--text-secondary)',
            }}
          >
            All ({nodes.length})
          </button>
          {uniqueTypes.map((t) => {
            const cfg = TYPE_CONFIG[t] || TYPE_CONFIG.Default;
            const count = nodes.filter((n) => n.label === t).length;
            const isActive = filterType === t;
            return (
              <button
                key={t}
                onClick={() => setFilterType(t)}
                style={{
                  padding: '3px 8px',
                  borderRadius: 6,
                  fontSize: 11,
                  fontWeight: 600,
                  cursor: 'pointer',
                  border: `1px solid ${isActive ? cfg.color : 'var(--glass-border)'}`,
                  background: isActive ? cfg.bg : 'transparent',
                  color: isActive ? cfg.color : 'var(--text-secondary)',
                  display: 'flex',
                  alignItems: 'center',
                  gap: 4,
                }}
              >
                <span style={{ width: 6, height: 6, borderRadius: '50%', background: cfg.color }} />
                {t} ({count})
              </button>
            );
          })}
          {hasSwitchNodes && (
            <button
              onClick={() => setFilterType('switch')}
              style={{
                padding: '3px 8px',
                borderRadius: 6,
                fontSize: 11,
                fontWeight: 600,
                cursor: 'pointer',
                border: `1px solid ${filterType === 'switch' ? '#ec4899' : 'var(--glass-border)'}`,
                background: filterType === 'switch' ? 'rgba(236, 72, 153, 0.2)' : 'transparent',
                color: filterType === 'switch' ? '#f472b6' : 'var(--text-secondary)',
                display: 'flex',
                alignItems: 'center',
                gap: 4,
              }}
            >
              <span style={{ width: 6, height: 6, borderRadius: '50%', background: '#ec4899' }} />
              ⚡ Switch Rooms ({nodes.filter((n) => n.id.toLowerCase().includes('switch')).length})
            </button>
          )}
        </div>
      </div>

      {/* SVG Canvas Area */}
      <div style={{ position: 'relative', height, width: '100%', overflow: 'hidden' }}>
        <svg
          viewBox={`0 0 800 ${height}`}
          preserveAspectRatio="xMidYMid meet"
          style={{ width: '100%', height: '100%', display: 'block' }}
        >
          <defs>
            {/* Edge arrowhead markers */}
            <marker
              id="arrow-default"
              viewBox="0 0 10 10"
              refX="18"
              refY="5"
              markerWidth="6"
              markerHeight="6"
              orient="auto-start-reverse"
            >
              <path d="M 0 1 L 9 5 L 0 9 z" fill="rgba(148, 163, 184, 0.4)" />
            </marker>
            <marker
              id="arrow-highlight"
              viewBox="0 0 10 10"
              refX="18"
              refY="5"
              markerWidth="7"
              markerHeight="7"
              orient="auto-start-reverse"
            >
              <path d="M 0 1 L 9 5 L 0 9 z" fill="#3ccbff" />
            </marker>

            {/* Glowing filter */}
            <filter id="glow-cyan" x="-20%" y="-20%" width="140%" height="140%">
              <feGaussianBlur stdDeviation="3" result="blur" />
              <feComposite in="SourceGraphic" in2="blur" operator="over" />
            </filter>
          </defs>

          {/* Grid background dots */}
          <pattern id="graph-grid" width="24" height="24" patternUnits="userSpaceOnUse">
            <circle cx="12" cy="12" r="0.75" fill="rgba(255, 255, 255, 0.05)" />
          </pattern>
          <rect width="100%" height="100%" fill="url(#graph-grid)" />

          {/* Edges */}
          <g>
            {positionedEdges.map((edge, idx) => {
              const midX = (edge.x1 + edge.x2) / 2;
              const midY = (edge.y1 + edge.y2) / 2;

              return (
                <g key={`edge-${edge.source}-${edge.target}-${idx}`}>
                  {/* Base connecting line */}
                  <line
                    x1={edge.x1}
                    y1={edge.y1}
                    x2={edge.x2}
                    y2={edge.y2}
                    stroke={edge.isHighlighted ? '#3ccbff' : 'rgba(255, 255, 255, 0.12)'}
                    strokeWidth={edge.isHighlighted ? 2.5 : 1.2}
                    strokeDasharray={edge.isHighlighted ? '6 3' : 'none'}
                    markerEnd={edge.isHighlighted ? 'url(#arrow-highlight)' : 'url(#arrow-default)'}
                    filter={edge.isHighlighted ? 'url(#glow-cyan)' : undefined}
                    style={{
                      transition: 'stroke 0.3s, stroke-width 0.3s',
                      animation: edge.isHighlighted ? 'pulse-edge 2s linear infinite' : 'none',
                    }}
                  />

                  {/* Relationship Label Pill */}
                  {showEdgeLabels && edge.label && (
                    <g transform={`translate(${midX}, ${midY})`}>
                      <rect
                        x="-24"
                        y="-8"
                        width="48"
                        height="16"
                        rx="4"
                        fill={edge.isHighlighted ? 'rgba(6, 182, 212, 0.9)' : 'rgba(15, 23, 42, 0.85)'}
                        stroke={edge.isHighlighted ? '#3ccbff' : 'rgba(255, 255, 255, 0.15)'}
                        strokeWidth="0.7"
                      />
                      <text
                        x="0"
                        y="3"
                        textAnchor="middle"
                        fill={edge.isHighlighted ? '#020617' : '#94a3b8'}
                        fontSize="8.5"
                        fontWeight="700"
                        fontFamily="var(--font-mono)"
                      >
                        {edge.label.length > 8 ? edge.label.substring(0, 8) + '…' : edge.label}
                      </text>
                    </g>
                  )}
                </g>
              );
            })}
          </g>

          {/* Nodes */}
          <g>
            {positionedNodes.map((node) => {
              const cfg = TYPE_CONFIG[node.label] || TYPE_CONFIG.Default;
              const isSelected = activeSelected?.id === node.id;
              const isHighlighted =
                highlightedNodeSet.has(node.id) || highlightedNodeSet.has(node.title);

              return (
                <g
                  key={node.id}
                  transform={`translate(${node.x}, ${node.y})`}
                  onClick={() => handleNodeClick(node)}
                  style={{ cursor: 'pointer', transition: 'transform 0.2s' }}
                >
                  {/* Outer Pulsing Halo when highlighted or selected */}
                  {(isSelected || isHighlighted) && (
                    <circle
                      r={nodeRadius + 7}
                      fill="none"
                      stroke={cfg.color}
                      strokeWidth="1.5"
                      strokeOpacity="0.4"
                      strokeDasharray="4 2"
                      style={{
                        animation: 'spin 12s linear infinite',
                      }}
                    />
                  )}

                  {/* Node Circle */}
                  <circle
                    r={nodeRadius}
                    fill={isSelected ? cfg.bg : 'rgba(18, 26, 42, 0.95)'}
                    stroke={isSelected || isHighlighted ? cfg.color : cfg.border}
                    strokeWidth={isSelected || isHighlighted ? 2.5 : 1.5}
                    filter={isSelected || isHighlighted ? 'url(#glow-cyan)' : undefined}
                  />

                  {/* Center Dot or Mini Icon representation */}
                  <circle r={dense ? 3 : compact ? 4 : 6} fill={cfg.color} opacity="0.9" />

                  <title>{`${node.label}: ${node.title}`}</title>

                  {showNodeLabels && (
                    <>
                      <text
                        x="0"
                        y={nodeRadius + 13}
                        textAnchor="middle"
                        fill={isSelected ? '#ffffff' : 'var(--text-primary)'}
                        fontSize={compact ? '9.5' : '11'}
                        fontWeight={isSelected ? '700' : '600'}
                        style={{ pointerEvents: 'none' }}
                      >
                        {node.title.length > 18 ? node.title.substring(0, 16) + '…' : node.title}
                      </text>

                      <text
                        x="0"
                        y={nodeRadius + 24}
                        textAnchor="middle"
                        fill={cfg.color}
                        fontSize="8.5"
                        fontWeight="700"
                        fontFamily="var(--font-mono)"
                        style={{ pointerEvents: 'none', letterSpacing: '0.04em' }}
                      >
                        {node.label.toUpperCase()}
                      </text>
                    </>
                  )}
                </g>
              );
            })}
          </g>
        </svg>

        {/* Selected Node Details Floating Overlay */}
        {activeSelected && (
          <div
            style={{
              position: 'absolute',
              bottom: 12,
              right: 12,
              maxWidth: 320,
              padding: '12px 14px',
              borderRadius: 10,
              background: 'rgba(10, 15, 26, 0.95)',
              border: '1px solid var(--glass-border-highlight)',
              boxShadow: '0 8px 24px rgba(0,0,0,0.5)',
              backdropFilter: 'blur(10px)',
              fontSize: 12,
              zIndex: 10,
            }}
          >
            <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 6 }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                <span
                  style={{
                    fontSize: 10,
                    fontWeight: 800,
                    color: (TYPE_CONFIG[activeSelected.label] || TYPE_CONFIG.Default).color,
                    textTransform: 'uppercase',
                    letterSpacing: '0.06em',
                  }}
                >
                  {activeSelected.label} Node
                </span>
                {activeSelected.id.toLowerCase().includes('switch') && (
                  <span
                    style={{
                      fontSize: 9,
                      fontWeight: 700,
                      padding: '1px 5px',
                      borderRadius: 4,
                      background: 'rgba(236, 72, 153, 0.2)',
                      color: '#f472b6',
                      border: '1px solid rgba(236, 72, 153, 0.4)',
                      letterSpacing: '0.04em',
                    }}
                  >
                    ⚡ Switch Room
                  </span>
                )}
              </div>
              <button
                onClick={() => setInternalSelected(null)}
                style={{
                  background: 'transparent',
                  border: 'none',
                  color: 'var(--text-muted)',
                  cursor: 'pointer',
                  fontSize: 14,
                  lineHeight: 1,
                }}
              >
                ✕
              </button>
            </div>
            <div style={{ fontWeight: 700, fontSize: 13, color: '#fff', marginBottom: 4 }}>
              {activeSelected.title}
            </div>
            <div style={{ color: 'var(--text-secondary)', fontSize: 11, fontFamily: 'var(--font-mono)', wordBreak: 'break-all' }}>
              ID: {activeSelected.id}
            </div>
            {activeSelected.properties && (
              <div style={{ marginTop: 8, borderTop: '1px solid rgba(255,255,255,0.08)', paddingTop: 6 }}>
                {Object.entries(activeSelected.properties).map(([k, v]) => (
                  <div key={k} style={{ display: 'flex', justifyContent: 'space-between', gap: 8, fontSize: 11, marginTop: 2 }}>
                    <span style={{ color: 'var(--text-muted)' }}>{k}:</span>
                    <span style={{ color: 'var(--text-primary)', fontWeight: 500, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', maxWidth: 180 }}>
                      {String(v)}
                    </span>
                  </div>
                ))}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
