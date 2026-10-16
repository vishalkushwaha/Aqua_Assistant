import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties, type PointerEvent as RPointerEvent, type ReactNode } from 'react';
import { GitBranch, Play, Plus, Trash2, Copy, Save, X, LayoutGrid, History, Download, Upload, AlertTriangle, CheckCircle2, Power } from 'lucide-react';
import { CommentInbox } from './FlowComments';
import { HealthDashboard, N8nStatus, StatusChat } from './FlowHealth';
import { SchedulePanel, ScheduledDashboard, Notifications } from './FlowSchedule';
import { Inbox, RunDialog, RunHistory, RunPanel, STATUS_COLOUR, STATUS_LABEL, useRun } from './FlowRun';
import { cannotRunNote, kindOptionLabel, loopLimitNote } from '../flowStepNotes';
import {
  autoLayout, flowsApi, NODE_H, NODE_W, RISK_COLOUR,
  type ChatMessage, type NodeRun, type Diff, type FieldSpec, type Flow, type FlowDef, type FlowNode, type FlowSummary, type Issue, type NodeKind, type NodeSpec, type Proposal, type Report, type VariableDef, type VersionInfo,
} from '../api/flows';

const card: CSSProperties = { background: 'rgba(255,255,255,0.04)', border: '1px solid var(--glass-border)', borderRadius: 12, padding: 14 };
const muted: CSSProperties = { fontSize: 12.5, color: 'var(--text-secondary)' };
const button: CSSProperties = { padding: '7px 12px', borderRadius: 8, border: '1px solid rgba(6,182,212,0.4)', background: 'rgba(6,182,212,0.12)', color: 'var(--accent-cyan)', fontSize: 12.5, cursor: 'pointer', display: 'inline-flex', alignItems: 'center', gap: 6 };
const dangerButton: CSSProperties = { ...button, border: '1px solid rgba(248,113,113,0.5)', background: 'rgba(248,113,113,0.12)', color: '#f87171' };
const input: CSSProperties = { width: '100%', background: 'rgba(0,0,0,0.35)', border: '1px solid var(--glass-border)', borderRadius: 8, padding: '6px 8px', color: 'inherit', fontSize: 13, boxSizing: 'border-box' };
const mono: CSSProperties = { ...input, fontFamily: 'ui-monospace, Consolas, monospace', fontSize: 12 };

const when = (t: number) => new Date(t * 1000).toLocaleString();
const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <label style={{ display: 'block' }}>
      <div style={{ ...muted, marginBottom: 3 }}>{label}</div>
      {children}
      {hint && <div style={{ ...muted, fontSize: 11.5, marginTop: 2 }}>{hint}</div>}
    </label>
  );
}

// ---- helpers over the definition --------------------------------------------------------------

function uniqueId(def: FlowDef, base: string): string {
  const taken = new Set(def.nodes.map((n) => n.id));
  if (!taken.has(base)) return base;
  let i = 2;
  while (taken.has(`${base}_${i}`)) i += 1;
  return `${base}_${i}`;
}

function makeNode(def: FlowDef, spec: NodeSpec, x: number, y: number): FlowNode {
  const config: Record<string, unknown> = {};
  spec.fields.forEach((f) => {
    if (f.default !== null && f.default !== undefined) config[f.name] = f.default;
  });
  return {
    id: uniqueId(def, spec.kind), kind: spec.kind, name: spec.label, description: '', config, inputs: [], outputs: [...spec.default_outputs],
    position: { x, y }, enabled: true, retry: { retries: 0, delay_secs: 10, backoff: 'exponential' }, on_error: 'fail', credential: null,
  };
}

const edgeId = (from: string, port: string, to: string) => `${from}:${port}->${to}`;

function portX(node: FlowNode, ports: string[], port: string): number {
  const i = Math.max(0, ports.indexOf(port));
  return node.position.x + (NODE_W * (i + 1)) / (ports.length + 1);
}

// ---- canvas -----------------------------------------------------------------------------------

type Selection = { type: 'node'; id: string } | { type: 'edge'; id: string } | null;

function Canvas({ def, specs, selection, issuesByNode, onSelect, onMove, onConnect, readOnly = false, highlight, minHeight, runState }: {
  def: FlowDef;
  specs: Map<NodeKind, NodeSpec>;
  selection: Selection;
  issuesByNode: Map<string, Issue[]>;
  onSelect: (s: Selection) => void;
  onMove: (id: string, x: number, y: number) => void;
  onConnect: (from: string, port: string, to: string) => void;
  /** A preview: nothing can be moved or connected. */
  readOnly?: boolean;
  /** Steps a proposal adds or changes, drawn in colour. */
  highlight?: Map<string, 'added' | 'changed'>;
  minHeight?: number;
  /** Where a run has got to: each step drawn in the colour of its state. */
  runState?: Map<string, NodeRun>;
}) {
  const box = useRef<HTMLDivElement>(null);
  const [drag, setDrag] = useState<{ id: string; dx: number; dy: number } | null>(null);
  const [link, setLink] = useState<{ from: string; port: string; x: number; y: number } | null>(null);

  const point = (e: { clientX: number; clientY: number }) => {
    const r = box.current!.getBoundingClientRect();
    return { x: e.clientX - r.left + box.current!.scrollLeft, y: e.clientY - r.top + box.current!.scrollTop };
  };

  const onPointerMove = (e: RPointerEvent) => {
    if (drag) {
      const p = point(e);
      onMove(drag.id, Math.max(0, Math.round(p.x - drag.dx)), Math.max(0, Math.round(p.y - drag.dy)));
    } else if (link) {
      const p = point(e);
      setLink({ ...link, x: p.x, y: p.y });
    }
  };

  const onPointerUp = (e: RPointerEvent) => {
    if (link) {
      const p = point(e);
      const target = def.nodes.find((n) => n.id !== link.from && p.x >= n.position.x && p.x <= n.position.x + NODE_W && p.y >= n.position.y - 14 && p.y <= n.position.y + NODE_H);
      if (target) onConnect(link.from, link.port, target.id);
    }
    setDrag(null);
    setLink(null);
  };

  const width = Math.max(900, ...def.nodes.map((n) => n.position.x + NODE_W + 80));
  const height = Math.max(600, ...def.nodes.map((n) => n.position.y + NODE_H + 100));

  return (
    <div ref={box} onPointerMove={onPointerMove} onPointerUp={onPointerUp} onPointerLeave={() => { setDrag(null); setLink(null); }}
      onPointerDown={(e) => { if (e.target === e.currentTarget) onSelect(null); }}
      style={{ position: 'relative', overflow: 'auto', height: '100%', minHeight: minHeight ?? 520, background: 'rgba(0,0,0,0.25)', borderRadius: 12, border: '1px solid var(--glass-border)', backgroundImage: 'radial-gradient(rgba(255,255,255,0.06) 1px, transparent 1px)', backgroundSize: '22px 22px', touchAction: 'none' }}
      data-testid="flow-canvas">
      <div style={{ position: 'relative', width, height }} onPointerDown={(e) => { if (e.target === e.currentTarget) onSelect(null); }}>
        <svg width={width} height={height} style={{ position: 'absolute', inset: 0, pointerEvents: 'none' }}>
          <defs>
            <marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" fill="#64748b" /></marker>
            <marker id="arrow-sel" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" fill="#06b6d4" /></marker>
          </defs>
          {def.edges.map((e) => {
            const a = def.nodes.find((n) => n.id === e.from);
            const b = def.nodes.find((n) => n.id === e.to);
            if (!a || !b) return null;
            const ports = specs.get(a.kind)?.out_ports ?? ['out'];
            const x1 = portX(a, ports, e.port);
            const y1 = a.position.y + NODE_H;
            const x2 = b.position.x + NODE_W / 2;
            const y2 = b.position.y;
            const dy = Math.max(40, Math.abs(y2 - y1) / 2);
            const d = `M ${x1} ${y1} C ${x1} ${y1 + dy}, ${x2} ${y2 - dy}, ${x2} ${y2 - 2}`;
            const sel = selection?.type === 'edge' && selection.id === e.id;
            return (
              <g key={e.id}>
                <path d={d} fill="none" stroke={sel ? '#06b6d4' : '#64748b'} strokeWidth={sel ? 2.5 : 1.6} markerEnd={`url(#${sel ? 'arrow-sel' : 'arrow'})`} />
                <path d={d} fill="none" stroke="transparent" strokeWidth={14} style={{ pointerEvents: 'stroke', cursor: 'pointer' }} data-edge={e.id} onPointerDown={(ev) => { ev.stopPropagation(); onSelect({ type: 'edge', id: e.id }); }} />
                {ports.length > 1 && <text x={(x1 + x2) / 2 + 6} y={(y1 + y2) / 2} fontSize={11} fill="#94a3b8">{e.port}</text>}
              </g>
            );
          })}
          {link && (() => {
            const a = def.nodes.find((n) => n.id === link.from);
            if (!a) return null;
            const ports = specs.get(a.kind)?.out_ports ?? ['out'];
            return <path d={`M ${portX(a, ports, link.port)} ${a.position.y + NODE_H} L ${link.x} ${link.y}`} stroke="#06b6d4" strokeWidth={2} strokeDasharray="5 4" fill="none" />;
          })()}
        </svg>
        {def.nodes.map((n) => {
          const spec = specs.get(n.kind);
          const ports = spec?.out_ports ?? ['out'];
          const issues = issuesByNode.get(n.id) ?? [];
          const errors = issues.filter((i) => i.severity === 'error').length;
          const warns = issues.filter((i) => i.severity === 'warning').length;
          const sel = selection?.type === 'node' && selection.id === n.id;
          const hl = highlight?.get(n.id);
          const rs = runState?.get(n.id);
          const first = spec?.fields.map((f) => n.config[f.name]).find((v) => typeof v === 'string' && v.length > 0) as string | undefined;
          return (
            <div key={n.id} data-node={n.id}
              onPointerDown={(e) => { e.stopPropagation(); onSelect({ type: 'node', id: n.id }); if (readOnly || runState) return; const p = point(e); setDrag({ id: n.id, dx: p.x - n.position.x, dy: p.y - n.position.y }); }}
              style={{ position: 'absolute', left: n.position.x, top: n.position.y, width: NODE_W, height: NODE_H, boxSizing: 'border-box', padding: '8px 10px', borderRadius: 10, cursor: 'grab', userSelect: 'none',
                background: sel ? 'rgba(6,182,212,0.16)' : 'rgba(15,23,42,0.92)', border: `${rs && rs.status !== 'pending' ? 2.5 : 1.5}px ${n.enabled ? 'solid' : 'dashed'} ${rs && rs.status !== 'pending' ? STATUS_COLOUR[rs.status] : hl === 'added' ? '#22c55e' : hl === 'changed' ? '#a78bfa' : errors ? '#f87171' : sel ? '#06b6d4' : warns ? '#f59e0b' : 'rgba(255,255,255,0.18)'}`, opacity: n.enabled ? 1 : 0.5, overflow: 'visible' }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                <span title={`risk: ${spec?.risk}`} style={{ width: 8, height: 8, borderRadius: '50%', background: RISK_COLOUR[spec?.risk ?? 'low'], flexShrink: 0 }} />
                <strong style={{ fontSize: 13, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', flex: 1 }}>{n.name}</strong>
                {hl && <span style={{ position: 'absolute', top: -9, right: 8, fontSize: 10, padding: '0 6px', borderRadius: 6, background: hl === 'added' ? '#166534' : '#5b21b6', color: '#fff' }}>{hl === 'added' ? 'new' : 'changed'}</span>}
                {(errors > 0 || warns > 0) && <span title={issues.map((i) => i.message).join('\n')} style={{ fontSize: 11, color: errors ? '#f87171' : '#f59e0b' }}>{errors ? `${errors} ✕` : `${warns} !`}</span>}
              </div>
              <div style={{ ...muted, fontSize: 11.5, marginTop: 2, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{spec?.label}{first ? ` · ${first}` : ''}</div>
              {rs && <span data-run-status={rs.status} style={{ position: 'absolute', bottom: 4, right: 8, fontSize: 10.5, color: STATUS_COLOUR[rs.status] }}>{STATUS_LABEL[rs.status]}</span>}
              {n.kind !== 'trigger' && <span style={{ position: 'absolute', top: -5, left: NODE_W / 2 - 5, width: 10, height: 10, borderRadius: '50%', background: '#334155', border: '1.5px solid #94a3b8' }} />}
              {ports.map((p, i) => (
                <span key={p} data-port={`${n.id}:${p}`} title={ports.length > 1 ? `exit: ${p}` : 'drag to connect'}
                  onPointerDown={(e) => { e.stopPropagation(); onSelect({ type: 'node', id: n.id }); if (readOnly) return; const pt = point(e); setLink({ from: n.id, port: p, x: pt.x, y: pt.y }); }}
                  style={{ position: 'absolute', bottom: -7, left: (NODE_W * (i + 1)) / (ports.length + 1) - 7, width: 14, height: 14, borderRadius: '50%', background: '#0e7490', border: '2px solid #06b6d4', cursor: 'crosshair' }}>
                  {ports.length > 1 && <span style={{ position: 'absolute', top: 14, left: -12, width: 40, textAlign: 'center', fontSize: 10, color: '#94a3b8' }}>{p}</span>}
                </span>
              ))}
            </div>
          );
        })}
      </div>
    </div>
  );
}

// ---- forms ------------------------------------------------------------------------------------

function ConfigField({ spec, value, onChange }: { spec: FieldSpec; value: unknown; onChange: (v: unknown) => void }) {
  const text = value === undefined || value === null ? '' : typeof value === 'string' ? value : typeof value === 'object' ? JSON.stringify(value, null, 2) : String(value);
  const label = `${spec.label}${spec.required ? ' *' : ''}`;
  switch (spec.kind) {
    case 'bool':
      return <label style={{ display: 'flex', gap: 8, alignItems: 'center', fontSize: 13 }}><input type="checkbox" checked={value === true} onChange={(e) => onChange(e.target.checked)} /> {label}</label>;
    case 'choice':
      return (
        <Field label={label} hint={spec.help}>
          <select style={input} value={text} onChange={(e) => onChange(e.target.value)} aria-label={spec.label}>
            {!spec.choices.includes(text) && <option value={text}>{text || '(choose)'}</option>}
            {spec.choices.map((c) => <option key={c} value={c}>{c}</option>)}
          </select>
        </Field>
      );
    case 'long_text':
      return <Field label={label} hint={spec.help}><textarea style={{ ...input, minHeight: 70 }} value={text} onChange={(e) => onChange(e.target.value)} aria-label={spec.label} /></Field>;
    case 'json':
      return (
        <Field label={label} hint={spec.help}>
          <textarea style={{ ...mono, minHeight: 64 }} defaultValue={text} key={text.length === 0 ? 'e' : 'f'} aria-label={spec.label}
            onBlur={(e) => { const t = e.target.value.trim(); if (!t) return onChange(undefined); try { onChange(JSON.parse(t)); } catch { onChange(t); } }} />
        </Field>
      );
    case 'number':
      return <Field label={label} hint={spec.help}><input style={input} value={text} aria-label={spec.label} onChange={(e) => { const t = e.target.value; onChange(t === '' ? undefined : /^-?\d+(\.\d+)?$/.test(t) ? Number(t) : t); }} /></Field>;
    default:
      return <Field label={label} hint={spec.help}><input style={input} value={text} aria-label={spec.label} onChange={(e) => onChange(e.target.value)} /></Field>;
  }
}

function NodeForm({ node, spec, issues, onChange, onDelete, onDuplicate }: {
  node: FlowNode; spec: NodeSpec; issues: Issue[]; onChange: (n: FlowNode) => void; onDelete: () => void; onDuplicate: () => void;
}) {
  const setConfig = (name: string, v: unknown) => {
    const config = { ...node.config };
    if (v === undefined) delete config[name];
    else config[name] = v;
    onChange({ ...node, config });
  };
  return (
    <div style={{ display: 'grid', gap: 10 }}>
      <div>
        <div style={{ fontWeight: 700 }}>{spec.label}</div>
        <div style={muted}>{spec.description}</div>
        <div style={{ ...muted, marginTop: 4 }}>Risk: <span style={{ color: RISK_COLOUR[spec.risk] }}>{spec.risk}</span>{spec.waits_for_person ? ' · waits for you' : ''}{node.outputs.length ? ` · produces: ${node.outputs.join(', ')}` : ''}</div>
      </div>
      {node.kind === 'n8n' && <N8nStatus />}
      {cannotRunNote(spec) !== '' && <div role="status" style={{ fontSize: 12.5, color: '#f59e0b' }}>{cannotRunNote(spec)}</div>}
      {loopLimitNote(spec) !== '' && <div style={muted}>{loopLimitNote(spec)}</div>}
      {issues.map((i, k) => <div key={k} style={{ fontSize: 12.5, color: i.severity === 'error' ? '#f87171' : i.severity === 'warning' ? '#f59e0b' : 'var(--text-secondary)' }}>{i.severity === 'error' ? '✕' : i.severity === 'warning' ? '!' : 'i'} {i.message}</div>)}
      <Field label="Name"><input style={input} value={node.name} onChange={(e) => onChange({ ...node, name: e.target.value })} aria-label="Step name" /></Field>
      <Field label="Id" hint="Other steps refer to this step's results as {{nodes.<id>.<value>}}."><input style={mono} value={node.id} readOnly aria-label="Step id" /></Field>
      <label style={{ display: 'flex', gap: 8, alignItems: 'center', fontSize: 13 }}><input type="checkbox" checked={node.enabled} onChange={(e) => onChange({ ...node, enabled: e.target.checked })} aria-label="Enabled" /> Enabled (a switched-off step is skipped)</label>
      {spec.fields.map((f) => <ConfigField key={f.name} spec={f} value={node.config[f.name]} onChange={(v) => setConfig(f.name, v)} />)}
      <Field label="Credential" hint="The name of a stored account (for example linkedin/main). Never a password: it stays in the Windows Credential Manager."><input style={mono} value={node.credential ?? ''} onChange={(e) => onChange({ ...node, credential: e.target.value.trim() === '' ? null : e.target.value.trim() })} aria-label="Credential" /></Field>
      <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap' }}>
        <Field label="Retries (0–5)"><input type="number" min={0} max={5} style={{ ...input, width: 80 }} value={node.retry.retries} onChange={(e) => onChange({ ...node, retry: { ...node.retry, retries: Number(e.target.value) } })} /></Field>
        <Field label="Wait (s)"><input type="number" min={0} style={{ ...input, width: 80 }} value={node.retry.delay_secs} onChange={(e) => onChange({ ...node, retry: { ...node.retry, delay_secs: Number(e.target.value) } })} /></Field>
        <Field label="Backoff">
          <select style={{ ...input, width: 120 }} value={node.retry.backoff} onChange={(e) => onChange({ ...node, retry: { ...node.retry, backoff: e.target.value as 'fixed' | 'exponential' } })}>
            <option value="exponential">exponential</option><option value="fixed">fixed</option>
          </select>
        </Field>
      </div>
      <Field label="If it fails">
        <select style={input} value={node.on_error} onChange={(e) => onChange({ ...node, on_error: e.target.value as FlowNode['on_error'] })} aria-label="If it fails">
          <option value="fail">Stop the run</option><option value="continue">Carry on</option><option value="ask_user">Ask me what to do</option>
        </select>
      </Field>
      <div style={{ display: 'flex', gap: 8 }}>
        <button style={button} onClick={onDuplicate}><Copy size={14} /> Duplicate</button>
        <button style={dangerButton} onClick={onDelete}><Trash2 size={14} /> Delete</button>
      </div>
    </div>
  );
}

function FlowSettings({ def, report, onChange, onSelectNode }: { def: FlowDef; report: Report | null; onChange: (d: FlowDef) => void; onSelectNode: (id: string) => void }) {
  const setVar = (i: number, patch: Partial<VariableDef>) => onChange({ ...def, variables: def.variables.map((v, j) => (j === i ? { ...v, ...patch } : v)) });
  const errors = report?.issues.filter((i) => i.severity === 'error') ?? [];
  const warnings = report?.issues.filter((i) => i.severity === 'warning') ?? [];
  const infos = report?.issues.filter((i) => i.severity === 'info') ?? [];
  return (
    <div style={{ display: 'grid', gap: 12 }}>
      <div style={{ ...card, borderColor: errors.length ? 'rgba(248,113,113,0.5)' : 'rgba(16,185,129,0.4)' }} data-testid="validation">
        <div style={{ display: 'flex', gap: 8, alignItems: 'center', fontWeight: 600 }}>
          {errors.length ? <AlertTriangle size={16} color="#f87171" /> : <CheckCircle2 size={16} color="#10b981" />}
          {errors.length ? `${errors.length} problem${errors.length === 1 ? '' : 's'} to fix before it can run` : 'Ready to run'}
        </div>
        {[...errors, ...warnings, ...infos].map((i, k) => (
          <div key={k} style={{ fontSize: 12.5, marginTop: 6, color: i.severity === 'error' ? '#f87171' : i.severity === 'warning' ? '#f59e0b' : 'var(--text-secondary)', cursor: i.node ? 'pointer' : 'default' }} onClick={() => i.node && onSelectNode(i.node)}>
            {i.severity === 'error' ? '✕' : i.severity === 'warning' ? '!' : 'i'} {i.message}
          </div>
        ))}
      </div>
      <Field label="Flow name"><input style={input} value={def.name} onChange={(e) => onChange({ ...def, name: e.target.value })} aria-label="Flow name" /></Field>
      <Field label="Description"><input style={input} value={def.description} onChange={(e) => onChange({ ...def, description: e.target.value })} /></Field>
      <Field label="Ask before high-impact steps" hint="Publishing, sending and deleting. 'Never' lets them run without asking.">
        <select style={input} value={def.settings.approval} onChange={(e) => onChange({ ...def, settings: { ...def.settings, approval: e.target.value as FlowDef['settings']['approval'] } })} aria-label="Approval policy">
          <option value="always">Always ask</option><option value="new_only">Only the first run of a new or changed flow</option><option value="never">Never ask</option>
        </select>
      </Field>
      <Field label="Time zone" hint="For example Asia/Kolkata. Empty means this computer's zone."><input style={input} value={def.settings.timezone} onChange={(e) => onChange({ ...def, settings: { ...def.settings, timezone: e.target.value } })} /></Field>
      <div>
        <div style={{ ...muted, marginBottom: 4 }}>Inputs (asked for each run, used as {'{{vars.name}}'})</div>
        {def.variables.map((v, i) => (
          <div key={i} style={{ display: 'flex', gap: 6, marginBottom: 6, alignItems: 'center' }}>
            <input style={{ ...mono, width: 90 }} value={v.name} onChange={(e) => setVar(i, { name: e.target.value })} aria-label="Input name" />
            <input style={{ ...input, flex: 1 }} value={v.label} placeholder="Label" onChange={(e) => setVar(i, { label: e.target.value })} />
            <label style={{ ...muted, display: 'flex', gap: 3, alignItems: 'center' }}><input type="checkbox" checked={v.required} onChange={(e) => setVar(i, { required: e.target.checked })} />req.</label>
            <button style={dangerButton} onClick={() => onChange({ ...def, variables: def.variables.filter((_, j) => j !== i) })} aria-label="Remove input"><X size={12} /></button>
          </div>
        ))}
        <button style={button} onClick={() => onChange({ ...def, variables: [...def.variables, { name: `input${def.variables.length + 1}`, label: '', kind: 'text', required: false, default: null, choices: [] }] })}><Plus size={14} /> Add input</button>
      </div>
    </div>
  );
}

// ---- versions ---------------------------------------------------------------------------------

function VersionsPanel({ flow, onRestored, onClose }: { flow: Flow; onRestored: (f: Flow) => void; onClose: () => void }) {
  const [versions, setVersions] = useState<VersionInfo[]>([]);
  const [picked, setPicked] = useState<number | null>(null);
  const [diff, setDiff] = useState<Diff | null>(null);
  const [error, setError] = useState('');
  useEffect(() => { flowsApi.versions(flow.id).then((r) => setVersions(r.versions)).catch((e) => setError(message(e))); }, [flow.id, flow.version]);
  const pick = async (v: number) => {
    setPicked(v);
    try { setDiff(await flowsApi.diff(flow.id, v, flow.version)); setError(''); } catch (e) { setError(message(e)); }
  };
  return (
    <div style={{ display: 'grid', gap: 10 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between' }}><strong>Versions</strong><button style={button} onClick={onClose}><X size={14} /></button></div>
      {error && <div role="alert" style={{ color: '#f87171', fontSize: 12.5 }}>{error}</div>}
      {versions.map((v) => (
        <button key={v.version} onClick={() => void pick(v.version)} style={{ ...card, padding: 8, textAlign: 'left', cursor: 'pointer', color: 'inherit', borderColor: picked === v.version ? 'rgba(6,182,212,0.6)' : undefined }}>
          <div style={{ fontWeight: 600 }}>v{v.version}{v.version === flow.version ? ' (current)' : ''} · {v.author}</div>
          <div style={muted}>{when(v.created_at)}{v.note ? ` · ${v.note}` : ''}</div>
        </button>
      ))}
      {picked !== null && picked !== flow.version && diff && (
        <div style={card}>
          <div style={{ fontWeight: 600, marginBottom: 6 }}>Changes since v{picked}</div>
          {diff.summary.length === 0 ? <div style={muted}>No difference.</div> : diff.summary.map((s, i) => <div key={i} style={{ fontSize: 12.5 }}>{s}</div>)}
          <div style={{ marginTop: 8 }}>
            <button style={button} onClick={() => { if (window.confirm(`Go back to version ${picked}? This is saved as a new version, so it can be undone.`)) void flowsApi.restore(flow.id, picked).then(onRestored).catch((e) => setError(message(e))); }}>Restore v{picked}</button>
          </div>
        </div>
      )}
    </div>
  );
}

// ---- the AI builder ---------------------------------------------------------------------------

function highlightOf(p: Proposal): Map<string, 'added' | 'changed'> {
  const m = new Map<string, 'added' | 'changed'>();
  p.diff.nodes_added.forEach((id) => m.set(id, 'added'));
  p.diff.nodes_changed.forEach((c) => m.set(c.id, 'changed'));
  return m;
}

/** What the assistant proposes: why, what changes, what it still needs to know, and whether it would work. */
function ProposalCard({ p, busy, applyLabel, onApply, onDismiss, onAnswer }: {
  p: Proposal; busy: boolean; applyLabel: string; onApply: () => void; onDismiss: () => void; onAnswer: (a: Record<string, string>) => void;
}) {
  const [answers, setAnswers] = useState<Record<string, string>>({});
  const errors = p.validation.issues.filter((i) => i.severity === 'error');
  const blocked = p.commands.length === 0;
  const canApply = !blocked && !busy && (p.kind === 'edit' || errors.length === 0);
  return (
    <div style={{ ...card, display: 'grid', gap: 10, borderColor: 'rgba(6,182,212,0.45)' }} data-testid="proposal">
      <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
        <strong style={{ flex: 1 }}>{p.kind === 'create' ? `New flow: ${p.result.name || 'untitled'}` : 'Proposed change'}</strong>
        <span style={{ ...muted, fontSize: 11 }} title="Source of the automated routine suggestion">{p.source === 'model' ? 'guided' : p.source}</span>
      </div>
      {p.explanation.length > 0 && (
        <ul style={{ margin: 0, paddingLeft: 18, display: 'grid', gap: 3, fontSize: 12.5 }}>
          {p.explanation.map((e, i) => <li key={i}>{e}</li>)}
        </ul>
      )}
      {p.schedule && <div style={{ ...muted, color: '#22c55e' }} data-testid="proposal-schedule">Also creates a schedule: {p.schedule.summary}. It is a separate schedule you can change or switch off in the Schedule tab.</div>}
      {p.kind === 'edit' && p.diff.summary.length > 0 && (
        <div style={{ fontSize: 12, display: 'flex', gap: 6, flexWrap: 'wrap' }}>
          {p.diff.summary.map((s, i) => <span key={i} style={{ padding: '2px 8px', borderRadius: 999, border: '1px solid var(--glass-border)', color: s.startsWith('Removed') ? '#f87171' : s.startsWith('Added') ? '#22c55e' : 'var(--text-secondary)' }}>{s}</span>)}
        </div>
      )}
      {p.questions.map((q) => (
        <div key={q.id} style={{ display: 'grid', gap: 4 }}>
          <div style={{ fontSize: 13, fontWeight: 600 }}>{q.question}{q.blocking ? ' *' : ''}</div>
          <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
            {q.choices.map((c) => <button key={c} style={button} onClick={() => onAnswer({ ...answers, [q.id]: c })}>{c}</button>)}
            <input style={{ ...input, flex: 1, minWidth: 140 }} placeholder="Type an answer" value={answers[q.id] ?? ''} aria-label={q.question}
              onChange={(e) => setAnswers({ ...answers, [q.id]: e.target.value })} onKeyDown={(e) => { if (e.key === 'Enter' && (answers[q.id] ?? '').trim()) onAnswer(answers); }} />
          </div>
        </div>
      ))}
      {p.questions.length > 0 && Object.values(answers).some((v) => v.trim()) && <div><button style={button} onClick={() => onAnswer(answers)}>Answer</button></div>}
      {p.warnings.map((w, i) => <div key={i} style={{ fontSize: 12.5, color: '#f59e0b' }}>! {w}</div>)}
      {p.not_understood.length > 0 && !p.understood && <div style={{ fontSize: 12.5, color: '#f87171' }}>Not understood: {p.not_understood.join(' / ')}</div>}
      {errors.length > 0 && (
        <div style={{ fontSize: 12.5, color: '#f87171' }}>
          {errors.length} problem{errors.length === 1 ? '' : 's'} in the result: {errors.slice(0, 3).map((e) => e.message).join(' · ')}
        </div>
      )}
      {!blocked && errors.length === 0 && <div style={{ fontSize: 12.5, color: '#10b981' }}>The result is valid and ready to run.</div>}
      {p.resolution.length > 0 && (
        <details>
          <summary style={{ ...muted, cursor: 'pointer' }}>How each step will run</summary>
          <div style={{ display: 'grid', gap: 2, marginTop: 6, fontSize: 12 }}>
            {p.resolution.filter((r) => r.mechanism !== 'internal').map((r) => <div key={r.node}><strong>{r.name}</strong> <span style={muted}>· {r.mechanism}{r.note ? ` · ${r.note}` : ''}</span></div>)}
          </div>
        </details>
      )}
      <div style={{ display: 'flex', gap: 8 }}>
        <button style={{ ...button, opacity: canApply ? 1 : 0.5 }} disabled={!canApply} onClick={onApply}>{busy ? 'Working…' : applyLabel}</button>
        <button style={button} onClick={onDismiss}>Dismiss</button>
      </div>
    </div>
  );
}

function AssistantPanel({ flow, dirty, pending, undoTo, onPending, onApplied, onUndo }: {
  flow: Flow; dirty: boolean; pending: Proposal | null; undoTo: number | null;
  onPending: (p: Proposal | null) => void; onApplied: (r: { flow: Flow; previous_version: number | null }) => void; onUndo: () => void;
}) {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [lastRequest, setLastRequest] = useState('');

  const refresh = useCallback(() => { flowsApi.chat(flow.id).then((r) => setMessages(r.messages)).catch(() => undefined); }, [flow.id]);
  useEffect(() => { refresh(); }, [refresh, flow.version]);

  const ask = async (request: string, answers?: Record<string, string>) => {
    if (dirty) { setError('Save your changes first: the assistant works on the saved flow.'); return; }
    setBusy(true);
    setError('');
    try {
      const p = await flowsApi.plan(request, flow.id, answers);
      setLastRequest(request);
      onPending(p);
      refresh();
    } catch (e) { setError(message(e)); } finally { setBusy(false); }
  };

  const send = () => {
    const t = text.trim();
    if (!t || busy) return;
    setText('');
    void ask(t);
  };

  const apply = async () => {
    if (!pending) return;
    setBusy(true);
    try {
      const r = await flowsApi.apply(pending, flow.id);
      onApplied(r);
      refresh();
    } catch (e) { setError(message(e)); } finally { setBusy(false); }
  };

  return (
    <div style={{ display: 'grid', gap: 10 }} data-testid="assistant">
      <div style={muted}>Tell me what to change, for example “switch LinkedIn to Instagram”, “add an approval step”, “schedule it for tomorrow at 10 AM”, “remove the image”, “save the posts to Excel”. I show you the change before anything happens.</div>
      <div style={{ display: 'grid', gap: 6, maxHeight: 180, overflow: 'auto' }}>
        {messages.map((m) => (
          <div key={m.id} style={{ fontSize: 12.5, padding: '6px 9px', borderRadius: 8, background: m.role === 'user' ? 'rgba(6,182,212,0.12)' : 'rgba(255,255,255,0.05)' }}>
            <span style={{ ...muted, fontSize: 11 }}>{m.role === 'user' ? 'You' : 'Assistant'}</span><div>{m.text}</div>
          </div>
        ))}
      </div>
      {error && <div role="alert" style={{ color: '#f87171', fontSize: 12.5 }}>{error}</div>}
      {undoTo !== null && !pending && <div><button style={button} onClick={onUndo}>Undo the last change (back to v{undoTo})</button></div>}
      {pending && (
        <ProposalCard p={pending} busy={busy} applyLabel="Apply this change" onApply={() => void apply()} onDismiss={() => onPending(null)} onAnswer={(a) => void ask(lastRequest, a)} />
      )}
      <div style={{ display: 'flex', gap: 6 }}>
        <input style={{ ...input, flex: 1 }} value={text} placeholder="What should change?" aria-label="Ask the assistant" onChange={(e) => setText(e.target.value)} onKeyDown={(e) => { if (e.key === 'Enter') send(); }} disabled={busy} />
        <button style={button} onClick={send} disabled={busy || !text.trim()}>{busy ? '…' : 'Send'}</button>
      </div>
    </div>
  );
}

// ---- editor -----------------------------------------------------------------------------------

function FlowEditor({ flowId, specs, initialRunId, onClose }: { flowId: number; specs: NodeSpec[]; initialRunId?: number | null; onClose: () => void }) {
  const specMap = useMemo(() => new Map(specs.map((s) => [s.kind, s])), [specs]);
  const [flow, setFlow] = useState<Flow | null>(null);
  const [def, setDef] = useState<FlowDef | null>(null);
  const [selection, setSelection] = useState<Selection>(null);
  const [report, setReport] = useState<Report | null>(null);
  const [error, setError] = useState('');
  const [note, setNote] = useState('');
  const [saving, setSaving] = useState(false);
  const [showVersions, setShowVersions] = useState(false);
  const [tab, setTab] = useState<'inspect' | 'assistant' | 'run' | 'schedule'>(initialRunId ? 'run' : 'inspect');
  const [runId, setRunId] = useState<number | null>(initialRunId ?? null);
  const [showRunDialog, setShowRunDialog] = useState(false);
  const [historyKey, setHistoryKey] = useState(0);
  const live = useRun(runId);
  const [pending, setPending] = useState<Proposal | null>(null);
  const [undoTo, setUndoTo] = useState<number | null>(null);
  const base = useRef('');

  const load = useCallback(async () => {
    try {
      const f = await flowsApi.get(flowId);
      setFlow(f);
      const { id: _id, owner_id: _o, version: _v, created_at: _c, updated_at: _u, ...d } = f;
      void _id; void _o; void _v; void _c; void _u;
      setDef(d);
      base.current = JSON.stringify(d);
      setError('');
    } catch (e) { setError(message(e)); }
  }, [flowId]);
  useEffect(() => { void load(); }, [load]);

  const dirty = def !== null && JSON.stringify(def) !== base.current;

  useEffect(() => {
    if (!def) return;
    const t = setTimeout(() => { flowsApi.validate(def).then(setReport).catch(() => undefined); }, 350);
    return () => clearTimeout(t);
  }, [def]);

  const issuesByNode = useMemo(() => {
    const m = new Map<string, Issue[]>();
    (report?.issues ?? []).forEach((i) => { if (i.node) m.set(i.node, [...(m.get(i.node) ?? []), i]); });
    return m;
  }, [report]);

  const update = useCallback((fn: (d: FlowDef) => FlowDef) => setDef((d) => (d ? fn(d) : d)), []);

  const deleteSelection = useCallback(() => {
    if (!selection) return;
    if (selection.type === 'node') update((d) => ({ ...d, nodes: d.nodes.filter((n) => n.id !== selection.id), edges: d.edges.filter((e) => e.from !== selection.id && e.to !== selection.id) }));
    else update((d) => ({ ...d, edges: d.edges.filter((e) => e.id !== selection.id) }));
    setSelection(null);
  }, [selection, update]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const tag = (e.target as HTMLElement | null)?.tagName;
      if ((e.key === 'Delete' || e.key === 'Backspace') && tag !== 'INPUT' && tag !== 'TEXTAREA' && tag !== 'SELECT') deleteSelection();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [deleteSelection]);

  const addNode = (kind: NodeKind) => {
    const spec = specMap.get(kind);
    if (!spec || !def) return;
    const selected = selection?.type === 'node' ? def.nodes.find((n) => n.id === selection.id) : undefined;
    const lowest = def.nodes.reduce((m, n) => Math.max(m, n.position.y), 0);
    const x = selected ? selected.position.x : 80;
    const y = selected ? selected.position.y + NODE_H + 50 : lowest + NODE_H + 50;
    const node = makeNode(def, spec, x, y);
    const selSpec = selected ? specMap.get(selected.kind) : undefined;
    const port = selSpec?.out_ports.find((p) => !def.edges.some((e) => e.from === selected?.id && e.port === p));
    update((d) => ({ ...d, nodes: [...d.nodes, node], edges: selected && port && kind !== 'trigger' ? [...d.edges, { id: edgeId(selected.id, port, node.id), from: selected.id, to: node.id, port }] : d.edges }));
    setSelection({ type: 'node', id: node.id });
  };

  const duplicate = (n: FlowNode) => {
    if (!def) return;
    const id = uniqueId(def, n.id);
    const copy: FlowNode = { ...n, id, name: `${n.name} copy`, position: { x: n.position.x + 40, y: n.position.y + 40 } };
    update((d) => ({ ...d, nodes: [...d.nodes, copy] }));
    setSelection({ type: 'node', id });
  };

  const connect = (from: string, port: string, to: string) =>
    update((d) => (d.edges.some((e) => e.from === from && e.port === port && e.to === to) ? d : { ...d, edges: [...d.edges, { id: edgeId(from, port, to), from, to, port }] }));

  const save = async () => {
    if (!def || !flow) return;
    setSaving(true);
    try {
      const saved = await flowsApi.save(flow.id, def, flow.version, note.trim());
      setNote('');
      setFlow(saved);
      const { id: _id, owner_id: _o, version: _v, created_at: _c, updated_at: _u, ...d } = saved;
      void _id; void _o; void _v; void _c; void _u;
      setDef(d);
      base.current = JSON.stringify(d);
      setError('');
    } catch (e) { setError(message(e)); } finally { setSaving(false); }
  };

  const download = () => {
    if (!def) return;
    const url = URL.createObjectURL(new Blob([JSON.stringify(def, null, 2)], { type: 'application/json' }));
    const a = document.createElement('a');
    a.href = url;
    a.download = `${def.name.replace(/[^a-z0-9]+/gi, '-').toLowerCase() || 'flow'}.json`;
    a.click();
    URL.revokeObjectURL(url);
  };

  if (!def || !flow) return <div style={muted}>{error || 'Loading…'}</div>;
  const selNode = selection?.type === 'node' ? def.nodes.find((n) => n.id === selection.id) : undefined;
  const selSpec = selNode ? specMap.get(selNode.kind) : undefined;
  const categories = [...new Set(specs.map((s) => s.category))];
  const errorCount = report?.issues.filter((i) => i.severity === 'error').length ?? 0;

  return (
    <div style={{ display: 'grid', gap: 10, height: '100%', gridTemplateRows: 'auto auto 1fr' }}>
      <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
        <button style={button} onClick={onClose}>← Flows</button>
        <strong style={{ fontSize: 16 }}>{def.name}</strong>
        <span style={muted}>v{flow.version}{dirty ? ' · unsaved changes' : ''}</span>
        <span style={{ flex: 1 }} />
        <select style={{ ...input, width: 170 }} value="" onChange={(e) => { if (e.target.value) addNode(e.target.value as NodeKind); }} aria-label="Add a step">
          <option value="">+ Add a step…</option>
          {categories.map((c) => <optgroup key={c} label={c}>{specs.filter((s) => s.category === c).map((s) => <option key={s.kind} value={s.kind}>{kindOptionLabel(s)}</option>)}</optgroup>)}
        </select>
        <button style={button} onClick={() => { setTab('run'); setShowRunDialog(true); }} data-testid="run-button"><Play size={14} /> Run</button>
        <button style={button} onClick={() => update(autoLayout)}><LayoutGrid size={14} /> Tidy</button>
        <button style={button} onClick={() => setShowVersions((v) => !v)}><History size={14} /> Versions</button>
        <button style={button} onClick={download}><Download size={14} /> Export</button>
        <input style={{ ...input, width: 170 }} placeholder="What changed? (optional)" value={note} onChange={(e) => setNote(e.target.value)} aria-label="Change note" />
        <button style={{ ...button, opacity: dirty ? 1 : 0.5 }} onClick={() => void save()} disabled={!dirty || saving}><Save size={14} /> {saving ? 'Saving…' : 'Save'}</button>
      </div>
      {error && <div role="alert" style={{ ...card, borderColor: 'rgba(248,113,113,0.5)', color: '#f87171', fontSize: 13 }}>{error} {error.startsWith('conflict') && <button style={button} onClick={() => void load()}>Reload</button>}</div>}
      <div style={{ display: 'grid', gridTemplateColumns: 'minmax(0,1fr) 340px', gap: 12, minHeight: 0 }}>
        <div style={{ minHeight: 0, display: 'grid', gridTemplateRows: '1fr auto', gap: 6 }}>
          <Canvas def={pending ? pending.result : tab === 'run' && live.detail ? live.detail.run.def : def} specs={specMap} selection={selection} issuesByNode={pending || (tab === 'run' && live.detail) ? new Map() : issuesByNode} onSelect={setSelection} readOnly={!!pending || (tab === 'run' && !!live.detail)} highlight={pending ? highlightOf(pending) : undefined} runState={tab === 'run' && live.detail ? new Map(live.detail.nodes.map((n) => [n.node_id, n])) : undefined}
            onMove={(id, x, y) => update((d) => ({ ...d, nodes: d.nodes.map((n) => (n.id === id ? { ...n, position: { x, y } } : n)) }))} onConnect={connect} />
          <div style={{ ...muted, display: 'flex', gap: 14 }}>
            <span>Drag a step to move it. Drag from the round handle under a step to another step to connect. Select and press Delete to remove.</span>
            {selection?.type === 'edge' && <button style={dangerButton} onClick={deleteSelection}><Trash2 size={12} /> Delete connection</button>}
            <span style={{ marginLeft: 'auto', color: errorCount ? '#f87171' : '#10b981' }}>{errorCount ? `${errorCount} to fix` : 'valid'}</span>
          </div>
        </div>
        <div style={{ ...card, overflow: 'auto', maxHeight: 'calc(100vh - 220px)' }} data-testid="side-panel">
          <div style={{ display: 'flex', gap: 6, marginBottom: 10 }} role="tablist">
            <button role="tab" aria-selected={tab === 'inspect'} style={{ ...button, opacity: tab === 'inspect' ? 1 : 0.6 }} onClick={() => setTab('inspect')}>Step</button>
            <button role="tab" aria-selected={tab === 'assistant'} style={{ ...button, opacity: tab === 'assistant' ? 1 : 0.6 }} onClick={() => setTab('assistant')}>Assistant</button>
            <button role="tab" aria-selected={tab === 'run'} style={{ ...button, opacity: tab === 'run' ? 1 : 0.6 }} onClick={() => setTab('run')}>Run{live.detail && ['running', 'waiting', 'queued'].includes(live.detail.run.status) ? ' •' : ''}</button>
            <button role="tab" aria-selected={tab === 'schedule'} style={{ ...button, opacity: tab === 'schedule' ? 1 : 0.6 }} onClick={() => setTab('schedule')}>Schedule</button>
          </div>
          {tab === 'schedule' ? (
            <SchedulePanel flowId={flow.id} onOpenRun={(id) => { setRunId(id); setTab('run'); setHistoryKey((k) => k + 1); }} />
          ) : tab === 'run' ? (
            showRunDialog ? (
              <RunDialog flowId={flow.id} def={def} blocked={dirty ? 'Save your changes first: a run uses the saved flow.' : errorCount ? `The flow has ${errorCount} problem${errorCount === 1 ? '' : 's'} to fix first.` : ''}
                onStarted={(id) => { setRunId(id); setShowRunDialog(false); setHistoryKey((k) => k + 1); }} onClose={() => setShowRunDialog(false)} />
            ) : runId !== null ? (
              <RunPanel live={live} def={live.detail?.run.def ?? def} selected={selection?.type === 'node' ? selection.id : null} onSelect={(id) => setSelection(id ? { type: 'node', id } : null)}
                onOpenRun={(id) => { setRunId(id); setHistoryKey((k) => k + 1); }} onGone={() => { setRunId(null); setHistoryKey((k) => k + 1); }} />
            ) : (
              <div style={{ display: 'grid', gap: 10 }}>
                <button style={{ ...button, justifySelf: 'start' }} onClick={() => setShowRunDialog(true)}><Play size={14} /> Run this flow</button>
                <RunHistory flowId={flow.id} onOpen={setRunId} refreshKey={historyKey} />
              </div>
            )
          ) : tab === 'assistant' ? (
            <AssistantPanel flow={flow} dirty={dirty} pending={pending} undoTo={undoTo} onPending={setPending}
              onApplied={(r) => { setPending(null); setUndoTo(r.previous_version); void load(); }}
              onUndo={() => { if (undoTo === null) return; void flowsApi.restore(flow.id, undoTo).then(() => { setUndoTo(null); return load(); }).catch((e) => setError(message(e))); }} />
          ) : showVersions ? <VersionsPanel flow={flow} onRestored={(f) => { setShowVersions(false); setFlow(f); void load(); }} onClose={() => setShowVersions(false)} />
            : selNode && selSpec ? <NodeForm node={selNode} spec={selSpec} issues={issuesByNode.get(selNode.id) ?? []} onChange={(n) => update((d) => ({ ...d, nodes: d.nodes.map((x) => (x.id === n.id ? n : x)) }))} onDelete={deleteSelection} onDuplicate={() => duplicate(selNode)} />
            : <FlowSettings def={def} report={report} onChange={setDef} onSelectNode={(id) => setSelection({ type: 'node', id })} />}
        </div>
      </div>
    </div>
  );
}

// ---- the panel --------------------------------------------------------------------------------

export function FlowsPanel() {
  const [flows, setFlows] = useState<FlowSummary[]>([]);
  const [specs, setSpecs] = useState<NodeSpec[]>([]);
  const [templates, setTemplates] = useState<{ id: string; name: string; description: string }[]>([]);
  const [openId, setOpenId] = useState<number | null>(null);
  const [openRun, setOpenRun] = useState<number | null>(null);
  const [error, setError] = useState('');
  const fileInput = useRef<HTMLInputElement>(null);
  const [request, setRequest] = useState('');
  const [proposal, setProposal] = useState<Proposal | null>(null);
  const [building, setBuilding] = useState(false);
  const specMap = useMemo(() => new Map(specs.map((x) => [x.kind, x])), [specs]);

  const load = useCallback(async () => {
    try {
      const [l, t, k] = await Promise.all([flowsApi.list(), flowsApi.templates(), flowsApi.nodeTypes()]);
      setFlows(l.flows);
      setTemplates(t.templates);
      setSpecs(k.node_types);
      setError('');
    } catch (e) { setError(message(e)); }
  }, []);
  useEffect(() => { void load(); }, [load]);

  const create = async (template: string, name?: string) => {
    try { const f = await flowsApi.create({ template, name }); await load(); setOpenId(f.id); } catch (e) { setError(message(e)); }
  };

  const build = async (answers?: Record<string, string>) => {
    const text = request.trim();
    if (!text || building) return;
    setBuilding(true);
    setError('');
    try { setProposal(await flowsApi.plan(text, undefined, answers)); } catch (e) { setError(message(e)); } finally { setBuilding(false); }
  };

  const accept = async () => {
    if (!proposal) return;
    setBuilding(true);
    try {
      const r = await flowsApi.apply(proposal);
      setProposal(null);
      setRequest('');
      await load();
      setOpenId(r.flow.id);
    } catch (e) { setError(message(e)); } finally { setBuilding(false); }
  };

  const importFile = async (file: File) => {
    try {
      const def = JSON.parse(await file.text()) as FlowDef;
      const f = await flowsApi.importFlow(def);
      await load();
      setOpenId(f.id);
    } catch (e) { setError(message(e)); }
  };

  if (openId !== null && specs.length > 0) return <FlowEditor key={`${openId}:${openRun ?? ''}`} flowId={openId} specs={specs} initialRunId={openRun} onClose={() => { setOpenId(null); setOpenRun(null); void load(); }} />;

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: '24px', maxWidth: '1200px', margin: '0 auto', width: '100%' }}>
      <div className="glass-panel" style={{ padding: '28px', borderRadius: '16px', position: 'relative', overflow: 'hidden' }}>
        <div style={{ position: 'absolute', top: 0, left: 0, right: 0, height: '4px', background: 'linear-gradient(90deg, var(--accent-cyan), var(--accent-purple))' }} />
        
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: '16px', flexWrap: 'wrap', marginBottom: '16px' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '12px' }}>
            <div style={{ width: '48px', height: '48px', borderRadius: '12px', background: 'rgba(0, 240, 255, 0.1)', display: 'flex', alignItems: 'center', justifyContent: 'center', border: '1px solid rgba(0, 240, 255, 0.2)' }}>
              <GitBranch size={24} color="var(--accent-cyan)" />
            </div>
            <div>
              <h2 style={{ margin: 0, fontSize: '24px', fontWeight: '700', letterSpacing: '-0.02em', color: 'var(--text-primary)' }}>AI Workflow Studio</h2>
              <p style={{ margin: '4px 0 0 0', fontSize: '14px', color: 'var(--text-secondary)' }}>Design, execute, and monitor autonomous multi-step pipelines.</p>
            </div>
          </div>
          
          <div style={{ display: 'flex', gap: '12px' }}>
            <input ref={fileInput} type="file" accept="application/json,.json" style={{ display: 'none' }} onChange={(e) => { const f = e.target.files?.[0]; if (f) void importFile(f); e.target.value = ''; }} />
            <button 
              onClick={() => fileInput.current?.click()}
              style={{
                padding: '10px 18px',
                borderRadius: '30px',
                border: '1px solid var(--glass-border)',
                background: 'rgba(255,255,255,0.05)',
                color: 'var(--text-primary)',
                fontWeight: '600',
                cursor: 'pointer',
                display: 'flex',
                alignItems: 'center',
                gap: '8px',
                fontSize: '14px',
                transition: 'all 0.2s'
              }}
            >
              <Upload size={16} /> Import Flow
            </button>
          </div>
        </div>
      </div>

      {error && (
        <div style={{ padding: '16px', borderRadius: '12px', background: 'rgba(239, 68, 68, 0.05)', border: '1px solid rgba(239, 68, 68, 0.3)', color: 'var(--accent-rose)', display: 'flex', alignItems: 'center', gap: '10px', fontSize: '14px' }}>
          <AlertTriangle size={20} />
          <span>{error}</span>
        </div>
      )}

      {/* Health & Status Dashboards */}
      <div style={{ display: 'flex', flexDirection: 'column', gap: '16px' }}>
        <Inbox onOpen={(flowId, runId) => { setOpenRun(runId); setOpenId(flowId); }} />
        <CommentInbox onOpen={(flowId, runId) => { setOpenRun(runId); setOpenId(flowId); }} />
        <Notifications onOpen={(flowId, runId) => { setOpenRun(runId); setOpenId(flowId); }} />
        <ScheduledDashboard onOpen={(flowId, runId) => { setOpenRun(runId); setOpenId(flowId); }} />
        <StatusChat onChanged={() => void load()} />
        <HealthDashboard onOpen={(flowId, runId) => { setOpenRun(runId ?? null); setOpenId(flowId); }} />
      </div>

      <div className="glass-panel" style={{ padding: '28px', borderRadius: '16px', display: 'grid', gap: '16px' }} data-testid="composer">
        <div>
          <h3 style={{ margin: 0, fontSize: '18px', fontWeight: '700', color: 'var(--text-primary)' }}>Natural Language Generation</h3>
          <p style={{ margin: '4px 0 12px 0', fontSize: '13px', color: 'var(--text-secondary)' }}>Describe the workflow you want to build and Aqua will generate it for you.</p>
        </div>
        
        <textarea 
          value={request} 
          onChange={(e) => setRequest(e.target.value)} 
          placeholder="E.g., Create a flow that researches the latest AI news, generates a summary image, and posts it to LinkedIn..." 
          style={{ 
            width: '100%',
            padding: '16px',
            borderRadius: '12px',
            background: 'rgba(0,0,0,0.4)',
            border: '1px solid rgba(192, 132, 252, 0.3)',
            color: 'var(--text-primary)',
            fontSize: '15px',
            lineHeight: '1.6',
            resize: 'vertical',
            outline: 'none',
            minHeight: '100px',
            boxShadow: 'inset 0 2px 10px rgba(0,0,0,0.2)'
          }} 
        />
        
        <div>
          <button 
            className="btn-primary"
            onClick={() => void build()} 
            disabled={building || !request.trim()}
            style={{
              padding: '12px 24px',
              borderRadius: '30px',
              border: 'none',
              background: building ? 'var(--bg-tertiary)' : 'linear-gradient(135deg, var(--accent-cyan), var(--accent-purple))',
              color: building ? 'var(--text-muted)' : '#fff',
              fontWeight: '700',
              cursor: building || !request.trim() ? 'not-allowed' : 'pointer',
              display: 'inline-flex',
              alignItems: 'center',
              gap: '8px',
              fontSize: '14px',
              opacity: building || !request.trim() ? 0.6 : 1,
              boxShadow: building ? 'none' : '0 4px 15px rgba(192, 132, 252, 0.3)'
            }}
          >
            <Plus size={16} className={building ? 'spin' : ''} /> {building ? 'Generating Flow...' : 'Generate Flow'}
          </button>
        </div>
        
        {proposal && (
          <div style={{ marginTop: '16px', padding: '16px', borderRadius: '12px', background: 'rgba(0, 0, 0, 0.3)', border: '1px solid var(--glass-border)' }}>
            <ProposalCard p={proposal} busy={building} applyLabel="Create this flow" onApply={() => void accept()} onDismiss={() => setProposal(null)} onAnswer={(a) => void build(a)} />
            {proposal.result.nodes.length > 0 && (
              <div style={{ height: '380px', marginTop: '16px', borderRadius: '12px', overflow: 'hidden' }}>
                <Canvas def={proposal.result} specs={specMap} selection={null} issuesByNode={new Map()} onSelect={() => undefined} onMove={() => undefined} onConnect={() => undefined} readOnly minHeight={360} />
              </div>
            )}
          </div>
        )}
      </div>

      <div>
        <h3 style={{ fontSize: '20px', fontWeight: '700', color: 'var(--text-primary)', margin: '0 0 16px 0' }}>Quick Templates</h3>
        <div style={{ display: 'grid', gap: '16px', gridTemplateColumns: 'repeat(auto-fill, minmax(280px, 1fr))' }}>
          {templates.map((t) => (
            <div key={t.id} className="glass-panel" style={{ padding: '24px', borderRadius: '12px', display: 'flex', flexDirection: 'column', gap: '12px', transition: 'transform 0.2s', cursor: 'pointer' }} onClick={() => void create(t.id, t.id === 'blank' ? undefined : undefined)}>
              <h4 style={{ margin: 0, fontSize: '16px', fontWeight: '700', color: 'var(--accent-cyan)' }}>{t.name}</h4>
              <p style={{ margin: 0, fontSize: '13px', color: 'var(--text-secondary)', lineHeight: '1.5', flex: 1 }}>{t.description}</p>
              <div style={{ display: 'flex', alignItems: 'center', gap: '6px', color: 'var(--text-primary)', fontSize: '13px', fontWeight: '600', marginTop: '8px' }}>
                <Plus size={14} /> Create from template
              </div>
            </div>
          ))}
        </div>
      </div>

      <div style={{ marginTop: '8px' }}>
        <h3 style={{ fontSize: '20px', fontWeight: '700', color: 'var(--text-primary)', margin: '0 0 16px 0' }}>Active Flows</h3>
        
        {flows.length === 0 && (
          <div className="glass-panel" style={{ padding: '32px', borderRadius: '12px', textAlign: 'center', color: 'var(--text-muted)' }}>
            <GitBranch size={32} style={{ opacity: 0.5, marginBottom: '12px' }} />
            <div style={{ fontSize: '15px' }}>No workflows configured yet.</div>
            <div style={{ fontSize: '13px', marginTop: '4px' }}>Create one using natural language or select a template above.</div>
          </div>
        )}
        
        <div style={{ display: 'flex', flexDirection: 'column', gap: '12px' }}>
          {flows.map((f) => (
            <div key={f.id} className="glass-panel" style={{ padding: '20px', borderRadius: '12px', display: 'flex', gap: '16px', alignItems: 'center', transition: 'all 0.2s' }}>
              <div style={{ width: '40px', height: '40px', borderRadius: '10px', background: 'rgba(16, 185, 129, 0.1)', display: 'flex', alignItems: 'center', justifyContent: 'center', border: '1px solid rgba(16, 185, 129, 0.2)', flexShrink: 0 }}>
                <LayoutGrid size={20} color="var(--accent-emerald)" />
              </div>
              
              <div style={{ flex: 1, minWidth: 0 }}>
                <div style={{ fontWeight: '700', fontSize: '16px', color: 'var(--text-primary)', marginBottom: '4px' }}>{f.name}</div>
                <div style={{ fontSize: '13px', color: 'var(--text-secondary)', display: 'flex', alignItems: 'center', gap: '8px', flexWrap: 'wrap' }}>
                  <span style={{ padding: '2px 8px', background: 'rgba(255,255,255,0.05)', borderRadius: '4px' }}>{f.node_count} nodes</span>
                  <span style={{ padding: '2px 8px', background: 'rgba(255,255,255,0.05)', borderRadius: '4px' }}>v{f.version}</span>
                  <span>Updated: {when(f.updated_at)}</span>
                  {f.description && <span style={{ width: '100%', marginTop: '2px' }}>{f.description}</span>}
                </div>
              </div>
              
              <div style={{ display: 'flex', gap: '8px' }}>
                <button 
                  onClick={() => setOpenId(f.id)}
                  style={{
                    padding: '8px 16px',
                    borderRadius: '8px',
                    border: 'none',
                    background: 'rgba(0, 240, 255, 0.15)',
                    color: 'var(--accent-cyan)',
                    fontWeight: '600',
                    cursor: 'pointer',
                    display: 'flex',
                    alignItems: 'center',
                    gap: '6px',
                    fontSize: '13px'
                  }}
                >
                  <Power size={14} /> Open
                </button>
                <button 
                  onClick={() => { if (window.confirm(`Delete “${f.name}” and its history?`)) void flowsApi.remove(f.id).then(load).catch((e) => setError(message(e))); }}
                  style={{
                    padding: '8px 12px',
                    borderRadius: '8px',
                    border: '1px solid rgba(239, 68, 68, 0.3)',
                    background: 'rgba(239, 68, 68, 0.1)',
                    color: 'var(--accent-rose)',
                    cursor: 'pointer',
                    display: 'flex',
                    alignItems: 'center',
                    justifyContent: 'center'
                  }}
                  title="Delete Flow"
                >
                  <Trash2 size={16} />
                </button>
              </div>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
